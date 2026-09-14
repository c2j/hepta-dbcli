// ─── Audit event schema v1 (issue #57 §4) ───────────────────────────
//
// Frozen interface for the parallel implementation work: callers build a
// `DraftEvent` and hand it to `AuditSession::record`, which stamps the
// envelope (v/ts/event_id/session_id/seq). Nothing else may construct an
// `AuditEvent` envelope, so `seq` and `session_id` invariants live in one
// place.

use serde::Serialize;

use super::scrub::{percent_decode, truncate_for_audit};
use super::sha256::sha256_hex;

pub(crate) const SCHEMA_VERSION: u8 = 1;

// ─── Enums ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Channel {
    Mcp,
    Cli,
    Repl,
    DeltaDiff,
    Synth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Decision {
    Allow,
    Deny,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionClass {
    Dql,
    Dml,
    Ddl,
    Dcl,
    Call,
    Admin,
    Meta,
}

// ─── Actor ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Actor {
    pub os_user: String,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

pub(crate) fn current_actor(client: Option<String>) -> Actor {
    let os_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    Actor {
        os_user,
        pid: std::process::id(),
        client,
    }
}

// ─── Connection ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ConnectionInfo {
    pub name: String,
    pub driver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    pub read_only_session: bool,
}

impl ConnectionInfo {
    /// Parse connection metadata from a DSN. Never records the password.
    pub(crate) fn from_url(name: &str, url: &str, read_only_session: bool) -> Self {
        let (scheme, rest) = match url.find("://") {
            Some(i) => (&url[..i], &url[i + 3..]),
            None => ("", url),
        };
        let rest = rest.split('?').next().unwrap_or(rest);

        // DuckDB is embedded: the whole remainder is the file path (or
        // `:memory:`), with no user/host/port.
        if scheme == "duckdb" {
            return ConnectionInfo {
                name: name.to_string(),
                driver: scheme.to_string(),
                user: None,
                host: None,
                port: None,
                database: non_empty(rest),
                read_only_session,
            };
        }

        let authority_end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let path = &rest[authority_end..];

        let (userinfo, hostport) = match authority.rfind('@') {
            Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
            None => (None, authority),
        };

        let user = userinfo.and_then(|ui| {
            let raw = ui.split(':').next().unwrap_or(ui);
            if raw.is_empty() {
                None
            } else {
                Some(percent_decode(raw))
            }
        });

        let (host, port) = split_host_port(hostport);
        let database = path
            .strip_prefix('/')
            .and_then(non_empty)
            .map(|d| percent_decode(&d));

        ConnectionInfo {
            name: name.to_string(),
            driver: if scheme.is_empty() {
                "unknown".to_string()
            } else {
                scheme.to_string()
            },
            user,
            host: host.map(|h| percent_decode(&h)),
            port,
            database,
            read_only_session,
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn split_host_port(hostport: &str) -> (Option<String>, Option<u16>) {
    if hostport.is_empty() {
        return (None, None);
    }
    // IPv6 literal: [::1]:3306
    if let Some(close) = hostport.find(']') {
        let host = hostport[..=close].to_string();
        let port = hostport[close + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok());
        return (Some(host), port);
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) => (non_empty(host), port.parse::<u16>().ok()),
        None => (non_empty(hostport), None),
    }
}

// ─── SQL ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SqlInfo {
    pub text: String,
    pub sha256: String,
    pub truncated: bool,
}

impl SqlInfo {
    /// SQL text cap (issue #57 §4). The digest always covers the *original*
    /// text, even when `text` is truncated.
    pub(crate) const MAX_BYTES: usize = 8192;

    pub(crate) fn new(text: &str) -> Self {
        let (cut, truncated) = truncate_for_audit(text, Self::MAX_BYTES);
        Self {
            text: cut,
            sha256: sha256_hex(text.as_bytes()),
            truncated,
        }
    }
}

// ─── Outcome ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AuditOutcome {
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_affected: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqlstate: Option<String>,
}

impl AuditOutcome {
    pub(crate) fn ok(duration_ms: u64) -> Self {
        Self {
            ok: true,
            duration_ms,
            row_count: None,
            rows_affected: None,
            error_kind: None,
            sqlstate: None,
        }
    }

    pub(crate) fn error(duration_ms: u64, error_kind: impl Into<String>) -> Self {
        Self {
            ok: false,
            duration_ms,
            row_count: None,
            rows_affected: None,
            error_kind: Some(error_kind.into()),
            sqlstate: None,
        }
    }

    pub(crate) fn with_row_count(mut self, n: u64) -> Self {
        self.row_count = Some(n);
        self
    }

    pub(crate) fn with_rows_affected(mut self, n: u64) -> Self {
        self.rows_affected = Some(n);
        self
    }

    pub(crate) fn with_sqlstate(mut self, state: impl Into<String>) -> Self {
        self.sqlstate = Some(state.into());
        self
    }
}

// ─── Draft (caller input) ───────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DraftEvent {
    pub channel: Channel,
    pub client: Option<String>,
    pub connection: ConnectionInfo,
    pub action: String,
    pub class: ActionClass,
    pub decision: Decision,
    pub sql: Option<SqlInfo>,
    pub outcome: Option<AuditOutcome>,
    pub deny_reason: Option<String>,
    pub redacted: bool,
}

impl DraftEvent {
    pub(crate) fn new(
        channel: Channel,
        connection: ConnectionInfo,
        action: impl Into<String>,
        class: ActionClass,
        decision: Decision,
    ) -> Self {
        Self {
            channel,
            client: None,
            connection,
            action: action.into(),
            class,
            decision,
            sql: None,
            outcome: None,
            deny_reason: None,
            redacted: false,
        }
    }

    pub(crate) fn with_client(mut self, client: impl Into<String>) -> Self {
        self.client = Some(client.into());
        self
    }

    pub(crate) fn with_sql(mut self, sql: SqlInfo) -> Self {
        self.sql = Some(sql);
        self
    }

    pub(crate) fn with_outcome(mut self, outcome: AuditOutcome) -> Self {
        self.outcome = Some(outcome);
        self
    }

    pub(crate) fn with_deny_reason(mut self, reason: impl Into<String>) -> Self {
        self.deny_reason = Some(reason.into());
        self
    }

    pub(crate) fn redacted(mut self) -> Self {
        self.redacted = true;
        self
    }
}

// ─── Stamped event (envelope) ───────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct AuditEvent {
    pub v: u8,
    pub ts: String,
    pub event_id: String,
    pub session_id: String,
    pub seq: u64,
    pub channel: Channel,
    pub actor: Actor,
    pub connection: ConnectionInfo,
    pub action: String,
    pub class: ActionClass,
    pub decision: Decision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql: Option<SqlInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<AuditOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_reason: Option<String>,
    pub redacted: bool,
}

impl AuditEvent {
    pub(crate) fn stamp(
        draft: DraftEvent,
        session_id: &str,
        seq: u64,
        event_id: String,
        ts: String,
    ) -> Self {
        Self {
            v: SCHEMA_VERSION,
            ts,
            event_id,
            session_id: session_id.to_string(),
            seq,
            channel: draft.channel,
            actor: current_actor(draft.client),
            connection: draft.connection,
            action: draft.action,
            class: draft.class,
            decision: draft.decision,
            sql: draft.sql,
            outcome: draft.outcome,
            deny_reason: draft.deny_reason,
            redacted: draft.redacted,
        }
    }

    pub(crate) fn to_json_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

pub(crate) fn now_rfc3339_millis() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(conn: ConnectionInfo) -> AuditEvent {
        AuditEvent::stamp(
            DraftEvent::new(
                Channel::Mcp,
                conn,
                "execute_query",
                ActionClass::Dql,
                Decision::Allow,
            ),
            "abc123",
            42,
            "01K5Q3ABCDEFGHJKMNPQRSTVWX".to_string(),
            "2026-09-14T08:12:03.441Z".to_string(),
        )
    }

    #[test]
    fn should_serialize_envelope_fields() {
        let json = ev(ConnectionInfo::from_url(
            "gauss",
            "gaussdb://u:p@h:5432/db",
            true,
        ))
        .to_json_line()
        .expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["v"], 1);
        assert_eq!(v["channel"], "mcp");
        assert_eq!(v["decision"], "allow");
        assert_eq!(v["class"], "dql");
        assert_eq!(v["action"], "execute_query");
        assert_eq!(v["seq"], 42);
        assert_eq!(v["session_id"], "abc123");
        assert_eq!(v["connection"]["read_only_session"], true);
        assert_eq!(v["connection"]["driver"], "gaussdb");
    }

    #[test]
    fn should_omit_optional_fields_when_absent() {
        let json = ev(ConnectionInfo::from_url(
            "gauss",
            "gaussdb://u:p@h:5432/db",
            true,
        ))
        .to_json_line()
        .expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(v.get("sql").is_none());
        assert!(v.get("outcome").is_none());
        assert!(v.get("deny_reason").is_none());
        assert!(v["actor"].get("client").is_none());
    }

    #[test]
    fn should_never_leak_password_into_serialized_event() {
        let boundary = "gaussdb://u:sup3rs3cr3t@h:5432/db".to_string();
        let json = ev(ConnectionInfo::from_url("gauss", &boundary, true))
            .to_json_line()
            .expect("serialize");
        assert!(!json.contains("sup3rs3cr3t"), "password leaked: {json}");
    }

    #[test]
    fn should_encode_all_channel_values_as_snake_case() {
        for (ch, want) in [
            (Channel::Mcp, "mcp"),
            (Channel::Cli, "cli"),
            (Channel::Repl, "repl"),
            (Channel::DeltaDiff, "delta_diff"),
            (Channel::Synth, "synth"),
        ] {
            let conn = ConnectionInfo::from_url("d", "mysql://h/db", false);
            let e = AuditEvent::stamp(
                DraftEvent::new(ch, conn, "x", ActionClass::Meta, Decision::Allow),
                "s",
                1,
                "id".into(),
                "t".into(),
            );
            assert_eq!(serde_json::to_value(&e).unwrap()["channel"], want);
        }
    }

    #[test]
    fn should_parse_duckdb_database_from_path() {
        let c = ConnectionInfo::from_url("d", "duckdb:///tmp/shop.duckdb", false);
        assert_eq!(c.driver, "duckdb");
        assert_eq!(c.database.as_deref(), Some("/tmp/shop.duckdb"));
        assert_eq!(c.host, None);
        assert_eq!(c.port, None);
        assert_eq!(c.user, None);
    }

    #[test]
    fn should_parse_duckdb_memory_database() {
        let c = ConnectionInfo::from_url("d", "duckdb://:memory:", false);
        assert_eq!(c.database.as_deref(), Some(":memory:"));
    }

    #[test]
    fn should_parse_mysql_authority_and_database() {
        let c = ConnectionInfo::from_url("m", "mysql://mcp:pw@127.0.0.1:3306/testdb", false);
        assert_eq!(c.driver, "mysql");
        assert_eq!(c.user.as_deref(), Some("mcp"));
        assert_eq!(c.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(c.port, Some(3306));
        assert_eq!(c.database.as_deref(), Some("testdb"));
        assert!(!c.read_only_session);
    }

    #[test]
    fn should_parse_oracle_service_name_as_database() {
        let c = ConnectionInfo::from_url(
            "ora",
            "oracle://system:tiger@oracle.internal:1521/FREEPDB1",
            false,
        );
        assert_eq!(c.database.as_deref(), Some("FREEPDB1"));
        assert_eq!(c.port, Some(1521));
    }

    #[test]
    fn should_strip_query_string_before_parsing() {
        let c = ConnectionInfo::from_url("g", "gaussdb://u:p@h:5432/db?sslmode=require", true);
        assert_eq!(c.database.as_deref(), Some("db"));
        assert_eq!(c.host.as_deref(), Some("h"));
    }

    #[test]
    fn should_percent_decode_credentials() {
        let c = ConnectionInfo::from_url("m", "mysql://a%40b:p%3Ass@h:3306/db", false);
        assert_eq!(c.user.as_deref(), Some("a@b"));
    }

    #[test]
    fn should_handle_ipv6_host() {
        let c = ConnectionInfo::from_url("m", "mysql://u:p@[::1]:3306/db", false);
        assert_eq!(c.host.as_deref(), Some("[::1]"));
        assert_eq!(c.port, Some(3306));
    }

    #[test]
    fn sql_info_hashes_original_text_and_flags_truncation() {
        let long = "x".repeat(SqlInfo::MAX_BYTES + 100);
        let info = SqlInfo::new(&long);
        assert!(info.truncated);
        assert_eq!(info.text.len(), SqlInfo::MAX_BYTES);
        assert_eq!(info.sha256, sha256_hex(long.as_bytes()));
    }

    #[test]
    fn sql_info_does_not_truncate_short_text() {
        let info = SqlInfo::new("SELECT 1");
        assert!(!info.truncated);
        assert_eq!(info.text, "SELECT 1");
        assert_eq!(info.sha256.len(), 64);
    }
}
