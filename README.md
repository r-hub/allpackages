# allpackages

Incrementally maintains a CRAN-like `PACKAGES` metadata file covering *all* CRAN
packages, including past versions.

Each run:

1. downloads the existing compressed metadata from R2,
2. fetches new package events from crandb (resuming after the last record held),
3. appends the new records as DCF — old records are never modified or reordered,
4. recompresses and uploads the result (gzip + zstd) back to R2,
5. purges the affected URLs from Cloudflare's cache.

It runs hourly as a GitHub Actions job
([`.github/workflows/update-metadata.yml`](.github/workflows/update-metadata.yml))
using the container image built from the [`Dockerfile`](Dockerfile).

## Usage

```
❯ ./target/debug/allpackages
allpackages 0.1.0 — update CRAN-like package repository metadata

Usage:
    allpackages run             download metadata, append new records, re-upload, then
                                purge cache (the in-memory equivalent of download + get-new
                                + update + upload + purge-cache; never rewrites old records)
    allpackages download [DST]  download and decompress the metadata object from R2 to DST
                                (default: the object key's basename, sans compression
                                suffix, in the current dir)
    allpackages get-new [DST]   download new package events from CouchDB, starting after
                                the last record's Published date in ./ALLPACKAGES; write
                                the JSON to DST (default: new.json)
    allpackages update [SRC]    convert new package events (default: new.json) to DCF and
                                append them to ./ALLPACKAGES, after a blank line, ordered by
                                Published; boundary records already present are dropped
    allpackages upload          write the max-compressed variants ./ALLPACKAGES.gz and
                                ./ALLPACKAGES.zst, then upload all OBJECT_KEYS to R2
    allpackages purge-cache     purge the configured PURGE_URLS from Cloudflare's cache
    allpackages --help          show this message

The `download` subcommand only needs the R2_* and OBJECT_KEYS variables.
The `get-new` subcommand only needs the COUCH_URL variable and a local ALLPACKAGES file.
The `update` subcommand is offline: it only reads SRC (new.json) and ALLPACKAGES.
The `upload` subcommand needs the R2_* and OBJECT_KEYS variables and a local ALLPACKAGES file.
The `purge-cache` subcommand only needs the CF_ZONE_ID, CF_API_TOKEN and PURGE_URLS variables.

Configuration is read from the environment:
    R2_ENDPOINT            https://<account>.r2.cloudflarestorage.com
    R2_BUCKET              target bucket name
    R2_ACCESS_KEY_ID       R2 / S3 access key id
    R2_SECRET_ACCESS_KEY   R2 / S3 secret access key
    OBJECT_KEYS            comma-separated metadata object keys; the same content
                           is written to each, compressed per its suffix
                           (e.g. src/contrib/PACKAGES,...PACKAGES.gz,...PACKAGES.zst)
    COUCH_URL              CouchDB URL returning the new package events JSON; a `{}`
                           placeholder is replaced with the resume startkey (used by
                           both get-new and run)
    CF_ZONE_ID             Cloudflare zone id
    CF_API_TOKEN           Cloudflare API token (cache purge)
    PURGE_URLS             comma-separated absolute URLs to purge

```
