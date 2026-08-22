// ─── delta-diff rowdiff: keyset 分页拉取 + 页式归并（§6.2.2）────────────
//
// 双侧按 key 升序分页拉取（render_keyset_page_sql），客户端页式归并，
// 内存 O(页大小)。行值比较为 serde_json::Value 逐列相等（§九-2 客户端路径）。

use serde_json::Value;

use crate::backend::{DbConn, DbError, KeysetPageSpec};
use crate::delta_diff::report::{DiffRow, DiffStatus};

const PAGE_SIZE: usize = 8192;

/// 一个分片范围内的行级差异。
pub(crate) struct RangeDiff {
    pub(crate) rows: Vec<DiffRow>,
    pub(crate) left_count: u64,
    pub(crate) right_count: u64,
}

/// 双侧 keyset 分页归并。两侧各自持有分页 spec（表名/schema 可不同）。
/// `key_arity` 是每行前缀中的键列数（单列=1，复合>1）。
pub(crate) async fn row_level_diff(
    left: &mut (dyn DbConn + Send),
    right: &mut (dyn DbConn + Send),
    left_spec: &KeysetPageSpec,
    right_spec: &KeysetPageSpec,
    range: Option<(i64, i64)>,
    key_arity: usize,
    verbose: bool,
) -> Result<RangeDiff, DbError> {
    let arity = key_arity.max(1);
    let mut left_page = PageCursor::new(left, left_spec, range, arity, verbose);
    let mut right_page = PageCursor::new(right, right_spec, range, arity, verbose);
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
                drain(
                    &mut rbuf,
                    &mut ri,
                    &mut right_page,
                    &mut out,
                    Side::Right,
                    arity,
                )
                .await?;
                break;
            }
            lbuf = left_page.next_page().await?;
            li = 0;
            continue;
        }
        if ri >= rbuf.len() {
            if right_page.exhausted {
                drain(
                    &mut lbuf,
                    &mut li,
                    &mut left_page,
                    &mut out,
                    Side::Left,
                    arity,
                )
                .await?;
                break;
            }
            rbuf = right_page.next_page().await?;
            ri = 0;
            continue;
        }

        let lk = row_key_tuple(&lbuf[li], arity);
        let rk = row_key_tuple(&rbuf[ri], arity);
        match cmp_key(&lk, &rk) {
            std::cmp::Ordering::Less => {
                out.rows
                    .push(diff_row_n(&lbuf[li], arity, true, DiffStatus::MissingRight));
                out.left_count += 1;
                li += 1;
            }
            std::cmp::Ordering::Greater => {
                out.rows
                    .push(diff_row_n(&rbuf[ri], arity, false, DiffStatus::MissingLeft));
                out.right_count += 1;
                ri += 1;
            }
            std::cmp::Ordering::Equal => {
                out.left_count += 1;
                out.right_count += 1;
                if lbuf[li].get(arity..) != rbuf[ri].get(arity..) {
                    out.rows.push(DiffRow {
                        key: diff_key(&lbuf[li], arity),
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
    arity: usize,
) -> Result<(), DbError> {
    loop {
        while *idx < buf.len() {
            let row = &buf[*idx];
            let (status, is_left) = match side {
                Side::Left => (DiffStatus::MissingRight, true),
                Side::Right => (DiffStatus::MissingLeft, false),
            };
            out.rows.push(diff_row_n(row, arity, is_left, status));
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

fn row_key_tuple(row: &[Value], arity: usize) -> Vec<Value> {
    let n = arity.min(row.len());
    row[..n].to_vec()
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn cmp_value(l: &Value, r: &Value) -> std::cmp::Ordering {
    if matches!(l, Value::Number(_)) || matches!(r, Value::Number(_)) {
        if let (Some(li), Some(ri)) = (as_i64(l), as_i64(r)) {
            return li.cmp(&ri);
        }
        if let (Some(lu), Some(ru)) = (as_u64(l), as_u64(r)) {
            return lu.cmp(&ru);
        }
        if let (Some(ln), Some(rn)) = (as_f64(l), as_f64(r)) {
            return ln.partial_cmp(&rn).unwrap_or(std::cmp::Ordering::Equal);
        }
    }
    match (l.as_str(), r.as_str()) {
        (Some(ls), Some(rs)) => ls.cmp(rs),
        _ => l.to_string().cmp(&r.to_string()),
    }
}

fn cmp_key(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    for (l, r) in a.iter().zip(b.iter()) {
        match cmp_value(l, r) {
            std::cmp::Ordering::Equal => continue,
            o => return o,
        }
    }
    a.len().cmp(&b.len())
}

fn diff_key(row: &[Value], arity: usize) -> Value {
    if arity <= 1 {
        row.first().cloned().unwrap_or(Value::Null)
    } else {
        Value::Array(row_key_tuple(row, arity))
    }
}

fn diff_row_n(row: &[Value], arity: usize, is_left: bool, status: DiffStatus) -> DiffRow {
    DiffRow {
        key: diff_key(row, arity),
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
    range: Option<(i64, i64)>,
    last_key: Option<Vec<Value>>,
    key_arity: usize,
    exhausted: bool,
    verbose: bool,
}

impl<'a> PageCursor<'a> {
    fn new(
        conn: &'a mut (dyn DbConn + Send),
        base: &'a KeysetPageSpec,
        range: Option<(i64, i64)>,
        key_arity: usize,
        verbose: bool,
    ) -> Self {
        Self {
            conn,
            base,
            range,
            last_key: None,
            key_arity,
            exhausted: false,
            verbose,
        }
    }

    async fn next_page(&mut self) -> Result<Vec<Vec<Value>>, DbError> {
        let spec = KeysetPageSpec {
            schema: self.base.schema.clone(),
            table: self.base.table.clone(),
            columns: self.base.columns.clone(),
            raw_exprs: self.base.raw_exprs,
            key_columns: self.base.key_columns.clone(),
            range: self.range,
            last_key: self.last_key.clone(),
            page_size: PAGE_SIZE,
            filter: self.base.filter.clone(),
            scn: self.base.scn,
        };
        let sql = self.conn.dialect().render_keyset_page_sql(&spec);
        if self.verbose || std::env::var("DELTA_DIFF_DEBUG_SQL").is_ok() {
            eprintln!("[sql] {sql}");
        }
        let result = self.conn.query(&sql).await?;
        if result.rows.len() < PAGE_SIZE {
            self.exhausted = true;
        }
        if let Some(last) = result.rows.last() {
            self.last_key = Some(row_key_tuple(last, self.key_arity));
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
    use serde_json::json;
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
            key_columns: vec!["id".into()],
            range: None,
            last_key: None,
            page_size: PAGE_SIZE,
            filter: None,
            scn: None,
        }
    }

    #[test]
    fn cmp_key_tuple_numeric_coercion() {
        let a = vec![json!(1), json!("x")];
        let b = vec![json!("1"), json!("x")];
        assert_eq!(cmp_key(&a, &b), std::cmp::Ordering::Equal);
        let c = vec![json!(1), json!("y")];
        assert_eq!(cmp_key(&a, &c), std::cmp::Ordering::Less);
    }

    #[test]
    fn cmp_key_two_numeric_strings_stay_lexical() {
        assert_eq!(
            cmp_key(&[json!("10")], &[json!("2")]),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn cmp_value_i64_beyond_f64_mantissa_is_exact() {
        let a = json!(9007199254740993i64);
        let b = json!(9007199254740992i64);
        assert_ne!(cmp_key(&[a], &[b]), std::cmp::Ordering::Equal);
    }

    #[test]
    fn cmp_value_number_vs_numeric_string_still_equal() {
        assert_eq!(
            cmp_key(&[json!(1)], &[json!("1")]),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn diff_row_composite_key_is_array() {
        let row = vec![json!(1), json!("t"), json!("payload")];
        let d = diff_row_n(&row, 2, true, DiffStatus::MissingRight);
        assert_eq!(d.key, json!([1, "t"]));
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
        let diff = row_level_diff(
            &mut left,
            &mut right,
            &spec(),
            &spec(),
            Some((0, 100)),
            1,
            false,
        )
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
        let diff = row_level_diff(
            &mut left,
            &mut right,
            &spec(),
            &spec(),
            Some((0, 100)),
            1,
            false,
        )
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
        let diff = row_level_diff(
            &mut left,
            &mut right,
            &spec(),
            &spec(),
            Some((0, 100)),
            1,
            false,
        )
        .await
        .unwrap();
        assert_eq!(diff.rows.len(), 2);
        assert!(diff
            .rows
            .iter()
            .all(|r| r.status == DiffStatus::MissingLeft));
    }
}
