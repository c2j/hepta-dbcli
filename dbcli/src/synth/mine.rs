//! Conditional business-rule mining for `rules-draft` (issue #69).
//!
//! Scans categorical column pairs for observable `A=a => B=b` dependencies and
//! reports them as candidates. Candidates are **never enabled automatically**:
//! they are rendered as YAML comment lines (or written to a standalone list via
//! `--emit-candidates`) and a human decides what to copy into `rules:`.
//!
//! Hard constraint (issue #69, non-functional): mining must not enable any
//! rule. Everything this module produces is inert text.
//!
//! The core is pure: given sampled rows plus a [`MineConfig`] it returns a
//! deterministic [`MineReport`] (same rows, same config -> byte-identical
//! candidate list). Sampling and I/O live in `synth/mod.rs`.

use serde_json::Value;
use std::collections::BTreeMap;

/// Distinct-level cap for a column to take part in mining. Above this the
/// column is treated as high-cardinality and skipped (mirrors the profile
/// `top_values` cap, so low-cardinality numerics participate and ids do not).
pub const DEFAULT_MAX_LEVELS: usize = 50;

/// One `A=a => B=b` candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Antecedent column.
    pub column: String,
    /// Antecedent value, rendered for display (`'1'` for strings).
    pub value: String,
    /// Consequent column.
    pub implies_column: String,
    /// Consequent value, rendered for display (`'0'` for strings).
    pub implies_value: String,
    /// `P(B=b | A=a)`.
    pub confidence: f64,
    /// `P(A=a)` over the rows where both columns are non-NULL.
    pub support: f64,
}

/// Thresholds for one mining run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MineConfig {
    /// Minimum `P(B=b | A=a)` for a candidate.
    pub confidence: f64,
    /// Minimum antecedent support. Doubles as the minimum total-variation
    /// distance between the conditional and the marginal distribution: a rule
    /// must move the distribution by at least this much to be reported.
    pub support: f64,
    /// Cap on directed column pairs scanned per table (`c*(c-1)`).
    pub max_pairs: usize,
    /// Distinct-level cap for a column to take part.
    pub max_levels: usize,
}

impl Default for MineConfig {
    fn default() -> Self {
        Self {
            confidence: 0.95,
            support: 0.05,
            max_pairs: 2000,
            max_levels: DEFAULT_MAX_LEVELS,
        }
    }
}

/// Result of mining one table.
#[derive(Debug, Clone, Default)]
pub struct MineReport {
    pub candidates: Vec<Candidate>,
    pub participating_columns: usize,
    pub pairs_total: usize,
    pub pairs_considered: usize,
}

impl MineReport {
    /// True when `max_pairs` cut the scan short.
    pub fn pairs_truncated(&self) -> bool {
        self.pairs_considered < self.pairs_total
    }
}

/// Candidates for one table, tagged with the table name for rendering.
#[derive(Debug, Clone)]
pub struct TableCandidates {
    pub table: String,
    pub candidates: Vec<Candidate>,
}

/// Sentinel level index for a NULL (or missing) cell. NULL never matches a
/// rule condition, so these rows take part in neither distributions.
const NULL_LEVEL: usize = usize::MAX;

const CANDIDATE_HEADER: &str = "# Mined conditional rule candidates (issue #69) - NOT enabled. \
Review, then copy the ones you want into the `rules:` section by hand:";

struct Levels {
    /// Display form per level, index = level index.
    display: Vec<String>,
    /// Level index per row (`NULL_LEVEL` when the cell is NULL/absent).
    row_levels: Vec<usize>,
}

fn display_value(v: &Value) -> String {
    match v {
        Value::String(s) => format!("'{s}'"),
        other => other.to_string(),
    }
}

/// Level-index a column. `None` when the column has fewer than two levels or
/// more than `max_levels` (high cardinality -> excluded, per issue #69).
fn build_levels(rows: &[Vec<Value>], column: usize, max_levels: usize) -> Option<Levels> {
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    let mut display = Vec::new();
    let mut row_levels = Vec::with_capacity(rows.len());

    for row in rows {
        let level = match row.get(column) {
            None | Some(Value::Null) => NULL_LEVEL,
            Some(value) => {
                let key = value.to_string();
                match index.get(&key) {
                    Some(&i) => i,
                    None => {
                        if display.len() >= max_levels {
                            return None;
                        }
                        let i = display.len();
                        display.push(display_value(value));
                        index.insert(key, i);
                        i
                    }
                }
            }
        };
        row_levels.push(level);
    }

    if display.len() < 2 {
        return None;
    }

    // Identity guard (issue #69, "高基数数值不做（误报率高）"): on a small
    // sample a unique-ish key stays under `max_levels` and then "proves" a
    // rule for every one of its values (`id=1 => note='a'`), because a key
    // maps to exactly one row. A column whose distinct values cover more than
    // half of its non-NULL rows behaves like an identifier, so it is excluded.
    let non_null = row_levels.iter().filter(|l| **l != NULL_LEVEL).count();
    if display.len().saturating_mul(2) > non_null {
        return None;
    }

    Some(Levels {
        display,
        row_levels,
    })
}

/// Mine `A=a => B=b` candidates from sampled rows.
///
/// `columns` names the columns of every row vector; columns whose sampled
/// cardinality is outside `1 < levels <= max_levels` are skipped.
pub fn mine_candidates(columns: &[String], rows: &[Vec<Value>], config: &MineConfig) -> MineReport {
    let participating: Vec<(usize, Levels)> = (0..columns.len())
        .filter_map(|i| build_levels(rows, i, config.max_levels).map(|levels| (i, levels)))
        .collect();
    let p = participating.len();
    let pairs_total = p.saturating_mul(p.saturating_sub(1));
    let pairs_considered = pairs_total.min(config.max_pairs);

    let mut candidates = Vec::new();
    let mut considered = 0usize;
    'pairs: for a in 0..p {
        for b in 0..p {
            if a == b {
                continue;
            }
            if considered >= pairs_considered {
                break 'pairs;
            }
            considered += 1;
            scan_pair(
                &columns[participating[a].0],
                &participating[a].1,
                &columns[participating[b].0],
                &participating[b].1,
                config,
                &mut candidates,
            );
        }
    }

    candidates.sort_by(|x, y| {
        (&x.column, &x.value, &x.implies_column, &x.implies_value).cmp(&(
            &y.column,
            &y.value,
            &y.implies_column,
            &y.implies_value,
        ))
    });

    MineReport {
        candidates,
        participating_columns: p,
        pairs_total,
        pairs_considered,
    }
}

fn scan_pair(
    a_name: &str,
    a: &Levels,
    b_name: &str,
    b: &Levels,
    config: &MineConfig,
    out: &mut Vec<Candidate>,
) {
    let b_len = b.display.len();
    let mut joint = vec![0u32; a.display.len() * b_len];
    let mut denom: u64 = 0;

    for row in 0..a.row_levels.len() {
        let la = a.row_levels[row];
        if la == NULL_LEVEL {
            continue;
        }
        let lb = b.row_levels[row];
        if lb == NULL_LEVEL {
            continue;
        }
        joint[la * b_len + lb] += 1;
        denom += 1;
    }
    if denom == 0 {
        return;
    }
    let total = denom as f64;

    let mut marginal = vec![0f64; b_len];
    for la in 0..a.display.len() {
        for lb in 0..b_len {
            marginal[lb] += joint[la * b_len + lb] as f64;
        }
    }
    for m in marginal.iter_mut() {
        *m /= total;
    }

    for la in 0..a.display.len() {
        let a_count: u32 = (0..b_len).map(|lb| joint[la * b_len + lb]).sum();
        if a_count == 0 {
            continue;
        }
        let support = a_count as f64 / total;
        if support < config.support {
            continue;
        }

        let mut tv = 0.0;
        let mut best_value = 0usize;
        let mut best_cond = -1.0f64;
        for lb in 0..b_len {
            let cond = joint[la * b_len + lb] as f64 / a_count as f64;
            tv += (cond - marginal[lb]).abs();
            if cond > best_cond {
                best_cond = cond;
                best_value = lb;
            }
        }
        tv *= 0.5;

        // The conditional distribution must depart from the marginal by at
        // least the support threshold, and the dominant consequent value must
        // clear the confidence threshold.
        if tv <= config.support || best_cond < config.confidence {
            continue;
        }

        out.push(Candidate {
            column: a_name.to_string(),
            value: a.display[la].clone(),
            implies_column: b_name.to_string(),
            implies_value: b.display[best_value].clone(),
            confidence: best_cond,
            support,
        });
    }
}

fn candidate_line(table: &str, c: &Candidate) -> String {
    format!(
        "# candidate: {table}.{a}={av} => {table}.{b}={bv} (confidence={conf:.4} support={sup:.4})",
        table = table,
        a = c.column,
        av = c.value,
        b = c.implies_column,
        bv = c.implies_value,
        conf = c.confidence,
        sup = c.support,
    )
}

/// YAML comment block for the mined candidates. Empty when there is nothing to
/// report, so callers can append unconditionally and keep legacy output.
pub fn render_candidate_comments(tables: &[TableCandidates]) -> String {
    let mut lines = Vec::new();
    for tc in tables {
        for c in &tc.candidates {
            lines.push(candidate_line(&tc.table, c));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::from(CANDIDATE_HEADER);
    out.push('\n');
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Append the candidate comment block to a draft YAML. Returning the input
/// unchanged when there is nothing to report is what keeps `rules-draft`
/// without `--mine` byte-identical to the pre-#69 output.
pub fn append_candidate_comments(yaml: &str, tables: &[TableCandidates]) -> String {
    let block = render_candidate_comments(tables);
    if block.is_empty() {
        return yaml.to_string();
    }
    let mut out = yaml.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&block);
    out
}

/// Standalone candidate list for `--emit-candidates <file>`.
pub fn render_candidate_report(tables: &[TableCandidates]) -> String {
    let mut out = String::from("# hepta-dbcli rules-draft --mine candidate list (NOT enabled)\n");
    let mut any = false;
    for tc in tables {
        for c in &tc.candidates {
            any = true;
            out.push_str(&format!(
                "{table}.{a}={av} => {table}.{b}={bv} confidence={conf:.4} support={sup:.4}\n",
                table = tc.table,
                a = c.column,
                av = c.value,
                b = c.implies_column,
                bv = c.implies_value,
                conf = c.confidence,
                sup = c.support,
            ));
        }
    }
    if !any {
        out.push_str("# no candidates\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn columns(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn config() -> MineConfig {
        MineConfig::default()
    }

    /// AC1 fixture: `bs='1' => yhs='0'` holds for 99% of the 4000 `bs='1'`
    /// rows (support 0.40), while the marginal of `yhs='0'` is 0.596.
    fn known_rule_rows() -> Vec<Vec<Value>> {
        let mut rows = Vec::new();
        for i in 0..4000 {
            rows.push(vec![json!("1"), json!(if i < 3960 { "0" } else { "1" })]);
        }
        for i in 0..6000 {
            rows.push(vec![json!("0"), json!(if i < 2000 { "0" } else { "1" })]);
        }
        rows
    }

    #[test]
    fn should_skip_identity_like_columns_on_small_samples() {
        // 20 rows where `id` is unique: every id implies exactly one `tag`,
        // which is an identity mapping (a key), not a business rule. Found by
        // running `rules-draft --mine` against a real MySQL table.
        let mut rows = Vec::new();
        for i in 0..20 {
            rows.push(vec![json!(i), json!(if i % 2 == 0 { "x" } else { "y" })]);
        }
        let report = mine_candidates(&columns(&["id", "tag"]), &rows, &config());
        assert!(
            report.candidates.is_empty(),
            "identity-like columns must not produce candidates: {:?}",
            report.candidates
        );
    }

    #[test]
    fn should_keep_low_cardinality_columns_on_small_samples() {
        let mut rows = Vec::new();
        for i in 0..20 {
            rows.push(vec![
                json!(if i < 15 { "1" } else { "2" }),
                json!(if i < 15 { "0" } else { "9" }),
            ]);
        }
        let report = mine_candidates(&columns(&["bs", "yhs"]), &rows, &config());
        assert!(
            report
                .candidates
                .iter()
                .any(|c| c.column == "bs" && c.implies_column == "yhs"),
            "a repeated 2-level column must still participate: {:?}",
            report.candidates
        );
    }

    #[test]
    fn should_mine_detects_known_conditional_rule() {
        let report = mine_candidates(&columns(&["bs", "yhs"]), &known_rule_rows(), &config());

        let c = report
            .candidates
            .iter()
            .find(|c| c.column == "bs" && c.value == "'1'")
            .expect("bs='1' => yhs='0' must be mined");
        assert_eq!(c.implies_column, "yhs");
        assert_eq!(c.implies_value, "'0'");
        assert!(
            (c.confidence - 0.99).abs() < 1e-9,
            "confidence {} should be 0.99",
            c.confidence
        );
        assert!(
            (c.support - 0.40).abs() < 1e-9,
            "support {} should be 0.40",
            c.support
        );
    }

    #[test]
    fn should_mine_stays_silent_on_independent_columns() {
        let mut rows = Vec::new();
        for i in 0..900 {
            rows.push(vec![json!(i % 3), json!((i / 9) % 3)]);
        }
        let report = mine_candidates(&columns(&["a", "b"]), &rows, &config());
        assert!(
            report.candidates.is_empty(),
            "independent columns must not produce candidates: {:?}",
            report.candidates
        );
    }

    #[test]
    fn should_respect_mine_confidence_threshold() {
        // `bs='1' => yhs='0'` at confidence 0.90.
        let mut rows = Vec::new();
        for i in 0..4000 {
            rows.push(vec![json!("1"), json!(if i < 3600 { "0" } else { "1" })]);
        }
        for i in 0..6000 {
            rows.push(vec![json!("0"), json!(if i < 2000 { "0" } else { "1" })]);
        }
        let cols = columns(&["bs", "yhs"]);

        let strict = mine_candidates(
            &cols,
            &rows,
            &MineConfig {
                confidence: 0.95,
                ..config()
            },
        );
        assert!(
            !strict
                .candidates
                .iter()
                .any(|c| c.column == "bs" && c.value == "'1'"),
            "confidence 0.90 rule must not clear a 0.95 threshold"
        );

        let loose = mine_candidates(
            &cols,
            &rows,
            &MineConfig {
                confidence: 0.85,
                ..config()
            },
        );
        assert!(
            loose
                .candidates
                .iter()
                .any(|c| c.column == "bs" && c.value == "'1'"),
            "confidence 0.90 rule must clear a 0.85 threshold"
        );
    }

    #[test]
    fn should_skip_high_cardinality_columns() {
        let mut rows = Vec::new();
        for i in 0..5000 {
            rows.push(vec![json!(i), json!(if i % 2 == 0 { "0" } else { "1" })]);
        }
        let report = mine_candidates(&columns(&["id", "flag"]), &rows, &config());
        assert!(
            !report.candidates.iter().any(|c| c.column == "id"),
            "high-cardinality column must not be an antecedent"
        );
    }

    #[test]
    fn should_report_pairs_truncated_at_max_pairs() {
        let mut rows = Vec::new();
        for i in 0..200 {
            rows.push(vec![json!(i % 3), json!((i / 3) % 3), json!((i / 9) % 3)]);
        }
        let report = mine_candidates(
            &columns(&["a", "b", "c"]),
            &rows,
            &MineConfig {
                max_pairs: 2,
                ..config()
            },
        );
        assert_eq!(report.pairs_total, 6);
        assert_eq!(report.pairs_considered, 2);
        assert!(report.pairs_truncated());
    }

    #[test]
    fn should_ignore_null_cells() {
        let mut rows = Vec::new();
        for i in 0..4000 {
            rows.push(vec![json!("1"), json!(if i < 3960 { "0" } else { "1" })]);
        }
        for i in 0..6000 {
            rows.push(vec![json!("0"), json!(if i < 2000 { "0" } else { "1" })]);
        }
        for _ in 0..500 {
            rows.push(vec![json!("1"), Value::Null]);
        }
        let report = mine_candidates(&columns(&["bs", "yhs"]), &rows, &config());
        let c = report
            .candidates
            .iter()
            .find(|c| c.column == "bs" && c.value == "'1'")
            .expect("rule must survive NULL cells");
        // NULLs are excluded from both distributions: 3960/4000 unchanged.
        assert!((c.confidence - 0.99).abs() < 1e-9);
    }

    #[test]
    fn should_mine_deterministically() {
        let rows = known_rule_rows();
        let first = mine_candidates(&columns(&["bs", "yhs"]), &rows, &config());
        let second = mine_candidates(&columns(&["bs", "yhs"]), &rows, &config());
        assert_eq!(first.candidates, second.candidates);
    }

    #[test]
    fn should_emit_candidates_as_disabled_comments() {
        let report = mine_candidates(&columns(&["bs", "yhs"]), &known_rule_rows(), &config());
        let tables = vec![TableCandidates {
            table: "orders".to_string(),
            candidates: report.candidates,
        }];

        let block = render_candidate_comments(&tables);
        assert!(!block.is_empty());
        for line in block.lines() {
            assert!(
                line.starts_with('#'),
                "every line must be a comment: {line}"
            );
        }
        assert!(block.contains("candidate: orders.bs='1' => orders.yhs='0'"));
        assert!(block.contains("confidence=0.99"));
        assert!(block.contains("support=0.40"));

        let yaml = "version: \"1\"\ntables: []\n";
        let with_comments = append_candidate_comments(yaml, &tables);
        let parsed: crate::synth::rules::SynthRules =
            serde_yaml::from_str(&with_comments).expect("draft with comments must stay parseable");
        assert!(parsed.tables.is_empty());
    }

    #[test]
    fn draft_without_candidates_is_unchanged() {
        let yaml = "version: \"1\"\ntables: []\n";
        assert_eq!(append_candidate_comments(yaml, &[]), yaml);
        let empty = vec![TableCandidates {
            table: "orders".to_string(),
            candidates: vec![],
        }];
        assert_eq!(append_candidate_comments(yaml, &empty), yaml);
    }

    #[test]
    fn render_candidate_report_lists_everything() {
        let report = mine_candidates(&columns(&["bs", "yhs"]), &known_rule_rows(), &config());
        let tables = vec![TableCandidates {
            table: "orders".to_string(),
            candidates: report.candidates,
        }];
        let text = render_candidate_report(&tables);
        assert!(text.contains("orders.bs='1' => orders.yhs='0'"));
        assert_eq!(
            render_candidate_report(&[]),
            "# hepta-dbcli rules-draft --mine candidate list (NOT enabled)\n# no candidates\n"
        );
    }
}
