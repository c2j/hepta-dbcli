//! Client audit log end-to-end regression (issue #57).
//!
//! Spawns the real binary and inspects the JSONL ledger it writes. Skips
//! (rather than fails) when no MySQL is configured.
//!
//! Run:
//!   HEPTA_DBCLI_TEST_URL=mysql://mcp:testpass@127.0.0.1:3306/testdb \
//!     cargo test --all --features integration --test regress_audit

#![cfg(feature = "integration")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn test_url() -> Option<String> {
    std::env::var("HEPTA_DBCLI_TEST_URL").ok()
}

const BIN: &str = env!("CARGO_BIN_EXE_hepta_dbcli");

fn audit_events(dir: &Path) -> Vec<serde_json::Value> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("audit dir {} missing: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 1, "expected exactly one audit file: {files:?}");

    let contents = std::fs::read_to_string(&files[0]).expect("read audit file");
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad json line {l:?}: {e}")))
        .collect()
}

fn only<'a>(events: &'a [serde_json::Value], action: &str) -> &'a serde_json::Value {
    let matches: Vec<&serde_json::Value> =
        events.iter().filter(|e| e["action"] == action).collect();
    assert_eq!(
        matches.len(),
        1,
        "expected one {action} event, got {matches:?}"
    );
    matches[0]
}

#[test]
fn cli_select_audits_full_sql_without_secrets_or_rows() {
    let Some(url) = test_url() else {
        eprintln!("skipping: HEPTA_DBCLI_TEST_URL not set");
        return;
    };
    let home = tempfile::tempdir().expect("tempdir");
    let audit_dir = home.path().join("audit");

    let output = Command::new(BIN)
        .env("HEPTA_DBCLI_URL", &url)
        .env("HOME", home.path())
        .args([
            "--audit-dir",
            audit_dir.to_str().unwrap(),
            "cli",
            "--sql",
            "SELECT 1 AS one",
        ])
        .output()
        .expect("run hepta_dbcli");
    assert!(
        output.status.success(),
        "cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = audit_events(&audit_dir);
    let event = only(&events, "cli_sql");
    assert_eq!(event["channel"], "cli");
    assert_eq!(event["decision"], "allow");
    assert_eq!(event["class"], "dql");
    assert_eq!(event["sql"]["text"], "SELECT 1 AS one");
    assert_eq!(event["sql"]["truncated"], false);
    assert_eq!(event["outcome"]["ok"], true);
    assert_eq!(event["outcome"]["row_count"], 1);
    assert_eq!(event["source"], "argv");

    let raw = std::fs::read_to_string(audit_dir.join(format!(
        "hepta-dbcli-audit.{}.jsonl",
        chrono::Utc::now().format("%Y-%m-%d")
    )))
    .expect("audit file");
    assert!(
        !raw.contains("testpass"),
        "audit file must never contain the password: {raw}"
    );
    assert!(
        !raw.contains("\"rows\""),
        "audit file must never contain result rows: {raw}"
    );
}

#[test]
fn mcp_insert_is_denied_and_still_audited() {
    let Some(url) = test_url() else {
        eprintln!("skipping: HEPTA_DBCLI_TEST_URL not set");
        return;
    };
    let home = tempfile::tempdir().expect("tempdir");
    let audit_dir = home.path().join("audit");

    let mut child = Command::new(BIN)
        .env("HEPTA_DBCLI_URL", &url)
        .env("HOME", home.path())
        .args(["--audit-dir", audit_dir.to_str().unwrap(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp server");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        let requests = [
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2024-11-05","capabilities":{},
                "clientInfo":{"name":"regress-audit","version":"1"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"execute_query",
                "arguments":{"sql":"INSERT INTO nope VALUES (1)"}}}),
        ];
        for request in requests {
            writeln!(stdin, "{request}").expect("write request");
        }
        stdin.flush().expect("flush");
    }
    // EOF on stdin shuts the server down.
    let output = child.wait_with_output().expect("wait mcp server");
    assert!(
        output.status.success(),
        "mcp server exited with {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let events = audit_events(&audit_dir);
    let event = only(&events, "execute_query");
    assert_eq!(event["channel"], "mcp");
    assert_eq!(event["decision"], "deny");
    assert_eq!(event["deny_reason"], "prefix");
    assert_eq!(event["class"], "dml");
    assert_eq!(event["sql"]["text"], "INSERT INTO nope VALUES (1)");

    // The MCP accept path must have produced a connect event too.
    assert!(
        events.iter().any(|e| e["action"] == "connect"),
        "expected a connect event: {events:?}"
    );
}

#[test]
fn no_audit_flag_writes_nothing() {
    let Some(url) = test_url() else {
        eprintln!("skipping: HEPTA_DBCLI_TEST_URL not set");
        return;
    };
    let home = tempfile::tempdir().expect("tempdir");
    let audit_dir = home.path().join("audit");

    let output = Command::new(BIN)
        .env("HEPTA_DBCLI_URL", &url)
        .env("HOME", home.path())
        .args([
            "--no-audit",
            "--audit-dir",
            audit_dir.to_str().unwrap(),
            "cli",
            "--sql",
            "SELECT 1",
        ])
        .output()
        .expect("run hepta_dbcli");
    assert!(output.status.success());
    assert!(
        !audit_dir.exists(),
        "--no-audit must not create the audit dir"
    );
}
