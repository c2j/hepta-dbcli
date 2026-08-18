// ─── delta-diff api: CLI/MCP 共用的比对执行入口（§四/§13.1）─────────────
//
// run_diff 接受已就绪的连接与选项，完成：schema 解析 → 计划构建 → 路由 →
// 策略执行 → 断点 finalize。CLI（mod.rs）与 MCP（server.rs delta_diff）共用。

use std::sync::Arc;

use crate::backend::{DbConn, DbPool};
use crate::delta_diff::report::DiffReport;
use crate::delta_diff::{engine, metadata, strategy};
use crate::delta_diff::{progress, side_schema_from_conn};

pub(crate) struct SideInput {
    pub(crate) pool: Arc<dyn DbPool>,
    pub(crate) conn: Box<dyn DbConn + Send>,
    pub(crate) name: String,
    pub(crate) schema: Option<String>,
    pub(crate) table: String,
    pub(crate) connection_url: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DiffOptions {
    pub(crate) strategy: Option<crate::delta_diff::cmd::Strategy>,
    pub(crate) iblt_capacity: u64,
    pub(crate) strict: bool,
    pub(crate) key: Vec<String>,
    pub(crate) columns: Vec<String>,
    pub(crate) filter: Option<String>,
    pub(crate) incremental: Option<(String, String)>,
    pub(crate) bisection_factor: usize,
    pub(crate) bisection_threshold: u64,
    pub(crate) sample_limit: usize,
    pub(crate) threads: usize,
    pub(crate) snapshot: bool,
    pub(crate) recheck: bool,
    pub(crate) checkpoint: Option<String>,
}

pub(crate) async fn run_diff(
    mut left: SideInput,
    mut right: SideInput,
    opts: DiffOptions,
) -> Result<DiffReport, String> {
    let lschema = resolve_schema(
        left.schema.as_deref(),
        &mut *left.conn,
        &left.connection_url,
        &left.name,
    )
    .await?;
    let rschema = resolve_schema(
        right.schema.as_deref(),
        &mut *right.conn,
        &right.connection_url,
        &right.name,
    )
    .await?;

    let lplan = metadata::build_table_plan(
        &mut *left.conn,
        &lschema,
        &left.table,
        &opts.columns,
        &opts.key,
    )
    .await
    .map_err(|e| format!("left plan: {}", e))?;
    let rplan = metadata::build_table_plan(
        &mut *right.conn,
        &rschema,
        &right.table,
        &opts.columns,
        &opts.key,
    )
    .await
    .map_err(|e| format!("right plan: {}", e))?;

    // MySQL 会话时区固定为 UTC（§九 TIMESTAMP 规范化前提；评审修复）
    for conn in [&mut *left.conn, &mut *right.conn] {
        if conn.dialect().url_scheme() == "mysql" {
            conn.query_drop("SET time_zone = '+00:00'")
                .await
                .map_err(|e| format!("session time_zone pin failed: {}", e))?;
        }
    }

    // 跨库规范化告警（评审修复）
    let cross_warnings = cross_db_warnings(&left, &right, &lplan, &rplan);

    let routed = engine::route_plan(
        &lplan,
        &rplan,
        &left.connection_url,
        &right.connection_url,
        opts.strategy,
    )?;

    let checkpoint = match &opts.checkpoint {
        Some(path) => Some(Arc::new(tokio::sync::Mutex::new(
            progress::CheckpointManager::open(path).map_err(|e| e.to_string())?,
        ))),
        None => None,
    };

    let ctx = strategy::DiffContext {
        left: strategy::SideCtx {
            connection_name: left.name.clone(),
            schema: Some(lschema),
            table: left.table.clone(),
            plan: lplan,
        },
        right: strategy::SideCtx {
            connection_name: right.name.clone(),
            schema: Some(rschema),
            table: right.table.clone(),
            plan: rplan,
        },
        left_pool: Arc::clone(&left.pool),
        right_pool: Arc::clone(&right.pool),
        key_column: routed.key_column,
        filter: opts.filter.clone(),
        incremental: opts.incremental.clone(),
        bisection_factor: opts.bisection_factor.max(2),
        bisection_threshold: opts.bisection_threshold.max(1),
        sample_limit: opts.sample_limit,
        threads: opts.threads.max(1),
        consistency: if opts.snapshot {
            strategy::ConsistencyMode::Snapshot
        } else {
            strategy::ConsistencyMode::None
        },
        recheck: opts.recheck,
        route_warnings: {
            let mut w = routed.warnings;
            w.extend(cross_warnings);
            w
        },
        checkpoint,
        iblt_capacity: opts.iblt_capacity.max(16),
        strict: opts.strict,
        scns: std::sync::OnceLock::new(),
    };

    let report = routed
        .strategy
        .diff(&mut *left.conn, &mut *right.conn, &ctx)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(path) = &opts.checkpoint {
        progress::finalize_path(path).map_err(|e| e.to_string())?;
    }
    Ok(report)
}

async fn resolve_schema(
    explicit: Option<&str>,
    conn: &mut (dyn DbConn + Send),
    url: &str,
    name: &str,
) -> Result<String, String> {
    if let Some(s) = explicit {
        return Ok(s.to_string());
    }
    side_schema_from_conn(conn, url, name).await
}

/// 跨库规范化告警（评审修复）：
/// - 浮点列跨库文本表示不可移植（MySQL `1e30` vs PG `1e+30`）；
/// - DATE ↔ DATETIME/TIMESTAMP 同名配对会静默截断时间分量（假阴性）。
fn cross_db_warnings(
    left: &SideInput,
    right: &SideInput,
    lplan: &metadata::TablePlan,
    rplan: &metadata::TablePlan,
) -> Vec<String> {
    let mut out = Vec::new();
    let lscheme = left.conn.dialect().url_scheme();
    let rscheme = right.conn.dialect().url_scheme();
    if lscheme == rscheme {
        return out;
    }

    let type_base = |t: &str| {
        t.split('(')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase()
            .to_string()
    };
    let float_types = [
        "float",
        "double",
        "real",
        "float4",
        "float8",
        "binary_float",
        "binary_double",
    ];
    let date_only = ["date"];
    let date_time = [
        "datetime",
        "timestamp",
        "timestamp without time zone",
        "timestamp with time zone",
        "timestamptz",
    ];

    let ltypes: std::collections::HashMap<&str, &str> = lplan
        .norm_specs
        .iter()
        .map(|s| (s.name.as_str(), s.data_type.as_str()))
        .collect();
    for spec in &rplan.norm_specs {
        let base = type_base(&spec.data_type);
        let lty = ltypes.get(spec.name.as_str()).map(|t| type_base(t));
        if float_types.contains(&base.as_str())
            || lty
                .as_deref()
                .map(|t| float_types.contains(&t))
                .unwrap_or(false)
        {
            out.push(format!(
                "column '{}' is float-typed; cross-database text representation is not portable — \
                 results may show false differences (v2.1 §九)",
                spec.name
            ));
        }
        let date_mismatch = lty
            .map(|lt| {
                (date_only.contains(&lt.as_str()) && date_time.contains(&base.as_str()))
                    || (date_time.contains(&lt.as_str()) && date_only.contains(&base.as_str()))
            })
            .unwrap_or(false);
        if date_mismatch {
            out.push(format!(
                "column '{}' pairs DATE with DATETIME/TIMESTAMP across sides — \
                 time-of-day is truncated on the DATE side; time-only differences are invisible",
                spec.name
            ));
        }
    }
    out.dedup();
    out
}
