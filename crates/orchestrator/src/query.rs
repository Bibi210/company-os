//! Query normalisation for the FTS5 MATCH operator.
//!
//! RFC bdee1af4 proposition 1 — natural-language queries that contain
//! reserved FTS5 characters (`-`, `*`, `:`, `"`, `(`, `)`, `^`) must be
//! sanitised before being injected into a `MATCH ?` clause, otherwise
//! SQLite raises a parse error and the lexical path silently returns
//! zero results (FACTEUR 4 of diagnostic 6cfd1c62).
//!
//! Three modes:
//!
//! - [`QueryMode::Natural`] — default. Tokenise on whitespace, strip
//!   reserved chars, wrap each token in double quotes, join with `OR`.
//!   Breaks the AND-implicit default of FTS5 (FACTEUR 2).
//! - [`QueryMode::Advanced`] — passthrough. Caller assumes a valid FTS5
//!   expression.
//! - [`QueryMode::Exact`] — wrap the whole input in a single quoted
//!   phrase for an exact-match. Used for id_prefix filters.

/// Mode of FTS5 query construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    Natural,
    Advanced,
    Exact,
}

/// Characters reserved by the FTS5 parser. When the caller does not opt
/// into the advanced mode, we strip these from every token to avoid
/// "no such column" parse errors. `-` is treated specially (NOT prefix)
/// so we always strip it in Natural mode.
const FTS5_RESERVED: &[char] = &['*', ':', '"', '(', ')', '^', '\\'];

/// Sanitise a user query string into an FTS5 MATCH expression.
///
/// On empty input (or input that contains no usable token after
/// sanitisation), returns an empty string. Callers can treat that as a
/// signal to skip the lexical path entirely.
pub fn sanitize_fts_query(input: &str, mode: QueryMode) -> String {
    match mode {
        QueryMode::Advanced => input.to_string(),
        QueryMode::Exact => {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                return String::new();
            }
            // Escape internal double-quotes by doubling them (FTS5
            // syntax for a literal quote inside a quoted phrase).
            let escaped = trimmed.replace('"', "\"\"");
            format!("\"{escaped}\"")
        }
        QueryMode::Natural => natural_to_fts5(input),
    }
}

fn natural_to_fts5(input: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    for raw in input.split_whitespace() {
        // Strip both reserved chars and leading dashes.
        let cleaned: String = raw
            .chars()
            .filter(|c| !FTS5_RESERVED.contains(c) && *c != '-')
            .collect();
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            continue;
        }
        // Wrap each token as a quoted phrase. This neutralises any
        // remaining special chars (digit-only words, accents, …) and
        // guarantees the FTS5 parser sees a literal token.
        tokens.push(format!("\"{cleaned}\""));
    }
    tokens.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_simple_words_joined_by_or() {
        let out = sanitize_fts_query("hello world", QueryMode::Natural);
        assert_eq!(out, "\"hello\" OR \"world\"");
    }

    #[test]
    fn natural_strips_leading_dash() {
        let out = sanitize_fts_query("-foo bar", QueryMode::Natural);
        assert_eq!(out, "\"foo\" OR \"bar\"");
    }

    #[test]
    fn natural_strips_reserved_chars() {
        let out = sanitize_fts_query("foo(bar)*baz:", QueryMode::Natural);
        // All reserved chars stripped, the whole thing collapses to one
        // token "foobarbaz".
        assert_eq!(out, "\"foobarbaz\"");
    }

    #[test]
    fn natural_uuid_passes_through_cleanly() {
        let q = "bdee1af4 4944";
        let out = sanitize_fts_query(q, QueryMode::Natural);
        assert_eq!(out, "\"bdee1af4\" OR \"4944\"");
    }

    #[test]
    fn natural_empty_returns_empty() {
        assert_eq!(sanitize_fts_query("", QueryMode::Natural), "");
        assert_eq!(sanitize_fts_query("   ", QueryMode::Natural), "");
    }

    #[test]
    fn natural_only_reserved_chars_returns_empty() {
        assert_eq!(sanitize_fts_query("*** : -- ()", QueryMode::Natural), "");
    }

    #[test]
    fn advanced_passthrough() {
        let q = "foo OR bar AND baz";
        assert_eq!(sanitize_fts_query(q, QueryMode::Advanced), q);
    }

    #[test]
    fn exact_wraps_double_quotes() {
        let out = sanitize_fts_query("sqlite-vec 0.1.6", QueryMode::Exact);
        assert_eq!(out, "\"sqlite-vec 0.1.6\"");
    }

    #[test]
    fn exact_escapes_internal_quotes() {
        let out = sanitize_fts_query("foo \"bar\" baz", QueryMode::Exact);
        assert_eq!(out, "\"foo \"\"bar\"\" baz\"");
    }

    #[test]
    fn exact_empty_input_returns_empty() {
        assert_eq!(sanitize_fts_query("", QueryMode::Exact), "");
        assert_eq!(sanitize_fts_query("   ", QueryMode::Exact), "");
    }

    #[test]
    fn natural_multilingual_input() {
        // FR + EN technical mix — both should remain as distinct tokens.
        let out = sanitize_fts_query("embeddings sémantiques retrieval", QueryMode::Natural);
        assert!(out.contains("\"embeddings\""));
        assert!(out.contains("\"sémantiques\""));
        assert!(out.contains("\"retrieval\""));
    }
}
