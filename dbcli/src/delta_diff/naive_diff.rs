// ─── delta-diff NaiveDiffer: one full scan per side + client merge (#87) ──
//
// Per side a single `SELECT … WHERE …` full scan (raw key ORDER BY, no
// NLSSORT/COLLATE, no LIMIT); rows are matched client-side by canonical
// fingerprints (HashMap join), so correctness never depends on the server
// row order. Filled in by the naivediff implementation tasks; the `diff`
// body is a placeholder until then.

use crate::backend::{DbConn, DbError};
use crate::delta_diff::report::DiffReport;
use crate::delta_diff::strategy::{DiffContext, DiffStrategy};

pub(crate) struct NaiveDiffer;

#[async_trait::async_trait]
impl DiffStrategy for NaiveDiffer {
    fn name(&self) -> &'static str {
        "naivediff"
    }

    async fn diff(
        &self,
        _left: &mut (dyn DbConn + Send),
        _right: &mut (dyn DbConn + Send),
        _ctx: &DiffContext,
    ) -> Result<DiffReport, DbError> {
        todo!("naivediff end-to-end flow lands with the merge implementation tasks")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn compare_primitives_are_shared_across_modules() {
        assert!(crate::delta_diff::rowdiff::row_values_equal(
            &[json!(1)],
            &[json!(1)],
            &[true]
        ));
    }
}
