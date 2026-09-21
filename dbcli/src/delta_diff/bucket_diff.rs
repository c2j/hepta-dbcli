// ─── delta-diff BucketDiffer：无主键表内容分桶比对（v2.1 §6.3）──────────
//
// 按 MOD(row_hash, N) 内容分桶（跨库天然对齐），桶内位切片 checksum 快筛，
// 差异桶拉 (hash, count) 行集做客户端多重集合比对（multiset diff）。
// 语义极限：无主键表只能回答"哪些行内容多/少了几次"，报告头部固定提示。

use std::collections::HashMap;
use std::time::Instant;

use chrono::Utc;
use serde_json::Value;

use crate::backend::{ChecksumSqlSpec, DbConn, DbError};
use crate::delta_diff::checksum::{run_batch_checksum, ChecksumTuple};
use crate::delta_diff::hash_diff::open_snapshot;
use crate::delta_diff::report::{
    DiffReport, DiffRow, DiffStatus, DiffSummary, PerfMetrics, RowPayload, ShardResult,
    ShardStatus, TableRef,
};
use crate::delta_diff::strategy::{side_filter, ConsistencyMode, DiffContext, DiffStrategy};

const MAX_BUCKETS: u64 = 1024;

/// Key-domain probe over one side: `SELECT MIN(k) AS mn, MAX(k) AS mx FROM
/// {table} WHERE {filter}` (same shape as hash_diff's keyset planner).
#[derive(Debug, Clone)]
pub(crate) struct BucketPlan {
    pub(crate) key_column: String,
    pub(crate) min: i64,
    pub(crate) max: i64,
    pub(crate) n: u64,
}

/// Per-side key-domain probe input: table reference plus optional filter.
#[derive(Debug, Clone)]
pub(crate) struct KeySide {
    pub(crate) table: TableRef,
    pub(crate) filter: Option<String>,
}

/// Key-domain probe request: per-side table+filter plus shared key/budget.
#[derive(Debug, Clone)]
pub(crate) struct ProbeSpec {
    pub(crate) key_column: String,
    pub(crate) left: KeySide,
    pub(crate) right: KeySide,
    pub(crate) max_buckets: u64,
}

/// Why the key-domain probe produced no `RangePlan` (issue #108).
///
/// `Domain` is a data condition the probe itself proved (no rows, all-NULL
/// keys, disjoint ranges): bucketdiff keeps MOD(rowHash, N) bucketing.
/// `Db` is the engine rejecting the probe statement: that must not be
/// papered over, because inside a snapshot transaction it aborts the
/// session (every later statement then fails with SQLSTATE 25P02) and in
/// `--consistency none` it silently downgrades the whole table to a MOD
/// scan.
#[derive(Debug)]
pub(crate) enum ProbeError {
    Domain(String),
    Db(DbError),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Domain(reason) => write!(f, "{reason}"),
            ProbeError::Db(e) => write!(f, "{e}"),
        }
    }
}

impl BucketPlan {
    /// Probe the overlapping integer key domain and split it into at most
    /// `spec.max_buckets` contiguous PK ranges (WP2, issue #77). Fails closed
    /// when a side exposes no usable key domain (empty table or all-NULL keys).
    pub(crate) async fn probe(
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        spec: &ProbeSpec,
        ctx: &DiffContext,
        queries: &mut u64,
    ) -> Result<Self, ProbeError> {
        let key_column = spec.key_column.as_str();
        let (lkey, rkey) = (
            side_range_key(ctx, true, key_column),
            side_range_key(ctx, false, key_column),
        );
        let (lt, rt) = (&spec.left, &spec.right);
        let lp = Self::probe_side(
            left,
            &lkey,
            &lt.table.schema,
            &lt.table.table,
            lt.filter.as_deref(),
            ctx,
        );
        let rp = Self::probe_side(
            right,
            &rkey,
            &rt.table.schema,
            &rt.table.table,
            rt.filter.as_deref(),
            ctx,
        );
        let (l, r) = tokio::join!(lp, rp);
        let ((lmin, lmax), lq) = l?;
        let ((rmin, rmax), rq) = r?;
        *queries += lq + rq;
        // 键域取两侧并集：交集在部分重叠时会静默丢弃重叠区外的行
        //（如左 1..=1000 / 右 500..=1500 时，左 1-499 与右 1001-1500
        // 不进任何 checksum/multiset，与旧 MOD 路径计入全部行不一致）。
        // 并集下只有一侧有行的区间 checksum 天然为 0，成为正常 diff。
        let min = lmin.min(rmin);
        let max = lmax.max(rmax);
        if min > max {
            return Err(ProbeError::Domain(
                "key domain unresolved: sides' key ranges do not overlap \
                 (bucketdiff falls back to MOD(rowHash, N) bucketing)"
                    .to_string(),
            ));
        }
        Ok(Self {
            key_column: key_column.to_owned(),
            min,
            max,
            n: spec.max_buckets,
        })
    }

    async fn probe_side(
        conn: &mut (dyn DbConn + Send),
        key_column: &str,
        schema: &Option<String>,
        table: &str,
        filter: Option<&str>,
        ctx: &DiffContext,
    ) -> Result<((i64, i64), u64), ProbeError> {
        let scheme = conn.dialect().url_scheme().to_owned();
        let quote = conn.dialect().identifier_quote();
        // Each side's own physical column, quoted like every other key
        // reference (hash_diff::key_range does the same): an unquoted shared
        // name misresolves on case-sensitive identifiers.
        let key = conn.dialect().quote_ident(key_column);
        let mut sql = format!(
            "SELECT MIN({key}) AS mn, MAX({key}) AS mx, \
             SUM(CASE WHEN {key} IS NULL THEN 1 ELSE 0 END) AS nulls FROM {}",
            crate::backend::quote_table_scheme(&scheme, quote, schema.as_deref(), table)
        );
        if let Some(f) = filter {
            sql.push_str(&format!(" WHERE ({f})"));
        }
        ctx.vlog(format!("[sql] {sql}"));
        let r = conn.query(&sql).await.map_err(ProbeError::Db)?;
        let used = 1u64;
        let row = r.rows.first().ok_or_else(|| {
            ProbeError::Domain(format!("key domain probe returned no rows for {table}"))
        })?;
        let parse = |v: Option<&Value>| -> Result<Option<i64>, ProbeError> {
            match v {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Number(n)) => n
                    .as_i64()
                    .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))
                    .map(Some)
                    .ok_or_else(|| ProbeError::Domain(format!("non-integer key domain: {n}"))),
                Some(Value::String(s)) => s
                    .trim()
                    .parse::<i64>()
                    .map(Some)
                    .map_err(|_| ProbeError::Domain(format!("non-integer key domain: {s}"))),
                Some(other) => Err(ProbeError::Domain(format!(
                    "non-integer key domain: {other}"
                ))),
            }
        };
        let cols: Vec<Option<&Value>> = (0..2).map(|i| row.get(i)).collect();
        let (mn, mx) = (parse(cols[0])?, parse(cols[1])?);
        // NULL 键在范围谓词 `k >= a AND k <= b` 下永远不可见，而旧 MOD
        // 路径按内容哈希计入这些行；检测到 NULL 键时失败关闭（回退
        // MOD bucketing），保持与旧行为一致。同一查询内顺带计数，
        // 不额外增加探查成本。
        let nulls = row
            .get(2)
            .and_then(|v| match v {
                Value::Null => Some(0),
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.trim().parse().ok(),
                _ => None,
            })
            .unwrap_or(0);
        if nulls > 0 {
            return Err(ProbeError::Domain(format!(
                "key domain probe found {nulls} NULL {key_column} values in {table} \
                 (range bucketing cannot see NULL keys; bucketdiff falls back \
                 to MOD(rowHash, N) bucketing)"
            )));
        }
        match (mn, mx) {
            (Some(a), Some(b)) if a <= b => Ok(((a, b), used)),
            _ => Err(ProbeError::Domain(format!(
                "key domain unresolved for {table}: no non-NULL {key_column} values \
                 (bucketdiff falls back to MOD(rowHash, N) bucketing)"
            ))),
        }
    }
}

/// The PK-ordered partition plan for one bucketdiff run (WP2, issue #77).
///
/// The integer key domain [min, max] is split into `n` contiguous ranges so
/// each per-bucket pull is an indexed scan: per-bucket SQL turns
/// `MOD(rowHash, N) = b` full scans into `pk BETWEEN a AND b` index ranges.
///
/// Division layout: first `rem` ranges get `seg_len + 1` keys, the rest get
/// `seg_len` (seg_len = domain / n, rem = domain % n, n <= domain enforced by
/// halving). Buckets exactly tile the domain with no overlap or gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RangePlan {
    pub(crate) key_column: String,
    pub(crate) min: i64,
    pub(crate) max: i64,
    pub(crate) n: u64,
}

impl RangePlan {
    /// Build a plan over the key domain [min, max] with at most `n` buckets.
    /// The bucket count shrinks (n → ceil(n/2) while n > domain) so every
    /// range owns at least one key — a provably empty range would otherwise
    /// still cost one query per side.
    pub(crate) fn new(key_column: &str, min: i64, max: i64, n: u64) -> Self {
        assert!(min <= max, "invalid key domain [{min}, {max}]");
        assert!(n >= 1, "bucket count must be positive");
        let domain = i128::from(max) - i128::from(min) + 1;
        let buckets = u64::try_from(domain).unwrap_or(u64::MAX).min(n);
        Self {
            key_column: key_column.to_owned(),
            min,
            max,
            n: buckets,
        }
    }

    /// Width of a bucket in keys: buckets 0..rem get seg_len + 1, the rest
    /// seg_len. Invariant (asserted): sizes exactly tile [min, max].
    pub(crate) fn seg_len(&self) -> u64 {
        let domain = i128::from(self.max) - i128::from(self.min) + 1;
        u64::try_from(domain / i128::from(self.n)).unwrap_or(u64::MAX)
    }

    /// Number of "wide" buckets (seg_len + 1 keys).
    pub(crate) fn remainder(&self) -> u64 {
        let domain = i128::from(self.max) - i128::from(self.min) + 1;
        u64::try_from(domain % i128::from(self.n)).unwrap_or(0)
    }

    /// Inclusive key range of bucket `b`: [start, end], with
    /// start = min + b*seg_len + min(b, rem) and width from `seg_len`.
    pub(crate) fn range(&self, b: u64) -> (i64, i64) {
        assert!(b < self.n, "bucket {b} out of range (n = {})", self.n);
        let domain = i128::from(self.max) - i128::from(self.min) + 1;
        let (seg, rem) = (domain / i128::from(self.n), domain % i128::from(self.n));
        debug_assert_eq!(rem * (seg + 1) + (i128::from(self.n) - rem) * seg, domain);
        let width = if u128::from(b) < u128::try_from(rem).unwrap_or(0) {
            seg + 1
        } else {
            seg
        };
        let start = i128::from(self.min)
            + i128::from(b) * seg
            + i128::from(b.min(u64::try_from(rem).unwrap_or(0)));
        let end = start + width - 1;
        debug_assert!(start >= i128::from(self.min) && end <= i128::from(self.max));
        (
            i64::try_from(start).expect("range start within key domain"),
            i64::try_from(end).expect("range end within key domain"),
        )
    }

    /// Bucket owning key `k` (inverse of `range`); callers only pass keys
    /// read from the table, i.e. within [min, max].
    pub(crate) fn locate(&self, k: i64) -> u64 {
        let domain = i128::from(self.max) - i128::from(self.min) + 1;
        let (seg, rem) = (domain / i128::from(self.n), domain % i128::from(self.n));
        let wide = seg + 1;
        let off = i128::from(k) - i128::from(self.min);
        let rem64 = u64::try_from(rem).unwrap_or(0);
        if off < rem * wide {
            u64::try_from(off / wide).unwrap_or(rem64)
        } else {
            rem64 + u64::try_from((off - rem * wide) / seg).unwrap_or(0)
        }
    }

    /// SQL predicate selecting this bucket's keys, spliced into checksum
    /// pulls: `(k >= a AND k <= b)` — sargable, PK-index-friendly.
    pub(crate) fn range_predicate(&self, b: u64) -> String {
        self.range_predicate_for(&self.key_column, b)
    }

    /// Same predicate with an explicit (already quoted) key name: the two
    /// sides may spell the same logical key differently, so each side's pull
    /// must name its own column.
    pub(crate) fn range_predicate_for(&self, key: &str, b: u64) -> String {
        let (lo, hi) = self.range(b);
        format!("({key} >= {lo} AND {key} <= {hi})")
    }
}

pub(crate) struct BucketDiffer;

#[async_trait::async_trait]
impl DiffStrategy for BucketDiffer {
    fn name(&self) -> &'static str {
        "bucketdiff"
    }

    async fn diff(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
    ) -> Result<DiffReport, DbError> {
        let started = Utc::now();
        let mut queries = 0u64;

        ctx.vlog(format!(
            "[delta-diff] strategy=bucketdiff consistency={} threads={} threshold={}",
            ctx.consistency.as_str(),
            ctx.threads,
            ctx.bisection_threshold
        ));

        if ctx.consistency == ConsistencyMode::Snapshot {
            open_snapshot(left, ctx.verbose).await?;
            open_snapshot(right, ctx.verbose).await?;
            let _ = ctx.scns.set((
                crate::delta_diff::hash_diff::capture_scn(left, ctx.verbose).await?,
                crate::delta_diff::hash_diff::capture_scn(right, ctx.verbose).await?,
            ));
        }

        let result = self.diff_inner(left, right, ctx, &mut queries).await;

        if ctx.consistency == ConsistencyMode::Snapshot {
            ctx.vlog("[sql] COMMIT");
            let _ = left.query_drop("COMMIT").await;
            let _ = right.query_drop("COMMIT").await;
        }
        let (buckets, diff_rows, bucket_count) = result?;

        let mut report = assemble(ctx, buckets, diff_rows, bucket_count, ctx.sample_limit);
        report.started_at = started;
        report.finished_at = Utc::now();
        report.perf.queries_total = queries;
        Ok(report)
    }
}

impl BucketDiffer {
    async fn diff_inner(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        queries: &mut u64,
    ) -> Result<(Vec<ShardResult>, Vec<DiffRow>, u64), DbError> {
        let n = self.bucket_count(left, right, ctx, queries).await?;
        let range_plan = self.probe_key_domain(left, right, ctx, n, queries).await?;
        let t0 = Instant::now();

        // WP2 (issue #77): with a usable integer key domain, per-bucket
        // predicates become contiguous PK ranges so each pull is an indexed
        // scan; the batch checksum walks n slices of [min, max] via
        // range: Some(...). Otherwise the legacy MOD(rowHash, N) path runs.
        let (lmap, rmap) = if let Some(plan) = &range_plan {
            // In-domain walk: the batch checksum GROUP BY runs per chunk of
            // the domain, then maps local chunk bucket ids onto the global
            // plan bucket space. Left and right walks interleave via join!
            run_range_checksum_maps(left, right, ctx, plan, queries).await?
        } else {
            let lspec = bucket_checksum_spec(ctx, true, n, 0, left.dialect())?;
            let rspec = bucket_checksum_spec(ctx, false, n, 0, right.dialect())?;
            let (lmap, rmap) = tokio::join!(
                run_batch_checksum(left, &lspec, ctx.verbose),
                run_batch_checksum(right, &rspec, ctx.verbose)
            );
            *queries += 2;
            (lmap?, rmap?)
        };
        let total_n = range_plan.as_ref().map_or(n, |p| p.n);

        let mut shards = Vec::new();
        if let Some(plan) = &range_plan {
            for b in 0..plan.n {
                let (lo, hi) = plan.range(b);
                let l = lmap.get(&b).copied().unwrap_or_else(ChecksumTuple::zero);
                let r = rmap.get(&b).copied().unwrap_or_else(ChecksumTuple::zero);
                let status = if l == r {
                    ShardStatus::Match
                } else {
                    ShardStatus::Diff
                };
                shards.push(shard_of_range(&plan.key_column, lo, hi, l, r, status, t0));
            }
        } else {
            for b in 0..n {
                let l = lmap.get(&b).copied().unwrap_or_else(ChecksumTuple::zero);
                let r = rmap.get(&b).copied().unwrap_or_else(ChecksumTuple::zero);
                let status = if l == r {
                    ShardStatus::Match
                } else {
                    ShardStatus::Diff
                };
                shards.push(shard_of_bucket(n, b, l, r, status, t0));
            }
        }
        let diff_buckets = maps_to_diff_buckets(&lmap, &rmap, total_n);

        let mut rows = Vec::new();
        for b in &diff_buckets {
            let (lsql, rsql) = match &range_plan {
                Some(plan) => (
                    left.dialect().render_bucket_multiset_sql(&range_pull_spec(
                        ctx,
                        true,
                        plan,
                        *b,
                        left.dialect(),
                    )?),
                    right.dialect().render_bucket_multiset_sql(&range_pull_spec(
                        ctx,
                        false,
                        plan,
                        *b,
                        right.dialect(),
                    )?),
                ),
                None => (
                    left.dialect()
                        .render_bucket_multiset_sql(&bucket_checksum_spec(
                            ctx,
                            true,
                            n,
                            *b,
                            left.dialect(),
                        )?),
                    right
                        .dialect()
                        .render_bucket_multiset_sql(&bucket_checksum_spec(
                            ctx,
                            false,
                            n,
                            *b,
                            right.dialect(),
                        )?),
                ),
            };
            ctx.vlog(format!("[sql:left] {lsql}"));
            ctx.vlog(format!("[sql:right] {rsql}"));
            let (lr, rr) = tokio::join!(left.query(&lsql), right.query(&rsql));
            *queries += 2;
            rows.extend(multiset_diff(lr?.rows, rr?.rows));
        }
        Ok((shards, rows, total_n))
    }

    /// Estimate bucket count: target ~threshold rows per bucket, capped at
    /// MAX_BUCKETS (v2.1 section 6.3). Runs before the key-domain probe and
    /// stays authoritative for the legacy MOD(rowHash, N) path.
    async fn bucket_count(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        queries: &mut u64,
    ) -> Result<u64, DbError> {
        let lf = side_filter(ctx, left.dialect().url_scheme());
        let rf = side_filter(ctx, right.dialect().url_scheme());
        let (le, re) = if lf.is_some() || rf.is_some() {
            let lsql = filtered_count_sql(
                left.dialect().url_scheme(),
                left.dialect().identifier_quote(),
                ctx.left.schema.as_deref(),
                &ctx.left.table,
                lf.as_deref(),
            );
            let rsql = filtered_count_sql(
                right.dialect().url_scheme(),
                right.dialect().identifier_quote(),
                ctx.right.schema.as_deref(),
                &ctx.right.table,
                rf.as_deref(),
            );
            ctx.vlog(format!("[sql:left] {lsql}"));
            ctx.vlog(format!("[sql:right] {rsql}"));
            let (lr, rr) = tokio::join!(left.query(&lsql), right.query(&rsql));
            (parse_count_cell(&lr?)?, parse_count_cell(&rr?)?)
        } else {
            (
                estimate_rows(left, ctx, true).await?,
                estimate_rows(right, ctx, false).await?,
            )
        };
        *queries += 2;
        let rows = le.max(re).max(1);
        let per = ctx.bisection_threshold.max(1);
        Ok((rows.div_ceil(per)).clamp(1, MAX_BUCKETS))
    }

    /// Probe the overlapping integer key domain [min, max] via MIN/MAX on
    /// both sides. Returns `None` (with a stderr note) when the table exposes
    /// no usable integer key, when either side has no rows, all-NULL keys,
    /// non-integer keys, or the ranges are disjoint; bucketdiff then keeps
    /// the legacy MOD(rowHash, N) bucketing. An engine rejection of the probe
    /// statement itself is fatal (issue #108): it aborts a snapshot session
    /// and silently downgrades `--consistency none` runs.
    async fn probe_key_domain(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        n: u64,
        queries: &mut u64,
    ) -> Result<Option<RangePlan>, DbError> {
        let Some(key_column) = range_probe_key(ctx) else {
            ctx.vlog(
                "[delta-diff] bucketdiff key-domain probe skipped: no single integer \
                 comparison key; keeping MOD(rowHash, N) bucketing",
            );
            return Ok(None);
        };
        let spec = ProbeSpec {
            key_column: key_column.clone(),
            left: KeySide {
                table: table_ref(&ctx.left),
                filter: side_filter(ctx, left.dialect().url_scheme()),
            },
            right: KeySide {
                table: table_ref(&ctx.right),
                filter: side_filter(ctx, right.dialect().url_scheme()),
            },
            max_buckets: n,
        };
        match BucketPlan::probe(left, right, &spec, ctx, queries).await {
            Ok(p) => {
                let plan = RangePlan::new(&p.key_column, p.min, p.max, p.n);
                ctx.vlog(format!(
                    "[delta-diff] bucketdiff PK-range plan: {} in [{}..={}], {} buckets",
                    plan.key_column, plan.min, plan.max, plan.n
                ));
                Ok(Some(plan))
            }
            Err(ProbeError::Domain(reason)) => {
                ctx.vlog(format!(
                    "[delta-diff] bucketdiff key-domain probe unavailable ({reason}); \
                     keeping MOD(rowHash, N) bucketing"
                ));
                Ok(None)
            }
            Err(ProbeError::Db(e)) => Err(DbError::query(format!(
                "bucketdiff key-domain probe failed on key '{key_column}': {e} \
                 (the PK-range path needs a MIN/MAX-able key on both sides; \
                 re-run with --strategy naivediff or --strategy keyeddiff to skip it)"
            ))),
        }
    }
}

/// WP2: run the MOD(rowHash, 1) batch checksum over successive key-domain
/// chunks and merge chunk-local bucket ids into the global plan space
/// (chunk k contributes buckets k*CHUNK_BUCKETS + b). MOD(h, 1) ≡ 0 makes
/// the per-chunk GROUP BY a single group; chunked only to bound per-statement
/// scan cost. 1 statement per side per chunk, 1 row per chunk.
const CHUNK_BUCKETS: u64 = 16_384;

async fn run_range_checksum_maps(
    left: &mut (dyn DbConn + Send),
    right: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    plan: &RangePlan,
    queries: &mut u64,
) -> Result<
    (
        std::collections::BTreeMap<u64, ChecksumTuple>,
        std::collections::BTreeMap<u64, ChecksumTuple>,
    ),
    DbError,
> {
    // One checksum statement per plan bucket per side: MOD(h, 1) collapses
    // the GROUP BY to a single group and the range predicate selects the
    // bucket's rows, so each group maps to exactly one global bucket id `b`.
    let mut lmap = std::collections::BTreeMap::new();
    let mut rmap = std::collections::BTreeMap::new();
    for b in 0..plan.n {
        let (lo, hi) = plan.range(b);
        let lspec = range_checksum_spec(ctx, true, plan, b, left.dialect())?;
        let rspec = range_checksum_spec(ctx, false, plan, b, right.dialect())?;
        ctx.vlog(format!(
            "[delta-diff] bucketdiff checksum slice {b}: [{lo}..={hi}]"
        ));
        let (lres, rres) = tokio::join!(
            run_batch_checksum(left, &lspec, ctx.verbose),
            run_batch_checksum(right, &rspec, ctx.verbose)
        );
        merge_chunk_map(&mut lmap, lres?, b);
        merge_chunk_map(&mut rmap, rres?, b);
        *queries += 2;
    }
    Ok((lmap, rmap))
}

/// Remap a single-group (modulus 1) chunk result onto global bucket id
/// `global_id`; additive merge keeps repeated-row (multiset) counts.
fn merge_chunk_map(
    out: &mut std::collections::BTreeMap<u64, ChecksumTuple>,
    chunk: std::collections::BTreeMap<u64, ChecksumTuple>,
    global_id: u64,
) {
    for (_b, tuple) in chunk {
        let slot = out.entry(global_id).or_insert_with(ChecksumTuple::zero);
        slot.count += tuple.count;
        for i in 0..4 {
            slot.s[i] = slot.s[i].wrapping_add(tuple.s[i]);
        }
    }
}

/// Per-bucket multiset pull spec on the range path: MOD(h, 1) keeps the
/// bucket cond (renderers require the field) while neutralizing hash
/// filtering; the PK-range predicate selects the bucket's rows. `range`
/// stays None so dialects never AND a second key predicate into the pull.
fn range_pull_spec(
    ctx: &DiffContext,
    is_left: bool,
    plan: &RangePlan,
    bucket: u64,
    dialect: &dyn crate::backend::Dialect,
) -> Result<ChecksumSqlSpec, DbError> {
    let mut spec = bucket_checksum_spec(ctx, is_left, 1, 0, dialect)?;
    let key = dialect.quote_ident(&side_range_key(ctx, is_left, &plan.key_column));
    let predicate = plan.range_predicate_for(&key, bucket);
    spec.filter = match spec.filter {
        Some(f) => Some(format!("({f}) AND {predicate}")),
        None => Some(predicate),
    };
    Ok(spec)
}

/// One range-path checksum spec for bucket `b`: the bucket's key range
/// selects the rows, the side's own key column carries the predicate.
fn range_checksum_spec(
    ctx: &DiffContext,
    is_left: bool,
    plan: &RangePlan,
    b: u64,
    dialect: &dyn crate::backend::Dialect,
) -> Result<ChecksumSqlSpec, DbError> {
    let mut spec = bucket_checksum_spec(ctx, is_left, 1, 0, dialect)?;
    spec.key_column = Some(side_range_key(ctx, is_left, &plan.key_column));
    let (lo, hi) = plan.range(b);
    spec.range = Some((lo, hi + 1));
    Ok(spec)
}

/// This side's physical key column for the range path, falling back to the
/// plan's display name when the pairing produced no per-side key.
fn side_range_key(ctx: &DiffContext, is_left: bool, fallback: &str) -> String {
    ctx.side_key_columns(is_left)
        .first()
        .cloned()
        .unwrap_or_else(|| fallback.to_owned())
}

fn table_ref(side: &crate::delta_diff::strategy::SideCtx) -> TableRef {
    TableRef {
        connection: side.connection_name.clone(),
        schema: side.schema.clone(),
        table: side.table.clone(),
    }
}

/// Key column eligible for the PK-range path, or `None` to keep MOD bucketing
/// without spending a probe query (issue #108).
///
/// A keyless table has no key domain at all, and a key whose declared type
/// cannot be an integer cannot produce one: `MIN(name)` on a text column
/// either fails to parse or (on strict engines) makes the range predicate a
/// type error. Both cases used to run the probe against a hardcoded `id`
/// column, which aborted the snapshot transaction on PostgreSQL-family
/// engines and turned every later statement into "current transaction is
/// aborted".
fn range_probe_key(ctx: &DiffContext) -> Option<String> {
    let key = ctx.key_column.trim();
    if key.is_empty() {
        return None;
    }
    if !ctx.left.plan.column_type_may_be_integer(key)
        || !ctx.right.plan.column_type_may_be_integer(key)
    {
        return None;
    }
    Some(key.to_owned())
}

fn shard_of_range(
    key_column: &str,
    lo: i64,
    hi: i64,
    l: ChecksumTuple,
    r: ChecksumTuple,
    status: ShardStatus,
    t0: Instant,
) -> ShardResult {
    ShardResult {
        shard_id: format!("{key_column}[{lo}..={hi}]"),
        key_range: (Value::from(lo), Value::from(hi)),
        left_count: l.count,
        right_count: r.count,
        diff_count: u64::from(status == ShardStatus::Diff),
        status,
        duration_ms: t0.elapsed().as_millis() as u64,
    }
}

fn maps_to_diff_buckets(
    left: &std::collections::BTreeMap<u64, ChecksumTuple>,
    right: &std::collections::BTreeMap<u64, ChecksumTuple>,
    n: u64,
) -> Vec<u64> {
    (0..n)
        .filter(|b| {
            left.get(b).copied().unwrap_or_else(ChecksumTuple::zero)
                != right.get(b).copied().unwrap_or_else(ChecksumTuple::zero)
        })
        .collect()
}

fn expected_queries(diff_buckets: u64) -> u64 {
    2 + 2 + 2 * diff_buckets
}

fn filtered_count_sql(
    scheme: &str,
    quote: char,
    schema: Option<&str>,
    table: &str,
    filter: Option<&str>,
) -> String {
    let table = crate::backend::quote_table_scheme(scheme, quote, schema, table);
    match filter {
        Some(f) => format!("SELECT COUNT(*) AS cnt FROM {table} WHERE ({f})"),
        None => format!("SELECT COUNT(*) AS cnt FROM {table}"),
    }
}

fn parse_count_cell(result: &crate::backend::QueryResult) -> Result<u64, DbError> {
    match result.rows.first().and_then(|r| r.first()) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok()))
            .ok_or_else(|| DbError::query(format!("unparseable COUNT: {n}"))),
        Some(Value::String(s)) => s
            .trim()
            .split('.')
            .next()
            .unwrap_or("")
            .parse()
            .map_err(|_| DbError::query(format!("unparseable COUNT: {s}"))),
        Some(other) => Err(DbError::query(format!("unparseable COUNT: {other}"))),
    }
}

/// 行数估算：优先读方言的轻量 catalog 统计（issue #111），无可用值时降级
/// 到精确 COUNT(*)（§7.1，仅用于分片规划，不作一致性依据）。
async fn estimate_rows(
    conn: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    is_left: bool,
) -> Result<u64, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let schema = side.schema.as_deref().unwrap_or("");
    let sql = conn.dialect().estimate_table_rows_sql(schema, &side.table);
    ctx.vlog(format!("[sql] {sql}"));
    // The GaussDB override binds ($1 schema, $2 table); the default fallback is
    // the parameterless `list_tables()` scan and is matched by name below.
    let estimate = if sql.contains("$1") {
        conn.exec(
            &sql,
            &[
                Value::String(schema.to_string()),
                Value::String(side.table.clone()),
            ],
        )
        .await
    } else {
        conn.query(&sql).await
    };
    if let Ok(result) = estimate {
        if let Some(rows) = parse_row_estimate(&result, side.schema.as_deref(), &side.table) {
            return Ok(rows);
        }
    }

    // Catalog statistics are unavailable, stale, or non-positive (openGauss
    // never-ANALYZEd tables report reltuples = -1): fall back to an exact
    // count. Estimates are unfiltered by design, so no side filter is applied.
    let count_sql = filtered_count_sql(
        conn.dialect().url_scheme(),
        conn.dialect().identifier_quote(),
        side.schema.as_deref(),
        &side.table,
        None,
    );
    ctx.vlog(format!("[sql] {count_sql}"));
    let result = conn.query(&count_sql).await?;
    parse_count_cell(&result)
}

/// Pull a positive row-count estimate out of an estimate result. Handles both
/// the single-cell catalog estimate (`row_count`, one row) and the legacy
/// `list_tables()` shape, where the requested table is matched by name.
/// `None` means "no usable estimate" (absent, NULL, non-numeric, or <= 0) and
/// the caller must fall back to an exact `COUNT(*)`.
fn parse_row_estimate(
    result: &crate::backend::QueryResult,
    schema: Option<&str>,
    table: &str,
) -> Option<u64> {
    let ri = result.columns.iter().position(|c| c == "row_count")?;
    let Some(ti) = result.columns.iter().position(|c| c == "table_name") else {
        // Single-cell catalog estimate: trust it only when positive.
        return result
            .rows
            .first()
            .and_then(|r| r.get(ri))
            .and_then(value_as_u64)
            .filter(|n| *n > 0);
    };
    let si = result.columns.iter().position(|c| c == "schema_name");
    for row in &result.rows {
        let schema_matches = match (schema, si.and_then(|i| row.get(i))) {
            (Some(want), Some(Value::String(got))) => want == got,
            _ => true,
        };
        if schema_matches && row.get(ti) == Some(&Value::String(table.to_string())) {
            return row.get(ri).and_then(value_as_u64).filter(|n| *n > 0);
        }
    }
    None
}

/// Best-effort unsigned integer from a JSON cell (numbers, or numeric strings
/// as returned by some catalog aggregations). Non-positive values are reported
/// as-is; callers decide whether to reject them.
fn value_as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok())),
        Value::String(s) => s.trim().split('.').next().unwrap_or("").parse().ok(),
        _ => None,
    }
}

fn bucket_checksum_spec(
    ctx: &DiffContext,
    is_left: bool,
    modulus: u64,
    bucket: u64,
    dialect: &dyn crate::backend::Dialect,
) -> Result<ChecksumSqlSpec, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    Ok(ChecksumSqlSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        key_column: None,
        range: None,
        bucket: Some((modulus, bucket)),
        filter: side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
        normalized_exprs: side.plan.normalized_exprs(dialect)?,
        key_hash_exprs: vec![],
    })
}

/// 客户端多重集合比对：hash → 两侧次数差即差异（§6.3 multiset 语义）。
fn multiset_diff(lrows: Vec<Vec<Value>>, rrows: Vec<Vec<Value>>) -> Vec<DiffRow> {
    let lm = count_map(lrows);
    let rm = count_map(rrows);
    let mut out = Vec::new();
    let mut keys: Vec<&String> = lm.keys().chain(rm.keys()).collect();
    keys.sort();
    keys.dedup();
    for h in keys {
        let lc = lm.get(h).copied().unwrap_or(0);
        let rc = rm.get(h).copied().unwrap_or(0);
        if lc == rc {
            continue;
        }
        let status = if lc > rc {
            DiffStatus::MissingRight
        } else {
            DiffStatus::MissingLeft
        };
        out.push(DiffRow {
            key: Value::String(format!("{}…(×{}/{})", &h[..12.min(h.len())], lc, rc)),
            left: Some(vec![Value::String(h.clone()), Value::from(lc)]),
            right: Some(vec![Value::String(h.clone()), Value::from(rc)]),
            status,
            confirmed: true,
        });
    }
    out
}

fn count_map(rows: Vec<Vec<Value>>) -> HashMap<String, u64> {
    let mut m = HashMap::new();
    for row in rows {
        let h = match row.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let c = match row.get(1) {
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
            _ => 0,
        };
        m.insert(h, c);
    }
    m
}

fn shard_of_bucket(
    modulus: u64,
    bucket: u64,
    l: ChecksumTuple,
    r: ChecksumTuple,
    status: ShardStatus,
    t0: Instant,
) -> ShardResult {
    ShardResult {
        shard_id: format!("bucket-{bucket}/{modulus}"),
        key_range: (Value::from(bucket), Value::from(bucket + 1)),
        left_count: l.count,
        right_count: r.count,
        diff_count: u64::from(status == ShardStatus::Diff),
        status,
        duration_ms: t0.elapsed().as_millis() as u64,
    }
}

fn assemble(
    ctx: &DiffContext,
    shards: Vec<ShardResult>,
    diff_rows: Vec<DiffRow>,
    bucket_count: u64,
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
    let mut warnings: Vec<String> = ctx
        .left
        .plan
        .warnings
        .iter()
        .chain(ctx.right.plan.warnings.iter())
        .chain(ctx.route_warnings.iter())
        .cloned()
        .collect();
    let note = format!(
        "note: keyless table diff reports row-content multiset differences only \
         (buckets={bucket_count})"
    );
    if !warnings
        .iter()
        .any(|w| w.starts_with("note: keyless table diff"))
    {
        warnings.push(note);
    }
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
        strategy: "bucketdiff".into(),
        consistency: ctx.consistency.as_str().into(),
        hash_algorithm: "md5".into(),
        summary,
        perf: PerfMetrics::default(),
        shards,
        sample_diffs: diff_rows,
        warnings,
        row_payload: RowPayload::HashCount,
        key_columns: vec![],
        value_columns: vec![],
        column_data_types: vec![],
        ident_quote: '"',
        ident_scheme: String::new(),
        backslash_escape: false,
        modified_columns: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_pool() -> std::sync::Arc<dyn crate::backend::DbPool> {
        struct Pool;
        #[async_trait::async_trait]
        impl crate::backend::DbPool for Pool {
            async fn acquire(
                &self,
            ) -> Result<Box<dyn crate::backend::DbConn + Send>, crate::backend::DbError>
            {
                Err(crate::backend::DbError::unsupported("dummy"))
            }
        }
        std::sync::Arc::new(Pool)
    }

    #[test]
    fn assemble_keeps_hash_count_payload_despite_plan_columns() {
        let plan = crate::delta_diff::metadata::TablePlan {
            url_scheme: "mysql".into(),
            key_columns: vec!["id".into()],
            compare_columns: vec!["id".into(), "name".into()],
            norm_specs: vec![],
            warnings: vec![],
        };
        let ctx = DiffContext {
            left: crate::delta_diff::strategy::SideCtx {
                connection_name: "left".into(),
                schema: None,
                table: "t".into(),
                plan: plan.clone(),
            },
            right: crate::delta_diff::strategy::SideCtx {
                connection_name: "right".into(),
                schema: None,
                table: "t".into(),
                plan,
            },
            left_pool: dummy_pool(),
            right_pool: dummy_pool(),
            key_column: String::new(),
            key_columns: vec![],
            left_key_columns: vec![],
            right_key_columns: vec![],
            filter: None,
            incremental: None,
            bisection_factor: 32,
            bisection_threshold: 16_384,
            sample_limit: 20,
            threads: 4,
            consistency: ConsistencyMode::None,
            recheck: false,
            route_warnings: vec![],
            checkpoint: None,
            iblt_capacity: 65_536,
            fetch_all_threshold: 4096,
            naive_max_rows: 4096,
            strict: false,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        };

        let report = assemble(&ctx, vec![], vec![], 1, 20);

        assert!(report.key_columns.is_empty());
        assert!(report.value_columns.is_empty());
        assert_eq!(
            report.row_payload,
            crate::delta_diff::report::RowPayload::HashCount
        );
    }

    #[test]
    fn maps_to_diff_buckets_treats_missing_as_zero() {
        let mut l = std::collections::BTreeMap::new();
        l.insert(
            1,
            ChecksumTuple {
                count: 5,
                s: [1, 0, 0, 0],
            },
        );
        let mut r = std::collections::BTreeMap::new();
        r.insert(
            1,
            ChecksumTuple {
                count: 5,
                s: [1, 0, 0, 0],
            },
        );
        r.insert(
            2,
            ChecksumTuple {
                count: 3,
                s: [9, 0, 0, 0],
            },
        );
        assert_eq!(maps_to_diff_buckets(&l, &r, 4), vec![2]);
    }

    #[test]
    fn batch_query_count_formula() {
        assert_eq!(expected_queries(5), 2 + 2 + 10);
    }

    #[test]
    fn multiset_diff_counts_direction() {
        let l = vec![
            vec![Value::from("aaa"), Value::from(2)],
            vec![Value::from("bbb"), Value::from(1)],
            vec![Value::from("ccc"), Value::from(1)],
        ];
        let r = vec![
            vec![Value::from("aaa"), Value::from(1)],
            vec![Value::from("bbb"), Value::from(3)],
        ];
        let diffs = multiset_diff(l, r);
        assert_eq!(diffs.len(), 3);
        assert!(diffs
            .iter()
            .any(|d| d.status == DiffStatus::MissingRight && d.key.to_string().contains("aaa")));
        assert!(diffs
            .iter()
            .any(|d| d.status == DiffStatus::MissingLeft && d.key.to_string().contains("bbb")));
        assert!(diffs
            .iter()
            .any(|d| d.status == DiffStatus::MissingRight && d.key.to_string().contains("ccc")));
    }

    #[test]
    fn multiset_diff_identical_empty() {
        let l = vec![vec![Value::from("aaa"), Value::from(2)]];
        let r = vec![vec![Value::from("aaa"), Value::from(2)]];
        assert!(multiset_diff(l, r).is_empty());
    }

    fn estimate_result(columns: &[&str], rows: Vec<Vec<Value>>) -> crate::backend::QueryResult {
        crate::backend::QueryResult {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            row_count: rows.len(),
            rows,
            rows_affected: None,
        }
    }

    // ── key-domain probe gating (issue #108) ──

    /// Records every SQL it is asked to run and answers with a canned reply,
    /// so probe gating can be asserted without a database.
    struct RecordingConn {
        dialect: crate::backend::mysql::dialect::MySqlDialect,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        reply: ProbeReply,
    }

    use crate::backend::QueryResult;

    enum ProbeReply {
        MinMax(QueryResult),
        Fail(&'static str),
    }

    impl RecordingConn {
        fn recording(reply: ProbeReply) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Self {
                    dialect: crate::backend::mysql::dialect::MySqlDialect,
                    seen: std::sync::Arc::clone(&seen),
                    reply,
                },
                seen,
            )
        }
    }

    #[async_trait::async_trait]
    impl DbConn for RecordingConn {
        async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            self.seen.lock().expect("lock").push(sql.to_string());
            match &self.reply {
                ProbeReply::MinMax(r) => Ok(r.clone()),
                ProbeReply::Fail(msg) => Err(DbError::query((*msg).to_string())),
            }
        }

        async fn exec(&mut self, _sql: &str, _params: &[Value]) -> Result<QueryResult, DbError> {
            Err(DbError::unsupported("recording: exec not supported"))
        }

        async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
            Err(DbError::unsupported("recording: query_drop not supported"))
        }

        fn dialect(&self) -> &dyn crate::backend::Dialect {
            &self.dialect
        }
    }

    fn min_max_result(min: Value, max: Value, nulls: u64) -> QueryResult {
        QueryResult {
            columns: vec!["mn".into(), "mx".into(), "nulls".into()],
            row_count: 1,
            rows: vec![vec![min, max, Value::from(nulls)]],
            rows_affected: None,
        }
    }

    #[test]
    fn row_estimate_reads_single_cell_catalog_stat() {
        let r = estimate_result(&["row_count"], vec![vec![Value::from(42)]]);
        assert_eq!(parse_row_estimate(&r, Some("public"), "orders"), Some(42));
        let r = estimate_result(&["row_count"], vec![vec![Value::String("123".into())]]);
        assert_eq!(parse_row_estimate(&r, None, "orders"), Some(123));
    }

    #[test]
    fn row_estimate_signals_fallback_when_unusable() {
        for rows in [
            vec![],
            vec![vec![Value::from(0)]],
            vec![vec![Value::from(-1)]],
            vec![vec![Value::Null]],
            vec![vec![Value::String("stale".into())]],
        ] {
            let r = estimate_result(&["row_count"], rows);
            assert_eq!(
                parse_row_estimate(&r, None, "orders"),
                None,
                "unusable estimate must fall back to COUNT(*)"
            );
        }
    }

    #[test]
    fn row_estimate_matches_named_table_in_list_shape() {
        let r = estimate_result(
            &["schema_name", "table_name", "row_count"],
            vec![
                vec![Value::from("other"), Value::from("t"), Value::from(99)],
                vec![Value::from("s"), Value::from("t"), Value::from(7)],
            ],
        );
        assert_eq!(parse_row_estimate(&r, Some("s"), "t"), Some(7));
        assert_eq!(parse_row_estimate(&r, None, "missing"), None);
        assert_eq!(parse_row_estimate(&r, Some("nope"), "t"), None);
    }

    fn probe_plan(columns: &[(&str, &str)]) -> crate::delta_diff::metadata::TablePlan {
        crate::delta_diff::metadata::TablePlan {
            url_scheme: "mysql".into(),
            key_columns: vec![],
            compare_columns: columns.iter().map(|(n, _)| (*n).to_string()).collect(),
            norm_specs: columns
                .iter()
                .map(|(n, ty)| crate::backend::ColumnNormSpec {
                    name: (*n).to_string(),
                    data_type: (*ty).to_string(),
                    nullable: true,
                    rtrim_fixed_char: false,
                })
                .collect(),
            warnings: vec![],
        }
    }

    /// DiffContext with a single comparison key (`None` = keyless table).
    fn probe_ctx(key: Option<(&str, &str)>) -> DiffContext {
        let mut plan = probe_plan(&[("a", "int"), ("b", "varchar(32)")]);
        if let Some((name, ty)) = key {
            plan.key_columns = vec![name.to_string()];
            plan.compare_columns.insert(0, name.to_string());
            plan.norm_specs.insert(
                0,
                crate::backend::ColumnNormSpec {
                    name: name.to_string(),
                    data_type: ty.to_string(),
                    nullable: false,
                    rtrim_fixed_char: false,
                },
            );
        }
        let (key_column, key_columns, left_key, right_key) = match key {
            Some((name, _)) => (
                name.to_string(),
                vec![name.to_string()],
                vec![name.to_string()],
                vec![name.to_string()],
            ),
            None => (String::new(), vec![], vec![], vec![]),
        };
        DiffContext {
            left: crate::delta_diff::strategy::SideCtx {
                connection_name: "left".into(),
                schema: None,
                table: "t".into(),
                plan: plan.clone(),
            },
            right: crate::delta_diff::strategy::SideCtx {
                connection_name: "right".into(),
                schema: None,
                table: "t".into(),
                plan,
            },
            left_pool: dummy_pool(),
            right_pool: dummy_pool(),
            key_column,
            key_columns,
            left_key_columns: left_key,
            right_key_columns: right_key,
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
            naive_max_rows: 4096,
            strict: false,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        }
    }

    #[tokio::test]
    async fn keyless_context_never_probes_a_key_domain() {
        let ctx = probe_ctx(None);
        let (mut left, lseen) = RecordingConn::recording(ProbeReply::MinMax(min_max_result(
            Value::from(1),
            Value::from(9),
            0,
        )));
        let (mut right, rseen) = RecordingConn::recording(ProbeReply::MinMax(min_max_result(
            Value::from(1),
            Value::from(9),
            0,
        )));
        let mut queries = 0;

        let plan = BucketDiffer
            .probe_key_domain(&mut left, &mut right, &ctx, 4, &mut queries)
            .await
            .expect("gating must not fail the diff");

        assert!(plan.is_none(), "keyless tables keep MOD(rowHash, N)");
        assert!(
            lseen.lock().expect("lock").is_empty() && rseen.lock().expect("lock").is_empty(),
            "keyless tables must not probe any key domain: {:?} / {:?}",
            lseen.lock().expect("lock"),
            rseen.lock().expect("lock")
        );
        assert_eq!(queries, 0, "a skipped probe costs no queries");
    }

    #[tokio::test]
    async fn text_key_context_never_probes_a_key_domain() {
        let ctx = probe_ctx(Some(("code", "varchar(64)")));
        let (mut left, lseen) = RecordingConn::recording(ProbeReply::Fail("probe must not run"));
        let (mut right, rseen) = RecordingConn::recording(ProbeReply::Fail("probe must not run"));
        let mut queries = 0;

        let plan = BucketDiffer
            .probe_key_domain(&mut left, &mut right, &ctx, 4, &mut queries)
            .await
            .expect("gating must not fail the diff");

        assert!(plan.is_none());
        assert!(
            lseen.lock().expect("lock").is_empty() && rseen.lock().expect("lock").is_empty(),
            "a non-integer key cannot yield an integer key domain"
        );
    }

    #[tokio::test]
    async fn integer_key_context_still_probes_a_key_domain() {
        let ctx = probe_ctx(Some(("id", "bigint")));
        let (mut left, lseen) = RecordingConn::recording(ProbeReply::MinMax(min_max_result(
            Value::from(1),
            Value::from(9),
            0,
        )));
        let (mut right, _rseen) = RecordingConn::recording(ProbeReply::MinMax(min_max_result(
            Value::from(1),
            Value::from(9),
            0,
        )));
        let mut queries = 0;

        let plan = BucketDiffer
            .probe_key_domain(&mut left, &mut right, &ctx, 4, &mut queries)
            .await
            .expect("integer key probe must succeed");

        let plan = plan.expect("integer key keeps the PK-range path");
        assert_eq!(plan.key_column, "id");
        assert_eq!((plan.min, plan.max), (1, 9));
        assert_eq!(queries, 2, "one probe statement per side");
        assert_eq!(
            lseen.lock().expect("lock").len(),
            1,
            "exactly one probe statement per side"
        );
    }

    #[tokio::test]
    async fn probe_statement_failure_is_fatal_not_silently_downgraded() {
        let ctx = probe_ctx(Some(("id", "bigint")));
        let (mut left, _lseen) =
            RecordingConn::recording(ProbeReply::Fail("column \"id\" does not exist"));
        let (mut right, _rseen) = RecordingConn::recording(ProbeReply::MinMax(min_max_result(
            Value::from(1),
            Value::from(9),
            0,
        )));
        let mut queries = 0;

        let err = BucketDiffer
            .probe_key_domain(&mut left, &mut right, &ctx, 4, &mut queries)
            .await
            .expect_err("a failed probe statement must surface");

        let msg = err.to_string();
        assert!(
            msg.contains("does not exist") && msg.contains("key 'id'"),
            "engine error and failing key must both be reported: {msg}"
        );
    }

    #[tokio::test]
    async fn unusable_key_domain_still_falls_back_to_mod_bucketing() {
        let ctx = probe_ctx(Some(("id", "bigint")));
        let reply = || ProbeReply::MinMax(min_max_result(Value::Null, Value::Null, 3));
        let (mut left, _lseen) = RecordingConn::recording(reply());
        let (mut right, _rseen) = RecordingConn::recording(reply());
        let mut queries = 0;

        let plan = BucketDiffer
            .probe_key_domain(&mut left, &mut right, &ctx, 4, &mut queries)
            .await
            .expect("a data condition must not fail the diff");

        assert!(
            plan.is_none(),
            "all-NULL keys are a data condition, not a probe failure"
        );
    }

    // ── per-side physical key names on the range path (issue #108) ──

    /// Same logical key spelled differently on each side, as GaussDB/Oracle
    /// casing produces (left `ID`, right `rid`) — the paired per-side key
    /// names, not the shared logical name, must reach every statement.
    fn probe_ctx_with_side_keys(left_key: &str, right_key: &str) -> DiffContext {
        let mut ctx = probe_ctx(Some(("id", "bigint")));
        ctx.left_key_columns = vec![left_key.to_string()];
        ctx.right_key_columns = vec![right_key.to_string()];
        ctx
    }

    #[tokio::test]
    async fn key_domain_probe_names_each_side_key_column() {
        let ctx = probe_ctx_with_side_keys("ID", "rid");
        let reply = || ProbeReply::MinMax(min_max_result(Value::from(0), Value::from(10), 0));
        let (mut left, lseen) = RecordingConn::recording(reply());
        let (mut right, rseen) = RecordingConn::recording(reply());
        let mut queries = 0;

        BucketDiffer
            .probe_key_domain(&mut left, &mut right, &ctx, 4, &mut queries)
            .await
            .expect("probe");

        let lsql = lseen.lock().expect("lock").join(";");
        let rsql = rseen.lock().expect("lock").join(";");
        assert!(lsql.contains("MIN(`ID`)"), "left probe SQL: {lsql}");
        assert!(rsql.contains("MIN(`rid`)"), "right probe SQL: {rsql}");
    }

    #[test]
    fn range_checksum_spec_names_each_side_key_column() {
        let ctx = probe_ctx_with_side_keys("ID", "rid");
        let plan = RangePlan::new("ID", 0, 10, 2);
        let dialect = crate::backend::mysql::dialect::MySqlDialect;

        let ls = range_checksum_spec(&ctx, true, &plan, 0, &dialect).expect("left spec");
        let rs = range_checksum_spec(&ctx, false, &plan, 0, &dialect).expect("right spec");

        assert_eq!(ls.key_column.as_deref(), Some("ID"));
        assert_eq!(ls.range, Some((0, 6)));
        assert_eq!(rs.key_column.as_deref(), Some("rid"));
        assert_eq!(rs.range, Some((0, 6)));
    }

    #[test]
    fn range_pull_spec_filters_on_each_side_key_column() {
        let ctx = probe_ctx_with_side_keys("ID", "rid");
        let plan = RangePlan::new("ID", 0, 10, 2);
        let dialect = crate::backend::mysql::dialect::MySqlDialect;

        let lp = range_pull_spec(&ctx, true, &plan, 1, &dialect).expect("left pull");
        let rp = range_pull_spec(&ctx, false, &plan, 1, &dialect).expect("right pull");

        assert_eq!(lp.filter.as_deref(), Some("(`ID` >= 6 AND `ID` <= 10)"));
        assert_eq!(rp.filter.as_deref(), Some("(`rid` >= 6 AND `rid` <= 10)"));
    }
}

// ─── WP2 tests: PK-range bucketing (issue #77) ──────────────────────────

#[cfg(all(test, feature = "duckdb"))]
mod range_tests {
    use super::*;
    use crate::backend::DbPool;

    fn plan() -> RangePlan {
        RangePlan::new("id", 0, 999, 16)
    }

    // ── partition math ──

    #[test]
    fn partition_covers_domain_contiguously() {
        let p = plan();
        let mut covered: Vec<(i64, i64)> = Vec::new();
        for b in 0..p.n {
            let (lo, hi) = p.range(b);
            assert!(lo <= hi, "bucket {b} inverted");
            if let Some(&(_, prev_hi)) = covered.last() {
                assert_eq!(lo, prev_hi + 1, "bucket {b} not contiguous");
            }
            covered.push((lo, hi));
        }
        assert_eq!(covered[0].0, 0);
        assert_eq!(covered.last().unwrap().1, 999);
    }

    #[test]
    fn partition_single_bucket_spans_domain() {
        let p = RangePlan::new("id", -5, 5, 1);
        assert_eq!(p.n, 1);
        assert_eq!(p.range(0), (-5, 5));
    }

    #[test]
    fn partition_buckets_shrink_when_domain_smaller_than_n() {
        // domain = 3 keys but 8 buckets requested → halve until n <= domain.
        let p = RangePlan::new("id", 0, 2, 8);
        assert_eq!(p.n, 3, "buckets clamp to domain size: one key per bucket");
        assert_eq!(p.range(0), (0, 0));
        assert_eq!(p.range(1), (1, 1));
        assert_eq!(p.range(2), (2, 2));
    }

    #[test]
    fn partition_min_equals_max_is_single_one_key_bucket() {
        let p = RangePlan::new("id", 42, 42, 16);
        assert_eq!(p.n, 1);
        assert_eq!(p.range(0), (42, 42));
        assert_eq!(p.locate(42), 0);
    }

    #[test]
    fn partition_non_divisible_widths_differ_by_one() {
        // domain = 1000, n = 16 → rem = 8 wide (63 keys) + 8 narrow (62 keys)
        let p = plan();
        assert_eq!(p.seg_len(), 62);
        assert_eq!(p.remainder(), 8);
        assert_eq!(p.range(0), (0, 62));
        assert_eq!(p.range(7), (441, 503));
        assert_eq!(p.range(8), (504, 565));
        assert_eq!(p.range(15), (938, 999));
    }

    #[test]
    fn locate_returns_owning_bucket_for_every_domain_key() {
        for (min, max, n) in [(0i64, 999i64, 16u64), (-50, 49, 7), (1, 2, 64), (0, 0, 4)] {
            let p = RangePlan::new("id", min, max, n);
            for k in min..=max {
                let b = p.locate(k);
                assert!(b < p.n, "k={k} located outside bucket range");
                let (lo, hi) = p.range(b);
                assert!(k >= lo && k <= hi, "k={k} outside its bucket range");
            }
        }
    }

    #[test]
    fn locate_boundary_keys_land_in_adjacent_buckets() {
        let p = plan();
        assert_eq!(p.locate(62), 0);
        assert_eq!(p.locate(63), 1);
        assert_eq!(p.locate(503), 7);
        assert_eq!(p.locate(504), 8);
        assert_eq!(p.locate(999), 15);
    }

    #[test]
    fn range_predicate_is_sargable_between() {
        let p = plan();
        assert_eq!(p.range_predicate(3), "(id >= 189 AND id <= 251)");
    }

    #[test]
    fn drill_down_assigns_each_key_to_exactly_one_bucket() {
        let p = RangePlan::new("id", 400_000, 400_099, 8);
        let mut hits = vec![0u32; p.n as usize];
        for k in 400_000..=400_099 {
            hits[p.locate(k) as usize] += 1;
        }
        assert_eq!(hits.iter().sum::<u32>(), 100);
        assert!(hits.iter().all(|&h| h == 12 || h == 13));
    }

    // ── probe against real (embedded) DuckDB ──

    async fn duck_pool() -> Box<dyn DbPool> {
        Box::new(
            crate::backend::duckdb::pool::create_duckdb_pool("duckdb://:memory:")
                .await
                .expect("pool"),
        )
    }

    async fn make_table(pool: &dyn DbPool, rows: &[(i64, &str)]) {
        let mut conn = pool.acquire().await.expect("acquire");
        conn.query("CREATE OR REPLACE TABLE t (id BIGINT, v VARCHAR)")
            .await
            .expect("create");
        for (id, v) in rows {
            conn.query(&format!("INSERT INTO t VALUES ({id}, '{v}')"))
                .await
                .expect("insert");
        }
    }

    fn dummy_pool() -> std::sync::Arc<dyn DbPool> {
        struct Pool;
        #[async_trait::async_trait]
        impl DbPool for Pool {
            async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
                Err(DbError::unsupported("dummy"))
            }
        }
        std::sync::Arc::new(Pool)
    }

    fn test_ctx() -> DiffContext {
        fn side_plan() -> crate::delta_diff::metadata::TablePlan {
            crate::delta_diff::metadata::TablePlan {
                url_scheme: "duckdb".into(),
                key_columns: vec!["id".into()],
                compare_columns: vec!["id".into(), "v".into()],
                norm_specs: vec![],
                warnings: vec![],
            }
        }
        DiffContext {
            left: crate::delta_diff::strategy::SideCtx {
                connection_name: "l".into(),
                schema: None,
                table: "t".into(),
                plan: side_plan(),
            },
            right: crate::delta_diff::strategy::SideCtx {
                connection_name: "r".into(),
                schema: None,
                table: "t".into(),
                plan: side_plan(),
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
            consistency: ConsistencyMode::None,
            recheck: false,
            route_warnings: vec![],
            checkpoint: None,
            iblt_capacity: 65_536,
            fetch_all_threshold: 4096,
            naive_max_rows: 4096,
            strict: false,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        }
    }

    async fn probe(
        lpool: &dyn DbPool,
        rpool: &dyn DbPool,
        lrows: &[(i64, &str)],
        rrows: &[(i64, &str)],
        lfilter: Option<&str>,
        rfilter: Option<&str>,
    ) -> Result<BucketPlan, ProbeError> {
        make_table(lpool, lrows).await;
        make_table(rpool, rrows).await;
        let (mut lc, mut rc) = (
            lpool.acquire().await.expect("l"),
            rpool.acquire().await.expect("r"),
        );
        let ctx = test_ctx();
        let mut queries = 0u64;
        let tr = |s: &crate::delta_diff::strategy::SideCtx| TableRef {
            connection: s.connection_name.clone(),
            schema: s.schema.clone(),
            table: s.table.clone(),
        };
        let spec = ProbeSpec {
            key_column: "id".to_owned(),
            left: KeySide {
                table: tr(&ctx.left),
                filter: lfilter.map(str::to_owned),
            },
            right: KeySide {
                table: tr(&ctx.right),
                filter: rfilter.map(str::to_owned),
            },
            max_buckets: 4,
        };
        BucketPlan::probe(&mut *lc, &mut *rc, &spec, &ctx, &mut queries).await
    }

    /// B2 并集语义：键域取两侧并集（部分重叠时交集会静默丢行），
    /// 这里验证重叠场景的并集边界正确。
    #[tokio::test]
    async fn probe_returns_union_domain_and_bucket_count() {
        let (l, r) = (duck_pool().await, duck_pool().await);
        let p = probe(
            &*l,
            &*r,
            &[(7, "a"), (3, "b"), (9, "c")],
            &[(3, "a"), (8, "b")],
            None,
            None,
        )
        .await
        .expect("probe");
        assert_eq!(p.min, 3);
        assert_eq!(p.max, 9, "union of [3,9] and [3,8] must reach 9");
        assert_eq!(p.n, 4);
        assert_eq!(p.key_column, "id");
    }

    #[tokio::test]
    async fn probe_skips_null_keys() {
        let (l, r) = (duck_pool().await, duck_pool().await);
        let p = probe(&*l, &*r, &[(5, "a")], &[(5, "a")], None, None)
            .await
            .expect("probe");
        assert_eq!((p.min, p.max), (5, 5));
    }

    #[tokio::test]
    async fn probe_all_null_keys_fails_with_key_domain_error() {
        let (l, r) = (duck_pool().await, duck_pool().await);
        let err = probe(&*l, &*r, &[], &[], None, None)
            .await
            .expect_err("empty/all-null key domain must error");
        assert!(err.to_string().contains("key domain"), "{err}");
    }

    #[tokio::test]
    async fn probe_uses_union_of_two_sides() {
        // 评审修复：键域取并集。旧交集实现（3..=5）会静默丢弃左
        // 1..=2、右 6..=10，部分重叠时漏 diff；并集下这些区间只有
        // 一侧有行，checksum 自然为 0，成为正常差异。
        let (l, r) = (duck_pool().await, duck_pool().await);
        let p = probe(
            &*l,
            &*r,
            &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
            &[(3, "a"), (4, "b"), (5, "c"), (6, "f"), (10, "g")],
            None,
            None,
        )
        .await
        .expect("probe");
        assert_eq!((p.min, p.max), (1, 10), "union covers both sides' keys");
    }

    #[tokio::test]
    async fn probe_partial_overlap_is_not_disjoint() {
        let (l, r) = (duck_pool().await, duck_pool().await);
        let p = probe(
            &*l,
            &*r,
            &[(1, "a"), (10, "b")],
            &[(5, "a"), (20, "b")],
            None,
            None,
        )
        .await
        .expect("partial overlap must resolve, not fall back");
        assert_eq!((p.min, p.max), (1, 20));
    }

    #[tokio::test]
    async fn probe_null_key_fails_closed_to_mod_fallback() {
        // NULL 键在范围谓词下不可见而旧 MOD 路径会计入：检测到即
        // 失败关闭，回退 MOD(rowHash, N) bucketing。
        let pool = duck_pool().await;
        let mut conn = pool.acquire().await.expect("acquire");
        conn.query("CREATE OR REPLACE TABLE t (id BIGINT, v VARCHAR)")
            .await
            .expect("create");
        conn.query("INSERT INTO t VALUES (NULL, 'n'), (1, 'a')")
            .await
            .expect("insert");
        drop(conn);
        let lpool = duck_pool().await;
        make_table(&*lpool, &[(1, "a")]).await;
        let err = probe(&*lpool, &*pool, &[(1, "a")], &[], None, None)
            .await
            .expect_err("NULL keys must fail closed");
        assert!(err.to_string().contains("NULL"), "{err}");
    }

    #[tokio::test]
    async fn probe_respects_side_filters() {
        let (l, r) = (duck_pool().await, duck_pool().await);
        let p = probe(
            &*l,
            &*r,
            &[(1, "a"), (2, "b"), (100, "c")],
            &[(2, "b"), (100, "c")],
            Some("v = 'b'"),
            Some("v = 'b'"),
        )
        .await
        .expect("probe");
        assert_eq!((p.min, p.max), (2, 2));
        assert_eq!(p.n, 4, "probe keeps the requested count; RangePlan shrinks");
    }

    /// B2 并集语义回归：不相交的键域不再报错（旧 fail-closed 基于
    /// 交集，会静默丢行）；并集 [1,10] 下单侧独有的区间 checksum
    /// 恒为 0，成为正常 diff 输出。全 NULL 键的 fail-closed 路径由
    /// probe_all_null_keys_fails_with_key_domain_error 覆盖。
    #[tokio::test]
    async fn probe_disjoint_domains_resolve_to_union() {
        let (l, r) = (duck_pool().await, duck_pool().await);
        let p = probe(
            &*l,
            &*r,
            &[(1, "a"), (2, "b")],
            &[(9, "a"), (10, "b")],
            None,
            None,
        )
        .await
        .expect("disjoint domains resolve to their union");
        assert_eq!(p.min, 1);
        assert_eq!(p.max, 10);
        assert_eq!(p.n, 4);
    }

    // ── WP2-fix regression: slice checksum SQL must carry the PK range ──

    /// 记录收到的 checksum SQL 的 mock 连接（MySQL 方言；mysql 模块
    /// 无 feature 门控，与 range_tests 的 duckdb 门控正交）。
    struct SqlCaptureConn {
        dialect: crate::backend::mysql::dialect::MySqlDialect,
        first_sql: std::sync::Arc<std::sync::Mutex<String>>,
    }

    #[async_trait::async_trait]
    impl DbConn for SqlCaptureConn {
        async fn query(&mut self, sql: &str) -> Result<crate::backend::QueryResult, DbError> {
            // 只记录第一条 SQL（bucket 0）：左右连接各持一个 Arc，
            // 共享 Arc 只能看到最后一条（bucket 15），断言无从谈起。
            let mut slot = self.first_sql.lock().unwrap();
            if slot.is_empty() {
                *slot = sql.to_string();
            }
            drop(slot);
            // render_batch_checksum_sql 的聚合无 GROUP BY，每支 1 行：
            // bkt=0, cnt=0, s1..s4 全 0。
            Ok(crate::backend::QueryResult {
                columns: vec![
                    "bkt".into(),
                    "cnt".into(),
                    "s1".into(),
                    "s2".into(),
                    "s3".into(),
                    "s4".into(),
                ],
                rows: vec![vec![
                    serde_json::json!(0),
                    serde_json::json!(0),
                    serde_json::json!(0),
                    serde_json::json!(0),
                    serde_json::json!(0),
                    serde_json::json!(0),
                ]],
                row_count: 1,
                rows_affected: None,
            })
        }
        async fn exec(
            &mut self,
            _sql: &str,
            _params: &[Value],
        ) -> Result<crate::backend::QueryResult, DbError> {
            Err(DbError::unsupported("capture"))
        }
        async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
            Ok(())
        }
        fn dialect(&self) -> &dyn crate::backend::Dialect {
            &self.dialect
        }
    }

    #[tokio::test]
    async fn range_slice_checksum_sql_carries_quoted_key_and_range() {
        // 5796485 回归防复发：`spec.range` 有值但 `key_column: None` 时
        // 方言不渲染范围谓词，每个切片退化为全表扫描（性能曾 110s→349s，
        // 且排序归并前提被破坏）。对 run_range_checksum_maps 实际下发的
        // SQL 断言同时含 quoted key_column 与 `>= lo` / `< hi+1` 谓词。
        let ctx = test_ctx();
        let lfirst = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let rfirst = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let plan = RangePlan::new("id", 0, 999, 16);
        let mut queries = 0u64;
        let mut lconn = SqlCaptureConn {
            dialect: crate::backend::mysql::dialect::MySqlDialect,
            first_sql: std::sync::Arc::clone(&lfirst),
        };
        let mut rconn = SqlCaptureConn {
            dialect: crate::backend::mysql::dialect::MySqlDialect,
            first_sql: std::sync::Arc::clone(&rfirst),
        };
        run_range_checksum_maps(&mut lconn, &mut rconn, &ctx, &plan, &mut queries)
            .await
            .expect("slice checksums");
        assert_eq!(queries, 32, "2 statements per bucket x 16 buckets");
        for (side, sql_slot) in [("left", &lfirst), ("right", &rfirst)] {
            let sql = sql_slot.lock().unwrap().clone();
            assert!(
                sql.contains("`id` >= 0") && sql.contains("`id` < 63"),
                "{side} slice must constrain the PK range with the quoted key: {sql}"
            );
            assert!(sql.contains("GROUP BY"), "checksum shape unchanged: {sql}");
        }
    }

    // ── drill-down pull: only rows inside the PK range are fetched ──

    #[tokio::test]
    async fn drill_down_pulls_only_rows_in_pk_range() {
        let pool = duck_pool().await;
        make_table(
            &*pool,
            &[(0, "a"), (1, "b"), (2, "c"), (3, "d"), (9, "e"), (10, "f")],
        )
        .await;
        let mut conn = pool.acquire().await.expect("acquire");
        let mk = |b: u64| ChecksumSqlSpec {
            schema: None,
            table: "t".into(),
            key_column: Some("id".into()),
            range: None,
            // Real range path: modulus 1 neutralizes the hash cond (all rows
            // land in one group); the PK-range predicate selects the bucket.
            bucket: Some((1, 0)),
            filter: Some(RangePlan::new("id", 0, 10, 2).range_predicate(b)),
            scn: None,
            normalized_exprs: vec!["v".into()],
            key_hash_exprs: vec![],
        };
        let sql = conn.dialect().render_bucket_multiset_sql(&mk(1));
        let r = conn.query(&sql).await.expect("multiset pull");
        assert_eq!(r.columns, vec!["h", "cnt"]);
        assert_eq!(
            r.rows.len(),
            2,
            "bucket 1 covers ids 5..=10 (only 9,10 exist)"
        );
        let sql = conn.dialect().render_bucket_multiset_sql(&mk(0));
        let r = conn.query(&sql).await.expect("multiset pull");
        assert_eq!(r.rows.len(), 4, "bucket 0 covers ids 0..=4");
    }
}
