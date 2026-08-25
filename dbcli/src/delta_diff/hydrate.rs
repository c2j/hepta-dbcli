// ─── keyless hash → row attach and SQL builders (no I/O) ───────────────

use std::collections::HashMap;

use serde_json::Value;

use crate::backend::sql_literal;
use crate::delta_diff::cmd::DeltaDiffArgs;
use crate::delta_diff::pairing::pair_plans;
use crate::delta_diff::report::{DiffReport, DiffStatus, RowPayload};

pub(crate) const HASH_IN_CHUNK: usize = 500;

pub(crate) fn attach_keyless_rows(
    report: &mut DiffReport,
    fetched: &HashMap<String, Vec<Value>>,
    value_columns: Vec<String>,
) {
    if fetched.is_empty()
        || !report.sample_diffs.iter().all(|row| {
            row_hash(row)
                .as_ref()
                .is_some_and(|hash| fetched.contains_key(hash))
        })
    {
        return;
    }
    for row in &mut report.sample_diffs {
        let Some(hash) = row_hash(row) else {
            continue;
        };
        let Some(cells) = fetched.get(&hash) else {
            continue;
        };
        let lc = cell_u64(row.left.as_deref(), 1);
        let rc = cell_u64(row.right.as_deref(), 1);
        row.key = serde_json::json!({ "hash": hash, "left": lc, "right": rc });
        match row.status {
            DiffStatus::MissingLeft => row.right = Some(cells.clone()),
            DiffStatus::MissingRight => row.left = Some(cells.clone()),
            DiffStatus::Modified => {
                if row.left.is_some() {
                    row.left = Some(cells.clone());
                }
                if row.right.is_some() {
                    row.right = Some(cells.clone());
                }
            }
        }
    }
    report.value_columns = value_columns;
    report.row_payload = RowPayload::Columns;
    debug_assert!(report
        .sample_diffs
        .iter()
        .all(|row| row.key.get("hash").is_some()));
}

fn cell_u64(row: Option<&[Value]>, idx: usize) -> u64 {
    match row.and_then(|r| r.get(idx)) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

pub(crate) fn row_hash(row: &crate::delta_diff::report::DiffRow) -> Option<String> {
    row.left
        .as_ref()
        .or(row.right.as_ref())
        .and_then(|r| r.first())
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub(crate) struct HashInSql<'a> {
    pub scheme: &'a str,
    pub quote: char,
    pub schema: Option<&'a str>,
    pub table: &'a str,
    pub columns: &'a [String],
    pub filter: Option<&'a str>,
    pub row_hash_expr: &'a str,
    pub hashes: &'a [String],
    pub backslash_escape: bool,
}

pub(crate) fn render_hash_in_sql(p: &HashInSql<'_>) -> String {
    let table = crate::backend::quote_table_scheme(p.scheme, p.quote, p.schema, p.table);
    let cols = if p.columns.is_empty() {
        "*".to_string()
    } else {
        p.columns
            .iter()
            .map(|c| crate::backend::quote_ident(p.quote, c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let list = p
        .hashes
        .iter()
        .map(|h| sql_literal(&Value::from(h.as_str()), p.backslash_escape))
        .collect::<Vec<_>>()
        .join(", ");
    let mut conds = Vec::new();
    if let Some(f) = p.filter {
        conds.push(format!("({f})"));
    }
    conds.push(format!("{} IN ({list})", p.row_hash_expr));
    format!("SELECT {cols} FROM {table} WHERE {}", conds.join(" AND "))
}

fn render_side_hash_sql(
    side: &crate::delta_diff::strategy::SideCtx,
    scheme: &str,
    quote: char,
    filter: Option<&str>,
    row_hash_expr: &str,
    hashes: &[String],
) -> String {
    render_hash_in_sql(&HashInSql {
        scheme,
        quote,
        schema: side.schema.as_deref(),
        table: &side.table,
        columns: &side.plan.compare_columns,
        filter,
        row_hash_expr,
        hashes,
        backslash_escape: scheme == "mysql",
    })
}

pub(crate) fn chunk_hashes(hashes: &[String]) -> Vec<&[String]> {
    hashes.chunks(HASH_IN_CHUNK).collect()
}

pub(crate) fn keyless_fetch_cap(export: bool, export_rows: bool, sample: usize) -> usize {
    if export || export_rows {
        0
    } else {
        sample
    }
}

pub(crate) fn sides_to_refill(report: &DiffReport) -> (bool, bool) {
    let mut left = false;
    let mut right = false;
    for row in &report.sample_diffs {
        match row.status {
            DiffStatus::MissingRight => left = true,
            DiffStatus::MissingLeft => right = true,
            DiffStatus::Modified => {
                left = true;
                right = true;
            }
        }
    }
    (left, right)
}

pub(crate) fn wants_keyless_fetch(report: &DiffReport, args: &DeltaDiffArgs) -> bool {
    report.row_payload == RowPayload::HashCount && !args.no_fetch_sample
}

pub(crate) async fn post_diff_fetch(
    args: &crate::delta_diff::cmd::DeltaDiffArgs,
    report: &mut DiffReport,
    left: &mut (dyn crate::backend::DbConn + Send),
    right: &mut (dyn crate::backend::DbConn + Send),
    ctx: &crate::delta_diff::strategy::DiffContext,
) -> Result<bool, String> {
    let mut fetched = false;
    if wants_keyless_fetch(report, args) {
        let cap = keyless_fetch_cap(
            args.export.is_some(),
            args.export_rows_effective(),
            args.sample,
        );
        fetch_and_attach_keyless(report, left, right, ctx, cap).await?;
        fetched = true;
    } else if args.export_rows_effective() {
        refill_raw_rows(report, left, right, ctx).await?;
        fetched = true;
    }
    Ok(fetched)
}

async fn fetch_and_attach_keyless(
    report: &mut DiffReport,
    left: &mut (dyn crate::backend::DbConn + Send),
    right: &mut (dyn crate::backend::DbConn + Send),
    ctx: &crate::delta_diff::strategy::DiffContext,
    _sample: usize,
) -> Result<(), String> {
    let mut hashes: Vec<String> = report.sample_diffs.iter().filter_map(row_hash).collect();
    hashes.sort();
    hashes.dedup();
    if hashes.len() > 1000 {
        report.warnings.push(format!(
            "keyless row fetch of {} hashes (warning threshold 1000)",
            hashes.len()
        ));
    }
    if hashes.is_empty() {
        return Ok(());
    }
    let pairing = pair_plans(&ctx.left.plan, &ctx.right.plan);
    if !pairing.ambiguous.is_empty()
        || !pairing.unmatched_left.is_empty()
        || !pairing.unmatched_right.is_empty()
    {
        return Err(format!(
            "cannot align keyless row columns: unmatched left [{}], unmatched right [{}], \
             ambiguous [{}]",
            pairing.unmatched_left.join(", "),
            pairing.unmatched_right.join(", "),
            pairing.ambiguous.join(", ")
        ));
    }
    let display_columns = ctx.left.plan.compare_columns.clone();
    let mut map = std::collections::HashMap::new();
    pull_hash_rows(left, ctx, &pairing, true, &hashes, &mut map).await?;
    pull_hash_rows(right, ctx, &pairing, false, &hashes, &mut map).await?;
    attach_keyless_rows(report, &map, display_columns);
    Ok(())
}

async fn pull_hash_rows(
    conn: &mut (dyn crate::backend::DbConn + Send),
    ctx: &crate::delta_diff::strategy::DiffContext,
    pairing: &crate::delta_diff::pairing::Pairing,
    is_left: bool,
    hashes: &[String],
    out: &mut std::collections::HashMap<String, Vec<Value>>,
) -> Result<(), String> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let (hash_expr, quote, scheme, filter) = {
        let dialect = conn.dialect();
        let exprs = side
            .plan
            .normalized_exprs(dialect)
            .map_err(|e| e.to_string())?;
        let hash_expr = dialect.row_hash_expr(&exprs);
        let quote = dialect.identifier_quote();
        let scheme = dialect.url_scheme().to_string();
        let filter = crate::delta_diff::strategy::side_filter(ctx, &scheme);
        (hash_expr, quote, scheme, filter)
    };
    let projection = if is_left {
        side.plan.compare_columns.clone()
    } else {
        pairing
            .right_of_left
            .iter()
            .map(|right_index| {
                right_index
                    .and_then(|index| side.plan.compare_columns.get(index))
                    .cloned()
                    .ok_or_else(|| "cannot align keyless row columns".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    for chunk in chunk_hashes(hashes) {
        let mut projected_side = side.clone();
        projected_side.plan.compare_columns = projection.clone();
        let inner = render_side_hash_sql(
            &projected_side,
            &scheme,
            quote,
            filter.as_deref(),
            &hash_expr,
            chunk,
        );
        let sql = inner.replacen("SELECT ", &format!("SELECT {hash_expr} AS h, "), 1);
        if ctx.verbose {
            eprintln!("[sql] {sql}");
        }
        let result = conn.query(&sql).await.map_err(|e| e.to_string())?;
        for row in result.rows {
            let Some(Value::String(h)) = row.first().cloned() else {
                continue;
            };
            out.entry(h)
                .or_insert_with(|| row.into_iter().skip(1).collect());
        }
    }
    Ok(())
}

async fn refill_raw_rows(
    report: &mut DiffReport,
    left: &mut (dyn crate::backend::DbConn + Send),
    right: &mut (dyn crate::backend::DbConn + Send),
    ctx: &crate::delta_diff::strategy::DiffContext,
) -> Result<(), String> {
    let mut lmap = std::collections::HashMap::new();
    let mut rmap = std::collections::HashMap::new();
    let (need_left, need_right) = sides_to_refill(report);
    if need_left {
        pull_raw_side(left, ctx, true, &mut lmap).await?;
    }
    if need_right {
        pull_raw_side(right, ctx, false, &mut rmap).await?;
    }
    let key_len = report.key_columns.len();
    for row in &mut report.sample_diffs {
        let key = key_tuple(row, key_len);
        if let Some(cells) = lmap.get(&key) {
            row.left = Some(cells.clone());
        }
        if let Some(cells) = rmap.get(&key) {
            row.right = Some(cells.clone());
        }
    }
    Ok(())
}

fn key_tuple(row: &crate::delta_diff::report::DiffRow, key_len: usize) -> Vec<Value> {
    let src = row.left.as_ref().or(row.right.as_ref());
    if let Some(cells) = src {
        return cells.iter().take(key_len).cloned().collect();
    }
    match &row.key {
        Value::Array(a) => a.clone(),
        v => vec![v.clone()],
    }
}

async fn pull_raw_side(
    conn: &mut (dyn crate::backend::DbConn + Send),
    ctx: &crate::delta_diff::strategy::DiffContext,
    is_left: bool,
    out: &mut std::collections::HashMap<Vec<Value>, Vec<Value>>,
) -> Result<(), String> {
    let dialect = conn.dialect();
    let side = if is_left { &ctx.left } else { &ctx.right };
    let q = dialect.identifier_quote();
    let side_keys = ctx.side_key_columns(is_left);
    let mut columns: Vec<String> = side_keys
        .iter()
        .map(|c| crate::backend::quote_ident(q, c))
        .collect();
    for spec in side
        .plan
        .norm_specs
        .iter()
        .filter(|s| !side_keys.iter().any(|k| k == &s.name))
    {
        columns.push(crate::backend::quote_ident(q, &spec.name));
    }
    let spec = crate::backend::KeysetPageSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        columns,
        raw_exprs: true,
        key_columns: side_keys.to_vec(),
        string_key: side.plan.string_key_flags_for(side_keys),
        range: None,
        last_key: None,
        page_size: 4096,
        filter: crate::delta_diff::strategy::side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
    };
    let mut last_key = None;
    loop {
        let mut page = spec.clone();
        page.last_key = last_key.clone();
        let sql = conn.dialect().render_keyset_page_sql(&page);
        if ctx.verbose {
            eprintln!("[sql] {sql}");
        }
        let result = conn.query(&sql).await.map_err(|e| e.to_string())?;
        let n = result.rows.len();
        if let Some(last) = result.rows.last() {
            last_key = Some(last.iter().take(spec.key_columns.len()).cloned().collect());
        }
        for row in result.rows {
            let key: Vec<Value> = row.iter().take(spec.key_columns.len()).cloned().collect();
            out.insert(key, row);
        }
        if n < spec.page_size {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendFactory;
    use crate::delta_diff::report::*;
    use chrono::Utc;
    use clap::Parser;

    fn side(compare_columns: &[&str]) -> crate::delta_diff::strategy::SideCtx {
        crate::delta_diff::strategy::SideCtx {
            connection_name: "x".into(),
            schema: Some("s".into()),
            table: "t".into(),
            plan: crate::delta_diff::metadata::TablePlan {
                url_scheme: "mysql".into(),
                key_columns: vec![],
                compare_columns: compare_columns
                    .iter()
                    .map(|column| (*column).to_string())
                    .collect(),
                norm_specs: vec![],
                warnings: vec![],
            },
        }
    }

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: crate::delta_diff::cmd::DeltaDiffArgs,
    }

    fn args(extra: &[&str]) -> crate::delta_diff::cmd::DeltaDiffArgs {
        let mut argv = vec!["test", "--left", "l", "--right", "r", "--table", "t"];
        argv.extend_from_slice(extra);
        TestCli::try_parse_from(argv)
            .expect("test arguments should parse")
            .args
    }

    fn empty_report() -> DiffReport {
        DiffReport {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            left: TableRef {
                connection: "l".into(),
                schema: None,
                table: "t".into(),
            },
            right: TableRef {
                connection: "r".into(),
                schema: None,
                table: "t".into(),
            },
            strategy: "bucketdiff".into(),
            consistency: "none".into(),
            hash_algorithm: "md5".into(),
            summary: DiffSummary::default(),
            perf: PerfMetrics::default(),
            shards: vec![],
            sample_diffs: vec![],
            warnings: vec![],
            row_payload: RowPayload::HashCount,
            key_columns: vec![],
            value_columns: vec![],
            ident_quote: '"',
            ident_scheme: String::new(),
            backslash_escape: false,
        }
    }

    #[test]
    fn keyless_attach_matches_hash() {
        let mut report = empty_report();
        report.sample_diffs.push(DiffRow {
            key: Value::from("abc…(×0/1)"),
            left: Some(vec![Value::from("abc123"), Value::from(0)]),
            right: Some(vec![Value::from("abc123"), Value::from(1)]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        });
        let mut fetched = HashMap::new();
        fetched.insert(
            "abc123".into(),
            vec![Value::from("59267"), Value::from(100)],
        );
        attach_keyless_rows(&mut report, &fetched, vec!["xwdm".into(), "cjsl".into()]);
        assert_eq!(report.sample_diffs[0].key["hash"], "abc123");
        assert_eq!(report.sample_diffs[0].key["left"], 0);
        assert_eq!(report.sample_diffs[0].key["right"], 1);
        assert_eq!(report.value_columns, vec!["xwdm", "cjsl"]);
        assert_eq!(report.row_payload, RowPayload::Columns);
        assert_eq!(
            report.sample_diffs[0].right,
            Some(vec![Value::from("59267"), Value::from(100)])
        );
    }

    #[test]
    fn wants_keyless_fetch_uses_payload_tag_not_column_names() {
        let mut report = empty_report();
        report.row_payload = RowPayload::HashCount;
        report.key_columns = vec!["wrongly_stamped".into()];
        assert!(wants_keyless_fetch(&report, &args(&[])));
        assert!(!wants_keyless_fetch(&report, &args(&["--no-fetch-sample"])));
        report.row_payload = RowPayload::Columns;
        assert!(!wants_keyless_fetch(&report, &args(&[])));
    }

    #[test]
    fn keyless_attach_skips_when_fetch_empty() {
        let mut report = empty_report();
        report.sample_diffs.push(DiffRow {
            key: Value::from("h"),
            left: Some(vec![Value::from("abc123"), Value::from(0)]),
            right: Some(vec![Value::from("abc123"), Value::from(1)]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        });
        attach_keyless_rows(&mut report, &HashMap::new(), vec!["xwdm".into()]);
        assert!(report.value_columns.is_empty());
        assert_eq!(report.row_payload, RowPayload::HashCount);
        assert_eq!(
            report.sample_diffs[0].right,
            Some(vec![Value::from("abc123"), Value::from(1)])
        );
    }

    #[test]
    fn keyless_attach_is_all_or_nothing() {
        let mut report = empty_report();
        for hash in ["found", "missing"] {
            report.sample_diffs.push(DiffRow {
                key: Value::from(hash),
                left: Some(vec![Value::from(hash), Value::from(1)]),
                right: None,
                status: DiffStatus::MissingRight,
                confirmed: true,
            });
        }
        let fetched = HashMap::from([("found".into(), vec![Value::from("payload")])]);

        attach_keyless_rows(&mut report, &fetched, vec!["value".into()]);

        assert_eq!(report.row_payload, RowPayload::HashCount);
        assert!(report.value_columns.is_empty());
        assert!(report
            .sample_diffs
            .iter()
            .all(|row| row.left.as_ref().is_none_or(|cells| cells.len() == 2)));
    }

    #[test]
    fn hash_in_sql_includes_filter_and_in_list() {
        let cols = ["a".into(), "b".into()];
        let hashes = ["aa".into(), "bb".into()];
        let sql = render_hash_in_sql(&HashInSql {
            scheme: "gaussdb",
            quote: '"',
            schema: Some("s"),
            table: "t",
            columns: &cols,
            filter: Some("bcrq='20260114'"),
            row_hash_expr: "MD5(concat_ws('#', a, b))",
            hashes: &hashes,
            backslash_escape: false,
        });
        assert!(sql.contains("IN ("), "{sql}");
        assert!(sql.contains("(bcrq='20260114')"), "{sql}");
        assert!(sql.contains("\"s\".\"t\""), "{sql}");
        assert!(sql.contains("\"a\", \"b\""), "{sql}");
    }

    #[test]
    fn right_side_projection_uses_right_side_column_names() {
        let hashes = vec!["abc".into()];
        let sql = render_side_hash_sql(
            &side(&["k_xwdm", "security_id"]),
            "gaussdb",
            '"',
            None,
            "MD5(row_expr)",
            &hashes,
        );
        assert!(sql.contains("\"k_xwdm\", \"security_id\""), "{sql}");
        assert!(!sql.contains("\"K_XWDM\""), "{sql}");
    }

    #[test]
    fn keyless_fetch_cap_is_unlimited_when_exporting() {
        assert_eq!(keyless_fetch_cap(false, false, 20), 20);
        assert_eq!(keyless_fetch_cap(true, false, 20), 0);
        assert_eq!(keyless_fetch_cap(false, true, 20), 0);
    }

    #[test]
    fn sides_to_refill_skips_empty_side() {
        let mut r = empty_report();
        r.sample_diffs.push(DiffRow {
            key: Value::from(1),
            left: None,
            right: Some(vec![Value::from(1)]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        });
        assert_eq!(sides_to_refill(&r), (false, true));
    }

    #[test]
    fn hash_chunks_split_at_500() {
        let hashes: Vec<String> = (0..501).map(|i| i.to_string()).collect();
        let chunks = chunk_hashes(&hashes);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 500);
        assert_eq!(chunks[1].len(), 1);
    }

    fn spec(exprs: Vec<String>) -> crate::backend::ChecksumSqlSpec {
        crate::backend::ChecksumSqlSpec {
            schema: Some("s".into()),
            table: "t".into(),
            key_column: None,
            range: None,
            bucket: Some((4, 1)),
            filter: None,
            scn: None,
            normalized_exprs: exprs,
            key_hash_exprs: vec![],
        }
    }

    #[test]
    fn row_hash_expr_is_substring_of_mysql_bucket_sql() {
        let d = crate::backend::mysql::MySqlFactory.create_dialect();
        let exprs = vec!["a".into(), "b".into()];
        let hash = d.row_hash_expr(&exprs);
        let sql = d.render_bucket_multiset_sql(&spec(exprs));
        assert!(sql.contains(&hash), "sql={sql} hash={hash}");
    }

    #[test]
    fn row_hash_expr_is_substring_of_gaussdb_bucket_sql() {
        let d = crate::backend::gaussdb::GaussdbFactory.create_dialect();
        let exprs = vec!["a".into(), "b".into()];
        let hash = d.row_hash_expr(&exprs);
        let sql = d.render_bucket_multiset_sql(&spec(exprs));
        assert!(sql.contains(&hash), "sql={sql} hash={hash}");
    }
}
