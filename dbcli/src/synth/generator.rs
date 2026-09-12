use crate::synth::copula::GaussianCopula;
use crate::synth::fk_pool::{FkPool, SelectionStrategy};
use crate::synth::model::TableModel;
use crate::synth::rules::{PoolStrategy, SynthRules, TableStrategy};
use rand::SeedableRng;
use serde_json::Value;
use std::collections::HashMap;

pub struct GeneratorConfig {
    pub rows_per_table: HashMap<String, usize>,
    pub seed: Option<u64>,
}

#[derive(Debug)]
pub struct GeneratedData {
    pub tables: HashMap<String, Vec<Vec<Value>>>,
    pub columns: HashMap<String, Vec<String>>,
    pub dialect: String,
}

struct RelPool {
    column: String,
    pool: FkPool,
    strategy: SelectionStrategy,
    unique: bool,
    pool_size: usize,
}

pub fn generate(
    models: &HashMap<String, TableModel>,
    rules: &SynthRules,
    config: &GeneratorConfig,
) -> Result<GeneratedData, String> {
    let mut rng = if let Some(s) = config.seed {
        rand::rngs::StdRng::seed_from_u64(s)
    } else {
        rand::rngs::StdRng::from_entropy()
    };

    let table_order = crate::synth::graph::topological_sort(
        &rules
            .tables
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<String>>(),
        &fk_edges(rules),
    )
    .map_err(|e| format!("cycle detected: {}", e))?;

    let mut tables: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
    let mut table_columns: HashMap<String, Vec<String>> = HashMap::new();
    let mut dialect = "mysql".to_string();

    // "table.column" -> 该列已生成的全部值；子表 FK 从这里采样，保证引用完整性
    let mut column_pools: HashMap<String, Vec<Value>> = HashMap::new();

    for table_name in &table_order {
        let model = models
            .get(table_name)
            .ok_or_else(|| format!("no model for table '{}'", table_name))?;

        let rule = rules
            .tables
            .iter()
            .find(|t| &t.name == table_name)
            .ok_or_else(|| format!("no rule for table '{}'", table_name))?;

        let row_count = config
            .rows_per_table
            .get(table_name)
            .copied()
            .unwrap_or(100);

        let strategy = match rule.strategy {
            TableStrategy::Uniform => SelectionStrategy::Uniform,
            TableStrategy::Zipf => SelectionStrategy::Zipf,
            TableStrategy::Weighted => {
                return Err(format!(
                    "table '{}': Weighted strategy is not supported; use uniform or zipf",
                    table_name
                ));
            }
        };

        let mut rel_pools = build_rel_pools(table_name, rule, &column_pools, strategy)?;

        let column_order = &model.copula.column_order;
        let copula = GaussianCopula::new(model.copula.correlation.clone());
        let uniform_samples = copula.sample(row_count, table_seed(config.seed, table_name));

        let mut rows = Vec::with_capacity(row_count);

        for t in 0..row_count {
            let mut row = Vec::with_capacity(column_order.len());

            for (col_idx, col_name) in column_order.iter().enumerate() {
                if let Some(rel) = rel_pools.iter_mut().find(|r| &r.column == col_name) {
                    let value = if rel.unique {
                        rel.pool.sample_unique(&mut rng).ok_or_else(|| {
                            format!(
                                "table '{}': unique FK '{}' exhausted its parent pool \
                                 ({} parent rows); reduce row count or set unique: false",
                                table_name, rel.column, rel.pool_size
                            )
                        })?
                    } else {
                        rel.pool.sample_one(rel.strategy, &mut rng).ok_or_else(|| {
                            format!(
                                "FK pool for '{}.{}' is empty; parent table generated no rows",
                                table_name, col_name
                            )
                        })?
                    };
                    row.push(value);
                    continue;
                }

                let uniform_val = uniform_samples
                    .get(col_idx)
                    .and_then(|col| col.get(t))
                    .copied()
                    .unwrap_or(0.5);

                let column_model = model.columns.get(col_name);
                match column_model.map(|c| &c.marginal) {
                    Some(crate::synth::marginal::Marginal::Categorical(p)) => {
                        let idx = p.sample_index(uniform_val);
                        row.push(Value::String(
                            p.values.get(idx).cloned().unwrap_or_default(),
                        ));
                    }
                    Some(marginal) => {
                        let generated = marginal.inverse_cdf(uniform_val);
                        if column_model.and_then(|c| c.rounding) == Some(0) {
                            row.push(Value::from(generated.round() as i64));
                        } else {
                            row.push(Value::from(generated));
                        }
                    }
                    None => row.push(Value::Null),
                }
            }

            rows.push(row);
        }

        for (col_idx, col_name) in column_order.iter().enumerate() {
            let values: Vec<Value> = rows
                .iter()
                .filter_map(|r| r.get(col_idx).cloned())
                .collect();
            column_pools.insert(format!("{}.{}", table_name, col_name), values);
        }

        table_columns.insert(table_name.clone(), column_order.clone());
        if model.dialect != "test" {
            dialect = model.dialect.clone();
        }
        tables.insert(table_name.clone(), rows);
    }

    Ok(GeneratedData {
        tables,
        columns: table_columns,
        dialect,
    })
}

// 同一 --seed 下各表不能共用一条高斯流：djb2（跨平台/版本稳定）混淆出每表种子
fn table_seed(base: Option<u64>, table: &str) -> Option<u64> {
    base.map(|s| {
        let mut h = 5381u64;
        for b in table.as_bytes() {
            h = h.wrapping_mul(33).wrapping_add(u64::from(*b));
        }
        s ^ h
    })
}

fn fk_edges(rules: &SynthRules) -> Vec<(String, String)> {
    let known: std::collections::HashSet<&str> =
        rules.tables.iter().map(|t| t.name.as_str()).collect();
    let mut edges = Vec::new();
    for t in &rules.tables {
        for r in &t.relationships {
            for ref_str in &r.references {
                let parts: Vec<&str> = ref_str.split('.').collect();
                if parts.len() == 2 && known.contains(parts[0]) {
                    // 边语义 (from, to) = from 先于 to：父表必须先生成
                    edges.push((parts[0].to_string(), t.name.clone()));
                }
            }
        }
    }
    edges
}

fn build_rel_pools(
    table_name: &str,
    rule: &crate::synth::rules::TableRule,
    column_pools: &HashMap<String, Vec<Value>>,
    strategy: SelectionStrategy,
) -> Result<Vec<RelPool>, String> {
    let mut rel_pools = Vec::new();
    for rel in &rule.relationships {
        let ref_str = rel
            .references
            .first()
            .ok_or_else(|| format!("relationship '{}' has no references", rel.pk))?;

        let (pool, unique) = match &rel.pool_strategy {
            PoolStrategy::Fixed { values } => (
                FkPool::new(values.iter().map(|v| Value::String(v.clone())).collect()),
                false,
            ),
            PoolStrategy::Projection { unique } | PoolStrategy::Generated { unique } => {
                let values = column_pools.get(ref_str).ok_or_else(|| {
                    format!(
                        "table '{}' references '{}' but that table.column was not generated \
                         first; add a rule and model for it",
                        table_name, ref_str
                    )
                })?;
                (FkPool::new(values.clone()), *unique)
            }
        };

        if unique && strategy == SelectionStrategy::Zipf {
            return Err(format!(
                "table '{}': zipf strategy cannot be combined with unique FK '{}'; \
                 unique requires uniform selection",
                table_name, rel.pk
            ));
        }

        let pool_size = pool.len();

        rel_pools.push(RelPool {
            column: rel.pk.clone(),
            pool,
            strategy,
            unique,
            pool_size,
        });
    }
    Ok(rel_pools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::marginal::{CategoricalParams, Marginal, NormalParams};
    use crate::synth::model::{ColumnModel, CopulaInfo, LogicalType, Provenance};
    use crate::synth::rules::{Relationship, TableRule};

    fn numerical_model(table: &str, column: &str, loc: f64, scale: f64) -> TableModel {
        let mut columns = HashMap::new();
        columns.insert(
            column.to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                marginal: Marginal::Normal(NormalParams { loc, scale }),
            },
        );
        TableModel {
            version: 1,
            table: table.to_string(),
            dialect: "mysql".to_string(),
            provenance: Provenance {
                source: "test".to_string(),
                converter_version: None,
                sdv_version: None,
            },
            pk: vec![column.to_string()],
            columns,
            copula: CopulaInfo {
                column_order: vec![column.to_string()],
                correlation: vec![vec![1.0]],
            },
        }
    }

    fn single_rule(table: &str, relationships: Vec<Relationship>) -> TableRule {
        TableRule {
            name: table.to_string(),
            relationships,
            strategy: TableStrategy::default(),
        }
    }

    fn config(tables: &[&str], rows: usize) -> GeneratorConfig {
        GeneratorConfig {
            rows_per_table: tables.iter().map(|t| (t.to_string(), rows)).collect(),
            seed: Some(42),
        }
    }

    #[test]
    fn generator_produces_correct_row_count() {
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("users", vec![])],
        };

        let config = config(&["users"], 50);

        let result = generate(&models, &rules, &config).unwrap();
        assert_eq!(result.tables.get("users").unwrap().len(), 50);
    }

    #[test]
    fn child_fk_values_come_from_parent_column() {
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );

        // orders 模型含全部列（与真实训练产物一致）：total + FK 列 user_id
        let mut order_columns = HashMap::new();
        order_columns.insert(
            "total".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 10.0,
                    scale: 2.0,
                }),
            },
        );
        order_columns.insert(
            "user_id".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 0.0,
                    scale: 1.0,
                }),
            },
        );
        models.insert(
            "orders".to_string(),
            TableModel {
                version: 1,
                table: "orders".to_string(),
                dialect: "mysql".to_string(),
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                },
                pk: vec![],
                columns: order_columns,
                copula: CopulaInfo {
                    column_order: vec!["total".to_string(), "user_id".to_string()],
                    correlation: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
                },
            },
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![
                single_rule(
                    "orders",
                    vec![Relationship {
                        pk: "user_id".to_string(),
                        references: vec!["users.id".to_string()],
                        pool_strategy: PoolStrategy::Projection { unique: false },
                        null_label: "null".to_string(),
                    }],
                ),
                single_rule("users", vec![]),
            ],
        };

        let config = config(&["users", "orders"], 30);

        let result = generate(&models, &rules, &config).unwrap();

        let users = result.tables.get("users").unwrap();
        let orders = result.tables.get("orders").unwrap();
        assert_eq!(users.len(), 30);
        assert_eq!(orders.len(), 30);

        let parent_ids: std::collections::HashSet<u64> = users
            .iter()
            .filter_map(|r| r.first())
            .filter_map(|v| v.as_f64())
            .map(|f| f.to_bits())
            .collect();
        assert!(!parent_ids.is_empty());

        let user_id_idx = 1;
        for row in orders {
            let f = row[user_id_idx].as_f64().expect("FK must stay numeric");
            assert!(
                parent_ids.contains(&f.to_bits()),
                "FK value {} not in parent ids",
                f
            );
        }
    }

    #[test]
    fn generate_errors_when_reference_target_missing() {
        let mut models = HashMap::new();
        models.insert(
            "orders".to_string(),
            numerical_model("orders", "total", 10.0, 2.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule(
                "orders",
                vec![Relationship {
                    pk: "total".to_string(),
                    references: vec!["ghost.id".to_string()],
                    pool_strategy: PoolStrategy::Projection { unique: false },
                    null_label: "null".to_string(),
                }],
            )],
        };

        let config = config(&["orders"], 5);
        let err = generate(&models, &rules, &config).unwrap_err();
        assert!(
            err.contains("ghost.id"),
            "error should name the target: {}",
            err
        );
    }

    #[test]
    fn weighted_strategy_rejected_with_clear_error() {
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![TableRule {
                name: "users".to_string(),
                relationships: vec![],
                strategy: TableStrategy::Weighted,
            }],
        };

        let config = config(&["users"], 5);
        let err = generate(&models, &rules, &config).unwrap_err();
        assert!(err.contains("Weighted"), "error: {}", err);
    }

    #[test]
    fn fixed_pool_strategy_uses_yaml_values() {
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule(
                "users",
                vec![Relationship {
                    pk: "id".to_string(),
                    references: vec!["region.code".to_string()],
                    pool_strategy: PoolStrategy::Fixed {
                        values: vec!["CN".to_string(), "US".to_string()],
                    },
                    null_label: "null".to_string(),
                }],
            )],
        };

        let config = config(&["users"], 10);
        let result = generate(&models, &rules, &config).unwrap();
        let users = result.tables.get("users").unwrap();
        assert_eq!(users.len(), 10);
        for row in users {
            let v = row.last().unwrap();
            if let Some(s) = v.as_str() {
                assert!(s == "CN" || s == "US", "unexpected FK value: {}", s);
            }
        }
    }

    #[test]
    fn categorical_column_generates_declared_values() {
        let mut columns = HashMap::new();
        columns.insert(
            "status".to_string(),
            ColumnModel {
                logical_type: LogicalType::Categorical,
                rounding: None,
                datetime_epoch: None,
                marginal: Marginal::Categorical(CategoricalParams {
                    values: vec!["open".to_string(), "closed".to_string()],
                    weights: vec![0.5, 0.5],
                }),
            },
        );
        let mut models = HashMap::new();
        models.insert(
            "tasks".to_string(),
            TableModel {
                version: 1,
                table: "tasks".to_string(),
                dialect: "mysql".to_string(),
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                },
                pk: vec![],
                columns,
                copula: CopulaInfo {
                    column_order: vec!["status".to_string()],
                    correlation: vec![vec![1.0]],
                },
            },
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("tasks", vec![])],
        };

        let config = config(&["tasks"], 20);
        let result = generate(&models, &rules, &config).unwrap();
        for row in result.tables.get("tasks").unwrap() {
            let v = row.first().unwrap();
            let s = v.as_str().expect("categorical value must be a string");
            assert!(s == "open" || s == "closed", "unexpected: {}", s);
        }
    }

    #[test]
    fn zipf_strategy_generates_all_rows() {
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![TableRule {
                name: "users".to_string(),
                relationships: vec![],
                strategy: TableStrategy::Zipf,
            }],
        };

        let config = config(&["users"], 25);
        let result = generate(&models, &rules, &config).unwrap();
        assert_eq!(result.tables.get("users").unwrap().len(), 25);
    }

    #[test]
    fn unique_fk_never_repeats() {
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );
        models.insert(
            "orders".to_string(),
            numerical_model("orders", "total", 10.0, 2.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![
                single_rule(
                    "orders",
                    vec![Relationship {
                        pk: "total".to_string(),
                        references: vec!["users.id".to_string()],
                        pool_strategy: PoolStrategy::Projection { unique: true },
                        null_label: "null".to_string(),
                    }],
                ),
                single_rule("users", vec![]),
            ],
        };

        let config = config(&["users", "orders"], 5);
        let result = generate(&models, &rules, &config).unwrap();

        let users = result.tables.get("users").unwrap();
        let orders = result.tables.get("orders").unwrap();
        assert_eq!(users.len(), 5);
        assert_eq!(orders.len(), 5);

        let parent_ids: std::collections::HashSet<u64> = users
            .iter()
            .filter_map(|r| r.first())
            .filter_map(|v| v.as_f64())
            .map(|f| f.to_bits())
            .collect();

        let mut fk_seen = std::collections::HashSet::new();
        for row in orders {
            let f = row[0].as_f64().expect("FK must stay numeric");
            assert!(parent_ids.contains(&f.to_bits()));
            assert!(fk_seen.insert(f.to_bits()), "unique FK repeated: {}", f);
        }
    }

    #[test]
    fn unique_fk_errors_when_child_exceeds_parent_pool() {
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );
        models.insert(
            "orders".to_string(),
            numerical_model("orders", "total", 10.0, 2.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![
                single_rule(
                    "orders",
                    vec![Relationship {
                        pk: "total".to_string(),
                        references: vec!["users.id".to_string()],
                        pool_strategy: PoolStrategy::Projection { unique: true },
                        null_label: "null".to_string(),
                    }],
                ),
                single_rule("users", vec![]),
            ],
        };

        let mut config = config(&["orders"], 5);
        config.rows_per_table.insert("users".to_string(), 3);

        let err = generate(&models, &rules, &config).unwrap_err();
        assert!(err.contains("unique FK"), "error: {}", err);
        assert!(
            err.contains("'total'"),
            "error should name the column: {}",
            err
        );
        assert!(err.contains("3 parent rows"), "error: {}", err);
    }

    #[test]
    fn unique_fk_with_zipf_is_rejected() {
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );
        models.insert(
            "orders".to_string(),
            numerical_model("orders", "total", 5.0, 1.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![
                TableRule {
                    name: "orders".to_string(),
                    relationships: vec![Relationship {
                        pk: "total".to_string(),
                        references: vec!["users.id".to_string()],
                        pool_strategy: PoolStrategy::Projection { unique: true },
                        null_label: "null".to_string(),
                    }],
                    strategy: TableStrategy::Zipf,
                },
                single_rule("users", vec![]),
            ],
        };

        let config = config(&["users", "orders"], 5);
        let err = generate(&models, &rules, &config).unwrap_err();
        assert!(err.contains("zipf"), "error: {}", err);
        assert!(err.contains("unique"), "error: {}", err);
    }

    #[test]
    fn same_seed_yields_different_streams_per_table() {
        let mut models = HashMap::new();
        models.insert("alpha".to_string(), numerical_model("alpha", "v", 0.0, 1.0));
        models.insert("beta".to_string(), numerical_model("beta", "v", 0.0, 1.0));

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("alpha", vec![]), single_rule("beta", vec![])],
        };

        let config = config(&["alpha", "beta"], 8);
        let result = generate(&models, &rules, &config).unwrap();

        let a = &result.tables.get("alpha").unwrap()[0][0];
        let b = &result.tables.get("beta").unwrap()[0][0];
        assert_ne!(
            a, b,
            "tables must not share one Gaussian stream under the same --seed"
        );
    }

    #[test]
    fn integer_column_generates_whole_numbers() {
        let mut columns = HashMap::new();
        columns.insert(
            "id".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: Some(0),
                datetime_epoch: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 100.0,
                    scale: 15.0,
                }),
            },
        );
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            TableModel {
                version: 1,
                table: "users".to_string(),
                dialect: "mysql".to_string(),
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                },
                pk: vec![],
                columns,
                copula: CopulaInfo {
                    column_order: vec!["id".to_string()],
                    correlation: vec![vec![1.0]],
                },
            },
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("users", vec![])],
        };

        let config = config(&["users"], 30);
        let result = generate(&models, &rules, &config).unwrap();
        for row in result.tables.get("users").unwrap() {
            let v = row.first().unwrap();
            let f = v
                .as_i64()
                .expect("integer column must emit integral values");
            assert_eq!(f as f64, f as f64);
        }
    }

    #[test]
    fn fk_copies_preserve_parent_integer_type() {
        let mut columns = HashMap::new();
        columns.insert(
            "id".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: Some(0),
                datetime_epoch: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 100.0,
                    scale: 15.0,
                }),
            },
        );
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            TableModel {
                version: 1,
                table: "users".to_string(),
                dialect: "mysql".to_string(),
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                },
                pk: vec![],
                columns,
                copula: CopulaInfo {
                    column_order: vec!["id".to_string()],
                    correlation: vec![vec![1.0]],
                },
            },
        );
        models.insert(
            "orders".to_string(),
            numerical_model("orders", "total", 5.0, 1.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![
                single_rule(
                    "orders",
                    vec![Relationship {
                        pk: "total".to_string(),
                        references: vec!["users.id".to_string()],
                        pool_strategy: PoolStrategy::Projection { unique: true },
                        null_label: "null".to_string(),
                    }],
                ),
                single_rule("users", vec![]),
            ],
        };

        let config = config(&["users", "orders"], 4);
        let result = generate(&models, &rules, &config).unwrap();
        for row in result.tables.get("orders").unwrap() {
            assert!(
                row[0].is_i64() || row[0].is_u64(),
                "FK copy must stay integer, got {}",
                row[0]
            );
        }
    }

    #[test]
    fn generated_data_carries_column_names() {
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("users", vec![])],
        };

        let config = config(&["users"], 3);
        let result = generate(&models, &rules, &config).unwrap();
        assert_eq!(result.columns.get("users"), Some(&vec!["id".to_string()]));
    }
}
