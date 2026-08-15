//! Indexer binary. The storage layer (`indexer::db`: migrations runner + slot-guarded
//! upserts + append-only inserts + sync-state helpers) landed in Phase 2 as this crate's
//! library target. The pipeline itself (datasources, processors that call into `db`) lands
//! in Phase 3 and will be wired up here.

fn main() {}
