//! Cross-program DERIVED reads (ADR-40): `claimableIncome` and `pendingFinalizations`.
//!
//! Unlike the per-program modules, nothing here mirrors one account table: these two fields
//! compute, server-side and against one wall-clock reading, exactly what the mobile app's
//! C# reference model (`XcavateMarketplaceIndexerModel.cs`: `PropertyIncomeState.GetClaimable`
//! and `EvaluatePendingFinalizations`) computes client-side from the raw mirror columns. The
//! SQL below only reads the existing mirror tables (property's income ledger / checkpoint /
//! letting / proposal / challenge / candidacy, marketplace's share holding / listing /
//! property asset / lawyer candidacy / config); every gate is a pure function of the fetched
//! snapshots (`evaluate_pending_finalizations`, `claimable`) so the boundary semantics are
//! unit-testable with a frozen clock, and the DB-seeded tests in `super::super::tests` only
//! need to prove the wiring. Semantics were verified against the on-chain handlers
//! (`realxmarket-solana` `property/src/instructions/income.rs` `bank`/`sync_entries`,
//! `marketplace/src/instructions/election.rs` `finalize_spv_election_handler`).

use carbon_core::graphql::primitives::I64;
use juniper::{FieldResult, GraphQLObject};
use sqlx::PgPool;

use super::{b58, parse_b58};
use crate::graphql::context::GraphQLContext;
use crate::graphql::enums::{unknown_enum_value, DocumentStatus, ListingStatus};

/// The on-chain "seat vacant" sentinel: the all-zero pubkey, stored verbatim in the mirror
/// (see migration 0010's `property_letting` note; base58 `11111111111111111111111111111111`).
const VACANT_SEAT: [u8; 32] = [0u8; 32];

/// One wall clock per request (the ADR-39 `feedStates` precedent): every gate below reads
/// the same `now`, so two cranks can never disagree about a deadline crossing mid-request.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ============================================================================================
// `claimableIncome` — one holder's per-mint claimable income on one property.
//
// The on-chain settlement (`income.rs` `bank`): per stream i,
// `accrued = (streams[i].per_share - entries[i].per_share) * shares`, banked into
// `entries[i].pending`; there is NO fixed-point divisor (SCALE = 1). `per_share` is u128
// on-chain and stored as a decimal string in the JSONB `streams`/`entries` columns
// (migration 0012), so the math here is u128 throughout and the results ship as decimal
// STRINGS (`claimableBaseUnits` / `pendingBaseUnits`) — GraphQL `Int` is i32 and the
// string-serialized `I64` scalar cannot carry a u128 delta.
// ============================================================================================

/// One income stream's settlement state for one holder, in BASE UNITS of the mint.
/// `claimableBaseUnits` = `(per_share delta since the holder's checkpoint) * shares +
/// banked pending`; `pendingBaseUnits` = the banked part alone (what `claim_income` pays
/// even for a zero holding).
#[derive(GraphQLObject, Clone, Debug)]
pub struct ClaimableIncome {
    /// The stream's payment mint (base58).
    pub mint: String,
    /// Total claimable right now, base units, decimal string (u128-safe).
    pub claimable_base_units: String,
    /// The banked `pending` part of `claimableBaseUnits`, base units, decimal string.
    pub pending_base_units: String,
}

/// One parsed income stream: the mint and its u128 per-share accumulator.
#[derive(Debug, Clone)]
struct StreamCursor {
    mint: String,
    per_share: u128,
}

/// One parsed checkpoint entry: the settled `per_share` cursor and the banked `pending`.
#[derive(Debug, Clone, Copy)]
struct EntryCursor {
    per_share: u128,
    pending: u64,
}

/// A u128 stored as a decimal string (migration 0012's JSONB shape). An unparseable value
/// reads as 0 — the C# reference's `ParseBigInteger` fallback; the indexer writes these
/// columns itself from decoded accounts, so a bad string means data corruption upstream,
/// which the lenient read treats as "no accrual recorded" rather than inventing a debt.
fn parse_u128_string(value: Option<&serde_json::Value>) -> u128 {
    value
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<u128>().ok())
        .unwrap_or(0)
}

/// The ledger's raw `streams` JSONB (`[{"mint": "<base58>", "per_share": "<decimal>", "dust":
/// N}, ...]`). Malformed input (not an array) reads as no streams — C# `ParseIncomeStreams`;
/// entries with no usable `mint` are skipped, likewise.
fn parse_streams(value: &serde_json::Value) -> Vec<StreamCursor> {
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let mint = row.get("mint")?.as_str().filter(|m| !m.is_empty())?;
            Some(StreamCursor {
                mint: mint.to_owned(),
                per_share: parse_u128_string(row.get("per_share")),
            })
        })
        .collect()
}

/// One checkpoint's raw `entries` JSONB (`[{"per_share": "<decimal>", "pending": N}, ...]`),
/// entries[i] tracking streams[i]. Malformed input reads as no entries — every cursor zero —
/// C# `ParseIncomeCheckpoint`'s "never claimed" state.
fn parse_entries(value: &serde_json::Value) -> Vec<EntryCursor> {
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .map(|row| EntryCursor {
            per_share: parse_u128_string(row.get("per_share")),
            pending: row
                .get("pending")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        })
        .collect()
}

/// The settlement formula for one stream: `(stream.per_share - entry.per_share) * shares +
/// entry.pending`, entries pairing with streams by index and a missing entry reading as a
/// zero cursor (`sync_entries`). `per_share` is monotone on-chain, so a checkpoint row that
/// outran the ledger is unreachable; the delta CLAMPS to 0 rather than reporting a debt
/// (the C# reference's explicit clamp). The saturating u128 mul/add are likewise defensive:
/// on-chain the accrued total fits a u64 (`bank` `try_into`s it), so a real ledger cannot
/// reach the saturation bound.
fn claimable(stream: &StreamCursor, entry: Option<&EntryCursor>, shares: u64) -> Claimable {
    let (cursor, pending) = entry.map(|e| (e.per_share, e.pending)).unwrap_or((0, 0));
    let delta = stream.per_share.saturating_sub(cursor);
    let accrued = delta.saturating_mul(shares as u128);
    Claimable {
        total: accrued.saturating_add(pending as u128),
        pending,
    }
}

/// One stream's outcome: the total claimable and its banked-pending part.
struct Claimable {
    total: u128,
    pending: u64,
}

pub async fn claimable_income(
    context: &GraphQLContext,
    owner: String,
    asset_id: I64,
) -> FieldResult<Vec<ClaimableIncome>> {
    let owner = parse_b58("owner", &owner)?;

    // The property's income ledger: one active row per property, created by the first
    // `distribute_income`, never closed. No row (or no usable streams) = nothing claimable.
    let income = sqlx::query!(
        r#"
        SELECT streams FROM property_income
        WHERE asset_id = $1 AND closed_at_slot IS NULL
        "#,
        asset_id.0,
    )
    .fetch_optional(&context.pool)
    .await?;
    let streams = income
        .as_ref()
        .map(|r| parse_streams(&r.streams))
        .unwrap_or_default();
    if streams.is_empty() {
        return Ok(Vec::new());
    }

    // The holder's current shares (the marketplace program's ShareHolding mirror). No active
    // holding row = zero shares: only banked pending remains claimable (on-chain a closed
    // holding reads the same way — `claim_income_handler`).
    let shares = sqlx::query!(
        r#"
        SELECT amount FROM marketplace_share_holding
        WHERE asset_id = $1 AND owner = $2 AND closed_at_slot IS NULL
        "#,
        asset_id.0,
        owner.as_slice(),
    )
    .fetch_optional(&context.pool)
    .await?
    .map(|r| u64::try_from(r.amount).unwrap_or(0))
    .unwrap_or(0);

    // The holder's checkpoint; a MISSING row means "held since before every stream opened"
    // — all cursors zero, full accrual from each stream's start (`sync_entries`).
    let entries = sqlx::query!(
        r#"
        SELECT entries FROM property_income_checkpoint
        WHERE asset_id = $1 AND owner = $2 AND closed_at_slot IS NULL
        "#,
        asset_id.0,
        owner.as_slice(),
    )
    .fetch_optional(&context.pool)
    .await?
    .map(|r| parse_entries(&r.entries))
    .unwrap_or_default();

    Ok(streams
        .iter()
        .enumerate()
        .map(|(i, stream)| {
            let c = claimable(stream, entries.get(i), shares);
            ClaimableIncome {
                mint: stream.mint.clone(),
                claimable_base_units: c.total.to_string(),
                pending_base_units: c.pending.to_string(),
            }
        })
        .collect())
}

// ============================================================================================
// `pendingFinalizations` — which of the six finalization cranks one listing/property is
// ready for RIGHT NOW. One object with one nullable (or bool) field per crank, each
// carrying exactly the inputs the matching on-chain instruction derives its accounts from.
// The gates are the C# reference's `EvaluatePendingFinalizations` ported 1:1 (itself a
// mirror of the on-chain `require!`s); the snapshots below are its `*Snapshot` records.
// ============================================================================================

/// An ended, unfinalized spending proposal (`finalize_proposal` is due).
#[derive(GraphQLObject, Clone, Debug)]
pub struct ProposalFinalization {
    /// The proposal's on-chain id (its PDA seed).
    pub proposal_id: I64,
    /// Who fronts the proposal account's rent (refunded by the crank).
    pub rent_payer: String,
}

/// An ended, unfinalized challenge (`finalize_challenge` is due).
#[derive(GraphQLObject, Clone, Debug)]
pub struct ChallengeFinalization {
    pub challenge_id: I64,
    pub rent_payer: String,
    pub challenger: String,
    /// The sitting agent the challenge strikes.
    pub struck_agent: String,
}

/// An ended, unfinalized election round (`finalize_agent_election` /
/// `finalize_spv_election` is due). `winner` is the strict-unique vote-power leader of the
/// round — `null` on a tie at the top (the seat reopens either way, so the crank still runs
/// with no winner account). Quorum and a non-zero tally are enforced ON CHAIN on top of
/// this: a returned winner can still fail assignment and just reopen the round.
#[derive(GraphQLObject, Clone, Debug)]
pub struct ElectionFinalization {
    /// The plurality winner's wallet (base58); `null` on a tie (or no candidacies).
    pub winner: Option<String>,
    /// The settled round (the candidacy PDAs the crank walks are seeded with it).
    pub round: I64,
    /// Every candidate pubkey of the round, strongest first (base58).
    pub candidates: Vec<String>,
}

/// A deal ready to settle (`execute_deal` is due): the listing's parties and region plus
/// the marketplace config's mints/treasury the payout loop derives its accounts from.
#[derive(GraphQLObject, Clone, Debug)]
pub struct ExecuteDealFinalization {
    pub region_id: i32,
    pub developer: String,
    pub developer_lawyer: String,
    pub spv_lawyer: String,
    /// The XCAV mint (deposit release), from the marketplace config.
    pub xcav_mint: String,
    /// The marketplace treasury, from the config.
    pub treasury: String,
    /// The config's accepted payment mints (base58).
    pub accepted_payment_mints: Vec<String>,
    /// The mints the listing actually collected (the per-mint payout loop's iteration set).
    pub collected_mints: Vec<String>,
}

/// Which finalization cranks one listing/property is ready for, evaluated once against a
/// single wall-clock reading (ADR-40). Each field is `null` while its crank's on-chain
/// `require!`s would reject the call; a non-null field carries exactly the account-derivation
/// inputs that crank needs.
#[derive(GraphQLObject, Clone, Debug)]
pub struct PendingFinalizations {
    pub proposal: Option<ProposalFinalization>,
    pub challenge: Option<ChallengeFinalization>,
    /// The property's letting-agent election (property program).
    pub agent_election: Option<ElectionFinalization>,
    /// The listing's SPV-lawyer election (marketplace program).
    pub spv_election: Option<ElectionFinalization>,
    /// `resolve_silent_verdict` is due: SOLD_OUT, the SPV side ruled, the developer's lawyer
    /// is silent, and the clock sits in the review window's final stretch.
    pub silent_verdict_overdue: bool,
    pub execute_deal: Option<ExecuteDealFinalization>,
}

// --- the snapshots (pure inputs; the C# reference's `*Snapshot` records) -------------------

/// The letting seat's governance/election counters (`property_letting`).
#[derive(Debug, Clone, Copy)]
struct LettingSnapshot {
    election_round: i64,
    election_expiry: i64,
    election_candidate_count: i64,
    active_proposal: i64,
    active_challenge: i64,
}

/// The open proposal row (`property_proposal`; the account closes at finalization).
#[derive(Debug, Clone)]
struct ProposalSnapshot {
    proposal_id: i64,
    expiry: i64,
    rent_payer: Vec<u8>,
}

/// The open challenge row (`property_challenge`).
#[derive(Debug, Clone)]
struct ChallengeSnapshot {
    challenge_id: i64,
    expiry: i64,
    rent_payer: Vec<u8>,
    challenger: Vec<u8>,
    struck_agent: Vec<u8>,
}

/// One current-round candidacy with its SUMMED vote power: both programs' candidacy
/// accounts accumulate power as votes land (`vote_on_agent` / `vote_on_lawyer` add to
/// `candidacy.vote_power` — verified in `election.rs`), so no vote-row summing is needed.
#[derive(Debug, Clone)]
struct CandidacyRow {
    candidate: Vec<u8>,
    vote_power: i64,
}

/// The listing's legal-phase fields plus the asset's region (`marketplace_listing` +
/// `marketplace_property_asset`).
#[derive(Debug, Clone)]
struct ListingSnapshot {
    status: ListingStatus,
    legal_deadline: i64,
    legal_process_time: i64,
    developer: Vec<u8>,
    developer_lawyer: Vec<u8>,
    developer_lawyer_doc_status: DocumentStatus,
    spv_lawyer: Vec<u8>,
    spv_lawyer_doc_status: DocumentStatus,
    spv_election_round: i64,
    spv_election_expiry: i64,
    spv_election_candidate_count: i64,
    region_id: Option<i32>,
    collected: serde_json::Value,
}

/// The marketplace singleton config (the mints/treasury the deal crank derives with).
#[derive(Debug, Clone)]
struct ConfigSnapshot {
    xcav_mint: Vec<u8>,
    treasury: Vec<u8>,
    accepted_payment_mints: serde_json::Value,
}

/// The strictly-unique maximum of one round's candidacies (`PickPluralityWinner`, and the
/// same fold `finalize_agent_election` / `finalize_spv_election_handler` run on-chain):
/// equality against the running leader marks a tie, only a later strictly-higher tally
/// clears it, and an empty field has no winner. Order-independent.
fn pick_plurality_winner(candidacies: &[CandidacyRow]) -> Option<Vec<u8>> {
    let mut best: i64 = -1;
    let mut winner: Option<Vec<u8>> = None;
    let mut tied = false;
    for c in candidacies {
        if c.vote_power > best {
            best = c.vote_power;
            winner = Some(c.candidate.clone());
            tied = false;
        } else if c.vote_power == best {
            tied = true;
        }
    }
    if tied {
        None
    } else {
        winner
    }
}

/// The listing's `collected` JSONB (`[{"mint": "<base58>", "funds": N, "fee": N, "tax": N},
/// ...]`, migration 0009) reduced to the payment mints settlement pays out. Malformed input
/// reads as none (C# `ParseCollectedMints`).
fn parse_collected_mints(value: &serde_json::Value) -> Vec<String> {
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| row.get("mint")?.as_str().filter(|m| !m.is_empty()))
        .map(str::to_owned)
        .collect()
}

/// The config's `accepted_payment_mints` JSONB (`["<base58>", ...]`, migration 0009).
/// Malformed input reads as an empty list (C# `ParseStringArray`).
fn parse_string_array(value: &serde_json::Value) -> Vec<String> {
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// The pure gate evaluation (C# `EvaluatePendingFinalizations`, ported 1:1). Every
/// comparison mirrors the on-chain handler's `require!`: proposal/challenge gates are
/// inclusive of their expiry (`now >= expiry`), the silent-verdict window is inclusive on
/// both ends, and `execute_deal` lapses AT `legal_deadline` inclusive (`now <= deadline`).
#[allow(clippy::too_many_arguments)]
fn evaluate_pending_finalizations(
    now: i64,
    letting: Option<&LettingSnapshot>,
    proposal: Option<&ProposalSnapshot>,
    challenge: Option<&ChallengeSnapshot>,
    agent_candidacies: &[CandidacyRow],
    listing: Option<&ListingSnapshot>,
    spv_candidacies: &[CandidacyRow],
    config: Option<&ConfigSnapshot>,
) -> PendingFinalizations {
    let proposal_out = match (letting, proposal) {
        (Some(l), Some(p))
            if l.active_proposal > 0 && p.proposal_id == l.active_proposal && now >= p.expiry =>
        {
            Some(ProposalFinalization {
                proposal_id: I64(p.proposal_id),
                rent_payer: b58(&p.rent_payer),
            })
        }
        _ => None,
    };

    let challenge_out = match (letting, challenge) {
        (Some(l), Some(c))
            if l.active_challenge > 0
                && c.challenge_id == l.active_challenge
                && now >= c.expiry =>
        {
            Some(ChallengeFinalization {
                challenge_id: I64(c.challenge_id),
                rent_payer: b58(&c.rent_payer),
                challenger: b58(&c.challenger),
                struck_agent: b58(&c.struck_agent),
            })
        }
        _ => None,
    };

    let agent_election_out = match letting {
        Some(l)
            if l.election_expiry != 0
                && l.election_candidate_count > 0
                && now >= l.election_expiry =>
        {
            Some(ElectionFinalization {
                winner: pick_plurality_winner(agent_candidacies).map(|w| b58(&w)),
                round: I64(l.election_round),
                candidates: agent_candidacies
                    .iter()
                    .map(|c| b58(&c.candidate))
                    .collect(),
            })
        }
        _ => None,
    };

    let mut spv_election_out = None;
    let mut silent_verdict_overdue = false;
    let mut execute_deal_out = None;
    if let Some(l) = listing {
        if l.spv_election_expiry != 0
            && l.spv_election_candidate_count > 0
            && now >= l.spv_election_expiry
            && l.spv_lawyer.as_slice() == VACANT_SEAT
        {
            spv_election_out = Some(ElectionFinalization {
                winner: pick_plurality_winner(spv_candidacies).map(|w| b58(&w)),
                round: I64(l.spv_election_round),
                candidates: spv_candidacies.iter().map(|c| b58(&c.candidate)).collect(),
            });
        }

        // resolve_silent_verdict: the sale still SOLD_OUT, the SPV side ruled, the
        // developer's side silent, and the clock in the review window's final stretch
        // (`legal_deadline - legal_process_time / 5 <= now <= legal_deadline`; the i64
        // division truncates exactly like the C# long division). Past the deadline the
        // timeout refund path owns the vault and this crank no longer applies.
        let final_stretch = l.legal_deadline - l.legal_process_time / 5;
        silent_verdict_overdue = l.status == ListingStatus::SoldOut
            && l.developer_lawyer_doc_status == DocumentStatus::Pending
            && l.spv_lawyer_doc_status != DocumentStatus::Pending
            && now >= final_stretch
            && now <= l.legal_deadline;

        // execute_deal: LEGAL (both lawyers approved — the status flips exactly then) and
        // the legal window not yet lapsed; the region comes from the asset join and the
        // mints/treasury from the config, both required.
        if l.status == ListingStatus::Legal && now <= l.legal_deadline {
            if let (Some(region_id), Some(config)) = (l.region_id, config) {
                execute_deal_out = Some(ExecuteDealFinalization {
                    // On-chain `region_id` is a u16; the mirror column is INT. Clamp rather
                    // than fail on a hypothetical out-of-range row (C# `Math.Clamp` to ushort).
                    region_id: region_id.clamp(0, u16::MAX as i32),
                    developer: b58(&l.developer),
                    developer_lawyer: b58(&l.developer_lawyer),
                    spv_lawyer: b58(&l.spv_lawyer),
                    xcav_mint: b58(&config.xcav_mint),
                    treasury: b58(&config.treasury),
                    accepted_payment_mints: parse_string_array(&config.accepted_payment_mints),
                    collected_mints: parse_collected_mints(&l.collected),
                });
            }
        }
    }

    PendingFinalizations {
        proposal: proposal_out,
        challenge: challenge_out,
        agent_election: agent_election_out,
        spv_election: spv_election_out,
        silent_verdict_overdue,
        execute_deal: execute_deal_out,
    }
}

// --- the resolver: SQL snapshots -> the pure evaluation ------------------------------------

/// One active `property_letting` row's governance/election counters.
async fn letting_snapshot(pool: &PgPool, asset_id: i64) -> FieldResult<Option<LettingSnapshot>> {
    let row = sqlx::query!(
        r#"
        SELECT election_round, election_expiry, election_candidate_count,
               governance_active_proposal, governance_active_challenge
        FROM property_letting
        WHERE asset_id = $1 AND closed_at_slot IS NULL
        "#,
        asset_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| LettingSnapshot {
        election_round: r.election_round,
        election_expiry: r.election_expiry,
        election_candidate_count: r.election_candidate_count,
        active_proposal: r.governance_active_proposal,
        active_challenge: r.governance_active_challenge,
    }))
}

/// The OPEN proposal row the letting points at (`property_proposal`, keyed `(asset_id,
/// id)`; a closed proposal's PDA is gone, so the open row IS the active one).
async fn proposal_snapshot(
    pool: &PgPool,
    asset_id: i64,
    proposal_id: i64,
) -> FieldResult<Option<ProposalSnapshot>> {
    let row = sqlx::query!(
        r#"
        SELECT id, expiry, rent_payer FROM property_proposal
        WHERE asset_id = $1 AND id = $2 AND closed_at_slot IS NULL
        "#,
        asset_id,
        proposal_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| ProposalSnapshot {
        proposal_id: r.id,
        expiry: r.expiry,
        rent_payer: r.rent_payer,
    }))
}

/// The open challenge row the letting points at (`property_challenge`).
async fn challenge_snapshot(
    pool: &PgPool,
    asset_id: i64,
    challenge_id: i64,
) -> FieldResult<Option<ChallengeSnapshot>> {
    let row = sqlx::query!(
        r#"
        SELECT id, expiry, rent_payer, challenger, agent FROM property_challenge
        WHERE asset_id = $1 AND id = $2 AND closed_at_slot IS NULL
        "#,
        asset_id,
        challenge_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| ChallengeSnapshot {
        challenge_id: r.id,
        expiry: r.expiry,
        rent_payer: r.rent_payer,
        challenger: r.challenger,
        struck_agent: r.agent,
    }))
}

/// One election round's candidacies, strongest first. `slot DESC, pubkey ASC` settles ties
/// in the CANDIDATES list order (the winner fold itself is order-independent); only still-
/// open candidacy PDAs count — a closed one cannot be passed to the on-chain finalize.
async fn agent_candidacy_rows(
    pool: &PgPool,
    asset_id: i64,
    round: i64,
) -> FieldResult<Vec<CandidacyRow>> {
    let rows = sqlx::query!(
        r#"
        SELECT agent, vote_power FROM property_agent_candidacy
        WHERE asset_id = $1 AND round = $2 AND closed_at_slot IS NULL
        ORDER BY vote_power DESC, slot DESC, pubkey ASC
        "#,
        asset_id,
        round,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| CandidacyRow {
            candidate: r.agent,
            vote_power: r.vote_power,
        })
        .collect())
}

/// The SPV election's candidacies (`marketplace_lawyer_candidacy`), same shape and ordering
/// as the agent side.
async fn spv_candidacy_rows(
    pool: &PgPool,
    listing_id: i64,
    round: i64,
) -> FieldResult<Vec<CandidacyRow>> {
    let rows = sqlx::query!(
        r#"
        SELECT lawyer, vote_power FROM marketplace_lawyer_candidacy
        WHERE listing_id = $1 AND round = $2 AND closed_at_slot IS NULL
        ORDER BY vote_power DESC, slot DESC, pubkey ASC
        "#,
        listing_id,
        round,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| CandidacyRow {
            candidate: r.lawyer,
            vote_power: r.vote_power,
        })
        .collect())
}

/// The listing's legal-phase snapshot plus the asset's region (the `listings` resolver's
/// 1:1 LEFT JOIN key, `pa.asset_id = l.asset_id`, read here as its own lookup; the asset
/// side deliberately carries no `active` filter, matching that join).
async fn listing_snapshot(pool: &PgPool, listing_id: i64) -> FieldResult<Option<ListingSnapshot>> {
    let row = sqlx::query!(
        r#"
        SELECT asset_id, status, legal_deadline, legal_process_time, developer,
               developer_lawyer, developer_lawyer_doc_status, spv_lawyer,
               spv_lawyer_doc_status, spv_election_round, spv_election_expiry,
               spv_election_candidate_count, collected
        FROM marketplace_listing
        WHERE listing_id = $1 AND closed_at_slot IS NULL
        "#,
        listing_id,
    )
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else {
        return Ok(None);
    };

    let status = ListingStatus::from_db_str(&r.status)
        .ok_or_else(|| unknown_enum_value("status", &r.status))?;
    let developer_lawyer_doc_status = DocumentStatus::from_db_str(&r.developer_lawyer_doc_status)
        .ok_or_else(|| {
        unknown_enum_value(
            "developer_lawyer_doc_status",
            &r.developer_lawyer_doc_status,
        )
    })?;
    let spv_lawyer_doc_status = DocumentStatus::from_db_str(&r.spv_lawyer_doc_status)
        .ok_or_else(|| unknown_enum_value("spv_lawyer_doc_status", &r.spv_lawyer_doc_status))?;

    let region_id = sqlx::query!(
        r#"
        SELECT region_id FROM marketplace_property_asset
        WHERE asset_id = $1
        "#,
        r.asset_id,
    )
    .fetch_optional(pool)
    .await?
    .map(|a| a.region_id);

    Ok(Some(ListingSnapshot {
        status,
        legal_deadline: r.legal_deadline,
        legal_process_time: r.legal_process_time,
        developer: r.developer,
        developer_lawyer: r.developer_lawyer,
        developer_lawyer_doc_status,
        spv_lawyer: r.spv_lawyer,
        spv_lawyer_doc_status,
        spv_election_round: r.spv_election_round,
        spv_election_expiry: r.spv_election_expiry,
        spv_election_candidate_count: r.spv_election_candidate_count,
        region_id,
        collected: r.collected,
    }))
}

/// The marketplace singleton config (same latest-row pick as the `marketplace_config`
/// resolver); `None` until `initialize_config` has been indexed.
async fn config_snapshot(pool: &PgPool) -> FieldResult<Option<ConfigSnapshot>> {
    let row = sqlx::query!(
        r#"
        SELECT xcav_mint, treasury, accepted_payment_mints
        FROM marketplace_config
        ORDER BY slot DESC
        LIMIT 1
        "#
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| ConfigSnapshot {
        xcav_mint: r.xcav_mint,
        treasury: r.treasury,
        accepted_payment_mints: r.accepted_payment_mints,
    }))
}

pub async fn pending_finalizations(
    context: &GraphQLContext,
    asset_id: I64,
    listing_id: I64,
) -> FieldResult<PendingFinalizations> {
    // One clock reading for the whole evaluation (see `now_unix`); every gate below reads
    // this same value.
    let now = now_unix();

    // Dependent rows are fetched only when their gate's clock-free counters pass — the
    // evaluation cannot reach them otherwise (a `governance_active_*` of 0 means no open
    // proposal/challenge exists to point at; a zero election expiry/count means no round
    // ever opened).
    let letting = letting_snapshot(&context.pool, asset_id.0).await?;
    let proposal = match letting {
        Some(l) if l.active_proposal > 0 => {
            proposal_snapshot(&context.pool, asset_id.0, l.active_proposal).await?
        }
        _ => None,
    };
    let challenge = match letting {
        Some(l) if l.active_challenge > 0 => {
            challenge_snapshot(&context.pool, asset_id.0, l.active_challenge).await?
        }
        _ => None,
    };
    let agent_candidacies = match letting {
        Some(l) if l.election_expiry != 0 && l.election_candidate_count > 0 => {
            agent_candidacy_rows(&context.pool, asset_id.0, l.election_round).await?
        }
        _ => Vec::new(),
    };

    let listing = listing_snapshot(&context.pool, listing_id.0).await?;
    let spv_candidacies = match &listing {
        Some(l) if l.spv_election_expiry != 0 && l.spv_election_candidate_count > 0 => {
            spv_candidacy_rows(&context.pool, listing_id.0, l.spv_election_round).await?
        }
        _ => Vec::new(),
    };
    let config = config_snapshot(&context.pool).await?;

    Ok(evaluate_pending_finalizations(
        now,
        letting.as_ref(),
        proposal.as_ref(),
        challenge.as_ref(),
        &agent_candidacies,
        listing.as_ref(),
        &spv_candidacies,
        config.as_ref(),
    ))
}

// ============================================================================================
// Pure-core unit tests: the boundary semantics with a frozen clock (the DB-seeded wiring
// tests live in `super::super::tests`). These mirror the C# reference's frozen-time tests:
// every gate's on/off edge, the tie rule, the clamps, and the u128 ranges.
// ============================================================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn stream(mint: &str, per_share: u128) -> StreamCursor {
        StreamCursor {
            mint: mint.to_owned(),
            per_share,
        }
    }

    fn entry(per_share: u128, pending: u64) -> EntryCursor {
        EntryCursor { per_share, pending }
    }

    // --- claimable ---------------------------------------------------------------------------

    #[test]
    fn missing_checkpoint_entry_means_full_accrual() {
        // No entry at index 0 (the "held since before the stream opened" read): the whole
        // per_share accrues.
        let s = stream("mint-a", 1_000);
        let c = claimable(&s, None, 5);
        assert_eq!(c.total, 5_000);
        assert_eq!(c.pending, 0);
    }

    #[test]
    fn claimable_is_delta_times_shares_plus_pending() {
        let s = stream("mint-a", 1_000);
        let e = entry(600, 42);
        let c = claimable(&s, Some(&e), 5);
        assert_eq!(c.total, (1_000 - 600) * 5 + 42);
        assert_eq!(c.pending, 42);
    }

    #[test]
    fn a_checkpoint_ahead_of_the_ledger_clamps_to_zero_plus_pending() {
        // Unreachable on-chain (per_share is monotone); the read clamps rather than
        // reporting a debt.
        let s = stream("mint-a", 100);
        let e = entry(300, 7);
        let c = claimable(&s, Some(&e), 5);
        assert_eq!(c.total, 7);
        assert_eq!(c.pending, 7);
    }

    #[test]
    fn zero_shares_leave_only_the_banked_pending() {
        let s = stream("mint-a", 1_000);
        let e = entry(0, 9);
        let c = claimable(&s, Some(&e), 0);
        assert_eq!(c.total, 9);
    }

    #[test]
    fn u128_per_share_beyond_u64_stays_exact() {
        // per_share is u128 on-chain (stored as a decimal string); the math must not
        // truncate at u64::MAX.
        let big = (u64::MAX as u128) + 1_000_000;
        let s = stream("mint-a", big);
        let e = entry(u64::MAX as u128, 0);
        let c = claimable(&s, Some(&e), 3);
        assert_eq!(c.total, 3_000_000);
        // And full accrual of a >u64 cursor.
        let c = claimable(&s, None, 2);
        assert_eq!(c.total, big * 2);
    }

    #[test]
    fn streams_and_entries_parse_the_migration_0012_shapes() {
        let streams = parse_streams(&serde_json::json!([
            {"mint": "mint-a", "per_share": "123", "dust": 4},
            {"mint": "mint-b", "per_share": "340282366920938463463374607431768211455", "dust": 0}
        ]));
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].mint, "mint-a");
        assert_eq!(streams[0].per_share, 123);
        assert_eq!(streams[1].per_share, u128::MAX);

        let entries = parse_entries(&serde_json::json!([
            {"per_share": "100", "pending": 5},
            {"per_share": "0", "pending": 0}
        ]));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].per_share, 100);
        assert_eq!(entries[0].pending, 5);
    }

    #[test]
    fn malformed_json_reads_as_the_empty_state() {
        // The C# reference's lenient read: malformed whole-JSON = no streams / no entries;
        // an unparseable per_share = a zero cursor; a mint-less stream is dropped.
        assert!(parse_streams(&serde_json::json!({"not": "an array"})).is_empty());
        assert!(parse_entries(&serde_json::json!("nope")).is_empty());
        let streams = parse_streams(&serde_json::json!([
            {"mint": "", "per_share": "5"},
            {"mint": "mint-a", "per_share": "not-a-number"},
            {"per_share": "5"}
        ]));
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].mint, "mint-a");
        assert_eq!(streams[0].per_share, 0);
        let entries = parse_entries(&serde_json::json!([{"per_share": null, "pending": "x"}]));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].per_share, 0);
        assert_eq!(entries[0].pending, 0);
    }

    // --- pick_plurality_winner ---------------------------------------------------------------

    fn cand(byte: u8, power: i64) -> CandidacyRow {
        CandidacyRow {
            candidate: vec![byte; 32],
            vote_power: power,
        }
    }

    #[test]
    fn plurality_winner_requires_a_strictly_unique_max() {
        // Empty field: no winner.
        assert_eq!(pick_plurality_winner(&[]), None);
        // Unique leader.
        assert_eq!(
            pick_plurality_winner(&[cand(1, 10), cand(2, 5), cand(3, 1)]),
            Some(vec![1u8; 32])
        );
        // A tie at the top reopens the seat: no winner, whichever way the ties are ordered.
        assert_eq!(
            pick_plurality_winner(&[cand(1, 10), cand(2, 10), cand(3, 1)]),
            None
        );
        assert_eq!(
            pick_plurality_winner(&[cand(1, 5), cand(2, 10), cand(3, 10)]),
            None
        );
        // A later strictly-higher tally clears an earlier tie.
        assert_eq!(
            pick_plurality_winner(&[cand(1, 5), cand(2, 5), cand(3, 6)]),
            Some(vec![3u8; 32])
        );
        // A lone zero-power candidacy still leads (the chain's own fold seats `leader` on
        // the first candidacy; the on-chain top_power > 0 check is the crank's problem —
        // see ElectionFinalization's doc).
        assert_eq!(pick_plurality_winner(&[cand(7, 0)]), Some(vec![7u8; 32]));
        // Two zero-power candidacies tie.
        assert_eq!(pick_plurality_winner(&[cand(7, 0), cand(8, 0)]), None);
    }

    // --- evaluate_pending_finalizations --------------------------------------------------------

    fn letting() -> LettingSnapshot {
        LettingSnapshot {
            election_round: 2,
            election_expiry: 1_000,
            election_candidate_count: 2,
            active_proposal: 11,
            active_challenge: 12,
        }
    }

    fn proposal() -> ProposalSnapshot {
        ProposalSnapshot {
            proposal_id: 11,
            expiry: 1_000,
            rent_payer: vec![21u8; 32],
        }
    }

    fn challenge() -> ChallengeSnapshot {
        ChallengeSnapshot {
            challenge_id: 12,
            expiry: 1_000,
            rent_payer: vec![22u8; 32],
            challenger: vec![23u8; 32],
            struck_agent: vec![24u8; 32],
        }
    }

    fn listing() -> ListingSnapshot {
        ListingSnapshot {
            status: ListingStatus::Legal,
            legal_deadline: 10_000,
            legal_process_time: 1_000,
            developer: vec![31u8; 32],
            developer_lawyer: vec![32u8; 32],
            developer_lawyer_doc_status: DocumentStatus::Approved,
            spv_lawyer: vec![33u8; 32],
            spv_lawyer_doc_status: DocumentStatus::Approved,
            spv_election_round: 0,
            spv_election_expiry: 0,
            spv_election_candidate_count: 0,
            region_id: Some(3),
            collected: serde_json::json!([{"mint": "mint-a", "funds": 5, "fee": 1, "tax": 1}]),
        }
    }

    fn config() -> ConfigSnapshot {
        ConfigSnapshot {
            xcav_mint: vec![41u8; 32],
            treasury: vec![42u8; 32],
            accepted_payment_mints: serde_json::json!(["mint-a", "mint-b"]),
        }
    }

    fn evaluate(
        now: i64,
        letting: Option<&LettingSnapshot>,
        listing: Option<&ListingSnapshot>,
    ) -> PendingFinalizations {
        evaluate_pending_finalizations(now, letting, None, None, &[], listing, &[], None)
    }

    #[test]
    fn an_empty_read_turns_every_gate_off() {
        let out = evaluate(1_000, None, None);
        assert!(out.proposal.is_none());
        assert!(out.challenge.is_none());
        assert!(out.agent_election.is_none());
        assert!(out.spv_election.is_none());
        assert!(!out.silent_verdict_overdue);
        assert!(out.execute_deal.is_none());
    }

    #[test]
    fn proposal_and_challenge_are_due_at_their_expiry_inclusive() {
        // now == expiry: due (the on-chain require is `now >= expiry`).
        let out = evaluate_pending_finalizations(
            1_000,
            Some(&letting()),
            Some(&proposal()),
            Some(&challenge()),
            &[],
            None,
            &[],
            None,
        );
        let p = out.proposal.expect("proposal due at expiry");
        assert_eq!(p.proposal_id.0, 11);
        let c = out.challenge.expect("challenge due at expiry");
        assert_eq!(c.challenge_id.0, 12);

        // One second early: neither is due.
        let out = evaluate_pending_finalizations(
            999,
            Some(&letting()),
            Some(&proposal()),
            Some(&challenge()),
            &[],
            None,
            &[],
            None,
        );
        assert!(out.proposal.is_none());
        assert!(out.challenge.is_none());
    }

    #[test]
    fn proposal_gate_requires_the_pointed_at_open_row() {
        // The letting's counter and the row's id disagree (or the row is closed/gone): off.
        let mut p = proposal();
        p.proposal_id = 999;
        let out = evaluate_pending_finalizations(
            1_000,
            Some(&letting()),
            Some(&p),
            None,
            &[],
            None,
            &[],
            None,
        );
        assert!(out.proposal.is_none());
        let out = evaluate_pending_finalizations(
            1_000,
            Some(&letting()),
            None,
            None,
            &[],
            None,
            &[],
            None,
        );
        assert!(out.proposal.is_none());
        // A zero counter turns the gate off even with a row present.
        let mut l = letting();
        l.active_proposal = 0;
        let out = evaluate_pending_finalizations(
            1_000,
            Some(&l),
            Some(&proposal()),
            None,
            &[],
            None,
            &[],
            None,
        );
        assert!(out.proposal.is_none());
    }

    #[test]
    fn agent_election_gate_and_its_tie_rule() {
        let l = letting();
        let cands = vec![cand(1, 10), cand(2, 7)];
        // Expired with candidates: due, winner is the unique leader, round and the full
        // candidate list come along.
        let out =
            evaluate_pending_finalizations(1_000, Some(&l), None, None, &cands, None, &[], None);
        let e = out.agent_election.expect("election due");
        assert_eq!(e.winner, Some(b58(&[1u8; 32])));
        assert_eq!(e.round.0, 2);
        assert_eq!(e.candidates, vec![b58(&[1u8; 32]), b58(&[2u8; 32])]);

        // Tie at the top: the crank is still due (the seat reopens), winner is null.
        let tied = vec![cand(1, 10), cand(2, 10)];
        let out =
            evaluate_pending_finalizations(1_000, Some(&l), None, None, &tied, None, &[], None);
        let e = out.agent_election.expect("tied election still due");
        assert_eq!(e.winner, None);
        assert_eq!(e.candidates.len(), 2);

        // Not yet expired / never opened / no candidates: off.
        let out =
            evaluate_pending_finalizations(999, Some(&l), None, None, &cands, None, &[], None);
        assert!(out.agent_election.is_none());
        let mut l0 = letting();
        l0.election_expiry = 0;
        let out =
            evaluate_pending_finalizations(1_000, Some(&l0), None, None, &cands, None, &[], None);
        assert!(out.agent_election.is_none());
        let mut l1 = letting();
        l1.election_candidate_count = 0;
        let out =
            evaluate_pending_finalizations(1_000, Some(&l1), None, None, &cands, None, &[], None);
        assert!(out.agent_election.is_none());
    }

    #[test]
    fn spv_election_gate_requires_the_vacant_seat() {
        let mut l = listing();
        l.spv_election_round = 1;
        l.spv_election_expiry = 500;
        l.spv_election_candidate_count = 2;
        l.spv_lawyer = VACANT_SEAT.to_vec();
        let cands = vec![cand(1, 10), cand(2, 10)];
        let out =
            evaluate_pending_finalizations(500, None, None, None, &[], Some(&l), &cands, None);
        let e = out.spv_election.expect("spv election due");
        assert_eq!(e.winner, None, "tie at the top");
        assert_eq!(e.round.0, 1);
        assert_eq!(e.candidates.len(), 2);

        // Seat already taken: off even expired.
        let mut seated = listing();
        seated.spv_election_round = 1;
        seated.spv_election_expiry = 500;
        seated.spv_election_candidate_count = 2;
        let out =
            evaluate_pending_finalizations(500, None, None, None, &[], Some(&seated), &cands, None);
        assert!(out.spv_election.is_none());
    }

    #[test]
    fn silent_verdict_window_is_the_final_stretch_inclusive() {
        // legal_deadline 10_000, legal_process_time 1_000 -> final stretch starts at
        // 10_000 - 1_000/5 = 9_800.
        let mut l = listing();
        l.status = ListingStatus::SoldOut;
        l.developer_lawyer_doc_status = DocumentStatus::Pending;
        l.spv_lawyer_doc_status = DocumentStatus::Approved;

        assert!(!evaluate(9_799, None, Some(&l)).silent_verdict_overdue);
        assert!(evaluate(9_800, None, Some(&l)).silent_verdict_overdue);
        assert!(evaluate(10_000, None, Some(&l)).silent_verdict_overdue);
        assert!(!evaluate(10_001, None, Some(&l)).silent_verdict_overdue);

        // The developer's lawyer already ruled: off.
        let mut ruled = listing();
        ruled.status = ListingStatus::SoldOut;
        ruled.developer_lawyer_doc_status = DocumentStatus::Approved;
        ruled.spv_lawyer_doc_status = DocumentStatus::Approved;
        assert!(!evaluate(9_900, None, Some(&ruled)).silent_verdict_overdue);

        // The SPV side still pending: off. LEGAL status: off (that phase is execute_deal's).
        let mut spv_pending = listing();
        spv_pending.status = ListingStatus::SoldOut;
        spv_pending.developer_lawyer_doc_status = DocumentStatus::Pending;
        spv_pending.spv_lawyer_doc_status = DocumentStatus::Pending;
        assert!(!evaluate(9_900, None, Some(&spv_pending)).silent_verdict_overdue);
        let mut legal = listing();
        legal.developer_lawyer_doc_status = DocumentStatus::Pending;
        assert!(!evaluate(9_900, None, Some(&legal)).silent_verdict_overdue);
    }

    #[test]
    fn execute_deal_is_due_through_the_legal_deadline_inclusive() {
        let l = listing();
        let c = config();
        let out =
            evaluate_pending_finalizations(10_000, None, None, None, &[], Some(&l), &[], Some(&c));
        let d = out.execute_deal.expect("due at the deadline");
        assert_eq!(d.region_id, 3);
        assert_eq!(d.developer, b58(&[31u8; 32]));
        assert_eq!(d.developer_lawyer, b58(&[32u8; 32]));
        assert_eq!(d.spv_lawyer, b58(&[33u8; 32]));
        assert_eq!(d.xcav_mint, b58(&[41u8; 32]));
        assert_eq!(d.treasury, b58(&[42u8; 32]));
        assert_eq!(d.accepted_payment_mints, vec!["mint-a", "mint-b"]);
        assert_eq!(d.collected_mints, vec!["mint-a"]);

        // Past the deadline / not LEGAL / no region / no config: off.
        let out =
            evaluate_pending_finalizations(10_001, None, None, None, &[], Some(&l), &[], Some(&c));
        assert!(out.execute_deal.is_none());
        let mut sold_out = listing();
        sold_out.status = ListingStatus::SoldOut;
        let out = evaluate_pending_finalizations(
            9_000,
            None,
            None,
            None,
            &[],
            Some(&sold_out),
            &[],
            Some(&c),
        );
        assert!(out.execute_deal.is_none());
        let mut no_region = listing();
        no_region.region_id = None;
        let out = evaluate_pending_finalizations(
            9_000,
            None,
            None,
            None,
            &[],
            Some(&no_region),
            &[],
            Some(&c),
        );
        assert!(out.execute_deal.is_none());
        let out = evaluate_pending_finalizations(9_000, None, None, None, &[], Some(&l), &[], None);
        assert!(out.execute_deal.is_none());
    }

    #[test]
    fn collected_and_accepted_mints_parse_leniently() {
        assert_eq!(
            parse_collected_mints(&serde_json::json!([
                {"mint": "mint-a", "funds": 5},
                {"mint": "", "funds": 1},
                {"nomint": true}
            ])),
            vec!["mint-a"]
        );
        assert!(parse_collected_mints(&serde_json::json!({"x": 1})).is_empty());
        assert_eq!(
            parse_string_array(&serde_json::json!(["a", 1, "b"])),
            vec!["a", "b"]
        );
        assert!(parse_string_array(&serde_json::json!("nope")).is_empty());
    }

    #[test]
    fn region_id_clamps_to_the_on_chain_u16() {
        let mut l = listing();
        l.region_id = Some(70_000);
        let c = config();
        let out =
            evaluate_pending_finalizations(9_000, None, None, None, &[], Some(&l), &[], Some(&c));
        assert_eq!(
            out.execute_deal.expect("due").region_id,
            u16::MAX as i32,
            "clamped like the C# Math.Clamp to ushort"
        );
    }
}
