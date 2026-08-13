import { Pool, types } from 'pg';
import type { QueryResult, QueryResultRow } from 'pg';
import { config } from '../config';

// SubQuery writes UTC wall-clock values into `timestamp without time zone`
// columns. node-postgres would otherwise parse those in the server's local
// timezone, so force UTC interpretation. (OID 1114 = TIMESTAMP WITHOUT TIME
// ZONE; TIMESTAMPTZ (1184) already carries an offset and needs no override.)
const TIMESTAMP_OID = 1114;
types.setTypeParser(TIMESTAMP_OID, (value: string) => new Date(`${value.replace(' ', 'T')}Z`));

export const pool = new Pool({
  host: config.db.host,
  port: config.db.port,
  user: config.db.user,
  password: config.db.password,
  database: config.db.database,
  max: config.db.poolMax,
  // Abort any statement running longer than 10s server-side.
  statement_timeout: 10_000,
  connectionTimeoutMillis: 5_000,
  idleTimeoutMillis: 30_000,
});

// Idle clients can error (e.g. the DB restarts). Without a listener that
// would crash the process; we only log — health checking handles visibility.
pool.on('error', (err: Error) => {
  console.log(
    JSON.stringify({
      ts: new Date().toISOString(),
      level: 'error',
      msg: 'pg_pool_error',
      error: err.message,
    }),
  );
});

/** Typed query helper. All SQL text lives in src/db/queries.ts. */
export async function query<R extends QueryResultRow = QueryResultRow>(
  text: string,
  params: unknown[] = [],
): Promise<QueryResult<R>> {
  return pool.query<R>(text, params);
}

export async function closePool(): Promise<void> {
  await pool.end();
}
