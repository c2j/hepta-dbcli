// ─── delta-diff HashDiffer：分段并行快筛 + 二分 + keyset 行级复核 ───────
//
// 算法（设计文档 §6.2）：MIN/MAX 取键域 → 首轮 threads×8 段并行快筛 →
// 不一致段递归二分（factor=32）→ 段内行数 ≤ threshold 时 keyset 分页行级归并。
// MVP 约束：单列整型键（§6.4）；侧间并行、侧内串行（快照兼容，§8.2）。

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use serde_json::Value;

use crate::backend::{ChecksumSqlSpec, DbConn, DbError, KeysetPageSpec};
use crate::delta_diff::checksum::{run_checksum, run_checksum_sql, ChecksumTuple};
use crate::delta_diff::recheck::recheck_diffs;
use crate::delta_diff::report::{
    DiffReport, DiffRow, DiffStatus, DiffSummary, PerfMetrics, ShardResult, ShardStatus, TableRef,
};
use crate::delta_diff::rowdiff::row_level_diff;
use crate::delta_diff::strategy::{ConsistencyMode, DiffContext, DiffStrategy};

pub(crate) struct HashDiffer;

struct Counters {
    queries: u64,
    shard_ms: Vec<u64>,
}

type SegmentJoinSet =
    tokio::task::JoinSet<Result<(i64, i64, ChecksumTuple, ChecksumTuple, u64), DbError>>;

#[async_trait::async_trait]
impl DiffStrategy for HashDiffer {
    fn name(&self) -> &'static str {
        "hashdiff"
    }

    async fn diff(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
    ) -> Result<DiffReport, DbError> {
        let started = Utc::now();
        let mut counters = Counters {
            queries: 0,
            shard_ms: Vec::new(),
        };

        ctx.vlog(format!(
            "[delta-diff] strategy=hashdiff consistency={} key={} threads={} bisection_factor={} threshold={}",
            ctx.consistency.as_str(),
            ctx.key_column,
            ctx.threads,
            ctx.bisection_factor,
            ctx.bisection_threshold
        ));

        if ctx.consistency == ConsistencyMode::Snapshot {
            open_snapshot(left, ctx.verbose).await?;
            open_snapshot(right, ctx.verbose).await?;
            let _ = ctx.scns.set((
                capture_scn(left, ctx.verbose).await?,
                capture_scn(right, ctx.verbose).await?,
            ));
        }

        let result = self.diff_inner(left, right, ctx, &mut counters).await;

        if ctx.consistency == ConsistencyMode::Snapshot {
            ctx.vlog("[sql] COMMIT");
            let _ = left.query_drop("COMMIT").await;
            let _ = right.query_drop("COMMIT").await;
        }
        let (shards, mut diffs) = result?;

        // §8.3 二次复核：快照提交后的当前读点查，剔除比对窗口内的并发伪差异
        if ctx.recheck && !diffs.is_empty() {
            let lspec = keyset_spec(ctx, true, left.dialect())?;
            let rspec = keyset_spec(ctx, false, right.dialect())?;
            recheck_diffs(left, right, &lspec, &rspec, &mut diffs, ctx.verbose).await?;
            counters.queries += 2 * diffs.len() as u64;
        }

        let mut report = assemble_report(ctx, shards, diffs, ctx.sample_limit);
        report.started_at = started;
        report.finished_at = Utc::now();
        report.perf.queries_total = counters.queries;
        report.perf.shard_duration_p50_ms = percentile(&mut counters.shard_ms, 50);
        report.perf.shard_duration_p99_ms = percentile(&mut counters.shard_ms, 99);
        Ok(report)
    }
}

impl HashDiffer {
    async fn diff_inner(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        counters: &mut Counters,
    ) -> Result<(Vec<ShardResult>, Vec<DiffRow>), DbError> {
        let (lmin, lmax) = key_range(left, ctx, true).await?;
        let (rmin, rmax) = key_range(right, ctx, false).await?;
        counters.queries += 2;

        let l = lmin.zip(lmax);
        let r = rmin.zip(rmax);
        let (lo, hi) = match (l, r) {
            (Some((a, b)), Some((c, d))) => (a.min(c), b.max(d)),
            (Some((a, b)), None) => (a, b),
            (None, Some((c, d))) => (c, d),
            (None, None) => {
                return Ok((vec![], vec![]));
            }
        };
        let domain = (lo, hi + 1);

        let segments = split_range(domain.0, domain.1, ctx.threads * 8);
        let mut shards: Vec<ShardResult> = Vec::new();
        let mut diffs: Vec<DiffRow> = Vec::new();

        match ctx.consistency {
            ConsistencyMode::Snapshot => {
                for seg in segments {
                    self.compare_segment(
                        left,
                        right,
                        ctx,
                        seg,
                        0,
                        &mut shards,
                        &mut diffs,
                        counters,
                    )
                    .await?;
                }
            }
            ConsistencyMode::None => {
                let first = self
                    .first_pass_parallel(left, right, ctx, &segments, counters)
                    .await?;
                for (lo, hi, lsum, rsum, elapsed_ms) in first {
                    self.handle_segment(
                        left,
                        right,
                        ctx,
                        (lo, hi),
                        lsum,
                        rsum,
                        0,
                        elapsed_ms,
                        &mut shards,
                        &mut diffs,
                        counters,
                    )
                    .await?;
                }
            }
        }
        Ok((shards, diffs))
    }

    /// none 档首轮并行快筛：SQL 预渲染（方言在主连接上），任务经信号量
    /// 提交到两侧连接池（每侧 ≤ ⌈threads/2⌉ 并发会话，§8.4）。
    async fn first_pass_parallel(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        segments: &[(i64, i64)],
        counters: &mut Counters,
    ) -> Result<Vec<(i64, i64, ChecksumTuple, ChecksumTuple, u64)>, DbError> {
        use tokio::sync::Semaphore;

        let per_side = (ctx.threads / 2).max(1);
        let sem = Arc::new(Semaphore::new(per_side * 2));
        let mut set: SegmentJoinSet = tokio::task::JoinSet::new();

        for &seg in segments {
            let lsql =
                left.dialect()
                    .render_checksum_sql(&checksum_spec(ctx, true, seg, left.dialect())?);
            let rsql = right.dialect().render_checksum_sql(&checksum_spec(
                ctx,
                false,
                seg,
                right.dialect(),
            )?);
            ctx.vlog(format!("[sql:left] {lsql}"));
            ctx.vlog(format!("[sql:right] {rsql}"));
            let (lp, rp) = (Arc::clone(&ctx.left_pool), Arc::clone(&ctx.right_pool));
            let sem = Arc::clone(&sem);
            set.spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|e| DbError::query(format!("semaphore: {e}")))?;
                let t0 = Instant::now();
                let (mut lc, mut rc) = (lp.acquire().await?, rp.acquire().await?);
                // SQL 已在父循环按序打印（ctx.vlog），任务内不重复输出
                let (l, r) = tokio::join!(
                    run_checksum_sql(&mut *lc, &lsql, false),
                    run_checksum_sql(&mut *rc, &rsql, false)
                );
                Ok((seg.0, seg.1, l?, r?, t0.elapsed().as_millis() as u64))
            });
        }

        let mut out = Vec::with_capacity(segments.len());
        while let Some(res) = set.join_next().await {
            out.push(res.map_err(|e| DbError::query(format!("join: {e}")))??);
        }
        counters.queries += 2 * segments.len() as u64;
        out.sort_by_key(|(lo, ..)| *lo);
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    async fn compare_segment(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        range: (i64, i64),
        depth: usize,
        shards: &mut Vec<ShardResult>,
        diffs: &mut Vec<DiffRow>,
        counters: &mut Counters,
    ) -> Result<(), DbError> {
        let t0 = Instant::now();
        if let Some(cp) = &ctx.checkpoint {
            let id = format!("{}-{}", range.0, range.1);
            if let Some((lc, rc, dc)) = cp.lock().await.completed(&id) {
                ctx.vlog(format!(
                    "[shard] {id} skipped left={lc} right={rc} diff={dc}"
                ));
                shards.push(ShardResult {
                    shard_id: id,
                    key_range: (Value::from(range.0), Value::from(range.1)),
                    left_count: lc,
                    right_count: rc,
                    diff_count: dc,
                    status: ShardStatus::Skipped,
                    duration_ms: 0,
                });
                return Ok(());
            }
        }
        let lspec = checksum_spec(ctx, true, range, left.dialect())?;
        let rspec = checksum_spec(ctx, false, range, right.dialect())?;
        let (lsum, rsum) = tokio::join!(
            run_checksum(left, &lspec, ctx.verbose),
            run_checksum(right, &rspec, ctx.verbose)
        );
        let (lsum, rsum) = (lsum?, rsum?);
        counters.queries += 2;
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        self.handle_segment(
            left, right, ctx, range, lsum, rsum, depth, elapsed_ms, shards, diffs, counters,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_segment(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        range: (i64, i64),
        lsum: ChecksumTuple,
        rsum: ChecksumTuple,
        depth: usize,
        elapsed_ms: u64,
        shards: &mut Vec<ShardResult>,
        diffs: &mut Vec<DiffRow>,
        counters: &mut Counters,
    ) -> Result<(), DbError> {
        if lsum == rsum {
            shards.push(shard_result(
                range,
                lsum,
                rsum,
                ShardStatus::Match,
                0,
                elapsed_ms,
            ));
            self.record_checkpoint(ctx, range, "Match", lsum.count, rsum.count, 0)
                .await?;
            ctx.vlog(format!(
                "[shard] {}-{} match left={} right={} diff=0 ({}ms)",
                range.0, range.1, lsum.count, rsum.count, elapsed_ms
            ));
            return Ok(());
        }

        let max_count = lsum.count.max(rsum.count);
        if max_count <= ctx.bisection_threshold || range.1 - range.0 <= 1 {
            let t0 = Instant::now();
            let lspec = keyset_spec(ctx, true, left.dialect())?;
            let rspec = keyset_spec(ctx, false, right.dialect())?;
            let detail =
                row_level_diff(left, right, &lspec, &rspec, Some(range), 1, ctx.verbose).await?;
            counters.queries += 2 * (1 + detail.left_count.max(detail.right_count) / 8192);
            let n = detail.rows.len() as u64;
            diffs.extend(detail.rows);
            let total_ms = elapsed_ms + t0.elapsed().as_millis() as u64;
            shards.push(shard_result(
                range,
                lsum,
                rsum,
                ShardStatus::Diff,
                n,
                total_ms,
            ));
            self.record_checkpoint(ctx, range, "Diff", lsum.count, rsum.count, n)
                .await?;
            ctx.vlog(format!(
                "[shard] {}-{} diff left={} right={} diff={} ({}ms)",
                range.0, range.1, lsum.count, rsum.count, n, total_ms
            ));
            return Ok(());
        }

        if depth > 16 {
            return Err(DbError::query(format!(
                "delta-diff: bisection depth limit exceeded at range {range:?} \
                 (possible key distribution anomaly)"
            )));
        }
        for sub in split_range(range.0, range.1, ctx.bisection_factor) {
            Box::pin(self.compare_segment(
                left,
                right,
                ctx,
                sub,
                depth + 1,
                shards,
                diffs,
                counters,
            ))
            .await?;
        }
        Ok(())
    }

    async fn record_checkpoint(
        &self,
        ctx: &DiffContext,
        range: (i64, i64),
        status: &str,
        lc: u64,
        rc: u64,
        dc: u64,
    ) -> Result<(), DbError> {
        if let Some(cp) = &ctx.checkpoint {
            let id = format!("{}-{}", range.0, range.1);
            cp.lock().await.record(&id, status, lc, rc, dc)?;
        }
        Ok(())
    }
}

// ─── helpers ───────────────────────────────────────────────────────────

pub(crate) async fn open_snapshot(
    conn: &mut (dyn DbConn + Send),
    verbose: bool,
) -> Result<(), DbError> {
    let (scheme, snapshot_sql, polardbx_sql) = {
        let d = conn.dialect();
        (
            d.url_scheme().to_string(),
            d.begin_snapshot_sql().to_string(),
            d.begin_snapshot_sql_polardbx(),
        )
    };
    if scheme == "mysql" {
        if verbose {
            eprintln!("[sql] SELECT VERSION()");
        }
        let version: Option<String> = conn
            .query("SELECT VERSION()")
            .await
            .ok()
            .and_then(|r| r.rows.first()?.first()?.as_str().map(str::to_string));
        if let Some(v) = version {
            if crate::backend::is_polardbx_version(&v) {
                if let Some([set_iso, start]) = polardbx_sql {
                    if verbose {
                        eprintln!("[sql] {set_iso}");
                        eprintln!("[sql] {start}");
                    }
                    conn.query_drop(set_iso).await?;
                    conn.query_drop(start).await?;
                    return Ok(());
                }
            }
        }
    }
    if verbose {
        eprintln!("[sql] {snapshot_sql}");
    }
    conn.query_drop(&snapshot_sql).await
}

/// Oracle 快照模式下捕获 CURRENT_SCN（§8.2；其余方言返回 None）。
pub(crate) async fn capture_scn(
    conn: &mut (dyn DbConn + Send),
    verbose: bool,
) -> Result<Option<u64>, DbError> {
    let sql = match conn.dialect().snapshot_scn_sql() {
        Some(s) => s.to_string(),
        None => return Ok(None),
    };
    if verbose {
        eprintln!("[sql] {sql}");
    }
    let r = conn.query(&sql).await?;
    let scn = r
        .rows
        .first()
        .and_then(|row| row.first())
        .and_then(|v| match v {
            Value::Number(n) => n.as_u64().or_else(|| n.as_i64().map(|i| i as u64)),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        });
    Ok(scn)
}

/// SELECT MIN(k), MAX(k) — 键域探查（单次索引探查）
async fn key_range(
    conn: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    is_left: bool,
) -> Result<(Option<i64>, Option<i64>), DbError> {
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
    let sql = format!(
        "SELECT MIN({q}{key}{q}), MAX({q}{key}{q}) FROM {table}{where_clause}",
        key = ctx.key_column
    );
    ctx.vlog(format!("[sql] {sql}"));
    let r = conn.query(&sql).await?;
    let Some(row) = r.rows.first() else {
        return Ok((None, None));
    };
    // 非空值但解析失败（如 UNSIGNED BIGINT > i64::MAX 在驱动层丢失）必须显式报错，
    // 否则该侧键域被静默视为空、差行永不比对（评审修复）
    let parse_or_err = |v: Option<&Value>| -> Result<Option<i64>, DbError> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(other) => value_to_i64(other).map(Some).ok_or_else(|| {
                DbError::unsupported(format!(
                    "key column '{}' value {other} is not representable as i64 \
                     (unsigned BIGINT > i64::MAX is unsupported in this version)",
                    ctx.key_column
                ))
            }),
        }
    };
    Ok((parse_or_err(row.first())?, parse_or_err(row.get(1))?))
}

fn value_to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().split('.').next()?.parse().ok(),
        _ => None,
    }
}

/// [lo, hi) 等距切 n 段（n 段可空集被跳过由调用方保证 hi>lo）
fn split_range(lo: i64, hi: i64, n: usize) -> Vec<(i64, i64)> {
    if hi <= lo || n == 0 {
        return vec![];
    }
    let span = (hi - lo) as u128;
    let n = n.min(span as usize).max(1);
    let step = span / n as u128;
    let mut out = Vec::with_capacity(n);
    let mut cur = lo;
    for i in 0..n {
        let next = if i == n - 1 {
            hi
        } else {
            lo + (step * (i as u128 + 1)) as i64
        };
        if next > cur {
            out.push((cur, next));
            cur = next;
        }
    }
    out
}

fn checksum_spec(
    ctx: &DiffContext,
    is_left: bool,
    range: (i64, i64),
    dialect: &dyn crate::backend::Dialect,
) -> Result<ChecksumSqlSpec, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    Ok(ChecksumSqlSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        key_column: Some(ctx.key_column.clone()),
        range: Some(range),
        bucket: None,
        filter: crate::delta_diff::strategy::side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
        normalized_exprs: side.plan.normalized_exprs(dialect)?,
    })
}

/// 行级拉取 spec：key 列原样（排序/分页需要数值序），其余列用 §九 规范化
/// 表达式（raw_exprs=true）——两侧文本表示字节级一致，跨库行比较成立，
/// 同时覆盖 Oracle 行级精度降级（v2.1 §九-2）。
pub(crate) fn keyset_spec(
    ctx: &DiffContext,
    is_left: bool,
    dialect: &dyn crate::backend::Dialect,
) -> Result<KeysetPageSpec, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let mut columns = vec![format!(
        "{q}{key}{q}",
        q = dialect.identifier_quote(),
        key = ctx.key_column
    )];
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
        key_columns: vec![ctx.key_column.clone()],
        range: None,
        last_key: None,
        page_size: 8192,
        filter: crate::delta_diff::strategy::side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
    })
}

fn shard_result(
    range: (i64, i64),
    l: ChecksumTuple,
    r: ChecksumTuple,
    status: ShardStatus,
    diff_count: u64,
    elapsed_ms: u64,
) -> ShardResult {
    ShardResult {
        shard_id: format!("{}-{}", range.0, range.1),
        key_range: (Value::from(range.0), Value::from(range.1)),
        left_count: l.count,
        right_count: r.count,
        diff_count,
        status,
        duration_ms: elapsed_ms,
    }
}

fn percentile(sorted: &mut [u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted.sort_unstable();
    let idx = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[idx]
}

fn started_now() -> chrono::DateTime<Utc> {
    Utc::now()
}

fn assemble_report(
    ctx: &DiffContext,
    shards: Vec<ShardResult>,
    diffs: Vec<DiffRow>,
    sample_limit: usize,
) -> DiffReport {
    let mut summary = DiffSummary {
        left_total: shards.iter().map(|s| s.left_count).sum(),
        right_total: shards.iter().map(|s| s.right_count).sum(),
        ..Default::default()
    };
    for d in &diffs {
        if !d.confirmed {
            continue;
        }
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
    let sample_diffs = diffs.into_iter().take(sample_limit).collect();
    DiffReport {
        started_at: started_now(),
        finished_at: started_now(),
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
        strategy: "hashdiff".into(),
        consistency: ctx.consistency.as_str().into(),
        hash_algorithm: "md5".into(),
        summary,
        perf: PerfMetrics::default(),
        shards,
        sample_diffs,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_range_even() {
        assert_eq!(
            split_range(0, 100, 4),
            vec![(0, 25), (25, 50), (50, 75), (75, 100)]
        );
    }

    #[test]
    fn split_range_uneven_and_tiny() {
        let parts = split_range(0, 10, 3);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.first().unwrap().0, 0);
        assert_eq!(parts.last().unwrap().1, 10);
        assert_eq!(split_range(5, 6, 32), vec![(5, 6)]);
        assert!(split_range(7, 7, 8).is_empty());
        assert!(split_range(9, 3, 8).is_empty());
    }

    #[test]
    fn split_range_covers_domain_without_gaps() {
        let parts = split_range(-50, 1_000_001, 32);
        assert_eq!(parts.first().unwrap().0, -50);
        assert_eq!(parts.last().unwrap().1, 1_000_001);
        for w in parts.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
    }

    #[test]
    fn percentile_basic() {
        assert_eq!(percentile(&mut [], 50), 0);
        assert_eq!(percentile(&mut [10, 20, 30, 40], 50), 30);
        assert_eq!(percentile(&mut [10, 20, 30, 40], 99), 40);
    }
}
