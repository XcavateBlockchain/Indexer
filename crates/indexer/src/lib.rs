//! Indexer library: the storage layer (`db`) built in Phase 2 -- migrations runner,
//! slot-guarded upserts, append-only inserts, and sync-state helpers. The pipeline
//! (datasources, processors that call into `db`) lands in Phase 3 and will live in this
//! crate too, wired up from `main.rs`.

pub mod db;
