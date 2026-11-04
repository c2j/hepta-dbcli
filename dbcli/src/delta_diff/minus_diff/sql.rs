// ─── minusdiff SQL: shape, rendering, and classification ─────────────
//
// Pure. Nothing here opens a connection. `prepare_minusdiff` checks the
// plan before any operator is chosen. `render_prepared` only formats a
// shape that has already been checked.

use std::collections::HashMap;

use serde_json::Value;

use crate::backend::{DbError, Dialect, QueryResult, SetDifferenceOp};
use crate::delta_diff::keyed_diff::parse_count;
use crate::delta_diff::pairing::pair_plans;
use crate::delta_diff::report::{DiffRow, DiffStatus};
use crate::delta_diff::strategy::{side_filter, DiffContext};

/// What a MySQL-family `VERSION()` string means for `EXCEPT`.
/// `Refuse` has the same outcome as [`SetDifferenceOp::None`]: do not emit a
/// set difference. `Except` corresponds to [`SetDifferenceOp::Except`].
/// `Probe` is not an operator; a later probe becomes `Except` or `None`.
/// This function never returns [`SetDifferenceOp::Minus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MysqlSetOpDecision {
    Refuse,
    Except,
    Probe,
}

pub(crate) fn mysql_set_op_decision(version: &str) -> MysqlSetOpDecision {
    if crate::backend::is_polardbx_version(version) {
        return MysqlSetOpDecision::Refuse;
    }
    let upper = version.to_ascii_uppercase();
    if upper.contains("MARIADB") || upper.contains("POLARDB") {
        return MysqlSetOpDecision::Probe;
    }
    match mysql_version_at_least_8_0_31(version) {
        Some(true) => MysqlSetOpDecision::Except,
        Some(false) => MysqlSetOpDecision::Refuse,
        None => MysqlSetOpDecision::Probe,
    }
}

fn mysql_version_at_least_8_0_31(version: &str) -> Option<bool> {
    let mut nums = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty());
    let major: u64 = nums.next()?.parse().ok()?;
    let minor: u64 = nums.next()?.parse().ok()?;
    let patch: u64 = nums.next()?.parse().ok()?;
    Some((major, minor, patch) >= (8, 0, 31))
}

/// Prepend a constant `side` cell so an empty-side `SELECT` (no `side`
/// column) matches the set-difference layout `[side, rk…, c…, k…]`.
/// Left table empty → `"R"`. Right table empty → `"L"`.
pub(crate) fn tag_empty_side_rows(side: &str, rows: &[Vec<Value>]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| {
            let mut tagged = Vec::with_capacity(row.len() + 1);
            tagged.push(Value::String(side.to_string()));
            tagged.extend(row.iter().cloned());
            tagged
        })
        .collect()
}

/// Column alignment and value projections, checked before any operator.
#[derive(Debug)]
pub(crate) struct MinusShape {
    pub(crate) key_len: usize,
    pub(crate) left_values: Vec<String>,
    pub(crate) right_values: Vec<String>,
}

/// Reject an empty key, a misaligned plan, or a missing normalization spec.
/// Does not look at [`SetDifferenceOp`].
pub(crate) fn prepare_minusdiff(
    ctx: &DiffContext,
    dialect: &dyn Dialect,
) -> Result<MinusShape, DbError> {
    if ctx.key_columns.is_empty() {
        return Err(DbError::config(
            "minusdiff: set difference requires a comparison key",
        ));
    }

    let pairing = pair_plans(&ctx.left.plan, &ctx.right.plan);
    if !pairing.unmatched_left.is_empty()
        || !pairing.unmatched_right.is_empty()
        || !pairing.ambiguous.is_empty()
    {
        return Err(DbError::config(format!(
            "minusdiff: columns are not aligned; unmatched left [{}], unmatched right [{}], ambiguous [{}]",
            pairing.unmatched_left.join(", "),
            pairing.unmatched_right.join(", "),
            pairing.ambiguous.join(", ")
        )));
    }
    require_key_specs(ctx, true)?;
    require_key_specs(ctx, false)?;

    let left_values = value_exprs(ctx, dialect, &pairing.right_of_left, true)?;
    let right_values = value_exprs(ctx, dialect, &pairing.right_of_left, false)?;
    if left_values.len() != right_values.len() {
        return Err(DbError::config(
            "minusdiff: value projections differ between the two sides",
        ));
    }
    Ok(MinusShape {
        key_len: ctx.key_columns.len(),
        left_values,
        right_values,
    })
}

pub(crate) fn render_minusdiff_sql(
    ctx: &DiffContext,
    dialect: &dyn Dialect,
    op: SetDifferenceOp,
    summary_only: bool,
    scn: Option<u64>,
) -> Result<String, DbError> {
    let shape = prepare_minusdiff(ctx, dialect)?;
    render_prepared(ctx, dialect, &shape, op, summary_only, scn)
}

/// Format one statement from an already-checked [`MinusShape`].
pub(super) fn render_prepared(
    ctx: &DiffContext,
    dialect: &dyn Dialect,
    shape: &MinusShape,
    op: SetDifferenceOp,
    summary_only: bool,
    scn: Option<u64>,
) -> Result<String, DbError> {
    let keyword = match op {
        SetDifferenceOp::Minus => "MINUS",
        SetDifferenceOp::Except => "EXCEPT",
        SetDifferenceOp::None => {
            return Err(DbError::unsupported(
                "minusdiff: dialect has no set-difference operator",
            ));
        }
    };

    let oracle_scn = (dialect.url_scheme() == "oracle").then_some(scn).flatten();
    let where_sql = side_filter(ctx, dialect.url_scheme())
        .map(|pred| format!(" WHERE ({pred})"))
        .unwrap_or_default();

    let l = cte_select(ctx, dialect, true, &shape.left_values, summary_only)?;
    let r = cte_select(ctx, dialect, false, &shape.right_values, summary_only)?;
    let lfrom = from_clause(
        &dialect.quote_table(ctx.left.schema.as_deref(), &ctx.left.table),
        oracle_scn,
    );
    let rfrom = from_clause(
        &dialect.quote_table(ctx.right.schema.as_deref(), &ctx.right.table),
        oracle_scn,
    );
    let key_list = (0..shape.key_len)
        .map(|i| format!("k{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let operand = format!("SELECT {key_list}, h");
    // summary_only reads each set-difference CTE more than once. MySQL 8.4
    // mis-reads that shared EXCEPT temp table (Bug #121239; handler error
    // 124, or an InnoDB abort once the temp table spills). Copy the rows
    // through a plain derived table first. The alias has no AS: Oracle
    // rejects `) AS setdiff`.
    let ld_body = format!("{operand} FROM l\n  {keyword}\n  {operand} FROM r");
    let rd_body = format!("{operand} FROM r\n  {keyword}\n  {operand} FROM l");
    let (ld_sql, rd_sql) = if summary_only {
        (
            format!("SELECT * FROM (\n  {ld_body}\n) setdiff"),
            format!("SELECT * FROM (\n  {rd_body}\n) setdiff"),
        )
    } else {
        (ld_body, rd_body)
    };

    let mut sql = format!(
        "WITH l AS (\n  SELECT {l}\n  FROM {lfrom}{where_sql}\n),\nr AS (\n  SELECT {r}\n  FROM {rfrom}{where_sql}\n),\nld AS (\n  {ld_sql}\n),\nrd AS (\n  {rd_sql}\n)\n"
    );
    if summary_only {
        sql.push_str(&summary_select(
            dialect.url_scheme() == "oracle",
            shape.key_len,
        ));
    } else {
        sql.push_str(&body_select(shape.key_len, shape.left_values.len()));
    }
    Ok(sql)
}

fn require_key_specs(ctx: &DiffContext, is_left: bool) -> Result<(), DbError> {
    let plan = if is_left {
        &ctx.left.plan
    } else {
        &ctx.right.plan
    };
    for key in ctx.side_key_columns(is_left) {
        if plan.spec_for(key).is_none() {
            return Err(DbError::config(format!(
                "minusdiff: key column '{key}' has no normalization spec"
            )));
        }
    }
    Ok(())
}

pub(super) fn from_clause(table: &str, scn: Option<u64>) -> String {
    match scn {
        Some(n) => format!("{table} AS OF SCN {n}"),
        None => table.to_string(),
    }
}

fn is_key_name(name: &str, keys: &[String]) -> bool {
    keys.iter().any(|key| key == name)
}

/// Non-key compare expressions. Left uses its own specs. Right uses
/// `right.norm_specs[right_of_left[i]]` at the left compare-column index,
/// not the filtered non-key ordinal.
pub(super) fn value_exprs(
    ctx: &DiffContext,
    dialect: &dyn Dialect,
    right_of_left: &[Option<usize>],
    is_left: bool,
) -> Result<Vec<String>, DbError> {
    let left_keys = ctx.side_key_columns(true);
    let mut out = Vec::new();
    for (index, spec) in ctx.left.plan.norm_specs.iter().enumerate() {
        if is_key_name(&spec.name, left_keys) {
            continue;
        }
        let expr = if is_left {
            dialect.normalize_expr(spec)?
        } else {
            let right_index = right_of_left.get(index).copied().flatten().ok_or_else(|| {
                DbError::config(format!(
                    "minusdiff: column '{}' has no right-hand match",
                    spec.name
                ))
            })?;
            let right_spec = ctx.right.plan.norm_specs.get(right_index).ok_or_else(|| {
                DbError::config(format!(
                    "minusdiff: column '{}' right index {right_index} is out of range",
                    spec.name
                ))
            })?;
            dialect.normalize_expr(right_spec)?
        };
        out.push(expr);
    }
    Ok(out)
}

fn cte_select(
    ctx: &DiffContext,
    dialect: &dyn Dialect,
    is_left: bool,
    value_exprs: &[String],
    summary_only: bool,
) -> Result<String, DbError> {
    let plan = if is_left {
        &ctx.left.plan
    } else {
        &ctx.right.plan
    };
    let keys = ctx.side_key_columns(is_left);
    let mut items = Vec::new();
    let mut raw_keys = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        let spec = plan.spec_for(key).ok_or_else(|| {
            DbError::config(format!(
                "minusdiff: key column '{key}' has no normalization spec"
            ))
        })?;
        items.push(format!("{} AS k{i}", dialect.normalize_expr(spec)?));
        raw_keys.push(format!("{} AS rk{i}", dialect.quote_catalog_ident(key)));
    }
    let hash_inputs: Vec<String> = plan
        .norm_specs
        .iter()
        .map(|spec| dialect.normalize_expr(spec))
        .collect::<Result<_, _>>()?;
    let hashed = if hash_inputs.is_empty() {
        dialect.row_hash_expr(&["''".to_string()])
    } else {
        dialect.row_hash_expr(&hash_inputs)
    };
    items.push(format!("{hashed} AS h"));
    if !summary_only {
        items.extend(raw_keys);
        for (j, expr) in value_exprs.iter().enumerate() {
            items.push(format!("{expr} AS c{j}"));
        }
    }
    Ok(items.join(", "))
}

fn null_safe_eq(left: &str, right: &str) -> String {
    format!("({left} = {right} OR ({left} IS NULL AND {right} IS NULL))")
}

fn key_match(left: &str, right: &str, key_len: usize) -> String {
    (0..key_len)
        .map(|i| null_safe_eq(&format!("{left}.k{i}"), &format!("{right}.k{i}")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn summary_select(oracle: bool, key_len: usize) -> String {
    let keys = key_match("d", "e", key_len);
    let missing_right = format!(
        "(SELECT COUNT(*) FROM ld d WHERE NOT EXISTS (SELECT 1 FROM rd e WHERE {keys})) AS missing_right"
    );
    let missing_left = format!(
        "(SELECT COUNT(*) FROM rd d WHERE NOT EXISTS (SELECT 1 FROM ld e WHERE {keys})) AS missing_left"
    );
    let modified = format!(
        "(SELECT COUNT(*) FROM ld d WHERE EXISTS (SELECT 1 FROM rd e WHERE {keys})) AS modified"
    );
    let from = if oracle { " FROM dual" } else { "" };
    format!("SELECT {missing_right}, {missing_left}, {modified}{from}")
}

fn body_select(key_len: usize, value_len: usize) -> String {
    let mut outer = vec!["side".to_string()];
    outer.extend((0..key_len).map(|i| format!("rk{i}")));
    outer.extend((0..value_len).map(|i| format!("c{i}")));
    outer.extend((0..key_len).map(|i| format!("k{i}")));
    let project = |alias: &str, side: &str| {
        let mut cols = vec![format!("'{side}' AS side")];
        cols.extend((0..key_len).map(|i| format!("{alias}.rk{i} AS rk{i}")));
        cols.extend((0..value_len).map(|i| format!("{alias}.c{i} AS c{i}")));
        cols.extend((0..key_len).map(|i| format!("{alias}.k{i} AS k{i}")));
        cols.join(", ")
    };
    let lkeys = key_match("ld", "l", key_len);
    let rkeys = key_match("rd", "r", key_len);
    let lh = null_safe_eq("ld.h", "l.h");
    let rh = null_safe_eq("rd.h", "r.h");
    let order = (0..key_len)
        .map(|i| format!("k{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT {outer}\nFROM (\n  SELECT {lproj}\n  FROM ld\n  JOIN l\n    ON {lkeys} AND {lh}\n  UNION ALL\n  SELECT {rproj}\n  FROM rd\n  JOIN r\n    ON {rkeys} AND {rh}\n) d\nORDER BY {order}",
        outer = outer.join(", "),
        lproj = project("l", "L"),
        rproj = project("r", "R"),
    )
}

pub(crate) fn classify_minus_rows(
    key_len: usize,
    value_len: usize,
    rows: &[Vec<Value>],
) -> Result<Vec<DiffRow>, DbError> {
    let expected = 1 + key_len + value_len + key_len;
    let mut order = Vec::new();
    let mut groups: HashMap<String, Group> = HashMap::new();
    for row in rows {
        if row.len() < expected {
            return Err(DbError::query(format!(
                "minusdiff: projection too short: result row has {} cells, expected {expected}",
                row.len()
            )));
        }
        if row.len() != expected {
            return Err(DbError::query(format!(
                "minusdiff: result row has {} cells, expected {expected}",
                row.len()
            )));
        }
        let side = match row.first() {
            Some(Value::String(side)) if side == "L" || side == "R" => side.as_str(),
            _ => {
                return Err(DbError::query("minusdiff: result side must be 'L' or 'R'"));
            }
        };
        let rk_end = 1 + key_len;
        let c_end = rk_end + value_len;
        let raw_key = diff_key(&row[1..rk_end]);
        let payload = row[1..c_end].to_vec();
        let norm_key =
            serde_json::to_string(&Value::Array(row[c_end..].to_vec())).map_err(|err| {
                DbError::query(format!("minusdiff: cannot encode normalized key: {err}"))
            })?;
        let group = groups.entry(norm_key.clone()).or_insert_with(|| {
            order.push(norm_key);
            Group {
                key: raw_key,
                left: None,
                right: None,
            }
        });
        let slot = if side == "L" {
            &mut group.left
        } else {
            &mut group.right
        };
        if slot.is_some() {
            return Err(DbError::query("minusdiff: normalized key is not unique"));
        }
        *slot = Some(payload);
    }

    let mut out = Vec::with_capacity(order.len());
    for key in order {
        let group = groups.remove(&key).ok_or_else(|| {
            DbError::query("minusdiff: normalized key group was lost before classification")
        })?;
        let (status, left, right) = match (group.left, group.right) {
            (Some(left), None) => (DiffStatus::MissingRight, Some(left), None),
            (None, Some(right)) => (DiffStatus::MissingLeft, None, Some(right)),
            (Some(left), Some(right)) => (DiffStatus::Modified, Some(left), Some(right)),
            (None, None) => continue,
        };
        out.push(DiffRow {
            key: group.key,
            left,
            right,
            status,
            confirmed: true,
        });
    }
    Ok(out)
}

struct Group {
    key: Value,
    left: Option<Vec<Value>>,
    right: Option<Vec<Value>>,
}

#[derive(Debug)]
pub(super) struct MinusCounts {
    pub(super) missing_left: u64,
    pub(super) missing_right: u64,
    pub(super) modified: u64,
}

impl MinusCounts {
    pub(super) const ZERO: Self = Self {
        missing_left: 0,
        missing_right: 0,
        modified: 0,
    };
}

pub(super) fn summary_counts(result: &QueryResult) -> Result<MinusCounts, DbError> {
    let row = result.rows.first().ok_or_else(|| {
        DbError::query("minusdiff: projection too short: summary returned no rows")
    })?;
    let missing_left_at = summary_column(&result.columns, "missing_left")?;
    let missing_right_at = summary_column(&result.columns, "missing_right")?;
    let modified_at = summary_column(&result.columns, "modified")?;
    Ok(MinusCounts {
        missing_left: summary_cell(row, missing_left_at)?,
        missing_right: summary_cell(row, missing_right_at)?,
        modified: summary_cell(row, modified_at)?,
    })
}

fn summary_column(columns: &[String], name: &str) -> Result<usize, DbError> {
    columns
        .iter()
        .position(|column| column.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            DbError::query(format!(
                "minusdiff: summary result is missing column '{name}'"
            ))
        })
}

fn summary_cell(row: &[Value], index: usize) -> Result<u64, DbError> {
    let value = row.get(index).ok_or_else(|| {
        DbError::query(format!(
            "minusdiff: projection too short: summary row has {} cells, expected {}",
            row.len(),
            index + 1
        ))
    })?;
    cell_count(value)
}

fn cell_count(value: &Value) -> Result<u64, DbError> {
    parse_count(&QueryResult {
        columns: vec!["c".into()],
        rows: vec![vec![value.clone()]],
        row_count: 1,
        rows_affected: None,
    })
}

pub(super) fn tally(rows: &[DiffRow]) -> MinusCounts {
    let mut counts = MinusCounts::ZERO;
    for row in rows {
        match row.status {
            DiffStatus::MissingLeft => counts.missing_left += 1,
            DiffStatus::MissingRight => counts.missing_right += 1,
            DiffStatus::Modified => counts.modified += 1,
        }
    }
    counts
}

fn diff_key(raw: &[Value]) -> Value {
    if raw.len() == 1 {
        raw[0].clone()
    } else {
        Value::Array(raw.to_vec())
    }
}
