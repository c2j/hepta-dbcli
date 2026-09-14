// GaussDB/openGauss regression test suite
// Run: GAUSSDB_TEST_URL="host=127.0.0.1 port=5432 user=gaussdb password=testpass@123 dbname=testdb"
//        cargo test --features "gaussdb,integration" --test regress_gaussdb

#[cfg(all(feature = "integration", feature = "gaussdb"))]
mod common;

#[cfg(all(feature = "integration", feature = "gaussdb"))]
mod tests {
    use crate::common;
    use gaussdb::NoTls;

    const TABLE: &str = "regress_test";

    fn gaussdb_url() -> Option<String> {
        std::env::var("GAUSSDB_TEST_URL").ok()
    }

    async fn client() -> Option<gaussdb::Client> {
        let url = gaussdb_url()?;
        let (client, connection) = gaussdb::connect(&url, NoTls).await.ok()?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Some(client)
    }

    async fn setup() -> Option<gaussdb::Client> {
        let c = client().await?;
        let _ = c
            .simple_query(&format!("DROP TABLE IF EXISTS {}", TABLE))
            .await;
        c.simple_query(&format!(
            "CREATE TABLE {} (id SERIAL PRIMARY KEY, name VARCHAR(100), amount NUMERIC(10,2), active BOOLEAN DEFAULT TRUE, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP, data JSONB)",
            TABLE
        )).await.expect("create table");
        c.execute(
            &format!(
                "INSERT INTO {} (name, amount, active) VALUES ($1, $2, $3), ($4, $5, $6)",
                TABLE
            ),
            &[
                &"alice" as &(dyn gaussdb::types::ToSql + Sync),
                &99.99_f64,
                &true,
                &"bob",
                &50.0_f64,
                &false,
            ],
        )
        .await
        .expect("insert data");
        Some(c)
    }

    async fn teardown(c: gaussdb::Client) {
        let _ = c
            .simple_query(&format!("DROP TABLE IF EXISTS {}", TABLE))
            .await;
    }

    #[tokio::test]
    async fn gaussdb_database_info() {
        let Some(c) = setup().await else {
            return;
        };
        let rows = c.query(
            "SELECT version()::text, current_database()::text, current_user::text, inet_server_addr()::text, inet_server_port()::text, NULL::text, (SELECT setting FROM pg_settings WHERE name='server_encoding')::text, (SELECT setting FROM pg_settings WHERE name='lc_collate')::text, NULL::text",
            &[],
        ).await.expect("database_info");
        assert!(!rows.is_empty());
        let cols: Vec<String> = rows[0]
            .columns()
            .iter()
            .map(|col| col.name().to_string())
            .collect();
        let expected = &[
            "version",
            "database",
            "current_user",
            "hostname",
            "port",
            "os",
            "charset",
            "collation",
            "version_comment",
        ];
        common::assert_columns(&cols, expected);
        teardown(c).await;
    }

    #[tokio::test]
    async fn gaussdb_list_tables() {
        let Some(c) = setup().await else {
            return;
        };
        let rows = c.query(
            "SELECT n.nspname, c.relname, CASE c.relkind WHEN 'r' THEN 'table' END, NULL, c.reltuples::bigint, pg_total_relation_size(c.oid), obj_description(c.oid, 'pg_class') FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind = 'r' AND n.nspname = 'public' ORDER BY c.relname",
            &[],
        ).await.expect("list_tables");
        assert!(!rows.is_empty());
        let names: Vec<String> = rows.iter().map(|r| r.get::<_, String>(1)).collect();
        assert!(names.contains(&TABLE.to_string()));
        teardown(c).await;
    }

    #[tokio::test]
    async fn gaussdb_add_limit() {
        let Some(c) = setup().await else {
            return;
        };
        let rows = c
            .query(&format!("SELECT * FROM {} LIMIT 1", TABLE), &[])
            .await
            .expect("limit query");
        assert!(!rows.is_empty());
        teardown(c).await;
    }

    #[tokio::test]
    async fn gaussdb_read_only_prefixes() {
        let Some(url) = gaussdb_url() else {
            return;
        };
        use polar_mysql::backend::BackendFactory;
        use std::sync::Arc;
        let factory: Arc<dyn BackendFactory> =
            Arc::new(polar_mysql::backend::gaussdb::GaussdbFactory);
        let mut registry = polar_mysql::backend::factory::BackendRegistry::new();
        registry.register(factory);
        let pool = registry
            .connect_with_fallback("gaussdb", &url, None, false)
            .await
            .expect("connect");
        let conn = pool.acquire().await.expect("acquire");
        let prefixes = conn.dialect().read_only_prefixes();
        assert!(prefixes.contains(&"SELECT"));
        assert!(prefixes.contains(&"EXPLAIN"));
        assert!(prefixes.contains(&"WITH"));
        assert!(!prefixes.contains(&"SHOW"));
    }

    #[tokio::test]
    async fn gaussdb_build_explain() {
        let Some(c) = setup().await else {
            return;
        };
        let rows = c
            .query("EXPLAIN (FORMAT JSON) SELECT 1", &[])
            .await
            .expect("explain");
        assert!(!rows.is_empty());
        teardown(c).await;
    }

    #[tokio::test]
    async fn gaussdb_query_error() {
        let Some(c) = setup().await else {
            return;
        };
        let result = c.query("SELECT * FROM nonexistent_table_xyz", &[]).await;
        assert!(result.is_err());
        teardown(c).await;
    }
}
