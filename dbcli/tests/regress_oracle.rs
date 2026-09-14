// Oracle regression test suite
// Run: POLARDB_ORACLE_TEST_URL=oracle://system:testpass@127.0.0.1:1521/FREEPDB1 cargo test --features "oracle,integration" --test regress_oracle

#[cfg(all(feature = "integration", feature = "oracle"))]
mod common;

#[cfg(all(feature = "integration", feature = "oracle"))]
mod tests {
    use polar_mysql::backend::oracle::OracleFactory;
    use polar_mysql::backend::{DbConn, KeysetPageSpec};
    use serde_json::Value;

    fn oracle_url() -> Option<String> {
        std::env::var("POLARDB_ORACLE_TEST_URL").ok()
    }

    async fn connect() -> Option<Box<dyn polar_mysql::backend::DbConn + Send>> {
        let url = oracle_url()?;
        let pool = crate::common::connect_pool(OracleFactory, &url).await;
        Some(pool.acquire().await.expect("acquire"))
    }

    async fn try_connect_native() -> Option<Box<dyn polar_mysql::backend::DbConn + Send>> {
        let url = oracle_url()?;
        let factory: std::sync::Arc<dyn polar_mysql::backend::BackendFactory> =
            std::sync::Arc::new(polar_mysql::backend::oracle_native::OracleFactory);
        let mut registry = polar_mysql::backend::factory::BackendRegistry::new();
        registry.register(factory);
        let pool = registry
            .connect_with_fallback("oracle", &url, None, false)
            .await
            .ok()?;
        pool.acquire().await.ok()
    }

    const TABLE: &str = "REGRESS_TEST";

    async fn ensure_table(conn: &mut dyn polar_mysql::backend::DbConn) {
        let _ = conn
            .query_drop(&format!(
                "BEGIN EXECUTE IMMEDIATE 'DROP TABLE {}'; EXCEPTION WHEN OTHERS THEN NULL; END;",
                TABLE
            ))
            .await;
        conn.query_drop(&format!(
            "CREATE TABLE {} (id NUMBER PRIMARY KEY, name VARCHAR2(100), amount NUMBER(10,2))",
            TABLE
        ))
        .await
        .expect("create table");
        conn.query_drop(&format!("INSERT INTO {} VALUES (1, 'hello', 99.99)", TABLE))
            .await
            .expect("insert");
    }

    async fn drop_table(mut conn: Box<dyn polar_mysql::backend::DbConn>) {
        let _ = conn.query_drop(&format!("DROP TABLE {}", TABLE)).await;
    }

    #[tokio::test]
    async fn oracle_database_info() {
        let Some(mut conn) = connect().await else {
            return;
        };
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
    async fn oracle_list_tables() {
        let Some(mut conn) = connect().await else {
            return;
        };
        ensure_table(&mut *conn).await;
        let sql = conn.dialect().list_tables().to_string();
        let result = polar_mysql::backend::DbConn::query(&mut *conn, &sql)
            .await
            .expect("list_tables");
        assert!(result.row_count >= 1);
        crate::common::assert_columns(
            &result.columns,
            &[
                "schema_name",
                "table_name",
                "table_type",
                "engine",
                "row_count",
                "total_size",
                "comment",
            ],
        );
        drop_table(conn).await;
    }

    #[tokio::test]
    async fn oracle_table_columns() {
        let Some(mut conn) = connect().await else {
            return;
        };
        ensure_table(&mut *conn).await;
        let sql = conn.dialect().table_columns().to_string();
        let result = polar_mysql::backend::DbConn::exec(
            &mut *conn,
            &sql,
            &[Value::String("SYSTEM".into()), Value::String(TABLE.into())],
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
        drop_table(conn).await;
    }

    #[tokio::test]
    async fn oracle_table_indexes() {
        let Some(mut conn) = connect().await else {
            return;
        };
        ensure_table(&mut *conn).await;
        let sql = conn.dialect().table_indexes().to_string();
        let result = polar_mysql::backend::DbConn::exec(
            &mut *conn,
            &sql,
            &[Value::String("SYSTEM".into()), Value::String(TABLE.into())],
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
        drop_table(conn).await;
    }

    #[tokio::test]
    async fn oracle_execute_query() {
        let Some(mut conn) = connect().await else {
            return;
        };
        let result = polar_mysql::backend::DbConn::query(&mut *conn, "SELECT 1 FROM dual")
            .await
            .expect("query");
        assert_eq!(result.row_count, 1);
    }

    #[tokio::test]
    async fn oracle_add_limit() {
        let Some(conn) = connect().await else {
            return;
        };
        let limited = conn.dialect().add_limit("SELECT * FROM t", 10);
        assert!(limited.contains("FETCH FIRST"));
    }

    #[tokio::test]
    async fn oracle_build_explain() {
        let Some(conn) = connect().await else {
            return;
        };
        let explain = conn
            .dialect()
            .build_explain("SELECT 1 FROM dual", false, "BASIC");
        assert!(explain.contains("EXPLAIN PLAN"));
    }

    #[tokio::test]
    async fn oracle_read_only_prefixes() {
        let Some(conn) = connect().await else {
            return;
        };
        let prefixes = conn.dialect().read_only_prefixes();
        assert!(prefixes.contains(&"SELECT"));
        assert!(prefixes.contains(&"WITH"));
        assert!(!prefixes.contains(&"SHOW"));
    }

    #[tokio::test]
    async fn oracle_query_error() {
        let Some(mut conn) = connect().await else {
            return;
        };
        let result =
            polar_mysql::backend::DbConn::query(&mut *conn, "SELECT * FROM nonexistent_xyz").await;
        assert!(result.is_err());
    }

    async fn drop_table_named(conn: &mut dyn DbConn, table: &str) {
        let _ = conn
            .query_drop(&format!(
                "BEGIN EXECUTE IMMEDIATE 'DROP TABLE {table}'; EXCEPTION WHEN OTHERS THEN NULL; END;"
            ))
            .await;
    }

    async fn create_varchar_key_table(conn: &mut dyn DbConn, table: &str) {
        drop_table_named(conn, table).await;
        conn.query_drop(&format!(
            "CREATE TABLE {table} (
               xwdm VARCHAR2(20) NOT NULL,
               bs   VARCHAR2(8)  NOT NULL,
               gddm VARCHAR2(20) NOT NULL,
               CONSTRAINT pk_{table} PRIMARY KEY (xwdm, bs, gddm)
             )"
        ))
        .await
        .expect("create varchar-key table");
        conn.query_drop(&format!(
            "INSERT INTO {table} (xwdm, bs, gddm) VALUES ('47872','-1','D890523805')"
        ))
        .await
        .expect("insert 47872");
        conn.query_drop(&format!(
            "INSERT INTO {table} (xwdm, bs, gddm) VALUES ('55958','-1','D890216050')"
        ))
        .await
        .expect("insert 55958");
        conn.query_drop(&format!(
            "INSERT INTO {table} (xwdm, bs, gddm) VALUES ('A100','-1','D890000001')"
        ))
        .await
        .expect("insert A100");
    }

    /// Digit-only VARCHAR2 must stay a JSON string. `serde_json::from_str("55958")`
    /// used to yield a Number, and keyset SQL then emitted `NLSSORT(55958)` (ORA-01722).
    #[tokio::test]
    async fn oracle_varchar_digit_stays_json_string() {
        let Some(mut conn) = connect().await else {
            return;
        };
        const TABLE: &str = "DD_KEYSET_STR";
        create_varchar_key_table(&mut *conn, TABLE).await;
        let result = DbConn::query(
            &mut *conn,
            &format!("SELECT xwdm FROM {TABLE} WHERE xwdm = '55958'"),
        )
        .await
        .expect("select xwdm");
        assert_eq!(result.row_count, 1);
        match &result.rows[0][0] {
            Value::String(s) => assert_eq!(s, "55958"),
            other => panic!("VARCHAR2 '55958' became {other}, expected JSON string"),
        }
        drop_table_named(&mut *conn, TABLE).await;
    }

    /// Live Oracle: NLS_SORT=BINARY rewrites `NLSSORT(col) > NLSSORT(55958)` into
    /// `col > 55958`. A non-numeric VARCHAR2 value then raises ORA-01722.
    #[tokio::test]
    async fn oracle_nlssort_unquoted_number_is_ora_01722() {
        let Some(mut conn) = connect().await else {
            return;
        };
        const TABLE: &str = "DD_KEYSET_ORA01722";
        create_varchar_key_table(&mut *conn, TABLE).await;
        let err = DbConn::query(
            &mut *conn,
            &format!(
                "SELECT COUNT(*) FROM {TABLE}
                 WHERE NLSSORT(\"XWDM\",'NLS_SORT=BINARY') > NLSSORT(55958,'NLS_SORT=BINARY')"
            ),
        )
        .await
        .expect_err("unquoted NLSSORT(55958) must fail when XWDM has 'A100'");
        let msg = err.to_string();
        // oracle-rs sometimes drops the session instead of surfacing ORA-01722.
        assert!(
            msg.contains("01722")
                || msg.to_lowercase().contains("invalid number")
                || msg.to_lowercase().contains("closed the connection"),
            "expected ORA-01722 or a dropped session, got {msg}"
        );
        drop_table_named(&mut *conn, TABLE).await;
    }

    /// Dialect keyset SQL must quote JSON-number last-keys for VARCHAR2 columns
    /// so pagination does not ORA-01722 on mixed alphanumeric PK values.
    #[tokio::test]
    async fn oracle_keyset_quotes_numeric_json_last_key() {
        let Some(mut conn) = connect().await else {
            return;
        };
        const TABLE: &str = "DD_KEYSET_PAGE";
        create_varchar_key_table(&mut *conn, TABLE).await;
        let spec = KeysetPageSpec {
            schema: None,
            table: TABLE.into(),
            columns: vec!["XWDM".into(), "BS".into(), "GDDM".into()],
            raw_exprs: false,
            key_columns: vec!["XWDM".into(), "BS".into(), "GDDM".into()],
            string_key: vec![true, true, true],
            range: None,
            last_key: Some(vec![
                serde_json::json!(55958),
                serde_json::json!(-1),
                serde_json::json!("D890216050"),
            ]),
            page_size: 8192,
            filter: None,
            scn: None,
        };
        let sql = conn.dialect().render_keyset_page_sql(&spec);
        assert!(
            sql.contains("NLSSORT('55958','NLS_SORT=BINARY')"),
            "sql={sql}"
        );
        assert!(sql.contains("NLSSORT('-1','NLS_SORT=BINARY')"), "sql={sql}");
        assert!(!sql.contains("NLSSORT(55958,"), "sql={sql}");
        assert!(!sql.contains("NLSSORT(-1,"), "sql={sql}");
        let result = DbConn::query(&mut *conn, &sql)
            .await
            .unwrap_or_else(|e| panic!("quoted keyset SQL failed: {e}; sql={sql}"));
        assert!(
            result.row_count >= 1,
            "expected rows after last_key, got {result:?}"
        );
        drop_table_named(&mut *conn, TABLE).await;
    }

    /// Drive a real page-to-page cursor: last_key comes from the driver row,
    /// not a hand-built JSON number.
    #[tokio::test]
    async fn oracle_keyset_pages_with_driver_last_key() {
        let Some(mut conn) = connect().await else {
            return;
        };
        const TABLE: &str = "DD_KEYSET_CURSOR";
        create_varchar_key_table(&mut *conn, TABLE).await;
        let mut spec = KeysetPageSpec {
            schema: None,
            table: TABLE.into(),
            columns: vec!["XWDM".into(), "BS".into(), "GDDM".into()],
            raw_exprs: false,
            key_columns: vec!["XWDM".into(), "BS".into(), "GDDM".into()],
            string_key: vec![true, true, true],
            range: None,
            last_key: None,
            page_size: 1,
            filter: None,
            scn: None,
        };
        let first_sql = conn.dialect().render_keyset_page_sql(&spec);
        let first = DbConn::query(&mut *conn, &first_sql)
            .await
            .expect("first page");
        assert_eq!(first.row_count, 1);
        spec.last_key = Some(first.rows[0][..3].to_vec());
        let sql = conn.dialect().render_keyset_page_sql(&spec);
        assert!(
            !sql.contains("NLSSORT(47872,") && !sql.contains("NLSSORT(55958,"),
            "driver last_key must stay quoted: sql={sql}"
        );
        let second = DbConn::query(&mut *conn, &sql)
            .await
            .unwrap_or_else(|e| panic!("second page failed: {e}; sql={sql}"));
        assert_eq!(second.row_count, 1);
        match &second.rows[0][0] {
            Value::String(s) => assert_ne!(s, first.rows[0][0].as_str().unwrap_or("")),
            other => panic!("second-page XWDM should be a string, got {other}"),
        }
        drop_table_named(&mut *conn, TABLE).await;
    }

    // oracle-rs 0.1.7：execute 硬编码 prefetch=100、has_more_rows 恒 false、
    // fetch_more 协议损坏（空批 + 服务器断连）。驱动修复后移除 ignore。
    #[tokio::test]
    #[ignore = "oracle-rs 0.1.7 truncates queries at the 100-row prefetch batch; fetch_more is protocol-broken"]
    async fn oracle_query_fetches_beyond_prefetch_batch() {
        let Some(mut conn) = connect().await else {
            return;
        };
        const TABLE: &str = "DD_FETCH_MANY";
        let _ = conn
            .query_drop(&format!(
                "BEGIN EXECUTE IMMEDIATE 'DROP TABLE {TABLE}'; EXCEPTION WHEN OTHERS THEN NULL; END;"
            ))
            .await;
        conn.query_drop(&format!("CREATE TABLE {TABLE} (id NUMBER PRIMARY KEY)"))
            .await
            .expect("create table");
        for base in (0..250).step_by(50) {
            let values: String = (base..base + 50)
                .map(|i| format!("SELECT {i} FROM dual"))
                .collect::<Vec<_>>()
                .join(" UNION ALL ");
            conn.query_drop(&format!("INSERT INTO {TABLE} SELECT * FROM ({values})"))
                .await
                .expect("insert batch");
        }
        let result =
            polar_mysql::backend::DbConn::query(&mut *conn, &format!("SELECT id FROM {TABLE}"))
                .await
                .expect("select all");
        assert_eq!(
            result.row_count, 250,
            "query must drain all fetch batches, not stop at the driver prefetch size"
        );
        let _ = conn
            .query_drop(&format!(
                "BEGIN EXECUTE IMMEDIATE 'DROP TABLE {TABLE}'; EXCEPTION WHEN OTHERS THEN NULL; END;"
            ))
            .await;
    }

    #[tokio::test]
    async fn oracle_native_varchar_digit_stays_json_string() {
        let Some(mut conn) = try_connect_native().await else {
            return;
        };
        const TABLE: &str = "DD_KEYSET_NATIVE";
        create_varchar_key_table(&mut *conn, TABLE).await;
        let result = DbConn::query(
            &mut *conn,
            &format!("SELECT xwdm FROM {TABLE} WHERE xwdm = '55958'"),
        )
        .await
        .expect("select xwdm via oracle_native");
        assert_eq!(result.row_count, 1);
        match &result.rows[0][0] {
            Value::String(s) => assert_eq!(s, "55958"),
            other => panic!("native VARCHAR2 '55958' became {other}, expected JSON string"),
        }
        drop_table_named(&mut *conn, TABLE).await;
    }
}
