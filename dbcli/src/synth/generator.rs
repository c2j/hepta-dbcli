use crate::synth::copula::GaussianCopula;
use crate::synth::fk_pool::{FkPool, SelectionStrategy};
use crate::synth::model::TableModel;
use crate::synth::rules::{ColumnMode, PoolStrategy, SynthRules, TableStrategy, ValuePool};
use rand::Rng;
use rand::SeedableRng;
use serde_json::Value;
use std::cmp::Ordering;
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
    pub schemas: HashMap<String, String>,
    /// Branch coverage after repair, one entry per `branches[]` rule.
    pub branches: Vec<BranchOutcome>,
    /// Sampled-vs-declared conformance of every `values` pool.
    pub value_pools: Vec<ValuePoolOutcome>,
}

/// Absolute deviation allowed between a `values` pool's declared weights and
/// the shares actually generated (#76-C).
pub const VALUE_POOL_TOLERANCE: f64 = 0.05;

/// How well one weighted `values` pool reproduced its declared distribution.
#[derive(Debug, Clone)]
pub struct ValuePoolOutcome {
    pub table: String,
    pub column: String,
    /// Declared `(value, share)` pairs, normalised to sum to 1.
    pub declared: Vec<(String, f64)>,
    /// Shares actually present in the generated rows.
    pub actual: Vec<(String, f64)>,
    /// Largest absolute difference between the two.
    pub max_deviation: f64,
}

impl ValuePoolOutcome {
    /// `true` when the generated shares match the declared weights closely
    /// enough to stay quiet.
    pub fn is_within_tolerance(&self) -> bool {
        self.max_deviation <= VALUE_POOL_TOLERANCE
    }
}

/// Coverage state of one branch after the repair loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageStatus {
    /// Within `target_ratio ± tolerance`.
    Pass,
    /// Repaired as far as the rules allowed, but still off target.
    Warn,
    /// No row could be rewritten (misconfigured predicate or repair).
    Fail,
}

/// Result of measuring and repairing one `branches[]` rule.
#[derive(Debug, Clone)]
pub struct BranchOutcome {
    pub id: String,
    pub target_ratio: f64,
    pub actual_ratio: f64,
    pub status: CoverageStatus,
    /// Repair rounds executed (0 when the branch was already covered).
    pub rounds: usize,
    /// Rows rewritten across all rounds.
    pub flips: usize,
    /// Rows whose predicate could not be evaluated.
    pub failed_evaluations: usize,
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
    let mut branch_outcomes: Vec<BranchOutcome> = Vec::new();
    let mut value_pool_outcomes: Vec<ValuePoolOutcome> = Vec::new();
    let mut table_columns: HashMap<String, Vec<String>> = HashMap::new();
    let mut table_schemas: HashMap<String, String> = HashMap::new();
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

        // `mode: copula_conditional` replaces the plain draw: the pinned
        // columns take a fixed/range quantile and every other column is drawn
        // from the conditional distribution, so the learned correlation with
        // the pinned column survives.
        let pins = conditional_pins(table_name, rule, model, column_order, row_count, config)?;
        let uniform_samples = if pins.is_empty() {
            copula.sample(row_count, table_seed(config.seed, table_name))
        } else {
            copula
                .sample_with_fixed_uniform_rows(
                    row_count,
                    &pins,
                    table_seed(config.seed, table_name),
                )
                .map_err(|e| format!("table '{}': {}", table_name, e))?
        };
        let pinned_columns: std::collections::HashSet<usize> =
            pins.iter().map(|(index, _)| *index).collect();

        let null_rates: Vec<f64> = column_order
            .iter()
            .map(|col_name| {
                let is_referenced =
                    referenced_targets.contains(&format!("{}.{}", table_name, col_name));
                effective_null_rate(rule, model, table_name, col_name, is_referenced)
            })
            .collect();
        // Independent streams, built only when the effective rate is > 0 so
        // all-zero models keep the same copula/FK draws as before this change.
        let mut null_rngs: Vec<Option<rand::rngs::StdRng>> = column_order
            .iter()
            .zip(null_rates.iter())
            .map(|(col_name, rate)| {
                if *rate > 0.0 {
                    Some(column_null_rng(config.seed, table_name, col_name))
                } else {
                    None
                }
            })
            .collect();

        let mut rows = Vec::with_capacity(row_count);

        for t in 0..row_count {
            let mut row = Vec::with_capacity(column_order.len());

            for (col_idx, col_name) in column_order.iter().enumerate() {
                if let Some(null_rng) = null_rngs[col_idx].as_mut() {
                    let u: f64 = null_rng.gen();
                    if u < null_rates[col_idx] {
                        row.push(Value::Null);
                        continue;
                    }
                }

                if let Some(rel) = rel_pools.iter_mut().find(|r| &r.column == col_name) {
                    let value = if rel.unique {
                        rel.pool
                            .sample_unique(rel.strategy, &mut rng)
                            .ok_or_else(|| {
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
                    // A conditional pin is an explicit user range: clipping it
                    // to the trained min/max would undo it.
                    config.enforce_min_max_values && !pinned_columns.contains(&col_idx),
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

        // Phase 4 (plan §1): column-level fixed / values / fixed_range
        // overrides run after copula/marginal, FK and NULL injection.
        apply_column_value_overrides(
            &mut rows,
            table_name,
            rule,
            model,
            column_order,
            config,
            &referenced_targets,
        )?;

        // Phase 5 (plan §1): derived columns run last, over the values every
        // earlier phase produced.
        apply_derive_rules(
            &mut rows,
            table_name,
            rule,
            model,
            column_order,
            &referenced_targets,
        )?;

        value_pool_outcomes.extend(check_value_pools(
            &rows,
            table_name,
            rule,
            model,
            column_order,
        ));

        // Phase 6 (plan §1): branch coverage repair, restricted to the
        // flippable columns (never FK, referenced, derived or pinned).
        branch_outcomes.extend(apply_branch_repair(
            &mut rows,
            table_name,
            rule,
            model,
            column_order,
            &referenced_targets,
        )?);

        for (col_idx, col_name) in column_order.iter().enumerate() {
            let values: Vec<Value> = rows
                .iter()
                .filter_map(|r| r.get(col_idx).cloned())
                .collect();
            column_pools.insert(format!("{}.{}", table_name, col_name), values);
        }

        table_columns.insert(table_name.clone(), column_order.clone());
        if let Some(schema) = model.schema.as_ref().filter(|s| !s.is_empty()) {
            table_schemas.insert(table_name.clone(), schema.clone());
        }
        if model.dialect != "test" {
            dialect = model.dialect.clone();
        }
        tables.insert(table_name.clone(), rows);
    }

    Ok(GeneratedData {
        tables,
        columns: table_columns,
        dialect,
        schemas: table_schemas,
        branches: branch_outcomes,
        value_pools: value_pool_outcomes,
    })
}

fn round_to_scale(value: f64, scale: u8, strategy: rust_decimal::RoundingStrategy) -> f64 {
    use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
    use rust_decimal::Decimal;

    let Some(dec) = Decimal::from_f64(value) else {
        return value;
    };
    let dp = u32::from(scale).min(Decimal::MAX_SCALE);
    let rounded = dec.round_dp_with_strategy(dp, strategy);
    rounded.to_f64().unwrap_or(value)
}

/// Quantize to `scale` decimals, half away from zero.
fn quantize(value: f64, scale: u8) -> f64 {
    round_to_scale(
        value,
        scale,
        rust_decimal::RoundingStrategy::MidpointAwayFromZero,
    )
}

/// The tightest on-grid interval covering `[min, max]`: smallest grid point
/// `>= min` and largest grid point `<= max`. Clamping a quantized value to
/// these keeps it both on the output grid and inside the trained range —
/// clamping to raw `min`/`max` after quantization would let half-up rounding
/// push a value past the range (e.g. max 1.225 at scale 2 rounding up to 1.23).
fn grid_bounds(min: f64, max: f64, scale: u8) -> (f64, f64) {
    (
        round_to_scale(
            min,
            scale,
            rust_decimal::RoundingStrategy::ToPositiveInfinity,
        ),
        round_to_scale(
            max,
            scale,
            rust_decimal::RoundingStrategy::ToNegativeInfinity,
        ),
    )
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
            if let Some(col) = column_model {
                if matches!(col.logical_type, crate::synth::model::LogicalType::Datetime) {
                    if let Some(fmt) = col.datetime_format.as_deref() {
                        return crate::synth::datetime::format_epoch(generated, fmt)
                            .map(Value::String)
                            .unwrap_or(Value::Null);
                    }
                }
            }
            let is_integer_column = column_model.and_then(|c| c.rounding) == Some(0);
            let decimal_scale = column_model
                .filter(|c| matches!(c.logical_type, crate::synth::model::LogicalType::Numerical))
                .and_then(|c| c.decimal_scale);
            if !is_integer_column && decimal_scale.is_none() {
                return Value::from(generated);
            }

            let scale = if is_integer_column {
                0
            } else {
                decimal_scale.unwrap_or(0)
            };
            let mut snapped = if is_integer_column {
                generated.round()
            } else {
                quantize(generated, scale)
            };
            if enforce_min_max_values {
                if let Some((min, max)) = column_model.and_then(|c| Some((c.min?, c.max?))) {
                    let (lo, hi) = grid_bounds(min, max, scale);
                    // Degenerate ranges (no grid point inside [min, max]) keep
                    // the pre-quantization clip result.
                    if lo <= hi {
                        snapped = snapped.clamp(lo, hi);
                    }
                }
            }
            if is_integer_column {
                Value::from(snapped as i64)
            } else {
                Value::from(snapped)
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

fn effective_null_rate(
    rule: &crate::synth::rules::TableRule,
    model: &TableModel,
    table_name: &str,
    col_name: &str,
    is_referenced: bool,
) -> f64 {
    // Phase 4 (plan §1) overwrites every row, so a trained NULL rate on a
    // fixed/values/fixed_range column must not inject NULLs first.
    if let Some(column_rule) = rule
        .columns
        .get(col_name)
        .filter(|column_rule| column_rule.has_column_override())
    {
        // `validate` (V2) already rejects this, so the warning only fires for
        // callers that generate without validating. Stage 4 overwrites every
        // row, so both rates are ignored either way.
        let rule_rate = column_rule.null_rate.unwrap_or(0.0);
        let model_rate = model
            .columns
            .get(col_name)
            .and_then(|c| c.null_rate)
            .unwrap_or(0.0);
        if model_rate > 0.0 {
            eprintln!(
                "warning: column '{}.{}' has a fixed/values/fixed_range rule; ignoring trained null_rate {}",
                table_name, col_name, model_rate
            );
        }
        if rule_rate > 0.0 {
            eprintln!(
                "warning: column '{}.{}' has a fixed/values/fixed_range rule; ignoring rule null_rate {}",
                table_name, col_name, rule_rate
            );
        }
        return 0.0;
    }

    let rate = rule
        .columns
        .get(col_name)
        .and_then(|c| c.null_rate)
        .or_else(|| model.columns.get(col_name).and_then(|c| c.null_rate))
        .unwrap_or(0.0);
    if is_referenced && rate > 0.0 {
        eprintln!(
            "warning: referenced column '{}.{}' cannot be NULL; ignoring null_rate {}",
            table_name, col_name, rate
        );
        0.0
    } else {
        rate
    }
}

fn column_null_rng(base: Option<u64>, table: &str, column: &str) -> rand::rngs::StdRng {
    match table_seed(base, &format!("{}:{}:null", table, column)) {
        Some(s) => rand::rngs::StdRng::seed_from_u64(s),
        None => rand::rngs::StdRng::from_entropy(),
    }
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

/// Independent per-column stream for phase-4 rules, seeded djb2-style so the
/// same `--seed` reproduces the same draw and other columns are untouched.
fn column_value_rng(
    base: Option<u64>,
    table: &str,
    column: &str,
    purpose: &str,
) -> rand::rngs::StdRng {
    match table_seed(base, &format!("{}:{}:{}", table, column, purpose)) {
        Some(s) => rand::rngs::StdRng::seed_from_u64(s),
        None => rand::rngs::StdRng::from_entropy(),
    }
}

/// Compare each `values` pool's declared weights with the shares actually
/// generated (#76-C). Purely observational: the caller only warns.
fn check_value_pools(
    rows: &[Vec<Value>],
    table_name: &str,
    rule: &crate::synth::rules::TableRule,
    model: &TableModel,
    column_order: &[String],
) -> Vec<ValuePoolOutcome> {
    let mut outcomes = Vec::new();
    if rows.is_empty() {
        return outcomes;
    }

    for (col_idx, col_name) in column_order.iter().enumerate() {
        let Some(pool) = rule
            .columns
            .get(col_name)
            .and_then(|column_rule| column_rule.values.as_ref())
        else {
            continue;
        };

        let column_model = model.columns.get(col_name);
        let declared_raw: Vec<(String, f64)> = match pool {
            ValuePool::Weighted(weights) => weights
                .iter()
                .map(|(value, w)| (value.clone(), *w))
                .collect(),
            ValuePool::Uniform(values) => {
                let share = 1.0 / values.len() as f64;
                values.iter().map(|value| (value.clone(), share)).collect()
            }
        };
        let total: f64 = declared_raw.iter().map(|(_, weight)| weight).sum();
        if total <= 0.0 {
            continue;
        }

        let row_count = rows.len() as f64;
        let mut declared = Vec::with_capacity(declared_raw.len());
        let mut actual = Vec::with_capacity(declared_raw.len());
        let mut max_deviation = 0.0f64;
        for (value, weight) in &declared_raw {
            let share = weight / total;
            let typed = typed_literal(value, column_model);
            let hits = rows
                .iter()
                .filter(|row| row.get(col_idx) == Some(&typed))
                .count() as f64;
            let observed = hits / row_count;
            max_deviation = max_deviation.max((observed - share).abs());
            declared.push((value.clone(), share));
            actual.push((value.clone(), observed));
        }

        outcomes.push(ValuePoolOutcome {
            table: table_name.to_string(),
            column: col_name.clone(),
            declared,
            actual,
            max_deviation,
        });
    }

    outcomes
}

/// A round's rewrites, kept so a round that moves no predicate can be undone:
/// the branch position, its flip count before the round, and the pre-write
/// snapshot of every row it touched.
type RepairBackup = (usize, usize, Vec<(usize, Vec<Value>)>);

/// Measure every `branches[]` predicate and rewrite rows until each target is
/// met, in at most `rules::MAX_REPAIR_ROUNDS` rounds (issue #70).
///
/// Only `repair.set` literals are written; a branch whose predicate does not
/// depend on those columns cannot make progress and is reported as `Fail`
/// instead of looping or silently passing.
fn apply_branch_repair(
    rows: &mut [Vec<Value>],
    table_name: &str,
    rule: &crate::synth::rules::TableRule,
    model: &TableModel,
    column_order: &[String],
    referenced_targets: &std::collections::HashSet<String>,
) -> Result<Vec<BranchOutcome>, String> {
    if rule.branches.is_empty() {
        return Ok(Vec::new());
    }

    let index_of: HashMap<&str, usize> = column_order
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();

    let mut prepared = Vec::with_capacity(rule.branches.len());
    for branch in &rule.branches {
        let predicate = crate::synth::expr::Expr::parse(&branch.predicate).map_err(|e| {
            format!(
                "table '{}' branch '{}': predicate '{}' rejected: {}",
                table_name, branch.id, branch.predicate, e
            )
        })?;
        for name in predicate.referenced_columns() {
            if !index_of.contains_key(name.as_str()) {
                return Err(format!(
                    "table '{}' branch '{}': predicate references unknown column '{}'",
                    table_name, branch.id, name
                ));
            }
        }

        let derived: std::collections::HashSet<&str> = rule
            .derive
            .iter()
            .map(|entry| entry.column.as_str())
            .collect();
        let mut assignments = Vec::with_capacity(branch.repair.set.len());
        for (column, literal) in &branch.repair.set {
            let Some(&index) = index_of.get(column.as_str()) else {
                return Err(format!(
                    "table '{}' branch '{}': repair.set targets unknown column '{}'",
                    table_name, branch.id, column
                ));
            };
            if rule.relationships.iter().any(|rel| &rel.pk == column) {
                return Err(format!(
                    "table '{}' branch '{}': repair.set cannot write FK column '{}' (referential integrity)",
                    table_name, branch.id, column
                ));
            }
            if referenced_targets.contains(&format!("{}.{}", table_name, column)) {
                return Err(format!(
                    "table '{}' branch '{}': repair.set cannot write parent key '{}.{}' referenced by another table (uniqueness is enforced before the repair phase)",
                    table_name, branch.id, table_name, column
                ));
            }
            if derived.contains(column.as_str()) {
                return Err(format!(
                    "table '{}' branch '{}': repair.set cannot write derived column '{}'",
                    table_name, branch.id, column
                ));
            }
            if rule
                .columns
                .get(column)
                .is_some_and(|column_rule| column_rule.has_column_override())
            {
                return Err(format!(
                    "table '{}' branch '{}': repair.set cannot write pinned column '{}'",
                    table_name, branch.id, column
                ));
            }
            assignments.push((
                index,
                typed_literal(literal, model.columns.get(column.as_str())),
            ));
        }

        prepared.push((branch, predicate, assignments));
    }

    // The candidate filter below has to know what a `set` does to *derived*
    // columns: `repair.set` cannot write a derive target, but it can write a
    // derive source, and `derive` only runs at the end of the round. Judging a
    // simulated row without re-deriving it both misses destruction that only
    // shows up after `derive` and lets a sibling overwrite cells a branch just
    // wrote for a derived predicate.
    let derive_plan = DerivePlan::build(table_name, rule, model, column_order)?;

    // A predicate that cannot be evaluated on a single row is a type or
    // column mistake (e.g. `bs == \'1\'` against a numeric column), not an
    // uncovered branch. Fail loudly instead of reporting 0% coverage.
    for (branch, predicate, _) in &prepared {
        if rows.is_empty() {
            break;
        }
        let (matching, failures) = measure_predicate(rows, predicate, &index_of);
        if failures == rows.len() {
            let sample = rows.first().map(|row| {
                predicate.eval_bool(&|name: &str| {
                    index_of
                        .get(name)
                        .and_then(|index| row.get(*index))
                        .cloned()
                })
            });
            let detail = match sample {
                Some(Err(error)) => error.to_string(),
                _ => "unknown evaluation error".to_string(),
            };
            return Err(format!(
                "table '{}' branch '{}': predicate '{}' could not be evaluated on any of the {} generated row(s): {}",
                table_name,
                branch.id,
                branch.predicate,
                rows.len(),
                detail
            ));
        }
        let _ = matching;
    }

    let mut outcomes: Vec<BranchOutcome> = prepared
        .iter()
        .map(|(branch, _, _)| BranchOutcome {
            id: branch.id.clone(),
            target_ratio: branch.target_ratio,
            actual_ratio: 0.0,
            status: CoverageStatus::Warn,
            rounds: 0,
            flips: 0,
            failed_evaluations: 0,
        })
        .collect();

    let mut rounds_used = 0usize;
    for _round in 0..crate::synth::rules::MAX_REPAIR_ROUNDS {
        let mut touched = 0usize;
        // Branches measured at or under their target still need their matches:
        // `set` can only add matches, so a branch that has not reached its
        // target cannot claw back a row another branch converts away. A branch
        // that is *over* target is the opposite case — converting its surplus
        // is how a partition reaches its targets — so its matches stay
        // available (see `destroys_protected_coverage` below, which is what
        // actually protects the rows; being under target alone is not enough
        // when the sibling writes a column this predicate does not read).
        let protected: Vec<bool> = prepared
            .iter()
            .map(|(branch, predicate, _)| {
                let (matching, _) = measure_predicate(rows, predicate, &index_of);
                let actual = matching as f64 / rows.len().max(1) as f64;
                let tolerance = branch
                    .tolerance
                    .unwrap_or(crate::synth::rules::DEFAULT_BRANCH_TOLERANCE);
                actual <= branch.target_ratio + tolerance
            })
            .collect();
        // A rewrite is destructive only when it turns a row that matches a
        // protected sibling into one that no longer does. Writing a column the
        // sibling's predicate does not read destroys nothing, so two branches
        // on disjoint columns may stack on the same rows in the same round.
        let destroys_protected_coverage =
            |position: usize, row: &[Value], assignments: &[(usize, Value)]| {
                prepared.iter().enumerate().any(|(other, (_, sibling, _))| {
                    if other == position || !protected[other] {
                        return false;
                    }
                    // Both sides are judged after re-running `derive`: within a
                    // round the derived columns of rows a sibling just rewrote
                    // are stale, and a write to a derive source does not change
                    // them until the round-end pass. Simulating the write
                    // without the derive both overlooks destruction and lets a
                    // sibling clobber rows a branch just wrote.
                    let mut before = row.to_vec();
                    if derive_plan.apply_to_row(&mut before).is_err() {
                        // A row this branch cannot derive would abort the
                        // round-end pass as well; let that pass report it
                        // rather than turning a configuration error into a
                        // silently skipped candidate.
                        return false;
                    }
                    if !predicate_matches(sibling, &before, &index_of) {
                        return false;
                    }
                    let mut after = row.to_vec();
                    for (index, value) in assignments {
                        after[*index] = value.clone();
                    }
                    if derive_plan.apply_to_row(&mut after).is_err() {
                        return false;
                    }
                    !predicate_matches(sibling, &after, &index_of)
                })
            };
        // `(branch position, flips before the round, [(row index, snapshot)])`.
        let mut backups: Vec<RepairBackup> = Vec::new();

        for (position, (branch, predicate, assignments)) in prepared.iter().enumerate() {
            let (matching, failures) = measure_predicate(rows, predicate, &index_of);
            let row_count = rows.len().max(1);
            let actual = matching as f64 / row_count as f64;
            let tolerance = branch
                .tolerance
                .unwrap_or(crate::synth::rules::DEFAULT_BRANCH_TOLERANCE);

            outcomes[position].actual_ratio = actual;
            outcomes[position].failed_evaluations = failures;

            if (actual - branch.target_ratio).abs() <= tolerance {
                continue;
            }

            let wanted = (branch.target_ratio * row_count as f64).round() as usize;
            if wanted <= matching {
                // `set` can only make new rows match; overshoot is reported,
                // never silently "fixed" by writing values we cannot derive.
                continue;
            }

            let candidates: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| !predicate_matches(predicate, row, &index_of))
                .filter(|(_, row)| !destroys_protected_coverage(position, row, assignments))
                .map(|(index, _)| index)
                .collect();
            let needed = (wanted - matching).min(candidates.len());
            if needed == 0 {
                continue;
            }

            // Spread the rewrites over the candidate list instead of
            // clustering them on the first rows. Snapshot every row we touch so
            // a round that turns out to move nothing can be undone instead of
            // leaving `set` values behind.
            let flips_before = outcomes[position].flips;
            let mut backup: Vec<(usize, Vec<Value>)> = Vec::with_capacity(needed);
            for pick in 0..needed {
                let offset = pick * candidates.len() / needed;
                let candidate = candidates[offset.min(candidates.len() - 1)];
                backup.push((candidate, rows[candidate].clone()));
                for (index, value) in assignments {
                    rows[candidate][*index] = value.clone();
                }
                outcomes[position].flips += 1;
                touched += 1;
            }
            backups.push((position, flips_before, backup));
        }

        if touched == 0 {
            break;
        }
        rounds_used += 1;

        // `derive` is idempotent and may read a column the repair just wrote,
        // so it must run *before* progress is measured: a predicate that reads
        // a derived column cannot be judged until its inputs are refreshed.
        apply_derive_rules(
            rows,
            table_name,
            rule,
            model,
            column_order,
            referenced_targets,
        )?;

        // A repair that cannot move any predicate (typically `set` writing
        // columns the predicate does not read) must stop instead of burning
        // the remaining rounds, and must leave no trace behind.
        let progressed = prepared
            .iter()
            .enumerate()
            .any(|(position, (_, predicate, _))| {
                let (matching, _) = measure_predicate(rows, predicate, &index_of);
                let actual = matching as f64 / rows.len().max(1) as f64;
                (actual - outcomes[position].actual_ratio).abs() > f64::EPSILON
            });
        if !progressed {
            for (position, flips_before, backup) in backups.drain(..) {
                for (index, snapshot) in backup {
                    rows[index] = snapshot;
                }
                outcomes[position].flips = flips_before;
            }
            break;
        }
        backups.clear();
    }

    for (position, (branch, predicate, _)) in prepared.iter().enumerate() {
        let (matching, failures) = measure_predicate(rows, predicate, &index_of);
        let row_count = rows.len().max(1);
        let actual = matching as f64 / row_count as f64;
        let tolerance = branch
            .tolerance
            .unwrap_or(crate::synth::rules::DEFAULT_BRANCH_TOLERANCE);
        outcomes[position].actual_ratio = actual;
        outcomes[position].failed_evaluations = failures;
        outcomes[position].rounds = rounds_used;
        outcomes[position].status = if (actual - branch.target_ratio).abs() <= tolerance {
            CoverageStatus::Pass
        } else if outcomes[position].flips > 0 {
            CoverageStatus::Warn
        } else {
            CoverageStatus::Fail
        };
    }

    Ok(outcomes)
}

fn measure_predicate(
    rows: &[Vec<Value>],
    predicate: &crate::synth::expr::Expr,
    index_of: &HashMap<&str, usize>,
) -> (usize, usize) {
    let mut matching = 0usize;
    let mut failures = 0usize;
    for row in rows {
        match predicate.eval_bool(&|name: &str| {
            index_of
                .get(name)
                .and_then(|index| row.get(*index))
                .cloned()
        }) {
            Ok(true) => matching += 1,
            Ok(false) => {}
            Err(_) => failures += 1,
        }
    }
    (matching, failures)
}

fn predicate_matches(
    predicate: &crate::synth::expr::Expr,
    row: &[Value],
    index_of: &HashMap<&str, usize>,
) -> bool {
    predicate
        .eval_bool(&|name: &str| {
            index_of
                .get(name)
                .and_then(|index| row.get(*index))
                .cloned()
        })
        .unwrap_or(false)
}

/// Overwrite derived columns with `expr` evaluated over the finished row
/// (issue #70). Derive rules are applied in dependency order, so a rule may
/// reference a column another rule produced.
fn apply_derive_rules(
    rows: &mut [Vec<Value>],
    table_name: &str,
    rule: &crate::synth::rules::TableRule,
    model: &TableModel,
    column_order: &[String],
    referenced_targets: &std::collections::HashSet<String>,
) -> Result<(), String> {
    if rule.derive.is_empty() {
        return Ok(());
    }

    for derive in &rule.derive {
        if referenced_targets.contains(&format!("{}.{}", table_name, derive.column)) {
            return Err(format!(
                "table '{}': derive cannot target parent key '{}.{}' referenced by another table (uniqueness is enforced before the derive phase, so derived values could repeat)",
                table_name, table_name, derive.column
            ));
        }
    }

    DerivePlan::build(table_name, rule, model, column_order)?.apply_to_rows(rows)
}

/// Per-row `derive` evaluation in dependency order. The round-end pass applies
/// it to every row; the branch repair loop applies it to a *single* simulated
/// row, because `repair.set` may write a column a derived predicate reads.
struct DerivePlan {
    table_name: String,
    index_of: HashMap<String, usize>,
    steps: Vec<DeriveStep>,
}

struct DeriveStep {
    column: String,
    index: usize,
    expr: crate::synth::expr::Expr,
    is_integer: bool,
    scale: Option<u8>,
}

impl DerivePlan {
    /// Parse and order the `derive` rules (Kahn over the derive graph: a target
    /// waits for the derive columns it references; cycles were already
    /// rejected by `rules.validate`).
    fn build(
        table_name: &str,
        rule: &crate::synth::rules::TableRule,
        model: &TableModel,
        column_order: &[String],
    ) -> Result<Self, String> {
        let index_of: HashMap<&str, usize> = column_order
            .iter()
            .enumerate()
            .map(|(index, name)| (name.as_str(), index))
            .collect();

        let mut parsed: HashMap<&str, crate::synth::expr::Expr> = HashMap::new();
        for derive in &rule.derive {
            let expr = crate::synth::expr::Expr::parse(&derive.expr).map_err(|e| {
                format!(
                    "table '{}' derive '{}': expression '{}' rejected: {}",
                    table_name, derive.column, derive.expr, e
                )
            })?;
            parsed.insert(derive.column.as_str(), expr);
        }

        // Repeatedly take whatever is ready; `n` is tiny and this keeps the
        // dependency rule readable.
        let mut ordered: Vec<&str> = Vec::with_capacity(rule.derive.len());
        let mut done: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut progress = true;
        while progress {
            progress = false;
            for derive in &rule.derive {
                let name = derive.column.as_str();
                if done.contains(name) {
                    continue;
                }
                let Some(expr) = parsed.get(name) else {
                    continue;
                };
                let ready = expr.referenced_columns().iter().all(|referenced| {
                    !parsed.contains_key(referenced.as_str()) || done.contains(referenced.as_str())
                });
                if ready {
                    ordered.push(name);
                    done.insert(name);
                    progress = true;
                }
            }
        }

        if ordered.len() != rule.derive.len() {
            let mut unresolved: Vec<&str> = rule
                .derive
                .iter()
                .map(|derive| derive.column.as_str())
                .filter(|name| !done.contains(name))
                .collect();
            unresolved.sort_unstable();
            return Err(format!(
                "table '{}': derive rules form a cycle involving {}",
                table_name,
                unresolved.join(", ")
            ));
        }

        // Referenced columns must exist in this table; `.` is rejected by the
        // expression grammar, so a name here is always a local column.
        for (target, expr) in &parsed {
            for name in expr.referenced_columns() {
                if !index_of.contains_key(name.as_str()) {
                    return Err(format!(
                        "table '{}' derive '{}': unknown column '{}'",
                        table_name, target, name
                    ));
                }
            }
            if !index_of.contains_key(target) {
                return Err(format!(
                    "table '{}' derive '{}': unknown target column",
                    table_name, target
                ));
            }
        }

        let mut steps = Vec::with_capacity(ordered.len());
        for target in ordered {
            let Some(&index) = index_of.get(target) else {
                return Err(format!(
                    "table '{}': derive target '{}' vanished",
                    table_name, target
                ));
            };
            let expr = parsed.get(target).cloned().ok_or_else(|| {
                format!("table '{}': derive '{}' was not parsed", table_name, target)
            })?;
            let column_model = model.columns.get(target);
            steps.push(DeriveStep {
                column: target.to_string(),
                index,
                expr,
                is_integer: column_model.and_then(|column| column.rounding) == Some(0),
                scale: column_model.and_then(|column| column.decimal_scale),
            });
        }

        Ok(DerivePlan {
            table_name: table_name.to_string(),
            index_of: index_of
                .iter()
                .map(|(name, index)| (name.to_string(), *index))
                .collect(),
            steps,
        })
    }

    fn apply_to_row(&self, row: &mut [Value]) -> Result<(), String> {
        use rust_decimal::prelude::ToPrimitive;

        for step in &self.steps {
            let evaluated = {
                let lookup = |name: &str| -> Option<Value> {
                    self.index_of
                        .get(name)
                        .and_then(|index| row.get(*index))
                        .cloned()
                };
                step.expr.eval_decimal(&lookup)
            };

            let value = match evaluated {
                Ok(value) => value,
                // SQL three-valued logic: a NULL input makes the expression
                // NULL, and stage 6 recomputes the target unconditionally, so
                // the target becomes NULL as well. Aborting here would make
                // `total = price * qty` unusable on any table whose inputs
                // carry a `null_rate`. Division by zero and type errors stay
                // fatal: those are configuration mistakes, not NULL input.
                Err(crate::synth::expr::ExprError::NullResult) => {
                    row[step.index] = Value::Null;
                    continue;
                }
                Err(error) => {
                    return Err(format!(
                        "table '{}' derive '{}': {}",
                        self.table_name, step.column, error
                    ));
                }
            };

            let number = if step.is_integer {
                let rounded = value.round();
                let as_i64 = rounded.to_i64().ok_or_else(|| {
                    format!(
                        "table '{}' derive '{}': result {} does not fit an integer column",
                        self.table_name, step.column, rounded
                    )
                })?;
                Value::Number(as_i64.into())
            } else {
                let as_f64 = value.to_f64().ok_or_else(|| {
                    format!(
                        "table '{}' derive '{}': result {} is out of range for a double",
                        self.table_name, step.column, value
                    )
                })?;
                let quantized = match step.scale {
                    Some(scale) => quantize(as_f64, scale),
                    None => as_f64,
                };
                serde_json::Number::from_f64(quantized)
                    .map(Value::Number)
                    .ok_or_else(|| {
                        format!(
                            "table '{}' derive '{}': result {} is not a finite number",
                            self.table_name, step.column, quantized
                        )
                    })?
            };
            row[step.index] = number;
        }

        Ok(())
    }

    fn apply_to_rows(&self, rows: &mut [Vec<Value>]) -> Result<(), String> {
        for row in rows.iter_mut() {
            self.apply_to_row(row)?;
        }
        Ok(())
    }
}

/// Resolve a rules literal into the numeric domain the marginal was fitted on:
/// a plain number for numerical columns, epoch seconds for datetime ones.
fn literal_in_marginal_domain(
    table_name: &str,
    col_name: &str,
    literal: &str,
    column_model: Option<&crate::synth::model::ColumnModel>,
) -> Result<f64, String> {
    let Some(column_model) = column_model else {
        return Err(format!(
            "table '{}' column '{}': no trained column model, cannot condition on '{}'",
            table_name, col_name, literal
        ));
    };

    if matches!(
        column_model.logical_type,
        crate::synth::model::LogicalType::Datetime
    ) {
        let format = column_model.datetime_format.as_deref();
        let value = Value::String(literal.to_string());
        return datetime_literal_epoch(&value, format).ok_or_else(|| {
            format!(
                "table '{}' column '{}': cannot read '{}' as a datetime{}",
                table_name,
                col_name,
                literal,
                format
                    .map(|f| format!(" with format '{}'", f))
                    .unwrap_or_default()
            )
        });
    }

    literal.parse::<f64>().map_err(|_| {
        format!(
            "table '{}' column '{}': '{}' is not a number",
            table_name, col_name, literal
        )
    })
}

/// A `fixed_range` endpoint, which may be a YAML number or a quoted string.
fn range_endpoint_in_marginal_domain(
    table_name: &str,
    col_name: &str,
    endpoint: &serde_json::Value,
    column_model: Option<&crate::synth::model::ColumnModel>,
) -> Result<f64, String> {
    match endpoint {
        serde_json::Value::Number(number) => number.as_f64().ok_or_else(|| {
            format!(
                "table '{}' column '{}': fixed_range endpoint {} is not a finite number",
                table_name, col_name, number
            )
        }),
        serde_json::Value::String(text) => {
            literal_in_marginal_domain(table_name, col_name, text, column_model)
        }
        other => Err(format!(
            "table '{}' column '{}': fixed_range endpoint {} must be a number or a string",
            table_name, col_name, other
        )),
    }
}

/// Uniforms to pin the copula draw to, one vector per conditional column.
///
/// `mode: copula_conditional` means "draw the other columns *given* this
/// column". `fixed` pins the same quantile for every row; `fixed_range` draws
/// a quantile uniformly inside `[F(low), F(high)]`, which turns the column's
/// marginal into its truncation to the range (issue #68).
fn conditional_pins(
    table_name: &str,
    rule: &crate::synth::rules::TableRule,
    model: &TableModel,
    column_order: &[String],
    row_count: usize,
    config: &GeneratorConfig,
) -> Result<Vec<(usize, Vec<f64>)>, String> {
    let mut pins = Vec::new();

    for (col_idx, col_name) in column_order.iter().enumerate() {
        let Some(column_rule) = rule.columns.get(col_name) else {
            continue;
        };
        if column_rule.mode != ColumnMode::CopulaConditional {
            continue;
        }

        let column_model = model.columns.get(col_name);
        let Some(marginal) = column_model.map(|column| &column.marginal) else {
            return Err(format!(
                "table '{}' column '{}': mode 'copula_conditional' needs a trained column model",
                table_name, col_name
            ));
        };
        if matches!(marginal, crate::synth::marginal::Marginal::Categorical(_)) {
            return Err(format!(
                "table '{}' column '{}': mode 'copula_conditional' needs an invertible numeric marginal; a categorical column has no CDF",
                table_name, col_name
            ));
        }

        let uniforms: Vec<f64> = if let Some(fixed) = &column_rule.fixed {
            let value = literal_in_marginal_domain(table_name, col_name, fixed, column_model)?;
            vec![clamp_unit(marginal.cdf(value)); row_count]
        } else if let Some(range) = &column_rule.fixed_range {
            let low =
                range_endpoint_in_marginal_domain(table_name, col_name, &range[0], column_model)?;
            let high =
                range_endpoint_in_marginal_domain(table_name, col_name, &range[1], column_model)?;
            let low_u = clamp_unit(marginal.cdf(low));
            let high_u = clamp_unit(marginal.cdf(high));
            if high_u <= low_u {
                return Err(format!(
                    "table '{}' column '{}': fixed_range covers no probability mass in the trained marginal; the range lies outside the observed values",
                    table_name, col_name
                ));
            }
            // Independent stream per column, so pinning does not disturb any
            // other column's draws.
            let mut rng = column_value_rng(config.seed, table_name, col_name, "range");
            (0..row_count)
                .map(|_| clamp_unit(low_u + rng.gen::<f64>() * (high_u - low_u)))
                .collect()
        } else {
            return Err(format!(
                "table '{}' column '{}': mode 'copula_conditional' needs a 'fixed' value or a 'fixed_range'",
                table_name, col_name
            ));
        };

        pins.push((col_idx, uniforms));
    }

    Ok(pins)
}

fn clamp_unit(value: f64) -> f64 {
    const EPS: f64 = 1e-12;
    value.clamp(EPS, 1.0 - EPS)
}

fn apply_column_value_overrides(
    rows: &mut [Vec<Value>],
    table_name: &str,
    rule: &crate::synth::rules::TableRule,
    model: &TableModel,
    column_order: &[String],
    config: &GeneratorConfig,
    referenced_targets: &std::collections::HashSet<String>,
) -> Result<(), String> {
    for (col_idx, col_name) in column_order.iter().enumerate() {
        let Some(column_rule) = rule.columns.get(col_name) else {
            continue;
        };
        if !column_rule.has_column_override() {
            continue;
        }

        // The frozen validation matrix (V3/V4) covers fixed/values; this
        // generator-side guard keeps fixed_range (and any caller that skips
        // validate) from silently corrupting referential integrity.
        let key = format!("{}.{}", table_name, col_name);
        let is_referenced = referenced_targets.contains(&key);
        let is_fk = rule.relationships.iter().any(|r| &r.pk == col_name);
        if is_referenced || is_fk {
            return Err(format!(
                "table '{}' column '{}': fixed/values/fixed_range cannot be applied to a column that participates in a relationship",
                table_name, col_name
            ));
        }

        if column_rule.mode == ColumnMode::CopulaConditional {
            // The value already comes from the conditional copula draw (the
            // phase-4 rejection path must not redraw it). `fixed` is rewritten
            // with the exact literal so typo-level float drift cannot leak
            // into a partition key; it consumes no randomness.
            if let Some(fixed) = &column_rule.fixed {
                let value = typed_literal(fixed, model.columns.get(col_name));
                for row in rows.iter_mut() {
                    row[col_idx] = value.clone();
                }
            }
            continue;
        }

        if let Some(fixed) = &column_rule.fixed {
            let value = typed_literal(fixed, model.columns.get(col_name));
            for row in rows.iter_mut() {
                row[col_idx] = value.clone();
            }
        } else if let Some(pool) = &column_rule.values {
            if value_pool_is_empty(pool) {
                return Err(format!(
                    "table '{}' column '{}': 'values' pool is empty",
                    table_name, col_name
                ));
            }
            let mut rng = column_value_rng(config.seed, table_name, col_name, "values");
            for row in rows.iter_mut() {
                let pick = sample_value_pool(pool, &mut rng);
                row[col_idx] = typed_literal(&pick, model.columns.get(col_name));
            }
        } else if let Some(range) = &column_rule.fixed_range {
            apply_fixed_range(
                rows,
                table_name,
                col_name,
                col_idx,
                model.columns.get(col_name),
                range,
                config,
            )?;
        }
    }
    Ok(())
}

fn typed_literal(literal: &str, column_model: Option<&crate::synth::model::ColumnModel>) -> Value {
    let numerical = column_model
        .map(|c| matches!(c.logical_type, crate::synth::model::LogicalType::Numerical))
        .unwrap_or(false);
    if numerical {
        numeric_value_or_string(literal.to_string())
    } else {
        Value::String(literal.to_string())
    }
}

fn value_pool_is_empty(pool: &ValuePool) -> bool {
    match pool {
        ValuePool::Weighted(weights) => weights.is_empty(),
        ValuePool::Uniform(values) => values.is_empty(),
    }
}

fn sample_value_pool(pool: &ValuePool, rng: &mut rand::rngs::StdRng) -> String {
    match pool {
        ValuePool::Uniform(values) => {
            let idx = ((rng.gen::<f64>() * values.len() as f64) as usize).min(values.len() - 1);
            values[idx].clone()
        }
        ValuePool::Weighted(weights) => {
            let total: f64 = weights.values().sum();
            let target = rng.gen::<f64>() * total;
            let mut acc = 0.0;
            for (value, weight) in weights {
                acc += *weight;
                if target < acc {
                    return value.clone();
                }
            }
            weights.keys().next_back().cloned().unwrap_or_default()
        }
    }
}

fn apply_fixed_range(
    rows: &mut [Vec<Value>],
    table_name: &str,
    col_name: &str,
    col_idx: usize,
    column_model: Option<&crate::synth::model::ColumnModel>,
    range: &[Value; 2],
    config: &GeneratorConfig,
) -> Result<(), String> {
    // A categorical marginal maps uniforms to category indices, not to a
    // numeric value axis, so rejection against [low, high] is meaningless.
    if matches!(
        column_model.map(|c| &c.marginal),
        Some(crate::synth::marginal::Marginal::Categorical(_))
    ) {
        return Err(format!(
            "table '{}' column '{}': fixed_range needs an invertible numeric marginal but the column is categorical; use a value pool or a numeric marginal",
            table_name, col_name
        ));
    }

    // A datetime column with a stored format generates *text*, so the range
    // check must convert both sides to epoch seconds instead of comparing the
    // formatted strings lexicographically.
    let datetime_format = column_model
        .filter(|c| matches!(c.logical_type, crate::synth::model::LogicalType::Datetime))
        .and_then(|c| c.datetime_format.as_deref());

    let mut rng = column_value_rng(config.seed, table_name, col_name, "range");
    let mut total_attempts = 0usize;
    const MAX_ATTEMPTS_PER_VALUE: usize = 10_000;

    for row in rows.iter_mut() {
        let mut accepted = false;
        for _ in 0..MAX_ATTEMPTS_PER_VALUE {
            total_attempts += 1;
            let candidate = gen_column_value(
                column_model,
                rng.gen::<f64>(),
                config.enforce_min_max_values,
            );
            match value_in_range(&candidate, range, datetime_format) {
                Some(true) => {
                    row[col_idx] = candidate;
                    accepted = true;
                    break;
                }
                Some(false) => continue,
                None => {
                    return Err(format!(
                        "table '{}' column '{}': fixed_range endpoints are not comparable with the generated values",
                        table_name, col_name
                    ));
                }
            }
        }
        if !accepted {
            return Err(format!(
                "table '{}' column '{}': fixed_range rejection exceeded {} draws for one value; the range is likely empty or nearly degenerate",
                table_name, col_name, MAX_ATTEMPTS_PER_VALUE
            ));
        }
    }

    if total_attempts > 0 {
        let quality = rows.len() as f64 / total_attempts as f64;
        if quality < 0.01 {
            return Err(format!(
                "table '{}' column '{}': fixed_range acceptance is {:.2}% (below 1%); use mode: copula_conditional instead of rejection",
                table_name,
                col_name,
                quality * 100.0
            ));
        }
    }
    Ok(())
}

/// Closed-interval check for rejection mode. A datetime column with a stored
/// format is compared in epoch seconds, the same domain `copula_conditional`
/// uses, so a value on the end date is not rejected merely because its text
/// sorts after a date-only endpoint.
fn value_in_range(
    value: &Value,
    range: &[Value; 2],
    datetime_format: Option<&str>,
) -> Option<bool> {
    let compare = |a: &Value, b: &Value| -> Option<Ordering> {
        match datetime_format {
            Some(format) => {
                let x = datetime_literal_epoch(a, Some(format))?;
                let y = datetime_literal_epoch(b, Some(format))?;
                x.partial_cmp(&y)
            }
            None => {
                let x = value_as_f64(a)?;
                let y = value_as_f64(b)?;
                x.partial_cmp(&y)
            }
        }
    };
    let low_ok = compare(&range[0], value)?;
    let high_ok = compare(value, &range[1])?;
    Some(low_ok != Ordering::Greater && high_ok != Ordering::Greater)
}

/// Epoch seconds for a datetime literal or generated value. The column's own
/// format wins; a generic ISO-ish shape is accepted as a fallback so
/// `"2026-01-31"` works on a `%Y-%m-%d %H:%M:%S` column. A bare number is
/// already epoch seconds, which is how `copula_conditional` reads endpoints.
fn datetime_literal_epoch(literal: &Value, format: Option<&str>) -> Option<f64> {
    if literal.is_number() {
        return literal.as_f64();
    }
    crate::synth::datetime::parse_to_epoch(literal, format)
        .or_else(|| crate::synth::datetime::parse_to_epoch(literal, None))
}

fn value_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
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
    use crate::synth::marginal::{CategoricalParams, Marginal, NormalParams, UniformParams};
    use crate::synth::model::{ColumnModel, CopulaInfo, LogicalType, Provenance};
    use crate::synth::rules::{ColumnRule, Relationship, TableRule, ValuePool};
    use std::collections::BTreeMap;

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
            derive: vec![],
            branches: vec![],
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
            derive: vec![],
            branches: vec![],
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
                derive: vec![],
                branches: vec![],
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
        // Behaviour change (#65c): unique + zipf is now supported via
        // Efraimidis–Spirakis sampling without replacement.
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
                    derive: vec![],
                    branches: vec![],
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
        let result = generate(&models, &rules, &config).expect("unique+zipf must not error");
        let orders = result.tables.get("orders").unwrap();
        let mut seen = std::collections::HashSet::new();
        for row in orders {
            let bits = row[0].as_f64().expect("numeric fk").to_bits();
            assert!(seen.insert(bits), "unique+zipf repeated a parent key");
        }
        assert_eq!(seen.len(), 5);
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

    fn zero_null_rate_snapshot() -> GeneratedData {
        // Two tables, mixed None/Some(0.0) null_rate, FK sampling + copula.
        // Captured before null-injection landed so AC2 can lock byte identity.
        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );

        let mut order_columns = HashMap::new();
        order_columns.insert(
            "amount".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                null_rate: Some(0.0),
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
                null_rate: Some(0.0),
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
                    column_order: vec!["amount".to_string(), "user_id".to_string()],
                    correlation: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
                },
            },
        );

        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![
                single_rule("users", vec![]),
                single_rule(
                    "orders",
                    vec![Relationship {
                        pk: "user_id".to_string(),
                        references: vec!["users.id".to_string()],
                        pool_strategy: PoolStrategy::Projection { unique: false },
                        null_label: "null".to_string(),
                    }],
                ),
            ],
        };

        generate(&models, &rules, &config(&["users", "orders"], 4)).unwrap()
    }

    #[test]
    fn should_keep_zero_null_rate_output_byte_identical() {
        let result = zero_null_rate_snapshot();
        let users = serde_json::to_string(result.tables.get("users").unwrap()).unwrap();
        let orders = serde_json::to_string(result.tables.get("orders").unwrap()).unwrap();
        // Captured from this test on the pre-null-injection generator
        // (seed 42, 4 rows, mixed None/Some(0.0) null_rate, FK + copula).
        assert_eq!(
            users,
            "[[2.3076754763602025],[-0.5264344435639146],[-0.48212996526018514],[-0.07395728769978405]]"
        );
        assert_eq!(
            orders,
            "[[8.56627954969241,-0.48212996526018514],[9.248506620196224,-0.48212996526018514],[8.233932622046893,2.3076754763602025],[7.009810102099152,-0.07395728769978405]]"
        );
    }

    /// Guards the schema extension in
    /// `docs/plans/2026-09-15-synth-rules-v1-extension.md`: rules loaded from a
    /// real YAML file (not a Rust struct) must keep producing byte-identical
    /// output. Values captured from this test before the schema extension.
    #[test]
    fn should_keep_legacy_rules_yaml_output_byte_identical() {
        let yaml = r#"
version: "1"
tables:
  - name: users
    rows: 4
    relationships: []
  - name: orders
    rows: 4
    columns:
      user_id:
        null_rate: 0.25
      amount:
        null_rate: 0.0
    relationships:
      - pk: user_id
        references: [users.id]
        pool_strategy: !projection
          unique: false
    strategy: zipf
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rules.yaml");
        std::fs::write(&path, yaml).expect("write rules");
        let rules = SynthRules::load(&path).expect("load rules");
        rules.validate().expect("validate rules");

        let mut models = HashMap::new();
        models.insert(
            "users".to_string(),
            numerical_model("users", "id", 0.0, 1.0),
        );

        let mut order_columns = HashMap::new();
        order_columns.insert(
            "amount".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                null_rate: None,
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
                null_rate: Some(0.0),
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
                    column_order: vec!["amount".to_string(), "user_id".to_string()],
                    correlation: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
                },
            },
        );

        let data = generate(&models, &rules, &config(&["users", "orders"], 4)).unwrap();
        let users = serde_json::to_string(data.tables.get("users").unwrap()).unwrap();
        let orders = serde_json::to_string(data.tables.get("orders").unwrap()).unwrap();
        // Captured from this test on the pre-extension generator (seed 42, 4
        // rows, FK projection pool, rules-driven `null_rate` on the FK column
        // and `strategy: zipf`). Any schema change that perturbs these numbers
        // for a legacy rules file must be justified in the PR.
        assert_eq!(users, "[[2.3076754763602025],[-0.5264344435639146],[-0.48212996526018514],[-0.07395728769978405]]");
        assert_eq!(orders, "[[8.56627954969241,null],[9.248506620196224,-0.5264344435639146],[8.233932622046893,null],[7.009810102099152,-0.5264344435639146]]");
    }

    fn model_with_null_rate(table: &str, column: &str, null_rate: f64) -> TableModel {
        let mut model = numerical_model(table, column, 0.0, 1.0);
        model.columns.get_mut(column).unwrap().null_rate = Some(null_rate);
        model
    }

    #[test]
    fn should_reproduce_training_null_rate() {
        let models = HashMap::from([(
            "users".to_string(),
            model_with_null_rate("users", "email", 0.20),
        )]);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("users", vec![])],
        };

        let result = generate(&models, &rules, &config(&["users"], 10_000)).unwrap();
        let rows = result.tables.get("users").unwrap();
        assert_eq!(rows.len(), 10_000);
        let nulls = rows.iter().filter(|row| row[0].is_null()).count();
        let observed = nulls as f64 / rows.len() as f64;
        assert!(
            (0.17..=0.23).contains(&observed),
            "observed null rate {observed} outside [0.17, 0.23] ({nulls}/10000)"
        );
    }

    #[test]
    fn should_place_nulls_identically_for_same_seed() {
        let models = HashMap::from([(
            "users".to_string(),
            model_with_null_rate("users", "email", 0.20),
        )]);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("users", vec![])],
        };
        let cfg = config(&["users"], 200);

        let first = generate(&models, &rules, &cfg).unwrap();
        let second = generate(&models, &rules, &cfg).unwrap();
        let mask = |data: &GeneratedData| -> Vec<bool> {
            data.tables["users"]
                .iter()
                .map(|row| row[0].is_null())
                .collect()
        };
        let first_mask = mask(&first);
        let second_mask = mask(&second);
        assert_eq!(first_mask, second_mask);
        assert!(
            first_mask.iter().any(|is_null| *is_null),
            "same-seed match must include real NULLs, not an all-filled column"
        );
        assert!(
            first_mask.iter().any(|is_null| !*is_null),
            "same-seed match must include real values, not an all-NULL column"
        );
    }

    #[test]
    fn should_keep_fk_values_in_parent_pool_when_null_rate_is_set() {
        let parent = model_with_null_rate("users", "id", 0.5);

        let mut child_columns = HashMap::new();
        child_columns.insert(
            "user_id".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: Some(0),
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                null_rate: Some(0.1),
                marginal: Marginal::Normal(NormalParams {
                    loc: 0.0,
                    scale: 1.0,
                }),
                ..Default::default()
            },
        );
        let child = TableModel {
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
            columns: child_columns,
            copula: CopulaInfo {
                column_order: vec!["user_id".to_string()],
                correlation: vec![vec![1.0]],
            },
        };

        let models = HashMap::from([("users".to_string(), parent), ("orders".to_string(), child)]);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![
                single_rule("users", vec![]),
                single_rule(
                    "orders",
                    vec![Relationship {
                        pk: "user_id".to_string(),
                        references: vec!["users.id".to_string()],
                        pool_strategy: PoolStrategy::Projection { unique: false },
                        null_label: "null".to_string(),
                    }],
                ),
            ],
        };

        let result = generate(&models, &rules, &config(&["users", "orders"], 200)).unwrap();
        let parent_rows = result.tables.get("users").unwrap();
        assert!(
            parent_rows.iter().all(|row| !row[0].is_null()),
            "referenced parent key must never be NULL"
        );
        let pool: std::collections::HashSet<String> =
            parent_rows.iter().map(|row| row[0].to_string()).collect();

        let child_rows = result.tables.get("orders").unwrap();
        let mut saw_null = false;
        let mut saw_member = false;
        for row in child_rows {
            if row[0].is_null() {
                saw_null = true;
                continue;
            }
            assert!(
                pool.contains(&row[0].to_string()),
                "FK value {} is neither NULL nor in the parent pool",
                row[0]
            );
            saw_member = true;
        }
        assert!(
            saw_null,
            "FK column with null_rate 0.1 must emit some NULLs"
        );
        assert!(saw_member, "FK column must still draw some parent keys");
    }

    #[test]
    fn should_override_null_rate_from_rules() {
        let models = HashMap::from([(
            "users".to_string(),
            model_with_null_rate("users", "email", 0.50),
        )]);
        let mut forced_zero = single_rule("users", vec![]);
        forced_zero.columns.insert(
            "email".to_string(),
            ColumnRule {
                null_rate: Some(0.0),
                marginal: None,
                ..Default::default()
            },
        );
        let zero_rules = SynthRules {
            version: "1".to_string(),
            tables: vec![forced_zero],
        };
        let zero_result = generate(&models, &zero_rules, &config(&["users"], 200)).unwrap();
        assert!(
            zero_result.tables["users"]
                .iter()
                .all(|row| !row[0].is_null()),
            "rules null_rate 0.0 must suppress the model's 0.50 rate"
        );

        let mut forced_rate = single_rule("users", vec![]);
        forced_rate.columns.insert(
            "email".to_string(),
            ColumnRule {
                null_rate: Some(0.20),
                marginal: None,
                ..Default::default()
            },
        );
        let mut zero_model = model_with_null_rate("users", "email", 0.0);
        zero_model.columns.get_mut("email").unwrap().null_rate = Some(0.0);
        let models = HashMap::from([("users".to_string(), zero_model)]);
        let rate_rules = SynthRules {
            version: "1".to_string(),
            tables: vec![forced_rate],
        };
        let rate_result = generate(&models, &rate_rules, &config(&["users"], 10_000)).unwrap();
        let nulls = rate_result.tables["users"]
            .iter()
            .filter(|row| row[0].is_null())
            .count();
        let observed = nulls as f64 / 10_000.0;
        assert!(
            (0.17..=0.23).contains(&observed),
            "rules null_rate 0.20 must win over model 0.0, got {observed}"
        );
    }

    #[test]
    fn should_not_consume_unique_fk_pool_on_null_rows() {
        let parent = numerical_model("users", "id", 0.0, 1.0);
        let mut child_columns = HashMap::new();
        child_columns.insert(
            "user_id".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                null_rate: Some(0.5),
                marginal: Marginal::Normal(NormalParams {
                    loc: 0.0,
                    scale: 1.0,
                }),
                ..Default::default()
            },
        );
        let child = TableModel {
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
            columns: child_columns,
            copula: CopulaInfo {
                column_order: vec!["user_id".to_string()],
                correlation: vec![vec![1.0]],
            },
        };
        let models = HashMap::from([("users".to_string(), parent), ("orders".to_string(), child)]);
        let mut parent_rule = single_rule("users", vec![]);
        parent_rule.rows = Some(10);
        let mut child_rule = single_rule(
            "orders",
            vec![Relationship {
                pk: "user_id".to_string(),
                references: vec!["users.id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: true },
                null_label: "null".to_string(),
            }],
        );
        child_rule.rows = Some(14);
        child_rule.columns.insert(
            "user_id".to_string(),
            ColumnRule {
                null_rate: Some(0.5),
                marginal: None,
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![parent_rule, child_rule],
        };

        let result = generate(&models, &rules, &GeneratorConfig::default())
            .expect("NULL unique-FK rows must not exhaust the parent pool");
        let pool: std::collections::HashSet<String> = result.tables["users"]
            .iter()
            .map(|row| row[0].to_string())
            .collect();
        let mut used = std::collections::HashSet::new();
        for row in &result.tables["orders"] {
            if row[0].is_null() {
                continue;
            }
            let key = row[0].to_string();
            assert!(pool.contains(&key));
            assert!(used.insert(key), "non-null unique FK must not repeat");
        }
        assert!(
            result.tables["orders"].iter().any(|row| row[0].is_null()),
            "expected some NULL FK rows so the extra children can fit"
        );
    }

    fn legacy_unquantized_amt_json() -> String {
        let mut columns = HashMap::new();
        columns.insert(
            "amt".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                min: Some(1.0),
                max: Some(2000.0),
                null_rate: Some(0.0),
                marginal: Marginal::Normal(NormalParams {
                    loc: 1004.5678,
                    scale: 12.5,
                }),
            },
        );
        let model = TableModel {
            version: 1,
            table: "payments".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "native".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
            },
            pk: vec![],
            columns,
            copula: CopulaInfo {
                column_order: vec!["amt".to_string()],
                correlation: vec![vec![1.0]],
            },
        };
        let models = HashMap::from([("payments".to_string(), model)]);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("payments", vec![])],
        };
        let result = generate(&models, &rules, &config(&["payments"], 8)).unwrap();
        serde_json::to_string(result.tables.get("payments").unwrap()).expect("serialize rows")
    }

    #[test]
    fn should_keep_legacy_model_without_decimal_scale_byte_identical() {
        // Captured from current generator output (decimal_scale: None, seed 42,
        // 8 rows) BEFORE quantization landed. A regression that starts
        // quantizing legacy models will change this JSON.
        const EXPECTED: &str = "[[1007.664388321776],[1021.4910250901899],[1000.258291072789],[1022.4674214957709],[989.8366471906627],[1004.3359119647007],[1007.8413059613897],[982.1004635402403]]";
        assert_eq!(legacy_unquantized_amt_json(), EXPECTED);
    }

    fn scaled_amt_model(decimal_scale: u8, loc: f64, std_dev: f64) -> TableModel {
        let mut columns = HashMap::new();
        columns.insert(
            "amt".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                decimal_scale: Some(decimal_scale),
                datetime_format: None,
                min: Some(1.0),
                max: Some(9999.0),
                null_rate: Some(0.0),
                marginal: Marginal::Normal(NormalParams {
                    loc,
                    scale: std_dev,
                }),
            },
        );
        TableModel {
            version: 1,
            table: "payments".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "native".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
            },
            pk: vec![],
            columns,
            copula: CopulaInfo {
                column_order: vec!["amt".to_string()],
                correlation: vec![vec![1.0]],
            },
        }
    }

    fn matches_scaled_decimal(s: &str, max_frac: usize) -> bool {
        let Some((int_part, frac)) = s.split_once('.') else {
            return false;
        };
        !int_part.is_empty()
            && int_part.bytes().all(|b| b.is_ascii_digit())
            && (1..=max_frac).contains(&frac.len())
            && frac.bytes().all(|b| b.is_ascii_digit())
    }

    fn generated_amt_strings(decimal_scale: u8, rows: usize) -> Vec<String> {
        let models = HashMap::from([(
            "payments".to_string(),
            scaled_amt_model(decimal_scale, 1004.5678, 50.0),
        )]);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("payments", vec![])],
        };
        let result = generate(&models, &rules, &config(&["payments"], rows)).unwrap();
        result
            .tables
            .get("payments")
            .unwrap()
            .iter()
            .map(|row| match &row[0] {
                Value::Number(n) => n.to_string(),
                other => panic!("expected number, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn quantize_is_bit_exact_and_uses_decimal_half_up() {
        assert_eq!(quantize(1.23456, 4).to_bits(), 1.2346f64.to_bits());
        assert_eq!(quantize(1.23455, 4).to_bits(), 1.2346f64.to_bits());
        assert_eq!(quantize(1.23454, 4).to_bits(), 1.2345f64.to_bits());
        assert_eq!(quantize(1.225, 2).to_bits(), 1.23f64.to_bits());
        assert_eq!(quantize(-1.225, 2).to_bits(), (-1.23f64).to_bits());
        let artefact = quantize(1004.9999999999999, 4);
        assert_eq!(artefact.to_bits(), 1005.0f64.to_bits());
        assert!(!format!("{artefact}").contains("9999999"));
    }

    #[test]
    fn should_quantize_generated_values_to_four_place_scale() {
        for s in generated_amt_strings(4, 10_000) {
            assert!(
                matches_scaled_decimal(&s, 4),
                "value {s} must match ^\\d+\\.\\d{{1,4}}$"
            );
            assert!(!s.contains("9999999"), "trailing 9s artefact: {s}");
        }
    }

    #[test]
    fn should_quantize_generated_values_to_two_place_scale() {
        for s in generated_amt_strings(2, 10_000) {
            assert!(
                matches_scaled_decimal(&s, 2),
                "value {s} must match ^\\d+\\.\\d{{1,2}}$"
            );
            assert!(!s.contains("9999999"), "trailing 9s artefact: {s}");
        }
    }

    #[test]
    fn should_keep_integer_columns_emitting_i64() {
        let mut columns = HashMap::new();
        columns.insert(
            "id".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: Some(0),
                datetime_epoch: None,
                decimal_scale: Some(0),
                datetime_format: None,
                min: Some(0.0),
                max: Some(100.0),
                null_rate: Some(0.0),
                marginal: Marginal::Normal(NormalParams {
                    loc: 50.0,
                    scale: 10.0,
                }),
            },
        );
        let model = TableModel {
            version: 1,
            table: "ids".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "native".to_string(),
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
        let models = HashMap::from([("ids".to_string(), model)]);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("ids", vec![])],
        };
        let result = generate(&models, &rules, &config(&["ids"], 200)).unwrap();
        for row in result.tables.get("ids").unwrap() {
            assert!(
                row[0].as_i64().is_some(),
                "rounding Some(0) must emit Value::Number(i64), got {:?}",
                row[0]
            );
        }
    }

    #[test]
    fn should_keep_legacy_datetime_model_behaviour() {
        fn datetime_categorical(logical_type: LogicalType) -> TableModel {
            let mut columns = HashMap::new();
            columns.insert(
                "created_at".to_string(),
                ColumnModel {
                    logical_type,
                    rounding: None,
                    datetime_epoch: None,
                    decimal_scale: None,
                    datetime_format: None,
                    min: None,
                    max: None,
                    null_rate: None,
                    marginal: Marginal::Categorical(CategoricalParams {
                        values: vec!["2024-01-01".into(), "2024-06-01".into()],
                        weights: vec![0.5, 0.5],
                    }),
                },
            );
            TableModel {
                version: 1,
                table: "t".to_string(),
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
                    column_order: vec!["created_at".to_string()],
                    correlation: vec![vec![1.0]],
                },
            }
        }

        let legacy = datetime_categorical(LogicalType::Datetime);
        let as_cat = datetime_categorical(LogicalType::Categorical);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("t", vec![])],
        };
        let cfg = config(&["t"], 80);
        let mut models_dt = HashMap::new();
        models_dt.insert("t".to_string(), legacy);
        let mut models_cat = HashMap::new();
        models_cat.insert("t".to_string(), as_cat);

        let from_dt = generate(&models_dt, &rules, &cfg).unwrap();
        let from_cat = generate(&models_cat, &rules, &cfg).unwrap();
        assert_eq!(
            from_dt.tables.get("t").unwrap(),
            from_cat.tables.get("t").unwrap(),
            "Datetime + Categorical + datetime_format None must match the pre-change path"
        );
        for row in from_dt.tables.get("t").unwrap() {
            let s = row[0].as_str().expect("legacy datetime stays a string");
            assert!(s == "2024-01-01" || s == "2024-06-01");
        }
    }

    fn scaled_model(scale: u8, min: f64, max: f64, loc: f64) -> ColumnModel {
        ColumnModel {
            logical_type: LogicalType::Numerical,
            rounding: None,
            datetime_epoch: None,
            decimal_scale: Some(scale),
            datetime_format: None,
            min: Some(min),
            max: Some(max),
            null_rate: Some(0.0),
            marginal: Marginal::Uniform(crate::synth::marginal::UniformParams {
                low: loc,
                high: loc,
            }),
        }
    }

    fn integer_model(min: f64, max: f64, loc: f64) -> ColumnModel {
        ColumnModel {
            rounding: Some(0),
            ..scaled_model(0, min, max, loc)
        }
    }

    #[test]
    fn should_keep_quantized_values_within_training_max() {
        // Training max sits between two scale-2 grid points (1.22, 1.23);
        // rounding the clipped value up would emit 1.23 and violate the range.
        let m = scaled_model(2, 1.0, 1.225, 1.225);
        let value = gen_column_value(Some(&m), 0.5, true);
        assert_eq!(value, Value::from(1.22));
        assert!(value.as_f64().unwrap() <= 1.225);
    }

    #[test]
    fn should_keep_quantized_values_within_training_min() {
        let m = scaled_model(2, 1.225, 2.0, 1.225);
        let value = gen_column_value(Some(&m), 0.5, true);
        assert_eq!(value, Value::from(1.23));
        assert!(value.as_f64().unwrap() >= 1.225);
    }

    #[test]
    fn should_keep_rounded_integers_within_training_max() {
        let m = integer_model(0.0, 1.5, 1.5);
        let value = gen_column_value(Some(&m), 0.5, true);
        assert_eq!(value, Value::from(1));
    }

    #[test]
    fn should_leave_quantized_values_alone_without_min_max() {
        let mut m = scaled_model(2, 0.0, 0.0, 1.225);
        m.min = None;
        m.max = None;
        assert_eq!(gen_column_value(Some(&m), 0.5, true), Value::from(1.23));
    }

    fn categorical_model(table: &str, column: &str) -> TableModel {
        let mut columns = HashMap::new();
        columns.insert(
            column.to_string(),
            ColumnModel {
                logical_type: LogicalType::Categorical,
                marginal: Marginal::Categorical(CategoricalParams {
                    values: vec!["seed".to_string()],
                    weights: vec![1.0],
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
            pk: vec![],
            columns,
            copula: CopulaInfo {
                column_order: vec![column.to_string()],
                correlation: vec![vec![1.0]],
            },
        }
    }

    fn two_column_numerical_model(table: &str, a: &str, b: &str) -> TableModel {
        let mut columns = HashMap::new();
        for (name, loc) in [(a, 0.0), (b, 10.0)] {
            columns.insert(
                name.to_string(),
                ColumnModel {
                    logical_type: LogicalType::Numerical,
                    marginal: Marginal::Normal(NormalParams { loc, scale: 1.0 }),
                    ..Default::default()
                },
            );
        }
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
            pk: vec![],
            columns,
            copula: CopulaInfo {
                column_order: vec![a.to_string(), b.to_string()],
                correlation: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
        }
    }

    #[test]
    fn should_pin_column_to_fixed_value() {
        let mut models = HashMap::new();
        models.insert("t".to_string(), numerical_model("t", "amount", 0.0, 1.0));

        let mut rule = single_rule("t", vec![]);
        rule.columns.insert(
            "amount".to_string(),
            ColumnRule {
                fixed: Some("42".to_string()),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };

        let result = generate(&models, &rules, &config(&["t"], 5)).unwrap();
        let rows = result.tables.get("t").unwrap();
        assert_eq!(rows.len(), 5);
        assert!(
            rows.iter().all(|row| row[0] == 42i64),
            "every row must be pinned to 42, got {:?}",
            rows.iter().map(|r| &r[0]).collect::<Vec<_>>()
        );
    }

    #[test]
    fn should_emit_numeric_fixed_value_without_quotes() {
        let mut models = HashMap::new();
        models.insert("t".to_string(), numerical_model("t", "part_date", 0.0, 1.0));

        let mut rule = single_rule("t", vec![]);
        rule.columns.insert(
            "part_date".to_string(),
            ColumnRule {
                fixed: Some("20240101".to_string()),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };

        let result = generate(&models, &rules, &config(&["t"], 4)).unwrap();
        for row in result.tables.get("t").unwrap() {
            assert!(
                row[0].is_i64(),
                "numeric fixed literal must not be a string, got {:?}",
                row[0]
            );
            assert_eq!(serde_json::to_string(&row[0]).unwrap(), "20240101");
        }
    }

    #[test]
    fn should_sample_weighted_value_pool_according_to_weights() {
        let mut models = HashMap::new();
        models.insert("t".to_string(), categorical_model("t", "status"));

        let mut rule = single_rule("t", vec![]);
        let mut weights = BTreeMap::new();
        weights.insert("normal".to_string(), 0.7);
        weights.insert("peak".to_string(), 0.3);
        rule.columns.insert(
            "status".to_string(),
            ColumnRule {
                values: Some(ValuePool::Weighted(weights)),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };

        let result = generate(&models, &rules, &config(&["t"], 10_000)).unwrap();
        let rows = result.tables.get("t").unwrap();
        let mut counts = HashMap::new();
        for row in rows {
            *counts
                .entry(row[0].as_str().expect("categorical value").to_string())
                .or_insert(0) += 1;
        }
        let share = |k: &str| *counts.get(k).unwrap_or(&0) as f64 / 10_000.0;
        assert!(
            (share("normal") - 0.7).abs() < 0.05,
            "normal share {} not within 5pp of 0.7",
            share("normal")
        );
        assert!(
            (share("peak") - 0.3).abs() < 0.05,
            "peak share {} not within 5pp of 0.3",
            share("peak")
        );
    }

    #[test]
    fn should_emit_numeric_value_pool_without_quotes() {
        let mut models = HashMap::new();
        models.insert("t".to_string(), numerical_model("t", "amount", 0.0, 1.0));

        let mut rule = single_rule("t", vec![]);
        rule.columns.insert(
            "amount".to_string(),
            ColumnRule {
                values: Some(ValuePool::Uniform(vec![
                    "1".to_string(),
                    "2".to_string(),
                    "3".to_string(),
                ])),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };

        let result = generate(&models, &rules, &config(&["t"], 30)).unwrap();
        for row in result.tables.get("t").unwrap() {
            let value = row[0]
                .as_i64()
                .expect("numeric value pool must emit integers");
            assert!((1..=3).contains(&value), "unexpected pool value {value}");
        }
    }

    #[test]
    fn should_reproduce_value_pool_for_same_seed() {
        let mut models = HashMap::new();
        models.insert("t".to_string(), categorical_model("t", "status"));

        let mut weights = BTreeMap::new();
        weights.insert("normal".to_string(), 0.6);
        weights.insert("peak".to_string(), 0.4);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![{
                let mut rule = single_rule("t", vec![]);
                rule.columns.insert(
                    "status".to_string(),
                    ColumnRule {
                        values: Some(ValuePool::Weighted(weights)),
                        ..Default::default()
                    },
                );
                rule
            }],
        };

        let cfg = config(&["t"], 500);
        let first = generate(&models, &rules, &cfg).unwrap();
        let second = generate(&models, &rules, &cfg).unwrap();
        assert_eq!(
            serde_json::to_string(first.tables.get("t").unwrap()).unwrap(),
            serde_json::to_string(second.tables.get("t").unwrap()).unwrap(),
            "same seed must reproduce the same value pool draw"
        );
    }

    #[test]
    fn should_keep_other_columns_rng_stream_untouched_by_fixed_column() {
        let models = HashMap::from([("t".to_string(), two_column_numerical_model("t", "a", "b"))]);

        let base_rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("t", vec![])],
        };
        let baseline = generate(&models, &base_rules, &config(&["t"], 50)).unwrap();

        let mut rule = single_rule("t", vec![]);
        rule.columns.insert(
            "a".to_string(),
            ColumnRule {
                fixed: Some("999".to_string()),
                ..Default::default()
            },
        );
        let fixed_rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };
        let fixed = generate(&models, &fixed_rules, &config(&["t"], 50)).unwrap();

        let b_baseline: Vec<String> = baseline.tables["t"]
            .iter()
            .map(|row| serde_json::to_string(&row[1]).unwrap())
            .collect();
        let b_fixed: Vec<String> = fixed.tables["t"]
            .iter()
            .map(|row| serde_json::to_string(&row[1]).unwrap())
            .collect();
        assert_eq!(
            b_baseline, b_fixed,
            "fixing column 'a' must not perturb column 'b'"
        );
        assert!(
            fixed.tables["t"].iter().all(|row| row[0] == 999i64),
            "column 'a' must be pinned to 999"
        );
    }

    #[test]
    fn should_redraw_until_value_inside_fixed_range() {
        let mut columns = HashMap::new();
        columns.insert(
            "v".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                marginal: Marginal::Uniform(UniformParams {
                    low: 0.0,
                    high: 100.0,
                }),
                ..Default::default()
            },
        );
        let model = TableModel {
            version: 1,
            table: "t".to_string(),
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
                column_order: vec!["v".to_string()],
                correlation: vec![vec![1.0]],
            },
        };
        let models = HashMap::from([("t".to_string(), model)]);

        let mut rule = single_rule("t", vec![]);
        rule.columns.insert(
            "v".to_string(),
            ColumnRule {
                fixed_range: Some([Value::from(10.0), Value::from(20.0)]),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };

        let result = generate(&models, &rules, &config(&["t"], 1000)).unwrap();
        for row in result.tables.get("t").unwrap() {
            let value = row[0].as_f64().expect("numeric value");
            assert!(
                (10.0..=20.0).contains(&value),
                "fixed_range redraw produced out-of-range value {value}"
            );
        }
    }

    #[test]
    fn should_error_when_fixed_range_on_categorical_column() {
        let models = HashMap::from([("t".to_string(), categorical_model("t", "status"))]);

        let mut rule = single_rule("t", vec![]);
        rule.columns.insert(
            "status".to_string(),
            ColumnRule {
                fixed_range: Some([Value::from("a"), Value::from("b")]),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };

        let err = generate(&models, &rules, &config(&["t"], 10))
            .expect_err("categorical fixed_range must fail");
        assert!(err.contains("status"), "error must name the column: {err}");
        assert!(err.contains("categorical"), "error must be explicit: {err}");
    }

    #[test]
    fn should_error_when_fixed_range_acceptance_is_below_one_percent() {
        let mut models = HashMap::new();
        models.insert("t".to_string(), numerical_model("t", "v", 0.0, 1.0));

        let mut rule = single_rule("t", vec![]);
        rule.columns.insert(
            "v".to_string(),
            ColumnRule {
                fixed_range: Some([Value::from(2.0), Value::from(2.1)]),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };

        let err = generate(&models, &rules, &config(&["t"], 100))
            .expect_err("sub-1% acceptance must fail fast");
        assert!(err.contains("v"), "error must name the column: {err}");
        assert!(
            err.contains("copula_conditional"),
            "error must suggest copula_conditional: {err}"
        );
    }

    // ─── copula_conditional（#68 条件采样）─────────────────────────────

    /// Two correlated standard-normal columns `a` and `b`.
    fn correlated_pair(rho: f64) -> (TableModel, HashMap<String, TableModel>) {
        let mut columns = HashMap::new();
        for name in ["a", "b"] {
            columns.insert(
                name.to_string(),
                ColumnModel {
                    logical_type: LogicalType::Numerical,
                    marginal: Marginal::Normal(NormalParams {
                        loc: 0.0,
                        scale: 1.0,
                    }),
                    ..Default::default()
                },
            );
        }
        let model = TableModel {
            version: 1,
            table: "t".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "test".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
            },
            pk: vec!["a".to_string()],
            columns,
            copula: CopulaInfo {
                column_order: vec!["a".to_string(), "b".to_string()],
                correlation: vec![vec![1.0, rho], vec![rho, 1.0]],
            },
        };
        (model.clone(), HashMap::from([("t".to_string(), model)]))
    }

    fn conditional_rule(column: &str, rule: ColumnRule) -> SynthRules {
        let mut table = single_rule("t", vec![]);
        table.columns.insert(column.to_string(), rule);
        SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        }
    }

    fn column_stats(data: &GeneratedData, table: &str, column: usize) -> (f64, f64, f64) {
        let rows = data.tables.get(table).unwrap();
        let values: Vec<f64> = rows
            .iter()
            .map(|row| row[column].as_f64().unwrap())
            .collect();
        let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        (min, max, mean)
    }

    #[test]
    fn should_truncate_pinned_column_to_the_conditional_range() {
        let (_, models) = correlated_pair(0.9);
        let rules = conditional_rule(
            "a",
            ColumnRule {
                fixed_range: Some([Value::from(-1.0), Value::from(1.0)]),
                mode: ColumnMode::CopulaConditional,
                ..Default::default()
            },
        );

        let data = generate(&models, &rules, &config(&["t"], 3000)).unwrap();
        let (min, max, _) = column_stats(&data, "t", 0);
        assert!(
            min >= -1.0 && max <= 1.0,
            "pinned column escaped the range: [{min}, {max}]"
        );
        // The range is symmetric, so the pinned column still centres on 0.
        assert!(min < -0.5 && max > 0.5, "range not covered: [{min}, {max}]");
    }

    #[test]
    fn should_condition_other_columns_on_the_pinned_value() {
        // b = 0.9 * a + noise. Pinning a at its 90th percentile (+1.2816)
        // must drag b's mean to roughly 0.9 * 1.2816 ≈ 1.15, which plain
        // independent sampling (mean ≈ 0) cannot produce.
        let (_, models) = correlated_pair(0.9);
        let rules = conditional_rule(
            "a",
            ColumnRule {
                fixed: Some("1.2816".to_string()),
                mode: ColumnMode::CopulaConditional,
                ..Default::default()
            },
        );

        let data = generate(&models, &rules, &config(&["t"], 4000)).unwrap();
        let (a_min, a_max, _) = column_stats(&data, "t", 0);
        assert_eq!(
            (a_min, a_max),
            (1.2816, 1.2816),
            "fixed value must be exact"
        );

        let (_, _, b_mean) = column_stats(&data, "t", 1);
        assert!(
            (b_mean - 1.15).abs() < 0.1,
            "conditioned mean of b was {b_mean}, expected ≈1.15"
        );
    }

    #[test]
    fn should_apply_the_conditional_pin_to_every_row_deterministically() {
        let (_, models) = correlated_pair(0.5);
        let rules = conditional_rule(
            "a",
            ColumnRule {
                fixed_range: Some([Value::from(-0.5), Value::from(0.5)]),
                mode: ColumnMode::CopulaConditional,
                ..Default::default()
            },
        );

        let first = generate(&models, &rules, &config(&["t"], 200)).unwrap();
        let second = generate(&models, &rules, &config(&["t"], 200)).unwrap();
        assert_eq!(
            serde_json::to_string(first.tables.get("t").unwrap()).unwrap(),
            serde_json::to_string(second.tables.get("t").unwrap()).unwrap()
        );
    }

    #[test]
    fn should_reject_copula_conditional_on_a_categorical_column() {
        let models = HashMap::from([("t".to_string(), categorical_model("t", "status"))]);
        let rules = conditional_rule(
            "status",
            ColumnRule {
                fixed: Some("a".to_string()),
                mode: ColumnMode::CopulaConditional,
                ..Default::default()
            },
        );

        let err = generate(&models, &rules, &config(&["t"], 10))
            .expect_err("categorical conditioning must fail");
        assert!(err.contains("status"), "error must name the column: {err}");
        assert!(err.contains("categorical"), "error must be explicit: {err}");
    }

    #[test]
    fn should_reject_copula_conditional_range_outside_the_trained_marginal() {
        let (_, models) = correlated_pair(0.5);
        let rules = conditional_rule(
            "a",
            ColumnRule {
                fixed_range: Some([Value::from(10.0), Value::from(20.0)]),
                mode: ColumnMode::CopulaConditional,
                ..Default::default()
            },
        );

        let err = generate(&models, &rules, &config(&["t"], 10))
            .expect_err("an unreachable range must fail");
        assert!(err.contains('a'), "error must name the column: {err}");
        assert!(
            err.contains("probability mass"),
            "error must explain the cause: {err}"
        );
    }

    #[test]
    fn should_keep_unpinned_output_byte_identical_when_no_conditional_mode_is_set() {
        // Guard: adding the conditional path must not perturb tables that do
        // not use it.
        let (_, models) = correlated_pair(0.9);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("t", vec![])],
        };

        let data = generate(&models, &rules, &config(&["t"], 4)).unwrap();
        let rows = serde_json::to_string(data.tables.get("t").unwrap()).unwrap();
        assert_eq!(
            rows, "[[-0.7745645303296556,-1.0475389209122454],[1.0044406514899151,0.8203717137465105],[-2.1981105969970827,-2.360240735362993],[0.1440950566134802,-0.4979425620572897]]",
            "unexpected unpinned snapshot"
        );
    }

    // ─── derive（#70 派生列）────────────────────────────────────────────

    fn three_column_model(total: ColumnModel) -> HashMap<String, TableModel> {
        let mut columns = HashMap::new();
        for name in ["price", "qty"] {
            columns.insert(
                name.to_string(),
                numerical_model("t", name, 0.0, 1.0).columns[name].clone(),
            );
        }
        columns.insert("total".to_string(), total);
        let model = TableModel {
            version: 1,
            table: "t".to_string(),
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
                column_order: vec!["price".to_string(), "qty".to_string(), "total".to_string()],
                correlation: vec![
                    vec![1.0, 0.0, 0.0],
                    vec![0.0, 1.0, 0.0],
                    vec![0.0, 0.0, 1.0],
                ],
            },
        };
        HashMap::from([("t".to_string(), model)])
    }

    fn plain_total_model() -> ColumnModel {
        numerical_model("t", "total", 0.0, 1.0).columns["total"].clone()
    }

    fn derive_rules(entries: &[(&str, &str)]) -> SynthRules {
        let mut table = single_rule("t", vec![]);
        for (column, expr) in entries {
            table.derive.push(crate::synth::rules::DeriveRule {
                column: column.to_string(),
                expr: expr.to_string(),
            });
        }
        SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        }
    }

    #[test]
    fn should_derive_column_as_an_exact_function_of_other_columns() {
        let models = three_column_model(plain_total_model());
        let rules = derive_rules(&[("total", "price * qty")]);

        let data = generate(&models, &rules, &config(&["t"], 500)).unwrap();
        for row in data.tables.get("t").unwrap() {
            let price = rust_decimal::Decimal::from_f64_retain(row[0].as_f64().unwrap()).unwrap();
            let qty = rust_decimal::Decimal::from_f64_retain(row[1].as_f64().unwrap()).unwrap();
            let total = rust_decimal::Decimal::from_f64_retain(row[2].as_f64().unwrap()).unwrap();
            assert_eq!(
                total.round_dp(12),
                (price * qty).round_dp(12),
                "row {row:?}"
            );
        }
    }

    #[test]
    fn should_derive_in_dependency_order() {
        let mut model = three_column_model(plain_total_model()).remove("t").unwrap();
        model.columns.remove("total");
        for name in ["b", "c"] {
            model.columns.insert(
                name.to_string(),
                numerical_model("t", name, 0.0, 1.0).columns[name].clone(),
            );
        }
        model.copula.column_order = vec![
            "price".to_string(),
            "qty".to_string(),
            "b".to_string(),
            "c".to_string(),
        ];
        model.copula.correlation = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
            vec![0.0, 0.0, 0.0, 1.0],
        ];
        let models = HashMap::from([("t".to_string(), model)]);
        // Declared out of order: `c` depends on `b`.
        let rules = derive_rules(&[("c", "b + 1"), ("b", "price * 2")]);

        let data = generate(&models, &rules, &config(&["t"], 200)).unwrap();
        for row in data.tables.get("t").unwrap() {
            let price = rust_decimal::Decimal::from_f64_retain(row[0].as_f64().unwrap()).unwrap();
            let b = rust_decimal::Decimal::from_f64_retain(row[2].as_f64().unwrap()).unwrap();
            let c = rust_decimal::Decimal::from_f64_retain(row[3].as_f64().unwrap()).unwrap();
            assert_eq!(
                b.round_dp(12),
                (price * rust_decimal::Decimal::TWO).round_dp(12)
            );
            assert_eq!(
                c.round_dp(12),
                (b + rust_decimal::Decimal::ONE).round_dp(12)
            );
        }
    }

    #[test]
    fn should_reject_derive_referencing_an_unknown_column() {
        let models = three_column_model(plain_total_model());
        let rules = derive_rules(&[("total", "price * ghost")]);

        let err =
            generate(&models, &rules, &config(&["t"], 10)).expect_err("unknown column must fail");
        assert!(err.contains("ghost"), "error must name the column: {err}");
    }

    // ─── branches（#70 覆盖修复）─────────────────────────────────────────

    /// `status` with the given values/weights plus a numeric `amount`.
    fn binary_model(values: &[&str], weights: &[f64]) -> HashMap<String, TableModel> {
        let mut columns = HashMap::new();
        columns.insert(
            "status".to_string(),
            ColumnModel {
                logical_type: LogicalType::Categorical,
                marginal: Marginal::Categorical(CategoricalParams {
                    values: values.iter().map(|value| value.to_string()).collect(),
                    weights: weights.to_vec(),
                }),
                ..Default::default()
            },
        );
        columns.insert(
            "amount".to_string(),
            numerical_model("t", "amount", 0.0, 1.0).columns["amount"].clone(),
        );
        let model = TableModel {
            version: 1,
            table: "t".to_string(),
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
                column_order: vec!["status".to_string(), "amount".to_string()],
                correlation: vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            },
        };
        HashMap::from([("t".to_string(), model)])
    }

    fn branch_rules(target: f64, tolerance: Option<f64>, set: &[(&str, &str)]) -> SynthRules {
        let mut table = single_rule("t", vec![]);
        table.branches.push(crate::synth::rules::BranchRule {
            id: "paid".to_string(),
            predicate: "status == 'A'".to_string(),
            target_ratio: target,
            tolerance,
            repair: crate::synth::rules::BranchRepair {
                set: set
                    .iter()
                    .map(|(column, value)| (column.to_string(), value.to_string()))
                    .collect(),
                linked_derive_recompute: true,
            },
        });
        SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        }
    }

    fn ratio_of(data: &GeneratedData, value: &str) -> f64 {
        let rows = data.tables.get("t").unwrap();
        let hits = rows
            .iter()
            .filter(|row| row[0] == Value::String(value.to_string()))
            .count();
        hits as f64 / rows.len() as f64
    }

    #[test]
    fn should_repair_branch_coverage_up_to_the_target() {
        let models = binary_model(&["A", "B"], &[0.5, 0.5]);
        let rules = branch_rules(0.8, Some(0.02), &[("status", "A")]);

        let data = generate(&models, &rules, &config(&["t"], 1000)).unwrap();
        let outcome = &data.branches[0];

        assert_eq!(outcome.status, CoverageStatus::Pass, "{outcome:?}");
        assert!(
            (ratio_of(&data, "A") - 0.8).abs() <= 0.02,
            "actual A ratio {} is outside tolerance",
            ratio_of(&data, "A")
        );
        assert!(outcome.flips > 0, "rows must have been rewritten");
        assert_eq!(outcome.failed_evaluations, 0);
    }

    #[test]
    fn should_leave_rows_untouched_when_the_branch_is_already_covered() {
        let models = binary_model(&["A", "B"], &[0.8, 0.2]);
        let rules = branch_rules(0.8, Some(0.05), &[("status", "A")]);

        let data = generate(&models, &rules, &config(&["t"], 1000)).unwrap();
        let outcome = &data.branches[0];

        assert_eq!(outcome.status, CoverageStatus::Pass, "{outcome:?}");
        assert_eq!(outcome.flips, 0, "a covered branch must not rewrite rows");
    }

    #[test]
    fn should_recompute_derived_columns_after_a_repair() {
        let mut models = binary_model(&["A", "B"], &[0.5, 0.5]);
        {
            let model = models.get_mut("t").unwrap();
            model.columns.insert(
                "double".to_string(),
                numerical_model("t", "double", 0.0, 1.0).columns["double"].clone(),
            );
            model.copula.column_order = vec![
                "status".to_string(),
                "amount".to_string(),
                "double".to_string(),
            ];
            model.copula.correlation = vec![
                vec![1.0, 0.0, 0.0],
                vec![0.0, 1.0, 0.0],
                vec![0.0, 0.0, 1.0],
            ];
        }

        // `double` depends on `amount`, which the repair rewrites.
        let mut table = single_rule("t", vec![]);
        table.derive.push(crate::synth::rules::DeriveRule {
            column: "double".to_string(),
            expr: "amount * 2".to_string(),
        });
        // The predicate reads `amount`, the same column the repair writes, so
        // the rewrite must also refresh everything derived from it.
        table.branches.push(crate::synth::rules::BranchRule {
            id: "big".to_string(),
            predicate: "amount > 3".to_string(),
            target_ratio: 0.8,
            tolerance: Some(0.02),
            repair: crate::synth::rules::BranchRepair {
                set: std::collections::BTreeMap::from([("amount".to_string(), "7.5".to_string())]),
                linked_derive_recompute: true,
            },
        });
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 600)).unwrap();
        assert_eq!(data.branches[0].status, CoverageStatus::Pass);
        for row in data.tables.get("t").unwrap() {
            let amount = rust_decimal::Decimal::from_f64_retain(row[1].as_f64().unwrap()).unwrap();
            let double = rust_decimal::Decimal::from_f64_retain(row[2].as_f64().unwrap()).unwrap();
            assert_eq!(
                double.round_dp(12),
                (amount * rust_decimal::Decimal::TWO).round_dp(12)
            );
        }
    }

    #[test]
    fn should_recompute_derived_columns_when_the_predicate_reads_them() {
        // Same shape as `should_recompute_derived_columns_after_a_repair`, but
        // the predicate reads the *derived* column. The repair writes `amount`,
        // so the branch cannot be seen to move until `double` is recomputed.
        // If the loop measures progress on stale derived values and bails out,
        // `double != amount * 2` and the branch is reported as uncovered.
        let mut models = binary_model(&["A", "B"], &[0.5, 0.5]);
        {
            let model = models.get_mut("t").unwrap();
            model.columns.insert(
                "double".to_string(),
                numerical_model("t", "double", 0.0, 1.0).columns["double"].clone(),
            );
            model.copula.column_order = vec![
                "status".to_string(),
                "amount".to_string(),
                "double".to_string(),
            ];
            model.copula.correlation = vec![
                vec![1.0, 0.0, 0.0],
                vec![0.0, 1.0, 0.0],
                vec![0.0, 0.0, 1.0],
            ];
        }

        let mut table = single_rule("t", vec![]);
        table.derive.push(crate::synth::rules::DeriveRule {
            column: "double".to_string(),
            expr: "amount * 2".to_string(),
        });
        table.branches.push(crate::synth::rules::BranchRule {
            id: "big".to_string(),
            predicate: "double > 3".to_string(),
            target_ratio: 0.8,
            tolerance: Some(0.02),
            repair: crate::synth::rules::BranchRepair {
                set: std::collections::BTreeMap::from([("amount".to_string(), "7.5".to_string())]),
                linked_derive_recompute: true,
            },
        });
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 600)).unwrap();
        assert_eq!(
            data.branches[0].status,
            CoverageStatus::Pass,
            "repair must be seen to move the predicate: {:?}",
            data.branches[0]
        );
        for row in data.tables.get("t").unwrap() {
            let amount = rust_decimal::Decimal::from_f64_retain(row[1].as_f64().unwrap()).unwrap();
            let double = rust_decimal::Decimal::from_f64_retain(row[2].as_f64().unwrap()).unwrap();
            assert_eq!(
                double.round_dp(12),
                (amount * rust_decimal::Decimal::TWO).round_dp(12),
                "derived column must stay consistent with its inputs"
            );
        }
    }

    fn datetime_model(table: &str, column: &str, format: &str, loc: f64, scale: f64) -> TableModel {
        let mut columns = HashMap::new();
        columns.insert(
            column.to_string(),
            ColumnModel {
                logical_type: LogicalType::Datetime,
                rounding: None,
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: Some(format.to_string()),
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
            pk: vec![],
            columns,
            copula: CopulaInfo {
                column_order: vec![column.to_string()],
                correlation: vec![vec![1.0]],
            },
        }
    }

    #[test]
    fn should_compare_a_datetime_fixed_range_in_epochs_not_as_text() {
        // `%d/%m/%Y` endpoints compared as text instead of as instants: the
        // high endpoint "31/01/2026" sorts below every February/December text
        // ("01/02/2026" < "31/01/2026" because '1' < '3'), so dates *outside*
        // the range leak through. The marginal is centred on 2026-01-15 with a
        // 10-day scale, so roughly 5% of the sample lands after 01-31 and must
        // be rejected.
        const FORMAT: &str = "%d/%m/%Y";
        let epoch = |s: &str| {
            crate::synth::datetime::parse_to_epoch(&Value::from(s), Some(FORMAT))
                .unwrap_or_else(|| panic!("{s} must parse"))
        };
        let mut models = HashMap::new();
        models.insert(
            "t".to_string(),
            datetime_model(
                "t",
                "created_at",
                FORMAT,
                epoch("15/01/2026"),
                10.0 * 86_400.0,
            ),
        );

        let mut table = single_rule("t", vec![]);
        table.columns.insert(
            "created_at".to_string(),
            crate::synth::rules::ColumnRule {
                fixed_range: Some([Value::from("01/01/2026"), Value::from("31/01/2026")]),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 400)).unwrap();
        let rows = data.tables.get("t").unwrap();
        let low = epoch("01/01/2026");
        let high = epoch("31/01/2026");
        let mut leaked = Vec::new();
        for row in rows {
            let text = row[0].as_str().expect("a datetime with a format is text");
            let value = crate::synth::datetime::parse_to_epoch(&Value::from(text), Some(FORMAT))
                .unwrap_or_else(|| panic!("{text} must parse"));
            if value < low || value > high {
                leaked.push(text.to_string());
            }
        }
        assert!(
            leaked.is_empty(),
            "{} of {} rows fell outside {:?}: {:?}",
            leaked.len(),
            rows.len(),
            ("01/01/2026", "31/01/2026"),
            &leaked[..leaked.len().min(5)]
        );
    }

    #[test]
    fn should_accept_a_date_only_endpoint_on_a_datetime_column() {
        // `fixed_range: ["2026-01-01", "2026-01-31"]` on a
        // `%Y-%m-%d %H:%M:%S` column is the documented shape. A date-only
        // endpoint means midnight, so the range is a closed interval of
        // instants: [01-01 00:00:00, 01-31 00:00:00].
        const FORMAT: &str = "%Y-%m-%d %H:%M:%S";
        let epoch = |s: &str| {
            crate::synth::datetime::parse_to_epoch(&Value::from(s), Some(FORMAT))
                .unwrap_or_else(|| panic!("{s} must parse"))
        };
        let mut models = HashMap::new();
        models.insert(
            "t".to_string(),
            datetime_model(
                "t",
                "created_at",
                FORMAT,
                epoch("2026-01-15 12:00:00"),
                5.0 * 86_400.0,
            ),
        );

        let mut table = single_rule("t", vec![]);
        table.columns.insert(
            "created_at".to_string(),
            crate::synth::rules::ColumnRule {
                fixed_range: Some([Value::from("2026-01-01"), Value::from("2026-01-31")]),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 200)).unwrap();
        for row in data.tables.get("t").unwrap() {
            let text = row[0].as_str().expect("a datetime with a format is text");
            let value = crate::synth::datetime::parse_to_epoch(&Value::from(text), Some(FORMAT))
                .unwrap_or_else(|| panic!("{text} must parse"));
            assert!(
                (epoch("2026-01-01 00:00:00")..=epoch("2026-01-31 00:00:00")).contains(&value),
                "{text} is outside the closed range"
            );
        }
    }

    #[test]
    fn should_accept_a_date_only_endpoint_for_copula_conditional_too() {
        // `copula_conditional` converts endpoints through
        // `literal_in_marginal_domain`, so the two modes must accept the same
        // documented shape instead of one of them demanding the column's own
        // format ("cannot read '2026-01-01' as a datetime with format
        // '%Y-%m-%d %H:%M:%S'").
        const FORMAT: &str = "%Y-%m-%d %H:%M:%S";
        let epoch = |s: &str| {
            crate::synth::datetime::parse_to_epoch(&Value::from(s), Some(FORMAT))
                .unwrap_or_else(|| panic!("{s} must parse"))
        };
        let mut models = HashMap::new();
        models.insert(
            "t".to_string(),
            datetime_model(
                "t",
                "created_at",
                FORMAT,
                epoch("2026-01-15 12:00:00"),
                5.0 * 86_400.0,
            ),
        );

        let mut table = single_rule("t", vec![]);
        table.columns.insert(
            "created_at".to_string(),
            crate::synth::rules::ColumnRule {
                fixed_range: Some([Value::from("2026-01-01"), Value::from("2026-01-31")]),
                mode: crate::synth::rules::ColumnMode::CopulaConditional,
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 200)).unwrap();
        for row in data.tables.get("t").unwrap() {
            let text = row[0].as_str().expect("a datetime with a format is text");
            let value = crate::synth::datetime::parse_to_epoch(&Value::from(text), Some(FORMAT))
                .unwrap_or_else(|| panic!("{text} must parse"));
            assert!(
                (epoch("2026-01-01 00:00:00")..=epoch("2026-01-31 00:00:00")).contains(&value),
                "{text} is outside the closed range"
            );
        }
    }

    #[test]
    fn should_propagate_null_from_a_derived_expression_source() {
        // `price * qty` is NULL whenever either input is NULL (SQL three-valued
        // logic), so a NULL source must yield a NULL target instead of
        // aborting the whole table. NULL injection runs in an earlier phase,
        // so any table with a trained or rule `null_rate > 0` hits this.
        let mut models = three_column_model(plain_total_model());
        models
            .get_mut("t")
            .unwrap()
            .columns
            .get_mut("price")
            .unwrap()
            .null_rate = Some(0.5);

        let mut table = single_rule("t", vec![]);
        table.derive.push(crate::synth::rules::DeriveRule {
            column: "total".to_string(),
            expr: "price * qty".to_string(),
        });
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 200))
            .expect("a NULL source column must not abort the generate");
        let rows = data.tables.get("t").unwrap();
        let null_rows = rows.iter().filter(|row| row[2].is_null()).count();
        assert!(
            null_rows > 0 && null_rows < rows.len(),
            "expected a mix of NULL and derived rows, got {null_rows} NULL of {}",
            rows.len()
        );
        for row in rows {
            if row[2].is_null() {
                assert!(
                    row[0].is_null(),
                    "a NULL total can only come from a NULL source: {row:?}"
                );
            } else {
                let price =
                    rust_decimal::Decimal::from_f64_retain(row[0].as_f64().unwrap()).unwrap();
                let qty = rust_decimal::Decimal::from_f64_retain(row[1].as_f64().unwrap()).unwrap();
                let total =
                    rust_decimal::Decimal::from_f64_retain(row[2].as_f64().unwrap()).unwrap();
                assert_eq!(
                    total.round_dp(12),
                    (price * qty).round_dp(12),
                    "row {row:?}"
                );
            }
        }
    }

    #[test]
    fn should_leave_no_stale_derived_value_after_a_stalled_repair_rollback() {
        // Characterisation: the per-round rollback snapshots the *whole* row,
        // so a stalled round must also undo whatever `derive` wrote on top of
        // the rewrites. Storing only the `set` cells would leave those rows
        // with `double == 14` while `amount` is back to its original value.
        let mut models = binary_model(&["A", "B"], &[0.5, 0.5]);
        {
            let model = models.get_mut("t").unwrap();
            model.columns.insert(
                "double".to_string(),
                numerical_model("t", "double", 0.0, 1.0).columns["double"].clone(),
            );
            model.copula.column_order = vec![
                "status".to_string(),
                "amount".to_string(),
                "double".to_string(),
            ];
            model.copula.correlation = vec![
                vec![1.0, 0.0, 0.0],
                vec![0.0, 1.0, 0.0],
                vec![0.0, 0.0, 1.0],
            ];
        }
        let clean = generate(
            &models,
            &SynthRules {
                version: "1".to_string(),
                tables: vec![single_rule("t", vec![])],
            },
            &config(&["t"], 400),
        )
        .unwrap();

        let mut table = single_rule("t", vec![]);
        table.derive.push(crate::synth::rules::DeriveRule {
            column: "double".to_string(),
            expr: "amount * 2".to_string(),
        });
        table.branches.push(crate::synth::rules::BranchRule {
            id: "paid".to_string(),
            predicate: "status == 'A'".to_string(),
            target_ratio: 0.9,
            tolerance: Some(0.02),
            repair: crate::synth::rules::BranchRepair {
                set: std::collections::BTreeMap::from([("amount".to_string(), "7".to_string())]),
                linked_derive_recompute: true,
            },
        });
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let repaired = generate(&models, &rules, &config(&["t"], 400)).unwrap();
        assert_ne!(
            repaired.branches[0].status,
            CoverageStatus::Pass,
            "the predicate cannot move, so the branch must not pass"
        );
        let clean_amounts: Vec<String> = clean.tables["t"]
            .iter()
            .map(|row| row[1].to_string())
            .collect();
        let repaired_amounts: Vec<String> = repaired.tables["t"]
            .iter()
            .map(|row| row[1].to_string())
            .collect();
        assert_eq!(
            clean_amounts, repaired_amounts,
            "stalled repair must leave the written column untouched"
        );
        for row in &repaired.tables["t"] {
            let amount = rust_decimal::Decimal::from_f64_retain(row[1].as_f64().unwrap()).unwrap();
            let double = rust_decimal::Decimal::from_f64_retain(row[2].as_f64().unwrap()).unwrap();
            assert_eq!(
                double.round_dp(12),
                (amount * rust_decimal::Decimal::TWO).round_dp(12),
                "a rolled-back row must not keep a stale derived value: {row:?}"
            );
        }
    }

    #[test]
    fn should_not_let_a_later_branch_undo_an_earlier_branch_in_the_same_round() {
        // Two branches flip the same column to different literals. A candidate
        // list rebuilt after the sibling wrote still sees the sibling's rows as
        // non-matching, so the later branch can steal them; the loop then
        // oscillates and only the last declared branch reaches its target.
        // The 80% `C` rows are what makes one round enough here: the two
        // branches draw from a pool neither of them covers yet.
        // `should_convert_surplus_from_an_over_target_sibling_branch` is the
        // complementary case, where no such pool exists.
        let models = binary_model(&["A", "B", "C"], &[0.1, 0.1, 0.8]);
        let mut table = single_rule("t", vec![]);
        for (id, predicate) in [("a", "status == 'A'"), ("b", "status == 'B'")] {
            table.branches.push(crate::synth::rules::BranchRule {
                id: id.to_string(),
                predicate: predicate.to_string(),
                target_ratio: if id == "a" { 0.3 } else { 0.7 },
                tolerance: Some(0.02),
                repair: crate::synth::rules::BranchRepair {
                    set: std::collections::BTreeMap::from([(
                        "status".to_string(),
                        if id == "a" { "A" } else { "B" }.to_string(),
                    )]),
                    linked_derive_recompute: false,
                },
            });
        }
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 500)).unwrap();
        let rows = data.tables.get("t").unwrap();
        let a = rows.iter().filter(|row| row[0] == "A").count() as f64 / rows.len() as f64;
        let b = rows.iter().filter(|row| row[0] == "B").count() as f64 / rows.len() as f64;
        assert!(
            (a - 0.3).abs() <= 0.02,
            "first branch must keep its 30%: got {a} ({:?})",
            data.branches
        );
        assert!(
            (b - 0.7).abs() <= 0.02,
            "second branch must reach its 70%: got {b} ({:?})",
            data.branches
        );
        assert_eq!(
            data.branches[0].rounds, 1,
            "one round is enough when the branches stop fighting: {:?}",
            data.branches
        );
        for outcome in &data.branches {
            assert_eq!(
                outcome.status,
                CoverageStatus::Pass,
                "every branch must pass: {outcome:?}"
            );
        }
    }

    #[test]
    fn should_convert_surplus_from_an_over_target_sibling_branch() {
        // `status` is binary and both branches claim one value each, so every
        // row matches *some* predicate. Reserving every already-matching row
        // leaves both branches with an empty candidate list and the partition
        // freezes at the trained 10%/90%, even though 30%/70% only asks for the
        // second branch's surplus to be converted.
        let models = binary_model(&["A", "B"], &[0.1, 0.9]);
        let mut table = single_rule("t", vec![]);
        for (id, predicate, target, literal) in [
            ("a", "status == 'A'", 0.3, "A"),
            ("b", "status == 'B'", 0.7, "B"),
        ] {
            table.branches.push(crate::synth::rules::BranchRule {
                id: id.to_string(),
                predicate: predicate.to_string(),
                target_ratio: target,
                tolerance: Some(0.02),
                repair: crate::synth::rules::BranchRepair {
                    set: std::collections::BTreeMap::from([(
                        "status".to_string(),
                        literal.to_string(),
                    )]),
                    linked_derive_recompute: false,
                },
            });
        }
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 500)).unwrap();
        let rows = data.tables.get("t").unwrap();
        let a = rows.iter().filter(|row| row[0] == "A").count() as f64 / rows.len() as f64;
        let b = rows.iter().filter(|row| row[0] == "B").count() as f64 / rows.len() as f64;
        assert!(
            (a - 0.3).abs() <= 0.02,
            "the under-target branch must reach 30%: got {a} ({:?})",
            data.branches
        );
        assert!(
            (b - 0.7).abs() <= 0.02,
            "the over-target branch must give up its surplus: got {b} ({:?})",
            data.branches
        );
        assert_eq!(
            data.branches[0].rounds, 1,
            "converting surplus is one round of work: {:?}",
            data.branches
        );
        for outcome in &data.branches {
            assert_eq!(
                outcome.status,
                CoverageStatus::Pass,
                "every branch must pass: {outcome:?}"
            );
        }
    }

    #[test]
    fn should_stack_two_branches_that_write_different_columns() {
        // The two predicates read and write disjoint columns, so the same row
        // can satisfy both. Treating every sibling match as untouchable (or
        // every row a sibling wrote this round as claimed) caps the second
        // branch at whatever rows the first branch did not use.
        let models = binary_model(&["A", "B"], &[0.2, 0.8]);
        let mut table = single_rule("t", vec![]);
        table.branches.push(crate::synth::rules::BranchRule {
            id: "a".to_string(),
            predicate: "status == 'A'".to_string(),
            target_ratio: 0.9,
            tolerance: Some(0.02),
            repair: crate::synth::rules::BranchRepair {
                set: std::collections::BTreeMap::from([("status".to_string(), "A".to_string())]),
                linked_derive_recompute: false,
            },
        });
        table.branches.push(crate::synth::rules::BranchRule {
            id: "seven".to_string(),
            predicate: "amount == 7".to_string(),
            target_ratio: 0.5,
            tolerance: Some(0.02),
            repair: crate::synth::rules::BranchRepair {
                set: std::collections::BTreeMap::from([("amount".to_string(), "7".to_string())]),
                linked_derive_recompute: false,
            },
        });
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 500)).unwrap();
        let rows = data.tables.get("t").unwrap();
        let a = rows.iter().filter(|row| row[0] == "A").count() as f64 / rows.len() as f64;
        let seven = rows
            .iter()
            .filter(|row| row[1].as_f64() == Some(7.0))
            .count() as f64
            / rows.len() as f64;
        assert!(
            (a - 0.9).abs() <= 0.02,
            "first branch must reach 90%: got {a} ({:?})",
            data.branches
        );
        assert!(
            (seven - 0.5).abs() <= 0.02,
            "the second branch must also reach 50% on the rows the first just wrote: got {seven} ({:?})",
            data.branches
        );
        for outcome in &data.branches {
            assert_eq!(
                outcome.status,
                CoverageStatus::Pass,
                "every branch must pass: {outcome:?}"
            );
        }
    }

    #[test]
    fn should_not_let_a_branch_destroy_coverage_that_runs_through_a_derived_column() {
        // `big` matches on the derived `double`, which is only refreshed at the
        // end of a round. Simulating a sibling's `set` without re-running
        // `derive` leaves the stale `double` on the clone, so `killed` writing
        // `amount = 0` looks harmless while it actually wipes out matches
        // `big` has not finished collecting.
        let models = derived_amount_model();
        let (baseline, deficit) = derived_baseline(&models);
        let data = generate(&models, &derived_branch_rules(false), &config(&["t"], 600)).unwrap();
        assert_derived_repair(&data, baseline, deficit);
    }

    #[test]
    fn should_not_let_a_sibling_clobber_rows_written_for_a_derived_predicate() {
        // Same fixture, reversed declaration order: `big` writes `amount = 7.5`
        // first, and those rows only match `double > 3` once `derive` runs.
        // Judging the clone without re-deriving makes them look unconverted, so
        // `killed` would rewrite the very cells `big` just wrote and send it
        // back for another round.
        let models = derived_amount_model();
        let (baseline, deficit) = derived_baseline(&models);
        let data = generate(&models, &derived_branch_rules(true), &config(&["t"], 600)).unwrap();
        assert_derived_repair(&data, baseline, deficit);
    }

    /// `derive: double = amount * 2` plus a branch on the derived column
    /// (`big`) and one rewriting its source (`killed`), declared in the given
    /// order.
    fn derived_branch_rules(derived_first: bool) -> SynthRules {
        derived_branch_rules_with(derived_first, 0.6, 0.3)
    }

    fn derived_branch_rules_with(
        derived_first: bool,
        big_target: f64,
        killed_target: f64,
    ) -> SynthRules {
        let mut table = single_rule("t", vec![]);
        table.derive.push(derive_double_rule());
        table.branches = vec![
            derived_test_branch("killed", "amount == 0", killed_target, "amount", "0"),
            derived_test_branch("big", "double > 3", big_target, "amount", "7.5"),
        ];
        if derived_first {
            table.branches.reverse();
        }
        SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        }
    }

    fn derived_test_branch(
        id: &str,
        predicate: &str,
        target: f64,
        column: &str,
        literal: &str,
    ) -> crate::synth::rules::BranchRule {
        crate::synth::rules::BranchRule {
            id: id.to_string(),
            predicate: predicate.to_string(),
            target_ratio: target,
            tolerance: Some(0.02),
            repair: crate::synth::rules::BranchRepair {
                set: std::collections::BTreeMap::from([(column.to_string(), literal.to_string())]),
                linked_derive_recompute: true,
            },
        }
    }

    fn derive_double_rule() -> crate::synth::rules::DeriveRule {
        crate::synth::rules::DeriveRule {
            column: "double".to_string(),
            expr: "amount * 2".to_string(),
        }
    }

    /// Rows matching `double > 3` in a branch-free run (same seed), and how many
    /// rows `big` is short of its 60% target.
    fn derived_baseline(models: &HashMap<String, TableModel>) -> (usize, usize) {
        let mut clean_table = single_rule("t", vec![]);
        clean_table.derive.push(derive_double_rule());
        let clean = generate(
            models,
            &SynthRules {
                version: "1".to_string(),
                tables: vec![clean_table],
            },
            &config(&["t"], 600),
        )
        .unwrap();
        let baseline = clean.tables["t"]
            .iter()
            .filter(|row| row[2].as_f64().is_some_and(|double| double > 3.0))
            .count();
        (baseline, (0.6 * 600f64).round() as usize - baseline)
    }

    fn assert_derived_repair(data: &GeneratedData, baseline: usize, deficit: usize) {
        let rows = data.tables.get("t").unwrap();
        let big = rows
            .iter()
            .filter(|row| row[2].as_f64().is_some_and(|double| double > 3.0))
            .count() as f64
            / rows.len() as f64;
        let killed = rows
            .iter()
            .filter(|row| row[1].as_f64() == Some(0.0))
            .count() as f64
            / rows.len() as f64;
        for row in rows {
            let amount = rust_decimal::Decimal::from_f64_retain(row[1].as_f64().unwrap()).unwrap();
            let double = rust_decimal::Decimal::from_f64_retain(row[2].as_f64().unwrap()).unwrap();
            assert_eq!(
                double.round_dp(12),
                (amount * rust_decimal::Decimal::TWO).round_dp(12),
                "derived column must stay consistent with its inputs"
            );
        }
        assert!(
            (big - 0.6).abs() <= 0.02,
            "the branch reading the derived column must keep its 60%: got {big} ({:?})",
            data.branches
        );
        assert!(
            (killed - 0.3).abs() <= 0.02,
            "the sibling must reach its 30% without eating those rows: got {killed} ({:?})",
            data.branches
        );
        for outcome in &data.branches {
            assert_eq!(
                outcome.status,
                CoverageStatus::Pass,
                "every branch must pass: {outcome:?}"
            );
        }
        let flips = |id: &str| {
            data.branches
                .iter()
                .find(|outcome| outcome.id == id)
                .unwrap()
                .flips
        };
        assert_eq!(
            flips("big"),
            deficit,
            "the derived branch must only add the rows it is short of, not redo \
             matches a sibling wiped out; baseline {baseline}, deficit {deficit}: {:?}",
            data.branches
        );
        assert_eq!(
            flips("killed"),
            180,
            "the source-rewriting branch must not have to repeat work: {:?}",
            data.branches
        );
        for outcome in &data.branches {
            assert_eq!(
                outcome.rounds, 1,
                "both branches must settle in one round: {:?}",
                data.branches
            );
        }
    }

    #[test]
    fn should_not_let_an_unreachable_branch_destroy_a_reachable_one() {
        // `killed` cannot reach 95% without rewriting rows `big` still needs,
        // so left unchecked it steals `big`'s matches every round until `big`
        // reports 5% coverage. Protecting an under-target sibling keeps the
        // achievable branch achievable; the impossible one is the one that
        // warns.
        let models = derived_amount_model();
        let rules = derived_branch_rules_with(true, 0.6, 0.95);
        let data = generate(&models, &rules, &config(&["t"], 600)).unwrap();
        let rows = data.tables.get("t").unwrap();
        let big = rows
            .iter()
            .filter(|row| row[2].as_f64().is_some_and(|double| double > 3.0))
            .count() as f64
            / rows.len() as f64;
        for row in rows {
            let amount = rust_decimal::Decimal::from_f64_retain(row[1].as_f64().unwrap()).unwrap();
            let double = rust_decimal::Decimal::from_f64_retain(row[2].as_f64().unwrap()).unwrap();
            assert_eq!(
                double.round_dp(12),
                (amount * rust_decimal::Decimal::TWO).round_dp(12),
                "derived column must stay consistent with its inputs"
            );
        }
        let outcome = |id: &str| {
            data.branches
                .iter()
                .find(|outcome| outcome.id == id)
                .unwrap()
                .clone()
        };
        assert_eq!(
            outcome("big").status,
            CoverageStatus::Pass,
            "an unreachable sibling must not sink the reachable branch: {:?}",
            data.branches
        );
        assert!(
            (big - 0.6).abs() <= 0.02,
            "the reachable branch must keep its 60%: got {big} ({:?})",
            data.branches
        );
        assert_ne!(
            outcome("killed").status,
            CoverageStatus::Pass,
            "the impossible target must be reported, not quietly met: {:?}",
            data.branches
        );
    }

    /// `status` (A/B) plus a numeric `amount` centred on the boundary of
    /// `double > 3` for `double = amount * 2`, and the derived column itself.
    fn derived_amount_model() -> HashMap<String, TableModel> {
        let mut models = binary_model(&["A", "B"], &[0.5, 0.5]);
        let model = models.get_mut("t").unwrap();
        model.columns.insert(
            "amount".to_string(),
            numerical_model("t", "amount", 1.5, 1.0).columns["amount"].clone(),
        );
        model.columns.insert(
            "double".to_string(),
            numerical_model("t", "double", 0.0, 1.0).columns["double"].clone(),
        );
        model.copula.column_order = vec![
            "status".to_string(),
            "amount".to_string(),
            "double".to_string(),
        ];
        model.copula.correlation = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        models
    }

    #[test]
    fn should_leave_no_writes_behind_when_a_repair_cannot_move_the_predicate() {
        // `set` writes `amount`, but the predicate only reads `status`, so no
        // round can ever move it. The values already written must be rolled
        // back: a branch that reports no progress must not still mutate data.
        let models = binary_model(&["A", "B"], &[0.5, 0.5]);
        let clean = generate(
            &models,
            &SynthRules {
                version: "1".to_string(),
                tables: vec![single_rule("t", vec![])],
            },
            &config(&["t"], 400),
        )
        .unwrap();
        let rules = branch_rules(0.8, Some(0.02), &[("amount", "1")]);
        let repaired = generate(&models, &rules, &config(&["t"], 400)).unwrap();

        let outcome = &repaired.branches[0];
        assert_ne!(outcome.status, CoverageStatus::Pass, "{outcome:?}");
        assert_eq!(
            outcome.flips, 0,
            "a repair that could not move the predicate must not report flips"
        );

        let clean_amounts: Vec<String> = clean.tables["t"]
            .iter()
            .map(|row| row[1].to_string())
            .collect();
        let repaired_amounts: Vec<String> = repaired.tables["t"]
            .iter()
            .map(|row| row[1].to_string())
            .collect();
        assert_eq!(
            clean_amounts, repaired_amounts,
            "stalled repair must leave the column untouched"
        );
    }

    #[test]
    fn should_report_a_branch_whose_repair_cannot_move_the_predicate() {
        // `set` writes `amount`, but the predicate only reads `status`.
        let models = binary_model(&["A", "B"], &[0.5, 0.5]);
        let rules = branch_rules(0.8, Some(0.02), &[("amount", "1")]);

        let data = generate(&models, &rules, &config(&["t"], 1000)).unwrap();
        let outcome = &data.branches[0];

        assert_ne!(outcome.status, CoverageStatus::Pass, "{outcome:?}");
        assert!(
            outcome.rounds < crate::synth::rules::MAX_REPAIR_ROUNDS,
            "the loop must stop as soon as a round makes no progress"
        );
    }

    #[test]
    fn should_fail_a_branch_that_needs_rows_it_cannot_write() {
        // Every row matches and the target is 0: `set` cannot un-match rows.
        let models = binary_model(&["A"], &[1.0]);
        let rules = branch_rules(0.0, None, &[("status", "A")]);

        let data = generate(&models, &rules, &config(&["t"], 200)).unwrap();
        let outcome = &data.branches[0];

        assert_eq!(outcome.status, CoverageStatus::Fail, "{outcome:?}");
        assert_eq!(outcome.flips, 0);
    }

    #[test]
    fn should_reject_branch_repair_on_a_fk_column() {
        let mut models = HashMap::new();
        models.insert(
            "parent".to_string(),
            numerical_model("parent", "id", 0.0, 1.0),
        );
        models.insert(
            "t".to_string(),
            binary_model(&["A", "B"], &[0.5, 0.5]).remove("t").unwrap(),
        );

        let mut table = single_rule(
            "t",
            vec![Relationship {
                pk: "amount".to_string(),
                references: vec!["parent.id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                null_label: "null".to_string(),
            }],
        );
        table.branches.push(crate::synth::rules::BranchRule {
            id: "paid".to_string(),
            predicate: "status == 'A'".to_string(),
            target_ratio: 0.8,
            tolerance: None,
            repair: crate::synth::rules::BranchRepair {
                set: std::collections::BTreeMap::from([("amount".to_string(), "1".to_string())]),
                linked_derive_recompute: false,
            },
        });
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("parent", vec![]), table],
        };

        let err = generate(&models, &rules, &config(&["parent", "t"], 50))
            .expect_err("repairing an FK column must fail");
        assert!(err.contains("amount"), "error must name the column: {err}");
        assert!(
            err.contains("referential integrity"),
            "error must explain why: {err}"
        );
    }

    #[test]
    fn should_fail_a_branch_whose_predicate_matches_no_row_type() {
        // `status` is a string column; comparing it to a number never
        // evaluates. Reporting 0% coverage here would hide a real mistake.
        let models = binary_model(&["A", "B"], &[0.5, 0.5]);
        let mut table = single_rule("t", vec![]);
        table.branches.push(crate::synth::rules::BranchRule {
            id: "paid".to_string(),
            predicate: "status == 1".to_string(),
            target_ratio: 0.8,
            tolerance: None,
            repair: crate::synth::rules::BranchRepair {
                set: std::collections::BTreeMap::from([("status".to_string(), "A".to_string())]),
                linked_derive_recompute: false,
            },
        });
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let err = generate(&models, &rules, &config(&["t"], 50))
            .expect_err("an unevaluable predicate must fail");
        assert!(err.contains("paid"), "error must name the branch: {err}");
        assert!(
            err.contains("evaluated"),
            "error must explain the predicate could not run: {err}"
        );
    }

    // ─── values 分布校验（#76-C）────────────────────────────────────────

    fn value_pool_rules(weights: &[(&str, f64)]) -> SynthRules {
        let mut table = single_rule("t", vec![]);
        table.columns.insert(
            "status".to_string(),
            ColumnRule {
                values: Some(ValuePool::Weighted(
                    weights
                        .iter()
                        .map(|(value, weight)| (value.to_string(), *weight))
                        .collect(),
                )),
                ..Default::default()
            },
        );
        SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        }
    }

    #[test]
    fn should_report_value_pool_shares_close_to_the_declared_weights() {
        let models = binary_model(&["A", "B"], &[0.5, 0.5]);
        let rules = value_pool_rules(&[("A", 0.7), ("B", 0.3)]);

        let data = generate(&models, &rules, &config(&["t"], 5000)).unwrap();
        assert_eq!(data.value_pools.len(), 1, "one pool must be audited");
        let outcome = &data.value_pools[0];
        assert_eq!(outcome.column, "status");
        assert!(
            outcome.is_within_tolerance(),
            "a correct pool must not warn: {outcome:?}"
        );
        // The two shares must add up to the whole table.
        let total: f64 = outcome.actual.iter().map(|(_, share)| share).sum();
        assert!((total - 1.0).abs() < 1e-9, "shares summed to {total}");
    }

    #[test]
    fn should_flag_a_value_pool_whose_shares_miss_the_declared_weights() {
        // Exercises the real checker: the rows below hold a 50/50 split while
        // the pool declares 70/30, which is what a broken pool would look
        // like. The sampler cannot produce this by itself, so the audit is
        // the guard against a regression.
        let models = binary_model(&["A", "B"], &[0.5, 0.5]);
        let model = models.get("t").unwrap();
        let rules = value_pool_rules(&[("A", 0.7), ("B", 0.3)]);
        let rows: Vec<Vec<Value>> = (0..10)
            .map(|index| vec![Value::String(if index < 5 { "A" } else { "B" }.to_string())])
            .collect();

        let outcomes =
            check_value_pools(&rows, "t", &rules.tables[0], model, &["status".to_string()]);
        assert_eq!(outcomes.len(), 1);
        let outcome = &outcomes[0];
        assert!(
            (outcome.max_deviation - 0.2).abs() < 1e-12,
            "deviation was {}",
            outcome.max_deviation
        );
        assert!(outcome.declared[0].1 > outcome.actual[0].1);
        assert!(!outcome.is_within_tolerance());
    }

    #[test]
    fn should_audit_a_uniform_value_pool_against_equal_shares() {
        let mut table = single_rule("t", vec![]);
        table.columns.insert(
            "status".to_string(),
            ColumnRule {
                values: Some(ValuePool::Uniform(vec!["A".to_string(), "B".to_string()])),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };
        let models = binary_model(&["A", "B"], &[0.5, 0.5]);

        let data = generate(&models, &rules, &config(&["t"], 4000)).unwrap();
        let outcome = &data.value_pools[0];
        assert!((outcome.declared[0].1 - 0.5).abs() < 1e-12);
        assert!(outcome.is_within_tolerance(), "{outcome:?}");
    }

    #[test]
    fn should_audit_numeric_value_pools_by_typed_literal() {
        // A numeric pool key must be matched against the generated number,
        // not against its string form.
        let mut models = binary_model(&["A", "B"], &[0.5, 0.5]);
        models
            .get_mut("t")
            .unwrap()
            .columns
            .get_mut("amount")
            .unwrap()
            .logical_type = LogicalType::Numerical;
        let mut table = single_rule("t", vec![]);
        table.columns.insert(
            "amount".to_string(),
            ColumnRule {
                values: Some(ValuePool::Weighted(std::collections::BTreeMap::from([
                    ("1".to_string(), 0.8),
                    ("2".to_string(), 0.2),
                ]))),
                ..Default::default()
            },
        );
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![table],
        };

        let data = generate(&models, &rules, &config(&["t"], 2000)).unwrap();
        let outcome = &data.value_pools[0];
        assert_eq!(outcome.column, "amount");
        assert!(
            outcome.is_within_tolerance(),
            "typed numeric pool must match: {outcome:?}"
        );
    }

    /// Parent `p(id)` referenced by `c(fk)`, plus a plain `x` column.
    fn parent_child_models() -> HashMap<String, TableModel> {
        let mut models = HashMap::new();
        models.insert("p".to_string(), numerical_model("p", "id", 0.0, 1.0));
        models.insert("c".to_string(), numerical_model("c", "fk", 0.0, 1.0));
        models
    }

    fn parent_child_rules(parent: TableRule) -> SynthRules {
        SynthRules {
            version: "1".to_string(),
            tables: vec![
                parent,
                single_rule(
                    "c",
                    vec![Relationship {
                        pk: "fk".to_string(),
                        references: vec!["p.id".to_string()],
                        pool_strategy: PoolStrategy::Projection { unique: false },
                        null_label: "null".to_string(),
                    }],
                ),
            ],
        }
    }

    #[test]
    fn should_reject_derive_on_a_referenced_parent_key_at_generate_time() {
        // `run_generate` calls `validate`, but the generator keeps its own
        // guard so a direct `generate` call cannot duplicate a parent key.
        let models = parent_child_models();
        let mut parent = single_rule("p", vec![]);
        parent.derive.push(crate::synth::rules::DeriveRule {
            column: "id".to_string(),
            expr: "id * 0".to_string(),
        });
        let rules = parent_child_rules(parent);

        let err = generate(&models, &rules, &config(&["p", "c"], 20))
            .expect_err("deriving a referenced parent key must fail");
        assert!(err.contains("p.id"), "error must name the key: {err}");
        assert!(
            err.contains("derive") || err.contains("derived"),
            "error must name the phase: {err}"
        );
    }

    #[test]
    fn should_reject_branch_repair_on_a_referenced_parent_key_at_generate_time() {
        let models = parent_child_models();
        let mut parent = single_rule("p", vec![]);
        parent.branches.push(crate::synth::rules::BranchRule {
            id: "pin".to_string(),
            predicate: "id > 0".to_string(),
            target_ratio: 0.9,
            tolerance: None,
            repair: crate::synth::rules::BranchRepair {
                set: std::collections::BTreeMap::from([("id".to_string(), "1".to_string())]),
                linked_derive_recompute: false,
            },
        });
        let rules = parent_child_rules(parent);

        let err = generate(&models, &rules, &config(&["p", "c"], 20))
            .expect_err("repairing a referenced parent key must fail");
        assert!(err.contains("id"), "error must name the column: {err}");
        assert!(
            err.contains("referenced"),
            "error must explain the reference: {err}"
        );
    }

    #[test]
    fn should_keep_output_byte_identical_without_branch_rules() {
        let models = binary_model(&["A", "B"], &[0.5, 0.5]);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("t", vec![])],
        };

        let data = generate(&models, &rules, &config(&["t"], 4)).unwrap();
        assert!(data.branches.is_empty());
        let rows = serde_json::to_string(data.tables.get("t").unwrap()).unwrap();
        assert_eq!(rows, "[[\"A\",-0.803943491589223],[\"B\",-0.19184861516094998],[\"A\",-0.8762332024966213],[\"B\",-1.4398776414381587]]");
    }

    #[test]
    fn should_keep_output_byte_identical_without_derive_rules() {
        let models = three_column_model(plain_total_model());
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![single_rule("t", vec![])],
        };

        let data = generate(&models, &rules, &config(&["t"], 3)).unwrap();
        let rows = serde_json::to_string(data.tables.get("t").unwrap()).unwrap();
        assert_eq!(rows, "[[-0.7745645303296556,0.1440950566134802,-0.8762332024966213],[1.0044406514899151,-0.803943491589223,-1.4398776414381587],[-2.1981105969970827,-0.19184861516094998,0.5787749357941152]]");
    }
}
