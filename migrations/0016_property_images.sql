-- Mirrored property images: one row per `propertyImages` entry, owned by the image-mirror
-- background loop (ADR-31).
--
-- The off-chain property metadata (0013) carries `property_images`, a JSONB array of image
-- URIs hosted by the protocol team. Those originals are large (full-resolution photos,
-- often several MiB), which is bad for any client that renders a card or a list. This table
-- is where the indexer's background image mirror (`crates/indexer/src/images.rs`) stores the
-- result of downloading each source image, re-encoding it as a center-cropped 720x720
-- JPEG, and uploading it to the operator's public object storage (Hetzner Object Storage
-- in production; the endpoint is configurable, ADR-31). `thumb_uri` is the public URL the
-- GraphQL API serves (`PropertyMetadata.propertyImageThumbnails`).
--
-- SHAPE -- a derived table, NOT an account-state table (same exclusion as
-- `marketplace_property_metadata`, ADR-27, and `webhook_events`, ADR-28): no `slot` guard,
-- no soft close, no `db::close::StateTable` entry, no `ProgramSpec.tables` roster (the
-- roster test enforces the partition stays clean). The row's lifetime is owned by the
-- mirror loop's upserts: a row exists while the source URI is present in the metadata's
-- `propertyImages` at `image_index`, and the loop's work-set query simply never touches it
-- again once a success is recorded. If the metadata's array shrinks, the leftover rows are
-- harmless orphans (the API groups thumbnails by `thumb_uri IS NOT NULL` and the asset's
-- current array; a stale row only costs a few bytes). A devnet reset / volume drop wipes
-- the table with everything else; the live mirror (or `indexer mirror-images`) re-uploads.
--
-- PRIMARY KEY (`asset_pubkey`, `image_index`): the asset PDA's `pubkey` (BYTEA, the pubkey
-- convention) plus the zero-based position of the URI in `propertyImages`. `image_index` is
-- the POSITION, not a document id, because the metadata document carries no per-image ids
-- (0013's JSONB convention). A changed URI at the same (pubkey, index) is an UPDATE via the
-- upsert, so the row's key stays stable; the object-storage key
-- (`properties/<base58 pubkey>/<index>/<sha256(source_uri)>.jpg`, built in
-- `images::object_key`) embeds the source URI's SHA-256, so a changed URI uploads a NEW
-- object under a new key (and a new `thumb_uri`, which cache-busts clients on the URL
-- change). The old object lingers in the bucket as a harmless orphan (a few hundred KB,
-- never referenced again) rather than being overwritten.
--
-- COLUMN SHAPES:
--   * `source_uri`  = the URI the LAST ATTEMPT targeted (success or failure).
--   * `thumb_uri` / `uploaded_at` = the public URL of the LAST SUCCESSFUL upload and when
--     it landed (NULL until one exists); the loop re-uploads on URI change even if an old
--     success is recorded, so a changed `source_uri` transiently shows the old thumbnail
--     until the new upload succeeds (same last-successful-snapshot semantics as 0013's
--     `fetched_at`).
--
-- MIRROR STATE (the loop's retry machinery, read by the work-set query in
-- `db::property_images`):
--   * `attempts`        = consecutive failures for `source_uri` (reset to 0 by a success);
--   * `next_attempt_at` = backoff deadline (30 s doubling per failure, 1 h cap, computed in
--                         the failure upsert); NULL after a success;
--   * `last_error`      = the last failure's message (truncated to 500 chars); NULL after a
--                         success.
--
-- One row per image keeps failures isolated: a 404 on image 3 never blocks image 4, and a
-- transient network blip retries only the images that failed, on the shared backoff.

CREATE TABLE marketplace_property_image (
    asset_pubkey    BYTEA NOT NULL,
    image_index     INT NOT NULL,
    source_uri      TEXT NOT NULL,
    -- Last successful upload (NULL until one exists).
    thumb_uri       TEXT,
    uploaded_at     TIMESTAMPTZ,
    -- Mirror state (the loop's retry machinery; see the header).
    attempts        INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ,
    last_error      TEXT,
    PRIMARY KEY (asset_pubkey, image_index)
);

-- The API reads thumbnails for a page of assets in one query
-- (`WHERE asset_pubkey = ANY($1) AND thumb_uri IS NOT NULL ORDER BY image_index`); the
-- partial index keeps that scan cheap as the pending / never-uploaded backlog grows.
CREATE INDEX idx_mkt_property_image_thumb
    ON marketplace_property_image (asset_pubkey, image_index)
    WHERE thumb_uri IS NOT NULL;
