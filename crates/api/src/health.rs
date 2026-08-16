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
    let sync_state = sqlx::query!(
        r#"SELECT last_contiguous_slot, backfill_complete FROM sync_state WHERE id = 1"#
    )
    .fetch_optional(&state.pool)
    .await;

    let (last_contiguous_slot, backfill_complete, db_reachable) = match sync_state {
        Ok(Some(row)) => (Some(row.last_contiguous_slot), row.backfill_complete, true),
        // The DB answered, just with no rows yet (fresh, pre-init database) -- still reachable.
        Ok(None) => (None, false, true),
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
