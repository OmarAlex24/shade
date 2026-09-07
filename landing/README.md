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

`src/consts.ts` holds the two placeholder URLs the page points at:

- `REPO_URL` is the GitHub repository. Shade is not published yet, so this is a
  placeholder. Change it there and every link on the page follows.
- `SITE_URL` is the canonical origin used for `og:url`, the canonical link and the
  sitemap. `astro.config.mjs` reads it.
