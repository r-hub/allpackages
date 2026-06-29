# Cloud infrastructure

`allpackages` is a small Rust binary, but it only does useful work against a set
of cloud services. This document describes those services, how they fit
together, and the configuration each one needs. For *what* the program does with
them, see [`README.md`](README.md).

## Overview

```
                 GitHub Actions (cron, hourly)
                            │
                            │ docker run ghcr.io/<repo>:latest run
                            ▼
   CouchDB  ──get new events──►  allpackages  ──upload PACKAGES──►  Cloudflare R2
 (crandb)                            │                              (object storage)
                                     │
                                     └──purge changed URLs──►  Cloudflare cache
```

Each scheduled run:

1. reads the current metadata object from **R2**,
2. fetches new package events from **CouchDB**,
3. uploads the updated object back to **R2**,
4. purges the changed URLs from the **Cloudflare** edge cache.

The binary itself is built once and published as a container image to
**GHCR**, which the scheduled job pulls.

## Services

### Cloudflare R2 (object storage)

Holds the canonical `PACKAGES` metadata, in one or more objects that all carry
the same content compressed per their key suffix (plain / `.gz` / `.zst`). R2
exposes an S3-compatible API; the binary signs requests with SigV4 (via
`rusty-s3`) and drives them with `reqwest`.

Configuration:

| Variable               | Kind   | Meaning                                           |
| ---------------------- | ------ | ------------------------------------------------- |
| `R2_ENDPOINT`          | secret | `https://<account>.r2.cloudflarestorage.com`      |
| `R2_BUCKET`            | var    | Target bucket name                                |
| `R2_ACCESS_KEY_ID`     | secret | R2 / S3 access key id                             |
| `R2_SECRET_ACCESS_KEY` | secret | R2 / S3 secret access key                         |
| `OBJECT_KEYS`          | var    | Comma-separated object keys (e.g. `src/contrib/PACKAGES,src/contrib/PACKAGES.gz,src/contrib/PACKAGES.zst`) |

Used by the `download`, `upload`, and `run` subcommands. Region is fixed to
`auto` and path-style URLs are used, as R2 expects.

Uploads set `Content-Type` per variant: the uncompressed object is stored as
`text/plain; charset=utf-8` (so browsers and tools treat it as text), while the
`.gz` / `.zst` variants stay `application/octet-stream`.

### CouchDB (crandb — source of new package events)

A read-only HTTP/JSON endpoint that returns new CRAN package events after a
given resume point. The binary computes the resume `startkey` from the last
`Published` date already in the metadata and substitutes it into the `{}`
placeholder in the URL.

| Variable    | Kind   | Meaning                                                       |
| ----------- | ------ | ------------------------------------------------------------- |
| `COUCH_URL` | secret | CouchDB URL returning the events JSON; `{}` → resume startkey |

Used by the `get-new` and `run` subcommands. Only read access is needed.

### Cloudflare cache (edge purge)

After uploading new metadata, the binary purges the public URLs that serve it so
the edge re-fetches the fresh objects. Uses the Cloudflare REST API
(`/zones/<zone>/purge_cache`) with a bearer token.

| Variable       | Kind   | Meaning                                          |
| -------------- | ------ | ------------------------------------------------ |
| `CF_ZONE_ID`   | secret | Cloudflare zone id                               |
| `CF_API_TOKEN` | secret | API token with cache-purge permission            |
| `PURGE_URLS`   | var    | Comma-separated absolute URLs to purge           |

Used by the `purge-cache` and `run` subcommands. The token needs only the
`Zone → Cache Purge` permission on the relevant zone.

### GitHub Container Registry (GHCR — image hosting)

The runtime image is published to `ghcr.io/<owner>/<repo>`, tagged `:latest`
and with the commit SHA. The scheduled job pulls `:latest`.

- Built by [`.github/workflows/build-image.yml`](.github/workflows/build-image.yml)
  on pushes to `main` that touch the source, `Dockerfile`, or that workflow.
- Pulled by [`.github/workflows/update-metadata.yml`](.github/workflows/update-metadata.yml)
  as a `container:` job image.
- While the package is private the pulling job authenticates with the built-in
  `GITHUB_TOKEN` (`packages: read`). Making the package public lets you drop the
  `credentials:` block in the update workflow.

### GitHub Actions (scheduler + CI)

Three workflows under [`.github/workflows/`](.github/workflows/):

| Workflow                | Trigger                          | Purpose                                      |
| ----------------------- | -------------------------------- | -------------------------------------------- |
| `update-metadata.yml`   | hourly cron (`27 * * * *`) + manual | Runs `allpackages run` in the GHCR image  |
| `build-image.yml`       | push to `main` (source/Docker)   | Tests, builds, and pushes the image to GHCR  |
| `test.yml`              | PRs and pushes to `main`         | `cargo fmt --check`, `clippy -D warnings`, `cargo test` |

`concurrency` groups prevent overlapping runs (the update job uses
`cancel-in-progress: false` so a running update is never interrupted mid-flight).

## Where configuration lives

The same nine variables drive every environment:

- **GitHub Actions** — set in the repo's Settings:
  - *Secrets*: `R2_ENDPOINT`, `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`,
    `COUCH_URL`, `CF_ZONE_ID`, `CF_API_TOKEN`.
  - *Variables*: `R2_BUCKET`, `OBJECT_KEYS`, `PURGE_URLS`.
  - Wired into the job's `env:` in
    [`update-metadata.yml`](.github/workflows/update-metadata.yml).
- **Local runs** — exported from a `.env` file (gitignored). Source it before
  running a subcommand, e.g. `set -a; . ./.env; set +a`.

Not every subcommand needs every variable — see the table in
[`README.md`](README.md) for the per-subcommand requirements.

## Running locally against the real services

```sh
set -a; . ./.env; set +a          # load credentials (never commit .env)
cargo build --release

./target/release/allpackages download   # pull current metadata from R2
./target/release/allpackages get-new     # fetch new events from CouchDB
./target/release/allpackages update      # append them locally (offline)
./target/release/allpackages upload      # push back to R2
./target/release/allpackages purge-cache # purge the edge cache
# or the whole pipeline in one shot, entirely in memory:
./target/release/allpackages run
```

## Security notes

- `.env` is gitignored; keep all six secrets out of the repo and in GitHub
  *Secrets* (not *Variables*, which are plaintext-visible).
- The R2 keys should be scoped to just the metadata bucket; the Cloudflare token
  to just cache-purge on the relevant zone.
- Signed R2 URLs are short-lived (120 s), generated per request.
