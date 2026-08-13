import { SolanaInstruction } from "@subql/types-solana";
import {
  InitializeConfigInstruction,
  UpdateAuthorityInstruction,
  AcceptAuthorityInstruction,
  AddAdminInstruction,
  RemoveAdminInstruction,
  AssignRoleInstruction,
  RemoveRoleInstruction,
  RenounceRoleInstruction,
  SetPermissionInstruction,
} from "../types/handler-inputs/2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn";
import {
  Role as OnchainRole,
  AccessPermission as OnchainPermission,
} from "../types/program-interfaces/2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn/types";
import {
  Config,
  Admin,
  RoleAssignment,
  WhitelistAction,
  Role,
  Permission,
  RemovalKind,
  ActionType,
} from "../types";

/** Singleton entity id for the program config. */
const CONFIG_ID = "config";

/**
 * Sandbox-friendly assert (no node:assert import, which the SubQuery sandbox
 * blocks without --unsafe). Indexing halts loudly on violated invariants —
 * for a compliance registry, data integrity beats liveness.
 */
function invariant(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(`Invariant violated: ${message}`);
  }
}

/** On-chain borsh variant index -> schema enum. Order mirrors the Rust enum. */
const ROLE_BY_INDEX: Record<OnchainRole, Role> = {
  [OnchainRole.RegionalOperator]: Role.REGIONAL_OPERATOR,
  [OnchainRole.RealEstateInvestor]: Role.REAL_ESTATE_INVESTOR,
  [OnchainRole.RealEstateDeveloper]: Role.REAL_ESTATE_DEVELOPER,
  [OnchainRole.Lawyer]: Role.LAWYER,
  [OnchainRole.LettingAgent]: Role.LETTING_AGENT,
  [OnchainRole.SpvConfirmation]: Role.SPV_CONFIRMATION,
};

const PERMISSION_BY_INDEX: Record<OnchainPermission, Permission> = {
  [OnchainPermission.Compliant]: Permission.COMPLIANT,
  [OnchainPermission.Revoked]: Permission.REVOKED,
};

interface IxMeta {
  txSignature: string;
  blockHeight: bigint;
  blockTime: Date;
  instructionIndex: string;
}

function metaOf(ix: SolanaInstruction): IxMeta {
  return {
    txSignature: ix.transaction.transaction.signatures[0],
    blockHeight: BigInt(ix.block.blockHeight),
    blockTime: new Date(Number(ix.block.blockTime) * 1000),
    instructionIndex: ix.index.join("."),
  };
}

/**
 * Resolve the address of the instruction's account at `position` (the
 * position within the instruction's account list, which indexes into the
 * transaction's combined static + lookup-table account keys).
 */
function accountAt(ix: SolanaInstruction, position: number): string {
  const tx = ix.transaction;
  const all = [
    ...tx.transaction.message.accountKeys,
    ...(tx.meta?.loadedAddresses.writable ?? []),
    ...(tx.meta?.loadedAddresses.readonly ?? []),
  ];
  const keyIndex = ix.accounts[position];
  invariant(
    keyIndex !== undefined && keyIndex < all.length,
    `Instruction account position ${position} out of range (tx ${tx.transaction.signatures[0]})`,
  );
  return all[keyIndex];
}

/** Await the IDL-driven decode of the instruction data; fail loudly if absent. */
async function decodedArgs<T>(ix: SolanaInstruction<T>): Promise<T> {
  const decoded = await ix.decodedData;
  invariant(
    decoded,
    `Failed to decode instruction data (tx ${ix.transaction.transaction.signatures[0]})`,
  );
  return decoded.data;
}

async function recordAction(
  meta: IxMeta,
  fields: {
    type: ActionType;
    actor: string;
    subject?: string;
    role?: Role;
    permission?: Permission;
  },
): Promise<void> {
  await WhitelistAction.create({
    id: `${meta.txSignature}-${meta.instructionIndex}`,
    type: fields.type,
    subject: fields.subject,
    role: fields.role,
    permission: fields.permission,
    actor: fields.actor,
    blockHeight: meta.blockHeight,
    blockTime: meta.blockTime,
    txSignature: meta.txSignature,
    instructionIndex: meta.instructionIndex,
  }).save();
}

/** initialize_config: accounts [authority, program, program_data, config, system_program] */
export async function handleInitializeConfig(
  ix: InitializeConfigInstruction,
): Promise<void> {
  const meta = metaOf(ix);
  const authority = accountAt(ix, 0);
  logger.info(`initialize_config by ${authority} (tx ${meta.txSignature})`);

  await Config.create({
    id: CONFIG_ID,
    authority,
    pendingAuthority: undefined,
    updatedAtBlock: meta.blockHeight,
    updatedAt: meta.blockTime,
    updatedInTx: meta.txSignature,
  }).save();

  await recordAction(meta, {
    type: ActionType.CONFIG_INITIALIZED,
    actor: authority,
    subject: authority,
  });
}

/** update_authority(new_authority): accounts [authority, config] */
export async function handleUpdateAuthority(
  ix: UpdateAuthorityInstruction,
): Promise<void> {
  const meta = metaOf(ix);
  const authority = accountAt(ix, 0);
  const { newAuthority } = await decodedArgs(ix);

  const config = await Config.get(CONFIG_ID);
  invariant(config, `update_authority before config exists (tx ${meta.txSignature})`);
  config.pendingAuthority = newAuthority;
  config.updatedAtBlock = meta.blockHeight;
  config.updatedAt = meta.blockTime;
  config.updatedInTx = meta.txSignature;
  await config.save();

  await recordAction(meta, {
    type: ActionType.AUTHORITY_UPDATE_PROPOSED,
    actor: authority,
    subject: newAuthority,
  });
}

/** accept_authority: accounts [new_authority, config] */
export async function handleAcceptAuthority(
  ix: AcceptAuthorityInstruction,
): Promise<void> {
  const meta = metaOf(ix);
  const newAuthority = accountAt(ix, 0);

  const config = await Config.get(CONFIG_ID);
  invariant(config, `accept_authority before config exists (tx ${meta.txSignature})`);
  config.authority = newAuthority;
  config.pendingAuthority = undefined;
  config.updatedAtBlock = meta.blockHeight;
  config.updatedAt = meta.blockTime;
  config.updatedInTx = meta.txSignature;
  await config.save();

  await recordAction(meta, {
    type: ActionType.AUTHORITY_UPDATED,
    actor: newAuthority,
    subject: newAuthority,
  });
}

/** add_admin: accounts [authority, config, new_admin, admin, system_program] */
export async function handleAddAdmin(ix: AddAdminInstruction): Promise<void> {
  const meta = metaOf(ix);
  const authority = accountAt(ix, 0);
  const newAdmin = accountAt(ix, 2);
  logger.info(`add_admin ${newAdmin} (tx ${meta.txSignature})`);

  // Re-adding a previously removed admin resets the row; history is in actions.
  await Admin.create({
    id: newAdmin,
    active: true,
    addedBy: authority,
    addedAtBlock: meta.blockHeight,
    addedAt: meta.blockTime,
    addedInTx: meta.txSignature,
    removedAtBlock: undefined,
    removedAt: undefined,
    removedInTx: undefined,
  }).save();

  await recordAction(meta, {
    type: ActionType.ADMIN_ADDED,
    actor: authority,
    subject: newAdmin,
  });
}

/** remove_admin(admin_key): accounts [authority, config, admin] */
export async function handleRemoveAdmin(
  ix: RemoveAdminInstruction,
): Promise<void> {
  const meta = metaOf(ix);
  const authority = accountAt(ix, 0);
  const { adminKey } = await decodedArgs(ix);

  const admin = await Admin.get(adminKey);
  invariant(admin, `remove_admin for unknown admin ${adminKey} (tx ${meta.txSignature})`);
  admin.active = false;
  admin.removedAtBlock = meta.blockHeight;
  admin.removedAt = meta.blockTime;
  admin.removedInTx = meta.txSignature;
  await admin.save();

  await recordAction(meta, {
    type: ActionType.ADMIN_REMOVED,
    actor: authority,
    subject: adminKey,
  });
}

/** assign_role(role): accounts [admin_signer, admin, user, role_account, system_program] */
export async function handleAssignRole(ix: AssignRoleInstruction): Promise<void> {
  const meta = metaOf(ix);
  const adminSigner = accountAt(ix, 0);
  const user = accountAt(ix, 2);
  const { role } = await decodedArgs(ix);
  const schemaRole = ROLE_BY_INDEX[role];
  invariant(schemaRole, `Unknown role variant ${role} (tx ${meta.txSignature})`);
  logger.info(`assign_role ${schemaRole} -> ${user} (tx ${meta.txSignature})`);

  // Id mirrors the on-chain PDA identity ["role", user, role_byte]. A
  // re-assignment after removal recreates the row (audit trail in actions).
  await RoleAssignment.create({
    id: `${user}-${role}`,
    user,
    role: schemaRole,
    permission: Permission.COMPLIANT,
    active: true,
    rentPayer: adminSigner,
    assignedBy: adminSigner,
    assignedAtBlock: meta.blockHeight,
    assignedAt: meta.blockTime,
    assignedInTx: meta.txSignature,
    updatedAtBlock: meta.blockHeight,
    updatedAt: meta.blockTime,
    removedAtBlock: undefined,
    removedAt: undefined,
    removedInTx: undefined,
    removalKind: undefined,
    removedBy: undefined,
  }).save();

  await recordAction(meta, {
    type: ActionType.ROLE_ASSIGNED,
    actor: adminSigner,
    subject: user,
    role: schemaRole,
  });
}

async function closeRoleAssignment(
  meta: IxMeta,
  user: string,
  roleIndex: OnchainRole,
  removedBy: string,
  kind: RemovalKind,
): Promise<Role> {
  const schemaRole = ROLE_BY_INDEX[roleIndex];
  invariant(schemaRole, `Unknown role variant ${roleIndex} (tx ${meta.txSignature})`);

  const assignment = await RoleAssignment.get(`${user}-${roleIndex}`);
  invariant(
    assignment,
    `Role removal for unknown assignment ${user}-${roleIndex} (tx ${meta.txSignature})`,
  );
  assignment.active = false;
  assignment.removalKind = kind;
  assignment.removedBy = removedBy;
  assignment.removedAtBlock = meta.blockHeight;
  assignment.removedAt = meta.blockTime;
  assignment.removedInTx = meta.txSignature;
  assignment.updatedAtBlock = meta.blockHeight;
  assignment.updatedAt = meta.blockTime;
  await assignment.save();
  return schemaRole;
}

/** remove_role(role): accounts [admin_signer, admin, user, rent_payer, role_account] */
export async function handleRemoveRole(ix: RemoveRoleInstruction): Promise<void> {
  const meta = metaOf(ix);
  const adminSigner = accountAt(ix, 0);
  const user = accountAt(ix, 2);
  const { role } = await decodedArgs(ix);

  const schemaRole = await closeRoleAssignment(
    meta,
    user,
    role,
    adminSigner,
    RemovalKind.REMOVED,
  );

  await recordAction(meta, {
    type: ActionType.ROLE_REMOVED,
    actor: adminSigner,
    subject: user,
    role: schemaRole,
  });
}

/** renounce_role(role): accounts [user, rent_payer, role_account] */
export async function handleRenounceRole(
  ix: RenounceRoleInstruction,
): Promise<void> {
  const meta = metaOf(ix);
  const user = accountAt(ix, 0);
  const { role } = await decodedArgs(ix);

  const schemaRole = await closeRoleAssignment(
    meta,
    user,
    role,
    user,
    RemovalKind.RENOUNCED,
  );

  await recordAction(meta, {
    type: ActionType.ROLE_RENOUNCED,
    actor: user,
    subject: user,
    role: schemaRole,
  });
}

/** set_permission(role, permission): accounts [admin_signer, admin, user, role_account] */
export async function handleSetPermission(
  ix: SetPermissionInstruction,
): Promise<void> {
  const meta = metaOf(ix);
  const adminSigner = accountAt(ix, 0);
  const user = accountAt(ix, 2);
  const { role, permission } = await decodedArgs(ix);
  const schemaRole = ROLE_BY_INDEX[role];
  const schemaPermission = PERMISSION_BY_INDEX[permission];
  invariant(schemaRole, `Unknown role variant ${role} (tx ${meta.txSignature})`);
  invariant(
    schemaPermission,
    `Unknown permission variant ${permission} (tx ${meta.txSignature})`,
  );

  const assignment = await RoleAssignment.get(`${user}-${role}`);
  invariant(
    assignment,
    `set_permission for unknown assignment ${user}-${role} (tx ${meta.txSignature})`,
  );
  assignment.permission = schemaPermission;
  assignment.updatedAtBlock = meta.blockHeight;
  assignment.updatedAt = meta.blockTime;
  await assignment.save();

  await recordAction(meta, {
    type: ActionType.PERMISSION_UPDATED,
    actor: adminSigner,
    subject: user,
    role: schemaRole,
    permission: schemaPermission,
  });
}
