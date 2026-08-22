// ─── delta-diff KeyedDiffer: composite/string key identity diff ────────
//
// COUNT both sides → empty-side short-circuit → FETCH_ALL or
// scan-once bucket checksum + composite keyset merge.
// Not used for keyless tables (those stay on bucketdiff).

use chrono::Utc;
use serde_json::Value;

use std::collections::BTreeMap;

use crate::backend::{quote_ident, ChecksumSqlSpec, DbConn, DbError, KeysetPageSpec};
use crate::delta_diff::checksum::{run_batch_checksum, ChecksumTuple};
use crate::delta_diff::hash_diff::open_snapshot;
use crate::delta_diff::report::{
    DiffReport, DiffRow, DiffStatus, DiffSummary, PerfMetrics, ShardResult, ShardStatus, TableRef,
};
use crate::delta_diff::rowdiff::row_level_diff;
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

        let (diff_rows, left_total, right_total) = result?;
        let mut report = assemble(ctx, diff_rows, left_total, right_total, ctx.sample_limit);
        report.started_at = started;
        report.finished_at = Utc::now();
        report.perf.queries_total = queries;
        Ok(report)
    }
}

impl KeyedDiffer {
    async fn diff_inner(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        queries: &mut u64,
    ) -> Result<(Vec<DiffRow>, u64, u64), DbError> {
        let lq = left.dialect().identifier_quote();
        let rq = right.dialect().identifier_quote();
        let lfilter = side_filter(ctx, left.dialect().url_scheme());
        let rfilter = side_filter(ctx, right.dialect().url_scheme());
        let lsql = render_count_sql(
            lq,
            ctx.left.schema.as_deref(),
            &ctx.left.table,
            lfilter.as_deref(),
        );
        let rsql = render_count_sql(
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
            return Ok((Vec::new(), 0, 0));
        }

        let arity = ctx.key_columns.len().max(1);

        if left_total == 0 {
            let spec = keys_only_spec(ctx, false, right.dialect())?;
            let rows = fetch_all_pages(right, &spec, ctx.verbose, queries).await?;
            return Ok((
                keys_only_diffs(&rows, arity, false),
                left_total,
                right_total,
            ));
        }
        if right_total == 0 {
            let spec = keys_only_spec(ctx, true, left.dialect())?;
            let rows = fetch_all_pages(left, &spec, ctx.verbose, queries).await?;
            return Ok((keys_only_diffs(&rows, arity, true), left_total, right_total));
        }

        if left_total.max(right_total) <= ctx.fetch_all_threshold {
            let lspec = full_row_spec(ctx, true, left.dialect(), None)?;
            let rspec = full_row_spec(ctx, false, right.dialect(), None)?;
            let detail = row_level_diff(
                left,
                right,
                &lspec,
                &rspec,
                None,
                arity,
                ctx.verbose,
                queries,
            )
            .await?;
            return Ok((detail.rows, left_total, right_total));
        }

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
        if buckets.is_empty() {
            return Ok((Vec::new(), left_total, right_total));
        }

        let mut rows = Vec::new();
        for b in buckets {
            let lpred = left.dialect().render_bucket_predicate(
                &ctx.left.plan.key_hash_exprs(left.dialect())?,
                n,
                b,
            );
            let rpred = right.dialect().render_bucket_predicate(
                &ctx.right.plan.key_hash_exprs(right.dialect())?,
                n,
                b,
            );
            let lspec = full_row_spec(ctx, true, left.dialect(), Some(&lpred))?;
            let rspec = full_row_spec(ctx, false, right.dialect(), Some(&rpred))?;
            let detail = row_level_diff(
                left,
                right,
                &lspec,
                &rspec,
                None,
                arity,
                ctx.verbose,
                queries,
            )
            .await?;
            rows.extend(detail.rows);
        }
        Ok((rows, left_total, right_total))
    }
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

fn render_count_sql(
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

fn parse_count(result: &crate::backend::QueryResult) -> Result<u64, DbError> {
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
    Ok(KeysetPageSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        columns: ctx.key_columns.clone(),
        raw_exprs: false,
        key_columns: ctx.key_columns.clone(),
        string_key: side.plan.string_key_flags(),
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
    let q = dialect.identifier_quote();
    let mut columns: Vec<String> = ctx.key_columns.iter().map(|c| quote_ident(q, c)).collect();
    for spec in side
        .plan
        .norm_specs
        .iter()
        .filter(|s| !ctx.key_columns.iter().any(|k| k == &s.name))
    {
        columns.push(dialect.normalize_expr(spec)?);
    }
    Ok(KeysetPageSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        columns,
        raw_exprs: true,
        key_columns: ctx.key_columns.clone(),
        string_key: side.plan.string_key_flags(),
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
    sample_limit: usize,
) -> DiffReport {
    let mut summary = DiffSummary {
        left_total,
        right_total,
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
    let warnings: Vec<String> = ctx
        .left
        .plan
        .warnings
        .iter()
        .chain(ctx.right.plan.warnings.iter())
        .chain(ctx.route_warnings.iter())
        .cloned()
        .collect();
    let sample: Vec<DiffRow> = diff_rows.into_iter().take(sample_limit).collect();
    let diff_count = summary.missing_left + summary.missing_right + summary.modified;
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta_diff::report::DiffStatus;
    use serde_json::json;

    #[test]
    fn parse_count_rejects_garbage() {
        let r = crate::backend::QueryResult {
            columns: vec!["cnt".into()],
            rows: vec![vec![json!("nope")]],
            row_count: 1,
        };
        assert!(parse_count(&r).is_err());
    }

    #[test]
    fn count_sql_includes_filter_and_quotes() {
        let sql = render_count_sql('`', Some("s"), "t", Some("bcrq='20260114'"));
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
