// ─── delta-diff HashDiffer：分段并行快筛 + 二分 + keyset 行级复核 ───────
//
// 算法（设计文档 §6.2）：MIN/MAX 取键域 → 首轮快筛 → 不一致段递归二分
// （factor=32）→ 段内行数 ≤ threshold 时 keyset 分页行级归并。
// none 档首轮为 WP3 聚合下推：每侧一条 UNION ALL 宽聚合语句（全部首段
// 的 render_checksum_sql 串接，结果第 k 行即第 k 段的精确校验元组），
// 全等段直接判 Match，零行级传输；失配段走既有二分路径。snapshot 档
// 绑定单连接（会话快照无法跨池化会话），保持逐段聚合。SQL 形态完全
// 复用既有 render_checksum_sql，零方言改动。
// MVP 约束：单列整型键（§6.4）；侧间并行（两条宽聚合 tokio::join!）、
// 侧内串行（快照兼容，§8.2）。

use std::time::Instant;

use chrono::Utc;
use serde_json::Value;

use crate::backend::{ChecksumSqlSpec, DbConn, DbError, KeysetPageSpec};
use crate::delta_diff::checksum::{parse_wide_aggregate_row, run_checksum, ChecksumTuple};
use crate::delta_diff::recheck::recheck_diffs;
use crate::delta_diff::report::{
    DiffReport, DiffRow, DiffStatus, DiffSummary, PerfMetrics, ShardResult, ShardStatus, TableRef,
};
use crate::delta_diff::rowdiff::row_level_diff;
use crate::delta_diff::strategy::{ConsistencyMode, DiffContext, DiffStrategy};

/// 每条 UNION ALL 语句覆盖的段数（WP3）。8 段/语句在实测中平衡了
/// 语句数削减（2n → 2⌈n/8⌉）与 DB 端串行化损失（UNION ALL 分支在
/// 服务器内串行执行，过宽的语句反而慢于并发逐段查询）。
const UNION_BRANCHES: usize = 8;

/// 宽聚合结果条目：段范围 + 该段精确 checksum 五元组（UNION ALL 第 k
/// 行对应该 chunk 的第 k 段）。
type SegmentAgg = (i64, i64, ChecksumTuple);

pub(crate) struct HashDiffer;

struct Counters {
    queries: u64,
    shard_ms: Vec<u64>,
}

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
                // WP3 pushdown: chunks of UNION ALL aggregate statements
                // (per side) replace the per-segment query sweep. Segment
                // tuples come back one row per segment; matching segments
                // become Match shards with zero row transfer, mismatches
                // fall through to the bisection path.
                let first = self
                    .first_pass_aggregate(left, right, ctx, &segments, counters)
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

    /// none 档首轮快筛（WP3 聚合下推）：每侧把全部首段按
    /// `UNION_BRANCHES` 段一组串成 UNION ALL 宽聚合语句，共
    /// ⌈n/UNION_BRANCHES⌉ 条；语句经两侧连接池并发执行（每侧
    /// ⌈threads/2⌉ 并发会话，§8.4，与旧逐段并行扫一致的并发度）。
    /// 结果第 k 行即第 k 段的精确 (cnt, s1..s4)——复用各方言
    /// render_checksum_sql（键域谓词互斥，UNION ALL 语义安全），查询
    /// 数从 2n 降到 2⌈n/UNION_BRANCHES⌉。
    ///
    /// 全部分支到齐后按段序与旧实现相同的输出；快筛后失配段顺序走
    /// 既有 compare_segment 二分（侧内串行，快照兼容语义不变，§8.2）。
    /// SQL 在主连接预渲染后 vlog（与原实现相同的 [sql:left]/
    /// [sql:right] 通道）。
    async fn first_pass_aggregate(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        segments: &[(i64, i64)],
        counters: &mut Counters,
    ) -> Result<Vec<(i64, i64, ChecksumTuple, ChecksumTuple, u64)>, DbError> {
        use std::sync::Arc as StdArc;
        use tokio::sync::Semaphore;

        let t0 = Instant::now();
        // Per-side chunk SQL, pre-rendered on the dialect-owning main conn.
        let mut lsqls: Vec<String> = Vec::new();
        let mut rsqls: Vec<String> = Vec::new();
        for chunk in segments.chunks(UNION_BRANCHES) {
            let lspecs: Vec<ChecksumSqlSpec> = chunk
                .iter()
                .map(|&seg| checksum_spec(ctx, true, seg, left.dialect()))
                .collect::<Result<_, _>>()?;
            let rspecs: Vec<ChecksumSqlSpec> = chunk
                .iter()
                .map(|&seg| checksum_spec(ctx, false, seg, right.dialect()))
                .collect::<Result<_, _>>()?;
            lsqls.push(render_segment_aggregate_sql(&lspecs, chunk, left.dialect()));
            rsqls.push(render_segment_aggregate_sql(
                &rspecs,
                chunk,
                right.dialect(),
            ));
        }
        for sql in &lsqls {
            ctx.vlog(format!("[sql:left] {sql}"));
        }
        for sql in &rsqls {
            ctx.vlog(format!("[sql:right] {sql}"));
        }

        // Execute both sides' chunks through the pools (side-parallel;
        // per-side concurrency capped like the old per-segment sweep).
        // Each task tags its result with the side, so pairing is exact
        // regardless of JoinSet completion order.
        #[derive(Clone, Copy, PartialEq)]
        enum Side {
            L,
            R,
        }
        let sem = StdArc::new(Semaphore::new((ctx.threads / 2).max(1) * 2));
        let mut set: tokio::task::JoinSet<Result<(Side, Vec<SegmentAgg>), DbError>> =
            tokio::task::JoinSet::new();
        let sides = [
            (Side::L, &ctx.left_pool, lsqls),
            (Side::R, &ctx.right_pool, rsqls),
        ];
        for (side, pool, sqls) in sides {
            for sql in sqls {
                let pool = StdArc::clone(pool);
                let sem = StdArc::clone(&sem);
                set.spawn(async move {
                    let _permit = sem
                        .acquire()
                        .await
                        .map_err(|e| DbError::query(format!("semaphore: {e}")))?;
                    let mut conn = pool.acquire().await?;
                    let rows = run_wide_segment_aggregate(&mut *conn, &sql).await?;
                    Ok((side, rows))
                });
            }
        }

        // Collect per-side chunk results keyed by their first segment lo
        // (chunk partitions are identical on both sides).
        let mut left_by_lo: std::collections::BTreeMap<i64, Vec<SegmentAgg>> = Default::default();
        let mut right_by_lo: std::collections::BTreeMap<i64, Vec<SegmentAgg>> = Default::default();
        let mut statement_count = 0usize;
        while let Some(res) = set.join_next().await {
            let (side, rows) = res.map_err(|e| DbError::query(format!("join: {e}")))??;
            // 聚合无 GROUP BY 时每支恰 1 行，但这是拆掉 debug_assert 后
            // 重新加固的安全网：空结果直接段错位（237eb2f 引入的回归）。
            let first_lo = rows
                .first()
                .ok_or_else(|| {
                    DbError::query(
                        "delta-diff: wide chunk aggregate returned no rows (expected one \
                     row per segment)",
                    )
                })?
                .0;
            match side {
                Side::L => left_by_lo.insert(first_lo, rows),
                Side::R => right_by_lo.insert(first_lo, rows),
            };
            statement_count += 1;
        }
        counters.queries += statement_count as u64;
        if left_by_lo.len() != right_by_lo.len() {
            return Err(DbError::query(format!(
                "delta-diff: wide chunk statements mismatched: left {} vs right {}",
                left_by_lo.len(),
                right_by_lo.len()
            )));
        }

        // Flatten left/right in segment order (chunks are contiguous
        // partition slices, so ordered concatenation = segment order).
        let flatten = |by_lo: &std::collections::BTreeMap<i64, Vec<SegmentAgg>>| {
            by_lo
                .values()
                .flatten()
                .cloned()
                .collect::<Vec<SegmentAgg>>()
        };
        let lflat = flatten(&left_by_lo);
        let rflat = flatten(&right_by_lo);
        // 行数校验是硬错误（非 debug_assert）：左右行数不齐时 zip 会按
        // 最短截断、段错位配对，release 下可能静默假 Match。
        if lflat.len() != segments.len() || rflat.len() != segments.len() {
            return Err(DbError::query(format!(
                "delta-diff: wide aggregate returned {} left / {} right rows, expected {} segments",
                lflat.len(),
                rflat.len(),
                segments.len()
            )));
        }

        // SegmentAgg carries only lo (decoded from the row marker); the
        // authoritative hi comes from the segment partition, which both
        // sides' chunk SQL were rendered from.
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        let out = lflat
            .iter()
            .zip(rflat.iter())
            .zip(segments.iter())
            .map(|((lk, rk), &(lo, hi))| {
                if lk.0 != lo || rk.0 != lo {
                    return Err(DbError::query(format!(
                        "delta-diff: wide chunk row seg_lo mismatch: got ({}, {}), expected {lo}",
                        lk.0, rk.0
                    )));
                }
                Ok((lo, hi, lk.2, rk.2, elapsed_ms))
            })
            .collect::<Result<Vec<_>, DbError>>()?;
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
            let left_numeric = keyset_numeric_flags(ctx, true);
            let right_numeric = keyset_numeric_flags(ctx, false);
            let detail = row_level_diff(
                left,
                right,
                (&lspec, &rspec),
                Some(range),
                1,
                (&left_numeric, &right_numeric),
                ctx.verbose,
            )
            .await?;
            counters.queries += detail.queries;
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

/// WP3 宽聚合 SQL 渲染：把全部首段压成每侧一条语句。每个段直接复用
/// 方言的 `render_checksum_sql`（键域范围谓词 + 四个位切片，与旧逐段
/// 快筛完全相同的 SQL），以 UNION ALL 串接；每个内层 SELECT 前插一个
/// 字面量 `seg_lo` 标记段归属，外层按 `seg_lo` 排序。段谓词互斥（键域
/// 划分不重叠），UNION ALL 语义安全；结果第 k 行即第 k 段的精确
/// (cnt, s1..s4)。零方言改动、零新依赖，段归属与二分输入与旧实现
/// 逐字节一致。
fn render_segment_aggregate_sql(
    specs: &[ChecksumSqlSpec],
    segments: &[(i64, i64)],
    dialect: &dyn crate::backend::Dialect,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(segments.len());
    for (spec, &(lo, _)) in specs.iter().zip(segments.iter()) {
        let sql = dialect.render_checksum_sql(spec);
        // SELECT-list expansion: prefix each inner SELECT with a literal
        // segment marker so result rows are self-describing.
        let expanded = sql.replacen("SELECT ", &format!("SELECT {lo} AS seg_lo, "), 1);
        parts.push(expanded);
    }
    // 表别名不能按方言硬编码：Oracle 的 FROM 子查询别名不允许 AS
    //（ORA-00933），MySQL/PG/DuckDB/GaussDB 则两者皆可。与仓库其余
    // 方言渲染一致（见 oracle/dialect.rs 的 `FROM (...) t`）。
    let wrapper = if dialect.url_scheme() == "oracle" {
        " wide"
    } else {
        " AS wide"
    };
    format!(
        "SELECT seg_lo, cnt, s1, s2, s3, s4 FROM (\n  {}\n){wrapper} ORDER BY seg_lo",
        parts.join("\n  UNION ALL\n  ")
    )
}

/// 执行宽聚合并按段序解码（WP3）。
///
/// SQL 已在方言宿主连接预渲染（与旧并行快筛同一模式）；verbose 日志由
/// 调用方的 [sql:left]/[sql:right] 通道输出，此处静默执行。返回
/// `(lo, hi, tuple)` 列表（按 lo 升序，与输入 segments 顺序一致）。
async fn run_wide_segment_aggregate(
    conn: &mut (dyn DbConn + Send),
    sql: &str,
) -> Result<Vec<SegmentAgg>, DbError> {
    let result = conn.query(sql).await?;
    let mut out = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        // row = [seg_lo, cnt, s1, s2, s3, s4]; skip the marker column.
        let lo = row
            .first()
            .and_then(|v| match v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.trim().split('.').next()?.parse().ok(),
                _ => None,
            })
            .ok_or_else(|| DbError::query("delta-diff: wide chunk row missing seg_lo marker"))?;
        let wide = parse_wide_aggregate_row(&row[1..])?;
        out.push((lo, lo, wide));
    }
    // Result order follows ORDER BY seg_lo, so rows are ascending by lo.
    if out.windows(2).any(|w| w[0].0 >= w[1].0) {
        return Err(DbError::query(
            "delta-diff: wide chunk rows not strictly ordered by seg_lo",
        ));
    }
    Ok(out)
}

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
    let d = conn.dialect();
    let table = d.quote_table(side.schema.as_deref(), &side.table);
    let key = d.quote_ident(&ctx.side_key_columns(is_left)[0]);
    let where_clause = ctx
        .filter
        .as_ref()
        .map(|f| format!(" WHERE ({f})"))
        .unwrap_or_default();
    let sql = format!("SELECT MIN({key}), MAX({key}) FROM {table}{where_clause}");
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
    let side_key = ctx.side_key_columns(is_left)[0].clone();
    Ok(ChecksumSqlSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        key_column: Some(side_key),
        range: Some(range),
        bucket: None,
        filter: crate::delta_diff::strategy::side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
        normalized_exprs: side.plan.normalized_exprs(dialect)?,
        key_hash_exprs: vec![],
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
    let side_key = &ctx.side_key_columns(is_left)[0];
    let mut columns = vec![dialect.quote_ident(side_key)];
    for spec in side.plan.norm_specs.iter().filter(|s| &s.name != side_key) {
        columns.push(dialect.normalize_expr(spec)?);
    }
    Ok(KeysetPageSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        columns,
        raw_exprs: true,
        key_columns: vec![side_key.clone()],
        string_key: vec![false],
        range: None,
        last_key: None,
        page_size: 8192,
        filter: crate::delta_diff::strategy::side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
    })
}

fn keyset_numeric_flags(ctx: &DiffContext, is_left: bool) -> Vec<bool> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let side_key = &ctx.side_key_columns(is_left)[0];
    let mut columns = vec![side_key.clone()];
    columns.extend(
        side.plan
            .norm_specs
            .iter()
            .filter(|spec| &spec.name != side_key)
            .map(|spec| spec.name.clone()),
    );
    side.plan.numeric_value_flags_for(&columns)
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
    _sample_limit: usize,
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
    let sample_diffs = diffs;
    let mut report = DiffReport {
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
        row_payload: crate::delta_diff::report::RowPayload::Columns,
        key_columns: vec![],
        value_columns: vec![],
        column_data_types: vec![],
        ident_quote: '"',
        ident_scheme: String::new(),
        backslash_escape: false,
        modified_columns: None,
    };
    crate::delta_diff::report::stamp_columns_from_plan(&mut report, &ctx.left.plan);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mysql::dialect::MySqlDialect;
    use crate::backend::{Dialect, QueryResult};
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ── split_range / percentile (existing) ──

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

    // ── WP3 wide segment aggregate: renderer ──

    fn agg_spec(range: (i64, i64)) -> ChecksumSqlSpec {
        ChecksumSqlSpec {
            schema: Some("verify".into()),
            table: "verify_t".into(),
            key_column: Some("id".into()),
            range: Some(range),
            bucket: None,
            filter: None,
            scn: None,
            normalized_exprs: vec!["CAST(`id` AS CHAR)".into()],
            key_hash_exprs: vec![],
        }
    }

    #[test]
    fn wide_sql_chains_all_segments_with_union_all() {
        let d = MySqlDialect;
        let segments = [(0, 50), (50, 100), (100, 150)];
        let specs: Vec<ChecksumSqlSpec> = segments.iter().map(|&s| agg_spec(s)).collect();
        let sql = render_segment_aggregate_sql(&specs, &segments, &d);
        assert_eq!(
            sql.matches("UNION ALL").count(),
            2,
            "n segments → n-1 unions"
        );
        assert_eq!(
            sql.matches("MD5(CONCAT_WS").count(),
            3,
            "one checksum per segment"
        );
        assert!(sql.contains("0 AS seg_lo"), "segment markers present");
        assert!(sql.contains("50 AS seg_lo"));
        assert!(sql.contains("100 AS seg_lo"));
        assert!(
            sql.contains("`id` >= 0 AND `id` < 50"),
            "segment 0 range predicate"
        );
        assert!(
            sql.contains("`id` >= 100 AND `id` < 150"),
            "segment 2 range predicate"
        );
        assert!(sql.contains("ORDER BY seg_lo"), "rows ordered by segment");
    }

    #[test]
    fn wide_sql_single_segment_has_no_union() {
        let d = MySqlDialect;
        let segments = [(0, 50)];
        let specs = vec![agg_spec((0, 50))];
        let sql = render_segment_aggregate_sql(&specs, &segments, &d);
        assert!(!sql.contains("UNION ALL"));
        assert!(sql.contains("0 AS seg_lo"));
    }

    #[test]
    fn wide_sql_table_alias_is_dialect_correct() {
        // 回归：`) AS wide` 在 Oracle 上非法（ORA-00933，FROM 子查询别名
        // 不允许 AS）。MySQL 系保留 `) AS wide`，Oracle 必须是 `) wide`。
        let d = MySqlDialect;
        let segments = [(0, 50)];
        let specs = vec![agg_spec((0, 50))];
        let sql = render_segment_aggregate_sql(&specs, &segments, &d);
        assert!(sql.contains(") AS wide"), "mysql: {sql}");

        #[cfg(feature = "oracle")]
        {
            use crate::backend::oracle_native::dialect::OracleDialect;
            let sql = render_segment_aggregate_sql(&specs, &segments, &OracleDialect::new());
            assert!(
                sql.contains(") wide") && !sql.contains(") AS wide"),
                "oracle must not use AS for the table alias: {sql}"
            );
        }
    }

    #[test]
    fn wide_sql_preserves_filter_and_scn_of_each_segment() {
        // MySQL ignores spec.scn (snapshot is session-level); filter must
        // still land in every segment's WHERE. Oracle applies scn per
        // segment via `AS OF SCN`; verify with the Oracle dialect.
        let d = MySqlDialect;
        let segments = [(0, 50), (50, 100)];
        let mut specs: Vec<ChecksumSqlSpec> = segments.iter().map(|&s| agg_spec(s)).collect();
        for spec in &mut specs {
            spec.filter = Some("status = 1".into());
            spec.scn = Some(42);
        }
        let sql = render_segment_aggregate_sql(&specs, &segments, &d);
        assert_eq!(sql.matches("status = 1").count(), 2, "filter per segment");

        #[cfg(feature = "oracle")]
        {
            use crate::backend::oracle_native::dialect::OracleDialect;
            let sql = render_segment_aggregate_sql(&specs, &segments, &OracleDialect::new());
            assert_eq!(sql.matches("AS OF SCN 42").count(), 2, "scn per segment");
        }
    }

    // ── WP3 wide segment aggregate: execution + decode ──

    /// Mock conn: scripted `query` responses, records last SQL.
    struct WideConn {
        responses: VecDeque<QueryResult>,
        last_sql: String,
        dialect: MySqlDialect,
    }

    #[async_trait]
    impl DbConn for WideConn {
        async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            self.last_sql = sql.to_string();
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

    fn agg_row(seg_lo: i64, cnt: u64, s: [u64; 4]) -> Vec<Value> {
        vec![
            json!(seg_lo),
            json!(cnt),
            json!(s[0]),
            json!(s[1]),
            json!(s[2]),
            json!(s[3]),
        ]
    }

    #[tokio::test]
    async fn run_wide_aggregate_maps_rows_to_segments_in_order() {
        let result = QueryResult {
            columns: vec![
                "seg_lo".into(),
                "cnt".into(),
                "s1".into(),
                "s2".into(),
                "s3".into(),
                "s4".into(),
            ],
            rows: vec![agg_row(0, 50, [1, 2, 3, 4]), agg_row(50, 0, [0, 0, 0, 0])],
            row_count: 2,
            rows_affected: None,
        };
        let mut conn = WideConn {
            responses: VecDeque::from([result]),
            last_sql: String::new(),
            dialect: MySqlDialect,
        };
        let out = run_wide_segment_aggregate(&mut conn, "SELECT ...")
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            (
                0,
                0,
                ChecksumTuple {
                    count: 50,
                    s: [1, 2, 3, 4]
                }
            )
        );
        assert_eq!(out[1], (50, 50, ChecksumTuple::zero()));
    }

    #[tokio::test]
    async fn run_wide_aggregate_disordered_rows_are_error() {
        // Rows must arrive ascending by seg_lo (ORDER BY seg_lo in the
        // rendered statement); disorder means the marker/parse contract
        // broke, so fail loudly instead of pairing mismatched segments.
        let result = QueryResult {
            columns: vec![
                "seg_lo".into(),
                "cnt".into(),
                "s1".into(),
                "s2".into(),
                "s3".into(),
                "s4".into(),
            ],
            rows: vec![agg_row(50, 0, [0, 0, 0, 0]), agg_row(0, 50, [1, 2, 3, 4])],
            row_count: 2,
            rows_affected: None,
        };
        let mut conn = WideConn {
            responses: VecDeque::from([result]),
            last_sql: String::new(),
            dialect: MySqlDialect,
        };
        let err = run_wide_segment_aggregate(&mut conn, "SELECT ...")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not strictly ordered"));
    }

    #[tokio::test]
    async fn run_wide_aggregate_marker_as_string_parses() {
        // Some drivers/materializations hand back the marker as a string
        // (e.g. "0" or "0.0" from DECIMAL); the parser must still recover lo.
        let result = QueryResult {
            columns: vec![
                "seg_lo".into(),
                "cnt".into(),
                "s1".into(),
                "s2".into(),
                "s3".into(),
                "s4".into(),
            ],
            rows: vec![
                vec![json!("0"), json!(7), json!(1), json!(2), json!(3), json!(4)],
                vec![
                    json!("8.0"),
                    json!(0),
                    json!(0),
                    json!(0),
                    json!(0),
                    json!(0),
                ],
            ],
            row_count: 2,
            rows_affected: None,
        };
        let mut conn = WideConn {
            responses: VecDeque::from([result]),
            last_sql: String::new(),
            dialect: MySqlDialect,
        };
        let out = run_wide_segment_aggregate(&mut conn, "SELECT ...")
            .await
            .unwrap();
        assert_eq!(out[0].0, 0);
        assert_eq!(out[1].0, 8);
    }

    // ── WP3 integration shape: full HashDiffer over two segments ──

    fn plan() -> crate::delta_diff::metadata::TablePlan {
        use crate::backend::ColumnNormSpec;
        crate::delta_diff::metadata::TablePlan {
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
        }
    }

    fn side() -> crate::delta_diff::strategy::SideCtx {
        crate::delta_diff::strategy::SideCtx {
            connection_name: "x".into(),
            schema: Some("s".into()),
            table: "t".into(),
            plan: plan(),
        }
    }

    fn scripted_pool(
        responses: std::sync::Arc<Mutex<VecDeque<QueryResult>>>,
    ) -> std::sync::Arc<dyn crate::backend::DbPool> {
        struct Pool(std::sync::Arc<Mutex<VecDeque<QueryResult>>>);
        #[async_trait]
        impl crate::backend::DbPool for Pool {
            async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
                Ok(Box::new(WideConn {
                    responses: self.0.lock().unwrap().clone(),
                    last_sql: String::new(),
                    dialect: MySqlDialect,
                }))
            }
        }
        std::sync::Arc::new(Pool(responses))
    }

    fn wide_result(rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: vec![
                "seg_lo".into(),
                "cnt".into(),
                "s1".into(),
                "s2".into(),
                "s3".into(),
                "s4".into(),
            ],
            row_count: rows.len(),
            rows,
            rows_affected: None,
        }
    }

    fn ctx() -> DiffContext {
        DiffContext {
            left: side(),
            right: side(),
            left_pool: scripted_pool(Default::default()),
            right_pool: scripted_pool(Default::default()),
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
            consistency: ConsistencyMode::None,
            recheck: false,
            route_warnings: vec![],
            checkpoint: None,
            iblt_capacity: 65_536,
            fetch_all_threshold: 4096,
            naive_max_rows: 10_000,
            strict: false,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        }
    }

    /// Scripted run over the full ctx.threads*8 segment partition
    /// (threads=1 → 8 segments for domain [0,100)): both sides return
    /// minmax, then ONE wide aggregate statement per side.
    async fn run_two_segments(
        lminmax: (i64, i64),
        rminmax: (i64, i64),
        lwide: Vec<Vec<Value>>,
        rwide: Vec<Vec<Value>>,
    ) -> Result<DiffReport, DbError> {
        let minmax = |(a, b): (i64, i64)| QueryResult {
            columns: vec!["MIN(id)".into(), "MAX(id)".into()],
            rows: vec![vec![json!(a), json!(b)]],
            row_count: 1,
            rows_affected: None,
        };
        let wide = |rows: Vec<Vec<Value>>| QueryResult {
            columns: vec![
                "seg_lo".into(),
                "cnt".into(),
                "s1".into(),
                "s2".into(),
                "s3".into(),
                "s4".into(),
            ],
            row_count: rows.len(),
            rows,
            rows_affected: None,
        };
        let mut lconn = WideConn {
            responses: VecDeque::from([
                minmax(lminmax),
                wide(lwide.clone()),
                // bisection callbacks for the mismatched segment:
                minmax(lminmax),
                wide(vec![agg_row(0, 0, [0, 0, 0, 0]); 4]),
            ]),
            last_sql: String::new(),
            dialect: MySqlDialect,
        };
        let mut rconn = WideConn {
            responses: VecDeque::from([
                minmax(rminmax),
                wide(rwide.clone()),
                minmax(rminmax),
                wide(vec![agg_row(0, 0, [0, 0, 0, 0]); 4]),
            ]),
            last_sql: String::new(),
            dialect: MySqlDialect,
        };
        let mut ctx = ctx();
        ctx.left_pool = scripted_pool(std::sync::Arc::new(Mutex::new(VecDeque::from([wide(
            lwide.clone(),
        )]))));
        ctx.right_pool = scripted_pool(std::sync::Arc::new(Mutex::new(VecDeque::from([wide(
            rwide.clone(),
        )]))));
        HashDiffer.diff(&mut lconn, &mut rconn, &ctx).await
    }

    /// First-pass wide rows for domain [0,100) with threads=1 → 8
    /// segments: segment 0 (0..12) carries cnt=50, the rest are empty.
    fn all_match_wide() -> Vec<Vec<Value>> {
        let mut rows = vec![agg_row(0, 50, [1, 2, 3, 4])];
        for k in 1..8 {
            rows.push(agg_row(k * 12, 0, [0, 0, 0, 0]));
        }
        rows
    }

    #[tokio::test]
    async fn hashdiff_all_match_segments_skip_bisection() {
        let report = run_two_segments((0, 99), (0, 99), all_match_wide(), all_match_wide())
            .await
            .unwrap();
        assert_eq!(report.summary.diff_rate, 0.0);
        assert_eq!(
            report.shards.len(),
            8,
            "all segments resolved in first pass"
        );
        assert!(
            report.shards.iter().all(|s| s.status == ShardStatus::Match),
            "fully matching segments must Match without bisection"
        );
        assert_eq!(report.perf.queries_total, 4, "2 minmax + 2 wide aggregates");
    }

    #[tokio::test]
    async fn hashdiff_partial_match_descends_only_mismatched_segment() {
        // Segment 0 matches; segment 1 differs by count (50 vs 49).
        // Scripted bisection callbacks return empty aggregates for every
        // sub-shard, so each mismatched leaf falls to threshold → Match
        // eventually; segment 0 must appear as a first-pass Match shard.
        let mut rwide = all_match_wide();
        rwide[1] = agg_row(12, 49, [5, 6, 7, 9]);
        let report = run_two_segments((0, 99), (0, 99), all_match_wide(), rwide)
            .await
            .unwrap();
        let shard0 = report
            .shards
            .iter()
            .find(|s| s.shard_id == "0-12")
            .expect("first-pass shard 0");
        assert_eq!(shard0.status, ShardStatus::Match);
        assert_eq!(
            shard0.left_count, 50,
            "segment 0 counts come from the wide row"
        );
        assert!(
            report.shards.len() >= 8,
            "mismatched segments descend, matched ones stay single shards"
        );
    }
}
