//! Property tests for `VPath`'s `Display`/`FromStr` round-trip (T-2.2.1 AC).
//!
//! Strategy: generate structured `VPath` values (scheme, optional
//! authority, path segments, and — recursively — nested nesting depth),
//! not raw strings. This is the standard shape for a round-trip property:
//! `parse(display(v)) == v` for arbitrary well-formed `v`, which is exactly
//! what higher layers (the mount table, the panel model, persisted
//! bookmarks) rely on. A second property fuzzes raw strings through
//! `FromStr` and asserts only that parsing never panics.

use std::str::FromStr;
use std::sync::Arc;

use duet_types::{Authority, MountId, Scheme, UnixPathBuf, VPath};
use proptest::prelude::*;

/// A single path-segment strategy: 1-10 arbitrary Unicode scalars,
/// excluding `/` and NUL (both structurally invalid within one segment),
/// and excluding the literal string `.` (normalised away by
/// `UnixPathBuf`, so not useful to construct through it). Deliberately
/// includes our own grammar's reserved characters (`!`, `%`, `:`, `@`) to
/// exercise percent-encoding.
fn segment_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[^/\x00]{1,10}")
        .unwrap()
        .prop_filter("not a bare '.' component", |s| s != ".")
}

fn path_strategy() -> impl Strategy<Value = UnixPathBuf> {
    proptest::collection::vec(segment_strategy(), 0..4).prop_map(|segments| {
        segments.into_iter().fold(UnixPathBuf::root(), |acc, seg| {
            acc.join(&seg).expect("valid component")
        })
    })
}

fn scheme_strategy() -> impl Strategy<Value = Scheme> {
    proptest::string::string_regex("[a-z][a-z0-9+.-]{0,7}")
        .unwrap()
        .prop_map(|s| Scheme::new(&s).expect("regex produces a valid scheme"))
}

fn authority_field_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[^/\x00]{1,10}").unwrap()
}

fn authority_strategy() -> impl Strategy<Value = Option<Authority>> {
    proptest::option::of((
        proptest::option::of(authority_field_strategy()),
        authority_field_strategy(),
        proptest::option::of(1u16..=65535u16),
    ))
    .prop_map(|opt| opt.map(|(user, host, port)| Authority { user, host, port }))
}

/// Generates arbitrary `VPath`s, including nested (archive-on-archive)
/// mounts up to a bounded depth so the strategy always terminates.
fn vpath_strategy() -> impl Strategy<Value = VPath> {
    let root_leaf = (scheme_strategy(), authority_strategy(), path_strategy()).prop_map(
        |(scheme, authority, inner)| VPath::new(MountId::Root { scheme, authority }, inner),
    );

    root_leaf.prop_recursive(3, 16, 2, |inner_vpath| {
        (scheme_strategy(), inner_vpath, path_strategy()).prop_map(|(scheme, parent, inner)| {
            VPath::new(
                MountId::Nested {
                    scheme,
                    parent: Arc::new(parent),
                },
                inner,
            )
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// The core AC: `VPath` round-trips through `Display`/`FromStr` for
    /// arbitrary well-formed values, including nested mounts.
    #[test]
    fn vpath_display_parse_round_trips(v in vpath_strategy()) {
        let s = v.to_string();
        let parsed = VPath::from_str(&s).unwrap_or_else(|e| {
            panic!("failed to reparse {s:?}: {e}")
        });
        prop_assert_eq!(parsed, v);
    }

    /// Formatting is itself idempotent: re-parsing and re-displaying a
    /// value produces byte-identical output (canonical form is stable).
    #[test]
    fn vpath_display_is_stable_under_reparse(v in vpath_strategy()) {
        let s1 = v.to_string();
        let reparsed: VPath = s1.parse().unwrap();
        let s2 = reparsed.to_string();
        prop_assert_eq!(s1, s2);
    }

    /// Parsing arbitrary strings must never panic, whether or not they
    /// happen to be valid `VPath` syntax.
    #[test]
    fn vpath_from_str_never_panics(s in ".{0,64}") {
        let _ = VPath::from_str(&s);
    }

    /// A round-tripped `VPath`'s nesting depth (root vs. nested) matches
    /// the source value's, i.e. nesting itself survives the trip, not
    /// just the leaf path text.
    #[test]
    fn nesting_is_preserved(v in vpath_strategy()) {
        let s = v.to_string();
        let parsed: VPath = s.parse().unwrap();
        prop_assert_eq!(parsed.is_nested(), v.is_nested());
    }
}

#[test]
fn readme_examples_parse_and_round_trip() {
    for s in [
        "file:///home/u/x.zip",
        "zip:file:///home/u/x.zip!/a/b",
        "sftp://host/srv/logs",
    ] {
        let v: VPath = s.parse().unwrap();
        assert_eq!(v.to_string(), s);
    }
}
