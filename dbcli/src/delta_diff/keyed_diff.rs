// ─── delta-diff KeyedDiffer: composite/string key identity diff ────────
//
// COUNT both sides → empty-side short-circuit → FETCH_ALL or
// scan-once bucket checksum + composite keyset merge.
// Not used for keyless tables (those stay on bucketdiff).

use chrono::Utc;
use serde_json::Value;

use std::collections::BTreeMap;

use crate::backend::{ChecksumSqlSpec, DbConn, DbError, KeysetPageSpec};
use crate::delta_diff::checksum::{run_batch_checksum, ChecksumTuple};
use crate::delta_diff::hash_diff::open_snapshot;
use crate::delta_diff::report::{
    DiffReport, DiffRow, DiffStatus, DiffSummary, PerfMetrics, ShardResult, ShardStatus, TableRef,
};
use crate::delta_diff::rowdiff::{diff_row_n, row_level_diff};
use crate::delta_diff::strategy::{side_filter, ConsistencyMode, DiffContext, DiffStrategy};

const PAGE_SIZE: usize = 8192;

pub(crate) struct KeyedDiffer;

#[async_trait::async_trait]
impl DiffStrategy for KeyedDiffer {
    fn name(&self) -> &'static str {
        "keyeddiff"
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
            "[delta-diff] strategy=keyeddiff consistency={} keys={} fetch_all_threshold={}",
            ctx.consistency.as_str(),
            ctx.key_columns.join(","),
            ctx.fetch_all_threshold
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

        let (diff_rows, left_total, right_total, counts, extra_warnings) = result?;
        let mut report = assemble(
            ctx,
            diff_rows,
            left_total,
            right_total,
            ctx.sample_limit,
            counts,
            extra_warnings,
        );
        report.started_at = started;
        report.finished_at = Utc::now();
        report.perf.queries_total = queries;
        Ok(report)
    }
}

/// 点查路径的计数覆盖：大表缺失按记账得出，不在 diff_rows 里逐行出现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SkewCounts {
    pub(crate) missing_left: u64,
    pub(crate) missing_right: u64,
    pub(crate) modified: u64,
}

impl KeyedDiffer {
    async fn diff_inner(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        queries: &mut u64,
    ) -> Result<(Vec<DiffRow>, u64, u64, Option<SkewCounts>, Vec<String>), DbError> {
        let lq = left.dialect().identifier_quote();
        let rq = right.dialect().identifier_quote();
        let lscheme = left.dialect().url_scheme();
        let rscheme = right.dialect().url_scheme();
        let lfilter = side_filter(ctx, lscheme);
        let rfilter = side_filter(ctx, rscheme);
        let lsql = render_count_sql(
            lscheme,
            lq,
            ctx.left.schema.as_deref(),
            &ctx.left.table,
            lfilter.as_deref(),
        );
        let rsql = render_count_sql(
            rscheme,
            rq,
            ctx.right.schema.as_deref(),
            &ctx.right.table,
            rfilter.as_deref(),
        );
        ctx.vlog(format!("[sql:left] {lsql}"));
        ctx.vlog(format!("[sql:right] {rsql}"));
        let (lr, rr) = tokio::join!(left.query(&lsql), right.query(&rsql));
        *queries += 2;
        let left_total = parse_count(&lr?)?;
        let right_total = parse_count(&rr?)?;

        if left_total == 0 && right_total == 0 {
            return Ok((Vec::new(), 0, 0, None, Vec::new()));
        }

        let arity = ctx.key_columns.len().max(1);

        if left_total == 0 {
            let spec = keys_only_spec(ctx, false, right.dialect())?;
            let rows = fetch_all_pages(right, &spec, ctx.verbose, queries).await?;
            let mut extra = Vec::new();
            if let Some(w) =
                fetch_count_mismatch_warning("empty-left", 0, 0, right_total, rows.len() as u64)
            {
                extra.push(w);
            }
            return Ok((
                keys_only_diffs(&rows, arity, false),
                left_total,
                right_total,
                None,
                extra,
            ));
        }
        if right_total == 0 {
            let spec = keys_only_spec(ctx, true, left.dialect())?;
            let rows = fetch_all_pages(left, &spec, ctx.verbose, queries).await?;
            let mut extra = Vec::new();
            if let Some(w) =
                fetch_count_mismatch_warning("empty-right", left_total, rows.len() as u64, 0, 0)
            {
                extra.push(w);
            }
            return Ok((
                keys_only_diffs(&rows, arity, true),
                left_total,
                right_total,
                None,
                extra,
            ));
        }

        if left_total.max(right_total) <= ctx.fetch_all_threshold {
            let lspec = full_row_spec(ctx, true, left.dialect(), None)?;
            let rspec = full_row_spec(ctx, false, right.dialect(), None)?;
            let left_numeric = full_row_numeric_flags(ctx, true);
            let right_numeric = full_row_numeric_flags(ctx, false);
            let detail = row_level_diff(
                left,
                right,
                (&lspec, &rspec),
                None,
                arity,
                (&left_numeric, &right_numeric),
                ctx.verbose,
            )
            .await?;
            *queries += detail.queries;
            let mut extra = Vec::new();
            if let Some(w) = fetch_count_mismatch_warning(
                "fetch-all",
                left_total,
                detail.left_count,
                right_total,
                detail.right_count,
            ) {
                extra.push(w);
            }
            return Ok((detail.rows, left_total, right_total, None, extra));
        }

        let both_keys_unique = ctx.left.plan.key_unique && ctx.right.plan.key_unique;
        let route = skew_route(
            left_total,
            right_total,
            ctx.fetch_all_threshold,
            ctx.summary_only,
            both_keys_unique,
        );
        let (big_total, small_total) = if left_total >= right_total {
            (left_total, right_total)
        } else {
            (right_total, left_total)
        };
        if let SkewRoute::PointQuery { big_is_left } = route {
            let plan = PointQueryPlan {
                big_is_left,
                big_total,
                small_total,
            };
            return self.point_query_diff(left, right, ctx, queries, plan).await;
        }

        let scan = self
            .checksum_buckets(left, right, ctx, queries, left_total, right_total)
            .await?;
        if scan.buckets.is_empty() {
            return Ok((Vec::new(), left_total, right_total, None, Vec::new()));
        }

        match route {
            SkewRoute::SharedScan => {
                self.shared_scan_from(left, right, ctx, queries, &scan)
                    .await
            }
            _ => self.per_bucket_from(left, right, ctx, queries, &scan).await,
        }
    }

    /// 两侧一次 `GROUP BY MOD(key_hash, N)` 校验，返回失配桶集合。
    async fn checksum_buckets(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        queries: &mut u64,
        left_total: u64,
        right_total: u64,
    ) -> Result<BucketScan, DbError> {
        let n = {
            let per = ctx.bisection_threshold.max(1);
            (left_total.max(right_total).div_ceil(per)).clamp(1, 1024)
        };
        let lspec = batch_spec(ctx, true, left.dialect(), n)?;
        let rspec = batch_spec(ctx, false, right.dialect(), n)?;
        let (lmap, rmap) = tokio::join!(
            run_batch_checksum(left, &lspec, ctx.verbose),
            run_batch_checksum(right, &rspec, ctx.verbose)
        );
        *queries += 2;
        let lmap = lmap?;
        let rmap = rmap?;
        let buckets = diff_bucket_ids(&lmap, &rmap, n);
        Ok(BucketScan {
            n,
            buckets,
            lmap,
            rmap,
            left_total,
            right_total,
        })
    }

    /// 失配桶共享一次键序扫描：MOD IN 谓词下做一次双侧归并（O(1) 次扫描）。
    async fn shared_scan_from(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        queries: &mut u64,
        scan: &BucketScan,
    ) -> Result<(Vec<DiffRow>, u64, u64, Option<SkewCounts>, Vec<String>), DbError> {
        let arity = ctx.key_columns.len().max(1);
        let lpred = left.dialect().render_bucket_set_predicate(
            &ctx.left.plan.key_hash_exprs(left.dialect())?,
            scan.n,
            &scan.buckets,
        );
        let rpred = right.dialect().render_bucket_set_predicate(
            &ctx.right.plan.key_hash_exprs(right.dialect())?,
            scan.n,
            &scan.buckets,
        );
        let lspec = full_row_spec(ctx, true, left.dialect(), Some(&lpred))?;
        let rspec = full_row_spec(ctx, false, right.dialect(), Some(&rpred))?;
        let left_numeric = full_row_numeric_flags(ctx, true);
        let right_numeric = full_row_numeric_flags(ctx, false);
        let detail = row_level_diff(
            left,
            right,
            (&lspec, &rspec),
            None,
            arity,
            (&left_numeric, &right_numeric),
            ctx.verbose,
        )
        .await?;
        *queries += detail.queries;
        let mut extra = Vec::new();
        let exp_l: u64 = scan
            .buckets
            .iter()
            .map(|b| scan.lmap.get(b).map(|t| t.count).unwrap_or(0))
            .sum();
        let exp_r: u64 = scan
            .buckets
            .iter()
            .map(|b| scan.rmap.get(b).map(|t| t.count).unwrap_or(0))
            .sum();
        if let Some(w) = fetch_count_mismatch_warning(
            "shared-scan",
            exp_l,
            detail.left_count,
            exp_r,
            detail.right_count,
        ) {
            extra.push(w);
        }
        Ok((detail.rows, scan.left_total, scan.right_total, None, extra))
    }

    /// 既有逐桶 MOD 分页归并（比例 < 8 的锁定形状）。
    async fn per_bucket_from(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        queries: &mut u64,
        scan: &BucketScan,
    ) -> Result<(Vec<DiffRow>, u64, u64, Option<SkewCounts>, Vec<String>), DbError> {
        let arity = ctx.key_columns.len().max(1);
        let mut rows = Vec::new();
        let mut extra = Vec::new();
        for b in &scan.buckets {
            let lpred = left.dialect().render_bucket_predicate(
                &ctx.left.plan.key_hash_exprs(left.dialect())?,
                scan.n,
                *b,
            );
            let rpred = right.dialect().render_bucket_predicate(
                &ctx.right.plan.key_hash_exprs(right.dialect())?,
                scan.n,
                *b,
            );
            let lspec = full_row_spec(ctx, true, left.dialect(), Some(&lpred))?;
            let rspec = full_row_spec(ctx, false, right.dialect(), Some(&rpred))?;
            let left_numeric = full_row_numeric_flags(ctx, true);
            let right_numeric = full_row_numeric_flags(ctx, false);
            let detail = row_level_diff(
                left,
                right,
                (&lspec, &rspec),
                None,
                arity,
                (&left_numeric, &right_numeric),
                ctx.verbose,
            )
            .await?;
            *queries += detail.queries;
            let exp_l = scan.lmap.get(b).map(|t| t.count).unwrap_or(0);
            let exp_r = scan.rmap.get(b).map(|t| t.count).unwrap_or(0);
            if let Some(w) = fetch_count_mismatch_warning(
                &format!("bucket {b}"),
                exp_l,
                detail.left_count,
                exp_r,
                detail.right_count,
            ) {
                extra.push(w);
            }
            rows.extend(detail.rows);
        }
        Ok((rows, scan.left_total, scan.right_total, None, extra))
    }

    /// 倾斜点查：小表整读 + 大表分块等值点查 + 客户端归并；
    /// 大表缺失按「COUNT − 命中」记账，不物化。
    async fn point_query_diff(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        queries: &mut u64,
        plan: PointQueryPlan,
    ) -> Result<(Vec<DiffRow>, u64, u64, Option<SkewCounts>, Vec<String>), DbError> {
        let PointQueryPlan {
            big_is_left,
            big_total,
            small_total,
        } = plan;
        let arity = ctx.key_columns.len().max(1);
        let small_is_left = !big_is_left;
        ctx.vlog(format!(
            "[delta-diff] skewed point-query: big={}({}) small={}({})",
            if big_is_left { "left" } else { "right" },
            big_total,
            if small_is_left { "left" } else { "right" },
            small_total
        ));

        let small_rows = if small_is_left {
            let spec = full_row_spec(ctx, true, left.dialect(), None)?;
            fetch_all_pages(left, &spec, ctx.verbose, queries).await?
        } else {
            let spec = full_row_spec(ctx, false, right.dialect(), None)?;
            fetch_all_pages(right, &spec, ctx.verbose, queries).await?
        };

        let mut extra = Vec::new();
        if let Some(w) = fetch_count_mismatch_warning(
            "point-query small",
            small_total,
            small_rows.len() as u64,
            big_total,
            big_total,
        ) {
            extra.push(w);
        }

        let chunk = point_query_chunk_rows(arity);
        let quote = if big_is_left {
            left.dialect().identifier_quote()
        } else {
            right.dialect().identifier_quote()
        };
        let mut big_hits: Vec<Vec<Value>> = Vec::new();
        for group in small_rows.chunks(chunk) {
            let pred = render_point_query_predicate(ctx, big_is_left, quote, group)?;
            let spec = if big_is_left {
                full_row_spec(ctx, true, left.dialect(), Some(&pred))?
            } else {
                full_row_spec(ctx, false, right.dialect(), Some(&pred))?
            };
            let hits = if big_is_left {
                fetch_all_pages(left, &spec, ctx.verbose, queries).await?
            } else {
                fetch_all_pages(right, &spec, ctx.verbose, queries).await?
            };
            big_hits.extend(hits);
        }
        ctx.vlog(format!(
            "[delta-diff] point-query chunks={} hits={}",
            small_rows.len().div_ceil(chunk),
            big_hits.len()
        ));

        let left_numeric = full_row_numeric_flags(ctx, true);
        let right_numeric = full_row_numeric_flags(ctx, false);
        // 与 row_level_diff 相同的对齐守卫：列排除不对称时宁可报错，
        // 也不能让归并的长度失配把全部命中误报成 Modified
        if left_numeric.len() != right_numeric.len() {
            return Err(DbError::config(
                "delta-diff: row comparison type flags do not align with selected columns",
            ));
        }
        let numeric: Vec<bool> = left_numeric
            .iter()
            .zip(right_numeric.iter())
            .map(|(l, r)| *l && *r)
            .collect();
        let merge = merge_small_vs_hits(
            &small_rows,
            &big_hits,
            arity,
            &numeric,
            small_is_left,
            big_total,
        );
        extra.extend(merge.extra_warnings);
        if merge.unpaired_hits > 0 {
            return Err(DbError::query(format!(
                "delta-diff point-query: {} big-side hit(s) could not be paired by key \
                 order; summary would be unreliable (concurrent writes, non-unique keys, \
                 or cross-engine key-type mismatch)",
                merge.unpaired_hits
            )));
        }

        let mut missing_left = 0u64;
        let mut missing_right = 0u64;
        let mut modified = 0u64;
        for d in &merge.rows {
            match d.status {
                DiffStatus::MissingLeft => missing_left += 1,
                DiffStatus::MissingRight => missing_right += 1,
                DiffStatus::Modified => modified += 1,
            }
        }
        if big_is_left {
            missing_right += merge.missing_big;
        } else {
            missing_left += merge.missing_big;
        }
        let (left_total, right_total) = if big_is_left {
            (big_total, small_total)
        } else {
            (small_total, big_total)
        };
        Ok((
            merge.rows,
            left_total,
            right_total,
            Some(SkewCounts {
                missing_left,
                missing_right,
                modified,
            }),
            extra,
        ))
    }
}

/// 点查等值谓词：每行 `AND` 连接各键列等值，行间 `OR`。等值语义与 keyset
/// 完全一致（二进制校对 + 文本字面量 + NULL 用 IS NULL），见
/// `render_key_equality`；谓词是 OR-of-ANDs，不受 ORA-01795 约束。
fn render_point_query_predicate(
    ctx: &DiffContext,
    big_is_left: bool,
    quote: char,
    rows: &[Vec<Value>],
) -> Result<String, DbError> {
    let side = if big_is_left { &ctx.left } else { &ctx.right };
    let scheme = side.plan.url_scheme.as_str();
    let backslash_escape = scheme == "mysql";
    let key_columns = ctx.side_key_columns(big_is_left);
    let string_flags = side.plan.string_key_flags_for(key_columns);
    let arity = key_columns.len().max(1);
    let mut terms = Vec::with_capacity(rows.len());
    for row in rows {
        let mut conds = Vec::with_capacity(arity);
        for (idx, key) in key_columns.iter().enumerate() {
            let value = row.get(idx).cloned().unwrap_or(Value::Null);
            conds.push(crate::backend::render_key_equality(
                quote,
                key,
                string_flags.get(idx).copied().unwrap_or(false),
                scheme,
                &value,
                backslash_escape,
            ));
        }
        terms.push(format!("({})", conds.join(" AND ")));
    }
    Ok(terms.join(" OR "))
}

struct PointQueryPlan {
    big_is_left: bool,
    big_total: u64,
    small_total: u64,
}

/// 一次校验的产物：桶数 N、失配桶集合与两侧计数映射。
pub(crate) struct BucketScan {
    n: u64,
    buckets: Vec<u64>,
    lmap: BTreeMap<u64, ChecksumTuple>,
    rmap: BTreeMap<u64, ChecksumTuple>,
    left_total: u64,
    right_total: u64,
}

fn diff_bucket_ids(
    left: &BTreeMap<u64, ChecksumTuple>,
    right: &BTreeMap<u64, ChecksumTuple>,
    n: u64,
) -> Vec<u64> {
    (0..n)
        .filter(|b| {
            left.get(b).copied().unwrap_or_else(ChecksumTuple::zero)
                != right.get(b).copied().unwrap_or_else(ChecksumTuple::zero)
        })
        .collect()
}

fn batch_spec(
    ctx: &DiffContext,
    is_left: bool,
    dialect: &dyn crate::backend::Dialect,
    modulus: u64,
) -> Result<ChecksumSqlSpec, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    Ok(ChecksumSqlSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        key_column: None,
        range: None,
        bucket: Some((modulus, 0)),
        filter: side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
        normalized_exprs: side.plan.identity_hash_exprs(dialect)?,
        key_hash_exprs: side.plan.key_hash_exprs(dialect)?,
    })
}

pub(crate) fn render_count_sql(
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

fn fetch_count_mismatch_warning(
    scope: &str,
    expected_left: u64,
    fetched_left: u64,
    expected_right: u64,
    fetched_right: u64,
) -> Option<String> {
    if expected_left == fetched_left && expected_right == fetched_right {
        return None;
    }
    Some(format!(
        "keyset fetch count mismatch {scope}: left checksum={expected_left} fetched={fetched_left}, \
         right checksum={expected_right} fetched={fetched_right}; \
         NULL-key pagination or concurrent writes may have dropped rows"
    ))
}

pub(crate) fn parse_count(result: &crate::backend::QueryResult) -> Result<u64, DbError> {
    parse_count_value(result.rows.first().and_then(|r| r.first()))
}

fn parse_count_value(cell: Option<&Value>) -> Result<u64, DbError> {
    match cell {
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

fn keys_only_diffs(rows: &[Vec<Value>], arity: usize, is_left: bool) -> Vec<DiffRow> {
    let status = if is_left {
        DiffStatus::MissingRight
    } else {
        DiffStatus::MissingLeft
    };
    rows.iter()
        .map(|row| {
            let key = if arity <= 1 {
                row.first().cloned().unwrap_or(Value::Null)
            } else {
                Value::Array(row.iter().take(arity).cloned().collect())
            };
            DiffRow {
                key,
                left: if is_left { Some(row.clone()) } else { None },
                right: if is_left { None } else { Some(row.clone()) },
                status,
                confirmed: true,
            }
        })
        .collect()
}

fn keys_only_spec(
    ctx: &DiffContext,
    is_left: bool,
    dialect: &dyn crate::backend::Dialect,
) -> Result<KeysetPageSpec, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let side_keys = ctx.side_key_columns(is_left);
    Ok(KeysetPageSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        columns: side_keys.to_vec(),
        raw_exprs: false,
        key_columns: side_keys.to_vec(),
        string_key: side.plan.string_key_flags_for(side_keys),
        range: None,
        last_key: None,
        page_size: PAGE_SIZE,
        filter: side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
    })
}

fn full_row_spec(
    ctx: &DiffContext,
    is_left: bool,
    dialect: &dyn crate::backend::Dialect,
    extra_pred: Option<&str>,
) -> Result<KeysetPageSpec, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let side_keys = ctx.side_key_columns(is_left);
    // Catalog-physical names: quote without re-folding (issue #116).
    let mut columns: Vec<String> = side_keys
        .iter()
        .map(|c| dialect.quote_catalog_ident(c))
        .collect();
    for spec in side
        .plan
        .norm_specs
        .iter()
        .filter(|s| !side_keys.iter().any(|k| k == &s.name))
    {
        columns.push(dialect.normalize_expr(spec)?);
    }
    Ok(KeysetPageSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        columns,
        raw_exprs: true,
        key_columns: side_keys.to_vec(),
        string_key: side.plan.string_key_flags_for(side_keys),
        range: None,
        last_key: None,
        page_size: PAGE_SIZE,
        filter: {
            let base = side_filter(ctx, dialect.url_scheme());
            match (base, extra_pred) {
                (Some(f), Some(p)) => Some(format!("({f}) AND ({p})")),
                (Some(f), None) => Some(f),
                (None, Some(p)) => Some(p.to_string()),
                (None, None) => None,
            }
        },
        scn: ctx.scn_of(is_left),
    })
}

pub(crate) fn full_row_numeric_flags(ctx: &DiffContext, is_left: bool) -> Vec<bool> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let side_keys = ctx.side_key_columns(is_left);
    let mut columns = side_keys.to_vec();
    columns.extend(
        side.plan
            .norm_specs
            .iter()
            .filter(|spec| !side_keys.iter().any(|key| key == &spec.name))
            .map(|spec| spec.name.clone()),
    );
    side.plan.numeric_value_flags_for(&columns)
}

async fn fetch_all_pages(
    conn: &mut (dyn DbConn + Send),
    spec: &KeysetPageSpec,
    verbose: bool,
    queries: &mut u64,
) -> Result<Vec<Vec<Value>>, DbError> {
    let mut out = Vec::new();
    let mut last_key = None;
    loop {
        let page_spec = KeysetPageSpec {
            last_key: last_key.clone(),
            ..spec.clone()
        };
        let sql = conn.dialect().render_keyset_page_sql(&page_spec);
        if verbose {
            eprintln!("[sql] {sql}");
        }
        let result = conn.query(&sql).await?;
        *queries += 1;
        let n = result.rows.len();
        if let Some(last) = result.rows.last() {
            last_key = Some(last.iter().take(spec.key_columns.len()).cloned().collect());
        }
        out.extend(result.rows);
        if n < spec.page_size {
            break;
        }
    }
    Ok(out)
}

fn assemble(
    ctx: &DiffContext,
    diff_rows: Vec<DiffRow>,
    left_total: u64,
    right_total: u64,
    _sample_limit: usize,
    counts: Option<SkewCounts>,
    extra_warnings: Vec<String>,
) -> DiffReport {
    let mut summary = DiffSummary {
        left_total,
        right_total,
        ..Default::default()
    };
    match counts {
        Some(c) => {
            summary.missing_left = c.missing_left;
            summary.missing_right = c.missing_right;
            summary.modified = c.modified;
        }
        None => {
            for d in &diff_rows {
                match d.status {
                    DiffStatus::MissingLeft => summary.missing_left += 1,
                    DiffStatus::MissingRight => summary.missing_right += 1,
                    DiffStatus::Modified => summary.modified += 1,
                }
            }
        }
    }
    let total = summary.left_total.max(summary.right_total);
    summary.diff_rate = if total > 0 {
        (summary.missing_left + summary.missing_right + summary.modified) as f64 / total as f64
    } else {
        0.0
    };
    let warnings: Vec<String> = ctx
        .left
        .plan
        .warnings
        .iter()
        .chain(ctx.right.plan.warnings.iter())
        .chain(ctx.route_warnings.iter())
        .cloned()
        .chain(extra_warnings)
        .collect();
    let sample: Vec<DiffRow> = diff_rows;
    let diff_count = summary.missing_left + summary.missing_right + summary.modified;
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
        strategy: "keyeddiff".into(),
        consistency: ctx.consistency.as_str().into(),
        hash_algorithm: "md5".into(),
        summary,
        perf: PerfMetrics::default(),
        shards: vec![ShardResult {
            shard_id: "keyed-all".into(),
            key_range: (Value::Null, Value::Null),
            left_count: left_total,
            right_count: right_total,
            diff_count,
            status: if diff_count > 0 {
                ShardStatus::Diff
            } else {
                ShardStatus::Match
            },
            duration_ms: 0,
        }],
        sample_diffs: sample,
        warnings,
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

// ─── skew routing (issue #124) ─────────────────────────────────────────

/// 点查路径小表行数上限：比例门槛只约束相对大小，不约束小表绝对规模；
/// 无上限时 1 亿 vs 1250 万会退化成数万条块状点查。超过走共享键序扫描。
pub(crate) const POINT_QUERY_MAX_KEYS: u64 = 1_048_576;

/// 点查谓词单块行字面量预算（语句长度/表达式复杂度的保守上限；点查谓词是
/// OR-of-ANDs，不受 Oracle ORA-01795 的 IN 列表限制。实际块行数 =
/// budget / 键列数，复合键不超限）。
pub(crate) const POINT_QUERY_LITERAL_BUDGET: u64 = 1000;

/// 倾斜路径选择。调用前提：两侧均非空（空侧短路已由 diff_inner 处理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkewRoute {
    /// 小表整读 + 大表分块等值点查 + 客户端归并；大表缺失按计数记账不物化。
    PointQuery { big_is_left: bool },
    /// 全部失配桶合并为一次键序扫描（MOD IN 谓词），行仍全量物化。
    SharedScan,
    /// 比例 < 8：保持逐桶 MOD 分页归并（既有形状，验收锁定）。
    PerBucket,
}

/// 倾斜判定：`big / 8 >= small`（等价 `big >= small * 8`，防溢出写法）。
fn is_skewed(big: u64, small: u64) -> bool {
    small > 0 && big / 8 >= small
}

pub(crate) fn skew_route(
    left_total: u64,
    right_total: u64,
    fetch_all_threshold: u64,
    summary_only: bool,
    both_keys_unique: bool,
) -> SkewRoute {
    let (big, small) = if left_total >= right_total {
        (left_total, right_total)
    } else {
        (right_total, left_total)
    };
    let big_is_left = left_total >= right_total;
    if big <= fetch_all_threshold || !is_skewed(big, small) {
        return SkewRoute::PerBucket;
    }
    if summary_only && both_keys_unique && small <= POINT_QUERY_MAX_KEYS {
        return SkewRoute::PointQuery { big_is_left };
    }
    SkewRoute::SharedScan
}

/// 点查分块行数：总字面量预算均摊到每行 arity 个键等式。
pub(crate) fn point_query_chunk_rows(arity: usize) -> usize {
    (POINT_QUERY_LITERAL_BUDGET / arity.max(1) as u64).max(1) as usize
}

/// 点查归并结果：只物化 Modified 与小表独有行；大表缺失按计数记账。
pub(crate) struct PointQueryMerge {
    pub(crate) rows: Vec<DiffRow>,
    pub(crate) missing_big: u64,
    /// 未能与 cmp_key 配对的大侧命中（排序失步/语义不一致）。非零即不可信，
    /// 调用方必须报错而不是静默给出摘要。
    pub(crate) unpaired_hits: u64,
    pub(crate) extra_warnings: Vec<String>,
}

/// 客户端归并小表行 × 大表点查命中。两侧均键序（小表 keyset 序、
/// 命中块按小表序拼接且块内 ORDER BY）。`numeric` 为两侧标志按列 AND。
pub(crate) fn merge_small_vs_hits(
    small_rows: &[Vec<Value>],
    big_hits: &[Vec<Value>],
    arity: usize,
    numeric: &[bool],
    small_is_left: bool,
    big_total: u64,
) -> PointQueryMerge {
    let mut rows = Vec::new();
    let mut extra_warnings = Vec::new();

    let small_status = if small_is_left {
        DiffStatus::MissingRight
    } else {
        DiffStatus::MissingLeft
    };

    let mut si = 0usize;
    let mut bi = 0usize;
    while si < small_rows.len() {
        if bi >= big_hits.len() {
            rows.push(diff_row_n(
                &small_rows[si],
                arity,
                small_is_left,
                small_status,
            ));
            si += 1;
            continue;
        }
        let sk = &small_rows[si][..arity.min(small_rows[si].len())];
        let bk = &big_hits[bi][..arity.min(big_hits[bi].len())];
        match crate::delta_diff::rowdiff::cmp_key(sk, bk, &numeric[..arity.min(numeric.len())]) {
            std::cmp::Ordering::Less => {
                rows.push(diff_row_n(
                    &small_rows[si],
                    arity,
                    small_is_left,
                    small_status,
                ));
                si += 1;
            }
            // 命中键必然来自小表键集：Greater 只能是排序失步。命中不配对，
            // 由 unpaired_hits 让调用方报错；`COUNT − 命中` 只扣配对上的。
            std::cmp::Ordering::Greater => {
                bi += 1;
            }
            std::cmp::Ordering::Equal => {
                let equal = crate::delta_diff::rowdiff::row_values_equal(
                    &small_rows[si][arity..],
                    &big_hits[bi][arity..],
                    &numeric[arity..],
                );
                if !equal {
                    let (left, right) = if small_is_left {
                        (Some(small_rows[si].clone()), Some(big_hits[bi].clone()))
                    } else {
                        (Some(big_hits[bi].clone()), Some(small_rows[si].clone()))
                    };
                    rows.push(DiffRow {
                        key: crate::delta_diff::rowdiff::diff_key(&small_rows[si], arity),
                        left,
                        right,
                        status: DiffStatus::Modified,
                        confirmed: true,
                    });
                }
                si += 1;
                bi += 1;
            }
        }
    }

    let paired = bi as u64;
    let unpaired_hits = (big_hits.len() as u64).saturating_sub(paired);
    if unpaired_hits > 0 {
        extra_warnings.push(format!(
            "point-query: {unpaired_hits} big-side hit(s) could not be paired by key order"
        ));
    }
    if (big_hits.len() as u64) > big_total {
        extra_warnings.push(format!(
            "point-query hits ({}) exceed big-side COUNT ({big_total}); \
             concurrent writes may have shifted the table",
            big_hits.len()
        ));
    }
    let missing_big = big_total.saturating_sub(paired);
    PointQueryMerge {
        rows,
        missing_big,
        unpaired_hits,
        extra_warnings,
    }
}

#[cfg(test)]
mod skew_tests {
    use super::*;
    use crate::delta_diff::report::DiffStatus;
    use serde_json::json;

    const THRESHOLD: u64 = 4096;

    #[test]
    fn should_point_query_when_skewed_and_summary_only_and_unique() {
        let route = skew_route(3_212_540, 2, THRESHOLD, true, true);
        assert_eq!(route, SkewRoute::PointQuery { big_is_left: true });
    }

    #[test]
    fn should_point_query_when_small_side_is_left() {
        let route = skew_route(2, 3_212_540, THRESHOLD, true, true);
        assert_eq!(route, SkewRoute::PointQuery { big_is_left: false });
    }

    #[test]
    fn should_point_query_at_ratio_exactly_eight() {
        let route = skew_route(8000, 1000, THRESHOLD, true, true);
        assert_eq!(route, SkewRoute::PointQuery { big_is_left: true });
    }

    #[test]
    fn should_stay_per_bucket_when_ratio_below_eight() {
        let route = skew_route(10_000, 2000, THRESHOLD, true, true);
        assert_eq!(route, SkewRoute::PerBucket);
    }

    #[test]
    fn should_stay_per_bucket_when_big_within_threshold() {
        let route = skew_route(100, 2, THRESHOLD, true, true);
        assert_eq!(route, SkewRoute::PerBucket);
    }

    #[test]
    fn should_shared_scan_when_summary_only_false() {
        let route = skew_route(3_212_540, 2, THRESHOLD, false, true);
        assert_eq!(route, SkewRoute::SharedScan);
    }

    #[test]
    fn should_shared_scan_when_key_not_unique() {
        let route = skew_route(3_212_540, 2, THRESHOLD, true, false);
        assert_eq!(route, SkewRoute::SharedScan);
    }

    #[test]
    fn should_shared_scan_when_small_side_exceeds_point_query_cap() {
        let small = POINT_QUERY_MAX_KEYS + 1;
        let big = small * 8;
        let route = skew_route(big, small, THRESHOLD, true, true);
        assert_eq!(route, SkewRoute::SharedScan);
    }

    #[test]
    fn should_be_overflow_safe_at_extreme_counts() {
        // small * 8 形式在 debug 构建会溢出 panic；big / 8 形式必须安全
        let big = u64::MAX;
        let small = u64::MAX / 4;
        assert_eq!(
            skew_route(big, small, THRESHOLD, true, true),
            SkewRoute::PerBucket
        );
        assert_eq!(
            skew_route(u64::MAX, 2, THRESHOLD, true, true),
            SkewRoute::PointQuery { big_is_left: true }
        );
    }

    #[test]
    fn point_query_chunk_rows_divides_literal_budget_by_arity() {
        assert_eq!(point_query_chunk_rows(1), 1000);
        assert_eq!(point_query_chunk_rows(2), 500);
        assert_eq!(point_query_chunk_rows(3), 333);
        assert_eq!(point_query_chunk_rows(0), 1000, "arity 至少按 1 计");
    }

    #[test]
    fn point_query_chunk_rows_never_zero() {
        assert_eq!(point_query_chunk_rows(2000), 1);
    }

    fn row(cells: &[Value]) -> Vec<Value> {
        cells.to_vec()
    }

    #[test]
    fn merge_counts_small_only_rows_as_missing() {
        let small = vec![row(&[json!(1), json!("a")]), row(&[json!(3), json!("c")])];
        let hits = vec![row(&[json!(1), json!("a")])];
        let out = merge_small_vs_hits(&small, &hits, 1, &[false, false], true, 10);
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].status, DiffStatus::MissingRight);
        assert_eq!(out.rows[0].key, json!(3));
        assert_eq!(out.missing_big, 9);
        assert!(out.extra_warnings.is_empty());
    }

    #[test]
    fn merge_flags_value_mismatch_as_modified() {
        let small = vec![row(&[json!(1), json!("old")])];
        let hits = vec![row(&[json!(1), json!("new")])];
        let out = merge_small_vs_hits(&small, &hits, 1, &[false, false], true, 5);
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].status, DiffStatus::Modified);
        assert_eq!(out.rows[0].left, Some(row(&[json!(1), json!("old")])));
        assert_eq!(out.rows[0].right, Some(row(&[json!(1), json!("new")])));
        assert_eq!(out.missing_big, 4);
    }

    #[test]
    fn merge_respects_small_side_direction() {
        let small = vec![row(&[json!(7), json!("x")])];
        let out = merge_small_vs_hits(&small, &[], 1, &[false, false], false, 3);
        assert_eq!(out.rows[0].status, DiffStatus::MissingLeft);
        assert_eq!(out.rows[0].right, Some(row(&[json!(7), json!("x")])));
        assert_eq!(out.rows[0].left, None);
    }

    #[test]
    fn merge_handles_composite_keys_in_order() {
        let small = vec![
            row(&[json!("b"), json!(1), json!(10)]),
            row(&[json!("b"), json!(2), json!(20)]),
        ];
        let hits = vec![row(&[json!("b"), json!(1), json!(10)])];
        let out = merge_small_vs_hits(&small, &hits, 2, &[false, false, false], true, 4);
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].key, json!(["b", 2]));
        assert_eq!(out.missing_big, 3);
    }

    #[test]
    fn merge_clamps_missing_when_hits_exceed_total_and_warns() {
        let small = vec![row(&[json!(1), json!("a")]), row(&[json!(2), json!("b")])];
        let hits = vec![row(&[json!(1), json!("a")]), row(&[json!(2), json!("b")])];
        let out = merge_small_vs_hits(&small, &hits, 1, &[false, false], true, 1);
        assert_eq!(out.missing_big, 0);
        assert_eq!(out.unpaired_hits, 0);
        assert!(out
            .extra_warnings
            .iter()
            .any(|w| w.contains("exceed big-side COUNT")));
    }

    #[test]
    fn merge_counts_unpaired_hits_for_caller_to_error() {
        // 排序失步：命中键比当前小表键大且小表已耗尽后续命中 —— bi 前进但不配对
        let small = vec![row(&[json!(1), json!("a")])];
        let hits = vec![row(&[json!(1), json!("a")]), row(&[json!(9), json!("z")])];
        let out = merge_small_vs_hits(&small, &hits, 1, &[false, false], true, 5);
        assert_eq!(out.unpaired_hits, 1);
        // 只扣配对上的 1 个命中
        assert_eq!(out.missing_big, 4);
        assert!(out
            .extra_warnings
            .iter()
            .any(|w| w.contains("could not be paired")));
    }

    fn string_key_ctx(scheme: &str) -> DiffContext {
        fn side(scheme: &str, key: &str) -> crate::delta_diff::strategy::SideCtx {
            crate::delta_diff::strategy::SideCtx {
                connection_name: "x".into(),
                schema: Some("s".into()),
                table: "t".into(),
                plan: crate::delta_diff::metadata::TablePlan {
                    url_scheme: scheme.into(),
                    key_columns: vec![key.into()],
                    compare_columns: vec![key.into(), "v".into()],
                    norm_specs: vec![],
                    warnings: vec![],
                    key_specs: vec![crate::backend::ColumnNormSpec {
                        name: key.into(),
                        data_type: "varchar(64)".into(),
                        nullable: false,
                        rtrim_fixed_char: false,
                    }],
                    key_unique: true,
                },
            }
        }
        DiffContext {
            left: side(scheme, "K"),
            right: side(scheme, "k"),
            left_pool: dummy_pool(),
            right_pool: dummy_pool(),
            key_column: "K".into(),
            key_columns: vec!["K".into()],
            left_key_columns: vec!["K".into()],
            right_key_columns: vec!["k".into()],
            filter: None,
            incremental: None,
            bisection_factor: 32,
            bisection_threshold: 16_384,
            sample_limit: 20,
            threads: 4,
            consistency: crate::delta_diff::strategy::ConsistencyMode::None,
            recheck: false,
            route_warnings: vec![],
            checkpoint: None,
            iblt_capacity: 65_536,
            fetch_all_threshold: 4096,
            naive_max_rows: 4096,
            strict: false,
            summary_only: true,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        }
    }

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
    fn point_predicate_uses_binary_collation_for_mysql_string_keys() {
        let ctx = string_key_ctx("mysql");
        let sql = render_point_query_predicate(&ctx, true, '`', &[vec![json!("ABC"), json!("v1")]])
            .unwrap();
        assert!(
            sql.contains("`K` COLLATE utf8mb4_bin = 'ABC'"),
            "must pin binary collation: {sql}"
        );
        assert!(!sql.contains("= NULL"), "{sql}");
    }

    #[test]
    fn point_predicate_uses_binary_collation_for_gaussdb_string_keys() {
        let ctx = string_key_ctx("gaussdb");
        let sql =
            render_point_query_predicate(&ctx, false, '"', &[vec![json!("ABC"), json!("v1")]])
                .unwrap();
        assert!(sql.contains("\"k\" COLLATE \"C\" = 'ABC'"), "{sql}");
    }

    #[test]
    fn point_predicate_wraps_oracle_string_keys_in_nlssort() {
        let ctx = string_key_ctx("oracle");
        let sql = render_point_query_predicate(&ctx, true, '"', &[vec![json!("ABC"), json!("v1")]])
            .unwrap();
        assert!(
            sql.contains("NLSSORT(\"K\",'NLS_SORT=BINARY') = NLSSORT('ABC','NLS_SORT=BINARY')"),
            "{sql}"
        );
    }

    #[test]
    fn point_predicate_renders_is_null_for_null_key_values() {
        let ctx = string_key_ctx("mysql");
        let sql = render_point_query_predicate(&ctx, true, '`', &[vec![json!(null), json!("v1")]])
            .unwrap();
        assert!(sql.contains("`K` COLLATE utf8mb4_bin IS NULL"), "{sql}");
        assert!(!sql.contains("= NULL"), "{sql}");
    }

    #[test]
    fn merge_produces_no_rows_when_identical() {
        let small = vec![row(&[json!(1), json!("a")])];
        let hits = vec![row(&[json!(1), json!("a")])];
        let out = merge_small_vs_hits(&small, &hits, 1, &[false, false], true, 1);
        assert!(out.rows.is_empty());
        assert_eq!(out.missing_big, 0);
    }

    #[test]
    fn merge_uses_numeric_ordering_for_int_keys() {
        // "10" 与 "2" 按数值序：2 < 10；按文本序会错配
        let small = vec![
            row(&[json!(2), json!("two")]),
            row(&[json!(10), json!("ten")]),
        ];
        let hits = vec![row(&[json!(2), json!("two")])];
        let out = merge_small_vs_hits(&small, &hits, 1, &[true, false], true, 10);
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].key, json!(10));
    }
}

#[cfg(all(test, feature = "gaussdb"))]
mod tests {
    use super::*;
    use crate::backend::BackendFactory;
    use crate::delta_diff::report::DiffStatus;
    use serde_json::json;

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

    fn side(key_columns: &[&str]) -> crate::delta_diff::strategy::SideCtx {
        crate::delta_diff::strategy::SideCtx {
            connection_name: "x".into(),
            schema: Some("s".into()),
            table: "t".into(),
            plan: crate::delta_diff::metadata::TablePlan {
                key_unique: false,
                url_scheme: "mysql".into(),
                key_columns: key_columns.iter().map(|name| (*name).into()).collect(),
                compare_columns: key_columns.iter().map(|name| (*name).into()).collect(),
                norm_specs: vec![],
                warnings: vec![],
                key_specs: vec![],
            },
        }
    }

    #[test]
    fn right_keyset_sql_uses_right_side_key_casing() {
        let ctx = DiffContext {
            left: side(&["K_XWDM", "SECURITY_ID"]),
            right: side(&["k_xwdm", "security_id"]),
            left_pool: dummy_pool(),
            right_pool: dummy_pool(),
            key_column: "K_XWDM".into(),
            key_columns: vec!["K_XWDM".into(), "SECURITY_ID".into()],
            left_key_columns: vec!["K_XWDM".into(), "SECURITY_ID".into()],
            right_key_columns: vec!["k_xwdm".into(), "security_id".into()],
            filter: None,
            incremental: None,
            bisection_factor: 32,
            bisection_threshold: 16_384,
            sample_limit: 20,
            threads: 4,
            consistency: crate::delta_diff::strategy::ConsistencyMode::None,
            recheck: false,
            route_warnings: vec![],
            checkpoint: None,
            iblt_capacity: 65_536,
            fetch_all_threshold: 4096,
            naive_max_rows: 4096,
            strict: false,
            summary_only: false,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        };
        let dialect = crate::backend::gaussdb::GaussdbFactory.create_dialect();
        let spec = keys_only_spec(&ctx, false, dialect.as_ref()).unwrap();
        let sql = dialect.render_keyset_page_sql(&spec);

        assert!(sql.contains("\"k_xwdm\", \"security_id\""), "{sql}");
        assert!(!sql.contains("\"K_XWDM\""), "{sql}");
    }

    #[test]
    fn fetch_count_mismatch_warning_none_when_equal() {
        assert!(fetch_count_mismatch_warning("bucket 1", 5, 5, 3, 3).is_none());
    }

    #[test]
    fn fetch_count_mismatch_warning_when_fetched_short() {
        let w = fetch_count_mismatch_warning("bucket 2", 10, 8, 10, 10).unwrap();
        assert!(w.contains("bucket 2"), "{w}");
        assert!(w.contains("left checksum=10 fetched=8"), "{w}");
        assert!(w.contains("NULL-key"), "{w}");
    }

    #[test]
    fn parse_count_rejects_garbage() {
        let r = crate::backend::QueryResult {
            columns: vec!["cnt".into()],
            rows: vec![vec![json!("nope")]],
            row_count: 1,
            rows_affected: None,
        };
        assert!(parse_count(&r).is_err());
    }

    #[test]
    fn count_sql_includes_filter_and_quotes() {
        let sql = render_count_sql("mysql", '`', Some("s"), "t", Some("bcrq='20260114'"));
        assert_eq!(
            sql,
            "SELECT COUNT(*) AS cnt FROM `s`.`t` WHERE (bcrq='20260114')"
        );
    }

    #[test]
    fn buckets_that_differ() {
        let mut l = std::collections::BTreeMap::new();
        l.insert(
            1,
            crate::delta_diff::checksum::ChecksumTuple {
                count: 5,
                s: [1, 0, 0, 0],
            },
        );
        let mut r = std::collections::BTreeMap::new();
        r.insert(
            1,
            crate::delta_diff::checksum::ChecksumTuple {
                count: 5,
                s: [1, 0, 0, 0],
            },
        );
        r.insert(
            2,
            crate::delta_diff::checksum::ChecksumTuple {
                count: 3,
                s: [9, 0, 0, 0],
            },
        );
        let d = diff_bucket_ids(&l, &r, 4);
        assert_eq!(d, vec![2]);
    }

    #[test]
    fn assemble_empty_right_marks_missing_right() {
        let rows = vec![vec![json!(1), json!("a")]];
        let report_rows = keys_only_diffs(&rows, 2, true);
        assert_eq!(report_rows.len(), 1);
        assert_eq!(report_rows[0].status, DiffStatus::MissingRight);
        assert_eq!(report_rows[0].key, json!([1, "a"]));
    }
}
