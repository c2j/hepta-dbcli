//! Inline URL end-to-end regression (PR #97 review).
//!
//! Spawns the real binary with DuckDB inline URLs — no external service,
//! no config file. Verifies:
//! - `check --url` probes the inline connection (exit 0)
//! - subcommands that do not support `--url` reject it explicitly (exit 2)
//! - a configless MCP server still serves an inline-URL delta_diff, and
//!   named-connection tools fail loudly per call
//!
//! Run:
//!   cargo test --features "duckdb,integration" --test regress_inline_url

#![cfg(all(feature = "integration", feature = "duckdb"))]

use std::io::Write;
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_hepta_dbcli");

/// Empty HOME keeps the "no config file" path deterministic even on
/// developer machines that do have ~/.hepta-dbcli.toml. The tempdir is
/// removed when the returned guard is dropped (end of test).
fn isolated(command: Command) -> (Command, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("tempdir");
    let mut cmd = command;
    cmd.env("HOME", home.path()).env_remove("HEPTA_DBCLI_URL");
    (cmd, home)
}

#[test]
fn check_with_inline_url_probes_without_config() {
    let (mut cmd, _home) = isolated(Command::new(BIN));
    let out = cmd
        .args(["--url", "duckdb://:memory:", "check"])
        .output()
        .expect("spawn check");
    assert!(
        out.status.success(),
        "check --url should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn store_password_rejects_inline_url_loudly() {
    let (mut cmd, _home) = isolated(Command::new(BIN));
    let out = cmd
        .args(["--url", "duckdb://:memory:", "store-password"])
        .output()
        .expect("spawn store-password");
    assert_eq!(out.status.code(), Some(2), "must exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--url"),
        "rejection must mention --url: {stderr}"
    );
}

#[test]
fn synth_rejects_inline_url_loudly() {
    let (mut cmd, _home) = isolated(Command::new(BIN));
    let out = cmd
        .args([
            "--url",
            "duckdb://:memory:",
            "synth",
            "train",
            "--tables",
            "users",
            "--name",
            "x",
        ])
        .output()
        .expect("spawn synth");
    assert_eq!(out.status.code(), Some(2), "must exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--url"),
        "rejection must mention --url: {stderr}"
    );
}

fn seed_duckdb(path: &std::path::Path, rows: &str) {
    // The .duckdb file must not pre-exist as an empty file (DuckDB treats a
    // zero-byte path as corrupt), so create it directly via the embedded
    // driver — same pattern as regress_duckdb.rs.
    if path.exists() {
        std::fs::remove_file(path).expect("remove stale fixture");
    }
    let conn = duckdb::Connection::open(path).expect("bootstrap create");
    conn.execute_batch(&format!("CREATE TABLE t (id INTEGER, v VARCHAR); {rows}"))
        .expect("bootstrap table");
}

#[test]
fn mcp_with_broken_explicit_config_fails_closed() {
    // `--config` 指向坏文件时必须 exit 1，绝不降级为空连接表
    // （HOME 下没有默认配置也不能掩盖显式配置的错误）。
    let dir = tempfile::tempdir().expect("tempdir");
    let broken = dir.path().join("broken.toml");
    std::fs::write(&broken, "not valid toml {{{{").expect("write");
    let (mut cmd, _home) = isolated(Command::new(BIN));
    let out = cmd
        .args(["--config"])
        .arg(&broken)
        .arg("mcp")
        .stdin(Stdio::null())
        .output()
        .expect("spawn mcp");
    assert_eq!(
        out.status.code(),
        Some(1),
        "broken --config must exit 1, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error"),
        "must print an error, not a degrading warning: {stderr}"
    );
    assert!(
        !stderr.contains("warning: no connection configuration"),
        "must not degrade with --config present: {stderr}"
    );
}

#[test]
fn mcp_with_missing_explicit_config_fails_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nope.toml");
    let (mut cmd, _home) = isolated(Command::new(BIN));
    let out = cmd
        .args(["--config"])
        .arg(&missing)
        .arg("mcp")
        .stdin(Stdio::null())
        .output()
        .expect("spawn mcp");
    assert_eq!(
        out.status.code(),
        Some(1),
        "missing --config must exit 1, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn mcp_rejects_global_url_loudly() {
    let (mut cmd, _home) = isolated(Command::new(BIN));
    let out = cmd
        .args(["--url", "duckdb://:memory:", "mcp"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn mcp");
    assert_eq!(out.status.code(), Some(2), "must exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--url"),
        "rejection must mention --url: {stderr}"
    );
}

#[test]
fn mcp_get_table_metadata_resolves_schema_and_fails_loudly_for_missing_table() {
    // Issue #100: a schema-less get_table_metadata must resolve the
    // connection's own current schema (DuckDB `main`), never "public", and
    // a nonexistent table must yield a JSON-RPC error — never a silent
    // {"columns":[],"indexes":[]} that reads as success.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("meta.duckdb");
    seed_duckdb(&db, "INSERT INTO t VALUES (1,'a')");
    let cfg = dir.path().join("cfg.toml");
    std::fs::write(
        &cfg,
        format!("[connections.test]\nurl = \"duckdb://{}\"\n", db.display()),
    )
    .expect("write cfg");

    let call = |payload: String| {
        let (mut mcp_cmd, _home) = isolated(Command::new(BIN));
        let mut child = mcp_cmd
            .arg("--config")
            .arg(&cfg)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mcp");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(payload.as_bytes())
            .expect("write stdio");
        let out = child.wait_with_output().expect("wait");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    fn rpc(id: u64, tool: &str, args: serde_json::Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        })
        .to_string()
    }

    fn handshake() -> String {
        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "probe", "version": "0"}
            }
        });
        format!(
            "{init}\n{}\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
    }

    /// Parse each stdout line as JSON-RPC and return the entry with `id`.
    fn response_for<'a>(stdout: &'a str, id: u64) -> serde_json::Value {
        stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|msg| msg.get("id") == Some(&serde_json::json!(id)))
            .unwrap_or_else(|| panic!("no JSON-RPC response with id {id} in: {stdout}"))
    }

    /// The tool result payload of a successful call (content[0].text parsed).
    fn tool_payload(result: &serde_json::Value) -> serde_json::Value {
        let text = result["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text content");
        serde_json::from_str(text).expect("tool payload JSON")
    }

    let handshake = handshake();

    // 1. Schema-less call resolves the connection's current schema and
    //    returns the real columns.
    let payload = rpc(
        2,
        "get_table_metadata",
        serde_json::json!({"connection_name": "test", "table_name": "t"}),
    );
    let stdout = call(format!("{handshake}{payload}\n"));
    let response = response_for(&stdout, 2);
    assert!(
        response.get("error").is_none(),
        "schema-less metadata for an existing table must succeed: {stdout}"
    );
    let payload_json = tool_payload(&response);
    let columns = payload_json["columns"].as_array().expect("columns array");
    let names: Vec<&str> = columns
        .iter()
        .filter_map(|c| c["column_name"].as_str())
        .collect();
    assert!(
        names.contains(&"id") && names.contains(&"v"),
        "must return the DuckDB table's real columns (schema resolved to main): {stdout}"
    );

    // 2. A nonexistent table (schema resolved automatically) must fail
    //    loudly with a JSON-RPC error, not a silent empty result.
    let payload = rpc(
        3,
        "get_table_metadata",
        serde_json::json!({"connection_name": "test", "table_name": "no_such_table"}),
    );
    let stdout = call(format!("{handshake}{payload}\n"));
    let response = response_for(&stdout, 3);
    assert!(
        response.get("error").is_some(),
        "metadata for a missing table must be a JSON-RPC error: {stdout}"
    );

    // 3. Same for an explicit schema: empty metadata must never pass as
    //    success.
    let payload = rpc(
        4,
        "get_table_metadata",
        serde_json::json!({
            "connection_name": "test",
            "table_name": "no_such_table",
            "schema_name": "main"
        }),
    );
    let stdout = call(format!("{handshake}{payload}\n"));
    let response = response_for(&stdout, 4);
    assert!(
        response.get("error").is_some(),
        "metadata for a missing table (explicit schema) must be a JSON-RPC error: {stdout}"
    );
}

#[test]
fn mcp_configless_startup_serves_inline_url_diff_and_rejects_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    let left = dir.path().join("left.duckdb");
    let right = dir.path().join("right.duckdb");
    seed_duckdb(&left, "INSERT INTO t VALUES (1,'a'), (2,'b')");
    seed_duckdb(&right, "INSERT INTO t VALUES (1,'a'), (2,'CHANGED')");

    let call = |payload: String| {
        let (mut mcp_cmd, _home) = isolated(Command::new(BIN));
        let mut child = mcp_cmd
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mcp");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(payload.as_bytes())
            .expect("write stdio");
        let out = child.wait_with_output().expect("wait");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    fn rpc(id: u64, tool: &str, args: serde_json::Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args}
        })
        .to_string()
    }

    fn handshake() -> String {
        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "probe", "version": "0"}
            }
        });
        format!(
            "{init}\n{}\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
    }

    let handshake = handshake();

    // 1. Inline-URL diff on both sides must work with zero config.
    let diff_payload = rpc(
        2,
        "delta_diff",
        serde_json::json!({
            "left_url": format!("duckdb://{}", left.display()),
            "right_url": format!("duckdb://{}", right.display()),
            "table": "t",
            "key_columns": ["id"],
        }),
    );
    let stdout = call(format!("{handshake}{diff_payload}\n"));
    assert!(
        stdout.contains("\"id\":2"),
        "expected a delta_diff response: {stdout}"
    );
    assert!(
        !stdout.contains("\"isError\":true"),
        "inline-URL diff must not error: {stdout}"
    );
    assert!(
        stdout.contains("inline-duckdb"),
        "report must show inline-duckdb as the connection name: {stdout}"
    );

    // 2. A named connection must fail loudly (no silent fallback).
    let named_payload = rpc(
        2,
        "execute_query",
        serde_json::json!({"connection_name": "prod", "sql": "SELECT 1"}),
    );
    let stdout = call(format!("{handshake}{named_payload}\n"));
    assert!(
        stdout.contains("'prod'")
            || (stdout.contains("prod")
                && (stdout.to_lowercase().contains("not found")
                    || stdout.contains("unknown_connection"))),
        "named connection must fail loudly without config: {stdout}"
    );
}
