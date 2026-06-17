//! Background blockhash refresh for transaction generators.
//!
//! The updater polls RPC and publishes blockhashes through a [`tokio::sync::watch`]
//! channel. By default it publishes the freshest blockhash. When configured with a
//! non-zero `stale_slots`, it instead publishes the blockhash of a block that many
//! slots behind the tip (fetched via `getBlock`), so generated transactions carry a
//! near-expiry blockhash and exercise the scheduler's discard-on-age path.

use {
    log::*,
    solana_hash::Hash,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcBlockConfig,
    solana_transaction_status::TransactionDetails,
    std::sync::Arc,
    thiserror::Error,
    tokio::{
        sync::watch,
        time::{self, Duration, Instant},
    },
};

/// How many slots below the target to search for a produced block, in case the exact
/// target slot was skipped. Generous enough to span any realistic run of skipped slots
/// while keeping the `getBlocks` range cheap (it returns slot numbers only).
const STALE_BLOCK_SEARCH_WINDOW: u64 = 150;

/// Config was introduced for test purposes.
#[derive(Clone, Copy, Debug)]
struct BlockhashUpdaterConfig {
    /// How often request the new blockhash.
    update_interval: Duration,
    /// If we fail to update blockhash for this period of time, we give up and report error.
    stuck_interval: Duration,
    /// When start to report warnings about blockhash not updating.
    not_updating_interval: Duration,
    /// How often to report blockhash errors if any.
    error_report_interval: Duration,
}

impl Default for BlockhashUpdaterConfig {
    fn default() -> Self {
        Self {
            update_interval: Duration::from_secs(1),
            stuck_interval: Duration::from_secs(120),
            not_updating_interval: Duration::from_secs(30),
            error_report_interval: Duration::from_secs(1),
        }
    }
}

#[derive(Error, Debug, PartialEq, Eq, Clone, Copy)]
pub enum BlockhashUpdaterError {
    /// The blockhash did not change within the configured stuck interval.
    #[error("Blockhash is stuck.")]
    BlockhashStuck,
}

/// Polls RPC for blockhashes and publishes them to a watch channel.
///
/// With `stale_slots == 0` it publishes the latest blockhash. With `stale_slots > 0` it
/// publishes the blockhash of a block roughly `stale_slots` behind the current slot,
/// re-fetched each tick so the published blockhash stays a constant number of slots old as
/// the tip advances.
///
/// The updater exits when all receivers for the watch channel have been dropped, or returns
/// [`BlockhashUpdaterError::BlockhashStuck`] if RPC keeps failing (or returns the same
/// blockhash) for too long.
pub struct BlockhashUpdater {
    rpc_client: Arc<RpcClient>,
    sender: watch::Sender<Hash>,
    config: BlockhashUpdaterConfig,
    last_blockhash: Hash,
    /// Publish a blockhash this many slots (blocks) behind the tip; `0` means freshest.
    stale_slots: u64,
}

impl BlockhashUpdater {
    /// Creates a blockhash updater that always publishes the freshest blockhash.
    pub fn new(rpc_client: Arc<RpcClient>, sender: watch::Sender<Hash>) -> Self {
        Self::with_stale_slots(rpc_client, sender, 0)
    }

    /// Creates a blockhash updater that publishes the blockhash of a block roughly
    /// `stale_slots` behind the tip (via `getBlock`). `stale_slots == 0` is equivalent to
    /// [`BlockhashUpdater::new`]. The target RPC must serve `getBlock`/`getBlocks`.
    pub fn with_stale_slots(
        rpc_client: Arc<RpcClient>,
        sender: watch::Sender<Hash>,
        stale_slots: u64,
    ) -> Self {
        Self {
            rpc_client,
            sender,
            config: BlockhashUpdaterConfig::default(),
            last_blockhash: Hash::default(),
            stale_slots,
        }
    }

    #[cfg(test)]
    fn with_config(
        rpc_client: Arc<RpcClient>,
        sender: watch::Sender<Hash>,
        config: BlockhashUpdaterConfig,
    ) -> Self {
        Self {
            rpc_client,
            sender,
            config,
            last_blockhash: Hash::default(),
            stale_slots: 0,
        }
    }

    /// Runs the updater until the watch channel is closed or the blockhash is
    /// considered stuck.
    pub async fn run(mut self) -> Result<(), BlockhashUpdaterError> {
        let mut blockhash_last_updated = Instant::now();
        let mut last_error_log = Instant::now();
        let mut interval = time::interval(self.config.update_interval);
        while !self.sender.is_closed() {
            interval.tick().await;

            let fetched = if self.stale_slots == 0 {
                self.rpc_client.get_latest_blockhash().await.ok()
            } else {
                match self.fetch_aged_blockhash().await {
                    Ok(hash) => Some(hash),
                    Err(err) => {
                        // Surface at warn (not debug): a persistent failure here means we keep
                        // publishing nothing and the generator falls back to the seed blockhash,
                        // which silently defeats the staleness. Stuck detection still trips.
                        warn!("Failed to fetch aged blockhash: {err}");
                        None
                    }
                }
            };

            if let Some(new_blockhash) = fetched
                && new_blockhash != self.last_blockhash
            {
                self.last_blockhash = new_blockhash;
                if self.sender.send(new_blockhash).is_err() {
                    break;
                }
                blockhash_last_updated = Instant::now();
            }
            if blockhash_last_updated.elapsed() > self.config.stuck_interval {
                return Err(BlockhashUpdaterError::BlockhashStuck);
            } else if blockhash_last_updated.elapsed() > self.config.not_updating_interval
                && last_error_log.elapsed() >= self.config.error_report_interval
            {
                last_error_log = Instant::now();
                let last_updated_s = blockhash_last_updated.elapsed().as_secs();
                warn!("Blockhash is not updating for {last_updated_s} s.");
            }
        }
        Ok(())
    }

    /// One-shot check that the aged-blockhash path works, for failing fast at startup.
    ///
    /// A no-op when `stale_slots == 0`. Otherwise performs a single fetch and returns a
    /// descriptive error (e.g. when the RPC does not serve `getBlock`) so the caller can
    /// abort instead of silently running with fresh blockhashes.
    pub async fn check_stale_blockhash_available(&self) -> Result<(), String> {
        if self.stale_slots == 0 {
            return Ok(());
        }
        self.fetch_aged_blockhash().await.map(|_| ())
    }

    /// Fetches the blockhash of a block roughly `stale_slots` behind the current slot, so
    /// signed transactions carry a near-expiry blockhash. Returns a descriptive error on any
    /// RPC failure; in the run loop the caller logs it and treats it like a missed update
    /// (stuck detection still applies), while startup uses it to fail fast.
    async fn fetch_aged_blockhash(&self) -> Result<Hash, String> {
        let current_slot = self
            .rpc_client
            .get_slot()
            .await
            .map_err(|err| format!("getSlot failed: {err}"))?;
        let target_slot = current_slot.saturating_sub(self.stale_slots);
        // The target slot may have been skipped; find the newest produced block at or below
        // it via `getBlocks`, which returns only the slots that actually produced a block.
        let start_slot = target_slot.saturating_sub(STALE_BLOCK_SEARCH_WINDOW);
        let blocks = self
            .rpc_client
            .get_blocks(start_slot, Some(target_slot))
            .await
            .map_err(|err| {
                format!(
                    "getBlocks({start_slot}..={target_slot}) failed: {err}. The target RPC must \
                     serve getBlock/getBlocks; run the validator with \
                     --enable-rpc-transaction-history."
                )
            })?;
        let block_slot = *blocks
            .last()
            .ok_or_else(|| format!("no produced block in [{start_slot}, {target_slot}]"))?;
        // Fetch only the block header (no transactions) since we just need its blockhash.
        let config = RpcBlockConfig {
            transaction_details: Some(TransactionDetails::None),
            rewards: Some(false),
            max_supported_transaction_version: Some(0),
            ..RpcBlockConfig::default()
        };
        let block = self
            .rpc_client
            .get_block_with_config(block_slot, config)
            .await
            .map_err(|err| {
                format!(
                    "getBlock({block_slot}) failed: {err}. The target RPC must serve getBlock; \
                     run the validator with --enable-rpc-transaction-history."
                )
            })?;
        block
            .blockhash
            .parse::<Hash>()
            .map_err(|err| format!("failed to parse blockhash of slot {block_slot}: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        serde_json::{self, json},
        solana_rpc_client::mock_sender::PUBKEY,
        solana_rpc_client_api::{
            request::RpcRequest,
            response::{Response, RpcBlockhash, RpcResponseContext},
        },
        solana_sha256_hasher::hash,
        std::collections::HashMap,
        tokio::{sync::watch, task, time::sleep},
    };

    // Lower than default values to avoid long running unit tests.
    fn test_config() -> BlockhashUpdaterConfig {
        BlockhashUpdaterConfig {
            update_interval: Duration::from_millis(200),
            stuck_interval: Duration::from_millis(600),
            not_updating_interval: Duration::from_millis(300),
            error_report_interval: Duration::from_millis(1000),
        }
    }

    #[tokio::test]
    async fn test_blockhash_updates_successfully() {
        let rpc_blockhash = hash(&[1u8]);
        let mut mocks = HashMap::new();
        mocks.insert(
            RpcRequest::GetLatestBlockhash,
            json!(Response {
                context: RpcResponseContext {
                    slot: 1,
                    api_version: None
                },
                value: json!(RpcBlockhash {
                    blockhash: rpc_blockhash.to_string(),
                    last_valid_block_height: 42,
                }),
            }),
        );
        let rpc_client = Arc::new(RpcClient::new_mock_with_mocks("".to_string(), mocks));
        let (sender, receiver) = watch::channel(Hash::default());
        let updater_config = test_config();
        let updater = BlockhashUpdater::with_config(rpc_client, sender, updater_config);
        let handle = task::spawn(async move { updater.run().await });
        // sleep to let updater task entering the update loop.
        sleep(updater_config.update_interval / 2).await;
        let blockhash = *receiver.borrow();
        assert_eq!(rpc_blockhash, blockhash);
        drop(receiver);
        let result = handle.await.expect("task should not panic.");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_blockhash_updates_stuck() {
        let rpc_client = Arc::new(RpcClient::new_mock("fails".to_string()));
        let (sender, receiver) = watch::channel(Hash::default());
        let updater = BlockhashUpdater::with_config(rpc_client, sender, test_config());
        let handle = task::spawn(async move { updater.run().await });
        assert_eq!(*receiver.borrow(), Hash::default());
        let err = handle.await.expect("task should not panic.");
        assert_eq!(err, Err(BlockhashUpdaterError::BlockhashStuck));
    }

    #[tokio::test]
    async fn test_blockhash_updates_stuck_recover() {
        // MockRpcClient will first return specified response and later will always send
        // the same PUBKEY hash.
        let expected_blockhash: Hash = PUBKEY.parse().unwrap();
        let mut mocks = HashMap::new();
        mocks.insert(
            RpcRequest::GetLatestBlockhash,
            json!(Response {
                context: RpcResponseContext {
                    slot: 1,
                    api_version: None
                },
                value: serde_json::value::Value::Null,
            }),
        );
        let rpc_client = Arc::new(RpcClient::new_mock_with_mocks("".to_string(), mocks));
        let (sender, receiver) = watch::channel(Hash::default());
        let updater_config = test_config();
        let updater = BlockhashUpdater::with_config(rpc_client, sender, updater_config);
        let handle = task::spawn(async move { updater.run().await });
        // sleep to let updater task entering the update loop.
        sleep(updater_config.update_interval / 2).await;
        let blockhash = *receiver.borrow();
        assert_eq!(
            Hash::default(),
            blockhash,
            "Cannot update blockhash because rpc_client returns Null."
        );

        sleep(updater_config.update_interval).await;
        let blockhash = *receiver.borrow();
        assert_eq!(expected_blockhash, blockhash);
        drop(receiver);
        let result = handle.await.expect("task should not panic.");
        assert!(result.is_ok());
    }
}
