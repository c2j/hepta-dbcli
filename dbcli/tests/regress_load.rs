// End-to-end regression tests for `hepta_dbcli load` (issue #98).
//
// The DuckDB section drives the REAL binary (`CARGO_BIN_EXE_hepta_dbcli`)
// against an embedded DuckDB file: bootstrap schema → write data dir →
// run `load` with `--config`/`--audit-dir` pointed at tempdirs (never the
// user's real ~/.hepta-dbcli.toml or audit dir) → reopen the file and SELECT.
//
// Run: cargo test --features "duckdb,integration" --test regress_load
// (the MySQL-only module also compiles with plain `--features integration`
// and is what the CI `test` job runs against the service container)

#[cfg(all(feature = "integration", feature = "duckdb"))]
mod duckdb_tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const BIN: &str = env!("CARGO_BIN_EXE_hepta_dbcli");

    // ─── Shared helpers ─────────────────────────────────────────────────

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(path, content).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }

    /// Bootstrap `<root>/shop.duckdb` with the shop schema (users ← orders FK).
    /// Optionally pre-seeds one orders row (id 1) to prove duplicate-PK
    /// failures inside a data file are caught.
    fn bootstrap_shop_db(root: &Path, seed_order: bool) {
        let db_path = root.join("shop.duckdb");
        let boot = duckdb::Connection::open(&db_path).expect("bootstrap open");
        boot.execute_batch(
            "CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, \
             balance DECIMAL(10,2), note VARCHAR, created_at TIMESTAMP);
             CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT REFERENCES users(id), \
             amount DECIMAL(8,2));",
        )
        .expect("bootstrap tables");
        if seed_order {
            boot.execute_batch("INSERT INTO orders VALUES (1, NULL, 1.00);")
                .expect("seed order");
        }
    }

    /// `<root>/cfg.toml` with a single named connection `test` to the shop DB.
    /// Written into the workspace root so the user's real config is never read.
    fn write_config(root: &Path) -> PathBuf {
        let cfg = root.join("cfg.toml");
        let url = format!(
            "[connections.test]\nurl = \"duckdb://{}\"\n",
            root.join("shop.duckdb").display()
        );
        write_file(&cfg, &url);
        cfg
    }

    fn data_dir(root: &Path) -> PathBuf {
        root.join("data")
    }

    /// Run the real binary against the workspace: global flags first, then
    /// the `load` subcommand. HOME is isolated so config resolution never
    /// sees the developer's real file even by accident.
    fn run_load(
        root: &Path,
        extra_load_args: &[&str],
    ) -> (std::process::ExitStatus, String, String) {
        let out = Command::new(BIN)
            .env("HOME", root) // no ~/.hepta-dbcli.toml leakage
            .env_remove("HEPTA_DBCLI_URL")
            .args([
                "--config",
                root.join("cfg.toml").to_str().expect("cfg path"),
                "--audit-dir",
                root.join("audit").to_str().expect("audit path"),
                "--allow-write",
            ])
            .args(extra_load_args)
            .output()
            .expect("spawn hepta_dbcli");
        (
            out.status,
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn run_load_jsonl(root: &Path, extra: &[&str]) -> (std::process::ExitStatus, String, String) {
        let data_path = data_dir(root).to_string_lossy().into_owned();
        let mut args = vec!["load", "--data", data_path.as_str(), "--name", "test"];
        args.extend_from_slice(extra);
        run_load(root, &args)
    }

    /// Reopen the shop DB directly and SELECT a single scalar.
    fn query_scalar(root: &Path, sql: &str) -> serde_json::Value {
        let conn = duckdb::Connection::open(root.join("shop.duckdb")).expect("reopen");
        conn.query_row(sql, [], |row| {
            let v: duckdb::types::Value = row.get(0)?;
            match v {
                duckdb::types::Value::Null => Ok(serde_json::Value::Null),
                duckdb::types::Value::BigInt(i) => Ok(serde_json::json!(i)),
                duckdb::types::Value::Double(f) => Ok(serde_json::json!(f)),
                duckdb::types::Value::Text(s) => Ok(serde_json::json!(s)),
                duckdb::types::Value::Decimal(d) => Ok(serde_json::json!(d.to_string())),
                duckdb::types::Value::Timestamp(_, micros) => Ok(serde_json::json!(
                    chrono::DateTime::from_timestamp_micros(micros)
                        .expect("timestamp")
                        .naive_utc()
                        .to_string()
                )),
                other => Ok(serde_json::Value::String(format!("{other:?}"))),
            }
        })
        .unwrap_or(serde_json::Value::Null)
    }

    fn count_rows(root: &Path, table: &str) -> i64 {
        match query_scalar(root, &format!("SELECT count(*) FROM {table}")) {
            serde_json::Value::Number(n) => n.as_i64().expect("integer count"),
            other => panic!("count for {table} was not a number: {other}"),
        }
    }

    /// Two-row users + two-row orders data set shared by several tests.
    /// File names matter: orders < users alphabetically, so a name-ordered
    /// loader would load orders first and violate the FK — a pass proves the
    /// topological plan wins.
    fn write_shop_data(root: &Path) {
        let dir = data_dir(root);
        write_file(
            &dir.join("orders.jsonl"),
            concat!(
                "{\"id\": 10, \"user_id\": 1, \"amount\": \"19.99\"}\n",
                "{\"id\": 11, \"user_id\": 2, \"amount\": \"5.50\"}\n",
            ),
        );
        write_file(
            &dir.join("users.jsonl"),
            concat!(
                "{\"id\": 1, \"name\": \"alice\", \"balance\": \"12345.67\", \
                 \"note\": null, \"created_at\": \"2024-01-02 03:04:05\"}\n",
                "{\"id\": 2, \"name\": \"bob\", \"balance\": \"0.05\", \
                 \"note\": \"\", \"created_at\": \"2024-06-01 12:00:00\"}\n",
            ),
        );
    }

    fn shop_workspace(root: &Path) {
        bootstrap_shop_db(root, false);
        write_shop_data(root);
        write_config(root);
    }

    // ─── Tests ──────────────────────────────────────────────────────────

    #[test]
    fn load_duckdb_jsonl_end_to_end_fk_order_and_value_fidelity() {
        let root = tempfile::tempdir().expect("tempdir");
        shop_workspace(root.path());

        let (status, stdout, stderr) = run_load_jsonl(root.path(), &[]);
        assert!(
            status.success(),
            "load should succeed, stderr: {stderr}, stdout: {stdout}"
        );
        assert_eq!(count_rows(root.path(), "users"), 2, "users row count");
        assert_eq!(count_rows(root.path(), "orders"), 2, "orders row count");

        // Value fidelity after the round-trip.
        assert_eq!(
            query_scalar(root.path(), "SELECT name FROM users WHERE id = 1"),
            json_str("alice")
        );
        // DECIMAL precision preserved (not f64-rounded).
        assert_eq!(
            query_scalar(root.path(), "SELECT balance FROM users WHERE id = 1"),
            json_str("12345.67"),
            "DECIMAL(10,2) must round-trip exactly"
        );
        // NULL vs empty string stay distinct.
        let note1 = query_scalar(root.path(), "SELECT note FROM users WHERE id = 1");
        let note2 = query_scalar(root.path(), "SELECT note FROM users WHERE id = 2");
        assert!(note1.is_null(), "NULL note must stay NULL, got {note1:?}");
        assert_eq!(note2, json_str(""), "quoted empty string must stay ''");
        // TIMESTAMP round-trips.
        assert_eq!(
            query_scalar(root.path(), "SELECT created_at FROM users WHERE id = 1"),
            json_str("2024-01-02 03:04:05")
        );
        // FK row landed with correct reference and decimal.
        assert_eq!(
            query_scalar(root.path(), "SELECT user_id FROM orders WHERE id = 10"),
            json_num(1)
        );
        assert_eq!(
            query_scalar(root.path(), "SELECT amount FROM orders WHERE id = 10"),
            json_str("19.99")
        );
        // Success summary on stdout.
        assert!(
            stdout.contains("loaded 4 rows into 2 table(s)"),
            "unexpected stdout: {stdout}"
        );
    }

    #[test]
    fn load_duckdb_without_allow_write_exits_2() {
        let root = tempfile::tempdir().expect("tempdir");
        shop_workspace(root.path());

        let cfg_path = root.path().join("cfg.toml");
        let audit_path = root.path().join("audit");
        let data_path = data_dir(root.path());
        let args = [
            "--config",
            cfg_path.to_str().expect("cfg"),
            "--audit-dir",
            audit_path.to_str().expect("audit"),
            "load",
            "--data",
            data_path.to_str().expect("data"),
            "--name",
            "test",
        ];
        let out = Command::new(BIN)
            .env("HOME", root.path())
            .env_remove("HEPTA_DBCLI_URL")
            .args(args)
            .output()
            .expect("spawn hepta_dbcli");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

        assert_eq!(
            out.status.code(),
            Some(2),
            "missing --allow-write must exit 2, stderr: {stderr}"
        );
        assert!(
            stderr.contains("--allow-write"),
            "stderr must name the flag: {stderr}"
        );
        assert_eq!(
            count_rows(root.path(), "users"),
            0,
            "nothing may be written"
        );
        assert_eq!(
            count_rows(root.path(), "orders"),
            0,
            "nothing may be written"
        );
    }

    #[test]
    fn load_duckdb_dry_run_touches_no_rows() {
        let root = tempfile::tempdir().expect("tempdir");
        shop_workspace(root.path());

        let (status, stdout, stderr) = run_load_jsonl(root.path(), &["--dry-run"]);
        assert!(
            status.success(),
            "dry run should exit 0, stderr: {stderr}, stdout: {stdout}"
        );
        // Per-table plan lines: table name, row count, file path.
        assert!(
            stdout.contains("users (2 rows"),
            "plan line missing: {stdout}"
        );
        assert!(
            stdout.contains("orders (2 rows"),
            "plan line missing: {stdout}"
        );
        // FK-topo order visible in the plan itself: users before orders even
        // though the file scan is alphabetical.
        let users_pos = stdout.find("users (").expect("users in plan");
        let orders_pos = stdout.find("orders (").expect("orders in plan");
        assert!(
            users_pos < orders_pos,
            "plan must order users before orders"
        );
        // And the database was not touched.
        assert_eq!(
            count_rows(root.path(), "users"),
            0,
            "dry run must not write"
        );
        assert_eq!(
            count_rows(root.path(), "orders"),
            0,
            "dry run must not write"
        );
    }

    #[test]
    fn load_duckdb_fail_fast_rolls_back_failed_table() {
        let root = tempfile::tempdir().expect("tempdir");
        shop_workspace(root.path());
        // Pre-seed orders id=1; the data file re-offers id=1 in its third row,
        // so users must fully commit first, then orders hits a duplicate PK.
        // The seeding connection is scoped: DuckDB holds an exclusive file
        // lock, so it must be dropped before the binary opens the database.
        {
            let boot = duckdb::Connection::open(root.path().join("shop.duckdb")).expect("reopen");
            boot.execute_batch("INSERT INTO orders VALUES (1, NULL, 1.00);")
                .expect("seed order");
        }
        write_file(
            &data_dir(root.path()).join("orders.jsonl"),
            concat!(
                "{\"id\": 10, \"user_id\": 1, \"amount\": \"19.99\"}\n",
                "{\"id\": 11, \"user_id\": 2, \"amount\": \"5.50\"}\n",
                "{\"id\": 1, \"user_id\": 1, \"amount\": \"2.00\"}\n",
            ),
        );

        let (status, stdout, stderr) = run_load_jsonl(root.path(), &[]);
        let code = status.code();
        assert_eq!(code, Some(1), "duplicate PK must exit 1, stdout: {stdout}");
        // stderr names both the completed table and the failed one.
        assert!(stderr.contains("table 'orders' failed"), "stderr: {stderr}");
        assert!(stderr.contains("completed: users"), "stderr: {stderr}");
        // Fail-fast: users stays committed, the failed table rolled back.
        assert_eq!(
            count_rows(root.path(), "users"),
            2,
            "users must be committed"
        );
        assert_eq!(
            count_rows(root.path(), "orders"),
            1,
            "orders must hold only the seed row"
        );
    }

    #[test]
    fn load_duckdb_column_mismatch_fails_cleanly() {
        let root = tempfile::tempdir().expect("tempdir");
        shop_workspace(root.path());
        // users file carries an extra column the table does not have.
        write_file(
            &data_dir(root.path()).join("users.jsonl"),
            concat!(
                "{\"id\": 1, \"name\": \"alice\", \"balance\": \"1.00\", \"note\": null, \
                 \"created_at\": \"2024-01-02 03:04:05\", \"email\": \"a@example.com\"}\n",
            ),
        );

        let (status, _stdout, stderr) = run_load_jsonl(root.path(), &[]);
        let code = status.code();
        assert_eq!(
            code,
            Some(1),
            "column mismatch must exit 1, stderr: {stderr}"
        );
        assert!(
            stderr.contains("users") && stderr.contains("email"),
            "stderr must name the table and the extra column: {stderr}"
        );
        // Nothing was loaded (validation is fail-closed, before any INSERT).
        assert_eq!(count_rows(root.path(), "users"), 0);
        assert_eq!(count_rows(root.path(), "orders"), 0);
    }

    #[test]
    fn load_duckdb_csv_type_coercion() {
        let root = tempfile::tempdir().expect("tempdir");
        shop_workspace(root.path());
        // Only users, as CSV: numeric strings must coerce to BIGINT, the
        // decimal string must keep its exact form, and the empty unquoted
        // note field is NULL while the quoted "" stays an empty string.
        write_file(
            &data_dir(root.path()).join("users.csv"),
            concat!(
                "id,name,balance,note,created_at\n",
                "1,alice,12345.67,,2024-01-02 03:04:05\n",
                "2,bob,0.05,\"\",2024-06-01 12:00:00\n",
            ),
        );
        // Orders stays JSONL; --tables pins the load set to users.
        write_file(
            &data_dir(root.path()).join("orders.jsonl"),
            "{\"id\": 10, \"user_id\": 1, \"amount\": \"19.99\"}\n",
        );

        let (status, stdout, stderr) = run_load_jsonl(root.path(), &["--tables", "users"]);
        assert!(
            status.success(),
            "csv load should succeed, stderr: {stderr}, stdout: {stdout}"
        );
        assert!(
            stdout.contains("loaded 2 rows into 1 table(s)"),
            "stdout: {stdout}"
        );
        assert_eq!(count_rows(root.path(), "users"), 2);
        assert_eq!(
            count_rows(root.path(), "orders"),
            0,
            "--tables users must skip orders"
        );

        assert_eq!(
            query_scalar(root.path(), "SELECT id FROM users WHERE name = 'alice'"),
            json_num(1)
        );
        assert_eq!(
            query_scalar(root.path(), "SELECT balance FROM users WHERE id = 1"),
            json_str("12345.67"),
            "csv decimal string must keep exact precision"
        );
        let note1 = query_scalar(root.path(), "SELECT note FROM users WHERE id = 1");
        let note2 = query_scalar(root.path(), "SELECT note FROM users WHERE id = 2");
        assert!(
            note1.is_null(),
            "empty unquoted csv field is NULL, got {note1:?}"
        );
        assert_eq!(note2, json_str(""), "quoted empty csv field is ''");
    }

    // ─── --name must route to the named connection (two-connection rig) ──

    /// Config with `default_connection = "dev"` plus a second `prod`
    /// connection. `load --name prod` must write to prod and never to dev.
    fn write_two_connection_config(root: &Path) {
        let dev = root.join("dev.duckdb");
        let prod = root.join("prod.duckdb");
        boot_db_file(&dev);
        boot_db_file(&prod);
        let cfg = root.join("two.toml");
        write_file(
            &cfg,
            &format!(
                "default_connection = \"dev\"\n\n[connections.dev]\nurl = \"duckdb://{}\"\n\n[connections.prod]\nurl = \"duckdb://{}\"\n",
                dev.display(),
                prod.display()
            ),
        );
        write_file(&root.join("data/items.csv"), "id,name\n1,alpha\n2,beta\n");
    }

    /// A DuckDB file with the `items` table (id, name).
    fn boot_db_file(path: &Path) {
        let conn = duckdb::Connection::open(path).expect("bootstrap open");
        conn.execute_batch("CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR);")
            .expect("bootstrap items");
    }

    fn count_items_in(db: &Path) -> i64 {
        let conn = duckdb::Connection::open(db).expect("reopen");
        conn.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
            .expect("count")
    }

    #[test]
    fn should_route_load_to_named_connection() {
        let root = tempfile::tempdir().expect("tempdir");
        write_two_connection_config(root.path());

        let out = Command::new(BIN)
            .env("HOME", root.path())
            .env_remove("HEPTA_DBCLI_URL")
            .args([
                "--config",
                root.path().join("two.toml").to_str().unwrap(),
                "--audit-dir",
                root.path().join("audit").to_str().unwrap(),
                "load",
                "--allow-write",
                "--name",
                "prod",
                "--data",
                root.path().join("data").to_str().unwrap(),
            ])
            .output()
            .expect("spawn hepta_dbcli");
        assert!(
            out.status.success(),
            "load --name prod failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        assert_eq!(
            count_items_in(&root.path().join("prod.duckdb")),
            2,
            "rows must land in the connection named on the command line"
        );
        assert_eq!(
            count_items_in(&root.path().join("dev.duckdb")),
            0,
            "the default connection must stay untouched when --name is given"
        );
    }

    #[test]
    fn should_fail_load_when_named_connection_missing() {
        let root = tempfile::tempdir().expect("tempdir");
        write_two_connection_config(root.path());

        let out = Command::new(BIN)
            .env("HOME", root.path())
            .env_remove("HEPTA_DBCLI_URL")
            .args([
                "--config",
                root.path().join("two.toml").to_str().unwrap(),
                "--audit-dir",
                root.path().join("audit").to_str().unwrap(),
                "load",
                "--allow-write",
                "--name",
                "doesnotexist",
                "--data",
                root.path().join("data").to_str().unwrap(),
            ])
            .output()
            .expect("spawn hepta_dbcli");

        assert!(
            !out.status.success(),
            "load --name <unknown> must fail loudly, not silently use the default"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("doesnotexist"),
            "error must name the missing connection, got: {stderr}"
        );
    }

    // ─── Tiny JSON literals (kept local to avoid a serde_json re-export) ──

    fn json_str(s: &str) -> serde_json::Value {
        serde_json::Value::String(s.to_string())
    }

    fn json_num(n: i64) -> serde_json::Value {
        serde_json::Value::Number(n.into())
    }
}

// ─── MySQL (env-gated) ──────────────────────────────────────────────────
// Gated on `integration` only (no duckdb requirement): the CI `test` job has
// the MySQL service but not the duckdb feature, so this module must compile
// there. It self-skips without HEPTA_DBCLI_TEST_URL.
#[cfg(feature = "integration")]
mod mysql_tests {
    use std::process::Command;

    const BIN: &str = env!("CARGO_BIN_EXE_hepta_dbcli");

    /// URL for the integration MySQL server, or None when unset (test skips).
    fn mysql_url() -> Option<String> {
        std::env::var("HEPTA_DBCLI_TEST_URL").ok()
    }

    fn table_suffix() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(1);
        format!("{}", NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Minimal mirror of the DuckDB happy-path test: FK-topo order, NULL vs
    /// '' distinct, decimal precision. Lowercase names, explicit PKs, no
    /// AUTO_INCREMENT (deterministic round-trip).
    #[tokio::test]
    async fn load_mysql_jsonl_end_to_end_fk_order_and_value_fidelity() {
        let Some(url) = mysql_url() else {
            eprintln!("skipping: HEPTA_DBCLI_TEST_URL not set");
            return;
        };
        let suffix = table_suffix();
        let users_table = format!("load_e2e_users_{suffix}");
        let orders_table = format!("load_e2e_orders_{suffix}");

        let root = tempfile::tempdir().expect("tempdir");
        let cfg_path = root.path().join("cfg.toml");
        let cfg = format!(
            "[connections.test]\nurl = \"{}\"\n",
            // Reuse the integration server; schema comes from the URL path.
            url
        );
        std::fs::write(&cfg_path, cfg).expect("write cfg");

        // Bootstrap via mysql_async directly (the binary has no DDL mode).
        let opts = mysql_async::Opts::from_url(&url).expect("parse test url");
        let mut boot = mysql_async::Conn::new(opts)
            .await
            .expect("bootstrap connect");
        {
            use mysql_async::prelude::Queryable;
            // A previous crashed run may have left the tables behind.
            boot.query_drop(format!("DROP TABLE IF EXISTS {orders_table}"))
                .await
                .ok();
            boot.query_drop(format!("DROP TABLE IF EXISTS {users_table}"))
                .await
                .ok();
            boot.query_drop(format!(
                "CREATE TABLE {users_table} (id BIGINT PRIMARY KEY, name VARCHAR(100), \
                 balance DECIMAL(10,2), note VARCHAR(100), created_at DATETIME)"
            ))
            .await
            .expect("create users");
            boot.query_drop(format!(
                "CREATE TABLE {orders_table} (id BIGINT PRIMARY KEY, user_id BIGINT, \
                 amount DECIMAL(8,2), FOREIGN KEY (user_id) REFERENCES {users_table}(id))"
            ))
            .await
            .expect("create orders");
        }
        drop(boot);

        let data = root.path().join("data");
        std::fs::create_dir_all(&data).expect("mkdir data");
        // orders first alphabetically: a name-ordered loader would fail FK.
        std::fs::write(
            data.join(format!("{orders_table}.jsonl")),
            format!(
                "{{\"id\": 10, \"user_id\": 1, \"amount\": \"19.99\"}}\n\
                 {{\"id\": 11, \"user_id\": 2, \"amount\": \"5.50\"}}\n"
            ),
        )
        .expect("write orders");
        std::fs::write(
            data.join(format!("{users_table}.jsonl")),
            concat!(
                "{\"id\": 1, \"name\": \"alice\", \"balance\": \"12345.67\", \
                 \"note\": null, \"created_at\": \"2024-01-02 03:04:05\"}\n",
                "{\"id\": 2, \"name\": \"bob\", \"balance\": \"0.05\", \
                 \"note\": \"\", \"created_at\": \"2024-06-01 12:00:00\"}\n",
            ),
        )
        .expect("write users");

        let out = Command::new(BIN)
            .env("HOME", root.path()) // no real ~/.hepta-dbcli.toml
            .env_remove("HEPTA_DBCLI_URL")
            .args([
                "--config",
                cfg_path.to_str().expect("cfg"),
                "--audit-dir",
                root.path().join("audit").to_str().expect("audit"),
                "--allow-write",
                "load",
                "--data",
                data.to_str().expect("data"),
                "--name",
                "test",
            ])
            .output()
            .expect("spawn hepta_dbcli");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "load should succeed, stderr: {stderr}, stdout: {stdout}"
        );
        // The FK order must be visible in the run itself: orders loads only
        // after users, regardless of alphabetical file order.
        let users_pos = stdout.find(&users_table).expect("users in stdout");
        let orders_pos = stdout.find(&orders_table).expect("orders in stdout");
        assert!(
            users_pos < orders_pos,
            "users must be reported before orders: {stdout}"
        );
        assert!(
            stdout.contains(&format!("loaded 4 rows into 2 table(s)")),
            "unexpected stdout: {stdout}"
        );

        // Verify through the driver.
        let opts = mysql_async::Opts::from_url(&url).expect("parse test url");
        let mut conn = mysql_async::Conn::new(opts).await.expect("verify connect");
        let (users_count, orders_count, order_amount, note_null, note_empty): (
            u64,
            u64,
            String,
            Option<String>,
            Option<String>,
        ) = {
            use mysql_async::prelude::Queryable;
            let users_count: u64 = conn
                .query_first(format!("SELECT count(*) FROM {users_table}"))
                .await
                .expect("count users")
                .expect("count row");
            let orders_count: u64 = conn
                .query_first(format!("SELECT count(*) FROM {orders_table}"))
                .await
                .expect("count orders")
                .expect("count row");
            let amount: (String,) = conn
                .query_first(format!("SELECT amount FROM {orders_table} WHERE id = 10"))
                .await
                .expect("amount")
                .expect("amount row");
            let n1: (Option<String>,) = conn
                .query_first(format!("SELECT note FROM {users_table} WHERE id = 1"))
                .await
                .expect("note1")
                .expect("row");
            let n2: (Option<String>,) = conn
                .query_first(format!("SELECT note FROM {users_table} WHERE id = 2"))
                .await
                .expect("note2")
                .expect("row");
            (users_count, orders_count, amount.0, n1.0, n2.0)
        };
        assert_eq!(users_count, 2);
        assert_eq!(orders_count, 2);
        assert_eq!(order_amount, "19.99", "decimal must round-trip exactly");
        assert!(note_null.is_none(), "JSON null must load as SQL NULL");
        assert_eq!(note_empty.as_deref(), Some(""), "quoted '' must stay ''");

        // Cleanup.
        use mysql_async::prelude::Queryable;
        conn.query_drop(format!("DROP TABLE IF EXISTS {orders_table}"))
            .await
            .ok();
        conn.query_drop(format!("DROP TABLE IF EXISTS {users_table}"))
            .await
            .ok();
    }
}
