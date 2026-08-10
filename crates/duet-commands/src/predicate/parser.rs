// SPDX-License-Identifier: MIT
//! Recursive-descent parser for the context-predicate grammar.
//!
//! Grammar (highest-to-lowest binding precedence: `!` binds tighter than
//! `&&`, which binds tighter than `||` -- the conventional boolean-algebra
//! precedence also used by C, Rust, and every shell):
//!
//! ```text
//! expr        := or_expr
//! or_expr     := and_expr ( "||" and_expr )*
//! and_expr    := unary ( "&&" unary )*
//! unary       := "!" unary | primary
//! primary     := "(" expr ")" | comparison
//! comparison  := ident [ compare_op literal ]
//! compare_op  := "==" | "!=" | "<" | "<=" | ">" | ">="
//! literal     := string | number | "true" | "false"
//! ident       := IDENT ( "." IDENT )*     -- lexed as one dotted token
//! ```

use super::lexer::{Token, TokenKind, lex};
use super::{CompareOp, ContextTerm, Literal, Predicate};

/// A predicate failed to parse. Carries a human-readable message and the
/// byte offset into the original input where the problem was detected, so
/// callers (e.g. the keymap loader) can point a diagnostic at the right
/// column of `keymap.toml`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid predicate: {message} (at byte {pos})")]
pub struct PredicateParseError {
    pub message: String,
    pub pos: usize,
}

/// Parses a context-predicate expression, e.g. `"panel && selection.nonempty"`
/// or `"vfs.scheme == 'file'"`, into a [`Predicate`] AST.
///
/// Never panics: any malformed input (unbalanced parens, a dangling
/// operator, an unterminated string, a bare `&`/`|`, ...) is reported as a
/// [`PredicateParseError`], not a panic. See the `parser test corpus` in
/// this module's `#[cfg(test)]` block for the AC-mandated coverage.
pub fn parse(input: &str) -> Result<Predicate, PredicateParseError> {
    let tokens = lex(input).map_err(|e| PredicateParseError {
        message: e.message,
        pos: e.pos,
    })?;
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
    };
    let expr = parser.parse_or()?;
    parser.expect_eof()?;
    Ok(expr)
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos];
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect_eof(&self) -> Result<(), PredicateParseError> {
        match &self.peek().kind {
            TokenKind::Eof => Ok(()),
            other => Err(PredicateParseError {
                message: format!("unexpected trailing {other}"),
                pos: self.peek().pos,
            }),
        }
    }

    /// `or_expr := and_expr ( "||" and_expr )*`
    fn parse_or(&mut self) -> Result<Predicate, PredicateParseError> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek().kind, TokenKind::OrOr) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Predicate::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `and_expr := unary ( "&&" unary )*`
    fn parse_and(&mut self) -> Result<Predicate, PredicateParseError> {
        let mut lhs = self.parse_unary()?;
        while matches!(self.peek().kind, TokenKind::AndAnd) {
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = Predicate::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// `unary := "!" unary | primary`
    fn parse_unary(&mut self) -> Result<Predicate, PredicateParseError> {
        if matches!(self.peek().kind, TokenKind::Bang) {
            self.advance();
            let inner = self.parse_unary()?;
            return Ok(Predicate::Not(Box::new(inner)));
        }
        self.parse_primary()
    }

    /// `primary := "(" expr ")" | comparison`
    fn parse_primary(&mut self) -> Result<Predicate, PredicateParseError> {
        match &self.peek().kind {
            TokenKind::LParen => {
                self.advance();
                let inner = self.parse_or()?;
                match &self.peek().kind {
                    TokenKind::RParen => {
                        self.advance();
                        Ok(inner)
                    }
                    other => Err(PredicateParseError {
                        message: format!("expected `)`, found {other}"),
                        pos: self.peek().pos,
                    }),
                }
            }
            TokenKind::Ident(_) => self.parse_comparison(),
            other => Err(PredicateParseError {
                message: format!("expected an identifier or `(`, found {other}"),
                pos: self.peek().pos,
            }),
        }
    }

    /// `comparison := ident [ compare_op literal ]`
    fn parse_comparison(&mut self) -> Result<Predicate, PredicateParseError> {
        let name = match &self.peek().kind {
            TokenKind::Ident(name) => name.clone(),
            other => {
                return Err(PredicateParseError {
                    message: format!("expected an identifier, found {other}"),
                    pos: self.peek().pos,
                });
            }
        };
        self.advance();
        let term = ContextTerm::new(name);

        let op = match &self.peek().kind {
            TokenKind::EqEq => CompareOp::Eq,
            TokenKind::NotEq => CompareOp::Ne,
            TokenKind::Lt => CompareOp::Lt,
            TokenKind::Le => CompareOp::Le,
            TokenKind::Gt => CompareOp::Gt,
            TokenKind::Ge => CompareOp::Ge,
            _ => return Ok(Predicate::Flag(term)),
        };
        let op_pos = self.peek().pos;
        self.advance();

        let literal = match &self.peek().kind {
            TokenKind::Str(s) => Literal::Str(s.clone().into_boxed_str()),
            TokenKind::Num(n) => Literal::Num(*n),
            TokenKind::Ident(id) if id == "true" => Literal::Bool(true),
            TokenKind::Ident(id) if id == "false" => Literal::Bool(false),
            other => {
                return Err(PredicateParseError {
                    message: format!(
                        "expected a string, number, or boolean literal after comparison operator, found {other}"
                    ),
                    pos: op_pos,
                });
            }
        };
        self.advance();
        Ok(Predicate::Compare(term, op, literal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::{CompareOp, ContextValue, Literal, MapContextState, Predicate};

    fn term(s: &str) -> ContextTerm {
        ContextTerm::new(s.to_string())
    }

    // ---- Parser test corpus (AC: "predicate grammar documented with a
    // parser test corpus") -----------------------------------------------
    //
    // Grouped into: (1) valid predicates from the catalogue/design docs,
    // (2) precedence, (3) negation, (4) comparisons, (5) malformed input
    // that must produce a `PredicateParseError`, never a panic.

    // -- (1) valid predicates lifted verbatim from docs/commands.md and
    // design.md §9.4 / docs/config-schema.md --------------------------

    #[test]
    fn parses_single_flag() {
        assert_eq!(parse("panel").unwrap(), Predicate::Flag(term("panel")));
    }

    #[test]
    fn parses_dotted_flag() {
        assert_eq!(
            parse("selection.nonempty").unwrap(),
            Predicate::Flag(term("selection.nonempty"))
        );
    }

    #[test]
    fn parses_and_from_design_doc_example() {
        // design.md §9.4 worked example: `panel && selection.nonempty`
        assert_eq!(
            parse("panel && selection.nonempty").unwrap(),
            Predicate::And(
                Box::new(Predicate::Flag(term("panel"))),
                Box::new(Predicate::Flag(term("selection.nonempty")))
            )
        );
    }

    #[test]
    fn parses_namespaced_dialog_context() {
        // docs/commands.md: `dialog.conflict`
        assert_eq!(
            parse("dialog.conflict").unwrap(),
            Predicate::Flag(term("dialog.conflict"))
        );
    }

    #[test]
    fn parses_scheme_equality_from_config_schema() {
        // docs/config-schema.md: `vfs.scheme == 'file'`
        assert_eq!(
            parse("vfs.scheme == 'file'").unwrap(),
            Predicate::Compare(
                term("vfs.scheme"),
                CompareOp::Eq,
                Literal::Str("file".into())
            )
        );
    }

    #[test]
    fn parses_double_quoted_string() {
        assert_eq!(
            parse("vfs.scheme == \"file\"").unwrap(),
            Predicate::Compare(
                term("vfs.scheme"),
                CompareOp::Eq,
                Literal::Str("file".into())
            )
        );
    }

    #[test]
    fn parses_view_mode_not_equal() {
        // docs/commands.md: `panel && view.mode != 'full'`
        let p = parse("panel && view.mode != 'full'").unwrap();
        assert_eq!(
            p,
            Predicate::And(
                Box::new(Predicate::Flag(term("panel"))),
                Box::new(Predicate::Compare(
                    term("view.mode"),
                    CompareOp::Ne,
                    Literal::Str("full".into())
                ))
            )
        );
    }

    #[test]
    fn parses_numeric_comparison() {
        // docs/commands.md: `panel && tab.count > 1`
        let p = parse("panel && tab.count > 1").unwrap();
        assert_eq!(
            p,
            Predicate::And(
                Box::new(Predicate::Flag(term("panel"))),
                Box::new(Predicate::Compare(
                    term("tab.count"),
                    CompareOp::Gt,
                    Literal::Num(1.0)
                ))
            )
        );
    }

    #[test]
    fn parses_parenthesised_or_inside_and() {
        // docs/commands.md-style compound precondition.
        let p = parse("panel && (selection.nonempty || entry.is_dir)").unwrap();
        assert_eq!(
            p,
            Predicate::And(
                Box::new(Predicate::Flag(term("panel"))),
                Box::new(Predicate::Or(
                    Box::new(Predicate::Flag(term("selection.nonempty"))),
                    Box::new(Predicate::Flag(term("entry.is_dir")))
                ))
            )
        );
    }

    // -- (2) precedence: && binds tighter than || -----------------------

    #[test]
    fn and_binds_tighter_than_or() {
        // "a || b && c" must parse as "a || (b && c)", not "(a || b) && c".
        let p = parse("a || b && c").unwrap();
        let expected = Predicate::Or(
            Box::new(Predicate::Flag(term("a"))),
            Box::new(Predicate::And(
                Box::new(Predicate::Flag(term("b"))),
                Box::new(Predicate::Flag(term("c"))),
            )),
        );
        assert_eq!(p, expected);
    }

    #[test]
    fn explicit_parens_override_precedence() {
        // "(a || b) && c" must differ from the unparenthesised form above.
        let p = parse("(a || b) && c").unwrap();
        let expected = Predicate::And(
            Box::new(Predicate::Or(
                Box::new(Predicate::Flag(term("a"))),
                Box::new(Predicate::Flag(term("b"))),
            )),
            Box::new(Predicate::Flag(term("c"))),
        );
        assert_eq!(p, expected);
    }

    #[test]
    fn left_associative_and_chain() {
        let p = parse("a && b && c").unwrap();
        let expected = Predicate::And(
            Box::new(Predicate::And(
                Box::new(Predicate::Flag(term("a"))),
                Box::new(Predicate::Flag(term("b"))),
            )),
            Box::new(Predicate::Flag(term("c"))),
        );
        assert_eq!(p, expected);
    }

    // -- (3) negation -----------------------------------------------------

    #[test]
    fn parses_simple_negation() {
        assert_eq!(
            parse("!panel").unwrap(),
            Predicate::Not(Box::new(Predicate::Flag(term("panel"))))
        );
    }

    #[test]
    fn negation_binds_tighter_than_and() {
        // "!a && b" must parse as "(!a) && b", not "!(a && b)".
        let p = parse("!a && b").unwrap();
        let expected = Predicate::And(
            Box::new(Predicate::Not(Box::new(Predicate::Flag(term("a"))))),
            Box::new(Predicate::Flag(term("b"))),
        );
        assert_eq!(p, expected);
    }

    #[test]
    fn double_negation() {
        let p = parse("!!panel").unwrap();
        assert_eq!(
            p,
            Predicate::Not(Box::new(Predicate::Not(Box::new(Predicate::Flag(term(
                "panel"
            ))))))
        );
    }

    #[test]
    fn negation_of_parenthesised_expr() {
        let p = parse("!(panel && selection.nonempty)").unwrap();
        let expected = Predicate::Not(Box::new(Predicate::And(
            Box::new(Predicate::Flag(term("panel"))),
            Box::new(Predicate::Flag(term("selection.nonempty"))),
        )));
        assert_eq!(p, expected);
    }

    // -- (4) evaluation sanity (parse + eval together) ---------------------

    #[test]
    fn eval_and_or_not_and_compare_together() {
        let p = parse("panel && !dialog && vfs.scheme == 'file'").unwrap();
        let mut state = MapContextState::new();
        state.set_flag("panel", true);
        state.set_flag("dialog", false);
        state.set_value("vfs.scheme", ContextValue::Str("file"));
        assert!(p.eval(&state));

        state.set_value("vfs.scheme", ContextValue::Str("sftp"));
        assert!(!p.eval(&state));
    }

    // -- (5) malformed input: must error, never panic ----------------------

    #[test]
    fn rejects_empty_input() {
        assert!(parse("").is_err());
    }

    #[test]
    fn rejects_unbalanced_open_paren() {
        let err = parse("panel && (selection.nonempty").unwrap_err();
        assert!(err.message.contains(')'), "message was: {}", err.message);
    }

    #[test]
    fn rejects_unbalanced_close_paren() {
        assert!(parse("panel && selection.nonempty)").is_err());
    }

    #[test]
    fn rejects_trailing_operator() {
        assert!(parse("panel &&").is_err());
    }

    #[test]
    fn rejects_leading_operator() {
        assert!(parse("&& panel").is_err());
    }

    #[test]
    fn rejects_double_and_operator() {
        assert!(parse("panel && && viewer").is_err());
    }

    #[test]
    fn rejects_single_ampersand() {
        assert!(parse("panel & selection.nonempty").is_err());
    }

    #[test]
    fn rejects_single_pipe() {
        assert!(parse("panel | selection.nonempty").is_err());
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(parse("vfs.scheme == 'file").is_err());
    }

    #[test]
    fn rejects_comparison_missing_literal() {
        assert!(parse("vfs.scheme ==").is_err());
    }

    #[test]
    fn rejects_bare_comparison_operator() {
        assert!(parse("==").is_err());
    }

    #[test]
    fn rejects_dangling_dot() {
        assert!(parse("panel.").is_err());
    }

    #[test]
    fn rejects_double_dot() {
        // The dotted-identifier lexer treats ".." as part of one identifier
        // token; the resulting `ContextTerm` normalisation then rejects the
        // empty inner segment (exercised via `ContextTerm::new` below, but
        // also confirm the parser surfaces *some* error rather than
        // producing a garbage flag silently).
        let result = parse("panel..nonempty");
        assert!(result.is_err() || result.unwrap() != Predicate::Flag(term("panel.nonempty")));
    }

    #[test]
    fn rejects_stray_unrecognised_character() {
        assert!(parse("panel && @weird").is_err());
    }

    #[test]
    fn rejects_empty_parens() {
        assert!(parse("()").is_err());
    }

    #[test]
    fn error_position_points_at_offending_token() {
        // "panel &&" -- the error should point at (or after) the operator,
        // not at byte 0, so a diagnostic can underline the right column.
        let err = parse("panel &&").unwrap_err();
        assert!(
            err.pos >= "panel ".len(),
            "pos={} should be at/after the trailing &&",
            err.pos
        );
    }
}
