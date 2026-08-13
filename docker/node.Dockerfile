# Bakes the compiled SubQuery project into the Solana indexer node image so a
# deployment is an atomic image swap (no project files rsynced to the server).

FROM node:22-slim AS builder
WORKDIR /build
COPY package.json package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY tsconfig.json project.ts schema.graphql ./
COPY idls ./idls
COPY src ./src
RUN npx subql codegen && npx subql build

FROM subquerynetwork/subql-node-solana:v6.3.1
WORKDIR /app
# The generated manifest (project.yaml) is self-contained: it references only
# schema.graphql, idls/ and the webpack-bundled dist/index.js.
COPY --from=builder /build/project.yaml /build/schema.graphql ./
COPY --from=builder /build/idls ./idls
COPY --from=builder /build/dist ./dist
CMD ["-f=/app"]
