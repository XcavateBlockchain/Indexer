//! `GET /health` -- readiness JSON (task-5-brief.md): `last_contiguous_slot`, `backfill_complete`,
//! `chain_tip_slot` (RPC `getSlot` at `confirmed`, cached <=5s via [`crate::chain_tip`]),
//! `slot_lag` (tip - contiguous), `healthy` (DB reachable). 200 when the DB is reachable, 503
//! otherwise -- the chain-tip RPC read is best-effort and never turns a DB-reachable process
//! unhealthy by itself (an RPC outage should not take a otherwise-fine API server out of a load
//! balancer's rotation).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use crate::state::ApiState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub last_contiguous_slot: Option<i64>,
    pub backfill_complete: bool,
    pub chain_tip_slot: Option<i64>,
    pub slot_lag: Option<i64>,
    pub healthy: bool,
}

pub async fn health(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    // Fleet aggregates across the per-program sync rows: the stack is only as caught-up as
    // its laggiest program, so `last_contiguous_slot` is the minimum and `backfill_complete`
    // is true only when every program's backfill is complete. Scoped by the optional
    // `PROGRAMS` filter so subset operation (which freezes the excluded programs' rows) does
    // not drag the aggregates -- see `Config::programs`.
    let sync_state = sqlx::query!(
        r#"
        SELECT min(last_contiguous_slot) AS "last_contiguous_slot",
               bool_and(backfill_complete) AS "backfill_complete"
        FROM sync_state
        WHERE ($1::bytea[] IS NULL OR program_id = ANY($1))
        "#,
        state.program_filter.as_deref(),
    )
    .fetch_one(&state.pool)
    .await;

    let (last_contiguous_slot, backfill_complete, db_reachable) = match sync_state {
        // The aggregates are NULL on a fresh, pre-init database (no rows) -- still reachable.
        Ok(row) => (
            row.last_contiguous_slot,
            row.backfill_complete.unwrap_or(false),
            true,
        ),
        Err(e) => {
            log::warn!("health: DB query failed: {e:#}");
            (None, false, false)
        }
    };

    // Best-effort: an RPC hiccup shouldn't flip a DB-healthy process to 503.
    let chain_tip_slot = state.chain_tip.get().await.ok().map(|s| s as i64);
    let slot_lag = match (chain_tip_slot, last_contiguous_slot) {
        (Some(tip), Some(contiguous)) => Some((tip - contiguous).max(0)),
        _ => None,
    };

    let body = HealthResponse {
        last_contiguous_slot,
        backfill_complete,
        chain_tip_slot,
        slot_lag,
        healthy: db_reachable,
    };

    let status = if db_reachable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body))
}
