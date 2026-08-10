// SPDX-License-Identifier: MIT
//! The context-predicate grammar and evaluator (design.md §9.4).
//!
//! Contexts are boolean predicates over UI state: base scopes like `panel`,
//! `viewer`, `editor`, `dialog`, `cmdline`, `palette`, plus state terms like
//! `selection.nonempty`, `entry.is_dir`, or `vfs.scheme == 'file'`, composed
//! with `&&` / `||` / `!` and parentheses. Every `Command::precondition` and
//! every `Binding::context` (§9.4's `keymap.toml` sketch, formalised in
//! `docs/config-schema.md` §2) is one of these predicates.
//!
//! ```text
//! panel && selection.nonempty
//! panel && (selection.nonempty || entry.is_dir)
//! vfs.scheme == 'file'
//! dialog.conflict
//! !panel
//! ```
//!
//! Design note (ADR-002): §9.4 says "GPUI's own action-dispatch context
//! system is used as the substrate". This crate does **not** depend on
//! `gpui` (`../../scripts/check-gpui-isolation.sh` enforces that at the
//! workspace level). What lives here is the UI-framework-agnostic grammar,
//! AST, parser, and evaluator; `duet-ui` is expected to own a thin bridge
//! that implements [`ContextState`] over GPUI's `KeyContext`/focus stack so
//! `Predicate::eval` can be driven from real UI state without this crate
//! ever importing `gpui`.
//!
//! See [`parser::parse`] for the parser (recursive-descent, hand-rolled) and
//! its `#[cfg(test)]` module for the parser test corpus required by this
//! task's acceptance criteria.

mod lexer;
mod parser;

pub use parser::{PredicateParseError, parse};

use std::fmt;
use std::str::FromStr;

/// A dotted context-state path a predicate refers to, e.g. `"panel"`,
/// `"selection.nonempty"`, or `"vfs.scheme"`.
///
/// This is intentionally *not* validated as strictly as [`crate::CommandId`]
/// (context terms are an open vocabulary: every UI surface, dialog, and
/// plugin can contribute new ones, per `docs/commands.md`'s "namespaced
/// pseudo-contexts" note) -- the parser guarantees it is non-empty and has
/// no dangling `.`, and that is the extent of validation done here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContextTerm(Box<str>);

impl ContextTerm {
    /// Wraps a dotted path string as a [`ContextTerm`] without additional
    /// validation (the parser already guarantees well-formedness for terms
    /// it produces; this constructor exists for callers building predicates
    /// programmatically, e.g. tests or a future keymap-editor UI).
    pub fn new(path: impl Into<Box<str>>) -> Self {
        Self(path.into())
    }

    /// Borrows the dotted path as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContextTerm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A comparison operator usable in a `term OP literal` predicate, e.g. the
/// `==` in `vfs.scheme == 'file'` or the `>` in `tab.count > 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl fmt::Display for CompareOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CompareOp::Eq => "==",
            CompareOp::Ne => "!=",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
        };
        f.write_str(s)
    }
}

/// A literal value on the right-hand side of a comparison, e.g. `'file'` or
/// `1` or `true`.
///
/// Note: this derives only `PartialEq` (not `Eq`/`Hash`) because of the
/// `Num(f64)` variant. Keymap conflict detection (see [`crate::keymap`])
/// therefore uses linear structural-equality scans over the (typically
/// low-hundreds-sized) binding list rather than hashing predicates -- fine
/// at load-time-once scale, and it sidesteps `f64`'s non-`Eq`/`Hash` nature
/// entirely rather than papering over it with a bit-pattern hack.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Str(Box<str>),
    Num(f64),
    Bool(bool),
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Str(s) => write!(f, "{s:?}"),
            Literal::Num(n) => write!(f, "{n}"),
            Literal::Bool(b) => write!(f, "{b}"),
        }
    }
}

/// The context-predicate AST. Produced by [`parse`], consumed by
/// [`Predicate::eval`].
///
/// Precedence at parse time (tightest to loosest): `!`, `&&`, `||`. The
/// tree itself has no notion of precedence once built -- it is already
/// shaped correctly, e.g. `a || b && c` parses to
/// `Or(Flag(a), And(Flag(b), Flag(c)))`.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// A bare boolean flag term, e.g. `panel` or `selection.nonempty`.
    Flag(ContextTerm),
    /// A comparison, e.g. `vfs.scheme == 'file'` or `tab.count > 1`.
    Compare(ContextTerm, CompareOp, Literal),
    /// Logical negation, e.g. `!panel`.
    Not(Box<Predicate>),
    /// Logical conjunction, e.g. `panel && selection.nonempty`.
    And(Box<Predicate>, Box<Predicate>),
    /// Logical disjunction, e.g. `panel || viewer`.
    Or(Box<Predicate>, Box<Predicate>),
}

impl Predicate {
    /// Evaluates this predicate against a [`ContextState`] implementation.
    ///
    /// Missing terms are treated as `false` for flags and as "comparison
    /// fails" for comparisons -- an unknown context never accidentally
    /// grants a binding or a command's precondition. This is a deliberate
    /// fail-closed default appropriate for a security- and
    /// correctness-sensitive dispatch path.
    pub fn eval(&self, state: &dyn ContextState) -> bool {
        match self {
            Predicate::Flag(term) => state.flag(term.as_str()),
            Predicate::Compare(term, op, literal) => match state.value(term.as_str()) {
                Some(value) => compare(&value, *op, literal),
                None => false,
            },
            Predicate::Not(inner) => !inner.eval(state),
            Predicate::And(lhs, rhs) => lhs.eval(state) && rhs.eval(state),
            Predicate::Or(lhs, rhs) => lhs.eval(state) || rhs.eval(state),
        }
    }
}

fn compare(value: &ContextValue<'_>, op: CompareOp, literal: &Literal) -> bool {
    match (value, literal) {
        (ContextValue::Str(v), Literal::Str(l)) => str_cmp(v, op, l),
        (ContextValue::Bool(v), Literal::Bool(l)) => bool_cmp(*v, op, *l),
        (ContextValue::Num(v), Literal::Num(l)) => num_cmp(*v, op, *l),
        // Type mismatch between the runtime value and the literal (e.g.
        // comparing a string-valued term against a numeric literal) is
        // always false rather than a panic or a coercion surprise.
        _ => false,
    }
}

fn str_cmp(v: &str, op: CompareOp, l: &str) -> bool {
    match op {
        CompareOp::Eq => v == l,
        CompareOp::Ne => v != l,
        CompareOp::Lt => v < l,
        CompareOp::Le => v <= l,
        CompareOp::Gt => v > l,
        CompareOp::Ge => v >= l,
    }
}

fn bool_cmp(v: bool, op: CompareOp, l: bool) -> bool {
    match op {
        CompareOp::Eq => v == l,
        CompareOp::Ne => v != l,
        // Ordering comparisons on booleans are nonsensical; treat as false
        // rather than adopting Rust's `false < true` convention silently.
        _ => false,
    }
}

fn num_cmp(v: f64, op: CompareOp, l: f64) -> bool {
    match op {
        CompareOp::Eq => v == l,
        CompareOp::Ne => v != l,
        CompareOp::Lt => v < l,
        CompareOp::Le => v <= l,
        CompareOp::Gt => v > l,
        CompareOp::Ge => v >= l,
    }
}

impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Predicate::Flag(term) => write!(f, "{term}"),
            Predicate::Compare(term, op, literal) => write!(f, "{term} {op} {literal}"),
            Predicate::Not(inner) => write!(f, "!{}", Paren(inner)),
            Predicate::And(lhs, rhs) => write!(f, "{} && {}", Paren(lhs), Paren(rhs)),
            Predicate::Or(lhs, rhs) => write!(f, "{} || {}", Paren(lhs), Paren(rhs)),
        }
    }
}

/// Wraps a sub-predicate in parens on [`fmt::Display`] when it is a lower-
/// precedence compound (`&&`/`||`), so the printed form always round-trips
/// through [`parse`] with the same structure.
struct Paren<'a>(&'a Predicate);

impl fmt::Display for Paren<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Predicate::And(..) | Predicate::Or(..) => write!(f, "({})", self.0),
            other => write!(f, "{other}"),
        }
    }
}

impl FromStr for Predicate {
    type Err = PredicateParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse(s)
    }
}

/// Deserializes from a plain TOML/JSON string, e.g. `context = "panel &&
/// selection.nonempty"` in `keymap.toml` (`docs/config-schema.md` §2,
/// `binding.context`).
impl<'de> serde::Deserialize<'de> for Predicate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A resolved context value used on the right-hand side of a comparison
/// lookup. Borrowed (`Str`) rather than owned, since [`ContextState::value`]
/// is called on the hot dispatch path (every keystroke, every palette
/// filter) and should not need to allocate.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextValue<'a> {
    Bool(bool),
    Str(&'a str),
    Num(f64),
}

/// The bridge trait between this crate's predicate evaluator and whatever
/// owns real UI state (`duet-ui`, or a headless harness in tests).
///
/// This is the seam ADR-002 requires: `duet-ui` implements this over GPUI's
/// focus/context stack and panel/selection state; nothing in
/// `duet-commands` needs to know GPUI exists.
pub trait ContextState {
    /// Returns whether a boolean flag term (e.g. `"panel"`,
    /// `"selection.nonempty"`) currently holds. Unknown terms must return
    /// `false` (fail closed), not panic.
    fn flag(&self, term: &str) -> bool;

    /// Returns the current value of a comparable term (e.g. `"vfs.scheme"`,
    /// `"tab.count"`) for use in a `Compare` predicate, or `None` if the
    /// term is unknown or not currently meaningful in this context.
    fn value(&self, term: &str) -> Option<ContextValue<'_>>;
}

/// A trivial in-memory [`ContextState`] backed by two maps -- one for flags,
/// one for comparable values. Useful for tests and for headless tools (e.g.
/// a `duet --dry-run` CLI) that need to drive command dispatch without a
/// real UI behind them.
#[derive(Debug, Default, Clone)]
pub struct MapContextState {
    flags: std::collections::HashMap<String, bool>,
    values: std::collections::HashMap<String, OwnedContextValue>,
}

#[derive(Debug, Clone, PartialEq)]
enum OwnedContextValue {
    Bool(bool),
    Str(String),
    Num(f64),
}

impl MapContextState {
    /// Creates an empty state (every flag `false`, every value `None`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a boolean flag term.
    pub fn set_flag(&mut self, term: impl Into<String>, value: bool) -> &mut Self {
        self.flags.insert(term.into(), value);
        self
    }

    /// Sets a comparable value term.
    pub fn set_value(&mut self, term: impl Into<String>, value: ContextValue<'_>) -> &mut Self {
        let owned = match value {
            ContextValue::Bool(b) => OwnedContextValue::Bool(b),
            ContextValue::Str(s) => OwnedContextValue::Str(s.to_string()),
            ContextValue::Num(n) => OwnedContextValue::Num(n),
        };
        self.values.insert(term.into(), owned);
        self
    }
}

impl ContextState for MapContextState {
    fn flag(&self, term: &str) -> bool {
        self.flags.get(term).copied().unwrap_or(false)
    }

    fn value(&self, term: &str) -> Option<ContextValue<'_>> {
        self.values.get(term).map(|v| match v {
            OwnedContextValue::Bool(b) => ContextValue::Bool(*b),
            OwnedContextValue::Str(s) => ContextValue::Str(s.as_str()),
            OwnedContextValue::Num(n) => ContextValue::Num(*n),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_round_trips_through_parse() {
        for src in [
            "panel",
            "panel && selection.nonempty",
            "panel || viewer",
            "!panel",
            "panel && (selection.nonempty || entry.is_dir)",
            "vfs.scheme == \"file\"",
        ] {
            let ast = parse(src).unwrap();
            let printed = ast.to_string();
            let reparsed = parse(&printed).unwrap();
            assert_eq!(
                ast, reparsed,
                "round-trip mismatch for {src:?} -> {printed:?}"
            );
        }
    }

    #[test]
    fn eval_unknown_flag_is_false() {
        let state = MapContextState::new();
        assert!(!parse("some.unknown.flag").unwrap().eval(&state));
    }

    #[test]
    fn eval_unknown_value_comparison_is_false() {
        let state = MapContextState::new();
        assert!(!parse("vfs.scheme == 'file'").unwrap().eval(&state));
    }

    #[test]
    fn eval_type_mismatch_is_false_not_panic() {
        let mut state = MapContextState::new();
        state.set_value("tab.count", ContextValue::Str("oops"));
        assert!(!parse("tab.count > 1").unwrap().eval(&state));
    }
}
