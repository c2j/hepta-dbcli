// ─── delta-diff MinusDiffer: same-session set difference ─────────────
//
// SQL shape, rendering, and row classification live in sql.rs. This
// module runs that plan on the left connection: capability probe, one
// shared snapshot, counts, checksum, then one set-difference statement.

use std::time::Instant;

use chrono::Utc;
use serde_json::Value;

use crate::backend::{ChecksumSqlSpec, DbConn, DbError, Dialect, SetDifferenceOp};
use crate::delta_diff::checksum::{run_checksum, tuples_equal};
use crate::delta_diff::hash_diff::{capture_scn, open_snapshot};
use crate::delta_diff::keyed_diff::{parse_count, KeyedDiffer};
use crate::delta_diff::pairing::pair_plans;
use crate::delta_diff::report::{
    stamp_columns_from_plan, DiffReport, DiffRow, DiffSummary, PerfMetrics, ShardResult,
    ShardStatus, TableRef,
};
use crate::delta_diff::strategy::{side_filter, ConsistencyMode, DiffContext, DiffStrategy};

mod sql;

#[cfg(test)]
use sql::render_minusdiff_sql;
pub(crate) use sql::{
    classify_minus_rows, mysql_set_op_decision, prepare_minusdiff, tag_empty_side_rows, MinusShape,
    MysqlSetOpDecision,
};
use sql::{from_clause, render_prepared, summary_counts, tally, value_exprs, MinusCounts};

pub(crate) struct MinusDiffer;

#[async_trait::async_trait]
impl DiffStrategy for MinusDiffer {
    fn name(&self) -> &'static str {
        "minusdiff"
    }

    async fn diff(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
    ) -> Result<DiffReport, DbError> {
        let started = Utc::now();
        // Shape errors must not probe, open a snapshot, or touch a user table.
        // The operator is not known yet; `None` must not hide a bad plan.
        let shape = prepare_minusdiff(ctx, left.dialect())?;

        let mut sent = 0u64;
        let mut opened = false;
        let result = self
            .execute(left, right, ctx, &shape, &mut sent, &mut opened)
            .await;
        if opened {
            ctx.vlog("[sql] COMMIT");
            let _ = left.query_drop("COMMIT").await;
        }
        let mut report = result?;
        if report.strategy == "minusdiff" {
            report.perf.queries_total = sent;
        }
        report.started_at = started;
        report.finished_at = Utc::now();
        Ok(report)
    }
}

impl MinusDiffer {
    async fn execute(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        shape: &MinusShape,
        sent: &mut u64,
        opened: &mut bool,
    ) -> Result<DiffReport, DbError> {
        let t0 = Instant::now();
        let op = resolve_set_op(left, ctx, sent).await?;
        if op == SetDifferenceOp::None {
            return fallback_keyed(
                left,
                right,
                ctx,
                sent,
                opened,
                "minusdiff: no MINUS/EXCEPT on this server; falling back to keyeddiff",
            )
            .await;
        }
        let keyword = match op {
            SetDifferenceOp::Minus => "MINUS",
            SetDifferenceOp::Except => "EXCEPT",
            SetDifferenceOp::None => "NONE",
        };
        ctx.vlog(format!(
            "[delta-diff] strategy=minusdiff consistency={} op={keyword} keys={}",
            ctx.consistency.as_str(),
            ctx.key_columns.join(",")
        ));

        let scn = open_shared_anchor(left, ctx, sent, opened).await?;
        let (mut left_total, mut right_total) = count_both(left, ctx, scn, sent).await?;
        let width = ctx.left.plan.compare_columns.len() as u64;
        let rows = left_total.max(right_total);
        let cells = rows.saturating_mul(width);
        if ctx.minus_max_cells != 0 && cells > ctx.minus_max_cells {
            let warning = format!(
                "minusdiff cell budget exceeded: {cells} > {} (rows {rows} × width {width}); falling back to keyeddiff. Pass --minus-max-cells 0 to force the set-difference.",
                ctx.minus_max_cells
            );
            return fallback_keyed(left, right, ctx, sent, opened, &warning).await;
        }

        if left_total == 0 && right_total == 0 {
            return Ok(assemble(
                ctx,
                Vec::new(),
                0,
                0,
                MinusCounts::ZERO,
                t0,
                flag_warnings(ctx),
            ));
        }
        if left_total == 0 || right_total == 0 {
            return empty_side(left, ctx, scn, left_total, right_total, sent, t0).await;
        }

        let (left_tuple, right_tuple) = checksum_both(left, ctx, scn, sent).await?;
        let mut extra = flag_warnings(ctx);
        if left_tuple.count != left_total || right_tuple.count != right_total {
            extra.push(format!(
                "minusdiff: checksum count differs from COUNT(*): left checksum={} count={left_total}, right checksum={} count={right_total}; summary totals use the checksum count",
                left_tuple.count, right_tuple.count
            ));
            left_total = left_tuple.count;
            right_total = right_tuple.count;
        }
        if tuples_equal(&left_tuple, &right_tuple) {
            return Ok(assemble(
                ctx,
                Vec::new(),
                left_total,
                right_total,
                MinusCounts::ZERO,
                t0,
                extra,
            ));
        }

        let sql = render_prepared(ctx, left.dialect(), shape, op, ctx.summary_only, scn)?;
        ctx.vlog(format!("[sql] {sql}"));
        *sent += 1;
        let result = left.query(&sql).await.map_err(explain_set_diff_error)?;
        if ctx.summary_only {
            let counts = summary_counts(&result)?;
            return Ok(assemble(
                ctx,
                Vec::new(),
                left_total,
                right_total,
                counts,
                t0,
                extra,
            ));
        }
        let diffs = classify_minus_rows(shape.key_len, shape.left_values.len(), &result.rows)?;
        let counts = tally(&diffs);
        Ok(assemble(
            ctx,
            diffs,
            left_total,
            right_total,
            counts,
            t0,
            extra,
        ))
    }
}

async fn resolve_set_op(
    left: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    sent: &mut u64,
) -> Result<SetDifferenceOp, DbError> {
    let scheme = left.dialect().url_scheme().to_string();
    if scheme != "mysql" {
        return Ok(left.dialect().set_difference_op());
    }
    *sent += 1;
    ctx.vlog("[sql] SELECT VERSION()");
    let version = match left.query("SELECT VERSION()").await {
        Ok(result) => result
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Err(_) => String::new(),
    };
    match mysql_set_op_decision(&version) {
        MysqlSetOpDecision::Refuse => Ok(SetDifferenceOp::None),
        MysqlSetOpDecision::Except => Ok(SetDifferenceOp::Except),
        MysqlSetOpDecision::Probe => {
            let probe = "SELECT 1 AS c EXCEPT SELECT 1 AS c";
            *sent += 1;
            ctx.vlog(format!("[sql] {probe}"));
            match left.query(probe).await {
                Ok(_) => Ok(SetDifferenceOp::Except),
                Err(_) => Ok(SetDifferenceOp::None),
            }
        }
    }
}

async fn open_shared_anchor(
    left: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    sent: &mut u64,
    opened: &mut bool,
) -> Result<Option<u64>, DbError> {
    if ctx.consistency != ConsistencyMode::Snapshot {
        return Ok(None);
    }
    let opened_statements = open_snapshot(left, ctx.verbose).await?;
    *opened = true;
    *sent += opened_statements;
    let captured = capture_scn(left, ctx.verbose).await?;
    *sent += captured.statements;
    let _ = ctx.scns.set((captured.scn, captured.scn));
    Ok(captured.scn)
}

async fn count_both(
    left: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    scn: Option<u64>,
    sent: &mut u64,
) -> Result<(u64, u64), DbError> {
    let lsql = render_anchored_count(ctx, left.dialect(), true, scn);
    let rsql = render_anchored_count(ctx, left.dialect(), false, scn);
    ctx.vlog(format!("[sql] {lsql}"));
    *sent += 1;
    let left_total = parse_count(&left.query(&lsql).await.map_err(explain_set_diff_error)?)?;
    ctx.vlog(format!("[sql] {rsql}"));
    *sent += 1;
    let right_total = parse_count(&left.query(&rsql).await.map_err(explain_set_diff_error)?)?;
    Ok((left_total, right_total))
}

async fn checksum_both(
    left: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    scn: Option<u64>,
    sent: &mut u64,
) -> Result<
    (
        crate::delta_diff::checksum::ChecksumTuple,
        crate::delta_diff::checksum::ChecksumTuple,
    ),
    DbError,
> {
    let lspec = checksum_spec(ctx, left.dialect(), true, scn)?;
    let rspec = checksum_spec(ctx, left.dialect(), false, scn)?;
    *sent += 1;
    let left_tuple = run_checksum(left, &lspec, ctx.verbose)
        .await
        .map_err(explain_set_diff_error)?;
    *sent += 1;
    let right_tuple = run_checksum(left, &rspec, ctx.verbose)
        .await
        .map_err(explain_set_diff_error)?;
    Ok((left_tuple, right_tuple))
}

async fn empty_side(
    left: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    scn: Option<u64>,
    left_total: u64,
    right_total: u64,
    sent: &mut u64,
    t0: Instant,
) -> Result<DiffReport, DbError> {
    let missing_right = if right_total == 0 { left_total } else { 0 };
    let missing_left = if left_total == 0 { right_total } else { 0 };
    if ctx.summary_only {
        return Ok(assemble(
            ctx,
            Vec::new(),
            left_total,
            right_total,
            MinusCounts {
                missing_left,
                missing_right,
                modified: 0,
            },
            t0,
            flag_warnings(ctx),
        ));
    }
    let read_left = right_total == 0;
    let sql = render_empty_side_sql(ctx, left.dialect(), read_left, scn)?;
    ctx.vlog(format!("[sql] {sql}"));
    *sent += 1;
    let result = left.query(&sql).await.map_err(explain_set_diff_error)?;
    let side = if read_left { "L" } else { "R" };
    let tagged = tag_empty_side_rows(side, &result.rows);
    let value_len = value_width(ctx, left.dialect())?;
    let diffs = classify_minus_rows(ctx.key_columns.len(), value_len, &tagged)?;
    let counts = tally(&diffs);
    Ok(assemble(
        ctx,
        diffs,
        left_total,
        right_total,
        counts,
        t0,
        flag_warnings(ctx),
    ))
}

async fn fallback_keyed(
    left: &mut (dyn DbConn + Send),
    right: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    sent: &mut u64,
    opened: &mut bool,
    warning: &str,
) -> Result<DiffReport, DbError> {
    if *opened {
        ctx.vlog("[sql] COMMIT");
        let _ = left.query_drop("COMMIT").await;
        *opened = false;
    }
    let already = *sent;
    let mut report = KeyedDiffer.diff(left, right, ctx).await?;
    report.perf.queries_total = report.perf.queries_total.saturating_add(already);
    report.warnings.push(warning.to_string());
    Ok(report)
}

fn checksum_spec(
    ctx: &DiffContext,
    dialect: &dyn Dialect,
    is_left: bool,
    scn: Option<u64>,
) -> Result<ChecksumSqlSpec, DbError> {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let scheme = dialect.url_scheme();
    let scn = (scheme == "oracle").then_some(scn).flatten();
    Ok(ChecksumSqlSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        key_column: None,
        range: None,
        bucket: None,
        filter: side_filter(ctx, scheme),
        scn,
        normalized_exprs: side.plan.identity_hash_exprs(dialect)?,
        key_hash_exprs: side.plan.key_hash_exprs(dialect)?,
    })
}

fn render_anchored_count(
    ctx: &DiffContext,
    dialect: &dyn Dialect,
    is_left: bool,
    scn: Option<u64>,
) -> String {
    let side = if is_left { &ctx.left } else { &ctx.right };
    let oracle_scn = (dialect.url_scheme() == "oracle").then_some(scn).flatten();
    let from = from_clause(
        &dialect.quote_table(side.schema.as_deref(), &side.table),
        oracle_scn,
    );
    let where_sql = side_filter(ctx, dialect.url_scheme())
        .map(|pred| format!(" WHERE ({pred})"))
        .unwrap_or_default();
    format!("SELECT COUNT(*) AS cnt FROM {from}{where_sql}")
}

fn render_empty_side_sql(
    ctx: &DiffContext,
    dialect: &dyn Dialect,
    is_left: bool,
    scn: Option<u64>,
) -> Result<String, DbError> {
    let pairing = pair_plans(&ctx.left.plan, &ctx.right.plan);
    let values = value_exprs(ctx, dialect, &pairing.right_of_left, is_left)?;
    let plan = if is_left {
        &ctx.left.plan
    } else {
        &ctx.right.plan
    };
    let keys = ctx.side_key_columns(is_left);
    let mut items = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        items.push(format!("{} AS rk{i}", dialect.quote_catalog_ident(key)));
    }
    for (j, expr) in values.iter().enumerate() {
        items.push(format!("{expr} AS c{j}"));
    }
    for (i, key) in keys.iter().enumerate() {
        let spec = plan.spec_for(key).ok_or_else(|| {
            DbError::config(format!(
                "minusdiff: key column '{key}' has no normalization spec"
            ))
        })?;
        items.push(format!("{} AS k{i}", dialect.normalize_expr(spec)?));
    }
    let side = if is_left { &ctx.left } else { &ctx.right };
    let oracle_scn = (dialect.url_scheme() == "oracle").then_some(scn).flatten();
    let from = from_clause(
        &dialect.quote_table(side.schema.as_deref(), &side.table),
        oracle_scn,
    );
    let where_sql = side_filter(ctx, dialect.url_scheme())
        .map(|pred| format!(" WHERE ({pred})"))
        .unwrap_or_default();
    Ok(format!(
        "SELECT {} FROM {from}{where_sql}",
        items.join(", ")
    ))
}

fn value_width(ctx: &DiffContext, dialect: &dyn Dialect) -> Result<usize, DbError> {
    let pairing = pair_plans(&ctx.left.plan, &ctx.right.plan);
    Ok(value_exprs(ctx, dialect, &pairing.right_of_left, true)?.len())
}

fn flag_warnings(ctx: &DiffContext) -> Vec<String> {
    let mut warnings = Vec::new();
    if ctx.recheck {
        warnings.push(
            "minusdiff ignores --recheck: classification comes from one set-difference statement"
                .to_string(),
        );
    }
    if ctx.checkpoint.is_some() {
        warnings.push(
            "minusdiff does not record shard checkpoints; a cancelled statement restarts from scratch"
                .to_string(),
        );
    }
    warnings
}

fn explain_set_diff_error(err: DbError) -> DbError {
    let text = err.to_string();
    let lower = text.to_ascii_lowercase();
    let prefix = if lower.contains("ora-01790")
        || lower.contains("ora-00932")
        || lower.contains("union types")
        || lower.contains("incompatible types")
    {
        "minusdiff: set operands are not union-compatible"
    } else if lower.contains("ora-00942")
        || lower.contains("ora-01031")
        || lower.contains("1142")
        || lower.contains("1044")
        || lower.contains("permission denied")
        || lower.contains("does not exist")
    {
        "minusdiff: left session must be able to SELECT both tables"
    } else if lower.contains("ora-01652")
        || lower.contains("ora-01653")
        || lower.contains("temp_file_limit")
        || lower.contains("is full")
        || lower.contains("max_execution_time")
        || lower.contains("canceling statement due to statement timeout")
    {
        "minusdiff: set-difference failed (temp spill, timeout, or engine error); not retrying as keyeddiff"
    } else {
        "minusdiff: set-difference failed"
    };
    DbError::query(format!("{prefix}: {text}"))
}

fn assemble(
    ctx: &DiffContext,
    diffs: Vec<DiffRow>,
    left_total: u64,
    right_total: u64,
    counts: MinusCounts,
    t0: Instant,
    extra_warnings: Vec<String>,
) -> DiffReport {
    let total = left_total.max(right_total);
    let diff_count = counts.missing_left + counts.missing_right + counts.modified;
    let summary = DiffSummary {
        left_total,
        right_total,
        missing_left: counts.missing_left,
        missing_right: counts.missing_right,
        modified: counts.modified,
        diff_rate: if total > 0 {
            diff_count as f64 / total as f64
        } else {
            0.0
        },
    };
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
        strategy: "minusdiff".into(),
        consistency: ctx.consistency.as_str().into(),
        hash_algorithm: "md5".into(),
        summary,
        perf: PerfMetrics::default(),
        shards: vec![ShardResult {
            shard_id: "minus".into(),
            key_range: (Value::Null, Value::Null),
            left_count: left_total,
            right_count: right_total,
            diff_count,
            status: if diff_count == 0 {
                ShardStatus::Match
            } else {
                ShardStatus::Diff
            },
            duration_ms: t0.elapsed().as_millis() as u64,
        }],
        sample_diffs: diffs,
        warnings: ctx
            .left
            .plan
            .warnings
            .iter()
            .chain(ctx.right.plan.warnings.iter())
            .chain(ctx.route_warnings.iter())
            .cloned()
            .chain(extra_warnings)
            .collect(),
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
    stamp_columns_from_plan(&mut report, &ctx.left.plan);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ColumnNormSpec, DbConn, DbError, DbPool, QueryResult, SetDifferenceOp};
    use crate::delta_diff::metadata::TablePlan;
    use crate::delta_diff::report::DiffStatus;
    use crate::delta_diff::strategy::{ConsistencyMode, DiffContext, SideCtx};
    use serde_json::json;

    fn spec(name: &str, data_type: &str) -> ColumnNormSpec {
        ColumnNormSpec {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: false,
            rtrim_fixed_char: false,
        }
    }

    fn plan(
        scheme: &str,
        keys: &[&str],
        cols: &[(&str, &str)],
        warnings: Vec<String>,
    ) -> TablePlan {
        let key_columns: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
        let compare_columns: Vec<String> =
            cols.iter().map(|(name, _)| (*name).to_string()).collect();
        let norm_specs: Vec<ColumnNormSpec> =
            cols.iter().map(|(name, ty)| spec(name, ty)).collect();
        let key_specs = keys
            .iter()
            .map(|key| {
                cols.iter()
                    .find(|(name, _)| *name == *key)
                    .map(|(name, ty)| spec(name, ty))
                    .unwrap_or_else(|| spec(key, "int"))
            })
            .collect();
        TablePlan {
            url_scheme: scheme.to_string(),
            key_columns,
            compare_columns,
            norm_specs,
            key_specs,
            warnings,
            key_unique: true,
            aux: Default::default(),
        }
    }

    fn ctx_with(
        left: TablePlan,
        right: TablePlan,
        keys: &[&str],
        filter: Option<&str>,
        incremental: Option<(&str, &str)>,
        sample_limit: usize,
    ) -> DiffContext {
        let key_columns: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
        DiffContext {
            left: side("left_t", left),
            right: side("right_t", right),
            left_pool: dummy_pool(),
            right_pool: dummy_pool(),
            key_column: key_columns.first().cloned().unwrap_or_default(),
            key_columns: key_columns.clone(),
            left_key_columns: key_columns.clone(),
            right_key_columns: key_columns,
            filter: filter.map(str::to_string),
            incremental: incremental.map(|(col, since)| (col.to_string(), since.to_string())),
            bisection_factor: 32,
            bisection_threshold: 16384,
            sample_limit,
            threads: 4,
            consistency: ConsistencyMode::None,
            recheck: false,
            route_warnings: vec![],
            checkpoint: None,
            iblt_capacity: 65536,
            fetch_all_threshold: 4096,
            naive_max_rows: 4096,
            strict: false,
            summary_only: false,
            same_connection: true,
            scns: std::sync::OnceLock::new(),
            verbose: false,
            minus_max_cells: crate::delta_diff::cmd::DEFAULT_MINUS_MAX_CELLS,
        }
    }

    fn side(table: &str, plan: TablePlan) -> SideCtx {
        SideCtx {
            connection_name: "c".into(),
            schema: Some("app".into()),
            table: table.into(),
            plan,
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

    fn paired(scheme: &str) -> (TablePlan, TablePlan) {
        // Each dialect only normalizes its own type names. Oracle rejects
        // `INT`; DuckDB rejects `int` and wants `integer`.
        let cols: &[(&str, &str)] = match scheme {
            "oracle" => &[("id", "NUMBER"), ("note", "VARCHAR2")],
            "duckdb" => &[("id", "integer"), ("note", "varchar")],
            _ => &[("id", "int"), ("note", "varchar")],
        };
        (
            plan(scheme, &["id"], cols, vec![]),
            plan(scheme, &["id"], cols, vec![]),
        )
    }

    fn render(
        dialect: &dyn Dialect,
        op: SetDifferenceOp,
        ctx: &DiffContext,
        summary_only: bool,
        scn: Option<u64>,
    ) -> String {
        render_minusdiff_sql(ctx, dialect, op, summary_only, scn).unwrap()
    }

    #[test]
    fn polardbx_version_refuses_without_probe() {
        let _ = MinusDiffer;
        assert_eq!(
            mysql_set_op_decision("5.6.29-PXC-5.4.19-SNAPSHOT"),
            MysqlSetOpDecision::Refuse
        );
        assert_eq!(
            mysql_set_op_decision("8.0.30-pxc-cluster"),
            MysqlSetOpDecision::Refuse
        );
    }

    #[test]
    fn mysql_8_0_31_selects_except() {
        assert_eq!(mysql_set_op_decision("8.0.31"), MysqlSetOpDecision::Except);
        assert_eq!(
            mysql_set_op_decision("8.0.36-0ubuntu0.22.04.1"),
            MysqlSetOpDecision::Except
        );
    }

    #[test]
    fn mysql_8_0_30_refuses() {
        assert_eq!(mysql_set_op_decision("8.0.30"), MysqlSetOpDecision::Refuse);
        assert_eq!(
            mysql_set_op_decision("5.7.44-log"),
            MysqlSetOpDecision::Refuse
        );
    }

    #[test]
    fn mariadb_and_polardb_strings_ask_for_probe() {
        assert_eq!(
            mysql_set_op_decision("10.11.6-MariaDB"),
            MysqlSetOpDecision::Probe
        );
        assert_eq!(
            mysql_set_op_decision("8.0.32-PolarDB"),
            MysqlSetOpDecision::Probe
        );
        assert_eq!(
            mysql_set_op_decision("not-a-version"),
            MysqlSetOpDecision::Probe
        );
        assert_eq!(mysql_set_op_decision("8.0"), MysqlSetOpDecision::Probe);
    }

    #[test]
    fn oracle_sql_emits_minus_not_except() {
        let dialect = crate::backend::oracle::dialect::OracleDialect::new();
        let (left, right) = paired("oracle");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Minus, &ctx, false, None);
        assert!(sql.contains("MINUS"), "{sql}");
        assert!(!sql.contains("EXCEPT"), "{sql}");
        assert!(sql.contains("SELECT k0, h FROM l"), "{sql}");
        assert!(sql.contains("SELECT k0, h FROM r"), "{sql}");
    }

    #[test]
    fn oracle_sql_order_by_is_outer_only_and_from_alias_has_no_as() {
        let dialect = crate::backend::oracle::dialect::OracleDialect::new();
        let (left, right) = paired("oracle");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Minus, &ctx, false, None);
        let order_at = sql.rfind("ORDER BY").expect("outer ORDER BY");
        assert!(
            !sql[..order_at].contains("ORDER BY"),
            "ORDER BY inside a set operand: {sql}"
        );
        // Table aliases must not use AS (`ORA-00933`). Column aliases such as
        // `STANDARD_HASH(...) AS h` and `WITH l AS (` stay legal.
        assert!(!sql.contains(") AS d"), "{sql}");
        assert!(!sql.contains(") AS l"), "{sql}");
        assert!(!sql.contains(") AS r"), "{sql}");
        assert!(sql.contains(") d\nORDER BY k0"), "{sql}");
        assert!(sql.trim_end().ends_with("ORDER BY k0"), "{sql}");
    }

    #[test]
    fn mysql_sql_emits_except_not_minus() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Except, &ctx, false, None);
        assert!(sql.contains("EXCEPT"), "{sql}");
        assert!(!sql.contains("MINUS"), "{sql}");
        assert!(sql.contains("`app`.`left_t`"), "{sql}");
        assert!(sql.contains("`app`.`right_t`"), "{sql}");
    }

    #[test]
    fn gaussdb_sql_emits_except() {
        let dialect = crate::backend::gaussdb::GaussdbDialect;
        let (left, right) = paired("gaussdb");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Except, &ctx, false, None);
        assert!(sql.contains("EXCEPT"), "{sql}");
        assert!(!sql.contains("MINUS"), "{sql}");
        assert!(sql.contains("\"app\".\"left_t\""), "{sql}");
    }

    #[cfg(feature = "duckdb")]
    #[test]
    fn duckdb_sql_emits_except() {
        let dialect = crate::backend::duckdb::dialect::DuckDbDialect;
        let (left, right) = paired("duckdb");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Except, &ctx, false, None);
        assert!(sql.contains("EXCEPT"), "{sql}");
        assert!(!sql.contains("MINUS"), "{sql}");
        assert!(sql.contains("\"app\".\"left_t\""), "{sql}");
    }

    #[test]
    fn where_pushdown_is_parenthesized_on_both_ctes() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(
            left,
            right,
            &["id"],
            Some("status = 1 OR status = 2"),
            None,
            1000,
        );
        let sql = render(&dialect, SetDifferenceOp::Except, &ctx, false, None);
        assert_eq!(
            sql.matches("WHERE (status = 1 OR status = 2)").count(),
            2,
            "{sql}"
        );
    }

    #[test]
    fn incremental_day_predicate_uses_side_filter() {
        let oracle = crate::backend::oracle::dialect::OracleDialect::new();
        let (left, right) = paired("oracle");
        let ctx = ctx_with(
            left,
            right,
            &["id"],
            None,
            Some(("updated_at", "3 days")),
            1000,
        );
        let sql = render(&oracle, SetDifferenceOp::Minus, &ctx, false, None);
        assert!(sql.contains("updated_at >= SYSDATE - 3"), "{sql}");
        assert!(!sql.contains("DATE_SUB"), "{sql}");

        let mysql = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(
            left,
            right,
            &["id"],
            None,
            Some(("updated_at", "3 days")),
            1000,
        );
        let sql = render(&mysql, SetDifferenceOp::Except, &ctx, false, None);
        assert!(
            sql.contains("updated_at >= DATE_SUB(NOW(), INTERVAL 3 DAY)"),
            "{sql}"
        );
    }

    #[test]
    fn lob_column_absent_from_projection() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let warning = "column 'app.left_t.secret_blob' excluded from comparison: LOB".to_string();
        let left = plan(
            "mysql",
            &["id"],
            &[("id", "int"), ("note", "varchar")],
            vec![warning],
        );
        let right = plan(
            "mysql",
            &["id"],
            &[("id", "int"), ("note", "varchar")],
            vec![],
        );
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Except, &ctx, false, None);
        assert!(!sql.contains("secret_blob"), "{sql}");
        assert!(ctx
            .left
            .plan
            .warnings
            .iter()
            .any(|w| w.contains("secret_blob")));
    }

    #[test]
    fn set_operand_is_normalized_keys_and_row_hash_only() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Except, &ctx, false, None);
        assert!(sql.contains("SELECT k0, h FROM l"), "{sql}");
        assert!(sql.contains("SELECT k0, h FROM r"), "{sql}");
        assert!(!sql.contains("SELECT k0, h, rk"), "{sql}");
        assert!(!sql.contains("c0 FROM l"), "{sql}");
    }

    #[test]
    fn minusdiff_sql_has_no_limit_fetch_or_rownum() {
        let dialect = crate::backend::oracle::dialect::OracleDialect::new();
        let (left, right) = paired("oracle");
        let ctx = ctx_with(left, right, &["id"], None, None, 7);
        let sql = render(&dialect, SetDifferenceOp::Minus, &ctx, false, None);
        let upper = sql.to_ascii_uppercase();
        assert!(!upper.contains("LIMIT"), "{sql}");
        assert!(!upper.contains("FETCH FIRST"), "{sql}");
        assert!(!upper.contains("ROWNUM"), "{sql}");
    }

    #[test]
    fn summary_only_sql_projects_three_counts_and_no_body() {
        let oracle = crate::backend::oracle::dialect::OracleDialect::new();
        let (left, right) = paired("oracle");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&oracle, SetDifferenceOp::Minus, &ctx, true, None);
        assert!(sql.contains("AS missing_right"), "{sql}");
        assert!(sql.contains("AS missing_left"), "{sql}");
        assert!(sql.contains("AS modified"), "{sql}");
        assert!(sql.contains("FROM dual"), "{sql}");
        assert!(!sql.contains("rk0"), "{sql}");
        assert!(!sql.contains("c0"), "{sql}");

        let mysql = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&mysql, SetDifferenceOp::Except, &ctx, true, None);
        assert!(!sql.to_ascii_lowercase().contains("dual"), "{sql}");
        assert!(!sql.contains("rk0"), "{sql}");
    }

    #[test]
    fn summary_only_materializes_set_difference_before_reread() {
        // MySQL 8.4 consumes an EXCEPT CTE incorrectly when one statement
        // reads it more than once (wrong cardinality, error 124, or an
        // InnoDB abort). The three summary counts do that, so each set
        // difference is copied through a plain derived table first.
        let mysql = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&mysql, SetDifferenceOp::Except, &ctx, true, None);
        assert!(
            sql.contains(
                "ld AS (\n  SELECT * FROM (\n  SELECT k0, h FROM l\n  EXCEPT\n  SELECT k0, h FROM r\n) setdiff\n)"
            ),
            "{sql}"
        );
        assert!(
            sql.contains(
                "rd AS (\n  SELECT * FROM (\n  SELECT k0, h FROM r\n  EXCEPT\n  SELECT k0, h FROM l\n) setdiff\n)"
            ),
            "{sql}"
        );
        assert!(!sql.contains(") AS setdiff"), "{sql}");

        let body = render(&mysql, SetDifferenceOp::Except, &ctx, false, None);
        assert!(!body.contains("SELECT * FROM ("), "{body}");

        let oracle = crate::backend::oracle::dialect::OracleDialect::new();
        let (left, right) = paired("oracle");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&oracle, SetDifferenceOp::Minus, &ctx, true, None);
        assert!(
            sql.contains(
                "ld AS (\n  SELECT * FROM (\n  SELECT k0, h FROM l\n  MINUS\n  SELECT k0, h FROM r\n) setdiff\n)"
            ),
            "{sql}"
        );
    }

    #[test]
    fn null_safe_key_predicate_is_present() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Except, &ctx, false, None);
        assert!(
            sql.contains("(ld.k0 = l.k0 OR (ld.k0 IS NULL AND l.k0 IS NULL))"),
            "{sql}"
        );
        assert!(
            sql.contains("(rd.k0 = r.k0 OR (rd.k0 IS NULL AND r.k0 IS NULL))"),
            "{sql}"
        );
    }

    #[test]
    fn rejoin_matches_row_hash_not_only_key() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Except, &ctx, false, None);
        assert!(
            sql.contains("(ld.h = l.h OR (ld.h IS NULL AND l.h IS NULL))"),
            "{sql}"
        );
        assert!(
            sql.contains("(rd.h = r.h OR (rd.h IS NULL AND r.h IS NULL))"),
            "{sql}"
        );
        let summary = render(&dialect, SetDifferenceOp::Except, &ctx, true, None);
        assert!(
            summary.contains("(d.k0 = e.k0 OR (d.k0 IS NULL AND e.k0 IS NULL))"),
            "{summary}"
        );
        assert!(!summary.contains("d.h"), "{summary}");
        assert!(!summary.contains("e.h"), "{summary}");
    }

    #[test]
    fn misaligned_columns_are_config_error() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let left = plan(
            "mysql",
            &["id"],
            &[("id", "int"), ("note", "varchar")],
            vec![],
        );
        let right = plan("mysql", &["id"], &[("id", "int")], vec![]);
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let err = render_minusdiff_sql(&ctx, &dialect, SetDifferenceOp::Except, false, None)
            .expect_err("misaligned columns");
        assert_eq!(err.kind, crate::backend::error::DbErrorKind::Config);
        let text = err.to_string();
        assert!(text.contains("note"), "{text}");
        assert!(text.contains("unmatched"), "{text}");
    }

    #[test]
    fn missing_key_spec_is_config_error() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        // Compare columns match, so alignment succeeds. The key is absent
        // from both specs, so the failure is the missing normalization spec.
        let mut left = plan("mysql", &["id"], &[("note", "varchar")], vec![]);
        left.key_specs.clear();
        let mut right = plan("mysql", &["id"], &[("note", "varchar")], vec![]);
        right.key_specs.clear();
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let err = render_minusdiff_sql(&ctx, &dialect, SetDifferenceOp::Except, false, None)
            .expect_err("missing key spec");
        assert_eq!(err.kind, crate::backend::error::DbErrorKind::Config);
        let text = err.to_string();
        assert!(text.contains("no normalization spec"), "{text}");
        assert!(text.contains("'id'"), "{text}");
        assert!(!text.contains("unmatched"), "{text}");
    }

    #[test]
    fn right_value_uses_compare_index_not_non_key_index() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        // Key is not the first compare column. Right physical order puts the
        // key first, so indexing the right specs by the non-key ordinal
        // would project CAST(`id`) as c0.
        let left = plan(
            "mysql",
            &["id"],
            &[("note", "varchar"), ("id", "int")],
            vec![],
        );
        let right = plan(
            "mysql",
            &["id"],
            &[("id", "int"), ("note", "varchar")],
            vec![],
        );
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&dialect, SetDifferenceOp::Except, &ctx, false, None);
        let right_cte = sql.split("r AS (").nth(1).expect("right cte");
        let right_select = right_cte.split("FROM").next().expect("right select");
        assert!(
            right_select.contains("`note` AS c0"),
            "c0 must be the paired value, got {right_select}"
        );
        assert!(
            !right_select.contains("CAST(`id` AS CHAR) AS c0"),
            "c0 must not be the key expression, got {right_select}"
        );
    }

    #[test]
    fn oracle_scn_is_on_both_ctes_or_on_neither() {
        let oracle = crate::backend::oracle::dialect::OracleDialect::new();
        let (left, right) = paired("oracle");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&oracle, SetDifferenceOp::Minus, &ctx, false, Some(42));
        assert_eq!(sql.matches("AS OF SCN 42").count(), 2, "{sql}");
        let none = render(&oracle, SetDifferenceOp::Minus, &ctx, false, None);
        assert!(!none.contains("AS OF SCN"), "{none}");
        let summary = render(&oracle, SetDifferenceOp::Minus, &ctx, true, Some(42));
        assert_eq!(summary.matches("AS OF SCN 42").count(), 2, "{summary}");

        let mysql = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let sql = render(&mysql, SetDifferenceOp::Except, &ctx, false, Some(42));
        assert!(!sql.contains("AS OF SCN"), "{sql}");
    }

    fn row(side: &str, rk: Value, value: Value, key: Value) -> Vec<Value> {
        vec![Value::String(side.into()), rk, value, key]
    }

    #[test]
    fn classify_left_only_as_missing_right() {
        let rows =
            classify_minus_rows(1, 1, &[row("L", json!(1), json!("a"), json!("1"))]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, DiffStatus::MissingRight);
        assert_eq!(rows[0].key, json!(1));
        assert_eq!(rows[0].left.as_ref().unwrap(), &vec![json!(1), json!("a")]);
        assert!(rows[0].right.is_none());
    }

    #[test]
    fn classify_right_only_as_missing_left() {
        let rows =
            classify_minus_rows(1, 1, &[row("R", json!(2), json!("b"), json!("2"))]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, DiffStatus::MissingLeft);
        assert_eq!(rows[0].key, json!(2));
        assert!(rows[0].left.is_none());
        assert_eq!(rows[0].right.as_ref().unwrap(), &vec![json!(2), json!("b")]);
    }

    #[test]
    fn classify_both_sides_as_modified_with_both_payloads() {
        let rows = classify_minus_rows(
            1,
            1,
            &[
                row("L", json!(1), json!("old"), json!("1")),
                row("R", json!(1), json!("new"), json!("1")),
            ],
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, DiffStatus::Modified);
        assert_eq!(
            rows[0].left.as_ref().unwrap(),
            &vec![json!(1), json!("old")]
        );
        assert_eq!(
            rows[0].right.as_ref().unwrap(),
            &vec![json!(1), json!("new")]
        );
        assert!(rows[0].confirmed);
    }

    #[test]
    fn classify_null_key_pairs_as_modified_without_cartesian() {
        let rows = classify_minus_rows(
            1,
            1,
            &[
                row("L", Value::Null, json!("old"), Value::Null),
                row("R", Value::Null, json!("new"), Value::Null),
            ],
        )
        .unwrap();
        assert_eq!(rows.len(), 1, "null keys must not cartesian-product");
        assert_eq!(rows[0].status, DiffStatus::Modified);
        assert_eq!(rows[0].key, Value::Null);
    }

    #[test]
    fn classify_duplicate_normalized_key_is_error() {
        let err = classify_minus_rows(
            1,
            1,
            &[
                row("L", json!(1), json!("a"), json!("1")),
                row("L", json!(1), json!("b"), json!("1")),
            ],
        )
        .expect_err("duplicate key");
        assert!(
            err.to_string().contains("normalized key is not unique"),
            "{err}"
        );
    }

    #[test]
    fn classify_counts_do_not_depend_on_sample_limit() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let small = ctx_with(left, right, &["id"], None, None, 1);
        let (left, right) = paired("mysql");
        let large = ctx_with(left, right, &["id"], None, None, 1000);
        let small_sql = render(&dialect, SetDifferenceOp::Except, &small, false, None);
        let large_sql = render(&dialect, SetDifferenceOp::Except, &large, false, None);
        assert_eq!(small_sql, large_sql);
        assert!(!small_sql.to_ascii_uppercase().contains("LIMIT"));
        let rows = vec![
            row("L", json!(1), json!("a"), json!("1")),
            row("R", json!(2), json!("b"), json!("2")),
        ];
        let a = classify_minus_rows(1, 1, &rows).unwrap();
        let b = classify_minus_rows(1, 1, &rows).unwrap();
        assert_eq!(a.len(), b.len());
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn empty_side_rows_are_tagged_then_classified() {
        let bare = vec![vec![json!(7), json!("only"), json!("7")]];
        let tagged = tag_empty_side_rows("L", &bare);
        let rows = classify_minus_rows(1, 1, &tagged).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, DiffStatus::MissingRight);
        assert_eq!(rows[0].key, json!(7));
        assert_eq!(
            rows[0].left.as_ref().unwrap(),
            &vec![json!(7), json!("only")]
        );
    }

    enum Step {
        Rows(QueryResult),
        Fail(&'static str),
    }

    struct ScriptedConn {
        steps: std::collections::VecDeque<Step>,
        sqls: Vec<String>,
        dialect: Box<dyn Dialect>,
    }

    #[async_trait::async_trait]
    impl DbConn for ScriptedConn {
        async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            self.sqls.push(sql.to_string());
            match self.steps.pop_front() {
                Some(Step::Rows(result)) => Ok(result),
                Some(Step::Fail(message)) => Err(DbError::query(message)),
                None => Err(DbError::query(format!(
                    "mock: no scripted response for {sql}"
                ))),
            }
        }
        async fn exec(&mut self, _sql: &str, _params: &[Value]) -> Result<QueryResult, DbError> {
            Err(DbError::unsupported("mock"))
        }
        async fn query_drop(&mut self, sql: &str) -> Result<(), DbError> {
            self.sqls.push(sql.to_string());
            Ok(())
        }
        fn dialect(&self) -> &dyn Dialect {
            self.dialect.as_ref()
        }
    }

    fn script(dialect: Box<dyn Dialect>, steps: Vec<Step>) -> ScriptedConn {
        ScriptedConn {
            steps: std::collections::VecDeque::from(steps),
            sqls: Vec::new(),
            dialect,
        }
    }

    fn count_step(n: u64) -> Step {
        Step::Rows(QueryResult {
            columns: vec!["cnt".into()],
            rows: vec![vec![json!(n)]],
            row_count: 1,
            rows_affected: None,
        })
    }

    fn checksum_step(count: u64, s1: u64) -> Step {
        Step::Rows(QueryResult {
            columns: vec![
                "c".into(),
                "s1".into(),
                "s2".into(),
                "s3".into(),
                "s4".into(),
            ],
            rows: vec![vec![json!(count), json!(s1), json!(0), json!(0), json!(0)]],
            row_count: 1,
            rows_affected: None,
        })
    }

    fn scn_step(n: u64) -> Step {
        Step::Rows(QueryResult {
            columns: vec!["CURRENT_SCN".into()],
            rows: vec![vec![json!(n)]],
            row_count: 1,
            rows_affected: None,
        })
    }

    fn body_step(rows: Vec<Vec<Value>>) -> Step {
        Step::Rows(QueryResult {
            columns: vec![],
            rows: rows.clone(),
            row_count: rows.len(),
            rows_affected: None,
        })
    }

    fn oracle_conn(steps: Vec<Step>) -> ScriptedConn {
        script(
            Box::new(crate::backend::oracle::dialect::OracleDialect::new()),
            steps,
        )
    }

    fn mysql_conn(steps: Vec<Step>) -> ScriptedConn {
        script(
            Box::new(crate::backend::mysql::dialect::MySqlDialect),
            steps,
        )
    }

    fn idle() -> ScriptedConn {
        oracle_conn(vec![])
    }

    #[tokio::test]
    async fn identical_checksum_does_not_issue_set_difference() {
        let (left, right) = paired("oracle");
        let mut ctx = ctx_with(left, right, &["id"], Some("status = 1"), None, 1000);
        ctx.consistency = ConsistencyMode::None;
        ctx.recheck = true;
        let path =
            std::env::temp_dir().join(format!("hepta-minusdiff-checkpoint-{}", std::process::id()));
        let checkpoint =
            crate::delta_diff::progress::CheckpointManager::open(&path).expect("checkpoint");
        ctx.checkpoint = Some(std::sync::Arc::new(tokio::sync::Mutex::new(checkpoint)));
        let mut left_conn = oracle_conn(vec![
            count_step(2),
            count_step(2),
            checksum_step(2, 1),
            checksum_step(2, 1),
        ]);
        let mut right_conn = idle();
        let report = MinusDiffer
            .diff(&mut left_conn, &mut right_conn, &ctx)
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(report.strategy, "minusdiff");
        assert_eq!(report.perf.queries_total, 4, "{:?}", left_conn.sqls);
        assert!(report.sample_diffs.is_empty());
        assert_eq!(report.shards[0].shard_id, "minus");
        assert!(
            left_conn
                .sqls
                .iter()
                .any(|sql| sql.contains("TO_CHAR(\"id\")") && sql.contains("status = 1")),
            "checksum must hash the key and the side filter: {:?}",
            left_conn.sqls
        );
        assert!(
            left_conn.sqls.iter().all(|sql| !sql.contains("MINUS")),
            "{:?}",
            left_conn.sqls
        );
        let warnings = report.warnings.join(" ");
        assert!(warnings.contains("ignores --recheck"), "{warnings}");
        assert!(
            warnings.contains("does not record shard checkpoints"),
            "{warnings}"
        );
        assert!(right_conn.sqls.is_empty());
    }

    #[tokio::test]
    async fn excluded_key_swapped_values_are_modified_not_identical() {
        let mut left = plan("oracle", &["id"], &[("note", "VARCHAR2")], vec![]);
        left.key_specs = vec![spec("id", "NUMBER")];
        let mut right = left.clone();
        right.url_scheme = "oracle".into();
        let mut ctx = ctx_with(left, right, &["id"], None, None, 1000);
        ctx.consistency = ConsistencyMode::None;
        let mut left_conn = oracle_conn(vec![
            count_step(2),
            count_step(2),
            checksum_step(2, 1),
            checksum_step(2, 9),
            body_step(vec![
                row("L", json!(1), json!("a"), json!("1")),
                row("L", json!(2), json!("b"), json!("2")),
                row("R", json!(1), json!("b"), json!("1")),
                row("R", json!(2), json!("a"), json!("2")),
            ]),
        ]);
        let mut right_conn = idle();
        let report = MinusDiffer
            .diff(&mut left_conn, &mut right_conn, &ctx)
            .await
            .unwrap();
        assert!(
            left_conn.sqls.iter().any(|sql| {
                sql.contains("STANDARD_HASH")
                    && sql.contains("TO_CHAR(\"id\")")
                    && !sql.contains("MINUS")
            }),
            "identity checksum must include the excluded key: {:?}",
            left_conn.sqls
        );
        assert!(
            left_conn.sqls.iter().any(|sql| sql.contains("MINUS")),
            "{:?}",
            left_conn.sqls
        );
        assert_eq!(report.summary.modified, 2);
        assert!(report
            .sample_diffs
            .iter()
            .all(|row| row.status == DiffStatus::Modified));
    }

    #[tokio::test]
    async fn empty_side_does_not_issue_set_operator() {
        let (left, right) = paired("oracle");
        let mut ctx = ctx_with(left, right, &["id"], None, None, 1000);
        ctx.consistency = ConsistencyMode::None;
        let mut left_conn = oracle_conn(vec![
            count_step(0),
            count_step(1),
            body_step(vec![vec![json!(7), json!("only"), json!("7")]]),
        ]);
        let mut right_conn = idle();
        let report = MinusDiffer
            .diff(&mut left_conn, &mut right_conn, &ctx)
            .await
            .unwrap();
        assert!(
            left_conn.sqls.iter().all(|sql| !sql.contains("MINUS")),
            "{:?}",
            left_conn.sqls
        );
        assert_eq!(report.summary.missing_left, 1);
        assert_eq!(report.sample_diffs[0].key, json!(7));
        assert_eq!(report.sample_diffs[0].status, DiffStatus::MissingLeft);
        assert_eq!(report.perf.queries_total, 3);

        let (left, right) = paired("oracle");
        let mut summary = ctx_with(left, right, &["id"], None, None, 1000);
        summary.consistency = ConsistencyMode::None;
        summary.summary_only = true;
        let mut summary_conn = oracle_conn(vec![count_step(0), count_step(1)]);
        let mut summary_right = idle();
        let report = MinusDiffer
            .diff(&mut summary_conn, &mut summary_right, &summary)
            .await
            .unwrap();
        assert_eq!(summary_conn.sqls.len(), 2, "{:?}", summary_conn.sqls);
        assert!(summary_conn.sqls.iter().all(|sql| !sql.contains("MINUS")));
        assert_eq!(report.summary.missing_left, 1);
        assert!(report.sample_diffs.is_empty());
    }

    #[tokio::test]
    async fn over_cell_budget_falls_back_before_set_operator() {
        let (left, right) = paired("oracle");
        let mut ctx = ctx_with(left, right, &["id"], None, None, 1000);
        ctx.consistency = ConsistencyMode::None;
        ctx.minus_max_cells = 1;
        ctx.fetch_all_threshold = 0;
        let mut left_conn = oracle_conn(vec![
            count_step(10),
            count_step(10),
            count_step(10),
            Step::Rows(QueryResult::empty()),
        ]);
        let mut right_conn = oracle_conn(vec![count_step(10), Step::Rows(QueryResult::empty())]);
        let report = MinusDiffer
            .diff(&mut left_conn, &mut right_conn, &ctx)
            .await
            .unwrap();
        assert_eq!(report.strategy, "keyeddiff");
        assert!(
            left_conn.sqls.iter().all(|sql| !sql.contains("MINUS")),
            "{:?}",
            left_conn.sqls
        );
        let warnings = report.warnings.join(" ");
        assert!(warnings.contains("cell budget exceeded"), "{warnings}");
        assert!(warnings.contains("falling back to keyeddiff"), "{warnings}");
        assert_eq!(report.perf.queries_total, 6, "{:?}", left_conn.sqls);
    }

    #[tokio::test]
    async fn zero_cell_budget_disables_guard() {
        let (left, right) = paired("oracle");
        let mut ctx = ctx_with(left, right, &["id"], None, None, 1000);
        ctx.consistency = ConsistencyMode::None;
        ctx.minus_max_cells = 0;
        let mut left_conn = oracle_conn(vec![
            count_step(10),
            count_step(10),
            checksum_step(10, 1),
            checksum_step(10, 2),
            body_step(vec![row("L", json!(1), json!("a"), json!("1"))]),
        ]);
        let mut right_conn = idle();
        let report = MinusDiffer
            .diff(&mut left_conn, &mut right_conn, &ctx)
            .await
            .unwrap();
        assert_eq!(report.strategy, "minusdiff");
        assert!(
            left_conn.sqls.iter().any(|sql| sql.contains("MINUS")),
            "{:?}",
            left_conn.sqls
        );
        assert_eq!(report.summary.missing_right, 1);
    }

    #[tokio::test]
    async fn probe_failure_falls_back_to_keyeddiff_without_opening_user_minus() {
        let (left, right) = paired("mysql");
        let mut ctx = ctx_with(left, right, &["id"], None, None, 1000);
        ctx.consistency = ConsistencyMode::None;
        let mut left_conn = mysql_conn(vec![
            body_step(vec![vec![json!("10.11.6-MariaDB")]]),
            Step::Fail("syntax"),
            count_step(0),
        ]);
        let mut right_conn = mysql_conn(vec![count_step(0)]);
        let report = MinusDiffer
            .diff(&mut left_conn, &mut right_conn, &ctx)
            .await
            .unwrap();
        assert_eq!(report.strategy, "keyeddiff");
        assert!(
            left_conn
                .sqls
                .iter()
                .any(|sql| sql == "SELECT 1 AS c EXCEPT SELECT 1 AS c"),
            "{:?}",
            left_conn.sqls
        );
        assert!(
            left_conn
                .sqls
                .iter()
                .all(|sql| !sql.contains("left_t") || !sql.contains("EXCEPT")),
            "{:?}",
            left_conn.sqls
        );
        assert!(
            left_conn
                .sqls
                .iter()
                .all(|sql| !sql.contains("START TRANSACTION")),
            "probe failure must not open a snapshot before fallback: {:?}",
            left_conn.sqls
        );
        let warnings = report.warnings.join(" ");
        assert!(warnings.contains("no MINUS/EXCEPT"), "{warnings}");
        assert!(warnings.contains("falling back to keyeddiff"), "{warnings}");
        assert!(
            report.perf.queries_total > 2,
            "{}",
            report.perf.queries_total
        );
        assert_eq!(report.perf.queries_total, 4);
    }

    #[tokio::test]
    async fn fallback_queries_total_includes_statements_already_sent() {
        let (left, right) = paired("mysql");
        let mut ctx = ctx_with(left, right, &["id"], None, None, 1000);
        ctx.consistency = ConsistencyMode::None;
        let mut minus_conn = mysql_conn(vec![
            body_step(vec![vec![json!("10.11.6-MariaDB")]]),
            Step::Fail("syntax"),
            count_step(0),
        ]);
        let mut minus_right = mysql_conn(vec![count_step(0)]);
        let minus = MinusDiffer
            .diff(&mut minus_conn, &mut minus_right, &ctx)
            .await
            .unwrap();
        let mut keyed_conn = mysql_conn(vec![count_step(0)]);
        let mut keyed_right = mysql_conn(vec![count_step(0)]);
        let keyed = KeyedDiffer
            .diff(&mut keyed_conn, &mut keyed_right, &ctx)
            .await
            .unwrap();
        assert!(
            minus.perf.queries_total > keyed.perf.queries_total,
            "minus {} keyed {}",
            minus.perf.queries_total,
            keyed.perf.queries_total
        );
    }

    #[tokio::test]
    async fn config_error_does_not_open_snapshot() {
        let left = plan(
            "mysql",
            &["id"],
            &[("id", "int"), ("note", "varchar")],
            vec![],
        );
        let right = plan("mysql", &["id"], &[("id", "int")], vec![]);
        let mut ctx = ctx_with(left, right, &["id"], None, None, 1000);
        ctx.consistency = ConsistencyMode::Snapshot;
        let mut left_conn = mysql_conn(vec![]);
        let mut right_conn = mysql_conn(vec![]);
        let err = MinusDiffer
            .diff(&mut left_conn, &mut right_conn, &ctx)
            .await
            .expect_err("misaligned");
        assert_eq!(err.kind, crate::backend::error::DbErrorKind::Config);
        assert!(left_conn.sqls.is_empty(), "{:?}", left_conn.sqls);
    }

    #[tokio::test]
    async fn oracle_count_checksum_and_set_diff_share_one_scn() {
        let (left, right) = paired("oracle");
        let mut ctx = ctx_with(left, right, &["id"], None, None, 1000);
        ctx.consistency = ConsistencyMode::Snapshot;
        let mut left_conn = oracle_conn(vec![
            scn_step(42),
            count_step(2),
            count_step(2),
            checksum_step(2, 1),
            checksum_step(2, 4),
            body_step(vec![row("L", json!(1), json!("a"), json!("1"))]),
        ]);
        let mut right_conn = idle();
        let report = MinusDiffer
            .diff(&mut left_conn, &mut right_conn, &ctx)
            .await
            .unwrap();
        assert_eq!(ctx.scns.get().copied(), Some((Some(42), Some(42))));
        let counts: Vec<_> = left_conn
            .sqls
            .iter()
            .filter(|sql| sql.starts_with("SELECT COUNT(*)"))
            .collect();
        assert_eq!(counts.len(), 2, "{:?}", left_conn.sqls);
        assert!(counts.iter().all(|sql| sql.contains("AS OF SCN 42")));
        assert!(left_conn
            .sqls
            .iter()
            .filter(|sql| sql.contains("STANDARD_HASH") || sql.contains("DBMS_CRYPTO"))
            .all(|sql| sql.contains("AS OF SCN 42")));
        let set_diff = left_conn
            .sqls
            .iter()
            .find(|sql| sql.contains("MINUS"))
            .expect("set diff");
        assert_eq!(set_diff.matches("AS OF SCN 42").count(), 2, "{set_diff}");
        assert_eq!(report.summary.missing_right, 1);

        let (left, right) = paired("oracle");
        let mut none = ctx_with(left, right, &["id"], None, None, 1000);
        none.consistency = ConsistencyMode::None;
        let mut none_conn = oracle_conn(vec![
            count_step(2),
            count_step(2),
            checksum_step(2, 1),
            checksum_step(2, 4),
            body_step(vec![row("L", json!(1), json!("a"), json!("1"))]),
        ]);
        let mut none_right = idle();
        MinusDiffer
            .diff(&mut none_conn, &mut none_right, &none)
            .await
            .unwrap();
        assert!(
            none_conn.sqls.iter().all(|sql| !sql.contains("AS OF SCN")),
            "{:?}",
            none_conn.sqls
        );
    }

    #[test]
    fn misaligned_columns_fail_before_the_operator_is_considered() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let left = plan(
            "mysql",
            &["id"],
            &[("id", "int"), ("note", "varchar")],
            vec![],
        );
        let right = plan("mysql", &["id"], &[("id", "int")], vec![]);
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let err = render_minusdiff_sql(&ctx, &dialect, SetDifferenceOp::None, false, None)
            .expect_err("misaligned columns");
        assert_eq!(err.kind, crate::backend::error::DbErrorKind::Config);
        let text = err.to_string();
        assert!(text.contains("unmatched"), "{text}");
        assert!(text.contains("note"), "{text}");
    }

    #[test]
    fn empty_key_fails_before_the_operator_is_considered() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &[], None, None, 1000);
        let err = render_minusdiff_sql(&ctx, &dialect, SetDifferenceOp::None, false, None)
            .expect_err("empty key");
        assert_eq!(err.kind, crate::backend::error::DbErrorKind::Config);
        let text = err.to_string();
        assert!(text.contains("comparison key"), "{text}");
    }

    #[test]
    fn prepare_minusdiff_checks_shape_without_an_operator() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let shape = prepare_minusdiff(&ctx, &dialect).expect("aligned shape");
        assert_eq!(shape.key_len, 1);
        assert_eq!(shape.left_values.len(), 1);
        assert_eq!(shape.left_values.len(), shape.right_values.len());

        let left = plan(
            "mysql",
            &["id"],
            &[("id", "int"), ("note", "varchar")],
            vec![],
        );
        let right = plan("mysql", &["id"], &[("id", "int")], vec![]);
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let err = prepare_minusdiff(&ctx, &dialect).expect_err("misaligned");
        assert_eq!(err.kind, crate::backend::error::DbErrorKind::Config);
        assert!(err.to_string().contains("unmatched"), "{err}");
    }

    #[test]
    fn aligned_plan_without_an_operator_stays_unsupported() {
        let dialect = crate::backend::mysql::dialect::MySqlDialect;
        let (left, right) = paired("mysql");
        let ctx = ctx_with(left, right, &["id"], None, None, 1000);
        let err = render_minusdiff_sql(&ctx, &dialect, SetDifferenceOp::None, false, None)
            .expect_err("no operator");
        assert_eq!(err.kind, crate::backend::error::DbErrorKind::Unsupported);
    }

    #[test]
    fn summary_counts_names_a_missing_column() {
        let result = QueryResult {
            columns: vec!["missing_right".into(), "modified".into(), "extra".into()],
            rows: vec![vec![json!(1), json!(2), json!(3)]],
            row_count: 1,
            rows_affected: None,
        };
        let err = summary_counts(&result).expect_err("missing column");
        let text = err.to_string();
        assert!(text.contains("missing_left"), "{text}");
    }

    #[tokio::test]
    async fn summary_counts_follow_column_names_not_select_position() {
        let (left, right) = paired("oracle");
        let mut ctx = ctx_with(left, right, &["id"], None, None, 1000);
        ctx.consistency = ConsistencyMode::None;
        ctx.summary_only = true;
        // Oracle returns unquoted aliases in uppercase, and not necessarily
        // in SELECT order. Positional (row[1], row[0], row[2]) would report
        // missing_left=3, missing_right=7, modified=5.
        let mut left_conn = oracle_conn(vec![
            count_step(4),
            count_step(6),
            checksum_step(4, 1),
            checksum_step(6, 2),
            Step::Rows(QueryResult {
                columns: vec![
                    "MODIFIED".into(),
                    "MISSING_RIGHT".into(),
                    "MISSING_LEFT".into(),
                ],
                rows: vec![vec![json!(7), json!(3), json!(5)]],
                row_count: 1,
                rows_affected: None,
            }),
        ]);
        let mut right_conn = idle();
        let report = MinusDiffer
            .diff(&mut left_conn, &mut right_conn, &ctx)
            .await
            .unwrap();
        assert_eq!(report.summary.missing_left, 5, "{:?}", report.summary);
        assert_eq!(report.summary.missing_right, 3, "{:?}", report.summary);
        assert_eq!(report.summary.modified, 7, "{:?}", report.summary);
    }

    #[test]
    fn summary_counts_short_row_stays_projection_too_short() {
        let result = QueryResult {
            columns: vec![
                "missing_right".into(),
                "missing_left".into(),
                "modified".into(),
            ],
            rows: vec![vec![json!(1), json!(2)]],
            row_count: 1,
            rows_affected: None,
        };
        let err = summary_counts(&result).expect_err("short row");
        assert!(err.to_string().contains("projection too short"), "{err}");
    }

    #[tokio::test]
    async fn open_snapshot_counts_mysql_version_and_start() {
        let mut conn = mysql_conn(vec![body_step(vec![vec![json!("8.0.36")]])]);
        let sent = open_snapshot(&mut conn, false).await.unwrap();
        assert_eq!(sent, 2);
        assert_eq!(
            conn.sqls,
            vec![
                "SELECT VERSION()".to_string(),
                "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn open_snapshot_counts_polardbx_as_three_statements() {
        let mut conn = mysql_conn(vec![body_step(vec![vec![json!(
            "5.6.29-PXC-5.4.19-SNAPSHOT"
        )]])]);
        let sent = open_snapshot(&mut conn, false).await.unwrap();
        assert_eq!(sent, 3);
        assert_eq!(
            conn.sqls,
            vec![
                "SELECT VERSION()".to_string(),
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ".to_string(),
                "START TRANSACTION READ ONLY".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn capture_scn_reports_whether_it_queried() {
        let mut oracle = oracle_conn(vec![scn_step(7)]);
        let captured = capture_scn(&mut oracle, false).await.unwrap();
        assert_eq!(captured.scn, Some(7));
        assert_eq!(captured.statements, 1);
        assert_eq!(
            oracle.sqls,
            vec!["SELECT CURRENT_SCN FROM V$DATABASE".to_string()]
        );

        let mut mysql = mysql_conn(vec![]);
        let captured = capture_scn(&mut mysql, false).await.unwrap();
        assert_eq!(captured.scn, None);
        assert_eq!(captured.statements, 0);
        assert!(mysql.sqls.is_empty(), "{:?}", mysql.sqls);
    }
}
