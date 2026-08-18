// ─── delta-diff recheck: 差异行二次复核（v2.1 §8.3）─────────────────────
//
// 行级 diff 产出的每条差异，在快照提交后按 key 向两侧各发一次当前读点查：
// 复核一致 → 比对窗口内的并发写入（伪差异），confirmed=false 且不计入统计；
// 复核仍不一致 → confirmed=true。点查量 = 差异行数，与总行数无关。

use serde_json::Value;

use crate::backend::{DbConn, DbError, KeysetPageSpec};
use crate::delta_diff::report::DiffRow;

/// 对差异行逐条复核；返回确认后的差异数（confirmed=true 的条数）。
pub(crate) async fn recheck_diffs(
    left: &mut (dyn DbConn + Send),
    right: &mut (dyn DbConn + Send),
    left_spec: &KeysetPageSpec,
    right_spec: &KeysetPageSpec,
    diffs: &mut [DiffRow],
) -> Result<u64, DbError> {
    let mut confirmed = 0u64;
    for d in diffs.iter_mut() {
        let key = match &d.key {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        };
        let Some(k) = key else {
            d.confirmed = true;
            confirmed += 1;
            continue;
        };
        let lrow = point_query(left, left_spec, k).await?;
        let rrow = point_query(right, right_spec, k).await?;
        let now_equal = match (&lrow, &rrow) {
            (Some(l), Some(r)) => l[1..] == r[1..],
            (None, None) => true,
            _ => false,
        };
        d.confirmed = !now_equal;
        if d.confirmed {
            confirmed += 1;
        }
    }
    Ok(confirmed)
}

async fn point_query(
    conn: &mut (dyn DbConn + Send),
    spec: &KeysetPageSpec,
    key: i64,
) -> Result<Option<Vec<Value>>, DbError> {
    let point = KeysetPageSpec {
        range: Some((key, key + 1)),
        last_key: None,
        page_size: 1,
        ..spec.clone()
    };
    let sql = conn.dialect().render_keyset_page_sql(&point);
    let r = conn.query(&sql).await?;
    Ok(r.rows.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mysql::dialect::MySqlDialect;
    use crate::backend::{Dialect, QueryResult};
    use crate::delta_diff::report::DiffStatus;
    use async_trait::async_trait;

    /// 按 key 供给当前读结果的 mock（模拟"比对后已一致"的并发写入场景）
    struct PointConn {
        rows: Vec<Vec<Value>>,
        dialect: MySqlDialect,
    }

    #[async_trait]
    impl DbConn for PointConn {
        async fn query(&mut self, _sql: &str) -> Result<QueryResult, DbError> {
            Ok(QueryResult {
                columns: vec!["id".into(), "v".into()],
                row_count: self.rows.len(),
                rows: self.rows.clone(),
            })
        }
        async fn exec(&mut self, _sql: &str, _params: &[Value]) -> Result<QueryResult, DbError> {
            Err(DbError::unsupported("mock"))
        }
        async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
            Err(DbError::unsupported("mock"))
        }
        fn dialect(&self) -> &dyn Dialect {
            &self.dialect
        }
    }

    fn spec() -> KeysetPageSpec {
        KeysetPageSpec {
            schema: None,
            table: "t".into(),
            columns: vec!["id".into(), "v".into()],
            raw_exprs: false,
            key_column: "id".into(),
            range: None,
            last_key: None,
            page_size: 8192,
            filter: None,
            scn: None,
        }
    }

    #[tokio::test]
    async fn transient_diff_marked_unconfirmed() {
        let row = vec![Value::from(7), Value::from("same")];
        let mut left = PointConn {
            rows: vec![row.clone()],
            dialect: MySqlDialect,
        };
        let mut right = PointConn {
            rows: vec![row],
            dialect: MySqlDialect,
        };
        let mut diffs = vec![DiffRow {
            key: Value::from(7),
            left: Some(vec![Value::from(7), Value::from("old")]),
            right: Some(vec![Value::from(7), Value::from("new")]),
            status: DiffStatus::Modified,
            confirmed: true,
        }];
        let n = recheck_diffs(&mut left, &mut right, &spec(), &spec(), &mut diffs)
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(!diffs[0].confirmed);
    }

    #[tokio::test]
    async fn persistent_diff_stays_confirmed() {
        let mut left = PointConn {
            rows: vec![vec![Value::from(7), Value::from("a")]],
            dialect: MySqlDialect,
        };
        let mut right = PointConn {
            rows: vec![vec![Value::from(7), Value::from("b")]],
            dialect: MySqlDialect,
        };
        let mut diffs = vec![DiffRow {
            key: Value::from(7),
            left: None,
            right: None,
            status: DiffStatus::Modified,
            confirmed: false,
        }];
        let n = recheck_diffs(&mut left, &mut right, &spec(), &spec(), &mut diffs)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(diffs[0].confirmed);
    }
}
