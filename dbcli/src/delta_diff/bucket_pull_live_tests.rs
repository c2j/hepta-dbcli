// ─── issue #129 端到端（DuckDB 内嵌）：失配桶合并拉取 ──
//
// 锁三件事：2 个失配桶 → 拉取是 1 对查询（queries_total == 6）；
// summary 与多重集合语义一致（含重复内容：不同内容各计 1，不按行数差）；
// 相同表 → 0 对拉取（queries_total == 4）。
//
// 落桶由内容哈希 MOD(hash, 3) 决定（threshold=4 → 12 行 → 3 桶），
// 数据按真实落桶固定：(r00#x, r06#x, r08#x, r09#x) → 桶 0；
// (r01, r02, r05, r07, r12) → 桶 1；(r03, r04, r10, r13) → 桶 2。

use std::sync::Arc;

use crate::backend::duckdb::DuckDbFactory;
use crate::backend::{BackendFactory, DbPool};
use crate::delta_diff::api::{run_diff, DiffOptions, SideInput};

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

fn keyless_opts() -> DiffOptions {
    DiffOptions {
        bisection_threshold: 4,
        ..Default::default()
    }
}

fn keyless_summary_opts() -> DiffOptions {
    DiffOptions {
        summary_only: true,
        bisection_threshold: 4,
        ..Default::default()
    }
}

fn bootstrap(path: &std::path::Path, tables: &[(&str, &[(&str, &str)])]) -> Result<(), String> {
    let boot = duckdb::Connection::open(path).map_err(|e| e.to_string())?;
    let mut batch = String::new();
    for (table, rows) in tables {
        batch.push_str(&format!(
            "CREATE TABLE {table} (k VARCHAR(8), v VARCHAR(8));"
        ));
        if !rows.is_empty() {
            let values: Vec<String> = rows.iter().map(|(k, v)| format!("('{k}','{v}')")).collect();
            batch.push_str(&format!("INSERT INTO {table} VALUES {};", values.join(",")));
        }
    }
    boot.execute_batch(&batch).map_err(|e| e.to_string())?;
    Ok(())
}

#[tokio::test]
async fn two_diff_buckets_pull_with_one_query_pair() -> Result<(), String> {
    // 12 行分 3 桶（threshold=4）；桶 0 与桶 2 各差 1 个内容，桶 1 相同。
    let left = [
        ("r00", "x"),
        ("r06", "x"),
        ("r08", "x"),
        ("r09", "x"), // 桶 0
        ("r01", "x"),
        ("r02", "x"),
        ("r05", "x"),
        ("r07", "x"), // 桶 1
        ("r03", "x"),
        ("r04", "x"),
        ("r10", "x"),
        ("r13", "x"), // 桶 2
    ];
    let right = [
        ("r06", "x"),
        ("r08", "x"),
        ("r09", "x"), // 桶 0，缺 r00
        ("r01", "x"),
        ("r02", "x"),
        ("r05", "x"),
        ("r07", "x"), // 桶 1
        ("r04", "x"),
        ("r10", "x"),
        ("r13", "x"), // 桶 2，缺 r03
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("issue129.duckdb");
    bootstrap(&path, &[("t_left", &left[..]), ("t_right", &right[..])]).expect("bootstrap");
    let pool = file_pool(&path).await.expect("pool");
    let url = format!("duckdb://{}", path.display());

    let report = run_diff(
        side_input(&pool, &url, "t_left").await?,
        side_input(&pool, &url, "t_right").await?,
        keyless_opts(),
    )
    .await
    .expect("run_diff");

    assert_eq!(report.strategy, "bucketdiff");
    assert_eq!(report.perf.queries_total, 6);
    assert_eq!(report.summary.left_total, 12);
    assert_eq!(report.summary.right_total, 10);
    assert_eq!(report.summary.missing_left, 0);
    assert_eq!(report.summary.missing_right, 2);
    assert_eq!(report.summary.modified, 0);
    let note = report
        .warnings
        .iter()
        .find(|w| w.starts_with("note: keyless table diff"))
        .expect("keyless note");
    assert!(
        note.starts_with("note: keyless table diff reports row-content multiset differences only"),
        "{note}"
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_contents_count_one_per_distinct_content() -> Result<(), String> {
    // 桶 0 的内容 (r00#x) 左 3 份右 1 份 → missing_right 只 +1（不是行数差 2）。
    let left = [
        ("r00", "x"),
        ("r00", "x"),
        ("r00", "x"), // 桶 0，3 份
        ("r01", "x"),
        ("r02", "x"),
        ("r05", "x"),
        ("r07", "x"),
        ("r12", "x"), // 桶 1
        ("r03", "x"),
        ("r04", "x"),
        ("r10", "x"),
        ("r13", "x"), // 桶 2
    ];
    let right = [
        ("r00", "x"), // 桶 0，1 份
        ("r01", "x"),
        ("r02", "x"),
        ("r05", "x"),
        ("r07", "x"),
        ("r12", "x"), // 桶 1
        ("r03", "x"),
        ("r04", "x"),
        ("r10", "x"),
        ("r13", "x"), // 桶 2
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("issue129_dup.duckdb");
    bootstrap(&path, &[("t_left", &left[..]), ("t_right", &right[..])]).expect("bootstrap");
    let pool = file_pool(&path).await.expect("pool");
    let url = format!("duckdb://{}", path.display());

    let report = run_diff(
        side_input(&pool, &url, "t_left").await?,
        side_input(&pool, &url, "t_right").await?,
        keyless_opts(),
    )
    .await
    .expect("run_diff");

    assert_eq!(report.perf.queries_total, 6);
    assert_eq!(
        report.summary.missing_right, 1,
        "distinct content counted once"
    );
    assert_eq!(report.summary.missing_left, 0);
    assert_eq!(report.summary.modified, 0);
    Ok(())
}

#[tokio::test]
async fn identical_tables_skip_the_pull_pair() -> Result<(), String> {
    let rows = [
        ("r00", "x"),
        ("r06", "x"),
        ("r08", "x"),
        ("r09", "x"),
        ("r01", "x"),
        ("r02", "x"),
        ("r05", "x"),
        ("r07", "x"),
        ("r03", "x"),
        ("r04", "x"),
        ("r10", "x"),
        ("r13", "x"),
    ];

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("issue129_same.duckdb");
    bootstrap(&path, &[("t_left", &rows[..]), ("t_right", &rows[..])]).expect("bootstrap");
    let pool = file_pool(&path).await.expect("pool");
    let url = format!("duckdb://{}", path.display());

    let report = run_diff(
        side_input(&pool, &url, "t_left").await?,
        side_input(&pool, &url, "t_right").await?,
        keyless_opts(),
    )
    .await
    .expect("run_diff");

    assert_eq!(report.perf.queries_total, 4);
    assert_eq!(report.summary.missing_right, 0);
    assert_eq!(report.summary.missing_left, 0);
    assert_eq!(report.summary.modified, 0);
    Ok(())
}

// ─── issue #129：keyless 内容哈希 IBLT（summary-only 单遍）──────────

#[tokio::test]
async fn keyless_summary_only_iblt_skips_bucket_pulls() -> Result<(), String> {
    // 5 vs 3 行、2 个不同内容只在左侧（各 1 份，净差 +1 可剥）→ IBLT 直接
    // 解码：queries_total == 4（2 COUNT + 2 IBLT；无 GROUP BY、无拉取）。
    let left = [
        ("r00", "x"),
        ("r01", "x"),
        ("r02", "x"),
        ("r03", "x"),
        ("r04", "x"),
    ];
    let right = [("r00", "x"), ("r01", "x"), ("r02", "x")];

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("issue129_summary.duckdb");
    bootstrap(&path, &[("t_left", &left[..]), ("t_right", &right[..])]).expect("bootstrap");
    let pool = file_pool(&path).await.expect("pool");
    let url = format!("duckdb://{}", path.display());

    let report = run_diff(
        side_input(&pool, &url, "t_left").await?,
        side_input(&pool, &url, "t_right").await?,
        keyless_summary_opts(),
    )
    .await
    .expect("run_diff");

    assert_eq!(report.strategy, "bucketdiff");
    assert_eq!(report.perf.queries_total, 4);
    assert_eq!(report.summary.missing_right, 2);
    assert_eq!(report.summary.missing_left, 0);
    assert_eq!(report.summary.modified, 0);
    let note = report
        .warnings
        .iter()
        .find(|w| w.starts_with("note: keyless table diff"))
        .expect("keyless note");
    assert!(note.contains("iblt"), "{note}");
    Ok(())
}

#[tokio::test]
async fn keyless_iblt_decode_failure_falls_back_to_combined_pull() -> Result<(), String> {
    // 同内容左 4 份右 2 份：净差 +2、key_xor=0、val_xor=0 → 剥不出 → 安全回退
    // Tier 1。queries_total == 8（2 COUNT + 2 白扫 IBLT + 2 GROUP BY + 2 拉取），
    // summary 与 Tier 1 语义一致（1 个不同内容 → missing_right=1）。
    let left = [
        ("a", "x"),
        ("a", "x"),
        ("a", "x"),
        ("a", "x"),
        ("b", "x"),
        ("c", "x"),
    ];
    let right = [("a", "x"), ("a", "x"), ("b", "x"), ("c", "x")];

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("issue129_fallback.duckdb");
    bootstrap(&path, &[("t_left", &left[..]), ("t_right", &right[..])]).expect("bootstrap");
    let pool = file_pool(&path).await.expect("pool");
    let url = format!("duckdb://{}", path.display());

    let report = run_diff(
        side_input(&pool, &url, "t_left").await?,
        side_input(&pool, &url, "t_right").await?,
        keyless_summary_opts(),
    )
    .await
    .expect("run_diff");

    assert_eq!(report.strategy, "bucketdiff");
    assert_eq!(report.perf.queries_total, 8);
    assert_eq!(report.summary.missing_right, 1);
    assert_eq!(report.summary.missing_left, 0);
    assert_eq!(report.summary.modified, 0);
    Ok(())
}
