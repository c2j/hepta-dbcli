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
    /// PII handling (issue #71). `auto` (default) uses the recognizer; `keep`
    /// disables anonymization for this column; `pii` forces it.
    #[serde(default, skip_serializing_if = "SdType::is_auto")]
    pub sdtype: SdType,
    /// Provider to force when `sdtype: pii` (`email` / `phone` / `name` /
    /// `id_card`); omitted means the recognizer's choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pii_provider: Option<String>,
    /// Emit each fake value at most once (aligned with the FK-pool semantics).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pii_unique: bool,
    /// Map an equal training value to an equal fake value (deterministic by
    /// level, so the equality structure survives without storing the value).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pii_stable_mapping: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SdType {
    #[default]
    Auto,
    Keep,
    Pii,
}

impl SdType {
    fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }
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
    /// How the child row count per parent key is produced (issue #72).
    /// `exact_rows` (default) keeps `--rows` as the literal child row count;
    /// `modeled` samples the child table's learned count distribution and
    /// derives the child row count from it.
    #[serde(default, skip_serializing_if = "CardinalityMode::is_exact_rows")]
    pub cardinality: CardinalityMode,
    /// Parsed for YAML compatibility with existing rules files. Generation
    /// does not read this field; NULL injection uses per-column `null_rate`.
    #[serde(default = "default_null_label")]
    pub null_label: String,
    /// Cross-table derivations (issue #117): each target column is a function
    /// of the referenced parent's columns (`parent.<col>`) plus local columns.
    /// Empty by default so existing rules files load unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derive: Vec<DeriveRule>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardinalityMode {
    #[default]
    ExactRows,
    Modeled,
}

impl CardinalityMode {
    fn is_exact_rows(&self) -> bool {
        matches!(self, Self::ExactRows)
    }
}

/// Pool sampling strategies for a relationship FK column. `Density` (issue
/// #89 S2b) weights the parent pool by the child table's own trained
/// marginal for the FK column, so FK value frequency tracks the child's
/// observed parent distribution instead of the pool's uniform spread.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolStrategy {
    Projection { unique: bool },
    Generated { unique: bool },
    Fixed { values: Vec<String> },
    Density,
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

                // PII overrides (issue #71): an explicit provider must be one
                // the generator knows, and `keep` cannot carry one.
                if let Some(provider) = rule.pii_provider.as_deref() {
                    if crate::synth::pii::PiiProvider::parse(provider).is_none() {
                        return Err(format!(
                            "table '{}' column '{}': unknown pii_provider '{}' \
                             (expected email, phone, name or id_card)",
                            table.name, column, provider
                        ));
                    }
                    if !matches!(rule.sdtype, SdType::Pii) {
                        return Err(format!(
                            "table '{}' column '{}': 'pii_provider' requires 'sdtype: pii'",
                            table.name, column
                        ));
                    }
                }
                if matches!(rule.sdtype, SdType::Keep)
                    && (rule.pii_unique || rule.pii_stable_mapping)
                {
                    return Err(format!(
                        "table '{}' column '{}': 'pii_unique'/'pii_stable_mapping' require \
                         anonymization (drop 'sdtype: keep')",
                        table.name, column
                    ));
                }
                if matches!(rule.sdtype, SdType::Pii)
                    && table.relationships.iter().any(|rel| &rel.pk == column)
                {
                    return Err(format!(
                        "table '{}' column '{}': 'sdtype: pii' on a relationship key would \
                         replace the foreign key with fake values and break referential \
                         integrity; drop the override or point the relationship elsewhere",
                        table.name, column
                    ));
                }

                // V1: `fixed` and `values` are mutually exclusive.
                if rule.fixed.is_some() && rule.values.is_some() {
                    return Err(format!(
                        "table '{}' column '{}': 'fixed' and 'values' are mutually exclusive",
                        table.name, column
                    ));
                }

                let has_fixed_or_values = rule.fixed.is_some() || rule.values.is_some();

                // V2: a pinned column cannot be combined with a non-zero null
                // rate. Stage 4 overwrites every row of the column, so the
                // rate is a no-op; `fixed_range` is included so the three
                // pinned fields agree instead of making ranges the one silent
                // case.
                if rule.has_column_override() {
                    if let Some(rate) = rule.null_rate {
                        if rate > 0.0 {
                            return Err(format!(
                                "table '{}' column '{}': 'fixed'/'values'/'fixed_range' cannot be combined with null_rate > 0 (got {})",
                                table.name, column, rate
                            ));
                        }
                    }
                }

                // V3: any pinned value on a parent key referenced by another
                // table cannot stay unique. `generate` already refuses all
                // three fields; validating it here too keeps the error at
                // load time and keeps both layers in agreement.
                let is_pinned = has_fixed_or_values || rule.fixed_range.is_some();
                if is_pinned && referenced.contains(&format!("{}.{}", table.name, column)) {
                    return Err(format!(
                        "table '{}' column '{}': 'fixed'/'values'/'fixed_range' on a parent key referenced by another table breaks uniqueness",
                        table.name, column
                    ));
                }

                // V4: a pinned value on a relationship pk (the FK child
                // column) would break referential integrity. Same three fields
                // as V3, matching what `generate` refuses.
                if is_pinned && table.relationships.iter().any(|r| &r.pk == column) {
                    return Err(format!(
                        "table '{}' column '{}': 'fixed'/'values'/'fixed_range' on a relationship pk would break referential integrity",
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
            validate_derive_rules(table, &referenced)?;
            validate_branches(table, &referenced)?;

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
fn validate_branches(
    table: &TableRule,
    parent_keys: &std::collections::HashSet<String>,
) -> Result<(), String> {
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

        // V11: the flippable-column whitelist (plan §1). Known columns and
        // known relationships make FK / derive-target / pinned writes
        // decidable without the model, so they fail here rather than after the
        // table has been generated. Unknown names can only be checked against
        // the model, which `generate` does.
        let derived: std::collections::HashSet<&str> = table
            .derive
            .iter()
            .map(|entry| entry.column.as_str())
            .collect();
        let fk_columns: std::collections::HashSet<&str> = table
            .relationships
            .iter()
            .map(|relationship| relationship.pk.as_str())
            .collect();

        for column in branch.repair.set.keys() {
            if parent_keys.contains(&format!("{}.{}", table.name, column)) {
                return Err(format!(
                    "table '{}' branch '{}': repair.set cannot write parent key '{}' referenced by another table (uniqueness is unreachable)",
                    table.name, branch.id, column
                ));
            }
            if fk_columns.contains(column.as_str()) {
                return Err(format!(
                    "table '{}' branch '{}': repair.set cannot write FK column '{}' (referential integrity)",
                    table.name, branch.id, column
                ));
            }
            if derived.contains(column.as_str()) {
                return Err(format!(
                    "table '{}' branch '{}': repair.set cannot write derived column '{}'",
                    table.name, branch.id, column
                ));
            }
            if table
                .columns
                .get(column)
                .is_some_and(ColumnRule::has_column_override)
            {
                return Err(format!(
                    "table '{}' branch '{}': repair.set cannot write pinned column '{}'",
                    table.name, branch.id, column
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
fn validate_derive_rules(
    table: &TableRule,
    parent_keys: &std::collections::HashSet<String>,
) -> Result<(), String> {
    // Issue #117: at most one relationship per table may carry derive rules
    // (the parent snapshot is single-sourced). Enforced here at load time to
    // match the generator's plan-time check (`DerivePlan::build`) so `train`
    // fails before any generation work, but this version names the offending
    // relationships instead of just the table.
    let derive_rels: Vec<&str> = table
        .relationships
        .iter()
        .filter(|rel| !rel.derive.is_empty())
        .map(|rel| rel.pk.as_str())
        .collect();
    if derive_rels.len() > 1 {
        return Err(format!(
            "table '{}': at most one relationship may carry derive rules, but {} and {} both do",
            table.name,
            derive_rels[0],
            derive_rels[1..].join(", ")
        ));
    }

    // Issue #117: relationship-level derive rules share the target namespace
    // and the topology with table-level derive, so everything below works on
    // one merged list. Whether a `parent.<col>` name actually exists is a
    // model question and is checked where the models are (`DerivePlan::build`).
    let mut rules: Vec<(&str, &str, crate::synth::expr::Expr)> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for derive in &table.derive {
        if !seen.insert(derive.column.as_str()) {
            return Err(format!(
                "table '{}': derive lists column '{}' more than once",
                table.name, derive.column
            ));
        }
        let expr = crate::synth::expr::Expr::parse(&derive.expr).map_err(|e| {
            format!(
                "table '{}' derive '{}': expression '{}' rejected: {}",
                table.name, derive.column, derive.expr, e
            )
        })?;
        // A table-level derive has no relationship to resolve `parent.` with.
        if expr
            .referenced_columns()
            .iter()
            .any(|name| name.contains('.'))
        {
            return Err(format!(
                "table '{}' derive '{}': 'parent.<col>' references are only allowed in a relationship's derive list",
                table.name, derive.column
            ));
        }
        rules.push((derive.column.as_str(), "", expr));
    }

    for rel in &table.relationships {
        let parent_tables: Vec<&str> = rel
            .references
            .iter()
            .filter_map(|reference| reference.split_once('.').map(|(t, _)| t))
            .collect();
        for derive in &rel.derive {
            if !seen.insert(derive.column.as_str()) {
                return Err(format!(
                    "table '{}': derive lists column '{}' more than once",
                    table.name, derive.column
                ));
            }
            let expr = crate::synth::expr::Expr::parse(&derive.expr).map_err(|e| {
                format!(
                    "table '{}' relationship '{}': derive '{}': expression '{}' rejected: {}",
                    table.name, rel.pk, derive.column, derive.expr, e
                )
            })?;
            for name in expr.referenced_columns() {
                // The grammar allows exactly one qualified form, `parent.<col>`;
                // it must resolve against a table this relationship references.
                // Column existence is checked against the parent model at
                // generation time (`DerivePlan::build`).
                if name.starts_with("parent.") && parent_tables.is_empty() {
                    return Err(format!(
                        "table '{}' relationship '{}': derive '{}' references '{}' but the relationship has no references",
                        table.name, rel.pk, derive.column, name
                    ));
                }
            }
            rules.push((derive.column.as_str(), rel.pk.as_str(), expr));
        }
    }

    let mut referenced: std::collections::BTreeMap<&str, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();

    for (column, rel_pk, expr) in &rules {
        if let Some(column_rule) = table.columns.get(*column) {
            if column_rule.has_column_override() {
                return Err(format!(
                    "table '{}' column '{}': a derived column cannot also use 'fixed'/'values'/'fixed_range'",
                    table.name, column
                ));
            }
        }

        if table.relationships.iter().any(|rel| rel.pk == *column) {
            return Err(format!(
                "table '{}' column '{}': a derived column cannot be a relationship pk (referential integrity)",
                table.name, column
            ));
        }

        if parent_keys.contains(&format!("{}.{}", table.name, column)) {
            return Err(format!(
                "table '{}' column '{}': a derived column cannot be a parent key referenced by another table (its uniqueness is enforced before the derive phase, so the derived values could repeat)",
                table.name, column
            ));
        }

        let _ = rel_pk;
        referenced.insert(column, expr.referenced_columns());
    }

    // Cycle detection over derive columns only: an edge target -> referenced
    // means "target depends on referenced". Cross-table (`parent.`) names are
    // not derive targets, so they count as ready inputs.
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
    fn should_reject_branch_repair_that_writes_a_fk_column() {
        // `generate` refuses this; the FK pk is already known from the YAML, so
        // it must fail at load time instead of after the whole table is built.
        let yaml = r#"
version: "1"
tables:
  - name: orders
    relationships:
      - pk: user_id
        references: [users.id]
    branches:
      - id: paid
        predicate: "status == 'A'"
        target_ratio: 0.3
        repair: {set: {user_id: "1"}}
  - name: users
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("writing an FK column must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("user_id"), "error must name the column: {err}");
        assert!(
            err.contains("referential integrity"),
            "error must explain why: {err}"
        );
    }

    #[test]
    fn should_reject_branch_repair_that_writes_a_derived_column() {
        // A derived column is recomputed after `set`, so the write can never
        // move the predicate. That is decidable from the YAML alone.
        let yaml = r#"
version: "1"
tables:
  - name: orders
    derive:
      - column: total
        expr: "price * qty"
    branches:
      - id: paid
        predicate: "status == 'A'"
        target_ratio: 0.3
        repair: {set: {total: "1"}}
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("writing a derived column must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("total"), "error must name the column: {err}");
        assert!(err.contains("derived"), "error must explain why: {err}");
    }

    #[test]
    fn should_reject_branch_repair_that_writes_a_pinned_column() {
        // Stage 4 pins the column for every row, so a repair write to it is a
        // priority conflict that the YAML already reveals.
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      status:
        fixed: "A"
    branches:
      - id: paid
        predicate: "status == 'A'"
        target_ratio: 0.3
        repair: {set: {status: "B"}}
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("writing a pinned column must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("status"), "error must name the column: {err}");
        assert!(err.contains("pinned"), "error must explain why: {err}");
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

    // #94 decision (user-approved): unknown function names are still
    // rejected fail-fast at load time; the error text changed from the
    // generic "function call" category to naming the function and listing
    // the known whitelist. Table and column attribution is unchanged.
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
            err.contains("`min`") && err.contains("not permitted"),
            "error must name the offending function: {err}"
        );
        assert!(
            err.contains("known functions"),
            "error must list the known functions: {err}"
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
    fn should_reject_fixed_range_on_a_parent_key_referenced_by_another_table() {
        // `generate` refuses all three pinning fields on a referenced parent
        // key; validation must not be laxer than the generator.
        let yaml = r#"
version: "1"
tables:
  - name: parent
    columns:
      id:
        fixed_range: [1, 2]
    relationships: []
  - name: child
    relationships:
      - pk: fk
        references: [parent.id]
"#;
        let err = validate_yaml(yaml).expect_err("pinning a parent key must fail");
        assert!(err.contains("id"), "error must name the column: {err}");
        assert!(
            err.contains("uniqueness"),
            "error must explain the reason: {err}"
        );
    }

    #[test]
    fn should_reject_derive_on_a_parent_key_referenced_by_another_table() {
        // A derived parent key loses the uniqueness guarantee the generator
        // enforces before the derive phase, so an FK-enforced load breaks.
        let yaml = r#"
version: "1"
tables:
  - name: parent
    derive:
      - column: id
        expr: "other * 2"
    relationships: []
  - name: child
    relationships:
      - pk: fk
        references: [parent.id]
"#;
        let err = validate_yaml(yaml).expect_err("deriving a parent key must fail");
        assert!(err.contains("parent"), "error must name the table: {err}");
        assert!(err.contains("id"), "error must name the column: {err}");
    }

    #[test]
    fn should_reject_branch_repair_on_a_parent_key_referenced_by_another_table() {
        let yaml = r#"
version: "1"
tables:
  - name: parent
    branches:
      - id: pin
        predicate: "other > 1"
        target_ratio: 0.8
        repair: {set: {id: "1"}}
    relationships: []
  - name: child
    relationships:
      - pk: fk
        references: [parent.id]
"#;
        let err = validate_yaml(yaml).expect_err("repairing a parent key must fail");
        assert!(err.contains("id"), "error must name the column: {err}");
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

    // ─── relationship derive (#117) ──────────────────────────────────────

    #[test]
    fn should_accept_relationship_derive_referencing_the_declared_parent() {
        let yaml = r#"
version: "1"
tables:
  - name: par
    relationships: []
  - name: zgh
    relationships:
      - pk: fk
        references: [par.id]
        derive:
          - column: vol
            expr: "parent.cjsl / 1000"
"#;
        validate_yaml(yaml).expect("parent reference through the declared relationship is valid");
    }

    #[test]
    fn should_reject_relationship_derive_using_parent_without_references() {
        // `parent.` can only be resolved through the relationship's own
        // references; without them the snapshot has no source.
        let yaml = r#"
version: "1"
tables:
  - name: par
    relationships: []
  - name: zgh
    relationships:
      - pk: fk
        references: []
        derive:
          - column: vol
            expr: "parent.cjsl / 1000"
"#;
        let err = validate_yaml(yaml).expect_err("parent reference without a parent must fail");
        assert!(err.contains("zgh"), "error must name the table: {err}");
        assert!(
            err.contains("parent.cjsl"),
            "error must name the reference: {err}"
        );
    }

    // An unknown `parent.<col>` name is a model question (rules carry no
    // parent column list) and is rejected by `DerivePlan::build` at generation
    // time; see the generator tests.

    // Issue #117 constraint (UserGuide: at most one relationship per table
    // may carry derive rules). The rejection belongs at load time, together
    // with every other derive conflict, so `train` fails before burning a
    // generation run on rules it can never execute.
    #[test]
    fn should_reject_two_relationships_carrying_derive_at_load_time() {
        let yaml = r#"
version: "1"
tables:
  - name: buyer
    relationships: []
  - name: seller
    relationships: []
  - name: orders
    relationships:
      - pk: buyer_fk
        references: [buyer.id]
        derive:
          - column: buyer_note
            expr: "parent.id + 1"
      - pk: seller_fk
        references: [seller.id]
        derive:
          - column: seller_note
            expr: "parent.id + 2"
"#;
        let err = validate_yaml(yaml)
            .expect_err("two relationships carrying derive must fail at load time");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(
            err.contains("buyer_fk") && err.contains("seller_fk"),
            "error must name both relationships: {err}"
        );
    }

    #[test]
    fn should_reject_relationship_derive_targeting_the_fk_column() {
        let yaml = r#"
version: "1"
tables:
  - name: par
    relationships: []
  - name: zgh
    relationships:
      - pk: fk
        references: [par.id]
        derive:
          - column: fk
            expr: "parent.id / 2"
"#;
        let err = validate_yaml(yaml).expect_err("deriving the FK column must fail");
        assert!(err.contains("fk"), "error must name the FK column: {err}");
        assert!(
            err.contains("referential integrity") || err.contains("relationship pk"),
            "error must explain the reason: {err}"
        );
    }

    #[test]
    fn should_reject_relationship_derive_on_a_fixed_column() {
        let yaml = r#"
version: "1"
tables:
  - name: par
    relationships: []
  - name: zgh
    columns:
      vol:
        fixed: "1"
    relationships:
      - pk: fk
        references: [par.id]
        derive:
          - column: vol
            expr: "parent.id + 1"
"#;
        let err = validate_yaml(yaml).expect_err("fixed + relationship derive must conflict");
        assert!(err.contains("vol"), "error must name the column: {err}");
        assert!(err.contains("fixed"), "error must explain the clash: {err}");
    }

    #[test]
    fn should_accept_relationship_derive_feeding_local_derive() {
        // Cross-table inputs count as ready; `vol2` (parent snapshot) then
        // `vol` (local) resolves in one topology, not a cycle.
        let yaml = r#"
version: "1"
tables:
  - name: par
    relationships: []
  - name: zgh
    derive:
      - column: vol
        expr: "vol2 + 1"
    relationships:
      - pk: fk
        references: [par.id]
        derive:
          - column: vol2
            expr: "parent.id + 1"
"#;
        validate_yaml(yaml).expect("cross -> local chain is acyclic");
    }

    #[test]
    fn should_reject_cycle_between_relationship_and_local_derive() {
        let yaml = r#"
version: "1"
tables:
  - name: par
    relationships: []
  - name: zgh
    derive:
      - column: vol
        expr: "vol2 + 1"
    relationships:
      - pk: fk
        references: [par.id]
        derive:
          - column: vol2
            expr: "vol * 3"
"#;
        let err =
            validate_yaml(yaml).expect_err("cycle across table and relationship derive must fail");
        assert!(err.contains("cycle"), "error must say cycle: {err}");
    }

    #[test]
    fn should_accept_local_derive_mixing_parent_reference_in_relationship_derive() {
        // Relationship derive may also read local columns; mixed expressions
        // stay within one topology with the table-level derive list.
        let yaml = r#"
version: "1"
tables:
  - name: par
    relationships: []
  - name: zgh
    derive:
      - column: half
        expr: "vol * 2"
    relationships:
      - pk: fk
        references: [par.id]
        derive:
          - column: vol
            expr: "parent.cjsl / 1000"
"#;
        validate_yaml(yaml).expect("mixed local+cross derive chain is valid");
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
                    cardinality: Default::default(),
                    derive: Vec::new(),
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
    fn density_pool_strategy_parses() {
        // Issue #89 S2b: `!density` weights the parent pool by the child's
        // own trained marginal for the FK column, so the generated FK value
        // distribution matches what the child observed in training instead
        // of a uniform draw over the parent pool.
        let yaml = r#"
version: "1"
tables:
  - name: t
    relationships:
      - pk: id
        references: [other.id]
        pool_strategy: !density
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        assert!(
            matches!(
                rules.tables[0].relationships[0].pool_strategy,
                PoolStrategy::Density
            ),
            "expected Density pool strategy, got {:?}",
            rules.tables[0].relationships[0].pool_strategy
        );
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
    fn should_reject_fixed_range_with_nonzero_null_rate() {
        // V2 must cover `fixed_range` too: stage 4 overwrites the whole column,
        // so a `null_rate` next to any pinned field is a no-op. Rejecting only
        // `fixed`/`values` made the identical mistake silent for ranges.
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      amount:
        fixed_range: [1, 10]
        null_rate: 0.2
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("fixed_range + null_rate > 0 must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("amount"), "error must name the column: {err}");
        assert!(
            err.contains("null_rate"),
            "error must explain the conflict: {err}"
        );

        // The boundary stays open: a zero rate is not a conflict.
        let ok = r#"
version: "1"
tables:
  - name: orders
    columns:
      amount:
        fixed_range: [1, 10]
        null_rate: 0.0
    relationships: []
"#;
        validate_yaml(ok).expect("fixed_range + null_rate == 0 must stay valid");
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
    fn should_reject_fixed_range_on_relationship_pk() {
        // V4 has the same asymmetry V2 had: `generate` refuses all three pinned
        // fields on an FK child column, so `fixed_range` must not be the one
        // that only fails after the whole table has been generated.
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      user_id:
        fixed_range: [1, 10]
    relationships:
      - pk: user_id
        references: [users.id]
  - name: users
    relationships: []
"#;
        let err = validate_yaml(yaml).expect_err("fixed_range on a relationship pk must fail");
        assert!(err.contains("orders"), "error must name the table: {err}");
        assert!(err.contains("user_id"), "error must name the column: {err}");
        assert!(
            err.contains("referential integrity"),
            "error must explain why: {err}"
        );
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
    fn should_read_unquoted_numeric_uniform_value_pool_entries() {
        // Mirrors `should_read_unquoted_numeric_value_pool_keys`: the weighted
        // shape already accepts `{1: 0.7}`, so `[1, 2]` must not force quotes.
        let yaml = r#"
version: "1"
tables:
  - name: orders
    columns:
      part_id:
        values: [1, 2]
    relationships: []
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        match rules.tables[0].columns["part_id"].values.as_ref().unwrap() {
            ValuePool::Uniform(values) => {
                assert_eq!(values, &vec!["1".to_string(), "2".to_string()])
            }
            other => panic!("expected a uniform pool, got {other:?}"),
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

    #[test]
    fn should_default_cardinality_to_exact_rows() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    relationships:
      - pk: user_id
        references: [users.id]
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            rules.tables[0].relationships[0].cardinality,
            CardinalityMode::ExactRows
        );
        // The default must not be serialized back, so legacy rules files stay
        // byte-identical after a load/save round-trip.
        let serialized = serde_yaml::to_string(&rules).unwrap();
        assert!(!serialized.contains("cardinality"));
    }

    #[test]
    fn should_parse_modeled_cardinality() {
        let yaml = r#"
version: "1"
tables:
  - name: orders
    relationships:
      - pk: user_id
        references: [users.id]
        cardinality: modeled
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            rules.tables[0].relationships[0].cardinality,
            CardinalityMode::Modeled
        );
    }
    #[test]
    fn should_parse_sdtype_keep_and_pii() {
        let yaml = r#"
version: "1"
tables:
  - name: users
    relationships: []
    columns:
      email:
        sdtype: keep
      mobile:
        sdtype: pii
        pii_provider: phone
        pii_unique: true
        pii_stable_mapping: true
"#;
        let rules: SynthRules = serde_yaml::from_str(yaml).unwrap();
        let columns = &rules.tables[0].columns;
        assert_eq!(columns["email"].sdtype, SdType::Keep);
        assert_eq!(columns["mobile"].sdtype, SdType::Pii);
        assert_eq!(columns["mobile"].pii_provider.as_deref(), Some("phone"));
        assert!(columns["mobile"].pii_unique);
        assert!(columns["mobile"].pii_stable_mapping);

        // Defaults are not serialized, so old rules files round-trip byte
        // identically.
        let plain = SynthRules {
            version: "1".to_string(),
            tables: vec![TableRule {
                name: "t".to_string(),
                columns: HashMap::from([("amount".to_string(), ColumnRule::default())]),
                derive: vec![],
                branches: vec![],
                rows: None,
                relationships: vec![],
                strategy: TableStrategy::Uniform,
            }],
        };
        let text = serde_yaml::to_string(&plain).unwrap();
        assert!(!text.contains("sdtype"), "{text}");
        assert!(!text.contains("pii_"), "{text}");
    }

    #[test]
    fn should_reject_invalid_pii_overrides() {
        let mut rule = TableRule {
            name: "t".to_string(),
            columns: HashMap::from([(
                "mobile".to_string(),
                ColumnRule {
                    sdtype: SdType::Pii,
                    pii_provider: Some("nope".to_string()),
                    ..Default::default()
                },
            )]),
            derive: vec![],
            branches: vec![],
            rows: None,
            relationships: vec![],
            strategy: TableStrategy::Uniform,
        };
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule.clone()],
        };
        assert!(rules
            .validate()
            .unwrap_err()
            .contains("unknown pii_provider"));

        rule.columns.get_mut("mobile").unwrap().pii_provider = Some("phone".to_string());
        rule.columns.get_mut("mobile").unwrap().sdtype = SdType::Auto;
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule.clone()],
        };
        assert!(rules
            .validate()
            .unwrap_err()
            .contains("requires 'sdtype: pii'"));

        rule.columns.get_mut("mobile").unwrap().sdtype = SdType::Keep;
        rule.columns.get_mut("mobile").unwrap().pii_provider = None;
        rule.columns.get_mut("mobile").unwrap().pii_unique = true;
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };
        assert!(rules.validate().unwrap_err().contains("require"));
    }
    #[test]
    fn should_reject_pii_on_a_relationship_key() {
        let rule = TableRule {
            name: "orders".to_string(),
            columns: HashMap::from([(
                "user_id".to_string(),
                ColumnRule {
                    sdtype: SdType::Pii,
                    ..Default::default()
                },
            )]),
            derive: vec![],
            branches: vec![],
            rows: None,
            relationships: vec![Relationship {
                pk: "user_id".to_string(),
                references: vec!["users.id".to_string()],
                pool_strategy: PoolStrategy::Projection { unique: false },
                cardinality: CardinalityMode::ExactRows,
                null_label: "null".to_string(),
                derive: Vec::new(),
            }],
            strategy: TableStrategy::Uniform,
        };
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![rule],
        };
        assert!(rules
            .validate()
            .unwrap_err()
            .contains("referential integrity"));
    }
}
