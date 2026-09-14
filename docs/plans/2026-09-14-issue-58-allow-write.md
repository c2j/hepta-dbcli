# CLI `--allow-write` Implementation Plan (issue #58)

> **Status: implemented** in PR #61 (`feat/issue-58-allow-write`), stacked on #59.
> This document is kept as the design rationale; the "Open decisions" section
> below records what was actually shipped and why.

**Goal:** Let a human at the terminal run data-changing statements (`INSERT` / `UPDATE` / `DELETE` and explicitly allowed `CALL`) on MySQL, Oracle and GaussDB, while MCP stays read-only and destructive DDL stays rejected. Every write goes through the audit ledger first.

**Depends on:** #57 (client audit log). Landed in PR #59. Provided seams:

- `audit::AuditSession::record(&DraftEvent) -> Result<(), AuditError>` — fallible, so the write path can fail closed.
- `audit::DraftEvent` builders with `class: dml | call` and `outcome.rows_affected`.
- `ConnectionInfo.read_only_session` is already recorded on every event.

**Architecture:** layered authorization, not a single boolean.

| Layer | Statements | This issue |
|---|---|---|
| L1 read-only (default) | `SELECT` / `EXPLAIN` / `SHOW` / `DESCRIBE` | unchanged |
| L2 data change (`--allow-write`) | `INSERT` / `UPDATE` / `DELETE`, explicit `CALL` | **enabled on CLI/REPL only** |
| L3 destructive | `DROP` / `TRUNCATE` / `ALTER` / `CREATE` / `GRANT` | **still rejected** |

MCP keeps the L1 prefix gate and a read-only session; `--allow-write` is a CLI/REPL flag.

---

## Current state (verified in code, not assumed)

| Fact | Location | Consequence for #58 |
|---|---|---|
| CLI/REPL have **no** statement gate | `cli.rs::run_cli`, `interactive.rs` | MySQL/Oracle CLI can already write today; #58 formalizes it and adds audit + DDL refusal |
| MCP gate is `starts_with` on a dialect prefix list | `cli.rs::is_read_only_mcp`, `Dialect::read_only_prefixes` | do not touch it; it stays MCP-only |
| GaussDB pins the session read-only | `backend/gaussdb/pool.rs` `connect_one()` → `SET default_transaction_read_only = ON` (per `acquire()`) | the flag must branch **per connection creation**, keyed on "is this process allow-write" |
| `QueryResult { columns, rows, row_count }` | `backend/mod.rs` | **no `rows_affected`** → must be added to the trait and to **5** backends: `mysql`, `oracle`, `oracle_native`, `gaussdb`, `duckdb` |
| Empty result renders `(0 rows)` | `cli.rs::render_result` | DML currently prints a fake empty result; needs `N rows affected` |
| `add_limit` appends unconditionally | `backend/{mysql,oracle,oracle_native,gaussdb,duckdb}/dialect.rs` | must not run for DML/`CALL` (D6) |
| DuckDB read-only is `?mode=ro`; its prefix list also has `SHOW`/`DESC`/`SUMMARIZE` | `backend/duckdb/dialect.rs` | the issue's dialect table omits DuckDB entirely |
| `oracle_native` duplicates `oracle` | `backend/oracle_native/` | both need the same `rows_affected` change |

---

## Decisions taken

All six open questions were resolved during implementation; the reasoning is
kept here because the code alone does not record *why*.

| # | Question | Shipped |
|---|---|---|
| 1 | Scope of `--allow-write` | global clap flag; the `mcp` arm rejects it with exit code 2. A global flag is the only shape that keeps it out of a subcommand while still refusing it for MCP |
| 2 | DuckDB | covered by the same classifier; DuckDB has no session GUC, so its `?mode=ro` still wins at the driver level |
| 3 | `CALL` / `DO` / anonymous blocks | explicit `CALL`/`EXEC`/`DO`/`DECLARE` and `BEGIN … END;` are L2 (need the flag); a bare `BEGIN` stays transaction control and is untouched |
| 4 | `deny_reason` | no new value. Client-side refusals never execute, so they are not audited as statements at all; engine rejections stay `decision=error` with SQLSTATE (e.g. GaussDB `25006`) |
| 5 | DML output | `N rows affected` for table/vertical/csv, `{"rows_affected": n}` for json; `QueryResult::empty()` keeps `(0 rows)` |
| 6 | Audit failure while writing | fail closed — the intent event is written before execution via `AuditSession::record`, and `--no-audit --allow-write` is refused at startup |

### One interpretation still open

Issue §4 says the default is "behaviour unchanged (… MySQL/Oracle CLI can still
write)" while D4 says L2 requires `--allow-write`. PR #61 implements D4
uniformly, so a bare MySQL/Oracle CLI `INSERT` is now refused. If the intent was
to keep MySQL/Oracle L2 ungated, only `cli.rs::write_gate` changes.

---

---

## TDD task breakdown

One behaviour per loop, red → green → refactor. Loops 1, 2, 4, 5 need no database.

1. **Seam first, no DB.** `GaussdbPool` must expose the session-init SQL it will send. Introduce `fn session_init_sql(allow_write: bool) -> Vec<String>` (pure) and a fake `DbConn` that captures the session pins. *Red:* with `allow_write = false` the list contains `SET default_transaction_read_only = ON`; *Green:* implement the branch; assert `allow_write = true` omits it.
2. **MCP is unaffected** — the MCP connection path always builds the read-only list, even when the process was started with `--allow-write` (it should not get that far; belt and braces).
3. **`rows_affected` in the trait** — add the field to `QueryResult`, then make each of the 5 backends populate it (MySQL `AffectedRows`, GaussDB/Postgres `execute` tag, Oracle `row_count`, DuckDB `execute`). Unit-test the *rendering* first (`render_result` prints `N rows affected`), then wire each backend.
4. **Statement classifier** — pure function `classify(sql) -> ActionClass` (+ `is_destructive`). Table-driven tests: `INSERT/UPDATE/DELETE` → `dml`; `CALL` → `call`; `DROP/TRUNCATE/ALTER/GRANT/CREATE` → destructive; `SELECT/WITH` → `dql`; leading comments and mixed case.
5. **Flag gate** — without `--allow-write`, L2 statements are rejected client-side with a pointer to the flag; with it, L3 is still rejected with "destructive DDL is out of scope". `DROP` must never reach `conn.query`.
6. **Audit order** — assert the audit event is written *before* execution and that a failing writer aborts the statement (inject a writer whose directory cannot be created).

Integration (existing docker fixtures, `integration` feature): real `INSERT` into a temp table and rollback; `CALL` against a fixture procedure; GaussDB engine-level read-only check without the flag.

## Out of scope (per issue #58 §9)

MCP write tools (`execute_dml` / `call_procedure`), `--dangerously-skip-permissions` aliases, DDL/`TRUNCATE`/`GRANT`, transactions API, preview/confirm two-phase, replacing DB roles with string classification.

## Documentation

- README + UserGuide: `--allow-write` semantics, the L1/L2/L3 table, GaussDB GUC, audit fail-closed, "must pair with a low-privilege account".
- Keep the umbrella CLI↔MCP table saying "MCP read-only"; the gap becomes "writes / `CALL` only via CLI `--allow-write`".
