//! Resolving a slot's block time (ruling R14).
//!
//! `program_instructions.block_time` and `whitelist_actions.block_time` are both `NOT NULL`,
//! and the old SubQuery rows always had a real timestamp, so parity means we must never
//! invent one. The Yellowstone transaction stream does not carry it: the datasource passes
//! `block_time: None` for `UpdateOneof::Transaction` (only the far heavier block subscription
//! has it), which lands as `TransactionMetadata::block_time == None`. The RPC crawler, by
//! contrast, gets it for free from `getTransaction`.
//!
//! So: use the stream's value when present, otherwise ask an RPC node for `getBlockTime(slot)`
//! and cache it. Blocks in a slot are shared by every instruction in every transaction in that
//! slot, so the cache turns a burst of updates into a single lookup.
//!
//! On failure this retries (primary endpoint, then the public fallback, alternating with
//! exponential backoff) and finally errors rather than substituting a guess -- the caller
//! turns that into a failed update, which is loud and re-processable, instead of a silently
//! wrong timestamp that would be indistinguishable from a correct one forever after.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use tokio::sync::Mutex;

/// How many slot -> timestamp pairs to keep. A slot is ~400 ms, so this is a bit over an hour
/// of devnet history -- far more than any realistic in-flight batch needs, and bounded so a
/// long-running process cannot grow this map without limit.
const CACHE_CAPACITY: usize = 8_192;

/// Total attempts across both endpoints before giving up on a slot.
const MAX_ATTEMPTS: u32 = 10;

pub struct BlockTimeResolver {
    primary: Arc<RpcClient>,
    fallback: Arc<RpcClient>,
    cache: Mutex<SlotCache>,
}

impl BlockTimeResolver {
    /// `primary_url` is normally Alchemy; `fallback_url` the public devnet endpoint, used when
    /// the primary errors (throttling is the expected reason -- see MIGRATION_LOG.md).
    pub fn new(primary_url: &str, fallback_url: &str) -> Self {
        Self {
            primary: Arc::new(RpcClient::new_with_commitment(
                primary_url.to_string(),
                CommitmentConfig::confirmed(),
            )),
            fallback: Arc::new(RpcClient::new_with_commitment(
                fallback_url.to_string(),
                CommitmentConfig::confirmed(),
            )),
            cache: Mutex::new(SlotCache::with_capacity(CACHE_CAPACITY)),
        }
    }

    /// `hint` is `TransactionMetadata::block_time` -- `Some` from the RPC crawler, `None` from
    /// the Yellowstone transaction stream.
    pub async fn resolve(&self, slot: u64, hint: Option<i64>) -> Result<DateTime<Utc>> {
        if let Some(ts) = hint {
            crate::metrics::inc_block_time_lookup("stream");
            // Populate the cache from the hint too: other instructions in the same slot that
            // arrive without one then cost nothing.
            self.cache.lock().await.insert(slot, ts);
            return to_datetime(slot, ts);
        }

        if let Some(ts) = self.cache.lock().await.get(slot) {
            crate::metrics::inc_block_time_lookup("cache");
            return to_datetime(slot, ts);
        }

        let mut backoff = Duration::from_millis(250);
        let mut last_err = None;
        for attempt in 0..MAX_ATTEMPTS {
            // Alternate endpoints so a throttled primary does not consume every attempt.
            let use_fallback = attempt % 2 == 1;
            let client = if use_fallback {
                &self.fallback
            } else {
                &self.primary
            };

            match client.get_block_time(slot).await {
                Ok(ts) => {
                    crate::metrics::inc_block_time_lookup(if use_fallback {
                        "rpc_fallback"
                    } else {
                        "rpc"
                    });
                    self.cache.lock().await.insert(slot, ts);
                    return to_datetime(slot, ts);
                }
                Err(e) => {
                    log::warn!(
                        "getBlockTime({slot}) failed on the {} endpoint (attempt {}/{MAX_ATTEMPTS}): {e}",
                        if use_fallback { "fallback" } else { "primary" },
                        attempt + 1,
                    );
                    last_err = Some(e);
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(10));
                }
            }
        }

        Err(anyhow!(
            "could not resolve block_time for slot {slot} after {MAX_ATTEMPTS} attempts; \
             refusing to write a row with a guessed timestamp. Last error: {}",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "none".to_string())
        ))
    }
}

fn to_datetime(slot: u64, ts: i64) -> Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .ok_or_else(|| anyhow!("slot {slot} reported an out-of-range unix timestamp: {ts}"))
}

/// Insertion-ordered bounded map. Not an LRU on purpose: slot lookups arrive roughly in slot
/// order, so first-in-first-out evicts exactly the entries that will never be asked for again,
/// and it costs one `VecDeque` push instead of a reordering per hit.
struct SlotCache {
    map: HashMap<u64, i64>,
    order: VecDeque<u64>,
    capacity: usize,
}

impl SlotCache {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn get(&self, slot: u64) -> Option<i64> {
        self.map.get(&slot).copied()
    }

    fn insert(&mut self, slot: u64, ts: i64) {
        if self.map.insert(slot, ts).is_some() {
            return; // already tracked in `order`
        }
        self.order.push_back(slot);
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SlotCache;

    #[test]
    fn cache_evicts_oldest_and_does_not_double_track_repeats() {
        let mut c = SlotCache::with_capacity(3);
        c.insert(1, 100);
        c.insert(2, 200);
        c.insert(1, 111); // repeat: must not push a second `order` entry
        c.insert(3, 300);
        assert_eq!(c.get(1), Some(111));
        assert_eq!(c.get(2), Some(200));
        assert_eq!(c.get(3), Some(300));

        c.insert(4, 400); // evicts slot 1, the oldest *insertion*
        assert_eq!(c.get(1), None);
        assert_eq!(c.get(4), Some(400));
        assert_eq!(c.map.len(), 3);
        assert_eq!(c.order.len(), 3);
    }
}
