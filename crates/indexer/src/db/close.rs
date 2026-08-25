//! The full roster of account-state tables and the generic slot-guarded soft close.
//!
//! Every state table shares the exact same close shape (`UPDATE <t> SET slot = $2,
//! closed_at_slot = $2 WHERE pubkey = $1 AND slot < $2`), so with 31 tables across four
//! programs the close is ONE dynamically-built statement over an enum-constrained table name
//! rather than 31 copies of a compile-checked macro call. The table name can only come from
//! [`StateTable`], so nothing user-controlled ever reaches the SQL string; schema drift
//! (a table missing the shared columns) is caught by `db::tests`, which exercises every
//! [`StateTable::ALL`] entry against the migrated schema.
//!
//! `open_account_pubkeys` -- the deletion-tracker seed -- is built the same way, as a UNION
//! over every table in the roster.

use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

/// Every account-state table, across all four programs. The whitelist's three (0002) keep
/// their legacy unprefixed names; the sibling programs' tables (0008..0010, extended by
/// 0012 for the 2026-08 redeploy) are program-prefixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateTable {
    // xcavate_whitelist (migrations/0002)
    Config,
    Admin,
    RoleAccount,
    // regions (migrations/0008)
    RegionsConfig,
    RegionsLocation,
    RegionsRegion,
    RegionsRegionProposal,
    RegionsRegionState,
    RegionsVoteRecord,
    // marketplace (migrations/0009 + 0012)
    MarketplaceConfig,
    MarketplaceInvestorPosition,
    MarketplaceLawyer,
    MarketplaceLawyerCandidacy,
    MarketplaceLawyerVote,
    MarketplaceListing,
    MarketplaceOffer,
    MarketplacePropertyAsset,
    MarketplaceReservation,
    MarketplaceShareHolding,
    MarketplaceShareListing,
    // property (migrations/0010 + 0012)
    PropertyConfig,
    PropertyAgentCandidacy,
    PropertyAgentVote,
    PropertyChallenge,
    PropertyGovVote,
    PropertyIncome,
    PropertyIncomeCheckpoint,
    PropertyLettingAgent,
    PropertyLetting,
    PropertyProposal,
    PropertyResignationNotice,
}

impl StateTable {
    pub const ALL: &'static [StateTable] = &[
        StateTable::Config,
        StateTable::Admin,
        StateTable::RoleAccount,
        StateTable::RegionsConfig,
        StateTable::RegionsLocation,
        StateTable::RegionsRegion,
        StateTable::RegionsRegionProposal,
        StateTable::RegionsRegionState,
        StateTable::RegionsVoteRecord,
        StateTable::MarketplaceConfig,
        StateTable::MarketplaceInvestorPosition,
        StateTable::MarketplaceLawyer,
        StateTable::MarketplaceLawyerCandidacy,
        StateTable::MarketplaceLawyerVote,
        StateTable::MarketplaceListing,
        StateTable::MarketplaceOffer,
        StateTable::MarketplacePropertyAsset,
        StateTable::MarketplaceReservation,
        StateTable::MarketplaceShareHolding,
        StateTable::MarketplaceShareListing,
        StateTable::PropertyConfig,
        StateTable::PropertyAgentCandidacy,
        StateTable::PropertyAgentVote,
        StateTable::PropertyChallenge,
        StateTable::PropertyGovVote,
        StateTable::PropertyIncome,
        StateTable::PropertyIncomeCheckpoint,
        StateTable::PropertyLettingAgent,
        StateTable::PropertyLetting,
        StateTable::PropertyProposal,
        StateTable::PropertyResignationNotice,
    ];

    pub const fn table_name(self) -> &'static str {
        match self {
            StateTable::Config => "config",
            StateTable::Admin => "admin",
            StateTable::RoleAccount => "role_account",
            StateTable::RegionsConfig => "regions_config",
            StateTable::RegionsLocation => "regions_location",
            StateTable::RegionsRegion => "regions_region",
            StateTable::RegionsRegionProposal => "regions_region_proposal",
            StateTable::RegionsRegionState => "regions_region_state",
            StateTable::RegionsVoteRecord => "regions_vote_record",
            StateTable::MarketplaceConfig => "marketplace_config",
            StateTable::MarketplaceInvestorPosition => "marketplace_investor_position",
            StateTable::MarketplaceLawyer => "marketplace_lawyer",
            StateTable::MarketplaceLawyerCandidacy => "marketplace_lawyer_candidacy",
            StateTable::MarketplaceLawyerVote => "marketplace_lawyer_vote",
            StateTable::MarketplaceListing => "marketplace_listing",
            StateTable::MarketplaceOffer => "marketplace_offer",
            StateTable::MarketplacePropertyAsset => "marketplace_property_asset",
            StateTable::MarketplaceReservation => "marketplace_reservation",
            StateTable::MarketplaceShareHolding => "marketplace_share_holding",
            StateTable::MarketplaceShareListing => "marketplace_share_listing",
            StateTable::PropertyConfig => "property_config",
            StateTable::PropertyAgentCandidacy => "property_agent_candidacy",
            StateTable::PropertyAgentVote => "property_agent_vote",
            StateTable::PropertyChallenge => "property_challenge",
            StateTable::PropertyGovVote => "property_gov_vote",
            StateTable::PropertyIncome => "property_income",
            StateTable::PropertyIncomeCheckpoint => "property_income_checkpoint",
            StateTable::PropertyLettingAgent => "property_letting_agent",
            StateTable::PropertyLetting => "property_letting",
            StateTable::PropertyProposal => "property_proposal",
            StateTable::PropertyResignationNotice => "property_resignation_notice",
        }
    }
}

/// Slot-guarded soft close against one table: sets `closed_at_slot` (and bumps `slot`, so a
/// later stale write can't undo the close) only if the stored row is STRICTLY older than
/// `slot`. A close on a pubkey the table has never seen is a silent no-op -- same contract as
/// `db::accounts::close_*` always had.
///
/// The strict guard has a known blind spot: slot-granularity bookkeeping cannot order two
/// events inside one slot, so a PDA created and closed in the same slot keeps an open row
/// (the upsert at slot S lands, the close at slot S is guarded out). Loosening to `<=` would
/// trade this for the opposite wrong-closed case (close-then-recreate in one slot) AND
/// deadlock with the upserts' own strict guard, so the tie stays unresolvable at this
/// granularity. The healing path is [`close_missing_in_table`]: the next
/// `getProgramAccounts` snapshot proves which accounts actually exist and sweeps the
/// stale-open rows closed.
pub async fn close_in_table<'e, E>(
    executor: E,
    table: StateTable,
    pubkey: &[u8],
    slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let sql = format!(
        "UPDATE {} SET slot = $2, closed_at_slot = $2 WHERE pubkey = $1 AND slot < $2",
        table.table_name()
    );
    sqlx::query(&sql)
        .bind(pubkey)
        .bind(slot)
        .execute(executor)
        .await
}

/// Every still-open account-state pubkey across every table: the seed for the deletion
/// tracker (see `processors::TrackedAccounts`). Carbon's Yellowstone datasource only
/// synthesises an `AccountDeletion` for pubkeys already in that in-memory set, so a
/// restarted indexer must reload it or it would be blind to the closure of any PDA it had
/// not happened to see an update for yet.
pub async fn open_account_pubkeys<'e, E>(executor: E) -> Result<Vec<Vec<u8>>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let sql = StateTable::ALL
        .iter()
        .map(|t| {
            format!(
                "SELECT pubkey FROM {} WHERE closed_at_slot IS NULL",
                t.table_name()
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(&sql).fetch_all(executor).await?;
    Ok(rows.into_iter().map(|(pk,)| pk).collect())
}

/// The snapshot's close-missing sweep: soft-close every still-open row in `table` whose
/// account was NOT in a fresh `getProgramAccounts` result (`live_pubkeys`) taken at
/// `snapshot_slot`. Such a row's account is provably gone on-chain -- closed by an
/// instruction whose close op could not land (the same-slot tie above), by a path the
/// instruction mappings do not know, or by anything else the deletion pipe missed.
///
/// Slot-guarded like every other write: a row at `slot >= snapshot_slot` was written from
/// evidence at least as fresh as the snapshot's read (the stream runs ahead of the snapshot
/// by design) and is left alone.
pub async fn close_missing_in_table<'e, E>(
    executor: E,
    table: StateTable,
    live_pubkeys: &[Vec<u8>],
    snapshot_slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let sql = format!(
        "UPDATE {} SET slot = $2, closed_at_slot = $2 \
         WHERE closed_at_slot IS NULL AND slot < $2 AND NOT (pubkey = ANY($1))",
        table.table_name()
    );
    sqlx::query(&sql)
        .bind(live_pubkeys)
        .bind(snapshot_slot)
        .execute(executor)
        .await
}
