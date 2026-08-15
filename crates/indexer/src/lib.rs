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
//! | [`pipeline`] | Datasource wiring: Yellowstone gRPC (live) and RPC crawler (replay). |
//! | [`sync_frontier`] | The contiguity contract governing `sync_state.last_contiguous_slot`. |
//! | [`metrics`] | `carbon_core::metrics::Metrics` on Prometheus + the `/metrics` listener. |
//! | [`grpc_smoke`] | The `smoke-grpc` connectivity check. |
//!
//! Data flows one way: datasource -> carbon decodes -> processor maps -> batcher commits.
//! Processors never touch the database directly and the batcher never decodes anything.

pub mod batcher;
pub mod block_time;
pub mod config;
pub mod db;
pub mod grpc_smoke;
pub mod mapping;
pub mod metrics;
pub mod pipeline;
pub mod processors;
pub mod sync_frontier;

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod test_fixtures;
