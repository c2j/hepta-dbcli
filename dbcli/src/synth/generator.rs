use crate::synth::copula::GaussianCopula;
use crate::synth::fk_pool::{FkPool, SelectionStrategy};
use crate::synth::model::TableModel;
use crate::synth::rules::{PoolStrategy, SynthRules, TableStrategy};
use rand::Rng;
use rand::SeedableRng;
use serde_json::Value;
use std::collections::HashMap;

pub struct GeneratorConfig {
    pub rows_per_table: HashMap<String, usize>,
    pub seed: Option<u64>,
    pub enforce_min_max_values: bool,
}

impl Default for GeneratorConfig {
    fn default() -> Self {
        Self {
            rows_per_table: HashMap::new(),
            seed: None,
            enforce_min_max_values: true,
        }
    }
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

    // Columns referenced by any relationship ("parent.col") must come out
    // unique per parent table: real referenced keys are PK/unique, and a
    // FK-enforced load of duplicated keys is impossible.
    let referenced_targets: std::collections::HashSet<String> = rules
        .tables
        .iter()
        .flat_map(|t| t.relationships.iter())
        .flat_map(|r| r.references.iter())
        .cloned()
        .collect();

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
            .or(rule.rows)
            .unwrap_or(100);

        let strategy = match rule.strategy {
            TableStrategy::Uniform => SelectionStrategy::Uniform,
            TableStrategy::Zipf => SelectionStrategy::Zipf,
            TableStrategy::Weighted => SelectionStrategy::Weighted,
        };

        let mut rel_pools = build_rel_pools(table_name, rule, &column_pools, models, strategy)?;

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

                row.push(gen_column_value(
                    model.columns.get(col_name),
                    uniform_val,
                    config.enforce_min_max_values,
                ));
            }

            rows.push(row);
        }

        // Referenced columns get rejection-redraw until every value is
        // distinct; a duplicated parent key cannot be FK-loaded downstream.
        for (col_idx, col_name) in column_order.iter().enumerate() {
            if !referenced_targets.contains(&format!("{}.{}", table_name, col_name)) {
                continue;
            }
            let column_model = model.columns.get(col_name);
            // 该列同时是本表的 FK 列时，只能从父池重抽——从自身边际重抽
            // 会产生脱离父表值域的值，破坏引用完整性。
            let fk_pool_column = rel_pools
                .iter()
                .find(|r| &r.column == col_name)
                .map(|r| r.column.clone());
            if fk_pool_column.is_none() {
                if let Some(crate::synth::marginal::Marginal::Categorical(p)) =
                    column_model.map(|c| &c.marginal)
                {
                    if p.values.len() < row_count {
                        return Err(format!(
                            "referenced column '{}.{}' has only {} categorical level(s) \
                             but {} rows are requested; unique values are impossible — \
                             reduce --rows or drop the table from the rules",
                            table_name,
                            col_name,
                            p.values.len(),
                            row_count
                        ));
                    }
                }
            } else {
                let pool = rel_pools
                    .iter()
                    .find(|r| &r.column == col_name)
                    .expect("fk_pool_column implies a rel pool");
                if pool.pool.distinct_len() < row_count {
                    return Err(format!(
                        "referenced FK column '{}.{}' draws from a pool of {} distinct \
                         value(s) but {} rows are requested; unique values are impossible",
                        table_name,
                        col_name,
                        pool.pool.distinct_len(),
                        row_count
                    ));
                }
            }
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for row in &mut rows {
                let current = &mut row[col_idx];
                if seen.insert(current.to_string()) {
                    continue;
                }
                let mut attempts = 0usize;
                loop {
                    attempts += 1;
                    let candidate = if fk_pool_column.is_some() {
                        rel_pools
                            .iter_mut()
                            .find(|r| &r.column == col_name)
                            .and_then(|r| r.pool.sample_one(r.strategy, &mut rng))
                            .unwrap_or_else(|| current.clone())
                    } else {
                        gen_column_value(
                            column_model,
                            rng.gen::<f64>(),
                            config.enforce_min_max_values,
                        )
                    };
                    if seen.insert(candidate.to_string()) {
                        *current = candidate;
                        break;
                    }
                    if attempts >= 10_000 {
                        return Err(format!(
                            "referenced column '{}.{}' exhausted its value space after \
                             {attempts} redraws (degenerate marginal?); duplicated parent \
                             keys cannot satisfy an FK-enforced load",
                            table_name, col_name
                        ));
                    }
                }
            }
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

fn gen_column_value(
    column_model: Option<&crate::synth::model::ColumnModel>,
    uniform_val: f64,
    enforce_min_max_values: bool,
) -> Value {
    // Clip uniform to (ε, 1-ε) to avoid ppf at extremes
    const EPS: f64 = 1e-12;
    let uniform_val = uniform_val.clamp(EPS, 1.0 - EPS);

    // A column whose training sample held only NULLs is kept in the model so
    // the generated table keeps the column, but there is nothing to sample:
    // emit NULL rather than a constant fabricated by a degenerate marginal.
    if let Some(column_model) = column_model {
        if column_model.null_rate.is_some_and(|rate| rate >= 1.0) {
            return Value::Null;
        }
    }

    match column_model.map(|c| &c.marginal) {
        Some(crate::synth::marginal::Marginal::Categorical(p)) => {
            let idx = p.sample_index(uniform_val);
            let value = p.values.get(idx).cloned().unwrap_or_default();
            if column_model
                .map(|column| {
                    matches!(
                        column.logical_type,
                        crate::synth::model::LogicalType::Numerical
                    )
                })
                .unwrap_or(false)
            {
                numeric_value_or_string(value)
            } else {
                Value::String(value)
            }
        }
        Some(marginal) => {
            let mut generated = marginal.inverse_cdf(uniform_val);
            // Clip to min/max if enabled
            if enforce_min_max_values {
                if let Some(col_model) = column_model {
                    if let Some(min) = col_model.min {
                        generated = generated.max(min);
                    }
                    if let Some(max) = col_model.max {
                        generated = generated.min(max);
                    }
                }
            }
            if column_model.and_then(|c| c.rounding) == Some(0) {
                Value::from(generated.round() as i64)
            } else {
                Value::from(generated)
            }
        }
        None => Value::Null,
    }
}

fn numeric_value_or_string(value: String) -> Value {
    if let Ok(integer) = value.parse::<i64>() {
        return Value::from(integer);
    }
    value
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or(Value::String(value))
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

fn parent_categorical<'a>(
    models: &'a HashMap<String, TableModel>,
    ref_str: &str,
) -> Option<&'a crate::synth::marginal::CategoricalParams> {
    let (table, col) = ref_str.split_once('.')?;
    match models.get(table)?.columns.get(col).map(|c| &c.marginal) {
        Some(crate::synth::marginal::Marginal::Categorical(p)) => Some(p),
        _ => None,
    }
}

fn build_rel_pools(
    table_name: &str,
    rule: &crate::synth::rules::TableRule,
    column_pools: &HashMap<String, Vec<Value>>,
    models: &HashMap<String, TableModel>,
    strategy: SelectionStrategy,
) -> Result<Vec<RelPool>, String> {
    let mut rel_pools = Vec::new();
    for rel in &rule.relationships {
        let ref_str = rel
            .references
            .first()
            .ok_or_else(|| format!("relationship '{}' has no references", rel.pk))?;

        let (pool, unique) = match &rel.pool_strategy {
            PoolStrategy::Fixed { values } => {
                let raw: Vec<Value> = values.iter().map(|v| Value::String(v.clone())).collect();
                let pool = if strategy == SelectionStrategy::Weighted {
                    FkPool::from_observed_weights(raw, None)
                } else {
                    FkPool::new(raw)
                };
                (pool, false)
            }
            PoolStrategy::Projection { unique } | PoolStrategy::Generated { unique } => {
                let values = column_pools.get(ref_str).ok_or_else(|| {
                    format!(
                        "table '{}' references '{}' but that table.column was not generated \
                         first; add a rule and model for it",
                        table_name, ref_str
                    )
                })?;
                let pool = if strategy == SelectionStrategy::Weighted {
                    FkPool::from_observed_weights(
                        values.clone(),
                        parent_categorical(models, ref_str),
                    )
                } else {
                    FkPool::new(values.clone())
                };
                (pool, *unique)
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
                decimal_scale: None,
                datetime_format: None,
                marginal: Marginal::Normal(NormalParams { loc, scale }),
                ..Default::default()
            },
        );
        TableModel {
            version: 1,
            table: table.to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "test".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
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
            columns: HashMap::new(),
            rows: None,
            relationships,
            strategy: TableStrategy::default(),
        }
    }

    fn config(tables: &[&str], rows: usize) -> GeneratorConfig {
        GeneratorConfig {
            rows_per_table: tables.iter().map(|t| (t.to_string(), rows)).collect(),
            seed: Some(42),
            enforce_min_max_values: true,
        }
    }

    fn int_key_model(table: &str, column: &str, loc: f64) -> TableModel {
        let mut columns = HashMap::new();
        columns.insert(
            column.to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                marginal: Marginal::Normal(NormalParams { loc, scale: 1.0 }),
                ..Default::default()
            },
        );
        TableModel {
            version: 1,
            table: table.to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "test".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
            },
            pk: vec![column.to_string()],
            columns,
            copula: CopulaInfo {
                column_order: vec![column.to_string()],
                correlation: vec![vec![1.0]],
            },
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
    fn should_generate_per_table_row_counts_from_rules() {
        let models = HashMap::from([
            (
                "parent".to_string(),
                numerical_model("parent", "id", 0.0, 1.0),
            ),
            (
                "child".to_string(),
                numerical_model("child", "id", 0.0, 1.0),
            ),
        ]);
        let mut parent = single_rule("parent", vec![]);
        parent.rows = Some(2);
        let mut child = single_rule("child", vec![]);
        child.rows = Some(5);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![parent, child],
        };

        let result = generate(&models, &rules, &GeneratorConfig::default()).unwrap();

        assert_eq!(result.tables.get("parent").unwrap().len(), 2);
        assert_eq!(result.tables.get("child").unwrap().len(), 5);
    }

    #[test]
    fn should_generate_null_for_all_null_column() {
        // An all-null training sample is kept in the model (so the generated
        // table keeps the column) but must emit NULL, not a degenerate constant.
        let mut model = numerical_model("t", "empty_num", 0.0, 0.0);
        model.columns.get_mut("empty_num").unwrap().null_rate = Some(1.0);
        let models = HashMap::from([("t".to_string(), model)]);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("t", vec![])],
        };

        let result = generate(&models, &rules, &config(&["t"], 6)).unwrap();
        let rows = result.tables.get("t").unwrap();
        assert_eq!(rows.len(), 6);
        assert!(
            rows.iter().all(|row| row[0].is_null()),
            "all-null column should generate NULL, got {:?}",
            rows.iter().map(|r| &r[0]).collect::<Vec<_>>()
        );
    }

    #[test]
    fn should_not_null_out_a_zero_variance_column_with_values() {
        // A normal column whose sampled values are all identical has scale 0
        // but observed min/max: it must still emit that value, not NULL.
        let mut model = numerical_model("t", "constant", 5.0, 0.0);
        model.columns.get_mut("constant").unwrap().null_rate = Some(0.0);
        model.columns.get_mut("constant").unwrap().min = Some(5.0);
        model.columns.get_mut("constant").unwrap().max = Some(5.0);
        let models = HashMap::from([("t".to_string(), model)]);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("t", vec![])],
        };

        let result = generate(&models, &rules, &config(&["t"], 4)).unwrap();
        let rows = result.tables.get("t").unwrap();
        assert!(rows.iter().all(|row| row[0] == 5.0));
    }

    #[test]
    fn should_sample_referenced_columns_without_duplicates() {
        // Integer-keyed Uniform(0,100) over 40 rows: naive sampling collides
        // with near-certainty, and the value space is wide enough that
        // rejection-redraw can recover full uniqueness.
        fn int_key_model(table: &str, column: &str) -> TableModel {
            let mut columns = HashMap::new();
            columns.insert(
                column.to_string(),
                ColumnModel {
                    logical_type: LogicalType::Numerical,
                    rounding: Some(0),
                    datetime_epoch: None,
                    decimal_scale: None,
                    datetime_format: None,
                    marginal: Marginal::Uniform(crate::synth::marginal::UniformParams {
                        low: 0.0,
                        high: 100.0,
                    }),
                    ..Default::default()
                },
            );
            TableModel {
                version: 1,
                table: table.to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
                },
                pk: vec![column.to_string()],
                columns,
                copula: CopulaInfo {
                    column_order: vec![column.to_string()],
                    correlation: vec![vec![1.0]],
                },
            }
        }

        let mut models = HashMap::new();
        models.insert("parent".to_string(), int_key_model("parent", "id"));
        models.insert("child".to_string(), int_key_model("child", "id"));

        let mut parent_rule = single_rule("parent", vec![]);
        parent_rule.rows = Some(40);
        let child_rule = single_rule(
            "child",
            vec![Relationship {
                pk: "id".to_string(),
                references: vec!["parent.id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                null_label: "null".to_string(),
            }],
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![parent_rule, child_rule],
        };

        let result = generate(&models, &rules, &GeneratorConfig::default()).unwrap();

        let parent = result.tables.get("parent").unwrap();
        assert_eq!(parent.len(), 40);
        let distinct: std::collections::HashSet<String> =
            parent.iter().map(|r| r[0].to_string()).collect();
        assert_eq!(
            distinct.len(),
            40,
            "referenced parent keys must be unique, got {} distinct of {}",
            distinct.len(),
            parent.len()
        );
    }

    #[test]
    fn should_keep_fk_values_in_parent_pool_when_referenced_column_is_fk() {
        // 场景：b.a_id 引用 a.id，而 c 又引用 b.a_id。b.a_id 是被引用列，
        // 去重重抽必须仍从父池取样；若从 b.a_id 自身边际重抽，值会脱离
        // a.id 的值域，破坏引用完整性。
        let mut models = HashMap::new();
        models.insert("a".to_string(), int_key_model("a", "id", 1_000_000.0));
        models.insert("b".to_string(), int_key_model("b", "a_id", 0.0));
        models.insert("c".to_string(), int_key_model("c", "b_a_id", 0.0));

        let mut a_rule = single_rule("a", vec![]);
        a_rule.rows = Some(50);
        let mut b_rule = single_rule(
            "b",
            vec![Relationship {
                pk: "a_id".to_string(),
                references: vec!["a.id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                null_label: "null".to_string(),
            }],
        );
        b_rule.rows = Some(30);
        let mut c_rule = single_rule(
            "c",
            vec![Relationship {
                pk: "b_a_id".to_string(),
                references: vec!["b.a_id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                null_label: "null".to_string(),
            }],
        );
        c_rule.rows = Some(10);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![a_rule, b_rule, c_rule],
        };

        let result = generate(&models, &rules, &GeneratorConfig::default()).unwrap();

        let a_ids: std::collections::HashSet<String> = result.tables["a"]
            .iter()
            .map(|r| r[0].to_string())
            .collect();
        let b_a_id_idx = 0;
        for row in &result.tables["b"] {
            let v = &row[b_a_id_idx];
            assert!(
                a_ids.contains(&v.to_string()),
                "b.a_id value {} must stay within a.id values after dedup redraw",
                v
            );
        }
    }

    #[test]
    fn should_error_when_referenced_categorical_levels_cannot_cover_rows() {
        // 离散被引用列档位数 < 请求行数时唯一性在数学上不可达：
        // 必须 fail-fast 报错，而不是每行空转 1 万次重抽后静默留重。
        let mut columns = HashMap::new();
        columns.insert(
            "k".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: Some(0),
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                marginal: Marginal::Categorical(CategoricalParams {
                    values: vec!["1".to_string(), "2".to_string(), "3".to_string()],
                    weights: vec![1.0 / 3.0; 3],
                }),
                ..Default::default()
            },
        );
        let parent = TableModel {
            version: 1,
            table: "parent".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "test".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
            },
            pk: vec!["k".to_string()],
            columns,
            copula: CopulaInfo {
                column_order: vec!["k".to_string()],
                correlation: vec![vec![1.0]],
            },
        };
        let mut models = HashMap::new();
        models.insert("parent".to_string(), parent);
        models.insert("child".to_string(), int_key_model("child", "k", 0.0));

        let mut parent_rule = single_rule("parent", vec![]);
        parent_rule.rows = Some(10);
        let child_rule = single_rule(
            "child",
            vec![Relationship {
                pk: "k".to_string(),
                references: vec!["parent.k".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                null_label: "null".to_string(),
            }],
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![parent_rule, child_rule],
        };

        let err = generate(&models, &rules, &GeneratorConfig::default())
            .expect_err("impossible uniqueness must error");
        assert!(
            err.contains("parent.k"),
            "error must name the column: {err}"
        );
    }

    #[test]
    fn should_error_when_referenced_fk_pool_cannot_cover_rows() {
        // 被引用列同时是 FK 列（链表场景）：其可去重的值来自父池。
        // 父池 distinct=2 而本表 5 行时唯一性不可达，必须 fail-fast。
        fn two_level_model(table: &str, column: &str) -> TableModel {
            let mut columns = HashMap::new();
            columns.insert(
                column.to_string(),
                ColumnModel {
                    logical_type: LogicalType::Numerical,
                    rounding: Some(0),
                    datetime_epoch: None,
                    decimal_scale: None,
                    datetime_format: None,
                    marginal: Marginal::Categorical(CategoricalParams {
                        values: vec!["1".to_string(), "2".to_string()],
                        weights: vec![0.5, 0.5],
                    }),
                    ..Default::default()
                },
            );
            TableModel {
                version: 1,
                table: table.to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
                },
                pk: vec![column.to_string()],
                columns,
                copula: CopulaInfo {
                    column_order: vec![column.to_string()],
                    correlation: vec![vec![1.0]],
                },
            }
        }

        let mut models = HashMap::new();
        models.insert("a".to_string(), two_level_model("a", "id"));
        models.insert("b".to_string(), two_level_model("b", "a_id"));
        models.insert("c".to_string(), int_key_model("c", "b_a_id", 0.0));

        let mut a_rule = single_rule("a", vec![]);
        a_rule.rows = Some(2);
        let mut b_rule = single_rule(
            "b",
            vec![Relationship {
                pk: "a_id".to_string(),
                references: vec!["a.id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                null_label: "null".to_string(),
            }],
        );
        b_rule.rows = Some(5);
        let c_rule = single_rule(
            "c",
            vec![Relationship {
                pk: "b_a_id".to_string(),
                references: vec!["b.a_id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                null_label: "null".to_string(),
            }],
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![a_rule, b_rule, c_rule],
        };

        let err = generate(&models, &rules, &GeneratorConfig::default())
            .expect_err("pool smaller than row count must error");
        assert!(err.contains("b.a_id"), "error must name the column: {err}");
    }

    #[test]
    fn should_error_when_referenced_column_value_space_is_exhausted() {
        // 值域塌缩（σ=0.01 取整后只剩 {0}）的被引用列：10k 次重抽也造不出
        // 第二个值，warn+留重复等于静默产出无法 FK 装载的数据，必须报错。
        let mut columns = HashMap::new();
        columns.insert(
            "id".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: Some(0),
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 0.0,
                    scale: 0.01,
                }),
                ..Default::default()
            },
        );
        let parent = TableModel {
            version: 1,
            table: "parent".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "test".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
            },
            pk: vec!["id".to_string()],
            columns,
            copula: CopulaInfo {
                column_order: vec!["id".to_string()],
                correlation: vec![vec![1.0]],
            },
        };
        let mut models = HashMap::new();
        models.insert("parent".to_string(), parent);
        models.insert("child".to_string(), int_key_model("child", "id", 0.0));

        let mut parent_rule = single_rule("parent", vec![]);
        parent_rule.rows = Some(200);
        let child_rule = single_rule(
            "child",
            vec![Relationship {
                pk: "id".to_string(),
                references: vec!["parent.id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                null_label: "null".to_string(),
            }],
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![parent_rule, child_rule],
        };

        let err = generate(&models, &rules, &GeneratorConfig::default())
            .expect_err("exhausted value space must error");
        assert!(
            err.contains("parent.id"),
            "error must name the column: {err}"
        );
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
                decimal_scale: None,
                datetime_format: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 10.0,
                    scale: 2.0,
                }),
                ..Default::default()
            },
        );
        order_columns.insert(
            "user_id".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 0.0,
                    scale: 1.0,
                }),
                ..Default::default()
            },
        );
        models.insert(
            "orders".to_string(),
            TableModel {
                version: 1,
                table: "orders".to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
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
    fn should_generate_three_table_chain_with_referential_integrity() {
        fn chain_model(table: &str, columns: &[&str]) -> TableModel {
            let modeled_columns = columns
                .iter()
                .map(|column| {
                    (
                        (*column).to_string(),
                        ColumnModel {
                            logical_type: LogicalType::Numerical,
                            rounding: Some(0),
                            datetime_epoch: None,
                            decimal_scale: None,
                            datetime_format: None,
                            marginal: Marginal::Normal(NormalParams {
                                loc: 100.0,
                                scale: 15.0,
                            }),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            let dimension = columns.len();
            TableModel {
                version: 1,
                table: table.to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
                },
                pk: vec!["id".to_string()],
                columns: modeled_columns,
                copula: CopulaInfo {
                    column_order: columns.iter().map(|column| (*column).to_string()).collect(),
                    correlation: (0..dimension)
                        .map(|i| {
                            (0..dimension)
                                .map(|j| if i == j { 1.0 } else { 0.0 })
                                .collect()
                        })
                        .collect(),
                },
            }
        }

        let models = HashMap::from([
            ("a".to_string(), chain_model("a", &["id"])),
            ("b".to_string(), chain_model("b", &["id", "parent"])),
            ("c".to_string(), chain_model("c", &["id", "parent"])),
        ]);
        let relationship = |parent: &str| Relationship {
            pk: "parent".to_string(),
            references: vec![format!("{}.id", parent)],
            pool_strategy: PoolStrategy::Projection { unique: false },
            null_label: "null".to_string(),
        };
        let mut a = single_rule("a", vec![]);
        a.rows = Some(3);
        let mut b = single_rule("b", vec![relationship("a")]);
        b.rows = Some(10);
        let mut c = single_rule("c", vec![relationship("b")]);
        c.rows = Some(20);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![c, b, a],
        };

        let result = generate(&models, &rules, &GeneratorConfig::default()).unwrap();
        let a_rows = result.tables.get("a").unwrap();
        let b_rows = result.tables.get("b").unwrap();
        let c_rows = result.tables.get("c").unwrap();
        let a_ids: std::collections::HashSet<i64> =
            a_rows.iter().filter_map(|row| row[0].as_i64()).collect();
        let b_ids: std::collections::HashSet<i64> =
            b_rows.iter().filter_map(|row| row[0].as_i64()).collect();

        assert_eq!(a_rows.len(), 3);
        assert_eq!(b_rows.len(), 10);
        assert_eq!(c_rows.len(), 20);
        assert!(b_rows.iter().all(|row| row[1]
            .as_i64()
            .map(|id| a_ids.contains(&id))
            .unwrap_or(false)));
        assert!(c_rows.iter().all(|row| row[1]
            .as_i64()
            .map(|id| b_ids.contains(&id))
            .unwrap_or(false)));
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
    fn should_weight_fk_references_by_parent_frequency() {
        fn cat_model(table: &str, column: &str, values: &[&str], weights: &[f64]) -> TableModel {
            let mut columns = HashMap::new();
            columns.insert(
                column.to_string(),
                ColumnModel {
                    logical_type: LogicalType::Categorical,
                    rounding: None,
                    datetime_epoch: None,
                    decimal_scale: None,
                    datetime_format: None,
                    marginal: Marginal::Categorical(CategoricalParams {
                        values: values.iter().map(|s| s.to_string()).collect(),
                        weights: weights.to_vec(),
                    }),
                    ..Default::default()
                },
            );
            TableModel {
                version: 1,
                table: table.to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
                },
                pk: vec![column.to_string()],
                columns,
                copula: CopulaInfo {
                    column_order: vec![column.to_string()],
                    correlation: vec![vec![1.0]],
                },
            }
        }

        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            cat_model("users", "id", &["a", "b", "c"], &[0.6, 0.3, 0.1]),
        );
        models.insert(
            "orders".to_string(),
            cat_model("orders", "user_id", &["a", "b", "c"], &[0.6, 0.3, 0.1]),
        );

        let mut users = single_rule("users", vec![]);
        users.rows = Some(3);
        let orders = TableRule {
            name: "orders".to_string(),
            columns: HashMap::new(),
            rows: Some(10_000),
            relationships: vec![Relationship {
                pk: "user_id".to_string(),
                references: vec!["users.id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                null_label: "null".to_string(),
            }],
            strategy: TableStrategy::Weighted,
        };
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![users, orders],
        };

        let cfg = GeneratorConfig {
            seed: Some(42),
            ..GeneratorConfig::default()
        };
        let result = generate(&models, &rules, &cfg).expect("Weighted must not error");
        let child = result.tables.get("orders").unwrap();
        assert_eq!(child.len(), 10_000);

        let mut counts = HashMap::new();
        for row in child {
            let key = row[0].as_str().expect("fk string").to_string();
            *counts.entry(key).or_insert(0) += 1;
        }
        let share = |k: &str| *counts.get(k).unwrap_or(&0) as f64 / 10_000.0;
        assert!(
            (share("a") - 0.6).abs() < 0.05,
            "a share {} not within 5pp of 0.6",
            share("a")
        );
        assert!(
            (share("b") - 0.3).abs() < 0.05,
            "b share {} not within 5pp of 0.3",
            share("b")
        );
        assert!(
            (share("c") - 0.1).abs() < 0.05,
            "c share {} not within 5pp of 0.1",
            share("c")
        );
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
                decimal_scale: None,
                datetime_format: None,
                marginal: Marginal::Categorical(CategoricalParams {
                    values: vec!["open".to_string(), "closed".to_string()],
                    weights: vec![0.5, 0.5],
                }),
                ..Default::default()
            },
        );
        let mut models = HashMap::new();
        models.insert(
            "tasks".to_string(),
            TableModel {
                version: 1,
                table: "tasks".to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
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
    fn should_generate_on_grid_values_for_discrete_numeric() {
        let levels: Vec<String> = (1..=19).map(|level| level.to_string()).collect();
        let mut models = HashMap::new();
        models.insert(
            "payments".to_string(),
            TableModel {
                version: 1,
                table: "payments".to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
                },
                pk: vec![],
                columns: HashMap::from([(
                    "amount".to_string(),
                    ColumnModel {
                        logical_type: LogicalType::Numerical,
                        rounding: Some(0),
                        datetime_epoch: None,
                        decimal_scale: None,
                        datetime_format: None,
                        min: Some(1.0),
                        max: Some(19.0),
                        null_rate: None,
                        marginal: Marginal::Categorical(CategoricalParams {
                            values: levels.clone(),
                            weights: vec![1.0 / 19.0; 19],
                        }),
                    },
                )]),
                copula: CopulaInfo {
                    column_order: vec!["amount".to_string()],
                    correlation: vec![vec![1.0]],
                },
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("payments", vec![])],
        };

        let result = generate(&models, &rules, &config(&["payments"], 1000)).unwrap();
        let rows = result.tables.get("payments").unwrap();

        assert_eq!(rows.len(), 1000);
        assert!(rows.iter().all(|row| row[0].is_number()));
        assert!(rows.iter().all(|row| {
            row[0]
                .as_i64()
                .map(|value| (1..=19).contains(&value))
                .unwrap_or(false)
        }));
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
                columns: HashMap::new(),
                rows: None,
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
                    columns: HashMap::new(),
                    rows: None,
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
                decimal_scale: None,
                datetime_format: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 100.0,
                    scale: 15.0,
                }),
                ..Default::default()
            },
        );
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            TableModel {
                version: 1,
                table: "users".to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
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
                decimal_scale: None,
                datetime_format: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 100.0,
                    scale: 15.0,
                }),
                ..Default::default()
            },
        );
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            TableModel {
                version: 1,
                table: "users".to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
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
    fn clipping_enforces_min_max_bounds() {
        let mut models = HashMap::new();
        models.insert(
            "metrics".to_string(),
            TableModel {
                version: 1,
                table: "metrics".to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
                },
                pk: vec![],
                columns: HashMap::from([(
                    "value".to_string(),
                    ColumnModel {
                        logical_type: LogicalType::Numerical,
                        rounding: None,
                        datetime_epoch: None,
                        decimal_scale: None,
                        datetime_format: None,
                        min: Some(0.0),
                        max: Some(120.0),
                        null_rate: None,
                        marginal: Marginal::Normal(NormalParams {
                            loc: 100.0,
                            scale: 50.0,
                        }),
                    },
                )]),
                copula: CopulaInfo {
                    column_order: vec!["value".to_string()],
                    correlation: vec![vec![1.0]],
                },
            },
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("metrics", vec![])],
        };

        let mut config = config(&["metrics"], 200);
        config.enforce_min_max_values = true;
        let result = generate(&models, &rules, &config).unwrap();
        for row in result.tables.get("metrics").unwrap() {
            let v = row[0].as_f64().unwrap();
            assert!(
                (0.0..=120.0).contains(&v),
                "clipped value {} out of [0, 120]",
                v
            );
        }
    }

    #[test]
    fn clipping_disabled_allows_out_of_range_values() {
        let mut models = HashMap::new();
        models.insert(
            "metrics".to_string(),
            TableModel {
                version: 1,
                table: "metrics".to_string(),
                dialect: "mysql".to_string(),
                schema: None,
                provenance: Provenance {
                    source: "test".to_string(),
                    converter_version: None,
                    sdv_version: None,
                    truncated: false,
                },
                pk: vec![],
                columns: HashMap::from([(
                    "value".to_string(),
                    ColumnModel {
                        logical_type: LogicalType::Numerical,
                        rounding: None,
                        datetime_epoch: None,
                        decimal_scale: None,
                        datetime_format: None,
                        min: Some(0.0),
                        max: Some(101.0),
                        null_rate: None,
                        marginal: Marginal::Normal(NormalParams {
                            loc: 100.0,
                            scale: 50.0,
                        }),
                    },
                )]),
                copula: CopulaInfo {
                    column_order: vec!["value".to_string()],
                    correlation: vec![vec![1.0]],
                },
            },
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("metrics", vec![])],
        };

        let mut config = config(&["metrics"], 200);
        config.enforce_min_max_values = false;
        let result = generate(&models, &rules, &config).unwrap();
        let any_over = result
            .tables
            .get("metrics")
            .unwrap()
            .iter()
            .any(|row| row[0].as_f64().unwrap() > 101.0);
        assert!(any_over, "with clipping disabled the tail must exceed max");
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
