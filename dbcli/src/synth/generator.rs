use crate::synth::copula::GaussianCopula;
use crate::synth::fk_pool::FkPool;
use crate::synth::model::TableModel;
use crate::synth::rules::SynthRules;
use rand::SeedableRng;
use serde_json::Value;

pub struct GeneratorConfig {
    pub rows_per_table: std::collections::HashMap<String, usize>,
    pub seed: Option<u64>,
}

pub struct GeneratedData {
    pub tables: std::collections::HashMap<String, Vec<Vec<Value>>>,
}

pub fn generate(
    models: &std::collections::HashMap<String, TableModel>,
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
        &{
            let mut edges = Vec::new();
            for t in &rules.tables {
                for r in &t.relationships {
                    for ref_str in &r.references {
                        let parts: Vec<&str> = ref_str.split('.').collect();
                        if parts.len() == 2 {
                            edges.push((t.name.clone(), parts[0].to_string()));
                        }
                    }
                }
            }
            edges
        },
    )
    .map_err(|e| format!("cycle detected: {}", e))?;

    let mut tables: std::collections::HashMap<String, Vec<Vec<Value>>> =
        std::collections::HashMap::new();

    let mut fk_pools: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

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

        let column_order = &model.copula.column_order;
        let copula = GaussianCopula::new(model.copula.correlation.clone());
        let uniform_samples = copula.sample(row_count, config.seed);

        let mut rows = Vec::with_capacity(row_count);

        for t in 0..row_count {
            let mut row = Vec::with_capacity(column_order.len());

            for (col_idx, col_name) in column_order.iter().enumerate() {
                let column_model = model.columns.get(col_name);

                let rel = rule.relationships.iter().find(|r| &r.pk == col_name);

                if let Some(rel) = rel {
                    let ref_str = rel
                        .references
                        .first()
                        .ok_or_else(|| format!("relationship '{}' has no references", col_name))?;
                    let parts: Vec<&str> = ref_str.split('.').collect();
                    let ref_table = parts[0].to_string();

                    if let Some(pool) = fk_pools.get(&ref_table) {
                        let values = pool.clone();
                        let fanout = crate::synth::fk_pool::FanoutStrategy::Fixed(1);
                        let fk_pool = FkPool::new_projection(values);
                        let sampled = fk_pool.sample(&fanout, &mut rng);
                        let val = sampled.first().cloned().unwrap_or_default();
                        row.push(Value::String(val));
                    } else if let Some(ref_model) = models.get(&ref_table) {
                        if let Some(ref_col) = ref_model.columns.get(parts[1]) {
                            let uniform_val = uniform_samples
                                .get(col_idx)
                                .and_then(|col| col.get(t))
                                .copied()
                                .unwrap_or(0.5);
                            let generated = ref_col.marginal.inverse_cdf(uniform_val);
                            row.push(Value::from(generated));
                        } else {
                            row.push(Value::Null);
                        }
                    } else {
                        row.push(Value::Null);
                    }
                } else if let Some(col_model) = column_model {
                    let uniform_val = uniform_samples
                        .get(col_idx)
                        .and_then(|col| col.get(t))
                        .copied()
                        .unwrap_or(0.5);
                    let generated = col_model.marginal.inverse_cdf(uniform_val);
                    row.push(Value::from(generated));
                } else {
                    row.push(Value::Null);
                }
            }

            rows.push(row);
        }

        if let Some(_pk_col) = model.pk.first() {
            let pk_values: Vec<String> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    row.first()
                        .map(|v| match v {
                            Value::Number(n) => n.to_string(),
                            Value::String(s) => s.clone(),
                            _ => i.to_string(),
                        })
                        .unwrap_or_else(|| i.to_string())
                })
                .collect();
            fk_pools.insert(table_name.clone(), pk_values);
        }

        tables.insert(table_name.clone(), rows);
    }

    Ok(GeneratedData { tables })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::marginal::{Marginal, NormalParams};
    use std::collections::HashMap;

    fn make_test_model(table: &str) -> TableModel {
        let mut columns = HashMap::new();
        columns.insert(
            "id".to_string(),
            crate::synth::model::ColumnModel {
                logical_type: crate::synth::model::LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 0.0,
                    scale: 1.0,
                }),
            },
        );

        TableModel {
            version: 1,
            table: table.to_string(),
            dialect: "mysql".to_string(),
            provenance: crate::synth::model::Provenance {
                source: "test".to_string(),
                converter_version: None,
                sdv_version: None,
            },
            pk: vec!["id".to_string()],
            columns,
            copula: crate::synth::model::CopulaInfo {
                column_order: vec!["id".to_string()],
                correlation: vec![vec![1.0]],
            },
        }
    }

    #[test]
    fn generator_produces_correct_row_count() {
        let mut models = HashMap::new();
        models.insert("users".to_string(), make_test_model("users"));

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![crate::synth::rules::TableRule {
                name: "users".to_string(),
                relationships: vec![],
                strategy: crate::synth::rules::TableStrategy::default(),
            }],
        };

        let mut rows_per_table = HashMap::new();
        rows_per_table.insert("users".to_string(), 50);

        let config = GeneratorConfig {
            rows_per_table,
            seed: Some(42),
        };

        let result = generate(&models, &rules, &config).unwrap();
        assert_eq!(result.tables.get("users").unwrap().len(), 50);
    }
}
