/**
 * Standalone health probe for Docker HEALTHCHECK.
 * Makes a grpc.health.v1.Health/Check RPC against localhost and exits
 * 0 when SERVING, 1 otherwise (NOT_SERVING, connection failure, timeout).
 */
import * as path from 'node:path';
import * as grpc from '@grpc/grpc-js';
import * as protoLoader from '@grpc/proto-loader';
import type { ProtoGrpcType as HealthProto } from './generated/health';

const port = process.env.GRPC_PORT && process.env.GRPC_PORT !== '' ? process.env.GRPC_PORT : '50051';

const healthDef = protoLoader.loadSync(
  path.resolve(__dirname, '..', 'proto', 'health', 'v1', 'health.proto'),
  { longs: String, enums: String, defaults: true, oneofs: true },
);
const healthProto = grpc.loadPackageDefinition(healthDef) as unknown as HealthProto;

const client = new healthProto.grpc.health.v1.Health(
  `127.0.0.1:${port}`,
  grpc.credentials.createInsecure(),
);

const deadline = new Date(Date.now() + 3_000);

client.Check({ service: '' }, { deadline }, (err, res) => {
  if (err) {
    console.error(`healthcheck: RPC failed: ${err.message}`);
    process.exit(1);
  }
  if (res?.status === 'SERVING') {
    console.log('healthcheck: SERVING');
    process.exit(0);
  }
  console.error(`healthcheck: status=${res?.status ?? 'UNKNOWN'}`);
  process.exit(1);
});
