// ─── cross-dialect column and key pairing ──────────────────────────────

use crate::delta_diff::metadata::TablePlan;

pub(crate) struct Pairing {
    /// index into right compare_columns for each left compare_columns index
    pub(crate) right_of_left: Vec<Option<usize>>,
    /// resolved comparison key, in key order, using LEFT-side casing
    pub(crate) key_columns: Vec<String>,
    /// resolved left physical key names, in logical key order
    pub(crate) left_key_columns: Vec<String>,
    /// resolved right physical key names, in logical key order
    pub(crate) right_key_columns: Vec<String>,
    pub(crate) unmatched_left: Vec<String>,
    pub(crate) unmatched_right: Vec<String>,
    pub(crate) ambiguous: Vec<String>,
}

pub(crate) fn find_unique_ci<'a, T>(
    items: &'a [T],
    name: &str,
    item_name: impl Fn(&T) -> &str,
) -> Option<&'a T> {
    if let Some(item) = items.iter().find(|item| item_name(item) == name) {
        return Some(item);
    }
    let mut ci = items
        .iter()
        .filter(|item| item_name(item).eq_ignore_ascii_case(name));
    match (ci.next(), ci.next()) {
        (Some(item), None) => Some(item),
        _ => None,
    }
}

pub(crate) fn pair_plans(l: &TablePlan, r: &TablePlan) -> Pairing {
    let (right_of_left, unmatched_left, unmatched_right, ambiguous) =
        pair_names(&l.compare_columns, &r.compare_columns);
    let (right_key_of_left, _, _, key_ambiguous) = pair_names(&l.key_columns, &r.key_columns);
    let keys_align = !l.key_columns.is_empty()
        && !r.key_columns.is_empty()
        && l.key_columns.len() == r.key_columns.len()
        && key_ambiguous.is_empty()
        && right_key_of_left.iter().all(Option::is_some);
    let right_key_columns = if keys_align {
        right_key_of_left
            .iter()
            .filter_map(|index| index.map(|index| r.key_columns[index].clone()))
            .collect()
    } else {
        Vec::new()
    };

    Pairing {
        right_of_left,
        key_columns: if keys_align {
            l.key_columns.clone()
        } else {
            Vec::new()
        },
        left_key_columns: if keys_align {
            l.key_columns.clone()
        } else {
            Vec::new()
        },
        right_key_columns,
        unmatched_left,
        unmatched_right,
        ambiguous,
    }
}

fn pair_names(left: &[String], right: &[String]) -> PairingParts {
    let mut right_of_left = vec![None; left.len()];
    let mut right_paired = vec![false; right.len()];
    let mut ambiguous = Vec::new();

    // Reserve all exact matches before allowing a case-insensitive fallback
    // to consume a right-side name needed by a later exact match.
    for (left_index, left_name) in left.iter().enumerate() {
        if let Some(right_index) = right
            .iter()
            .enumerate()
            .find(|(index, right_name)| !right_paired[*index] && *right_name == left_name)
            .map(|(index, _)| index)
        {
            right_of_left[left_index] = Some(right_index);
            right_paired[right_index] = true;
        }
    }

    for (left_index, left_name) in left.iter().enumerate() {
        if right_of_left[left_index].is_some() {
            continue;
        }
        let mut candidates = right.iter().enumerate().filter(|(index, right_name)| {
            !right_paired[*index] && right_name.eq_ignore_ascii_case(left_name)
        });
        match (candidates.next(), candidates.next()) {
            (Some((right_index, _)), None) => {
                right_of_left[left_index] = Some(right_index);
                right_paired[right_index] = true;
            }
            (Some(_), Some(_)) => ambiguous.push(left_name.clone()),
            _ => {}
        }
    }

    let unmatched_left = left
        .iter()
        .zip(&right_of_left)
        .filter(|(_, right_index)| right_index.is_none())
        .map(|(name, _)| name.clone())
        .collect();
    let unmatched_right = right
        .iter()
        .zip(right_paired)
        .filter(|(_, paired)| !paired)
        .map(|(name, _)| name.clone())
        .collect();
    (right_of_left, unmatched_left, unmatched_right, ambiguous)
}

type PairingParts = (Vec<Option<usize>>, Vec<String>, Vec<String>, Vec<String>);

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(compare: &[&str]) -> TablePlan {
        TablePlan {
            key_columns: Vec::new(),
            compare_columns: compare.iter().map(|name| (*name).to_string()).collect(),
            norm_specs: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn exact_match_wins_over_case_insensitive_candidates() {
        let paired = pair_plans(&plan(&["Id"]), &plan(&["Id", "ID"]));
        assert_eq!(paired.right_of_left, vec![Some(0)]);
        assert!(paired.ambiguous.is_empty());
        assert_eq!(paired.unmatched_right, vec!["ID"]);
    }

    #[test]
    fn ambiguous_case_insensitive_match_is_not_paired() {
        let paired = pair_plans(&plan(&["id"]), &plan(&["Id", "ID"]));
        assert_eq!(paired.right_of_left, vec![None]);
        assert_eq!(paired.ambiguous, vec!["id"]);
    }

    #[test]
    fn unmatched_columns_are_reported_on_each_side() {
        let paired = pair_plans(&plan(&["id", "left_only"]), &plan(&["ID", "right_only"]));
        assert_eq!(paired.right_of_left, vec![Some(0), None]);
        assert_eq!(paired.unmatched_left, vec!["left_only"]);
        assert_eq!(paired.unmatched_right, vec!["right_only"]);
    }

    #[test]
    fn key_pairing_preserves_left_logical_order_and_right_physical_names() {
        let mut left = plan(&[]);
        left.key_columns = vec!["K_XWDM".into(), "SECURITY_ID".into()];
        let mut right = plan(&[]);
        right.key_columns = vec!["security_id".into(), "k_xwdm".into()];

        let paired = pair_plans(&left, &right);

        assert_eq!(paired.key_columns, vec!["K_XWDM", "SECURITY_ID"]);
        assert_eq!(paired.left_key_columns, vec!["K_XWDM", "SECURITY_ID"]);
        assert_eq!(paired.right_key_columns, vec!["k_xwdm", "security_id"]);
    }
}
