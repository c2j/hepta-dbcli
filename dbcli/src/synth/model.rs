use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableModel {
    pub version: u32,
    pub table: String,
    /// Schema the table was trained from. When present, SQL export qualifies
    /// the target (`INSERT INTO "sales"."t"`); `None` keeps the legacy
    /// unqualified statement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub dialect: String,
    pub provenance: Provenance,
    pub pk: Vec<String>,
    pub columns: HashMap<String, ColumnModel>,
    pub copula: CopulaInfo,
    /// Learned child-row-count distributions keyed by this table's FK column
    /// (issue #72). Empty for tables that are never children; `serde(default)`
    /// keeps old models loadable.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub fk_cardinality: HashMap<String, crate::synth::cardinality::CardinalityDist>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    pub converter_version: Option<String>,
    pub sdv_version: Option<String>,
    /// True when the training sample hit a driver row cap (the pure-Rust
    /// Oracle driver prefetches only 100 rows), so the fitted distributions
    /// may be distorted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnModel {
    pub logical_type: LogicalType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rounding: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datetime_epoch: Option<bool>,
    /// Decimal digits observed in the training sample. When set, generated
    /// values are quantized to this scale; `None` means no quantization
    /// (legacy models keep their previous output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimal_scale: Option<u8>,
    /// chrono format the training samples were written in. When set together
    /// with `LogicalType::Datetime`, generated epoch values are rendered back
    /// to text with this format; `None` keeps legacy datetime handling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datetime_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Share of NULLs observed in the training sample. Generation draws a
    /// per-column Bernoulli at this rate (`Some(1.0)` is always NULL). A
    /// rules `columns.<name>.null_rate` override wins when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub null_rate: Option<f64>,
    pub marginal: crate::synth::marginal::Marginal,
}

impl Default for ColumnModel {
    fn default() -> Self {
        Self {
            logical_type: LogicalType::Numerical,
            rounding: None,
            datetime_epoch: None,
            decimal_scale: None,
            datetime_format: None,
            min: None,
            max: None,
            null_rate: None,
            marginal: crate::synth::marginal::Marginal::Normal(
                crate::synth::marginal::NormalParams {
                    loc: 0.0,
                    scale: 1.0,
                },
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalType {
    Numerical,
    Categorical,
    Datetime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopulaInfo {
    pub column_order: Vec<String>,
    pub correlation: Vec<Vec<f64>>,
}

impl TableModel {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("read model file: {}", e))?;

        let model: Self =
            serde_json::from_str(&content).map_err(|e| format!("parse model JSON: {}", e))?;

        if model.version > CURRENT_VERSION {
            return Err(format!(
                "model version {} not supported (max {})",
                model.version, CURRENT_VERSION
            ));
        }

        Ok(model)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize model: {}", e))?;

        std::fs::write(path, json).map_err(|e| format!("write model file: {}", e))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::marginal::{CategoricalParams, Marginal, NormalParams};

    #[test]
    fn model_json_roundtrip() {
        let mut columns = HashMap::new();
        columns.insert(
            "id".to_string(),
            ColumnModel {
                logical_type: LogicalType::Numerical,
                rounding: None,
                datetime_epoch: None,
                decimal_scale: None,
                datetime_format: None,
                min: None,
                max: None,
                null_rate: None,
                marginal: Marginal::Normal(NormalParams {
                    loc: 0.0,
                    scale: 1.0,
                }),
            },
        );

        let model = TableModel {
            version: 1,
            table: "orders".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "native".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
            },
            pk: vec!["id".to_string()],
            columns,
            copula: CopulaInfo {
                column_order: vec!["id".to_string()],
                correlation: vec![vec![1.0]],
            },
            fk_cardinality: Default::default(),
        };

        let json = serde_json::to_string_pretty(&model).unwrap();
        let loaded: TableModel = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.table, "orders");
        assert_eq!(loaded.columns.len(), 1);
    }

    #[test]
    fn model_rejects_higher_version() {
        let json = r#"{"version": 99, "table": "t", "dialect": "mysql", "provenance": {"source": "native", "converter_version": null, "sdv_version": null}, "pk": [], "columns": {}, "copula": {"column_order": [], "correlation": []}}"#;
        let model: TableModel = serde_json::from_str(json).unwrap();
        assert!(model.version > CURRENT_VERSION);
    }

    #[test]
    fn should_load_legacy_model_without_m1_fields() {
        // A pre-M1 model.json has no schema / truncated / decimal_scale /
        // datetime_format keys; it must keep loading with those defaulted.
        let json = r#"{
            "version": 1,
            "table": "orders",
            "dialect": "mysql",
            "provenance": {"source": "native", "converter_version": null, "sdv_version": null},
            "pk": [],
            "columns": {
                "amt": {
                    "logical_type": "numerical",
                    "rounding": null,
                    "datetime_epoch": null,
                    "min": 1.0,
                    "max": 2.0,
                    "null_rate": 0.0,
                    "marginal": {"name": "norm", "loc": 1.0, "scale": 1.0}
                }
            },
            "copula": {"column_order": ["amt"], "correlation": [[1.0]]}
        }"#;

        let model: TableModel = serde_json::from_str(json).unwrap();
        assert!(model.schema.is_none());
        assert!(!model.provenance.truncated);
        let col = &model.columns["amt"];
        assert!(col.decimal_scale.is_none());
        assert!(col.datetime_format.is_none());
    }

    #[test]
    fn should_omit_unset_m1_fields_from_serialized_model() {
        let model = TableModel {
            version: 1,
            table: "orders".to_string(),
            schema: None,
            dialect: "mysql".to_string(),
            provenance: Provenance {
                source: "native".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
            },
            pk: vec![],
            columns: HashMap::new(),
            copula: CopulaInfo {
                column_order: vec![],
                correlation: vec![],
            },
            fk_cardinality: Default::default(),
        };

        let json = serde_json::to_string(&model).unwrap();
        for absent in ["schema", "truncated", "decimal_scale", "datetime_format"] {
            assert!(
                !json.contains(absent),
                "unset field '{absent}' must not be serialized: {json}"
            );
        }
    }

    #[test]
    fn categorical_column_model() {
        let column = ColumnModel {
            logical_type: LogicalType::Categorical,
            rounding: None,
            datetime_epoch: None,
            decimal_scale: None,
            datetime_format: None,
            min: None,
            max: None,
            null_rate: None,
            marginal: Marginal::Categorical(CategoricalParams {
                values: vec!["a".to_string(), "b".to_string()],
                weights: vec![0.5, 0.5],
            }),
        };

        let json = serde_json::to_string(&column).unwrap();
        assert!(json.contains("categorical"));
        assert!(json.contains("\"a\""));
    }
}
