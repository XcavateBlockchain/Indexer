//! The property image mirror (ADR-31): `marketplace_property_image` work set ->
//! bounded HTTP GET of the source image -> 720x720 JPEG thumbnail ->
//! `PUT` to the public object storage bucket.
//!
//! ## Why a separate loop and not the account pipeline
//!
//! Same shape, same reason as the metadata fetcher (ADR-27): the source URIs are off-chain,
//! external, and may be slow, flaky, or rate-limited, and a transaction's on-chain meaning
//! does not depend on the image ever downloading. The work is durably recorded by the
//! metadata fetcher (`db::property_metadata` writes the rows, one per `propertyImages`
//! entry); this loop drains the work set -- a bounded, per-image-backed-off Postgres query --
//! so the bounded crawl/snapshot/backfill paths stay clean of arbitrary external calls.
//!
//! ## One cycle
//!
//! 1. `db::property_images::pending_images` -> the work set (pending mirrors: never-attempted,
//!    failed-and-backoff-expired, or source-URI-changed -- see the module doc there for the
//!    exact predicate).
//! 2. For each image, in order: SSRF-guard the URI (`metadata::uri_allowed`), download it with
//!    the same bounded streaming read as the metadata fetcher (capped at
//!    [`MAX_IMAGE_BYTES`]), decode it, center-crop it to a square and re-encode it as a 720x720
//!    JPEG ([`encode_thumbnail`]), then `PUT` it to object storage under a deterministic key
//!    (see [`object_key`]).
//! 3. On success, `db::property_images::upsert_success` records the public thumbnail URI and
//!    clears the failure state; on any failure, `db::property_images::record_failure` bumps the
//!    per-image attempt count and backs the image off (30 s doubling, 1 h cap) while the loop
//!    moves on.
//!
//! A `PUT` failure or a failed download never leaves a partial object behind: the thumbnail is
//! fully encoded in memory before the single `PUT`, and a non-image or undecodable body fails
//! the mirror (with backoff) rather than uploading garbage.
//!
//! ## What this deliberately does NOT do
//!
//! - **Delete orphaned objects.** When a source URI changes the object key changes with it
//!   (the key embeds the URI's SHA-256), so the old object simply stops being referenced. It
//!   is left in place -- cheap, immutable, and an object-storage lifecycle rule or a manual
//!   `s3://<bucket>/properties/` sweep is the right tool for pruning, not an indexer.
//! - **Run when unconfigured.** Without the `OBJECT_STORAGE_*` variables the supervisor is
//!   never spawned at all (see `main::run`): no external call is ever made, and the API keeps
//!   serving `propertyImageThumbnails: null` until the mirror is enabled.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use futures::StreamExt;
use image::codecs::jpeg::JpegEncoder;
use image::DynamicImage;
use s3::bucket::Bucket;
use s3::creds::Credentials;
use s3::region::Region;
use sha2::Digest;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::config::ObjectStorage;
use crate::db::property_images::{self, PendingImage};
use crate::metadata::{self};
use crate::metrics::{inc_property_image_mirror, set_property_images_pending};

/// How many image mirrors one cycle will attempt. Same reasoning as `metadata::CYCLE_LIMIT`:
/// the work is bounded per cycle (so one slow host cannot hold the loop), and the remaining
/// work is retried on the next cycle.
pub const CYCLE_LIMIT: i64 = 50;

/// How long a single image download may take, end to end (the source is arbitrary third-party
/// storage; a stalled connection must not stall the loop).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// How long establishing the image download connection may take before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap on a single source image, in bytes. The source images are property listings'
/// marketing photography -- multi-megabyte JPEGs are normal -- but the thumbnail is 720x720,
/// so anything past 10 MiB is pathological (or a body that is not an image at all, e.g. an
/// HTML error page that a misconfigured host returns with a 200). Such a body is a failure
/// with backoff, never an upload.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// The thumbnail's output size, in pixels: a 720x720 square. The source is center-cropped to
/// its smallest side before the resize, so the aspect ratio is preserved by the crop and the
/// resize is always square -> square.
const THUMB_SIZE: (u32, u32) = (720, 720);

/// JPEG quality for the thumbnails (1-100). 85 is the standard "visually lossless for
/// display" point: file sizes land in the tens of KB for a 720x720 crop of typical
/// photography, with no visible banding in the gradients.
const JPEG_QUALITY: u8 = 85;

/// `last_error` column width (`migrations/0016_property_images.sql`) -- truncate the
/// stored diagnostic to fit.
const MAX_ERROR_LEN: usize = 500;

/// What one cycle did, for the structured log line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CycleSummary {
    /// Mirrors attempted this cycle (work-set size, capped at [`CYCLE_LIMIT`]).
    pub attempted: usize,
    /// Thumbnails uploaded.
    pub uploaded: usize,
    /// Mirrors that failed (and are now backed off).
    pub failed: usize,
}

/// The deterministic object-storage key for a mirrored thumbnail.
///
/// `properties/<base58(asset_pubkey)>/<image_index>/<sha256-hex(source_uri)>.jpg`
///
/// - **The asset's pubkey (base58)** makes the key human-joinable with the API rows and keeps
///   one asset's images in one prefix (convenient for a lifecycle sweep).
/// - **`image_index`** (zero-based, the position in the `propertyImages` array) distinguishes
///   an asset's images from one another.
/// - **The SHA-256 of the source URI** is the cache-buster: when the metadata's URI for that
///   index changes, the work set re-enters the image and the new body uploads under a new key
///   -- the API row is replaced, and the old object is simply orphaned (see the module doc on
///   what this deliberately does not do). The hash also keeps the key free of URI characters
///   that would need escaping (`?`, `#`, non-ASCII).
pub fn object_key(pubkey: &[u8], image_index: i32, source_uri: &str) -> String {
    let hash: String = sha2::Sha256::digest(source_uri.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "properties/{}/{}/{}.jpg",
        bs58::encode(pubkey).into_string(),
        image_index,
        hash
    )
}

/// Decode `bytes`, center-crop to a square, resize to [`THUMB_SIZE`], and re-encode as a JPEG
/// at quality [`JPEG_QUALITY`].
///
/// The crop takes the center square of the source: for a landscape source the left/right
/// edges are trimmed, for a portrait one the top/bottom. A source smaller than the target is
/// upscaled (Lanczos) -- a small image is better than a broken listing page. Alpha (PNG
/// sources) is flattened onto black before the JPEG encode, since JPEG has no alpha channel;
/// `to_rgb8` keeps the existing RGB channels untouched, so photographic (opaque) sources are
/// not darkened by the flatten.
fn encode_thumbnail(bytes: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory(bytes).with_context(|| "decoding the source image")?;
    // Flatten alpha (if any) first: the center crop and the resize both operate on the
    // flattened RGB, so no premultiplied-edge artifacts survive into the JPEG.
    let mut img = DynamicImage::ImageRgb8(img.to_rgb8());

    let (w, h) = (img.width(), img.height());
    if w != h {
        // Center square: trim the wider side from both edges.
        let side = w.min(h);
        img = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);
    }

    let thumb = img.resize_exact(
        THUMB_SIZE.0,
        THUMB_SIZE.1,
        image::imageops::FilterType::Lanczos3,
    );

    let mut buffer: Vec<u8> = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut buffer, JPEG_QUALITY);
    thumb
        .write_with_encoder(encoder)
        .with_context(|| "encoding the thumbnail JPEG")?;
    Ok(buffer)
}

/// The mirror's state: a pre-configured object-storage bucket and the HTTP client.
///
/// Built once per process (`ImageMirror::new`) -- the `Bucket` client holds the credential
/// and region state, and the `reqwest::Client` pools its connections.
pub struct ImageMirror {
    bucket: Box<Bucket>,
    client: reqwest::Client,
    /// The prefix of every public thumbnail URL, trailing-slash-trimmed
    /// (`ObjectStorage::public_base_url`).
    public_base_url: String,
}

impl ImageMirror {
    pub fn new(os: &ObjectStorage) -> Result<Self> {
        // Hetzner Object Storage serves objects in virtual-hosted style
        // (`https://<bucket>.<endpoint>/<key>`) -- rust-s3's default (`path_style:
        // false`), so no `.with_path_style()` here; the derived public base URL
        // (`{scheme}://{bucket}.{host}`, `config::public_base_from_endpoint`) matches
        // that shape.
        let region = Region::Custom {
            endpoint: os.endpoint.clone(),
            region: os.region.clone(),
        };
        let credentials =
            Credentials::new(Some(&os.access_key), Some(&os.secret_key), None, None, None)
                .context("constructing object-storage credentials")?;
        let bucket = Bucket::new(&os.bucket, region, credentials)
            .with_context(|| "constructing the object-storage bucket client")?;

        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .context("building the image-download HTTP client")?;

        Ok(Self {
            bucket,
            client,
            public_base_url: os.public_base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Mirror one image: download, thumbnail, upload, record.
    ///
    /// Never returns an error for a *download/decode/upload* failure -- those are recorded
    /// (`record_failure` + backoff) and reported as `false` so the cycle moves on to the next
    /// image. Errors propagate only for the *database* writes (same contract as
    /// `metadata::cycle`), which would mean the mirror cannot record its state at all.
    async fn mirror_image(
        &self,
        pool: &PgPool,
        img: &PendingImage,
        shutdown: &CancellationToken,
    ) -> Result<bool> {
        // The URI came from on-chain metadata we cannot control: apply the same SSRF guard
        // the metadata fetcher applies (metadata.rs: only http/https, block private/loopback
        // hosts) before any external call.
        metadata::uri_allowed(&img.source_uri).with_context(|| {
            format!(
                "image index {} of {}",
                img.image_index,
                bs58::encode(&img.asset_pubkey).into_string()
            )
        })?;

        if shutdown.is_cancelled() {
            return Ok(false);
        }

        // Bounded streaming read, mirroring `metadata::fetch_document`'s chunk loop, but
        // capped at the image-specific limit.
        let response = self
            .client
            .get(&img.source_uri)
            .send()
            .await
            .with_context(|| "downloading the source image")?;
        if !response.status().is_success() {
            anyhow::bail!(
                "source image host returned status {} for image index {}",
                response.status(),
                img.image_index
            );
        }
        let mut bytes: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| "reading the source image body")?;
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_IMAGE_BYTES {
                anyhow::bail!(
                    "source image exceeds {MAX_IMAGE_BYTES} bytes (image index {}); refusing to buffer it",
                    img.image_index
                );
            }
        }

        let thumb = encode_thumbnail(&bytes).with_context(|| {
            format!("building the thumbnail for image index {}", img.image_index)
        })?;

        let key = object_key(&img.asset_pubkey, img.image_index, &img.source_uri);
        self.bucket
            .put_object_with_content_type(&key, &thumb, "image/jpeg")
            .await
            .with_context(|| "uploading the thumbnail to object storage")?;

        let thumb_uri = format!("{}/{}", self.public_base_url, key);
        property_images::upsert_success(
            pool,
            &img.asset_pubkey,
            img.image_index,
            &img.source_uri,
            &thumb_uri,
            Utc::now(),
        )
        .await
        .with_context(|| "recording the mirror success")?;
        inc_property_image_mirror("success");
        log::info!(
            "image mirrored: asset={} index={} thumb={}",
            bs58::encode(&img.asset_pubkey).into_string(),
            img.image_index,
            thumb_uri
        );
        Ok(true)
    }

    /// One cycle: drain the work set (bounded at [`CYCLE_LIMIT`]), mirroring each image
    /// sequentially. Returns what the cycle did.
    pub async fn cycle(&self, pool: &PgPool, shutdown: &CancellationToken) -> Result<CycleSummary> {
        let pending = property_images::pending_images(pool, CYCLE_LIMIT)
            .await
            .context("querying the pending image mirrors")?;

        let mut summary = CycleSummary::default();
        for img in &pending {
            if shutdown.is_cancelled() {
                break;
            }
            match self.mirror_image(pool, img, shutdown).await {
                Ok(true) => summary.uploaded += 1,
                // `Ok(false)` = shutdown skipped it; no accounting.
                Ok(false) => {}
                Err(e) => {
                    // A mirror failure is expected (that is what the backoff exists for) --
                    // record it and continue. The DB write itself failing, however,
                    // propagates: the mirror cannot safely record its state.
                    let err: String = format!("{e:#}").chars().take(MAX_ERROR_LEN).collect();
                    property_images::record_failure(
                        pool,
                        &img.asset_pubkey,
                        img.image_index,
                        &img.source_uri,
                        &err,
                    )
                    .await
                    .with_context(|| "recording the mirror failure")?;
                    inc_property_image_mirror("failure");
                    log::warn!(
                        "image mirror failed: asset={} index={} err={err}",
                        bs58::encode(&img.asset_pubkey).into_string(),
                        img.image_index
                    );
                    summary.failed += 1;
                }
            }
        }
        summary.attempted = pending.len();

        // Publish how much work remains for the dashboard / alerting.
        set_property_images_pending(
            property_images::count_pending(pool)
                .await
                .context("counting the pending image mirrors")?,
        );
        Ok(summary)
    }
}

/// The supervisor: run a cycle, wait [`interval`], repeat, until `shutdown` fires.
///
/// Mirrors `metadata::supervise` -- a cycle failure (a work-set query error) is logged and the
/// loop continues; only a DB-write failure propagating out of `cycle` would break the loop,
/// and that is a process-level incident worth a restart.
pub async fn supervise(
    mirror: &ImageMirror,
    pool: &PgPool,
    interval: Duration,
    shutdown: CancellationToken,
) {
    log::info!("image mirror supervisor started (interval: {interval:?})");
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        if let Err(e) = mirror.cycle(pool, &shutdown).await {
            log::error!("image mirror cycle failed: {e:#}");
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => break,
        }
    }
    log::info!("image mirror supervisor stopping");
}

#[cfg(test)]
mod tests {
    use super::{encode_thumbnail, object_key};

    fn fake_pubkey() -> Vec<u8> {
        // A fixed 32-byte key so the base58 is deterministic.
        vec![
            0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45,
            0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01,
            0x23, 0x45, 0x67, 0x89,
        ]
    }

    // --- object_key ------------------------------------------------------------------------

    #[test]
    fn object_key_is_deterministic_and_well_formed() {
        let pk = fake_pubkey();
        let a = object_key(&pk, 0, "https://example.com/a.jpg");
        let b = object_key(&pk, 0, "https://example.com/a.jpg");
        assert_eq!(a, b, "same inputs -> same key");

        // Shape: properties/<base58>/<index>/<64 hex chars>.jpg
        let parts: Vec<&str> = a.split('/').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "properties");
        assert!(
            !parts[1].is_empty() && !parts[1].contains('='),
            "base58 pubkey, no padding"
        );
        assert_eq!(parts[2], "0");
        assert!(parts[3].ends_with(".jpg"));
        let hash = parts[3].strip_suffix(".jpg").unwrap();
        assert_eq!(hash.len(), 64, "sha256 hex is 64 chars");
        assert!(hash.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn object_key_changes_with_the_source_uri() {
        let pk = fake_pubkey();
        let a = object_key(&pk, 0, "https://example.com/a.jpg");
        let b = object_key(&pk, 0, "https://example.com/a-v2.jpg");
        assert_ne!(a, b, "a URI change must move the object (the cache-bust)");
    }

    #[test]
    fn object_key_distinguishes_indices() {
        let pk = fake_pubkey();
        assert_ne!(
            object_key(&pk, 0, "https://e.com/x"),
            object_key(&pk, 1, "https://e.com/x")
        );
    }

    #[test]
    fn object_key_never_contains_uri_metacharacters() {
        let pk = fake_pubkey();
        let key = object_key(&pk, 2, "https://e.com/img?q=1#frag/日本語");
        // The hash component is pure hex; the pubkey is base58; the index is a decimal.
        assert!(!key.contains('?') && !key.contains('#') && !key.contains('日'));
    }

    // --- encode_thumbnail ------------------------------------------------------------------

    /// Encode an RGBA image to a real JPEG in memory, so `encode_thumbnail` exercises the
    /// decode path exactly as it will with a downloaded body.
    fn jpeg_bytes(w: u32, h: u32, rgb: (u8, u8, u8)) -> Vec<u8> {
        use image::{ImageFormat, RgbaImage};
        let buf = RgbaImage::from_pixel(w, h, image::Rgba([rgb.0, rgb.1, rgb.2, 255]));
        let mut out: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(buf)
            .write_to(std::io::Cursor::new(&mut out), ImageFormat::Jpeg)
            .expect("encoding the test JPEG");
        out
    }

    #[test]
    fn thumbnail_is_720x720_jpeg_from_a_landscape_source() {
        let body = jpeg_bytes(1500, 1000, (120, 30, 90));
        let thumb = encode_thumbnail(&body).expect("thumbnail encode");
        let img = image::load_from_memory(&thumb).expect("decoding the produced JPEG");
        assert_eq!(
            img.as_rgb8()
                .expect("thumbnail should decode as RGB")
                .dimensions(),
            (720, 720)
        );
        // Must be a real JPEG container.
        assert_eq!(
            image::guess_format(&thumb).expect("format guess"),
            image::ImageFormat::Jpeg
        );
    }

    #[test]
    fn thumbnail_is_720x720_from_a_portrait_source() {
        let body = jpeg_bytes(800, 1600, (10, 80, 200));
        let thumb = encode_thumbnail(&body).expect("thumbnail encode");
        let img = image::load_from_memory(&thumb).expect("decoding");
        assert_eq!(
            img.as_rgb8()
                .expect("thumbnail should decode as RGB")
                .dimensions(),
            (720, 720)
        );
    }

    #[test]
    fn thumbnail_upscales_tiny_sources() {
        let body = jpeg_bytes(40, 120, (200, 200, 10));
        let thumb = encode_thumbnail(&body).expect("thumbnail encode");
        let img = image::load_from_memory(&thumb).expect("decoding");
        assert_eq!(
            img.as_rgb8()
                .expect("thumbnail should decode as RGB")
                .dimensions(),
            (720, 720)
        );
    }

    #[test]
    fn thumbnail_rejects_non_image_bodies() {
        let body = b"<!doctype html><html>error</html>";
        assert!(encode_thumbnail(body).is_err());
    }

    #[test]
    fn thumbnail_preserves_a_square_source_without_cropping() {
        let body = jpeg_bytes(1000, 1000, (5, 5, 5));
        let thumb = encode_thumbnail(&body).expect("thumbnail encode");
        let img = image::load_from_memory(&thumb).expect("decoding");
        assert_eq!(
            img.as_rgb8()
                .expect("thumbnail should decode as RGB")
                .dimensions(),
            (720, 720)
        );
    }
}
