# syntax=docker/dockerfile:1
#
# Build context is `landing/`, not the repository root: the site is a standalone
# Astro project and does not build or link against the Rust crates.
#
#   docker build -t shade-landing landing/
#
# The context MUST be `landing/`: every COPY below is relative to this file, so a
# root context fails at `COPY Caddyfile` with `"/Caddyfile": not found`.
#
# On Dokploy this is the `Dockerfile` build type with Build Path `landing`,
# Dockerfile Path `landing/Dockerfile` and Docker Context Path `landing` — Dokploy
# resolves the latter two from the repository root, not from Build Path.
# See "Deploy on Dokploy" in README.md.

# ---- build ------------------------------------------------------------------
# Pinned to the Bun release that produced bun.lock, so an image built a year from
# now resolves the same dependency tree as one built today.
FROM oven/bun:1.3.14-alpine AS build

WORKDIR /app

# Manifest first: the install layer is then reused across source-only edits.
COPY package.json bun.lock ./
RUN bun install --frozen-lockfile

COPY . .

# `astro build` emits the static site into dist/, then every text asset gets a
# maximum-quality .br and .gz sibling for the runtime stage to serve directly.
RUN bun run build && bun run scripts/precompress.ts

# ---- runtime ----------------------------------------------------------------
# Caddy serves the finished bytes. Nothing is compiled, fetched or generated at
# container start, so the runtime needs no network access at all.
FROM caddy:2.11.4-alpine AS runtime

COPY Caddyfile /etc/caddy/Caddyfile
COPY --from=build /app/dist /srv

EXPOSE 80

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD wget --quiet --spider http://127.0.0.1/ || exit 1

CMD ["caddy", "run", "--config", "/etc/caddy/Caddyfile", "--adapter", "caddyfile"]
