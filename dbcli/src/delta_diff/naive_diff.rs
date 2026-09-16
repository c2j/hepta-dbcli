// ─── delta-diff NaiveDiffer: one full scan per side + client merge (#87) ──
//
// Per side a single `SELECT … WHERE …` full scan (raw key ORDER BY, no
// NLSSORT/COLLATE, no LIMIT); rows are matched client-side by canonical
// fingerprints (HashMap join), so correctness never depends on the server
// row order. Filled in by the naivediff implementation tasks; the `diff`
// body is a placeholder until then.

use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;

use crate::backend::{DbConn, DbError};
use crate::delta_diff::report::{DiffReport, DiffRow, DiffStatus};
use crate::delta_diff::rowdiff;
use crate::delta_diff::strategy::{DiffContext, DiffStrategy};

pub(crate) struct NaiveDiffer;

/// Canonical client-side cell identity (issue #87 D2): Null and Text stay
/// strictly distinct, numeric-flagged text normalizes through Decimal so
/// `1` / `1.0` / `"1"` share one fingerprint across engines.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Fingerprint {
    Null,
    Num(Decimal),
    Text(String),
}

fn fingerprint_cell(v: &Value, numeric: bool) -> Fingerprint {
    if v.is_null() {
        return Fingerprint::Null;
    }
    let text = rowdiff::value_text(v);
    if numeric {
        if let Ok(d) = Decimal::from_str(text.trim()) {
            return Fingerprint::Num(d);
        }
    }
    Fingerprint::Text(text)
}

fn fingerprint_row(row: &[Value], flags: &[bool]) -> Vec<Fingerprint> {
    row.iter()
        .enumerate()
        .map(|(i, v)| fingerprint_cell(v, flags.get(i).copied().unwrap_or(false)))
        .collect()
}

/// Keyed multiset join over canonical fingerprints (issue #87 D1/D3).
/// Right rows pair with left rows in scan order; pairs with differing
/// non-key values become Modified, unpaired right rows become MissingLeft,
/// unpaired left rows (left scan order) become MissingRight. Correctness
/// never depends on server-side ORDER BY.
fn keyed_merge(
    lrows: &[Vec<Value>],
    rrows: &[Vec<Value>],
    arity: usize,
    lnum: &[bool],
    rnum: &[bool],
) -> Vec<DiffRow> {
    let arity = arity.max(1);
    let combined: Vec<bool> = lnum
        .iter()
        .zip(rnum.iter())
        .map(|(l, r)| *l && *r)
        .collect();

    let mut left_map: HashMap<Vec<Fingerprint>, Vec<usize>> = HashMap::new();
    for (i, row) in lrows.iter().enumerate() {
        let n = arity.min(row.len());
        left_map
            .entry(fingerprint_row(&row[..n], &combined))
            .or_default()
            .push(i);
    }

    let mut out = Vec::new();
    let mut consumed = vec![false; lrows.len()];
    for rrow in rrows {
        let n = arity.min(rrow.len());
        let paired = left_map
            .get_mut(&fingerprint_row(&rrow[..n], &combined))
            .and_then(|indices| indices.pop());
        match paired {
            Some(li) => {
                consumed[li] = true;
                let lrow = &lrows[li];
                let ln = arity.min(lrow.len());
                let rn = arity.min(rrow.len());
                if !rowdiff::row_values_equal(
                    &lrow[ln..],
                    &rrow[rn..],
                    &combined[arity.min(combined.len())..],
                ) {
                    out.push(DiffRow {
                        key: rowdiff::diff_key(lrow, arity),
                        left: Some(lrow.clone()),
                        right: Some(rrow.clone()),
                        status: DiffStatus::Modified,
                        confirmed: true,
                    });
                }
            }
            None => out.push(rowdiff::diff_row_n(
                rrow,
                arity,
                false,
                DiffStatus::MissingLeft,
            )),
        }
    }
    for (li, row) in lrows.iter().enumerate() {
        if !consumed[li] {
            out.push(rowdiff::diff_row_n(row, arity, true, DiffStatus::MissingRight));
        }
    }
    out
}

/// Keyless full-row multiset difference over canonical fingerprints
/// (issue #87 D6: the keyless naivediff mode is a bucketdiff superset —
/// it carries row payloads instead of hash counts). Rows cancel
/// min(left, right) per fingerprint; leftover right rows come first in
/// right scan order as MissingLeft, leftover left rows follow in left
/// scan order as MissingRight. There is no Modified status: without a
/// key there is no row identity to compare across sides.
fn keyless_merge(
    lrows: &[Vec<Value>],
    rrows: &[Vec<Value>],
    lnum: &[bool],
    rnum: &[bool],
) -> Vec<DiffRow> {
    let combined: Vec<bool> = lnum
        .iter()
        .zip(rnum.iter())
        .map(|(l, r)| *l && *r)
        .collect();

    let mut left_map: HashMap<Vec<Fingerprint>, Vec<usize>> = HashMap::new();
    for (i, row) in lrows.iter().enumerate() {
        left_map
            .entry(fingerprint_row(row, &combined))
            .or_default()
            .push(i);
    }

    let mut out = Vec::new();
    let mut consumed = vec![false; lrows.len()];
    for rrow in rrows {
        let paired = left_map
            .get_mut(&fingerprint_row(rrow, &combined))
            .and_then(|indices| indices.pop());
        match paired {
            Some(li) => consumed[li] = true,
            None => out.push(rowdiff::diff_row_n(
                rrow,
                0,
                false,
                DiffStatus::MissingLeft,
            )),
        }
    }
    for (li, row) in lrows.iter().enumerate() {
        if !consumed[li] {
            out.push(rowdiff::diff_row_n(row, 0, true, DiffStatus::MissingRight));
        }
    }
    out
}

#[async_trait::async_trait]
impl DiffStrategy for NaiveDiffer {
    fn name(&self) -> &'static str {
        "naivediff"
    }

    async fn diff(
        &self,
        _left: &mut (dyn DbConn + Send),
        _right: &mut (dyn DbConn + Send),
        _ctx: &DiffContext,
    ) -> Result<DiffReport, DbError> {
        todo!("naivediff end-to-end flow lands with the merge implementation tasks")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta_diff::report::DiffStatus;
    use serde_json::json;

    #[test]
    fn compare_primitives_are_shared_across_modules() {
        assert!(crate::delta_diff::rowdiff::row_values_equal(
            &[json!(1)],
            &[json!(1)],
            &[true]
        ));
    }

    #[test]
    fn keyed_merge_reports_modified_when_nonkey_values_differ() {
        let lrows = vec![vec![json!("A"), json!("20251215"), json!("100.00")]];
        let rrows = vec![vec![json!("A"), json!("20251215"), json!("999.00")]];
        let rows = keyed_merge(
            &lrows,
            &rrows,
            2,
            &[false, false, true],
            &[false, false, true],
        );
        assert_eq!(rows.len(), 1, "one paired row, not one-per-side: {rows:?}");
        assert_eq!(rows[0].status, DiffStatus::Modified);
        assert_eq!(rows[0].key, json!(["A", "20251215"]));
        assert!(rows[0].left.is_some() && rows[0].right.is_some());
    }

    #[test]
    fn keyed_merge_treats_numeric_one_and_text_one_as_same_key() {
        let lrows = vec![vec![json!(1), json!("x")]];
        let rrows = vec![vec![json!("1"), json!("x")]];
        let rows = keyed_merge(&lrows, &rrows, 1, &[true, false], &[true, false]);
        assert!(rows.is_empty(), "1 vs \"1\" is the same key: {rows:?}");
    }

    #[test]
    fn keyed_merge_marks_rows_missing_on_right_when_absent() {
        let lrows = vec![vec![json!("a"), json!(1)], vec![json!("b"), json!(2)]];
        let rows = keyed_merge(&lrows, &[], 1, &[false, false], &[false, false]);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.status == DiffStatus::MissingRight));
        assert!(rows.iter().all(|r| r.left.is_some() && r.right.is_none()));
    }

    #[test]
    fn keyed_merge_marks_rows_missing_on_left_when_absent() {
        let rrows = vec![vec![json!("a"), json!(1)], vec![json!("b"), json!(2)]];
        let rows = keyed_merge(&[], &rrows, 1, &[false, false], &[false, false]);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.status == DiffStatus::MissingLeft));
        assert!(rows.iter().all(|r| r.right.is_some() && r.left.is_none()));
    }

    #[test]
    fn keyed_merge_pairs_duplicate_keys_without_false_modified() {
        let lrows = vec![
            vec![json!("k"), json!("v1")],
            vec![json!("k"), json!("v2")],
        ];
        let rrows = vec![
            vec![json!("k"), json!("v2")],
            vec![json!("k"), json!("v1")],
        ];
        let rows = keyed_merge(&lrows, &rrows, 1, &[false, false], &[false, false]);
        assert!(
            rows.is_empty(),
            "multiset pairing must not report false Modified: {rows:?}"
        );
    }

    #[test]
    fn keyed_merge_null_key_pairs_with_null_key() {
        let lrows = vec![vec![Value::Null, json!("x")]];
        let rrows = vec![vec![Value::Null, json!("x")], vec![Value::Null, json!("y")]];
        let rows = keyed_merge(&lrows, &rrows, 1, &[false, false], &[false, false]);
        assert_eq!(rows.len(), 1, "identical null-key rows pair: {rows:?}");
        assert_eq!(rows[0].status, DiffStatus::MissingLeft);
        assert_eq!(rows[0].key, Value::Null);
    }

    #[test]
    fn keyed_merge_compares_decimals_exactly_across_scale() {
        let lrows = vec![vec![json!("k"), json!("1.50")]];
        let rrows = vec![vec![json!("k"), json!("1.5")]];
        let rows = keyed_merge(&lrows, &rrows, 1, &[false, true], &[false, true]);
        assert!(rows.is_empty(), "1.50 == 1.5 numerically: {rows:?}");
    }

    #[test]
    fn keyless_merge_reports_only_missing_statuses() {
        let lrows = vec![vec![json!("same")], vec![json!("left-only")]];
        let rrows = vec![vec![json!("same")], vec![json!("right-only")]];
        let rows = keyless_merge(&lrows, &rrows, &[false], &[false]);
        assert_eq!(rows.len(), 2, "identical rows cancel: {rows:?}");
        assert!(rows.iter().all(|r| r.status != DiffStatus::Modified));
        assert!(rows
            .iter()
            .any(|r| r.status == DiffStatus::MissingRight && r.left.is_some()));
        assert!(rows
            .iter()
            .any(|r| r.status == DiffStatus::MissingLeft && r.right.is_some()));
    }

    #[test]
    fn keyless_merge_multiset_cancel_counts() {
        let lrows = vec![vec![json!("x")], vec![json!("x")]];
        let rrows = vec![vec![json!("x")]];
        let rows = keyless_merge(&lrows, &rrows, &[false], &[false]);
        assert_eq!(rows.len(), 1, "2 left vs 1 right cancels one: {rows:?}");
        assert_eq!(rows[0].status, DiffStatus::MissingRight);
    }

    #[test]
    fn keyless_merge_null_and_empty_string_are_distinct_fingerprints() {
        let lrows = vec![vec![Value::Null]];
        let rrows = vec![vec![json!("")]];
        let rows = keyless_merge(&lrows, &rrows, &[false], &[false]);
        assert_eq!(
            rows.len(),
            2,
            "NULL and '' must never cancel each other (Java §6.1): {rows:?}"
        );
    }

    #[test]
    fn keyless_merge_numeric_equivalence_across_scales() {
        let lrows = vec![vec![json!(1)], vec![json!(1.0)]];
        let rrows = vec![vec![json!("1.0")]];
        let rows = keyless_merge(&lrows, &rrows, &[true], &[true]);
        assert_eq!(
            rows.len(),
            1,
            "1 / 1.0 / \"1.0\" are one fingerprint value: {rows:?}"
        );
        assert_eq!(rows[0].status, DiffStatus::MissingRight);
    }
}
