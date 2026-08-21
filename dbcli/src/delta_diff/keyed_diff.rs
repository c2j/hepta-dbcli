// ─── delta-diff KeyedDiffer: composite/string key identity diff ────────
//
// COUNT both sides → empty-side short-circuit → FETCH_ALL or
// scan-once bucket checksum + composite keyset merge.
// Not used for keyless tables (those stay on bucketdiff).

use crate::backend::{DbConn, DbError};
use crate::delta_diff::report::DiffReport;
use crate::delta_diff::strategy::{DiffContext, DiffStrategy};

pub(crate) struct KeyedDiffer;

#[async_trait::async_trait]
impl DiffStrategy for KeyedDiffer {
    fn name(&self) -> &'static str {
        "keyeddiff"
    }

    async fn diff(
        &self,
        _left: &mut (dyn DbConn + Send),
        _right: &mut (dyn DbConn + Send),
        _ctx: &DiffContext,
    ) -> Result<DiffReport, DbError> {
        Err(DbError::unsupported("keyeddiff is not implemented yet"))
    }
}
