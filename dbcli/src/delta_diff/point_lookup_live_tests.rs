// ─── issue #127 端到端（DuckDB 内嵌）：无键倾斜 summary-only 点查快路径 ──
//
// 夹具复刻 issue 现场：无主键表、左小右大、--where 过滤。断言精确的
// queries_total、多重集合记账方向、note 文本，以及与 MOD 分桶路径在同一份
// 全部差异内容互不重复的数据上汇总一致。

use std::sync::Arc;

use crate::backend::duckdb::DuckDbFactory;
use crate::backend::{BackendFactory, DbPool};
use crate::delta_diff::api::{run_diff, DiffOptions, SideInput};

const COLUMNS: &str = "(bcrq VARCHAR(16), zqdm VARCHAR(16), gddm VARCHAR(16), bs VARCHAR(4), \
     amt DECIMAL(16,2), note VARCHAR(64))";

async fn file_pool(path: &std::path::Path) -> Result<Arc<dyn DbPool>, String> {
    let url = format!("duckdb://{}", path.display());
    DuckDbFactory
        .connect(&url, None)
        .await
        .map_err(|e| e.to_string())
}

async fn side_input(pool: &Arc<dyn DbPool>, url: &str, table: &str) -> Result<SideInput, String> {
    let conn = acquire(pool).await?;
    Ok(SideInput {
        pool: pool.clone(),
        conn,
        name: "inline-duck".to_string(),
        schema: Some("main".to_string()),
        table: table.to_string(),
        connection_url: url.to_string(),
    })
}

async fn acquire(pool: &Arc<dyn DbPool>) -> Result<Box<dyn crate::backend::DbConn + Send>, String> {
    pool.acquire().await.map_err(|e| e.to_string())
}

fn point_opts(filter: &str, summary_only: bool) -> DiffOptions {
    DiffOptions {
        summary_only,
        fetch_all_threshold: 4096,
        filter: Some(filter.to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn keyless_skewed_summary_only_uses_point_lookup_end_to_end() -> Result<(), String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("issue127.duckdb");
    {
        let boot = duckdb::Connection::open(&path).expect("bootstrap create");
        boot.execute_batch(&format!(
            "CREATE TABLE diff_small {COLUMNS};
             CREATE TABLE diff_big {COLUMNS};
             CREATE INDEX idx_bcrq ON diff_big (bcrq, zqdm);
             INSERT INTO diff_small
               SELECT '20260122', 'S' || printf('%02d', g), 'G1', 'B',
                      CAST(1.5 + g AS DECIMAL(16,2)),
                      CASE WHEN g = 7 THEN NULL ELSE 'n' || g END
               FROM generate_series(1, 21) t(g);
             INSERT INTO diff_big SELECT * FROM diff_small;
             INSERT INTO diff_big
               SELECT '20260122', 'G' || g, 'G2', 'B',
                      CAST(9.5 + g AS DECIMAL(16,2)), 'x' || g
               FROM generate_series(1, 10000) t(g);"
        ))
        .expect("bootstrap fixture");
    }
    let pool = file_pool(&path).await.expect("pool");
    let url = format!("duckdb://{}", path.display());

    let report = run_diff(
        side_input(&pool, &url, "diff_small").await?,
        side_input(&pool, &url, "diff_big").await?,
        point_opts("bcrq = '20260122'", true),
    )
    .await
    .expect("run_diff");

    assert_eq!(report.strategy, "bucketdiff");
    // 2 次 COUNT + 1 次小表读回 + 21 条逐内容点查，无 MOD 分页。
    assert_eq!(report.perf.queries_total, 24);
    assert_eq!(report.summary.left_total, 21);
    assert_eq!(report.summary.right_total, 10021);
    assert_eq!(report.summary.missing_left, 10_000);
    assert_eq!(report.summary.missing_right, 0);
    assert_eq!(report.summary.modified, 0);
    assert!(report.sample_diffs.is_empty());
    let note = report
        .warnings
        .iter()
        .find(|w| w.starts_with("note: keyless table diff"))
        .expect("keyless note");
    assert!(
        note.contains("point-lookup small_rows=21 point_queries=21"),
        "{note}"
    );
    Ok(())
}

#[tokio::test]
async fn point_lookup_summary_matches_bucketdiff_on_distinct_content_data() -> Result<(), String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("issue127_eq.duckdb");
    {
        let boot = duckdb::Connection::open(&path).expect("bootstrap create");
        boot.execute_batch(&format!(
            "CREATE TABLE eq_small {COLUMNS};
             CREATE TABLE eq_big {COLUMNS};
             INSERT INTO eq_small
               SELECT '20260122', 'S' || printf('%02d', g), 'G1', 'B',
                      CAST(1.5 + g AS DECIMAL(16,2)), 'n' || g
               FROM generate_series(1, 21) t(g);
             INSERT INTO eq_big SELECT * FROM eq_small;
             INSERT INTO eq_big
               SELECT '20260122', 'G' || g, 'G2', 'B',
                      CAST(9.5 + g AS DECIMAL(16,2)), 'x' || g
               FROM generate_series(1, 10000) t(g);"
        ))
        .expect("bootstrap fixture");
    }
    let pool = file_pool(&path).await.expect("pool");
    let url = format!("duckdb://{}", path.display());

    let point = run_diff(
        side_input(&pool, &url, "eq_small").await?,
        side_input(&pool, &url, "eq_big").await?,
        point_opts("bcrq = '20260122'", true),
    )
    .await
    .expect("point-lookup run");
    let bucket = run_diff(
        side_input(&pool, &url, "eq_small").await?,
        side_input(&pool, &url, "eq_big").await?,
        point_opts("bcrq = '20260122'", false),
    )
    .await
    .expect("bucketdiff run");

    assert_eq!(point.summary.missing_left, bucket.summary.missing_left);
    assert_eq!(point.summary.missing_right, bucket.summary.missing_right);
    assert_eq!(point.summary.modified, 0);
    assert_eq!(bucket.summary.modified, 0);
    assert_eq!(point.summary.missing_left, 10_000);
    Ok(())
}

#[tokio::test]
async fn point_lookup_handles_duplicate_contents_and_null_columns() -> Result<(), String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("issue127_dup.duckdb");
    {
        let boot = duckdb::Connection::open(&path).expect("bootstrap create");
        boot.execute_batch(&format!(
            "CREATE TABLE dup_small {COLUMNS};
             CREATE TABLE dup_big {COLUMNS};
             INSERT INTO dup_small VALUES
               ('20260122', 'D01', 'G1', 'B', 1.11, NULL),
               ('20260122', 'D01', 'G1', 'B', 1.11, NULL),
               ('20260122', 'D02', 'G1', 'B', 2.22, 'n2'),
               ('20260122', 'D03', 'G1', 'B', 3.33, NULL),
               ('20260122', 'D04', 'G1', 'B', 4.44, 'n4');
             INSERT INTO dup_big SELECT * FROM dup_small;
             INSERT INTO dup_big
               SELECT '20260122', 'E' || g, 'G2', 'B',
                      CAST(5.5 + g AS DECIMAL(16,2)), NULL
               FROM generate_series(1, 5000) t(g);"
        ))
        .expect("bootstrap fixture");
    }
    let pool = file_pool(&path).await.expect("pool");
    let url = format!("duckdb://{}", path.display());

    let report = run_diff(
        side_input(&pool, &url, "dup_small").await?,
        side_input(&pool, &url, "dup_big").await?,
        point_opts("bcrq = '20260122'", true),
    )
    .await
    .expect("run_diff");

    // 5 行小表但只有 4 种内容：2 次 COUNT + 1 次读回 + 4 条点查。
    assert_eq!(report.perf.queries_total, 7);
    assert_eq!(report.summary.missing_right, 0);
    assert_eq!(report.summary.missing_left, 5_000);
    assert_eq!(report.summary.modified, 0);
    let note = report
        .warnings
        .iter()
        .find(|w| w.starts_with("note: keyless table diff"))
        .expect("keyless note");
    assert!(
        note.contains("point-lookup small_rows=5 point_queries=4"),
        "{note}"
    );
    Ok(())
}
