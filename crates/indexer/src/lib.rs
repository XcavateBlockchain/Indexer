//! The Xcavate whitelist indexer.
//!
//! Module map:
//!
//! | Module | Responsibility |
//! | --- | --- |
//! | [`db`] | Storage layer (Task 2): slot-guarded upserts, idempotent inserts, sync state. |
//! | [`config`] | Environment-driven process configuration. |
//! | [`mapping`] | Pure decoded-instruction -> rows mapping (the port of `mappingHandlers.ts`). |
//! | [`block_time`] | Slot -> block time, cached, with an RPC fallback (ruling R14). |
//! | [`batcher`] | The single writer: buffers `WriteOp`s, commits one batch per transaction. |
//! | [`processors`] | The three carbon processors (instructions, accounts, deletions). |
//! | [`pipeline`] | Datasource wiring: Yellowstone gRPC (live) and bounded RPC crawl windows. |
//! | [`crawl`] | Bounded newest -> oldest signature walks: the completeness mechanism. |
//! | [`snapshot`] | The one-shot `getProgramAccounts` state snapshot. |
//! | [`backfill`] | The resumable history walk down to `backfill_floor_slot`. |
//! | [`reconcile`] | The periodic supervisor: the only writer of `last_contiguous_slot`. |
//! | [`sync_frontier`] | The contiguity contract governing `sync_state.last_contiguous_slot`. |
//! | [`metrics`] | `carbon_core::metrics::Metrics` on Prometheus + the `/metrics` listener. |
//! | [`grpc_smoke`] | The `smoke-grpc` check, reused as `run`'s startup subscribe gate. |
//!
//! Data flows one way: datasource -> carbon decodes -> processor maps -> batcher commits.
//! Processors never touch the database directly and the batcher never decodes anything.
//!
//! The two data paths have distinct jobs, and it is worth saying out loud: the **live gRPC
//! stream is for freshness**, the **crawl is for completeness**. Only the crawl may move
//! `sync_state.last_contiguous_slot`.

pub mod backfill;
pub mod batcher;
pub mod block_time;
pub mod config;
pub mod crawl;
pub mod db;
pub mod grpc_smoke;
pub mod mapping;
pub mod metrics;
pub mod pipeline;
pub mod processors;
pub mod reconcile;
pub mod snapshot;
pub mod sync_frontier;

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod test_fixtures;
