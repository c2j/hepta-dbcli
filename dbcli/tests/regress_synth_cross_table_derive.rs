//! Issue #117 acceptance, no database needed: a child table whose rule
//! derives `vol = parent.cjsl / 1000` from the referenced parent must emit
//! rows where `vol * 1000` equals the parent `cjsl` the FK points at, and an
//! unknown `parent.<col>` must fail at load time with a precise message.
//! Runs against the real binary, like `regress_cli_flags`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_hepta_dbcli");

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn model_json(table: &str, columns: &str, order: &str) -> String {
    format!(
        r#"{{
        "version": 1,
        "table": "{table}",
        "dialect": "mysql",
        "provenance": {{"source": "native", "converter_version": null, "sdv_version": null}},
        "pk": ["id"],
        "columns": {columns},
        "copula": {{"column_order": {order}, "correlation": [[1.0, 0.0], [0.0, 1.0]]}}
    }}"#
    )
}

/// Parent `par(id, cjsl)`: integral columns (`rounding: 0`) so the acceptance
/// equation is exact in f64 space.
fn par_model() -> String {
    model_json(
        "par",
        r#"{
            "id": {
                "logical_type": "numerical",
                "rounding": 0,
                "datetime_epoch": null,
                "min": 1.0,
                "max": 5.0,
                "null_rate": 0.0,
                "marginal": {"name": "uniform", "low": 1.0, "high": 5.0}
            },
            "cjsl": {
                "logical_type": "numerical",
                "rounding": 0,
                "datetime_epoch": null,
                "min": 1000.0,
                "max": 9000.0,
                "null_rate": 0.0,
                "marginal": {"name": "uniform", "low": 1000.0, "high": 9000.0}
            }
        }"#,
        r#"["id", "cjsl"]"#,
    )
}

/// Child `zgh(fk, vol)`.
fn zgh_model() -> String {
    model_json(
        "zgh",
        r#"{
            "fk": {
                "logical_type": "numerical",
                "rounding": 0,
                "datetime_epoch": null,
                "min": 1.0,
                "max": 5.0,
                "null_rate": 0.0,
                "marginal": {"name": "uniform", "low": 1.0, "high": 5.0}
            },
            "vol": {
                "logical_type": "numerical",
                "rounding": null,
                "datetime_epoch": null,
                "min": 0.0,
                "max": 100.0,
                "null_rate": 0.0,
                "marginal": {"name": "norm", "loc": 5.0, "scale": 1.0}
            }
        }"#,
        r#"["fk", "vol"]"#,
    )
}

const RULES_YAML: &str = r#"version: "1"
tables:
  - name: par
    rows: 5
    relationships: []
  - name: zgh
    rows: 40
    relationships:
      - pk: fk
        references: [par.id]
        derive:
          - column: vol
            expr: "parent.cjsl / 1000"
"#;

fn write_fixtures(dir: &Path, rules: &str) {
    write_file(&dir.join("models/par.model.json"), &par_model());
    write_file(&dir.join("models/zgh.model.json"), &zgh_model());
    write_file(&dir.join("rules.yaml"), rules);
}

fn run_generate(dir: &Path) -> Result<(), String> {
    write_fixtures(dir, RULES_YAML);
    let output = Command::new(BIN)
        .args([
            "synth",
            "generate",
            "--models",
            dir.join("models").to_str().unwrap(),
            "--rules",
            dir.join("rules.yaml").to_str().unwrap(),
            "--output",
            dir.join("out").to_str().unwrap(),
            "--format",
            "csv",
            "--seed",
            "7",
        ])
        .env_remove("HEPTA_DBCLI_URL")
        .output()
        .expect("run synth generate");
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn parse_csv(csv: &str) -> Vec<HashMap<String, String>> {
    let mut lines = csv.lines();
    let header: Vec<String> = lines
        .next()
        .expect("header row")
        .split(',')
        .map(str::to_string)
        .collect();
    lines
        .map(|line| {
            header
                .iter()
                .zip(line.split(','))
                .map(|(column, value)| (column.clone(), value.to_string()))
                .collect()
        })
        .collect()
}

#[test]
fn vol_mirrors_parent_cjsl_over_the_fk() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_generate(dir.path()).unwrap_or_else(|error| panic!("generate failed: {error}"));

    let zgh = parse_csv(&fs::read_to_string(dir.path().join("out/zgh.csv")).unwrap());
    let par = parse_csv(&fs::read_to_string(dir.path().join("out/par.csv")).unwrap());
    let cjsl_by_id: HashMap<String, f64> = par
        .iter()
        .map(|row| {
            (
                row["id"].clone(),
                row["cjsl"].parse::<f64>().expect("cjsl numeric"),
            )
        })
        .collect();
    assert!(!zgh.is_empty(), "child must have rows");
    for row in &zgh {
        let fk = &row["fk"];
        let vol = row["vol"].parse::<f64>().expect("vol numeric");
        let cjsl = cjsl_by_id
            .get(fk)
            .unwrap_or_else(|| panic!("fk {fk} must exist in par"));
        assert!(
            (vol * 1000.0 - cjsl).abs() < 1e-6,
            "vol {vol} must mirror par.cjsl {cjsl} / 1000 (fk {fk})"
        );
    }
}

#[test]
fn unknown_parent_column_is_rejected_at_load_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rules = RULES_YAML.replace("parent.cjsl", "parent.ghost");
    write_fixtures(dir.path(), &rules);
    let output = Command::new(BIN)
        .args([
            "synth",
            "generate",
            "--models",
            dir.path().join("models").to_str().unwrap(),
            "--rules",
            dir.path().join("rules.yaml").to_str().unwrap(),
            "--output",
            dir.path().join("out").to_str().unwrap(),
            "--format",
            "csv",
            "--seed",
            "7",
        ])
        .env_remove("HEPTA_DBCLI_URL")
        .output()
        .expect("run synth generate");
    assert!(
        !output.status.success(),
        "unknown parent column must be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ghost"),
        "error must name the column: {stderr}"
    );
    assert!(
        stderr.contains("zgh"),
        "error must name the table: {stderr}"
    );
    assert!(
        stderr.contains("par"),
        "error must name the parent table: {stderr}"
    );
}

#[test]
fn table_level_derive_still_rejects_parent_references() {
    // The grammar accepts `parent.` in any derive expression, but only a
    // relationship has a snapshot to resolve it against.
    let dir = tempfile::tempdir().expect("tempdir");
    let rules = RULES_YAML.replace(
        r#"  - name: zgh
    rows: 40
    relationships:
      - pk: fk
        references: [par.id]
        derive:
          - column: vol
            expr: "parent.cjsl / 1000"
"#,
        r#"  - name: zgh
    rows: 40
    derive:
      - column: vol
        expr: "parent.cjsl / 1000"
    relationships:
      - pk: fk
        references: [par.id]
"#,
    );
    write_fixtures(dir.path(), &rules);
    let output = Command::new(BIN)
        .args([
            "synth",
            "generate",
            "--models",
            dir.path().join("models").to_str().unwrap(),
            "--rules",
            dir.path().join("rules.yaml").to_str().unwrap(),
            "--output",
            dir.path().join("out").to_str().unwrap(),
            "--format",
            "csv",
            "--seed",
            "7",
        ])
        .env_remove("HEPTA_DBCLI_URL")
        .output()
        .expect("run synth generate");
    assert!(
        !output.status.success(),
        "table-level parent reference must be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("parent.<col>"),
        "error must name the construct: {stderr}"
    );
    assert!(
        stderr.contains("relationship"),
        "error must point at the relationship list: {stderr}"
    );
}
