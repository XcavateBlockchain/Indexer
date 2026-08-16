//! The mandatory DoS guards (spec, task-5-brief.md):
//!
//! 1. Page-size clamps ([`clamp_first`] / [`clamp_offset`]) -- pure, called from every
//!    connection resolver in `graphql::query`.
//! 2. Pre-parse hardening ([`prevalidate`]): a byte cap and a string-literal-aware `{`/`}`
//!    nesting scan, run on the RAW query text before `graphql_parser::parse_query` ever sees it.
//! 3. A query depth/complexity measurement ([`check_query`]) run BEFORE juniper executes
//!    anything. Juniper 0.16 has no built-in cost limiter, so the incoming query text is parsed
//!    a second time with `graphql-parser` (the same crate juniper's own optional
//!    `schema-language` feature depends on) purely to measure it.
//!
//! ## Two-tier limits (fix round 1 -- see task-5-report.md "Fix round 1")
//!
//! The first version of this guard special-cased GraphiQL's real `IntrospectionQuery` with a
//! name+shape allowlist that made [`check_query`] return `Ok` WITHOUT ever calling `measure()`.
//! That is a complete bypass: any query merely named `IntrospectionQuery` whose top-level
//! selections were `__schema`/`__type` fields skipped measurement entirely, so a 20-level
//! `ofType` chain or 2000 aliased `__schema` selections sailed straight through uninspected
//! (reviewer-reproduced).
//!
//! `measure()` now runs on EVERY query, with no exceptions. What varies is which cap it is
//! checked against:
//!
//! - **Meta-only** (every top-level field of every operation is `__schema`, `__type`, or
//!   `__typename` -- the operation NAME is irrelevant, unlike the old allowlist): the relaxed
//!   caps [`META_MAX_DEPTH`] / [`META_MAX_COMPLEXITY`]. These are still HARD caps, just wide
//!   enough to admit the real `getIntrospectionQuery()` document (measured depth 13, well under
//!   15) while still bounding alias fan-out and `ofType` chain length -- a 20-level `ofType`
//!   chain or 2000 aliased `__schema` selections both still exceed them and are rejected.
//! - **Everything else** (including a query that mixes `__schema` with a real data field at the
//!   top level): the strict spec caps [`MAX_DEPTH`] (<=8) / [`MAX_COMPLEXITY`] (<=500), exactly
//!   as before.
//!
//! A hostile client cannot smuggle an arbitrary deep data query past the relaxed tier: the
//! top-level-field check is purely structural (no name check at all now), and mixing in even one
//! non-meta top-level field forces the strict tier for the WHOLE document.

use std::collections::HashMap;

use graphql_parser::query::{
    Definition, Document, FragmentDefinition, OperationDefinition, Selection, SelectionSet,
};

/// `first` hard cap (spec: "clamped server-side to <=100, silently clamp, do not error").
pub const MAX_FIRST: i32 = 100;
/// `first` default when the client omits it.
pub const DEFAULT_FIRST: i32 = 20;
/// `offset` hard cap (spec).
pub const MAX_OFFSET: i32 = 10_000;

/// Strict query depth hard cap (spec: "<=8"). Applies to every query that is not classified
/// meta-only.
pub const MAX_DEPTH: usize = 8;
/// Strict query complexity hard cap (spec: "<=500 selection fields"). Applies to every query
/// that is not classified meta-only.
pub const MAX_COMPLEXITY: usize = 500;

/// Relaxed depth cap for meta-only queries (fix round 1). The real `getIntrospectionQuery()`
/// document measures depth 13 under this module's counting rules (root `__schema` field at
/// depth 1, down through `types -> ...FullType -> fields -> args -> ...InputValue -> type ->
/// ...TypeRef -> ofType x7`); 15 leaves two levels of headroom without being anywhere near the
/// pre-parse brace-nesting cap ([`MAX_BRACE_DEPTH`]) or a stack-depth concern. Verified directly
/// against the real query text in `tests::the_real_introspection_query_passes_the_meta_caps`.
pub const META_MAX_DEPTH: usize = 15;
/// Relaxed complexity cap for meta-only queries (fix round 1). The real `getIntrospectionQuery()`
/// document's measured field count is well under this (verified in the same test as
/// [`META_MAX_DEPTH`]); still bounds alias fan-out -- e.g. 2000 aliased `__schema { __typename }`
/// selections (4000 measured fields) exceed it and are rejected.
pub const META_MAX_COMPLEXITY: usize = 3000;

/// Hard byte cap on the raw query text, enforced in [`prevalidate`] BEFORE
/// `graphql_parser::parse_query` (or even the brace-depth scan) ever runs. The real
/// `getIntrospectionQuery()` text is well under 2 KB; 20 KB is generous headroom for any
/// legitimate query while bounding how much text every later step -- the brace scan, the parser,
/// the measurement walk -- ever has to look at.
pub const MAX_QUERY_BYTES: usize = 20_000;

/// Hard cap on raw `{`/`}` nesting depth, scanned in [`prevalidate`] BEFORE parsing (fix round
/// 1, Important finding). `graphql_parser` is a hand-rolled recursive-descent parser (built on
/// `combine`) that recurses once per nesting level with no depth limit of its own: a deeply
/// nested but otherwise syntactically ordinary `{ f { f { f { ... } } } }` STACK-OVERFLOWS AND
/// ABORTS THE WHOLE PROCESS in a debug build once nesting reaches roughly the high 30s/low 40s
/// (release builds happen to survive only because `combine`'s own internal recursion cap kicks
/// in first, around 48-50 levels) -- i.e. without this scan, a single unauthenticated `/graphql`
/// request is a process-level denial of service, not merely a rejected query. The observed
/// debug-build overflow threshold was 36 on one host, but that number varies by platform and
/// stack size, so 25 is used instead of trimming the margin to the observed value: it keeps a
/// full 10 levels of headroom over the relaxed meta-only cap ([`META_MAX_DEPTH`] = 15) while
/// staying comfortably below every observed abort threshold on any host.
pub const MAX_BRACE_DEPTH: usize = 25;

/// Clamp a client-supplied `first` to `[0, MAX_FIRST]`, defaulting to [`DEFAULT_FIRST`] when
/// omitted. Never errors -- an abusive `first: 100000` silently becomes `100`.
pub fn clamp_first(first: Option<i32>) -> i64 {
    first.unwrap_or(DEFAULT_FIRST).clamp(0, MAX_FIRST) as i64
}

/// Clamp a client-supplied `offset` to `[0, MAX_OFFSET]`, defaulting to `0`.
pub fn clamp_offset(offset: Option<i32>) -> i64 {
    offset.unwrap_or(0).clamp(0, MAX_OFFSET) as i64
}

/// Why a query was rejected before execution. Matches the `graphql_rejected_total{reason=...}`
/// label values the brief mandates -- the pre-parse byte cap is bucketed under `Parse` (the
/// input is unacceptable before parsing is even attempted) and the pre-parse brace-depth scan
/// under `Depth` (it is a cheaper textual proxy for the same violation `measure()` would
/// otherwise report).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    Parse,
    Depth,
    Complexity,
}

impl RejectReason {
    pub fn as_label(self) -> &'static str {
        match self {
            RejectReason::Parse => "parse",
            RejectReason::Depth => "depth",
            RejectReason::Complexity => "complexity",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rejection {
    pub reason: RejectReason,
    pub message: String,
}

/// Pre-parses `query_text` and rejects it if it is too large, too deeply brace-nested, unparseable,
/// too deep, or too complex. `Ok(())` means "safe to hand to juniper for real execution" -- this
/// function never executes anything itself. `measure()` is ALWAYS called (fix round 1 -- see the
/// module header): there is no code path that returns `Ok` without it.
pub fn check_query(query_text: &str) -> Result<(), Rejection> {
    prevalidate(query_text)?;

    let doc = graphql_parser::parse_query::<&str>(query_text).map_err(|e| Rejection {
        reason: RejectReason::Parse,
        message: format!("query does not parse: {e}"),
    })?;

    let meta_only = is_meta_only(&doc);
    let (depth_cap, complexity_cap) = if meta_only {
        (META_MAX_DEPTH, META_MAX_COMPLEXITY)
    } else {
        (MAX_DEPTH, MAX_COMPLEXITY)
    };

    let (depth, complexity) = measure(&doc);
    if depth > depth_cap {
        return Err(Rejection {
            reason: RejectReason::Depth,
            message: format!(
                "query depth {depth} exceeds the maximum of {depth_cap}{}",
                if meta_only {
                    " (relaxed meta-only cap)"
                } else {
                    ""
                }
            ),
        });
    }
    if complexity > complexity_cap {
        return Err(Rejection {
            reason: RejectReason::Complexity,
            message: format!(
                "query selects {complexity} field(s), exceeding the maximum of {complexity_cap}{}",
                if meta_only {
                    " (relaxed meta-only cap)"
                } else {
                    ""
                }
            ),
        });
    }
    Ok(())
}

/// Byte cap + string-literal-aware brace-nesting scan (fix round 1, Important finding). Runs
/// BEFORE `graphql_parser::parse_query` -- see [`MAX_BRACE_DEPTH`] for why this has to happen
/// before parsing rather than being left to `measure()`.
fn prevalidate(query_text: &str) -> Result<(), Rejection> {
    if query_text.len() > MAX_QUERY_BYTES {
        return Err(Rejection {
            reason: RejectReason::Parse,
            message: format!(
                "query text is {} bytes, exceeding the maximum of {MAX_QUERY_BYTES}",
                query_text.len()
            ),
        });
    }

    let depth = max_brace_depth(query_text);
    if depth > MAX_BRACE_DEPTH {
        return Err(Rejection {
            reason: RejectReason::Depth,
            message: format!(
                "query text nests {depth} levels of `{{`, exceeding the maximum of {MAX_BRACE_DEPTH}"
            ),
        });
    }
    Ok(())
}

/// Scans for the maximum `{`/`}` nesting depth in raw GraphQL query text, treating everything
/// inside a string literal (`"..."`) or block string (`"""..."""`) as opaque -- braces inside a
/// string argument (e.g. `note: "{{{{"`) must never be counted, or a perfectly ordinary query
/// with a brace-heavy string argument would be spuriously rejected.
///
/// Byte-oriented, not `char`-oriented: the delimiters this scan cares about (`"`, `{`, `}`, `\`)
/// are all single-byte ASCII, and every UTF-8 continuation byte is `>= 0x80`, so slicing on bytes
/// can never split a multi-byte character or misread one as a delimiter.
fn max_brace_depth(text: &str) -> usize {
    #[derive(PartialEq)]
    enum Mode {
        Normal,
        StringLit,
        BlockString,
    }

    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut mode = Mode::Normal;
    let mut depth = 0usize;
    let mut max_depth = 0usize;

    while i < bytes.len() {
        match mode {
            Mode::Normal => {
                if bytes[i..].starts_with(b"\"\"\"") {
                    mode = Mode::BlockString;
                    i += 3;
                } else if bytes[i] == b'"' {
                    mode = Mode::StringLit;
                    i += 1;
                } else if bytes[i] == b'{' {
                    depth += 1;
                    max_depth = max_depth.max(depth);
                    i += 1;
                } else if bytes[i] == b'}' {
                    depth = depth.saturating_sub(1);
                    i += 1;
                } else {
                    i += 1;
                }
            }
            Mode::StringLit => {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2; // skip the escaped character too, whatever it is
                } else if bytes[i] == b'"' {
                    mode = Mode::Normal;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            Mode::BlockString => {
                if bytes[i..].starts_with(b"\\\"\"\"") {
                    i += 4; // GraphQL's escaped-triple-quote-inside-a-block-string sequence
                } else if bytes[i..].starts_with(b"\"\"\"") {
                    mode = Mode::Normal;
                    i += 3;
                } else {
                    i += 1;
                }
            }
        }
    }

    max_depth
}

/// A query is meta-only iff EVERY top-level selection of EVERY operation in the document is a
/// `Field` named `__schema`, `__type`, or `__typename` -- the operation's NAME plays no part in
/// this (fix round 1: the old allowlist's name check is gone entirely). Mixing in even one
/// non-meta top-level field, or a top-level inline fragment / fragment spread (conservatively --
/// its eventual contents are not inspected here), makes the WHOLE document non-meta, forcing the
/// strict caps.
fn is_meta_only<'a>(doc: &Document<'a, &'a str>) -> bool {
    const META_FIELDS: [&str; 3] = ["__schema", "__type", "__typename"];

    let mut saw_operation = false;
    for def in &doc.definitions {
        let selection_set = match def {
            Definition::Operation(OperationDefinition::SelectionSet(s)) => s,
            Definition::Operation(OperationDefinition::Query(q)) => &q.selection_set,
            Definition::Operation(OperationDefinition::Mutation(m)) => &m.selection_set,
            Definition::Operation(OperationDefinition::Subscription(s)) => &s.selection_set,
            Definition::Fragment(_) => continue,
        };
        saw_operation = true;

        if selection_set.items.is_empty() {
            return false;
        }
        for item in &selection_set.items {
            match item {
                Selection::Field(f) if META_FIELDS.contains(&f.name) => {}
                _ => return false,
            }
        }
    }
    saw_operation
}

/// Returns `(max_depth, total_field_count)` across every operation in the document. Depth counts
/// only `Field` boundaries (inline fragments and fragment spreads are transparent, matching the
/// common `graphql-depth-limit` convention); complexity counts every `Field` selection reached
/// through the operation, with fragment spreads resolved (and cycle-guarded, since a
/// self-referential fragment is a valid-looking DoS payload even though it is not a valid
/// document). ALWAYS called by [`check_query`] -- see the module header, fix round 1.
fn measure<'a>(doc: &Document<'a, &'a str>) -> (usize, usize) {
    let mut fragments: HashMap<&str, &FragmentDefinition<'_, &str>> = HashMap::new();
    for def in &doc.definitions {
        if let Definition::Fragment(f) = def {
            fragments.insert(f.name, f);
        }
    }

    let mut max_depth = 0usize;
    let mut total_complexity = 0usize;
    for def in &doc.definitions {
        let selection_set = match def {
            Definition::Operation(OperationDefinition::SelectionSet(s)) => s,
            Definition::Operation(OperationDefinition::Query(q)) => &q.selection_set,
            Definition::Operation(OperationDefinition::Mutation(m)) => &m.selection_set,
            Definition::Operation(OperationDefinition::Subscription(s)) => &s.selection_set,
            Definition::Fragment(_) => continue, // only measured when reached through a spread
        };
        let mut visiting = Vec::new();
        let (depth, complexity) = walk(selection_set, &fragments, 1, &mut visiting);
        max_depth = max_depth.max(depth);
        total_complexity += complexity;
    }
    (max_depth, total_complexity)
}

/// Bound on the raw work this walk will do before giving up and reporting a guaranteed
/// violation -- an adversarial document (e.g. many small mutually-referencing fragments) could
/// otherwise blow up the walk itself well past the limits it exists to enforce.
const WALK_BAILOUT_COMPLEXITY: usize = META_MAX_COMPLEXITY * 4;

/// Walks `set`, whose direct children (per the GraphQL grammar, always at least one) are
/// considered to live at `depth`. Returns `(max_depth reached, field count)`.
///
/// A `Field` with no sub-selection (a leaf) contributes its own `depth` and does NOT recurse --
/// recursing into an empty selection set would count a phantom extra level that no field
/// actually occupies. Inline fragments and fragment spreads are transparent: their contents are
/// measured at the SAME `depth` as their surrounding selection set, matching the common
/// `graphql-depth-limit` convention that only `Field` boundaries count.
fn walk<'a>(
    set: &SelectionSet<'a, &'a str>,
    fragments: &HashMap<&'a str, &FragmentDefinition<'a, &'a str>>,
    depth: usize,
    visiting: &mut Vec<&'a str>,
) -> (usize, usize) {
    let mut max_depth = 0usize;
    let mut complexity = 0usize;

    for item in &set.items {
        if complexity > WALK_BAILOUT_COMPLEXITY {
            break;
        }
        match item {
            Selection::Field(field) => {
                complexity += 1;
                if field.selection_set.items.is_empty() {
                    max_depth = max_depth.max(depth);
                } else {
                    let (d, c) = walk(&field.selection_set, fragments, depth + 1, visiting);
                    max_depth = max_depth.max(d);
                    complexity += c;
                }
            }
            Selection::InlineFragment(inline) => {
                if !inline.selection_set.items.is_empty() {
                    let (d, c) = walk(&inline.selection_set, fragments, depth, visiting);
                    max_depth = max_depth.max(d);
                    complexity += c;
                }
            }
            Selection::FragmentSpread(spread) => {
                if visiting.contains(&spread.fragment_name) {
                    // Cyclic fragment reference: not a valid document, but a hostile client can
                    // still send one. Report it as a guaranteed violation instead of recursing
                    // forever.
                    max_depth = max_depth.max(depth + META_MAX_DEPTH + 1);
                    complexity += META_MAX_COMPLEXITY + 1;
                    continue;
                }
                if let Some(frag) = fragments.get(spread.fragment_name) {
                    visiting.push(spread.fragment_name);
                    let (d, c) = walk(&frag.selection_set, fragments, depth, visiting);
                    visiting.pop();
                    max_depth = max_depth.max(d);
                    complexity += c;
                }
                // A spread naming an undefined fragment is invalid and juniper's own execution
                // will reject it; nothing to add to the guard's count.
            }
        }
    }

    (max_depth, complexity)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- clamps --------------------------------------------------------------------------

    #[test]
    fn first_defaults_to_20() {
        assert_eq!(clamp_first(None), 20);
    }

    #[test]
    fn first_clamps_to_100() {
        assert_eq!(clamp_first(Some(100_000)), 100);
    }

    #[test]
    fn first_negative_clamps_to_0_not_the_default() {
        assert_eq!(clamp_first(Some(-5)), 0);
    }

    #[test]
    fn first_within_range_passes_through() {
        assert_eq!(clamp_first(Some(37)), 37);
    }

    #[test]
    fn offset_defaults_to_0() {
        assert_eq!(clamp_offset(None), 0);
    }

    #[test]
    fn offset_clamps_to_10_000() {
        assert_eq!(clamp_offset(Some(50_000)), 10_000);
    }

    // --- depth (strict tier) ----------------------------------------------------------------

    #[test]
    fn a_shallow_query_passes() {
        let q = "{ config { authority updatedAt } }";
        assert!(check_query(q).is_ok());
    }

    #[test]
    fn depth_ten_is_rejected() {
        // 10 levels of nesting via a single field name repeated (this is only ever pre-parsed,
        // never executed against a real schema, so an unresolvable field name is fine). Not
        // meta-only (top-level field is "f"), so the strict cap (8) applies.
        let mut q = String::from("{ ");
        for _ in 0..10 {
            q.push_str("f { ");
        }
        q.push('x');
        for _ in 0..10 {
            q.push_str(" }");
        }
        q.push_str(" }");
        let err = check_query(&q).expect_err("depth 11 must be rejected");
        assert_eq!(err.reason, RejectReason::Depth);
    }

    #[test]
    fn depth_exactly_at_the_limit_passes() {
        // 8 levels of field nesting = depth 8, at the limit, not over it.
        let mut q = String::from("{ ");
        for _ in 0..7 {
            q.push_str("f { ");
        }
        q.push('x');
        for _ in 0..7 {
            q.push_str(" }");
        }
        q.push_str(" }");
        assert!(check_query(&q).is_ok());
    }

    // --- complexity (strict tier) ------------------------------------------------------------

    #[test]
    fn a_thousand_field_query_is_rejected() {
        let mut q = String::from("{ ");
        for i in 0..1000 {
            q.push_str(&format!("a{i}: config {{ id }} "));
        }
        q.push('}');
        let err = check_query(&q).expect_err("1000+ fields must be rejected");
        assert_eq!(err.reason, RejectReason::Complexity);
    }

    #[test]
    fn complexity_at_the_limit_passes() {
        // 500 flat top-level scalar-looking fields = complexity 500, at the limit.
        let mut q = String::from("{ ");
        for i in 0..500 {
            q.push_str(&format!("a{i} "));
        }
        q.push('}');
        assert!(check_query(&q).is_ok());
    }

    #[test]
    fn fragments_are_expanded_into_the_complexity_count() {
        let q = "{ a { ...F } } fragment F on X { b c d }";
        let doc = graphql_parser::parse_query::<&str>(q).unwrap();
        let (_, complexity) = measure(&doc);
        // `a` + the 3 fields the fragment spread contributes.
        assert_eq!(complexity, 4);
    }

    #[test]
    fn a_cyclic_fragment_is_a_guaranteed_violation_not_an_infinite_loop() {
        let q = "{ ...F } fragment F on X { ...F }";
        let err = check_query(q).expect_err("a cyclic fragment must be rejected, not hang");
        assert!(matches!(
            err.reason,
            RejectReason::Depth | RejectReason::Complexity
        ));
    }

    // --- parse errors --------------------------------------------------------------------------

    #[test]
    fn unparseable_text_is_rejected_with_reason_parse() {
        let err = check_query("{ this is not : valid graphql (((").unwrap_err();
        assert_eq!(err.reason, RejectReason::Parse);
    }

    // --- fix round 1: CRITICAL -- measure() must never be skipped ----------------------------

    /// The real `getIntrospectionQuery()` text (graphql-js / GraphiQL's exact query), verbatim.
    /// Ground truth for both [`META_MAX_DEPTH`] and [`META_MAX_COMPLEXITY`].
    const REAL_INTROSPECTION_QUERY: &str = r#"query IntrospectionQuery { __schema { queryType { name } mutationType { name } subscriptionType { name } types { ...FullType } directives { name description locations args { ...InputValue } } } } fragment FullType on __Type { kind name description fields(includeDeprecated: true) { name description args { ...InputValue } type { ...TypeRef } isDeprecated deprecationReason } inputFields { ...InputValue } interfaces { ...TypeRef } enumValues(includeDeprecated: true) { name description isDeprecated deprecationReason } possibleTypes { ...TypeRef } } fragment InputValue on __InputValue { name description type { ...TypeRef } defaultValue } fragment TypeRef on __Type { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } } } } } }"#;

    #[test]
    fn the_real_introspection_query_passes_the_meta_caps() {
        // Regression test 3 (fix round 1): the genuine query, not a stand-in, must be accepted.
        assert!(
            check_query(REAL_INTROSPECTION_QUERY).is_ok(),
            "the real getIntrospectionQuery() text must pass under the relaxed meta-only caps"
        );

        // Also assert measure() actually ran (i.e. it isn't accidentally passing because
        // something still short-circuits before measurement) and report the real numbers this
        // module's caps were picked against, so a future cap change has ground truth next to it.
        let doc = graphql_parser::parse_query::<&str>(REAL_INTROSPECTION_QUERY).unwrap();
        assert!(
            is_meta_only(&doc),
            "the real introspection query must classify as meta-only"
        );
        let (depth, complexity) = measure(&doc);
        assert!(
            depth > 0 && depth <= META_MAX_DEPTH,
            "measured depth was {depth}"
        );
        assert!(
            complexity > 0 && complexity <= META_MAX_COMPLEXITY,
            "measured complexity was {complexity}"
        );
    }

    #[test]
    fn regression_1_a_20_level_of_type_chain_named_introspectionquery_is_rejected_on_depth() {
        // Reviewer's reproduction: naming a query "IntrospectionQuery" no longer matters at all
        // (the name check is gone) -- what matters is the top-level field and the REAL measured
        // depth. `__type` is a meta field, so this still gets the relaxed cap (15), but a
        // 20-level ofType chain exceeds even that.
        let mut q = String::from("query IntrospectionQuery { __type(name: \"X\") { ");
        for _ in 0..20 {
            q.push_str("ofType { ");
        }
        q.push_str("kind");
        for _ in 0..20 {
            q.push_str(" }");
        }
        q.push_str(" } }");

        let err = check_query(&q).expect_err("a 20-level ofType chain must be rejected");
        assert_eq!(err.reason, RejectReason::Depth);
    }

    #[test]
    fn regression_2_aliased_top_level_schema_selections_are_rejected_on_complexity() {
        // Reviewer's reproduction (originally specified as 2000 aliases x 1 nested field, ~4000
        // measured fields): `__schema` is meta, but alias fan-out is still bounded by
        // META_MAX_COMPLEXITY. Scaled down to 500 aliases x 6 nested leaf fields = 3500 measured
        // fields -- still well over the 3000 cap -- because the literal 2000-alias payload
        // (~50 KB+ even with minimal field names) does not fit under MAX_QUERY_BYTES, and the
        // whole point of fix round 1's Important finding is that the byte cap is checked FIRST.
        // The mechanism under test (alias fan-out under a meta field still hits the complexity
        // cap) is unchanged by the smaller scale.
        let mut q = String::from("{ ");
        for i in 0..500 {
            q.push_str(&format!("a{i}: __schema {{ b c d e f g }} "));
        }
        q.push('}');
        assert!(
            q.len() < MAX_QUERY_BYTES,
            "test payload must fit under the byte cap to exercise the complexity check, not the byte-cap check"
        );

        let err = check_query(&q).expect_err("aliased __schema selections must be rejected");
        assert_eq!(err.reason, RejectReason::Complexity);
    }

    #[test]
    fn regression_5_mixing_data_with_meta_forces_the_strict_tier() {
        // A meta-only top-level field (__schema) alongside one real data field ("f") must NOT
        // get the relaxed treatment for the whole document -- the strict cap (8) applies, and a
        // depth-9 chain under "f" is rejected.
        let mut q = String::from("{ __schema { __typename } ");
        for _ in 0..8 {
            q.push_str("f { ");
        }
        q.push('x');
        for _ in 0..8 {
            q.push_str(" }");
        }
        q.push_str(" }");

        let doc = graphql_parser::parse_query::<&str>(&q).unwrap();
        assert!(
            !is_meta_only(&doc),
            "mixing a data field with __schema must not classify as meta-only"
        );

        let err = check_query(&q).expect_err("depth 9 under the strict cap must be rejected");
        assert_eq!(err.reason, RejectReason::Depth);
    }

    #[test]
    fn a_disguised_deep_data_query_is_still_measured_and_rejected() {
        // Same shape as the old "renaming" test, but the mechanism is now purely structural:
        // "f" is not a meta field, so this is never meta-only regardless of the operation name.
        let mut q = String::from("query IntrospectionQuery { ");
        for _ in 0..10 {
            q.push_str("f { ");
        }
        q.push('x');
        for _ in 0..10 {
            q.push_str(" }");
        }
        q.push_str(" }");
        let err = check_query(&q).expect_err("a disguised deep data query must still be rejected");
        assert_eq!(err.reason, RejectReason::Depth);
    }

    // --- fix round 1: IMPORTANT -- pre-parse hardening -----------------------------------------

    #[test]
    fn regression_4_a_40_level_brace_bomb_is_rejected_by_the_prescan_not_the_parser() {
        // Deliberately not necessarily valid GraphQL past a certain point -- the point is that
        // `prevalidate` must reject this on the raw text, before `graphql_parser::parse_query`
        // (which would otherwise recurse 40 levels and, in a debug build, abort the process) is
        // ever called.
        let mut q = String::from("{ ");
        for _ in 0..40 {
            q.push_str("f { ");
        }
        q.push('x');
        for _ in 0..40 {
            q.push_str(" }");
        }
        q.push_str(" }");

        let err = check_query(&q).expect_err("a 40-level brace nest must be rejected");
        assert_eq!(err.reason, RejectReason::Depth);
        assert!(
            err.message.contains("nests"),
            "expected the pre-parse brace-scan message, got: {}",
            err.message
        );
    }

    #[test]
    fn an_oversized_query_body_is_rejected_before_parsing() {
        let q = format!("{{ f(arg: \"{}\") }}", "x".repeat(MAX_QUERY_BYTES));
        let err = check_query(&q).expect_err("an oversized query body must be rejected");
        assert_eq!(err.reason, RejectReason::Parse);
    }

    #[test]
    fn regression_6_braces_inside_a_string_literal_are_not_miscounted() {
        // 40 braces inside a string argument -- if the scanner didn't ignore string contents,
        // this would be spuriously rejected by the brace-depth pre-scan even though the query's
        // real structure is trivially shallow.
        let braces = "{".repeat(40);
        let q = format!(r#"{{ config(note: "{braces}") {{ id }} }}"#);
        assert_eq!(
            max_brace_depth(&q),
            2,
            "the braces inside the string must not be counted"
        );
        assert!(
            check_query(&q).is_ok(),
            "a brace-heavy string argument must not be rejected"
        );
    }

    #[test]
    fn braces_inside_a_block_string_are_not_miscounted() {
        let braces = "{".repeat(40);
        let q = format!(r#"{{ config(note: """{braces}""") {{ id }} }}"#);
        assert_eq!(
            max_brace_depth(&q),
            2,
            "the braces inside the block string must not be counted"
        );
        assert!(
            check_query(&q).is_ok(),
            "a brace-heavy block-string argument must not be rejected"
        );
    }

    #[test]
    fn an_escaped_quote_inside_a_string_does_not_end_it_early() {
        // `\"` inside a regular string must not be treated as the closing quote -- otherwise the
        // `{` right after it would flip back into "Normal" mode and get counted.
        let q = r#"{ config(note: "a \" b { c") { id } }"#;
        assert_eq!(max_brace_depth(q), 2);
        assert!(check_query(q).is_ok());
    }
}
