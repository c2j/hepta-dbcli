// ─── delta-diff rowdiff: keyset 分页拉取 + 页式归并（§6.2.2）────────────
//
// 双侧按 key 升序分页拉取（render_keyset_page_sql），客户端页式归并，
// 内存 O(页大小)。行值比较为 serde_json::Value 逐列相等（§九-2 客户端路径）。

use serde_json::Value;

use crate::backend::{DbConn, DbError, KeysetPageSpec};
use crate::delta_diff::report::{DiffRow, DiffStatus};

const PAGE_SIZE: usize = 8192;

/// 一个分片范围内的行级差异（key 为单列整型，MVP 约束见 §6.4）
pub(crate) struct RangeDiff {
    pub(crate) rows: Vec<DiffRow>,
    pub(crate) left_count: u64,
    pub(crate) right_count: u64,
}

/// 双侧 keyset 分页归并。两侧各自持有分页 spec（表名/schema 可不同）。
pub(crate) async fn row_level_diff(
    left: &mut (dyn DbConn + Send),
    right: &mut (dyn DbConn + Send),
    left_spec: &KeysetPageSpec,
    right_spec: &KeysetPageSpec,
    range: (i64, i64),
) -> Result<RangeDiff, DbError> {
    let mut left_page = PageCursor::new(left, left_spec, range);
    let mut right_page = PageCursor::new(right, right_spec, range);
    let mut out = RangeDiff {
        rows: Vec::new(),
        left_count: 0,
        right_count: 0,
    };

    let mut lbuf = left_page.next_page().await?;
    let mut rbuf = right_page.next_page().await?;
    let (mut li, mut ri) = (0usize, 0usize);

    loop {
        if li >= lbuf.len() {
            if left_page.exhausted {
                // 左尽：右侧剩余全部 MissingLeft
                drain(&mut rbuf, &mut ri, &mut right_page, &mut out, Side::Right).await?;
                break;
            }
            lbuf = left_page.next_page().await?;
            li = 0;
            continue;
        }
        if ri >= rbuf.len() {
            if right_page.exhausted {
                drain(&mut lbuf, &mut li, &mut left_page, &mut out, Side::Left).await?;
                break;
            }
            rbuf = right_page.next_page().await?;
            ri = 0;
            continue;
        }

        let lk = row_key(&lbuf[li]);
        let rk = row_key(&rbuf[ri]);
        match lk.cmp(&rk) {
            std::cmp::Ordering::Less => {
                out.rows
                    .push(diff_row(&lbuf[li], true, DiffStatus::MissingRight));
                out.left_count += 1;
                li += 1;
            }
            std::cmp::Ordering::Greater => {
                out.rows
                    .push(diff_row(&rbuf[ri], false, DiffStatus::MissingLeft));
                out.right_count += 1;
                ri += 1;
            }
            std::cmp::Ordering::Equal => {
                out.left_count += 1;
                out.right_count += 1;
                if lbuf[li][1..] != rbuf[ri][1..] {
                    out.rows.push(DiffRow {
                        key: lbuf[li][0].clone(),
                        left: Some(lbuf[li].clone()),
                        right: Some(rbuf[ri].clone()),
                        status: DiffStatus::Modified,
                        confirmed: true,
                    });
                }
                li += 1;
                ri += 1;
            }
        }
    }
    Ok(out)
}

enum Side {
    Left,
    Right,
}

async fn drain(
    buf: &mut Vec<Vec<Value>>,
    idx: &mut usize,
    cursor: &mut PageCursor<'_>,
    out: &mut RangeDiff,
    side: Side,
) -> Result<(), DbError> {
    loop {
        while *idx < buf.len() {
            let row = &buf[*idx];
            let (status, is_left) = match side {
                Side::Left => (DiffStatus::MissingRight, true),
                Side::Right => (DiffStatus::MissingLeft, false),
            };
            out.rows.push(diff_row(row, is_left, status));
            match side {
                Side::Left => out.left_count += 1,
                Side::Right => out.right_count += 1,
            }
            *idx += 1;
        }
        if cursor.exhausted {
            return Ok(());
        }
        *buf = cursor.next_page().await?;
        *idx = 0;
    }
}

fn row_key(row: &[Value]) -> i64 {
    match row.first() {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(i64::MIN),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(i64::MIN),
        _ => i64::MIN,
    }
}

fn diff_row(row: &[Value], is_left: bool, status: DiffStatus) -> DiffRow {
    DiffRow {
        key: row[0].clone(),
        left: if is_left { Some(row.to_vec()) } else { None },
        right: if is_left { None } else { Some(row.to_vec()) },
        status,
        confirmed: true,
    }
}

/// 单侧 keyset 分页游标
struct PageCursor<'a> {
    conn: &'a mut (dyn DbConn + Send),
    base: &'a KeysetPageSpec,
    range: (i64, i64),
    last_key: Option<i64>,
    exhausted: bool,
}

impl<'a> PageCursor<'a> {
    fn new(conn: &'a mut (dyn DbConn + Send), base: &'a KeysetPageSpec, range: (i64, i64)) -> Self {
        Self {
            conn,
            base,
            range,
            last_key: None,
            exhausted: false,
        }
    }

    async fn next_page(&mut self) -> Result<Vec<Vec<Value>>, DbError> {
        let spec = KeysetPageSpec {
            schema: self.base.schema.clone(),
            table: self.base.table.clone(),
            columns: self.base.columns.clone(),
            raw_exprs: self.base.raw_exprs,
            key_column: self.base.key_column.clone(),
            range: Some(self.range),
            last_key: self.last_key,
            page_size: PAGE_SIZE,
            filter: self.base.filter.clone(),
            scn: self.base.scn,
        };
        let sql = self.conn.dialect().render_keyset_page_sql(&spec);
        if std::env::var("DELTA_DIFF_DEBUG_SQL").is_ok() {
            eprintln!("[rowdiff sql]\n{sql}");
        }
        let result = self.conn.query(&sql).await?;
        if result.rows.len() < PAGE_SIZE {
            self.exhausted = true;
        }
        if let Some(last) = result.rows.last() {
            self.last_key = Some(row_key(last));
        }
        Ok(result.rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mysql::dialect::MySqlDialect;
    use crate::backend::{Dialect, QueryResult};
    use async_trait::async_trait;
    use std::collections::VecDeque;

    /// 按页供给预置行集的 mock 连接
    struct PagedConn {
        pages: VecDeque<Vec<Vec<Value>>>,
        dialect: MySqlDialect,
    }

    #[async_trait]
    impl DbConn for PagedConn {
        async fn query(&mut self, _sql: &str) -> Result<QueryResult, DbError> {
            let rows = self.pages.pop_front().unwrap_or_default();
            Ok(QueryResult {
                columns: vec!["id".into(), "v".into()],
                row_count: rows.len(),
                rows,
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

    fn rows(ids: &[(i64, &str)]) -> Vec<Vec<Value>> {
        ids.iter()
            .map(|(i, v)| vec![Value::from(*i), Value::from(*v)])
            .collect()
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
            page_size: PAGE_SIZE,
            filter: None,
            scn: None,
        }
    }

    #[tokio::test]
    async fn merge_detects_all_three_kinds() {
        let mut left = PagedConn {
            pages: VecDeque::from(vec![rows(&[(1, "a"), (2, "b"), (3, "c"), (5, "e")])]),
            dialect: MySqlDialect,
        };
        let mut right = PagedConn {
            pages: VecDeque::from(vec![rows(&[(1, "a"), (2, "B"), (4, "d"), (5, "e")])]),
            dialect: MySqlDialect,
        };
        let diff = row_level_diff(&mut left, &mut right, &spec(), &spec(), (0, 100))
            .await
            .unwrap();
        assert_eq!(diff.rows.len(), 3);
        assert_eq!(diff.rows[0].status, DiffStatus::Modified);
        assert_eq!(diff.rows[0].key, Value::from(2));
        assert_eq!(diff.rows[1].status, DiffStatus::MissingRight);
        assert_eq!(diff.rows[2].status, DiffStatus::MissingLeft);
        assert_eq!(diff.left_count, 4);
        assert_eq!(diff.right_count, 4);
    }

    #[tokio::test]
    async fn merge_identical_sides_empty() {
        let mut left = PagedConn {
            pages: VecDeque::from(vec![rows(&[(1, "a"), (2, "b")])]),
            dialect: MySqlDialect,
        };
        let mut right = PagedConn {
            pages: VecDeque::from(vec![rows(&[(1, "a"), (2, "b")])]),
            dialect: MySqlDialect,
        };
        let diff = row_level_diff(&mut left, &mut right, &spec(), &spec(), (0, 100))
            .await
            .unwrap();
        assert!(diff.rows.is_empty());
        assert_eq!(diff.left_count, 2);
        assert_eq!(diff.right_count, 2);
    }

    #[tokio::test]
    async fn merge_left_empty() {
        let mut left = PagedConn {
            pages: VecDeque::new(),
            dialect: MySqlDialect,
        };
        let mut right = PagedConn {
            pages: VecDeque::from(vec![rows(&[(1, "a"), (2, "b")])]),
            dialect: MySqlDialect,
        };
        let diff = row_level_diff(&mut left, &mut right, &spec(), &spec(), (0, 100))
            .await
            .unwrap();
        assert_eq!(diff.rows.len(), 2);
        assert!(diff
            .rows
            .iter()
            .all(|r| r.status == DiffStatus::MissingLeft));
    }
}
