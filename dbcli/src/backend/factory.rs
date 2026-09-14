use std::collections::HashMap;
use std::sync::Arc;

use super::{BackendFactory, DbPool};
use crate::config::TimeoutConfig;

#[derive(Default)]
pub struct BackendRegistry {
    by_scheme: HashMap<String, Vec<Arc<dyn BackendFactory>>>,
    by_name: HashMap<String, Arc<dyn BackendFactory>>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, factory: Arc<dyn BackendFactory>) {
        let scheme = factory.scheme().to_lowercase();
        let name = factory.name().to_lowercase();
        self.by_scheme
            .entry(scheme)
            .or_default()
            .push(Arc::clone(&factory));
        self.by_name.insert(name, factory);
    }

    pub fn get_by_scheme(&self, scheme: &str) -> Option<&Arc<dyn BackendFactory>> {
        self.by_scheme
            .get(&scheme.to_lowercase())
            .and_then(|factories| factories.first())
    }

    pub fn get_by_scheme_all(&self, scheme: &str) -> &[Arc<dyn BackendFactory>] {
        self.by_scheme
            .get(&scheme.to_lowercase())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn get_by_name(&self, name: &str) -> Option<&Arc<dyn BackendFactory>> {
        self.by_name.get(&name.to_lowercase())
    }

    /// Try all registered factories for a scheme, returning the first successful connection.
    /// For Oracle, tries oracle-rs first (12c+, pure Rust), then falls back to
    /// oracle-native (11g+, needs Instant Client).
    /// `allow_write` is the process-level CLI flag; backends with a
    /// session-level read-only guard (GaussDB) honour it, others ignore it.
    pub async fn connect_with_fallback(
        &self,
        scheme: &str,
        url: &str,
        timeout: Option<&TimeoutConfig>,
        allow_write: bool,
    ) -> Result<Arc<dyn DbPool>, String> {
        let factories = self.get_by_scheme_all(scheme);
        if factories.is_empty() {
            return Err(format!("No backend registered for scheme '{}'", scheme));
        }

        let mut errors: Vec<String> = Vec::new();
        for factory in factories {
            match factory.connect_with_mode(url, timeout, allow_write).await {
                Ok(pool) => {
                    // Validate by acquiring a real connection, then drop it.
                    // This ensures the backend actually works — needed for
                    // Oracle where pool creation (URL parsing) always succeeds.
                    match pool.acquire().await {
                        Ok(_conn) => return Ok(pool),
                        Err(e) => {
                            errors.push(format!("{} (acquire): {}", factory.name(), e));
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("{} (connect): {}", factory.name(), e));
                }
            }
        }
        Err(errors.join(" | fallback: "))
    }

    pub fn resolve(
        &self,
        driver: Option<&str>,
        url: Option<&str>,
        default: &str,
    ) -> Option<&Arc<dyn BackendFactory>> {
        if let Some(d) = driver {
            if let Some(f) = self.get_by_name(d) {
                return Some(f);
            }
            if let Some(f) = self.get_by_scheme(d) {
                return Some(f);
            }
        }
        if let Some(u) = url {
            if let Some(scheme_end) = u.find("://") {
                let scheme = &u[..scheme_end];
                if let Some(f) = self.get_by_scheme(scheme) {
                    return Some(f);
                }
            }
        }
        self.get_by_name(default)
            .or_else(|| self.get_by_scheme(default))
    }
}
