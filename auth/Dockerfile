# syntax=docker/dockerfile:1.7
FROM node:22-alpine AS builder
WORKDIR /app
RUN corepack enable
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN --mount=type=cache,id=clipboard-vault-pnpm,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile
COPY tsconfig.json ./
COPY src ./src
COPY migrations ./migrations
RUN pnpm build && pnpm prune --prod

FROM node:22-alpine AS runtime
RUN addgroup -S vault && adduser -S -G vault vault
WORKDIR /app
COPY --from=builder --chown=vault:vault /app/package.json ./
COPY --from=builder --chown=vault:vault /app/node_modules ./node_modules
COPY --from=builder --chown=vault:vault /app/dist ./dist
COPY --from=builder --chown=vault:vault /app/migrations ./migrations
USER vault
EXPOSE 3001
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s --start-interval=1s --retries=3 CMD wget -q -O - http://127.0.0.1:3001/health >/dev/null || exit 1
CMD ["node", "dist/index.js"]
