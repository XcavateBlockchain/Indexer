//! Per-program GraphQL surfaces for the sibling programs (marketplace, property, regions,
//! realxhub), one submodule each: entity types mirroring the account-state tables of
//! migrations 0008..0015, plus the resolver bodies `QueryRoot` delegates to (juniper
//! permits only one `#[graphql_object]` impl per type, so the fields live on `QueryRoot`
//! and the work lives here).
//!
//! Shared conventions (matching the whitelist surface in `super::types`):
//!
//! * `id` = the account's pubkey, base58.
//! * Every entity carries `slot`/`lamports` (as the string-serialized `I64` scalar),
//!   `active` (= `closed_at_slot IS NULL`) and `closedAtSlot`.
//! * Pubkey columns -> base58 `String`; BIGINT -> `I64`; INT/SMALLINT -> `Int`; TEXT enums ->
//!   GraphQL enums (`super::enums`); postcode-style BYTEA byte strings -> UTF-8 `String`;
//!   hashes -> lowercase hex `String`; JSONB lists -> the raw JSON as a `String` (juniper has
//!   no built-in JSON scalar, and the shapes are documented on the migrations).
//! * Connections are `{ nodes, totalCount }` with `first`/`offset` clamped by
//!   [`crate::guards`], ordered `slot DESC, pubkey ASC` (newest activity first, stable
//!   tiebreak).

use juniper::{FieldError, FieldResult, Value};

pub mod marketplace;
pub mod property;
pub mod realxhub;
pub mod regions;

pub(crate) fn b58(bytes: &[u8]) -> String {
    bs58::encode(bytes).into_string()
}

/// Parse a base58 filter argument into the 32 raw bytes the BYTEA columns store. A malformed
/// or wrong-length key is a loud input error, not an empty result set.
pub(crate) fn parse_b58(field: &'static str, s: &str) -> FieldResult<Vec<u8>> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|e| FieldError::new(format!("{field}: invalid base58: {e}"), Value::null()))?;
    if bytes.len() != 32 {
        return Err(FieldError::new(
            format!(
                "{field}: expected a 32-byte pubkey, got {} bytes",
                bytes.len()
            ),
            Value::null(),
        ));
    }
    Ok(bytes)
}

/// `count(*)` comes back as `i64`; connection `totalCount` is `Int!` (`i32`) to match the
/// whitelist surface. Saturating: documented, harmless fallback rather than a panic.
pub(crate) fn total_count_i32(count: i64) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX)
}

/// Postcode-style byte strings (on-chain validated ASCII) -> `String`.
pub(crate) fn utf8_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Hashes -> lowercase hex.
pub(crate) fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// JSONB columns -> their JSON text, verbatim.
pub(crate) fn json_string(value: &serde_json::Value) -> String {
    value.to_string()
}
