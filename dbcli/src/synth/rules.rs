use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthRules {
    pub version: String,
    pub tables: Vec<TableRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRule {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<usize>,
    /// Per-column overrides keyed by column name. Generation prefers these
    /// over the values learned during training.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub columns: HashMap<String, ColumnRule>,
    /// Derived columns (issue #70): `column = expr`, evaluated after the row
    /// is generated. Expressions are whitelisted by `synth::expr`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derive: Vec<DeriveRule>,
    /// Branch coverage targets (issue #70): after generation, rows are
    /// rewritten until each predicate hits `target_ratio`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<BranchRule>,
    pub relationships: Vec<Relationship>,
    #[serde(default)]
    pub strategy: TableStrategy,
}

/// Column-level generation override. Absent fields fall back to the trained
/// model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ColumnRule {
    /// Overrides the learned NULL rate for this column (`0.0` = never NULL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub null_rate: Option<f64>,
    /// Forces the marginal family used while training this column, skipping
    /// the KS auto-selection: one of `ALLOWED_MARGINALS`. Only read by
    /// `synth train --rules`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marginal: Option<String>,
    /// Exact literal applied to every row, typed by the trained logical type.
    /// Mutually exclusive with `values`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed: Option<String>,
    /// Value pool: either weighted (`{value: weight}`) or uniform (`[value]`).
    /// Mutually exclusive with `fixed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<ValuePool>,
    /// Closed interval `[low, high]`; out-of-range draws are rejection-redrawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_range: Option<[serde_json::Value; 2]>,
    /// Conditional sampling mode: the copula draw is conditioned on this
    /// column's `fixed` value or `fixed_range` (issue #68).
    #[serde(default, skip_serializing_if = "ColumnMode::is_rejection")]
    pub mode: ColumnMode,
}

impl ColumnRule {
    /// True when the rule writes a per-row value (phase 4 of the priority
    /// table in `docs/plans/2026-09-15-synth-rules-v1-extension.md`).
    pub fn has_column_override(&self) -> bool {
        self.fixed.is_some() || self.values.is_some() || self.fixed_range.is_some()
    }
}

/// YAML accepts two shapes: `values: {a: 0.7, b: 0.3}` (weighted) and
/// `values: [a, b]` (uniform). Weighted pools use a `BTreeMap` so iteration
/// order is deterministic and same-seed generation is reproducible.
///
/// Keys are read as **scalars**, so the natural `values: {1: 0.7, 2: 0.3}` for
/// a numeric column works without quoting.
#[derive(Debug, Clone, PartialEq)]
pub enum ValuePool {
    Weighted(BTreeMap<String, f64>),
    Uniform(Vec<String>),
}

impl Serialize for ValuePool {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        match self {
            ValuePool::Uniform(values) => values.serialize(serializer),
            ValuePool::Weighted(weights) => {
                let mut map = serializer.serialize_map(Some(weights.len()))?;
                for (value, weight) in weights {
                    map.serialize_entry(value, weight)?;
                }
                map.end()
            }
        }
    }
}

/// Scalar key of a weighted pool: YAML `9`, `9.5`, `'9'` and `true` all map to
/// their literal spelling.
struct PoolKey(String);

impl<'de> Deserialize<'de> for PoolKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(PoolKeyVisitor).map(PoolKey)
    }
}

struct PoolKeyVisitor;

impl<'de> serde::de::Visitor<'de> for PoolKeyVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a scalar value-pool key")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<String, E> {
        Ok(value.to_string())
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<String, E> {
        Ok(value.to_string())
    }
}

impl<'de> Deserialize<'de> for ValuePool {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PoolVisitor;

        impl<'de> serde::de::Visitor<'de> for PoolVisitor {
            type Value = ValuePool;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a list of values or a map of value -> weight")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<ValuePool, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<String>()? {
                    values.push(value);
                }
                Ok(ValuePool::Uniform(values))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<ValuePool, A::Error> {
                let mut weights = BTreeMap::new();
                while let Some((key, weight)) = map.next_entry::<PoolKey, f64>()? {
                    weights.insert(key.0, weight);
                }
                Ok(ValuePool::Weighted(weights))
            }
        }

        deserializer.deserialize_any(PoolVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnMode {
    #[default]
    Rejection,
    CopulaConditional,
}

impl ColumnMode {
    fn is_rejection(&self) -> bool {
        matches!(self, Self::Rejection)
    }
}

/// One `column = expr` derivation (issue #70).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeriveRule {
    /// Column to overwrite with the evaluated expression.
    pub column: String,
    /// Arithmetic expression over other columns of the same table.
    pub expr: String,
}

/// One branch-coverage target (issue #70).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRule {
    /// Stable identifier used in reports.
    pub id: String,
    /// Boolean expression over the generated columns (`synth::expr` grammar).
    pub predicate: String,
    /// Share of rows the predicate should match.
    pub target_ratio: f64,
    /// Allowed absolute deviation from `target_ratio` (default 0.05).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    #[serde(default)]
    pub repair: BranchRepair,
}

/// How to rewrite rows that do not satisfy a branch yet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchRepair {
    /// Literal assignments keyed by column. Values are typed by the target
    /// column's logical type, exactly like `columns.<name>.fixed`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Recompute `derive` columns on the touched rows (default false, but
    /// derived columns are always recomputed because the pass is idempotent).
    #[serde(default)]
    pub linked_derive_recompute: bool,
}

/// Default branch tolerance when a rule does not set one.
pub const DEFAULT_BRANCH_TOLERANCE: f64 = 0.05;

/// Hard cap on repair rounds, mirroring shadow-seed's `max_rounds`.
pub const MAX_REPAIR_ROUNDS: usize = 10;

/// Marginal families a rules file may force on a column.
pub const ALLOWED_MARGINALS: [&str; 6] =
    ["normal", "beta", "gamma", "uniform", "ecdf", "categorical"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub pk: String,
    pub references: Vec<String>,
    #[serde(default)]
    pub pool_strategy: PoolStrategy,
    /// Parsed for YAML compatibility with existing rules files. Generation
    /// does not read this field; NULL injection uses per-column `null_rate`.
    #[serde(default = "default_null_label")]
    pub null_label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolStrategy {
    Projection { unique: bool },
    Generated { unique: bool },
    Fixed { values: Vec<String> },
}

impl Default for PoolStrategy {
    fn default() -> Self {
        Self::Projection { unique: false }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableStrategy {
    #[default]
    Uniform,
    Weighted,
    Zipf,
}

fn default_null_label() -> String {
    "null".to_string()
}

impl SynthRules {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("read rules file: {}", e))?;

        let rules: Self =
            serde_yaml::from_str(&content).map_err(|e| format!("parse rules YAML: {}", e))?;

        if rules.version != "1" {
            return Err(format!(
                "rules version '{}' not supported (expected '1')",
                rules.version
            ));
        }

        Ok(rules)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let yaml = serde_yaml::to_string(self).map_err(|e| format!("serialize rules: {}", e))?;

        std::fs::write(path, yaml).map_err(|e| format!("write rules file: {}", e))?;

        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        // "table.column" of every parent key referenced by any relationship.
        let referenced: std::collections::HashSet<String> = self
            .tables
            .iter()
            .flat_map(|t| t.relationships.iter())
            .flat_map(|r| r.references.iter())
            .cloned()
            .collect();

        for table in &self.tables {
            for (column, rule) in &table.columns {
                if let Some(name) = rule.marginal.as_deref() {
                    if !ALLOWED_MARGINALS.contains(&name) {
                        return Err(format!(
                            "table '{}' column '{}': unknown marginal '{}' (expected one of {})",
                            table.name,
                            column,
                            name,
                            ALLOWED_MARGINALS.join(", ")
                        ));
                    }
                }

                // V1: `fixed` and `values` are mutually exclusive.
                if rule.fixed.is_some() && rule.values.is_some() {
                    return Err(format!(
                        "table '{}' column '{}': 'fixed' and 'values' are mutually exclusive",
                        table.name, column
                    ));
                }

                let has_fixed_or_values = rule.fixed.is_some() || rule.values.is_some();

                // V2: fixed/values cannot be combined with a non-zero null rate.
                if has_fixed_or_values {
                    if let Some(rate) = rule.null_rate {
                        if rate > 0.0 {
                            return Err(format!(
                                "table '{}' column '{}': 'fixed'/'values' cannot be combined with null_rate > 0 (got {})",
                                table.name, column, rate
                            ));
                        }
                    }
                }

                // V3: fixed/values on a parent key referenced by another table
                // cannot stay unique.
                if has_fixed_or_values && referenced.contains(&format!("{}.{}", table.name, column))
                {
                    return Err(format!(
                        "table '{}' column '{}': 'fixed'/'values' on a parent key referenced by another table breaks uniqueness",
                        table.name, column
                    ));
                }

                // V4: fixed/values on a relationship pk (the FK child column)
                // would break referential integrity.
                if has_fixed_or_values && table.relationships.iter().any(|r| &r.pk == column) {
                    return Err(format!(
                        "table '{}' column '{}': 'fixed'/'values' on a relationship pk would break referential integrity",
                        table.name, column
                    ));
                }

                // V5: weighted value pools must be a valid distribution.
                if let Some(ValuePool::Weighted(weights)) = &rule.values {
                    let sum: f64 = weights.values().sum();
                    if (sum - 1.0).abs() > 1e-6 {
                        return Err(format!(
                            "table '{}' column '{}': 'values' weights sum to {} (must be 1.0 ± 1e-6)",
                            table.name, column, sum
                        ));
                    }
                    if let Some((value, weight)) =
                        weights.iter().find(|(_, weight)| **weight <= 0.0)
                    {
                        return Err(format!(
                            "table '{}' column '{}': 'values' weight for '{}' is {} (must be > 0)",
                            table.name, column, value, weight
                        ));
                    }
                }

                // V6: fixed_range endpoints must be comparable and ordered.
                if let Some(range) = &rule.fixed_range {
                    validate_fixed_range(&table.name, column, range)?;
                }

                // `copula_conditional` conditions the copula draw on this
                // column, so it needs exactly one pinned value or range. A
                // row-independent `values` pool has nothing to condition on.
                if rule.mode == ColumnMode::CopulaConditional {
                    if rule.values.is_some() {
                        return Err(format!(
                            "table '{}' column '{}': mode 'copula_conditional' cannot be combined with a 'values' pool (a pool is row-independent)",
                            table.name, column
                        ));
                    }
                    if rule.fixed.is_none() && rule.fixed_range.is_none() {
                        return Err(format!(
                            "table '{}' column '{}': mode 'copula_conditional' needs a 'fixed' value or a 'fixed_range' to condition on",
                            table.name, column
                        ));
                    }
                }
            }
            validate_derive_rules(table)?;
            validate_branches(table)?;

            for rel in &table.relationships {
                if rel.references.is_empty() {
                    return Err(format!(
                        "table '{}' relationship '{}' has no references",
                        table.name, rel.pk
                    ));
                }
            }
        }
        Ok(())
    }
}

/// `#70` branch rules: valid predicate, sane ratio/tolerance, unique ids and a
/// repair that can actually change something.
fn validate_branches(table: &TableRule) -> Result<(), String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for branch in &table.branches {
        if !seen.insert(branch.id.as_str()) {
            return Err(format!(
                "table '{}': branch id '{}' is declared more than once",
                table.name, branch.id
            ));
        }

        crate::synth::expr::Expr::parse(&branch.predicate).map_err(|e| {
            format!(
                "table '{}' branch '{}': predicate '{}' rejected: {}",
                table.name, branch.id, branch.predicate, e
            )
        })?;

        if !branch.target_ratio.is_finite() || !(0.0..=1.0).contains(&branch.target_ratio) {
            return Err(format!(
                "table '{}' branch '{}': target_ratio {} must be within [0, 1]",
                table.name, branch.id, branch.target_ratio
            ));
        }

        if let Some(tolerance) = branch.tolerance {
            if !tolerance.is_finite() || tolerance <= 0.0 || tolerance > 1.0 {
                return Err(format!(
                    "table '{}' branch '{}': tolerance {} must be within (0, 1]",
                    table.name, branch.id, tolerance
                ));
            }
        }

        if branch.repair.set.is_empty() {
            return Err(format!(
                "table '{}' branch '{}': repair.set is empty, so the branch can never be repaired",
                table.name, branch.id
            ));
        }
    }

    Ok(())
}

/// `#70` derive rules: whitelist the expression, reject priority conflicts
/// and detect cycles in the derive graph.
fn validate_derive_rules(table: &TableRule) -> Result<(), String> {
    let mut referenced: std::collections::BTreeMap<&str, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for derive in &table.derive {
        if !seen.insert(derive.column.as_str()) {
            return Err(format!(
                "table '{}': derive lists column '{}' more than once",
                table.name, derive.column
            ));
        }

        if let Some(column_rule) = table.columns.get(&derive.column) {
            if column_rule.has_column_override() {
                return Err(format!(
                    "table '{}' column '{}': a derived column cannot also use 'fixed'/'values'/'fixed_range'",
                    table.name, derive.column
                ));
            }
        }

        if table
            .relationships
            .iter()
            .any(|rel| rel.pk == derive.column)
        {
            return Err(format!(
                "table '{}' column '{}': a derived column cannot be a relationship pk (referential integrity)",
                table.name, derive.column
            ));
        }

        let expr = crate::synth::expr::Expr::parse(&derive.expr).map_err(|e| {
            format!(
                "table '{}' derive '{}': expression '{}' rejected: {}",
                table.name, derive.column, derive.expr, e
            )
        })?;
        referenced.insert(derive.column.as_str(), expr.referenced_columns());
    }

    // Cycle detection over derive columns only: an edge target -> referenced
    // means "target depends on referenced".
    let mut indegree: std::collections::BTreeMap<&str, usize> =
        referenced.keys().map(|key| (*key, 0)).collect();
    let mut dependents: std::collections::BTreeMap<&str, Vec<&str>> =
        std::collections::BTreeMap::new();
    for (target, deps) in &referenced {
        for dep in deps {
            if referenced.contains_key(dep.as_str()) {
                if let Some(count) = indegree.get_mut(target) {
                    *count += 1;
                }
                dependents.entry(dep.as_str()).or_default().push(target);
            }
        }
    }

    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(key, _)| *key)
        .collect();
    let mut resolved = 0usize;
    while let Some(node) = ready.pop() {
        resolved += 1;
        if let Some(next) = dependents.get(node) {
            for target in next {
                if let Some(count) = indegree.get_mut(*target) {
                    *count -= 1;
                    if *count == 0 {
                        ready.push(target);
                    }
                }
            }
        }
    }

    if resolved != referenced.len() {
        let mut cycle: Vec<&str> = indegree
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(key, _)| *key)
            .collect();
        cycle.sort_unstable();
        return Err(format!(
            "table '{}': derive rules form a cycle involving {}",
            table.name,
            cycle.join(", ")
        ));
    }

    Ok(())
}

fn validate_fixed_range(
    table: &str,
    column: &str,
    range: &[serde_json::Value; 2],
) -> Result<(), String> {
    let (low, high) = (&range[0], &range[1]);
    if let (Some(low), Some(high)) = (low.as_f64(), high.as_f64()) {
        if low > high {
            return Err(format!(
                "table '{}' column '{}': fixed_range low {} is greater than high {}",
                table, column, low, high
            ));
        }
        return Ok(());
    }
    if let (Some(low), Some(high)) = (low.as_str(), high.as_str()) {
        if low > high {
            return Err(format!(
                "table '{}' column '{}': fixed_range low '{}' is greater than high '{}'",
                table, column, low, high
            ));
        }
        return Ok(());
    }
    Err(format!(
        "table '{}' column '{}': fixed_range endpoints must both be numbers or both be strings",
        table, column
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_parse_valid_yaml() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    relationships:
      - pk: user_id
        references: [users.id]
        pool_strategy: !projection
          unique: false
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(rules.tables.len(), 1);
        assert_eq!(rules.tables[0].name, "orders");
    }

    #[test]
    fn should_parse_per_table_rows() {
        let yaml = r#"
version: "1"
tables:
  - name: customer
    rows: 599
    relationships: []
  - name: rental
    relationships: []
"#;

        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(rules.tables[0].rows, Some(599));
        assert_eq!(rules.tables[1].rows, None);
        let serialized = serde_yaml::to_string(&rules).unwrap();
        assert_eq!(serialized.matches("rows:").count(), 1);
    }

    #[test]
    fn rules_reject_invalid_version() {
        let yaml = r#"
version: "2"
tables: []
"#;
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("test_rules_invalid.yaml");
        std::fs::write(&path, yaml).unwrap();
        let result = SynthRules::load(&path);
        std::fs::remove_file(&path).ok();
        assert!(result.is_err());
    }

    /// Guards the schema extension in
    /// `docs/plans/2026-09-15-synth-rules-v1-extension.md`: a rules file that
    /// predates every new field must keep parsing and validating.
    #[test]
    fn should_accept_legacy_yaml_without_new_fields() {
        let yaml = r#"
version: "1"
tables:
  - name: users
    rows: 10
    relationships: []
  - name: orders
    columns:
      user_id:
        null_rate: 0.0
      amount:
        marginal: normal
    relationships:
      - pk: user_id
        references: [users.id]
        pool_strategy: !projection
          unique: false
    strategy: weighted
"#;

        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        rules.validate().unwrap();

        assert_eq!(rules.version, "1");
        assert_eq!(rules.tables[1].columns.len(), 2);
        assert_eq!(rules.tables[1].columns["user_id"].null_rate, Some(0.0));
        assert_eq!(
            rules.tables[1].columns["amount"].marginal.as_deref(),
            Some("normal")
        );
        assert!(matches!(rules.tables[1].strategy, TableStrategy::Weighted));
    }

    #[test]
    fn should_accept_derive_rules_with_known_shape() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      total:
        null_rate: 0.0
    derive:
      - column: total
        expr: "price * qty"
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(rules.tables[0].derive.len(), 1);
        assert_eq!(rules.tables[0].derive[0].column, "total");
        assert_eq!(rules.tables[0].derive[0].expr, "price * qty");
    }

    #[test]
    fn should_accept_branch_rules_with_known_shape() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    branches:
      - id: paid
        predicate: "status == 'A'"
        target_ratio: 0.30
        tolerance: 0.015
        repair:
          set:
            status: "A"
          linked_derive_recompute: true
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        let branch = &rules.tables[0].branches[0];
        assert_eq!(branch.id, "paid");
        assert_eq!(branch.target_ratio, 0.30);
        assert_eq!(branch.tolerance, Some(0.015));
        assert_eq!(branch.repair.set["status"], "A");
        assert!(branch.repair.linked_derive_recompute);
        rules.validate().unwrap();
    }

    #[test]
    fn should_reject_branch_with_a_bad_predicate() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    branches:
      - id: broken
        predicate: "status =="
        target_ratio: 0.3
        repair: {set: {status: "A"}}
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("a broken predicate must fail");
        assert!(err.contains("broken"), "error must name the branch: {err}");
        assert!(
            err.contains("status"),
            "error must quote the predicate: {err}"
        );
    }

    #[test]
    fn should_reject_branch_with_an_out_of_range_target_ratio() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    branches:
      - id: paid
        predicate: "status == 'A'"
        target_ratio: 1.5
        repair: {set: {status: "A"}}
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("target_ratio > 1 must fail");
        assert!(
            err.contains("target_ratio"),
            "error must name the field: {err}"
        );
    }

    #[test]
    fn should_reject_branch_with_an_empty_repair_set() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    branches:
      - id: paid
        predicate: "status == 'A'"
        target_ratio: 0.3
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("an empty repair.set must fail");
        assert!(
            err.contains("repair.set"),
            "error must name the field: {err}"
        );
    }

    #[test]
    fn should_reject_duplicate_branch_ids() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    branches:
      - id: paid
        predicate: "status == 'A'"
        target_ratio: 0.3
        repair: {set: {status: "A"}}
      - id: paid
        predicate: "status == 'B'"
        target_ratio: 0.3
        repair: {set: {status: "B"}}
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("duplicate ids must fail");
        assert!(err.contains("paid"), "error must name the branch: {err}");
    }

    #[test]
    fn should_reject_derive_with_a_disallowed_expression_node() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    derive:
      - column: total
        expr: "min(price, qty)"
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("function calls must be rejected");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("total"), "error must name the target: {err}");
        assert!(
            err.contains("function call"),
            "error must name the offending node: {err}"
        );
    }

    #[test]
    fn should_reject_derive_on_a_column_that_is_also_fixed() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      total:
        fixed: "1"
    derive:
      - column: total
        expr: "price * qty"
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("fixed + derive must conflict");
        assert!(err.contains("total"), "error must name the column: {err}");
        assert!(err.contains("fixed"), "error must explain the clash: {err}");
    }

    #[test]
    fn should_reject_derive_cycles() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    derive:
      - column: a
        expr: "b + 1"
      - column: b
        expr: "a + 1"
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("a derive cycle must fail");
        assert!(err.contains("cycle"), "error must say cycle: {err}");
    }

    #[test]
    fn should_accept_derive_chains_in_dependency_order() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    derive:
      - column: c
        expr: "b + 1"
      - column: b
        expr: "price * 2"
    relationships: []
"#;
        validate_yaml(yaml).expect("acyclic derive chain is valid");
    }

    #[test]
    fn rules_validate_catches_empty_references() {
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![TableRule {
                name: "t".to_string(),
                columns: HashMap::new(),
                derive: vec![],
                branches: vec![],
                rows: None,
                relationships: vec![Relationship {
                    pk: "id".to_string(),
                    references: vec![],
                    pool_strategy: PoolStrategy::Projection { unique: false },
                    null_label: "null".to_string(),
                }],
                strategy: TableStrategy::Uniform,
            }],
        };

        let result = rules.validate();
        assert!(result.is_err());
    }

    #[test]
    fn generated_pool_strategy_parses() {
        let yaml = r#"
version: "1"
tables:
  - name: t
    relationships:
      - pk: id
        references: [other.id]
        pool_strategy: !generated
          unique: true
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        if let PoolStrategy::Generated { unique } = rules.tables[0].relationships[0].pool_strategy {
            assert!(unique);
        } else {
            panic!("expected Generated pool strategy");
        }
    }

    #[test]
    fn should_parse_legacy_table_rule_without_columns_section() {
        let yaml = r#"
version: "1"
tables:
  - name: users
    rows: 10
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        assert!(rules.tables[0].columns.is_empty());
    }

    #[test]
    fn should_parse_column_null_rate_override() {
        let yaml = r#"
version: "1"
tables:
  - name: users
    columns:
      email:
        null_rate: 0.35
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(rules.tables[0].columns["email"].null_rate, Some(0.35));

        let serialized = serde_yaml::to_string(&rules).unwrap();
        assert!(serialized.contains("null_rate: 0.35"));
    }

    #[test]
    fn should_parse_column_marginal_override() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      amount:
        marginal: gamma
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            rules.tables[0].columns["amount"].marginal.as_deref(),
            Some("gamma")
        );
        assert_eq!(rules.tables[0].columns["amount"].null_rate, None);
        rules.validate().unwrap();

        let serialized = serde_yaml::to_string(&rules).unwrap();
        assert!(serialized.contains("marginal: gamma"));
    }

    #[test]
    fn should_leave_marginal_absent_for_legacy_column_rule() {
        let yaml = r#"
version: "1"
tables:
  - name: users
    columns:
      email:
        null_rate: 0.35
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(rules.tables[0].columns["email"].marginal, None);
        let serialized = serde_yaml::to_string(&rules).unwrap();
        assert!(!serialized.contains("marginal"));
    }

    #[test]
    fn rules_validate_rejects_unknown_marginal_name() {
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![TableRule {
                name: "t".to_string(),
                columns: HashMap::from([(
                    "amount".to_string(),
                    ColumnRule {
                        null_rate: None,
                        marginal: Some("kde".to_string()),
                        ..Default::default()
                    },
                )]),
                derive: vec![],
                branches: vec![],
                rows: None,
                relationships: vec![],
                strategy: TableStrategy::Uniform,
            }],
        };

        let err = rules.validate().expect_err("unknown marginal must fail");
        assert!(err.contains("kde"), "error should name the value: {err}");
    }

    fn validate_yaml(yaml: &str) -> Result<(), String> {
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        rules.validate()
    }

    #[test]
    fn should_reject_fixed_and_values_together() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      part_date:
        fixed: "20240101"
        values:
          a: 0.5
          b: 0.5
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("fixed + values must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(
            err.contains("part_date"),
            "error must name the column: {err}"
        );
    }

    #[test]
    fn should_reject_fixed_with_nonzero_null_rate() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      part_date:
        fixed: "20240101"
        null_rate: 0.1
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("fixed + null_rate > 0 must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(
            err.contains("part_date"),
            "error must name the column: {err}"
        );
    }

    #[test]
    fn should_reject_fixed_on_referenced_parent_key() {
        let yaml = r#"
version: "1"
tables:
  - name: users
    columns:
      id:
        fixed: "1"
    relationships: []
  - name: orders
    relationships:
      - pk: user_id
        references: [users.id]
"#;
        let err = validate_yaml(yaml).expect_err("fixed referenced parent key must fail");
        assert!(err.contains("users"), "error must name the table: {err}");
        assert!(err.contains("id"), "error must name the column: {err}");
    }

    #[test]
    fn should_reject_values_on_relationship_pk() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      user_id:
        values: [a, b]
    relationships:
      - pk: user_id
        references: [users.id]
  - name: users
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("values on a relationship pk must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("user_id"), "error must name the column: {err}");
    }

    #[test]
    fn should_read_unquoted_numeric_value_pool_keys() {
        // `values: {1: 0.7, 2: 0.3}` is the natural spelling for a numeric
        // column; requiring quotes would be a footgun found only at runtime.
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      part_id:
        values: {1: 0.7, 2: 0.3}
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        match rules.tables[0].columns["part_id"].values.as_ref().unwrap() {
            ValuePool::Weighted(weights) => {
                assert_eq!(weights["1"], 0.7);
                assert_eq!(weights["2"], 0.3);
            }
            other => panic!("expected a weighted pool, got {other:?}"),
        }
        rules.validate().unwrap();
    }

    #[test]
    fn should_read_quoted_and_float_value_pool_keys() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      code:
        values: {"A": 0.5, 1.5: 0.5}
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        match rules.tables[0].columns["code"].values.as_ref().unwrap() {
            ValuePool::Weighted(weights) => {
                assert_eq!(weights["A"], 0.5);
                assert_eq!(weights["1.5"], 0.5);
            }
            other => panic!("expected a weighted pool, got {other:?}"),
        }
    }

    #[test]
    fn should_round_trip_both_value_pool_shapes() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      status:
        values: {a: 0.7, b: 0.3}
      kind:
        values: [x, y]
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        let text = serde_yaml::to_string(&rules).unwrap();
        let reparsed: SynthRules = serde_yaml::from_str(&text).unwrap();
        assert_eq!(
            reparsed.tables[0].columns["status"].values,
            rules.tables[0].columns["status"].values
        );
        assert_eq!(
            reparsed.tables[0].columns["kind"].values,
            rules.tables[0].columns["kind"].values
        );
    }

    #[test]
    fn should_reject_value_pool_with_invalid_weights() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      status:
        values:
          normal: 0.7
          peak: 0.4
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("weights summing to 1.1 must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("status"), "error must name the column: {err}");
        assert!(err.contains("1.1"), "error must name the sum: {err}");
    }

    #[test]
    fn should_reject_value_pool_with_nonpositive_weight() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      status:
        values:
          normal: 1.1
          peak: -0.1
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("a <= 0 weight must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("status"), "error must name the column: {err}");
    }

    #[test]
    fn should_accept_copula_conditional_with_fixed_range() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      part_date:
        fixed_range: [20240101, 20240131]
        mode: copula_conditional
    relationships: []
"#;
        validate_yaml(yaml).expect("copula_conditional with a range is valid");
    }

    #[test]
    fn should_reject_copula_conditional_without_a_pin() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      amount:
        mode: copula_conditional
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("a mode without a pin must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("amount"), "error must name the column: {err}");
        assert!(
            err.contains("copula_conditional") && err.contains("fixed"),
            "error must explain what is missing: {err}"
        );
    }

    #[test]
    fn should_reject_copula_conditional_with_value_pool() {
        // A weighted pool is row-independent, so there is nothing to condition
        // the copula on.
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      status:
        values: [a, b]
        mode: copula_conditional
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("values + copula_conditional must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("status"), "error must name the column: {err}");
    }

    #[test]
    fn should_reject_fixed_range_low_above_high() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      created_at:
        fixed_range: ["2026-01-31", "2026-01-01"]
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("low > high must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(
            err.contains("created_at"),
            "error must name the column: {err}"
        );
    }

    #[test]
    fn should_reject_fixed_range_with_mixed_endpoint_types() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      amount:
        fixed_range: [1, "5"]
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("mixed endpoint types must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("amount"), "error must name the column: {err}");
    }

    #[test]
    fn should_accept_valid_fixed_range() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      amount:
        fixed_range: [1, 5]
    relationships: []
"#;
        validate_yaml(yaml).expect("a valid numeric fixed_range must pass");
    }

    #[test]
    fn should_parse_weighted_and_uniform_value_pools() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      status:
        values:
          normal: 0.7
          peak: 0.3
      region:
        values: [CN, US, EU]
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        let columns = &rules.tables[0].columns;
        assert!(matches!(
            columns["status"].values,
            Some(ValuePool::Weighted(_))
        ));
        assert!(matches!(
            columns["region"].values,
            Some(ValuePool::Uniform(_))
        ));
    }

    #[test]
    fn rules_validate_accepts_every_documented_marginal_name() {
        for name in ALLOWED_MARGINALS {
            let rules = SynthRules {
                version: "1".to_string(),
                tables: vec![TableRule {
                    name: "t".to_string(),
                    columns: HashMap::from([(
                        "amount".to_string(),
                        ColumnRule {
                            null_rate: None,
                            marginal: Some(name.to_string()),
                            ..Default::default()
                        },
                    )]),
                    derive: vec![],
                    branches: vec![],
                    rows: None,
                    relationships: vec![],
                    strategy: TableStrategy::Uniform,
                }],
            };
            rules
                .validate()
                .unwrap_or_else(|e| panic!("'{name}' should be accepted: {e}"));
        }
    }
}
