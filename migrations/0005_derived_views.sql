-- Derived data (spec §5.4b, controller ruling R7): SQL VIEWS folding over whitelist_actions,
-- replacing the old SubQuery handlers' order-sensitive in-place mutation of Admin /
-- RoleAssignment entities. Each view sorts whitelist_actions internally by the canonical
-- event order below, so it is order-insensitive BY CONSTRUCTION: it does not matter whether
-- the row for slot 100 or the row for slot 200 was INSERTed first (live stream vs backwards
-- backfill can arrive in either order), the fold always reads them out in the same order and
-- produces the same result. They converge to the true state exactly once backfill completes.
--
-- Canonical event order, used everywhere below:
--   ORDER BY slot, block_time, tx_signature, string_to_array(instruction_index, '.')::int[]
-- Caveat (documented here, not re-derived at every call site): within a single slot, two
-- different transactions are ordered by comparing tx_signature as text, which has no
-- relationship to the transactions' actual position within the block. This program's volume
-- makes that an acceptable approximation (multiple whitelist transactions landing in the
-- same slot is rare), but it is not a real ordering guarantee -- if that ever matters, the
-- fix is to start recording each transaction's index within its block, not to change this
-- comment.

-- One row per distinct admin pubkey that has ever been the subject of an ADMIN_ADDED action.
-- `active` / removed_* fold the same way a re-added admin resets on-chain: the latest
-- ADMIN_ADDED wins, and removed_* only reflects an ADMIN_REMOVED strictly after it, so a
-- later re-add naturally clears removed_* back to NULL without any special-casing.
CREATE VIEW admins_view AS
WITH admin_events AS (
    SELECT
        subject,
        type,
        actor,
        slot,
        block_time,
        tx_signature,
        string_to_array(instruction_index, '.')::int[] AS ix_path
    FROM whitelist_actions
    WHERE type IN ('ADMIN_ADDED', 'ADMIN_REMOVED')
      AND subject IS NOT NULL
),
latest_add AS (
    SELECT DISTINCT ON (subject)
        subject,
        actor AS added_by,
        slot AS added_at_slot,
        block_time AS added_at,
        tx_signature AS added_in_tx,
        slot, block_time, tx_signature, ix_path
    FROM admin_events
    WHERE type = 'ADMIN_ADDED'
    ORDER BY subject, slot DESC, block_time DESC, tx_signature DESC, ix_path DESC
),
latest_removal_after_add AS (
    SELECT DISTINCT ON (r.subject)
        r.subject,
        r.slot AS removed_at_slot,
        r.block_time AS removed_at,
        r.tx_signature AS removed_in_tx
    FROM admin_events r
    JOIN latest_add a ON a.subject = r.subject
    WHERE r.type = 'ADMIN_REMOVED'
      AND (r.slot, r.block_time, r.tx_signature, r.ix_path) > (a.slot, a.block_time, a.tx_signature, a.ix_path)
    ORDER BY r.subject, r.slot DESC, r.block_time DESC, r.tx_signature DESC, r.ix_path DESC
)
SELECT
    a.subject AS id,
    (rm.subject IS NULL) AS active,
    a.added_by,
    a.added_at_slot,
    a.added_at,
    a.added_in_tx,
    rm.removed_at_slot,
    rm.removed_at,
    rm.removed_in_tx
FROM latest_add a
LEFT JOIN latest_removal_after_add rm ON rm.subject = a.subject;

-- One row per (subject, role) that has ever had a ROLE_ASSIGNED action. Same re-assign-resets
-- fold as admins_view: the latest ROLE_ASSIGNED wins, and permission/removal only reflect
-- events strictly after it.
CREATE VIEW role_assignments_view AS
WITH role_events AS (
    SELECT
        subject,
        role,
        type,
        actor,
        permission,
        slot,
        block_time,
        tx_signature,
        string_to_array(instruction_index, '.')::int[] AS ix_path
    FROM whitelist_actions
    WHERE type IN ('ROLE_ASSIGNED', 'ROLE_REMOVED', 'ROLE_RENOUNCED', 'PERMISSION_UPDATED')
      AND subject IS NOT NULL
      AND role IS NOT NULL
),
latest_assign AS (
    SELECT DISTINCT ON (subject, role)
        subject,
        role,
        actor AS assigned_by,
        slot AS assigned_at_slot,
        block_time AS assigned_at,
        tx_signature AS assigned_in_tx,
        slot, block_time, tx_signature, ix_path
    FROM role_events
    WHERE type = 'ROLE_ASSIGNED'
    ORDER BY subject, role, slot DESC, block_time DESC, tx_signature DESC, ix_path DESC
),
latest_permission_after_assign AS (
    SELECT DISTINCT ON (p.subject, p.role)
        p.subject,
        p.role,
        p.permission,
        p.slot, p.block_time, p.tx_signature, p.ix_path
    FROM role_events p
    JOIN latest_assign a ON a.subject = p.subject AND a.role = p.role
    WHERE p.type = 'PERMISSION_UPDATED'
      AND (p.slot, p.block_time, p.tx_signature, p.ix_path) > (a.slot, a.block_time, a.tx_signature, a.ix_path)
    ORDER BY p.subject, p.role, p.slot DESC, p.block_time DESC, p.tx_signature DESC, p.ix_path DESC
),
latest_removal_after_assign AS (
    SELECT DISTINCT ON (r.subject, r.role)
        r.subject,
        r.role,
        r.type AS removal_type,
        r.actor AS removed_by,
        r.slot AS removed_at_slot,
        r.block_time AS removed_at,
        r.tx_signature AS removed_in_tx,
        r.slot, r.block_time, r.tx_signature, r.ix_path
    FROM role_events r
    JOIN latest_assign a ON a.subject = r.subject AND a.role = r.role
    WHERE r.type IN ('ROLE_REMOVED', 'ROLE_RENOUNCED')
      AND (r.slot, r.block_time, r.tx_signature, r.ix_path) > (a.slot, a.block_time, a.tx_signature, a.ix_path)
    ORDER BY r.subject, r.role, r.slot DESC, r.block_time DESC, r.tx_signature DESC, r.ix_path DESC
),
-- "updated_at = latest of assign/permission-update/removal": union the three candidate
-- events per (subject, role) and take the one with the largest canonical-order key, rather
-- than assuming removal (if present) is always last -- true on-chain (a closed PDA can't
-- receive a later PERMISSION_UPDATED), but the fold shouldn't silently rely on that.
latest_activity AS (
    SELECT DISTINCT ON (subject, role)
        subject, role, slot AS updated_at_slot, block_time AS updated_at
    FROM (
        SELECT subject, role, slot, block_time, tx_signature, ix_path FROM latest_assign
        UNION ALL
        SELECT subject, role, slot, block_time, tx_signature, ix_path FROM latest_permission_after_assign
        UNION ALL
        SELECT subject, role, slot, block_time, tx_signature, ix_path FROM latest_removal_after_assign
    ) candidates
    ORDER BY subject, role, slot DESC, block_time DESC, tx_signature DESC, ix_path DESC
)
SELECT
    a.subject || '-' || (CASE a.role
        WHEN 'REGIONAL_OPERATOR'      THEN '0'
        WHEN 'REAL_ESTATE_INVESTOR'   THEN '1'
        WHEN 'REAL_ESTATE_DEVELOPER'  THEN '2'
        WHEN 'LAWYER'                 THEN '3'
        WHEN 'LETTING_AGENT'          THEN '4'
        WHEN 'SPV_CONFIRMATION'       THEN '5'
    END) AS id,
    a.subject AS user_pubkey,
    a.role,
    (rm.subject IS NULL) AS active,
    COALESCE(p.permission, 'COMPLIANT') AS permission,
    a.assigned_by AS rent_payer,
    a.assigned_by,
    a.assigned_at_slot,
    a.assigned_at,
    a.assigned_in_tx,
    act.updated_at_slot,
    act.updated_at,
    rm.removed_at_slot,
    rm.removed_at,
    rm.removed_in_tx,
    rm.removed_by,
    CASE rm.removal_type
        WHEN 'ROLE_REMOVED'   THEN 'REMOVED'
        WHEN 'ROLE_RENOUNCED' THEN 'RENOUNCED'
    END AS removal_kind
FROM latest_assign a
LEFT JOIN latest_permission_after_assign p ON p.subject = a.subject AND p.role = a.role
LEFT JOIN latest_removal_after_assign rm ON rm.subject = a.subject AND rm.role = a.role
LEFT JOIN latest_activity act ON act.subject = a.subject AND act.role = a.role;

-- Single-row view: authority/pending_authority come from the `config` account-state table
-- (authoritative -- it's slot-guarded on-chain state, not derived from the action log), while
-- updated_at_* comes from folding whitelist_actions the same way as the other views.
CREATE VIEW config_view AS
WITH latest_config_action AS (
    SELECT slot, block_time, tx_signature
    FROM whitelist_actions
    WHERE type IN ('CONFIG_INITIALIZED', 'AUTHORITY_UPDATE_PROPOSED', 'AUTHORITY_UPDATED')
    ORDER BY slot DESC, block_time DESC, tx_signature DESC,
             string_to_array(instruction_index, '.')::int[] DESC
    LIMIT 1
)
SELECT
    c.authority,
    c.pending_authority,
    a.slot AS updated_at_slot,
    a.block_time AS updated_at,
    a.tx_signature AS updated_in_tx
FROM config c
LEFT JOIN latest_config_action a ON true
LIMIT 1;
