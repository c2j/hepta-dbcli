// ─── delta-diff NaiveDiffer: one full scan per side + client merge (#87) ──
//
// Per side a single `SELECT … WHERE …` full scan (raw key ORDER BY, no
// NLSSORT/COLLATE, no LIMIT); rows are matched client-side by canonical
// fingerprints (HashMap join), so correctness never depends on the server
// row order.

use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;

use crate::backend::{DbConn, DbError, ScanSqlSpec};
use crate::delta_diff::hash_diff::{capture_scn, open_snapshot};
use crate::delta_diff::keyed_diff;
use crate::delta_diff::report::{
    DiffReport, DiffRow, DiffStatus, DiffSummary, PerfMetrics, RowPayload, ShardResult,
    ShardStatus, TableRef,
};
use crate::delta_diff::rowdiff;
use crate::delta_diff::strategy::{side_filter, ConsistencyMode, DiffContext, DiffStrategy};

pub(crate) struct NaiveDiffer;

#[async_trait::async_trait]
impl DiffStrategy for NaiveDiffer {
    fn name(&self) -> &'static str {
        "naivediff"
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
            "[delta-diff] strategy=naivediff consistency={} keys={} naive_max_rows={}",
            ctx.consistency.as_str(),
            ctx.key_columns.join(","),
            ctx.naive_max_rows
        ));
        if ctx.consistency == ConsistencyMode::Snapshot {
            open_snapshot(left, ctx.verbose).await?;
            open_snapshot(right, ctx.verbose).await?;
            let _ = ctx.scns.set((
                capture_scn(left, ctx.verbose).await?,
                capture_scn(right, ctx.verbose).await?,
            ));
        }
        let result = self.diff_inner(left, right, ctx, &mut queries).await;
        if ctx.consistency == ConsistencyMode::Snapshot {
            ctx.vlog("[sql] COMMIT");
            let _ = left.query_drop("COMMIT").await;
            let _ = right.query_drop("COMMIT").await;
        }
        let (rows, left_total, right_total, extra_warnings) = result?;
        let mut report = assemble(ctx, rows, left_total, right_total, extra_warnings);
        report.started_at = started;
        report.finished_at = Utc::now();
        report.perf.queries_total = queries;
        Ok(report)
    }
}

/// Canonical client-side cell identity (issue #87 D2): Null and Text stay
/// strictly distinct, numeric-flagged text normalizes through Decimal so
/// `1` / `1.0` / `"1"` share one fingerprint across engines.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Fingerprint {
    Null,
    Num(Decimal),
    Text(String),
}

fn fingerprint_cell(v: &Value, numeric: bool) -> Fingerprint {
    if v.is_null() {
        return Fingerprint::Null;
    }
    let text = rowdiff::value_text(v);
    if numeric {
        if let Ok(d) = Decimal::from_str(text.trim()) {
            return Fingerprint::Num(d);
        }
    }
    Fingerprint::Text(text)
}

fn fingerprint_row(row: &[Value], flags: &[bool]) -> Vec<Fingerprint> {
    row.iter()
        .enumerate()
        .map(|(i, v)| fingerprint_cell(v, flags.get(i).copied().unwrap_or(false)))
        .collect()
}

/// Keyed multiset join over canonical fingerprints (issue #87 D1/D3).
/// Right rows pair with left rows in scan order; pairs with differing
/// non-key values become Modified, unpaired right rows become MissingLeft,
/// unpaired left rows (left scan order) become MissingRight. Correctness
/// never depends on server-side ORDER BY.
fn keyed_merge(
    lrows: &[Vec<Value>],
    rrows: &[Vec<Value>],
    arity: usize,
    lnum: &[bool],
    rnum: &[bool],
) -> Vec<DiffRow> {
    let arity = arity.max(1);
    let combined: Vec<bool> = lnum
        .iter()
        .zip(rnum.iter())
        .map(|(l, r)| *l && *r)
        .collect();

    let mut left_map: HashMap<Vec<Fingerprint>, Vec<usize>> = HashMap::new();
    for (i, row) in lrows.iter().enumerate() {
        left_map
            .entry(fingerprint_row(&row[..arity], &combined))
            .or_default()
            .push(i);
    }

    let mut out = Vec::new();
    let mut consumed = vec![false; lrows.len()];
    for rrow in rrows {
        let paired = left_map
            .get_mut(&fingerprint_row(&rrow[..arity], &combined))
            .and_then(|indices| indices.pop());
        match paired {
            Some(li) => {
                consumed[li] = true;
                let lrow = &lrows[li];
                if !rowdiff::row_values_equal(&lrow[arity..], &rrow[arity..], &combined[arity..]) {
                    out.push(DiffRow {
                        key: rowdiff::diff_key(lrow, arity),
                        left: Some(lrow.clone()),
                        right: Some(rrow.clone()),
                        status: DiffStatus::Modified,
                        confirmed: true,
                    });
                }
            }
            None => out.push(rowdiff::diff_row_n(
                rrow,
                arity,
                false,
                DiffStatus::MissingLeft,
            )),
        }
    }
    for (li, row) in lrows.iter().enumerate() {
        if !consumed[li] {
            out.push(rowdiff::diff_row_n(
                row,
                arity,
                true,
                DiffStatus::MissingRight,
            ));
        }
    }
    out
}

/// Keyless full-row multiset difference over canonical fingerprints
/// (issue #87 D6: the keyless naivediff mode is a bucketdiff superset —
/// it carries row payloads instead of hash counts). Rows cancel
/// min(left, right) per fingerprint; leftover right rows come first in
/// right scan order as MissingLeft, leftover left rows follow in left
/// scan order as MissingRight. There is no Modified status: without a
/// key there is no row identity to compare across sides.
fn keyless_merge(
    lrows: &[Vec<Value>],
    rrows: &[Vec<Value>],
    lnum: &[bool],
    rnum: &[bool],
) -> Vec<DiffRow> {
    let combined: Vec<bool> = lnum
        .iter()
        .zip(rnum.iter())
        .map(|(l, r)| *l && *r)
        .collect();

    let mut left_map: HashMap<Vec<Fingerprint>, Vec<usize>> = HashMap::new();
    for (i, row) in lrows.iter().enumerate() {
        left_map
            .entry(fingerprint_row(row, &combined))
            .or_default()
            .push(i);
    }

    let mut out = Vec::new();
    let mut consumed = vec![false; lrows.len()];
    for rrow in rrows {
        let paired = left_map
            .get_mut(&fingerprint_row(rrow, &combined))
            .and_then(|indices| indices.pop());
        match paired {
            Some(li) => consumed[li] = true,
            None => out.push(DiffRow {
                key: keyless_key(rrow, &combined),
                left: None,
                right: Some(rrow.clone()),
                status: DiffStatus::MissingLeft,
                confirmed: true,
            }),
        }
    }
    for (li, row) in lrows.iter().enumerate() {
        if !consumed[li] {
            out.push(DiffRow {
                key: keyless_key(row, &combined),
                left: Some(row.clone()),
                right: None,
                status: DiffStatus::MissingRight,
                confirmed: true,
            });
        }
    }
    out
}

/// Keyless rows have no identity, so the key must not look like one: a
/// canonical fingerprint-text prefix plus a loud suffix (bucketdiff's
/// `…(×l/r)` convention). Identical row contents share one key.
fn keyless_key(row: &[Value], flags: &[bool]) -> Value {
    let text = fingerprint_row(row, flags)
        .iter()
        .map(|f| match f {
            Fingerprint::Null => "NULL".to_string(),
            Fingerprint::Num(d) => d.to_string(),
            Fingerprint::Text(s) => s.clone(),
        })
        .collect::<Vec<_>>()
        .join("#");
    let prefix: String = text.chars().take(12).collect();
    Value::String(format!("{prefix}…(keyless)"))
}

impl NaiveDiffer {
    async fn diff_inner(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        queries: &mut u64,
    ) -> Result<(Vec<DiffRow>, u64, u64, Vec<String>), DbError> {
        let lscheme = left.dialect().url_scheme();
        let rscheme = right.dialect().url_scheme();
        let lsql = keyed_diff::render_count_sql(
            lscheme,
            left.dialect().identifier_quote(),
            ctx.left.schema.as_deref(),
            &ctx.left.table,
            side_filter(ctx, lscheme).as_deref(),
        );
        let rsql = keyed_diff::render_count_sql(
            rscheme,
            right.dialect().identifier_quote(),
            ctx.right.schema.as_deref(),
            &ctx.right.table,
            side_filter(ctx, rscheme).as_deref(),
        );
        ctx.vlog(format!("[sql:left] {lsql}"));
        ctx.vlog(format!("[sql:right] {rsql}"));
        let (lr, rr) = tokio::join!(left.query(&lsql), right.query(&rsql));
        *queries += 2;
        let left_total = keyed_diff::parse_count(&lr?)?;
        let right_total = keyed_diff::parse_count(&rr?)?;

        if ctx.naive_max_rows > 0 && left_total.max(right_total) > ctx.naive_max_rows {
            return Err(DbError::query(format!(
                "naivediff row cap exceeded: left={left_total} right={right_total} cap={}; \
                 use --strategy keyeddiff for larger filtered sets",
                ctx.naive_max_rows
            )));
        }

        if left_total == 0 && right_total == 0 {
            return Ok((Vec::new(), 0, 0, Vec::new()));
        }

        let lspec = if left_total == 0 {
            None
        } else {
            Some(scan_spec(ctx, true, left.dialect())?)
        };
        let rspec = if right_total == 0 {
            None
        } else {
            Some(scan_spec(ctx, false, right.dialect())?)
        };
        let mut lqueries = 0u64;
        let mut rqueries = 0u64;
        let (lscan, rscan) = tokio::join!(
            async {
                match &lspec {
                    Some(spec) => Ok::<_, DbError>(Some(
                        scan_rows(&mut *left, spec, ctx.verbose, &mut lqueries).await?,
                    )),
                    None => Ok(None),
                }
            },
            async {
                match &rspec {
                    Some(spec) => Ok::<_, DbError>(Some(
                        scan_rows(&mut *right, spec, ctx.verbose, &mut rqueries).await?,
                    )),
                    None => Ok(None),
                }
            },
        );
        *queries += lqueries + rqueries;
        let lrows = lscan?.unwrap_or_default();
        let lfetched = lrows.len() as u64;
        let rrows = rscan?.unwrap_or_default();
        let rfetched = rrows.len() as u64;

        let mut extra = Vec::new();
        if lfetched != left_total || rfetched != right_total {
            extra.push(format!(
                "naivediff scan count mismatch: left count={left_total} fetched={lfetched}, \
                 right count={right_total} fetched={rfetched}; \
                 concurrent writes may have changed rows"
            ));
        }

        let (lnum, rnum) = if ctx.key_columns.is_empty() {
            (
                keyless_numeric_flags(ctx, true),
                keyless_numeric_flags(ctx, false),
            )
        } else {
            (
                keyed_diff::full_row_numeric_flags(ctx, true),
                keyed_diff::full_row_numeric_flags(ctx, false),
            )
        };
        validate_flag_alignment(&lrows, &rrows, &lnum, &rnum)?;
        let rows = if ctx.key_columns.is_empty() {
            keyless_merge(&lrows, &rrows, &lnum, &rnum)
        } else {
            keyed_merge(&lrows, &rrows, ctx.key_columns.len(), &lnum, &rnum)
        };
        Ok((rows, left_total, right_total, extra))
    }
}

/// Misaligned type flags would silently turn every compared row into a
/// false Modified (row_values_equal length-guard), so fail loudly first —
/// same contract as `row_level_diff`.
fn validate_flag_alignment(
    lrows: &[Vec<Value>],
    rrows: &[Vec<Value>],
    lnum: &[bool],
    rnum: &[bool],
) -> Result<(), DbError> {
    if lnum.len() != rnum.len()
        || lrows.iter().any(|row| row.len() != lnum.len())
        || rrows.iter().any(|row| row.len() != rnum.len())
    {
        return Err(DbError::config(
            "naivediff: row comparison type flags do not align with selected columns",
        ));
    }
    Ok(())
}

/// One statement per side: bare quoted key columns + normalized value
/// expressions; ORDER BY uses raw key columns only (empty for keyless).
fn scan_spec(
    ctx: &DiffContext,
    is_left: bool,
    dialect: &dyn crate::backend::Dialect,
) -> Result<ScanSqlSpec, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let side_keys = ctx.side_key_columns(is_left);
    let (columns, order_by) = if side_keys.is_empty() {
        let columns = side
            .plan
            .norm_specs
            .iter()
            .map(|spec| dialect.normalize_expr(spec))
            .collect::<Result<Vec<_>, _>>()?;
        (columns, Vec::new())
    } else {
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
        let order_by = side_keys
            .iter()
            .map(|c| dialect.quote_catalog_ident(c))
            .collect();
        (columns, order_by)
    };
    Ok(ScanSqlSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        columns,
        order_by,
        filter: side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
    })
}

async fn scan_rows(
    conn: &mut (dyn DbConn + Send),
    spec: &ScanSqlSpec,
    verbose: bool,
    queries: &mut u64,
) -> Result<Vec<Vec<Value>>, DbError> {
    let sql = conn.dialect().render_scan_sql(spec);
    if verbose {
        eprintln!("[sql] {sql}");
    }
    let result = conn.query(&sql).await?;
    *queries += 1;
    Ok(result.rows)
}

fn keyless_numeric_flags(ctx: &DiffContext, is_left: bool) -> Vec<bool> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let names: Vec<String> = side
        .plan
        .norm_specs
        .iter()
        .map(|s| s.name.clone())
        .collect();
    side.plan.numeric_value_flags_for(&names)
}

fn assemble(
    ctx: &DiffContext,
    diff_rows: Vec<DiffRow>,
    left_total: u64,
    right_total: u64,
    extra_warnings: Vec<String>,
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
        .chain(extra_warnings)
        .collect();
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
        strategy: "naivediff".into(),
        consistency: ctx.consistency.as_str().into(),
        hash_algorithm: "none".into(),
        summary,
        perf: PerfMetrics::default(),
        shards: vec![ShardResult {
            shard_id: "naive-all".into(),
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
        sample_diffs: diff_rows,
        warnings,
        row_payload: RowPayload::Columns,
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
    use crate::delta_diff::report::DiffStatus;
    use serde_json::json;

    #[test]
    fn compare_primitives_are_shared_across_modules() {
        assert!(crate::delta_diff::rowdiff::row_values_equal(
            &[json!(1)],
            &[json!(1)],
            &[true]
        ));
    }

    #[test]
    fn keyed_merge_reports_modified_when_nonkey_values_differ() {
        let lrows = vec![vec![json!("A"), json!("20251215"), json!("100.00")]];
        let rrows = vec![vec![json!("A"), json!("20251215"), json!("999.00")]];
        let rows = keyed_merge(
            &lrows,
            &rrows,
            2,
            &[false, false, true],
            &[false, false, true],
        );
        assert_eq!(rows.len(), 1, "one paired row, not one-per-side: {rows:?}");
        assert_eq!(rows[0].status, DiffStatus::Modified);
        assert_eq!(rows[0].key, json!(["A", "20251215"]));
        assert!(rows[0].left.is_some() && rows[0].right.is_some());
    }

    #[test]
    fn keyed_merge_treats_numeric_one_and_text_one_as_same_key() {
        let lrows = vec![vec![json!(1), json!("x")]];
        let rrows = vec![vec![json!("1"), json!("x")]];
        let rows = keyed_merge(&lrows, &rrows, 1, &[true, false], &[true, false]);
        assert!(rows.is_empty(), "1 vs \"1\" is the same key: {rows:?}");
    }

    #[test]
    fn keyed_merge_marks_rows_missing_on_right_when_absent() {
        let lrows = vec![vec![json!("a"), json!(1)], vec![json!("b"), json!(2)]];
        let rows = keyed_merge(&lrows, &[], 1, &[false, false], &[false, false]);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.status == DiffStatus::MissingRight));
        assert!(rows.iter().all(|r| r.left.is_some() && r.right.is_none()));
    }

    #[test]
    fn keyed_merge_marks_rows_missing_on_left_when_absent() {
        let rrows = vec![vec![json!("a"), json!(1)], vec![json!("b"), json!(2)]];
        let rows = keyed_merge(&[], &rrows, 1, &[false, false], &[false, false]);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.status == DiffStatus::MissingLeft));
        assert!(rows.iter().all(|r| r.right.is_some() && r.left.is_none()));
    }

    #[test]
    fn keyed_merge_pairs_duplicate_keys_without_false_modified() {
        let lrows = vec![vec![json!("k"), json!("v1")], vec![json!("k"), json!("v2")]];
        let rrows = vec![vec![json!("k"), json!("v2")], vec![json!("k"), json!("v1")]];
        let rows = keyed_merge(&lrows, &rrows, 1, &[false, false], &[false, false]);
        assert!(
            rows.is_empty(),
            "multiset pairing must not report false Modified: {rows:?}"
        );
    }

    #[test]
    fn keyed_merge_null_key_pairs_with_null_key() {
        let lrows = vec![vec![Value::Null, json!("x")]];
        let rrows = vec![vec![Value::Null, json!("x")], vec![Value::Null, json!("y")]];
        let rows = keyed_merge(&lrows, &rrows, 1, &[false, false], &[false, false]);
        assert_eq!(rows.len(), 1, "identical null-key rows pair: {rows:?}");
        assert_eq!(rows[0].status, DiffStatus::MissingLeft);
        assert_eq!(rows[0].key, Value::Null);
    }

    #[test]
    fn keyed_merge_compares_decimals_exactly_across_scale() {
        let lrows = vec![vec![json!("k"), json!("1.50")]];
        let rrows = vec![vec![json!("k"), json!("1.5")]];
        let rows = keyed_merge(&lrows, &rrows, 1, &[false, true], &[false, true]);
        assert!(rows.is_empty(), "1.50 == 1.5 numerically: {rows:?}");
    }

    #[test]
    fn keyless_merge_reports_only_missing_statuses() {
        let lrows = vec![vec![json!("same")], vec![json!("left-only")]];
        let rrows = vec![vec![json!("same")], vec![json!("right-only")]];
        let rows = keyless_merge(&lrows, &rrows, &[false], &[false]);
        assert_eq!(rows.len(), 2, "identical rows cancel: {rows:?}");
        assert!(rows.iter().all(|r| r.status != DiffStatus::Modified));
        assert!(rows
            .iter()
            .any(|r| r.status == DiffStatus::MissingRight && r.left.is_some()));
        assert!(rows
            .iter()
            .any(|r| r.status == DiffStatus::MissingLeft && r.right.is_some()));
    }

    #[test]
    fn keyless_merge_multiset_cancel_counts() {
        let lrows = vec![vec![json!("x")], vec![json!("x")]];
        let rrows = vec![vec![json!("x")]];
        let rows = keyless_merge(&lrows, &rrows, &[false], &[false]);
        assert_eq!(rows.len(), 1, "2 left vs 1 right cancels one: {rows:?}");
        assert_eq!(rows[0].status, DiffStatus::MissingRight);
    }

    #[test]
    fn keyless_merge_null_and_empty_string_are_distinct_fingerprints() {
        let lrows = vec![vec![Value::Null]];
        let rrows = vec![vec![json!("")]];
        let rows = keyless_merge(&lrows, &rrows, &[false], &[false]);
        assert_eq!(
            rows.len(),
            2,
            "NULL and '' must never cancel each other (Java §6.1): {rows:?}"
        );
    }

    #[test]
    fn keyless_merge_numeric_equivalence_across_scales() {
        let lrows = vec![vec![json!(1)], vec![json!(1.0)]];
        let rrows = vec![vec![json!("1.0")]];
        let rows = keyless_merge(&lrows, &rrows, &[true], &[true]);
        assert_eq!(
            rows.len(),
            1,
            "1 / 1.0 / \"1.0\" are one fingerprint value: {rows:?}"
        );
        assert_eq!(rows[0].status, DiffStatus::MissingRight);
    }

    #[test]
    fn keyless_rows_get_descriptive_non_identity_keys() {
        let lrows = vec![vec![json!("left-only")], vec![json!("left-only")]];
        let rrows = vec![vec![json!("right-only")]];
        let rows = keyless_merge(&lrows, &rrows, &[false], &[false]);
        for row in &rows {
            let key = row.key.as_str().expect("keyless key must be a string");
            assert!(key.ends_with("…(keyless)"), "{key}");
            assert_ne!(key, "left-only", "key must not read as an identity");
            assert_ne!(key, "right-only", "key must not read as an identity");
        }
        let surplus_left: Vec<&str> = rows
            .iter()
            .filter(|row| row.status == DiffStatus::MissingRight)
            .filter_map(|row| row.key.as_str())
            .collect();
        assert_eq!(surplus_left.len(), 2, "{rows:?}");
        assert_eq!(
            surplus_left[0], surplus_left[1],
            "identical surplus rows share one key"
        );
    }

    // ─── end-to-end flow over scripted connections ─────────────────────

    mod flow {
        use super::*;
        use crate::backend::mysql::dialect::MySqlDialect;
        use crate::backend::{DbPool, Dialect, QueryResult};
        use crate::delta_diff::metadata::TablePlan;
        use crate::delta_diff::report::RowPayload;
        use crate::delta_diff::strategy::{ConsistencyMode, SideCtx};
        use async_trait::async_trait;
        use std::collections::VecDeque;
        use std::sync::Arc;

        /// FIFO scripted connection: each `query` pops the next canned
        /// result (call order per side: COUNT, then scan).
        struct ScriptedConn {
            responses: VecDeque<QueryResult>,
            dialect: MySqlDialect,
        }

        impl ScriptedConn {
            fn count(n: u64) -> QueryResult {
                QueryResult {
                    columns: vec!["cnt".into()],
                    rows: vec![vec![json!(n)]],
                    row_count: 1,
                    rows_affected: None,
                }
            }

            fn scan(rows: Vec<Vec<Value>>) -> QueryResult {
                QueryResult {
                    columns: vec![],
                    row_count: rows.len(),
                    rows,
                    rows_affected: None,
                }
            }
        }

        #[async_trait]
        impl DbConn for ScriptedConn {
            async fn query(&mut self, _sql: &str) -> Result<QueryResult, DbError> {
                Ok(self
                    .responses
                    .pop_front()
                    .unwrap_or_else(QueryResult::empty))
            }
            async fn exec(
                &mut self,
                _sql: &str,
                _params: &[Value],
            ) -> Result<QueryResult, DbError> {
                Err(DbError::unsupported("scripted"))
            }
            async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
                Err(DbError::unsupported("scripted"))
            }
            fn dialect(&self) -> &dyn Dialect {
                &self.dialect
            }
        }

        fn plan(keys: &[&str], values: &[(&str, &str)]) -> TablePlan {
            let mut specs: Vec<crate::backend::ColumnNormSpec> = keys
                .iter()
                .map(|k| crate::backend::ColumnNormSpec {
                    name: (*k).into(),
                    data_type: "varchar(32)".into(),
                    nullable: true,
                    rtrim_fixed_char: false,
                })
                .collect();
            for (name, ty) in values {
                specs.push(crate::backend::ColumnNormSpec {
                    name: (*name).into(),
                    data_type: (*ty).into(),
                    nullable: true,
                    rtrim_fixed_char: false,
                });
            }
            TablePlan {
                url_scheme: "mysql".into(),
                key_columns: keys.iter().map(|k| (*k).into()).collect(),
                compare_columns: specs.iter().map(|s| s.name.clone()).collect(),
                norm_specs: specs,
                warnings: vec![],
                key_specs: vec![],
            }
        }

        fn side(keys: &[&str], values: &[(&str, &str)]) -> SideCtx {
            SideCtx {
                connection_name: "x".into(),
                schema: Some("s".into()),
                table: "t".into(),
                plan: plan(keys, values),
            }
        }

        fn dummy_pool() -> std::sync::Arc<dyn DbPool> {
            struct Pool;
            #[async_trait]
            impl DbPool for Pool {
                async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
                    Err(DbError::unsupported("dummy"))
                }
            }
            std::sync::Arc::new(Pool)
        }

        struct Flow {
            left: Box<dyn DbConn + Send>,
            right: Box<dyn DbConn + Send>,
        }

        async fn run(flow: Flow, naive_max_rows: u64) -> Result<DiffReport, DbError> {
            let ctx = DiffContext {
                left: side(&["k1", "k2"], &[("amt", "decimal(20,6)")]),
                right: side(&["k1", "k2"], &[("amt", "decimal(20,6)")]),
                left_pool: dummy_pool(),
                right_pool: dummy_pool(),
                key_column: "k1".into(),
                key_columns: vec!["k1".into(), "k2".into()],
                left_key_columns: vec!["k1".into(), "k2".into()],
                right_key_columns: vec!["k1".into(), "k2".into()],
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
                naive_max_rows,
                strict: false,
                scns: std::sync::OnceLock::new(),
                verbose: false,
            };
            let mut left = flow.left;
            let mut right = flow.right;
            NaiveDiffer.diff(&mut *left, &mut *right, &ctx).await
        }

        fn keyed_rows(rows: &[(&str, i64, &str)]) -> Vec<Vec<Value>> {
            rows.iter()
                .map(|(k1, k2, amt)| vec![json!(k1), json!(k2), json!(amt)])
                .collect()
        }

        #[tokio::test]
        async fn naive_diff_refuses_over_cap() {
            let err = run(
                Flow {
                    left: Box::new(ScriptedConn {
                        responses: VecDeque::from([ScriptedConn::count(300)]),
                        dialect: MySqlDialect,
                    }),
                    right: Box::new(ScriptedConn {
                        responses: VecDeque::from([ScriptedConn::count(300)]),
                        dialect: MySqlDialect,
                    }),
                },
                200,
            )
            .await
            .expect_err("over-cap must refuse");
            assert!(err.to_string().contains("row cap exceeded"), "{err}");
            assert!(err.to_string().contains("keyeddiff"), "{err}");
        }

        #[tokio::test]
        async fn naive_diff_zero_diff_single_scan() {
            let lrows = keyed_rows(&[("A", 1, "100.00"), ("B", 2, "5.50")]);
            let report = run(
                Flow {
                    left: Box::new(ScriptedConn {
                        responses: VecDeque::from([
                            ScriptedConn::count(2),
                            ScriptedConn::scan(lrows.clone()),
                        ]),
                        dialect: MySqlDialect,
                    }),
                    right: Box::new(ScriptedConn {
                        responses: VecDeque::from([
                            ScriptedConn::count(2),
                            ScriptedConn::scan(lrows),
                        ]),
                        dialect: MySqlDialect,
                    }),
                },
                200_000,
            )
            .await
            .expect("identical data");
            assert!(
                report.sample_diffs.is_empty(),
                "zero diff expected: {:?}",
                report.sample_diffs
            );
            assert_eq!(report.perf.queries_total, 4, "2 COUNT + 2 scan");
        }

        #[tokio::test]
        async fn naive_diff_reports_modified_missing_and_key_shape() {
            let lrows = keyed_rows(&[("A", 1, "100.00"), ("B", 2, "5.50"), ("C", 3, "7.25")]);
            let rrows = keyed_rows(&[("A", 1, "999.00"), ("D", 4, "1.00")]);
            let report = run(
                Flow {
                    left: Box::new(ScriptedConn {
                        responses: VecDeque::from([
                            ScriptedConn::count(3),
                            ScriptedConn::scan(lrows),
                        ]),
                        dialect: MySqlDialect,
                    }),
                    right: Box::new(ScriptedConn {
                        responses: VecDeque::from([
                            ScriptedConn::count(2),
                            ScriptedConn::scan(rrows),
                        ]),
                        dialect: MySqlDialect,
                    }),
                },
                200_000,
            )
            .await
            .expect("mixed diffs");
            assert_eq!(report.summary.modified, 1, "{:?}", report.sample_diffs);
            assert_eq!(report.summary.missing_right, 2, "{:?}", report.sample_diffs);
            assert_eq!(report.summary.missing_left, 1, "{:?}", report.sample_diffs);
            let modified = report
                .sample_diffs
                .iter()
                .find(|d| d.status == DiffStatus::Modified)
                .expect("modified row");
            assert_eq!(modified.key, json!(["A", 1]), "composite key is JSON array");
            let missing_right = report
                .sample_diffs
                .iter()
                .find(|d| d.status == DiffStatus::MissingRight)
                .expect("missing-right row");
            assert_eq!(missing_right.key, json!(["B", 2]));
        }

        #[tokio::test]
        async fn naive_diff_strategy_report_fields() {
            let lrows = keyed_rows(&[("A", 1, "100.00")]);
            let report = run(
                Flow {
                    left: Box::new(ScriptedConn {
                        responses: VecDeque::from([
                            ScriptedConn::count(1),
                            ScriptedConn::scan(lrows.clone()),
                        ]),
                        dialect: MySqlDialect,
                    }),
                    right: Box::new(ScriptedConn {
                        responses: VecDeque::from([
                            ScriptedConn::count(1),
                            ScriptedConn::scan(lrows),
                        ]),
                        dialect: MySqlDialect,
                    }),
                },
                200_000,
            )
            .await
            .expect("run");
            assert_eq!(report.strategy, "naivediff");
            assert_eq!(report.hash_algorithm, "none");
            assert_eq!(report.row_payload, RowPayload::Columns);
            assert_eq!(report.shards.len(), 1);
            assert_eq!(report.shards[0].shard_id, "naive-all");
        }

        #[tokio::test]
        async fn naive_diff_scans_run_concurrently() {
            use std::sync::atomic::{AtomicUsize, Ordering};

            /// Counts in-flight scan queries (COUNT responses return without
            /// yielding, isolating the probe to the scan phase).
            struct ProbeConn {
                inflight: Arc<AtomicUsize>,
                max_inflight: Arc<AtomicUsize>,
                responses: VecDeque<QueryResult>,
                dialect: MySqlDialect,
            }

            #[async_trait]
            impl DbConn for ProbeConn {
                async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
                    if !sql.contains("COUNT(*)") {
                        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                        self.max_inflight.fetch_max(now, Ordering::SeqCst);
                        for _ in 0..5 {
                            tokio::task::yield_now().await;
                        }
                        self.inflight.fetch_sub(1, Ordering::SeqCst);
                    }
                    Ok(self
                        .responses
                        .pop_front()
                        .unwrap_or_else(QueryResult::empty))
                }
                async fn exec(
                    &mut self,
                    _sql: &str,
                    _params: &[Value],
                ) -> Result<QueryResult, DbError> {
                    Err(DbError::unsupported("probe"))
                }
                async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
                    Err(DbError::unsupported("probe"))
                }
                fn dialect(&self) -> &dyn Dialect {
                    &self.dialect
                }
            }

            let inflight = Arc::new(AtomicUsize::new(0));
            let max_inflight = Arc::new(AtomicUsize::new(0));
            let probe = |responses: VecDeque<QueryResult>| ProbeConn {
                inflight: Arc::clone(&inflight),
                max_inflight: Arc::clone(&max_inflight),
                responses,
                dialect: MySqlDialect,
            };
            let rows = keyed_rows(&[("A", 1, "1.00")]);
            let report = run(
                Flow {
                    left: Box::new(probe(VecDeque::from([
                        ScriptedConn::count(1),
                        ScriptedConn::scan(rows.clone()),
                    ]))),
                    right: Box::new(probe(VecDeque::from([
                        ScriptedConn::count(1),
                        ScriptedConn::scan(rows),
                    ]))),
                },
                200_000,
            )
            .await
            .expect("run");
            assert!(report.sample_diffs.is_empty());
            assert_eq!(report.perf.queries_total, 4);
            assert_eq!(
                max_inflight.load(Ordering::SeqCst),
                2,
                "both side scans must be in flight together"
            );
        }

        #[tokio::test]
        async fn naive_diff_rejects_misaligned_numeric_flags() {
            let ctx = DiffContext {
                left: side(&["k1", "k2"], &[("amt", "decimal(20,6)"), ("extra", "int")]),
                right: side(&["k1", "k2"], &[("amt", "decimal(20,6)")]),
                left_pool: dummy_pool(),
                right_pool: dummy_pool(),
                key_column: "k1".into(),
                key_columns: vec!["k1".into(), "k2".into()],
                left_key_columns: vec!["k1".into(), "k2".into()],
                right_key_columns: vec!["k1".into(), "k2".into()],
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
                naive_max_rows: 200_000,
                strict: false,
                scns: std::sync::OnceLock::new(),
                verbose: false,
            };
            let mut left = Box::new(ScriptedConn {
                responses: VecDeque::from([
                    ScriptedConn::count(1),
                    ScriptedConn::scan(vec![vec![
                        json!("a"),
                        json!("1"),
                        json!("10"),
                        json!("20"),
                    ]]),
                ]),
                dialect: MySqlDialect,
            }) as Box<dyn DbConn + Send>;
            let mut right = Box::new(ScriptedConn {
                responses: VecDeque::from([
                    ScriptedConn::count(1),
                    ScriptedConn::scan(vec![vec![json!("a"), json!("1"), json!("10")]]),
                ]),
                dialect: MySqlDialect,
            }) as Box<dyn DbConn + Send>;
            let err = NaiveDiffer
                .diff(&mut *left, &mut *right, &ctx)
                .await
                .expect_err("misaligned flags must error, not fake Modified rows");
            assert!(
                err.to_string()
                    .contains("row comparison type flags do not align"),
                "{err}"
            );
        }

        #[tokio::test]
        async fn naive_diff_keyless_reports_only_missing() {
            let ctx_rows_l = vec![vec![json!("left-only")]];
            let ctx_rows_r = vec![vec![json!("right-only")]];
            let ctx = DiffContext {
                left: side(&[], &[("v", "varchar(32)")]),
                right: side(&[], &[("v", "varchar(32)")]),
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
                naive_max_rows: 200_000,
                strict: false,
                scns: std::sync::OnceLock::new(),
                verbose: false,
            };
            let mut left = Box::new(ScriptedConn {
                responses: VecDeque::from([ScriptedConn::count(1), ScriptedConn::scan(ctx_rows_l)]),
                dialect: MySqlDialect,
            }) as Box<dyn DbConn + Send>;
            let mut right = Box::new(ScriptedConn {
                responses: VecDeque::from([ScriptedConn::count(1), ScriptedConn::scan(ctx_rows_r)]),
                dialect: MySqlDialect,
            }) as Box<dyn DbConn + Send>;
            let report = NaiveDiffer
                .diff(&mut *left, &mut *right, &ctx)
                .await
                .expect("keyless run");
            assert_eq!(report.summary.missing_right, 1, "{:?}", report.sample_diffs);
            assert_eq!(report.summary.missing_left, 1, "{:?}", report.sample_diffs);
            assert_eq!(report.summary.modified, 0);
            assert_eq!(report.perf.queries_total, 4);
        }
    }
}

// ─── DuckDB end-to-end (embedded — the layer allowed to expose real bugs) ──

#[cfg(all(test, feature = "duckdb"))]
mod duckdb_e2e_tests {
    use super::*;
    use crate::backend::duckdb::DuckDbFactory;
    use crate::backend::{BackendFactory, DbPool};
    use crate::delta_diff::metadata::TablePlan;
    use crate::delta_diff::strategy::SideCtx;
    use std::sync::Arc;

    async fn pool_with(ddl: &str, rows: &str) -> Arc<dyn DbPool> {
        let pool = DuckDbFactory
            .connect("duckdb://:memory:", None)
            .await
            .expect("duckdb pool");
        let mut conn = pool.acquire().await.expect("conn");
        conn.query_drop(ddl).await.expect("create");
        if !rows.is_empty() {
            conn.query_drop(rows).await.expect("insert");
        }
        pool
    }

    fn dummy_pool() -> Arc<dyn DbPool> {
        struct Pool;
        #[async_trait::async_trait]
        impl DbPool for Pool {
            async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
                Err(DbError::unsupported("dummy"))
            }
        }
        Arc::new(Pool)
    }

    fn plan(amt_type: &str) -> TablePlan {
        TablePlan {
            url_scheme: "duckdb".into(),
            key_columns: vec!["k1".into(), "k2".into()],
            compare_columns: vec!["k1".into(), "k2".into(), "amt".into()],
            norm_specs: vec![
                crate::backend::ColumnNormSpec {
                    name: "k1".into(),
                    data_type: "VARCHAR".into(),
                    nullable: true,
                    rtrim_fixed_char: false,
                },
                crate::backend::ColumnNormSpec {
                    name: "k2".into(),
                    data_type: "VARCHAR".into(),
                    nullable: true,
                    rtrim_fixed_char: false,
                },
                crate::backend::ColumnNormSpec {
                    name: "amt".into(),
                    data_type: amt_type.into(),
                    nullable: true,
                    rtrim_fixed_char: false,
                },
            ],
            warnings: vec![],
            key_specs: vec![],
        }
    }

    async fn run_diff(
        left: Arc<dyn DbPool>,
        right: Arc<dyn DbPool>,
        left_amt: &str,
        right_amt: &str,
    ) -> DiffReport {
        let mut lconn = left.acquire().await.expect("left conn");
        let mut rconn = right.acquire().await.expect("right conn");
        let ctx = DiffContext {
            left: SideCtx {
                connection_name: "l".into(),
                schema: Some("main".into()),
                table: "t_nv".into(),
                plan: plan(left_amt),
            },
            right: SideCtx {
                connection_name: "r".into(),
                schema: Some("main".into()),
                table: "t_nv".into(),
                plan: plan(right_amt),
            },
            left_pool: dummy_pool(),
            right_pool: dummy_pool(),
            key_column: "k1".into(),
            key_columns: vec!["k1".into(), "k2".into()],
            left_key_columns: vec!["k1".into(), "k2".into()],
            right_key_columns: vec!["k1".into(), "k2".into()],
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
            naive_max_rows: 200_000,
            strict: false,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        };
        NaiveDiffer
            .diff(&mut *lconn, &mut *rconn, &ctx)
            .await
            .expect("naivediff run")
    }

    #[tokio::test]
    async fn duckdb_naivediff_zero_diff_runs_four_queries() {
        let ddl = "CREATE TABLE t_nv (k1 VARCHAR, k2 VARCHAR, amt DECIMAL(20,6))";
        let rows = "INSERT INTO t_nv VALUES \
                    ('a','1',1.50),('b','2',2.25),('K',NULL,3.75)";
        let report = run_diff(
            pool_with(ddl, rows).await,
            pool_with(ddl, rows).await,
            "DECIMAL(20,6)",
            "DECIMAL(20,6)",
        )
        .await;
        assert!(
            report.sample_diffs.is_empty(),
            "identical sides must produce zero diffs: {:?}",
            report.sample_diffs
        );
        assert_eq!(report.perf.queries_total, 4, "2 COUNT + 2 scan");
        assert_eq!(report.summary.left_total, 3);
        assert_eq!(report.summary.right_total, 3);
    }

    #[tokio::test]
    async fn duckdb_naivediff_reports_modified_and_missing_with_null_keys() {
        let ddl = "CREATE TABLE t_nv (k1 VARCHAR, k2 VARCHAR, amt DECIMAL(20,6))";
        let left_rows = "INSERT INTO t_nv VALUES \
                         ('a','1',1.50),('b','2',2.25),('K',NULL,3.75)";
        let right_rows = "INSERT INTO t_nv VALUES \
                          ('a','1',9.99),('c','3',4.00),('K',NULL,3.75)";
        let report = run_diff(
            pool_with(ddl, left_rows).await,
            pool_with(ddl, right_rows).await,
            "DECIMAL(20,6)",
            "DECIMAL(20,6)",
        )
        .await;
        assert_eq!(report.summary.modified, 1, "{:?}", report.sample_diffs);
        assert_eq!(report.summary.missing_right, 1, "{:?}", report.sample_diffs);
        assert_eq!(report.summary.missing_left, 1, "{:?}", report.sample_diffs);
        let modified = report
            .sample_diffs
            .iter()
            .find(|d| d.status == DiffStatus::Modified)
            .expect("modified row");
        assert_eq!(modified.key, serde_json::json!(["a", "1"]));
    }

    #[tokio::test]
    async fn duckdb_naivediff_cross_scale_decimal_reports_no_diff() {
        // Left stores 1.50 in DECIMAL(20,2), right stores 1.5 in
        // DECIMAL(20,6): client-side Decimal comparison must see them equal.
        let left = pool_with(
            "CREATE TABLE t_nv (k1 VARCHAR, k2 VARCHAR, amt DECIMAL(20,2))",
            "INSERT INTO t_nv VALUES ('a','1',1.50),('K',NULL,3.75)",
        )
        .await;
        let right = pool_with(
            "CREATE TABLE t_nv (k1 VARCHAR, k2 VARCHAR, amt DECIMAL(20,6))",
            "INSERT INTO t_nv VALUES ('a','1',1.5),('K',NULL,3.750000)",
        )
        .await;
        let report = run_diff(left, right, "DECIMAL(20,2)", "DECIMAL(20,6)").await;
        assert!(
            report.sample_diffs.is_empty(),
            "1.50 vs 1.5 is one value across scales: {:?}",
            report.sample_diffs
        );
    }
}
