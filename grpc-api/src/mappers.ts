/**
 * DB row -> proto message mapping, including the single place where DB enum
 * strings are mapped to/from proto enum values.
 */
import { status as grpcStatus } from '@grpc/grpc-js';
import { RpcError } from './errors';
import type { AdminRow, ConfigRow, RoleAssignmentRow, WhitelistActionRow } from './db/queries';
import type { Timestamp } from './generated/google/protobuf/Timestamp';
import type { ActionType__Output } from './generated/realxmarket/whitelist/v1/ActionType';
import type { Admin } from './generated/realxmarket/whitelist/v1/Admin';
import type { ConfigResponse } from './generated/realxmarket/whitelist/v1/ConfigResponse';
import type { Permission__Output } from './generated/realxmarket/whitelist/v1/Permission';
import type { RemovalKind__Output } from './generated/realxmarket/whitelist/v1/RemovalKind';
import type { Role__Output } from './generated/realxmarket/whitelist/v1/Role';
import type { RoleAssignment } from './generated/realxmarket/whitelist/v1/RoleAssignment';
import type { WhitelistAction } from './generated/realxmarket/whitelist/v1/WhitelistAction';

// ---------------------------------------------------------------------------
// Enum maps — proto enum value name <-> DB enum string. ONE place, by design.
// ---------------------------------------------------------------------------

const ROLE_TO_DB: Readonly<Record<string, string>> = {
  ROLE_REGIONAL_OPERATOR: 'REGIONAL_OPERATOR',
  ROLE_REAL_ESTATE_INVESTOR: 'REAL_ESTATE_INVESTOR',
  ROLE_REAL_ESTATE_DEVELOPER: 'REAL_ESTATE_DEVELOPER',
  ROLE_LAWYER: 'LAWYER',
  ROLE_LETTING_AGENT: 'LETTING_AGENT',
  ROLE_SPV_CONFIRMATION: 'SPV_CONFIRMATION',
};

const PERMISSION_TO_DB: Readonly<Record<string, string>> = {
  PERMISSION_COMPLIANT: 'COMPLIANT',
  PERMISSION_REVOKED: 'REVOKED',
};

const REMOVAL_KIND_TO_DB: Readonly<Record<string, string>> = {
  REMOVAL_KIND_REMOVED: 'REMOVED',
  REMOVAL_KIND_RENOUNCED: 'RENOUNCED',
};

const ACTION_TYPE_TO_DB: Readonly<Record<string, string>> = {
  ACTION_TYPE_CONFIG_INITIALIZED: 'CONFIG_INITIALIZED',
  ACTION_TYPE_AUTHORITY_UPDATE_PROPOSED: 'AUTHORITY_UPDATE_PROPOSED',
  ACTION_TYPE_AUTHORITY_UPDATED: 'AUTHORITY_UPDATED',
  ACTION_TYPE_ADMIN_ADDED: 'ADMIN_ADDED',
  ACTION_TYPE_ADMIN_REMOVED: 'ADMIN_REMOVED',
  ACTION_TYPE_ROLE_ASSIGNED: 'ROLE_ASSIGNED',
  ACTION_TYPE_ROLE_REMOVED: 'ROLE_REMOVED',
  ACTION_TYPE_ROLE_RENOUNCED: 'ROLE_RENOUNCED',
  ACTION_TYPE_PERMISSION_UPDATED: 'PERMISSION_UPDATED',
};

function invert<T extends string>(map: Readonly<Record<T, string>>): Readonly<Record<string, T>> {
  const out: Record<string, T> = {};
  for (const [k, v] of Object.entries(map as Record<string, string>)) {
    out[v] = k as T;
  }
  return out;
}

const ROLE_FROM_DB = invert(ROLE_TO_DB as Record<Role__Output, string>);
const PERMISSION_FROM_DB = invert(PERMISSION_TO_DB as Record<Permission__Output, string>);
const REMOVAL_KIND_FROM_DB = invert(REMOVAL_KIND_TO_DB as Record<RemovalKind__Output, string>);
const ACTION_TYPE_FROM_DB = invert(ACTION_TYPE_TO_DB as Record<ActionType__Output, string>);

function fromDb<T extends string>(
  map: Readonly<Record<string, T>>,
  value: string | null | undefined,
  fallback: T,
): T {
  if (value === null || value === undefined) {
    return fallback;
  }
  return map[value] ?? fallback;
}

// ---------------------------------------------------------------------------
// Request enum handling
// (proto-loader is configured with enums=String, so handlers receive proto
// enum value NAMES; an out-of-range wire value would surface as a number.)
// ---------------------------------------------------------------------------

function isUnset(value: string | number | undefined): boolean {
  return value === undefined || value === 0 || value === 'ROLE_UNSPECIFIED' ||
    value === 'PERMISSION_UNSPECIFIED' || value === 'ACTION_TYPE_UNSPECIFIED';
}

function toDbEnum(
  map: Readonly<Record<string, string>>,
  value: string | number,
  field: string,
): string {
  const db = map[String(value)];
  if (db === undefined) {
    throw new RpcError(grpcStatus.INVALID_ARGUMENT, `invalid ${field}: ${String(value)}`);
  }
  return db;
}

/** Required request role — UNSPECIFIED/unknown is INVALID_ARGUMENT. */
export function requireRole(value: string | number | undefined): string {
  if (isUnset(value)) {
    throw new RpcError(grpcStatus.INVALID_ARGUMENT, 'role must be specified');
  }
  return toDbEnum(ROLE_TO_DB, value as string | number, 'role');
}

/** Optional role filter — undefined/UNSPECIFIED means "no filter". */
export function optionalRole(value: string | number | undefined): string | undefined {
  return isUnset(value) ? undefined : toDbEnum(ROLE_TO_DB, value as string | number, 'role');
}

/** Optional permission filter — undefined/UNSPECIFIED means "no filter". */
export function optionalPermission(value: string | number | undefined): string | undefined {
  return isUnset(value) ? undefined : toDbEnum(PERMISSION_TO_DB, value as string | number, 'permission');
}

/** Optional action-type filter — undefined/UNSPECIFIED means "no filter". */
export function optionalActionType(value: string | number | undefined): string | undefined {
  return isUnset(value) ? undefined : toDbEnum(ACTION_TYPE_TO_DB, value as string | number, 'type');
}

/** DB permission string check used by CheckAccess. */
export const DB_PERMISSION_COMPLIANT = 'COMPLIANT';

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

export function dateToTimestamp(d: Date): Timestamp {
  const ms = d.getTime();
  const seconds = Math.floor(ms / 1000);
  return { seconds, nanos: (ms - seconds * 1000) * 1_000_000 };
}

export function msToTimestamp(ms: number): Timestamp {
  const seconds = Math.floor(ms / 1000);
  return { seconds, nanos: (ms - seconds * 1000) * 1_000_000 };
}

// ---------------------------------------------------------------------------
// Row -> proto message mappers
// ---------------------------------------------------------------------------

export function roleAssignmentToProto(r: RoleAssignmentRow): RoleAssignment {
  return {
    id: r.id,
    user: r.user,
    role: fromDb(ROLE_FROM_DB, r.role, 'ROLE_UNSPECIFIED'),
    permission: fromDb(PERMISSION_FROM_DB, r.permission, 'PERMISSION_UNSPECIFIED'),
    active: r.active,
    rentPayer: r.rent_payer,
    assignedBy: r.assigned_by,
    assignedAtBlock: r.assigned_at_block,
    assignedAt: dateToTimestamp(r.assigned_at),
    assignedInTx: r.assigned_in_tx,
    updatedAtBlock: r.updated_at_block,
    updatedAt: dateToTimestamp(r.updated_at),
    removedAtBlock: r.removed_at_block ?? undefined,
    removedAt: r.removed_at ? dateToTimestamp(r.removed_at) : undefined,
    removedInTx: r.removed_in_tx ?? undefined,
    removalKind: fromDb(REMOVAL_KIND_FROM_DB, r.removal_kind, 'REMOVAL_KIND_UNSPECIFIED'),
    removedBy: r.removed_by ?? undefined,
  };
}

export function adminToProto(r: AdminRow): Admin {
  return {
    address: r.id,
    active: r.active,
    addedBy: r.added_by,
    addedAtBlock: r.added_at_block,
    addedAt: dateToTimestamp(r.added_at),
    addedInTx: r.added_in_tx,
    removedAtBlock: r.removed_at_block ?? undefined,
    removedAt: r.removed_at ? dateToTimestamp(r.removed_at) : undefined,
    removedInTx: r.removed_in_tx ?? undefined,
  };
}

export function configToProto(r: ConfigRow): ConfigResponse {
  return {
    authority: r.authority,
    pendingAuthority: r.pending_authority ?? undefined,
    updatedAtBlock: r.updated_at_block,
    updatedAt: dateToTimestamp(r.updated_at),
    updatedInTx: r.updated_in_tx,
  };
}

export function actionToProto(r: WhitelistActionRow): WhitelistAction {
  return {
    id: r.id,
    type: fromDb(ACTION_TYPE_FROM_DB, r.type, 'ACTION_TYPE_UNSPECIFIED'),
    subject: r.subject ?? undefined,
    role: fromDb(ROLE_FROM_DB, r.role, 'ROLE_UNSPECIFIED'),
    permission: fromDb(PERMISSION_FROM_DB, r.permission, 'PERMISSION_UNSPECIFIED'),
    actor: r.actor,
    blockHeight: r.block_height,
    blockTime: dateToTimestamp(r.block_time),
    txSignature: r.tx_signature,
    instructionIndex: r.instruction_index,
  };
}
