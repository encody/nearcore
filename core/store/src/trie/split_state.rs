use crate::flat::FlatStateChanges;
use crate::{get, get_delayed_receipt_indices, set, ShardTries, StoreUpdate, Trie, TrieUpdate};
use borsh::BorshDeserialize;
use bytesize::ByteSize;
use near_primitives::account::id::AccountId;
use near_primitives::errors::StorageError;
use near_primitives::receipt::Receipt;
use near_primitives::shard_layout::ShardUId;
use near_primitives::state_part::PartId;
use near_primitives::trie_key::trie_key_parsers::parse_account_id_from_raw_key;
use near_primitives::trie_key::TrieKey;
use near_primitives::types::{
    ConsolidatedStateChange, StateChangeCause, StateChangesForSplitStates, StateRoot,
};
use std::collections::HashMap;

use super::iterator::TrieItem;

impl Trie {
    // TODO(#9446) remove function when shifting to flat storage iteration for resharding
    pub fn get_trie_items_for_part(&self, part_id: PartId) -> Result<Vec<TrieItem>, StorageError> {
        let path_begin = self.find_state_part_boundary(part_id.idx, part_id.total)?;
        let path_end = self.find_state_part_boundary(part_id.idx + 1, part_id.total)?;
        self.iter()?.get_trie_items(&path_begin, &path_end)
    }
}

impl ShardTries {
    /// applies `changes` to split states
    /// and returns the generated TrieUpdate for all split states
    /// Note that this function is different from the function `add_values_to_split_states`
    /// This function is used for applying updates to split states when processing blocks
    /// `add_values_to_split_states` are used to generate the initial states for shards split
    /// from the original parent shard.
    pub fn apply_state_changes_to_split_states(
        &self,
        state_roots: &HashMap<ShardUId, StateRoot>,
        changes: StateChangesForSplitStates,
        account_id_to_shard_uid: &dyn Fn(&AccountId) -> ShardUId,
    ) -> Result<HashMap<ShardUId, TrieUpdate>, StorageError> {
        let mut trie_updates: HashMap<_, _> = self.get_trie_updates(state_roots);
        let mut insert_receipts = Vec::new();
        for ConsolidatedStateChange { trie_key, value } in changes.changes {
            match &trie_key {
                TrieKey::DelayedReceiptIndices => {}
                TrieKey::DelayedReceipt { index } => match value {
                    Some(value) => {
                        let receipt = Receipt::try_from_slice(&value).map_err(|err| {
                            StorageError::StorageInconsistentState(format!(
                                "invalid delayed receipt {:?}, err: {}",
                                value,
                                err.to_string(),
                            ))
                        })?;
                        insert_receipts.push((*index, receipt));
                    }
                    None => {}
                },
                TrieKey::Account { account_id }
                | TrieKey::ContractCode { account_id }
                | TrieKey::AccessKey { account_id, .. }
                | TrieKey::ReceivedData { receiver_id: account_id, .. }
                | TrieKey::PostponedReceiptId { receiver_id: account_id, .. }
                | TrieKey::PendingDataCount { receiver_id: account_id, .. }
                | TrieKey::PostponedReceipt { receiver_id: account_id, .. }
                | TrieKey::ContractData { account_id, .. } => {
                    let new_shard_uid = account_id_to_shard_uid(account_id);
                    // we can safely unwrap here because the caller of this function guarantees trie_updates
                    // contains all shard_uids for the new shards
                    let trie_update = trie_updates.get_mut(&new_shard_uid).unwrap();
                    match value {
                        Some(value) => trie_update.set(trie_key, value),
                        None => trie_update.remove(trie_key),
                    }
                }
            }
        }
        for (_, update) in trie_updates.iter_mut() {
            // StateChangeCause should always be Resharding for processing split state.
            // We do not want to commit the state_changes from resharding as they are already handled while
            // processing parent shard
            update.commit(StateChangeCause::Resharding);
        }

        insert_receipts.sort_by_key(|it| it.0);

        let insert_receipts: Vec<_> =
            insert_receipts.into_iter().map(|(_, receipt)| receipt).collect();

        apply_delayed_receipts_to_split_states_impl(
            &mut trie_updates,
            &insert_receipts,
            &changes.processed_delayed_receipts,
            account_id_to_shard_uid,
        )?;

        Ok(trie_updates)
    }

    /// add `values` (key-value pairs of items stored in states) to build states for new shards
    /// `state_roots` contains state roots for the new shards
    /// The caller must guarantee that `state_roots` contains all shard_ids
    /// that `key_to_shard_id` that may return
    /// Ignore changes on DelayedReceipts or DelayedReceiptsIndices
    /// Returns `store_update` and the new state_roots for split states
    pub fn add_values_to_split_states(
        &self,
        state_roots: &HashMap<ShardUId, StateRoot>,
        values: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        account_id_to_shard_id: &dyn Fn(&AccountId) -> ShardUId,
    ) -> Result<(StoreUpdate, HashMap<ShardUId, StateRoot>), StorageError> {
        self.add_values_to_split_states_impl(state_roots, values, &|raw_key| {
            // Here changes on DelayedReceipts or DelayedReceiptsIndices will be excluded
            // This is because we cannot migrate delayed receipts part by part. They have to be
            // reconstructed in the new states after all DelayedReceipts are ready in the original
            // shard.
            if let Some(account_id) = parse_account_id_from_raw_key(raw_key).map_err(|e| {
                let err = format!("error parsing account id from trie key {:?}: {:?}", raw_key, e);
                StorageError::StorageInconsistentState(err)
            })? {
                let new_shard_uid = account_id_to_shard_id(&account_id);
                Ok(Some(new_shard_uid))
            } else {
                Ok(None)
            }
        })
    }

    fn add_values_to_split_states_impl(
        &self,
        state_roots: &HashMap<ShardUId, StateRoot>,
        values: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        key_to_shard_id: &dyn Fn(&[u8]) -> Result<Option<ShardUId>, StorageError>,
    ) -> Result<(StoreUpdate, HashMap<ShardUId, StateRoot>), StorageError> {
        let mut changes_by_shard: HashMap<_, Vec<_>> = HashMap::new();
        for (raw_key, value) in values.into_iter() {
            if let Some(new_shard_uid) = key_to_shard_id(&raw_key)? {
                changes_by_shard.entry(new_shard_uid).or_default().push((raw_key, value));
            }
        }
        let mut new_state_roots = state_roots.clone();
        let mut store_update = self.store_update();
        for (shard_uid, changes) in changes_by_shard {
            FlatStateChanges::from_raw_key_value(&changes)
                .apply_to_flat_state(&mut store_update, shard_uid);
            // Here we assume that state_roots contains shard_uid, the caller of this method will guarantee that.
            let trie_changes =
                self.get_trie_for_shard(shard_uid, state_roots[&shard_uid]).update(changes)?;
            let state_root = self.apply_all(&trie_changes, shard_uid, &mut store_update);
            new_state_roots.insert(shard_uid, state_root);
        }
        Ok((store_update, new_state_roots))
    }

    fn get_trie_updates(
        &self,
        state_roots: &HashMap<ShardUId, StateRoot>,
    ) -> HashMap<ShardUId, TrieUpdate> {
        state_roots
            .iter()
            .map(|(shard_uid, state_root)| {
                (*shard_uid, self.new_trie_update(*shard_uid, *state_root))
            })
            .collect()
    }

    pub fn apply_delayed_receipts_to_split_states(
        &self,
        state_roots: &HashMap<ShardUId, StateRoot>,
        receipts: &[Receipt],
        account_id_to_shard_uid: &dyn Fn(&AccountId) -> ShardUId,
    ) -> Result<(StoreUpdate, HashMap<ShardUId, StateRoot>), StorageError> {
        let mut trie_updates: HashMap<_, _> = self.get_trie_updates(state_roots);
        apply_delayed_receipts_to_split_states_impl(
            &mut trie_updates,
            receipts,
            &[],
            account_id_to_shard_uid,
        )?;
        self.finalize_and_apply_trie_updates(trie_updates)
    }

    fn finalize_and_apply_trie_updates(
        &self,
        updates: HashMap<ShardUId, TrieUpdate>,
    ) -> Result<(StoreUpdate, HashMap<ShardUId, StateRoot>), StorageError> {
        let mut new_state_roots = HashMap::new();
        let mut store_update = self.store_update();
        for (shard_uid, update) in updates {
            let (_, trie_changes, state_changes) = update.finalize()?;
            let state_root = self.apply_all(&trie_changes, shard_uid, &mut store_update);
            FlatStateChanges::from_state_changes(&state_changes)
                .apply_to_flat_state(&mut store_update, shard_uid);
            new_state_roots.insert(shard_uid, state_root);
        }
        Ok((store_update, new_state_roots))
    }
}

fn apply_delayed_receipts_to_split_states_impl(
    trie_updates: &mut HashMap<ShardUId, TrieUpdate>,
    insert_receipts: &[Receipt],
    delete_receipts: &[Receipt],
    account_id_to_shard_uid: &dyn Fn(&AccountId) -> ShardUId,
) -> Result<(), StorageError> {
    let mut delayed_receipts_indices_by_shard = HashMap::new();
    for (shard_uid, update) in trie_updates.iter() {
        delayed_receipts_indices_by_shard.insert(*shard_uid, get_delayed_receipt_indices(update)?);
    }

    for receipt in insert_receipts {
        let new_shard_uid: ShardUId = account_id_to_shard_uid(&receipt.receiver_id);
        if !trie_updates.contains_key(&new_shard_uid) {
            let err = format!(
                "Account {} is in new shard {:?} but state_roots only contains {:?}",
                receipt.receiver_id,
                new_shard_uid,
                trie_updates.keys(),
            );
            return Err(StorageError::StorageInconsistentState(err));
        }
        // we already checked that new_shard_uid is in trie_updates and delayed_receipts_indices
        // so we can safely unwrap here
        let delayed_receipts_indices =
            delayed_receipts_indices_by_shard.get_mut(&new_shard_uid).unwrap();
        set(
            trie_updates.get_mut(&new_shard_uid).unwrap(),
            TrieKey::DelayedReceipt { index: delayed_receipts_indices.next_available_index },
            receipt,
        );
        delayed_receipts_indices.next_available_index =
            delayed_receipts_indices.next_available_index.checked_add(1).ok_or_else(|| {
                StorageError::StorageInconsistentState(
                    "Next available index for delayed receipt exceeded the integer limit"
                        .to_string(),
                )
            })?;
    }

    for receipt in delete_receipts {
        let new_shard_uid: ShardUId = account_id_to_shard_uid(&receipt.receiver_id);
        if !trie_updates.contains_key(&new_shard_uid) {
            let err = format!(
                "Account {} is in new shard {:?} but state_roots only contains {:?}",
                receipt.receiver_id,
                new_shard_uid,
                trie_updates.keys(),
            );
            return Err(StorageError::StorageInconsistentState(err));
        }
        let delayed_receipts_indices =
            delayed_receipts_indices_by_shard.get_mut(&new_shard_uid).unwrap();

        let trie_update = trie_updates.get_mut(&new_shard_uid).unwrap();
        let trie_key = TrieKey::DelayedReceipt { index: delayed_receipts_indices.first_index };

        let stored_receipt = get::<Receipt>(trie_update, &trie_key)?
            .expect("removed receipt does not exist in new state");
        // check that the receipt to remove is at the first of delayed receipt queue
        assert_eq!(&stored_receipt, receipt);
        trie_update.remove(trie_key);
        delayed_receipts_indices.first_index += 1;
    }

    // commit the trie_updates and update state_roots
    for (shard_uid, trie_update) in trie_updates {
        set(
            trie_update,
            TrieKey::DelayedReceiptIndices,
            delayed_receipts_indices_by_shard.get(shard_uid).unwrap(),
        );
        // StateChangeCause should always be Resharding for processing split state.
        // We do not want to commit the state_changes from resharding as they are already handled while
        // processing parent shard
        trie_update.commit(StateChangeCause::Resharding);
    }
    Ok(())
}

/// Retrieve delayed receipts starting with `start_index` until `memory_limit` is hit
/// return None if there is no delayed receipts with index >= start_index
pub fn get_delayed_receipts(
    state_update: &TrieUpdate,
    start_index: Option<u64>,
    memory_limit: ByteSize,
) -> Result<Option<(u64, Vec<Receipt>)>, StorageError> {
    let mut delayed_receipt_indices = get_delayed_receipt_indices(state_update)?;
    if let Some(start_index) = start_index {
        if start_index >= delayed_receipt_indices.next_available_index {
            return Ok(None);
        }
        delayed_receipt_indices.first_index = start_index.max(delayed_receipt_indices.first_index);
    }
    let mut used_memory = 0;
    let mut receipts = vec![];

    while used_memory < memory_limit.as_u64()
        && delayed_receipt_indices.first_index < delayed_receipt_indices.next_available_index
    {
        let key = TrieKey::DelayedReceipt { index: delayed_receipt_indices.first_index };
        let data = state_update.get(&key)?.ok_or_else(|| {
            StorageError::StorageInconsistentState(format!(
                "Delayed receipt #{} should be in the state",
                delayed_receipt_indices.first_index
            ))
        })?;
        used_memory += data.len() as u64;
        delayed_receipt_indices.first_index += 1;

        let receipt = Receipt::try_from_slice(&data).map_err(|_| {
            StorageError::StorageInconsistentState("Failed to deserialize".to_string())
        })?;
        receipts.push(receipt);
    }
    Ok(Some((delayed_receipt_indices.first_index, receipts)))
}

#[cfg(test)]
mod tests {
    use crate::split_state::{apply_delayed_receipts_to_split_states_impl, get_delayed_receipts};
    use crate::test_utils::{
        gen_changes, gen_receipts, get_all_delayed_receipts, test_populate_trie, TestTriesBuilder,
    };

    use crate::{set, ShardTries, ShardUId, Trie};
    use near_primitives::account::id::AccountId;

    use near_primitives::hash::hash;
    use near_primitives::receipt::{DelayedReceiptIndices, Receipt};
    use near_primitives::trie_key::TrieKey;
    use near_primitives::types::{NumShards, StateChangeCause, StateRoot};
    use rand::Rng;
    use std::collections::HashMap;

    #[test]
    fn test_add_values_to_split_states() {
        let mut rng = rand::thread_rng();

        for _ in 0..20 {
            let tries = TestTriesBuilder::new().build();
            // add 4 new shards for version 1
            let num_shards = 4;
            let mut state_root = Trie::EMPTY_ROOT;
            let mut state_roots: HashMap<_, _> = (0..num_shards)
                .map(|x| (ShardUId { version: 1, shard_id: x as u32 }, Trie::EMPTY_ROOT))
                .collect();
            for _ in 0..10 {
                let changes = gen_changes(&mut rng, 100);
                state_root = test_populate_trie(
                    &tries,
                    &state_root,
                    ShardUId::single_shard(),
                    changes.clone(),
                );

                let (store_update, new_state_roots) = tries
                    .add_values_to_split_states_impl(&state_roots, changes, &|raw_key| {
                        Ok(Some(ShardUId {
                            version: 1,
                            shard_id: (hash(raw_key).0[0] as NumShards % num_shards) as u32,
                        }))
                    })
                    .unwrap();
                store_update.commit().unwrap();
                state_roots = new_state_roots;

                // check that the 4 tries combined to the orig trie
                let trie = tries.get_trie_for_shard(ShardUId::single_shard(), state_root);
                let trie_items: HashMap<_, _> = trie.iter().unwrap().map(Result::unwrap).collect();
                let mut combined_trie_items: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
                for (shard_uid, state_root) in state_roots.iter() {
                    let trie = tries.get_view_trie_for_shard(*shard_uid, *state_root);
                    combined_trie_items.extend(trie.iter().unwrap().map(Result::unwrap));
                }
                assert_eq!(trie_items, combined_trie_items);
            }
        }
    }

    #[test]
    fn test_get_delayed_receipts() {
        let mut rng = rand::thread_rng();
        for _ in 0..20 {
            let memory_limit = bytesize::ByteSize::b(rng.gen_range(200..1000));
            let all_receipts = gen_receipts(&mut rng, 200);

            // push receipt to trie
            let tries = TestTriesBuilder::new().build();
            let mut trie_update = tries.new_trie_update(ShardUId::single_shard(), Trie::EMPTY_ROOT);
            let mut delayed_receipt_indices = DelayedReceiptIndices::default();

            for (i, receipt) in all_receipts.iter().enumerate() {
                set(&mut trie_update, TrieKey::DelayedReceipt { index: i as u64 }, receipt);
            }
            delayed_receipt_indices.next_available_index = all_receipts.len() as u64;
            set(&mut trie_update, TrieKey::DelayedReceiptIndices, &delayed_receipt_indices);
            trie_update.commit(StateChangeCause::Resharding);
            let (_, trie_changes, _) = trie_update.finalize().unwrap();
            let mut store_update = tries.store_update();
            let state_root =
                tries.apply_all(&trie_changes, ShardUId::single_shard(), &mut store_update);
            store_update.commit().unwrap();

            assert_eq!(
                all_receipts,
                get_all_delayed_receipts(&tries, &ShardUId::single_shard(), &state_root)
            );
            let mut start_index = 0;

            let trie_update = tries.new_trie_update(ShardUId::single_shard(), state_root);
            while let Some((next_index, receipts)) =
                get_delayed_receipts(&trie_update, Some(start_index), memory_limit).unwrap()
            {
                assert_eq!(receipts, all_receipts[start_index as usize..next_index as usize]);
                start_index = next_index;

                let total_memory_use: u64 = receipts
                    .iter()
                    .map(|receipt| borsh::object_length(&receipt).unwrap() as u64)
                    .sum();
                let memory_use_without_last_receipt: u64 = receipts[..receipts.len() - 1]
                    .iter()
                    .map(|receipt| borsh::object_length(&receipt).unwrap() as u64)
                    .sum();

                assert!(
                    total_memory_use >= memory_limit.as_u64()
                        || next_index == all_receipts.len() as u64
                );
                assert!(memory_use_without_last_receipt < memory_limit.as_u64());
            }
        }
    }

    fn test_apply_delayed_receipts(
        tries: &ShardTries,
        new_receipts: &[Receipt],
        delete_receipts: &[Receipt],
        expected_all_receipts: &[Receipt],
        state_roots: HashMap<ShardUId, StateRoot>,
        account_id_to_shard_id: &dyn Fn(&AccountId) -> ShardUId,
    ) -> HashMap<ShardUId, StateRoot> {
        let mut trie_updates: HashMap<_, _> = tries.get_trie_updates(&state_roots);
        apply_delayed_receipts_to_split_states_impl(
            &mut trie_updates,
            new_receipts,
            delete_receipts,
            account_id_to_shard_id,
        )
        .unwrap();
        let (state_update, new_state_roots) =
            tries.finalize_and_apply_trie_updates(trie_updates).unwrap();
        state_update.commit().unwrap();

        let receipts_by_shard: HashMap<_, _> = new_state_roots
            .iter()
            .map(|(shard_uid, state_root)| {
                let receipts = get_all_delayed_receipts(tries, shard_uid, state_root);
                (shard_uid, receipts)
            })
            .collect();

        let mut expected_receipts_by_shard: HashMap<_, _> =
            state_roots.iter().map(|(shard_uid, _)| (shard_uid, vec![])).collect();
        for receipt in expected_all_receipts {
            let shard_uid = account_id_to_shard_id(&receipt.receiver_id);
            expected_receipts_by_shard.get_mut(&shard_uid).unwrap().push(receipt.clone());
        }
        assert_eq!(expected_receipts_by_shard, receipts_by_shard);

        new_state_roots
    }

    #[test]
    fn test_apply_delayed_receipts_to_new_states() {
        let mut rng = rand::thread_rng();

        let tries = TestTriesBuilder::new().build();
        let num_shards = 4;

        for _ in 0..10 {
            let mut state_roots: HashMap<_, _> = (0..num_shards)
                .map(|x| (ShardUId { version: 1, shard_id: x as u32 }, Trie::EMPTY_ROOT))
                .collect();
            let mut all_receipts = vec![];
            let mut start_index = 0;
            for _ in 0..10 {
                let receipts = gen_receipts(&mut rng, 100);
                let new_start_index = rng.gen_range(start_index..all_receipts.len() + 1);

                all_receipts.extend_from_slice(&receipts);
                state_roots = test_apply_delayed_receipts(
                    &tries,
                    &receipts,
                    &all_receipts[start_index..new_start_index],
                    &all_receipts[new_start_index..],
                    state_roots,
                    &|account_id| ShardUId {
                        shard_id: (hash(account_id.as_bytes()).0[0] as NumShards % num_shards)
                            as u32,
                        version: 1,
                    },
                );
                start_index = new_start_index;
            }
        }
    }
}
