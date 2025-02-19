// This file is part of Rundler.
//
// Rundler is free software: you can redistribute it and/or modify it under the
// terms of the GNU Lesser General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later version.
//
// Rundler is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
// without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
// See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with Rundler.
// If not, see https://www.gnu.org/licenses/.

use std::{sync::Arc, time::Duration};

use anyhow::{bail, Context};
use async_trait::async_trait;
use ethers::types::{transaction::eip2718::TypedTransaction, H256, U256};
use rundler_provider::Provider;
use rundler_sim::ExpectedStorage;
use rundler_types::GasFees;
use tokio::time;
use tracing::{info, warn};

use crate::sender::{TransactionSender, TxSenderError, TxStatus};

/// Keeps track of pending transactions in order to suggest nonces and
/// replacement fees and ensure that transactions do not get stalled. All sent
/// transactions should flow through here.
///
/// `check_for_update_now` and `send_transaction_and_wait` are intended to be
/// called by a single caller at a time, with no new transactions attempted
/// until it returns a `TrackerUpdate` to indicate whether a transaction has
/// succeeded (potentially not the most recent one) or whether circumstances
/// have changed so that it is worth making another attempt.
#[async_trait]
pub(crate) trait TransactionTracker: Send + Sync + 'static {
    fn get_nonce_and_required_fees(&self) -> anyhow::Result<(U256, Option<GasFees>)>;

    /// Sends the provided transaction and typically returns its transaction
    /// hash, but if the transaction failed to send because another transaction
    /// with the same nonce mined first, then returns information about that
    /// transaction instead.
    async fn send_transaction(
        &self,
        tx: TypedTransaction,
        expected_stroage: &ExpectedStorage,
    ) -> anyhow::Result<SendResult>;

    /// Waits until one of the following occurs:
    ///
    /// 1. One of our transactions mines (not necessarily the one just sent).
    /// 2. All our send transactions have dropped.
    /// 3. Our nonce has changed but none of our transactions mined. This means
    ///    that a transaction from our account other than one of the ones we are
    ///    tracking has mined. This should not normally happen.
    /// 4. Several new blocks have passed.
    async fn wait_for_update(&self) -> anyhow::Result<TrackerUpdate>;

    /// Like `wait_for_update`, except it returns immediately if there is no
    /// update rather than waiting for several new blocks.
    async fn check_for_update_now(&self) -> anyhow::Result<Option<TrackerUpdate>>;
}

pub(crate) enum SendResult {
    TxHash(H256),
    TrackerUpdate(TrackerUpdate),
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum TrackerUpdate {
    Mined {
        tx_hash: H256,
        nonce: U256,
        block_number: u64,
        attempt_number: u64,
        gas_limit: Option<U256>,
        gas_used: Option<U256>,
    },
    StillPendingAfterWait,
    LatestTxDropped {
        nonce: U256,
    },
    NonceUsedForOtherTx {
        nonce: U256,
    },
    ReplacementUnderpriced,
}

#[derive(Debug)]
pub(crate) struct TransactionTrackerImpl<P, T>(
    tokio::sync::Mutex<TransactionTrackerImplInner<P, T>>,
)
where
    P: Provider,
    T: TransactionSender;

#[derive(Debug)]
struct TransactionTrackerImplInner<P, T>
where
    P: Provider,
    T: TransactionSender,
{
    provider: Arc<P>,
    sender: T,
    settings: Settings,
    nonce: U256,
    transactions: Vec<PendingTransaction>,
    has_dropped: bool,
    attempt_count: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    pub(crate) poll_interval: Duration,
    pub(crate) max_blocks_to_wait_for_mine: u64,
    pub(crate) replacement_fee_percent_increase: u64,
}

#[derive(Clone, Copy, Debug)]
struct PendingTransaction {
    tx_hash: H256,
    gas_fees: GasFees,
    attempt_number: u64,
}

#[async_trait]
impl<P, T> TransactionTracker for TransactionTrackerImpl<P, T>
where
    P: Provider,
    T: TransactionSender,
{
    fn get_nonce_and_required_fees(&self) -> anyhow::Result<(U256, Option<GasFees>)> {
        Ok(self.inner()?.get_nonce_and_required_fees())
    }

    async fn send_transaction(
        &self,
        tx: TypedTransaction,
        expected_storage: &ExpectedStorage,
    ) -> anyhow::Result<SendResult> {
        self.inner()?.send_transaction(tx, expected_storage).await
    }

    async fn wait_for_update(&self) -> anyhow::Result<TrackerUpdate> {
        self.inner()?.wait_for_update().await
    }

    async fn check_for_update_now(&self) -> anyhow::Result<Option<TrackerUpdate>> {
        self.inner()?.check_for_update_now().await
    }
}

impl<P, T> TransactionTrackerImpl<P, T>
where
    P: Provider,
    T: TransactionSender,
{
    pub(crate) async fn new(
        provider: Arc<P>,
        sender: T,
        settings: Settings,
    ) -> anyhow::Result<Self> {
        let inner = TransactionTrackerImplInner::new(provider, sender, settings).await?;
        Ok(Self(tokio::sync::Mutex::new(inner)))
    }

    fn inner(
        &self,
    ) -> anyhow::Result<tokio::sync::MutexGuard<'_, TransactionTrackerImplInner<P, T>>> {
        self.0
            .try_lock()
            .context("tracker should not be called while waiting for a transaction")
    }
}

impl<P, T> TransactionTrackerImplInner<P, T>
where
    P: Provider,
    T: TransactionSender,
{
    async fn new(provider: Arc<P>, sender: T, settings: Settings) -> anyhow::Result<Self> {
        let nonce = provider
            .get_transaction_count(sender.address())
            .await
            .unwrap_or(U256::zero());
        Ok(Self {
            provider,
            sender,
            settings,
            nonce,
            transactions: vec![],
            has_dropped: false,
            attempt_count: 0,
        })
    }

    fn get_nonce_and_required_fees(&self) -> (U256, Option<GasFees>) {
        let gas_fees = if self.has_dropped {
            None
        } else {
            self.transactions.last().map(|tx| {
                tx.gas_fees
                    .increase_by_percent(self.settings.replacement_fee_percent_increase)
            })
        };
        (self.nonce, gas_fees)
    }

    async fn send_transaction(
        &mut self,
        tx: TypedTransaction,
        expected_storage: &ExpectedStorage,
    ) -> anyhow::Result<SendResult> {
        self.validate_transaction(&tx)?;
        let gas_fees = GasFees::from(&tx);
        let send_result = self.sender.send_transaction(tx, expected_storage).await;
        let sent_tx = match send_result {
            Ok(sent_tx) => sent_tx,
            Err(error) => {
                let tracker_update = self.handle_send_error(error).await?;
                return Ok(SendResult::TrackerUpdate(tracker_update));
            }
        };
        info!(
            "Sent transaction {:?} nonce: {:?}",
            sent_tx.tx_hash, sent_tx.nonce
        );
        self.transactions.push(PendingTransaction {
            tx_hash: sent_tx.tx_hash,
            gas_fees,
            attempt_number: self.attempt_count,
        });
        self.has_dropped = false;
        self.attempt_count += 1;
        self.update_metrics();
        Ok(SendResult::TxHash(sent_tx.tx_hash))
    }

    /// When we fail to send a transaction, it may be because another
    /// transaction has mined before it could be sent, invalidating the nonce.
    /// Thus, do one last check for an update before returning the error.
    async fn handle_send_error(&mut self, error: TxSenderError) -> anyhow::Result<TrackerUpdate> {
        match &error {
            TxSenderError::ReplacementUnderpriced => {
                return Ok(TrackerUpdate::ReplacementUnderpriced)
            }
            TxSenderError::Other(_error) => {}
        }

        let update = self.check_for_update_now().await?;
        let Some(update) = update else {
            return Err(error.into());
        };
        match &update {
            TrackerUpdate::StillPendingAfterWait | TrackerUpdate::LatestTxDropped { .. } => {
                Err(error.into())
            }
            _ => Ok(update),
        }
    }

    async fn wait_for_update(&mut self) -> anyhow::Result<TrackerUpdate> {
        let start_block_number = self
            .provider
            .get_block_number()
            .await
            .context("tracker should get starting block when waiting for update")?;
        let end_block_number = start_block_number + self.settings.max_blocks_to_wait_for_mine;
        loop {
            let update = self.check_for_update_now().await?;
            if let Some(update) = update {
                return Ok(update);
            }
            let current_block_number = self
                .provider
                .get_block_number()
                .await
                .context("tracker should get current block when polling for updates")?;
            if end_block_number <= current_block_number {
                return Ok(TrackerUpdate::StillPendingAfterWait);
            }
            time::sleep(self.settings.poll_interval).await;
        }
    }

    async fn check_for_update_now(&mut self) -> anyhow::Result<Option<TrackerUpdate>> {
        let external_nonce = self.get_external_nonce().await?;
        if self.nonce < external_nonce {
            // The nonce has changed. Check to see which of our transactions has
            // mined, if any.

            let mut out = TrackerUpdate::NonceUsedForOtherTx { nonce: self.nonce };
            for tx in self.transactions.iter().rev() {
                let status = self
                    .sender
                    .get_transaction_status(tx.tx_hash)
                    .await
                    .context("tracker should check transaction status when the nonce changes")?;
                if let TxStatus::Mined { block_number } = status {
                    let (gas_limit, gas_used) = self.get_mined_tx_gas_info(tx.tx_hash).await?;
                    out = TrackerUpdate::Mined {
                        tx_hash: tx.tx_hash,
                        nonce: self.nonce,
                        block_number,
                        attempt_number: tx.attempt_number,
                        gas_limit,
                        gas_used,
                    };
                    break;
                }
            }
            self.set_nonce_and_clear_state(external_nonce);
            return Ok(Some(out));
        }
        // The nonce has not changed. Check to see if the latest transaction has
        // dropped.
        if self.has_dropped {
            // has_dropped being true means that no new transactions have been
            // added since the last time we checked, hence no update.
            return Ok(None);
        }
        let Some(&last_tx) = self.transactions.last() else {
            // If there are no pending transactions, there's no update either.
            return Ok(None);
        };
        let status = self
            .sender
            .get_transaction_status(last_tx.tx_hash)
            .await
            .context("tracker should check for dropped transactions")?;
        Ok(match status {
            TxStatus::Pending | TxStatus::Dropped => None,
            TxStatus::Mined { block_number } => {
                let nonce = self.nonce;
                self.set_nonce_and_clear_state(nonce + 1);
                let (gas_limit, gas_used) = self.get_mined_tx_gas_info(last_tx.tx_hash).await?;
                Some(TrackerUpdate::Mined {
                    tx_hash: last_tx.tx_hash,
                    nonce,
                    block_number,
                    attempt_number: last_tx.attempt_number,
                    gas_limit,
                    gas_used,
                })
            } // TODO(#295): dropped status is often incorrect, for now just assume its still pending
              // TxStatus::Dropped => {
              //     self.has_dropped = true;
              //     Some(TrackerUpdate::LatestTxDropped { nonce: self.nonce })
              // }
        })
    }

    fn set_nonce_and_clear_state(&mut self, nonce: U256) {
        self.nonce = nonce;
        self.transactions.clear();
        self.has_dropped = false;
        self.attempt_count = 0;
        self.update_metrics();
    }

    async fn get_external_nonce(&self) -> anyhow::Result<U256> {
        self.provider
            .get_transaction_count(self.sender.address())
            .await
            .context("tracker should load current nonce from provider")
    }

    fn validate_transaction(&self, tx: &TypedTransaction) -> anyhow::Result<()> {
        let Some(&nonce) = tx.nonce() else {
            bail!("transaction given to tracker should have nonce set");
        };
        let gas_fees = GasFees::from(tx);
        let (required_nonce, required_gas_fees) = self.get_nonce_and_required_fees();
        if nonce != required_nonce {
            bail!("tried to send transaction with nonce {nonce}, but should match tracker's nonce of {required_nonce}");
        }
        if let Some(required_gas_fees) = required_gas_fees {
            if gas_fees.max_fee_per_gas < required_gas_fees.max_fee_per_gas
                || gas_fees.max_priority_fee_per_gas < required_gas_fees.max_priority_fee_per_gas
            {
                bail!("new transaction's gas fees should be at least the required fees")
            }
        }
        Ok(())
    }

    fn update_metrics(&self) {
        TransactionTrackerMetrics::set_num_pending_transactions(self.transactions.len());
        TransactionTrackerMetrics::set_nonce(self.nonce);
        TransactionTrackerMetrics::set_attempt_count(self.attempt_count);
        if let Some(tx) = self.transactions.last() {
            TransactionTrackerMetrics::set_current_fees(Some(tx.gas_fees));
        } else {
            TransactionTrackerMetrics::set_current_fees(None);
        }
    }

    async fn get_mined_tx_gas_info(
        &self,
        tx_hash: H256,
    ) -> anyhow::Result<(Option<U256>, Option<U256>)> {
        let (tx, tx_receipt) = tokio::try_join!(
            self.provider.get_transaction(tx_hash),
            self.provider.get_transaction_receipt(tx_hash),
        )?;
        let gas_limit = tx.map(|t| t.gas).or_else(|| {
            warn!("failed to fetch transaction data for tx: {}", tx_hash);
            None
        });
        let gas_used = match tx_receipt {
            Some(r) => r.gas_used,
            None => {
                warn!("failed to fetch transaction receipt for tx: {}", tx_hash);
                None
            }
        };
        Ok((gas_limit, gas_used))
    }
}

struct TransactionTrackerMetrics {}

impl TransactionTrackerMetrics {
    fn set_num_pending_transactions(num_pending_transactions: usize) {
        metrics::gauge!(
            "builder_tracker_num_pending_transactions",
            num_pending_transactions as f64
        );
    }

    fn set_nonce(nonce: U256) {
        metrics::gauge!("builder_tracker_nonce", nonce.as_u64() as f64);
    }

    fn set_attempt_count(attempt_count: u64) {
        metrics::gauge!("builder_tracker_attempt_count", attempt_count as f64);
    }

    fn set_current_fees(current_fees: Option<GasFees>) {
        let fees = current_fees.unwrap_or_default();

        metrics::gauge!(
            "builder_tracker_current_max_fee_per_gas",
            fees.max_fee_per_gas.as_u64() as f64
        );
        metrics::gauge!(
            "builder_tracker_current_max_priority_fee_per_gas",
            fees.max_priority_fee_per_gas.as_u64() as f64
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ethers::types::{Address, Eip1559TransactionRequest, Transaction, TransactionReceipt};
    use mockall::Sequence;
    use rundler_provider::MockProvider;

    use super::*;
    use crate::sender::{MockTransactionSender, SentTxInfo};

    fn create_base_config() -> (MockTransactionSender, MockProvider) {
        let sender = MockTransactionSender::new();
        let provider = MockProvider::new();

        (sender, provider)
    }

    async fn create_tracker(
        sender: MockTransactionSender,
        provider: MockProvider,
    ) -> TransactionTrackerImpl<MockProvider, MockTransactionSender> {
        let settings = Settings {
            poll_interval: Duration::from_secs(0),
            max_blocks_to_wait_for_mine: 3,
            replacement_fee_percent_increase: 5,
        };

        let tracker: TransactionTrackerImpl<MockProvider, MockTransactionSender> =
            TransactionTrackerImpl::new(Arc::new(provider), sender, settings)
                .await
                .unwrap();

        tracker
    }

    #[tokio::test]
    async fn test_nonce_and_fees() {
        let (mut sender, mut provider) = create_base_config();
        sender.expect_address().return_const(Address::zero());
        sender.expect_send_transaction().returning(move |_a, _b| {
            Box::pin(async {
                Ok(SentTxInfo {
                    nonce: U256::from(0),
                    tx_hash: H256::zero(),
                })
            })
        });

        provider
            .expect_get_transaction_count()
            .returning(move |_a| Ok(U256::from(0)));

        let tracker = create_tracker(sender, provider).await;

        let tx = Eip1559TransactionRequest::new()
            .nonce(0)
            .gas(10000)
            .max_fee_per_gas(10000);
        let exp = ExpectedStorage::default();

        // send dummy transaction
        let _sent = tracker.send_transaction(tx.into(), &exp).await;
        let nonce_and_fees = tracker.get_nonce_and_required_fees().unwrap();

        assert_eq!(
            (
                U256::from(0),
                Some(GasFees {
                    max_fee_per_gas: U256::from(10500),
                    max_priority_fee_per_gas: U256::zero(),
                })
            ),
            nonce_and_fees
        );
    }

    // TODO(#295): fix dropped status
    // #[tokio::test]
    // async fn test_nonce_and_fees_dropped() {
    //     let (mut sender, mut provider) = create_base_config();
    //     sender.expect_address().return_const(Address::zero());

    //     sender
    //         .expect_get_transaction_status()
    //         .returning(move |_a| Box::pin(async { Ok(TxStatus::Dropped) }));

    //     sender.expect_send_transaction().returning(move |_a, _b| {
    //         Box::pin(async {
    //             Ok(SentTxInfo {
    //                 nonce: U256::from(0),
    //                 tx_hash: H256::zero(),
    //             })
    //         })
    //     });

    //     provider
    //         .expect_get_transaction_count()
    //         .returning(move |_a| Ok(U256::from(0)));

    //     provider
    //         .expect_get_block_number()
    //         .returning(move || Ok(1))
    //         .times(1);

    //     let tracker = create_tracker(sender, provider).await;

    //     let tx = Eip1559TransactionRequest::new()
    //         .nonce(0)
    //         .gas(10000)
    //         .max_fee_per_gas(10000);
    //     let exp = ExpectedStorage::default();

    //     // send dummy transaction
    //     let _sent = tracker.send_transaction(tx.into(), &exp).await;
    //     let _tracker_update = tracker.wait_for_update().await.unwrap();

    //     let nonce_and_fees = tracker.get_nonce_and_required_fees().unwrap();

    //     assert_eq!((U256::from(0), None), nonce_and_fees);
    // }

    #[tokio::test]
    async fn test_send_transaction_without_nonce() {
        let (mut sender, mut provider) = create_base_config();
        sender.expect_address().return_const(Address::zero());
        sender.expect_send_transaction().returning(move |_a, _b| {
            Box::pin(async {
                Ok(SentTxInfo {
                    nonce: U256::from(0),
                    tx_hash: H256::zero(),
                })
            })
        });

        provider
            .expect_get_transaction_count()
            .returning(move |_a| Ok(U256::from(2)));

        let tracker = create_tracker(sender, provider).await;

        let tx = Eip1559TransactionRequest::new();
        let exp = ExpectedStorage::default();
        let sent_transaction = tracker.send_transaction(tx.into(), &exp).await;

        assert!(sent_transaction.is_err());
    }

    #[tokio::test]
    async fn test_send_transaction_with_invalid_nonce() {
        let (mut sender, mut provider) = create_base_config();

        sender.expect_address().return_const(Address::zero());
        sender.expect_send_transaction().returning(move |_a, _b| {
            Box::pin(async {
                Ok(SentTxInfo {
                    nonce: U256::from(0),
                    tx_hash: H256::zero(),
                })
            })
        });

        provider
            .expect_get_transaction_count()
            .returning(move |_a| Ok(U256::from(2)));

        let tracker = create_tracker(sender, provider).await;

        let tx = Eip1559TransactionRequest::new().nonce(0);
        let exp = ExpectedStorage::default();
        let sent_transaction = tracker.send_transaction(tx.into(), &exp).await;

        assert!(sent_transaction.is_err());
    }

    #[tokio::test]
    async fn test_send_transaction() {
        let (mut sender, mut provider) = create_base_config();
        sender.expect_address().return_const(Address::zero());
        sender.expect_send_transaction().returning(move |_a, _b| {
            Box::pin(async {
                Ok(SentTxInfo {
                    nonce: U256::from(0),
                    tx_hash: H256::zero(),
                })
            })
        });

        provider
            .expect_get_transaction_count()
            .returning(move |_a| Ok(U256::from(0)));

        let tracker = create_tracker(sender, provider).await;

        let tx = Eip1559TransactionRequest::new().nonce(0);
        let exp = ExpectedStorage::default();
        let sent_transaction = tracker.send_transaction(tx.into(), &exp).await.unwrap();

        assert!(matches!(sent_transaction, SendResult::TxHash(..)));
    }

    #[tokio::test]
    async fn test_wait_for_update_still_pending() {
        let (mut sender, mut provider) = create_base_config();
        sender.expect_address().return_const(Address::zero());

        let mut s = Sequence::new();

        provider
            .expect_get_transaction_count()
            .returning(move |_a| Ok(U256::from(0)));

        for block_number in 1..=4 {
            provider
                .expect_get_block_number()
                .returning(move || Ok(block_number))
                .times(1)
                .in_sequence(&mut s);
        }

        let tracker = create_tracker(sender, provider).await;
        let tracker_update = tracker.wait_for_update().await.unwrap();

        assert!(matches!(
            tracker_update,
            TrackerUpdate::StillPendingAfterWait
        ));
    }

    // TODO(#295): fix dropped status
    // #[tokio::test]
    // async fn test_wait_for_update_dropped() {
    //     let (mut sender, mut provider) = create_base_config();
    //     sender.expect_address().return_const(Address::zero());

    //     sender
    //         .expect_get_transaction_status()
    //         .returning(move |_a| Box::pin(async { Ok(TxStatus::Dropped) }));

    //     sender.expect_send_transaction().returning(move |_a, _b| {
    //         Box::pin(async {
    //             Ok(SentTxInfo {
    //                 nonce: U256::from(0),
    //                 tx_hash: H256::zero(),
    //             })
    //         })
    //     });

    //     provider
    //         .expect_get_transaction_count()
    //         .returning(move |_a| Ok(U256::from(0)));

    //     provider.expect_get_block_number().returning(move || Ok(1));

    //     let tracker = create_tracker(sender, provider).await;

    //     let tx = Eip1559TransactionRequest::new().nonce(0);
    //     let exp = ExpectedStorage::default();
    //     let _sent_transaction = tracker.send_transaction(tx.into(), &exp).await.unwrap();
    //     let tracker_update = tracker.wait_for_update().await.unwrap();

    //     assert!(matches!(
    //         tracker_update,
    //         TrackerUpdate::LatestTxDropped { .. }
    //     ));
    // }

    #[tokio::test]
    async fn test_wait_for_update_nonce_used() {
        let (mut sender, mut provider) = create_base_config();
        sender.expect_address().return_const(Address::zero());

        let mut provider_seq = Sequence::new();
        for transaction_count in 0..=1 {
            provider
                .expect_get_transaction_count()
                .returning(move |_a| Ok(U256::from(transaction_count)))
                .times(1)
                .in_sequence(&mut provider_seq);
        }

        provider
            .expect_get_block_number()
            .returning(move || Ok(1))
            .times(1);

        let tracker = create_tracker(sender, provider).await;

        let tracker_update = tracker.wait_for_update().await.unwrap();

        assert!(matches!(
            tracker_update,
            TrackerUpdate::NonceUsedForOtherTx { .. }
        ));
    }

    #[tokio::test]
    async fn test_wait_for_update_mined() {
        let (mut sender, mut provider) = create_base_config();
        sender.expect_address().return_const(Address::zero());
        sender
            .expect_get_transaction_status()
            .returning(move |_a| Box::pin(async { Ok(TxStatus::Mined { block_number: 1 }) }));

        sender.expect_send_transaction().returning(move |_a, _b| {
            Box::pin(async {
                Ok(SentTxInfo {
                    nonce: U256::from(0),
                    tx_hash: H256::zero(),
                })
            })
        });

        provider
            .expect_get_transaction_count()
            .returning(move |_a| Ok(U256::from(0)));

        provider
            .expect_get_block_number()
            .returning(move || Ok(1))
            .times(1);

        provider.expect_get_transaction().returning(|_: H256| {
            Ok(Some(Transaction {
                gas: U256::from(0),
                ..Default::default()
            }))
        });

        provider
            .expect_get_transaction_receipt()
            .returning(|_: H256| {
                Ok(Some(TransactionReceipt {
                    gas_used: Some(U256::from(0)),
                    ..Default::default()
                }))
            });

        let tracker = create_tracker(sender, provider).await;

        let tx = Eip1559TransactionRequest::new().nonce(0);
        let exp = ExpectedStorage::default();

        // send dummy transaction
        let _sent = tracker.send_transaction(tx.into(), &exp).await;
        let tracker_update = tracker.wait_for_update().await.unwrap();

        assert!(matches!(tracker_update, TrackerUpdate::Mined { .. }));
    }
}
