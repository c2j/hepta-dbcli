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
    /// Conditional sampling mode. `copula_conditional` is not implemented yet.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ValuePool {
    Weighted(BTreeMap<String, f64>),
    Uniform(Vec<String>),
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

                // `copula_conditional` is scheduled after this PR; reject it
                // rather than silently degrading to rejection sampling.
                if rule.mode == ColumnMode::CopulaConditional {
                    return Err(format!(
                        "table '{}' column '{}': mode 'copula_conditional' is not implemented yet",
                        table.name, column
                    ));
                }
            }
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
    fn rules_validate_catches_empty_references() {
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![TableRule {
                name: "t".to_string(),
                columns: HashMap::new(),
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
    fn should_reject_copula_conditional_mode_until_implemented() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      amount:
        fixed_range: [1, 5]
        mode: copula_conditional
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("copula_conditional must fail for now");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("amount"), "error must name the column: {err}");
        assert!(
            err.contains("not implemented"),
            "error must say not implemented: {err}"
        );
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
