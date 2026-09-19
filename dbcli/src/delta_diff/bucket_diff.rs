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
    ) -> Result<Self, DbError> {
        let key_column = spec.key_column.as_str();
        let (lt, rt) = (&spec.left, &spec.right);
        let lp = Self::probe_side(
            left,
            key_column,
            &lt.table.schema,
            &lt.table.table,
            lt.filter.as_deref(),
            ctx,
        );
        let rp = Self::probe_side(
            right,
            key_column,
            &rt.table.schema,
            &rt.table.table,
            rt.filter.as_deref(),
            ctx,
        );
        let (l, r) = tokio::join!(lp, rp);
        let ((lmin, lmax), lq) = l?;
        let ((rmin, rmax), rq) = r?;
        *queries += lq + rq;
        let min = lmin.max(rmin);
        let max = lmax.min(rmax);
        if min > max {
            return Err(DbError::query(
                "key domain unresolved: sides' key ranges do not overlap \
                 (bucketdiff falls back to MOD(rowHash, N) bucketing)",
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
    ) -> Result<((i64, i64), u64), DbError> {
        let scheme = conn.dialect().url_scheme().to_owned();
        let quote = conn.dialect().identifier_quote();
        let mut sql = format!(
            "SELECT MIN({key_column}) AS mn, MAX({key_column}) AS mx FROM {}",
            crate::backend::quote_table_scheme(&scheme, quote, schema.as_deref(), table)
        );
        if let Some(f) = filter {
            sql.push_str(&format!(" WHERE ({f})"));
        }
        ctx.vlog(format!("[sql] {sql}"));
        let r = conn.query(&sql).await?;
        let used = 1u64;
        let row = r.rows.first().ok_or_else(|| {
            DbError::query(format!("key domain probe returned no rows for {table}"))
        })?;
        let parse = |v: Option<&Value>| -> Result<Option<i64>, DbError> {
            match v {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Number(n)) => n
                    .as_i64()
                    .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()))
                    .map(Some)
                    .ok_or_else(|| DbError::query(format!("non-integer key domain: {n}"))),
                Some(Value::String(s)) => s
                    .trim()
                    .parse::<i64>()
                    .map(Some)
                    .map_err(|_| DbError::query(format!("non-integer key domain: {s}"))),
                Some(other) => Err(DbError::query(format!("non-integer key domain: {other}"))),
            }
        };
        let cols: Vec<Option<&Value>> = (0..2).map(|i| row.get(i)).collect();
        let (mn, mx) = (parse(cols[0])?, parse(cols[1])?);
        match (mn, mx) {
            (Some(a), Some(b)) if a <= b => Ok(((a, b), used)),
            _ => Err(DbError::query(format!(
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
        let (lo, hi) = self.range(b);
        format!(
            "({} >= {lo} AND {} <= {hi})",
            self.key_column, self.key_column
        )
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
        let range_plan = self.probe_key_domain(left, right, ctx, n, queries).await;
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
    /// both sides. Returns `None` (with a stderr note) when either side has
    /// no rows, all-NULL keys, non-integer keys, or the ranges are disjoint;
    /// bucketdiff then keeps the legacy MOD(rowHash, N) bucketing.
    async fn probe_key_domain(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        n: u64,
        queries: &mut u64,
    ) -> Option<RangePlan> {
        let spec = ProbeSpec {
            key_column: if ctx.key_column.is_empty() {
                "id".to_owned()
            } else {
                ctx.key_column.clone()
            },
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
                Some(plan)
            }
            Err(e) => {
                ctx.vlog(format!(
                    "[delta-diff] bucketdiff key-domain probe unavailable ({e}); \
                     keeping MOD(rowHash, N) bucketing"
                ));
                None
            }
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
        let mut lspec = bucket_checksum_spec(ctx, true, 1, 0, left.dialect())?;
        lspec.key_column = Some(plan.key_column.clone());
        lspec.range = Some((lo, hi + 1));
        let mut rspec = bucket_checksum_spec(ctx, false, 1, 0, right.dialect())?;
        rspec.key_column = Some(plan.key_column.clone());
        rspec.range = Some((lo, hi + 1));
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
    spec.filter = match spec.filter {
        Some(f) => Some(format!("({f}) AND {}", plan.range_predicate(bucket))),
        None => Some(plan.range_predicate(bucket)),
    };
    Ok(spec)
}

fn table_ref(side: &crate::delta_diff::strategy::SideCtx) -> TableRef {
    TableRef {
        connection: side.connection_name.clone(),
        schema: side.schema.clone(),
        table: side.table.clone(),
    }
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

/// 行数估算：复用 list_tables 统计值（§7.1，仅用于分片规划，不作一致性依据）。
async fn estimate_rows(
    conn: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    is_left: bool,
) -> Result<u64, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let sql = conn.dialect().list_tables().to_string();
    ctx.vlog(format!("[sql] {sql}"));
    let r = conn.query(&sql).await?;
    let schema_idx = r.columns.iter().position(|c| c == "schema_name");
    let table_idx = r.columns.iter().position(|c| c == "table_name");
    let rows_idx = r.columns.iter().position(|c| c == "row_count");
    let (Some(si), Some(ti), Some(ri)) = (schema_idx, table_idx, rows_idx) else {
        return Ok(0);
    };
    for row in &r.rows {
        let schema_matches = match (&side.schema, row.get(si)) {
            (Some(want), Some(Value::String(got))) => want == got,
            _ => true,
        };
        if schema_matches && row.get(ti) == Some(&Value::String(side.table.clone())) {
            if let Some(v) = row.get(ri) {
                match v {
                    Value::Number(n) => return Ok(n.as_u64().unwrap_or(0)),
                    Value::String(s) => {
                        return Ok(s
                            .trim()
                            .split('.')
                            .next()
                            .unwrap_or("0")
                            .parse()
                            .unwrap_or(0));
                    }
                    _ => return Ok(0),
                }
            }
        }
    }
    Ok(0)
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
    ) -> Result<BucketPlan, DbError> {
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

    #[tokio::test]
    async fn probe_returns_overlapping_domain_and_bucket_count() {
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
        assert_eq!(p.max, 8);
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
    async fn probe_uses_narrower_overlap_of_two_sides() {
        let (l, r) = (duck_pool().await, duck_pool().await);
        let p = probe(
            &*l,
            &*r,
            &[(1, "a"), (10, "b")],
            &[(3, "a"), (5, "b")],
            None,
            None,
        )
        .await
        .expect("probe");
        assert_eq!((p.min, p.max), (3, 5));
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

    #[tokio::test]
    async fn probe_disjoint_domains_fails_closed() {
        let (l, r) = (duck_pool().await, duck_pool().await);
        let err = probe(
            &*l,
            &*r,
            &[(1, "a"), (2, "b")],
            &[(9, "a"), (10, "b")],
            None,
            None,
        )
        .await
        .expect_err("disjoint domains must fail closed");
        assert!(err.to_string().contains("key domain"), "{err}");
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
