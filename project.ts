import {
  SolanaProject,
  SolanaDatasourceKind,
  SolanaHandlerKind,
} from "@subql/types-solana";

/** xcavate-whitelist program on Solana devnet (see addresses.json). */
export const WHITELIST_PROGRAM_ID =
  "2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn";

/** Slot the program was deployed in — full history starts here. */
const DEPLOYMENT_SLOT = 483386556;

const project: SolanaProject = {
  specVersion: "1.0.0",
  version: "0.1.0",
  name: "realxmarket-indexer",
  description:
    "Indexes the xcavate-whitelist program (roles and compliance registry) of the realXmarket protocol on Solana devnet",
  runner: {
    node: {
      name: "@subql/node-solana",
      version: ">=6.0.0",
    },
    query: {
      name: "@subql/query",
      version: "*",
    },
  },
  schema: {
    file: "./schema.graphql",
  },
  network: {
    // Solana devnet genesis hash. The node validates this against the
    // endpoint's getGenesisHash and refuses to start on a mismatch.
    chainId: "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG",
    // Public fallback only. Deployments override this with the keyed Alchemy
    // endpoint via `--network-endpoint` (see docker-compose.yml) so the API
    // key is never committed or baked into an image.
    endpoint: ["https://api.devnet.solana.com"],
  },
  dataSources: [
    {
      kind: SolanaDatasourceKind.Runtime,
      startBlock: DEPLOYMENT_SLOT,
      assets: new Map([
        [WHITELIST_PROGRAM_ID, { file: "./idls/xcavate_whitelist.idl.json" }],
      ]),
      mapping: {
        file: "./dist/index.js",
        handlers: [
          {
            kind: SolanaHandlerKind.Instruction,
            handler: "handleInitializeConfig",
            filter: {
              programId: WHITELIST_PROGRAM_ID,
              discriminator: "initialize_config",
            },
          },
          {
            kind: SolanaHandlerKind.Instruction,
            handler: "handleUpdateAuthority",
            filter: {
              programId: WHITELIST_PROGRAM_ID,
              discriminator: "update_authority",
            },
          },
          {
            kind: SolanaHandlerKind.Instruction,
            handler: "handleAcceptAuthority",
            filter: {
              programId: WHITELIST_PROGRAM_ID,
              discriminator: "accept_authority",
            },
          },
          {
            kind: SolanaHandlerKind.Instruction,
            handler: "handleAddAdmin",
            filter: {
              programId: WHITELIST_PROGRAM_ID,
              discriminator: "add_admin",
            },
          },
          {
            kind: SolanaHandlerKind.Instruction,
            handler: "handleRemoveAdmin",
            filter: {
              programId: WHITELIST_PROGRAM_ID,
              discriminator: "remove_admin",
            },
          },
          {
            kind: SolanaHandlerKind.Instruction,
            handler: "handleAssignRole",
            filter: {
              programId: WHITELIST_PROGRAM_ID,
              discriminator: "assign_role",
            },
          },
          {
            kind: SolanaHandlerKind.Instruction,
            handler: "handleRemoveRole",
            filter: {
              programId: WHITELIST_PROGRAM_ID,
              discriminator: "remove_role",
            },
          },
          {
            kind: SolanaHandlerKind.Instruction,
            handler: "handleRenounceRole",
            filter: {
              programId: WHITELIST_PROGRAM_ID,
              discriminator: "renounce_role",
            },
          },
          {
            kind: SolanaHandlerKind.Instruction,
            handler: "handleSetPermission",
            filter: {
              programId: WHITELIST_PROGRAM_ID,
              discriminator: "set_permission",
            },
          },
        ],
      },
    },
  ],
  repository: "https://github.com/XcavateBlockchain/realxmarket-solana",
};

export default project;
