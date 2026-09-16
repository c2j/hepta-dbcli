// ─── delta-diff api: CLI/MCP 共用的比对执行入口（§四/§13.1）─────────────
//
// run_diff 接受已就绪的连接与选项，完成：schema 解析 → 计划构建 → 路由 →
// 策略执行 → 断点 finalize。CLI（mod.rs）与 MCP（server.rs delta_diff）共用。

use std::sync::Arc;

use crate::backend::{DbConn, DbPool};
use crate::delta_diff::report::DiffReport;
use crate::delta_diff::{engine, metadata, pairing, strategy};
use crate::delta_diff::{progress, side_schema_from_conn};

pub(crate) struct SideInput {
    pub(crate) pool: Arc<dyn DbPool>,
    pub(crate) conn: Box<dyn DbConn + Send>,
    pub(crate) name: String,
    pub(crate) schema: Option<String>,
    pub(crate) table: String,
    pub(crate) connection_url: String,
}

pub(crate) struct Preflight {
    pub(crate) lplan: metadata::TablePlan,
    pub(crate) rplan: metadata::TablePlan,
    pub(crate) routed: engine::Route,
    pub(crate) paired: crate::delta_diff::pairing::Pairing,
    pub(crate) warnings: Vec<String>,
}

pub(crate) struct PreflightSide<'a> {
    pub(crate) conn: &'a mut (dyn DbConn + Send),
    pub(crate) schema: &'a str,
    pub(crate) table: &'a str,
    pub(crate) connection_url: &'a str,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DiffOptions {
    pub(crate) strategy: Option<crate::delta_diff::cmd::Strategy>,
    pub(crate) iblt_capacity: u64,
    pub(crate) fetch_all_threshold: u64,
    pub(crate) naive_max_rows: u64,
    pub(crate) strict: bool,
    pub(crate) key: Vec<String>,
    pub(crate) columns: Vec<String>,
    pub(crate) filter: Option<String>,
    pub(crate) incremental: Option<(String, String)>,
    pub(crate) bisection_factor: usize,
    pub(crate) bisection_threshold: u64,
    pub(crate) sample_limit: usize,
    pub(crate) threads: usize,
    pub(crate) snapshot: bool,
    pub(crate) recheck: bool,
    pub(crate) checkpoint: Option<String>,
    pub(crate) verbose: bool,
    pub(crate) rtrim_char_columns: bool,
}

pub(crate) async fn run_diff(
    mut left: SideInput,
    mut right: SideInput,
    opts: DiffOptions,
) -> Result<DiffReport, String> {
    let lschema = resolve_schema(
        left.schema.as_deref(),
        &mut *left.conn,
        &left.connection_url,
        &left.name,
    )
    .await?;
    let rschema = resolve_schema(
        right.schema.as_deref(),
        &mut *right.conn,
        &right.connection_url,
        &right.name,
    )
    .await?;

    let Preflight {
        lplan,
        rplan,
        routed,
        paired,
        warnings,
    } = preflight(
        PreflightSide {
            conn: &mut *left.conn,
            schema: &lschema,
            table: &left.table,
            connection_url: &left.connection_url,
        },
        PreflightSide {
            conn: &mut *right.conn,
            schema: &rschema,
            table: &right.table,
            connection_url: &right.connection_url,
        },
        &opts.columns,
        &opts.key,
        opts.strategy,
        !opts.snapshot,
        opts.rtrim_char_columns,
    )
    .await?;
    let (left_key_columns, right_key_columns) = super::paired_side_keys(
        &routed.key_columns,
        &lplan.key_columns,
        &rplan.key_columns,
        paired.left_key_columns,
        paired.right_key_columns,
    )?;

    let checkpoint = match &opts.checkpoint {
        Some(path) => Some(Arc::new(tokio::sync::Mutex::new(
            progress::CheckpointManager::open(path).map_err(|e| e.to_string())?,
        ))),
        None => None,
    };

    let ctx = strategy::DiffContext {
        left: strategy::SideCtx {
            connection_name: left.name.clone(),
            schema: Some(lschema),
            table: left.table.clone(),
            plan: lplan,
        },
        right: strategy::SideCtx {
            connection_name: right.name.clone(),
            schema: Some(rschema),
            table: right.table.clone(),
            plan: rplan,
        },
        left_pool: Arc::clone(&left.pool),
        right_pool: Arc::clone(&right.pool),
        key_column: routed.key_column,
        key_columns: routed.key_columns,
        left_key_columns,
        right_key_columns,
        filter: opts.filter.clone(),
        incremental: opts.incremental.clone(),
        bisection_factor: opts.bisection_factor.max(2),
        bisection_threshold: opts.bisection_threshold.max(1),
        sample_limit: opts.sample_limit,
        threads: opts.threads.max(1),
        consistency: if opts.snapshot {
            strategy::ConsistencyMode::Snapshot
        } else {
            strategy::ConsistencyMode::None
        },
        recheck: opts.recheck,
        route_warnings: warnings,
        checkpoint,
        iblt_capacity: opts.iblt_capacity.max(16),
        fetch_all_threshold: opts.fetch_all_threshold,
        naive_max_rows: opts.naive_max_rows,
        strict: opts.strict,
        scns: std::sync::OnceLock::new(),
        verbose: opts.verbose,
    };

    let report = routed
        .strategy
        .diff(&mut *left.conn, &mut *right.conn, &ctx)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(path) = &opts.checkpoint {
        progress::finalize_path(path).map_err(|e| e.to_string())?;
    }
    Ok(report)
}

pub(crate) async fn preflight(
    left: PreflightSide<'_>,
    right: PreflightSide<'_>,
    columns: &[String],
    key: &[String],
    strategy_hint: Option<crate::delta_diff::cmd::Strategy>,
    _consistency_none: bool,
    rtrim_char_columns: bool,
) -> Result<Preflight, String> {
    pin_session(left.conn, "left").await?;
    pin_session(right.conn, "right").await?;
    let lplan = metadata::build_table_plan(
        left.conn,
        left.schema,
        left.table,
        columns,
        key,
        rtrim_char_columns,
    )
    .await
    .map_err(|e| format!("left plan: {}", e))?;
    let rplan = metadata::build_table_plan(
        right.conn,
        right.schema,
        right.table,
        columns,
        key,
        rtrim_char_columns,
    )
    .await
    .map_err(|e| format!("right plan: {}", e))?;
    let routed = engine::route_plan(
        &lplan,
        &rplan,
        left.connection_url,
        right.connection_url,
        strategy_hint,
    )?;
    let paired = pairing::pair_plans(&lplan, &rplan);
    let mut warnings = routed.warnings.clone();
    warnings.extend(cross_db_column_type_warnings(
        &lplan,
        &rplan,
        &paired,
        rtrim_char_columns,
    ));
    Ok(Preflight {
        lplan,
        rplan,
        routed,
        paired,
        warnings,
    })
}

async fn pin_session(conn: &mut (dyn DbConn + Send), side: &str) -> Result<(), String> {
    for sql in conn.dialect().session_pin_sql() {
        conn.query_drop(&sql)
            .await
            .map_err(|e| format!("{side} session pin failed: {e}"))?;
    }
    Ok(())
}

async fn resolve_schema(
    explicit: Option<&str>,
    conn: &mut (dyn DbConn + Send),
    url: &str,
    name: &str,
) -> Result<String, String> {
    if let Some(s) = explicit {
        return Ok(s.to_string());
    }
    side_schema_from_conn(conn, url, name).await
}

/// Warn when paired cross-database columns have incompatible text normalization.
pub(crate) fn cross_db_column_type_warnings(
    lplan: &metadata::TablePlan,
    rplan: &metadata::TablePlan,
    pairing: &crate::delta_diff::pairing::Pairing,
    rtrim_char_columns: bool,
) -> Vec<String> {
    if lplan.url_scheme == rplan.url_scheme {
        return Vec::new();
    }

    let float_types = [
        "float",
        "double",
        "real",
        "float4",
        "float8",
        "binary_float",
        "binary_double",
    ];
    let date_only = ["date"];
    let date_time = [
        "datetime",
        "timestamp",
        "timestamp without time zone",
        "timestamp with time zone",
        "timestamptz",
    ];
    let fixed_char = ["char", "nchar", "character", "bpchar"];
    let variable_char = ["varchar", "varchar2", "nvarchar2", "character varying"];
    let mut out = Vec::new();

    for (left_index, right_index) in pairing.right_of_left.iter().enumerate() {
        let Some(right_index) = right_index else {
            continue;
        };
        let Some(left) = lplan.norm_specs.get(left_index) else {
            continue;
        };
        let Some(right) = rplan.norm_specs.get(*right_index) else {
            continue;
        };
        let left_base = type_base(&left.data_type);
        let right_base = type_base(&right.data_type);

        if float_types.contains(&left_base.as_str()) || float_types.contains(&right_base.as_str()) {
            out.push(format!(
                "column '{}': {} (left) vs {} (right) — cross-database float text representation \
                 is not portable; results may show false differences (v2.1 §九)",
                left.name, left.data_type, right.data_type
            ));
        } else if (date_only.contains(&left_base.as_str())
            && date_time.contains(&right_base.as_str()))
            || (date_time.contains(&left_base.as_str()) && date_only.contains(&right_base.as_str()))
        {
            out.push(format!(
                "column '{}': {} (left) vs {} (right) — date-only and time-bearing formats differ; \
                 this guarantees a mismatch for every non-null value, even at midnight",
                left.name, left.data_type, right.data_type
            ));
        } else if !rtrim_char_columns
            && ((fixed_char.contains(&left_base.as_str())
                && variable_char.contains(&right_base.as_str()))
                || (variable_char.contains(&left_base.as_str())
                    && fixed_char.contains(&right_base.as_str())))
        {
            out.push(format!(
                "column '{}': {} (left) vs {} (right) — blank-padding differs across engines; \
                 content hashes will show false positives for values shorter than the declared width",
                left.name, left.data_type, right.data_type
            ));
        } else if metadata::TablePlan::is_numeric_type(&left.data_type)
            && metadata::TablePlan::is_numeric_type(&right.data_type)
        {
            let left_scale = declared_scale(&left.data_type);
            let right_scale = declared_scale(&right.data_type);
            let differs = match (left_scale, right_scale) {
                (Some(left), Some(right)) => left != right,
                (Some(scale), None) | (None, Some(scale)) => scale > 0,
                (None, None) => false,
            };
            if differs {
                out.push(format!(
                    "column '{}': {} (left) vs {} (right) — declared numeric scale differs; \
                     content hashes will mismatch on values needing trailing zeros",
                    left.name, left.data_type, right.data_type
                ));
            }
        }
    }
    out
}

fn type_base(data_type: &str) -> String {
    data_type
        .split('(')
        .next()
        .unwrap_or(data_type)
        .trim()
        .to_ascii_lowercase()
}

fn declared_scale(data_type: &str) -> Option<i32> {
    let (_, args) = data_type.split_once('(')?;
    let args = args.split_once(')').map_or(args, |(inside, _)| inside);
    args.split_once(',')?.1.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use crate::backend::ColumnNormSpec;

    use super::*;

    fn plan(scheme: &str, columns: &[(&str, &str)]) -> metadata::TablePlan {
        metadata::TablePlan {
            url_scheme: scheme.to_string(),
            key_columns: Vec::new(),
            compare_columns: columns
                .iter()
                .map(|(name, _)| (*name).to_string())
                .collect(),
            norm_specs: columns
                .iter()
                .map(|(name, data_type)| ColumnNormSpec {
                    name: (*name).to_string(),
                    data_type: (*data_type).to_string(),
                    nullable: true,
                    rtrim_fixed_char: false,
                })
                .collect(),
            warnings: Vec::new(),
        }
    }

    fn warnings(left: metadata::TablePlan, right: metadata::TablePlan) -> Vec<String> {
        let paired = pairing::pair_plans(&left, &right);
        cross_db_column_type_warnings(&left, &right, &paired, false)
    }

    #[test]
    fn warns_when_only_one_numeric_side_declares_positive_scale() {
        let warnings = warnings(
            plan("oracle", &[("V_GHF", "NUMBER(16,2)")]),
            plan("gaussdb", &[("v_ghf", "numeric")]),
        );

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("V_GHF"));
        assert!(warnings[0].contains("NUMBER(16,2)"));
        assert!(warnings[0].contains("numeric"));
        assert!(warnings[0].contains("declared numeric scale differs"));
    }

    #[test]
    fn equal_declared_numeric_scales_are_silent() {
        assert!(warnings(
            plan("oracle", &[("V_GHF", "NUMBER(16,2)")]),
            plan("gaussdb", &[("v_ghf", "numeric(16,2)")]),
        )
        .is_empty());
    }

    #[test]
    fn warns_when_declared_numeric_scales_differ() {
        let warnings = warnings(
            plan("oracle", &[("V_GHF", "NUMBER(16,2)")]),
            plan("gaussdb", &[("v_ghf", "numeric(16,4)")]),
        );

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("declared numeric scale differs"));
    }

    #[test]
    fn warns_for_fixed_and_variable_width_character_pair() {
        let warnings = warnings(
            plan("oracle", &[("V_GDDM", "CHAR(12)")]),
            plan("gaussdb", &[("v_gddm", "character varying(12)")]),
        );

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("V_GDDM"));
        assert!(warnings[0].contains("blank-padding differs"));
    }

    #[test]
    fn rtrim_suppresses_only_padding_warning() {
        let left = plan(
            "oracle",
            &[("V_GDDM", "CHAR(12)"), ("V_GHF", "NUMBER(16,2)")],
        );
        let right = plan(
            "gaussdb",
            &[("v_gddm", "character varying(12)"), ("v_ghf", "numeric")],
        );
        let paired = pairing::pair_plans(&left, &right);
        let warnings = cross_db_column_type_warnings(&left, &right, &paired, true);

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("numeric scale differs"));
    }

    #[test]
    fn fixed_width_character_pair_is_silent() {
        assert!(warnings(
            plan("oracle", &[("V_GDDM", "CHAR(12)")]),
            plan("gaussdb", &[("v_gddm", "character(12)")]),
        )
        .is_empty());
    }

    #[test]
    fn warns_for_date_and_timestamp_pair() {
        let warnings = warnings(
            plan("oracle", &[("CREATED_AT", "DATE")]),
            plan("gaussdb", &[("created_at", "timestamp without time zone")]),
        );

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("guarantees a mismatch for every non-null value"));
    }

    #[test]
    fn case_differing_names_use_pairing_for_type_warning() {
        let warnings = warnings(
            plan("oracle", &[("V_GHF", "NUMBER(16,2)")]),
            plan("gaussdb", &[("v_ghf", "numeric")]),
        );

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("column 'V_GHF'"));
    }

    #[test]
    fn unpaired_column_has_no_type_warning() {
        assert!(warnings(
            plan("oracle", &[("LEFT_ONLY", "NUMBER(16,2)")]),
            plan("gaussdb", &[("right_only", "numeric")]),
        )
        .is_empty());
    }

    #[test]
    fn identical_schemes_suppress_type_warnings() {
        assert!(warnings(
            plan("oracle", &[("V_GHF", "NUMBER(16,2)")]),
            plan("oracle", &[("v_ghf", "numeric")]),
        )
        .is_empty());
    }
}

// ─── DuckDB end-to-end tests (issue #49 phase 2; embedded, no service) ──

#[cfg(all(test, feature = "duckdb"))]
mod duckdb_e2e_tests {
    use super::*;
    use crate::backend::duckdb::DuckDbFactory;
    use crate::backend::{BackendFactory, DbPool};
    use serde_json::json;
    use std::sync::Arc;

    const DDL: &str =
        "CREATE TABLE t_dd (id BIGINT PRIMARY KEY, name VARCHAR, amount DECIMAL(10,2))";
    const ROWS: &str =
        "INSERT INTO t_dd VALUES (1,'a',1.50),(2,'b',2.00),(3,'c',3.25),(4,'d',4.00),(5,'e',5.75)";

    async fn int_key_pool() -> Arc<dyn DbPool> {
        let pool = DuckDbFactory
            .connect("duckdb://:memory:", None)
            .await
            .expect("pool");
        let mut conn = pool.acquire().await.expect("conn");
        conn.query_drop(DDL).await.expect("create");
        conn.query_drop(ROWS).await.expect("insert");
        pool
    }

    async fn side_at(pool: &Arc<dyn DbPool>, name: &str) -> SideInput {
        SideInput {
            pool: Arc::clone(pool),
            conn: pool.acquire().await.expect("conn"),
            name: name.to_string(),
            schema: Some("main".to_string()),
            table: "t_dd".to_string(),
            connection_url: "duckdb://:memory:".to_string(),
        }
    }

    fn opts(strategy: Option<crate::delta_diff::cmd::Strategy>) -> DiffOptions {
        DiffOptions {
            strategy,
            iblt_capacity: 65536,
            // 5 probe rows stay below any full-fetch degrade threshold,
            // so Auto keeps the IBLT route visible in tests.
            fetch_all_threshold: 1,
            naive_max_rows: 200_000,
            strict: false,
            key: vec![],
            columns: vec![],
            filter: None,
            incremental: None,
            bisection_factor: 32,
            bisection_threshold: 16384,
            sample_limit: 1000,
            threads: 4,
            snapshot: false,
            recheck: false,
            checkpoint: None,
            verbose: false,
            rtrim_char_columns: false,
        }
    }

    #[tokio::test]
    async fn duckdb_iblt_identical_then_single_row_diff() {
        let left_pool = int_key_pool().await;
        let right_pool = int_key_pool().await;

        let report = run_diff(
            side_at(&left_pool, "l").await,
            side_at(&right_pool, "r").await,
            opts(None),
        )
        .await
        .expect("identical run");
        assert_eq!(
            report.strategy, "iblt",
            "cross-instance int key routes iblt"
        );
        assert_eq!(report.summary.modified, 0);
        assert_eq!(report.summary.missing_left, 0);
        assert_eq!(report.summary.missing_right, 0);
        assert!(report.sample_diffs.is_empty(), "identical data");

        {
            let mut conn = right_pool.acquire().await.expect("conn");
            conn.query_drop("UPDATE t_dd SET amount = 9.99, name = 'zz' WHERE id = 3")
                .await
                .expect("update");
        }
        let report2 = run_diff(
            side_at(&left_pool, "l").await,
            side_at(&right_pool, "r").await,
            opts(None),
        )
        .await
        .expect("diff run");
        assert_eq!(report2.strategy, "iblt");
        assert_eq!(report2.summary.modified, 1, "one modified row");
        assert_eq!(
            report2.summary.missing_left + report2.summary.missing_right,
            0
        );
        assert!(
            report2.sample_diffs.iter().any(|d| d.key == json!(3)),
            "diff must hit key 3: {:?}",
            report2.sample_diffs
        );
    }

    #[tokio::test]
    async fn duckdb_keyeddiff_for_varchar_key() {
        let pool = DuckDbFactory
            .connect("duckdb://:memory:", None)
            .await
            .expect("pool");
        {
            let mut conn = pool.acquire().await.expect("conn");
            conn.query_drop("CREATE TABLE t_dd (code VARCHAR PRIMARY KEY, v INTEGER)")
                .await
                .expect("create");
            conn.query_drop("INSERT INTO t_dd VALUES ('a',1),('b',2)")
                .await
                .expect("insert");
        }
        let report = run_diff(
            side_at(&pool, "l").await,
            side_at(&pool, "r").await,
            opts(None),
        )
        .await
        .expect("run");
        assert_eq!(report.strategy, "keyeddiff", "varchar key");
        assert_eq!(report.summary.modified, 0);
    }

    #[tokio::test]
    async fn duckdb_bucketdiff_for_keyless_table() {
        let pool = DuckDbFactory
            .connect("duckdb://:memory:", None)
            .await
            .expect("pool");
        {
            let mut conn = pool.acquire().await.expect("conn");
            conn.query_drop("CREATE TABLE t_dd (v INTEGER, tag VARCHAR)")
                .await
                .expect("create");
            conn.query_drop("INSERT INTO t_dd VALUES (1,'x'),(2,'y')")
                .await
                .expect("insert");
        }
        let report = run_diff(
            side_at(&pool, "l").await,
            side_at(&pool, "r").await,
            opts(None),
        )
        .await
        .expect("run");
        assert_eq!(report.strategy, "bucketdiff", "no key");
        assert_eq!(report.summary.modified, 0);
    }
}
