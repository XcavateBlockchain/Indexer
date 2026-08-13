/**
 * ALL SQL for the read API lives in this file — and only in this file — so
 * that any drift between the assumed schema and the live SubQuery-created
 * schema (column names, enum spellings, ...) can be fixed in one place.
 *
 * Assumptions (verify against the live DB):
 *   - Tables: <schema>.configs, <schema>.admins, <schema>.role_assignments,
 *     <schema>.whitelist_actions, <schema>._metadata
 *   - Columns are snake_case versions of the SubQuery entity fields
 *     (e.g. pendingAuthority -> pending_authority).
 *   - "user" is a reserved word and is always double-quoted.
 *   - With historical indexing enabled every entity table additionally has
 *     _id (uuid) and _block_range (int8range) and keeps multiple versions per
 *     id; the current version satisfies upper_inf(_block_range). This is
 *     detected once at runtime (see isHistoricalMode) and applied to every
 *     query from here.
 */
import { config } from '../config';
import { query } from './pool';

// ---------------------------------------------------------------------------
// Identifier handling
// ---------------------------------------------------------------------------

function quoteIdent(name: string): string {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) {
    throw new Error(`Refusing to use unsafe SQL identifier: ${JSON.stringify(name)}`);
  }
  return `"${name}"`;
}

/** Quoted schema name, validated once at module load. */
const schema = quoteIdent(config.db.schema);

// ---------------------------------------------------------------------------
// Row shapes as returned by node-postgres
// (numeric -> string, timestamp -> Date, jsonb -> parsed JSON value)
// ---------------------------------------------------------------------------

export type ConfigRow = {
  id: string;
  authority: string;
  pending_authority: string | null;
  updated_at_block: string;
  updated_at: Date;
  updated_in_tx: string;
};

export type AdminRow = {
  id: string;
  active: boolean;
  added_by: string;
  added_at_block: string;
  added_at: Date;
  added_in_tx: string;
  removed_at_block: string | null;
  removed_at: Date | null;
  removed_in_tx: string | null;
};

export type RoleAssignmentRow = {
  id: string;
  user: string;
  role: string;
  permission: string;
  active: boolean;
  rent_payer: string;
  assigned_by: string;
  assigned_at_block: string;
  assigned_at: Date;
  assigned_in_tx: string;
  updated_at_block: string;
  updated_at: Date;
  removed_at_block: string | null;
  removed_at: Date | null;
  removed_in_tx: string | null;
  removal_kind: string | null;
  removed_by: string | null;
};

export type WhitelistActionRow = {
  id: string;
  type: string;
  subject: string | null;
  role: string | null;
  permission: string | null;
  actor: string;
  block_height: string;
  block_time: Date;
  tx_signature: string;
  instruction_index: string;
};

type CountRow = { count: string };

export type Page<T> = { rows: T[]; totalCount: string };

// ---------------------------------------------------------------------------
// Column lists
// ---------------------------------------------------------------------------

const CONFIG_COLS = 'id, authority, pending_authority, updated_at_block, updated_at, updated_in_tx';

const ADMIN_COLS =
  'id, active, added_by, added_at_block, added_at, added_in_tx, ' +
  'removed_at_block, removed_at, removed_in_tx';

const ROLE_ASSIGNMENT_COLS =
  'id, "user", role, permission, active, rent_payer, assigned_by, ' +
  'assigned_at_block, assigned_at, assigned_in_tx, updated_at_block, updated_at, ' +
  'removed_at_block, removed_at, removed_in_tx, removal_kind, removed_by';

const ACTION_COLS =
  'id, type, subject, role, permission, actor, block_height, block_time, ' +
  'tx_signature, instruction_index';

// ---------------------------------------------------------------------------
// Historical-mode detection (SubQuery `--historical` indexing)
// ---------------------------------------------------------------------------

const HISTORICAL_PROBE_TABLE = 'role_assignments';

const SQL_DETECT_HISTORICAL = `
  SELECT
    EXISTS (
      SELECT 1 FROM information_schema.columns
      WHERE table_schema = $1 AND table_name = $2
    ) AS table_exists,
    EXISTS (
      SELECT 1 FROM information_schema.columns
      WHERE table_schema = $1 AND table_name = $2 AND column_name = '_block_range'
    ) AS historical`;

let historicalModeCache: boolean | null = null;

/**
 * True when the SubQuery tables carry _block_range (historical indexing).
 * The result is cached after the first successful detection against an
 * existing table; failures (DB down, tables not created yet) are not cached
 * so the check is retried on the next query.
 */
export async function isHistoricalMode(): Promise<boolean> {
  if (historicalModeCache !== null) {
    return historicalModeCache;
  }
  const res = await query<{ table_exists: boolean; historical: boolean }>(SQL_DETECT_HISTORICAL, [
    config.db.schema,
    HISTORICAL_PROBE_TABLE,
  ]);
  const row = res.rows[0];
  if (!row || !row.table_exists) {
    // Indexer has not created its tables yet — don't cache, the schema may
    // still appear (with or without historical mode) later.
    return false;
  }
  historicalModeCache = row.historical;
  return row.historical;
}

/** ` AND upper_inf(_block_range)` in historical mode, empty string otherwise. */
async function histAnd(): Promise<string> {
  return (await isHistoricalMode()) ? ' AND upper_inf(_block_range)' : '';
}

/** Pushes the historical predicate onto a WHERE-parts list when applicable. */
async function pushHist(where: string[]): Promise<void> {
  if (await isHistoricalMode()) {
    where.push('upper_inf(_block_range)');
  }
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

export async function pingDb(): Promise<void> {
  await query('SELECT 1');
}

// ---------------------------------------------------------------------------
// configs
// ---------------------------------------------------------------------------

export async function getConfig(): Promise<ConfigRow | null> {
  const sql = `SELECT ${CONFIG_COLS} FROM ${schema}.configs WHERE id = 'config'${await histAnd()} LIMIT 1`;
  const res = await query<ConfigRow>(sql);
  return res.rows[0] ?? null;
}

// ---------------------------------------------------------------------------
// role_assignments
// ---------------------------------------------------------------------------

/**
 * Fetches the (single) assignment row for (user, role) regardless of whether
 * it is active — soft-deleted assignments are returned too.
 * @param role DB enum spelling, e.g. 'REGIONAL_OPERATOR'.
 */
export async function getRoleAssignment(user: string, role: string): Promise<RoleAssignmentRow | null> {
  const sql =
    `SELECT ${ROLE_ASSIGNMENT_COLS} FROM ${schema}.role_assignments ` +
    `WHERE "user" = $1 AND role = $2${await histAnd()} LIMIT 1`;
  const res = await query<RoleAssignmentRow>(sql, [user, role]);
  return res.rows[0] ?? null;
}

export interface RoleAssignmentFilters {
  user?: string;
  /** DB enum spelling. */
  role?: string;
  /** DB enum spelling. */
  permission?: string;
  active?: boolean;
}

export async function listRoleAssignments(
  filters: RoleAssignmentFilters,
  limit: number,
  offset: number,
): Promise<Page<RoleAssignmentRow>> {
  const where: string[] = [];
  const params: unknown[] = [];
  if (filters.user !== undefined) {
    params.push(filters.user);
    where.push(`"user" = $${params.length}`);
  }
  if (filters.role !== undefined) {
    params.push(filters.role);
    where.push(`role = $${params.length}`);
  }
  if (filters.permission !== undefined) {
    params.push(filters.permission);
    where.push(`permission = $${params.length}`);
  }
  if (filters.active !== undefined) {
    params.push(filters.active);
    where.push(`active = $${params.length}`);
  }
  await pushHist(where);
  const whereSql = where.length > 0 ? ` WHERE ${where.join(' AND ')}` : '';
  const from = `FROM ${schema}.role_assignments${whereSql}`;

  const count = await query<CountRow>(`SELECT count(*)::text AS count ${from}`, params);
  const rows = await query<RoleAssignmentRow>(
    `SELECT ${ROLE_ASSIGNMENT_COLS} ${from} ` +
      `ORDER BY assigned_at_block DESC, id ASC LIMIT $${params.length + 1} OFFSET $${params.length + 2}`,
    [...params, limit, offset],
  );
  return { rows: rows.rows, totalCount: count.rows[0]?.count ?? '0' };
}

// ---------------------------------------------------------------------------
// admins
// ---------------------------------------------------------------------------

export async function listAdmins(
  active: boolean | undefined,
  limit: number,
  offset: number,
): Promise<Page<AdminRow>> {
  const where: string[] = [];
  const params: unknown[] = [];
  if (active !== undefined) {
    params.push(active);
    where.push(`active = $${params.length}`);
  }
  await pushHist(where);
  const whereSql = where.length > 0 ? ` WHERE ${where.join(' AND ')}` : '';
  const from = `FROM ${schema}.admins${whereSql}`;

  const count = await query<CountRow>(`SELECT count(*)::text AS count ${from}`, params);
  const rows = await query<AdminRow>(
    `SELECT ${ADMIN_COLS} ${from} ` +
      `ORDER BY added_at_block DESC, id ASC LIMIT $${params.length + 1} OFFSET $${params.length + 2}`,
    [...params, limit, offset],
  );
  return { rows: rows.rows, totalCount: count.rows[0]?.count ?? '0' };
}

// ---------------------------------------------------------------------------
// whitelist_actions
// ---------------------------------------------------------------------------

export interface ActionFilters {
  subject?: string;
  actor?: string;
  /** DB enum spelling. */
  type?: string;
  txSignature?: string;
}

export async function listActions(
  filters: ActionFilters,
  limit: number,
  offset: number,
): Promise<Page<WhitelistActionRow>> {
  const where: string[] = [];
  const params: unknown[] = [];
  if (filters.subject !== undefined) {
    params.push(filters.subject);
    where.push(`subject = $${params.length}`);
  }
  if (filters.actor !== undefined) {
    params.push(filters.actor);
    where.push(`actor = $${params.length}`);
  }
  if (filters.type !== undefined) {
    params.push(filters.type);
    where.push(`type = $${params.length}`);
  }
  if (filters.txSignature !== undefined) {
    params.push(filters.txSignature);
    where.push(`tx_signature = $${params.length}`);
  }
  await pushHist(where);
  const whereSql = where.length > 0 ? ` WHERE ${where.join(' AND ')}` : '';
  const from = `FROM ${schema}.whitelist_actions${whereSql}`;

  const count = await query<CountRow>(`SELECT count(*)::text AS count ${from}`, params);
  // instruction_index is TEXT holding a non-negative integer; ordering by
  // (length, value) yields numeric order without a cast that could fail on
  // unexpected content.
  const rows = await query<WhitelistActionRow>(
    `SELECT ${ACTION_COLS} ${from} ` +
      `ORDER BY block_height DESC, length(instruction_index) ASC, instruction_index ASC ` +
      `LIMIT $${params.length + 1} OFFSET $${params.length + 2}`,
    [...params, limit, offset],
  );
  return { rows: rows.rows, totalCount: count.rows[0]?.count ?? '0' };
}

// ---------------------------------------------------------------------------
// _metadata
// ---------------------------------------------------------------------------

export interface IndexerMetadata {
  lastProcessedHeight?: number;
  targetHeight?: number;
  /** Milliseconds since epoch. */
  lastProcessedTimestamp?: number;
  indexerHealthy?: boolean;
}

const METADATA_KEYS = ['lastProcessedHeight', 'targetHeight', 'lastProcessedTimestamp', 'indexerHealthy'];

function asNumber(value: unknown): number | undefined {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === 'string') {
    const n = Number(value);
    return Number.isFinite(n) ? n : undefined;
  }
  return undefined;
}

/** Any subset of the keys may be missing — callers must handle undefined. */
export async function getIndexerMetadata(): Promise<IndexerMetadata> {
  const res = await query<{ key: string; value: unknown }>(
    `SELECT key, value FROM ${schema}._metadata WHERE key = ANY($1)`,
    [METADATA_KEYS],
  );
  const meta: IndexerMetadata = {};
  for (const row of res.rows) {
    switch (row.key) {
      case 'lastProcessedHeight':
        meta.lastProcessedHeight = asNumber(row.value);
        break;
      case 'targetHeight':
        meta.targetHeight = asNumber(row.value);
        break;
      case 'lastProcessedTimestamp':
        meta.lastProcessedTimestamp = asNumber(row.value);
        break;
      case 'indexerHealthy':
        meta.indexerHealthy = row.value === true;
        break;
      default:
        break;
    }
  }
  return meta;
}
