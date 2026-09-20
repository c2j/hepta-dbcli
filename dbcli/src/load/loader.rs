//! Load execution (issue #98). Implemented in a later wave: this skeleton
//! owns the plan, the gate, and the audit trail; the executor turns each
//! `PlanEntry` into parameterized INSERTs.

use super::plan::LoadPlan;
use crate::backend::DbConn;

/// Execute the plan against `conn`, returning the number of rows inserted.
/// Not implemented in this wave (worker D owns it): every invocation fails
/// closed after the plan and audit intent are on disk.
pub(crate) async fn execute(_conn: &mut dyn DbConn, plan: &LoadPlan) -> Result<u64, String> {
    let _ = plan;
    Err("loader not implemented yet".to_string())
}
