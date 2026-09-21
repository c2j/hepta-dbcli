//! CLI subcommand flag compatibility regression (PR #119 review C6).
//!
//! Spawns the real binary. No database needed: these tests cover argument
//! validation that happens before any connection is attempted. Runs without
//! the `integration` feature too, because the contract under test (exit 2 on
//! unsupported flag combinations) must hold in every build.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_hepta_dbcli");

/// C6: `cli --interactive --allow-ddl` must refuse with exit 2, like the MCP
/// server does, instead of silently ignoring the flag and dropping DDL into a
/// read-only REPL session.
#[test]
fn interactive_repl_rejects_allow_ddl_with_exit_2() {
    let output = Command::new(BIN)
        .args(["cli", "--interactive", "--allow-ddl"])
        .output()
        .expect("run hepta_dbcli");
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2, got {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--allow-ddl"),
        "stderr should name the offending flag: {stderr}"
    );
    assert!(
        stderr.contains("interactive"),
        "stderr should name the interactive session: {stderr}"
    );
}

/// One-shot `cli --sql` keeps accepting `--allow-ddl` (the write gate
/// classifies statements per statement), so this must NOT regress.
#[test]
fn one_shot_cli_still_accepts_allow_ddl_for_the_gate() {
    // No connection config anywhere: the run must get past flag validation
    // and fail at connection resolution instead (exit 1), proving the flag
    // combination itself is not rejected.
    let home = tempfile::tempdir().expect("tempdir");
    let output = Command::new(BIN)
        .env("HOME", home.path())
        .env_remove("HEPTA_DBCLI_URL")
        .args(["cli", "--sql", "SELECT 1", "--allow-ddl"])
        .output()
        .expect("run hepta_dbcli");
    assert_ne!(
        output.status.code(),
        Some(2),
        "one-shot cli must keep accepting --allow-ddl; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
