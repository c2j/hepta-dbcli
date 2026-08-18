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
use crate::delta_diff::{bucket_diff, hash_diff, iblt_diff, join_diff};

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
            let k = key.clone().filter(|_| bisectable).ok_or_else(|| {
                "hashdiff requires a single bisectable integer key (§6.4)".to_string()
            })?;
            return Ok(Route {
                strategy: Box::new(hash_diff::HashDiffer),
                key_column: k,
                warnings,
            });
        }
        Strategy::Bucketdiff => {
            return Ok(Route {
                strategy: Box::new(bucket_diff::BucketDiffer),
                key_column: key.clone().unwrap_or_default(),
                warnings,
            });
        }
        Strategy::Joindiff => {
            let k = key
                .clone()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| "joindiff requires a comparison key".to_string())?;
            if !same_conn || !mysql_family {
                warnings.push(
                    "joindiff requires same-connection MySQL-family; falling back to hashdiff"
                        .to_string(),
                );
                if !bisectable {
                    return Err("hashdiff fallback requires a bisectable integer key".into());
                }
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
            (Some(k), false) => {
                warnings.push(format!(
                    "key '{k}' is not a bisectable integer column (§6.4); falling back to bucketdiff"
                ));
                Box::new(bucket_diff::BucketDiffer)
            }
            (Some(_), true) if same_conn && mysql_family => Box::new(join_diff::JoinDiffer),
            (Some(_), true) => Box::new(iblt_diff::IbltDiffer),
        },
        Strategy::Iblt => {
            let k = key.clone().filter(|_| bisectable).ok_or_else(|| {
                "iblt requires a single bisectable integer key (§6.4)".to_string()
            })?;
            return Ok(Route {
                strategy: Box::new(iblt_diff::IbltDiffer),
                key_column: k,
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
    fn explicit_iblt_requires_bisectable_key() {
        assert!(route(
            &args(Strategy::Iblt),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["code"], "varchar(32)"),
            &plan(vec!["code"], "varchar(32)")
        )
        .is_err());
    }

    #[test]
    fn explicit_hashdiff_requires_bisectable_key() {
        assert!(route(
            &args(Strategy::Hashdiff),
            &conn("mysql://a/t"),
            &conn("mysql://b/t"),
            &plan(vec!["code"], "varchar(32)"),
            &plan(vec!["code"], "varchar(32)")
        )
        .is_err());
    }
}
