// ─── delta-diff checksum: render → execute → five-tuple compare ────────
//
// Executes the order-independent bit-slice checksum SQL (design doc §十)
// produced by Dialect::render_checksum_sql and parses the single result
// row into a ChecksumTuple {count, s1..s4}. Cross-database equality of
// the tuple is the consistency assertion for a shard.

use serde_json::Value;

use crate::backend::{ChecksumSqlSpec, DbConn, DbError};

// ─── ChecksumTuple ─────────────────────────────────────────────────────

/// Bit-slice checksum five-tuple (§十): row count + four u64 slice sums.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChecksumTuple {
    pub count: u64,
    pub s: [u64; 4],
}

impl ChecksumTuple {
    /// Identity tuple of an empty shard (SUM over zero rows is NULL → 0).
    pub(crate) fn zero() -> Self {
        Self {
            count: 0,
            s: [0; 4],
        }
    }

    /// [count, s1, s2, s3, s4] — same shape as tests/delta-diff-verify/expected.json.
    pub(crate) fn as_array(&self) -> [u64; 5] {
        [self.count, self.s[0], self.s[1], self.s[2], self.s[3]]
    }
}

/// Shard-level consistency assertion (§十): identical iff every component
/// matches; the aggregate is order-independent so equality is exact.
pub(crate) fn tuples_equal(a: &ChecksumTuple, b: &ChecksumTuple) -> bool {
    a == b
}

// ─── Execution ─────────────────────────────────────────────────────────

/// Render the checksum SQL via the connection's dialect, execute it, and
/// parse the single result row into a ChecksumTuple.
pub(crate) async fn run_checksum(
    conn: &mut dyn DbConn,
    spec: &ChecksumSqlSpec,
) -> Result<ChecksumTuple, DbError> {
    let sql = conn.dialect().render_checksum_sql(spec);
    run_checksum_sql(conn, &sql).await
}

/// Execute a pre-rendered checksum SQL (none-mode parallel tasks render
/// SQL before spawning, since the dialect lives on the main connection).
pub(crate) async fn run_checksum_sql(
    conn: &mut dyn DbConn,
    sql: &str,
) -> Result<ChecksumTuple, DbError> {
    let result = conn.query(sql).await?;
    let row = result
        .rows
        .first()
        .ok_or_else(|| DbError::query("delta-diff: checksum query returned no rows"))?;
    parse_tuple_row(row)
}

fn parse_tuple_row(row: &[Value]) -> Result<ChecksumTuple, DbError> {
    if row.len() < 5 {
        return Err(DbError::query(format!(
            "delta-diff: checksum row has {} columns, expected 5",
            row.len()
        )));
    }
    Ok(ChecksumTuple {
        count: value_to_u64(&row[0])?,
        s: [
            value_to_u64(&row[1])?,
            value_to_u64(&row[2])?,
            value_to_u64(&row[3])?,
            value_to_u64(&row[4])?,
        ],
    })
}

fn value_to_u64(v: &Value) -> Result<u64, DbError> {
    match v {
        Value::Null => Ok(0),
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok()))
            .or_else(|| {
                n.as_f64()
                    .filter(|f| *f >= 0.0 && f.fract() == 0.0)
                    .map(|f| f as u64)
            })
            .ok_or_else(|| {
                DbError::query(format!("delta-diff: checksum value '{n}' is not a u64"))
            }),
        Value::String(s) => {
            let int_part = s.trim().split('.').next().unwrap_or("");
            int_part.parse::<u64>().map_err(|_| {
                DbError::query(format!("delta-diff: checksum value '{s}' is not a u64"))
            })
        }
        other => Err(DbError::query(format!(
            "delta-diff: unexpected checksum value type: {other}"
        ))),
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mysql::dialect::MySqlDialect;
    use crate::backend::{Dialect, QueryResult};
    use async_trait::async_trait;
    use serde_json::json;

    // ── Mock connection capturing the executed SQL ──

    struct MockChecksumConn {
        dialect: MySqlDialect,
        row: Vec<Value>,
        last_sql: String,
    }

    #[async_trait]
    impl DbConn for MockChecksumConn {
        async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            self.last_sql = sql.to_string();
            Ok(QueryResult {
                columns: vec![
                    "cnt".into(),
                    "s1".into(),
                    "s2".into(),
                    "s3".into(),
                    "s4".into(),
                ],
                rows: vec![self.row.clone()],
                row_count: 1,
            })
        }

        async fn exec(&mut self, _sql: &str, _params: &[Value]) -> Result<QueryResult, DbError> {
            Err(DbError::unsupported("mock: exec not supported"))
        }

        async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
            Err(DbError::unsupported("mock: query_drop not supported"))
        }

        fn dialect(&self) -> &dyn Dialect {
            &self.dialect
        }
    }

    fn spec() -> ChecksumSqlSpec {
        ChecksumSqlSpec {
            schema: Some("verify".into()),
            table: "verify_t".into(),
            key_column: Some("id".into()),
            range: Some((1, 501)),
            bucket: None,
            filter: None,
            scn: None,
            normalized_exprs: vec!["CAST(`id` AS CHAR)".into()],
        }
    }

    // ── Tuple basics ──

    #[test]
    fn zero_tuple_is_empty_shard() {
        let z = ChecksumTuple::zero();
        assert_eq!(z.as_array(), [0, 0, 0, 0, 0]);
        assert!(tuples_equal(&z, &ChecksumTuple::zero()));
    }

    #[test]
    fn tuples_equal_compares_all_components() {
        let a = ChecksumTuple {
            count: 3,
            s: [1, 2, 3, 4],
        };
        let b = ChecksumTuple {
            count: 3,
            s: [1, 2, 3, 4],
        };
        let c = ChecksumTuple {
            count: 3,
            s: [1, 2, 3, 5],
        };
        let d = ChecksumTuple {
            count: 4,
            s: [1, 2, 3, 4],
        };
        assert!(tuples_equal(&a, &b));
        assert!(!tuples_equal(&a, &c));
        assert!(!tuples_equal(&a, &d));
    }

    // ── Value coercion: number and string forms ──

    #[test]
    fn value_to_u64_accepts_number_forms() {
        assert_eq!(value_to_u64(&json!(2000)).unwrap(), 2000);
        assert_eq!(
            value_to_u64(&json!(18446744073709551615u64)).unwrap(),
            u64::MAX
        );
        assert_eq!(value_to_u64(&json!(0)).unwrap(), 0);
    }

    #[test]
    fn value_to_u64_accepts_string_forms() {
        // MySQL DECIMAL arrives as String (mysql/types.rs).
        assert_eq!(
            value_to_u64(&json!("4291226446338")).unwrap(),
            4291226446338
        );
        // DECIMAL may carry a fractional part of zeros.
        assert_eq!(
            value_to_u64(&json!("4291226446338.0000")).unwrap(),
            4291226446338
        );
    }

    #[test]
    fn value_to_u64_null_is_zero() {
        // SUM over an empty set is NULL.
        assert_eq!(value_to_u64(&Value::Null).unwrap(), 0);
    }

    #[test]
    fn value_to_u64_rejects_garbage() {
        assert!(value_to_u64(&json!("abc")).is_err());
        assert!(value_to_u64(&json!(-1)).is_err());
        assert!(value_to_u64(&json!(true)).is_err());
    }

    // ── Row parsing ──

    #[test]
    fn parse_tuple_row_mixed_forms() {
        let row = vec![
            json!(2000),
            json!("4291226446338"),
            json!(4287898926244u64),
            json!("4280383351798.0000"),
            json!(4301484973248u64),
        ];
        let t = parse_tuple_row(&row).unwrap();
        assert_eq!(
            t.as_array(),
            [
                2000,
                4291226446338,
                4287898926244,
                4280383351798,
                4301484973248
            ]
        );
    }

    #[test]
    fn parse_tuple_row_empty_shard() {
        let row = vec![json!(0), Value::Null, Value::Null, Value::Null, Value::Null];
        let t = parse_tuple_row(&row).unwrap();
        assert_eq!(t, ChecksumTuple::zero());
    }

    #[test]
    fn parse_tuple_row_short_row_errors() {
        let row = vec![json!(1), json!(2)];
        assert!(parse_tuple_row(&row).is_err());
    }

    // ── Execution chain: render → execute → parse ──

    #[tokio::test]
    async fn run_checksum_executes_rendered_sql_and_parses_row() {
        let mut conn = MockChecksumConn {
            dialect: MySqlDialect,
            row: vec![
                json!(500),
                json!("1039989922707"),
                json!(1035312664965u64),
                json!("1043926584672"),
                json!(1101813774624u64),
            ],
            last_sql: String::new(),
        };
        let t = run_checksum(&mut conn, &spec()).await.unwrap();
        assert_eq!(
            t.as_array(),
            [
                500,
                1039989922707,
                1035312664965,
                1043926584672,
                1101813774624
            ]
        );
        // The executed SQL must come from Dialect::render_checksum_sql.
        assert!(conn
            .last_sql
            .contains("MD5(CONCAT_WS('#', CAST(`id` AS CHAR)))"));
        assert!(conn.last_sql.contains("FROM `verify`.`verify_t`"));
        assert!(conn.last_sql.contains("`id` >= 1 AND `id` < 501"));
    }

    #[tokio::test]
    async fn run_checksum_no_rows_errors() {
        struct EmptyConn;
        #[async_trait]
        impl DbConn for EmptyConn {
            async fn query(&mut self, _sql: &str) -> Result<QueryResult, DbError> {
                Ok(QueryResult::empty())
            }
            async fn exec(
                &mut self,
                _sql: &str,
                _params: &[Value],
            ) -> Result<QueryResult, DbError> {
                Err(DbError::unsupported("mock"))
            }
            async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
                Err(DbError::unsupported("mock"))
            }
            fn dialect(&self) -> &dyn Dialect {
                &MySqlDialect
            }
        }
        let mut conn = EmptyConn;
        let err = run_checksum(&mut conn, &spec()).await.unwrap_err();
        assert!(err.to_string().contains("no rows"), "{err}");
    }
}

// ─── Integration tests (gated; require live containers) ────────────────
//
// Environment (unset → test skips):
//   DELTA_DIFF_MYSQL_URL    e.g. mysql://root:verify123@127.0.0.1:13306/verify
//   DELTA_DIFF_GAUSSDB_URL  e.g. host=127.0.0.1 port=15432 user=gaussdb
//                              password=Verify@123 dbname=verify
// Target table verify_t is loaded by tests/delta-diff-verify/gen_fixture.py
// (2000-row deterministic fixture). Hard-coded full-table expected values
// come from tests/delta-diff-verify/expected.json.

#[cfg(all(test, feature = "integration"))]
mod integration_tests {
    use super::*;
    use crate::backend::factory::BackendRegistry;
    use crate::backend::mysql::MySqlFactory;
    use crate::delta_diff::metadata::{self, TablePlan};
    use std::sync::Arc;

    #[cfg(feature = "gaussdb")]
    use crate::backend::gaussdb::GaussdbFactory;

    /// python 期望五元组（expected.json full 行，mysql 与 openGauss 一致）
    const EXPECTED_FULL: [u64; 5] = [
        2000,
        4_291_226_446_338,
        4_287_898_926_244,
        4_280_383_351_798,
        4_301_484_973_248,
    ];
    const RANGES: [(i64, i64); 4] = [(1, 501), (501, 1001), (1001, 1501), (1501, 2001)];
    const BUCKETS: [(u64, u64); 3] = [(8, 0), (8, 3), (8, 7)];

    struct Side {
        conn: Box<dyn DbConn + Send>,
        schema: String,
        plan: TablePlan,
    }

    fn registry() -> BackendRegistry {
        let mut r = BackendRegistry::new();
        r.register(Arc::new(MySqlFactory));
        #[cfg(feature = "gaussdb")]
        r.register(Arc::new(GaussdbFactory));
        r
    }

    async fn connect(env: &str, scheme: &str) -> Option<Box<dyn DbConn + Send>> {
        let url = std::env::var(env).ok()?;
        let pool = match registry().connect_with_fallback(scheme, &url, None).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("delta-diff integration: skip {scheme} ({env}): {e}");
                return None;
            }
        };
        Some(match pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("delta-diff integration: skip {scheme} acquire ({env}): {e}");
                return None;
            }
        })
    }

    async fn current_schema(conn: &mut (dyn DbConn + Send)) -> String {
        let sql = match conn.dialect().url_scheme() {
            "mysql" => "SELECT DATABASE()",
            _ => "SELECT current_schema()",
        };
        let r = conn.query(sql).await.expect("current_schema query failed");
        r.rows[0][0].as_str().expect("schema string").to_string()
    }

    async fn setup(scheme: &str, env: &str) -> Option<Side> {
        let mut conn = connect(env, scheme).await?;
        let schema = current_schema(&mut *conn).await;
        let plan = metadata::build_table_plan(&mut *conn, &schema, "verify_t", &[], &[])
            .await
            .expect("build_table_plan failed");
        assert_eq!(plan.key_columns, vec!["id".to_string()]);
        assert_eq!(
            plan.compare_columns.len(),
            7,
            "unexpected exclusions, warnings: {:?}",
            plan.warnings
        );
        assert!(plan.warnings.is_empty(), "warnings: {:?}", plan.warnings);
        Some(Side { conn, schema, plan })
    }

    impl Side {
        async fn checksum(
            &mut self,
            range: Option<(i64, i64)>,
            bucket: Option<(u64, u64)>,
        ) -> ChecksumTuple {
            let spec = ChecksumSqlSpec {
                schema: Some(self.schema.clone()),
                table: "verify_t".to_string(),
                key_column: self.plan.key_columns.first().cloned(),
                range,
                bucket,
                filter: None,
                scn: None,
                normalized_exprs: self
                    .plan
                    .normalized_exprs(self.conn.dialect())
                    .expect("normalized_exprs failed"),
            };
            run_checksum(&mut *self.conn, &spec)
                .await
                .expect("run_checksum failed")
        }
    }

    // ── mysql full table equals python expected ──

    #[tokio::test]
    async fn mysql_full_matches_python_expected() {
        let Some(mut side) = setup("mysql", "DELTA_DIFF_MYSQL_URL").await else {
            return;
        };
        let t = side.checksum(None, None).await;
        assert_eq!(t.as_array(), EXPECTED_FULL);
    }

    // ── cross-db consistency: full / ranges / buckets ──

    #[tokio::test]
    async fn cross_db_full_table_equal() {
        let (Some(mut left), Some(mut right)) = (
            setup("mysql", "DELTA_DIFF_MYSQL_URL").await,
            setup("gaussdb", "DELTA_DIFF_GAUSSDB_URL").await,
        ) else {
            return;
        };
        let l = left.checksum(None, None).await;
        let r = right.checksum(None, None).await;
        assert!(
            tuples_equal(&l, &r),
            "mysql={:?} vs gaussdb={:?}",
            l.as_array(),
            r.as_array()
        );
        assert_eq!(l.as_array(), EXPECTED_FULL);
    }

    #[tokio::test]
    async fn cross_db_id_ranges_equal() {
        let (Some(mut left), Some(mut right)) = (
            setup("mysql", "DELTA_DIFF_MYSQL_URL").await,
            setup("gaussdb", "DELTA_DIFF_GAUSSDB_URL").await,
        ) else {
            return;
        };
        for (lo, hi) in RANGES {
            let l = left.checksum(Some((lo, hi)), None).await;
            let r = right.checksum(Some((lo, hi)), None).await;
            assert!(
                tuples_equal(&l, &r),
                "range [{lo},{hi}) mysql={:?} gaussdb={:?}",
                l.as_array(),
                r.as_array()
            );
        }
    }

    #[tokio::test]
    async fn cross_db_buckets_mod8_equal() {
        let (Some(mut left), Some(mut right)) = (
            setup("mysql", "DELTA_DIFF_MYSQL_URL").await,
            setup("gaussdb", "DELTA_DIFF_GAUSSDB_URL").await,
        ) else {
            return;
        };
        for (m, b) in BUCKETS {
            let l = left.checksum(None, Some((m, b))).await;
            let r = right.checksum(None, Some((m, b))).await;
            assert!(
                tuples_equal(&l, &r),
                "bucket mod={m} b={b} mysql={:?} gaussdb={:?}",
                l.as_array(),
                r.as_array()
            );
        }
    }
}
