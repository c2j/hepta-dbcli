// MySQL regression test suite
// Run: HEPTA_DBCLI_TEST_URL=mysql://mcp:testpass@127.0.0.1:3306/testdb cargo test --features integration --test regress_mysql

#[cfg(feature = "integration")]
mod common;

#[cfg(feature = "integration")]
mod tests {
    use polar_mysql::backend::mysql::MySqlFactory;
    use serde_json::Value;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT_ID: AtomicU32 = AtomicU32::new(1);

    fn mysql_url() -> String {
        std::env::var("HEPTA_DBCLI_TEST_URL")
            .unwrap_or_else(|_| "mysql://root:testpass@127.0.0.1:3306/testdb".to_string())
    }

    async fn connect() -> Box<dyn polar_mysql::backend::DbConn + Send> {
        let pool = crate::common::connect_pool(MySqlFactory, &mysql_url()).await;
        pool.acquire().await.expect("acquire")
    }

    fn unique_name(prefix: &str) -> String {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        format!("{}_{}", prefix, id)
    }

    async fn ensure_table(conn: &mut dyn polar_mysql::backend::DbConn) -> String {
        let name = unique_name("_rt");
        conn.query_drop(&format!("DROP TABLE IF EXISTS {}", name))
            .await
            .ok();
        conn.query_drop(&format!(
            "CREATE TABLE {} (id INT PRIMARY KEY, name VARCHAR(100))",
            name
        ))
        .await
        .expect("create test table");
        conn.query_drop(&format!("INSERT INTO {} VALUES (1, 'hello')", name))
            .await
            .expect("insert test data");
        name
    }

    async fn drop_table(mut conn: Box<dyn polar_mysql::backend::DbConn>, name: &str) {
        let _ = conn
            .query_drop(&format!("DROP TABLE IF EXISTS {}", name))
            .await;
    }

    #[tokio::test]
    async fn mysql_database_info() {
        let mut conn = connect().await;
        let sql = conn.dialect().database_info().to_string();
        let result = polar_mysql::backend::DbConn::query(&mut *conn, &sql)
            .await
            .expect("database_info");
        assert!(result.row_count >= 1);
        crate::common::assert_columns(
            &result.columns,
            &[
                "version",
                "database",
                "current_user",
                "hostname",
                "port",
                "os",
                "charset",
                "collation",
                "version_comment",
            ],
        );
    }

    #[tokio::test]
    async fn mysql_list_tables() {
        let mut conn = connect().await;
        let name = ensure_table(&mut *conn).await;
        let sql = conn.dialect().list_tables().to_string();
        let result = polar_mysql::backend::DbConn::query(&mut *conn, &sql)
            .await
            .expect("list_tables");
        assert!(result.row_count >= 1);
        drop_table(conn, &name).await;
    }

    #[tokio::test]
    async fn mysql_table_columns() {
        let mut conn = connect().await;
        let name = ensure_table(&mut *conn).await;
        let sql = conn.dialect().table_columns().to_string();
        let result = polar_mysql::backend::DbConn::exec(
            &mut *conn,
            &sql,
            &[Value::String("testdb".into()), Value::String(name.clone())],
        )
        .await
        .expect("table_columns");
        assert!(result.row_count >= 1);
        crate::common::assert_columns(
            &result.columns,
            &[
                "column_name",
                "data_type",
                "nullable",
                "default_value",
                "ordinal_position",
                "comment",
                "column_key",
            ],
        );
        drop_table(conn, &name).await;
    }

    #[tokio::test]
    async fn mysql_table_indexes() {
        let mut conn = connect().await;
        let name = ensure_table(&mut *conn).await;
        let sql = conn.dialect().table_indexes().to_string();
        let result = polar_mysql::backend::DbConn::exec(
            &mut *conn,
            &sql,
            &[Value::String("testdb".into()), Value::String(name.clone())],
        )
        .await
        .expect("table_indexes");
        assert!(result.row_count >= 1);
        crate::common::assert_columns(
            &result.columns,
            &[
                "index_name",
                "is_unique",
                "is_primary",
                "columns",
                "index_type",
            ],
        );
        drop_table(conn, &name).await;
    }

    #[tokio::test]
    async fn mysql_execute_query() {
        let mut conn = connect().await;
        let result = polar_mysql::backend::DbConn::query(&mut *conn, "SELECT 1 AS one")
            .await
            .expect("query");
        assert_eq!(result.row_count, 1);
    }

    #[tokio::test]
    async fn mysql_add_limit() {
        let conn = connect().await;
        let limited = conn.dialect().add_limit("SELECT * FROM t", 10);
        assert!(limited.contains("LIMIT 10"));
        let no_double = conn.dialect().add_limit("SELECT * FROM t LIMIT 5", 10);
        assert_eq!(no_double, "SELECT * FROM t LIMIT 5");
    }

    #[tokio::test]
    async fn mysql_build_explain() {
        let conn = connect().await;
        let explain = conn.dialect().build_explain("SELECT 1", false, "JSON");
        assert!(explain.contains("EXPLAIN"));
    }

    #[tokio::test]
    async fn mysql_read_only_prefixes() {
        let conn = connect().await;
        let prefixes = conn.dialect().read_only_prefixes();
        assert!(prefixes.contains(&"SELECT"));
        assert!(prefixes.contains(&"SHOW"));
        assert!(!prefixes.contains(&"WITH"));
    }

    #[tokio::test]
    async fn mysql_query_error() {
        let mut conn = connect().await;
        let result =
            polar_mysql::backend::DbConn::query(&mut *conn, "SELECT * FROM nonexistent_table_xyz")
                .await;
        assert!(result.is_err());
    }

    // Issue #101: MySQL 8 marks several information_schema string columns
    // (DATA_TYPE, COLUMN_TYPE, ...) with the binary flag on the wire, which
    // used to render as `0x696e74` instead of `int`. Real text must come back
    // as text; only genuinely non-UTF-8 bytes keep the hex escape.
    #[tokio::test]
    async fn mysql_information_schema_strings_render_as_text_not_hex() {
        let mut conn = connect().await;
        let result = polar_mysql::backend::DbConn::query(
            &mut *conn,
            "SELECT DATA_TYPE AS data_type, COLUMN_TYPE AS column_type \
             FROM information_schema.COLUMNS LIMIT 1",
        )
        .await
        .expect("information_schema query");
        let row = result.rows.first().expect("at least one column exists");
        for idx in [0usize, 1] {
            match &row[idx] {
                Value::String(s) => {
                    assert!(
                        !s.starts_with("0x"),
                        "information_schema string rendered as hex: {s}"
                    );
                }
                other => panic!("expected string value, got {other}"),
            }
        }
    }

    #[tokio::test]
    async fn mysql_non_utf8_blob_still_renders_as_hex() {
        let mut conn = connect().await;
        let name = unique_name("_blob101");
        conn.query_drop(&format!("DROP TABLE IF EXISTS {name}"))
            .await
            .ok();
        conn.query_drop(&format!(
            "CREATE TABLE {name} (id INT PRIMARY KEY, raw VARBINARY(16))"
        ))
        .await
        .expect("create blob table");
        conn.query_drop(&format!("INSERT INTO {name} VALUES (1, UNHEX('00FFFE'))"))
            .await
            .expect("insert binary");
        let result =
            polar_mysql::backend::DbConn::query(&mut *conn, &format!("SELECT raw FROM {name}"))
                .await
                .expect("query blob");
        conn.query_drop(&format!("DROP TABLE {name}"))
            .await
            .expect("drop blob table");
        let row = result.rows.first().expect("blob row");
        assert_eq!(
            row[0],
            Value::String("0x00fffe".to_string()),
            "invalid UTF-8 bytes keep the hex escape"
        );
    }
}
