// SPDX-License-Identifier: MIT
//! Tokenizer for the context-predicate grammar (see `super` for the grammar
//! definition and `super::parser` for the recursive-descent parser that
//! consumes these tokens).

use std::fmt;

/// A lexical token, tagged with the byte offset in the source string it
/// started at (used for error reporting).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub pos: usize,
}

/// The kinds of token the predicate grammar's lexer produces.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenKind {
    /// A dotted identifier path, e.g. `panel`, `selection.nonempty`,
    /// `vfs.scheme`. Dots are consumed as part of the identifier itself, not
    /// as separate tokens -- there is no field-access operator distinct from
    /// this in the grammar.
    Ident(String),
    /// A single- or double-quoted string literal, already unescaped, e.g.
    /// `'file'` or `"file"` both lex to `Str("file".into())`.
    Str(String),
    /// A numeric literal, e.g. `1`, `0.5`, `-3`.
    Num(f64),
    AndAnd,
    OrOr,
    Bang,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    LParen,
    RParen,
    /// End of input. The parser treats this as a sentinel so it never reads
    /// past the token slice.
    Eof,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Ident(s) => write!(f, "identifier `{s}`"),
            TokenKind::Str(s) => write!(f, "string literal {s:?}"),
            TokenKind::Num(n) => write!(f, "number `{n}`"),
            TokenKind::AndAnd => write!(f, "`&&`"),
            TokenKind::OrOr => write!(f, "`||`"),
            TokenKind::Bang => write!(f, "`!`"),
            TokenKind::EqEq => write!(f, "`==`"),
            TokenKind::NotEq => write!(f, "`!=`"),
            TokenKind::Lt => write!(f, "`<`"),
            TokenKind::Le => write!(f, "`<=`"),
            TokenKind::Gt => write!(f, "`>`"),
            TokenKind::Ge => write!(f, "`>=`"),
            TokenKind::LParen => write!(f, "`(`"),
            TokenKind::RParen => write!(f, "`)`"),
            TokenKind::Eof => write!(f, "end of input"),
        }
    }
}

/// A lexical error: an unrecognised character or an unterminated string, at
/// the given byte offset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at byte {pos}")]
pub(crate) struct LexError {
    pub message: String,
    pub pos: usize,
}

/// Tokenizes `input` in full, returning every token including a trailing
/// [`TokenKind::Eof`]. On any lexical error, returns a [`LexError`] carrying
/// the byte offset of the offending character -- the lexer never panics on
/// malformed input.
pub(crate) fn lex(input: &str) -> Result<Vec<Token>, LexError> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let c = bytes[i] as char;

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        let start = i;
        match c {
            '(' => {
                tokens.push(Token {
                    kind: TokenKind::LParen,
                    pos: start,
                });
                i += 1;
            }
            ')' => {
                tokens.push(Token {
                    kind: TokenKind::RParen,
                    pos: start,
                });
                i += 1;
            }
            '!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    tokens.push(Token {
                        kind: TokenKind::NotEq,
                        pos: start,
                    });
                    i += 2;
                } else {
                    tokens.push(Token {
                        kind: TokenKind::Bang,
                        pos: start,
                    });
                    i += 1;
                }
            }
            '&' => {
                if bytes.get(i + 1) == Some(&b'&') {
                    tokens.push(Token {
                        kind: TokenKind::AndAnd,
                        pos: start,
                    });
                    i += 2;
                } else {
                    return Err(LexError {
                        message: "unexpected `&` (did you mean `&&`?)".to_string(),
                        pos: start,
                    });
                }
            }
            '|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    tokens.push(Token {
                        kind: TokenKind::OrOr,
                        pos: start,
                    });
                    i += 2;
                } else {
                    return Err(LexError {
                        message: "unexpected `|` (did you mean `||`?)".to_string(),
                        pos: start,
                    });
                }
            }
            '=' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    tokens.push(Token {
                        kind: TokenKind::EqEq,
                        pos: start,
                    });
                    i += 2;
                } else {
                    return Err(LexError {
                        message: "unexpected `=` (did you mean `==`?)".to_string(),
                        pos: start,
                    });
                }
            }
            '<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    tokens.push(Token {
                        kind: TokenKind::Le,
                        pos: start,
                    });
                    i += 2;
                } else {
                    tokens.push(Token {
                        kind: TokenKind::Lt,
                        pos: start,
                    });
                    i += 1;
                }
            }
            '>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    tokens.push(Token {
                        kind: TokenKind::Ge,
                        pos: start,
                    });
                    i += 2;
                } else {
                    tokens.push(Token {
                        kind: TokenKind::Gt,
                        pos: start,
                    });
                    i += 1;
                }
            }
            '\'' | '"' => {
                let quote = c;
                let mut j = i + 1;
                let mut value = String::new();
                let mut closed = false;
                while j < bytes.len() {
                    let cj = bytes[j] as char;
                    if cj == quote {
                        closed = true;
                        j += 1;
                        break;
                    }
                    value.push(cj);
                    j += 1;
                }
                if !closed {
                    return Err(LexError {
                        message: format!("unterminated string literal starting with {quote:?}"),
                        pos: start,
                    });
                }
                tokens.push(Token {
                    kind: TokenKind::Str(value),
                    pos: start,
                });
                i = j;
            }
            c if c.is_ascii_digit()
                || (c == '-'
                    && bytes
                        .get(i + 1)
                        .is_some_and(|b| (*b as char).is_ascii_digit())) =>
            {
                let mut j = i + 1;
                if c == '-' {
                    j = i + 1;
                }
                while j < bytes.len()
                    && ((bytes[j] as char).is_ascii_digit() || bytes[j] as char == '.')
                {
                    j += 1;
                }
                let text = &input[i..j];
                match text.parse::<f64>() {
                    Ok(n) => tokens.push(Token {
                        kind: TokenKind::Num(n),
                        pos: start,
                    }),
                    Err(_) => {
                        return Err(LexError {
                            message: format!("invalid numeric literal `{text}`"),
                            pos: start,
                        });
                    }
                }
                i = j;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut j = i + 1;
                while j < bytes.len() {
                    let cj = bytes[j] as char;
                    if cj.is_ascii_alphanumeric() || cj == '_' || cj == '.' || cj == '-' {
                        j += 1;
                    } else {
                        break;
                    }
                }
                let text = &input[i..j];
                // A trailing '.' (e.g. "panel.") is almost certainly a typo;
                // reject it rather than silently accepting a dangling dot.
                if text.ends_with('.') {
                    return Err(LexError {
                        message: format!("identifier `{text}` has a trailing `.`"),
                        pos: start,
                    });
                }
                tokens.push(Token {
                    kind: TokenKind::Ident(text.to_string()),
                    pos: start,
                });
                i = j;
            }
            other => {
                return Err(LexError {
                    message: format!("unexpected character {other:?}"),
                    pos: start,
                });
            }
        }
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        pos: input.len(),
    });
    Ok(tokens)
}
