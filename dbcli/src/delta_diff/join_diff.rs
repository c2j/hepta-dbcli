// ─── delta-diff JoinDiffer：同连接 JOIN 比对（v2.1 §6.1）─────────────────
//
// 左右表在同一数据库实例（同一连接 URL）时，用 JOIN + 行哈希比较在库内
// 直接产出差异行：JOIN 的 SELECT 即差异行体（issue #130），不再逐键回表。
// 连接形态按方言分支：MySQL 系（MySQL/PolarDB-X）无 FULL OUTER JOIN，
// 保持 LEFT JOIN UNION ALL；Oracle/GaussDB/DuckDB 用 FULL OUTER JOIN。
// 哈希与引用走方言 row_hash_expr / quote_table / quote_catalog_ident；
// oracle 与 oracle_native 共用同一套 SQL（按 url_scheme 分支）。

use std::time::Instant;

use chrono::Utc;
use serde_json::Value;

use crate::backend::{DbConn, DbError};
use crate::delta_diff::report::{
    DiffReport, DiffRow, DiffStatus, DiffSummary, PerfMetrics, ShardResult, ShardStatus, TableRef,
};
use crate::delta_diff::strategy::{DiffContext, DiffStrategy};

pub(crate) struct JoinDiffer;

#[async_trait::async_trait]
impl DiffStrategy for JoinDiffer {
    fn name(&self) -> &'static str {
        "joindiff"
    }

    async fn diff(
        &self,
        left: &mut (dyn DbConn + Send),
        _right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
    ) -> Result<DiffReport, DbError> {
        let started = Utc::now();
        let t0 = Instant::now();
        let mut queries = 0u64;

        ctx.vlog(format!(
            "[delta-diff] strategy=joindiff consistency={} key={}",
            ctx.consistency.as_str(),
            ctx.key_column
        ));

        let sql = join_diff_sql(ctx, left)?;
        ctx.vlog(format!("[sql] {sql}"));
        let result = left.query(&sql).await?;
        queries += 1;

        let (ltotal, rtotal) = (
            count_rows(left, ctx, true).await?,
            count_rows(left, ctx, false).await?,
        );
        queries += 2;

        let diffs = parse_join_rows(ctx, &result.rows)?;

        let shard = ShardResult {
            shard_id: "join".into(),
            key_range: (Value::Null, Value::Null),
            left_count: ltotal,
            right_count: rtotal,
            diff_count: diffs.len() as u64,
            status: if diffs.is_empty() {
                ShardStatus::Match
            } else {
                ShardStatus::Diff
            },
            duration_ms: t0.elapsed().as_millis() as u64,
        };

        ctx.vlog(format!(
            "[shard] join {} left={} right={} diff={} ({}ms)",
            if diffs.is_empty() { "match" } else { "diff" },
            ltotal,
            rtotal,
            diffs.len(),
            t0.elapsed().as_millis()
        ));

        let mut report = assemble(ctx, vec![shard], diffs, ctx.sample_limit);
        report.started_at = started;
        report.finished_at = Utc::now();
        report.perf.queries_total = queries;
        Ok(report)
    }
}

/// 行布局：[k, in_l, in_r, lh, rh, l_c0..l_c{n}, r_c0..r_c{m}]。
/// 行体列数是「键 + 非键比较列」：键被 --exclude-columns 移出比较列时
/// 仍是 1+n，不能用 norm_specs.len()（#130 review bug 3）。
fn parse_join_rows(ctx: &DiffContext, rows: &[Vec<Value>]) -> Result<Vec<DiffRow>, DbError> {
    let lcount = body_len(ctx, true);
    let rcount = body_len(ctx, false);
    let mut diffs = Vec::with_capacity(rows.len());
    for row in rows {
        let key = row.first().cloned().unwrap_or(Value::Null);
        let present_left = flag(row.get(1));
        let present_right = flag(row.get(2));
        let status = match (present_left, present_right) {
            (true, false) => DiffStatus::MissingRight,
            (false, true) => DiffStatus::MissingLeft,
            _ => DiffStatus::Modified,
        };
        let segment = |start: usize, len: usize| -> Result<Vec<Value>, DbError> {
            row.get(start..start + len)
                .map(<[Value]>::to_vec)
                .ok_or_else(|| {
                    DbError::query(format!(
                        "delta-diff: join projection too short: expected cells [{start}..{}], got {}",
                        start + len,
                        row.len()
                    ))
                })
        };
        let left = if present_left {
            Some(segment(5, lcount)?)
        } else {
            None
        };
        let right = if present_right {
            Some(segment(5 + lcount, rcount)?)
        } else {
            None
        };
        diffs.push(DiffRow {
            key,
            left,
            right,
            status,
            confirmed: true,
        });
    }
    Ok(diffs)
}

/// 单侧行体列数：键 + 非键比较列（与 keyset 行、stamp_columns 同构）。
/// 键被 --exclude-columns 移出比较列时仍是 1+n，不能用 norm_specs.len()
///（#130 review bug 3）。
fn body_len(ctx: &DiffContext, is_left: bool) -> usize {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let side_key = &ctx.side_key_columns(is_left)[0];
    1 + side
        .plan
        .norm_specs
        .iter()
        .filter(|s| &s.name != side_key)
        .count()
}

/// 单侧子查询：`SELECT key AS k, 1 AS p, <row_hash_expr> AS h, key AS c0,
/// <v> AS c1 .. FROM table [WHERE ..]`。行体列 c0.. 与键+非键比较列同序；
/// 常量存在列 p 供外层判定这一侧有没有行（键本身可为 NULL，#130 review）。
fn side_select_sql(
    ctx: &DiffContext,
    is_left: bool,
    dialect: &dyn crate::backend::Dialect,
) -> Result<String, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let side_key = &ctx.side_key_columns(is_left)[0];
    let exprs = side.plan.normalized_exprs(dialect)?;
    let table = dialect.quote_table(side.schema.as_deref(), &side.table);
    // Catalog-physical name: quote without re-folding (issue #116).
    let key = dialect.quote_catalog_ident(side_key);
    let hash = dialect.row_hash_expr(&exprs);
    let mut body_cols = vec![key.clone()];
    for spec in side.plan.norm_specs.iter().filter(|s| &s.name != side_key) {
        body_cols.push(dialect.normalize_expr(spec)?);
    }
    let body = body_cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{c} AS c{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let where_clause = ctx
        .filter
        .as_ref()
        .map(|f| format!(" WHERE ({f})"))
        .unwrap_or_default();
    // NULL 键窗口编号：PARTITION 只分 NULL/非 NULL 两组，NULL 组按行
    // 哈希排序编号——两侧同哈希的 NULL 键按序一一配对（多重集语义），
    // 多余的落 Missing，不会 N×M 笛卡尔（#130 review bug 5）。
    let rn = format!(
        "ROW_NUMBER() OVER (PARTITION BY CASE WHEN {key} IS NULL THEN 1 ELSE 0 END \
         ORDER BY {hash}) AS rn"
    );
    Ok(format!(
        "SELECT {key} AS k, 1 AS p, {hash} AS h, {body}, {rn} FROM {table}{where_clause}"
    ))
}

/// 差异判定 + 行体外层：`WHERE in_r = 0 OR in_l = 0 OR lh != rh`。
/// 存在性标志用常量存在列 p 渲染成 0/1——键本身可为 NULL，用键判存在
/// 会把 NULL 键行误判成"这一侧没有行"（#130 review bug 2）。键等值
/// NULL 安全：两侧都是 NULL 键按 cmp_key 语义配对，由行哈希比出
/// Modified 或相等；行内容比较仍是哈希，不用 IS DISTINCT FROM。
fn join_diff_sql(ctx: &DiffContext, conn: &mut dyn DbConn) -> Result<String, DbError> {
    let dialect = conn.dialect();
    let l = side_select_sql(ctx, true, dialect)?;
    let r = side_select_sql(ctx, false, dialect)?;
    let lcount = body_len(ctx, true);
    let rcount = body_len(ctx, false);
    let outer_cols = {
        let mut cols = vec![
            "k".to_string(),
            "in_l".to_string(),
            "in_r".to_string(),
            "lh".to_string(),
            "rh".to_string(),
        ];
        cols.extend((0..lcount).map(|i| format!("l_c{i}")));
        cols.extend((0..rcount).map(|i| format!("r_c{i}")));
        cols.join(", ")
    };
    let body_aliases = |prefix: &str, count: usize| -> String {
        (0..count)
            .map(|i| format!("{{p}}.c{i} AS {prefix}_c{i}").replace("{p}", prefix))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let l_body = body_aliases("l", lcount);
    let r_body = body_aliases("r", rcount);
    // 键谓词 NULL 安全 + 一一配对：`NULL = NULL` 不成立，双 NULL 键
    // 靠窗口编号 rn 相等配对（非 NULL 组的 rn 不参与），多余 NULL 键
    // 自然落 Missing；行内容比较仍是哈希，不用 IS DISTINCT FROM。
    let on = "ON l.k = r.k OR (l.k IS NULL AND r.k IS NULL AND l.rn = r.rn)";
    let on_reversed = "ON r.k = l.k OR (r.k IS NULL AND l.k IS NULL AND r.rn = l.rn)";

    // 表别名不写 AS：Oracle 的 FROM 子查询别名不允许 AS（ORA-00933），
    // 其余方言省略 AS 同样合法——五方言统一 `) l` 形态。
    let sql = if dialect.url_scheme() == "mysql" {
        let l_body_b = body_aliases("l", lcount);
        let r_body_b = body_aliases("r", rcount);
        format!(
            "SELECT {outer_cols} FROM (\n\
               SELECT l.k AS k, 1 AS in_l, CASE WHEN r.p IS NULL THEN 0 ELSE 1 END AS in_r, \
l.h AS lh, r.h AS rh, {l_body_b}, {r_body_b}\n\
               FROM ({l}) l LEFT JOIN ({r}) r {on}\n\
               UNION ALL\n\
               SELECT r.k AS k, CASE WHEN l.p IS NULL THEN 0 ELSE 1 END AS in_l, 1 AS in_r, \
l.h AS lh, r.h AS rh, {l_body_b}, {r_body_b}\n\
               FROM ({r}) r LEFT JOIN ({l}) l {on_reversed}\n\
               WHERE l.p IS NULL\n\
             ) j\n\
             WHERE in_r = 0 OR in_l = 0 OR lh != rh\n\
             ORDER BY k"
        )
    } else {
        format!(
            "SELECT {outer_cols} FROM (\n\
               SELECT COALESCE(l.k, r.k) AS k, \
CASE WHEN l.p IS NULL THEN 0 ELSE 1 END AS in_l, \
CASE WHEN r.p IS NULL THEN 0 ELSE 1 END AS in_r, \
l.h AS lh, r.h AS rh, {l_body}, {r_body}\n\
               FROM ({l}) l FULL OUTER JOIN ({r}) r {on}\n\
             ) j\n\
             WHERE in_r = 0 OR in_l = 0 OR lh != rh\n\
             ORDER BY k"
        )
    };
    Ok(sql)
}

/// 行数统计（joindiff 同连接，两侧各一次 COUNT(*)）。
async fn count_rows(
    conn: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    is_left: bool,
) -> Result<u64, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let table = conn
        .dialect()
        .quote_table(side.schema.as_deref(), &side.table);
    let where_clause = ctx
        .filter
        .as_ref()
        .map(|f| format!(" WHERE ({f})"))
        .unwrap_or_default();
    let sql = format!("SELECT COUNT(*) FROM {table}{where_clause}");
    ctx.vlog(format!("[sql] {sql}"));
    let r = conn.query(&sql).await?;
    Ok(r.rows
        .first()
        .and_then(|row| row.first())
        .and_then(|v| match v {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .unwrap_or(0))
}

fn flag(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0).unwrap_or(false),
        Some(Value::Bool(b)) => *b,
        _ => false,
    }
}

fn assemble(
    ctx: &DiffContext,
    shards: Vec<ShardResult>,
    diff_rows: Vec<DiffRow>,
    _sample_limit: usize,
) -> DiffReport {
    let mut summary = DiffSummary {
        left_total: shards.iter().map(|s| s.left_count).sum(),
        right_total: shards.iter().map(|s| s.right_count).sum(),
        ..Default::default()
    };
    for d in &diff_rows {
        match d.status {
            DiffStatus::MissingLeft => summary.missing_left += 1,
            DiffStatus::MissingRight => summary.missing_right += 1,
            DiffStatus::Modified => summary.modified += 1,
        }
    }
    let total = summary.left_total.max(summary.right_total);
    summary.diff_rate = if total > 0 {
        (summary.missing_left + summary.missing_right + summary.modified) as f64 / total as f64
    } else {
        0.0
    };
    let mut report = DiffReport {
        started_at: Utc::now(),
        finished_at: Utc::now(),
        left: TableRef {
            connection: ctx.left.connection_name.clone(),
            schema: ctx.left.schema.clone(),
            table: ctx.left.table.clone(),
        },
        right: TableRef {
            connection: ctx.right.connection_name.clone(),
            schema: ctx.right.schema.clone(),
            table: ctx.right.table.clone(),
        },
        strategy: "joindiff".into(),
        consistency: ctx.consistency.as_str().into(),
        hash_algorithm: "md5".into(),
        summary,
        perf: PerfMetrics::default(),
        shards,
        sample_diffs: diff_rows,
        warnings: ctx
            .left
            .plan
            .warnings
            .iter()
            .chain(ctx.right.plan.warnings.iter())
            .chain(ctx.route_warnings.iter())
            .cloned()
            .collect(),
        row_payload: crate::delta_diff::report::RowPayload::Columns,
        key_columns: vec![],
        value_columns: vec![],
        column_data_types: vec![],
        ident_quote: '"',
        ident_scheme: String::new(),
        backslash_escape: false,
        modified_columns: None,
        left_column_names: None,
        right_column_names: None,
    };
    crate::delta_diff::report::stamp_columns_from_plan(&mut report, &ctx.left.plan);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mysql::dialect::MySqlDialect;
    use crate::backend::{ColumnNormSpec, Dialect, QueryResult};
    use crate::delta_diff::metadata::TablePlan;
    use crate::delta_diff::strategy::{ConsistencyMode, SideCtx};
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::VecDeque;

    // ── 共享构造 ────────────────────────────────────────────────────────

    fn plan() -> TablePlan {
        plan_typed("bigint", "int")
    }

    fn plan_typed(key_ty: &str, v_ty: &str) -> TablePlan {
        TablePlan {
            aux: Default::default(),
            key_unique: false,
            url_scheme: "mysql".into(),
            key_columns: vec!["id".into()],
            compare_columns: vec!["id".into(), "v".into()],
            norm_specs: vec![
                ColumnNormSpec {
                    name: "id".into(),
                    data_type: key_ty.to_string(),
                    nullable: false,
                    rtrim_fixed_char: false,
                },
                ColumnNormSpec {
                    name: "v".into(),
                    data_type: v_ty.to_string(),
                    nullable: true,
                    rtrim_fixed_char: false,
                },
            ],
            warnings: vec![],
            key_specs: vec![],
        }
    }

    fn ctx() -> DiffContext {
        ctx_with_plan(plan())
    }

    fn ctx_with_plan(p: TablePlan) -> DiffContext {
        DiffContext {
            left: SideCtx {
                connection_name: "x".into(),
                schema: Some("s".into()),
                table: "t".into(),
                plan: p.clone(),
            },
            right: SideCtx {
                connection_name: "x".into(),
                schema: Some("s".into()),
                table: "t".into(),
                plan: p,
            },
            left_pool: dummy_pool(),
            right_pool: dummy_pool(),
            key_column: "id".into(),
            key_columns: vec!["id".into()],
            left_key_columns: vec!["id".into()],
            right_key_columns: vec!["id".into()],
            filter: None,
            incremental: None,
            bisection_factor: 32,
            bisection_threshold: 16_384,
            sample_limit: 20,
            threads: 1,
            consistency: ConsistencyMode::Snapshot,
            recheck: true,
            route_warnings: vec![],
            checkpoint: None,
            iblt_capacity: 65_536,
            fetch_all_threshold: 4096,
            naive_max_rows: 10_000,
            strict: false,
            summary_only: false,
            same_connection: true,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        }
    }

    fn dummy_pool() -> std::sync::Arc<dyn crate::backend::DbPool> {
        struct P;
        #[async_trait]
        impl crate::backend::DbPool for P {
            async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
                Err(DbError::unsupported("dummy"))
            }
        }
        std::sync::Arc::new(P)
    }

    /// 只提供方言的渲染用 mock（join_diff_sql 只调 dialect()）。
    struct DialectOnly<D> {
        dialect: D,
    }

    #[async_trait]
    impl<D: Dialect + Send + Sync> DbConn for DialectOnly<D> {
        async fn query(&mut self, _sql: &str) -> Result<QueryResult, DbError> {
            Err(DbError::unsupported("render-only mock"))
        }
        async fn exec(&mut self, _sql: &str, _params: &[Value]) -> Result<QueryResult, DbError> {
            Err(DbError::unsupported("render-only mock"))
        }
        async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
            Err(DbError::unsupported("render-only mock"))
        }
        fn dialect(&self) -> &dyn Dialect {
            &self.dialect
        }
    }

    // ── SQL 形态（issue #130 验收 #8 + #130 review NULL 键修复）────────

    /// NULL 键正确性（#130 review bug 2/3）的公共断言：
    /// - 存在性判定必须用常量存在列 p，而不是键是否为 NULL；
    /// - 键等值必须 NULL 安全（两个 NULL 键按 cmp_key 语义配对）。
    fn assert_null_safe_shape(name: &str, sql: &str) {
        assert!(
            sql.contains("CASE WHEN l.p IS NULL THEN 0 ELSE 1 END AS in_l")
                && sql.contains("CASE WHEN r.p IS NULL THEN 0 ELSE 1 END AS in_r"),
            "{name}: existence flags must test the presence column, not the key: {sql}"
        );
        assert!(
            sql.contains("ON l.k = r.k OR (l.k IS NULL AND r.k IS NULL AND l.rn = r.rn)"),
            "{name}: NULL keys must pair 1:1 by window number, not a cartesian OR: {sql}"
        );
        assert!(
            sql.contains("ROW_NUMBER() OVER"),
            "{name}: NULL-key rows must be numbered for 1:1 pairing: {sql}"
        );
        assert!(
            !sql.contains("l.k IS NULL THEN 0 ELSE 1 END AS in_l")
                && !sql.contains("r.k IS NULL THEN 0 ELSE 1 END AS in_r"),
            "{name}: flags must not be derived from key nullability: {sql}"
        );
    }

    #[test]
    fn mysql_keeps_left_join_union_all_shape() {
        let mut conn = DialectOnly {
            dialect: MySqlDialect,
        };
        let sql = join_diff_sql(&ctx(), &mut conn).unwrap();
        assert!(sql.contains("LEFT JOIN"), "mysql: {sql}");
        assert!(sql.contains("UNION ALL"), "mysql: {sql}");
        assert!(!sql.contains("FULL OUTER JOIN"), "mysql: {sql}");
        assert!(
            sql.contains("MD5(CONCAT_WS('#'"),
            "hash must come from row_hash_expr: {sql}"
        );
        assert!(
            sql.contains("CASE WHEN"),
            "existence flags must render 0/1 via CASE, not bare boolean: {sql}"
        );
        assert!(
            sql.contains("WHERE in_r = 0 OR in_l = 0 OR lh != rh"),
            "diff predicate compares 0/1 flags: {sql}"
        );
        assert_null_safe_shape("mysql", &sql);
    }

    #[test]
    #[cfg(feature = "gaussdb")]
    fn gaussdb_uses_full_outer_join_with_case_flags() {
        use crate::backend::gaussdb::GaussdbDialect;
        let mut conn = DialectOnly {
            dialect: GaussdbDialect,
        };
        let sql = join_diff_sql(&ctx(), &mut conn).unwrap();
        assert!(sql.contains("FULL OUTER JOIN"), "gaussdb: {sql}");
        assert!(!sql.contains("UNION ALL"), "gaussdb: {sql}");
        assert!(sql.contains("CASE WHEN"), "gaussdb: {sql}");
        assert!(
            !sql.contains("(r.k IS NOT NULL) AS in_r")
                && !sql.contains("(l.k IS NOT NULL) AS in_l"),
            "no bare-boolean flags on gaussdb: {sql}"
        );
        assert!(
            sql.contains("COALESCE(l.k, r.k) AS k"),
            "full outer join needs coalesced key: {sql}"
        );
        assert!(
            !sql.contains("CONCAT_WS"),
            "hash must come from row_hash_expr, not MySQL CONCAT_WS: {sql}"
        );
        assert_null_safe_shape("gaussdb", &sql);
    }

    #[test]
    #[cfg(feature = "duckdb")]
    fn duckdb_uses_full_outer_join_with_case_flags() {
        use crate::backend::duckdb::dialect::DuckDbDialect;
        let mut conn = DialectOnly {
            dialect: DuckDbDialect,
        };
        // DuckDB 规则表是 bigint/integer，没有 INT 缩写。
        let sql =
            join_diff_sql(&ctx_with_plan(plan_typed("bigint", "integer")), &mut conn).unwrap();
        assert!(sql.contains("FULL OUTER JOIN"), "duckdb: {sql}");
        assert!(sql.contains("CASE WHEN"), "duckdb: {sql}");
        assert!(sql.contains("COALESCE(l.k, r.k) AS k"), "duckdb: {sql}");
        assert!(!sql.contains("CONCAT_WS"), "duckdb: {sql}");
        assert_null_safe_shape("duckdb", &sql);
    }

    #[test]
    #[cfg(feature = "oracle")]
    fn oracle_uses_full_outer_join_without_as_table_alias() {
        // oracle 与 oracle_native 共用同一套 Oracle SQL（issue #130）。
        use crate::backend::oracle::dialect::OracleDialect as OracleRsDialect;
        use crate::backend::oracle_native::dialect::OracleDialect as OracleNativeDialect;
        let assert_oracle_shape = |name: &str, sql: &str| {
            assert!(sql.contains("FULL OUTER JOIN"), "{name}: {sql}");
            // 表别名紧跟子查询右括号；列别名（如 `... 'MD5') AS h`、
            // 窗口 `...) AS rn`）合法，断言需带词尾边界。
            assert!(
                !sql.contains(") AS l ") && !sql.contains(") AS r ") && !sql.contains(") AS j"),
                "oracle table aliases must not use AS (ORA-00933): {sql}"
            );
            assert!(
                sql.contains("STANDARD_HASH") || sql.contains("DBMS_CRYPTO.HASH"),
                "{name}: hash must come from row_hash_expr: {sql}"
            );
            assert!(!sql.contains("CONCAT_WS"), "{name}: {sql}");
            assert!(sql.contains("CASE WHEN"), "{name}: {sql}");
            assert!(sql.contains("COALESCE(l.k, r.k) AS k"), "{name}: {sql}");
            assert_null_safe_shape(name, sql);
        };
        {
            // Oracle 无 BIGINT/INT：类型用 normalize 规则表内的 INTEGER。
            let oracle_plan = plan_typed("INTEGER", "INTEGER");
            let mut conn = DialectOnly {
                dialect: OracleRsDialect::new(),
            };
            let sql = join_diff_sql(&ctx_with_plan(oracle_plan), &mut conn).unwrap();
            assert_oracle_shape("oracle-rs", &sql);
        }
        {
            let oracle_plan = plan_typed("INTEGER", "INTEGER");
            let mut conn = DialectOnly {
                dialect: OracleNativeDialect::new(),
            };
            let sql = join_diff_sql(&ctx_with_plan(oracle_plan), &mut conn).unwrap();
            assert_oracle_shape("oracle-native", &sql);
        }
    }

    // ── 查询数与行体来源（issue #130 验收 #7）──────────────────────────

    /// 脚本 mock：JOIN 结果 + 2 条 COUNT；脚本耗尽即报错（多发的
    /// 点查会立刻暴露）。
    struct ScriptedConn {
        responses: VecDeque<QueryResult>,
        sqls: Vec<String>,
        dialect: MySqlDialect,
    }

    #[async_trait]
    impl DbConn for ScriptedConn {
        async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            self.sqls.push(sql.to_string());
            self.responses
                .pop_front()
                .ok_or_else(|| DbError::query("mock: no scripted response"))
        }
        async fn exec(&mut self, _sql: &str, _params: &[Value]) -> Result<QueryResult, DbError> {
            Err(DbError::unsupported("mock"))
        }
        async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
            Err(DbError::unsupported("mock"))
        }
        fn dialect(&self) -> &dyn Dialect {
            &self.dialect
        }
    }

    fn join_result(rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: vec![
                "k".into(),
                "in_l".into(),
                "in_r".into(),
                "lh".into(),
                "rh".into(),
                "l_id".into(),
                "l_v".into(),
                "r_id".into(),
                "r_v".into(),
            ],
            row_count: rows.len(),
            rows,
            rows_affected: None,
        }
    }

    fn count_result(n: u64) -> QueryResult {
        QueryResult {
            columns: vec!["COUNT(*)".into()],
            rows: vec![vec![json!(n)]],
            row_count: 1,
            rows_affected: None,
        }
    }

    #[tokio::test]
    async fn join_query_count_has_no_point_lookups() {
        // 行体必须来自 JOIN 投影；查询数 = 1 JOIN + 2 COUNT = 3，
        // 不含 2×差异行数的点查。
        let mut lconn = ScriptedConn {
            responses: VecDeque::from([
                join_result(vec![
                    // key 1: 左有右无（行体来自 l 段）
                    vec![
                        json!(1),
                        json!(1),
                        json!(0),
                        json!("h1"),
                        Value::Null,
                        json!(1),
                        json!("lv"),
                        Value::Null,
                        Value::Null,
                    ],
                    // key 2: 两侧都有、哈希不同（Modified，两侧行体都有）
                    vec![
                        json!(2),
                        json!(1),
                        json!(1),
                        json!("ha"),
                        json!("hb"),
                        json!(2),
                        json!("l2"),
                        json!(2),
                        json!("r2"),
                    ],
                ]),
                count_result(2),
                count_result(2),
            ]),
            sqls: Vec::new(),
            dialect: MySqlDialect,
        };
        let mut rconn = ScriptedConn {
            responses: VecDeque::new(),
            sqls: Vec::new(),
            dialect: MySqlDialect,
        };
        let report = JoinDiffer
            .diff(&mut lconn, &mut rconn, &ctx())
            .await
            .unwrap();

        assert_eq!(
            report.perf.queries_total, 3,
            "1 JOIN + 2 COUNT, no point lookups"
        );
        assert_eq!(report.summary.missing_right, 1);
        assert_eq!(report.summary.modified, 1);
        let d1 = report
            .sample_diffs
            .iter()
            .find(|d| d.key == json!(1))
            .unwrap();
        assert_eq!(d1.status, DiffStatus::MissingRight);
        assert_eq!(
            d1.left,
            Some(vec![json!(1), json!("lv")]),
            "body from JOIN l segment"
        );
        assert_eq!(d1.right, None);
        let d2 = report
            .sample_diffs
            .iter()
            .find(|d| d.key == json!(2))
            .unwrap();
        assert_eq!(d2.status, DiffStatus::Modified);
        assert_eq!(d2.left, Some(vec![json!(2), json!("l2")]));
        assert_eq!(
            d2.right,
            Some(vec![json!(2), json!("r2")]),
            "body from JOIN r segment"
        );
    }

    #[test]
    fn parse_keeps_full_body_when_key_is_excluded_from_compare_columns() {
        // #130 review bug 3：--exclude-columns 把键移出比较列后，子查询
        // 行体仍是 [键, 全部比较列]（与 keyset 行/stamp_columns 一致）。
        // 投影别名与切片长度必须用实际行体列数（1+非键数），
        // 不能用 norm_specs.len()。
        let mut p = plan();
        p.norm_specs.retain(|s| s.name != "id");
        p.key_specs = vec![ColumnNormSpec {
            name: "id".into(),
            data_type: "bigint".into(),
            nullable: false,
            rtrim_fixed_char: false,
        }];
        let c = ctx_with_plan(p);

        let mut conn = DialectOnly {
            dialect: MySqlDialect,
        };
        let sql = join_diff_sql(&c, &mut conn).unwrap();
        assert!(
            sql.contains("l.c1 AS l_c1") && sql.contains("r.c1 AS r_c1"),
            "both body columns must survive the projection: {sql}"
        );

        // 行布局 [k, in_l, in_r, lh, rh, l_id, l_v, r_id, r_v]。
        let rows = vec![vec![
            json!(1),
            json!(1),
            json!(1),
            json!("ha"),
            json!("hb"),
            json!(1),
            json!("lv"),
            json!(1),
            json!("rv"),
        ]];
        let diffs = parse_join_rows(&c, &rows).unwrap();
        assert_eq!(diffs[0].status, DiffStatus::Modified);
        assert_eq!(
            diffs[0].left,
            Some(vec![json!(1), json!("lv")]),
            "left body = key + compare column"
        );
        assert_eq!(
            diffs[0].right,
            Some(vec![json!(1), json!("rv")]),
            "right body = key + compare column"
        );
    }

    #[tokio::test]
    async fn join_zero_diff_is_single_join_plus_counts() {
        let mut lconn = ScriptedConn {
            responses: VecDeque::from([join_result(vec![]), count_result(5), count_result(5)]),
            sqls: Vec::new(),
            dialect: MySqlDialect,
        };
        let mut rconn = ScriptedConn {
            responses: VecDeque::new(),
            sqls: Vec::new(),
            dialect: MySqlDialect,
        };
        let report = JoinDiffer
            .diff(&mut lconn, &mut rconn, &ctx())
            .await
            .unwrap();

        assert_eq!(
            report.perf.queries_total, 3,
            "one JOIN + counts, not 32 segments"
        );
        assert_eq!(report.summary.missing_left, 0);
        assert_eq!(report.summary.missing_right, 0);
        assert_eq!(report.summary.modified, 0);
        assert_eq!(report.summary.diff_rate, 0.0);
        assert!(report.sample_diffs.is_empty());
        assert_eq!(report.shards.len(), 1);
        assert_eq!(report.shards[0].status, ShardStatus::Match);
    }
}
