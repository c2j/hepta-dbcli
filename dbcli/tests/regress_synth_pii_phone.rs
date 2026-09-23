//! Issue #95 acceptance, no database needed: a hand-written model carrying
//! `pii_phone_style: cn_mobile` must drive `synth generate` to emit CN-format
//! phones with the trained prefix, and a legacy model (no style) must keep
//! the US template. Runs against the real binary, like `regress_cli_flags`.

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

/// A minimal one-table model: numeric `id` plus a PII phone column whose
/// dictionary is the erased placeholder form `synth train` produces.
fn phone_model_json(style_json: Option<&str>) -> String {
    let style_key = style_json
        .map(|json| format!(r#","pii_phone_style": {json}"#))
        .unwrap_or_default();
    format!(
        r#"{{
        "version": 1,
        "table": "users",
        "dialect": "mysql",
        "provenance": {{"source": "native", "converter_version": null, "sdv_version": null}},
        "pk": ["id"],
        "columns": {{
            "id": {{
                "logical_type": "numerical",
                "rounding": null,
                "datetime_epoch": null,
                "min": 1.0,
                "max": 1000.0,
                "null_rate": 0.0,
                "marginal": {{"name": "norm", "loc": 500.0, "scale": 100.0}}
            }},
            "phone": {{
                "logical_type": "categorical",
                "rounding": null,
                "datetime_epoch": null,
                "null_rate": 0.0,
                "marginal": {{"name": "categorical", "values": ["__pii_level_0", "__pii_level_1", "__pii_level_2", "__pii_level_3"], "weights": [0.25, 0.25, 0.25, 0.25]}},
                "pii": "phone"{style_key}
            }}
        }},
        "copula": {{"column_order": ["id", "phone"], "correlation": [[1.0, 0.0], [0.0, 1.0]]}}
    }}"#
    )
}

const RULES_YAML: &str =
    "version: \"1\"\ntables:\n  - name: users\n    rows: 120\n    relationships: []\n";

fn generate_csv(dir: &Path) -> String {
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
    assert!(
        output.status.success(),
        "synth generate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read_to_string(dir.join("out").join("users.csv")).expect("users.csv exists")
}

#[test]
fn cn_model_generates_cn_phones_with_trained_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("models/users.model.json"),
        &phone_model_json(Some(r#"{"style": "cn_mobile", "prefix": [1, 3, 8]}"#)),
    );
    write_file(&dir.path().join("rules.yaml"), RULES_YAML);

    let csv = generate_csv(dir.path());
    let mut cn = 0usize;
    let mut distinct: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in csv.lines().skip(1) {
        let phone = line.split(',').nth(1).expect("phone column");
        assert!(
            phone.starts_with("\"+86-138-") || phone.starts_with("+86-138-"),
            "expected +86-138- prefix in {line:?}"
        );
        cn += 1;
        distinct.insert(phone.to_string());
    }
    assert_eq!(cn, 120, "every row carries a phone");
    assert!(
        distinct.len() > 60,
        "random tails must vary: {} distinct of {cn}",
        distinct.len()
    );
}

#[test]
fn legacy_model_keeps_us_template() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("models/users.model.json"),
        &phone_model_json(None),
    );
    write_file(&dir.path().join("rules.yaml"), RULES_YAML);

    let csv = generate_csv(dir.path());
    for line in csv.lines().skip(1) {
        let phone = line.split(',').nth(1).expect("phone column");
        assert!(
            phone.contains("+1-"),
            "legacy model must keep the US template, got {line:?}"
        );
    }
}

#[test]
fn corrupted_prefix_is_rejected_at_model_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(
        &dir.path().join("models/users.model.json"),
        &phone_model_json(Some(r#"{"style": "cn_mobile", "prefix": [9, 9, 9]}"#)),
    );
    write_file(&dir.path().join("rules.yaml"), RULES_YAML);

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
        ])
        .env_remove("HEPTA_DBCLI_URL")
        .output()
        .expect("run synth generate");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not a mainland mobile prefix"),
        "stderr must explain the rejection: {stderr}"
    );
}
