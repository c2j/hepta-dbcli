// ─── Client-side audit log (issue #57) ──────────────────────────────
//
// A separate JSONL ledger, independent of the `tracing` troubleshooting log.
// It answers "who ran what, on which connection, with what outcome" even when
// `RUST_LOG` filters everything out.
//
// Frozen interface for the parallel wiring work:
//
//     use crate::audit::event::{ActionClass, Channel, Decision, DraftEvent, SqlInfo};
//
//     let session = AuditSession::new(&config);
//     session.record_best_effort(
//         DraftEvent::new(Channel::Cli, connection, "cli_sql", ActionClass::Dql, Decision::Allow)
//             .with_sql(SqlInfo::new(sql)),
//     );
//
// Read-only callers use `record_best_effort` (audit failure warns, query
// continues — D6). The fallible `record` exists for the future write path
// (fail-closed, issue #58).

pub(crate) mod event;
mod ids;
pub(crate) mod scrub;
mod sha256;
pub(crate) mod writer;

use event::{now_rfc3339_millis, AuditEvent, DraftEvent};
use writer::{AuditConfig, AuditWriter};

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

// ─── AuditError ─────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) enum AuditError {
    /// The audit writer could not persist the event.
    Io(std::io::Error),
    /// The event could not be serialized (should not happen).
    Serialize(serde_json::Error),
    /// Audit is turned off (`--no-audit`), so nothing was written.
    Disabled,
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuditError::Io(e) => write!(f, "audit write failed: {e}"),
            AuditError::Serialize(e) => write!(f, "audit serialize failed: {e}"),
            AuditError::Disabled => write!(f, "audit is disabled"),
        }
    }
}

impl std::error::Error for AuditError {}

impl From<std::io::Error> for AuditError {
    fn from(e: std::io::Error) -> Self {
        AuditError::Io(e)
    }
}

impl From<serde_json::Error> for AuditError {
    fn from(e: serde_json::Error) -> Self {
        AuditError::Serialize(e)
    }
}

// ─── AuditSession ───────────────────────────────────────────────────

/// One process lifetime of auditing. `session_id` is fixed and `seq` is
/// monotonic, so consumers can detect dropped lines.
pub(crate) struct AuditSession {
    session_id: String,
    seq: AtomicU64,
    writer: Mutex<Option<AuditWriter>>,
    warn_count: AtomicU64,
}

impl AuditSession {
    pub(crate) fn new(config: &AuditConfig) -> Self {
        let writer = if config.enabled {
            let dir = config.resolved_dir();
            match AuditWriter::open(&dir, config.fsync) {
                Ok(writer) => Some(writer),
                Err(e) => {
                    eprintln!("warning: audit log unavailable at {}: {e}", dir.display());
                    None
                }
            }
        } else {
            None
        };

        Self {
            session_id: ids::new_session_id(),
            seq: AtomicU64::new(0),
            writer: Mutex::new(writer),
            warn_count: AtomicU64::new(0),
        }
    }

    /// `--no-audit`: nothing is ever written.
    pub(crate) fn disabled() -> Self {
        Self::new(&AuditConfig {
            enabled: false,
            ..AuditConfig::default()
        })
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// True when a writer is attached.
    pub(crate) fn is_enabled(&self) -> bool {
        match self.writer.lock() {
            Ok(guard) => guard.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    /// Advance the sequence counter without emitting an event.
    pub(crate) fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Write one audit event. Fallible so the future write path can fail
    /// closed (issue #58 D/fail-closed).
    pub(crate) fn record(&self, draft: DraftEvent) -> Result<(), AuditError> {
        let seq = self.next_seq();
        let event = AuditEvent::stamp(
            draft,
            &self.session_id,
            seq,
            ids::new_event_id(),
            now_rfc3339_millis(),
        );
        let line = event.to_json_line()?;

        let mut guard = match self.writer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.as_mut() {
            Some(writer) => writer.append(&line).map_err(AuditError::Io),
            None => Err(AuditError::Disabled),
        }
    }

    /// Read-only channel policy (issue #57 D6): a failing audit must not fail
    /// the query, but it must be visible. Warn on the first failure and then
    /// every 100th to avoid flooding MCP's stderr.
    pub(crate) fn record_best_effort(&self, draft: DraftEvent) {
        if let Err(e) = self.record(draft) {
            let count = self.warn_count.fetch_add(1, Ordering::Relaxed) + 1;
            if count == 1 || count.is_multiple_of(100) {
                eprintln!("warning: audit event dropped (count={count}): {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::event::{
        ActionClass, AuditOutcome, Channel, ConnectionInfo, Decision, SqlInfo, SCHEMA_VERSION,
    };
    use super::*;
    use std::io::Read;
    use std::path::Path;

    fn draft() -> DraftEvent {
        DraftEvent::new(
            Channel::Cli,
            ConnectionInfo::from_url("dev", "mysql://u:p@h:3306/db", false),
            "cli_sql",
            ActionClass::Dql,
            Decision::Allow,
        )
        .with_sql(SqlInfo::new("SELECT 1"))
    }

    fn read_lines(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let mut contents = String::new();
            std::fs::File::open(entry.unwrap().path())
                .unwrap()
                .read_to_string(&mut contents)
                .unwrap();
            out.extend(contents.lines().map(|l| l.to_string()));
        }
        out
    }

    fn config(dir: &Path) -> AuditConfig {
        AuditConfig {
            dir: Some(dir.to_path_buf()),
            enabled: true,
            fsync: false,
        }
    }

    /// Guards the frozen interface the parallel wiring work codes against.
    #[test]
    fn should_expose_frozen_wiring_surface() {
        let conn = ConnectionInfo::from_url("n", "mysql://u:p@h:3306/db", false);
        let draft = DraftEvent::new(
            Channel::Cli,
            conn,
            "cli_sql",
            ActionClass::Dml,
            Decision::Allow,
        )
        .with_sql(SqlInfo::new("SELECT 1"))
        .with_outcome(AuditOutcome::ok(1))
        .with_client("test");
        let event = AuditEvent::stamp(draft, "s", 0, "id".into(), now_rfc3339_millis());
        assert_eq!(event.v, SCHEMA_VERSION);
        assert_eq!(event.actor.client.as_deref(), Some("test"));

        assert!(scrub::redact_dsn("mysql://u:p@h/db").contains("****"));
        assert_eq!(scrub::percent_decode("a%20b"), "a b");
        assert_eq!(scrub::truncate_for_audit("abc", 2).0, "ab");
        assert!(writer::file_name("2026-01-01").starts_with(writer::AUDIT_FILE_PREFIX));
    }

    #[test]
    fn should_keep_session_id_constant_across_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = AuditSession::new(&config(dir.path()));
        session.record(draft()).expect("record");
        session.record(draft()).expect("record");

        let ids: Vec<String> = read_lines(dir.path())
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["session_id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(ids, vec![session.session_id().to_string(); 2]);
    }

    #[test]
    fn should_increment_seq_monotonically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = AuditSession::new(&config(dir.path()));
        for _ in 0..3 {
            session.record(draft()).expect("record");
        }
        let seqs: Vec<u64> = read_lines(dir.path())
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[test]
    fn should_use_distinct_session_ids_per_process_session() {
        let a = AuditSession::disabled();
        let b = AuditSession::disabled();
        assert_ne!(a.session_id(), b.session_id());
    }

    #[test]
    fn should_stamp_schema_version_and_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = AuditSession::new(&config(dir.path()));
        session.record(draft()).expect("record");

        let v: serde_json::Value = serde_json::from_str(&read_lines(dir.path())[0]).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["event_id"].as_str().unwrap().len(), 26);
        let ts = v["ts"].as_str().unwrap();
        assert!(ts.ends_with('Z') && ts.contains('T'), "bad ts: {ts}");
    }

    #[test]
    fn should_write_nothing_when_disabled_and_report_disabled_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = AuditSession::disabled();
        let err = session.record(draft()).expect_err("must report disabled");
        assert!(matches!(err, AuditError::Disabled));
        assert!(!session.is_enabled());
        assert!(!dir.path().join("audit").exists());
    }

    #[test]
    fn should_not_panic_on_best_effort_when_disabled() {
        let session = AuditSession::disabled();
        session.record_best_effort(draft());
        assert!(!session.is_enabled());
    }

    #[test]
    fn should_still_advance_seq_when_disabled() {
        let session = AuditSession::disabled();
        assert_eq!(session.next_seq(), 0);
        // `record` consumes the next seq even though nothing is written.
        let _ = session.record(draft());
        assert_eq!(session.next_seq(), 2);
    }

    #[test]
    fn should_degrade_to_disabled_when_directory_cannot_be_created() {
        // A path under an existing *file* can never become a directory.
        let base = tempfile::tempdir().expect("tempdir");
        let blocker = base.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("write blocker");

        let session = AuditSession::new(&AuditConfig {
            dir: Some(blocker.join("audit")),
            enabled: true,
            fsync: false,
        });
        assert!(!session.is_enabled(), "must degrade, not panic");
        session.record_best_effort(draft());
    }
}
