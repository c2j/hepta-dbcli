use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthRules {
    pub version: String,
    pub tables: Vec<TableRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRule {
    pub name: String,
    pub relationships: Vec<Relationship>,
    #[serde(default)]
    pub strategy: TableStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub pk: String,
    pub references: Vec<String>,
    #[serde(default)]
    pub pool_strategy: PoolStrategy,
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
        for table in &self.tables {
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

    #[test]
    fn rules_validate_catches_empty_references() {
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![TableRule {
                name: "t".to_string(),
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
}
