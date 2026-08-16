//! The mandatory DoS guards (spec, task-5-brief.md):
//!
//! 1. Page-size clamps ([`clamp_first`] / [`clamp_offset`]) -- pure, called from every
//!    connection resolver in `graphql::query`.
//! 2. A query depth/complexity pre-parse ([`check_query`]) run BEFORE juniper executes anything.
//!    Juniper 0.16 has no built-in cost limiter, so the incoming query text is parsed a second
//!    time with `graphql-parser` (the same crate juniper's own optional `schema-language`
//!    feature depends on) purely to measure it.
//!
//! ## The introspection allowlist
//!
//! GraphiQL's standard `IntrospectionQuery` is legitimately deep: the canonical
//! `getIntrospectionQuery()` text (used by GraphiQL and every standard GraphQL client) nests
//! `fragment TypeRef on __Type { ofType { ofType { ... } } }` seven levels to describe
//! arbitrarily-wrapped `LIST`/`NON_NULL` types, reached through `__schema.types.fields.type`,
//! for a total depth well past 8 and a field count past 500 purely from `__Type`/`__Field`/
//! `__InputValue` meta-fields. That is measured against the schema's own fixed meta-shape, not
//! against attacker-controlled recursion over rows, so it is not the kind of query these guards
//! exist to stop.
//!
//! Rather than raise the limits to accommodate it (which would also raise them for real data
//! queries), [`is_allowlisted_introspection`] recognises *exactly* the shape the brief asks for:
//! a single operation named `IntrospectionQuery` whose top-level selections are only `__schema`
//! and/or `__type`. A hostile client cannot smuggle a large *data* query past this by renaming
//! it -- the top-level field check means only the two meta-fields (whose own depth is bounded by
//! the schema, not by the request) ever bypass the limits.

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

/// Query depth hard cap (spec: "<=8").
pub const MAX_DEPTH: usize = 8;
/// Query complexity hard cap (spec: "<=500 selection fields").
pub const MAX_COMPLEXITY: usize = 500;

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
/// label values the brief mandates.
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

/// Pre-parses `query_text` and rejects it if it is unparseable, too deep, or too complex.
/// `Ok(())` means "safe to hand to juniper for real execution" -- this function never executes
/// anything itself.
pub fn check_query(query_text: &str) -> Result<(), Rejection> {
    let doc = graphql_parser::parse_query::<&str>(query_text).map_err(|e| Rejection {
        reason: RejectReason::Parse,
        message: format!("query does not parse: {e}"),
    })?;

    if is_allowlisted_introspection(&doc) {
        return Ok(());
    }

    let (depth, complexity) = measure(&doc);
    if depth > MAX_DEPTH {
        return Err(Rejection {
            reason: RejectReason::Depth,
            message: format!("query depth {depth} exceeds the maximum of {MAX_DEPTH}"),
        });
    }
    if complexity > MAX_COMPLEXITY {
        return Err(Rejection {
            reason: RejectReason::Complexity,
            message: format!(
                "query selects {complexity} field(s), exceeding the maximum of {MAX_COMPLEXITY}"
            ),
        });
    }
    Ok(())
}

/// Exactly the shape documented in the module header: one operation named `IntrospectionQuery`,
/// selecting only `__schema` and/or `__type` at the top level.
fn is_allowlisted_introspection<'a>(doc: &Document<'a, &'a str>) -> bool {
    let mut operations = doc.definitions.iter().filter_map(|d| match d {
        Definition::Operation(op) => Some(op),
        Definition::Fragment(_) => None,
    });

    let (Some(op), None) = (operations.next(), operations.next()) else {
        return false; // zero or more-than-one operation: not the allowlisted shape.
    };

    let OperationDefinition::Query(query) = op else {
        return false;
    };
    if query.name != Some("IntrospectionQuery") {
        return false;
    }

    !query.selection_set.items.is_empty()
        && query.selection_set.items.iter().all(
            |sel| matches!(sel, Selection::Field(f) if f.name == "__schema" || f.name == "__type"),
        )
}

/// Returns `(max_depth, total_field_count)` across every operation in the document. Depth counts
/// only `Field` boundaries (inline fragments and fragment spreads are transparent, matching the
/// common `graphql-depth-limit` convention); complexity counts every `Field` selection reached
/// through the operation, with fragment spreads resolved (and cycle-guarded, since a
/// self-referential fragment is a valid-looking DoS payload even though it is not a valid
/// document).
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
const WALK_BAILOUT_COMPLEXITY: usize = MAX_COMPLEXITY * 4;

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
                    max_depth = max_depth.max(depth + MAX_DEPTH + 1);
                    complexity += MAX_COMPLEXITY + 1;
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

    // --- depth -----------------------------------------------------------------------------

    #[test]
    fn a_shallow_query_passes() {
        let q = "{ config { authority updatedAt } }";
        assert!(check_query(q).is_ok());
    }

    #[test]
    fn depth_ten_is_rejected() {
        // 10 levels of nesting via a single field name repeated (this is only ever pre-parsed,
        // never executed against a real schema, so an unresolvable field name is fine).
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

    // --- complexity --------------------------------------------------------------------------

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

    // --- the introspection allowlist --------------------------------------------------------

    #[test]
    fn the_standard_introspection_query_passes_despite_its_real_depth() {
        // A trimmed-down but structurally faithful stand-in for graphql-js's
        // `getIntrospectionQuery()`: __schema -> types -> fields -> type -> (TypeRef, nested
        // `ofType` 7 deep). Depth alone blows past 8; this must still pass via the allowlist.
        let q = r#"
            query IntrospectionQuery {
              __schema {
                queryType { name }
                types { ...FullType }
              }
            }
            fragment FullType on __Type {
              kind
              name
              fields {
                name
                type { ...TypeRef }
              }
            }
            fragment TypeRef on __Type {
              kind
              name
              ofType { kind name ofType { kind name ofType { kind name ofType { kind name
                ofType { kind name ofType { kind name ofType { kind name } } } } } } }
            }
        "#;
        assert!(check_query(q).is_ok());
    }

    #[test]
    fn renaming_a_deep_data_query_to_introspectionquery_does_not_bypass_the_guard() {
        // Same operation name, but the top-level selection isn't __schema/__type -- must still
        // be measured (and rejected) normally.
        let mut q = String::from("query IntrospectionQuery { ");
        for _ in 0..10 {
            q.push_str("f { ");
        }
        q.push('x');
        for _ in 0..10 {
            q.push_str(" }");
        }
        q.push_str(" }");
        let err = check_query(&q).expect_err("a disguised deep query must still be rejected");
        assert_eq!(err.reason, RejectReason::Depth);
    }
}
