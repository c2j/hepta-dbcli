// ─── delta-diff BucketDiffer：无主键表内容分桶比对（v2.1 §6.3）──────────
//
// 按 MOD(row_hash, N) 内容分桶（跨库天然对齐），桶内位切片 checksum 快筛，
// 差异桶拉 (hash, count) 行集做客户端多重集合比对（multiset diff）。
// 语义极限：无主键表只能回答"哪些行内容多/少了几次"，报告头部固定提示。

use std::collections::HashMap;
use std::time::Instant;

use chrono::Utc;
use serde_json::Value;

use crate::backend::{quote_ident, ChecksumSqlSpec, DbConn, DbError};
use crate::delta_diff::checksum::{run_batch_checksum, ChecksumTuple};
use crate::delta_diff::hash_diff::open_snapshot;
use crate::delta_diff::report::{
    DiffReport, DiffRow, DiffStatus, DiffSummary, PerfMetrics, ShardResult, ShardStatus, TableRef,
};
use crate::delta_diff::strategy::{side_filter, ConsistencyMode, DiffContext, DiffStrategy};

const MAX_BUCKETS: u64 = 1024;

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
        let t0 = Instant::now();
        let lspec = bucket_checksum_spec(ctx, true, n, 0, left.dialect())?;
        let rspec = bucket_checksum_spec(ctx, false, n, 0, right.dialect())?;
        let (lmap, rmap) = tokio::join!(
            run_batch_checksum(left, &lspec, ctx.verbose),
            run_batch_checksum(right, &rspec, ctx.verbose)
        );
        let (lmap, rmap) = (lmap?, rmap?);
        *queries += 2;

        let mut shards = Vec::new();
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
        let diff_buckets = maps_to_diff_buckets(&lmap, &rmap, n);

        let mut rows = Vec::new();
        for b in &diff_buckets {
            let lsql = left
                .dialect()
                .render_bucket_multiset_sql(&bucket_checksum_spec(
                    ctx,
                    true,
                    n,
                    *b,
                    left.dialect(),
                )?);
            let rsql = right
                .dialect()
                .render_bucket_multiset_sql(&bucket_checksum_spec(
                    ctx,
                    false,
                    n,
                    *b,
                    right.dialect(),
                )?);
            ctx.vlog(format!("[sql:left] {lsql}"));
            ctx.vlog(format!("[sql:right] {rsql}"));
            let (lr, rr) = tokio::join!(left.query(&lsql), right.query(&rsql));
            *queries += 2;
            rows.extend(multiset_diff(lr?.rows, rr?.rows));
        }
        Ok((shards, rows, n))
    }

    /// 估算桶数：目标每桶 ≈ threshold 行，上限 MAX_BUCKETS（§6.3）。
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
                left.dialect().identifier_quote(),
                ctx.left.schema.as_deref(),
                &ctx.left.table,
                lf.as_deref(),
            );
            let rsql = filtered_count_sql(
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
    quote: char,
    schema: Option<&str>,
    table: &str,
    filter: Option<&str>,
) -> String {
    let table = match schema {
        Some(s) => format!("{}.{}", quote_ident(quote, s), quote_ident(quote, table)),
        None => quote_ident(quote, table),
    };
    match filter {
        Some(f) => format!("SELECT COUNT(*) AS cnt FROM {table} WHERE ({f})"),
        None => format!("SELECT COUNT(*) AS cnt FROM {table}"),
    }
}

fn parse_count_cell(result: &crate::backend::QueryResult) -> Result<u64, DbError> {
    let Some(row) = result.rows.first() else {
        return Ok(0);
    };
    match row.first() {
        Some(Value::Number(n)) => Ok(n.as_u64().unwrap_or(0)),
        Some(Value::String(s)) => Ok(s
            .trim()
            .split('.')
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0)),
        _ => Ok(0),
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
        sample_diffs: diff_rows.into_iter().take(sample_limit).collect(),
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
