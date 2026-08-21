// ─── delta-diff JoinDiffer：同连接 JOIN 比对（v2.1 §6.1）─────────────────
//
// 左右表在同一数据库实例（同一连接 URL）时，用 LEFT/RIGHT JOIN + 行哈希比较
// 在库内直接产出差异 key，再按 key 拉行分类。跨实例不可用（无联邦层）时
// 由路由层回退 HashDiffer。当前实现覆盖 MySQL 系（MySQL/PolarDB-X）；
// 其他方言的同连接场景由路由层回退 HashDiffer 并告警。

use std::time::Instant;

use chrono::Utc;
use serde_json::Value;

use crate::backend::{DbConn, DbError, KeysetPageSpec};
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

        let mut diffs: Vec<DiffRow> = Vec::new();
        for row in &result.rows {
            let key = row.first().cloned().unwrap_or(Value::Null);
            let present_left = flag(row.get(1));
            let present_right = flag(row.get(2));
            let status = match (present_left, present_right) {
                (true, false) => DiffStatus::MissingRight,
                (false, true) => DiffStatus::MissingLeft,
                _ => DiffStatus::Modified,
            };
            diffs.push(DiffRow {
                key,
                left: None,
                right: None,
                status,
                confirmed: true,
            });
        }

        fetch_diff_rows(left, ctx, &mut diffs).await?;
        queries += 2 * diffs.len() as u64;

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

/// JOIN 差异探测 SQL：双侧 (key, row_hash) 投影 LEFT/RIGHT JOIN，
/// 输出 (key, in_left, in_right)；Modified 由哈希不等判定。
fn join_diff_sql(ctx: &DiffContext, conn: &mut dyn DbConn) -> Result<String, DbError> {
    let dialect = conn.dialect();
    let q = dialect.identifier_quote();
    let side_sql = |side: &crate::delta_diff::strategy::SideCtx| -> Result<String, DbError> {
        let exprs = side.plan.normalized_exprs(dialect)?;
        let table = match &side.schema {
            Some(s) => format!("{q}{s}{q}.{q}{}{q}", side.table),
            None => format!("{q}{}{q}", side.table),
        };
        let where_clause = ctx
            .filter
            .as_ref()
            .map(|f| format!(" WHERE ({f})"))
            .unwrap_or_default();
        Ok(format!(
            "SELECT {q}{key}{q} AS k, MD5(CONCAT_WS('#', {})) AS h FROM {table}{where_clause}",
            exprs.join(", "),
            key = ctx.key_column
        ))
    };
    let l = side_sql(&ctx.left)?;
    let r = side_sql(&ctx.right)?;
    Ok(format!(
        "SELECT k, in_l, in_r FROM (\n\
           SELECT l.k AS k, 1 AS in_l, (r.k IS NOT NULL) AS in_r, l.h AS lh, r.h AS rh\n\
           FROM ({l}) l LEFT JOIN ({r}) r ON l.k = r.k\n\
           UNION ALL\n\
           SELECT r.k AS k, (l.k IS NOT NULL) AS in_l, 1 AS in_r, l.h AS lh, r.h AS rh\n\
           FROM ({r}) r LEFT JOIN ({l}) l ON r.k = l.k\n\
           WHERE l.k IS NULL\n\
         ) j\n\
         WHERE in_r = 0 OR in_l = 0 OR lh != rh\n\
         ORDER BY k"
    ))
}

/// 按差异 key 拉行内容填充分类样本（复用 keyset 分页的点查形态）。
async fn fetch_diff_rows(
    conn: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    diffs: &mut [DiffRow],
) -> Result<(), DbError> {
    let q = conn.dialect().identifier_quote();
    for d in diffs.iter_mut() {
        let key = match &d.key {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        };
        let Some(k) = key else { continue };
        let lspec = point_spec(ctx, true, k, q, conn.dialect())?;
        let rspec = point_spec(ctx, false, k, q, conn.dialect())?;
        let lsql = conn.dialect().render_keyset_page_sql(&lspec);
        let rsql = conn.dialect().render_keyset_page_sql(&rspec);
        ctx.vlog(format!("[sql:left] {lsql}"));
        ctx.vlog(format!("[sql:right] {rsql}"));
        let lrow = conn.query(&lsql).await?.rows.into_iter().next();
        let rrow = conn.query(&rsql).await?.rows.into_iter().next();
        d.left = lrow;
        d.right = rrow;
    }
    Ok(())
}

fn point_spec(
    ctx: &DiffContext,
    is_left: bool,
    key: i64,
    quote: char,
    dialect: &dyn crate::backend::Dialect,
) -> Result<KeysetPageSpec, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let mut columns = vec![format!("{quote}{key}{quote}", key = ctx.key_column)];
    for spec in side
        .plan
        .norm_specs
        .iter()
        .filter(|s| s.name != ctx.key_column)
    {
        columns.push(dialect.normalize_expr(spec)?);
    }
    Ok(KeysetPageSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        columns,
        raw_exprs: true,
        key_column: ctx.key_column.clone(),
        range: Some((key, key + 1)),
        last_key: None,
        page_size: 1,
        filter: crate::delta_diff::strategy::side_filter(ctx, dialect.url_scheme()),
        scn: None,
    })
}

/// 行数统计（joindiff 同连接，两侧各一次 COUNT(*)）。
async fn count_rows(
    conn: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    is_left: bool,
) -> Result<u64, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let q = conn.dialect().identifier_quote();
    let table = match &side.schema {
        Some(s) => format!("{q}{s}{q}.{q}{}{q}", side.table),
        None => format!("{q}{}{q}", side.table),
    };
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
    sample_limit: usize,
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
    DiffReport {
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
        sample_diffs: diff_rows.into_iter().take(sample_limit).collect(),
        warnings: ctx
            .left
            .plan
            .warnings
            .iter()
            .chain(ctx.right.plan.warnings.iter())
            .chain(ctx.route_warnings.iter())
            .cloned()
            .collect(),
    }
}
