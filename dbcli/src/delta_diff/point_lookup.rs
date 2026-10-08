// ─── delta-diff 无键表倾斜点查快路径（issue #127）──────────────────────
//
// summary-only 且两侧行数严重倾斜时，读回小表内容做逐内容 COUNT 点查，
// 替代 bucketdiff 对大表的 N 遍 MOD(rowHash) 全表扫描。多重集合语义不变：
// paired = min(小表重数, 大表 COUNT)，永不产生 Modified；大表独有只记账
// 不拉回。索引对齐列用裸等值（可走索引 seek），其余列沿用规范化等值
// （与 checksum 身份逐字一致）。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::backend::{sql_literal_as_text, ColumnNormSpec, DbError, NULL_SENTINEL};
use crate::delta_diff::keyed_diff;
use crate::delta_diff::metadata::TablePlan;

/// 小表侧行数上限：超过即放弃点查回落分桶（issue #127）。
pub(crate) const KEYLESS_POINT_LOOKUP_MAX_SMALL_ROWS: u64 = 64;

pub(crate) struct KeylessPointCounts {
    pub(crate) small_is_left: bool,
    pub(crate) small_only: u64,
    pub(crate) big_only: u64,
    /// 两侧最终精确行数（估计计数复核后），供 shard/汇总 totals 使用。
    pub(crate) left_total: u64,
    pub(crate) right_total: u64,
    pub(crate) small_rows: u64,
    pub(crate) point_queries: u64,
    pub(crate) warnings: Vec<String>,
}

/// 小表内容按位置映射到大表谓词，两侧比对列集必须逐位同名
/// （大小写不敏感、顺序敏感）；不一致时点查路径必须放弃。
pub(crate) fn compare_columns_align(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(l, r)| l.eq_ignore_ascii_case(r))
}

pub(crate) fn point_lookup_gate(
    big: u64,
    small: u64,
    fetch_all_threshold: u64,
    summary_only: bool,
) -> bool {
    summary_only
        && big > 0
        && small > 0
        && big > fetch_all_threshold
        && keyed_diff::is_skewed(big, small)
        && small <= KEYLESS_POINT_LOOKUP_MAX_SMALL_ROWS
}

/// 每个比对列是否可裸等值：任一索引的**前导连续前缀**成员。第一个不在
/// 比对集里的 token 处断开（被排除列或表达式索引的系统生成列名都会断）。
pub(crate) fn bare_aligned_flags(plan: &TablePlan) -> Vec<bool> {
    let mut flags = vec![false; plan.compare_columns.len()];
    for prefix in &plan.aux.index_prefixes {
        for token in prefix {
            match plan
                .compare_columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(token))
            {
                Some(pos) => flags[pos] = true,
                None => break,
            }
        }
    }
    flags
}

fn base_type(data_type: &str) -> String {
    data_type
        .split('(')
        .next()
        .unwrap_or(data_type)
        .trim()
        .to_ascii_lowercase()
}

/// 数值等值与文本等值恒等的类型族（scale 固定）。
fn is_numeric_exact(data_type: &str) -> bool {
    let b = base_type(data_type);
    b.contains("int") || matches!(b.as_str(), "decimal" | "numeric" | "number" | "dec")
}

fn is_float_family(data_type: &str) -> bool {
    let b = base_type(data_type);
    b.contains("float") || b.contains("double") || b == "real"
}

fn is_binary_family(data_type: &str) -> bool {
    matches!(
        base_type(data_type).as_str(),
        "binary"
            | "varbinary"
            | "blob"
            | "tinyblob"
            | "mediumblob"
            | "longblob"
            | "raw"
            | "bytea"
            | "uuid"
    )
}

fn is_string_family(data_type: &str) -> bool {
    matches!(
        base_type(data_type).as_str(),
        "char"
            | "nchar"
            | "varchar"
            | "nvarchar"
            | "varchar2"
            | "nvarchar2"
            | "character"
            | "character varying"
            | "string"
            | "enum"
            | "set"
    )
}

fn bare_numeric_literal_ok(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
        && value.parse::<f64>().is_ok()
}

/// 裸等值资格：索引对齐 + 类型白名单 + 方言修正。白名单外一律规范化等值。
pub(crate) fn bare_eligible(
    spec: &ColumnNormSpec,
    scheme: &str,
    collation: Option<&str>,
    aligned: bool,
) -> bool {
    if !aligned {
        return false;
    }
    if crate::delta_diff::metadata::is_temporal_type(&spec.data_type)
        || is_float_family(&spec.data_type)
        || is_binary_family(&spec.data_type)
        || matches!(base_type(&spec.data_type).as_str(), "bool" | "boolean")
    {
        return false;
    }
    if is_numeric_exact(&spec.data_type) {
        return true;
    }
    if is_string_family(&spec.data_type) {
        return match scheme {
            // MySQL `=` 受 collation 影响（ci / PAD SPACE），只有 NO PAD
            // 二进制 collation 的裸等值与 checksum 字节级身份恒等。
            "mysql" => matches!(collation, Some(c) if c.ends_with("_0900_bin")),
            _ => true,
        };
    }
    false
}

/// 单条内容的点查谓词：`<裸或规范化等值> AND ...`。NULL 一律 `IS NULL`。
pub(crate) fn render_content_predicate(
    dialect: &dyn crate::backend::Dialect,
    specs: &[ColumnNormSpec],
    aligned: &[bool],
    collations: &std::collections::HashMap<String, String>,
    values: &[String],
) -> Result<String, DbError> {
    let scheme = dialect.url_scheme();
    let quote = dialect.identifier_quote();
    let backslash = scheme == "mysql";
    let mut terms = Vec::with_capacity(values.len());
    for (idx, spec) in specs.iter().enumerate() {
        let value = &values[idx];
        let q = crate::backend::quote_ident(quote, &spec.name);
        if value == NULL_SENTINEL {
            terms.push(format!("{q} IS NULL"));
            continue;
        }
        let coll = collations.get(&spec.name).map(String::as_str);
        let aligned_flag = aligned.get(idx).copied().unwrap_or(false);
        let value_lit = || sql_literal_as_text(&Value::String(value.clone()), backslash);
        if is_string_family(&spec.data_type) && scheme == "mysql" {
            if bare_eligible(spec, scheme, coll, aligned_flag) {
                terms.push(format!("{q} = {}", value_lit()));
            } else {
                // 与 #124 点查同款空格敏感逐字节比较：PAD SPACE / ci 修正。
                terms.push(crate::backend::render_key_equality(
                    quote,
                    &spec.name,
                    true,
                    "mysql",
                    &Value::String(value.clone()),
                    true,
                ));
            }
            continue;
        }
        if bare_eligible(spec, scheme, coll, aligned_flag) {
            if is_numeric_exact(&spec.data_type) && bare_numeric_literal_ok(value) {
                terms.push(format!("{q} = {value}"));
                continue;
            }
            if is_string_family(&spec.data_type) {
                terms.push(format!("{q} = {}", value_lit()));
                continue;
            }
        }
        let norm = dialect.normalize_expr(spec)?;
        terms.push(format!("({norm}) = {}", value_lit()));
    }
    Ok(terms.join(" AND "))
}

/// 多重数配对：返回 (小表独有, 配对数)。
pub(crate) fn pair_contents(small_mult: u64, big_count: u64) -> (u64, u64) {
    let paired = small_mult.min(big_count);
    (small_mult - paired, paired)
}

/// 小表内容多重集合的键：规范化文本元组。
pub(crate) type ContentKey = Vec<String>;

pub(crate) fn multiset_from_rows(rows: &[Vec<Value>]) -> BTreeMap<ContentKey, u64> {
    let mut m = BTreeMap::new();
    for row in rows {
        let key: ContentKey = row
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        *m.entry(key).or_insert(0) += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::gaussdb::GaussdbDialect;
    use crate::backend::mysql::dialect::MySqlDialect;
    use crate::backend::ColumnNormSpec;
    use crate::delta_diff::metadata::TablePlanAux;
    use std::collections::HashMap;

    fn spec(name: &str, ty: &str) -> ColumnNormSpec {
        ColumnNormSpec {
            name: name.to_string(),
            data_type: ty.to_string(),
            nullable: true,
            rtrim_fixed_char: false,
        }
    }

    fn plan(compare: &[&str], indexes: Vec<Vec<&str>>) -> TablePlan {
        TablePlan {
            aux: TablePlanAux {
                index_prefixes: indexes
                    .into_iter()
                    .map(|cols| cols.into_iter().map(String::from).collect())
                    .collect(),
                collations: HashMap::new(),
            },
            key_unique: false,
            url_scheme: "mysql".into(),
            key_columns: vec![],
            compare_columns: compare.iter().map(|c| (*c).to_string()).collect(),
            norm_specs: vec![],
            warnings: vec![],
            key_specs: vec![],
        }
    }

    // ── gate ──

    #[test]
    fn gate_requires_summary_only_and_threshold_and_ratio_and_small_cap() {
        assert!(point_lookup_gate(19_988_722, 21, 4096, true));
        // summary_only=false（含 export 优先级降级后的 ctx）直接关闭
        assert!(!point_lookup_gate(19_988_722, 21, 4096, false));
        // 大侧必须严格大于 fetch_all_threshold
        assert!(!point_lookup_gate(4096, 21, 4096, true));
        assert!(point_lookup_gate(4097, 21, 4096, true));
        // 倾斜比 8：big/8 >= small（64 上限下恒成立，保留以锁 issue 语义）
        assert!(point_lookup_gate(4104, 64, 4096, true));
        // 小侧上限 64
        assert!(point_lookup_gate(100_000, 64, 4096, true));
        assert!(!point_lookup_gate(100_000, 65, 4096, true));
        // 零行不启用
        assert!(!point_lookup_gate(0, 21, 4096, true));
        assert!(!point_lookup_gate(19_988_722, 0, 4096, true));
    }

    // ── alignment ──

    #[test]
    fn alignment_flags_follow_leading_prefix_of_any_index() {
        let p = plan(&["a", "b", "c"], vec![vec!["b", "c"]]);
        assert_eq!(bare_aligned_flags(&p), vec![false, true, true]);
    }

    #[test]
    fn alignment_flags_break_at_first_token_outside_compare_set() {
        let p = plan(&["a", "b", "c"], vec![vec!["a", "x", "c"]]);
        assert_eq!(bare_aligned_flags(&p), vec![true, false, false]);
    }

    #[test]
    fn alignment_flags_match_case_insensitively() {
        let p = plan(&["bcrq", "zqdm"], vec![vec!["BCRQ", "ZQDM"]]);
        assert_eq!(bare_aligned_flags(&p), vec![true, true]);
    }

    // ── predicate ──

    fn mysql_collations(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn predicate_bare_numeric_on_aligned_column() {
        let specs = vec![spec("id", "int"), spec("amt", "decimal(10,2)")];
        let p = render_content_predicate(
            &MySqlDialect,
            &specs,
            &[true, true],
            &HashMap::new(),
            &["5".to_string(), "1.10".to_string()],
        )
        .unwrap();
        assert_eq!(p, "`id` = 5 AND `amt` = 1.10");
    }

    #[test]
    fn predicate_normalized_on_unaligned_column() {
        let specs = vec![spec("amt", "decimal(10,2)")];
        let p = render_content_predicate(
            &MySqlDialect,
            &specs,
            &[false],
            &HashMap::new(),
            &["1.10".to_string()],
        )
        .unwrap();
        assert_eq!(
            p,
            format!("(COALESCE(CAST(`amt` AS CHAR), '{NULL_SENTINEL}')) = '1.10'")
        );
    }

    #[test]
    fn predicate_null_becomes_is_null_regardless_of_alignment() {
        let specs = vec![spec("note", "varchar(64)")];
        let p = render_content_predicate(
            &GaussdbDialect,
            &specs,
            &[true],
            &HashMap::new(),
            &[NULL_SENTINEL.to_string()],
        )
        .unwrap();
        assert_eq!(p, "\"note\" IS NULL");
    }

    #[test]
    fn predicate_mysql_string_is_space_sensitive_byte_equality_even_when_aligned() {
        let specs = vec![spec("code", "varchar(32)")];
        let coll = mysql_collations(&[("code", "utf8mb4_general_ci")]);
        let p =
            render_content_predicate(&MySqlDialect, &specs, &[true], &coll, &["ABC".to_string()])
                .unwrap();
        assert_eq!(p, "CAST(`code` AS BINARY) = CAST('ABC' AS BINARY)");
    }

    #[test]
    fn predicate_mysql_nopad_bin_collation_gets_plain_bare_equality() {
        let specs = vec![spec("code", "varchar(32)")];
        let coll = mysql_collations(&[("code", "utf8mb4_0900_bin")]);
        let p =
            render_content_predicate(&MySqlDialect, &specs, &[true], &coll, &["ABC".to_string()])
                .unwrap();
        assert_eq!(p, "`code` = 'ABC'");
    }

    #[test]
    fn predicate_gaussdb_string_aligned_gets_plain_bare_equality() {
        let specs = vec![spec("bcrq", "character varying(16)")];
        let p = render_content_predicate(
            &GaussdbDialect,
            &specs,
            &[true],
            &HashMap::new(),
            &["20260122".to_string()],
        )
        .unwrap();
        assert_eq!(p, "\"bcrq\" = '20260122'");
    }

    #[test]
    fn predicate_excluded_families_fall_back_to_normalized_even_when_aligned() {
        let specs = vec![
            spec("created", "timestamp without time zone"),
            spec("payload", "bytea"),
            spec("flag", "boolean"),
        ];
        let p = render_content_predicate(
            &GaussdbDialect,
            &specs,
            &[true, true, true],
            &HashMap::new(),
            &[
                "2026-01-22 00:00:00+00".to_string(),
                "AB".to_string(),
                "1".to_string(),
            ],
        )
        .unwrap();
        assert!(p.contains("to_char"));
        assert!(p.contains("encode"));
        assert!(p.contains("::int::text"));
    }

    #[test]
    fn predicate_non_numeric_text_on_numeric_column_falls_back() {
        let specs = vec![spec("amt", "decimal(10,2)")];
        let p = render_content_predicate(
            &MySqlDialect,
            &specs,
            &[true],
            &HashMap::new(),
            &["abc".to_string()],
        )
        .unwrap();
        assert_eq!(
            p,
            format!("(COALESCE(CAST(`amt` AS CHAR), '{NULL_SENTINEL}')) = 'abc'")
        );
    }

    #[test]
    fn predicate_escapes_quotes_in_string_literals() {
        let specs = vec![spec("name", "character varying(64)")];
        let p = render_content_predicate(
            &GaussdbDialect,
            &specs,
            &[true],
            &HashMap::new(),
            &["O'Brien".to_string()],
        )
        .unwrap();
        assert_eq!(p, "\"name\" = 'O''Brien'");
    }

    #[test]
    fn predicate_joins_terms_with_and() {
        let specs = vec![spec("bcrq", "character varying(16)"), spec("n", "integer")];
        let p = render_content_predicate(
            &GaussdbDialect,
            &specs,
            &[true, false],
            &HashMap::new(),
            &["20260122".to_string(), "7".to_string()],
        )
        .unwrap();
        assert_eq!(
            p,
            format!("\"bcrq\" = '20260122' AND (COALESCE(\"n\"::text, '{NULL_SENTINEL}')) = '7'")
        );
    }

    #[test]
    #[cfg(feature = "duckdb")]
    fn predicate_duckdb_string_aligned_gets_plain_bare_equality() {
        let specs = vec![spec("code", "VARCHAR")];
        let p = render_content_predicate(
            &crate::backend::duckdb::dialect::DuckDbDialect,
            &specs,
            &[true],
            &HashMap::new(),
            &["ABC".to_string()],
        )
        .unwrap();
        assert_eq!(p, "\"code\" = 'ABC'");
    }

    // ── pairing ──

    #[test]
    fn pairing_caps_at_min_multiplicity() {
        assert_eq!(pair_contents(1, 5), (0, 1));
        assert_eq!(pair_contents(3, 2), (1, 2));
        assert_eq!(pair_contents(0, 9), (0, 0));
    }

    #[test]
    fn multiset_merges_duplicate_rows_and_counts_them() {
        let rows = vec![
            vec![Value::from("a"), Value::from("1")],
            vec![Value::from("a"), Value::from("1")],
            vec![Value::from("b"), Value::from("2")],
        ];
        let m = multiset_from_rows(&rows);
        assert_eq!(m.get(&vec!["a".to_string(), "1".to_string()]), Some(&2));
        assert_eq!(m.get(&vec!["b".to_string(), "2".to_string()]), Some(&1));
    }

    #[test]
    fn compare_columns_align_requires_same_names_in_same_order() {
        let v = |names: &[&str]| names.iter().map(|n| (*n).to_string()).collect::<Vec<_>>();
        assert!(compare_columns_align(&v(&["a", "b"]), &v(&["a", "b"])));
        // 大小写不敏感（Oracle 大写编目 vs 规范小写）
        assert!(compare_columns_align(
            &v(&["BCRQ", "ZQDM"]),
            &v(&["bcrq", "zqdm"])
        ));
        // 顺序不同 = 位置映射错配，必须拒绝
        assert!(!compare_columns_align(&v(&["a", "b"]), &v(&["b", "a"])));
        // 列数不同 = 越界/丢列，必须拒绝
        assert!(!compare_columns_align(&v(&["a"]), &v(&["a", "b"])));
        assert!(!compare_columns_align(&v(&[]), &v(&["a"])));
    }
}
