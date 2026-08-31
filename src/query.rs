//! Compile a `memex search` pattern into PCRE2.
//!
//! Human mode is the default: the pattern is not slash-wrapped. A
//! single-quoted shell string is just the human query. Bare words join with
//! implicit AND (lookaheads, any order). Search applies those lookaheads to
//! one packed span (one title or one message), not the whole archive blob.
//! `AND` and `OR` are keywords (case-insensitive). `|` is also OR, matching
//! the man-page table. Double quotes mark a phrase (contiguous substring,
//! regex-escaped). Regex metacharacters in bare words and phrases are
//! escaped.
//!
//! Regex mode: a trimmed pattern that matches `/<regex>/<flags>`. `i` is
//! case insensitive. `g` means all matches; search already unique-hits by
//! packed message, so `g` is a no-op. `m` is multiline, `s` is dotall, `x`
//! is extended (PCRE2 flags grep-pcre2 already exposes). Other flag letters
//! are an error.
//!
//! `-F` / `--fixed-strings` skips this compile and keeps the pattern as a
//! literal, including slash characters. `-i` still sets case insensitive.
//! `-w` still means whole word: human mode applies that per term, regex
//! mode leaves it for the matcher.

use crate::Error;
use crate::trie::SearchFlags;

/// How [`compile_query`] interpreted the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryKind {
    /// Human query: implicit AND, `AND` / `OR` / `|`, quoted phrases.
    Human,
    /// Slash-wrapped `/pattern/flags`.
    Regex,
    /// `-F` / `--fixed-strings` literal. Not compiled as human or regex.
    Literal,
}

/// PCRE2 pattern plus matcher flags after [`compile_query`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQuery {
    /// Pattern passed to grep-pcre2.
    pub pattern: String,
    /// Matcher flags. Human compile clears [`SearchFlags::fixed_strings`].
    pub flags: SearchFlags,
    /// Which compile path produced this query.
    pub kind: QueryKind,
    /// PCRE2 multiline (`m` in `/pattern/m`).
    pub multi_line: bool,
    /// PCRE2 dotall (`s` in `/pattern/s`).
    pub dotall: bool,
    /// PCRE2 extended (`x` in `/pattern/x`).
    pub extended: bool,
}

impl CompiledQuery {
    /// Use `pattern` as PCRE2 with no human or slash compile.
    pub fn pcre2(pattern: impl Into<String>, flags: SearchFlags) -> Self {
        Self {
            pattern: pattern.into(),
            flags,
            kind: QueryKind::Regex,
            multi_line: false,
            dotall: false,
            extended: false,
        }
    }
}

/// Compile a user pattern into PCRE2.
///
/// Trimmed empty input is [`Error::EmptyPattern`]. Unknown `/pattern/` flags
/// are [`Error::InvalidPattern`].
pub fn compile_query(pattern: &str, flags: SearchFlags) -> Result<CompiledQuery, Error> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err(Error::EmptyPattern);
    }
    if flags.fixed_strings {
        return Ok(CompiledQuery {
            pattern: trimmed.to_owned(),
            flags,
            kind: QueryKind::Literal,
            multi_line: false,
            dotall: false,
            extended: false,
        });
    }
    if let Some((body, flag_chars)) = parse_slash_regex(trimmed) {
        return compile_regex_mode(body, flag_chars, flags);
    }
    compile_human(trimmed, flags)
}

/// `^/(.*)/([a-z]*)$` on the already-trimmed pattern.
fn parse_slash_regex(trimmed: &str) -> Option<(&str, &str)> {
    if !trimmed.starts_with('/') {
        return None;
    }
    let rest = &trimmed[1..];
    let slash = rest.rfind('/')?;
    let body = &rest[..slash];
    let flag_chars = &rest[slash + 1..];
    if !flag_chars.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return None;
    }
    Some((body, flag_chars))
}

fn compile_regex_mode(
    body: &str,
    flag_chars: &str,
    flags: SearchFlags,
) -> Result<CompiledQuery, Error> {
    if body.is_empty() {
        return Err(Error::EmptyPattern);
    }
    let mut ignore_case = flags.ignore_case;
    let mut multi_line = false;
    let mut dotall = false;
    let mut extended = false;
    for flag in flag_chars.chars() {
        match flag {
            'i' => ignore_case = true,
            'g' => {}
            'm' => multi_line = true,
            's' => dotall = true,
            'x' => extended = true,
            other => {
                return Err(Error::InvalidPattern(format!(
                    "unsupported regex flag '{other}'"
                )));
            }
        }
    }
    Ok(CompiledQuery {
        pattern: body.to_owned(),
        flags: SearchFlags {
            ignore_case,
            fixed_strings: false,
            word_regexp: flags.word_regexp,
        },
        kind: QueryKind::Regex,
        multi_line,
        dotall,
        extended,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Term(String),
    And,
    Or,
}

fn compile_human(pattern: &str, flags: SearchFlags) -> Result<CompiledQuery, Error> {
    let tokens = tokenize(pattern, flags.word_regexp)?;
    if tokens.is_empty() {
        return Err(Error::EmptyPattern);
    }
    let compiled = parse_or(&tokens)?;
    let mut out_flags = flags;
    out_flags.fixed_strings = false;
    if flags.word_regexp {
        // Whole word was applied per term. Do not wrap the whole expression.
        out_flags.word_regexp = false;
    }
    Ok(CompiledQuery {
        pattern: compiled,
        flags: out_flags,
        kind: QueryKind::Human,
        multi_line: false,
        dotall: false,
        extended: false,
    })
}

fn tokenize(input: &str, word_regexp: bool) -> Result<Vec<Token>, Error> {
    let mut tokens = Vec::new();
    let mut rest = input;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(stripped) = rest.strip_prefix('"') {
            let Some(end) = stripped.find('"') else {
                return Err(Error::InvalidPattern(
                    "unclosed quote in search pattern".into(),
                ));
            };
            let phrase = &stripped[..end];
            if phrase.is_empty() {
                return Err(Error::InvalidPattern("empty quoted phrase".into()));
            }
            tokens.push(Token::Term(wrap_word(escape_pcre2(phrase), word_regexp)));
            rest = &stripped[end + 1..];
            continue;
        }
        let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let raw = &rest[..len];
        rest = &rest[len..];
        push_unquoted_token(&mut tokens, raw, word_regexp)?;
    }
    Ok(tokens)
}

fn push_unquoted_token(tokens: &mut Vec<Token>, raw: &str, word_regexp: bool) -> Result<(), Error> {
    if raw.eq_ignore_ascii_case("AND") {
        tokens.push(Token::And);
        return Ok(());
    }
    if raw.eq_ignore_ascii_case("OR") || raw == "|" {
        tokens.push(Token::Or);
        return Ok(());
    }
    if raw.contains('|') {
        let mut parts = raw.split('|');
        let Some(first) = parts.next() else {
            return Err(Error::InvalidPattern("empty alternative next to |".into()));
        };
        push_or_part(tokens, first, word_regexp)?;
        for part in parts {
            tokens.push(Token::Or);
            push_or_part(tokens, part, word_regexp)?;
        }
        return Ok(());
    }
    tokens.push(Token::Term(wrap_word(escape_pcre2(raw), word_regexp)));
    Ok(())
}

fn push_or_part(tokens: &mut Vec<Token>, part: &str, word_regexp: bool) -> Result<(), Error> {
    if part.is_empty() {
        return Err(Error::InvalidPattern("empty alternative next to |".into()));
    }
    if part.eq_ignore_ascii_case("AND") || part.eq_ignore_ascii_case("OR") {
        return Err(Error::InvalidPattern(
            "AND or OR next to | is not a search term".into(),
        ));
    }
    tokens.push(Token::Term(wrap_word(escape_pcre2(part), word_regexp)));
    Ok(())
}

fn parse_or(tokens: &[Token]) -> Result<String, Error> {
    let mut alternatives = Vec::new();
    let mut current = Vec::new();
    for token in tokens {
        match token {
            Token::Or => {
                if current.is_empty() {
                    return Err(Error::InvalidPattern(
                        "OR must sit between search terms".into(),
                    ));
                }
                alternatives.push(parse_and(&current)?);
                current.clear();
            }
            other => current.push(other.clone()),
        }
    }
    if current.is_empty() {
        return Err(Error::InvalidPattern(
            "OR must sit between search terms".into(),
        ));
    }
    alternatives.push(parse_and(&current)?);
    Ok(alternatives.join("|"))
}

fn parse_and(tokens: &[Token]) -> Result<String, Error> {
    let mut terms = Vec::new();
    let mut expect_term = true;
    for token in tokens {
        match token {
            Token::And => {
                if expect_term {
                    return Err(Error::InvalidPattern(
                        "AND must sit between search terms".into(),
                    ));
                }
                expect_term = true;
            }
            Token::Term(term) => {
                terms.push(term.clone());
                expect_term = false;
            }
            Token::Or => {
                return Err(Error::InvalidPattern(
                    "OR must sit between search terms".into(),
                ));
            }
        }
    }
    if expect_term {
        return Err(Error::InvalidPattern(
            "AND must sit between search terms".into(),
        ));
    }
    match terms.len() {
        0 => Err(Error::EmptyPattern),
        1 => Ok(terms.remove(0)),
        _ => Ok(terms
            .into_iter()
            .map(|term| format!("(?=.*{term})"))
            .collect()),
    }
}

fn wrap_word(escaped: String, word_regexp: bool) -> String {
    if word_regexp {
        format!(r"(?<!\w)(?:{escaped})(?!\w)")
    } else {
        escaped
    }
}

/// Escape PCRE2 metacharacters in a human term or phrase.
pub fn escape_pcre2(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags() -> SearchFlags {
        SearchFlags::default()
    }

    #[test]
    fn human_and_compiles_to_lookaheads() {
        let query = compile_query("lizard AND the", flags()).expect("compile AND");
        assert_eq!(query.kind, QueryKind::Human);
        assert!(
            query.pattern.contains("(?=.*lizard)") && query.pattern.contains("(?=.*the)"),
            "human AND must compile to lookaheads, got {:?}",
            query.pattern
        );
        assert_eq!(query.pattern, "(?=.*lizard)(?=.*the)");
    }

    #[test]
    fn human_implicit_and_joins_bare_words() {
        let query = compile_query("lizard the", flags()).expect("implicit AND");
        assert_eq!(query.pattern, "(?=.*lizard)(?=.*the)");
        assert_eq!(query.kind, QueryKind::Human);
    }

    #[test]
    fn human_or_compiles_to_pipe() {
        let query = compile_query("a OR b", flags()).expect("compile OR");
        assert_eq!(query.pattern, "a|b");
        let pipe = compile_query("a|b", flags()).expect("pipe OR");
        assert_eq!(pipe.pattern, "a|b");
    }

    #[test]
    fn quoted_phrase_is_contiguous_literal() {
        let query = compile_query(r#""hello world""#, flags()).expect("phrase");
        assert_eq!(query.kind, QueryKind::Human);
        assert_eq!(query.pattern, "hello world");
        assert!(
            !query.pattern.contains("(?="),
            "a single phrase must not become lookahead AND, got {:?}",
            query.pattern
        );
    }

    #[test]
    fn quoted_phrase_escapes_metacharacters() {
        let query = compile_query(r#""hello.world""#, flags()).expect("phrase meta");
        assert_eq!(query.pattern, r"hello\.world");
    }

    #[test]
    fn slash_regex_i_sets_ignore_case() {
        let query = compile_query("/Catfooding/i", flags()).expect("regex i");
        assert_eq!(query.kind, QueryKind::Regex);
        assert_eq!(query.pattern, "Catfooding");
        assert!(query.flags.ignore_case);
        assert!(!query.flags.fixed_strings);
    }

    #[test]
    fn slash_regex_g_is_accepted_as_no_op() {
        let query = compile_query("/foo/gi", flags()).expect("regex gi");
        assert_eq!(query.pattern, "foo");
        assert!(query.flags.ignore_case);
    }

    #[test]
    fn slash_regex_unknown_flag_is_an_error() {
        let err = compile_query("/foo/q", flags()).expect_err("unknown flag");
        let text = err.to_string();
        assert!(text.contains('q'), "unknown flag must be named, got {text}");
    }

    #[test]
    fn fixed_strings_skips_human_and_slash_compile() {
        let query = compile_query(
            "/foo/i",
            SearchFlags {
                fixed_strings: true,
                ..SearchFlags::default()
            },
        )
        .expect("literal");
        assert_eq!(query.kind, QueryKind::Literal);
        assert_eq!(query.pattern, "/foo/i");
        assert!(query.flags.fixed_strings);
        assert!(!query.flags.ignore_case);
    }

    #[test]
    fn and_binds_tighter_than_or() {
        let query = compile_query("a OR b AND c", flags()).expect("precedence");
        assert_eq!(query.pattern, "a|(?=.*b)(?=.*c)");
    }

    #[test]
    fn empty_pattern_is_empty_error() {
        assert!(matches!(
            compile_query("   ", flags()),
            Err(Error::EmptyPattern)
        ));
        assert!(matches!(
            compile_query("//i", flags()),
            Err(Error::EmptyPattern)
        ));
    }

    #[test]
    fn unclosed_quote_is_invalid() {
        let err = compile_query("\"hello", flags()).expect_err("unclosed");
        assert!(err.to_string().contains("unclosed quote"), "got {err}");
    }
}
