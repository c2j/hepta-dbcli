use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ErrorData as McpError},
    tool, tool_handler, tool_router, ServerHandler,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::backend::factory::BackendRegistry;
use crate::backend::{BackendFactory, DbConn, DbPool};

pub(crate) fn format_error_chain(err: &dyn std::error::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(e) = source {
        parts.push(e.to_string());
        source = e.source();
    }
    parts.join(" | caused by: ")
}

pub(crate) fn redact_url(url: &str) -> String {
    if let Some(at_pos) = url.find('@') {
        if let Some(colon_pos) = url[..at_pos].rfind(':') {
            let prefix = &url[..colon_pos + 1];
            let suffix = &url[at_pos..];
            return format!("{}****{}", prefix, suffix);
        }
    }
    url.to_string()
}

fn connection_error(url: &str, err: &str) -> McpError {
    let redacted = redact_url(url);
    error!("database connection failed: {} (target: {})", err, redacted);
    McpError::internal_error(
        format!("Database connection failed: {}", err),
        Some(json!({
            "target": redacted,
            "hints": [
                "Check if the database server is running",
                "Verify host, port, user, and password in the connection string",
                "Ensure network connectivity and firewall rules allow the connection",
                "Check if SSL/TLS is required (use ssl-mode=REQUIRED in URL)",
            ]
        })),
    )
}

fn query_error(tool: &str, sql: &str, err: &str) -> McpError {
    let sql_preview = if sql.len() > 200 {
        format!("{}...", &sql[..200])
    } else {
        sql.to_string()
    };
    error!("{} failed: {} (sql: {})", tool, err, sql_preview);
    McpError::internal_error(
        format!("{} failed: {}", tool, err),
        Some(json!({ "sql": sql_preview })),
    )
}

fn col_str(row: &[Value], idx: usize) -> Option<String> {
    row.get(idx).and_then(|v| {
        if v.is_null() {
            None
        } else if let Some(s) = v.as_str() {
            Some(s.to_string())
        } else {
            Some(v.to_string())
        }
    })
}

fn col_u64(row: &[Value], idx: usize) -> Option<u64> {
    row.get(idx).and_then(|v| v.as_u64())
}

fn col_i32(row: &[Value], idx: usize) -> Option<i32> {
    row.get(idx).and_then(|v| v.as_i64().map(|n| n as i32))
}

fn col_bool(row: &[Value], idx: usize) -> Option<bool> {
    row.get(idx).and_then(|v| v.as_bool())
}

// ─── MCP Parameter Structs ──────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ConnectionNameParams {
    #[serde(default)]
    pub connection_name: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetTableMetadataParams {
    pub table_name: String,
    pub schema_name: Option<String>,
    #[serde(default)]
    pub connection_name: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExecuteQueryParams {
    pub sql: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_rows: Option<usize>,
    #[serde(default)]
    pub connection_name: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetExecutionPlanParams {
    pub sql: String,
    pub analyze: Option<bool>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub connection_name: Option<String>,
}

/// delta_diff 工具参数（§13.1）
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DeltaDiffParams {
    /// 左数据源连接名
    pub left_connection: String,
    /// 右数据源连接名
    pub right_connection: String,
    /// 表名（左右相同）
    pub table: String,
    /// 左表名（与右表不同名时使用）
    pub left_table: Option<String>,
    /// 右表名
    pub right_table: Option<String>,
    /// Schema（覆盖连接默认库；GaussDB/Oracle 为会话 current_schema）
    pub schema: Option<String>,
    pub left_schema: Option<String>,
    pub right_schema: Option<String>,
    /// 比对键列（自动发现失败时指定）
    pub key_columns: Option<Vec<String>>,
    /// 要比对的列（默认全部可比列）
    pub columns: Option<Vec<String>>,
    /// WHERE 条件（两侧同时应用；禁止分号）
    pub where_condition: Option<String>,
    /// 策略：auto | hashdiff | joindiff | bucketdiff | iblt
    pub strategy: Option<String>,
    /// 一致性模式：snapshot | none
    pub consistency: Option<String>,
    /// 差异行二次复核（snapshot 默认开启）
    pub recheck: Option<bool>,
    /// 差异行采样上限
    pub sample_limit: Option<usize>,
    /// 仅输出统计
    pub summary_only: Option<bool>,
}

// ─── Connection State ───────────────────────────────────────────────

type ResolveFn = Arc<dyn (Fn() -> Result<String, String>) + Send + Sync>;

struct ActiveConnection {
    pool: Arc<dyn DbPool>,
    url: String,
    connected_at: Instant,
}

enum ConnectionState {
    Pending(ResolveFn),
    Connecting { url: String },
    Connected(ActiveConnection),
    Unavailable(String),
}

// ─── DbMcp ──────────────────────────────────────────────────────────

pub struct DbMcp {
    registry: Arc<BackendRegistry>,
    connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    default_name: String,
}

impl DbMcp {
    pub fn new(
        registry: Arc<BackendRegistry>,
        entries: Vec<(String, Option<String>)>,
        default_name: String,
    ) -> Self {
        let mut connections = HashMap::new();
        for (name, url_opt) in entries {
            if let Some(url) = url_opt {
                connections.insert(name, ConnectionState::Connecting { url });
            }
        }
        Self {
            registry,
            connections: Arc::new(Mutex::new(connections)),
            default_name,
        }
    }

    pub fn new_with_lazy(
        registry: Arc<BackendRegistry>,
        eager: Vec<(String, String)>,
        lazy: Vec<(String, ResolveFn)>,
        default_name: String,
    ) -> Self {
        let mut connections = HashMap::new();
        for (name, url) in eager {
            connections.insert(name, ConnectionState::Connecting { url });
        }
        for (name, resolver) in lazy {
            connections.insert(name, ConnectionState::Pending(resolver));
        }
        Self {
            registry,
            connections: Arc::new(Mutex::new(connections)),
            default_name,
        }
    }

    pub fn new_empty(registry: Arc<BackendRegistry>, default_name: String) -> Self {
        Self {
            registry,
            connections: Arc::new(Mutex::new(HashMap::new())),
            default_name,
        }
    }

    pub async fn try_connect(&self) {
        let (name, url) = {
            let conns = self.connections.lock().await;
            match conns.get(&self.default_name) {
                Some(ConnectionState::Connecting { url }) => {
                    (self.default_name.clone(), url.clone())
                }
                _ => return,
            }
        };

        info!("probing database connection '{}' at startup", name);
        match self.connect_with_url(&name, &url).await {
            Ok(_) => {
                info!("startup probe: database '{}' connected successfully", name);
            }
            Err(e) => {
                let redacted = redact_url(&url);
                error!(
                    "startup probe: database '{}' connection failed: {} (target: {})",
                    name, e, redacted
                );
                let mut conns = self.connections.lock().await;
                conns.insert(name, ConnectionState::Unavailable(url));
            }
        }
    }

    fn resolve_factory(&self, url: &str) -> Option<&Arc<dyn BackendFactory>> {
        let scheme = url.find("://").map(|i| &url[..i]).unwrap_or("mysql");
        self.registry.get_by_scheme(scheme)
    }

    async fn get_connection(
        &self,
        connection_name: Option<&str>,
    ) -> Result<(Arc<dyn DbPool>, Box<dyn DbConn + Send>), McpError> {
        let name = connection_name.unwrap_or(&self.default_name).to_string();

        let (url, should_connect) = {
            let conns = self.connections.lock().await;
            match conns.get(&name) {
                Some(ConnectionState::Connected(active)) => {
                    let pool = Arc::clone(&active.pool);
                    let url = active.url.clone();
                    drop(conns);

                    return match pool.acquire().await {
                        Ok(conn) => Ok((pool, conn)),
                        Err(e) => {
                            error!("failed to get connection from pool for '{}': {}", name, e);
                            Err(connection_error(&url, &e.to_string()))
                        }
                    };
                }
                Some(ConnectionState::Pending(resolver)) => {
                    let resolver = Arc::clone(resolver);
                    drop(conns);
                    let url = resolver().map_err(|e| {
                        McpError::internal_error(
                            format!(
                                "Failed to resolve database credentials for '{}': {}",
                                name, e
                            ),
                            Some(json!({
                                "connection_name": name,
                                "hint": "Check your hepta_dbcli configuration and OS keychain access"
                            })),
                        )
                    })?;
                    info!(
                        "connection URL resolved for '{}', attempting database connection",
                        name
                    );
                    (url, true)
                }
                Some(ConnectionState::Connecting { url })
                | Some(ConnectionState::Unavailable(url)) => (url.clone(), true),
                None => {
                    let available: Vec<&String> = conns.keys().collect();
                    return Err(McpError::invalid_request(
                        "unknown_connection",
                        Some(json!({
                            "message": format!("Connection '{}' not found", name),
                            "available_connections": available,
                            "default_connection": self.default_name,
                        })),
                    ));
                }
            }
        };

        if should_connect {
            info!("attempting database connection for '{}'", name);
            self.connect_with_url(&name, &url).await
        } else {
            Err(McpError::internal_error(
                format!("Connection '{}' is in an unexpected state", name),
                None,
            ))
        }
    }

    async fn connect_with_url(
        &self,
        name: &str,
        url: &str,
    ) -> Result<(Arc<dyn DbPool>, Box<dyn DbConn + Send>), McpError> {
        let scheme = url.find("://").map(|i| &url[..i]).unwrap_or("mysql");
        let pool = self
            .registry
            .connect_with_fallback(scheme, url, None)
            .await
            .map_err(|e| connection_error(url, &e))?;

        let conn = pool.acquire().await.map_err(|e| {
            let chain = format_error_chain(&e);
            connection_error(url, &chain)
        })?;

        info!("database '{}' connected successfully", name);

        let active = ActiveConnection {
            pool: Arc::clone(&pool),
            url: url.to_string(),
            connected_at: Instant::now(),
        };

        let mut conns = self.connections.lock().await;
        conns.insert(name.to_string(), ConnectionState::Connected(active));

        Ok((pool, conn))
    }
}

// ─── Tool Implementations ───────────────────────────────────────────

#[tool_router]
impl DbMcp {
    #[tool(description = "Get database version and server information")]
    async fn get_database_info(
        &self,
        Parameters(params): Parameters<ConnectionNameParams>,
    ) -> Result<CallToolResult, McpError> {
        info!(
            "tool called: get_database_info connection={}",
            params.connection_name.as_deref().unwrap_or("(default)")
        );
        let (_pool, mut conn) = self
            .get_connection(params.connection_name.as_deref())
            .await?;

        let sql = { conn.dialect().database_info().to_string() };
        let result = conn
            .query(&sql)
            .await
            .map_err(|e| query_error("get_database_info", &sql, &e.to_string()))?;

        if result.rows.is_empty() {
            return Err(McpError::internal_error(
                "get_database_info returned no rows",
                None,
            ));
        }

        let row = &result.rows[0];
        let output = json!({
            "version": col_str(row, 0),
            "database": col_str(row, 1),
            "current_user": col_str(row, 2),
            "hostname": col_str(row, 3),
            "port": col_i32(row, 4),
            "os": col_str(row, 5),
            "charset": col_str(row, 6),
            "collation": col_str(row, 7),
            "version_comment": col_str(row, 8),
        });

        Ok(CallToolResult::success(vec![Content::text(
            output.to_string(),
        )]))
    }

    #[tool(description = "List all user tables and views in the database")]
    async fn list_tables(
        &self,
        Parameters(params): Parameters<ConnectionNameParams>,
    ) -> Result<CallToolResult, McpError> {
        info!(
            "tool called: list_tables connection={}",
            params.connection_name.as_deref().unwrap_or("(default)")
        );
        let (_pool, mut conn) = self
            .get_connection(params.connection_name.as_deref())
            .await?;

        let sql = { conn.dialect().list_tables().to_string() };
        let result = conn
            .query(&sql)
            .await
            .map_err(|e| query_error("list_tables", &sql, &e.to_string()))?;

        let tables: Vec<serde_json::Value> = result
            .rows
            .iter()
            .map(|row| {
                json!({
                    "schema_name": col_str(row, 0),
                    "table_name": col_str(row, 1),
                    "table_type": col_str(row, 2),
                    "engine": col_str(row, 3),
                    "row_count": col_u64(row, 4),
                    "total_size": col_u64(row, 5),
                    "comment": col_str(row, 6),
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            json!(tables).to_string(),
        )]))
    }

    #[tool(description = "Get column metadata, primary keys, and indexes for a specific table")]
    async fn get_table_metadata(
        &self,
        Parameters(params): Parameters<GetTableMetadataParams>,
    ) -> Result<CallToolResult, McpError> {
        let schema = params.schema_name.as_deref().unwrap_or("public");
        let table = &params.table_name;
        info!(
            "tool called: get_table_metadata schema={} table={} connection={}",
            schema,
            table,
            params.connection_name.as_deref().unwrap_or("(default)")
        );
        let (_pool, mut conn) = self
            .get_connection(params.connection_name.as_deref())
            .await?;

        let sql = { conn.dialect().table_columns().to_string() };
        let col_result = conn
            .exec(
                &sql,
                &[
                    Value::String(schema.to_string()),
                    Value::String(table.clone()),
                ],
            )
            .await
            .map_err(|e| query_error("get_table_metadata (columns)", &sql, &e.to_string()))?;

        let columns: Vec<serde_json::Value> = col_result
            .rows
            .iter()
            .map(|row| {
                json!({
                    "column_name": col_str(row, 0),
                    "data_type": col_str(row, 1),
                    "nullable": col_bool(row, 2),
                    "default_value": col_str(row, 3),
                    "ordinal_position": col_i32(row, 4),
                    "comment": col_str(row, 5),
                    "column_key": col_str(row, 6),
                })
            })
            .collect();

        let idx_sql = { conn.dialect().table_indexes().to_string() };
        let idx_result = conn
            .exec(
                &idx_sql,
                &[
                    Value::String(schema.to_string()),
                    Value::String(table.clone()),
                ],
            )
            .await
            .map_err(|e| query_error("get_table_metadata (indexes)", &idx_sql, &e.to_string()))?;

        let indexes: Vec<serde_json::Value> = idx_result
            .rows
            .iter()
            .map(|row| {
                json!({
                    "index_name": col_str(row, 0),
                    "is_unique": col_bool(row, 1),
                    "is_primary": col_bool(row, 2),
                    "columns": col_str(row, 3),
                    "index_type": col_str(row, 4),
                })
            })
            .collect();

        let result = json!({ "columns": columns, "indexes": indexes });
        Ok(CallToolResult::success(vec![Content::text(
            result.to_string(),
        )]))
    }

    #[tool(description = "Execute a read-only SQL query (SELECT or EXPLAIN only)")]
    async fn execute_query(
        &self,
        Parameters(params): Parameters<ExecuteQueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let trimmed = params.sql.trim();
        debug!(
            "tool called: execute_query sql_len={} connection={}",
            trimmed.len(),
            params.connection_name.as_deref().unwrap_or("(default)")
        );

        let (_pool, mut conn) = self
            .get_connection(params.connection_name.as_deref())
            .await?;

        let read_only_prefixes = conn.dialect().read_only_prefixes();
        if !crate::cli::is_read_only_mcp(trimmed, read_only_prefixes) {
            error!(
                "execute_query rejected non-SELECT query: {:?}",
                &trimmed[..trimmed.len().min(80)]
            );
            return Err(McpError::invalid_request(
                "invalid_query",
                Some(json!({
                    "message": format!(
                        "Only {} queries are allowed",
                        read_only_prefixes.join(", ")
                    )
                })),
            ));
        }

        if let Some(timeout_ms) = params.timeout_ms {
            if let Some(set_sql) = conn.dialect().set_statement_timeout_sql(timeout_ms) {
                let _ = conn.query_drop(&set_sql).await;
            }
        }

        let max_rows = params.max_rows.unwrap_or(1000).clamp(1, 10000);
        let sql_to_execute = conn.dialect().add_limit(trimmed, max_rows);

        let result = conn
            .query(&sql_to_execute)
            .await
            .map_err(|e| query_error("execute_query", trimmed, &e.to_string()))?;

        if result.rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                json!({"columns": [], "rows": [], "row_count": 0}).to_string(),
            )]));
        }

        let total_count = result.row_count;
        let truncated = total_count > max_rows;
        let visible = if truncated {
            &result.rows[..max_rows]
        } else {
            &result.rows[..]
        };

        let mut output = json!({
            "columns": result.columns,
            "rows": visible,
            "row_count": total_count,
        });
        if truncated {
            output["truncated"] = json!(true);
            output["hint"] = json!(format!(
                "Result exceeds max_rows ({max_rows}). Use CLI mode for full output.",
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(
            output.to_string(),
        )]))
    }

    #[tool(description = "Get the execution plan for a SQL query")]
    async fn get_execution_plan(
        &self,
        Parameters(params): Parameters<GetExecutionPlanParams>,
    ) -> Result<CallToolResult, McpError> {
        info!(
            "tool called: get_execution_plan analyze={} connection={}",
            params.analyze.unwrap_or(false),
            params.connection_name.as_deref().unwrap_or("(default)")
        );

        let (_pool, mut conn) = self
            .get_connection(params.connection_name.as_deref())
            .await?;

        if let Some(timeout_ms) = params.timeout_ms {
            if let Some(set_sql) = conn.dialect().set_statement_timeout_sql(timeout_ms) {
                let _ = conn.query_drop(&set_sql).await;
            }
        }

        let analyze = params.analyze.unwrap_or(false);
        let fmt = params.format.as_deref().unwrap_or("TEXT");
        let explain_sql = conn.dialect().build_explain(&params.sql, analyze, fmt);

        let result = conn
            .query(&explain_sql)
            .await
            .map_err(|e| query_error("get_execution_plan", &explain_sql, &e.to_string()))?;

        let plan: String = result
            .rows
            .iter()
            .filter_map(|row| {
                row.first()
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .or_else(|| row.first().map(|v| v.to_string()))
            })
            .collect::<Vec<String>>()
            .join("\n");

        let output = json!({ "plan": plan });
        Ok(CallToolResult::success(vec![Content::text(
            output.to_string(),
        )]))
    }

    #[tool(description = "List all configured database connections")]
    async fn list_connections(&self) -> Result<CallToolResult, McpError> {
        info!("tool called: list_connections");
        let conns = self.connections.lock().await;
        let connections: Vec<serde_json::Value> = conns
            .iter()
            .map(|(name, state)| {
                let status = match state {
                    ConnectionState::Connected(_) => "connected",
                    ConnectionState::Connecting { .. } => "connecting",
                    ConnectionState::Pending(_) => "pending",
                    ConnectionState::Unavailable(_) => "unavailable",
                };
                json!({
                    "name": name,
                    "status": status,
                    "is_default": name == &self.default_name,
                })
            })
            .collect();

        let result = json!({
            "connections": connections,
            "default_connection": self.default_name,
        });

        Ok(CallToolResult::success(vec![Content::text(
            result.to_string(),
        )]))
    }
    #[tool(
        description = "Compare two database tables and identify data differences. Read-only. Default snapshot consistency with diff recheck."
    )]
    async fn delta_diff(
        &self,
        Parameters(params): Parameters<DeltaDiffParams>,
    ) -> Result<CallToolResult, McpError> {
        info!(
            "tool called: delta_diff left={} right={} table={}",
            params.left_connection, params.right_connection, params.table
        );

        if let Some(w) = &params.where_condition {
            if w.contains(';') {
                return Err(McpError::invalid_request(
                    "where_condition must not contain ';'",
                    None,
                ));
            }
        }
        let strategy = match parse_delta_diff_strategy(params.strategy.as_deref()) {
            Ok(s) => s,
            Err(e) => {
                return Err(McpError::invalid_request(e, None));
            }
        };
        let snapshot = !matches!(params.consistency.as_deref(), Some("none"));
        let recheck = params.recheck.unwrap_or(snapshot);

        let (lpool, lconn) = self.get_connection(Some(&params.left_connection)).await?;
        let (rpool, rconn) = self.get_connection(Some(&params.right_connection)).await?;
        let lurl = self.connection_url_of(&params.left_connection).await;
        let rurl = self.connection_url_of(&params.right_connection).await;

        let left = crate::delta_diff::api::SideInput {
            pool: lpool,
            conn: lconn,
            name: params.left_connection.clone(),
            schema: params.left_schema.clone().or_else(|| params.schema.clone()),
            table: params
                .left_table
                .clone()
                .unwrap_or_else(|| params.table.clone()),
            connection_url: lurl,
        };
        let right = crate::delta_diff::api::SideInput {
            pool: rpool,
            conn: rconn,
            name: params.right_connection.clone(),
            schema: params
                .right_schema
                .clone()
                .or_else(|| params.schema.clone()),
            table: params
                .right_table
                .clone()
                .unwrap_or_else(|| params.table.clone()),
            connection_url: rurl,
        };
        let opts = crate::delta_diff::api::DiffOptions {
            iblt_capacity: 65536,
            fetch_all_threshold: 4096,
            strict: false,
            strategy,
            key: params.key_columns.clone().unwrap_or_default(),
            columns: params.columns.clone().unwrap_or_default(),
            filter: params.where_condition.clone(),
            incremental: None,
            bisection_factor: 32,
            bisection_threshold: 16384,
            sample_limit: params.sample_limit.unwrap_or(1000),
            threads: 4,
            snapshot,
            recheck,
            checkpoint: None,
            verbose: false,
        };

        match crate::delta_diff::api::run_diff(left, right, opts).await {
            Ok(report) => {
                let text = serde_json::to_string_pretty(&report)
                    .unwrap_or_else(|e| format!("{{\"error\":\"json serialize: {e}\"}}"));
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    /// 读取已建立连接的 URL（get_connection 保证状态为 Connected）。
    async fn connection_url_of(&self, name: &str) -> String {
        let conns = self.connections.lock().await;
        match conns.get(name) {
            Some(ConnectionState::Connected(active)) => active.url.clone(),
            _ => String::new(),
        }
    }
}

#[tool_handler(
    name = "hepta_dbcli",
    version = "0.2.8",
    instructions = "MCP server for MySQL/PolarDB-X/Oracle/GaussDB database introspection with multi-connection support"
)]
impl ServerHandler for DbMcp {}

fn parse_delta_diff_strategy(
    raw: Option<&str>,
) -> Result<Option<crate::delta_diff::cmd::Strategy>, String> {
    match raw {
        None | Some("auto") => Ok(None),
        Some("hashdiff") => Ok(Some(crate::delta_diff::cmd::Strategy::Hashdiff)),
        Some("joindiff") => Ok(Some(crate::delta_diff::cmd::Strategy::Joindiff)),
        Some("bucketdiff") => Ok(Some(crate::delta_diff::cmd::Strategy::Bucketdiff)),
        Some("iblt") => Ok(Some(crate::delta_diff::cmd::Strategy::Iblt)),
        Some("keyeddiff") => Ok(Some(crate::delta_diff::cmd::Strategy::Keyeddiff)),
        Some(other) => Err(format!("unknown strategy '{other}'")),
    }
}

#[cfg(test)]
mod delta_diff_strategy_tests {
    use super::parse_delta_diff_strategy;
    use crate::delta_diff::cmd::Strategy;

    #[test]
    fn parses_keyeddiff() {
        assert_eq!(
            parse_delta_diff_strategy(Some("keyeddiff")).unwrap(),
            Some(Strategy::Keyeddiff)
        );
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse_delta_diff_strategy(Some("magic")).is_err());
    }
}
