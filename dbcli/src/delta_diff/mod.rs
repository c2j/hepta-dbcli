// ─── delta-diff: table data comparison between two connections ────────
//
// CLI entry per design doc §二/§四: argument parsing (cmd.rs),
// connection-name resolution and the exit-code contract (§2.3).

use std::path::PathBuf;

use crate::config;

pub(crate) mod api;
pub(crate) mod bucket_diff;
pub(crate) mod checksum;
pub(crate) mod cmd;
pub(crate) mod engine;
pub(crate) mod hash_diff;
pub(crate) mod iblt_diff;
pub(crate) mod join_diff;
pub(crate) mod keyed_diff;
pub(crate) mod metadata;
pub(crate) mod output;
pub(crate) mod progress;
pub(crate) mod recheck;
pub(crate) mod report;
pub(crate) mod rowdiff;
pub(crate) mod strategy;

// ─── Exit codes (CI/CD contract, §2.3) ─────────────────────────────────

pub(crate) const EXIT_IDENTICAL: i32 = 0;
pub(crate) const EXIT_DIFF: i32 = 1;
pub(crate) const EXIT_ERROR: i32 = 2;

// ─── Entry Point ───────────────────────────────────────────────────────

pub(crate) async fn run(args: cmd::DeltaDiffArgs, config_path: Option<String>) -> i32 {
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

    if args.dry_run {
        return dry_run_enhanced(&args, &left, &right).await;
    }

    execute_diff(&args, &left, &right).await
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
    )
    .await
    .map_err(|e| format!("left plan: {}", e))?;
    let rplan = metadata::build_table_plan(
        &mut *rconn,
        &rschema,
        rtable,
        &args.columns_list(),
        &args.key_list(),
    )
    .await
    .map_err(|e| format!("right plan: {}", e))?;

    let routed = engine::route(args, left, right, &lplan, &rplan)?;

    let (lminmax, rminmax) = if routed.key_columns.len() == 1
        && matches!(routed.strategy.name(), "hashdiff" | "iblt" | "joindiff")
    {
        (
            min_max(&mut *lconn, &lschema, ltable, &routed.key_column)
                .await
                .ok(),
            min_max(&mut *rconn, &rschema, rtable, &routed.key_column)
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
    if !routed.warnings.is_empty() {
        out.push_str(&format!(
            "\n  route warnings   : {}",
            routed.warnings.join("; ")
        ));
    }
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
    match (lminmax, rminmax) {
        (Some(l), Some(r)) => {
            let lo = l.0.min(r.0);
            let hi = l.1.max(r.1);
            let segments = args.threads * 8;
            out.push_str(&format!(
                "\n  key domain       : [{}, {}]\n  first-pass segs  : {} (threads×8)\n  est. queries     : ≈{} (checksum) + bisect on diff segs",
                lo, hi, segments, segments * 2
            ));
        }
        _ => {
            out.push_str("\n  key domain       : (not applicable — bucketdiff)");
        }
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
    let q = conn.dialect().identifier_quote();
    let sql =
        format!("SELECT MIN({q}{key}{q}), MAX({q}{key}{q}) FROM {q}{schema}{q}.{q}{table}{q}");
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

    let lplan = metadata::build_table_plan(
        &mut *lconn,
        &lschema,
        ltable,
        &args.columns_list(),
        &args.key_list(),
    )
    .await
    .map_err(|e| format!("left plan: {}", e))?;
    let rplan = metadata::build_table_plan(
        &mut *rconn,
        &rschema,
        rtable,
        &args.columns_list(),
        &args.key_list(),
    )
    .await
    .map_err(|e| format!("right plan: {}", e))?;

    let routed = engine::route(args, left, right, &lplan, &rplan)?;

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
        route_warnings: routed.warnings,
        checkpoint,
        iblt_capacity: args.iblt_capacity,
        fetch_all_threshold: args.fetch_all_threshold,
        strict: args.strict,
        scns: std::sync::OnceLock::new(),
        verbose: args.verbose,
    };

    let result = routed
        .strategy
        .diff(&mut *lconn, &mut *rconn, &ctx)
        .await
        .map_err(|e| e.to_string());

    if result.is_ok() {
        if let Some(path) = &args.checkpoint {
            progress::finalize_path(path).map_err(|e| e.to_string())?;
        }
    }
    result
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
        .connect_with_fallback(scheme, &side.connection_url, Some(&side.timeout_config))
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
    let mut buf: Vec<u8> = Vec::new();
    match args.format {
        crate::cli::OutputFormat::Json => {
            let s = serde_json::to_string_pretty(report)
                .map_err(|e| format!("json serialize: {}", e))?;
            buf.extend_from_slice(s.as_bytes());
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
                let diffs = output::diffs_to_query_result(report);
                crate::cli::render_result(&diffs, &mut buf, fmt).map_err(|e| e.to_string())?;
            }
        }
    }
    match &args.output {
        Some(path) => std::fs::write(path, &buf).map_err(|e| e.to_string()),
        None => {
            print!("{}", String::from_utf8_lossy(&buf));
            Ok(())
        }
    }
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

#[cfg(test)]
mod dry_run_format_tests {
    use super::format_dry_run_key;

    #[test]
    fn composite_keys_join_with_comma() {
        assert_eq!(format_dry_run_key(&["k1".into(), "k2".into()]), "k1,k2");
    }
}
