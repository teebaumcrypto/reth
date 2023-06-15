use crate::{
    insert_canonical_block,
    post_state::StorageChangeset,
    traits::{AccountExtProvider, BlockSource, ReceiptProvider},
    AccountProvider, BlockHashProvider, BlockNumProvider, BlockProvider, EvmEnvProvider,
    HeaderProvider, PostState, ProviderError, StageCheckpointProvider, TransactionError,
    TransactionsProvider, WithdrawalsProvider,
};
use itertools::{izip, Itertools};
use reth_db::{
    common::KeyValue,
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO},
    database::{Database, DatabaseGAT},
    models::{
        sharded_key,
        storage_sharded_key::{self, StorageShardedKey},
        AccountBeforeTx, BlockNumberAddress, ShardedKey, StoredBlockBodyIndices,
    },
    table::Table,
    tables,
    transaction::{DbTx, DbTxMut, DbTxMutGAT},
    BlockNumberList, DatabaseError,
};
use reth_interfaces::Result;
use reth_primitives::{
    keccak256,
    stage::{StageCheckpoint, StageId},
    Account, Address, Block, BlockHash, BlockHashOrNumber, BlockNumber, BlockWithSenders,
    ChainInfo, ChainSpec, Hardfork, Head, Header, Receipt, SealedBlock, SealedBlockWithSenders,
    SealedHeader, StorageEntry, TransactionMeta, TransactionSigned, TransactionSignedEcRecovered,
    TransactionSignedNoHash, TxHash, TxNumber, Withdrawal, H256, U256,
};
use reth_revm_primitives::{
    config::revm_spec,
    env::{fill_block_env, fill_cfg_and_block_env, fill_cfg_env},
    primitives::{BlockEnv, CfgEnv, SpecId},
};
use reth_trie::StateRoot;
use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    fmt::Debug,
    ops::{Deref, DerefMut, Range, RangeBounds, RangeInclusive},
    sync::Arc,
};

use super::get_stage_checkpoint;

/// A [`DatabaseProvider`] that holds a read-only database transaction.
pub type DatabaseProviderRO<'this, DB> = DatabaseProvider<'this, <DB as DatabaseGAT<'this>>::TX>;

/// A [`DatabaseProvider`] that holds a read-write database transaction.
///
/// Ideally this would be an alias type. However, there's some weird compiler error (<https://github.com/rust-lang/rust/issues/102211>), that forces us to wrap this in a struct instead.
/// Once that issue is solved, we can probably revert back to being an alias type.
#[derive(Debug)]
pub struct DatabaseProviderRW<'this, DB: Database>(
    pub DatabaseProvider<'this, <DB as DatabaseGAT<'this>>::TXMut>,
);

impl<'this, DB: Database> Deref for DatabaseProviderRW<'this, DB> {
    type Target = DatabaseProvider<'this, <DB as DatabaseGAT<'this>>::TXMut>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'this, DB: Database> DerefMut for DatabaseProviderRW<'this, DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'this, DB: Database> DatabaseProviderRW<'this, DB> {
    /// Commit database transaction
    pub fn commit(self) -> Result<bool> {
        self.0.commit()
    }

    /// Consume `DbTx` or `DbTxMut`.
    pub fn into_tx(self) -> <DB as DatabaseGAT<'this>>::TXMut {
        self.0.into_tx()
    }
}

/// A provider struct that fetchs data from the database.
/// Wrapper around [`DbTx`] and [`DbTxMut`]. Example: [`HeaderProvider`] [`BlockHashProvider`]
#[derive(Debug)]
pub struct DatabaseProvider<'this, TX>
where
    Self: 'this,
{
    /// Database transaction.
    tx: TX,
    /// Chain spec
    chain_spec: Arc<ChainSpec>,
    _phantom_data: std::marker::PhantomData<&'this TX>,
}

impl<'this, TX: DbTxMut<'this>> DatabaseProvider<'this, TX> {
    /// Creates a provider with an inner read-write transaction.
    pub fn new_rw(tx: TX, chain_spec: Arc<ChainSpec>) -> Self {
        Self { tx, chain_spec, _phantom_data: std::marker::PhantomData }
    }
}

/// Unwind all history shards. For boundary shard, remove it from database and
/// return last part of shard with still valid items. If all full shard were removed, return list
/// would be empty.
fn unwind_account_history_shards<'a, TX: reth_db::transaction::DbTxMutGAT<'a>>(
    cursor: &mut <TX as DbTxMutGAT<'a>>::CursorMut<tables::AccountHistory>,
    address: Address,
    block_number: BlockNumber,
) -> std::result::Result<Vec<usize>, TransactionError> {
    let mut item = cursor.seek_exact(ShardedKey::new(address, u64::MAX))?;

    while let Some((sharded_key, list)) = item {
        // there is no more shard for address
        if sharded_key.key != address {
            break
        }
        cursor.delete_current()?;
        // check first item and if it is more and eq than `transition_id` delete current
        // item.
        let first = list.iter(0).next().expect("List can't empty");
        if first >= block_number as usize {
            item = cursor.prev()?;
            continue
        } else if block_number <= sharded_key.highest_block_number {
            // if first element is in scope whole list would be removed.
            // so at least this first element is present.
            return Ok(list.iter(0).take_while(|i| *i < block_number as usize).collect::<Vec<_>>())
        } else {
            let new_list = list.iter(0).collect::<Vec<_>>();
            return Ok(new_list)
        }
    }
    Ok(Vec::new())
}

/// Unwind all history shards. For boundary shard, remove it from database and
/// return last part of shard with still valid items. If all full shard were removed, return list
/// would be empty but this does not mean that there is none shard left but that there is no
/// split shards.
fn unwind_storage_history_shards<'a, TX: reth_db::transaction::DbTxMutGAT<'a>>(
    cursor: &mut <TX as DbTxMutGAT<'a>>::CursorMut<tables::StorageHistory>,
    address: Address,
    storage_key: H256,
    block_number: BlockNumber,
) -> std::result::Result<Vec<usize>, TransactionError> {
    let mut item = cursor.seek_exact(StorageShardedKey::new(address, storage_key, u64::MAX))?;

    while let Some((storage_sharded_key, list)) = item {
        // there is no more shard for address
        if storage_sharded_key.address != address ||
            storage_sharded_key.sharded_key.key != storage_key
        {
            // there is no more shard for address and storage_key.
            break
        }
        cursor.delete_current()?;
        // check first item and if it is more and eq than `transition_id` delete current
        // item.
        let first = list.iter(0).next().expect("List can't empty");
        if first >= block_number as usize {
            item = cursor.prev()?;
            continue
        } else if block_number <= storage_sharded_key.sharded_key.highest_block_number {
            // if first element is in scope whole list would be removed.
            // so at least this first element is present.
            return Ok(list.iter(0).take_while(|i| *i < block_number as usize).collect::<Vec<_>>())
        } else {
            return Ok(list.iter(0).collect::<Vec<_>>())
        }
    }
    Ok(Vec::new())
}

impl<'this, TX: DbTx<'this>> DatabaseProvider<'this, TX> {
    /// Creates a provider with an inner read-only transaction.
    pub fn new(tx: TX, chain_spec: Arc<ChainSpec>) -> Self {
        Self { tx, chain_spec, _phantom_data: std::marker::PhantomData }
    }

    /// Consume `DbTx` or `DbTxMut`.
    pub fn into_tx(self) -> TX {
        self.tx
    }

    /// Pass `DbTx` or `DbTxMut` mutable reference.
    pub fn tx_mut(&mut self) -> &mut TX {
        &mut self.tx
    }

    /// Pass `DbTx` or `DbTxMut` immutable reference.
    pub fn tx_ref(&self) -> &TX {
        &self.tx
    }

    /// Return full table as Vec
    pub fn table<T: Table>(&self) -> std::result::Result<Vec<KeyValue<T>>, DatabaseError>
    where
        T::Key: Default + Ord,
    {
        self.tx
            .cursor_read::<T>()?
            .walk(Some(T::Key::default()))?
            .collect::<std::result::Result<Vec<_>, DatabaseError>>()
    }

    // TODO(joshie) TEMPORARY should be moved to trait providers

    /// Iterate over account changesets and return all account address that were changed.
    pub fn get_addresses_and_keys_of_changed_storages(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<BTreeMap<Address, BTreeSet<H256>>, TransactionError> {
        self.tx
            .cursor_read::<tables::StorageChangeSet>()?
            .walk_range(BlockNumberAddress::range(range))?
            // fold all storages and save its old state so we can remove it from HashedStorage
            // it is needed as it is dup table.
            .try_fold(BTreeMap::new(), |mut accounts: BTreeMap<Address, BTreeSet<H256>>, entry| {
                let (BlockNumberAddress((_, address)), storage_entry) = entry?;
                accounts.entry(address).or_default().insert(storage_entry.key);
                Ok(accounts)
            })
    }

    /// Get plainstate storages
    #[allow(clippy::type_complexity)]
    pub fn get_plainstate_storages(
        &self,
        iter: impl IntoIterator<Item = (Address, impl IntoIterator<Item = H256>)>,
    ) -> std::result::Result<Vec<(Address, Vec<(H256, U256)>)>, TransactionError> {
        let mut plain_storage = self.tx.cursor_dup_read::<tables::PlainStorageState>()?;

        iter.into_iter()
            .map(|(address, storage)| {
                storage
                    .into_iter()
                    .map(|key| -> std::result::Result<_, TransactionError> {
                        let ret = plain_storage
                            .seek_by_key_subkey(address, key)?
                            .filter(|v| v.key == key)
                            .unwrap_or_default();
                        Ok((key, ret.value))
                    })
                    .collect::<std::result::Result<Vec<(_, _)>, _>>()
                    .map(|storage| (address, storage))
            })
            .collect::<std::result::Result<Vec<(_, _)>, _>>()
    }

    /// Get all transaction ids where account got changed.
    ///
    /// NOTE: Get inclusive range of blocks.
    pub fn get_storage_transition_ids_from_changeset(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<BTreeMap<(Address, H256), Vec<u64>>, TransactionError> {
        let mut changeset_cursor = self.tx.cursor_read::<tables::StorageChangeSet>()?;

        let storage_changeset_lists =
            changeset_cursor.walk_range(BlockNumberAddress::range(range))?.try_fold(
                BTreeMap::new(),
                |mut storages: BTreeMap<(Address, H256), Vec<u64>>,
                 entry|
                 -> std::result::Result<_, TransactionError> {
                    let (index, storage) = entry?;
                    storages
                        .entry((index.address(), storage.key))
                        .or_default()
                        .push(index.block_number());
                    Ok(storages)
                },
            )?;

        Ok(storage_changeset_lists)
    }

    /// Get all transaction ids where account got changed.
    ///
    /// NOTE: Get inclusive range of blocks.
    pub fn get_account_transition_ids_from_changeset(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<BTreeMap<Address, Vec<u64>>, TransactionError> {
        let mut changeset_cursor = self.tx.cursor_read::<tables::AccountChangeSet>()?;

        let account_transtions = changeset_cursor.walk_range(range)?.try_fold(
            BTreeMap::new(),
            |mut accounts: BTreeMap<Address, Vec<u64>>,
             entry|
             -> std::result::Result<_, TransactionError> {
                let (index, account) = entry?;
                accounts.entry(account.address).or_default().push(index);
                Ok(accounts)
            },
        )?;

        Ok(account_transtions)
    }

    /// Iterate over account changesets and return all account address that were changed.
    pub fn get_addresses_of_changed_accounts(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<BTreeSet<Address>, TransactionError> {
        self.tx.cursor_read::<tables::AccountChangeSet>()?.walk_range(range)?.try_fold(
            BTreeSet::new(),
            |mut accounts: BTreeSet<Address>, entry| {
                let (_, account_before) = entry?;
                accounts.insert(account_before.address);
                Ok(accounts)
            },
        )
    }

    /// Get plainstate account from iterator
    pub fn get_plainstate_accounts(
        &self,
        iter: impl IntoIterator<Item = Address>,
    ) -> std::result::Result<Vec<(Address, Option<Account>)>, TransactionError> {
        let mut plain_accounts = self.tx.cursor_read::<tables::PlainAccountState>()?;
        Ok(iter
            .into_iter()
            .map(|address| plain_accounts.seek_exact(address).map(|a| (address, a.map(|(_, v)| v))))
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

impl<'this, TX: DbTxMut<'this> + DbTx<'this>> DatabaseProvider<'this, TX> {
    /// Commit database transaction.
    pub fn commit(self) -> Result<bool> {
        Ok(self.tx.commit()?)
    }

    // TODO(joshie) TEMPORARY should be moved to trait providers

    /// Get range of blocks and its execution result
    pub fn get_block_and_execution_range(
        &self,
        chain_spec: &ChainSpec,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<Vec<(SealedBlockWithSenders, PostState)>, TransactionError> {
        self.get_take_block_and_execution_range::<false>(chain_spec, range)
    }

    /// Take range of blocks and its execution result
    pub fn take_block_and_execution_range(
        &self,
        chain_spec: &ChainSpec,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<Vec<(SealedBlockWithSenders, PostState)>, TransactionError> {
        self.get_take_block_and_execution_range::<true>(chain_spec, range)
    }

    /// Unwind and clear account hashing
    pub fn unwind_account_hashing(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<(), TransactionError> {
        let mut hashed_accounts = self.tx.cursor_write::<tables::HashedAccount>()?;

        // Aggregate all transition changesets and make a list of accounts that have been changed.
        self.tx
            .cursor_read::<tables::AccountChangeSet>()?
            .walk_range(range)?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .rev()
            // fold all account to get the old balance/nonces and account that needs to be removed
            .fold(
                BTreeMap::new(),
                |mut accounts: BTreeMap<Address, Option<Account>>, (_, account_before)| {
                    accounts.insert(account_before.address, account_before.info);
                    accounts
                },
            )
            .into_iter()
            // hash addresses and collect it inside sorted BTreeMap.
            // We are doing keccak only once per address.
            .map(|(address, account)| (keccak256(address), account))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            // Apply values to HashedState (if Account is None remove it);
            .try_for_each(
                |(hashed_address, account)| -> std::result::Result<(), TransactionError> {
                    if let Some(account) = account {
                        hashed_accounts.upsert(hashed_address, account)?;
                    } else if hashed_accounts.seek_exact(hashed_address)?.is_some() {
                        hashed_accounts.delete_current()?;
                    }
                    Ok(())
                },
            )?;

        Ok(())
    }

    /// Unwind and clear storage hashing
    pub fn unwind_storage_hashing(
        &self,
        range: Range<BlockNumberAddress>,
    ) -> std::result::Result<(), TransactionError> {
        let mut hashed_storage = self.tx.cursor_dup_write::<tables::HashedStorage>()?;

        // Aggregate all transition changesets and make list of accounts that have been changed.
        self.tx
            .cursor_read::<tables::StorageChangeSet>()?
            .walk_range(range)?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .rev()
            // fold all account to get the old balance/nonces and account that needs to be removed
            .fold(
                BTreeMap::new(),
                |mut accounts: BTreeMap<(Address, H256), U256>,
                 (BlockNumberAddress((_, address)), storage_entry)| {
                    accounts.insert((address, storage_entry.key), storage_entry.value);
                    accounts
                },
            )
            .into_iter()
            // hash addresses and collect it inside sorted BTreeMap.
            // We are doing keccak only once per address.
            .map(|((address, key), value)| ((keccak256(address), keccak256(key)), value))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            // Apply values to HashedStorage (if Value is zero just remove it);
            .try_for_each(
                |((hashed_address, key), value)| -> std::result::Result<(), TransactionError> {
                    if hashed_storage
                        .seek_by_key_subkey(hashed_address, key)?
                        .filter(|entry| entry.key == key)
                        .is_some()
                    {
                        hashed_storage.delete_current()?;
                    }

                    if value != U256::ZERO {
                        hashed_storage.upsert(hashed_address, StorageEntry { key, value })?;
                    }
                    Ok(())
                },
            )?;

        Ok(())
    }

    /// Unwind and clear account history indices.
    ///
    /// Returns number of changesets walked.
    pub fn unwind_account_history_indices(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<usize, TransactionError> {
        let account_changeset = self
            .tx
            .cursor_read::<tables::AccountChangeSet>()?
            .walk_range(range)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let changesets = account_changeset.len();

        let last_indices = account_changeset
            .into_iter()
            // reverse so we can get lowest transition id where we need to unwind account.
            .rev()
            // fold all account and get last transition index
            .fold(BTreeMap::new(), |mut accounts: BTreeMap<Address, u64>, (index, account)| {
                // we just need address and lowest transition id.
                accounts.insert(account.address, index);
                accounts
            });
        // try to unwind the index
        let mut cursor = self.tx.cursor_write::<tables::AccountHistory>()?;
        for (address, rem_index) in last_indices {
            let shard_part = unwind_account_history_shards::<TX>(&mut cursor, address, rem_index)?;

            // check last shard_part, if present, items needs to be reinserted.
            if !shard_part.is_empty() {
                // there are items in list
                self.tx.put::<tables::AccountHistory>(
                    ShardedKey::new(address, u64::MAX),
                    BlockNumberList::new(shard_part)
                        .expect("There is at least one element in list and it is sorted."),
                )?;
            }
        }

        Ok(changesets)
    }

    /// Unwind and clear storage history indices.
    ///
    /// Returns number of changesets walked.
    pub fn unwind_storage_history_indices(
        &self,
        range: Range<BlockNumberAddress>,
    ) -> std::result::Result<usize, TransactionError> {
        let storage_changesets = self
            .tx
            .cursor_read::<tables::StorageChangeSet>()?
            .walk_range(range)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let changesets = storage_changesets.len();

        let last_indices = storage_changesets
            .into_iter()
            // reverse so we can get lowest transition id where we need to unwind account.
            .rev()
            // fold all storages and get last transition index
            .fold(
                BTreeMap::new(),
                |mut accounts: BTreeMap<(Address, H256), u64>, (index, storage)| {
                    // we just need address and lowest transition id.
                    accounts.insert((index.address(), storage.key), index.block_number());
                    accounts
                },
            );

        let mut cursor = self.tx.cursor_write::<tables::StorageHistory>()?;
        for ((address, storage_key), rem_index) in last_indices {
            let shard_part =
                unwind_storage_history_shards::<TX>(&mut cursor, address, storage_key, rem_index)?;

            // check last shard_part, if present, items needs to be reinserted.
            if !shard_part.is_empty() {
                // there are items in list
                self.tx.put::<tables::StorageHistory>(
                    StorageShardedKey::new(address, storage_key, u64::MAX),
                    BlockNumberList::new(shard_part)
                        .expect("There is at least one element in list and it is sorted."),
                )?;
            }
        }

        Ok(changesets)
    }

    /// Traverse over changesets and plain state and recreate the [`PostState`]s for the given range
    /// of blocks.
    ///
    /// 1. Iterate over the [BlockBodyIndices][tables::BlockBodyIndices] table to get all
    /// the transition indices.
    /// 2. Iterate over the [StorageChangeSet][tables::StorageChangeSet] table
    /// and the [AccountChangeSet][tables::AccountChangeSet] tables in reverse order to reconstruct
    /// the changesets.
    ///     - In order to have both the old and new values in the changesets, we also access the
    ///       plain state tables.
    /// 3. While iterating over the changeset tables, if we encounter a new account or storage slot,
    /// we:
    ///     1. Take the old value from the changeset
    ///     2. Take the new value from the plain state
    ///     3. Save the old value to the local state
    /// 4. While iterating over the changeset tables, if we encounter an account/storage slot we
    /// have seen before we:
    ///     1. Take the old value from the changeset
    ///     2. Take the new value from the local state
    ///     3. Set the local state to the value in the changeset
    ///
    /// If `TAKE` is `true`, the local state will be written to the plain state tables.
    /// 5. Get all receipts from table
    fn get_take_block_execution_result_range<const TAKE: bool>(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<Vec<PostState>, TransactionError> {
        if range.is_empty() {
            return Ok(Vec::new())
        }

        // We are not removing block meta as it is used to get block transitions.
        let block_bodies = self.get_or_take::<tables::BlockBodyIndices, false>(range.clone())?;

        // get transaction receipts
        let from_transaction_num =
            block_bodies.first().expect("already checked if there are blocks").1.first_tx_num();
        let to_transaction_num =
            block_bodies.last().expect("already checked if there are blocks").1.last_tx_num();
        let receipts =
            self.get_or_take::<tables::Receipts, TAKE>(from_transaction_num..=to_transaction_num)?;

        let storage_range = BlockNumberAddress::range(range.clone());

        let storage_changeset =
            self.get_or_take::<tables::StorageChangeSet, TAKE>(storage_range)?;
        let account_changeset = self.get_or_take::<tables::AccountChangeSet, TAKE>(range)?;

        // iterate previous value and get plain state value to create changeset
        // Double option around Account represent if Account state is know (first option) and
        // account is removed (Second Option)
        type LocalPlainState = BTreeMap<Address, (Option<Option<Account>>, BTreeMap<H256, U256>)>;

        let mut local_plain_state: LocalPlainState = BTreeMap::new();

        // iterate in reverse and get plain state.

        // Bundle execution changeset to its particular transaction and block
        let mut block_states =
            BTreeMap::from_iter(block_bodies.iter().map(|(num, _)| (*num, PostState::default())));

        let mut plain_accounts_cursor = self.tx.cursor_write::<tables::PlainAccountState>()?;
        let mut plain_storage_cursor = self.tx.cursor_dup_write::<tables::PlainStorageState>()?;

        // add account changeset changes
        for (block_number, account_before) in account_changeset.into_iter().rev() {
            let AccountBeforeTx { info: old_info, address } = account_before;
            let new_info = match local_plain_state.entry(address) {
                Entry::Vacant(entry) => {
                    let new_account = plain_accounts_cursor.seek_exact(address)?.map(|kv| kv.1);
                    entry.insert((Some(old_info), BTreeMap::new()));
                    new_account
                }
                Entry::Occupied(mut entry) => {
                    let new_account = std::mem::replace(&mut entry.get_mut().0, Some(old_info));
                    new_account.expect("As we are stacking account first, account would always be Some(Some) or Some(None)")
                }
            };

            let post_state = block_states.entry(block_number).or_default();
            match (old_info, new_info) {
                (Some(old), Some(new)) => {
                    if new != old {
                        post_state.change_account(block_number, address, old, new);
                    } else {
                        unreachable!("Junk data in database: an account changeset did not represent any change");
                    }
                }
                (None, Some(account)) =>  post_state.create_account(block_number, address, account),
                (Some(old), None) =>
                    post_state.destroy_account(block_number, address, old),
                (None, None) => unreachable!("Junk data in database: an account changeset transitioned from no account to no account"),
            };
        }

        // add storage changeset changes
        let mut storage_changes: BTreeMap<BlockNumberAddress, StorageChangeset> = BTreeMap::new();
        for (block_and_address, storage_entry) in storage_changeset.into_iter().rev() {
            let BlockNumberAddress((_, address)) = block_and_address;
            let new_storage =
                match local_plain_state.entry(address).or_default().1.entry(storage_entry.key) {
                    Entry::Vacant(entry) => {
                        let new_storage = plain_storage_cursor
                            .seek_by_key_subkey(address, storage_entry.key)?
                            .filter(|storage| storage.key == storage_entry.key)
                            .unwrap_or_default();
                        entry.insert(storage_entry.value);
                        new_storage.value
                    }
                    Entry::Occupied(mut entry) => {
                        std::mem::replace(entry.get_mut(), storage_entry.value)
                    }
                };
            storage_changes.entry(block_and_address).or_default().insert(
                U256::from_be_bytes(storage_entry.key.0),
                (storage_entry.value, new_storage),
            );
        }

        for (BlockNumberAddress((block_number, address)), storage_changeset) in
            storage_changes.into_iter()
        {
            block_states.entry(block_number).or_default().change_storage(
                block_number,
                address,
                storage_changeset,
            );
        }

        if TAKE {
            // iterate over local plain state remove all account and all storages.
            for (address, (account, storage)) in local_plain_state.into_iter() {
                // revert account
                if let Some(account) = account {
                    let existing_entry = plain_accounts_cursor.seek_exact(address)?;
                    if let Some(account) = account {
                        plain_accounts_cursor.upsert(address, account)?;
                    } else if existing_entry.is_some() {
                        plain_accounts_cursor.delete_current()?;
                    }
                }

                // revert storages
                for (storage_key, storage_value) in storage.into_iter() {
                    let storage_entry = StorageEntry { key: storage_key, value: storage_value };
                    // delete previous value
                    // TODO: This does not use dupsort features
                    if plain_storage_cursor
                        .seek_by_key_subkey(address, storage_key)?
                        .filter(|s| s.key == storage_key)
                        .is_some()
                    {
                        plain_storage_cursor.delete_current()?
                    }

                    // TODO: This does not use dupsort features
                    // insert value if needed
                    if storage_value != U256::ZERO {
                        plain_storage_cursor.upsert(address, storage_entry)?;
                    }
                }
            }
        }

        // iterate over block body and create ExecutionResult
        let mut receipt_iter = receipts.into_iter();

        // loop break if we are at the end of the blocks.
        for (block_number, block_body) in block_bodies.into_iter() {
            for _ in block_body.tx_num_range() {
                if let Some((_, receipt)) = receipt_iter.next() {
                    block_states
                        .entry(block_number)
                        .or_default()
                        .add_receipt(block_number, receipt);
                }
            }
        }
        Ok(block_states.into_values().collect())
    }

    /// Return range of blocks and its execution result
    pub fn get_take_block_and_execution_range<const TAKE: bool>(
        &self,
        chain_spec: &ChainSpec,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<Vec<(SealedBlockWithSenders, PostState)>, TransactionError> {
        if TAKE {
            let storage_range = BlockNumberAddress::range(range.clone());

            self.unwind_account_hashing(range.clone())?;
            self.unwind_account_history_indices(range.clone())?;
            self.unwind_storage_hashing(storage_range.clone())?;
            self.unwind_storage_history_indices(storage_range)?;

            // merkle tree
            let (new_state_root, trie_updates) =
                StateRoot::incremental_root_with_updates(&self.tx, range.clone())?;

            let parent_number = range.start().saturating_sub(1);
            let parent_state_root = self
                .tx
                .get::<tables::Headers>(parent_number)?
                .ok_or_else(|| ProviderError::HeaderNotFound(parent_number.into()))?
                .state_root;

            // state root should be always correct as we are reverting state.
            // but for sake of double verification we will check it again.
            if new_state_root != parent_state_root {
                let parent_hash = self
                    .tx
                    .get::<tables::CanonicalHeaders>(parent_number)?
                    .ok_or_else(|| ProviderError::HeaderNotFound(parent_number.into()))?;
                return Err(TransactionError::UnwindStateRootMismatch {
                    got: new_state_root,
                    expected: parent_state_root,
                    block_number: parent_number,
                    block_hash: parent_hash,
                })
            }
            trie_updates.flush(&self.tx)?;
        }
        // get blocks
        let blocks = self.get_take_block_range::<TAKE>(chain_spec, range.clone())?;
        let unwind_to = blocks.first().map(|b| b.number.saturating_sub(1));
        // get execution res
        let execution_res = self.get_take_block_execution_result_range::<TAKE>(range.clone())?;
        // combine them
        let blocks_with_exec_result: Vec<_> =
            blocks.into_iter().zip(execution_res.into_iter()).collect();

        // remove block bodies it is needed for both get block range and get block execution results
        // that is why it is deleted afterwards.
        if TAKE {
            // rm block bodies
            self.get_or_take::<tables::BlockBodyIndices, TAKE>(range)?;

            // Update pipeline progress
            if let Some(fork_number) = unwind_to {
                self.update_pipeline_stages(fork_number, true)?;
            }
        }

        // return them
        Ok(blocks_with_exec_result)
    }

    /// Return list of entries from table
    ///
    /// If TAKE is true, opened cursor would be write and it would delete all values from db.
    #[inline]
    pub fn get_or_take<T: Table, const TAKE: bool>(
        &self,
        range: impl RangeBounds<T::Key>,
    ) -> std::result::Result<Vec<KeyValue<T>>, DatabaseError> {
        if TAKE {
            let mut cursor_write = self.tx.cursor_write::<T>()?;
            let mut walker = cursor_write.walk_range(range)?;
            let mut items = Vec::new();
            while let Some(i) = walker.next().transpose()? {
                walker.delete_current()?;
                items.push(i)
            }
            Ok(items)
        } else {
            self.tx
                .cursor_read::<T>()?
                .walk_range(range)?
                .collect::<std::result::Result<Vec<_>, _>>()
        }
    }

    /// Get requested blocks transaction with signer
    fn get_take_block_transaction_range<const TAKE: bool>(
        &self,
        range: impl RangeBounds<BlockNumber> + Clone,
    ) -> std::result::Result<Vec<(BlockNumber, Vec<TransactionSignedEcRecovered>)>, TransactionError>
    {
        // Raad range of block bodies to get all transactions id's of this range.
        let block_bodies = self.get_or_take::<tables::BlockBodyIndices, false>(range)?;

        if block_bodies.is_empty() {
            return Ok(Vec::new())
        }

        // Compute the first and last tx ID in the range
        let first_transaction = block_bodies.first().expect("If we have headers").1.first_tx_num();
        let last_transaction = block_bodies.last().expect("Not empty").1.last_tx_num();

        // If this is the case then all of the blocks in the range are empty
        if last_transaction < first_transaction {
            return Ok(block_bodies.into_iter().map(|(n, _)| (n, Vec::new())).collect())
        }

        // Get transactions and senders
        let transactions = self
            .get_or_take::<tables::Transactions, TAKE>(first_transaction..=last_transaction)?
            .into_iter()
            .map(|(id, tx)| (id, tx.into()))
            .collect::<Vec<(u64, TransactionSigned)>>();

        let senders =
            self.get_or_take::<tables::TxSenders, TAKE>(first_transaction..=last_transaction)?;

        if TAKE {
            // Remove TxHashNumber
            let mut tx_hash_cursor = self.tx.cursor_write::<tables::TxHashNumber>()?;
            for (_, tx) in transactions.iter() {
                if tx_hash_cursor.seek_exact(tx.hash())?.is_some() {
                    tx_hash_cursor.delete_current()?;
                }
            }

            // Remove TransactionBlock index if there are transaction present
            if !transactions.is_empty() {
                let tx_id_range = transactions.first().unwrap().0..=transactions.last().unwrap().0;
                self.get_or_take::<tables::TransactionBlock, TAKE>(tx_id_range)?;
            }
        }

        // Merge transaction into blocks
        let mut block_tx = Vec::with_capacity(block_bodies.len());
        let mut senders = senders.into_iter();
        let mut transactions = transactions.into_iter();
        for (block_number, block_body) in block_bodies {
            let mut one_block_tx = Vec::with_capacity(block_body.tx_count as usize);
            for _ in block_body.tx_num_range() {
                let tx = transactions.next();
                let sender = senders.next();

                let recovered = match (tx, sender) {
                    (Some((tx_id, tx)), Some((sender_tx_id, sender))) => {
                        if tx_id != sender_tx_id {
                            Err(ProviderError::MismatchOfTransactionAndSenderId { tx_id })
                        } else {
                            Ok(TransactionSignedEcRecovered::from_signed_transaction(tx, sender))
                        }
                    }
                    (Some((tx_id, _)), _) | (_, Some((tx_id, _))) => {
                        Err(ProviderError::MismatchOfTransactionAndSenderId { tx_id })
                    }
                    (None, None) => Err(ProviderError::BlockBodyTransactionCount),
                }?;
                one_block_tx.push(recovered)
            }
            block_tx.push((block_number, one_block_tx));
        }

        Ok(block_tx)
    }

    /// Return range of blocks and its execution result
    fn get_take_block_range<const TAKE: bool>(
        &self,
        chain_spec: &ChainSpec,
        range: impl RangeBounds<BlockNumber> + Clone,
    ) -> std::result::Result<Vec<SealedBlockWithSenders>, TransactionError> {
        // For block we need Headers, Bodies, Uncles, withdrawals, Transactions, Signers

        let block_headers = self.get_or_take::<tables::Headers, TAKE>(range.clone())?;
        if block_headers.is_empty() {
            return Ok(Vec::new())
        }

        let block_header_hashes =
            self.get_or_take::<tables::CanonicalHeaders, TAKE>(range.clone())?;
        let block_ommers = self.get_or_take::<tables::BlockOmmers, TAKE>(range.clone())?;
        let block_withdrawals =
            self.get_or_take::<tables::BlockWithdrawals, TAKE>(range.clone())?;

        let block_tx = self.get_take_block_transaction_range::<TAKE>(range.clone())?;

        if TAKE {
            // rm HeaderTD
            self.get_or_take::<tables::HeaderTD, TAKE>(range)?;
            // rm HeaderNumbers
            let mut header_number_cursor = self.tx.cursor_write::<tables::HeaderNumbers>()?;
            for (_, hash) in block_header_hashes.iter() {
                if header_number_cursor.seek_exact(*hash)?.is_some() {
                    header_number_cursor.delete_current()?;
                }
            }
        }

        // merge all into block
        let block_header_iter = block_headers.into_iter();
        let block_header_hashes_iter = block_header_hashes.into_iter();
        let block_tx_iter = block_tx.into_iter();

        // Ommers can be empty for some blocks
        let mut block_ommers_iter = block_ommers.into_iter();
        let mut block_withdrawals_iter = block_withdrawals.into_iter();
        let mut block_ommers = block_ommers_iter.next();
        let mut block_withdrawals = block_withdrawals_iter.next();

        let mut blocks = Vec::new();
        for ((main_block_number, header), (_, header_hash), (_, tx)) in izip!(
            block_header_iter.into_iter(),
            block_header_hashes_iter.into_iter(),
            block_tx_iter.into_iter()
        ) {
            let header = header.seal(header_hash);

            let (body, senders) = tx.into_iter().map(|tx| tx.to_components()).unzip();

            // Ommers can be missing
            let mut ommers = Vec::new();
            if let Some((block_number, _)) = block_ommers.as_ref() {
                if *block_number == main_block_number {
                    ommers = block_ommers.take().unwrap().1.ommers;
                    block_ommers = block_ommers_iter.next();
                }
            };

            // withdrawal can be missing
            let shanghai_is_active =
                chain_spec.fork(Hardfork::Shanghai).active_at_timestamp(header.timestamp);
            let mut withdrawals = Some(Vec::new());
            if shanghai_is_active {
                if let Some((block_number, _)) = block_withdrawals.as_ref() {
                    if *block_number == main_block_number {
                        withdrawals = Some(block_withdrawals.take().unwrap().1.withdrawals);
                        block_withdrawals = block_withdrawals_iter.next();
                    }
                }
            } else {
                withdrawals = None
            }

            blocks.push(SealedBlockWithSenders {
                block: SealedBlock { header, body, ommers, withdrawals },
                senders,
            })
        }

        Ok(blocks)
    }

    /// Update all pipeline sync stage progress.
    pub fn update_pipeline_stages(
        &self,
        block_number: BlockNumber,
        drop_stage_checkpoint: bool,
    ) -> std::result::Result<(), TransactionError> {
        // iterate over all existing stages in the table and update its progress.
        let mut cursor = self.tx.cursor_write::<tables::SyncStage>()?;
        while let Some((stage_name, checkpoint)) = cursor.next()? {
            cursor.upsert(
                stage_name,
                StageCheckpoint {
                    block_number,
                    ..if drop_stage_checkpoint { Default::default() } else { checkpoint }
                },
            )?
        }

        Ok(())
    }

    /// Insert storage change index to database. Used inside StorageHistoryIndex stage
    pub fn insert_storage_history_index(
        &self,
        storage_transitions: BTreeMap<(Address, H256), Vec<u64>>,
    ) -> std::result::Result<(), TransactionError> {
        for ((address, storage_key), mut indices) in storage_transitions {
            let mut last_shard = self.take_last_storage_shard(address, storage_key)?;
            last_shard.append(&mut indices);

            // chunk indices and insert them in shards of N size.
            let mut chunks = last_shard
                .iter()
                .chunks(storage_sharded_key::NUM_OF_INDICES_IN_SHARD)
                .into_iter()
                .map(|chunks| chunks.map(|i| *i as usize).collect::<Vec<usize>>())
                .collect::<Vec<_>>();
            let last_chunk = chunks.pop();

            // chunk indices and insert them in shards of N size.
            chunks.into_iter().try_for_each(|list| {
                self.tx.put::<tables::StorageHistory>(
                    StorageShardedKey::new(
                        address,
                        storage_key,
                        *list.last().expect("Chuck does not return empty list") as BlockNumber,
                    ),
                    BlockNumberList::new(list).expect("Indices are presorted and not empty"),
                )
            })?;
            // Insert last list with u64::MAX
            if let Some(last_list) = last_chunk {
                self.tx.put::<tables::StorageHistory>(
                    StorageShardedKey::new(address, storage_key, u64::MAX),
                    BlockNumberList::new(last_list).expect("Indices are presorted and not empty"),
                )?;
            }
        }
        Ok(())
    }

    /// Insert account change index to database. Used inside AccountHistoryIndex stage
    pub fn insert_account_history_index(
        &self,
        account_transitions: BTreeMap<Address, Vec<u64>>,
    ) -> std::result::Result<(), TransactionError> {
        // insert indexes to AccountHistory.
        for (address, mut indices) in account_transitions {
            let mut last_shard = self.take_last_account_shard(address)?;
            last_shard.append(&mut indices);
            // chunk indices and insert them in shards of N size.
            let mut chunks = last_shard
                .iter()
                .chunks(sharded_key::NUM_OF_INDICES_IN_SHARD)
                .into_iter()
                .map(|chunks| chunks.map(|i| *i as usize).collect::<Vec<usize>>())
                .collect::<Vec<_>>();
            let last_chunk = chunks.pop();

            chunks.into_iter().try_for_each(|list| {
                self.tx.put::<tables::AccountHistory>(
                    ShardedKey::new(
                        address,
                        *list.last().expect("Chuck does not return empty list") as BlockNumber,
                    ),
                    BlockNumberList::new(list).expect("Indices are presorted and not empty"),
                )
            })?;
            // Insert last list with u64::MAX
            if let Some(last_list) = last_chunk {
                self.tx.put::<tables::AccountHistory>(
                    ShardedKey::new(address, u64::MAX),
                    BlockNumberList::new(last_list).expect("Indices are presorted and not empty"),
                )?
            }
        }
        Ok(())
    }

    /// Get the stage checkpoint.
    pub fn get_stage_checkpoint(
        &self,
        id: StageId,
    ) -> std::result::Result<Option<StageCheckpoint>, DatabaseError> {
        get_stage_checkpoint(&self.tx, id)
    }

    /// Save stage checkpoint.
    pub fn save_stage_checkpoint(
        &self,
        id: StageId,
        checkpoint: StageCheckpoint,
    ) -> std::result::Result<(), DatabaseError> {
        self.tx.put::<tables::SyncStage>(id.to_string(), checkpoint)
    }

    /// Get stage checkpoint progress.
    pub fn get_stage_checkpoint_progress(
        &self,
        id: StageId,
    ) -> std::result::Result<Option<Vec<u8>>, DatabaseError> {
        self.tx.get::<tables::SyncStageProgress>(id.to_string())
    }

    /// Save stage checkpoint progress.
    pub fn save_stage_checkpoint_progress(
        &self,
        id: StageId,
        checkpoint: Vec<u8>,
    ) -> std::result::Result<(), DatabaseError> {
        self.tx.put::<tables::SyncStageProgress>(id.to_string(), checkpoint)
    }

    /// Get lastest block number.
    pub fn tip_number(&self) -> std::result::Result<u64, DatabaseError> {
        Ok(self.tx.cursor_read::<tables::CanonicalHeaders>()?.last()?.unwrap_or_default().0)
    }

    /// Query [tables::CanonicalHeaders] table for block hash by block number
    pub fn get_block_hash(
        &self,
        block_number: BlockNumber,
    ) -> std::result::Result<BlockHash, TransactionError> {
        let hash = self
            .tx
            .get::<tables::CanonicalHeaders>(block_number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(block_number.into()))?;
        Ok(hash)
    }

    /// Query the block body by number.
    pub fn block_body_indices(
        &self,
        number: BlockNumber,
    ) -> std::result::Result<StoredBlockBodyIndices, TransactionError> {
        let body = self
            .tx
            .get::<tables::BlockBodyIndices>(number)?
            .ok_or(ProviderError::BlockBodyIndicesNotFound(number))?;
        Ok(body)
    }

    /// Query the block header by number
    pub fn get_header(&self, number: BlockNumber) -> std::result::Result<Header, TransactionError> {
        let header = self
            .tx
            .get::<tables::Headers>(number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(number.into()))?;
        Ok(header)
    }

    /// Get the total difficulty for a block.
    pub fn get_td(&self, block: BlockNumber) -> std::result::Result<U256, TransactionError> {
        let td = self
            .tx
            .get::<tables::HeaderTD>(block)?
            .ok_or(ProviderError::TotalDifficultyNotFound { number: block })?;
        Ok(td.into())
    }

    /// Unwind table by some number key.
    /// Returns number of rows unwound.
    ///
    /// Note: Key is not inclusive and specified key would stay in db.
    #[inline]
    pub fn unwind_table_by_num<T>(&self, num: u64) -> std::result::Result<usize, DatabaseError>
    where
        T: Table<Key = u64>,
    {
        self.unwind_table::<T, _>(num, |key| key)
    }

    /// Unwind the table to a provided number key.
    /// Returns number of rows unwound.
    ///
    /// Note: Key is not inclusive and specified key would stay in db.
    pub(crate) fn unwind_table<T, F>(
        &self,
        key: u64,
        mut selector: F,
    ) -> std::result::Result<usize, DatabaseError>
    where
        T: Table,
        F: FnMut(T::Key) -> u64,
    {
        let mut cursor = self.tx.cursor_write::<T>()?;
        let mut reverse_walker = cursor.walk_back(None)?;
        let mut deleted = 0;

        while let Some(Ok((entry_key, _))) = reverse_walker.next() {
            if selector(entry_key.clone()) <= key {
                break
            }
            reverse_walker.delete_current()?;
            deleted += 1;
        }

        Ok(deleted)
    }

    /// Unwind a table forward by a [Walker][reth_db::abstraction::cursor::Walker] on another table
    pub fn unwind_table_by_walker<T1, T2>(
        &self,
        start_at: T1::Key,
    ) -> std::result::Result<(), DatabaseError>
    where
        T1: Table,
        T2: Table<Key = T1::Value>,
    {
        let mut cursor = self.tx.cursor_write::<T1>()?;
        let mut walker = cursor.walk(Some(start_at))?;
        while let Some((_, value)) = walker.next().transpose()? {
            self.tx.delete::<T2>(value, None)?;
        }
        Ok(())
    }

    /// Load last shard and check if it is full and remove if it is not. If list is empty, last
    /// shard was full or there is no shards at all.
    fn take_last_account_shard(
        &self,
        address: Address,
    ) -> std::result::Result<Vec<u64>, TransactionError> {
        let mut cursor = self.tx.cursor_read::<tables::AccountHistory>()?;
        let last = cursor.seek_exact(ShardedKey::new(address, u64::MAX))?;
        if let Some((shard_key, list)) = last {
            // delete old shard so new one can be inserted.
            self.tx.delete::<tables::AccountHistory>(shard_key, None)?;
            let list = list.iter(0).map(|i| i as u64).collect::<Vec<_>>();
            return Ok(list)
        }
        Ok(Vec::new())
    }

    /// Load last shard and check if it is full and remove if it is not. If list is empty, last
    /// shard was full or there is no shards at all.
    pub fn take_last_storage_shard(
        &self,
        address: Address,
        storage_key: H256,
    ) -> std::result::Result<Vec<u64>, TransactionError> {
        let mut cursor = self.tx.cursor_read::<tables::StorageHistory>()?;
        let last = cursor.seek_exact(StorageShardedKey::new(address, storage_key, u64::MAX))?;
        if let Some((storage_shard_key, list)) = last {
            // delete old shard so new one can be inserted.
            self.tx.delete::<tables::StorageHistory>(storage_shard_key, None)?;
            let list = list.iter(0).map(|i| i as u64).collect::<Vec<_>>();
            return Ok(list)
        }
        Ok(Vec::new())
    }
    /// iterate over storages and insert them to hashing table
    pub fn insert_storage_for_hashing(
        &self,
        storages: impl IntoIterator<Item = (Address, impl IntoIterator<Item = (H256, U256)>)>,
    ) -> std::result::Result<(), TransactionError> {
        // hash values
        let hashed = storages.into_iter().fold(BTreeMap::new(), |mut map, (address, storage)| {
            let storage = storage.into_iter().fold(BTreeMap::new(), |mut map, (key, value)| {
                map.insert(keccak256(key), value);
                map
            });
            map.insert(keccak256(address), storage);
            map
        });

        let mut hashed_storage = self.tx.cursor_dup_write::<tables::HashedStorage>()?;
        // Hash the address and key and apply them to HashedStorage (if Storage is None
        // just remove it);
        hashed.into_iter().try_for_each(|(hashed_address, storage)| {
            storage.into_iter().try_for_each(
                |(key, value)| -> std::result::Result<(), TransactionError> {
                    if hashed_storage
                        .seek_by_key_subkey(hashed_address, key)?
                        .filter(|entry| entry.key == key)
                        .is_some()
                    {
                        hashed_storage.delete_current()?;
                    }

                    if value != U256::ZERO {
                        hashed_storage.upsert(hashed_address, StorageEntry { key, value })?;
                    }
                    Ok(())
                },
            )
        })?;
        Ok(())
    }

    /// iterate over accounts and insert them to hashing table
    pub fn insert_account_for_hashing(
        &self,
        accounts: impl IntoIterator<Item = (Address, Option<Account>)>,
    ) -> std::result::Result<(), TransactionError> {
        let mut hashed_accounts = self.tx.cursor_write::<tables::HashedAccount>()?;

        let hashes_accounts = accounts.into_iter().fold(
            BTreeMap::new(),
            |mut map: BTreeMap<H256, Option<Account>>, (address, account)| {
                map.insert(keccak256(address), account);
                map
            },
        );

        hashes_accounts.into_iter().try_for_each(
            |(hashed_address, account)| -> std::result::Result<(), TransactionError> {
                if let Some(account) = account {
                    hashed_accounts.upsert(hashed_address, account)?
                } else if hashed_accounts.seek_exact(hashed_address)?.is_some() {
                    hashed_accounts.delete_current()?;
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    /// Append blocks and insert its post state.
    /// This will insert block data to all related tables and will update pipeline progress.
    pub fn append_blocks_with_post_state(
        &mut self,
        blocks: Vec<SealedBlockWithSenders>,
        state: PostState,
    ) -> std::result::Result<(), TransactionError> {
        if blocks.is_empty() {
            return Ok(())
        }
        let new_tip = blocks.last().unwrap();
        let new_tip_number = new_tip.number;

        let first_number = blocks.first().unwrap().number;

        let last = blocks.last().unwrap();
        let last_block_number = last.number;
        let last_block_hash = last.hash();
        let expected_state_root = last.state_root;

        // Insert the blocks
        for block in blocks {
            let (block, senders) = block.into_components();
            insert_canonical_block(self.tx_mut(), block, Some(senders))?;
        }

        // Write state and changesets to the database.
        // Must be written after blocks because of the receipt lookup.
        state.write_to_db(self.tx_mut())?;

        self.insert_hashes(first_number..=last_block_number, last_block_hash, expected_state_root)?;

        self.calculate_history_indices(first_number..=last_block_number)?;

        // Update pipeline progress
        self.update_pipeline_stages(new_tip_number, false)?;

        Ok(())
    }

    /// Insert full block and make it canonical.
    pub fn insert_block(
        &mut self,
        block: SealedBlock,
        senders: Option<Vec<Address>>,
    ) -> std::result::Result<(), TransactionError> {
        insert_canonical_block(self.tx_mut(), block, senders)?;
        Ok(())
    }

    /// Read account/storage changesets and update account/storage history indices.
    pub fn calculate_history_indices(
        &mut self,
        range: RangeInclusive<BlockNumber>,
    ) -> std::result::Result<(), TransactionError> {
        // account history stage
        {
            let indices = self.get_account_transition_ids_from_changeset(range.clone())?;
            self.insert_account_history_index(indices)?;
        }

        // storage history stage
        {
            let indices = self.get_storage_transition_ids_from_changeset(range)?;
            self.insert_storage_history_index(indices)?;
        }

        Ok(())
    }

    /// Calculate the hashes of all changed accounts and storages, and finally calculate the state
    /// root.
    ///
    /// The chain goes from `fork_block_number + 1` to `current_block_number`, and hashes are
    /// calculated from `from_transition_id` to `to_transition_id`.
    ///
    /// The resulting state root is compared with `expected_state_root`.
    pub fn insert_hashes(
        &mut self,
        range: RangeInclusive<BlockNumber>,
        end_block_hash: H256,
        expected_state_root: H256,
    ) -> std::result::Result<(), TransactionError> {
        // storage hashing stage
        {
            let lists = self.get_addresses_and_keys_of_changed_storages(range.clone())?;
            let storages = self.get_plainstate_storages(lists.into_iter())?;
            self.insert_storage_for_hashing(storages.into_iter())?;
        }

        // account hashing stage
        {
            let lists = self.get_addresses_of_changed_accounts(range.clone())?;
            let accounts = self.get_plainstate_accounts(lists.into_iter())?;
            self.insert_account_for_hashing(accounts.into_iter())?;
        }

        // merkle tree
        {
            let (state_root, trie_updates) =
                StateRoot::incremental_root_with_updates(&self.tx, range.clone())?;
            if state_root != expected_state_root {
                return Err(TransactionError::StateRootMismatch {
                    got: state_root,
                    expected: expected_state_root,
                    block_number: *range.end(),
                    block_hash: end_block_hash,
                })
            }
            trie_updates.flush(&self.tx)?;
        }
        Ok(())
    }
}

impl<'this, TX: DbTx<'this>> AccountProvider for DatabaseProvider<'this, TX> {
    fn basic_account(&self, address: Address) -> Result<Option<Account>> {
        Ok(self.tx.get::<tables::PlainAccountState>(address)?)
    }
}

impl<'this, TX: DbTx<'this>> AccountExtProvider for DatabaseProvider<'this, TX> {
    fn changed_accounts_with_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> Result<BTreeSet<Address>> {
        self.tx
            .cursor_read::<tables::AccountChangeSet>()?
            .walk_range(range)?
            .map(|entry| {
                entry.map(|(_, account_before)| account_before.address).map_err(Into::into)
            })
            .collect()
    }

    fn basic_accounts(
        &self,
        iter: impl IntoIterator<Item = Address>,
    ) -> Result<Vec<(Address, Option<Account>)>> {
        let mut plain_accounts = self.tx.cursor_read::<tables::PlainAccountState>()?;
        Ok(iter
            .into_iter()
            .map(|address| plain_accounts.seek_exact(address).map(|a| (address, a.map(|(_, v)| v))))
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

impl<'this, TX: DbTx<'this>> HeaderProvider for DatabaseProvider<'this, TX> {
    fn header(&self, block_hash: &BlockHash) -> Result<Option<Header>> {
        if let Some(num) = self.block_number(*block_hash)? {
            Ok(self.header_by_number(num)?)
        } else {
            Ok(None)
        }
    }

    fn header_by_number(&self, num: BlockNumber) -> Result<Option<Header>> {
        Ok(self.tx.get::<tables::Headers>(num)?)
    }

    fn header_td(&self, block_hash: &BlockHash) -> Result<Option<U256>> {
        if let Some(num) = self.block_number(*block_hash)? {
            self.header_td_by_number(num)
        } else {
            Ok(None)
        }
    }

    fn header_td_by_number(&self, number: BlockNumber) -> Result<Option<U256>> {
        if let Some(td) = self.chain_spec.final_paris_difficulty(number) {
            // if this block is higher than the final paris(merge) block, return the final paris
            // difficulty
            return Ok(Some(td))
        }

        Ok(self.tx.get::<tables::HeaderTD>(number)?.map(|td| td.0))
    }

    fn headers_range(&self, range: impl RangeBounds<BlockNumber>) -> Result<Vec<Header>> {
        let mut cursor = self.tx.cursor_read::<tables::Headers>()?;
        cursor
            .walk_range(range)?
            .map(|result| result.map(|(_, header)| header).map_err(Into::into))
            .collect::<Result<Vec<_>>>()
    }

    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> Result<Vec<SealedHeader>> {
        let mut headers = vec![];
        for entry in self.tx.cursor_read::<tables::Headers>()?.walk_range(range)? {
            let (number, header) = entry?;
            let hash = self
                .block_hash(number)?
                .ok_or_else(|| ProviderError::HeaderNotFound(number.into()))?;
            headers.push(header.seal(hash));
        }
        Ok(headers)
    }

    fn sealed_header(&self, number: BlockNumber) -> Result<Option<SealedHeader>> {
        if let Some(header) = self.header_by_number(number)? {
            let hash = self
                .block_hash(number)?
                .ok_or_else(|| ProviderError::HeaderNotFound(number.into()))?;
            Ok(Some(header.seal(hash)))
        } else {
            Ok(None)
        }
    }
}

impl<'this, TX: DbTx<'this>> BlockHashProvider for DatabaseProvider<'this, TX> {
    fn block_hash(&self, number: u64) -> Result<Option<H256>> {
        Ok(self.tx.get::<tables::CanonicalHeaders>(number)?)
    }

    fn canonical_hashes_range(&self, start: BlockNumber, end: BlockNumber) -> Result<Vec<H256>> {
        let range = start..end;
        let mut cursor = self.tx.cursor_read::<tables::CanonicalHeaders>()?;
        cursor
            .walk_range(range)?
            .map(|result| result.map(|(_, hash)| hash).map_err(Into::into))
            .collect::<Result<Vec<_>>>()
    }
}

impl<'this, TX: DbTx<'this>> BlockNumProvider for DatabaseProvider<'this, TX> {
    fn chain_info(&self) -> Result<ChainInfo> {
        let best_number = self.best_block_number()?;
        let best_hash = self.block_hash(best_number)?.unwrap_or_default();
        Ok(ChainInfo { best_hash, best_number })
    }

    fn best_block_number(&self) -> Result<BlockNumber> {
        Ok(self
            .get_stage_checkpoint(StageId::Finish)?
            .map(|checkpoint| checkpoint.block_number)
            .unwrap_or_default())
    }

    fn last_block_number(&self) -> Result<BlockNumber> {
        Ok(self.tx.cursor_read::<tables::CanonicalHeaders>()?.last()?.unwrap_or_default().0)
    }

    fn block_number(&self, hash: H256) -> Result<Option<BlockNumber>> {
        Ok(self.tx.get::<tables::HeaderNumbers>(hash)?)
    }
}

impl<'this, TX: DbTx<'this>> BlockProvider for DatabaseProvider<'this, TX> {
    fn find_block_by_hash(&self, hash: H256, source: BlockSource) -> Result<Option<Block>> {
        if source.is_database() {
            self.block(hash.into())
        } else {
            Ok(None)
        }
    }

    fn block(&self, id: BlockHashOrNumber) -> Result<Option<Block>> {
        if let Some(number) = self.convert_hash_or_number(id)? {
            if let Some(header) = self.header_by_number(number)? {
                let withdrawals = self.withdrawals_by_block(number.into(), header.timestamp)?;
                let ommers = if withdrawals.is_none() { self.ommers(number.into())? } else { None }
                    .unwrap_or_default();
                let transactions = self
                    .transactions_by_block(number.into())?
                    .ok_or(ProviderError::BlockBodyIndicesNotFound(number))?;

                return Ok(Some(Block { header, body: transactions, ommers, withdrawals }))
            }
        }

        Ok(None)
    }

    fn pending_block(&self) -> Result<Option<SealedBlock>> {
        Ok(None)
    }

    fn ommers(&self, id: BlockHashOrNumber) -> Result<Option<Vec<Header>>> {
        if let Some(number) = self.convert_hash_or_number(id)? {
            // TODO: this can be optimized to return empty Vec post-merge
            let ommers = self.tx.get::<tables::BlockOmmers>(number)?.map(|o| o.ommers);
            return Ok(ommers)
        }

        Ok(None)
    }

    fn block_body_indices(&self, num: u64) -> Result<Option<StoredBlockBodyIndices>> {
        Ok(self.tx.get::<tables::BlockBodyIndices>(num)?)
    }

    /// Returns the block with senders with matching number from database.
    ///
    /// **NOTE: The transactions have invalid hashes, since they would need to be calculated on the
    /// spot, and we want fast querying.**
    ///
    /// Returns `None` if block is not found.
    fn block_with_senders(&self, block_number: BlockNumber) -> Result<Option<BlockWithSenders>> {
        let header = self
            .header_by_number(block_number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(block_number.into()))?;

        let ommers = self.ommers(block_number.into())?.unwrap_or_default();
        let withdrawals = self.withdrawals_by_block(block_number.into(), header.timestamp)?;

        // Get the block body
        let body = self
            .block_body_indices(block_number)?
            .ok_or(ProviderError::BlockBodyIndicesNotFound(block_number))?;
        let tx_range = body.tx_num_range();

        let (transactions, senders) = if tx_range.is_empty() {
            (vec![], vec![])
        } else {
            (self.transactions_by_tx_range(tx_range.clone())?, self.senders_by_tx_range(tx_range)?)
        };

        let body = transactions
            .into_iter()
            .map(|tx| {
                TransactionSigned {
                    // TODO: This is the fastest way right now to make everything just work with
                    // a dummy transaction hash.
                    hash: Default::default(),
                    signature: tx.signature,
                    transaction: tx.transaction,
                }
            })
            .collect();

        Ok(Some(Block { header, body, ommers, withdrawals }.with_senders(senders)))
    }
}

impl<'this, TX: DbTx<'this>> TransactionsProvider for DatabaseProvider<'this, TX> {
    fn transaction_id(&self, tx_hash: TxHash) -> Result<Option<TxNumber>> {
        Ok(self.tx.get::<tables::TxHashNumber>(tx_hash)?)
    }

    fn transaction_by_id(&self, id: TxNumber) -> Result<Option<TransactionSigned>> {
        Ok(self.tx.get::<tables::Transactions>(id)?.map(Into::into))
    }

    fn transaction_by_hash(&self, hash: TxHash) -> Result<Option<TransactionSigned>> {
        if let Some(id) = self.transaction_id(hash)? {
            Ok(self.transaction_by_id(id)?)
        } else {
            Ok(None)
        }
        .map(|tx| tx.map(Into::into))
    }

    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<(TransactionSigned, TransactionMeta)>> {
        let mut transaction_cursor = self.tx.cursor_read::<tables::TransactionBlock>()?;
        if let Some(transaction_id) = self.transaction_id(tx_hash)? {
            if let Some(transaction) = self.transaction_by_id(transaction_id)? {
                if let Some(block_number) =
                    transaction_cursor.seek(transaction_id).map(|b| b.map(|(_, bn)| bn))?
                {
                    if let Some(sealed_header) = self.sealed_header(block_number)? {
                        let (header, block_hash) = sealed_header.split();
                        if let Some(block_body) = self.block_body_indices(block_number)? {
                            // the index of the tx in the block is the offset:
                            // len([start..tx_id])
                            // SAFETY: `transaction_id` is always `>=` the block's first
                            // index
                            let index = transaction_id - block_body.first_tx_num();

                            let meta = TransactionMeta {
                                tx_hash,
                                index,
                                block_hash,
                                block_number,
                                base_fee: header.base_fee_per_gas,
                            };

                            return Ok(Some((transaction, meta)))
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    fn transaction_block(&self, id: TxNumber) -> Result<Option<BlockNumber>> {
        let mut cursor = self.tx.cursor_read::<tables::TransactionBlock>()?;
        Ok(cursor.seek(id)?.map(|(_, bn)| bn))
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> Result<Option<Vec<TransactionSigned>>> {
        let mut tx_cursor = self.tx.cursor_read::<tables::Transactions>()?;
        if let Some(block_number) = self.convert_hash_or_number(id)? {
            if let Some(body) = self.block_body_indices(block_number)? {
                let tx_range = body.tx_num_range();
                return if tx_range.is_empty() {
                    Ok(Some(Vec::new()))
                } else {
                    let transactions = tx_cursor
                        .walk_range(tx_range)?
                        .map(|result| result.map(|(_, tx)| tx.into()))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(Some(transactions))
                }
            }
        }
        Ok(None)
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> Result<Vec<Vec<TransactionSigned>>> {
        let mut results = Vec::new();
        let mut body_cursor = self.tx.cursor_read::<tables::BlockBodyIndices>()?;
        let mut tx_cursor = self.tx.cursor_read::<tables::Transactions>()?;
        for entry in body_cursor.walk_range(range)? {
            let (_, body) = entry?;
            let tx_num_range = body.tx_num_range();
            if tx_num_range.is_empty() {
                results.push(Vec::new());
            } else {
                results.push(
                    tx_cursor
                        .walk_range(tx_num_range)?
                        .map(|result| result.map(|(_, tx)| tx.into()))
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                );
            }
        }
        Ok(results)
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> Result<Vec<TransactionSignedNoHash>> {
        Ok(self
            .tx
            .cursor_read::<tables::Transactions>()?
            .walk_range(range)?
            .map(|entry| entry.map(|tx| tx.1))
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn senders_by_tx_range(&self, range: impl RangeBounds<TxNumber>) -> Result<Vec<Address>> {
        Ok(self
            .tx
            .cursor_read::<tables::TxSenders>()?
            .walk_range(range)?
            .map(|entry| entry.map(|sender| sender.1))
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

impl<'this, TX: DbTx<'this>> ReceiptProvider for DatabaseProvider<'this, TX> {
    fn receipt(&self, id: TxNumber) -> Result<Option<Receipt>> {
        Ok(self.tx.get::<tables::Receipts>(id)?)
    }

    fn receipt_by_hash(&self, hash: TxHash) -> Result<Option<Receipt>> {
        if let Some(id) = self.transaction_id(hash)? {
            self.receipt(id)
        } else {
            Ok(None)
        }
    }

    fn receipts_by_block(&self, block: BlockHashOrNumber) -> Result<Option<Vec<Receipt>>> {
        if let Some(number) = self.convert_hash_or_number(block)? {
            if let Some(body) = self.block_body_indices(number)? {
                let tx_range = body.tx_num_range();
                return if tx_range.is_empty() {
                    Ok(Some(Vec::new()))
                } else {
                    let mut tx_cursor = self.tx.cursor_read::<tables::Receipts>()?;
                    let transactions = tx_cursor
                        .walk_range(tx_range)?
                        .map(|result| result.map(|(_, tx)| tx))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(Some(transactions))
                }
            }
        }
        Ok(None)
    }
}

impl<'this, TX: DbTx<'this>> WithdrawalsProvider for DatabaseProvider<'this, TX> {
    fn withdrawals_by_block(
        &self,
        id: BlockHashOrNumber,
        timestamp: u64,
    ) -> Result<Option<Vec<Withdrawal>>> {
        if self.chain_spec.is_shanghai_activated_at_timestamp(timestamp) {
            if let Some(number) = self.convert_hash_or_number(id)? {
                // If we are past shanghai, then all blocks should have a withdrawal list, even if
                // empty
                let withdrawals = self
                    .tx
                    .get::<tables::BlockWithdrawals>(number)
                    .map(|w| w.map(|w| w.withdrawals))?
                    .unwrap_or_default();
                return Ok(Some(withdrawals))
            }
        }
        Ok(None)
    }

    fn latest_withdrawal(&self) -> Result<Option<Withdrawal>> {
        let latest_block_withdrawal = self.tx.cursor_read::<tables::BlockWithdrawals>()?.last()?;
        Ok(latest_block_withdrawal
            .and_then(|(_, mut block_withdrawal)| block_withdrawal.withdrawals.pop()))
    }
}

impl<'this, TX: DbTx<'this>> EvmEnvProvider for DatabaseProvider<'this, TX> {
    fn fill_env_at(
        &self,
        cfg: &mut CfgEnv,
        block_env: &mut BlockEnv,
        at: BlockHashOrNumber,
    ) -> Result<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;
        self.fill_env_with_header(cfg, block_env, &header)
    }

    fn fill_env_with_header(
        &self,
        cfg: &mut CfgEnv,
        block_env: &mut BlockEnv,
        header: &Header,
    ) -> Result<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        fill_cfg_and_block_env(cfg, block_env, &self.chain_spec, header, total_difficulty);
        Ok(())
    }

    fn fill_block_env_at(&self, block_env: &mut BlockEnv, at: BlockHashOrNumber) -> Result<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;

        self.fill_block_env_with_header(block_env, &header)
    }

    fn fill_block_env_with_header(&self, block_env: &mut BlockEnv, header: &Header) -> Result<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        let spec_id = revm_spec(
            &self.chain_spec,
            Head {
                number: header.number,
                timestamp: header.timestamp,
                difficulty: header.difficulty,
                total_difficulty,
                // Not required
                hash: Default::default(),
            },
        );
        let after_merge = spec_id >= SpecId::MERGE;
        fill_block_env(block_env, &self.chain_spec, header, after_merge);
        Ok(())
    }

    fn fill_cfg_env_at(&self, cfg: &mut CfgEnv, at: BlockHashOrNumber) -> Result<()> {
        let hash = self.convert_number(at)?.ok_or(ProviderError::HeaderNotFound(at))?;
        let header = self.header(&hash)?.ok_or(ProviderError::HeaderNotFound(at))?;
        self.fill_cfg_env_with_header(cfg, &header)
    }

    fn fill_cfg_env_with_header(&self, cfg: &mut CfgEnv, header: &Header) -> Result<()> {
        let total_difficulty = self
            .header_td_by_number(header.number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        fill_cfg_env(cfg, &self.chain_spec, header, total_difficulty);
        Ok(())
    }
}

impl<'this, TX: DbTx<'this>> StageCheckpointProvider for DatabaseProvider<'this, TX> {
    fn get_stage_checkpoint(&self, id: StageId) -> Result<Option<StageCheckpoint>> {
        Ok(self.tx.get::<tables::SyncStage>(id.to_string())?)
    }
}
