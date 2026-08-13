import { status as grpcStatus } from '@grpc/grpc-js';

/**
 * An error carrying a gRPC status code that is safe to expose to clients.
 * Anything else thrown by a handler is logged and mapped to INTERNAL with a
 * generic message.
 */
export class RpcError extends Error {
  public readonly code: grpcStatus;

  constructor(code: grpcStatus, message: string) {
    super(message);
    this.name = 'RpcError';
    this.code = code;
  }
}
