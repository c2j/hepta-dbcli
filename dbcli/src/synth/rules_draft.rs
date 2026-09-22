use crate::synth::rules::{PoolStrategy, Relationship, SynthRules, TableRule};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    pub from_table: String,
    pub from_column: String,
    pub to_table: String,
    pub to_column: String,
}

pub fn generate_rules_draft(
    tables: &[String],
    foreign_keys: &[ForeignKeyInfo],
    table_stats: &std::collections::HashMap<String, TableStats>,
) -> SynthRules {
    let mut table_rules: std::collections::HashMap<String, TableRule> =
        std::collections::HashMap::new();

    for table in tables {
        table_rules.insert(
            table.clone(),
            TableRule {
                name: table.clone(),
                columns: std::collections::HashMap::new(),
                derive: vec![],
                branches: vec![],
                rows: None,
                relationships: vec![],
                strategy: crate::synth::rules::TableStrategy::default(),
            },
        );
    }

    for fk in foreign_keys {
        let unique = is_unique(fk, table_stats);

        let relationship = Relationship {
            pk: fk.from_column.clone(),
            references: vec![format!("{}.{}", fk.to_table, fk.to_column)],
            pool_strategy: PoolStrategy::Projection { unique },
            null_label: "null".to_string(),
            cardinality: Default::default(),
            derive: Vec::new(),
        };

        if let Some(rule) = table_rules.get_mut(&fk.from_table) {
            rule.relationships.push(relationship);
        }
    }

    let mut tables: Vec<TableRule> = table_rules.into_values().collect();
    tables.sort_by(|a, b| a.name.cmp(&b.name));

    SynthRules {
        version: "1".to_string(),
        tables,
    }
}

fn is_unique(
    fk: &ForeignKeyInfo,
    table_stats: &std::collections::HashMap<String, TableStats>,
) -> bool {
    if let Some(stats) = table_stats.get(&fk.from_table) {
        if let Some(col_stats) = stats.columns.get(&fk.from_column) {
            return col_stats.cardinality == stats.row_count;
        }
    }
    false
}

/// Infer implicit relationships for databases without (or with only partial)
/// foreign keys (issue #76-E, AC7).
///
/// The heuristic is deliberately strict: a child column is inferred only when
/// a *different* table has a column with the exact same name whose profile
/// cardinality equals that table's row count (a unique key), and the column is
/// not the child's own primary key. Suffix guessing and fuzzy name matching are
/// out of scope by design. Ambiguous matches (more than one candidate parent)
/// are skipped, and columns already covered by a database foreign key are never
/// re-inferred. Results are sorted, so the draft stays deterministic.
///
/// Inferred relationships project from the parent pool without replacement
/// only when the child column is itself unique (`cardinality == row_count`),
/// matching the explicit-FK path. A 1:N child column must stay `unique: false`,
/// otherwise `synth generate` would demand one parent row per child row.
pub fn infer_implicit_relationships(
    tables: &[String],
    foreign_keys: &[ForeignKeyInfo],
    profiles: &std::collections::HashMap<String, crate::synth::profile::TableProfile>,
    primary_keys: &std::collections::HashMap<String, String>,
) -> Vec<ForeignKeyInfo> {
    let mut inferred = Vec::new();

    for child in tables {
        let Some(child_profile) = profiles.get(child) else {
            continue;
        };
        let mut child_columns: Vec<&String> = child_profile.columns.keys().collect();
        child_columns.sort();

        for column in child_columns {
            if primary_keys.get(child).map(String::as_str) == Some(column.as_str()) {
                continue;
            }
            // Same-name timestamp columns (`last_update`) are unique per row
            // by construction; inferring them as keys links tables into
            // mutual references and `generate` fails with a cycle.
            if child_profile
                .columns
                .get(column)
                .map(|stats| stats.logical_type == "datetime")
                .unwrap_or(false)
            {
                continue;
            }
            if foreign_keys
                .iter()
                .any(|fk| fk.from_table == *child && fk.from_column == *column)
            {
                continue;
            }

            let mut parents: Vec<&str> = Vec::new();
            for parent in tables {
                if parent == child {
                    continue;
                }
                if is_unique_datetime_safe(profiles.get(parent), column) {
                    parents.push(parent.as_str());
                }
            }
            if parents.len() != 1 {
                continue;
            }

            inferred.push(ForeignKeyInfo {
                from_table: child.clone(),
                from_column: column.clone(),
                to_table: parents[0].to_string(),
                to_column: column.clone(),
            });
        }
    }

    inferred.sort_by(|a, b| {
        (&a.from_table, &a.from_column, &a.to_table, &a.to_column).cmp(&(
            &b.from_table,
            &b.from_column,
            &b.to_table,
            &b.to_column,
        ))
    });
    inferred
}

/// True when `profile` carries `column` and that column is unique in its table
/// (`cardinality == row_count`), i.e. a plausible referenced key.
fn is_unique_key(profile: Option<&crate::synth::profile::TableProfile>, column: &str) -> bool {
    let Some(profile) = profile else {
        return false;
    };
    if profile.row_count == 0 {
        return false;
    }
    profile
        .columns
        .get(column)
        .map(|stats| stats.cardinality == profile.row_count)
        .unwrap_or(false)
}

/// Like [`is_unique_key`], but datetime columns never qualify as a parent
/// key: a per-row-unique timestamp is an artifact of `last_update`-style
/// bookkeeping, not a join key.
fn is_unique_datetime_safe(
    profile: Option<&crate::synth::profile::TableProfile>,
    column: &str,
) -> bool {
    if let Some(profile) = profile {
        if profile
            .columns
            .get(column)
            .map(|stats| stats.logical_type == "datetime")
            .unwrap_or(false)
        {
            return false;
        }
    }
    is_unique_key(profile, column)
}

/// `generate_draft_from_profiles` plus the implicit relationships from
/// [`infer_implicit_relationships`]. Returns the draft and the number of
/// inferred relationships (for the `inferred N implicit relationship(s)`
/// summary).
pub fn generate_draft_with_implicit(
    tables: &[String],
    foreign_keys: &[ForeignKeyInfo],
    profiles: &std::collections::HashMap<String, crate::synth::profile::TableProfile>,
    primary_keys: &std::collections::HashMap<String, String>,
) -> (SynthRules, usize) {
    let mut rules = generate_draft_from_profiles(tables, foreign_keys, profiles);
    let inferred = infer_implicit_relationships(tables, foreign_keys, profiles, primary_keys);
    let table_stats = table_stats_from_profiles(profiles);

    for fk in &inferred {
        if let Some(rule) = rules.tables.iter_mut().find(|t| t.name == fk.from_table) {
            rule.relationships.push(Relationship {
                pk: fk.from_column.clone(),
                references: vec![format!("{}.{}", fk.to_table, fk.to_column)],
                pool_strategy: PoolStrategy::Projection {
                    unique: is_unique(fk, &table_stats),
                },
                null_label: "null".to_string(),
                cardinality: Default::default(),
                derive: Vec::new(),
            });
        }
    }

    (rules, inferred.len())
}

pub struct TableStats {
    pub row_count: usize,
    pub columns: std::collections::HashMap<String, ColumnStats>,
}

pub struct ColumnStats {
    pub cardinality: usize,
}

pub fn generate_draft_from_profiles(
    tables: &[String],
    foreign_keys: &[ForeignKeyInfo],
    profiles: &std::collections::HashMap<String, crate::synth::profile::TableProfile>,
) -> SynthRules {
    let table_stats = table_stats_from_profiles(profiles);
    generate_rules_draft(tables, foreign_keys, &table_stats)
}

/// Column cardinalities keyed by table, used to decide whether a projected
/// relationship column is unique.
fn table_stats_from_profiles(
    profiles: &std::collections::HashMap<String, crate::synth::profile::TableProfile>,
) -> std::collections::HashMap<String, TableStats> {
    profiles
        .iter()
        .map(|(name, profile)| {
            let columns: std::collections::HashMap<String, ColumnStats> = profile
                .columns
                .iter()
                .map(|(col_name, col_profile)| {
                    (
                        col_name.clone(),
                        ColumnStats {
                            cardinality: col_profile.cardinality,
                        },
                    )
                })
                .collect();

            (
                name.clone(),
                TableStats {
                    row_count: profile.row_count,
                    columns,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::profile::TableProfile;
    use serde_json::json;

    /// `(table, row_count, [(column, cardinality)])`.
    type ProfileSpec<'a> = (&'a str, usize, &'a [(&'a str, usize)]);

    /// Profile fixtures. Built through serde so the helper does not depend on
    /// `ColumnProfile`'s field list.
    fn profiles(spec: &[ProfileSpec<'_>]) -> std::collections::HashMap<String, TableProfile> {
        spec.iter()
            .map(|(table, row_count, columns)| {
                let columns: serde_json::Map<String, serde_json::Value> = columns
                    .iter()
                    .map(|(name, cardinality)| {
                        (
                            (*name).to_string(),
                            json!({
                                "logical_type": "categorical",
                                "null_rate": 0.0,
                                "cardinality": cardinality,
                                "min": null,
                                "max": null,
                                "mean": null,
                                "std_dev": null,
                            }),
                        )
                    })
                    .collect();
                let profile: TableProfile = serde_json::from_value(json!({
                    "table": table,
                    "row_count": row_count,
                    "columns": columns,
                }))
                .expect("test profile");
                ((*table).to_string(), profile)
            })
            .collect()
    }

    #[test]
    fn draft_creates_table_rules_for_all_tables() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let fks = vec![];
        let stats = std::collections::HashMap::new();

        let rules = generate_rules_draft(&tables, &fks, &stats);
        assert_eq!(rules.tables.len(), 2);
        assert!(rules.tables.iter().any(|t| t.name == "users"));
        assert!(rules.tables.iter().any(|t| t.name == "orders"));
    }

    #[test]
    fn draft_adds_relationships_for_foreign_keys() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let fks = vec![ForeignKeyInfo {
            from_table: "orders".to_string(),
            from_column: "user_id".to_string(),
            to_table: "users".to_string(),
            to_column: "id".to_string(),
        }];
        let stats = std::collections::HashMap::new();

        let rules = generate_rules_draft(&tables, &fks, &stats);
        let orders_rule = rules.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders_rule.relationships.len(), 1);
        assert_eq!(orders_rule.relationships[0].pk, "user_id");
        assert_eq!(
            orders_rule.relationships[0].references,
            vec!["users.id".to_string()]
        );
    }

    #[test]
    fn should_infer_implicit_relationship_from_unique_column_name() {
        // Both sides are unique here (1:1): `users.account_no` is 100/100 and
        // `orders.account_no` is 300/300. Both are their table's primary key, so
        // the heuristic only infers the child direction.
        let tables = vec!["users".to_string(), "orders".to_string()];
        let profiles = profiles(&[
            ("users", 100, &[("account_no", 100)]),
            ("orders", 300, &[("order_id", 300), ("account_no", 300)]),
        ]);
        let primary_keys = std::collections::HashMap::from([
            ("orders".to_string(), "order_id".to_string()),
            ("users".to_string(), "account_no".to_string()),
        ]);

        let inferred = infer_implicit_relationships(&tables, &[], &profiles, &primary_keys);

        assert_eq!(inferred.len(), 1, "expected exactly one inferred FK");
        assert_eq!(inferred[0].from_table, "orders");
        assert_eq!(inferred[0].from_column, "account_no");
        assert_eq!(inferred[0].to_table, "users");
        assert_eq!(inferred[0].to_column, "account_no");

        let (rules, count) = generate_draft_with_implicit(&tables, &[], &profiles, &primary_keys);
        assert_eq!(count, 1);
        let orders = rules.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders.relationships.len(), 1);
        assert_eq!(orders.relationships[0].pk, "account_no");
        assert_eq!(
            orders.relationships[0].references,
            vec!["users.account_no".to_string()]
        );
        match &orders.relationships[0].pool_strategy {
            PoolStrategy::Projection { unique } => assert!(*unique, "inferred FK projects unique"),
            other => panic!("expected Projection pool strategy, got {other:?}"),
        }
    }

    #[test]
    fn should_not_mark_implicit_one_to_many_relationship_as_unique() {
        // `users.account_no` is unique (100 distinct / 100 rows) but
        // `orders.account_no` is 1:N (80 distinct / 300 rows). The inferred
        // relationship must not claim `unique` on the child column, otherwise
        // `synth generate` would demand 300 distinct parent keys from a pool
        // of 100 and fail.
        let tables = vec!["users".to_string(), "orders".to_string()];
        let profiles = profiles(&[
            ("users", 100, &[("account_no", 100)]),
            ("orders", 300, &[("order_id", 300), ("account_no", 80)]),
        ]);
        let primary_keys =
            std::collections::HashMap::from([("orders".to_string(), "order_id".to_string())]);

        let (rules, count) = generate_draft_with_implicit(&tables, &[], &profiles, &primary_keys);
        assert_eq!(count, 1);

        let orders = rules.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders.relationships.len(), 1);
        match &orders.relationships[0].pool_strategy {
            PoolStrategy::Projection { unique } => assert!(
                !*unique,
                "1:N child column (80 distinct / 300 rows) must not be marked unique"
            ),
            other => panic!("expected Projection pool strategy, got {other:?}"),
        }
    }

    #[test]
    fn should_not_infer_implicit_relationship_when_parent_column_not_unique() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        // `account_no` is not unique in `users`: 90 distinct values over 100.
        let profiles = profiles(&[
            ("users", 100, &[("account_no", 90)]),
            ("orders", 300, &[("account_no", 120)]),
        ]);

        let inferred = infer_implicit_relationships(
            &tables,
            &[],
            &profiles,
            &std::collections::HashMap::new(),
        );

        assert!(
            inferred.is_empty(),
            "non-unique parent column must not match"
        );
    }

    #[test]
    fn should_dedupe_inferred_relationship_against_database_fk() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let profiles = profiles(&[
            ("users", 100, &[("account_no", 100)]),
            ("orders", 300, &[("account_no", 120)]),
        ]);
        let explicit = vec![ForeignKeyInfo {
            from_table: "orders".to_string(),
            from_column: "account_no".to_string(),
            to_table: "users".to_string(),
            to_column: "account_no".to_string(),
        }];

        let inferred = infer_implicit_relationships(
            &tables,
            &explicit,
            &profiles,
            &std::collections::HashMap::new(),
        );
        assert!(inferred.is_empty(), "database FK must not be re-inferred");

        let (rules, count) = generate_draft_with_implicit(
            &tables,
            &explicit,
            &profiles,
            &std::collections::HashMap::new(),
        );
        assert_eq!(count, 0);
        let orders = rules.tables.iter().find(|t| t.name == "orders").unwrap();
        assert_eq!(orders.relationships.len(), 1, "no duplicate relationship");
    }

    #[test]
    fn should_not_infer_implicit_relationship_for_child_primary_key() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let profiles = profiles(&[
            ("users", 100, &[("order_id", 100)]),
            ("orders", 300, &[("order_id", 150)]),
        ]);
        let primary_keys =
            std::collections::HashMap::from([("orders".to_string(), "order_id".to_string())]);

        let inferred = infer_implicit_relationships(&tables, &[], &profiles, &primary_keys);

        assert!(
            inferred.is_empty(),
            "the child's own primary key is not a foreign key"
        );
    }

    #[test]
    fn should_skip_ambiguous_implicit_relationship_parents() {
        let tables = vec!["a".to_string(), "b".to_string(), "child".to_string()];
        let profiles = profiles(&[
            ("a", 100, &[("code", 100)]),
            ("b", 100, &[("code", 100)]),
            ("child", 300, &[("code", 40)]),
        ]);
        // `a` and `b` are marked primary-keyed so only the ambiguous match from
        // `child` is exercised.
        let primary_keys = std::collections::HashMap::from([
            ("a".to_string(), "code".to_string()),
            ("b".to_string(), "code".to_string()),
        ]);

        let inferred = infer_implicit_relationships(&tables, &[], &profiles, &primary_keys);

        assert!(
            inferred.is_empty(),
            "two unique parents for the same name is ambiguous"
        );
    }

    #[test]
    fn should_leave_draft_without_profiles_unchanged() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let (rules, count) = generate_draft_with_implicit(
            &tables,
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert_eq!(count, 0);
        assert!(rules.tables.iter().all(|t| t.relationships.is_empty()));
    }

    #[test]
    fn draft_detects_unique_foreign_keys() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let fks = vec![ForeignKeyInfo {
            from_table: "orders".to_string(),
            from_column: "user_id".to_string(),
            to_table: "users".to_string(),
            to_column: "id".to_string(),
        }];

        let mut columns = std::collections::HashMap::new();
        columns.insert("user_id".to_string(), ColumnStats { cardinality: 100 });
        let mut stats = std::collections::HashMap::new();
        stats.insert(
            "orders".to_string(),
            TableStats {
                row_count: 100,
                columns,
            },
        );

        let rules = generate_rules_draft(&tables, &fks, &stats);
        let orders_rule = rules.tables.iter().find(|t| t.name == "orders").unwrap();
        if let PoolStrategy::Projection { unique } = orders_rule.relationships[0].pool_strategy {
            assert!(unique);
        } else {
            panic!("expected Projection pool strategy");
        }
    }

    fn profile_with_types(
        table: &str,
        row_count: usize,
        columns: &[(&str, usize, &str)],
    ) -> TableProfile {
        let columns_json: serde_json::Map<String, serde_json::Value> = columns
            .iter()
            .map(|(name, cardinality, logical_type)| {
                (
                    (*name).to_string(),
                    json!({
                        "logical_type": logical_type,
                        "null_rate": 0.0,
                        "cardinality": cardinality,
                        "min": null,
                        "max": null,
                        "mean": null,
                        "std_dev": null,
                    }),
                )
            })
            .collect();
        serde_json::from_value(json!({
            "table": table,
            "row_count": row_count,
            "columns": columns_json,
        }))
        .expect("test profile")
    }

    /// Same-name timestamp columns (`last_update` on customer/rental/...) are
    /// unique per row by construction, so the implicit heuristic links them
    /// into mutual references and `generate` dies with a cycle (real pagila
    /// data; see UserGuide §10.5 limitation #3). Datetime columns must never
    /// be inferred as relationship keys.
    #[test]
    fn should_not_infer_datetime_columns_as_implicit_relationships() {
        let tables = vec!["customer".to_string(), "rental".to_string()];
        let mut profiles = std::collections::HashMap::new();
        profiles.insert(
            "customer".to_string(),
            profile_with_types(
                "customer",
                602,
                &[
                    ("customer_id", 602, "numerical"),
                    ("last_update", 602, "datetime"),
                ],
            ),
        );
        profiles.insert(
            "rental".to_string(),
            profile_with_types(
                "rental",
                17679,
                &[
                    ("rental_id", 17679, "numerical"),
                    ("last_update", 17679, "datetime"),
                ],
            ),
        );
        let primary_keys = std::collections::HashMap::from([
            ("customer".to_string(), "customer_id".to_string()),
            ("rental".to_string(), "rental_id".to_string()),
        ]);

        let inferred = infer_implicit_relationships(&tables, &[], &profiles, &primary_keys);
        assert!(
            inferred.is_empty(),
            "unique datetime columns must not be inferred as FKs: {inferred:?}"
        );
    }

    /// The datetime guard must not over-fire: a *non-datetime* unique column
    /// keeps the existing 1:1 inference.
    #[test]
    fn should_still_infer_non_datetime_unique_columns() {
        let tables = vec!["users".to_string(), "orders".to_string()];
        let mut profiles = std::collections::HashMap::new();
        profiles.insert(
            "users".to_string(),
            profile_with_types("users", 100, &[("account_no", 100, "categorical")]),
        );
        profiles.insert(
            "orders".to_string(),
            profile_with_types(
                "orders",
                300,
                &[
                    ("order_id", 300, "categorical"),
                    ("account_no", 300, "categorical"),
                ],
            ),
        );
        let primary_keys =
            std::collections::HashMap::from([("orders".to_string(), "order_id".to_string())]);

        let inferred = infer_implicit_relationships(&tables, &[], &profiles, &primary_keys);
        assert_eq!(
            inferred.len(),
            2,
            "both order_id->users.account_no style links inferred"
        );
        assert!(inferred
            .iter()
            .any(|fk| fk.from_column == "account_no" && fk.to_table == "users"));
    }
}
