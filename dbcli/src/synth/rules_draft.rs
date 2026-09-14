use crate::synth::rules::{PoolStrategy, Relationship, SynthRules, TableRule};

#[derive(Debug, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    pub from_table: String,
    pub from_column: String,
    pub to_table: String,
    pub to_column: String,
}

pub fn generate_rules_draft(
    tables: &[String],
    foreign_keys: &[ForeignKeyInfo],
    table_stats: &std::collections::HashMap<String, TableStats>,
) -> SynthRules {
    let mut table_rules: std::collections::HashMap<String, TableRule> =
        std::collections::HashMap::new();

    for table in tables {
        table_rules.insert(
            table.clone(),
            TableRule {
                name: table.clone(),
                rows: None,
                relationships: vec![],
                strategy: crate::synth::rules::TableStrategy::default(),
            },
        );
    }

    for fk in foreign_keys {
        let unique = is_unique(fk, table_stats);

        let relationship = Relationship {
            pk: fk.from_column.clone(),
            references: vec![format!("{}.{}", fk.to_table, fk.to_column)],
            pool_strategy: PoolStrategy::Projection { unique },
            null_label: "null".to_string(),
        };

        if let Some(rule) = table_rules.get_mut(&fk.from_table) {
            rule.relationships.push(relationship);
        }
    }

    let mut tables: Vec<TableRule> = table_rules.into_values().collect();
    tables.sort_by(|a, b| a.name.cmp(&b.name));

    SynthRules {
        version: "1".to_string(),
        tables,
    }
}

fn is_unique(
    fk: &ForeignKeyInfo,
    table_stats: &std::collections::HashMap<String, TableStats>,
) -> bool {
    if let Some(stats) = table_stats.get(&fk.from_table) {
        if let Some(col_stats) = stats.columns.get(&fk.from_column) {
            return col_stats.cardinality == stats.row_count;
        }
    }
    false
}

pub struct TableStats {
    pub row_count: usize,
    pub columns: std::collections::HashMap<String, ColumnStats>,
}

pub struct ColumnStats {
    pub cardinality: usize,
}

pub fn generate_draft_from_profiles(
    tables: &[String],
    foreign_keys: &[ForeignKeyInfo],
    profiles: &std::collections::HashMap<String, crate::synth::profile::TableProfile>,
) -> SynthRules {
    let table_stats: std::collections::HashMap<String, TableStats> = profiles
        .iter()
        .map(|(name, profile)| {
            let columns: std::collections::HashMap<String, ColumnStats> = profile
                .columns
                .iter()
                .map(|(col_name, col_profile)| {
                    (
                        col_name.clone(),
                        ColumnStats {
                            cardinality: col_profile.cardinality,
                        },
                    )
                })
                .collect();

            (
                name.clone(),
                TableStats {
                    row_count: profile.row_count,
                    columns,
                },
            )
        })
        .collect();

    generate_rules_draft(tables, foreign_keys, &table_stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_creates_table_rules_for_all_tables() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let fks = vec![];
        let stats = std::collections::HashMap::new();

        let rules = generate_rules_draft(&tables, &fks, &stats);
        assert_eq!(rules.tables.len(), 2);
        assert!(rules.tables.iter().any(|t| t.name == "users"));
        assert!(rules.tables.iter().any(|t| t.name == "orders"));
    }

    #[test]
    fn draft_adds_relationships_for_foreign_keys() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let fks = vec![ForeignKeyInfo {
            from_table: "orders".to_string(),
            from_column: "user_id".to_string(),
            to_table: "users".to_string(),
            to_column: "id".to_string(),
        }];
        let stats = std::collections::HashMap::new();

        let rules = generate_rules_draft(&tables, &fks, &stats);
        let orders_rule = rules.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders_rule.relationships.len(), 1);
        assert_eq!(orders_rule.relationships[0].pk, "user_id");
        assert_eq!(
            orders_rule.relationships[0].references,
            vec!["users.id".to_string()]
        );
    }

    #[test]
    fn draft_detects_unique_foreign_keys() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let fks = vec![ForeignKeyInfo {
            from_table: "orders".to_string(),
            from_column: "user_id".to_string(),
            to_table: "users".to_string(),
            to_column: "id".to_string(),
        }];

        let mut columns = std::collections::HashMap::new();
        columns.insert("user_id".to_string(), ColumnStats { cardinality: 100 });
        let mut stats = std::collections::HashMap::new();
        stats.insert(
            "orders".to_string(),
            TableStats {
                row_count: 100,
                columns,
            },
        );

        let rules = generate_rules_draft(&tables, &fks, &stats);
        let orders_rule = rules.tables.iter().find(|t| t.name == "orders").unwrap();
        if let PoolStrategy::Projection { unique } = orders_rule.relationships[0].pool_strategy {
            assert!(unique);
        } else {
            panic!("expected Projection pool strategy");
        }
    }
}
