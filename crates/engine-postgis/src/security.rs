//! Identifier quoting and DSN redaction helpers.
//!
//! `quote_ident` wraps config-supplied identifiers in `"..."` with proper
//! escaping (PostgreSQL double-quote style). `redact_db_url` strips the
//! password from a postgres:// DSN before logging so credentials cannot
//! leak into logs, metrics, or error messages.

use thiserror::Error;

const MAX_IDENT_LEN: usize = 63;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum QuoteError {
    #[error("identifier must not be empty")]
    Empty,
    #[error("identifier exceeds 63-byte PostgreSQL NAMEDATALEN limit")]
    TooLong,
    #[error("identifier contains an invalid NUL byte")]
    InvalidChar,
}

/// Quote a PostgreSQL identifier (table, column, schema) for safe inlining.
///
/// Wraps the input in `"..."` and doubles any embedded `"`. Rejects empty
/// strings, identifiers over 63 bytes (the `NAMEDATALEN` limit), and
/// identifiers containing NUL bytes (which PostgreSQL would reject anyway,
/// but we surface earlier to keep error paths hermetic).
pub fn quote_ident(s: &str) -> Result<String, QuoteError> {
    if s.is_empty() {
        return Err(QuoteError::Empty);
    }
    if s.len() > MAX_IDENT_LEN {
        return Err(QuoteError::TooLong);
    }
    if s.contains('\0') {
        return Err(QuoteError::InvalidChar);
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
            out.push('"');
        } else {
            out.push(c);
        }
    }
    out.push('"');
    Ok(out)
}

/// Redact the password from a `postgres://` DSN for safe logging.
///
/// Idempotent when no password is present. For inputs that don't contain
/// `://` (treated as malformed), logs a warning and returns the input
/// unchanged so callers never panic on log paths.
pub fn redact_db_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        tracing::warn!("redact_db_url: input does not contain '://'; returning unchanged");
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let rest = &url[authority_start..];

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];

    let Some(at_pos) = authority.rfind('@') else {
        return url.to_string();
    };
    let userinfo = &authority[..at_pos];
    let Some(colon_pos) = userinfo.find(':') else {
        return url.to_string();
    };
    let user = &userinfo[..colon_pos];

    let mut out = String::with_capacity(url.len());
    out.push_str(&url[..authority_start]);
    // SAFETY: `user` is the userinfo fragment of a pre-parsed URL, not
    // a config- or request-derived value; this file builds log strings,
    // never SQL. The `// nosqlcheck` marker narrows the tripwire
    // exemption to this single line instead of the whole file.
    out.push_str(user); // nosqlcheck
    out.push_str(&rest[at_pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_simple() {
        assert_eq!(quote_ident("x").unwrap(), "\"x\"");
    }

    #[test]
    fn quote_embedded_double_quote() {
        assert_eq!(quote_ident("a\"b").unwrap(), "\"a\"\"b\"");
    }

    #[test]
    fn quote_injection_attempt() {
        assert_eq!(
            quote_ident("\"; DROP TABLE x;--").unwrap(),
            "\"\"\"; DROP TABLE x;--\""
        );
    }

    #[test]
    fn quote_rejects_nul_byte() {
        assert_eq!(quote_ident("a\0b"), Err(QuoteError::InvalidChar));
    }

    #[test]
    fn quote_rejects_empty() {
        assert_eq!(quote_ident(""), Err(QuoteError::Empty));
    }

    #[test]
    fn quote_rejects_too_long() {
        assert_eq!(quote_ident(&"x".repeat(64)), Err(QuoteError::TooLong));
    }

    #[test]
    fn quote_accepts_max_length() {
        let ident = "x".repeat(63);
        assert!(quote_ident(&ident).is_ok());
    }

    #[test]
    fn redact_strips_password() {
        assert_eq!(
            redact_db_url("postgres://user:secret@host:5432/db"),
            "postgres://user@host:5432/db"
        );
    }

    #[test]
    fn redact_missing_password_is_idempotent() {
        let url = "postgres://user@host:5432/db";
        assert_eq!(redact_db_url(url), url);
    }

    #[test]
    fn redact_no_userinfo_is_idempotent() {
        let url = "postgres://host:5432/db";
        assert_eq!(redact_db_url(url), url);
    }

    #[test]
    fn redact_url_encoded_password() {
        assert_eq!(
            redact_db_url("postgres://user:p%40ss@host:5432/db"),
            "postgres://user@host:5432/db"
        );
    }

    #[test]
    fn redact_empty_password() {
        assert_eq!(
            redact_db_url("postgres://user:@host:5432/db"),
            "postgres://user@host:5432/db"
        );
    }

    #[test]
    fn redact_malformed_url_unchanged() {
        let url = "not-a-url";
        assert_eq!(redact_db_url(url), url);
    }

    #[test]
    fn redact_preserves_path_and_query() {
        assert_eq!(
            redact_db_url("postgres://user:pw@host:5432/db?sslmode=require"),
            "postgres://user@host:5432/db?sslmode=require"
        );
    }

    #[test]
    fn redact_postgresql_scheme() {
        assert_eq!(
            redact_db_url("postgresql://user:pw@host/db"),
            "postgresql://user@host/db"
        );
    }
}
