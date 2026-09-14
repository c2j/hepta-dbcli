// ─── Audit scrubbing helpers (redaction / truncation / decoding) ────
//
// Issue #57 D7: passwords and DSN userinfo must never reach the audit file,
// and SQL text is capped at 8 KiB on a UTF-8 char boundary so every emitted
// line stays valid JSON.

/// Replace the password in a DSN's userinfo with `****`.
///
/// Only the authority component is inspected, so an `@` in a path
/// (`duckdb:///tmp/a@b.db`) or a bare `host:port` without userinfo is left
/// untouched.
pub(crate) fn redact_dsn(url: &str) -> String {
    let rest_start = match url.find("://") {
        Some(i) => i + 3,
        None => 0,
    };
    let rest = &url[rest_start..];
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];

    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    let Some(colon) = authority[..at].rfind(':') else {
        return url.to_string();
    };

    let pw_start = rest_start + colon + 1;
    let pw_end = rest_start + at;
    format!("{}****{}", &url[..pw_start], &url[pw_end..])
}

/// Percent-decode a URL component. Invalid escapes are preserved verbatim
/// rather than dropped, so we never corrupt a credential we cannot parse.
pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1]);
            let lo = hex_val(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Truncate to at most `max_bytes`, never splitting a UTF-8 char.
/// Returns `(text, truncated)`.
pub(crate) fn truncate_for_audit(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_redact_password_in_mysql_dsn() {
        assert_eq!(
            redact_dsn("mysql://mcp:s3cret@127.0.0.1:3306/testdb"),
            "mysql://mcp:****@127.0.0.1:3306/testdb"
        );
    }

    #[test]
    fn should_redact_password_in_oracle_dsn() {
        assert_eq!(
            redact_dsn("oracle://system:tiger@oracle.internal:1521/FREEPDB1"),
            "oracle://system:****@oracle.internal:1521/FREEPDB1"
        );
    }

    #[test]
    fn should_leave_dsn_without_userinfo_untouched() {
        assert_eq!(
            redact_dsn("mysql://127.0.0.1:3306/testdb"),
            "mysql://127.0.0.1:3306/testdb"
        );
        assert_eq!(
            redact_dsn("duckdb:///tmp/shop.duckdb"),
            "duckdb:///tmp/shop.duckdb"
        );
    }

    #[test]
    fn should_not_treat_at_in_path_as_userinfo() {
        assert_eq!(
            redact_dsn("duckdb:///tmp/weird@name.duckdb"),
            "duckdb:///tmp/weird@name.duckdb"
        );
    }

    #[test]
    fn should_keep_query_string_after_redaction() {
        assert_eq!(
            redact_dsn("gaussdb://gaussdb:pw@host:5432/testdb?sslmode=require"),
            "gaussdb://gaussdb:****@host:5432/testdb?sslmode=require"
        );
    }

    #[test]
    fn should_percent_decode_userinfo() {
        assert_eq!(percent_decode("a%40b"), "a@b");
        assert_eq!(percent_decode("pa%3Ass"), "pa:ss");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn should_not_truncate_short_text() {
        let (out, truncated) = truncate_for_audit("SELECT 1", 8192);
        assert_eq!(out, "SELECT 1");
        assert!(!truncated);
    }

    #[test]
    fn should_truncate_exactly_over_limit() {
        let text = "a".repeat(10);
        let (out, truncated) = truncate_for_audit(&text, 10);
        assert_eq!(out, text);
        assert!(!truncated);
    }

    #[test]
    fn should_cut_on_char_boundary_not_mid_codepoint() {
        // "中" is 3 bytes; limit of 4 must not split the second char.
        let text = "中中中";
        let (out, truncated) = truncate_for_audit(text, 4);
        assert!(truncated);
        assert_eq!(out, "中");
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn should_never_exceed_byte_limit() {
        let text = "é".repeat(100); // 2 bytes each
        let (out, truncated) = truncate_for_audit(&text, 101);
        assert!(truncated);
        assert!(out.len() <= 101);
    }
}
