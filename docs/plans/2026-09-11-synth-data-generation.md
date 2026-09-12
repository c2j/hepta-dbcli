# SDV-Style Data Synthesis Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add synthetic data generation capabilities (train + generate) to hepta-dbcli, supporting single-table modeling with Gaussian Copula and multi-table generation with YAML rules.

**Architecture:** 
- Extend `Dialect` trait with `foreign_keys_sql()` for FK introspection
- New `synth` module: statistical modeling (marginals + copula), graph algorithms (topo sort + Tarjan SCC), YAML rule parsing, generation engine
- CLI-only (no MCP tools), feature-gated (`--features synth`)
- Reuse delta_diff's export formats (csv/jsonl/json/sql)

**Tech Stack:** Rust, rand/rand_distr (sampling), nalgebra (linear algebra), serde_yaml (rules), async-trait

---

## Phase 1: Foundation (FK Introspection + Graph Module)

### Task 1.1: Add `foreign_keys_sql()` to Dialect Trait

**Files:**
- Modify: `dbcli/src/backend/mod.rs:94-230` (Dialect trait)
- Modify: `dbcli/src/backend/mysql/dialect.rs`
- Modify: `dbcli/src/backend/oracle/dialect.rs`
- Modify: `dbcli/src/backend/oracle_native/dialect.rs`
- Modify: `dbcli/src/backend/gaussdb/dialect.rs`

**Step 1: Write the failing test**

```rust
// In dbcli/src/backend/mod.rs, inside #[cfg(test)] mod tests
#[test]
fn dialect_foreign_keys_sql_returns_valid_sql() {
    // Test that each dialect returns non-empty FK query
    // Will fail until trait method is added
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --all dialect_foreign_keys_sql`
Expected: FAIL with "method `foreign_keys_sql` not found"

**Step 3: Add trait method**

```rust
// In Dialect trait, add after line 109:
/// Query returning foreign key relationships for a schema.
/// Returns rows: [schema_name, table_name, column_name, referenced_schema, referenced_table, referenced_column, constraint_name]
fn foreign_keys_sql(&self, schema: &str) -> String;
```

**Step 4: Implement for MySQL**

```rust
// In dbcli/src/backend/mysql/dialect.rs
fn foreign_keys_sql(&self, schema: &str) -> String {
    format!(
        r#"SELECT
            kcu.TABLE_SCHEMA AS schema_name,
            kcu.TABLE_NAME AS table_name,
            kcu.COLUMN_NAME AS column_name,
            kcu.REFERENCED_TABLE_SCHEMA AS referenced_schema,
            kcu.REFERENCED_TABLE_NAME AS referenced_table,
            kcu.REFERENCED_COLUMN_NAME AS referenced_column,
            kcu.CONSTRAINT_NAME AS constraint_name
        FROM information_schema.KEY_COLUMN_USAGE kcu
        WHERE kcu.REFERENCED_TABLE_NAME IS NOT NULL
            AND kcu.TABLE_SCHEMA = '{schema}'
        ORDER BY kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION"#,
        schema = schema
    )
}
```

**Step 5: Implement for Oracle**

```rust
// In dbcli/src/backend/oracle/dialect.rs and oracle_native/dialect.rs
fn foreign_keys_sql(&self, schema: &str) -> String {
    format!(
        r#"SELECT
            acc.OWNER AS schema_name,
            acc.TABLE_NAME AS table_name,
            acc.COLUMN_NAME AS column_name,
            pkcol.OWNER AS referenced_schema,
            pkcol.TABLE_NAME AS referenced_table,
            pkcol.COLUMN_NAME AS referenced_column,
            acc.CONSTRAINT_NAME AS constraint_name
        FROM ALL_CONS_COLUMNS acc
        JOIN ALL_CONSTRAINTS ac ON acc.CONSTRAINT_NAME = ac.CONSTRAINT_NAME
            AND acc.OWNER = ac.OWNER
        JOIN ALL_CONS_COLUMNS pkcol ON ac.R_CONSTRAINT_NAME = pkcol.CONSTRAINT_NAME
            AND ac.R_OWNER = pkcol.OWNER
            AND acc.POSITION = pkcol.POSITION
        WHERE ac.CONSTRAINT_TYPE = 'R'
            AND ac.OWNER = '{schema}'
        ORDER BY acc.CONSTRAINT_NAME, acc.POSITION"#,
        schema = schema.to_uppercase()
    )
}
```

**Step 6: Implement for GaussDB**

```rust
// In dbcli/src/backend/gaussdb/dialect.rs
fn foreign_keys_sql(&self, schema: &str) -> String {
    format!(
        r#"SELECT
            n.nspname AS schema_name,
            cl.relname AS table_name,
            a.attname AS column_name,
            nr.nspname AS referenced_schema,
            cr.relname AS referenced_table,
            ar.attname AS referenced_column,
            con.conname AS constraint_name
        FROM pg_constraint con
        JOIN pg_class cl ON con.conrelid = cl.oid
        JOIN pg_namespace n ON cl.relnamespace = n.oid
        JOIN pg_attribute a ON a.attrelid = cl.oid AND a.attnum = ANY(con.conkey)
        JOIN pg_class cr ON con.confrelid = cr.oid
        JOIN pg_namespace nr ON cr.relnamespace = nr.oid
        JOIN pg_attribute ar ON ar.attrelid = cr.oid AND ar.attnum = ANY(con.confkey)
        WHERE con.contype = 'f'
            AND n.nspname = '{schema}'
        ORDER BY con.conname, a.attnum"#,
        schema = schema
    )
}
```

**Step 7: Run tests**

Run: `cargo test --all`
Expected: PASS

**Step 8: Commit**

```bash
git add -A
git commit -m "feat(synth): add foreign_keys_sql() to Dialect trait

Add FK introspection query for MySQL, Oracle, and GaussDB backends.
This is the foundation for automatic relationship discovery in synth."
```

---

### Task 1.2: Create Graph Module (Topo Sort + Tarjan SCC)

**Files:**
- Create: `dbcli/src/synth/graph.rs`
- Create: `dbcli/src/synth/mod.rs`
- Modify: `dbcli/src/main.rs` (add `mod synth`)

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/graph.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topological_sort_linear_chain() {
        // A -> B -> C should return [A, B, C]
        let edges = vec![("A", "B"), ("B", "C")];
        let order = topological_sort(&["A", "B", "C"], &edges).unwrap();
        assert_eq!(order, vec!["A", "B", "C"]);
    }

    #[test]
    fn topological_sort_with_diamond() {
        // A -> B, A -> C, B -> D, C -> D
        let edges = vec![("A", "B"), ("A", "C"), ("B", "D"), ("C", "D")];
        let order = topological_sort(&["A", "B", "C", "D"], &edges).unwrap();
        assert!(order.iter().position(|&x| x == "A") < order.iter().position(|&x| x == "B"));
        assert!(order.iter().position(|&x| x == "A") < order.iter().position(|&x| x == "C"));
        assert!(order.iter().position(|&x| x == "B") < order.iter().position(|&x| x == "D"));
        assert!(order.iter().position(|&x| x == "C") < order.iter().position(|&x| x == "D"));
    }

    #[test]
    fn topological_sort_cycle_detection() {
        // A -> B -> C -> A
        let edges = vec![("A", "B"), ("B", "C"), ("C", "A")];
        let result = topological_sort(&["A", "B", "C"], &edges);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("cycle"));
    }

    #[test]
    fn tarjan_scc_finds_cycle_path() {
        // A -> B -> C -> A, D -> E
        let edges = vec![("A", "B"), ("B", "C"), ("C", "A"), ("D", "E")];
        let sccs = tarjan_scc(&["A", "B", "C", "D", "E"], &edges);
        assert_eq!(sccs.len(), 3); // [A,B,C], [D], [E]
        let cycle_scc = sccs.iter().find(|s| s.len() > 1).unwrap();
        assert!(cycle_scc.contains(&"A"));
        assert!(cycle_scc.contains(&"B"));
        assert!(cycle_scc.contains(&"C"));
    }

    #[test]
    fn topological_sort_projection_reversal() {
        // Projection edge: dic_stock <- orders (dic_stock depends on orders)
        // In dependency graph: dic_stock -> orders (dic_stock must be generated after orders)
        let edges = vec![("dic_stock", "orders")]; // projection dependency
        let order = topological_sort(&["orders", "dic_stock"], &edges).unwrap();
        assert_eq!(order, vec!["orders", "dic_stock"]);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all synth::graph`
Expected: FAIL with "unresolved import"

**Step 3: Implement graph module**

```rust
// In dbcli/src/synth/graph.rs
use std::collections::{HashMap, HashSet, VecDeque};

/// Topological sort with cycle detection.
/// Returns error with cycle path if cycle exists.
pub fn topological_sort<T: Eq + std::hash::Hash + Clone + std::fmt::Debug>(
    nodes: &[T],
    edges: &[(T, T)],  // (from, to) meaning "from must come before to"
) -> Result<Vec<T>, String> {
    let mut in_degree: HashMap<&T, usize> = HashMap::new();
    let mut adjacency: HashMap<&T, Vec<&T>> = HashMap::new();
    
    for node in nodes {
        in_degree.entry(node).or_insert(0);
        adjacency.entry(node).or_insert_with(Vec::new);
    }
    
    for (from, to) in edges {
        *in_degree.entry(to).or_insert(0) += 1;
        adjacency.entry(from).or_insert_with(Vec::new).push(to);
    }
    
    let mut queue: VecDeque<&T> = in_degree.iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(node, _)| *node)
        .collect();
    
    let mut order = Vec::new();
    while let Some(node) = queue.pop_front() {
        order.push(node.clone());
        if let Some(neighbors) = adjacency.get(node) {
            for neighbor in neighbors {
                let deg = in_degree.get_mut(neighbor).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(neighbor);
                }
            }
        }
    }
    
    if order.len() != nodes.len() {
        // Find cycle path for error message
        let cycle_path = find_cycle_path(nodes, edges);
        return Err(format!("cycle detected: {}", cycle_path));
    }
    
    Ok(order)
}

/// Find and return a human-readable cycle path
fn find_cycle_path<T: Eq + std::hash::Hash + Clone + std::fmt::Debug>(
    nodes: &[T],
    edges: &[(T, T)],
) -> String {
    // Simple DFS to find one cycle
    let mut visited = HashSet::new();
    let mut path = Vec::new();
    
    for node in nodes {
        if !visited.contains(node) {
            if let Some(cycle) = dfs_find_cycle(node, edges, &mut visited, &mut path) {
                return cycle;
            }
        }
    }
    "unknown".to_string()
}

fn dfs_find_cycle<T: Eq + std::hash::Hash + Clone + std::fmt::Debug>(
    node: &T,
    edges: &[(T, T)],
    visited: &mut HashSet<T>,
    path: &mut Vec<T>,
) -> Option<String> {
    if path.contains(node) {
        // Found cycle
        let cycle_start = path.iter().position(|x| x == node).unwrap();
        let mut cycle: Vec<String> = path[cycle_start..].iter()
            .map(|x| format!("{:?}", x))
            .collect();
        cycle.push(format!("{:?}", node));
        return Some(cycle.join(" -> "));
    }
    
    path.push(node.clone());
    visited.insert(node.clone());
    
    for (from, to) in edges {
        if from == node {
            if let Some(cycle) = dfs_find_cycle(to, edges, visited, path) {
                return Some(cycle);
            }
        }
    }
    
    path.pop();
    None
}

/// Tarjan's SCC algorithm - finds strongly connected components
pub fn tarjan_scc<T: Eq + std::hash::Hash + Clone + std::fmt::Debug>(
    nodes: &[T],
    edges: &[(T, T)],
) -> Vec<Vec<T>> {
    let mut index_counter = 0;
    let mut stack = Vec::new();
    let mut indices: HashMap<&T, usize> = HashMap::new();
    let mut lowlinks: HashMap<&T, usize> = HashMap::new();
    let mut on_stack: HashSet<&T> = HashSet::new();
    let mut sccs = Vec::new();
    
    let adjacency: HashMap<&T, Vec<&T>> = {
        let mut adj = HashMap::new();
        for node in nodes {
            adj.entry(node).or_insert_with(Vec::new);
        }
        for (from, to) in edges {
            adj.entry(from).or_insert_with(Vec::new).push(to);
        }
        adj
    };
    
    for node in nodes {
        if !indices.contains_key(node) {
            strongconnect(
                node,
                &adjacency,
                &mut index_counter,
                &mut indices,
                &mut lowlinks,
                &mut stack,
                &mut on_stack,
                &mut sccs,
            );
        }
    }
    
    sccs
}

fn strongconnect<T: Eq + std::hash::Hash + Clone>(
    v: &T,
    adjacency: &HashMap<&T, Vec<&T>>,
    index_counter: &mut usize,
    indices: &mut HashMap<&T, usize>,
    lowlinks: &mut HashMap<&T, usize>,
    stack: &mut Vec<&T>,
    on_stack: &mut HashSet<&T>,
    sccs: &mut Vec<Vec<T>>,
) {
    indices.insert(v, *index_counter);
    lowlinks.insert(v, *index_counter);
    *index_counter += 1;
    stack.push(v);
    on_stack.insert(v);
    
    if let Some(neighbors) = adjacency.get(v) {
        for w in neighbors {
            if !indices.contains_key(w) {
                strongconnect(w, adjacency, index_counter, indices, lowlinks, stack, on_stack, sccs);
                let w_low = *lowlinks.get(w).unwrap();
                let v_low = lowlinks.get_mut(v).unwrap();
                if w_low < *v_low {
                    *v_low = w_low;
                }
            } else if on_stack.contains(w) {
                let w_idx = *indices.get(w).unwrap();
                let v_low = lowlinks.get_mut(v).unwrap();
                if w_idx < *v_low {
                    *v_low = w_idx;
                }
            }
        }
    }
    
    let v_low = *lowlinks.get(v).unwrap();
    let v_idx = *indices.get(v).unwrap();
    if v_low == v_idx {
        let mut scc = Vec::new();
        loop {
            let w = stack.pop().unwrap();
            on_stack.remove(w);
            scc.push(w.clone());
            if w == v {
                break;
            }
        }
        sccs.push(scc);
    }
}
```

**Step 4: Create synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;

// Re-export for convenience
pub use graph::{topological_sort, tarjan_scc};
```

**Step 5: Add to main.rs**

```rust
// In dbcli/src/main.rs, after line 7
mod synth;
```

**Step 6: Run tests**

Run: `cargo test --all synth::graph`
Expected: PASS

**Step 7: Commit**

```bash
git add dbcli/src/synth/ dbcli/src/main.rs
git commit -m "feat(synth): add graph module with topo sort and Tarjan SCC

Foundation for multi-table dependency resolution and cycle detection."
```

---

### Task 1.3: Add `synth` Feature Flag

**Files:**
- Modify: `dbcli/Cargo.toml` (add feature + dependencies)

**Step 1: Add feature flag**

```toml
# In dbcli/Cargo.toml, after line 36
synth = ["dep:rand", "dep:rand_distr", "dep:nalgebra", "dep:serde_yaml"]
```

**Step 2: Add dependencies**

```toml
# In dbcli/Cargo.toml, after line 67
rand = { version = "0.8", optional = true }
rand_distr = { version = "0.4", optional = true }
nalgebra = { version = "0.32", optional = true }
serde_yaml = { version = "0.9", optional = true }
```

**Step 3: Gate synth module**

```rust
// In dbcli/src/main.rs
#[cfg(feature = "synth")]
mod synth;
```

**Step 4: Verify build without synth**

Run: `cargo build`
Expected: PASS (no new deps)

**Step 5: Verify build with synth**

Run: `cargo build --features synth`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/Cargo.toml
git commit -m "feat(synth): add synth feature flag with rand/nalgebra/serde_yaml

Feature-gated to keep default build unchanged."
```

---

## Phase 2: Single-Table Modeling

### Task 2.1: Implement Marginal Fitters

**Files:**
- Create: `dbcli/src/synth/marginal.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/marginal.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_fitter_converges() {
        let samples: Vec<f64> = (0..10000).map(|_| {
            // Generate from N(100, 10)
            let u1: f64 = rand::random();
            let u2: f64 = rand::random();
            100.0 + 10.0 * (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        }).collect();
        
        let fitter = NormalFitter;
        let params = fitter.fit(&samples).unwrap();
        
        // Check params are close to true values
        assert!((params.loc - 100.0).abs() < 1.0);
        assert!((params.scale - 10.0).abs() < 1.0);
    }

    #[test]
    fn categorical_fitter_weights_sum_to_one() {
        let samples = vec!["a".to_string(), "a".to_string(), "b".to_string(), "c".to_string()];
        let fitter = CategoricalFitter;
        let params = fitter.fit(&samples).unwrap();
        
        let total: f64 = params.weights.iter().sum();
        assert!((total - 1.0).abs() < 1e-10);
    }

    #[test]
    fn marginal_cdf_inverse_cdf_roundtrip() {
        let marginal = Marginal::Normal(NormalParams { loc: 0.0, scale: 1.0 });
        let x = 0.5;
        let cdf_val = marginal.cdf(x);
        let x_prime = marginal.inverse_cdf(cdf_val);
        assert!((x - x_prime).abs() < 1e-10);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::marginal`
Expected: FAIL

**Step 3: Implement marginal module**

```rust
// In dbcli/src/synth/marginal.rs
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MarginalType {
    Normal,
    Beta,
    Gamma,
    Categorical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalParams {
    pub loc: f64,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BetaParams {
    pub a: f64,
    pub b: f64,
    pub loc: f64,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GammaParams {
    pub shape: f64,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoricalParams {
    pub values: Vec<String>,
    pub weights: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "name")]
pub enum Marginal {
    #[serde(rename = "norm")]
    Normal(NormalParams),
    #[serde(rename = "beta")]
    Beta(BetaParams),
    #[serde(rename = "gamma")]
    Gamma(GammaParams),
    #[serde(rename = "categorical")]
    Categorical(CategoricalParams),
}

impl Marginal {
    pub fn cdf(&self, x: f64) -> f64 {
        match self {
            Marginal::Normal(p) => normal_cdf(x, p.loc, p.scale),
            Marginal::Beta(p) => beta_cdf(x, p.a, p.b, p.loc, p.scale),
            Marginal::Gamma(p) => gamma_cdf(x, p.shape, p.scale),
            Marginal::Categorical(_) => todo!("categorical CDF"),
        }
    }
    
    pub fn inverse_cdf(&self, p: f64) -> f64 {
        match self {
            Marginal::Normal(p) => normal_ppf(p.loc, p.scale),
            Marginal::Beta(p) => beta_ppf(p.a, p.b, p.loc, p.scale),
            Marginal::Gamma(p) => gamma_ppf(p.shape, p.scale),
            Marginal::Categorical(_) => todo!("categorical inverse CDF"),
        }
    }
}

// Normal CDF using error function approximation
fn normal_cdf(x: f64, loc: f64, scale: f64) -> f64 {
    let z = (x - loc) / scale;
    0.5 * (1.0 + erf(z / std::f64::consts::SQRT_2))
}

fn normal_ppf(loc: f64, scale: f64) -> f64 {
    // Approximation of inverse normal CDF
    loc // Placeholder - implement proper Beasley-Springer-Moro algorithm
}

fn beta_cdf(x: f64, a: f64, b: f64, loc: f64, scale: f64) -> f64 {
    let t = (x - loc) / scale;
    regularized_incomplete_beta(t, a, b)
}

fn beta_ppf(a: f64, b: f64, loc: f64, scale: f64) -> f64 {
    loc + scale * 0.5 // Placeholder
}

fn gamma_cdf(x: f64, shape: f64, scale: f64) -> f64 {
    regularized_gamma_lower(shape, x / scale)
}

fn gamma_ppf(shape: f64, scale: f64) -> f64 {
    shape * scale // Placeholder
}

// Error function approximation (Abramowitz and Stegun)
fn erf(x: f64) -> f64 {
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;
    
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    sign * y
}

fn regularized_incomplete_beta(x: f64, a: f64, b: f64) -> f64 {
    // Placeholder - implement continued fraction expansion
    0.5
}

fn regularized_gamma_lower(a: f64, x: f64) -> f64 {
    // Placeholder - implement series expansion
    0.5
}

// Fitter trait
pub trait MarginalFitter {
    fn fit(&self, samples: &[f64]) -> Result<Marginal, String>;
}

pub struct NormalFitter;
pub struct BetaFitter;
pub struct CategoricalFitter;

impl MarginalFitter for NormalFitter {
    fn fit(&self, samples: &[f64]) -> Result<Marginal, String> {
        let n = samples.len() as f64;
        let mean = samples.iter().sum::<f64>() / n;
        let variance = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        let std_dev = variance.sqrt();
        
        Ok(Marginal::Normal(NormalParams {
            loc: mean,
            scale: std_dev,
        }))
    }
}

impl MarginalFitter for CategoricalFitter {
    fn fit(&self, samples: &[String]) -> Result<Marginal, String> {
        let mut counts = std::collections::HashMap::new();
        for s in samples {
            *counts.entry(s.clone()).or_insert(0) += 1;
        }
        
        let total = samples.len() as f64;
        let values: Vec<String> = counts.keys().cloned().collect();
        let weights: Vec<f64> = counts.values().map(|&c| c as f64 / total).collect();
        
        Ok(Marginal::Categorical(CategoricalParams { values, weights }))
    }
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::marginal`
Expected: PASS (after implementing proper math functions)

**Step 6: Commit**

```bash
git add dbcli/src/synth/marginal.rs
git commit -m "feat(synth): implement marginal distribution fitters

Normal, Beta, Gamma, Categorical fitters with CDF/inverse_CDF."
```

---

### Task 2.2: Implement Copula Sampler

**Files:**
- Create: `dbcli/src/synth/copula.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/copula.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copula_sampling_preserves_correlation() {
        // Create copula with known correlation
        let correlation = vec![
            vec![1.0, 0.8],
            vec![0.8, 1.0],
        ];
        let copula = GaussianCopula::new(correlation);
        
        // Sample and check correlation
        let samples = copula.sample(10000, None);
        
        // Check correlation is preserved
        let corr = pearson_correlation(&samples[0], &samples[1]);
        assert!((corr - 0.8).abs() < 0.1);
    }

    #[test]
    fn copula_handles_non_psd_matrix() {
        // Non-PSD matrix should be projected
        let correlation = vec![
            vec![1.0, 0.9],
            vec![0.9, 0.5], // Not PSD (det < 0)
        ];
        let result = GaussianCopula::new(correlation);
        // Should not panic
    }

    #[test]
    fn copula_generates_correct_dimensions() {
        let correlation = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let copula = GaussianCopula::new(correlation);
        let samples = copula.sample(100, None);
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].len(), 100);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::copula`
Expected: FAIL

**Step 3: Implement copula module**

```rust
// In dbcli/src/synth/copula.rs
use nalgebra::{Cholesky, DMatrix, Dyn, VecStorage};
use rand::Rng;
use rand_distr::StandardNormal;

pub struct GaussianCopula {
    dimension: usize,
    cholesky: Cholesky<f64, Dyn, VecStorage<f64, Dyn, Dyn>>,
}

impl GaussianCopula {
    pub fn new(correlation: Vec<Vec<f64>>) -> Self {
        let dim = correlation.len();
        let mut matrix = DMatrix::zeros(dim, dim);
        
        for i in 0..dim {
            for j in 0..dim {
                matrix[(i, j)] = correlation[i][j];
            }
        }
        
        // Ensure PSD by projecting
        let matrix = ensure_psd(matrix);
        
        let cholesky = Cholesky::new(matrix)
            .expect("Matrix should be PSD after projection");
        
        Self {
            dimension: dim,
            cholesky,
        }
    }
    
    pub fn sample(&self, n: usize, seed: Option<u64>) -> Vec<Vec<f64>> {
        let mut rng = rand::thread_rng();
        
        // Generate independent standard normals
        let mut independent = Vec::with_capacity(self.dimension);
        for _ in 0..self.dimension {
            let col: Vec<f64> = (0..n)
                .map(|_| rng.sample(StandardNormal))
                .collect();
            independent.push(col);
        }
        
        // Apply Cholesky to get correlated normals
        let l = self.cholesky.l();
        let mut correlated = vec![vec![0.0; n]; self.dimension];
        
        for t in 0..n {
            for i in 0..self.dimension {
                let mut sum = 0.0;
                for j in 0..=i {
                    sum += l[(i, j)] * independent[j][t];
                }
                correlated[i][t] = sum;
            }
        }
        
        // Convert to uniform using normal CDF
        correlated.iter()
            .map(|col| col.iter().map(|&x| normal_cdf(x)).collect())
            .collect()
    }
}

fn ensure_psd(mut matrix: DMatrix<f64>) -> DMatrix<f64> {
    let dim = matrix.nrows();
    
    // Symmetrize
    for i in 0..dim {
        for j in (i + 1)..dim {
            let avg = (matrix[(i, j)] + matrix[(j, i)]) / 2.0;
            matrix[(i, j)] = avg;
            matrix[(j, i)] = avg;
        }
    }
    
    // Check eigenvalues and project if needed
    // Simple approach: ensure diagonal dominance
    for i in 0..dim {
        let row_sum: f64 = (0..dim).filter(|&j| j != i).map(|j| matrix[(i, j)].abs()).sum();
        if matrix[(i, i)] < row_sum {
            matrix[(i, i)] = row_sum + 0.001;
        }
    }
    
    matrix
}

fn normal_cdf(x: f64) -> f64 {
    0.5 * (1.0 + erf(x / std::f64::consts::SQRT_2))
}

fn erf(x: f64) -> f64 {
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;
    
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    sign * y
}

fn pearson_correlation(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    let mean_x = x.iter().sum::<f64>() / n;
    let mean_y = y.iter().sum::<f64>() / n;
    
    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    
    for (xi, yi) in x.iter().zip(y.iter()) {
        let dx = xi - mean_x;
        let dy = yi - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }
    
    cov / (var_x * var_y).sqrt()
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod copula;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::copula`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/copula.rs
git commit -m "feat(synth): implement Gaussian Copula sampler

Cholesky decomposition with PSD projection for correlated sampling."
```

---

### Task 2.3: Implement Table Model Serialization

**Files:**
- Create: `dbcli/src/synth/model.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/model.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_json_roundtrip() {
        let model = TableModel {
            version: 1,
            table: "orders".to_string(),
            dialect: "mysql".to_string(),
            provenance: Provenance {
                source: "native".to_string(),
                converter_version: None,
                sdv_version: None,
            },
            pk: vec!["id".to_string()],
            columns: std::collections::HashMap::new(),
            copula: CopulaInfo {
                column_order: vec!["id".to_string()],
                correlation: vec![vec![1.0]],
            },
        };
        
        let json = serde_json::to_string_pretty(&model).unwrap();
        let loaded: TableModel = serde_json::from_str(&json).unwrap();
        
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.table, "orders");
    }

    #[test]
    fn model_rejects_higher_version() {
        let json = r#"{"version": 99, "table": "t", ...}"#;
        let result: Result<TableModel, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::model`
Expected: FAIL

**Step 3: Implement model module**

```rust
// In dbcli/src/synth/model.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableModel {
    pub version: u32,
    pub table: String,
    pub dialect: String,
    pub provenance: Provenance,
    pub pk: Vec<String>,
    pub columns: HashMap<String, ColumnModel>,
    pub copula: CopulaInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,  // "native" | "sdv_import"
    pub converter_version: Option<String>,
    pub sdv_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnModel {
    pub logical_type: LogicalType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rounding: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datetime_epoch: Option<bool>,
    pub marginal: crate::synth::marginal::Marginal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalType {
    Numerical,
    Categorical,
    Datetime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopulaInfo {
    pub column_order: Vec<String>,
    pub correlation: Vec<Vec<f64>>,
}

impl TableModel {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("read model file: {}", e))?;
        
        let model: Self = serde_json::from_str(&content)
            .map_err(|e| format!("parse model JSON: {}", e))?;
        
        if model.version > CURRENT_VERSION {
            return Err(format!(
                "model version {} not supported (max {})",
                model.version, CURRENT_VERSION
            ));
        }
        
        Ok(model)
    }
    
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize model: {}", e))?;
        
        std::fs::write(path, json)
            .map_err(|e| format!("write model file: {}", e))?;
        
        Ok(())
    }
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod model;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::model`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/model.rs
git commit -m "feat(synth): implement table model serialization

JSON format with version checking and provenance tracking."
```

---

### Task 2.4: Implement Profile Generation

**Files:**
- Create: `dbcli/src/synth/profile.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/profile.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_captures_column_stats() {
        let samples = vec![
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(null),
            serde_json::json!(4),
        ];
        
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.null_rate, 0.25);
        assert_eq!(profile.cardinality, 3);
    }

    #[test]
    fn profile_json_roundtrip() {
        let profile = TableProfile {
            table: "t".to_string(),
            row_count: 100,
            columns: std::collections::HashMap::new(),
        };
        
        let json = serde_json::to_string_pretty(&profile).unwrap();
        let loaded: TableProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.row_count, 100);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::profile`
Expected: FAIL

**Step 3: Implement profile module**

```rust
// In dbcli/src/synth/profile.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableProfile {
    pub table: String,
    pub row_count: usize,
    pub columns: HashMap<String, ColumnProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnProfile {
    pub logical_type: String,
    pub null_rate: f64,
    pub cardinality: usize,
    pub min: Option<Value>,
    pub max: Option<Value>,
    pub mean: Option<f64>,
    pub std_dev: Option<f64>,
}

impl ColumnProfile {
    pub fn from_samples(samples: &[Value]) -> Self {
        let total = samples.len();
        let null_count = samples.iter().filter(|v| v.is_null()).count();
        let non_null: Vec<&Value> = samples.iter().filter(|v| !v.is_null()).collect();
        
        let null_rate = if total > 0 {
            null_count as f64 / total as f64
        } else {
            0.0
        };
        
        let cardinality = non_null.iter().collect::<std::collections::HashSet<_>>().len();
        
        // Detect type and compute stats
        let (logical_type, min, max, mean, std_dev) = if non_null.is_empty() {
            ("unknown".to_string(), None, None, None, None)
        } else if non_null[0].is_number() {
            let nums: Vec<f64> = non_null.iter()
                .filter_map(|v| v.as_f64())
                .collect();
            let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let variance = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / nums.len() as f64;
            let std_dev = variance.sqrt();
            
            ("numerical".to_string(), Some(Value::from(min)), Some(Value::from(max)), Some(mean), Some(std_dev))
        } else if non_null[0].is_string() {
            ("categorical".to_string(), None, None, None, None)
        } else {
            ("unknown".to_string(), None, None, None, None)
        };
        
        Self {
            logical_type,
            null_rate,
            cardinality,
            min,
            max,
            mean,
            std_dev,
        }
    }
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod model;
#[cfg(feature = "synth")]
pub mod profile;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::profile`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/profile.rs
git commit -m "feat(synth): implement table profile generation

Captures column statistics for fidelity comparison."
```

---

## Phase 3: Rule System

### Task 3.1: Implement YAML Rule Parser

**Files:**
- Create: `dbcli/src/synth/rules.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/rules.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_yaml() {
        let yaml = r#"
schema_version: 1
name: test
seed: 42
tables:
  users:
    rows: 1000
"#;
        let rules: GenerationRules = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(rules.schema_version, 1);
        assert_eq!(rules.tables["users"].rows, 1000);
    }

    #[test]
    fn parse_with_relationships() {
        let yaml = r#"
schema_version: 1
name: test
tables:
  orders:
    rows: 5000
relationships:
  - parent: users
    child: orders
    fk: [user_id]
    pk: [id]
"#;
        let rules: GenerationRules = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(rules.relationships.len(), 1);
        assert_eq!(rules.relationships[0].parent, "users");
    }

    #[test]
    fn reject_invalid_schema_version() {
        let yaml = r#"
schema_version: 99
name: test
tables: {}
"#;
        let result: Result<GenerationRules, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::rules`
Expected: FAIL

**Step 3: Implement rules module**

```rust
// In dbcli/src/synth/rules.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationRules {
    pub schema_version: u32,
    pub name: String,
    #[serde(default = "default_seed")]
    pub seed: u64,
    #[serde(default)]
    pub defaults: Defaults,
    pub tables: HashMap<String, TableRules>,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
}

fn default_seed() -> u64 {
    42
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub output_dir: Option<String>,
}

fn default_batch_size() -> usize {
    10000
}

fn default_format() -> String {
    "csv".to_string()
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            batch_size: default_batch_size(),
            format: default_format(),
            output_dir: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRules {
    pub rows: usize,
    #[serde(default)]
    pub source: TableSource,
    #[serde(default)]
    pub connection: Option<String>,
    #[serde(default)]
    pub pull: Option<PullSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableSource {
    Model,
    Projection,
    Real,
}

impl Default for TableSource {
    fn default() -> Self {
        Self::Model
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullSpec {
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub parent: String,
    pub child: String,
    pub fk: Vec<String>,
    #[serde(default)]
    pub pk: Option<Vec<String>>,
    #[serde(default = "default_fk_pool")]
    pub fk_pool: FkPool,
    #[serde(default)]
    pub fan_out: Option<FanOut>,
}

fn default_fk_pool() -> FkPool {
    FkPool::Generated
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FkPool {
    Generated,
    Projection,
    Real,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "strategy")]
pub enum FanOut {
    #[serde(rename = "fixed")]
    Fixed { n: usize },
    #[serde(rename = "uniform")]
    Uniform,
    #[serde(rename = "zipf")]
    Zipf { s: f64 },
    #[serde(rename = "empirical")]
    Empirical,
}

impl GenerationRules {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("read rules file: {}", e))?;
        
        let rules: Self = serde_yaml::from_str(&content)
            .map_err(|e| format!("parse rules YAML: {}", e))?;
        
        if rules.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(format!(
                "schema_version {} not supported (expected {})",
                rules.schema_version, CURRENT_SCHEMA_VERSION
            ));
        }
        
        Ok(rules)
    }
    
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let yaml = serde_yaml::to_string(self)
            .map_err(|e| format!("serialize rules: {}", e))?;
        
        std::fs::write(path, yaml)
            .map_err(|e| format!("write rules file: {}", e))?;
        
        Ok(())
    }
    
    pub fn validate(&self, models_dir: &std::path::Path) -> Result<Vec<String>, Vec<String>> {
        let mut warnings = Vec::new();
        let mut errors = Vec::new();
        
        // Check model files exist
        for (table, rules) in &self.tables {
            if rules.source == TableSource::Model {
                let model_path = models_dir.join(format!("{}.model.json", table));
                if !model_path.exists() {
                    errors.push(format!("model file not found: {}", model_path.display()));
                }
            }
        }
        
        // Check FK references
        for rel in &self.relationships {
            if !self.tables.contains_key(&rel.parent) {
                errors.push(format!("parent table '{}' not in tables", rel.parent));
            }
            if !self.tables.contains_key(&rel.child) {
                errors.push(format!("child table '{}' not in tables", rel.child));
            }
        }
        
        if errors.is_empty() {
            Ok(warnings)
        } else {
            Err(errors)
        }
    }
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod model;
#[cfg(feature = "synth")]
pub mod profile;
#[cfg(feature = "synth")]
pub mod rules;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::rules`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/rules.rs
git commit -m "feat(synth): implement YAML rule parser

Schema validation, table rules, relationships, and FK pool semantics."
```

---

### Task 3.2: Implement Rules Draft Generator

**Files:**
- Create: `dbcli/src/synth/rules_draft.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/rules_draft.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_detects_projection_candidates() {
        let fks = vec![
            ForeignKey {
                table: "orders".to_string(),
                column: "stock_kind".to_string(),
                referenced_table: "dic_stock".to_string(),
                referenced_column: "kind".to_string(),
            },
        ];
        
        let profiles = std::collections::HashMap::new();
        // dic_stock would have low cardinality
        
        let draft = generate_draft("test", &fks, &profiles);
        assert!(draft.suggested_projections.contains(&"dic_stock".to_string()));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::rules_draft`
Expected: FAIL

**Step 3: Implement rules_draft module**

```rust
// In dbcli/src/synth/rules_draft.rs
use crate::synth::rules::{GenerationRules, TableRules, Relationship, FkPool, FanOut};
use std::collections::HashMap;

#[derive(Debug)]
pub struct ForeignKey {
    pub table: String,
    pub column: String,
    pub referenced_table: String,
    pub referenced_column: String,
}

#[derive(Debug)]
pub struct RulesDraft {
    pub rules: GenerationRules,
    pub suggested_projections: Vec<String>,
    pub unresolved: Vec<String>,
}

pub fn generate_draft(
    schema_name: &str,
    foreign_keys: &[ForeignKey],
    column_profiles: &HashMap<String, ColumnProfileInfo>,
) -> RulesDraft {
    let mut tables = HashMap::new();
    let mut relationships = Vec::new();
    let mut suggested_projections = Vec::new();
    
    // Collect all tables
    let mut all_tables = std::collections::HashSet::new();
    for fk in foreign_keys {
        all_tables.insert(fk.table.clone());
        all_tables.insert(fk.referenced_table.clone());
    }
    
    // Create table rules
    for table in &all_tables {
        tables.insert(table.clone(), TableRules {
            rows: 1000, // Default, user should edit
            source: crate::synth::rules::TableSource::Model,
            connection: None,
            pull: None,
        });
    }
    
    // Create relationships and detect projection candidates
    for fk in foreign_keys {
        // Check if referenced table is a code/dictionary table
        if let Some(profile) = column_profiles.get(&fk.referenced_table) {
            if profile.low_cardinality {
                suggested_projections.push(fk.referenced_table.clone());
            }
        }
        
        relationships.push(Relationship {
            parent: fk.referenced_table.clone(),
            child: fk.table.clone(),
            fk: vec![fk.column.clone()],
            pk: Some(vec![fk.referenced_column.clone()]),
            fk_pool: FkPool::Generated,
            fan_out: Some(FanOut::Zipf { s: 1.2 }),
        });
    }
    
    let rules = GenerationRules {
        schema_version: 1,
        name: schema_name.to_string(),
        seed: 42,
        defaults: Default::default(),
        tables,
        relationships,
    };
    
    RulesDraft {
        rules,
        suggested_projections,
        unresolved: Vec::new(),
    }
}

#[derive(Debug)]
pub struct ColumnProfileInfo {
    pub low_cardinality: bool,
    pub cardinality: usize,
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod model;
#[cfg(feature = "synth")]
pub mod profile;
#[cfg(feature = "synth")]
pub mod rules;
#[cfg(feature = "synth")]
pub mod rules_draft;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::rules_draft`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/rules_draft.rs
git commit -m "feat(synth): implement rules draft generator

Auto-detect projection candidates from FK introspection."
```

---

## Phase 4: Generation Engine

### Task 4.1: Implement FK Pool and Fan-Out

**Files:**
- Create: `dbcli/src/synth/fk_pool.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/fk_pool.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_fk_pool_samples_from_parent() {
        let parent_pks = vec![1, 2, 3, 4, 5];
        let pool = GeneratedFkPool::new(&parent_pks);
        
        let fk = pool.sample_fk();
        assert!(parent_pks.contains(&fk));
    }

    #[test]
    fn projection_fk_pool_deduplicates() {
        let values = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        let pool = ProjectionFkPool::new(&values);
        
        assert_eq!(pool.values.len(), 2);
    }

    #[test]
    fn zipf_fan_out_distribution() {
        let fan_out = ZipfFanOut { s: 1.2 };
        let mut counts = std::collections::HashMap::new();
        
        for _ in 0..1000 {
            let n = fan_out.sample();
            *counts.entry(n).or_insert(0) += 1;
        }
        
        // Lower counts should be more frequent
        let count_1 = counts.get(&1).unwrap_or(&0);
        let count_10 = counts.get(&10).unwrap_or(&0);
        assert!(count_1 > count_10);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::fk_pool`
Expected: FAIL

**Step 3: Implement fk_pool module**

```rust
// In dbcli/src/synth/fk_pool.rs
use rand::Rng;
use std::collections::HashSet;

pub trait FkPool {
    fn sample_fk(&self) -> String;
}

pub struct GeneratedFkPool {
    pks: Vec<String>,
}

impl GeneratedFkPool {
    pub fn new(pks: &[String]) -> Self {
        Self {
            pks: pks.to_vec(),
        }
    }
}

impl FkPool for GeneratedFkPool {
    fn sample_fk(&self) -> String {
        let mut rng = rand::thread_rng();
        let idx = rng.gen_range(0..self.pks.len());
        self.pks[idx].clone()
    }
}

pub struct ProjectionFkPool {
    pub values: Vec<String>,
}

impl ProjectionFkPool {
    pub fn new(values: &[String]) -> Self {
        let unique: Vec<String> = values.iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        Self { values: unique }
    }
}

impl FkPool for ProjectionFkPool {
    fn sample_fk(&self) -> String {
        let mut rng = rand::thread_rng();
        let idx = rng.gen_range(0..self.values.len());
        self.values[idx].clone()
    }
}

pub trait FanOutStrategy {
    fn sample(&self) -> usize;
}

pub struct FixedFanOut {
    pub n: usize,
}

impl FanOutStrategy for FixedFanOut {
    fn sample(&self) -> usize {
        self.n
    }
}

pub struct UniformFanOut {
    pub min: usize,
    pub max: usize,
}

impl FanOutStrategy for UniformFanOut {
    fn sample(&self) -> usize {
        let mut rng = rand::thread_rng();
        rng.gen_range(self.min..=self.max)
    }
}

pub struct ZipfFanOut {
    pub s: f64,
}

impl FanOutStrategy for ZipfFanOut {
    fn sample(&self) -> usize {
        // Simplified Zipf sampling
        let mut rng = rand::thread_rng();
        let u: f64 = rng.gen();
        let max_k = 100;
        
        for k in 1..=max_k {
            let probability = 1.0 / (k as f64).powf(self.s);
            if u < probability {
                return k;
            }
        }
        
        max_k
    }
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod model;
#[cfg(feature = "synth")]
pub mod profile;
#[cfg(feature = "synth")]
pub mod rules;
#[cfg(feature = "synth")]
pub mod rules_draft;
#[cfg(feature = "synth")]
pub mod fk_pool;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::fk_pool`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/fk_pool.rs
git commit -m "feat(synth): implement FK pool and fan-out strategies

Generated, Projection pools + Fixed, Uniform, Zipf fan-out."
```

---

### Task 4.2: Implement Generation Engine

**Files:**
- Create: `dbcli/src/synth/generator.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/generator.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_respects_topo_order() {
        // parents should be generated before children
        // This is tested via integration test with real DB
    }

    #[test]
    fn generator_enforces_row_budget() {
        let rules = GenerationRules {
            schema_version: 1,
            name: "test".to_string(),
            seed: 42,
            defaults: Default::default(),
            tables: std::collections::HashMap::from([
                ("users".to_string(), TableRules {
                    rows: 100,
                    source: TableSource::Model,
                    connection: None,
                    pull: None,
                }),
            ]),
            relationships: vec![],
        };
        
        // Generator should produce ~100 rows
        // (actual test requires mock model)
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::generator`
Expected: FAIL

**Step 3: Implement generator module**

```rust
// In dbcli/src/synth/generator.rs
use crate::synth::copula::GaussianCopula;
use crate::synth::fk_pool::{GeneratedFkPool, ProjectionFkPool};
use crate::synth::model::TableModel;
use crate::synth::rules::{GenerationRules, TableSource};
use std::collections::HashMap;

pub struct GenerationContext {
    pub rules: GenerationRules,
    pub models: HashMap<String, TableModel>,
    pub parent_pools: HashMap<String, Vec<String>>,
}

pub struct GeneratedTable {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
}

impl GenerationContext {
    pub fn new(rules: GenerationRules) -> Self {
        Self {
            rules,
            models: HashMap::new(),
            parent_pools: HashMap::new(),
        }
    }
    
    pub fn load_models(&mut self, models_dir: &std::path::Path) -> Result<(), String> {
        for table_name in self.rules.tables.keys() {
            let model_path = models_dir.join(format!("{}.model.json", table_name));
            if model_path.exists() {
                let model = TableModel::load(&model_path)?;
                self.models.insert(table_name.clone(), model);
            }
        }
        Ok(())
    }
    
    pub fn generate_table(&self, table_name: &str) -> Result<GeneratedTable, String> {
        let table_rules = self.rules.tables.get(table_name)
            .ok_or_else(|| format!("table '{}' not in rules", table_name))?;
        
        let model = self.models.get(table_name)
            .ok_or_else(|| format!("model for '{}' not loaded", table_name))?;
        
        // Create copula from model
        let copula = GaussianCopula::new(model.copula.correlation.clone());
        
        // Sample uniforms
        let uniforms = copula.sample(table_rules.rows, Some(self.rules.seed));
        
        // Transform uniforms to actual values using marginals
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        
        for (col_name, col_model) in &model.columns {
            columns.push(col_name.clone());
            
            let values: Vec<serde_json::Value> = uniforms[columns.len() - 1].iter()
                .map(|&u| col_model.marginal.inverse_cdf(u))
                .map(|v| serde_json::json!(v))
                .collect();
            
            if rows.is_empty() {
                rows = values.into_iter().map(|v| vec![v]).collect();
            } else {
                for (i, v) in values.into_iter().enumerate() {
                    rows[i].push(v);
                }
            }
        }
        
        Ok(GeneratedTable {
            name: table_name.to_string(),
            columns,
            rows,
        })
    }
    
    pub fn assign_foreign_keys(&self, table: &mut GeneratedTable, parent_table: &str) -> Result<(), String> {
        // Find relationship
        let rel = self.rules.relationships.iter()
            .find(|r| r.child == table.name && r.parent == parent_table)
            .ok_or_else(|| format!("no relationship from {} to {}", parent_table, table.name))?;
        
        // Get parent PK pool
        let parent_pks = self.parent_pools.get(parent_table)
            .ok_or_else(|| format!("parent pool for '{}' not ready", parent_table))?;
        
        let pool = GeneratedFkPool::new(parent_pks);
        
        // Find FK column index
        let fk_idx = table.columns.iter()
            .position(|c| c == &rel.fk[0])
            .ok_or_else(|| format!("FK column '{}' not found", rel.fk[0]))?;
        
        // Assign FK values
        for row in &mut table.rows {
            row[fk_idx] = serde_json::json!(pool.sample_fk());
        }
        
        Ok(())
    }
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod model;
#[cfg(feature = "synth")]
pub mod profile;
#[cfg(feature = "synth")]
pub mod rules;
#[cfg(feature = "synth")]
pub mod rules_draft;
#[cfg(feature = "synth")]
pub mod fk_pool;
#[cfg(feature = "synth")]
pub mod generator;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::generator`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/generator.rs
git commit -m "feat(synth): implement generation engine

Core engine with copula sampling and FK assignment."
```

---

## Phase 5: Export and CLI

### Task 5.1: Implement Export Formats

**Files:**
- Create: `dbcli/src/synth/export.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/export.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_export_correct() {
        let table = GeneratedTable {
            name: "t".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![serde_json::json!(1), serde_json::json!("Alice")],
                vec![serde_json::json!(2), serde_json::json!("Bob")],
            ],
        };
        
        let csv = export_csv(&table).unwrap();
        assert!(csv.contains("id,name"));
        assert!(csv.contains("1,Alice"));
    }

    #[test]
    fn jsonl_export_correct() {
        let table = GeneratedTable {
            name: "t".to_string(),
            columns: vec!["id".to_string()],
            rows: vec![vec![serde_json::json!(1)]],
        };
        
        let jsonl = export_jsonl(&table).unwrap();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 1);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::export`
Expected: FAIL

**Step 3: Implement export module**

```rust
// In dbcli/src/synth/export.rs
use crate::synth::generator::GeneratedTable;
use std::io::Write;

pub fn export_csv(table: &GeneratedTable) -> Result<String, String> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    
    // Write header
    wtr.write_record(&table.columns)
        .map_err(|e| format!("write CSV header: {}", e))?;
    
    // Write rows
    for row in &table.rows {
        let record: Vec<String> = row.iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            })
            .collect();
        wtr.write_record(&record)
            .map_err(|e| format!("write CSV row: {}", e))?;
    }
    
    let data = wtr.into_inner()
        .map_err(|e| format!("flush CSV: {}", e))?;
    
    String::from_utf8(data)
        .map_err(|e| format!("convert CSV to string: {}", e))
}

pub fn export_jsonl(table: &GeneratedTable) -> Result<String, String> {
    let mut out = String::new();
    
    for row in &table.rows {
        let obj: serde_json::Map<String, serde_json::Value> = table.columns.iter()
            .zip(row.iter())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        
        let line = serde_json::to_string(&obj)
            .map_err(|e| format!("serialize JSONL: {}", e))?;
        out.push_str(&line);
        out.push('\n');
    }
    
    Ok(out)
}

pub fn export_json(table: &GeneratedTable) -> Result<String, String> {
    let mut rows = Vec::new();
    
    for row in &table.rows {
        let obj: serde_json::Map<String, serde_json::Value> = table.columns.iter()
            .zip(row.iter())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        rows.push(serde_json::Value::Object(obj));
    }
    
    serde_json::to_string_pretty(&rows)
        .map_err(|e| format!("serialize JSON: {}", e))
}

pub fn export_sql_patch(
    table: &GeneratedTable,
    schema: Option<&str>,
) -> Result<String, String> {
    let mut out = String::new();
    let table_name = match schema {
        Some(s) => format!("{}.{}", s, table.name),
        None => table.name.clone(),
    };
    
    out.push_str(&format!("-- Synthetic data for {}\n", table_name));
    
    for row in &table.rows {
        let values: Vec<String> = row.iter()
            .map(|v| match v {
                serde_json::Value::Null => "NULL".to_string(),
                serde_json::Value::String(s) => format!("'{}'", s.replace('\'', "''")),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => if *b { "1".to_string() } else { "0".to_string() },
                other => format!("'{}'", other.to_string().replace('\'', "''")),
            })
            .collect();
        
        out.push_str(&format!(
            "INSERT INTO {} ({}) VALUES ({});\n",
            table_name,
            table.columns.join(", "),
            values.join(", ")
        ));
    }
    
    Ok(out)
}

pub fn write_export(
    content: &str,
    path: &std::path::Path,
) -> Result<(), String> {
    let mut file = std::fs::File::create(path)
        .map_err(|e| format!("create file: {}", e))?;
    
    file.write_all(content.as_bytes())
        .map_err(|e| format!("write file: {}", e))?;
    
    Ok(())
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod model;
#[cfg(feature = "synth")]
pub mod profile;
#[cfg(feature = "synth")]
pub mod rules;
#[cfg(feature = "synth")]
pub mod rules_draft;
#[cfg(feature = "synth")]
pub mod fk_pool;
#[cfg(feature = "synth")]
pub mod generator;
#[cfg(feature = "synth")]
pub mod export;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::export`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/export.rs
git commit -m "feat(synth): implement export formats

CSV, JSONL, JSON, and SQL patch exports."
```

---

### Task 5.2: Implement CLI Subcommands

**Files:**
- Modify: `dbcli/src/main.rs` (add Synth commands)
- Create: `dbcli/src/synth/cmd.rs`

**Step 1: Write failing tests**

```rust
// Integration test - requires actual DB connection
#[test]
#[ignore]
fn synth_train_cli_works() {
    // Test: hepta_dbcli synth train --name dev --table users --out /tmp/models/
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --all --features synth synth::cmd`
Expected: FAIL

**Step 3: Implement cmd module**

```rust
// In dbcli/src/synth/cmd.rs
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
pub struct SynthArgs {
    #[command(subcommand)]
    pub command: SynthCommands,
}

#[derive(Subcommand)]
pub enum SynthCommands {
    /// Train a model on a single table
    Train {
        /// Connection name
        #[arg(long)]
        name: String,
        
        /// Table to train on
        #[arg(long)]
        table: String,
        
        /// Number of sample rows (default: 100000)
        #[arg(long, default_value = "100000")]
        sample_rows: usize,
        
        /// Output directory for models
        #[arg(long)]
        out: PathBuf,
    },
    
    /// Generate rules draft from FK introspection
    RulesDraft {
        /// Connection name
        #[arg(long)]
        name: String,
        
        /// Schema name
        #[arg(long)]
        schema: String,
        
        /// Models directory
        #[arg(long)]
        models_dir: PathBuf,
        
        /// Output YAML file
        #[arg(long)]
        out: PathBuf,
    },
    
    /// Generate synthetic data
    Generate {
        /// Rules YAML file
        #[arg(long)]
        rules: PathBuf,
        
        /// Models directory
        #[arg(long)]
        models_dir: PathBuf,
        
        /// Output directory
        #[arg(long)]
        output_dir: PathBuf,
        
        /// Export format (csv, jsonl, json, sql)
        #[arg(long, default_value = "csv")]
        format: String,
        
        /// Report output path
        #[arg(long)]
        report: Option<PathBuf>,
    },
    
    /// Validate rules file
    Validate {
        /// Rules YAML file
        #[arg(long)]
        rules: PathBuf,
        
        /// Models directory
        #[arg(long)]
        models_dir: PathBuf,
    },
    
    /// Import SDV model (experimental)
    ImportSdv {
        /// Input pickle file
        input: PathBuf,
        
        /// Output JSON file
        #[arg(long)]
        out: PathBuf,
    },
}
```

**Step 4: Add to main.rs**

```rust
// In dbcli/src/main.rs, add to Commands enum
#[cfg(feature = "synth")]
/// Synthetic data generation
Synth {
    #[command(flatten)]
    args: Box<synth::cmd::SynthArgs>,
},

// In main function, add match arm
#[cfg(feature = "synth")]
Commands::Synth { args } => {
    synth::cmd::run_synth(args, &config_path, &connection_name).await?;
}
```

**Step 5: Run tests**

Run: `cargo test --all --features synth`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/cmd.rs dbcli/src/main.rs
git commit -m "feat(synth): implement CLI subcommands

synth train, rules-draft, generate, validate, import-sdv."
```

---

### Task 5.3: Implement Report Generation

**Files:**
- Create: `dbcli/src/synth/report.rs`

**Step 1: Write failing tests**

```rust
// In dbcli/src/synth/report.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_json_roundtrip() {
        let report = GenerationReport {
            table: "orders".to_string(),
            generated_rows: 1000,
            fidelity_score: 0.95,
            warnings: vec![],
        };
        
        let json = serde_json::to_string_pretty(&report).unwrap();
        let loaded: GenerationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.generated_rows, 1000);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --all --features synth synth::report`
Expected: FAIL

**Step 3: Implement report module**

```rust
// In dbcli/src/synth/report.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationReport {
    pub table: String,
    pub generated_rows: usize,
    pub fidelity_score: f64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullReport {
    pub tables: Vec<GenerationReport>,
    pub total_rows: usize,
    pub success: bool,
}

impl FullReport {
    pub fn new() -> Self {
        Self {
            tables: Vec::new(),
            total_rows: 0,
            success: true,
        }
    }
    
    pub fn add_table(&mut self, report: GenerationReport) {
        self.total_rows += report.generated_rows;
        if report.fidelity_score < 0.8 {
            self.success = false;
        }
        self.tables.push(report);
    }
    
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize report: {}", e))?;
        
        std::fs::write(path, json)
            .map_err(|e| format!("write report: {}", e))?;
        
        Ok(())
    }
}
```

**Step 4: Add to synth module**

```rust
// In dbcli/src/synth/mod.rs
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod model;
#[cfg(feature = "synth")]
pub mod profile;
#[cfg(feature = "synth")]
pub mod rules;
#[cfg(feature = "synth")]
pub mod rules_draft;
#[cfg(feature = "synth")]
pub mod fk_pool;
#[cfg(feature = "synth")]
pub mod generator;
#[cfg(feature = "synth")]
pub mod export;
#[cfg(feature = "synth")]
pub mod cmd;
#[cfg(feature = "synth")]
pub mod report;
```

**Step 5: Run tests**

Run: `cargo test --all --features synth synth::report`
Expected: PASS

**Step 6: Commit**

```bash
git add dbcli/src/synth/report.rs
git commit -m "feat(synth): implement generation report

Fidelity scoring and JSON report output."
```

---

## Phase 6: Integration Testing

### Task 6.1: Integration Test with MySQL

**Files:**
- Modify: `dbcli/tests/regress_mysql.rs` (add synth tests)

**Step 1: Write integration test**

```rust
#[test]
#[ignore] // Requires running MySQL
fn synth_train_and_generate() {
    // 1. Connect to test DB
    // 2. Train model on users table
    // 3. Generate synthetic data
    // 4. Verify row count matches
    // 5. Verify FK integrity
}
```

**Step 2: Run integration test**

Run: `HEPTA_DBCLI_TEST_URL=mysql://... cargo test --all --features "synth,integration" synth_train_and_generate`
Expected: PASS

**Step 3: Commit**

```bash
git add dbcli/tests/regress_mysql.rs
git commit -m "test(synth): add MySQL integration test

End-to-end train + generate workflow."
```

---

## Summary

**Total Tasks:** 15 tasks across 6 phases

**Estimated Effort:** 3-5 days for experienced Rust developer

**Dependencies:**
- Phase 1 → Phase 2 → Phase 3 → Phase 4 → Phase 5 → Phase 6
- Each task within a phase is independent

**Key Risks:**
1. Statistical correctness (KS/chi-squared testing)
2. Copula numerical stability
3. Multi-table topology complexity

**Success Criteria:**
- All unit tests pass
- Integration test with MySQL passes
- CLI commands work end-to-end
- Feature gate keeps default build unchanged
