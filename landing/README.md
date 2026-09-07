# Shade landing page

The marketing page for Shade. It is a standalone Astro site: it is not part of the
root Bun workspace and it does not build or link against the Rust crates.

Every number and quoted constraint on the page comes from the repository docs
(`README.md`, `docs/STATUS.md`, `docs/ARCHITECTURE.md`) and is footnoted with the
conditions it was recorded under.

## Develop

```sh
bun install
bun run dev
```

## Build

```sh
bun run build     # static output in dist/
bun run preview   # serve dist/ locally
bun run check     # astro check
```

## Configuration

`src/consts.ts` holds the two external URLs the page points at:

- `REPO_URL` is the GitHub repository, `https://github.com/OmarAlex24/shade`.
  Change it there and every link on the page follows.
- `SITE_URL` is the canonical origin used for `og:url`, the canonical link and the
  sitemap. It is still a placeholder (`https://omaralex24.github.io/shade`) and must
  be set to the domain the site is served from. `astro.config.mjs` keeps its own copy
  because Astro loads that config outside the Vite graph, so change **both**.

## Deploy on Dokploy

The site is a static Astro build, so it deploys as a Dokploy **Application** using
the **Dockerfile** build type. Dokploy's own `Static` build type is not usable here:
it mounts the source directory straight into nginx without running a build step, and
this site has to be compiled first.

Create an Application and set:

| Section  | Field                | Value                                  |
| -------- | -------------------- | -------------------------------------- |
| Provider | Source               | `Git`                                  |
| Provider | Repository           | `https://github.com/OmarAlex24/shade`  |
| Provider | Branch               | `main`                                 |
| Provider | Build Path           | `landing`                              |
| Build    | Build Type           | `Dockerfile`                           |
| Build    | Dockerfile Path      | `landing/Dockerfile`                   |
| Build    | Docker Context Path  | `landing`                              |
| Build    | Docker Build Stage   | *(empty — the last stage is the one)*  |
| Domains  | Host                 | the domain you are serving from        |
| Domains  | Container Port       | `80`                                   |
| Domains  | HTTPS                | on, certificate provider `Let's Encrypt` |

Dokploy resolves both the Dockerfile path and the Docker context path from the
repository root rather than from `Build Path`, and the context has to be `landing`
because the Dockerfile copies `package.json`, `bun.lock`, `Caddyfile` and the sources
relative to itself — with the root as context those `COPY` paths do not resolve and
the build fails on `COPY Caddyfile /etc/caddy/Caddyfile` with `"/Caddyfile": not found`.

Before the first deploy, set `SITE_URL` in `src/consts.ts` **and** `astro.config.mjs`
to the domain configured above. It is baked into `og:url`, the canonical link and
`sitemap-index.xml` at build time, so a redeploy is needed after changing it.

### What the image does

`Dockerfile` is two stages. Both base images are pinned to an exact version, and
the runtime stage never touches the network — nothing is compiled, fetched or
generated at container start:

1. **build** — `oven/bun:1.3.14-alpine`, pinned to the Bun release that produced
   `bun.lock`. Runs `bun install --frozen-lockfile`, then `bun run build`, then
   `scripts/precompress.ts`, which writes maximum-quality `.br` and `.gz` siblings
   for every text asset in `dist/`.
2. **runtime** — `caddy:2.11.4-alpine` serving `dist/` from `/srv` on port 80 with
   `Caddyfile`. It serves the pre-compressed siblings when the client accepts them,
   sends `Cache-Control: public, max-age=31536000, immutable` for the content-hashed
   `/_astro/*` output and `public, max-age=0, must-revalidate` for everything else,
   and returns a plain 404 for unknown paths — this is a single page, not an SPA, so
   nothing is rewritten back to `index.html`.

Build and run it exactly as Dokploy will, from the repository root:

```sh
docker build -t shade-landing landing/
docker run --rm -p 8080:80 shade-landing
```
