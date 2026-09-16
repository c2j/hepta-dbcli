// ─── delta-diff: table data comparison between two connections ────────
//
// CLI entry per design doc §二/§四: argument parsing (cmd.rs),
// connection-name resolution and the exit-code contract (§2.3).

use std::path::PathBuf;

use crate::audit::event::{
    read_only_session_for, ActionClass, AuditOutcome, Channel, ConnectionInfo, Decision, DraftEvent,
};
use crate::config;

pub(crate) mod api;
pub(crate) mod bucket_diff;
pub(crate) mod checksum;
pub(crate) mod cmd;
pub(crate) mod engine;
pub(crate) mod export;
pub(crate) mod hash_diff;
pub(crate) mod hydrate;
pub(crate) mod iblt_diff;
pub(crate) mod join_diff;
pub(crate) mod keyed_diff;
pub(crate) mod metadata;
pub(crate) mod naive_diff;
pub(crate) mod output;
pub(crate) mod pairing;
pub(crate) mod progress;
pub(crate) mod recheck;
pub(crate) mod report;
pub(crate) mod rowdiff;
pub(crate) mod sample;
pub(crate) mod sql_patch;
pub(crate) mod strategy;

// ─── Exit codes (CI/CD contract, §2.3) ─────────────────────────────────

pub(crate) const EXIT_IDENTICAL: i32 = 0;
pub(crate) const EXIT_DIFF: i32 = 1;
pub(crate) const EXIT_ERROR: i32 = 2;

fn paired_side_keys(
    logical_keys: &[String],
    catalog_left_keys: &[String],
    catalog_right_keys: &[String],
    left_keys: Vec<String>,
    right_keys: Vec<String>,
) -> Result<(Vec<String>, Vec<String>), String> {
    if logical_keys.is_empty() && catalog_left_keys.is_empty() && catalog_right_keys.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if left_keys.len() == logical_keys.len() && right_keys.len() == logical_keys.len() {
        return Ok((left_keys, right_keys));
    }
    Err(format!(
        "cannot establish 1:1 key correspondence; left key [{}], right key [{}]",
        catalog_left_keys.join(", "),
        catalog_right_keys.join(", ")
    ))
}

// ─── Entry Point ───────────────────────────────────────────────────────

pub(crate) async fn run(
    args: cmd::DeltaDiffArgs,
    config_path: Option<String>,
    audit: &crate::audit::AuditSession,
) -> i32 {
    if let Err(e) = args.validate() {
        eprintln!("error: {}", e);
        return EXIT_ERROR;
    }

    let raw = match config::read_config(config_path.map(PathBuf::from)) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
    };

    let left = match resolve_named(&raw, &args.left) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
    };
    let right = match resolve_named(&raw, &args.right) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {}", e);
            return EXIT_ERROR;
        }
    };

    let left_info = ConnectionInfo::from_url(
        &left.name,
        &left.connection_url,
        read_only_session_for(&left.connection_url),
    );
    let right_info = ConnectionInfo::from_url(
        &right.name,
        &right.connection_url,
        read_only_session_for(&right.connection_url),
    );
    let tables: Vec<String> = [args.left_table_name(), args.right_table_name()]
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect();
    let strategy = args.strategy.to_string();

    audit.record_best_effort(delta_diff_start_event(
        &left_info,
        &right_info,
        &tables,
        &strategy,
    ));

    let started = std::time::Instant::now();
    let code = if args.dry_run {
        dry_run_enhanced(&args, &left, &right).await
    } else {
        execute_diff(&args, &left, &right).await
    };
    let duration_ms = started.elapsed().as_millis() as u64;

    let outcome = if code == EXIT_ERROR {
        AuditOutcome::error(duration_ms, "delta_diff")
    } else {
        AuditOutcome::ok(duration_ms)
    };
    audit.record_best_effort(delta_diff_outcome_event(
        &left_info,
        &right_info,
        &tables,
        &strategy,
        outcome,
    ));

    code
}

// ─── Audit event builders (issue #57) ────────────────────────────────────

/// Action-specific context for a `delta_diff` event. The left side is the
/// event's `connection`; the right side, tables and strategy go into `detail`.
/// Diff rows are never included.
pub(crate) fn delta_diff_detail(
    right: &ConnectionInfo,
    tables: &[String],
    strategy: &str,
) -> serde_json::Value {
    serde_json::json!({
        "right": right,
        "tables": tables,
        "strategy": strategy,
    })
}

pub(crate) fn delta_diff_start_event(
    left: &ConnectionInfo,
    right: &ConnectionInfo,
    tables: &[String],
    strategy: &str,
) -> DraftEvent {
    DraftEvent::new(
        Channel::DeltaDiff,
        left.clone(),
        "delta_diff",
        ActionClass::Meta,
        Decision::Allow,
    )
    .with_detail(delta_diff_detail(right, tables, strategy))
}

pub(crate) fn delta_diff_outcome_event(
    left: &ConnectionInfo,
    right: &ConnectionInfo,
    tables: &[String],
    strategy: &str,
    outcome: AuditOutcome,
) -> DraftEvent {
    let decision = if outcome.ok {
        Decision::Allow
    } else {
        Decision::Error
    };
    DraftEvent::new(
        Channel::DeltaDiff,
        left.clone(),
        "delta_diff",
        ActionClass::Meta,
        decision,
    )
    .with_detail(delta_diff_detail(right, tables, strategy))
    .with_outcome(outcome)
}

// ─── dry-run 预检（§2.2/§12.2：仅元数据与键域探查，不执行比对）────────

async fn dry_run_enhanced(
    args: &cmd::DeltaDiffArgs,
    left: &config::ResolvedConnection,
    right: &config::ResolvedConnection,
) -> i32 {
    match dry_run_inner(args, left, right).await {
        Ok(text) => {
            println!("{text}");
            EXIT_IDENTICAL
        }
        Err(e) => {
            eprintln!("error: {}", e);
            EXIT_ERROR
        }
    }
}

async fn dry_run_inner(
    args: &cmd::DeltaDiffArgs,
    left: &config::ResolvedConnection,
    right: &config::ResolvedConnection,
) -> Result<String, String> {
    let registry = crate::create_registry();
    let (_lp, mut lconn) = connect_side(&registry, left).await?;
    let (_rp, mut rconn) = connect_side(&registry, right).await?;

    let lschema = side_schema(
        args.left_schema.as_deref().or(args.schema.as_deref()),
        left,
        &mut *lconn,
    )
    .await?;
    let rschema = side_schema(
        args.right_schema.as_deref().or(args.schema.as_deref()),
        right,
        &mut *rconn,
    )
    .await?;
    let ltable = args.left_table_name().ok_or("missing --table")?;
    let rtable = args.right_table_name().ok_or("missing --table")?;

    let lplan = metadata::build_table_plan(
        &mut *lconn,
        &lschema,
        ltable,
        &args.columns_list(),
        &args.key_list(),
        args.rtrim_char_columns,
    )
    .await
    .map_err(|e| format!("left plan: {}", e))?;
    let rplan = metadata::build_table_plan(
        &mut *rconn,
        &rschema,
        rtable,
        &args.columns_list(),
        &args.key_list(),
        args.rtrim_char_columns,
    )
    .await
    .map_err(|e| format!("right plan: {}", e))?;

    let routed = engine::route(args, left, right, &lplan, &rplan)?;
    let paired = pairing::pair_plans(&lplan, &rplan);
    let mut dry_run_warnings = routed.warnings.clone();
    dry_run_warnings.extend(api::cross_db_column_type_warnings(
        &lplan,
        &rplan,
        &paired,
        args.rtrim_char_columns,
    ));
    let (left_key_columns, right_key_columns) = paired_side_keys(
        &routed.key_columns,
        &lplan.key_columns,
        &rplan.key_columns,
        paired.left_key_columns,
        paired.right_key_columns,
    )?;

    let (lminmax, rminmax) = if routed.key_columns.len() == 1
        && matches!(routed.strategy.name(), "hashdiff" | "iblt" | "joindiff")
    {
        (
            min_max(&mut *lconn, &lschema, ltable, &left_key_columns[0])
                .await
                .ok(),
            min_max(&mut *rconn, &rschema, rtable, &right_key_columns[0])
                .await
                .ok(),
        )
    } else {
        (None, None)
    };

    let mut out = String::new();
    out.push_str(&format!(
        "dry-run plan\n  strategy         : {}",
        routed.strategy.name()
    ));
    append_dry_run_warnings(&mut out, &dry_run_warnings);
    out.push_str(&format!(
        "\n  left             : {}.{} ({})\n  right            : {}.{} ({})",
        lschema, ltable, left.name, rschema, rtable, right.name
    ));
    if !routed.key_columns.is_empty() {
        out.push_str(&format!(
            "\n  key              : {}",
            format_dry_run_key(&routed.key_columns)
        ));
    }
    out.push_str(&format!(
        "\n  compare columns  : {} (left) / {} (right)",
        lplan.compare_columns.len(),
        rplan.compare_columns.len()
    ));
    if !lplan.warnings.is_empty() || !rplan.warnings.is_empty() {
        out.push_str(&format!(
            "\n  excluded columns : {}",
            lplan
                .warnings
                .iter()
                .chain(rplan.warnings.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let strategy = routed.strategy.name();
    let minmax = match (lminmax, rminmax) {
        (Some(l), Some(r)) => Some((l.0.min(r.0), l.1.max(r.1))),
        _ => None,
    };
    out.push('\n');
    out.push_str(&format_key_domain_line(strategy, minmax));
    if matches!(strategy, "hashdiff" | "iblt" | "joindiff") && minmax.is_some() {
        let segments = args.threads * 8;
        out.push_str(&format!(
            "\n  first-pass segs  : {segments} (threads×8)\n  est. queries     : ≈{} (checksum) + bisect on diff segs",
            segments * 2
        ));
    }
    out.push_str(&format!(
        "\n  consistency      : {}\n  recheck          : {}\n  statement timeout: {}s\n  threads          : {}",
        args.consistency,
        args.recheck_effective(),
        args.statement_timeout,
        args.threads
    ));
    Ok(out)
}

async fn min_max(
    conn: &mut (dyn crate::backend::DbConn + Send),
    schema: &str,
    table: &str,
    key: &str,
) -> Result<(i64, i64), String> {
    let d = conn.dialect();
    let k = d.quote_ident(key);
    let t = d.quote_table(Some(schema), table);
    let sql = format!("SELECT MIN({k}), MAX({k}) FROM {t}");
    let r = conn.query(&sql).await.map_err(|e| e.to_string())?;
    let row = r.rows.first().ok_or("no rows")?;
    let lo = row.first().and_then(|v| v.as_i64()).ok_or("no min")?;
    let hi = row.get(1).and_then(|v| v.as_i64()).ok_or("no max")?;
    Ok((lo, hi))
}

// ─── Strategy Execution ────────────────────────────────────────────────

async fn execute_diff(
    args: &cmd::DeltaDiffArgs,
    left: &config::ResolvedConnection,
    right: &config::ResolvedConnection,
) -> i32 {
    match execute_diff_inner(args, left, right).await {
        Ok(report) => {
            let exit = if report.has_diff() {
                EXIT_DIFF
            } else {
                EXIT_IDENTICAL
            };
            if let Err(e) = emit_report(args, &report) {
                eprintln!("error: failed to emit report: {}", e);
                return EXIT_ERROR;
            }
            exit
        }
        Err(e) => {
            eprintln!("error: {}", e);
            EXIT_ERROR
        }
    }
}

async fn execute_diff_inner(
    args: &cmd::DeltaDiffArgs,
    left: &config::ResolvedConnection,
    right: &config::ResolvedConnection,
) -> Result<report::DiffReport, String> {
    let registry = crate::create_registry();
    let timeout_ms = args.statement_timeout.saturating_mul(1000);

    let (lpool, mut lconn) = connect_side(&registry, left).await?;
    let (rpool, mut rconn) = connect_side(&registry, right).await?;
    for conn in [&mut lconn, &mut rconn] {
        if let Some(sql) = conn.dialect().set_statement_timeout_sql(timeout_ms) {
            conn.query_drop(&sql).await.map_err(|e| e.to_string())?;
        }
    }

    let lschema = side_schema(
        args.left_schema.as_deref().or(args.schema.as_deref()),
        left,
        &mut *lconn,
    )
    .await?;
    let rschema = side_schema(
        args.right_schema.as_deref().or(args.schema.as_deref()),
        right,
        &mut *rconn,
    )
    .await?;
    let ltable = args.left_table_name().ok_or("missing --table")?;
    let rtable = args.right_table_name().ok_or("missing --table")?;

    let api::Preflight {
        lplan,
        rplan,
        routed,
        paired,
        warnings,
    } = api::preflight(
        api::PreflightSide {
            conn: &mut *lconn,
            schema: &lschema,
            table: ltable,
            connection_url: &left.connection_url,
        },
        api::PreflightSide {
            conn: &mut *rconn,
            schema: &rschema,
            table: rtable,
            connection_url: &right.connection_url,
        },
        &args.columns_list(),
        &args.key_list(),
        Some(args.strategy),
        matches!(args.consistency, cmd::ConsistencyMode::None),
        args.rtrim_char_columns,
    )
    .await?;
    let (left_key_columns, right_key_columns) = paired_side_keys(
        &routed.key_columns,
        &lplan.key_columns,
        &rplan.key_columns,
        paired.left_key_columns,
        paired.right_key_columns,
    )?;

    let (filter, incremental) = effective_filter(args);
    let checkpoint = match &args.checkpoint {
        Some(path) => {
            let cp = progress::CheckpointManager::open(path).map_err(|e| e.to_string())?;
            if cp.corrupted_lines > 0 {
                eprintln!(
                    "warning: checkpoint file had {} corrupted line(s), skipped",
                    cp.corrupted_lines
                );
            }
            Some(std::sync::Arc::new(tokio::sync::Mutex::new(cp)))
        }
        None => None,
    };

    let ctx = strategy::DiffContext {
        left: strategy::SideCtx {
            connection_name: left.name.clone(),
            schema: Some(lschema),
            table: ltable.to_string(),
            plan: lplan,
        },
        right: strategy::SideCtx {
            connection_name: right.name.clone(),
            schema: Some(rschema),
            table: rtable.to_string(),
            plan: rplan,
        },
        left_pool: lpool,
        right_pool: rpool,
        key_column: routed.key_column,
        key_columns: routed.key_columns,
        left_key_columns,
        right_key_columns,
        filter,
        incremental,
        bisection_factor: args.bisection_factor,
        bisection_threshold: args.bisection_threshold as u64,
        sample_limit: args.sample,
        threads: args.threads,
        consistency: match args.consistency {
            cmd::ConsistencyMode::Snapshot => strategy::ConsistencyMode::Snapshot,
            cmd::ConsistencyMode::None => strategy::ConsistencyMode::None,
        },
        recheck: args.recheck_effective(),
        route_warnings: warnings,
        checkpoint,
        iblt_capacity: args.iblt_capacity,
        fetch_all_threshold: args.fetch_all_threshold,
        naive_max_rows: args.naive_max_rows,
        strict: args.strict,
        scns: std::sync::OnceLock::new(),
        verbose: args.verbose,
    };

    let mut report = routed
        .strategy
        .diff(&mut *lconn, &mut *rconn, &ctx)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(path) = &args.checkpoint {
        progress::finalize_path(path).map_err(|e| e.to_string())?;
    }

    let apply_right = matches!(args.apply_to, Some(cmd::ApplyTo::Right));
    let qconn = if apply_right { &rconn } else { &lconn };
    report.ident_quote = qconn.dialect().identifier_quote();
    report.ident_scheme = qconn.dialect().url_scheme().to_string();
    report.backslash_escape = qconn.dialect().url_scheme() == "mysql";

    let did_fetch =
        hydrate::post_diff_fetch(args, &mut report, &mut *lconn, &mut *rconn, &ctx).await?;
    if did_fetch && matches!(args.consistency, cmd::ConsistencyMode::Snapshot) {
        report
            .warnings
            .push("sample/export row fetch ran after snapshot commit".into());
    }
    // 直方图必须在 hydrate 之后计算：keyless 回查可能把 payload 从 HashCount
    // 翻成 Columns 并补全列名，先算会得到空直方图
    report.modified_columns = sample::compute_modified_columns(&report);
    if report.sample_diffs.len() > 100_000 {
        report.warnings.push(format!(
            "diff row count {} exceeds 100000; memory and export may be large",
            report.sample_diffs.len()
        ));
    }
    Ok(report)
}

async fn connect_side(
    registry: &crate::backend::factory::BackendRegistry,
    side: &config::ResolvedConnection,
) -> Result<
    (
        std::sync::Arc<dyn crate::backend::DbPool>,
        Box<dyn crate::backend::DbConn + Send>,
    ),
    String,
> {
    let scheme = side
        .connection_url
        .find("://")
        .map(|i| &side.connection_url[..i])
        .unwrap_or("mysql");
    let pool = registry
        .connect_with_fallback(
            scheme,
            &side.connection_url,
            Some(&side.timeout_config),
            false,
        )
        .await
        .map_err(|e| format!("connect '{}': {}", side.name, e))?;
    let conn = pool.acquire().await.map_err(|e| e.to_string())?;
    Ok((pool, conn))
}

/// schema 解析优先级：--left-schema/--schema > 连接默认库（MySQL 取 URL path；
/// GaussDB/Oracle 取会话 current_schema——其 URL dbname 是数据库而非元数据
/// schema，对应 §2.2 "覆盖连接默认库"语义）。
async fn side_schema(
    override_opt: Option<&str>,
    conn: &config::ResolvedConnection,
    db: &mut (dyn crate::backend::DbConn + Send),
) -> Result<String, String> {
    if let Some(s) = override_opt {
        return Ok(s.to_string());
    }
    side_schema_from_conn(db, &conn.connection_url, &conn.name).await
}

/// 无配置对象版本（api.rs/MCP 共用）。
pub(crate) async fn side_schema_from_conn(
    db: &mut (dyn crate::backend::DbConn + Send),
    url: &str,
    name: &str,
) -> Result<String, String> {
    match db.dialect().url_scheme() {
        "mysql" => Ok(default_schema_from_url(url).unwrap_or_else(|| name.to_string())),
        scheme => {
            let sql = match scheme {
                "oracle" => "SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') FROM dual",
                _ => "SELECT current_schema()",
            };
            let r = db.query(sql).await.map_err(|e| e.to_string())?;
            r.rows
                .first()
                .and_then(|row| row.first())
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| "cannot determine current schema".to_string())
        }
    }
}

/// 有效过滤：--update-column/--update-since 转为增量规格（谓词在 spec 构建
/// 时按方言渲染，见 strategy::side_filter；与 --where 互斥由 clap 保证）。
fn effective_filter(args: &cmd::DeltaDiffArgs) -> (Option<String>, Option<(String, String)>) {
    if args.where_condition.is_some() {
        return (args.where_condition.clone(), None);
    }
    match (&args.update_column, &args.update_since) {
        (Some(col), since) => (
            None,
            Some((col.clone(), since.clone().unwrap_or_else(|| "1 day".into()))),
        ),
        _ => (None, None),
    }
}

fn default_schema_from_url(url: &str) -> Option<String> {
    if let Some(pos) = url.find("://") {
        let rest = &url[pos + 3..];
        let path = rest.split('/').nth(1)?;
        let db = path.split(['?', '#']).next()?;
        if !db.is_empty() {
            return Some(db.to_string());
        }
        return None;
    }
    url.split_whitespace()
        .find_map(|kv| kv.strip_prefix("dbname=").map(str::to_string))
}

fn emit_report(args: &cmd::DeltaDiffArgs, report: &report::DiffReport) -> Result<(), String> {
    let buf = render_stdout(args, report)?;
    match &args.output {
        Some(path) => std::fs::write(path, &buf).map_err(|e| e.to_string())?,
        None => {
            print!("{}", String::from_utf8_lossy(&buf));
        }
    }
    write_export(args, report)
}

fn write_export(args: &cmd::DeltaDiffArgs, report: &report::DiffReport) -> Result<(), String> {
    let Some(path) = &args.export else {
        return Ok(());
    };
    let fmt = cmd::infer_export_format(Some(path), args.export_format)?;
    let body = match fmt {
        cmd::ExportFormat::Sql => {
            let apply_to = args
                .apply_to
                .ok_or_else(|| "--export .sql requires --apply-to left|right".to_string())?;
            let (conn, schema, table) = match apply_to {
                cmd::ApplyTo::Left => (
                    report.left.connection.as_str(),
                    report.left.schema.as_deref(),
                    report.left.table.as_str(),
                ),
                cmd::ApplyTo::Right => (
                    report.right.connection.as_str(),
                    report.right.schema.as_deref(),
                    report.right.table.as_str(),
                ),
            };
            sql_patch::render_sql_patch(
                report,
                &sql_patch::SqlPatchOpts {
                    apply_to,
                    scheme: if report.ident_scheme.is_empty() {
                        "gaussdb"
                    } else {
                        report.ident_scheme.as_str()
                    },
                    quote: if report.ident_quote == '\0' {
                        '"'
                    } else {
                        report.ident_quote
                    },
                    backslash_escape: report.backslash_escape,
                    target_conn: conn,
                    target_schema: schema,
                    target_table: table,
                },
            )?
        }
        other => export::render_export(report, other, args.export_rows_effective())?,
    };
    std::fs::write(path, body).map_err(|e| e.to_string())
}

fn render_stdout(
    args: &cmd::DeltaDiffArgs,
    report: &report::DiffReport,
) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::new();
    match args.format {
        crate::cli::OutputFormat::Json => {
            let s = serde_json::to_string_pretty(report)
                .map_err(|e| format!("json serialize: {}", e))?;
            buf.extend_from_slice(s.as_bytes());
        }
        crate::cli::OutputFormat::Table => {
            let summary = output::summary_to_query_result(report);
            crate::cli::render_result(&summary, &mut buf, crate::cli::OutputFormat::Table)
                .map_err(|e| e.to_string())?;
            if !report.warnings.is_empty() {
                buf.extend_from_slice(
                    format!("warnings: {}\n", report.warnings.join("; ")).as_bytes(),
                );
            }
            if !args.summary_only && !report.sample_diffs.is_empty() {
                buf.push(b'\n');
                buf.extend_from_slice(
                    output::render_compact_sample(report, args.sample, args.wide, args.sample_mode)
                        .as_bytes(),
                );
            }
        }
        fmt => {
            let summary = output::summary_to_query_result(report);
            crate::cli::render_result(&summary, &mut buf, fmt).map_err(|e| e.to_string())?;
            if !report.warnings.is_empty() {
                buf.extend_from_slice(
                    format!("warnings: {}\n", report.warnings.join("; ")).as_bytes(),
                );
            }
            if !args.summary_only && !report.sample_diffs.is_empty() {
                buf.extend_from_slice(b"\nsample diffs:\n");
                let indices = sample::select_sample_indices(report, args.sample, args.sample_mode);
                let mut sampled = report.clone();
                sampled.sample_diffs = indices
                    .into_iter()
                    .map(|i| report.sample_diffs[i].clone())
                    .collect();
                let diffs = output::diffs_to_query_result(&sampled);
                crate::cli::render_result(&diffs, &mut buf, fmt).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(buf)
}

/// 按名解析连接（风格对齐 main.rs handle_check_connection_cmd）；
/// 未命中时报错并列出可用连接名。
fn resolve_named(
    raw: &config::McpRawConfig,
    name: &str,
) -> Result<config::ResolvedConnection, String> {
    let target_conn = raw
        .connections
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| {
            let available: Vec<&str> = raw.connections.iter().map(|c| c.name.as_str()).collect();
            format!(
                "connection '{}' not found\n  available: {:?}",
                name, available
            )
        })?;

    if raw.is_env_var {
        Ok(config::resolve_env_var_connection(
            target_conn.url.clone().unwrap_or_default(),
        ))
    } else {
        config::resolve_single_connection(
            target_conn,
            raw.config_path.clone(),
            raw.base_timeout.as_ref(),
        )
    }
}

fn describe_side(
    conn: &config::ResolvedConnection,
    schema: Option<&str>,
    table: Option<&str>,
) -> String {
    let table = table.unwrap_or("<missing>");
    match schema {
        Some(s) => format!("{} ({}.{})", conn.name, s, table),
        None => format!("{} ({})", conn.name, table),
    }
}

fn format_name(fmt: crate::cli::OutputFormat) -> &'static str {
    match fmt {
        crate::cli::OutputFormat::Table => "table",
        crate::cli::OutputFormat::Json => "json",
        crate::cli::OutputFormat::Vertical => "vertical",
        crate::cli::OutputFormat::Csv => "csv",
    }
}

fn format_dry_run_key(key_columns: &[String]) -> String {
    key_columns.join(",")
}

fn append_dry_run_warnings(out: &mut String, warnings: &[String]) {
    if !warnings.is_empty() {
        out.push_str(&format!("\n  route warnings   : {}", warnings.join("; ")));
    }
}

fn format_key_domain_line(strategy: &str, minmax: Option<(i64, i64)>) -> String {
    match strategy {
        "keyeddiff" => "  key domain       : (not applicable — keyeddiff)".to_string(),
        "bucketdiff" => "  key domain       : (not applicable — bucketdiff)".to_string(),
        "naivediff" => "  key domain       : (not applicable — naivediff)".to_string(),
        _ => match minmax {
            Some((lo, hi)) => format!("  key domain       : [{lo}, {hi}]"),
            None => "  key domain       : (unavailable)".to_string(),
        },
    }
}

#[cfg(test)]
mod audit_event_tests {
    use super::{
        delta_diff_outcome_event, delta_diff_start_event, ActionClass, AuditOutcome, Channel,
        ConnectionInfo, Decision,
    };

    fn left() -> ConnectionInfo {
        ConnectionInfo::from_url("dev", "mysql://u:p@127.0.0.1:3306/testdb", true)
    }

    fn right() -> ConnectionInfo {
        ConnectionInfo::from_url(
            "prod",
            "oracle://scott:tiger@oracle.internal:1521/FREEPDB1",
            true,
        )
    }

    #[test]
    fn start_event_has_delta_diff_channel_action_and_decision() {
        let e = delta_diff_start_event(&left(), &right(), &["orders".into()], "hashdiff");
        assert_eq!(e.channel, Channel::DeltaDiff);
        assert_eq!(e.action, "delta_diff");
        assert_eq!(e.class, ActionClass::Meta);
        assert_eq!(e.decision, Decision::Allow);
        assert!(e.outcome.is_none());
    }

    #[test]
    fn start_event_records_left_connection_metadata() {
        let e = delta_diff_start_event(&left(), &right(), &["orders".into()], "hashdiff");
        assert_eq!(e.connection.name, "dev");
        assert_eq!(e.connection.driver, "mysql");
        assert_eq!(e.connection.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(e.connection.port, Some(3306));
        assert_eq!(e.connection.database.as_deref(), Some("testdb"));
        assert!(e.connection.read_only_session);
    }

    #[test]
    fn start_event_records_right_connection_tables_and_strategy() {
        let e = delta_diff_start_event(&left(), &right(), &["orders".into()], "keyeddiff");
        assert!(e.sql.is_none(), "delta_diff has no SQL text");
        let v = e.detail.expect("detail");
        assert_eq!(v["right"]["name"], "prod");
        assert_eq!(v["right"]["driver"], "oracle");
        assert_eq!(v["right"]["database"], "FREEPDB1");
        assert_eq!(v["tables"], serde_json::json!(["orders"]));
        assert_eq!(v["strategy"], "keyeddiff");
    }

    #[test]
    fn events_carry_no_diff_row_data() {
        let tables = ["orders".to_string(), "items".to_string()];
        let start = delta_diff_start_event(&left(), &right(), &tables, "hashdiff");
        let outcome =
            delta_diff_outcome_event(&left(), &right(), &tables, "hashdiff", AuditOutcome::ok(42));

        let v = start.detail.as_ref().expect("start detail");
        assert!(v.get("rows").is_none(), "must not record row payloads");
        assert!(v.get("diffs").is_none(), "must not record diff rows");

        let o = outcome.outcome.as_ref().expect("outcome");
        assert!(o.ok);
        assert_eq!(o.duration_ms, 42);
        assert!(o.row_count.is_none());
        assert!(o.rows_affected.is_none());
    }

    #[test]
    fn error_outcome_is_not_ok_and_marks_decision_error() {
        let event = delta_diff_outcome_event(
            &left(),
            &right(),
            &["orders".into()],
            "auto",
            AuditOutcome::error(7, "delta_diff"),
        );
        assert_eq!(event.decision, Decision::Error);
        let o = event.outcome.expect("outcome");
        assert!(!o.ok);
        assert_eq!(o.duration_ms, 7);
        assert_eq!(o.error_kind.as_deref(), Some("delta_diff"));
    }
}

#[cfg(test)]
mod dry_run_format_tests {
    use super::{
        append_dry_run_warnings, format_dry_run_key, format_key_domain_line, paired_side_keys,
    };

    #[test]
    fn composite_keys_join_with_comma() {
        assert_eq!(format_dry_run_key(&["k1".into(), "k2".into()]), "k1,k2");
    }

    #[test]
    fn dry_run_output_includes_scale_skew_warning() {
        let mut out = "dry-run plan".to_string();
        append_dry_run_warnings(
            &mut out,
            &["column 'amount': NUMBER(16,2) (left) vs numeric (right) — declared numeric scale differs".into()],
        );
        assert!(out.contains("declared numeric scale differs"), "{out}");
    }

    #[test]
    fn dry_run_keyeddiff_key_domain_label() {
        assert_eq!(
            format_key_domain_line("keyeddiff", None),
            "  key domain       : (not applicable — keyeddiff)"
        );
    }

    #[test]
    fn dry_run_shows_naivediff_strategy() {
        assert_eq!(
            format_key_domain_line("naivediff", None),
            "  key domain       : (not applicable — naivediff)"
        );
    }

    #[test]
    fn paired_side_keys_rejects_unresolved_correspondence() {
        let error = paired_side_keys(
            &["K_XWDM".into(), "SECURITY_ID".into()],
            &["K_XWDM".into(), "SECURITY_ID".into()],
            &["k_xwdm".into()],
            Vec::new(),
            Vec::new(),
        )
        .unwrap_err();

        assert!(error.contains("left key [K_XWDM, SECURITY_ID]"), "{error}");
        assert!(error.contains("right key [k_xwdm]"), "{error}");
    }
}

#[cfg(test)]
mod emit_tests {
    use super::*;
    use crate::delta_diff::report::*;
    use chrono::Utc;
    use clap::Parser;
    use serde_json::Value;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: cmd::DeltaDiffArgs,
    }

    fn parse(argv: &[&str]) -> cmd::DeltaDiffArgs {
        TestCli::try_parse_from(argv).unwrap().args
    }

    fn report_with_n_diffs(n: usize) -> DiffReport {
        let diffs = (0..n)
            .map(|i| DiffRow {
                key: Value::from(i as i64),
                left: Some(vec![Value::from(i as i64), Value::from("x")]),
                right: None,
                status: DiffStatus::MissingRight,
                confirmed: true,
            })
            .collect();
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
            strategy: "keyeddiff".into(),
            consistency: "none".into(),
            hash_algorithm: "md5".into(),
            summary: DiffSummary {
                left_total: n as u64,
                right_total: 0,
                missing_left: 0,
                missing_right: n as u64,
                modified: 0,
                diff_rate: 1.0,
            },
            perf: PerfMetrics::default(),
            shards: vec![],
            sample_diffs: diffs,
            warnings: vec![],
            row_payload: RowPayload::Columns,
            key_columns: vec!["id".into()],
            value_columns: vec!["name".into()],
            column_data_types: vec![],
            ident_quote: '"',
            ident_scheme: String::new(),
            backslash_escape: false,
            modified_columns: None,
        }
    }

    #[test]
    fn json_stdout_includes_all_diffs_even_if_sample_is_2() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--format",
            "json",
            "--sample",
            "2",
        ]);
        let buf = render_stdout(&args, &report_with_n_diffs(5)).unwrap();
        let v: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["sample_diffs"].as_array().unwrap().len(), 5);
    }

    fn mixed_status_report() -> report::DiffReport {
        // 5 行 MissingRight + 5 行 Modified(改 name)，列同 report_with_n_diffs
        let mut r = report_with_n_diffs(5);
        r.summary.missing_right = 5;
        r.summary.modified = 5;
        r.summary.diff_rate = 1.0;
        let mut rows: Vec<report::DiffRow> = r.sample_diffs.clone();
        for i in 5..10 {
            rows.push(report::DiffRow {
                key: Value::from(i),
                left: Some(vec![Value::from(i), Value::from("x")]),
                right: Some(vec![Value::from(i), Value::from("y")]),
                status: DiffStatus::Modified,
                confirmed: true,
            });
        }
        r.sample_diffs = rows;
        r
    }

    #[test]
    fn csv_stdout_diverse_sample_contains_both_statuses() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--format",
            "csv",
            "--sample",
            "4",
        ]);
        let text = String::from_utf8(render_stdout(&args, &mixed_status_report()).unwrap())
            .expect("utf8 output");
        let sample = text
            .split("sample diffs:\n")
            .nth(1)
            .expect("sample section");
        let data: Vec<&str> = sample
            .lines()
            .filter(|l| l.contains("MissingRight") || l.contains("Modified"))
            .collect();
        assert_eq!(data.len(), 4, "{text}");
        assert!(
            data.iter().any(|l| l.contains("MissingRight")),
            "quota row present: {data:?}"
        );
        assert!(
            data.iter().any(|l| l.contains("Modified")),
            "Modified present: {data:?}"
        );
    }

    #[test]
    fn table_stdout_default_mode_label_diverse() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--sample",
            "2",
        ]);
        let text =
            String::from_utf8(render_stdout(&args, &report_with_n_diffs(5)).unwrap()).unwrap();
        assert!(text.contains("sample diffs (2 of 5) [diverse]"), "{text}");
    }

    #[test]
    fn table_stdout_respects_sample() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--sample",
            "2",
        ]);
        let text =
            String::from_utf8(render_stdout(&args, &report_with_n_diffs(5)).unwrap()).unwrap();
        assert!(text.contains("sample diffs (2 of 5)"), "{text}");
        assert!(!text.contains("String("), "{text}");
    }

    #[test]
    fn summary_only_omits_terminal_details_but_json_still_full() {
        let table = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--summary-only",
        ]);
        let text =
            String::from_utf8(render_stdout(&table, &report_with_n_diffs(5)).unwrap()).unwrap();
        assert!(!text.contains("sample diffs"), "{text}");

        let json = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--format",
            "json",
            "--summary-only",
            "--sample",
            "1",
        ]);
        let buf = render_stdout(&json, &report_with_n_diffs(5)).unwrap();
        let v: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["sample_diffs"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn csv_stdout_respects_sample() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--format",
            "csv",
            "--sample",
            "2",
        ]);
        let text =
            String::from_utf8(render_stdout(&args, &report_with_n_diffs(5)).unwrap()).unwrap();
        let sample = text.split("sample diffs:\n").nth(1).unwrap_or("");
        let data_lines = sample
            .lines()
            .filter(|l| l.contains("MissingRight"))
            .count();
        assert_eq!(
            data_lines, 2,
            "csv stdout should honor --sample, got:\n{text}"
        );
    }

    #[test]
    fn export_csv_writes_all_rows_when_summary_only() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("hepta-dd-export-{}.csv", std::process::id()));
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--summary-only",
            "--sample",
            "1",
            "--export",
            path.to_str().unwrap(),
        ]);
        write_export(&args, &report_with_n_diffs(5)).unwrap();
        let csv = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(csv.lines().count(), 6);
    }
}
