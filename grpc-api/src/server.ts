import * as path from 'node:path';
import * as grpc from '@grpc/grpc-js';
import { status as grpcStatus } from '@grpc/grpc-js';
import * as protoLoader from '@grpc/proto-loader';
import { config } from './config';
import { closePool } from './db/pool';
import * as q from './db/queries';
import {
  actionToProto,
  adminToProto,
  configToProto,
  DB_PERMISSION_COMPLIANT,
  msToTimestamp,
  optionalActionType,
  optionalPermission,
  optionalRole,
  requireRole,
  roleAssignmentToProto,
} from './mappers';
import { RpcError } from './errors';
import type { ProtoGrpcType as WhitelistProto } from './generated/whitelist';
import type { ProtoGrpcType as HealthProto } from './generated/health';
import type { WhitelistServiceHandlers } from './generated/realxmarket/whitelist/v1/WhitelistService';
import type { HealthHandlers } from './generated/grpc/health/v1/Health';
import type { Empty__Output } from './generated/google/protobuf/Empty';
import type { CheckAccessRequest__Output } from './generated/realxmarket/whitelist/v1/CheckAccessRequest';
import type { CheckAccessResponse } from './generated/realxmarket/whitelist/v1/CheckAccessResponse';
import type { ConfigResponse } from './generated/realxmarket/whitelist/v1/ConfigResponse';
import type { GetRoleAssignmentRequest__Output } from './generated/realxmarket/whitelist/v1/GetRoleAssignmentRequest';
import type { IndexerStatusResponse } from './generated/realxmarket/whitelist/v1/IndexerStatusResponse';
import type { ListActionsRequest__Output } from './generated/realxmarket/whitelist/v1/ListActionsRequest';
import type { ListActionsResponse } from './generated/realxmarket/whitelist/v1/ListActionsResponse';
import type { ListAdminsRequest__Output } from './generated/realxmarket/whitelist/v1/ListAdminsRequest';
import type { ListAdminsResponse } from './generated/realxmarket/whitelist/v1/ListAdminsResponse';
import type { ListRoleAssignmentsRequest__Output } from './generated/realxmarket/whitelist/v1/ListRoleAssignmentsRequest';
import type { ListRoleAssignmentsResponse } from './generated/realxmarket/whitelist/v1/ListRoleAssignmentsResponse';
import type { RoleAssignment } from './generated/realxmarket/whitelist/v1/RoleAssignment';

// ---------------------------------------------------------------------------
// Logging — one JSON line per event on stdout
// ---------------------------------------------------------------------------

function log(fields: Record<string, unknown>): void {
  console.log(JSON.stringify({ ts: new Date().toISOString(), ...fields }));
}

function errorMessage(err: unknown): string {
  // pg connection failures surface as AggregateError with an empty message.
  if (err instanceof AggregateError && err.errors.length > 0) {
    return err.errors.map((e) => errorMessage(e)).join('; ');
  }
  if (err instanceof Error) {
    return err.message !== '' ? err.message : `${err.constructor.name} (no message)`;
  }
  return String(err);
}

// ---------------------------------------------------------------------------
// Proto loading — protos are loaded from disk at runtime, resolved relative
// to this compiled file (dist/server.js -> <root>/proto). The Dockerfile
// copies proto/ next to dist/ so the same relative path works in the image.
// ---------------------------------------------------------------------------

const PROTO_DIR = path.resolve(__dirname, '..', 'proto');

// Must match the flags passed to proto-loader-gen-types in package.json.
const LOADER_OPTIONS: protoLoader.Options = {
  longs: String,
  enums: String,
  defaults: true,
  oneofs: true,
  includeDirs: [PROTO_DIR],
};

const whitelistProto = grpc.loadPackageDefinition(
  protoLoader.loadSync(path.join(PROTO_DIR, 'whitelist.proto'), LOADER_OPTIONS),
) as unknown as WhitelistProto;

const healthProto = grpc.loadPackageDefinition(
  protoLoader.loadSync(path.join(PROTO_DIR, 'health', 'v1', 'health.proto'), LOADER_OPTIONS),
) as unknown as HealthProto;

// ---------------------------------------------------------------------------
// Handler plumbing
// ---------------------------------------------------------------------------

function toServiceError(err: unknown, method: string): { code: grpcStatus; error: grpc.ServerErrorResponse } {
  if (err instanceof RpcError) {
    return { code: err.code, error: Object.assign(new Error(err.message), { code: err.code }) };
  }
  // Unknown (usually DB) error: log details, return a generic INTERNAL.
  log({
    level: 'error',
    msg: 'rpc_internal_error',
    method,
    error: errorMessage(err),
    stack: err instanceof Error ? err.stack : undefined,
  });
  return {
    code: grpcStatus.INTERNAL,
    error: Object.assign(new Error('internal error'), { code: grpcStatus.INTERNAL }),
  };
}

/** Wraps an async unary implementation with logging + error mapping. */
function unary<Req, Res>(method: string, impl: (req: Req) => Promise<Res>): grpc.handleUnaryCall<Req, Res> {
  return (call, callback) => {
    const start = Date.now();
    impl(call.request).then(
      (res) => {
        log({ msg: 'rpc', method, ms: Date.now() - start, code: grpcStatus.OK, codeName: 'OK' });
        callback(null, res);
      },
      (err: unknown) => {
        const mapped = toServiceError(err, method);
        log({
          msg: 'rpc',
          method,
          ms: Date.now() - start,
          code: mapped.code,
          codeName: grpcStatus[mapped.code],
        });
        callback(mapped.error, null);
      },
    );
  };
}

function requireNonEmpty(value: string | undefined, field: string): string {
  if (value === undefined || value.trim() === '') {
    throw new RpcError(grpcStatus.INVALID_ARGUMENT, `${field} must be a non-empty string`);
  }
  return value;
}

const DEFAULT_PAGE_SIZE = 50;
const MAX_PAGE_SIZE = 500;

function paging(pageSize: number, page: number): { limit: number; offset: number } {
  const limit = pageSize > 0 ? Math.min(pageSize, MAX_PAGE_SIZE) : DEFAULT_PAGE_SIZE;
  return { limit, offset: page * limit };
}

// ---------------------------------------------------------------------------
// WhitelistService handlers
// ---------------------------------------------------------------------------

const whitelistHandlers: WhitelistServiceHandlers = {
  CheckAccess: unary<CheckAccessRequest__Output, CheckAccessResponse>('CheckAccess', async (req) => {
    const user = requireNonEmpty(req.user, 'user');
    const role = requireRole(req.role);
    const row = await q.getRoleAssignment(user, role);
    if (!row || !row.active) {
      return { hasRole: false, compliant: false };
    }
    return {
      hasRole: true,
      compliant: row.permission === DB_PERMISSION_COMPLIANT,
      assignment: roleAssignmentToProto(row),
    };
  }),

  GetConfig: unary<Empty__Output, ConfigResponse>('GetConfig', async () => {
    const row = await q.getConfig();
    if (!row) {
      throw new RpcError(
        grpcStatus.NOT_FOUND,
        'config not found (indexer has not processed initialize_config yet)',
      );
    }
    return configToProto(row);
  }),

  GetRoleAssignment: unary<GetRoleAssignmentRequest__Output, RoleAssignment>(
    'GetRoleAssignment',
    async (req) => {
      const user = requireNonEmpty(req.user, 'user');
      const role = requireRole(req.role);
      const row = await q.getRoleAssignment(user, role);
      if (!row) {
        throw new RpcError(grpcStatus.NOT_FOUND, `no role assignment for user=${user} role=${role}`);
      }
      return roleAssignmentToProto(row);
    },
  ),

  ListRoleAssignments: unary<ListRoleAssignmentsRequest__Output, ListRoleAssignmentsResponse>(
    'ListRoleAssignments',
    async (req) => {
      const { limit, offset } = paging(req.pageSize, req.page);
      const page = await q.listRoleAssignments(
        {
          user: req.user !== undefined ? requireNonEmpty(req.user, 'user') : undefined,
          role: optionalRole(req.role),
          permission: optionalPermission(req.permission),
          active: req.active,
        },
        limit,
        offset,
      );
      return { assignments: page.rows.map(roleAssignmentToProto), totalCount: page.totalCount };
    },
  ),

  ListAdmins: unary<ListAdminsRequest__Output, ListAdminsResponse>('ListAdmins', async (req) => {
    const { limit, offset } = paging(req.pageSize, req.page);
    const page = await q.listAdmins(req.active, limit, offset);
    return { admins: page.rows.map(adminToProto), totalCount: page.totalCount };
  }),

  ListActions: unary<ListActionsRequest__Output, ListActionsResponse>('ListActions', async (req) => {
    const { limit, offset } = paging(req.pageSize, req.page);
    const page = await q.listActions(
      {
        subject: req.subject !== undefined ? requireNonEmpty(req.subject, 'subject') : undefined,
        actor: req.actor !== undefined ? requireNonEmpty(req.actor, 'actor') : undefined,
        type: optionalActionType(req.type),
        txSignature:
          req.txSignature !== undefined ? requireNonEmpty(req.txSignature, 'tx_signature') : undefined,
      },
      limit,
      offset,
    );
    return { actions: page.rows.map(actionToProto), totalCount: page.totalCount };
  }),

  GetIndexerStatus: unary<Empty__Output, IndexerStatusResponse>('GetIndexerStatus', async () => {
    // Reaching this line means the metadata query succeeded, i.e. the DB is
    // reachable — so healthy = indexerHealthy && dbReachable collapses to the
    // flag itself. A DB failure surfaces as INTERNAL instead.
    const meta = await q.getIndexerMetadata();
    const last = meta.lastProcessedHeight ?? 0;
    const head = meta.targetHeight ?? 0;
    const lag = Math.max(0, head - last);
    return {
      lastProcessedSlot: String(last),
      chainHeadSlot: String(head),
      lastProcessedAt:
        meta.lastProcessedTimestamp !== undefined ? msToTimestamp(meta.lastProcessedTimestamp) : undefined,
      healthy: meta.indexerHealthy === true,
      lagSlots: String(lag),
    };
  }),
};

// ---------------------------------------------------------------------------
// grpc.health.v1.Health — SERVING iff `SELECT 1` succeeds (cached for 5s)
// ---------------------------------------------------------------------------

const HEALTH_CACHE_MS = 5_000;
let healthCheckedAt = 0;
let healthOk = false;

async function dbHealthy(): Promise<boolean> {
  const now = Date.now();
  if (now - healthCheckedAt < HEALTH_CACHE_MS) {
    return healthOk;
  }
  healthCheckedAt = now;
  try {
    await q.pingDb();
    healthOk = true;
  } catch (err) {
    healthOk = false;
    log({ level: 'warn', msg: 'health_db_ping_failed', error: errorMessage(err) });
  }
  return healthOk;
}

const KNOWN_SERVICES = new Set([
  '',
  'realxmarket.whitelist.v1.WhitelistService',
  'grpc.health.v1.Health',
]);

const healthHandlers: HealthHandlers = {
  Check(call, callback) {
    const start = Date.now();
    if (!KNOWN_SERVICES.has(call.request.service)) {
      log({ msg: 'rpc', method: 'Health.Check', ms: Date.now() - start, code: grpcStatus.NOT_FOUND, codeName: 'NOT_FOUND' });
      callback(
        Object.assign(new Error(`unknown service: ${call.request.service}`), {
          code: grpcStatus.NOT_FOUND,
        }),
      );
      return;
    }
    void dbHealthy().then((ok) => {
      log({ msg: 'rpc', method: 'Health.Check', ms: Date.now() - start, code: grpcStatus.OK, codeName: 'OK', serving: ok });
      callback(null, { status: ok ? 'SERVING' : 'NOT_SERVING' });
    });
  },

  // Minimal Watch implementation: reports the current status once, then ends
  // the stream (this server does not push ongoing status updates).
  Watch(call) {
    const start = Date.now();
    if (!KNOWN_SERVICES.has(call.request.service)) {
      call.write({ status: 'SERVICE_UNKNOWN' });
      call.end();
      log({ msg: 'rpc', method: 'Health.Watch', ms: Date.now() - start, code: grpcStatus.OK, codeName: 'OK' });
      return;
    }
    void dbHealthy().then((ok) => {
      call.write({ status: ok ? 'SERVING' : 'NOT_SERVING' });
      call.end();
      log({ msg: 'rpc', method: 'Health.Watch', ms: Date.now() - start, code: grpcStatus.OK, codeName: 'OK', serving: ok });
    });
  },
};

// ---------------------------------------------------------------------------
// Startup / shutdown
// ---------------------------------------------------------------------------

function main(): void {
  const server = new grpc.Server();
  server.addService(whitelistProto.realxmarket.whitelist.v1.WhitelistService.service, whitelistHandlers);
  server.addService(healthProto.grpc.health.v1.Health.service, healthHandlers);

  const address = `${config.grpcHost}:${config.grpcPort}`;
  server.bindAsync(address, grpc.ServerCredentials.createInsecure(), (err, boundPort) => {
    if (err) {
      log({ level: 'error', msg: 'bind_failed', address, error: err.message });
      process.exit(1);
    }
    log({ msg: 'server_started', address, port: boundPort, schema: config.db.schema });

    // Detect historical mode eagerly (best effort). The server must come up
    // even when the DB is unreachable — health just reports NOT_SERVING and
    // detection is retried lazily on the first successful query.
    void q
      .isHistoricalMode()
      .then((historical) => log({ msg: 'db_connected', historicalMode: historical }))
      .catch((e: unknown) =>
        log({
          level: 'warn',
          msg: 'db_unreachable_at_startup',
          error: errorMessage(e),
          note: 'serving anyway; health reports NOT_SERVING until the database is reachable',
        }),
      );
  });

  let shuttingDown = false;
  const shutdown = (signal: NodeJS.Signals): void => {
    if (shuttingDown) {
      return;
    }
    shuttingDown = true;
    log({ msg: 'shutdown_started', signal, timeoutMs: config.shutdownTimeoutMs });

    const finish = async (exitCode: number): Promise<never> => {
      try {
        await closePool();
      } catch (e) {
        log({ level: 'warn', msg: 'pool_close_failed', error: errorMessage(e) });
      }
      log({ msg: 'shutdown_complete', exitCode });
      process.exit(exitCode);
    };

    const force = setTimeout(() => {
      log({ level: 'warn', msg: 'shutdown_forced' });
      server.forceShutdown();
      void finish(1);
    }, config.shutdownTimeoutMs);

    server.tryShutdown((err2) => {
      clearTimeout(force);
      if (err2) {
        log({ level: 'warn', msg: 'try_shutdown_error', error: err2.message });
      }
      void finish(err2 ? 1 : 0);
    });
  };

  process.on('SIGINT', shutdown);
  process.on('SIGTERM', shutdown);
}

main();
