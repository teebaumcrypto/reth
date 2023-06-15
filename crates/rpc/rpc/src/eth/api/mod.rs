//! Provides everything related to `eth_` namespace
//!
//! The entire implementation of the namespace is quite large, hence it is divided across several
//! files.

use crate::eth::{
    cache::EthStateCache,
    error::{EthApiError, EthResult},
    gas_oracle::GasPriceOracle,
    signer::EthSigner,
};
use async_trait::async_trait;
use reth_interfaces::Result;
use reth_network_api::NetworkInfo;
use reth_primitives::{Address, BlockId, BlockNumberOrTag, ChainInfo, H256, U256, U64};
use reth_provider::{BlockProviderIdExt, EvmEnvProvider, StateProviderBox, StateProviderFactory};
use reth_rpc_types::{FeeHistoryCache, SyncInfo, SyncStatus};
use reth_tasks::{TaskSpawner, TokioTaskExecutor};
use reth_transaction_pool::TransactionPool;
use std::{future::Future, num::NonZeroUsize, sync::Arc};
use tokio::sync::oneshot;

mod block;
mod call;
mod fees;
mod server;
mod sign;
mod state;
mod transactions;

pub use transactions::{EthTransactions, TransactionSource};

/// Cache limit of block-level fee history for `eth_feeHistory` RPC method.
const FEE_HISTORY_CACHE_LIMIT: usize = 2048;

/// `Eth` API trait.
///
/// Defines core functionality of the `eth` API implementation.
#[async_trait]
pub trait EthApiSpec: EthTransactions + Send + Sync {
    /// Returns the current ethereum protocol version.
    async fn protocol_version(&self) -> Result<U64>;

    /// Returns the chain id
    fn chain_id(&self) -> U64;

    /// Returns provider chain info
    fn chain_info(&self) -> Result<ChainInfo>;

    /// Returns a list of addresses owned by provider.
    fn accounts(&self) -> Vec<Address>;

    /// Returns `true` if the network is undergoing sync.
    fn is_syncing(&self) -> bool;

    /// Returns the [SyncStatus] of the network
    fn sync_status(&self) -> Result<SyncStatus>;
}

/// `Eth` API implementation.
///
/// This type provides the functionality for handling `eth_` related requests.
/// These are implemented two-fold: Core functionality is implemented as [EthApiSpec]
/// trait. Additionally, the required server implementations (e.g. [`reth_rpc_api::EthApiServer`])
/// are implemented separately in submodules. The rpc handler implementation can then delegate to
/// the main impls. This way [`EthApi`] is not limited to [`jsonrpsee`] and can be used standalone
/// or in other network handlers (for example ipc).
pub struct EthApi<Provider, Pool, Network> {
    /// All nested fields bundled together.
    inner: Arc<EthApiInner<Provider, Pool, Network>>,
}

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Provider: BlockProviderIdExt,
{
    /// Creates a new, shareable instance using the default tokio task spawner.
    pub fn new(
        provider: Provider,
        pool: Pool,
        network: Network,
        eth_cache: EthStateCache,
        gas_oracle: GasPriceOracle<Provider>,
    ) -> Self {
        Self::with_spawner(
            provider,
            pool,
            network,
            eth_cache,
            gas_oracle,
            Box::<TokioTaskExecutor>::default(),
        )
    }

    /// Creates a new, shareable instance.
    pub fn with_spawner(
        provider: Provider,
        pool: Pool,
        network: Network,
        eth_cache: EthStateCache,
        gas_oracle: GasPriceOracle<Provider>,
        task_spawner: Box<dyn TaskSpawner>,
    ) -> Self {
        // get the block number of the latest block
        let latest_block = provider
            .header_by_number_or_tag(BlockNumberOrTag::Latest)
            .ok()
            .flatten()
            .map(|header| header.number)
            .unwrap_or_default();

        let inner = EthApiInner {
            provider,
            pool,
            network,
            signers: Default::default(),
            eth_cache,
            gas_oracle,
            starting_block: U256::from(latest_block),
            task_spawner,
            fee_history_cache: FeeHistoryCache::new(
                NonZeroUsize::new(FEE_HISTORY_CACHE_LIMIT).unwrap(),
            ),
        };
        Self { inner: Arc::new(inner) }
    }

    /// Executes the future on a new blocking task.
    ///
    /// This accepts a closure that creates a new future using a clone of this type and spawns the
    /// future onto a new task that is allowed to block.
    pub(crate) async fn on_blocking_task<C, F, R>(&self, c: C) -> EthResult<R>
    where
        C: FnOnce(Self) -> F,
        F: Future<Output = EthResult<R>> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let this = self.clone();
        let f = c(this);
        self.inner.task_spawner.spawn_blocking(Box::pin(async move {
            let res = f.await;
            let _ = tx.send(res);
        }));
        rx.await.map_err(|_| EthApiError::InternalEthError)?
    }

    /// Returns the state cache frontend
    pub(crate) fn cache(&self) -> &EthStateCache {
        &self.inner.eth_cache
    }

    /// Returns the gas oracle frontend
    pub(crate) fn gas_oracle(&self) -> &GasPriceOracle<Provider> {
        &self.inner.gas_oracle
    }

    /// Returns the inner `Provider`
    pub fn provider(&self) -> &Provider {
        &self.inner.provider
    }

    /// Returns the inner `Network`
    pub fn network(&self) -> &Network {
        &self.inner.network
    }

    /// Returns the inner `Pool`
    pub fn pool(&self) -> &Pool {
        &self.inner.pool
    }
}

// === State access helpers ===

impl<Provider, Pool, Network> EthApi<Provider, Pool, Network>
where
    Provider: BlockProviderIdExt + StateProviderFactory + EvmEnvProvider + 'static,
{
    fn convert_block_number(&self, num: BlockNumberOrTag) -> Result<Option<u64>> {
        self.provider().convert_block_number(num)
    }

    /// Returns the state at the given [BlockId] enum.
    pub fn state_at_block_id(&self, at: BlockId) -> EthResult<StateProviderBox<'_>> {
        match at {
            BlockId::Hash(hash) => Ok(self.state_at_hash(hash.into())?),
            BlockId::Number(num) => {
                self.state_at_block_number(num)?.ok_or(EthApiError::UnknownBlockNumber)
            }
        }
    }

    /// Returns the state at the given [BlockId] enum or the latest.
    pub fn state_at_block_id_or_latest(
        &self,
        block_id: Option<BlockId>,
    ) -> EthResult<StateProviderBox<'_>> {
        if let Some(block_id) = block_id {
            self.state_at_block_id(block_id)
        } else {
            Ok(self.latest_state()?)
        }
    }

    /// Returns the state at the given [BlockNumberOrTag] enum
    ///
    /// Returns `None` if no state available.
    pub fn state_at_block_number(
        &self,
        num: BlockNumberOrTag,
    ) -> Result<Option<StateProviderBox<'_>>> {
        if let Some(number) = self.convert_block_number(num)? {
            self.state_at_number(number).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Returns the state at the given block number
    pub fn state_at_hash(&self, block_hash: H256) -> Result<StateProviderBox<'_>> {
        self.provider().history_by_block_hash(block_hash)
    }

    /// Returns the state at the given block number
    pub fn state_at_number(&self, block_number: u64) -> Result<StateProviderBox<'_>> {
        match self.convert_block_number(BlockNumberOrTag::Latest)? {
            Some(num) if num == block_number => self.latest_state(),
            _ => self.provider().history_by_block_number(block_number),
        }
    }

    /// Returns the _latest_ state
    pub fn latest_state(&self) -> Result<StateProviderBox<'_>> {
        self.provider().latest()
    }
}

impl<Provider, Pool, Events> std::fmt::Debug for EthApi<Provider, Pool, Events> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EthApi").finish_non_exhaustive()
    }
}

impl<Provider, Pool, Events> Clone for EthApi<Provider, Pool, Events> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

#[async_trait]
impl<Provider, Pool, Network> EthApiSpec for EthApi<Provider, Pool, Network>
where
    Pool: TransactionPool + Clone + 'static,
    Provider: BlockProviderIdExt + StateProviderFactory + EvmEnvProvider + 'static,
    Network: NetworkInfo + 'static,
{
    /// Returns the current ethereum protocol version.
    ///
    /// Note: This returns an `U64`, since this should return as hex string.
    async fn protocol_version(&self) -> Result<U64> {
        let status = self.network().network_status().await?;
        Ok(U64::from(status.protocol_version))
    }

    /// Returns the chain id
    fn chain_id(&self) -> U64 {
        U64::from(self.network().chain_id())
    }

    /// Returns the current info for the chain
    fn chain_info(&self) -> Result<ChainInfo> {
        self.provider().chain_info()
    }

    fn accounts(&self) -> Vec<Address> {
        self.inner.signers.iter().flat_map(|s| s.accounts()).collect()
    }

    fn is_syncing(&self) -> bool {
        self.network().is_syncing()
    }

    /// Returns the [SyncStatus] of the network
    fn sync_status(&self) -> Result<SyncStatus> {
        let status = if self.is_syncing() {
            let current_block = U256::from(
                self.provider().chain_info().map(|info| info.best_number).unwrap_or_default(),
            );
            SyncStatus::Info(SyncInfo {
                starting_block: self.inner.starting_block,
                current_block,
                highest_block: current_block,
                warp_chunks_amount: None,
                warp_chunks_processed: None,
            })
        } else {
            SyncStatus::None
        };
        Ok(status)
    }
}

/// Container type `EthApi`
struct EthApiInner<Provider, Pool, Network> {
    /// The transaction pool.
    pool: Pool,
    /// The provider that can interact with the chain.
    provider: Provider,
    /// An interface to interact with the network
    network: Network,
    /// All configured Signers
    signers: Vec<Box<dyn EthSigner>>,
    /// The async cache frontend for eth related data
    eth_cache: EthStateCache,
    /// The async gas oracle frontend for gas price suggestions
    gas_oracle: GasPriceOracle<Provider>,
    /// The block number at which the node started
    starting_block: U256,
    /// The type that can spawn tasks which would otherwise block.
    task_spawner: Box<dyn TaskSpawner>,
    /// The cache for fee history entries,
    fee_history_cache: FeeHistoryCache,
}
