// Shared test utilities for regression tests.

use std::sync::Arc;
use std::time::Duration;

/// Assert that `actual` columns match `expected` column names (order and count).
pub fn assert_columns(actual: &[String], expected: &[&str]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "column count mismatch: expected {:?}, got {:?}",
        expected,
        actual
    );
    for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            a, e,
            "column[{}] mismatch: expected '{}', got '{}'",
            i, e, a
        );
    }
}

/// Wait for health. Useful after Fixture creation.
#[allow(dead_code)]
pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Connect to the backend using registration-free factory pattern.
/// Used by tests that have a direct connection to a backend.
#[allow(dead_code)]
pub async fn connect_pool<F>(factory: F, url: &str) -> Arc<dyn polar_mysql::backend::DbPool>
where
    F: polar_mysql::backend::BackendFactory + 'static,
{
    let factory: Arc<dyn polar_mysql::backend::BackendFactory> = Arc::new(factory);
    let mut registry = polar_mysql::backend::factory::BackendRegistry::new();
    registry.register(factory);
    registry
        .connect_with_fallback(&url[..url.find("://").unwrap_or(0)], url, None, false)
        .await
        .expect("failed to connect")
}
