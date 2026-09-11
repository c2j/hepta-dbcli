use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthReport {
    pub tables: Vec<TableReport>,
    pub total_rows: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableReport {
    pub name: String,
    pub rows_generated: usize,
    pub columns: Vec<ColumnReport>,
    pub fidelity_score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnReport {
    pub name: String,
    pub logical_type: String,
    pub null_rate: f64,
    pub unique_count: usize,
}

impl SynthReport {
    pub fn new() -> Self {
        Self {
            tables: vec![],
            total_rows: 0,
        }
    }

    pub fn add_table(&mut self, report: TableReport) {
        self.total_rows += report.rows_generated;
        self.tables.push(report);
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize report: {}", e))?;

        std::fs::write(path, json).map_err(|e| format!("write report file: {}", e))?;

        Ok(())
    }

    pub fn summary(&self) -> String {
        let mut lines = vec![];
        lines.push(format!("Synthesis Report"));
        lines.push(format!("================"));
        lines.push(format!("Total rows generated: {}", self.total_rows));
        lines.push(format!("Tables: {}", self.tables.len()));
        lines.push("".to_string());

        for table in &self.tables {
            lines.push(format!("  {} ({} rows)", table.name, table.rows_generated));
            if let Some(score) = table.fidelity_score {
                lines.push(format!("    Fidelity: {:.2}%", score * 100.0));
            }
            lines.push(format!("    Columns: {}", table.columns.len()));
        }

        lines.join("\n")
    }
}

impl Default for SynthReport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_summary_contains_table_names() {
        let mut report = SynthReport::new();
        report.add_table(TableReport {
            name: "users".to_string(),
            rows_generated: 100,
            columns: vec![],
            fidelity_score: Some(0.95),
        });

        let summary = report.summary();
        assert!(summary.contains("users"));
        assert!(summary.contains("100 rows"));
        assert!(summary.contains("95.00%"));
    }

    #[test]
    fn report_total_rows_sums() {
        let mut report = SynthReport::new();
        report.add_table(TableReport {
            name: "a".to_string(),
            rows_generated: 50,
            columns: vec![],
            fidelity_score: None,
        });
        report.add_table(TableReport {
            name: "b".to_string(),
            rows_generated: 75,
            columns: vec![],
            fidelity_score: None,
        });

        assert_eq!(report.total_rows, 125);
    }
}
