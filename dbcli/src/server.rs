use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ErrorData as McpError},
    service::{NotificationContext, RoleServer},
    tool, tool_handler, tool_router, ServerHandler,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::audit::event::{
    read_only_session_for, ActionClass, AuditOutcome, Channel, ConnectionInfo, Decision,
    DraftEvent, SqlInfo,
};
use crate::audit::AuditSession;
use crate::backend::factory::BackendRegistry;
use crate::backend::{BackendFactory, DbConn, DbPool};
use crate::cli::classify_query_error;

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
    /// 策略：auto | hashdiff | joindiff | bucketdiff | iblt | keyeddiff
    pub strategy: Option<String>,
    /// 一致性模式：snapshot | none
    pub consistency: Option<String>,
    /// 差异行二次复核（snapshot 默认开启）
    pub recheck: Option<bool>,
    /// 差异行采样上限
    pub sample_limit: Option<usize>,
    /// 仅输出统计
    pub summary_only: Option<bool>,
    /// 增量比对列（与 where_condition 互斥）
    pub update_column: Option<String>,
    /// 增量窗口，默认 "1 day"；需同时提供 update_column
    pub update_since: Option<String>,
    /// 断点续跑文件路径
    pub checkpoint: Option<String>,
    /// 只读导出路径（csv/jsonl/json；不支持 sql，SQL 补丁仍走 CLI --apply-to）
    pub export: Option<String>,
    /// 导出格式：csv | jsonl | json
    pub export_format: Option<String>,
    /// 导出时附带行内容
    pub export_rows: Option<bool>,
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
    audit: Arc<AuditSession>,
    /// MCP client name reported by `initialize`, when the client sent one.
    client: std::sync::Mutex<Option<String>>,
}

impl DbMcp {
    pub fn new(
        registry: Arc<BackendRegistry>,
        entries: Vec<(String, Option<String>)>,
        default_name: String,
        audit: Arc<AuditSession>,
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
            audit,
            client: std::sync::Mutex::new(None),
        }
    }

    pub fn new_with_lazy(
        registry: Arc<BackendRegistry>,
        eager: Vec<(String, String)>,
        lazy: Vec<(String, ResolveFn)>,
        default_name: String,
        audit: Arc<AuditSession>,
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
            audit,
            client: std::sync::Mutex::new(None),
        }
    }

    pub fn new_empty(
        registry: Arc<BackendRegistry>,
        default_name: String,
        audit: Arc<AuditSession>,
    ) -> Self {
        Self {
            registry,
            connections: Arc::new(Mutex::new(HashMap::new())),
            default_name,
            audit,
            client: std::sync::Mutex::new(None),
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
                            self.record(connect_event(&name, &url, Decision::Error));
                            Err(connection_error(&url, &e.to_string()))
                        }
                    };
                }
                Some(ConnectionState::Pending(resolver)) => {
                    let resolver = Arc::clone(resolver);
                    drop(conns);
                    let url = resolver().map_err(|e| {
                        self.audit
                            .record_best_effort(connect_event(&name, "", Decision::Error));
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
                    self.audit
                        .record_best_effort(connect_event(&name, "", Decision::Error));
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
            .connect_with_fallback(scheme, url, None, false)
            .await
            .map_err(|e| {
                self.audit
                    .record_best_effort(connect_event(name, url, Decision::Error));
                connection_error(url, &e)
            })?;

        let conn = pool.acquire().await.map_err(|e| {
            let chain = format_error_chain(&e);
            self.audit
                .record_best_effort(connect_event(name, url, Decision::Error));
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

        self.audit
            .record_best_effort(connect_event(name, url, Decision::Allow));

        Ok((pool, conn))
    }

    /// Record an audit event, stamped with the MCP client name when the
    /// client reported one (issue #57 §5 `actor.client`).
    fn record(&self, event: DraftEvent) {
        let event = match self.client_name() {
            Some(name) => event.with_client(name),
            None => event,
        };
        self.audit.record_best_effort(event);
    }

    fn client_name(&self) -> Option<String> {
        match self.client.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Record a meta tool event only when `--audit-meta` is on.
    fn record_meta(&self, name: &str, url: &str, action: &str, decision: Decision) {
        if self.audit.meta_enabled() {
            self.audit
                .record_best_effort(meta_event(name, url, action, decision));
        }
    }
}

// ─── Audit event builders (pure, unit-tested without a database) ─────

fn execute_query_denied_event(conn_name: &str, url: &str, sql: &str) -> DraftEvent {
    // The gate rejects anything that is not read-only, so the denied statement
    // is normally DML/DDL; classify it instead of claiming it was a query.
    let class = if crate::cli::is_read_only_query(sql) {
        ActionClass::Dql
    } else {
        ActionClass::Dml
    };
    DraftEvent::new(
        Channel::Mcp,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url)),
        "execute_query",
        class,
        Decision::Deny,
    )
    .with_sql(SqlInfo::new(sql))
    .with_deny_reason("prefix")
}

fn execute_query_allowed_event(
    conn_name: &str,
    url: &str,
    sql: &str,
    duration_ms: u64,
    row_count: u64,
    limit_applied: bool,
) -> DraftEvent {
    DraftEvent::new(
        Channel::Mcp,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url)),
        "execute_query",
        ActionClass::Dql,
        Decision::Allow,
    )
    .with_sql(SqlInfo::new(sql))
    .with_outcome(AuditOutcome::ok(duration_ms).with_row_count(row_count))
    .with_limit_applied(limit_applied)
}

fn execute_query_error_event(
    conn_name: &str,
    url: &str,
    sql: &str,
    duration_ms: u64,
    error_kind: &str,
    sqlstate: Option<&str>,
) -> DraftEvent {
    let mut outcome = AuditOutcome::error(duration_ms, error_kind);
    if let Some(state) = sqlstate {
        outcome = outcome.with_sqlstate(state);
    }
    DraftEvent::new(
        Channel::Mcp,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url)),
        "execute_query",
        ActionClass::Dql,
        Decision::Error,
    )
    .with_sql(SqlInfo::new(sql))
    .with_outcome(outcome)
}

fn get_execution_plan_event(
    conn_name: &str,
    url: &str,
    sql: &str,
    analyze: bool,
    decision: Decision,
) -> DraftEvent {
    DraftEvent::new(
        Channel::Mcp,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url)),
        "get_execution_plan",
        ActionClass::Dql,
        decision,
    )
    .with_sql(SqlInfo::new(sql))
    .with_analyze(analyze)
}

fn connect_event(conn_name: &str, url: &str, decision: Decision) -> DraftEvent {
    DraftEvent::new(
        Channel::Mcp,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url)),
        "connect",
        ActionClass::Admin,
        decision,
    )
}

fn meta_event(conn_name: &str, url: &str, action: &str, decision: Decision) -> DraftEvent {
    DraftEvent::new(
        Channel::Mcp,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url)),
        action,
        ActionClass::Meta,
        decision,
    )
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
        let name = params
            .connection_name
            .as_deref()
            .unwrap_or(&self.default_name)
            .to_string();
        let (_pool, mut conn) = self.get_connection(Some(&name)).await?;
        let url = self.connection_url_of(&name).await;

        let sql = { conn.dialect().database_info().to_string() };
        let result = match conn.query(&sql).await {
            Ok(result) => result,
            Err(e) => {
                self.record_meta(&name, &url, "get_database_info", Decision::Error);
                return Err(query_error("get_database_info", &sql, &e.to_string()));
            }
        };

        if result.rows.is_empty() {
            self.record_meta(&name, &url, "get_database_info", Decision::Error);
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

        self.record_meta(&name, &url, "get_database_info", Decision::Allow);
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
        let name = params
            .connection_name
            .as_deref()
            .unwrap_or(&self.default_name)
            .to_string();
        let (_pool, mut conn) = self.get_connection(Some(&name)).await?;
        let url = self.connection_url_of(&name).await;

        let sql = { conn.dialect().list_tables().to_string() };
        let result = match conn.query(&sql).await {
            Ok(result) => result,
            Err(e) => {
                self.record_meta(&name, &url, "list_tables", Decision::Error);
                return Err(query_error("list_tables", &sql, &e.to_string()));
            }
        };

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

        self.record_meta(&name, &url, "list_tables", Decision::Allow);
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
        let name = params
            .connection_name
            .as_deref()
            .unwrap_or(&self.default_name)
            .to_string();
        let (_pool, mut conn) = self.get_connection(Some(&name)).await?;
        let url = self.connection_url_of(&name).await;

        let sql = { conn.dialect().table_columns().to_string() };
        let col_result = match conn
            .exec(
                &sql,
                &[
                    Value::String(schema.to_string()),
                    Value::String(table.clone()),
                ],
            )
            .await
        {
            Ok(result) => result,
            Err(e) => {
                self.record_meta(&name, &url, "get_table_metadata", Decision::Error);
                return Err(query_error(
                    "get_table_metadata (columns)",
                    &sql,
                    &e.to_string(),
                ));
            }
        };

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
        let idx_result = match conn
            .exec(
                &idx_sql,
                &[
                    Value::String(schema.to_string()),
                    Value::String(table.clone()),
                ],
            )
            .await
        {
            Ok(result) => result,
            Err(e) => {
                self.record_meta(&name, &url, "get_table_metadata", Decision::Error);
                return Err(query_error(
                    "get_table_metadata (indexes)",
                    &idx_sql,
                    &e.to_string(),
                ));
            }
        };

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
        self.record_meta(&name, &url, "get_table_metadata", Decision::Allow);
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

        let name = params
            .connection_name
            .as_deref()
            .unwrap_or(&self.default_name)
            .to_string();
        let (_pool, mut conn) = self.get_connection(Some(&name)).await?;
        let url = self.connection_url_of(&name).await;

        let read_only_prefixes = conn.dialect().read_only_prefixes();
        if !crate::cli::is_read_only_mcp(trimmed, read_only_prefixes) {
            error!(
                "execute_query rejected non-SELECT query: {:?}",
                &trimmed[..trimmed.len().min(80)]
            );
            self.audit
                .record_best_effort(execute_query_denied_event(&name, &url, trimmed));
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
        let limit_applied = sql_to_execute != trimmed;

        let start = Instant::now();
        let result = match conn.query(&sql_to_execute).await {
            Ok(result) => result,
            Err(e) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                let (error_kind, sqlstate) = classify_query_error(&e);
                self.record(execute_query_error_event(
                    &name,
                    &url,
                    trimmed,
                    duration_ms,
                    &error_kind,
                    sqlstate.as_deref(),
                ));
                return Err(query_error("execute_query", trimmed, &e.to_string()));
            }
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        self.record(execute_query_allowed_event(
            &name,
            &url,
            trimmed,
            duration_ms,
            result.row_count as u64,
            limit_applied,
        ));

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

        let name = params
            .connection_name
            .as_deref()
            .unwrap_or(&self.default_name)
            .to_string();
        let (_pool, mut conn) = self.get_connection(Some(&name)).await?;
        let url = self.connection_url_of(&name).await;

        if let Some(timeout_ms) = params.timeout_ms {
            if let Some(set_sql) = conn.dialect().set_statement_timeout_sql(timeout_ms) {
                let _ = conn.query_drop(&set_sql).await;
            }
        }

        let analyze = params.analyze.unwrap_or(false);
        let fmt = params.format.as_deref().unwrap_or("TEXT");
        let explain_sql = conn.dialect().build_explain(&params.sql, analyze, fmt);

        let result = match conn.query(&explain_sql).await {
            Ok(result) => result,
            Err(e) => {
                self.record(get_execution_plan_event(
                    &name,
                    &url,
                    &params.sql,
                    analyze,
                    Decision::Error,
                ));
                return Err(query_error(
                    "get_execution_plan",
                    &explain_sql,
                    &e.to_string(),
                ));
            }
        };

        self.record(get_execution_plan_event(
            &name,
            &url,
            &params.sql,
            analyze,
            Decision::Allow,
        ));

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

        self.record_meta(&self.default_name, "", "list_connections", Decision::Allow);
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

        let opts = match build_mcp_diff_options(&params) {
            Ok(opts) => opts,
            Err(e) => return Err(McpError::invalid_request(e, None)),
        };
        let export_plan = match parse_mcp_export_format(
            params.export.as_deref(),
            params.export_format.as_deref(),
        ) {
            Ok(plan) => plan,
            Err(e) => return Err(McpError::invalid_request(e, None)),
        };

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
        let tables = vec![left.table.clone(), right.table.clone()];
        let strategy = params
            .strategy
            .clone()
            .unwrap_or_else(|| "auto".to_string());
        self.audit
            .record_best_effort(crate::delta_diff::delta_diff_start_event(
                &left_info,
                &right_info,
                &tables,
                &strategy,
            ));

        let started = Instant::now();
        let diff_result = crate::delta_diff::api::run_diff(left, right, opts).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        let outcome = if diff_result.is_ok() {
            AuditOutcome::ok(duration_ms)
        } else {
            AuditOutcome::error(duration_ms, "delta_diff")
        };
        self.audit
            .record_best_effort(crate::delta_diff::delta_diff_outcome_event(
                &left_info,
                &right_info,
                &tables,
                &strategy,
                outcome,
            ));

        match diff_result {
            Ok(mut report) => {
                report.modified_columns =
                    crate::delta_diff::sample::compute_modified_columns(&report);
                let mut export_path = None;
                if let (Some(path), Some(fmt)) = (params.export.as_deref(), export_plan) {
                    match crate::delta_diff::export::render_export(
                        &report,
                        fmt,
                        params.export_rows.unwrap_or(false),
                    ) {
                        Ok(body) => {
                            if let Err(e) = std::fs::write(path, body) {
                                return Ok(CallToolResult::error(vec![Content::text(format!(
                                    "export write failed: {e}"
                                ))]));
                            }
                            export_path = Some(path.to_string());
                        }
                        Err(e) => {
                            return Ok(CallToolResult::error(vec![Content::text(e)]));
                        }
                    }
                }
                // Engine keeps full diffs for export; MCP payload stays selected.
                crate::delta_diff::sample::retain_sample(
                    &mut report,
                    params.sample_limit.unwrap_or(1000),
                    crate::delta_diff::cmd::SampleMode::Diverse,
                );
                let mut payload = serde_json::to_value(&report)
                    .unwrap_or_else(|e| json!({"error": format!("json serialize: {e}")}));
                if let Some(path) = export_path {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert("export_path".into(), json!(path));
                    }
                }
                let text = serde_json::to_string_pretty(&payload)
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
    version = "0.5.0",
    instructions = "MCP server for MySQL/PolarDB-X/Oracle/GaussDB/DuckDB database introspection with multi-connection support"
)]
impl ServerHandler for DbMcp {
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if let Some(info) = context.peer.peer_info() {
            debug!(
                "MCP client initialized: {} {}",
                info.client_info.name, info.client_info.version
            );
            let name = info.client_info.name.clone();
            match self.client.lock() {
                Ok(mut guard) => *guard = Some(name),
                Err(poisoned) => *poisoned.into_inner() = Some(name),
            }
        }
    }
}

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
        Some("naivediff") => Ok(Some(crate::delta_diff::cmd::Strategy::Naivediff)),
        Some(other) => Err(format!("unknown strategy '{other}'")),
    }
}

fn mcp_incremental(params: &DeltaDiffParams) -> Result<Option<(String, String)>, String> {
    if params.where_condition.is_some() && params.update_column.is_some() {
        return Err("where_condition and update_column are mutually exclusive".into());
    }
    if params.update_since.is_some() && params.update_column.is_none() {
        return Err("update_since requires update_column".into());
    }
    Ok(params.update_column.as_ref().map(|col| {
        (
            col.clone(),
            params
                .update_since
                .clone()
                .unwrap_or_else(|| "1 day".into()),
        )
    }))
}

fn parse_mcp_export_format(
    path: Option<&str>,
    format: Option<&str>,
) -> Result<Option<crate::delta_diff::cmd::ExportFormat>, String> {
    if format.is_some() && path.is_none() {
        return Err("export_format requires export path".into());
    }
    if path.is_none() && format.is_none() {
        return Ok(None);
    }
    let override_fmt = match format {
        None => None,
        Some("csv") => Some(crate::delta_diff::cmd::ExportFormat::Csv),
        Some("jsonl") => Some(crate::delta_diff::cmd::ExportFormat::Jsonl),
        Some("json") => Some(crate::delta_diff::cmd::ExportFormat::Json),
        Some("sql") => {
            return Err(
                "export_format sql requires CLI --apply-to; MCP export is csv/jsonl/json only"
                    .into(),
            )
        }
        Some(other) => return Err(format!("unknown export_format '{other}'")),
    };
    let fmt = crate::delta_diff::cmd::infer_export_format(path, override_fmt)?;
    if matches!(fmt, crate::delta_diff::cmd::ExportFormat::Sql) {
        return Err(
            "export *.sql requires CLI --apply-to; MCP export is csv/jsonl/json only".into(),
        );
    }
    Ok(Some(fmt))
}

fn build_mcp_diff_options(
    params: &DeltaDiffParams,
) -> Result<crate::delta_diff::api::DiffOptions, String> {
    if let Some(w) = &params.where_condition {
        if w.contains(';') {
            return Err("where_condition must not contain ';'".into());
        }
    }
    let strategy = parse_delta_diff_strategy(params.strategy.as_deref())?;
    let snapshot = !matches!(params.consistency.as_deref(), Some("none"));
    let recheck = params.recheck.unwrap_or(snapshot);
    let incremental = mcp_incremental(params)?;
    let filter = if incremental.is_some() {
        None
    } else {
        params.where_condition.clone()
    };
    Ok(crate::delta_diff::api::DiffOptions {
        iblt_capacity: 65536,
        fetch_all_threshold: 4096,
        naive_max_rows: 200_000,
        strict: false,
        strategy,
        key: params.key_columns.clone().unwrap_or_default(),
        columns: params.columns.clone().unwrap_or_default(),
        filter,
        incremental,
        bisection_factor: 32,
        bisection_threshold: 16384,
        sample_limit: params.sample_limit.unwrap_or(1000),
        threads: 4,
        snapshot,
        recheck,
        checkpoint: params.checkpoint.clone(),
        verbose: false,
        rtrim_char_columns: false,
    })
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
    fn parses_naivediff() {
        assert_eq!(
            parse_delta_diff_strategy(Some("naivediff")).unwrap(),
            Some(Strategy::Naivediff)
        );
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse_delta_diff_strategy(Some("magic")).is_err());
    }
}

#[cfg(test)]
mod delta_diff_mcp_plan_tests {
    use super::{build_mcp_diff_options, parse_mcp_export_format, DeltaDiffParams};
    use crate::delta_diff::cmd::ExportFormat;

    fn base_params() -> DeltaDiffParams {
        DeltaDiffParams {
            left_connection: "l".into(),
            right_connection: "r".into(),
            table: "t".into(),
            left_table: None,
            right_table: None,
            schema: None,
            left_schema: None,
            right_schema: None,
            key_columns: None,
            columns: None,
            where_condition: None,
            strategy: None,
            consistency: None,
            recheck: None,
            sample_limit: None,
            summary_only: None,
            update_column: None,
            update_since: None,
            checkpoint: None,
            export: None,
            export_format: None,
            export_rows: None,
        }
    }

    #[test]
    fn incremental_uses_update_column_and_since() {
        let mut p = base_params();
        p.update_column = Some("updated_at".into());
        p.update_since = Some("2 hours".into());
        let opts = build_mcp_diff_options(&p).unwrap();
        assert_eq!(
            opts.incremental
                .as_ref()
                .map(|(c, s)| (c.as_str(), s.as_str())),
            Some(("updated_at", "2 hours"))
        );
    }

    #[test]
    fn incremental_defaults_since_to_one_day() {
        let mut p = base_params();
        p.update_column = Some("updated_at".into());
        let opts = build_mcp_diff_options(&p).unwrap();
        assert_eq!(
            opts.incremental
                .as_ref()
                .map(|(c, s)| (c.as_str(), s.as_str())),
            Some(("updated_at", "1 day"))
        );
    }

    #[test]
    fn rejects_update_since_without_column() {
        let mut p = base_params();
        p.update_since = Some("1 day".into());
        assert!(build_mcp_diff_options(&p).is_err());
    }

    #[test]
    fn rejects_where_with_update_column() {
        let mut p = base_params();
        p.where_condition = Some("id > 1".into());
        p.update_column = Some("updated_at".into());
        assert!(build_mcp_diff_options(&p).is_err());
    }

    #[test]
    fn checkpoint_is_passed_through() {
        let mut p = base_params();
        p.checkpoint = Some("/tmp/dd.ckpt".into());
        let opts = build_mcp_diff_options(&p).unwrap();
        assert_eq!(opts.checkpoint.as_deref(), Some("/tmp/dd.ckpt"));
    }

    #[test]
    fn rejects_sql_export_without_apply() {
        assert!(parse_mcp_export_format(Some("out.sql"), Some("sql")).is_err());
        assert!(parse_mcp_export_format(Some("out.sql"), None).is_err());
    }

    #[test]
    fn infers_csv_export_from_path() {
        assert_eq!(
            parse_mcp_export_format(Some("out.csv"), None).unwrap(),
            Some(ExportFormat::Csv)
        );
    }

    #[test]
    fn export_format_without_path_is_rejected() {
        for fmt in ["csv", "jsonl", "json"] {
            assert!(
                parse_mcp_export_format(None, Some(fmt)).is_err(),
                "export_format '{fmt}' without export path must be rejected"
            );
        }
    }

    #[test]
    fn export_format_overrides_path_inference() {
        assert_eq!(
            parse_mcp_export_format(Some("out.csv"), Some("jsonl")).unwrap(),
            Some(ExportFormat::Jsonl)
        );
    }
}

#[cfg(test)]
mod client_stamp_tests {
    use super::*;
    use crate::audit::{AuditConfig, AuditSession};

    fn session(dir: &std::path::Path) -> Arc<AuditSession> {
        Arc::new(AuditSession::new(&AuditConfig {
            dir: Some(dir.to_path_buf()),
            enabled: true,
            fsync: false,
            meta: false,
            retention_days: 0,
        }))
    }

    fn recorded(dir: &std::path::Path) -> Vec<serde_json::Value> {
        let mut events = Vec::new();
        for entry in std::fs::read_dir(dir).expect("audit dir") {
            let path = entry.expect("entry").path();
            let contents = std::fs::read_to_string(path).expect("read audit file");
            for line in contents.lines() {
                events.push(serde_json::from_str::<serde_json::Value>(line).expect("json line"));
            }
        }
        events
    }

    #[test]
    fn should_stamp_actor_client_from_the_mcp_client_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let audit = session(&dir.path().join("audit"));
        let server = DbMcp::new_empty(
            Arc::new(BackendRegistry::new()),
            "default".to_string(),
            Arc::clone(&audit),
        );

        // Before initialize: no client to report, so the field is omitted.
        server.record(connect_event("c", "mysql://u:p@h:3306/db", Decision::Allow));

        match server.client.lock() {
            Ok(mut guard) => *guard = Some("opencode".to_string()),
            Err(poisoned) => *poisoned.into_inner() = Some("opencode".to_string()),
        }
        server.record(connect_event("c", "mysql://u:p@h:3306/db", Decision::Allow));

        let events = recorded(&dir.path().join("audit"));
        assert_eq!(events.len(), 2);
        assert!(
            events[0]["actor"].get("client").is_none(),
            "unknown client must be omitted, not invented"
        );
        assert_eq!(events[1]["actor"]["client"], "opencode");
    }
}

#[cfg(test)]
mod audit_event_builder_tests {
    use super::*;
    use crate::backend::DbError;
    use crate::cli::extract_sqlstate;

    #[test]
    fn deny_event_carries_full_multi_kb_sql_and_prefix_reason() {
        let sql = format!("INSERT INTO t VALUES ('{}')", "x".repeat(5000));
        let ev = execute_query_denied_event("dev", "mysql://u:p@h:3306/db", &sql);
        assert_eq!(ev.decision, Decision::Deny);
        assert_eq!(ev.deny_reason.as_deref(), Some("prefix"));
        assert_eq!(ev.action, "execute_query");
        assert_eq!(ev.class, ActionClass::Dml, "denied INSERT is not a query");
        let info = ev.sql.as_ref().unwrap();
        assert!(!info.truncated);
        assert_eq!(info.text.as_str(), sql.as_str());
        assert!(
            info.text.len() > 80,
            "must carry full SQL, not an 80-char preview"
        );
    }

    #[test]
    fn allow_event_has_outcome_row_count_and_limit_flag() {
        let ev =
            execute_query_allowed_event("dev", "mysql://u:p@h:3306/db", "SELECT 1", 42, 7, true);
        assert_eq!(ev.decision, Decision::Allow);
        assert_eq!(ev.class, ActionClass::Dql);
        assert_eq!(ev.limit_applied, Some(true));
        let outcome = ev.outcome.as_ref().unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.duration_ms, 42);
        assert_eq!(outcome.row_count, Some(7));
    }

    #[test]
    fn connect_event_read_only_session_per_driver() {
        assert!(
            connect_event("g", "gaussdb://u:p@h:5432/db", Decision::Allow)
                .connection
                .read_only_session
        );
        assert!(
            !connect_event("m", "mysql://u:p@h:3306/db", Decision::Allow)
                .connection
                .read_only_session
        );
        assert!(
            !connect_event("o", "oracle://u:p@h:1521/F", Decision::Allow)
                .connection
                .read_only_session
        );
        assert!(
            connect_event("d", "duckdb:///x.db?mode=ro", Decision::Allow)
                .connection
                .read_only_session
        );
    }

    #[test]
    fn extract_sqlstate_parses_driver_code() {
        assert_eq!(
            extract_sqlstate("GaussDB query failed: [SQLSTATE 42P01] relation does not exist")
                .as_deref(),
            Some("42P01")
        );
        assert_eq!(extract_sqlstate("no code here"), None);
    }

    #[test]
    fn classify_query_error_returns_kind_and_sqlstate() {
        let err = DbError::query("boom [SQLSTATE 23505]");
        let (kind, sqlstate) = classify_query_error(&err);
        assert_eq!(kind, "QueryFailed");
        assert_eq!(sqlstate.as_deref(), Some("23505"));
    }
}
