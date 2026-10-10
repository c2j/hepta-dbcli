// ─── delta-diff recheck: 差异行二次复核（v2.1 §8.3）─────────────────────
//
// 行级 diff 产出的差异，在快照提交后按批 `key IN (...)` 向两侧各发一次
// 当前读查询：复核一致 → 比对窗口内的并发写入（伪差异），confirmed=false
// 且不计入统计；复核仍不一致 → confirmed=true。批大小 = 1000（Oracle IN
// 列表上限）；查询量随差异键按批增长，与差异行数解耦（issue #130）。
// 复核是提交后的当前读：任何情况下不得携带 `AS OF SCN`。

use std::collections::HashMap;

use serde_json::Value;

use crate::backend::{DbConn, DbError};
use crate::delta_diff::report::DiffRow;
use crate::delta_diff::strategy::DiffContext;

/// 每条 `IN (...)` 语句覆盖的键数上限（Oracle 表达式列表上限）。
pub(crate) const RECHECK_IN_BATCH: usize = 1000;

/// 复核结果：确认仍存在的差异数 + 本次复核发出的查询数。
pub(crate) struct RecheckOutcome {
    pub(crate) confirmed: u64,
    pub(crate) queries: u64,
}

/// 按批复核差异键；返回确认计数与查询数。
pub(crate) async fn recheck_diffs(
    left: &mut (dyn DbConn + Send),
    right: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    diffs: &mut [DiffRow],
) -> Result<RecheckOutcome, DbError> {
    // 键不可解析为 i64 的差异无法点查复核：维持既有豁免（视为确认）。
    let mut keys: Vec<i64> = Vec::new();
    let mut pre_confirmed = 0u64;
    for d in diffs.iter_mut() {
        match diff_key_to_i64(&d.key) {
            Some(k) => keys.push(k),
            None => {
                d.confirmed = true;
                pre_confirmed += 1;
            }
        }
    }
    keys.sort_unstable();
    keys.dedup();
    if keys.is_empty() {
        return Ok(RecheckOutcome {
            confirmed: pre_confirmed,
            queries: 0,
        });
    }

    let lrender = side_render(ctx, true, left.dialect())?;
    let rrender = side_render(ctx, false, right.dialect())?;

    let mut lrows: HashMap<i64, Vec<Value>> = HashMap::new();
    let mut rrows: HashMap<i64, Vec<Value>> = HashMap::new();
    let mut queries = 0u64;
    for batch in keys.chunks(RECHECK_IN_BATCH) {
        let lsql = render_key_in_sql(&lrender, batch);
        let rsql = render_key_in_sql(&rrender, batch);
        ctx.vlog(format!("[sql:left] {lsql}"));
        ctx.vlog(format!("[sql:right] {rsql}"));
        let lres = left.query(&lsql).await?;
        let rres = right.query(&rsql).await?;
        queries += 2;
        collect_rows(lres.rows, &mut lrows);
        collect_rows(rres.rows, &mut rrows);
    }

    let mut confirmed = pre_confirmed;
    for d in diffs.iter_mut() {
        let Some(k) = diff_key_to_i64(&d.key) else {
            continue;
        };
        let now_equal = match (lrows.get(&k), rrows.get(&k)) {
            (Some(l), Some(r)) => l[1..] == r[1..],
            (None, None) => true,
            _ => false,
        };
        d.confirmed = !now_equal;
        if d.confirmed {
            confirmed += 1;
        }
    }
    Ok(RecheckOutcome { confirmed, queries })
}

fn collect_rows(rows: Vec<Vec<Value>>, into: &mut HashMap<i64, Vec<Value>>) {
    for row in rows {
        if let Some(k) = row.first().and_then(row_cell_to_i64) {
            into.insert(k, row);
        }
    }
}

/// DiffRow.key 的解析口径与旧实现逐字一致：Number 取 i64，String trim 后解析。
fn diff_key_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 结果行键单元的宽容解析（驱动可能回传 "5" / "5.0" / DECIMAL 字符串）。
fn row_cell_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().split('.').next()?.parse().ok(),
        _ => None,
    }
}

// ─── 批量 IN 查询渲染（delta_diff 域内；不改 backend trait）────────────

struct SideRender {
    table: String,
    key_expr: String,
    value_exprs: Vec<String>,
    filter: Option<String>,
}

fn side_render(
    ctx: &DiffContext,
    is_left: bool,
    dialect: &dyn crate::backend::Dialect,
) -> Result<SideRender, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let side_key = &ctx.side_key_columns(is_left)[0];
    let table = dialect.quote_table(side.schema.as_deref(), &side.table);
    // Catalog-physical name: quote without re-folding (issue #116).
    let key_expr = dialect.quote_catalog_ident(side_key);
    let mut value_exprs = Vec::new();
    for spec in side.plan.norm_specs.iter().filter(|s| &s.name != side_key) {
        value_exprs.push(dialect.normalize_expr(spec)?);
    }
    Ok(SideRender {
        table,
        key_expr,
        value_exprs,
        filter: crate::delta_diff::strategy::side_filter(ctx, dialect.url_scheme()),
    })
}

/// `SELECT key, <compare cols> FROM table WHERE [filter AND] key IN (...)`。
///
/// 复核是提交后的当前读（§8.3）：这里**绝不**渲染 `AS OF SCN`——快照
/// SCN 指向比对窗口自身，用它复核恒等于复读快照，伪差异过滤失效。
fn render_key_in_sql(r: &SideRender, keys: &[i64]) -> String {
    let list = keys
        .iter()
        .map(|k| k.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut conds = Vec::new();
    if let Some(f) = &r.filter {
        conds.push(format!("({f})"));
    }
    conds.push(format!("{} IN ({list})", r.key_expr));
    let mut cols = vec![r.key_expr.clone()];
    cols.extend(r.value_exprs.iter().cloned());
    format!(
        "SELECT {} FROM {} WHERE {}",
        cols.join(", "),
        r.table,
        conds.join(" AND ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mysql::dialect::MySqlDialect;
    use crate::backend::{ColumnNormSpec, DbPool, Dialect, QueryResult};
    use crate::delta_diff::metadata::TablePlan;
    use crate::delta_diff::report::DiffStatus;
    use crate::delta_diff::strategy::{ConsistencyMode, SideCtx};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    type ValueFn = Arc<dyn Fn(i64) -> Option<Vec<Value>> + Send + Sync>;

    // ── mock：记录 SQL、按脚本/按键供数 ─────────────────────────────────

    /// 每次查询弹出一个脚本响应；SQL 全量留痕供断言。
    struct RecordingConn {
        responses: Mutex<VecDeque<QueryResult>>,
        sqls: Arc<Mutex<Vec<String>>>,
        dialect: MySqlDialect,
    }

    impl RecordingConn {
        fn empty_result() -> QueryResult {
            QueryResult {
                columns: vec!["id".into(), "v".into()],
                row_count: 0,
                rows: vec![],
                rows_affected: None,
            }
        }

        fn scripted(n: usize) -> (Self, Arc<Mutex<Vec<String>>>) {
            let sqls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    responses: Mutex::new(VecDeque::from(vec![Self::empty_result(); n])),
                    sqls: Arc::clone(&sqls),
                    dialect: MySqlDialect,
                },
                sqls,
            )
        }
    }

    #[async_trait]
    impl DbConn for RecordingConn {
        async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            self.sqls.lock().unwrap().push(sql.to_string());
            self.responses
                .lock()
                .unwrap()
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

    /// 解析 SQL 中 IN 列表并按 key 动态供数的 mock。
    struct KeyedConn {
        sqls: Arc<Mutex<Vec<String>>>,
        value_for: ValueFn,
        dialect: MySqlDialect,
    }

    impl KeyedConn {
        fn new(sqls: Arc<Mutex<Vec<String>>>, value_for: ValueFn) -> Self {
            Self {
                sqls,
                value_for,
                dialect: MySqlDialect,
            }
        }
    }

    #[async_trait]
    impl DbConn for KeyedConn {
        async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            self.sqls.lock().unwrap().push(sql.to_string());
            let rows: Vec<Vec<Value>> = in_list_keys(sql)
                .into_iter()
                .filter_map(|k| (self.value_for)(k))
                .collect();
            Ok(QueryResult {
                columns: vec!["id".into(), "v".into()],
                row_count: rows.len(),
                rows,
                rows_affected: None,
            })
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

    /// 提取 SQL 中 `IN (` 之后的整数列表。
    fn in_list_keys(sql: &str) -> Vec<i64> {
        let Some(pos) = sql.find(" IN (") else {
            return vec![];
        };
        let tail = &sql[pos + 5..];
        let end = tail.find(')').unwrap_or(tail.len());
        tail[..end]
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect()
    }

    // ── ctx 构造 ────────────────────────────────────────────────────────

    fn plan() -> TablePlan {
        TablePlan {
            aux: Default::default(),
            key_unique: false,
            url_scheme: "mysql".into(),
            key_columns: vec!["id".into()],
            compare_columns: vec!["id".into(), "v".into()],
            norm_specs: vec![
                ColumnNormSpec {
                    name: "id".into(),
                    data_type: "bigint".into(),
                    nullable: false,
                    rtrim_fixed_char: false,
                },
                ColumnNormSpec {
                    name: "v".into(),
                    data_type: "int".into(),
                    nullable: true,
                    rtrim_fixed_char: false,
                },
            ],
            warnings: vec![],
            key_specs: vec![],
        }
    }

    fn side() -> SideCtx {
        SideCtx {
            connection_name: "x".into(),
            schema: Some("s".into()),
            table: "t".into(),
            plan: plan(),
        }
    }

    fn dummy_pool() -> Arc<dyn DbPool> {
        struct P;
        #[async_trait]
        impl DbPool for P {
            async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
                Err(DbError::unsupported("dummy"))
            }
        }
        Arc::new(P)
    }

    fn ctx() -> DiffContext {
        DiffContext {
            left: side(),
            right: side(),
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
            same_connection: false,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        }
    }

    fn diff_row(key: i64) -> DiffRow {
        DiffRow {
            key: Value::from(key),
            left: Some(vec![Value::from(key), Value::from("old")]),
            right: Some(vec![Value::from(key), Value::from("new")]),
            status: DiffStatus::Modified,
            confirmed: true,
        }
    }

    fn row_for(key: i64, v: &str) -> Vec<Value> {
        vec![Value::from(key), Value::from(v)]
    }

    fn const_rows(v: &'static str) -> ValueFn {
        Arc::new(move |k| Some(row_for(k, v)))
    }

    // ── 行为测试 ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn recheck_batches_keys_into_in_lists() {
        // 2700 个键 → ceil(2700/1000)=3 批 × 2 侧 = 6 条查询；
        // 两侧都查不到行 → 全部判"已一致"，0 确认。
        let keys: Vec<i64> = (1..=2700).collect();
        let mut diffs: Vec<DiffRow> = keys.iter().map(|&k| diff_row(k)).collect();
        let (mut left, left_sqls) = RecordingConn::scripted(3);
        let (mut right, right_sqls) = RecordingConn::scripted(3);

        let out = recheck_diffs(&mut left, &mut right, &ctx(), &mut diffs)
            .await
            .unwrap();

        assert_eq!(out.queries, 6, "3 batches x 2 sides");
        assert_eq!(out.confirmed, 0, "missing on both sides means now equal");
        assert!(diffs.iter().all(|d| !d.confirmed));
        for sqls in [&left_sqls, &right_sqls] {
            let sqls = sqls.lock().unwrap();
            assert_eq!(sqls.len(), 3);
            assert!(sqls.iter().all(|s| s.contains(" IN (")));
            let mut seen: Vec<i64> = sqls.iter().flat_map(|s| in_list_keys(s)).collect();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen, keys, "every key queried exactly once");
        }
    }

    #[tokio::test]
    async fn recheck_batched_current_read_drops_transient_diffs() {
        // 键 2 提交后两侧一致（伪差异，剔除）；键 1、3 仍不同（确认）。
        let mut diffs = vec![diff_row(1), diff_row(2), diff_row(3)];
        let left_sqls = Arc::new(Mutex::new(Vec::new()));
        let right_sqls = Arc::new(Mutex::new(Vec::new()));
        let mut left = KeyedConn::new(Arc::clone(&left_sqls), const_rows("same-left"));
        let right_value: ValueFn = Arc::new(|k| {
            if k == 2 {
                Some(row_for(k, "same-left"))
            } else {
                Some(row_for(k, "still-different"))
            }
        });
        let mut right = KeyedConn::new(Arc::clone(&right_sqls), right_value);

        let out = recheck_diffs(&mut left, &mut right, &ctx(), &mut diffs)
            .await
            .unwrap();

        assert_eq!(out.queries, 2, "1 batch x 2 sides");
        assert_eq!(out.confirmed, 2);
        assert!(diffs[0].confirmed && diffs[2].confirmed);
        assert!(!diffs[1].confirmed, "transient diff must be unconfirmed");
        let sqls = right_sqls.lock().unwrap();
        assert_eq!(sqls.len(), 1);
        assert!(
            !sqls[0].contains("AS OF SCN"),
            "recheck is current read: {sqls:?}"
        );
    }

    #[tokio::test]
    async fn recheck_sql_is_current_read_even_when_scns_captured() {
        // Oracle snapshot 下 scns 已捕获：复核 SQL 仍不得带 AS OF SCN
        //（评审 R1：旧点查路径带 SCN 读自身快照，伪差异过滤失效）。
        let c = ctx();
        c.scns.set((Some(42), Some(43))).unwrap();
        let mut diffs = vec![diff_row(7)];
        let left_sqls = Arc::new(Mutex::new(Vec::new()));
        let mut left = KeyedConn::new(Arc::clone(&left_sqls), const_rows("a"));
        let mut right = KeyedConn::new(Arc::new(Mutex::new(Vec::new())), const_rows("b"));

        recheck_diffs(&mut left, &mut right, &c, &mut diffs)
            .await
            .unwrap();

        for sql in left_sqls.lock().unwrap().iter() {
            assert!(!sql.contains("AS OF SCN"), "current read only: {sql}");
        }
        assert!(diffs[0].confirmed, "a vs b still differs");
    }

    #[tokio::test]
    async fn recheck_unparseable_key_stays_confirmed() {
        let mut diffs = vec![DiffRow {
            key: Value::from("not-a-number"),
            left: None,
            right: None,
            status: DiffStatus::Modified,
            confirmed: false,
        }];
        let (mut left, left_sqls) = RecordingConn::scripted(0);
        let (mut right, _) = RecordingConn::scripted(0);

        let out = recheck_diffs(&mut left, &mut right, &ctx(), &mut diffs)
            .await
            .unwrap();

        assert_eq!(out.queries, 0, "no query for unparseable key");
        assert_eq!(out.confirmed, 1);
        assert!(diffs[0].confirmed);
        assert!(left_sqls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recheck_respects_batch_size_boundary() {
        // 恰 1000 键 → 1 批（2 条查询）；1001 键 → 2 批（4 条查询）。
        for (n, expected_queries) in [(1000usize, 2u64), (1001, 4)] {
            let mut diffs: Vec<DiffRow> = (1..=n as i64).map(diff_row).collect();
            let (mut left, _) = RecordingConn::scripted((expected_queries / 2) as usize);
            let (mut right, _) = RecordingConn::scripted((expected_queries / 2) as usize);
            let out = recheck_diffs(&mut left, &mut right, &ctx(), &mut diffs)
                .await
                .unwrap();
            assert_eq!(out.queries, expected_queries, "n={n}");
        }
    }

    // ── 旧语义保留：transient 剔除 / persistent 确认（批量化形态）───────

    #[tokio::test]
    async fn transient_diff_marked_unconfirmed() {
        let mut diffs = vec![diff_row(7)];
        let mut left = KeyedConn::new(Arc::new(Mutex::new(Vec::new())), const_rows("settled"));
        let mut right = KeyedConn::new(Arc::new(Mutex::new(Vec::new())), const_rows("settled"));
        let out = recheck_diffs(&mut left, &mut right, &ctx(), &mut diffs)
            .await
            .unwrap();
        assert_eq!(out.confirmed, 0);
        assert!(!diffs[0].confirmed);
    }

    #[tokio::test]
    async fn persistent_diff_stays_confirmed() {
        let mut diffs = vec![DiffRow {
            key: Value::from(7),
            left: None,
            right: None,
            status: DiffStatus::Modified,
            confirmed: false,
        }];
        let mut left = KeyedConn::new(Arc::new(Mutex::new(Vec::new())), const_rows("a"));
        let mut right = KeyedConn::new(Arc::new(Mutex::new(Vec::new())), const_rows("b"));
        let out = recheck_diffs(&mut left, &mut right, &ctx(), &mut diffs)
            .await
            .unwrap();
        assert_eq!(out.confirmed, 1);
        assert!(diffs[0].confirmed);
    }
}
