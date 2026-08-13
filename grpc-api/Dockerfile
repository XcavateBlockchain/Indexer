# syntax=docker/dockerfile:1

# ---------- Stage 1: build ----------
FROM node:22-alpine AS build
WORKDIR /app

COPY package*.json ./
RUN npm ci

COPY tsconfig.json ./
COPY proto ./proto
COPY src ./src
RUN npm run build && npm prune --omit=dev

# ---------- Stage 2: runtime ----------
FROM node:22-alpine
WORKDIR /app
ENV NODE_ENV=production

RUN addgroup -S app && adduser -S app -G app

COPY --from=build --chown=app:app /app/node_modules ./node_modules
COPY --from=build --chown=app:app /app/dist ./dist
# proto files are loaded from disk at startup (dist/server.js resolves
# __dirname/../proto == /app/proto), so they must ship with the image.
COPY --from=build --chown=app:app /app/proto ./proto

USER app
EXPOSE 50051

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD ["node", "dist/healthcheck.js"]

CMD ["node", "dist/server.js"]
