//! Updates CRAN-like package repository metadata:
//!
//!   1. download the existing (compressed) PACKAGES DCF from R2
//!   2. decompress it
//!   3. fetch new package events from CouchDB (JSON), resuming after the last
//!      record we already hold
//!   4. append the new records to the existing content; old records are never
//!      modified or reordered
//!   5. recompress and upload back to R2
//!   6. purge the relevant URLs from Cloudflare's cache
//!
//! All endpoints/keys are configured via environment variables (see `Config`).

mod dcf;

use std::io::{Read, Write as _};
use std::time::Duration;

use anyhow::{Context, Result};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use serde::Deserialize;

/// How a metadata object is compressed on R2, inferred from its key suffix.
/// Keys without a recognised suffix are stored uncompressed (`None`).
#[derive(Clone, Copy, PartialEq)]
enum Compression {
    None,
    Gzip,
    Zstd,
}

/// zstd compression level used for `.zst` outputs. A balance between speed and
/// size on the full ALLPACKAGES file (~10s, vs ~23s at the maximum level 22).
const ZSTD_LEVEL: i32 = 19;

impl Compression {
    fn from_key(key: &str) -> Self {
        if key.ends_with(".gz") {
            Compression::Gzip
        } else if key.ends_with(".zst") {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    fn decompress(self, bytes: &[u8]) -> Result<String> {
        let mut out = String::new();
        match self {
            Compression::None => {
                out = String::from_utf8(bytes.to_vec())?;
            }
            Compression::Gzip => {
                flate2::read::GzDecoder::new(bytes).read_to_string(&mut out)?;
            }
            Compression::Zstd => {
                zstd::stream::Decoder::new(bytes)?.read_to_string(&mut out)?;
            }
        }
        Ok(out)
    }

    /// HTTP `Content-Type` for an object stored under a key with this suffix. The
    /// uncompressed variant is plain UTF-8 DCF text; the `.gz`/`.zst` variants are
    /// opaque compressed blobs.
    fn content_type(self) -> &'static str {
        match self {
            Compression::None => "text/plain; charset=utf-8",
            _ => "application/octet-stream",
        }
    }

    fn compress(self, text: &str) -> Result<Vec<u8>> {
        self.compress_bytes(text.as_bytes())
    }

    fn compress_bytes(self, bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(match self {
            Compression::None => bytes.to_vec(),
            Compression::Gzip => {
                let mut enc =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
                enc.write_all(bytes)?;
                enc.finish()?
            }
            // gzip's `best()` above is already its maximum (level 9). For zstd
            // we cap at level 19: the max (22) takes ~2x as long on the full
            // ALLPACKAGES file for only a few percent smaller output.
            Compression::Zstd => zstd::stream::encode_all(bytes, ZSTD_LEVEL)?,
        })
    }
}

fn env_var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("missing env var {name}"))
}

/// R2 / S3 connection details plus the metadata object key. These are the only
/// settings the `download` subcommand needs.
struct R2Config {
    endpoint: String, // https://<account>.r2.cloudflarestorage.com
    bucket: String,
    access_key: String,
    secret_key: String,
    // One or more object keys holding the same metadata, each stored with the
    // compression implied by its suffix, e.g.
    // "src/contrib/PACKAGES,src/contrib/PACKAGES.gz,src/contrib/PACKAGES.zst".
    object_keys: Vec<String>,
}

impl R2Config {
    fn from_env() -> Result<Self> {
        let object_keys: Vec<String> = env_var("OBJECT_KEYS")?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if object_keys.is_empty() {
            anyhow::bail!("OBJECT_KEYS is empty");
        }
        Ok(R2Config {
            endpoint: env_var("R2_ENDPOINT")?,
            bucket: env_var("R2_BUCKET")?,
            access_key: env_var("R2_ACCESS_KEY_ID")?,
            secret_key: env_var("R2_SECRET_ACCESS_KEY")?,
            object_keys,
        })
    }

    /// Pick the key to read existing metadata from. The objects all hold the
    /// same content, so prefer the most compressed variant to save bandwidth:
    /// zstd, then gzip, then uncompressed.
    fn download_key(&self) -> &str {
        let rank = |key: &str| match Compression::from_key(key) {
            Compression::Zstd => 0,
            Compression::Gzip => 1,
            Compression::None => 2,
        };
        self.object_keys
            .iter()
            .min_by_key(|k| rank(k))
            .expect("object_keys is non-empty")
    }

    /// Build the signing primitives (`Bucket` + `Credentials`) for this config.
    fn bucket_and_creds(&self) -> Result<(Bucket, Credentials)> {
        let bucket = Bucket::new(
            self.endpoint.parse()?,
            UrlStyle::Path,
            self.bucket.clone(),
            "auto".to_string(),
        )?;
        let creds = Credentials::new(self.access_key.clone(), self.secret_key.clone());
        Ok((bucket, creds))
    }
}

/// Cloudflare cache-purge settings. These are the only settings the
/// `purge-cache` subcommand needs.
struct CfConfig {
    zone_id: String,
    api_token: String,
    purge_urls: Vec<String>,
}

impl CfConfig {
    fn from_env() -> Result<Self> {
        // Comma-separated list of absolute URLs to purge.
        let purge_urls: Vec<String> = env_var("PURGE_URLS")?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if purge_urls.is_empty() {
            anyhow::bail!("PURGE_URLS is empty");
        }
        Ok(CfConfig {
            zone_id: env_var("CF_ZONE_ID")?,
            api_token: env_var("CF_API_TOKEN")?,
            purge_urls,
        })
    }
}

struct Config {
    r2: R2Config,

    // CouchDB
    couch_url: String, // full URL returning the metadata JSON

    // Cloudflare cache purge
    cf: CfConfig,
}

impl Config {
    fn from_env() -> Result<Self> {
        Ok(Config {
            r2: R2Config::from_env()?,
            couch_url: env_var("COUCH_URL")?,
            cf: CfConfig::from_env()?,
        })
    }
}

/// Signed-URL lifetime for the R2 GET/PUT actions.
const SIGN_TTL: Duration = Duration::from_secs(120);

fn usage() {
    eprintln!(
        "allpackages {} — update CRAN-like package repository metadata

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
    COUCH_URL              CouchDB URL returning the new package events JSON; a `{{}}`
                           placeholder is replaced with the resume startkey (used by
                           both get-new and run)
    CF_ZONE_ID             Cloudflare zone id
    CF_API_TOKEN           Cloudflare API token (cache purge)
    PURGE_URLS             comma-separated absolute URLs to purge",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("run") => cmd_run(),
        Some("download") => cmd_download(args.next()),
        Some("get-new") => cmd_get_new(args.next()),
        Some("update") => cmd_update(args.next()),
        Some("upload") => cmd_upload(),
        Some("purge-cache") => cmd_purge_cache(),
        Some("--help") | Some("-h") | None => {
            usage();
            Ok(())
        }
        Some(other) => {
            eprintln!("error: unknown argument {other:?}\n");
            usage();
            std::process::exit(2);
        }
    }
}

/// Shared blocking HTTP client used by every subcommand.
fn http_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?)
}

/// `download` subcommand: fetch the compressed metadata object from R2,
/// decompress it, and write the uncompressed content to a local file.
fn cmd_download(dst: Option<String>) -> Result<()> {
    let cfg = R2Config::from_env()?;
    let http = http_client()?;
    let (bucket, creds) = cfg.bucket_and_creds()?;

    // The objects all hold the same content, so download the most compressed
    // variant to save bandwidth.
    let key = cfg.download_key();
    let compression = Compression::from_key(key);

    // Default destination: the object key's last path segment in the cwd, with
    // the compression suffix stripped since we write the decompressed content.
    let dst = dst.unwrap_or_else(|| {
        let name = key.rsplit('/').next().unwrap_or(key);
        name.strip_suffix(".zst")
            .or_else(|| name.strip_suffix(".gz"))
            .unwrap_or(name)
            .to_string()
    });

    let action = bucket.get_object(Some(&creds), key);
    let url = action.sign(SIGN_TTL);
    let bytes = http
        .get(url)
        .send()?
        .error_for_status()
        .with_context(|| format!("downloading {key}"))?
        .bytes()?;

    let text = compression
        .decompress(&bytes)
        .with_context(|| format!("decompressing {key}"))?;

    std::fs::write(&dst, &text).with_context(|| format!("writing {dst}"))?;
    eprintln!(
        "downloaded {} -> {} ({} bytes compressed, {} bytes decompressed)",
        key,
        dst,
        bytes.len(),
        text.len()
    );
    Ok(())
}

/// Local file holding the current metadata, read by `get-new` to find where to
/// resume fetching from CouchDB.
const ALLPACKAGES_FILE: &str = "ALLPACKAGES";

/// Find the `Published` value of the last record in a DCF document. Each record
/// carries at most one `Published` field, so the last such line in the file
/// belongs to the last record.
fn last_published(text: &str) -> Result<&str> {
    const MARKER: &str = "Published:";
    let start = text
        .rfind(&format!("\n{MARKER}"))
        .map(|i| i + 1) // skip the newline
        .or_else(|| text.starts_with(MARKER).then_some(0))
        .context("no Published field found in ALLPACKAGES")?;
    let line = text[start..].lines().next().unwrap_or("");
    Ok(line[MARKER.len()..].trim())
}

/// Turn a DCF `Published` timestamp into a CouchDB `startkey` value, e.g.
/// `2026-06-26 07:20:08 UTC` -> `2026-06-26T07:20:08+00:00`: drop the trailing
/// ` UTC`, put a `T` between date and time, and append the `+00:00` offset.
fn published_to_startkey(published: &str) -> String {
    let s = published
        .trim()
        .strip_suffix(" UTC")
        .unwrap_or(published.trim());
    format!("{}+00:00", s.replacen(' ', "T", 1))
}

/// Local file with new package events as produced by `get-new`: a JSON array of
/// `{date, name, event, package: {...}}` objects, where `package` holds the
/// DESCRIPTION-style metadata (dependency fields as `{name: constraint}` maps).
const NEW_JSON_FILE: &str = "new.json";

/// `get-new` subcommand: fetch new package events from CouchDB, starting just
/// after the last record we already have. The resume point is the last
/// `Published` date in the local ALLPACKAGES file, substituted into the `{}`
/// placeholder in COUCH_URL. The downloaded JSON is written to DST, or stdout.
fn cmd_get_new(dst: Option<String>) -> Result<()> {
    let url_template = env_var("COUCH_URL")?;

    let allpackages = std::fs::read_to_string(ALLPACKAGES_FILE)
        .with_context(|| format!("reading {ALLPACKAGES_FILE}"))?;
    let startkey = published_to_startkey(last_published(&allpackages)?);
    let url = url_template.replace("{}", &startkey);
    eprintln!("fetching {url}");

    let http = http_client()?;
    let body = http
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?
        .text()?;

    let path = dst.unwrap_or_else(|| NEW_JSON_FILE.to_string());
    std::fs::write(&path, &body).with_context(|| format!("writing {path}"))?;
    eprintln!("wrote {} bytes to {}", body.len(), path);
    Ok(())
}

/// One element of the `get-new` JSON array. We only need the event `date` (for
/// the `Published` field) and the embedded `package` metadata.
#[derive(Deserialize)]
struct NewEvent {
    date: String,
    package: serde_json::Value,
}

/// Collapse all internal whitespace runs (including the newlines used to fold
/// long DESCRIPTION lines) into single spaces, and trim the ends.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Turn a `get-new` event `date` into a DCF `Published` value, e.g.
/// `2026-06-26T07:20:08+00:00` -> `2026-06-26 07:20:08 UTC`: drop the `+00:00`
/// offset, replace the `T` with a space, and append ` UTC`. This is the inverse
/// of [`published_to_startkey`].
fn date_to_published(date: &str) -> String {
    let s = date.trim();
    let s = s.strip_suffix("+00:00").unwrap_or(s);
    format!("{} UTC", s.replacen('T', " ", 1))
}

/// Render a dependency-style field (a JSON object mapping package name to a
/// version constraint) in CRAN PACKAGES style, preserving the declared order:
/// `{"R": ">= 3.5", "stats": "*"}` -> `R (>= 3.5), stats`. A `*` (or empty)
/// constraint means "any version", so only the bare name is emitted.
fn render_deps(v: &serde_json::Value) -> Option<String> {
    let obj = v.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let parts: Vec<String> = obj
        .iter()
        .map(|(name, ver)| {
            let ver = ver.as_str().map(collapse_ws).unwrap_or_default();
            if ver.is_empty() || ver == "*" {
                name.clone()
            } else {
                format!("{name} ({ver})")
            }
        })
        .collect();
    Some(parts.join(", "))
}

/// Copy a plain string field from the source `package` object into `rec` under
/// the (possibly differently cased) ALLPACKAGES field name, collapsing folded
/// whitespace. Missing or empty values are skipped.
fn set_str(
    rec: &mut dcf::Record,
    obj: &serde_json::Map<String, serde_json::Value>,
    src: &str,
    dst: &str,
) {
    if let Some(s) = obj.get(src).and_then(|v| v.as_str()) {
        let v = collapse_ws(s);
        if !v.is_empty() {
            rec.set(dst, v);
        }
    }
}

/// Reconstruct a dependency field, mapping the source key to the ALLPACKAGES
/// field name (e.g. `LinkingTo` -> `Linkingto`). Missing/empty maps are skipped.
fn set_deps(
    rec: &mut dcf::Record,
    obj: &serde_json::Map<String, serde_json::Value>,
    src: &str,
    dst: &str,
) {
    if let Some(v) = obj.get(src).and_then(render_deps) {
        rec.set(dst, v);
    }
}

/// Convert one event's `package` metadata into an ALLPACKAGES DCF record. Fields
/// are emitted in the canonical ALLPACKAGES order, keeping the standard
/// DESCRIPTION casing (`MD5sum`, `LinkingTo`, `SystemRequirements`). Returns
/// `None` for anything without a `Package` name.
fn new_event_to_record(pkg: &serde_json::Value, published: &str) -> Option<dcf::Record> {
    let obj = pkg.as_object()?;
    let name = obj.get("Package").and_then(|v| v.as_str())?;

    let mut rec = dcf::Record::default();
    rec.set("Package", name);
    set_str(&mut rec, obj, "Version", "Version");
    set_str(&mut rec, obj, "Priority", "Priority");
    set_deps(&mut rec, obj, "Depends", "Depends");
    set_deps(&mut rec, obj, "Suggests", "Suggests");
    set_deps(&mut rec, obj, "Imports", "Imports");
    set_deps(&mut rec, obj, "LinkingTo", "LinkingTo");
    set_deps(&mut rec, obj, "Enhances", "Enhances");
    set_str(&mut rec, obj, "License", "License");
    set_str(&mut rec, obj, "License_is_FOSS", "License_is_FOSS");
    set_str(
        &mut rec,
        obj,
        "License_restricts_use",
        "License_restricts_use",
    );
    set_str(&mut rec, obj, "OS_type", "OS_type");
    set_str(&mut rec, obj, "Archs", "Archs");
    set_str(&mut rec, obj, "MD5sum", "MD5sum");
    set_str(&mut rec, obj, "NeedsCompilation", "NeedsCompilation");
    set_str(&mut rec, obj, "SystemRequirements", "SystemRequirements");
    rec.set("Published", published);
    Some(rec)
}

/// Choose the separator that places appended records exactly one blank line
/// after `existing`, normalizing whatever trailing newlines it currently has.
fn append_separator(existing: &str) -> &'static str {
    if existing.ends_with("\n\n") {
        ""
    } else if existing.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    }
}

/// From new package `events`, build the records to append to `existing`: those
/// `Published` strictly after the last record we already hold, ordered by
/// `Published` ascending. CouchDB's `startkey` is inclusive, so the boundary
/// record(s) we already have are re-served and dropped here. `existing` is only
/// read, never reordered or rewritten — we assume it is already sorted by
/// `Published`.
fn new_records_to_append(existing: &str, events: &[NewEvent]) -> Result<Vec<dcf::Record>> {
    let last = last_published(existing)?;

    let mut records: Vec<(String, dcf::Record)> = events
        .iter()
        .filter_map(|e| {
            let published = date_to_published(&e.date);
            new_event_to_record(&e.package, &published).map(|rec| (published, rec))
        })
        .filter(|(published, _)| published.as_str() > last)
        .collect();

    // Order by Published ascending. The fixed `YYYY-MM-DD HH:MM:SS UTC` layout
    // sorts correctly as plain strings.
    records.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(records.into_iter().map(|(_, rec)| rec).collect())
}

/// `update` subcommand: read new package events (default `new.json`), convert
/// each to an ALLPACKAGES DCF record, drop the boundary record(s) we already
/// have, order the rest by `Published`, and append them to ALLPACKAGES after a
/// blank line. The existing content is never modified. Entirely offline.
fn cmd_update(src: Option<String>) -> Result<()> {
    let src = src.unwrap_or_else(|| NEW_JSON_FILE.to_string());
    let json = std::fs::read_to_string(&src).with_context(|| format!("reading {src}"))?;
    let events: Vec<NewEvent> =
        serde_json::from_str(&json).with_context(|| format!("parsing {src}"))?;

    let allpackages = std::fs::read_to_string(ALLPACKAGES_FILE)
        .with_context(|| format!("reading {ALLPACKAGES_FILE}"))?;

    let new_records = new_records_to_append(&allpackages, &events)?;
    if new_records.is_empty() {
        eprintln!("no new records to append");
        return Ok(());
    }
    for rec in &new_records {
        println!(
            "{} {}",
            rec.get("Package").unwrap_or("?"),
            rec.get("Version").unwrap_or("?")
        );
    }
    let new_text = dcf::write(&new_records);

    // Append after exactly one blank line, normalizing whatever trailing
    // newlines the file currently ends with. The existing content is untouched.
    let sep = append_separator(&allpackages);

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(ALLPACKAGES_FILE)
        .with_context(|| format!("opening {ALLPACKAGES_FILE}"))?;
    f.write_all(sep.as_bytes())?;
    f.write_all(new_text.as_bytes())?;

    eprintln!(
        "appended {} record(s) to {}",
        new_records.len(),
        ALLPACKAGES_FILE
    );
    Ok(())
}

/// `upload` subcommand: read the local ALLPACKAGES file, write its max-compressed
/// variants (`ALLPACKAGES.gz`, `ALLPACKAGES.zst`) alongside it, then upload the
/// same content to every R2 object key, compressed per the key's suffix.
fn cmd_upload() -> Result<()> {
    let cfg = R2Config::from_env()?;
    let http = http_client()?;
    let (bucket, creds) = cfg.bucket_and_creds()?;

    let raw =
        std::fs::read(ALLPACKAGES_FILE).with_context(|| format!("reading {ALLPACKAGES_FILE}"))?;
    eprintln!("read {ALLPACKAGES_FILE} ({} bytes)", raw.len());

    // Compress each variant once (gzip at max, zstd at ZSTD_LEVEL) and reuse the
    // bytes for the local files and the matching R2 uploads.
    eprintln!("compressing...");
    let started = std::time::Instant::now();

    let gz = Compression::Gzip.compress_bytes(&raw)?;
    eprintln!("  gzip:  {} bytes", gz.len());
    let zst = Compression::Zstd.compress_bytes(&raw)?;
    eprintln!("  zstd:  {} bytes", zst.len());
    eprintln!(
        "compression done in {:.1}s",
        started.elapsed().as_secs_f64()
    );

    // Write the compressed variants next to the local ALLPACKAGES file.
    for (suffix, body) in [(".gz", &gz), (".zst", &zst)] {
        let path = format!("{ALLPACKAGES_FILE}{suffix}");
        std::fs::write(&path, body).with_context(|| format!("writing {path}"))?;
        eprintln!("wrote {} ({} bytes)", path, body.len());
    }

    // Upload the same content to every object key, picking the variant that
    // matches the key's compression suffix.
    eprintln!("uploading {} object(s) to R2...", cfg.object_keys.len());
    let started = std::time::Instant::now();

    for key in &cfg.object_keys {
        let comp = Compression::from_key(key);
        let body = match comp {
            Compression::None => raw.clone(),
            Compression::Gzip => gz.clone(),
            Compression::Zstd => zst.clone(),
        };
        let len = body.len();
        eprintln!("  uploading {key} ({len} bytes)...");
        upload(&http, &bucket, &creds, key, body, comp.content_type())?;
        eprintln!("  uploaded {key}");
    }
    eprintln!("uploads done in {:.1}s", started.elapsed().as_secs_f64());

    Ok(())
}

/// `run` subcommand: the full download → append → upload → purge pipeline.
fn cmd_run() -> Result<()> {
    let cfg = Config::from_env()?;
    let http = http_client()?;

    let (bucket, creds) = cfg.r2.bucket_and_creds()?;

    // 1 + 2: download the existing metadata and decompress. All keys hold the
    // same content, so read the most compressed variant to save bandwidth. We
    // only ever append to this content, never rewrite it, so keep it verbatim.
    let src_key = cfg.r2.download_key();
    let src_compression = Compression::from_key(src_key);
    let existing_text = download_existing(&http, &bucket, &creds, src_key, src_compression)?;

    // 3: fetch new package events from CouchDB, resuming just after the last
    // record we already hold (the same boundary logic as `get-new`).
    let startkey = published_to_startkey(last_published(&existing_text)?);
    let url = cfg.couch_url.replace("{}", &startkey);
    let events = fetch_couch_events(&http, &url)?;
    eprintln!("incoming events: {}", events.len());

    // 4: keep only the genuinely new records, ordered by Published. The
    // inclusive `startkey` re-serves the boundary record(s) we already have,
    // which are dropped here.
    let new_records = new_records_to_append(&existing_text, &events)?;
    if new_records.is_empty() {
        eprintln!("no new packages, nothing to do");
        return Ok(());
    }
    eprintln!("new records: {}", new_records.len());
    for rec in &new_records {
        println!(
            "{} {}",
            rec.get("Package").unwrap_or("?"),
            rec.get("Version").unwrap_or("?")
        );
    }

    // 5: append the new records after a single blank line, leaving the existing
    // content untouched, then recompress and upload the same bytes to every key.
    let mut new_text = existing_text;
    let sep = append_separator(&new_text);
    new_text.push_str(sep);
    new_text.push_str(&dcf::write(&new_records));
    for key in &cfg.r2.object_keys {
        let comp = Compression::from_key(key);
        let body = comp.compress(&new_text)?;
        let len = body.len();
        upload(&http, &bucket, &creds, key, body, comp.content_type())?;
        eprintln!("uploaded {key} ({len} bytes)");
    }

    // 6: purge Cloudflare cache.
    purge_cache(
        &http,
        &cfg.cf.zone_id,
        &cfg.cf.api_token,
        &cfg.cf.purge_urls,
    )?;
    eprintln!("purged {} url(s)", cfg.cf.purge_urls.len());

    Ok(())
}

/// `purge-cache` subcommand: purge the configured PURGE_URLS from Cloudflare's
/// cache. Only needs the CF_* and PURGE_URLS variables.
fn cmd_purge_cache() -> Result<()> {
    let cfg = CfConfig::from_env()?;
    let http = http_client()?;
    purge_cache(&http, &cfg.zone_id, &cfg.api_token, &cfg.purge_urls)?;
    eprintln!("purged {} url(s)", cfg.purge_urls.len());
    Ok(())
}

fn download_existing(
    http: &reqwest::blocking::Client,
    bucket: &Bucket,
    creds: &Credentials,
    key: &str,
    compression: Compression,
) -> Result<String> {
    let action = bucket.get_object(Some(creds), key);
    let url = action.sign(SIGN_TTL);
    let bytes = http
        .get(url)
        .send()?
        .error_for_status()
        .with_context(|| format!("downloading {key}"))?
        .bytes()?;
    compression.decompress(&bytes)
}

fn upload(
    http: &reqwest::blocking::Client,
    bucket: &Bucket,
    creds: &Credentials,
    key: &str,
    body: Vec<u8>,
    content_type: &str,
) -> Result<()> {
    let action = bucket.put_object(Some(creds), key);
    let url = action.sign(SIGN_TTL);
    http.put(url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(body)
        .send()?
        .error_for_status()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CouchDB
// ---------------------------------------------------------------------------

/// Fetch new package events from CouchDB at `url`, returning the same
/// `{date, package}` events that `get-new` writes to `new.json`. This is the
/// in-memory equivalent of `get-new` + reading `new.json` in `update`.
fn fetch_couch_events(http: &reqwest::blocking::Client, url: &str) -> Result<Vec<NewEvent>> {
    eprintln!("fetching {url}");
    http.get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?
        .json()
        .context("decoding CouchDB response")
}

// ---------------------------------------------------------------------------
// Cloudflare
// ---------------------------------------------------------------------------

fn purge_cache(
    http: &reqwest::blocking::Client,
    zone_id: &str,
    token: &str,
    urls: &[String],
) -> Result<()> {
    let endpoint = format!("https://api.cloudflare.com/client/v4/zones/{zone_id}/purge_cache");
    let resp = http
        .post(endpoint)
        .bearer_auth(token)
        .json(&serde_json::json!({ "files": urls }))
        .send()?;

    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("cloudflare purge failed ({status}): {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startkey_transforms_published_timestamp() {
        assert_eq!(
            published_to_startkey("2026-06-26 07:20:08 UTC"),
            "2026-06-26T07:20:08+00:00"
        );
    }

    #[test]
    fn last_published_finds_final_record() {
        let dcf = "\
Package: a
Version: 1.0
Published: 2026-06-25 01:00:00 UTC

Package: b
Version: 2.0
Published: 2026-06-26 07:20:08 UTC
";
        assert_eq!(last_published(dcf).unwrap(), "2026-06-26 07:20:08 UTC");
    }

    #[test]
    fn date_to_published_is_inverse_of_startkey() {
        assert_eq!(
            date_to_published("2026-06-26T07:20:08+00:00"),
            "2026-06-26 07:20:08 UTC"
        );
        // Round-trips back through published_to_startkey.
        assert_eq!(
            published_to_startkey(&date_to_published("2026-06-26T07:20:08+00:00")),
            "2026-06-26T07:20:08+00:00"
        );
    }

    #[test]
    fn render_deps_preserves_order_and_constraints() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"R": ">= 3.5", "stats": "*", "posterior": ">=\n1.7.0"}"#)
                .unwrap();
        // Declared order kept; `*` -> bare name; folded version unwrapped.
        assert_eq!(
            render_deps(&v).unwrap(),
            "R (>= 3.5), stats, posterior (>= 1.7.0)"
        );
        let empty: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert_eq!(render_deps(&empty), None);
    }

    #[test]
    fn new_event_orders_fields_canonically() {
        let pkg: serde_json::Value = serde_json::from_str(
            r#"{
                "Package": "loo",
                "Version": "2.10.0",
                "Depends": {"R": ">= 3.5"},
                "Suggests": {"knitr": "*"},
                "Imports": {"checkmate": "*"},
                "License": "GPL (>= 3)",
                "MD5sum": "abc",
                "NeedsCompilation": "no",
                "SystemRequirements": "pandoc"
            }"#,
        )
        .unwrap();
        let rec = new_event_to_record(&pkg, "2026-06-26 07:20:08 UTC").unwrap();
        let text = dcf::write(std::slice::from_ref(&rec));
        let expected = "\
Package: loo
Version: 2.10.0
Depends: R (>= 3.5)
Suggests: knitr
Imports: checkmate
License: GPL (>= 3)
MD5sum: abc
NeedsCompilation: no
SystemRequirements: pandoc
Published: 2026-06-26 07:20:08 UTC
";
        assert_eq!(text, expected);
    }

    #[test]
    fn get_new_builds_expected_url() {
        let last = last_published("Published: 2026-06-26 07:20:08 UTC\n").unwrap();
        let startkey = published_to_startkey(last);
        let url = "https://crandb.r-pkg.org/-/events?startkey=%22{}%22".replace("{}", &startkey);
        assert_eq!(
            url,
            "https://crandb.r-pkg.org/-/events?startkey=%222026-06-26T07:20:08+00:00%22"
        );
    }
}
