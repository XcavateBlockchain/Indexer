/**
 * Environment-driven configuration. No dotenv on purpose — the container
 * runtime (Docker/Kubernetes) is expected to provide the environment.
 */

function envStr(name: string, def: string): string {
  const v = process.env[name];
  return v === undefined || v === '' ? def : v;
}

function envInt(name: string, def: number): number {
  const v = process.env[name];
  if (v === undefined || v === '') {
    return def;
  }
  const n = Number.parseInt(v, 10);
  if (!Number.isFinite(n)) {
    throw new Error(`Environment variable ${name} must be an integer, got: ${JSON.stringify(v)}`);
  }
  return n;
}

export interface AppConfig {
  readonly grpcHost: string;
  readonly grpcPort: number;
  readonly db: {
    readonly host: string;
    readonly port: number;
    readonly user: string;
    readonly password: string;
    readonly database: string;
    readonly schema: string;
    readonly poolMax: number;
  };
  readonly shutdownTimeoutMs: number;
}

export const config: AppConfig = {
  grpcHost: envStr('GRPC_HOST', '0.0.0.0'),
  grpcPort: envInt('GRPC_PORT', 50051),
  db: {
    host: envStr('DB_HOST', 'localhost'),
    port: envInt('DB_PORT', 5432),
    user: envStr('DB_USER', 'postgres'),
    password: envStr('DB_PASS', ''),
    database: envStr('DB_DATABASE', 'postgres'),
    schema: envStr('DB_SCHEMA', 'app'),
    poolMax: envInt('DB_POOL_MAX', 10),
  },
  shutdownTimeoutMs: envInt('SHUTDOWN_TIMEOUT_MS', 10000),
};
