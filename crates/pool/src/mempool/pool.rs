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

use std::{
    cmp::Ordering,
    collections::{hash_map::Entry, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use anyhow::Context;
use ethers::{
    abi::Address,
    types::{H256, U256},
};
use rundler_types::{Entity, EntityType, Timestamp, UserOperation, UserOperationId};
use rundler_utils::math;
use tracing::info;

use super::{
    entity_tracker::EntityCounter,
    error::{MempoolError, MempoolResult},
    paymaster::PaymasterTracker,
    size::SizeTracker,
    PaymasterMetadata, PoolConfig, PoolOperation,
};
use crate::chain::{BalanceUpdate, MinedOp};

#[derive(Debug, Clone)]
pub(crate) struct PoolInnerConfig {
    entry_point: Address,
    chain_id: u64,
    max_size_of_pool_bytes: usize,
    min_replacement_fee_increase_percentage: u64,
    throttled_entity_mempool_count: u64,
    throttled_entity_live_blocks: u64,
    paymaster_tracking_enabled: bool,
}

impl From<PoolConfig> for PoolInnerConfig {
    fn from(config: PoolConfig) -> Self {
        Self {
            entry_point: config.entry_point,
            chain_id: config.chain_id,
            max_size_of_pool_bytes: config.max_size_of_pool_bytes,
            min_replacement_fee_increase_percentage: config.min_replacement_fee_increase_percentage,
            throttled_entity_mempool_count: config.throttled_entity_mempool_count,
            throttled_entity_live_blocks: config.throttled_entity_live_blocks,
            paymaster_tracking_enabled: config.paymaster_tracking_enabled,
        }
    }
}

/// Pool of user operations
#[derive(Debug)]
pub(crate) struct PoolInner {
    /// Pool settings
    config: PoolInnerConfig,
    /// Operations by hash
    by_hash: HashMap<H256, OrderedPoolOperation>,
    /// Operations by operation ID
    by_id: HashMap<UserOperationId, OrderedPoolOperation>,
    /// Best operations, sorted by gas price
    best: BTreeSet<OrderedPoolOperation>,
    /// Removed operations, temporarily kept around in case their blocks are
    /// reorged away. Stored along with the block number at which it was
    /// removed.
    mined_at_block_number_by_hash: HashMap<H256, (OrderedPoolOperation, u64)>,
    /// Removed operation hashes sorted by block number, so we can forget them
    /// when enough new blocks have passed.
    mined_hashes_with_block_numbers: BTreeSet<(u64, H256)>,
    /// Count of operations by entity address
    count_by_address: HashMap<Address, EntityCounter>,
    /// Submission ID counter
    submission_id: u64,
    /// A field that keeps track of paymaster balances across the mempool
    paymaster_balances: PaymasterTracker,
    /// keeps track of the size of the pool in bytes
    pool_size: SizeTracker,
    /// keeps track of the size of the removed cache in bytes
    cache_size: SizeTracker,
}

impl PoolInner {
    pub(crate) fn new(config: PoolInnerConfig) -> Self {
        Self {
            paymaster_balances: PaymasterTracker::new(config.paymaster_tracking_enabled),
            config,
            by_hash: HashMap::new(),
            by_id: HashMap::new(),
            best: BTreeSet::new(),
            mined_at_block_number_by_hash: HashMap::new(),
            mined_hashes_with_block_numbers: BTreeSet::new(),
            count_by_address: HashMap::new(),
            submission_id: 0,
            pool_size: SizeTracker::default(),
            cache_size: SizeTracker::default(),
        }
    }

    /// Returns hash of operation to replace if operation is a replacement
    pub(crate) fn check_replacement(&self, op: &UserOperation) -> MempoolResult<Option<H256>> {
        // Check if operation already known
        if self
            .by_hash
            .contains_key(&op.op_hash(self.config.entry_point, self.config.chain_id))
        {
            return Err(MempoolError::OperationAlreadyKnown);
        }

        if let Some(pool_op) = self.by_id.get(&op.id()) {
            let (replacement_priority_fee, replacement_fee) =
                self.get_min_replacement_fees(pool_op.uo());

            if op.max_priority_fee_per_gas < replacement_priority_fee
                || op.max_fee_per_gas < replacement_fee
            {
                return Err(MempoolError::ReplacementUnderpriced(
                    pool_op.uo().max_priority_fee_per_gas,
                    pool_op.uo().max_fee_per_gas,
                ));
            }

            Ok(Some(
                pool_op
                    .uo()
                    .op_hash(self.config.entry_point, self.config.chain_id),
            ))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn add_operation(
        &mut self,
        op: PoolOperation,
        paymaster_meta: Option<PaymasterMetadata>,
    ) -> MempoolResult<H256> {
        let ret = self.add_operation_internal(Arc::new(op), None, paymaster_meta);
        self.update_metrics();
        ret
    }

    pub(crate) fn paymaster_addresses(&self) -> Vec<Address> {
        self.paymaster_balances.paymaster_addresses()
    }

    pub(crate) fn set_confirmed_paymaster_balances(
        &mut self,
        addresses: &[Address],
        balances: &[U256],
    ) {
        self.paymaster_balances
            .set_confimed_balances(addresses, balances);
    }

    pub(crate) fn best_operations(&self) -> impl Iterator<Item = Arc<PoolOperation>> {
        self.best.clone().into_iter().map(|v| v.po)
    }

    /// Removes all operations using the given entity, returning the hashes of the removed operations.
    ///
    /// NOTE: This method is O(n) where n is the number of operations in the pool.
    /// It should be called sparingly (e.g. when a block is mined).
    pub(crate) fn remove_expired(&mut self, expire_before: Timestamp) -> Vec<(H256, Timestamp)> {
        let mut expired = Vec::new();
        for (hash, op) in &self.by_hash {
            if op.po.valid_time_range.valid_until < expire_before {
                expired.push((*hash, op.po.valid_time_range.valid_until));
            }
        }

        for (hash, _) in &expired {
            self.remove_operation_by_hash(*hash);
        }

        expired
    }

    pub(crate) fn address_count(&self, address: &Address) -> usize {
        if let Some(entity) = self.count_by_address.get(address) {
            return entity.total();
        };

        0
    }

    pub(crate) fn get_operation_by_hash(&self, hash: H256) -> Option<Arc<PoolOperation>> {
        self.by_hash.get(&hash).map(|o| o.po.clone())
    }

    pub(crate) fn remove_operation_by_hash(&mut self, hash: H256) -> Option<Arc<PoolOperation>> {
        let ret = self.remove_operation_internal(hash, None);
        self.update_metrics();
        ret
    }

    // STO-040
    pub(crate) fn check_multiple_roles_violation(&self, uo: &UserOperation) -> MempoolResult<()> {
        if let Some(ec) = self.count_by_address.get(&uo.sender) {
            if ec.includes_non_sender() {
                return Err(MempoolError::SenderAddressUsedAsAlternateEntity(uo.sender));
            }
        }

        for e in uo.entities() {
            match e.kind {
                EntityType::Factory | EntityType::Paymaster | EntityType::Aggregator => {
                    if let Some(ec) = self.count_by_address.get(&e.address) {
                        if ec.sender().gt(&0) {
                            return Err(MempoolError::MultipleRolesViolation(e));
                        }
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    // STO-041
    pub(crate) fn check_associated_storage(
        &self,
        accessed_storage: &HashSet<Address>,
        uo: &UserOperation,
    ) -> MempoolResult<()> {
        for storage_address in accessed_storage {
            if let Some(ec) = self.count_by_address.get(storage_address) {
                if ec.sender().gt(&0) && storage_address.ne(&uo.sender) {
                    // Reject UO if the sender is also an entity in another UO in the mempool
                    for entity in uo.entities() {
                        if storage_address.eq(&entity.address) {
                            return Err(MempoolError::AssociatedStorageIsAlternateSender);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn mine_operation(
        &mut self,
        mined_op: &MinedOp,
        block_number: u64,
    ) -> Option<Arc<PoolOperation>> {
        let tx_in_pool = self.by_id.get(&mined_op.id())?;

        let hash = tx_in_pool
            .uo()
            .op_hash(mined_op.entry_point, self.config.chain_id);

        self.paymaster_balances
            .update_paymaster_balance_from_mined_op(mined_op);

        let ret = self.remove_operation_internal(hash, Some(block_number));

        self.update_metrics();
        ret
    }

    pub(crate) fn unmine_operation(&mut self, mined_op: &MinedOp) -> Option<Arc<PoolOperation>> {
        let hash = mined_op.hash;
        let (op, block_number) = self.mined_at_block_number_by_hash.remove(&hash)?;
        self.mined_hashes_with_block_numbers
            .remove(&(block_number, hash));

        if let Err(error) = self.put_back_unmined_operation(op.clone(), mined_op) {
            info!("Could not put back unmined operation: {error}");
        };
        self.update_metrics();
        Some(op.po)
    }

    /// Remove all but THROTTLED_ENTITY_MEMPOOL_COUNT operations that are within THROTTLED_ENTITY_LIVE_BLOCKS of head
    /// using the given entity, returning the hashes of the removed operations.
    pub(crate) fn throttle_entity(
        &mut self,
        entity: Entity,
        current_block_number: u64,
    ) -> Vec<H256> {
        let mut uos_kept = self.config.throttled_entity_mempool_count;
        let to_remove = self
            .best
            .iter()
            .filter(|o| {
                // We want to remove ops that use the throttled entity and are older than THROTTLED_ENTITY_LIVE_BLOCKS behind head, or if we already have kept THROTTLED_ENTITY_MEMPOOL_COUNT ops
                if o.po.contains_entity(&entity) {
                    if o.po.sim_block_number + self.config.throttled_entity_live_blocks
                        < current_block_number
                        || uos_kept == 0
                    {
                        return true;
                    }
                    uos_kept = uos_kept.saturating_sub(1);
                }
                false
            })
            .map(|o| {
                o.po.uo
                    .op_hash(self.config.entry_point, self.config.chain_id)
            })
            .collect::<Vec<_>>();
        for &hash in &to_remove {
            self.remove_operation_internal(hash, None);
        }
        self.update_metrics();
        to_remove
    }

    /// Removes all operations using the given entity, returning the hashes of
    /// the removed operations.
    pub(crate) fn remove_entity(&mut self, entity: Entity) -> Vec<H256> {
        let to_remove = self
            .by_hash
            .iter()
            .filter(|(_, uo)| uo.po.contains_entity(&entity))
            .map(|(hash, _)| *hash)
            .collect::<Vec<_>>();
        for &hash in &to_remove {
            self.remove_operation_internal(hash, None);
        }
        self.update_metrics();
        to_remove
    }

    pub(crate) fn forget_mined_operations_before_block(&mut self, block_number: u64) {
        while let Some(&(bn, hash)) = self
            .mined_hashes_with_block_numbers
            .first()
            .filter(|(bn, _)| *bn < block_number)
        {
            if let Some((op, _)) = self.mined_at_block_number_by_hash.remove(&hash) {
                self.cache_size -= op.mem_size();
            }
            self.mined_hashes_with_block_numbers.remove(&(bn, hash));
        }
        self.update_metrics();
    }

    pub(crate) fn paymaster_metadata(&self, paymaster: Address) -> Option<PaymasterMetadata> {
        self.paymaster_balances.paymaster_metadata(paymaster)
    }

    pub(crate) fn paymaster_exists(&self, paymaster: Address) -> bool {
        self.paymaster_balances.paymaster_exists(paymaster)
    }

    pub(crate) fn update_paymaster_balances_after_update<'a>(
        &mut self,
        entity_balance_updates: impl Iterator<Item = &'a BalanceUpdate>,
        unmined_entity_balance_updates: impl Iterator<Item = &'a BalanceUpdate>,
    ) {
        for balance_update in entity_balance_updates {
            self.paymaster_balances.update_paymaster_balance_from_event(
                balance_update.address,
                balance_update.amount,
                balance_update.is_addition,
            )
        }

        for unmined_balance_update in unmined_entity_balance_updates {
            self.paymaster_balances.update_paymaster_balance_from_event(
                unmined_balance_update.address,
                unmined_balance_update.amount,
                !unmined_balance_update.is_addition,
            )
        }
    }

    pub(crate) fn clear(&mut self, clear_mempool: bool, clear_paymaster: bool) {
        if clear_mempool {
            self.by_hash.clear();
            self.by_id.clear();
            self.best.clear();
            self.mined_at_block_number_by_hash.clear();
            self.mined_hashes_with_block_numbers.clear();
            self.count_by_address.clear();
            self.pool_size = SizeTracker::default();
            self.cache_size = SizeTracker::default();
            self.update_metrics();
        }

        if clear_paymaster {
            self.paymaster_balances.clear();
        }
    }

    pub(crate) fn set_tracking(&mut self, paymaster: bool) {
        self.paymaster_balances.set_paymaster_tracker(paymaster);
    }

    fn enforce_size(&mut self) -> anyhow::Result<Vec<H256>> {
        let mut removed = Vec::new();

        while self.pool_size > self.config.max_size_of_pool_bytes {
            if let Some(worst) = self.best.pop_last() {
                let hash = worst
                    .uo()
                    .op_hash(self.config.entry_point, self.config.chain_id);

                let _ = self
                    .remove_operation_internal(hash, None)
                    .context("should have removed the worst operation")?;

                removed.push(hash);
            }
        }

        Ok(removed)
    }

    fn put_back_unmined_operation(
        &mut self,
        op: OrderedPoolOperation,
        mined_op: &MinedOp,
    ) -> MempoolResult<H256> {
        let mut paymaster_meta = None;
        if let Some(paymaster) = op.uo().paymaster() {
            self.paymaster_balances
                .unmine_actual_cost(&paymaster, mined_op.actual_gas_cost);

            paymaster_meta = self.paymaster_metadata(paymaster);
        }

        self.add_operation_internal(op.po, Some(op.submission_id), paymaster_meta)
    }

    fn add_operation_internal(
        &mut self,
        op: Arc<PoolOperation>,
        submission_id: Option<u64>,
        paymaster_meta: Option<PaymasterMetadata>,
    ) -> MempoolResult<H256> {
        // Check if operation already known or replacing an existing operation
        // if replacing, remove the existing operation
        if let Some(hash) = self.check_replacement(&op.uo)? {
            self.remove_operation_by_hash(hash);
        }

        // check or update paymaster balance
        if let Some(paymaster_meta) = paymaster_meta {
            self.paymaster_balances
                .add_or_update_balance(&op, &paymaster_meta)?;
        }

        let pool_op = OrderedPoolOperation {
            po: op,
            submission_id: submission_id.unwrap_or_else(|| self.next_submission_id()),
        };

        // update counts
        for e in pool_op.po.entities() {
            self.count_by_address
                .entry(e.address)
                .or_default()
                .increment_entity_count(&e.kind);
        }

        // create and insert ordered operation
        let hash = pool_op
            .uo()
            .op_hash(self.config.entry_point, self.config.chain_id);
        self.pool_size += pool_op.mem_size();
        self.by_hash.insert(hash, pool_op.clone());
        self.by_id.insert(pool_op.uo().id(), pool_op.clone());
        self.best.insert(pool_op);

        // TODO(danc): This silently drops UOs from the pool without reporting
        let removed = self
            .enforce_size()
            .context("should have succeeded in resizing the pool")?;

        if removed.contains(&hash) {
            Err(MempoolError::DiscardedOnInsert)?;
        }

        Ok(hash)
    }

    fn remove_operation_internal(
        &mut self,
        hash: H256,
        block_number: Option<u64>,
    ) -> Option<Arc<PoolOperation>> {
        let op = self.by_hash.remove(&hash)?;
        let id = &op.po.uo.id();
        self.by_id.remove(id);
        self.best.remove(&op);
        self.paymaster_balances.remove_operation(id);

        if let Some(block_number) = block_number {
            self.cache_size += op.mem_size();
            self.mined_at_block_number_by_hash
                .insert(hash, (op.clone(), block_number));
            self.mined_hashes_with_block_numbers
                .insert((block_number, hash));
        }

        for e in op.po.entities() {
            self.decrement_address_count(e.address, &e.kind);
        }

        self.pool_size -= op.mem_size();
        Some(op.po)
    }

    fn decrement_address_count(&mut self, address: Address, entity: &EntityType) {
        if let Entry::Occupied(mut count_entry) = self.count_by_address.entry(address) {
            count_entry.get_mut().decrement_entity_count(entity);
            if count_entry.get().total() == 0 {
                count_entry.remove_entry();
            }
        }
    }

    fn next_submission_id(&mut self) -> u64 {
        let id = self.submission_id;
        self.submission_id += 1;
        id
    }

    fn get_min_replacement_fees(&self, op: &UserOperation) -> (U256, U256) {
        let replacement_priority_fee = math::increase_by_percent(
            op.max_priority_fee_per_gas,
            self.config.min_replacement_fee_increase_percentage,
        );
        let replacement_fee = math::increase_by_percent(
            op.max_fee_per_gas,
            self.config.min_replacement_fee_increase_percentage,
        );
        (replacement_priority_fee, replacement_fee)
    }

    fn update_metrics(&self) {
        PoolMetrics::set_pool_metrics(
            self.by_hash.len(),
            self.pool_size.0,
            self.config.entry_point,
        );
        PoolMetrics::set_cache_metrics(
            self.mined_hashes_with_block_numbers.len(),
            self.cache_size.0,
            self.config.entry_point,
        );
    }
}

/// Wrapper around PoolOperation that adds a submission ID to implement
/// a custom ordering for the best operations
#[derive(Debug, Clone)]
struct OrderedPoolOperation {
    po: Arc<PoolOperation>,
    submission_id: u64,
}

impl OrderedPoolOperation {
    fn uo(&self) -> &UserOperation {
        &self.po.uo
    }

    fn mem_size(&self) -> usize {
        std::mem::size_of::<OrderedPoolOperation>() + self.po.mem_size()
    }
}

impl Eq for OrderedPoolOperation {}

impl Ord for OrderedPoolOperation {
    fn cmp(&self, other: &Self) -> Ordering {
        // Sort by gas price descending then by id ascending
        other
            .uo()
            .max_fee_per_gas
            .cmp(&self.uo().max_fee_per_gas)
            .then_with(|| self.submission_id.cmp(&other.submission_id))
    }
}

impl PartialOrd for OrderedPoolOperation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for OrderedPoolOperation {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

struct PoolMetrics {}

impl PoolMetrics {
    fn set_pool_metrics(num_ops: usize, size_bytes: isize, entry_point: Address) {
        metrics::gauge!("op_pool_num_ops_in_pool", num_ops as f64, "entrypoint_addr" => entry_point.to_string());
        metrics::gauge!("op_pool_size_bytes", size_bytes as f64, "entrypoint_addr" => entry_point.to_string());
    }
    fn set_cache_metrics(num_ops: usize, size_bytes: isize, entry_point: Address) {
        metrics::gauge!("op_pool_num_ops_in_cache", num_ops as f64, "entrypoint_addr" => entry_point.to_string());
        metrics::gauge!("op_pool_cache_size_bytes", size_bytes as f64, "entrypoint_addr" => entry_point.to_string());
    }
}

#[cfg(test)]
mod tests {
    use rundler_sim::{EntityInfo, EntityInfos};

    use super::*;

    #[test]
    fn add_single_op() {
        let mut pool = PoolInner::new(conf());
        let op = create_op(Address::random(), 0, 1);
        let hash = pool.add_operation(op.clone(), None).unwrap();

        check_map_entry(pool.by_hash.get(&hash), Some(&op));
        check_map_entry(pool.by_id.get(&op.uo.id()), Some(&op));
        check_map_entry(pool.best.iter().next(), Some(&op));
    }

    #[test]
    fn add_multiple_ops() {
        let mut pool = PoolInner::new(conf());
        let ops = vec![
            create_op(Address::random(), 0, 1),
            create_op(Address::random(), 0, 2),
            create_op(Address::random(), 0, 3),
        ];

        let mut hashes = vec![];
        for op in ops.iter() {
            hashes.push(pool.add_operation(op.clone(), None).unwrap());
        }

        for (hash, op) in hashes.iter().zip(&ops) {
            check_map_entry(pool.by_hash.get(hash), Some(op));
            check_map_entry(pool.by_id.get(&op.uo.id()), Some(op));
        }

        // best should be sorted by gas
        assert_eq!(pool.best.len(), 3);
        check_map_entry(pool.best.iter().next(), Some(&ops[2]));
        check_map_entry(pool.best.iter().nth(1), Some(&ops[1]));
        check_map_entry(pool.best.iter().nth(2), Some(&ops[0]));
    }

    #[test]
    fn best_ties() {
        let mut pool = PoolInner::new(conf());
        let ops = vec![
            create_op(Address::random(), 0, 1),
            create_op(Address::random(), 0, 1),
            create_op(Address::random(), 0, 1),
        ];

        let mut hashes = vec![];
        for op in ops.iter() {
            hashes.push(pool.add_operation(op.clone(), None).unwrap());
        }

        // best should be sorted by gas, then by submission id
        assert_eq!(pool.best.len(), 3);
        check_map_entry(pool.best.iter().next(), Some(&ops[0]));
        check_map_entry(pool.best.iter().nth(1), Some(&ops[1]));
        check_map_entry(pool.best.iter().nth(2), Some(&ops[2]));
    }

    #[test]
    fn remove_op() {
        let mut pool = PoolInner::new(conf());
        let ops = vec![
            create_op(Address::random(), 0, 3),
            create_op(Address::random(), 0, 2),
            create_op(Address::random(), 0, 1),
        ];

        let mut hashes = vec![];
        for op in ops.iter() {
            hashes.push(pool.add_operation(op.clone(), None).unwrap());
        }

        assert!(pool.remove_operation_by_hash(hashes[0]).is_some());
        check_map_entry(pool.by_hash.get(&hashes[0]), None);
        check_map_entry(pool.best.iter().next(), Some(&ops[1]));

        assert!(pool.remove_operation_by_hash(hashes[1]).is_some());
        check_map_entry(pool.by_hash.get(&hashes[1]), None);
        check_map_entry(pool.best.iter().next(), Some(&ops[2]));

        assert!(pool.remove_operation_by_hash(hashes[2]).is_some());
        check_map_entry(pool.by_hash.get(&hashes[2]), None);
        check_map_entry(pool.best.iter().next(), None);

        assert!(pool.remove_operation_by_hash(hashes[0]).is_none());
        assert!(pool.remove_operation_by_hash(hashes[1]).is_none());
        assert!(pool.remove_operation_by_hash(hashes[2]).is_none());
    }

    #[test]
    fn remove_account() {
        let mut pool = PoolInner::new(conf());
        let account = Address::random();
        let ops = vec![
            create_op(account, 0, 3),
            create_op(account, 1, 2),
            create_op(account, 2, 1),
        ];
        for mut op in ops.into_iter() {
            op.aggregator = Some(account);
            pool.add_operation(op.clone(), None).unwrap();
        }
        assert_eq!(pool.by_hash.len(), 3);

        pool.remove_entity(Entity::account(account));
        assert!(pool.by_hash.is_empty());
        assert!(pool.by_id.is_empty());
        assert!(pool.best.is_empty());
    }

    #[test]
    fn mine_op() {
        let mut pool = PoolInner::new(conf());
        let sender = Address::random();
        let nonce = 0;

        let op = create_op(sender, nonce, 1);

        let hash = op.uo.op_hash(pool.config.entry_point, pool.config.chain_id);

        pool.add_operation(op, None).unwrap();

        let mined_op = MinedOp {
            paymaster: None,
            actual_gas_cost: U256::zero(),
            hash,
            entry_point: pool.config.entry_point,
            sender,
            nonce: U256::from(nonce),
        };

        pool.mine_operation(&mined_op, 1);

        assert!(pool.by_hash.is_empty());
        assert!(pool.by_id.is_empty());
        assert!(pool.best.is_empty());
    }

    #[test]
    fn mine_op_with_replacement() {
        let mut pool = PoolInner::new(conf());
        let sender = Address::random();
        let nonce = 0;

        let op = create_op(sender, nonce, 1);
        let op_2 = create_op(sender, nonce, 2);

        let hash = op_2
            .uo
            .op_hash(pool.config.entry_point, pool.config.chain_id);

        pool.add_operation(op, None).unwrap();
        pool.add_operation(op_2, None).unwrap();

        let mined_op = MinedOp {
            paymaster: None,
            actual_gas_cost: U256::zero(),
            hash,
            entry_point: pool.config.entry_point,
            sender,
            nonce: U256::from(nonce),
        };

        pool.mine_operation(&mined_op, 1);

        assert!(pool.by_hash.is_empty());
        assert!(pool.by_id.is_empty());
        assert!(pool.best.is_empty());
    }

    #[test]
    fn remove_aggregator() {
        let mut pool = PoolInner::new(conf());
        let agg = Address::random();
        let ops = vec![
            create_op(Address::random(), 0, 3),
            create_op(Address::random(), 0, 2),
            create_op(Address::random(), 0, 1),
        ];
        for mut op in ops.into_iter() {
            op.aggregator = Some(agg);
            op.entity_infos.aggregator = Some(EntityInfo {
                address: agg,
                is_staked: false,
            });
            pool.add_operation(op.clone(), None).unwrap();
        }
        assert_eq!(pool.by_hash.len(), 3);

        pool.remove_entity(Entity::aggregator(agg));
        assert!(pool.by_hash.is_empty());
        assert!(pool.by_id.is_empty());
        assert!(pool.best.is_empty());
    }

    #[test]
    fn remove_paymaster() {
        let mut pool = PoolInner::new(conf());
        let paymaster = Address::random();
        let ops = vec![
            create_op(Address::random(), 0, 3),
            create_op(Address::random(), 0, 2),
            create_op(Address::random(), 0, 1),
        ];
        for mut op in ops.into_iter() {
            op.uo.paymaster_and_data = paymaster.as_bytes().to_vec().into();
            op.entity_infos.paymaster = Some(EntityInfo {
                address: op.uo.paymaster().unwrap(),
                is_staked: false,
            });
            pool.add_operation(op.clone(), None).unwrap();
        }
        assert_eq!(pool.by_hash.len(), 3);

        pool.remove_entity(Entity::paymaster(paymaster));
        assert!(pool.by_hash.is_empty());
        assert!(pool.by_id.is_empty());
        assert!(pool.best.is_empty());
    }

    #[test]
    fn address_count() {
        let mut pool = PoolInner::new(conf());
        let sender = Address::random();
        let paymaster = Address::random();
        let factory = Address::random();
        let aggregator = Address::random();

        let mut op = create_op(sender, 0, 1);
        op.uo.paymaster_and_data = paymaster.as_bytes().to_vec().into();
        op.entity_infos.paymaster = Some(EntityInfo {
            address: op.uo.paymaster().unwrap(),
            is_staked: false,
        });
        op.uo.init_code = factory.as_bytes().to_vec().into();
        op.entity_infos.factory = Some(EntityInfo {
            address: op.uo.factory().unwrap(),
            is_staked: false,
        });
        op.aggregator = Some(aggregator);
        op.entity_infos.aggregator = Some(EntityInfo {
            address: aggregator,
            is_staked: false,
        });

        let count = 5;
        let mut hashes = vec![];
        for i in 0..count {
            let mut op = op.clone();
            op.uo.nonce = i.into();
            hashes.push(pool.add_operation(op, None).unwrap());
        }

        assert_eq!(pool.address_count(&sender), 5);
        assert_eq!(pool.address_count(&paymaster), 5);
        assert_eq!(pool.address_count(&factory), 5);
        assert_eq!(pool.address_count(&aggregator), 5);

        for hash in hashes.iter() {
            assert!(pool.remove_operation_by_hash(*hash).is_some());
        }

        assert_eq!(pool.address_count(&sender), 0);
        assert_eq!(pool.address_count(&paymaster), 0);
        assert_eq!(pool.address_count(&factory), 0);
        assert_eq!(pool.address_count(&aggregator), 0);
    }

    #[test]
    fn pool_full_new_replaces_worst() {
        let args = conf();
        let mut pool = PoolInner::new(args.clone());
        for i in 0..20 {
            let op = create_op(Address::random(), i, i + 1);
            pool.add_operation(op, None).unwrap();
        }

        // on greater gas, new op should win
        let op = create_op(Address::random(), args.max_size_of_pool_bytes, 2);
        let result = pool.add_operation(op, None);
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn pool_full_worst_remains() {
        let args = conf();
        let mut pool = PoolInner::new(args.clone());
        for i in 0..20 {
            let op = create_op(Address::random(), i, i + 1);
            pool.add_operation(op, None).unwrap();
        }

        let op = create_op(Address::random(), 4, 1);
        assert!(pool.add_operation(op, None).is_err());

        // on equal gas, worst should remain because it came first
        let op = create_op(Address::random(), 4, 2);
        let result = pool.add_operation(op, None);
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn replace_op_underpriced() {
        let mut pool = PoolInner::new(conf());
        let sender = Address::random();
        let mut po1 = create_op(sender, 0, 100);
        po1.uo.max_priority_fee_per_gas = 100.into();
        let _ = pool.add_operation(po1.clone(), None).unwrap();

        let mut po2 = create_op(sender, 0, 101);
        po2.uo.max_priority_fee_per_gas = 101.into();
        let res = pool.add_operation(po2, None);
        assert!(res.is_err());
        match res.err().unwrap() {
            MempoolError::ReplacementUnderpriced(a, b) => {
                assert_eq!(a, 100.into());
                assert_eq!(b, 100.into());
            }
            _ => panic!("wrong error"),
        }

        assert_eq!(pool.address_count(&sender), 1);
        assert_eq!(
            pool.pool_size,
            OrderedPoolOperation {
                po: Arc::new(po1),
                submission_id: 0
            }
            .mem_size()
        );
    }

    #[test]
    fn replace_op() {
        let mut pool = PoolInner::new(conf());
        let sender = Address::random();
        let paymaster1 = Address::random();
        let mut po1 = create_op(sender, 0, 10);
        po1.uo.max_priority_fee_per_gas = 10.into();
        po1.uo.paymaster_and_data = paymaster1.as_bytes().to_vec().into();
        po1.entity_infos.paymaster = Some(EntityInfo {
            address: po1.uo.paymaster().unwrap(),
            is_staked: false,
        });
        let _ = pool.add_operation(po1, None).unwrap();
        assert_eq!(pool.address_count(&paymaster1), 1);

        let paymaster2 = Address::random();
        let mut po2 = create_op(sender, 0, 11);
        po2.uo.max_priority_fee_per_gas = 11.into();
        po2.uo.paymaster_and_data = paymaster2.as_bytes().to_vec().into();
        po2.entity_infos.paymaster = Some(EntityInfo {
            address: po2.uo.paymaster().unwrap(),
            is_staked: false,
        });
        let _ = pool.add_operation(po2.clone(), None).unwrap();

        assert_eq!(pool.address_count(&sender), 1);
        assert_eq!(pool.address_count(&paymaster1), 0);
        assert_eq!(pool.address_count(&paymaster2), 1);
        assert_eq!(
            pool.pool_size,
            OrderedPoolOperation {
                po: Arc::new(po2),
                submission_id: 0
            }
            .mem_size()
        );
    }

    #[test]
    fn test_already_known() {
        let mut pool = PoolInner::new(conf());
        let sender = Address::random();
        let mut po1 = create_op(sender, 0, 10);
        po1.uo.max_priority_fee_per_gas = 10.into();
        let _ = pool.add_operation(po1.clone(), None).unwrap();

        let res = pool.add_operation(po1, None);
        assert!(res.is_err());
        match res.err().unwrap() {
            MempoolError::OperationAlreadyKnown => (),
            _ => panic!("wrong error"),
        }
    }

    #[test]
    fn test_expired() {
        let conf = conf();
        let mut pool = PoolInner::new(conf.clone());
        let sender = Address::random();
        let mut po1 = create_op(sender, 0, 10);
        po1.valid_time_range.valid_until = Timestamp::from(1);
        let _ = pool.add_operation(po1.clone(), None).unwrap();

        let res = pool.remove_expired(Timestamp::from(2));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].0, po1.uo.op_hash(conf.entry_point, conf.chain_id));
        assert_eq!(res[0].1, Timestamp::from(1));
    }

    #[test]
    fn test_multiple_expired() {
        let conf = conf();
        let mut pool = PoolInner::new(conf.clone());

        let mut po1 = create_op(Address::random(), 0, 10);
        po1.valid_time_range.valid_until = 5.into();
        let _ = pool.add_operation(po1.clone(), None).unwrap();

        let mut po2 = create_op(Address::random(), 0, 10);
        po2.valid_time_range.valid_until = 10.into();
        let _ = pool.add_operation(po2.clone(), None).unwrap();

        let mut po3 = create_op(Address::random(), 0, 10);
        po3.valid_time_range.valid_until = 9.into();
        let _ = pool.add_operation(po3.clone(), None).unwrap();

        let res = pool.remove_expired(10.into());
        assert_eq!(res.len(), 2);
        assert!(res.contains(&(po1.uo.op_hash(conf.entry_point, conf.chain_id), 5.into())));
        assert!(res.contains(&(po3.uo.op_hash(conf.entry_point, conf.chain_id), 9.into())));
    }

    fn conf() -> PoolInnerConfig {
        PoolInnerConfig {
            entry_point: Address::random(),
            chain_id: 1,
            min_replacement_fee_increase_percentage: 10,
            max_size_of_pool_bytes: 20 * mem_size_of_ordered_pool_op(),
            throttled_entity_mempool_count: 4,
            throttled_entity_live_blocks: 10,
            paymaster_tracking_enabled: true,
        }
    }

    fn mem_size_of_ordered_pool_op() -> usize {
        OrderedPoolOperation {
            po: Arc::new(create_op(Address::random(), 1, 1)),
            submission_id: 1,
        }
        .mem_size()
    }

    fn create_op(sender: Address, nonce: usize, max_fee_per_gas: usize) -> PoolOperation {
        PoolOperation {
            uo: UserOperation {
                sender,
                nonce: nonce.into(),
                max_fee_per_gas: max_fee_per_gas.into(),

                ..UserOperation::default()
            },
            entity_infos: EntityInfos {
                factory: None,
                sender: EntityInfo {
                    address: sender,
                    is_staked: false,
                },
                paymaster: None,
                aggregator: None,
            },
            ..PoolOperation::default()
        }
    }

    fn check_map_entry(actual: Option<&OrderedPoolOperation>, expected: Option<&PoolOperation>) {
        match (actual, expected) {
            (Some(actual), Some(expected)) => assert_eq!(*actual.po, *expected),
            (None, None) => (),
            _ => panic!("Expected {expected:?}, got {actual:?}"),
        }
    }
}
