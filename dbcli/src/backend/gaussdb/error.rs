// ─── PG SQLSTATE ↔ SQLCODE mapping and structured error extraction ───

/// Map PostgreSQL SQLSTATE codes to Oracle-style SQLCODE.
/// Based on common SQLSTATE values encountered in PostgreSQL/GaussDB.
pub(crate) fn sqlstate_to_sqlcode(state: &str) -> i32 {
    match state {
        "00000" => 0,
        "01000" => 100,
        "02000" => 100,
        "08000" | "08006" => -402,
        "22000" | "22012" => -302,
        "22001" => -302,
        "22003" => -304,
        "22004" => -302,
        "22005" => -303,
        "22007" => -181,
        "22008" => -180,
        "23000" | "23502" => -407,
        "23503" => -530,
        "23505" => -803,
        "23514" => -543,
        "28P01" => -923,
        "2D000" => -502,
        "3D000" | "3F000" | "42P01" => -204,
        "42501" => -199,
        "42601" => -104,
        "42701" => -601,
        "42703" => -206,
        "42704" => -204,
        "42P02" => -516,
        "42P07" => -601,
        "57014" => -952,
        "P0001" => -461,
        "XX000" => -402,
        _ => -1,
    }
}

/// Extract structured PG error details from a gaussdb::Error for MCP response.
///
/// Walks the error source chain to find a gaussdb::Error and extract its
/// embedded DbError fields (sqlstate, severity, message, detail, hint,
/// schema, table, column, constraint).
pub(crate) fn extract_pg_error(
    err: &(dyn std::error::Error + 'static),
) -> Option<serde_json::Value> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        if let Some(pg_err) = e.downcast_ref::<gaussdb::Error>() {
            if let Some(db_err) = pg_err.as_db_error() {
                let sqlstate = db_err.code().code();
                let sqlcode = sqlstate_to_sqlcode(sqlstate);
                let mut data = serde_json::json!({
                    "sqlstate": sqlstate,
                    "sqlcode": sqlcode,
                    "severity": db_err.severity(),
                    "message": db_err.message(),
                });
                if let Some(detail) = db_err.detail() {
                    data["detail"] = serde_json::json!(detail);
                }
                if let Some(hint) = db_err.hint() {
                    data["hint"] = serde_json::json!(hint);
                }
                if let Some(schema) = db_err.schema() {
                    data["schema"] = serde_json::json!(schema);
                }
                if let Some(table) = db_err.table() {
                    data["table"] = serde_json::json!(table);
                }
                if let Some(column) = db_err.column() {
                    data["column"] = serde_json::json!(column);
                }
                if let Some(constraint) = db_err.constraint() {
                    data["constraint"] = serde_json::json!(constraint);
                }
                return Some(data);
            }
        }
        current = e.source();
    }
    None
}

/// Render a PG error message with SQLSTATE prefix (shared by query + connect paths).
pub(crate) fn render_pg_error(op: &str, sqlstate: &str, message: &str) -> String {
    format!("GaussDB {} failed: [SQLSTATE {}] {}", op, sqlstate, message)
}

/// Wrap a gaussdb::Error into a DbError with PG structured details embedded
/// in the display message.
pub(crate) fn wrap_gaussdb_error(op: &str, err: gaussdb::Error) -> crate::backend::error::DbError {
    let pg_info = extract_pg_error(&err);
    let msg = if let Some(info) = pg_info {
        render_pg_error(
            op,
            info["sqlstate"].as_str().unwrap_or("?"),
            info["message"].as_str().unwrap_or("?"),
        )
    } else {
        format!("GaussDB {} failed: {}", op, err)
    };
    crate::backend::error::DbError::query_with_source(msg, err)
}

/// Wrap a gaussdb::Error from a connection attempt into a DbError with PG
/// structured details embedded in the display message. Mirrors
/// `wrap_gaussdb_error` but classifies as ConnectionFailed and appends the
/// (already password-redacted) target.
pub(crate) fn wrap_gaussdb_connect_error(
    err: gaussdb::Error,
    target: &str,
) -> crate::backend::error::DbError {
    let pg_info = extract_pg_error(&err);
    let msg = if let Some(info) = pg_info {
        format!(
            "{} (target: {})",
            render_pg_error(
                "connect",
                info["sqlstate"].as_str().unwrap_or("?"),
                info["message"].as_str().unwrap_or("?"),
            ),
            target,
        )
    } else {
        format!("GaussDB connect failed: {} (target: {})", err, target)
    };
    crate::backend::error::DbError::connection_with_source(msg, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlstate_to_sqlcode_known() {
        assert_eq!(sqlstate_to_sqlcode("23505"), -803); // unique violation
        assert_eq!(sqlstate_to_sqlcode("42P01"), -204); // undefined table
        assert_eq!(sqlstate_to_sqlcode("57014"), -952); // query canceled
        assert_eq!(sqlstate_to_sqlcode("28P01"), -923); // invalid password
        assert_eq!(sqlstate_to_sqlcode("42601"), -104); // syntax error
    }

    #[test]
    fn test_sqlstate_to_sqlcode_unknown() {
        assert_eq!(sqlstate_to_sqlcode("XXXXX"), -1);
    }

    #[test]
    fn test_sqlstate_to_sqlcode_success() {
        assert_eq!(sqlstate_to_sqlcode("00000"), 0);
    }

    #[test]
    fn test_sqlstate_to_sqlcode_23000_family() {
        assert_eq!(sqlstate_to_sqlcode("23000"), -407);
        assert_eq!(sqlstate_to_sqlcode("23502"), -407);
        assert_eq!(sqlstate_to_sqlcode("23503"), -530);
        assert_eq!(sqlstate_to_sqlcode("23514"), -543);
    }

    #[test]
    fn test_sqlstate_to_sqlcode_42_family() {
        assert_eq!(sqlstate_to_sqlcode("42P01"), -204);
        assert_eq!(sqlstate_to_sqlcode("42501"), -199);
        assert_eq!(sqlstate_to_sqlcode("42601"), -104);
        assert_eq!(sqlstate_to_sqlcode("42703"), -206);
    }

    #[test]
    fn test_render_pg_error_shape() {
        assert_eq!(
            render_pg_error("query", "28P01", "password authentication failed"),
            "GaussDB query failed: [SQLSTATE 28P01] password authentication failed"
        );
    }
}
