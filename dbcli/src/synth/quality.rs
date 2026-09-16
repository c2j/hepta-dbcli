//! `synth report` quality metrics and the holdout baseline they score against
//! (#67).
//!
//! `train` records a compact summary of a held-out slice next to each model
//! (`<table>.report-baseline.json`): quantile knots per numeric column,
//! value frequencies per categorical column, and summary statistics for
//! selected column pairs. No raw rows are stored, so a model plus its
//! baseline can be scored offline, without the source database.
//!
//! Scoring follows the SDMetrics vocabulary on the single-table level:
//! `1 - KS` for numeric column shapes, `1 - TV` for categorical shapes,
//! Gaussian-free Pearson difference for numeric pairs, and joint
//! total-variation for low-cardinality categorical pairs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

use crate::synth::marginal::{EcdfFitter, EcdfParams, Marginal, MarginalFitter};
use crate::synth::model::{LogicalType, TableModel};

/// Version of the baseline file layout.
pub const BASELINE_SCHEMA_VERSION: u32 = 1;
/// Version of the report layout.
pub const REPORT_SCHEMA_VERSION: u32 = 1;
/// Holdout rows kept for the baseline summary, at most.
pub const HOLDOUT_MAX_ROWS: usize = 50_000;
/// Fraction of the sampled rows reserved as the holdout by default.
pub const DEFAULT_HOLDOUT_RATIO: f64 = 0.1;
/// Distinct levels per side above which a categorical pair is not baselined
/// (the joint table would grow quadratically).
pub const CATEGORICAL_PAIR_LEVEL_CAP: usize = 12;
/// Distinct levels above which a categorical column is displayed but not
/// averaged into the section score: total variation between two multinomial
/// samples over hundreds of levels is ~1 even for a perfect model, so the
/// number would swamp every other column rather than measure fidelity.
pub const CATEGORICAL_SCORE_LEVEL_CAP: usize = 50;

// ─── Baseline ───────────────────────────────────────────────────────────

/// Holdout summary written next to a trained model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineSummary {
    pub schema_version: u32,
    pub table: String,
    pub holdout_rows: usize,
    pub columns: HashMap<String, ColumnBaseline>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pairs: Vec<PairBaseline>,
}

/// Per-column holdout summary. Numeric columns keep the same uniform-grid
/// quantile knots as `Marginal::Ecdf`, so a baseline CDF is directly a
/// `EcdfParams`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ColumnBaseline {
    Numerical {
        knots: Vec<f64>,
    },
    Categorical {
        /// `(value, frequency)` sorted by value; frequencies sum to 1.
        values: Vec<(String, f64)>,
    },
}

/// Pair summary: the baseline decides which pairs are scored, which keeps the
/// report from enumerating O(d²) pairs it has no reference for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PairBaseline {
    Numerical {
        left: String,
        right: String,
        pearson: f64,
    },
    Categorical {
        left: String,
        right: String,
        /// `(left_value, right_value, joint_frequency)` sorted by value pair.
        joint: Vec<(String, String, f64)>,
    },
}

impl BaselineSummary {
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path).map_err(|e| format!("read baseline: {}", e))?;
        let summary: Self =
            serde_json::from_str(&content).map_err(|e| format!("parse baseline: {}", e))?;
        if summary.schema_version != BASELINE_SCHEMA_VERSION {
            return Err(format!(
                "baseline schema_version {} not supported (expected {})",
                summary.schema_version, BASELINE_SCHEMA_VERSION
            ));
        }
        Ok(summary)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let content =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize baseline: {}", e))?;
        std::fs::write(path, content).map_err(|e| format!("write baseline: {}", e))
    }
}

/// Indices of the rows that form the holdout, capped at [`HOLDOUT_MAX_ROWS`].
///
/// Selection is a deterministic hash of the row index. An index *stride*
/// (`i % 10`) aliases with any pattern whose period divides the stride: a
/// column cycling 5 levels puts a single level into every holdout row, and the
/// baseline then describes that level as the whole distribution. Hashing the
/// index keeps the sample reproducible while breaking that aliasing.
pub fn holdout_indices(row_count: usize, ratio: f64) -> Vec<usize> {
    if ratio <= 0.0 || !ratio.is_finite() || row_count == 0 {
        return Vec::new();
    }
    let stride = (1.0 / ratio).round().max(1.0) as u64;
    (0..row_count)
        .filter(|i| splitmix64(*i as u64).is_multiple_of(stride))
        .take(HOLDOUT_MAX_ROWS)
        .collect()
}

/// SplitMix64: a fixed integer mix used only to pick holdout rows.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Build the holdout baseline for one table.
///
/// `col_indices[i]` is the position of `model.copula.column_order[i]` inside
/// `rows` (the sampled rows are full width, the model keeps a filtered order).
pub fn build_baseline(
    table: &str,
    model: &TableModel,
    rows: &[Vec<Value>],
    col_indices: &[usize],
    ratio: f64,
) -> Result<BaselineSummary, String> {
    let holdout = holdout_indices(rows.len(), ratio);
    let mut columns = HashMap::new();
    for (name, &col_idx) in model.copula.column_order.iter().zip(col_indices) {
        let Some(model_column) = model.columns.get(name) else {
            continue;
        };
        let samples: Vec<&Value> = holdout
            .iter()
            .filter_map(|&row_idx| rows.get(row_idx).and_then(|row| row.get(col_idx)))
            .collect();
        // The baseline kind follows the fitted marginal, not the declared
        // type: a numeric-encoded datetime is modelled (and scored) on a
        // numeric axis, while a legacy datetime that fell back to a value
        // dictionary is scored as categorical.
        let baseline = if matches!(
            model_column.marginal,
            crate::synth::marginal::Marginal::Categorical(_)
        ) {
            let values = frequencies(&samples);
            if values.is_empty() {
                continue;
            }
            ColumnBaseline::Categorical { values }
        } else {
            let numeric: Vec<f64> = samples
                .iter()
                .filter_map(|value| crate::synth::marginal::numeric_axis_value(value, model_column))
                .collect();
            // An all-NULL holdout slice carries no shape to record.
            if numeric.len() < 2 {
                continue;
            }
            let Marginal::Ecdf(params) = EcdfFitter.fit(&numeric)? else {
                return Err(format!("column '{}': ECDF fit did not return Ecdf", name));
            };
            ColumnBaseline::Numerical {
                knots: params.knots,
            }
        };
        columns.insert(name.clone(), baseline);
    }

    let pairs = build_pair_baselines(model, rows, col_indices, &holdout);

    Ok(BaselineSummary {
        schema_version: BASELINE_SCHEMA_VERSION,
        table: table.to_string(),
        holdout_rows: holdout.len(),
        columns,
        pairs,
    })
}

fn build_pair_baselines(
    model: &TableModel,
    rows: &[Vec<Value>],
    col_indices: &[usize],
    holdout: &[usize],
) -> Vec<PairBaseline> {
    let mut numeric_columns: Vec<(String, usize)> = Vec::new();
    let mut categorical_columns: Vec<(String, usize)> = Vec::new();
    for (name, &col_idx) in model.copula.column_order.iter().zip(col_indices) {
        match model.columns.get(name).map(|c| &c.logical_type) {
            Some(LogicalType::Numerical) => numeric_columns.push((name.clone(), col_idx)),
            Some(LogicalType::Categorical) => categorical_columns.push((name.clone(), col_idx)),
            _ => {}
        }
    }
    // Fixed column order: pair output must not depend on HashMap iteration.
    numeric_columns.sort();
    categorical_columns.sort();

    let mut pairs = Vec::new();
    for i in 0..numeric_columns.len() {
        for j in (i + 1)..numeric_columns.len() {
            let (left, left_idx) = &numeric_columns[i];
            let (right, right_idx) = &numeric_columns[j];
            let paired: Vec<(Option<f64>, Option<f64>)> = holdout
                .iter()
                .map(|&row_idx| {
                    let row = rows.get(row_idx);
                    (
                        row.and_then(|r| r.get(*left_idx)).and_then(numeric_value),
                        row.and_then(|r| r.get(*right_idx)).and_then(numeric_value),
                    )
                })
                .collect();
            let Some(pearson) = pearson_pairwise(&paired) else {
                continue;
            };
            pairs.push(PairBaseline::Numerical {
                left: left.clone(),
                right: right.clone(),
                pearson,
            });
        }
    }

    for i in 0..categorical_columns.len() {
        for j in (i + 1)..categorical_columns.len() {
            let (left, left_idx) = &categorical_columns[i];
            let (right, right_idx) = &categorical_columns[j];
            let paired: Vec<(String, String)> = holdout
                .iter()
                .filter_map(|&row_idx| {
                    let row = rows.get(row_idx)?;
                    let a = row.get(*left_idx)?.as_str().map(str::to_string);
                    let b = row.get(*right_idx)?.as_str().map(str::to_string);
                    Some((a?, b?))
                })
                .collect();
            let levels = distinct_levels(&paired);
            let max_levels = levels.0.max(levels.1);
            if max_levels > CATEGORICAL_PAIR_LEVEL_CAP {
                continue;
            }
            let Some(joint) = joint_frequencies(&paired) else {
                continue;
            };
            pairs.push(PairBaseline::Categorical {
                left: left.clone(),
                right: right.clone(),
                joint,
            });
        }
    }

    pairs
}

/// `(value, frequency)` sorted by value.
fn frequencies(samples: &[&Value]) -> Vec<(String, f64)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for value in samples {
        if value.is_null() {
            continue;
        }
        let key = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        *counts.entry(key).or_insert(0) += 1;
    }
    let total: usize = counts.values().sum();
    let mut values: Vec<(String, f64)> = counts
        .into_iter()
        .map(|(value, count)| (value, count as f64 / total as f64))
        .collect();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values
}

fn distinct_levels(paired: &[(String, String)]) -> (usize, usize) {
    let left: std::collections::HashSet<&String> = paired.iter().map(|(a, _)| a).collect();
    let right: std::collections::HashSet<&String> = paired.iter().map(|(_, b)| b).collect();
    (left.len(), right.len())
}

/// `(left, right, joint_frequency)` sorted by value pair.
fn joint_frequencies(paired: &[(String, String)]) -> Option<Vec<(String, String, f64)>> {
    if paired.is_empty() {
        return None;
    }
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for (a, b) in paired {
        *counts.entry((a.clone(), b.clone())).or_insert(0) += 1;
    }
    let total = paired.len() as f64;
    let mut joint: Vec<(String, String, f64)> = counts
        .into_iter()
        .map(|((a, b), count)| (a, b, count as f64 / total))
        .collect();
    joint.sort_by(|x, y| x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
    Some(joint)
}

fn numeric_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

/// Pearson correlation over complete pairs; `None` when there are fewer than
/// two complete pairs or either side is constant.
fn pearson_pairwise(paired: &[(Option<f64>, Option<f64>)]) -> Option<f64> {
    let complete: Vec<(f64, f64)> = paired
        .iter()
        .filter_map(|(a, b)| Some(((*a)?, (*b)?)))
        .collect();
    if complete.len() < 2 {
        return None;
    }
    let n = complete.len() as f64;
    let mean_a = complete.iter().map(|(a, _)| a).sum::<f64>() / n;
    let mean_b = complete.iter().map(|(_, b)| b).sum::<f64>() / n;
    let mut cov = 0.0;
    let mut var_a = 0.0;
    let mut var_b = 0.0;
    for (a, b) in &complete {
        cov += (a - mean_a) * (b - mean_b);
        var_a += (a - mean_a).powi(2);
        var_b += (b - mean_b).powi(2);
    }
    if var_a <= 0.0 || var_b <= 0.0 {
        return None;
    }
    Some(cov / (var_a * var_b).sqrt())
}

// ─── Scoring ────────────────────────────────────────────────────────────

/// Total variation distance between two frequency tables over the union of
/// their keys.
pub fn total_variation(left: &[(String, f64)], right: &[(String, f64)]) -> f64 {
    // BTreeMap, not HashMap: the sum must be bitwise reproducible, and
    // floating-point addition is order dependent (issue #67 AC5).
    let mut table: std::collections::BTreeMap<&str, (f64, f64)> = std::collections::BTreeMap::new();
    for (key, freq) in left {
        table.entry(key.as_str()).or_insert((0.0, 0.0)).0 += freq;
    }
    for (key, freq) in right {
        table.entry(key.as_str()).or_insert((0.0, 0.0)).1 += freq;
    }
    let sum: f64 = table.values().map(|(a, b)| (a - b).abs()).sum();
    0.5 * sum
}

/// `1 - KS` for a numeric column scored against its baseline quantile sketch.
pub fn numeric_shape_score(generated: &[f64], baseline: &EcdfParams) -> f64 {
    let ks = crate::synth::stats::ks_statistic(generated, baseline);
    (1.0 - ks).clamp(0.0, 1.0)
}

/// `1 - TV` for a categorical column scored against its baseline frequencies.
pub fn categorical_shape_score(generated: &[(String, f64)], baseline: &[(String, f64)]) -> f64 {
    (1.0 - total_variation(generated, baseline)).clamp(0.0, 1.0)
}

/// `1 - |Δpearson|`, floored at 0.
pub fn numeric_pair_score(generated_pearson: f64, baseline_pearson: f64) -> f64 {
    (1.0 - (generated_pearson - baseline_pearson).abs()).clamp(0.0, 1.0)
}

// ─── Report ─────────────────────────────────────────────────────────────

/// One report section: either scored, or honestly skipped with a reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Section<T> {
    Scored { score: f64, items: T },
    Skipped { reason: String },
}

impl<T> Section<T> {
    pub fn score(&self) -> Option<f64> {
        match self {
            Section::Scored { score, .. } => Some(*score),
            Section::Skipped { .. } => None,
        }
    }

    pub fn items(&self) -> Option<&T> {
        match self {
            Section::Scored { items, .. } => Some(items),
            Section::Skipped { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnScore {
    pub name: String,
    /// `numerical` | `categorical`
    pub kind: String,
    /// `1-ks` | `1-tv`
    pub metric: String,
    pub score: f64,
    /// Distinct baseline levels, for categorical columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub levels: Option<usize>,
    /// `false` when the score is shown for information but excluded from the
    /// section mean (high-cardinality categorical sampling noise).
    #[serde(default = "default_true")]
    pub counted: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairScore {
    pub left: String,
    pub right: String,
    /// `pearson-delta` | `joint-tv`
    pub metric: String,
    pub score: f64,
}

/// Join rate of one generated child column against its parent key pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FkRate {
    pub child_table: String,
    pub child_column: String,
    pub parent_table: String,
    pub parent_column: String,
    pub keys: usize,
    pub hits: usize,
    pub rate: f64,
    /// Where the parent key pool came from: `database` (live keys read with
    /// `--against-db`) or `generated` (the parent column of the generated
    /// data).
    pub source: String,
    pub warn: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableQuality {
    pub table: String,
    pub rows: usize,
    pub shapes: Section<Vec<ColumnScore>>,
    pub pairs: Section<Vec<PairScore>>,
    pub fk: Section<Vec<FkRate>>,
    pub score: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityReport {
    pub schema_version: u32,
    pub tables: Vec<TableQuality>,
    pub overall_score: Option<f64>,
}

impl QualityReport {
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize report: {}", e))?;
        std::fs::write(path, json).map_err(|e| format!("write report: {}", e))
    }

    pub fn summary(&self) -> String {
        let mut lines = vec!["Synthesis quality report".to_string()];
        for table in &self.tables {
            lines.push(format!("  {} ({} rows)", table.table, table.rows));
            match &table.shapes {
                Section::Scored { score, items } => {
                    lines.push(format!(
                        "    shapes: {:.3} ({} columns)",
                        score,
                        items.len()
                    ));
                    for column in items {
                        let note = if column.counted { "" } else { " (not counted)" };
                        lines.push(format!(
                            "      {} [{}] {} = {:.3}{}",
                            column.name, column.kind, column.metric, column.score, note
                        ));
                    }
                }
                Section::Skipped { reason } => {
                    lines.push(format!("    shapes: skipped ({reason})"))
                }
            }
            match &table.pairs {
                Section::Scored { score, items } => {
                    lines.push(format!("    pairs: {:.3} ({} pairs)", score, items.len()))
                }
                Section::Skipped { reason } => lines.push(format!("    pairs: skipped ({reason})")),
            }
            match &table.fk {
                Section::Scored { items, .. } => {
                    for rate in items {
                        lines.push(format!(
                            "    fk {}.{} -> {}.{}: {}/{} = {:.4}{} ({})",
                            rate.child_table,
                            rate.child_column,
                            rate.parent_table,
                            rate.parent_column,
                            rate.hits,
                            rate.keys,
                            rate.rate,
                            if rate.warn { " WARN" } else { "" },
                            rate.source
                        ));
                    }
                }
                Section::Skipped { reason } => lines.push(format!("    fk: skipped ({reason})")),
            }
            if let Some(score) = table.score {
                lines.push(format!("    table score: {:.3}", score));
            }
        }
        match self.overall_score {
            Some(score) => lines.push(format!("overall score: {:.3}", score)),
            None => lines.push("overall score: n/a".to_string()),
        }
        lines.join("\n")
    }
}

/// Mean of the scored sections of a table; `None` when every section was
/// skipped. FK rates are reported but not averaged in: they measure
/// referential integrity, not shape fidelity, and the offline pool is the
/// generated parent column, where a rate of 1.0 is expected by construction
/// (the real check is `--against-db`).
pub fn mean_of(values: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    let collected: Vec<f64> = values.into_iter().flatten().collect();
    if collected.is_empty() {
        None
    } else {
        Some(collected.iter().sum::<f64>() / collected.len() as f64)
    }
}

/// Score the generated columns of one table against its baseline.
pub fn evaluate_table(
    table: &str,
    model: &TableModel,
    baseline: Option<&BaselineSummary>,
    columns: &[String],
    rows: &[Vec<Value>],
    fk: Section<Vec<FkRate>>,
) -> TableQuality {
    let shapes = evaluate_shapes(model, baseline, columns, rows);
    let pairs = evaluate_pairs(baseline, columns, rows);
    let score = mean_of([shapes.score(), pairs.score()]);
    TableQuality {
        table: table.to_string(),
        rows: rows.len(),
        shapes,
        pairs,
        fk,
        score,
    }
}

fn first_index(columns: &[String], name: &str) -> Option<usize> {
    columns.iter().position(|c| c == name)
}

fn column_values(rows: &[Vec<Value>], col_idx: usize) -> Vec<Value> {
    rows.iter()
        .filter_map(|row| row.get(col_idx).cloned())
        .collect()
}

pub fn evaluate_shapes(
    model: &TableModel,
    baseline: Option<&BaselineSummary>,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Section<Vec<ColumnScore>> {
    let Some(baseline) = baseline else {
        return Section::Skipped {
            reason: "no report baseline; retrain with --holdout-ratio > 0".to_string(),
        };
    };
    let mut items = Vec::new();
    for name in &model.copula.column_order {
        let Some(col_idx) = first_index(columns, name) else {
            continue;
        };
        let Some(column_baseline) = baseline.columns.get(name) else {
            continue;
        };
        let values = column_values(rows, col_idx);
        let model_column = model.columns.get(name);
        match column_baseline {
            ColumnBaseline::Numerical { knots } => {
                let generated: Vec<f64> = values
                    .iter()
                    .filter_map(|value| match model_column {
                        Some(column) => crate::synth::marginal::numeric_axis_value(value, column),
                        None => numeric_value(value),
                    })
                    .collect();
                if generated.len() < 2 {
                    continue;
                }
                items.push(ColumnScore {
                    name: name.clone(),
                    kind: "numerical".to_string(),
                    metric: "1-ks".to_string(),
                    score: numeric_shape_score(
                        &generated,
                        &EcdfParams {
                            knots: knots.clone(),
                        },
                    ),
                    levels: None,
                    counted: true,
                });
            }
            ColumnBaseline::Categorical {
                values: base_values,
            } => {
                let references: Vec<&Value> = values.iter().collect();
                let generated = frequencies(&references);
                if generated.is_empty() {
                    continue;
                }
                items.push(ColumnScore {
                    name: name.clone(),
                    kind: "categorical".to_string(),
                    metric: "1-tv".to_string(),
                    score: categorical_shape_score(&generated, base_values),
                    levels: Some(base_values.len()),
                    counted: base_values.len() <= CATEGORICAL_SCORE_LEVEL_CAP,
                });
            }
        }
    }
    let counted: Vec<&ColumnScore> = items.iter().filter(|item| item.counted).collect();
    if counted.is_empty() {
        return Section::Skipped {
            reason: if items.is_empty() {
                "baseline has no column that is also present in the generated data".to_string()
            } else {
                "every scoreable column is high-cardinality categorical (displayed, not averaged)"
                    .to_string()
            },
        };
    }
    let score = counted.iter().map(|item| item.score).sum::<f64>() / counted.len() as f64;
    Section::Scored { score, items }
}

pub fn evaluate_pairs(
    baseline: Option<&BaselineSummary>,
    columns: &[String],
    rows: &[Vec<Value>],
) -> Section<Vec<PairScore>> {
    let Some(baseline) = baseline else {
        return Section::Skipped {
            reason: "no report baseline; retrain with --holdout-ratio > 0".to_string(),
        };
    };
    let mut items = Vec::new();
    for pair in &baseline.pairs {
        match pair {
            PairBaseline::Numerical {
                left,
                right,
                pearson,
            } => {
                let (Some(left_idx), Some(right_idx)) =
                    (first_index(columns, left), first_index(columns, right))
                else {
                    continue;
                };
                let paired: Vec<(Option<f64>, Option<f64>)> = rows
                    .iter()
                    .map(|row| {
                        (
                            row.get(left_idx).and_then(numeric_value),
                            row.get(right_idx).and_then(numeric_value),
                        )
                    })
                    .collect();
                let Some(generated) = pearson_pairwise(&paired) else {
                    continue;
                };
                items.push(PairScore {
                    left: left.clone(),
                    right: right.clone(),
                    metric: "pearson-delta".to_string(),
                    score: numeric_pair_score(generated, *pearson),
                });
            }
            PairBaseline::Categorical { left, right, joint } => {
                let (Some(left_idx), Some(right_idx)) =
                    (first_index(columns, left), first_index(columns, right))
                else {
                    continue;
                };
                let paired: Vec<(String, String)> = rows
                    .iter()
                    .filter_map(|row| {
                        let a = row.get(left_idx)?.as_str().map(str::to_string);
                        let b = row.get(right_idx)?.as_str().map(str::to_string);
                        Some((a?, b?))
                    })
                    .collect();
                let Some(generated) = joint_frequencies(&paired) else {
                    continue;
                };
                let baseline_table: Vec<(String, f64)> = joint
                    .iter()
                    .map(|(a, b, freq)| (format!("{a}\u{1}{b}"), *freq))
                    .collect();
                let generated_table: Vec<(String, f64)> = generated
                    .iter()
                    .map(|(a, b, freq)| (format!("{a}\u{1}{b}"), *freq))
                    .collect();
                items.push(PairScore {
                    left: left.clone(),
                    right: right.clone(),
                    metric: "joint-tv".to_string(),
                    score: categorical_shape_score(&generated_table, &baseline_table),
                });
            }
        }
    }
    if items.is_empty() {
        return Section::Skipped {
            reason: "baseline has no scoreable column pair".to_string(),
        };
    }
    let score = items.iter().map(|i| i.score).sum::<f64>() / items.len() as f64;
    Section::Scored { score, items }
}

// ─── Generated data input ───────────────────────────────────────────────

/// Read a table written by `synth generate` from `dir`. Tries `.jsonl`,
/// `.json` and `.csv` (in that order); returns `None` when the table has no
/// file there.
pub fn read_generated_table(dir: &Path, table: &str) -> Result<Option<GeneratedTable>, String> {
    let jsonl = dir.join(format!("{}.jsonl", table));
    if jsonl.is_file() {
        return read_jsonl(&jsonl).map(Some);
    }
    let json = dir.join(format!("{}.json", table));
    if json.is_file() {
        return read_json(&json).map(Some);
    }
    let csv = dir.join(format!("{}.csv", table));
    if csv.is_file() {
        return read_csv(&csv).map(Some);
    }
    Ok(None)
}

fn read_jsonl(path: &Path) -> Result<GeneratedTable, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut columns: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let object: serde_json::Map<String, Value> = serde_json::from_str(line)
            .map_err(|e| format!("parse {}:{}: {}", path.display(), line_no + 1, e))?;
        if columns.is_empty() {
            columns = object.keys().cloned().collect();
        }
        rows.push(
            columns
                .iter()
                .map(|name| object.get(name).cloned().unwrap_or(Value::Null))
                .collect(),
        );
    }
    Ok((columns, rows))
}

fn read_json(path: &Path) -> Result<GeneratedTable, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let objects: Vec<serde_json::Map<String, Value>> =
        serde_json::from_str(&content).map_err(|e| format!("parse {}: {}", path.display(), e))?;
    let mut columns: Vec<String> = Vec::new();
    if let Some(first) = objects.first() {
        columns = first.keys().cloned().collect();
    }
    let rows = objects
        .iter()
        .map(|object| {
            columns
                .iter()
                .map(|name| object.get(name).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    Ok((columns, rows))
}

/// CSV fields come back as strings (the exporter does not record types);
/// numeric parsing happens at scoring time. An empty unquoted field is NULL
/// while `""` is the empty string, which is why this is a small RFC 4180
/// reader over the whole file rather than `csv::Reader`: the crate cannot tell
/// those two apart, and quoted fields may contain the line break the record
/// splitter would otherwise treat as a row.
fn read_csv(path: &Path) -> Result<GeneratedTable, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let records = parse_csv_records(&content);
    let mut records = records.into_iter();
    let columns: Vec<String> = match records.next() {
        Some(header) => header
            .into_iter()
            .map(|field| field.unwrap_or_default())
            .collect(),
        None => return Ok((Vec::new(), Vec::new())),
    };
    let rows = records
        .map(|record| {
            record
                .into_iter()
                .map(|field| match field {
                    Some(text) => Value::String(text),
                    None => Value::Null,
                })
                .collect()
        })
        .collect();
    Ok((columns, rows))
}

/// Split CSV text into records of fields. `None` is an empty unquoted field
/// (NULL), `Some("")` a quoted empty string, and a trailing newline does not
/// produce an empty record.
fn parse_csv_records(content: &str) -> Vec<Vec<Option<String>>> {
    let chars: Vec<char> = content.chars().collect();
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut field_is_quoted = false;
    let mut in_quotes = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_quotes {
            if c == '"' {
                if chars.get(i + 1) == Some(&'"') {
                    field.push('"');
                    i += 2;
                    continue;
                }
                in_quotes = false;
                i += 1;
                continue;
            }
            field.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' if field.is_empty() => {
                in_quotes = true;
                field_is_quoted = true;
                i += 1;
            }
            ',' => {
                record.push(field_value(&field, field_is_quoted));
                field.clear();
                field_is_quoted = false;
                i += 1;
            }
            '\n' | '\r' => {
                // Consume CRLF as one break.
                if c == '\r' && chars.get(i + 1) == Some(&'\n') {
                    i += 1;
                }
                record.push(field_value(&field, field_is_quoted));
                records.push(std::mem::take(&mut record));
                field.clear();
                field_is_quoted = false;
                i += 1;
            }
            _ => {
                field.push(c);
                i += 1;
            }
        }
    }
    if !field.is_empty() || field_is_quoted || !record.is_empty() {
        record.push(field_value(&field, field_is_quoted));
        records.push(record);
    }
    records
}

fn field_value(field: &str, quoted: bool) -> Option<String> {
    if field.is_empty() && !quoted {
        None
    } else {
        Some(field.to_string())
    }
}

// ─── Foreign keys ───────────────────────────────────────────────────────

/// One `child.column -> parent.column` edge taken from a rules file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkRelation {
    pub child_table: String,
    pub child_column: String,
    pub parent_table: String,
    pub parent_column: String,
}

/// Extract FK edges from rules. References are `table.column`; a reference
/// without a `.` cannot be routed and is dropped.
pub fn fk_relations(rules: &crate::synth::rules::SynthRules) -> Vec<FkRelation> {
    let mut relations = Vec::new();
    for table in &rules.tables {
        for relationship in &table.relationships {
            let Some(reference) = relationship.references.first() else {
                continue;
            };
            let Some((parent_table, parent_column)) = reference.rsplit_once('.') else {
                continue;
            };
            relations.push(FkRelation {
                child_table: table.name.clone(),
                child_column: relationship.pk.clone(),
                parent_table: parent_table.to_string(),
                parent_column: parent_column.to_string(),
            });
        }
    }
    relations.sort_by(|a, b| {
        (
            &a.child_table,
            &a.child_column,
            &a.parent_table,
            &a.parent_column,
        )
            .cmp(&(
                &b.child_table,
                &b.child_column,
                &b.parent_table,
                &b.parent_column,
            ))
    });
    relations
}

/// Generated rows per table, keyed by table name.
pub type GeneratedTables = HashMap<String, Vec<Vec<Value>>>;

/// Column names of the generated rows, per table.
pub type GeneratedColumns = HashMap<String, Vec<String>>;

/// Columns plus rows of one generated table.
pub type GeneratedTable = (Vec<String>, Vec<Vec<Value>>);

/// Parent key pool per `(table, column)`.
pub type ParentKeyPools = HashMap<(String, String), Vec<String>>;

/// Join rate of every generated child column against its parent key pool.
///
/// `parent_pools` is built by the caller: the live database keys with
/// `--against-db`, otherwise [`generated_key_pools`] over the generated parent
/// table. An edge whose parent column is not in that pool is skipped, so the
/// caller's chosen source is visible in every reported rate.
pub fn evaluate_fk(
    relations: &[FkRelation],
    table_columns: &GeneratedColumns,
    generated: &GeneratedTables,
    parent_pools: &ParentKeyPools,
    source: &str,
) -> Section<Vec<FkRate>> {
    if relations.is_empty() {
        return Section::Skipped {
            reason: "no foreign keys (pass --rules with relationships)".to_string(),
        };
    }
    let mut items = Vec::new();
    for relation in relations {
        let Some(columns) = table_columns.get(&relation.child_table) else {
            continue;
        };
        let Some(rows) = generated.get(&relation.child_table) else {
            continue;
        };
        let Some(pool) = parent_pools.get(&(
            relation.parent_table.clone(),
            relation.parent_column.clone(),
        )) else {
            continue;
        };
        let Some(col_idx) = first_index(columns, &relation.child_column) else {
            continue;
        };
        let pool_set: std::collections::HashSet<&str> = pool.iter().map(String::as_str).collect();
        let keys: std::collections::HashSet<String> = rows
            .iter()
            .filter_map(|row| row.get(col_idx))
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string())
            })
            .collect();
        let hits = keys
            .iter()
            .filter(|key| pool_set.contains(key.as_str()))
            .count();
        let rate = if keys.is_empty() {
            0.0
        } else {
            hits as f64 / keys.len() as f64
        };
        items.push(FkRate {
            child_table: relation.child_table.clone(),
            child_column: relation.child_column.clone(),
            parent_table: relation.parent_table.clone(),
            parent_column: relation.parent_column.clone(),
            keys: keys.len(),
            hits,
            rate,
            source: source.to_string(),
            warn: keys.is_empty() || rate < 0.99,
        });
    }
    if items.is_empty() {
        return Section::Skipped {
            reason: "no scorable foreign key: include the parent table in --data, or pass \
                     --against-db for real keys"
                .to_string(),
        };
    }
    let score = items.iter().map(|i| i.rate).sum::<f64>() / items.len() as f64;
    Section::Scored { score, items }
}

/// Narrow a scored FK section to the leaves owned by one table, so the report
/// does not repeat the same relation under every table. A table with no FK
/// edge of its own gets an honest skip (not an empty score).
pub fn fk_section_for_table(section: &Section<Vec<FkRate>>, table: &str) -> Section<Vec<FkRate>> {
    match section {
        Section::Scored { items, .. } => {
            let owned: Vec<FkRate> = items
                .iter()
                .filter(|rate| rate.child_table == table)
                .cloned()
                .collect();
            if owned.is_empty() {
                return Section::Skipped {
                    reason: "no foreign key leaves this table".to_string(),
                };
            }
            let score = owned.iter().map(|rate| rate.rate).sum::<f64>() / owned.len() as f64;
            Section::Scored {
                score,
                items: owned,
            }
        }
        Section::Skipped { reason } => Section::Skipped {
            reason: reason.clone(),
        },
    }
}

/// Parent key pool taken from the *generated* parent table: the child must
/// reference keys its parent actually produced. A model's value dictionary is
/// not a substitute - after #66 a high-cardinality integer PK is a continuous
/// marginal with no dictionary, and generated-parent keys are what the
/// generated child should be checked against.
pub fn generated_key_pools(
    relations: &[FkRelation],
    table_columns: &GeneratedColumns,
    generated: &GeneratedTables,
) -> ParentKeyPools {
    let mut pools = HashMap::new();
    for relation in relations {
        let key = (
            relation.parent_table.clone(),
            relation.parent_column.clone(),
        );
        if pools.contains_key(&key) {
            continue;
        }
        let Some(columns) = table_columns.get(&relation.parent_table) else {
            continue;
        };
        let Some(rows) = generated.get(&relation.parent_table) else {
            continue;
        };
        let Some(col_idx) = first_index(columns, &relation.parent_column) else {
            continue;
        };
        let mut values: Vec<String> = rows
            .iter()
            .filter_map(|row| row.get(col_idx))
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string())
            })
            .collect();
        values.sort();
        values.dedup();
        pools.insert(key, values);
    }
    pools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::marginal::{CategoricalParams, NormalParams};

    fn model_of(columns: &[(&str, LogicalType, Marginal)]) -> TableModel {
        let map: HashMap<String, crate::synth::model::ColumnModel> = columns
            .iter()
            .map(|(name, logical_type, marginal)| {
                (
                    name.to_string(),
                    crate::synth::model::ColumnModel {
                        logical_type: logical_type.clone(),
                        rounding: None,
                        datetime_epoch: None,
                        decimal_scale: None,
                        datetime_format: None,
                        min: None,
                        max: None,
                        null_rate: None,
                        marginal: marginal.clone(),
                    },
                )
            })
            .collect();
        TableModel {
            version: 1,
            table: "t".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: crate::synth::model::Provenance {
                source: "native".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
            },
            pk: vec![],
            columns: map,
            copula: crate::synth::model::CopulaInfo {
                column_order: columns.iter().map(|(n, _, _)| n.to_string()).collect(),
                correlation: vec![],
            },
            fk_cardinality: Default::default(),
        }
    }

    fn numeric_column(name: &str) -> (&str, LogicalType, Marginal) {
        (
            name,
            LogicalType::Numerical,
            Marginal::Normal(NormalParams {
                loc: 0.0,
                scale: 1.0,
            }),
        )
    }

    fn categorical_column(name: &str) -> (&str, LogicalType, Marginal) {
        (
            name,
            LogicalType::Categorical,
            Marginal::Categorical(CategoricalParams {
                values: vec!["a".to_string(), "b".to_string()],
                weights: vec![0.5, 0.5],
            }),
        )
    }

    fn rows_from(columns: &[Vec<Value>]) -> Vec<Vec<Value>> {
        let n = columns[0].len();
        (0..n)
            .map(|i| columns.iter().map(|col| col[i].clone()).collect())
            .collect()
    }

    fn numeric_rows(values: &[f64]) -> Vec<Vec<Value>> {
        values.iter().map(|v| vec![Value::from(*v)]).collect()
    }

    #[test]
    fn holdout_indices_are_deterministic_and_capped() {
        assert!(holdout_indices(100, 0.0).is_empty());
        let first = holdout_indices(1000, 0.1);
        assert_eq!(first, holdout_indices(1000, 0.1), "must be reproducible");
        // Roughly the requested share, never all or none.
        assert!((60..=140).contains(&first.len()), "{}", first.len());
        assert!(first.iter().all(|&i| i < 1000));
        assert_eq!(holdout_indices(10, 1.0).len(), 10);
        assert_eq!(holdout_indices(1_000_000, 0.5).len(), HOLDOUT_MAX_ROWS);
    }

    #[test]
    fn holdout_does_not_alias_with_short_periodic_patterns() {
        // Rows cycling 5 levels: an index stride of 10 would put one single
        // level into every holdout row and describe it as the whole
        // distribution.
        let levels = ["a", "b", "c", "d", "e"];
        let rows: Vec<Vec<Value>> = (0..500).map(|i| vec![Value::from(levels[i % 5])]).collect();
        let model = model_of(&[categorical_column("kind")]);
        let baseline = build_baseline("t", &model, &rows, &[0], 0.1).unwrap();
        let ColumnBaseline::Categorical { values } = &baseline.columns["kind"] else {
            panic!("expected categorical baseline");
        };
        assert_eq!(
            values.len(),
            5,
            "every level must reach the holdout: {values:?}"
        );
        for (_, frequency) in values {
            assert!((*frequency - 0.2).abs() < 0.1, "skewed holdout: {values:?}");
        }
    }

    #[test]
    fn baseline_records_shapes_without_raw_rows() {
        let model = model_of(&[numeric_column("amount"), categorical_column("kind")]);
        let rows = rows_from(&[
            (0..200).map(|i| Value::from(i as f64)).collect(),
            // Period 3 (not 2): the holdout stride is 10, so a period-2
            // pattern would collapse onto a single parity.
            (0..200)
                .map(|i| Value::from(["a", "b", "c"][i % 3]))
                .collect(),
        ]);

        let baseline = build_baseline("t", &model, &rows, &[0, 1], 0.1).unwrap();
        assert!(
            (10..=40).contains(&baseline.holdout_rows),
            "{}",
            baseline.holdout_rows
        );
        match baseline.columns.get("amount") {
            Some(ColumnBaseline::Numerical { knots }) => {
                assert!(knots.len() > 1 && knots.len() <= crate::synth::marginal::ECDF_MAX_KNOTS);
                // Quantiles of the sampled values (0..200), endpoints included.
                assert!(knots[0] >= 0.0 && knots[0] < 40.0, "{knots:?}");
                assert!(knots[knots.len() - 1] <= 199.0 && knots[knots.len() - 1] > 150.0);
            }
            other => panic!("expected numerical baseline, got {other:?}"),
        }
        match baseline.columns.get("kind") {
            Some(ColumnBaseline::Categorical { values }) => {
                let total: f64 = values.iter().map(|(_, f)| f).sum();
                assert!((total - 1.0).abs() < 1e-12);
                assert_eq!(values.len(), 3);
            }
            other => panic!("expected categorical baseline, got {other:?}"),
        }

        // Privacy: the JSON carries aggregate summaries only, keyed exactly
        // like the struct, and never a row record.
        let json = serde_json::to_string(&baseline).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut keys: Vec<&str> = parsed
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        let allowed = [
            "columns",
            "holdout_rows",
            "pairs",
            "schema_version",
            "table",
        ];
        assert!(
            keys.iter().all(|key| allowed.contains(key)),
            "unexpected baseline keys: {keys:?}"
        );
        assert!(parsed.get("rows").is_none());
        let amount_keys: Vec<&str> = parsed["columns"]["amount"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(amount_keys, vec!["kind", "knots"]);
    }

    #[test]
    fn baseline_roundtrips_through_json() {
        let model = model_of(&[numeric_column("amount")]);
        let rows = numeric_rows(&(0..100).map(|i| i as f64).collect::<Vec<_>>());
        let baseline = build_baseline("t", &model, &rows, &[0], 0.2).unwrap();

        let dir = std::env::temp_dir().join("synth_quality_baseline");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.report-baseline.json");
        baseline.save(&path).unwrap();
        let loaded = BaselineSummary::load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(baseline, loaded);
    }

    #[test]
    fn shape_score_detects_injected_shift() {
        let model = model_of(&[numeric_column("amount")]);
        // Deterministic non-degenerate sample.
        let values: Vec<f64> = (0..2_000)
            .map(|i| ((i * 37) % 1000) as f64 / 10.0)
            .collect();
        let rows = numeric_rows(&values);
        let baseline = build_baseline("t", &model, &rows, &[0], 0.2).unwrap();
        let ColumnBaseline::Numerical { knots } = &baseline.columns["amount"] else {
            panic!("expected numerical baseline");
        };
        let reference = EcdfParams {
            knots: knots.clone(),
        };

        let good = numeric_shape_score(&values, &reference);
        let sigma = {
            let n = values.len() as f64;
            let mean = values.iter().sum::<f64>() / n;
            (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt()
        };
        let shifted: Vec<f64> = values.iter().map(|v| v + 3.0 * sigma).collect();
        let bad = numeric_shape_score(&shifted, &reference);

        assert!(good > 0.9, "clean sample should score high, got {good}");
        assert!(bad < 0.8, "shifted sample should score low, got {bad}");
        assert!(
            good - bad > 0.15,
            "degradation must be visible: good {good} bad {bad}"
        );
    }

    #[test]
    fn categorical_shape_score_detects_frequency_change() {
        let baseline = vec![("a".to_string(), 0.5), ("b".to_string(), 0.5)];
        let same = vec![("a".to_string(), 0.5), ("b".to_string(), 0.5)];
        let shuffled = vec![("a".to_string(), 0.9), ("b".to_string(), 0.1)];
        assert!((categorical_shape_score(&same, &baseline) - 1.0).abs() < 1e-12);
        let score = categorical_shape_score(&shuffled, &baseline);
        assert!(score < 0.8, "got {score}");
        // An unseen level shows up as an extra key in the union.
        let extra = vec![
            ("a".to_string(), 0.5),
            ("b".to_string(), 0.4),
            ("c".to_string(), 0.1),
        ];
        assert!(categorical_shape_score(&extra, &baseline) < 1.0);
    }

    #[test]
    fn evaluate_pairs_flags_decorrelation() {
        let model = model_of(&[numeric_column("a"), numeric_column("b")]);
        let values: Vec<f64> = (0..500).map(|i| i as f64).collect();
        let rows = rows_from(&[
            values.iter().map(|v| Value::from(*v)).collect(),
            values.iter().map(|v| Value::from(v * 2.0)).collect(),
        ]);
        let baseline = build_baseline("t", &model, &rows, &[0, 1], 0.2).unwrap();
        assert_eq!(baseline.pairs.len(), 1);

        let columns = vec!["a".to_string(), "b".to_string()];
        let scored = evaluate_pairs(Some(&baseline), &columns, &rows);
        let Section::Scored { score, items } = &scored else {
            panic!("expected scored pairs, got {scored:?}");
        };
        assert!(items[0].score > 0.99, "got {}", items[0].score);

        // Reverse one column: rank order breaks, correlation dives to ~-1.
        let reversed: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| {
                vec![
                    row[0].clone(),
                    Value::from(1000.0 - row[1].as_f64().unwrap()),
                ]
            })
            .collect();
        let degraded = evaluate_pairs(Some(&baseline), &columns, &reversed);
        let Section::Scored { items, .. } = degraded else {
            panic!("expected scored pairs");
        };
        assert!(items[0].score < 0.2, "got {}", items[0].score);
        assert!(*score > items[0].score);
    }

    #[test]
    fn sections_skip_honestly_without_a_baseline() {
        let model = model_of(&[numeric_column("amount")]);
        let rows = numeric_rows(&[1.0, 2.0, 3.0]);
        let shapes = evaluate_shapes(&model, None, &["amount".to_string()], &rows);
        match shapes {
            Section::Skipped { reason } => assert!(reason.contains("baseline"), "{reason}"),
            other => panic!("expected skipped, got {other:?}"),
        }
        let pairs = evaluate_pairs(None, &["amount".to_string()], &rows);
        assert!(matches!(pairs, Section::Skipped { .. }));
    }

    #[test]
    fn parse_csv_records_handles_quotes_nulls_and_embedded_newlines() {
        let records = parse_csv_records("1,\"a,b\",\"\",2\nx,\"say \"\"hi\"\"\",,\n");
        assert_eq!(
            records,
            vec![
                vec![
                    Some("1".to_string()),
                    Some("a,b".to_string()),
                    Some(String::new()),
                    Some("2".to_string())
                ],
                vec![
                    Some("x".to_string()),
                    Some(r#"say "hi""#.to_string()),
                    None,
                    None
                ],
            ]
        );

        // A newline inside a quoted field is part of the value, not a record
        // break (the exporter quotes such fields).
        let records = parse_csv_records("a,b\n\"line1\nline2\",2\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[1][0], Some("line1\nline2".to_string()));
        assert_eq!(records[1][1], Some("2".to_string()));

        // CRLF files and a missing trailing newline both parse to one record.
        assert_eq!(parse_csv_records("a,b\r\n1,2").len(), 2);
    }

    #[test]
    fn read_generated_table_prefers_jsonl_then_csv() {
        let dir = std::env::temp_dir().join("synth_quality_generated");
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join("a.jsonl"),
            "{\"x\":1,\"y\":\"p\"}\n{\"x\":2,\"y\":\"q\"}\n",
        )
        .unwrap();
        let (columns, rows) = read_generated_table(&dir, "a").unwrap().unwrap();
        assert!(columns.contains(&"x".to_string()));
        assert_eq!(rows.len(), 2);

        std::fs::write(dir.join("b.csv"), "x,y\n,\"\"\n3,r\n").unwrap();
        let (columns, rows) = read_generated_table(&dir, "b").unwrap().unwrap();
        assert_eq!(columns, vec!["x".to_string(), "y".to_string()]);
        assert!(rows[0][0].is_null(), "empty unquoted field is NULL");
        assert_eq!(rows[0][1], Value::String(String::new()));
        assert_eq!(rows[1][0], Value::String("3".to_string()));

        assert!(read_generated_table(&dir, "missing").unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fk_join_rate_flags_keys_missing_from_the_pool() {
        let relations = vec![FkRelation {
            child_table: "orders".to_string(),
            child_column: "user_id".to_string(),
            parent_table: "users".to_string(),
            parent_column: "id".to_string(),
        }];
        let table_columns = HashMap::from([("orders".to_string(), vec!["user_id".to_string()])]);
        let generated = HashMap::from([(
            "orders".to_string(),
            vec![
                vec![Value::from("1")],
                vec![Value::from("2")],
                vec![Value::from("9")],
            ],
        )]);
        let pools = HashMap::from([(
            ("users".to_string(), "id".to_string()),
            vec!["1".to_string(), "2".to_string(), "3".to_string()],
        )]);

        let section = evaluate_fk(&relations, &table_columns, &generated, &pools, "model");
        let Section::Scored { items, .. } = section else {
            panic!("expected scored fk section");
        };
        assert_eq!(items[0].keys, 3);
        assert_eq!(items[0].hits, 2);
        assert!((items[0].rate - 2.0 / 3.0).abs() < 1e-12);
        assert!(items[0].warn, "rate below 0.99 must warn");
        assert_eq!(items[0].source, "model");
    }

    #[test]
    fn fk_skips_without_relations_or_pool() {
        let empty = evaluate_fk(
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            "model",
        );
        match empty {
            Section::Skipped { reason } => assert!(reason.contains("foreign keys"), "{reason}"),
            other => panic!("expected skipped, got {other:?}"),
        }

        let relations = vec![FkRelation {
            child_table: "orders".to_string(),
            child_column: "user_id".to_string(),
            parent_table: "users".to_string(),
            parent_column: "id".to_string(),
        }];
        let section = evaluate_fk(
            &relations,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            "model",
        );
        assert!(matches!(section, Section::Skipped { .. }));
    }

    #[test]
    fn report_is_deterministic_for_the_same_inputs() {
        let model = model_of(&[numeric_column("a"), categorical_column("kind")]);
        let rows = rows_from(&[
            (0..300).map(|i| Value::from(i as f64)).collect(),
            (0..300)
                .map(|i| Value::from(if i % 3 == 0 { "a" } else { "b" }))
                .collect(),
        ]);
        let baseline = build_baseline("t", &model, &rows, &[0, 1], 0.1).unwrap();
        let columns = vec!["a".to_string(), "kind".to_string()];

        let build = || {
            let table = evaluate_table(
                "t",
                &model,
                Some(&baseline),
                &columns,
                &rows,
                Section::Skipped {
                    reason: "no foreign keys".to_string(),
                },
            );
            let tables = vec![table];
            let overall = mean_of(tables.iter().map(|t| t.score));
            QualityReport {
                schema_version: REPORT_SCHEMA_VERSION,
                tables,
                overall_score: overall,
            }
        };

        let first = build();
        let second = build();
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        assert!(first.overall_score.unwrap() > 0.9);
    }

    #[test]
    fn fk_section_only_shows_the_child_tables_own_edges() {
        let section = Section::Scored {
            score: 0.5,
            items: vec![
                FkRate {
                    child_table: "orders".to_string(),
                    child_column: "user_id".to_string(),
                    parent_table: "users".to_string(),
                    parent_column: "id".to_string(),
                    keys: 3,
                    hits: 2,
                    rate: 0.666,
                    source: "model".to_string(),
                    warn: true,
                },
                FkRate {
                    child_table: "users".to_string(),
                    child_column: "region_id".to_string(),
                    parent_table: "regions".to_string(),
                    parent_column: "id".to_string(),
                    keys: 2,
                    hits: 2,
                    rate: 1.0,
                    source: "model".to_string(),
                    warn: false,
                },
            ],
        };

        let orders = fk_section_for_table(&section, "orders");
        let Section::Scored { items, .. } = orders else {
            panic!("orders owns an edge");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].child_column, "user_id");

        // A table with no edge of its own is skipped, not scored as 0.
        let skipped = fk_section_for_table(&section, "regions");
        assert!(matches!(skipped, Section::Skipped { .. }));
    }

    #[test]
    fn high_cardinality_categorical_is_shown_but_not_averaged() {
        let model = model_of(&[numeric_column("amount"), categorical_column("kind")]);
        // 200 distinct values: TV between two multinomial samples over that
        // many levels is ~1 regardless of model quality.
        let values: Vec<Vec<Value>> = (0..2_000)
            .map(|i| vec![Value::from(i % 200), Value::from(format!("v{}", i % 200))])
            .collect();
        let baseline = build_baseline("t", &model, &values, &[0, 1], 0.2).unwrap();
        let columns = vec!["amount".to_string(), "kind".to_string()];

        let shapes = evaluate_shapes(&model, Some(&baseline), &columns, &values);
        let Section::Scored { score, items } = &shapes else {
            panic!("expected scored shapes, got {shapes:?}");
        };
        assert_eq!(items.len(), 2);
        let wide = items.iter().find(|i| i.name == "kind").unwrap();
        // The holdout sees most but not necessarily all 200 levels.
        assert!(wide.levels.unwrap() > CATEGORICAL_SCORE_LEVEL_CAP);
        assert!(
            !wide.counted,
            "200 levels must not drive the mean: {wide:?}"
        );
        // The mean is the numeric column alone.
        let narrow = items.iter().find(|i| i.name == "amount").unwrap();
        assert!((*score - narrow.score).abs() < 1e-12, "score {score}");

        // A dictionary column inside the cap is still counted.
        let rows: Vec<Vec<Value>> = (0..2_000)
            .map(|i| vec![Value::from(i as f64), Value::from(["a", "b", "c"][i % 3])])
            .collect();
        let baseline = build_baseline("t", &model, &rows, &[0, 1], 0.2).unwrap();
        let Section::Scored { items, .. } =
            evaluate_shapes(&model, Some(&baseline), &columns, &rows)
        else {
            panic!("expected scored shapes");
        };
        assert!(items.iter().all(|item| item.counted));
    }

    #[test]
    fn fk_pools_come_from_the_generated_parent_table() {
        let relations = vec![FkRelation {
            child_table: "orders".to_string(),
            child_column: "user_id".to_string(),
            parent_table: "users".to_string(),
            parent_column: "id".to_string(),
        }];
        let table_columns = HashMap::from([
            ("users".to_string(), vec!["id".to_string()]),
            ("orders".to_string(), vec!["user_id".to_string()]),
        ]);
        let generated = HashMap::from([
            (
                "users".to_string(),
                vec![
                    vec![Value::from(1)],
                    vec![Value::from(2)],
                    vec![Value::from(2)],
                ],
            ),
            (
                "orders".to_string(),
                vec![vec![Value::from(2)], vec![Value::from(9)]],
            ),
        ]);

        let pools = generated_key_pools(&relations, &table_columns, &generated);
        assert_eq!(
            pools.get(&("users".to_string(), "id".to_string())).unwrap(),
            &vec!["1".to_string(), "2".to_string()]
        );

        // Without the parent table in the generated data there is no pool, and
        // the edge is skipped with a reason that says what to do.
        let section = evaluate_fk(
            &relations,
            &table_columns,
            &HashMap::from([("orders".to_string(), generated["orders"].clone())]),
            &generated_key_pools(
                &relations,
                &table_columns,
                &HashMap::from([("orders".to_string(), generated["orders"].clone())]),
            ),
            "generated",
        );
        match section {
            Section::Skipped { reason } => assert!(reason.contains("--against-db"), "{reason}"),
            other => panic!("expected skipped, got {other:?}"),
        }
    }

    #[test]
    fn total_variation_is_bitwise_stable() {
        // Many keys: with a HashMap the summation order varies per call and
        // the score drifts in the last bits, which makes two runs of
        // `synth report` differ byte for byte.
        let left: Vec<(String, f64)> = (0..257).map(|i| (format!("k{i}"), 1.0 / 257.0)).collect();
        let right: Vec<(String, f64)> = (0..257).map(|i| (format!("k{i}"), 1.0 / 256.0)).collect();
        let first = total_variation(&left, &right);
        for _ in 0..8 {
            assert_eq!(
                first.to_bits(),
                total_variation(&left, &right).to_bits(),
                "total_variation must be reproducible"
            );
        }
    }
}
