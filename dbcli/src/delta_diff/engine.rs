// ─── delta-diff engine: SmartRouter 策略路由（v2.1 §6.1）────────────────
//
// auto 路由规则：
//   无主键 / 键形态不一致 / 复合键      → bucketdiff
//   单列非数值键（字符串/浮点）          → bucketdiff + 告警（§6.4）
//   同连接（同库两表）且 MySQL 系        → joindiff
//   其余                               → hashdiff

use crate::config::ResolvedConnection;
use crate::delta_diff::cmd::{DeltaDiffArgs, Strategy};
use crate::delta_diff::metadata::TablePlan;
use crate::delta_diff::strategy::DiffStrategy;
use crate::delta_diff::{bucket_diff, hash_diff, iblt_diff, join_diff, keyed_diff};

pub(crate) struct Route {
    pub(crate) strategy: Box<dyn DiffStrategy>,
    pub(crate) key_column: String,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn route(
    args: &DeltaDiffArgs,
    left: &ResolvedConnection,
    right: &ResolvedConnection,
    lplan: &TablePlan,
    rplan: &TablePlan,
) -> Result<Route, String> {
    route_impl(
        lplan,
        rplan,
        &left.connection_url,
        &right.connection_url,
        Some(args.strategy),
    )
}

/// 无 CLI 依赖的路由（api.rs/MCP 共用）；strategy_hint=None 即 auto。
pub(crate) fn route_plan(
    lplan: &TablePlan,
    rplan: &TablePlan,
    left_url: &str,
    right_url: &str,
    strategy_hint: Option<Strategy>,
) -> Result<Route, String> {
    route_impl(lplan, rplan, left_url, right_url, strategy_hint)
}

fn route_impl(
    lplan: &TablePlan,
    rplan: &TablePlan,
    left_url: &str,
    right_url: &str,
    strategy_hint: Option<Strategy>,
) -> Result<Route, String> {
    let mut warnings = Vec::new();
    let key = resolve_key(lplan, rplan);
    let same_conn = left_url == right_url;
    let mysql_family = left_url
        .split("://")
        .next()
        .map(|s| s == "mysql")
        .unwrap_or(true);

    let bisectable = key
        .as_ref()
        .map(|k| !k.is_empty() && is_int_key(lplan, k) && is_int_key(rplan, k))
        .unwrap_or(false);

    let strategy: Box<dyn DiffStrategy> = match strategy_hint.unwrap_or(Strategy::Auto) {
        Strategy::Hashdiff => {
            if bisectable {
                return Ok(Route {
                    strategy: Box::new(hash_diff::HashDiffer),
                    key_column: key.clone().unwrap_or_default(),
                    warnings,
                });
            }
            return Ok(bucketdiff_fallback(
                &key,
                warnings,
                "strategy 'hashdiff' requires a single integer key",
                &non_bisectable_reason(lplan, rplan, &key),
            ));
        }
        Strategy::Bucketdiff => {
            return Ok(Route {
                strategy: Box::new(bucket_diff::BucketDiffer),
                key_column: key.clone().unwrap_or_default(),
                warnings,
            });
        }
        Strategy::Joindiff => {
            let Some(k) = key.clone().filter(|k| !k.is_empty()) else {
                return Ok(bucketdiff_fallback(
                    &key,
                    warnings,
                    "strategy 'joindiff' requires a single comparison key",
                    &non_bisectable_reason(lplan, rplan, &key),
                ));
            };
            if !same_conn || !mysql_family {
                if !bisectable {
                    return Ok(bucketdiff_fallback(
                        &key,
                        warnings,
                        "strategy 'joindiff' is unavailable across connections or non-MySQL; \
                         its hashdiff fallback requires a single integer key",
                        &non_bisectable_reason(lplan, rplan, &key),
                    ));
                }
                warnings.push(
                    "joindiff requires same-connection MySQL-family; falling back to hashdiff"
                        .to_string(),
                );
                return Ok(Route {
                    strategy: Box::new(hash_diff::HashDiffer),
                    key_column: k,
                    warnings,
                });
            }
            return Ok(Route {
                strategy: Box::new(join_diff::JoinDiffer),
                key_column: k,
                warnings,
            });
        }
        Strategy::Auto => match (&key, bisectable) {
            (None, _) => {
                warnings.push(
                    "note: keyless table diff reports row-content multiset differences only"
                        .to_string(),
                );
                Box::new(bucket_diff::BucketDiffer)
            }
            (Some(_), false) => {
                warnings.push(format!(
                    "{}; falling back to bucketdiff",
                    non_bisectable_reason(lplan, rplan, &key)
                ));
                Box::new(bucket_diff::BucketDiffer)
            }
            (Some(_), true) if same_conn && mysql_family => Box::new(join_diff::JoinDiffer),
            (Some(_), true) => Box::new(iblt_diff::IbltDiffer),
        },
        Strategy::Iblt => {
            if bisectable {
                return Ok(Route {
                    strategy: Box::new(iblt_diff::IbltDiffer),
                    key_column: key.clone().unwrap_or_default(),
                    warnings,
                });
            }
            return Ok(bucketdiff_fallback(
                &key,
                warnings,
                "strategy 'iblt' requires a single integer key",
                &non_bisectable_reason(lplan, rplan, &key),
            ));
        }
        Strategy::Keyeddiff => {
            return Ok(Route {
                strategy: Box::new(keyed_diff::KeyedDiffer),
                key_column: key.clone().unwrap_or_default(),
                warnings,
            });
        }
    };

    Ok(Route {
        strategy,
        key_column: key.unwrap_or_default(),
        warnings,
    })
}

/// 键解析：--key 优先；否则双侧主键自动发现，要求一致且单列。
fn resolve_key(lplan: &TablePlan, rplan: &TablePlan) -> Option<String> {
    if lplan.key_columns.len() == 1
        && rplan.key_columns.len() == 1
        && lplan.key_columns == rplan.key_columns
    {
        return Some(lplan.key_columns[0].clone());
    }
    None
}

fn is_int_key(plan: &TablePlan, key: &str) -> bool {
    let ty = plan
        .norm_specs
        .iter()
        .find(|s| s.name == key)
        .map(|s| s.data_type.as_str())
        .unwrap_or("");
    let base = ty.split('(').next().unwrap_or("").trim().to_lowercase();
    matches!(
        base.as_str(),
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "integer"
            | "bigint"
            | "int2"
            | "int4"
            | "int8"
            | "oid"
            | "number"
            | "numeric"
            | "decimal"
    )
}

/// 可读的「键不可切分」原因描述（不引用 §6.4 等内部章节号）。
fn non_bisectable_reason(lplan: &TablePlan, rplan: &TablePlan, key: &Option<String>) -> String {
    if let Some(k) = key {
        let compared = |p: &TablePlan| p.norm_specs.iter().any(|s| &s.name == k);
        if !compared(lplan) || !compared(rplan) {
            return format!("key column '{k}' is not among the compared columns");
        }
        return format!("key column '{k}' is not an integer type");
    }
    let (ln, rn) = (lplan.key_columns.len(), rplan.key_columns.len());
    if ln == 0 && rn == 0 {
        return "table has no key".to_string();
    }
    if ln == 0 || rn == 0 {
        return "key is missing on one side".to_string();
    }
    if lplan.key_columns != rplan.key_columns {
        return "key columns differ between the two sides".to_string();
    }
    format!("composite key ({ln} columns)")
}

/// 需要单列整型键的策略不可用时，回退 bucketdiff 并附加可读告警。
fn bucketdiff_fallback(
    key: &Option<String>,
    mut warnings: Vec<String>,
    requirement: &str,
    reason: &str,
) -> Route {
    warnings.push(format!(
        "{reason}; {requirement} — falling back to bucketdiff \
         (row-content multiset comparison)"
    ));
    Route {
        strategy: Box::new(bucket_diff::BucketDiffer),
        key_column: key.clone().unwrap_or_default(),
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ColumnNormSpec;
    use crate::delta_diff::cmd::DeltaDiffArgs;

    fn plan(keys: Vec<&str>, key_ty: &str) -> TablePlan {
        TablePlan {
            key_columns: keys.iter().map(|k| k.to_string()).collect(),
            compare_columns: vec![],
            norm_specs: keys
                .iter()
                .map(|k| ColumnNormSpec {
                    name: k.to_string(),
                    data_type: key_ty.to_string(),
                    nullable: false,
                })
                .collect(),
            warnings: vec![],
        }
    }

    fn conn(url: &str) -> ResolvedConnection {
        ResolvedConnection {
            name: "x".into(),
            connection_url: url.into(),
            password_source: crate::config::PasswordSource::None,
            keyring_username: String::new(),
            config_path: None,
            plaintext_password: None,
            timeout_config: crate::config::TimeoutConfig::default(),
        }
    }

    #[derive(clap::Parser)]
    struct Wrap {
        #[command(flatten)]
        inner: DeltaDiffArgs,
    }

    fn args(strategy: Strategy) -> DeltaDiffArgs {
        use clap::Parser;
        let raw = format!("x --left a --right b --table t --strategy {strategy}");
        Wrap::try_parse_from(raw.split_whitespace().collect::<Vec<_>>())
            .expect("args parse")
            .inner
    }

    #[test]
    fn auto_routes_bucketdiff_for_keyless() {
        let r = route(
            &args(Strategy::Auto),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec![], "int"),
            &plan(vec![], "int"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "bucketdiff");
    }

    #[test]
    fn auto_routes_bucketdiff_for_string_key_with_warning() {
        let r = route(
            &args(Strategy::Auto),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["code"], "varchar(32)"),
            &plan(vec!["code"], "varchar(32)"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "bucketdiff");
        assert!(!r.warnings.is_empty());
        assert!(r.warnings[0].contains("is not an integer type"));
        assert!(!r.warnings[0].contains('§'));
    }

    #[test]
    fn auto_routes_iblt_cross_instance() {
        let r = route(
            &args(Strategy::Auto),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["id"], "int"),
            &plan(vec!["id"], "int"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "iblt");
    }

    #[test]
    fn auto_routes_joindiff_same_connection() {
        let r = route(
            &args(Strategy::Auto),
            &conn("mysql://a/t"),
            &conn("mysql://a/t"),
            &plan(vec!["id"], "int"),
            &plan(vec!["id"], "int"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "joindiff");
    }

    #[test]
    fn explicit_joindiff_falls_back_with_warning_cross_instance() {
        let r = route(
            &args(Strategy::Joindiff),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["id"], "int"),
            &plan(vec!["id"], "int"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "hashdiff");
        assert!(!r.warnings.is_empty());
    }

    #[test]
    fn explicit_hashdiff_falls_back_for_string_key() {
        let r = route(
            &args(Strategy::Hashdiff),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["code"], "varchar(32)"),
            &plan(vec!["code"], "varchar(32)"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "bucketdiff");
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("is not an integer type")));
    }

    #[test]
    fn explicit_hashdiff_falls_back_for_composite_key() {
        let r = route(
            &args(Strategy::Hashdiff),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["id", "tenant_id"], "int"),
            &plan(vec!["id", "tenant_id"], "int"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "bucketdiff");
        assert!(r.warnings.iter().any(|w| w.contains("composite key")));
    }

    #[test]
    fn explicit_hashdiff_falls_back_for_keyless() {
        let r = route(
            &args(Strategy::Hashdiff),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec![], "int"),
            &plan(vec![], "int"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "bucketdiff");
        assert!(r.warnings.iter().any(|w| w.contains("no key")));
    }

    #[test]
    fn explicit_iblt_falls_back_for_string_key() {
        let r = route(
            &args(Strategy::Iblt),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["code"], "varchar(32)"),
            &plan(vec!["code"], "varchar(32)"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "bucketdiff");
        assert!(r
            .warnings
            .iter()
            .any(|w| w.contains("is not an integer type")));
    }

    #[test]
    fn explicit_iblt_falls_back_for_composite_key() {
        let r = route(
            &args(Strategy::Iblt),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["id", "tenant_id"], "int"),
            &plan(vec!["id", "tenant_id"], "int"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "bucketdiff");
        assert!(r.warnings.iter().any(|w| w.contains("composite key")));
    }

    #[test]
    fn explicit_joindiff_cross_instance_nonint_falls_back_to_bucketdiff() {
        let r = route(
            &args(Strategy::Joindiff),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["code"], "varchar(32)"),
            &plan(vec!["code"], "varchar(32)"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "bucketdiff");
        assert!(r.warnings.iter().any(|w| w.contains("joindiff")));
    }

    #[test]
    fn warning_text_has_no_section_ref() {
        let scenarios = [
            (Strategy::Hashdiff, vec!["code"], "varchar(32)"),
            (Strategy::Hashdiff, vec!["id", "tenant_id"], "int"),
            (Strategy::Hashdiff, vec![], "int"),
            (Strategy::Iblt, vec!["code"], "varchar(32)"),
            (Strategy::Iblt, vec!["id", "tenant_id"], "int"),
            (Strategy::Joindiff, vec!["code"], "varchar(32)"),
            (Strategy::Auto, vec!["code"], "varchar(32)"),
        ];
        for (strat, keys, ty) in scenarios {
            let r = route(
                &args(strat),
                &conn("mysql://a/t"),
                &conn("mysql://b/t"),
                &plan(keys.clone(), ty),
                &plan(keys, ty),
            )
            .unwrap();
            for w in &r.warnings {
                assert!(!w.contains('§'), "warning leaks section ref: {w}");
            }
        }
    }

    #[test]
    fn non_bisectable_reason_cases() {
        let varchar = plan(vec!["code"], "varchar(32)");
        let composite = plan(vec!["id", "tenant_id"], "int");
        let keyless = plan(vec![], "int");
        let single = plan(vec!["id"], "int");

        assert_eq!(
            non_bisectable_reason(&varchar, &varchar, &Some("code".into())),
            "key column 'code' is not an integer type"
        );
        assert_eq!(
            non_bisectable_reason(&keyless, &keyless, &None),
            "table has no key"
        );
        assert_eq!(
            non_bisectable_reason(&single, &plan(vec!["uid"], "int"), &None),
            "key columns differ between the two sides"
        );
        assert_eq!(
            non_bisectable_reason(&composite, &composite, &None),
            "composite key (2 columns)"
        );
        assert_eq!(
            non_bisectable_reason(&keyless, &single, &None),
            "key is missing on one side"
        );
    }

    #[test]
    fn non_bisectable_reason_for_excluded_key_column() {
        // --columns excludes the integer PK, so is_int_key cannot see its type.
        let excluded = TablePlan {
            key_columns: vec!["id".to_string()],
            compare_columns: vec!["c1".to_string()],
            norm_specs: vec![ColumnNormSpec {
                name: "c1".to_string(),
                data_type: "int".to_string(),
                nullable: false,
            }],
            warnings: vec![],
        };
        assert_eq!(
            non_bisectable_reason(&excluded, &excluded, &Some("id".into())),
            "key column 'id' is not among the compared columns"
        );
    }

    #[test]
    fn explicit_joindiff_falls_back_for_composite_key() {
        let r = route(
            &args(Strategy::Joindiff),
            &conn("mysql://a/t"),
            &conn("mysql://a/t"),
            &plan(vec!["id", "tenant_id"], "int"),
            &plan(vec!["id", "tenant_id"], "int"),
        )
        .unwrap();
        assert_eq!(r.strategy.name(), "bucketdiff");
        assert!(r.warnings.iter().any(|w| w.contains("composite key")));
    }
}
