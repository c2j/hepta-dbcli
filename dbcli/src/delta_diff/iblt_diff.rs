// ─── delta-diff IbltDiffer：IBLT 小差异快路径（Addendum A v1.1）──────────
//
// 每侧一条 j=4 子表摘要 SQL（Dialect::render_iblt_sql），客户端逐桶相减 +
// 剥洋葱解码（纯桶校验 + 失败检测）。解码失败（差异超容量）透明回退
// HashDiffer（--strict 则报错）。一致性/复核复用 §8.2/§8.3 通道。

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use chrono::Utc;
use serde_json::Value;

use crate::backend::{DbConn, DbError, IbltSqlSpec};
use crate::delta_diff::hash_diff::{self, open_snapshot};
use crate::delta_diff::recheck::recheck_diffs;
use crate::delta_diff::report::{
    DiffReport, DiffRow, DiffStatus, DiffSummary, PerfMetrics, ShardResult, ShardStatus, TableRef,
};
use crate::delta_diff::strategy::{ConsistencyMode, DiffContext, DiffStrategy};

/// IBLT 桶（相减后）：cnt 为代数和，key/val 为 XOR 余量
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Cell {
    cnt: i64,
    key_xor: u64,
    val_xor: [u64; 4],
}

type Summary = HashMap<(u8, u64), Cell>;

/// 解码出的差异条目：key、行哈希（4 切片）、来源侧
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    key: u64,
    val: [u64; 4],
    from_left: bool,
}

pub(crate) struct IbltDiffer;

#[async_trait::async_trait]
impl DiffStrategy for IbltDiffer {
    fn name(&self) -> &'static str {
        "iblt"
    }

    async fn diff(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
    ) -> Result<DiffReport, DbError> {
        let started = Utc::now();
        match self.try_iblt(left, right, ctx).await {
            Ok(mut report) => {
                report.started_at = started;
                report.finished_at = Utc::now();
                Ok(report)
            }
            Err(IbltFailure::Capacity) if !ctx.strict => {
                let mut report = hash_diff::HashDiffer.diff(left, right, ctx).await?;
                report.warnings.push(format!(
                    "fallback: hashdiff (iblt capacity exceeded, d > {})",
                    ctx.iblt_capacity
                ));
                Ok(report)
            }
            Err(IbltFailure::Capacity) => Err(DbError::query(format!(
                "iblt decode failed (capacity exceeded, d > {}); use --strategy hashdiff or drop --strict",
                ctx.iblt_capacity
            ))),
            // §2.3 + §16.3-F8：方言能力不足（如 Oracle 19c 无 BIT_XOR_AGG）也透明回退；
            // --strict 下原样报错
            Err(IbltFailure::Db(e)) if !ctx.strict => {
                let mut report = hash_diff::HashDiffer.diff(left, right, ctx).await?;
                report.warnings.push(format!(
                    "fallback: hashdiff (iblt unavailable on this backend: {})",
                    e
                ));
                Ok(report)
            }
            Err(IbltFailure::Db(e)) => Err(e),
        }
    }
}

enum IbltFailure {
    Capacity,
    Db(DbError),
}

impl From<DbError> for IbltFailure {
    fn from(e: DbError) -> Self {
        IbltFailure::Db(e)
    }
}

impl IbltDiffer {
    async fn try_iblt(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
    ) -> Result<DiffReport, IbltFailure> {
        let t0 = Instant::now();
        let m = (3 * ctx.iblt_capacity / 4).max(16);
        let mut queries = 0u64;

        if ctx.consistency == ConsistencyMode::Snapshot {
            open_snapshot(left).await.map_err(IbltFailure::Db)?;
            open_snapshot(right).await.map_err(IbltFailure::Db)?;
            let _ = ctx.scns.set((
                hash_diff::capture_scn(left)
                    .await
                    .map_err(IbltFailure::Db)?,
                hash_diff::capture_scn(right)
                    .await
                    .map_err(IbltFailure::Db)?,
            ));
        }

        let result = self
            .summarize_and_decode(left, right, ctx, m, &mut queries)
            .await;

        if ctx.consistency == ConsistencyMode::Snapshot {
            let _ = left.query_drop("COMMIT").await;
            let _ = right.query_drop("COMMIT").await;
        }
        let (mut diffs, note, left_total, right_total) = result?;

        // §8.3 复核：点查确认差异类型与真伪（复用现有通道）
        if ctx.recheck && !diffs.is_empty() {
            let lspec = hash_diff::keyset_spec(ctx, true, left.dialect())?;
            let rspec = hash_diff::keyset_spec(ctx, false, right.dialect())?;
            recheck_diffs(left, right, &lspec, &rspec, &mut diffs).await?;
            queries += 2 * diffs.len() as u64;
        }

        Ok(assemble(
            ctx,
            diffs,
            m,
            queries,
            t0,
            note,
            left_total,
            right_total,
        ))
    }

    async fn summarize_and_decode(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        m: u64,
        queries: &mut u64,
    ) -> Result<(Vec<DiffRow>, &'static str, u64, u64), IbltFailure> {
        let lsql = render_iblt(left, ctx, m, true).map_err(IbltFailure::Db)?;
        let rsql = render_iblt(right, ctx, m, false).map_err(IbltFailure::Db)?;

        let (lr, rr) = tokio::join!(left.query(&lsql), right.query(&rsql));
        *queries += 2;
        let (lr, rr) = (lr.map_err(IbltFailure::Db)?, rr.map_err(IbltFailure::Db)?);

        let lsum = parse_summary(&lr.rows, left.dialect().url_scheme());
        let rsum = parse_summary(&rr.rows, right.dialect().url_scheme());
        // 行数 = 各桶 cnt 之和 / 4（每行入 4 子表）
        let left_total = lsum.values().map(|c| c.cnt).sum::<i64>() as u64 / 4;
        let right_total = rsum.values().map(|c| c.cnt).sum::<i64>() as u64 / 4;

        let diff = subtract(&lsum, &rsum);
        if diff.values().all(|c| *c == Cell::default()) {
            return Ok((vec![], "decoded-empty", left_total, right_total));
        }
        let entries = peel(&diff, m).map_err(|_| IbltFailure::Capacity)?;
        Ok((classify(&entries), "decoded", left_total, right_total))
    }
}

/// 渲染单侧摘要 SQL：key_expr 为加引号键列，规范化表达式取本侧计划，
/// 过滤条件按方言渲染（strategy::side_filter）。
fn render_iblt(
    conn: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
    m: u64,
    is_left: bool,
) -> Result<String, DbError> {
    let dialect = conn.dialect();
    let side = if is_left { &ctx.left } else { &ctx.right };
    let q = dialect.identifier_quote();
    let spec = IbltSqlSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        key_expr: format!("{q}{}{q}", ctx.key_column),
        normalized_exprs: side.plan.normalized_exprs(dialect)?,
        cells_per_subtable: m,
        filter: crate::delta_diff::strategy::side_filter(ctx, dialect.url_scheme()),
        scn: ctx.scn_of(is_left),
    };
    dialect.render_iblt_sql(&spec)
}

fn subtract(l: &Summary, r: &Summary) -> Summary {
    let mut out: Summary = HashMap::new();
    for (k, c) in l {
        let e = out.entry(*k).or_default();
        e.cnt += c.cnt;
        e.key_xor ^= c.key_xor;
        for i in 0..4 {
            e.val_xor[i] ^= c.val_xor[i];
        }
    }
    for (k, c) in r {
        let e = out.entry(*k).or_default();
        e.cnt -= c.cnt;
        e.key_xor ^= c.key_xor;
        for i in 0..4 {
            e.val_xor[i] ^= c.val_xor[i];
        }
    }
    out
}

/// 剥洋葱解码（Addendum §1.2）：cnt=±1 纯桶 → 校验桶位 → 从 4 子表剔除。
fn peel(diff: &Summary, m: u64) -> Result<Vec<Entry>, ()> {
    let mut cells = diff.clone();
    let mut queue: VecDeque<(u8, u64)> = cells
        .iter()
        .filter(|(_, c)| c.cnt == 1 || c.cnt == -1)
        .map(|(k, _)| *k)
        .collect();
    let mut entries = Vec::new();

    while let Some((grp, cell_idx)) = queue.pop_front() {
        let Some(cell) = cells.get(&(grp, cell_idx)).copied() else {
            continue;
        };
        if cell.cnt != 1 && cell.cnt != -1 {
            continue;
        }
        let sign = cell.cnt;
        let entry = Entry {
            key: cell.key_xor,
            val: cell.val_xor,
            from_left: sign > 0,
        };
        // 纯桶校验（§1.4）：由 val_xor 重算 4 个子表桶位，当前桶必须等于其
        // 本子表应属桶位（严格校验；宽松的 contains 会放行伪纯桶）
        let mut buckets = [0u64; 4];
        for j in 1..=4usize {
            buckets[j - 1] = cell.val_xor[j - 1] % m.max(1);
        }
        if buckets[grp as usize - 1] != cell_idx {
            return Err(()); // 净 ±1 的伪纯桶 → 解码失败
        }
        entries.push(entry);
        for (j, b) in buckets.iter().enumerate() {
            let k = ((j + 1) as u8, *b);
            let c = cells.entry(k).or_default();
            c.cnt -= sign;
            c.key_xor ^= entry.key;
            for i in 0..4 {
                c.val_xor[i] ^= entry.val[i];
            }
            if c.cnt == 1 || c.cnt == -1 {
                queue.push_back(k);
            }
        }
    }

    if cells.values().any(|c| *c != Cell::default()) {
        return Err(()); // 卡死：差异超容量
    }
    Ok(entries)
}

type KeyedVals = HashMap<u64, (Option<[u64; 4]>, Option<[u64; 4]>)>;

/// 按 key 聚合差异条目并分类（§1.3：同 key 双侧条目 → Modified）
fn classify(entries: &[Entry]) -> Vec<DiffRow> {
    let mut by_key: KeyedVals = HashMap::new();
    for e in entries {
        let slot = by_key.entry(e.key).or_default();
        if e.from_left {
            slot.0 = Some(e.val);
        } else {
            slot.1 = Some(e.val);
        }
    }
    let mut out = Vec::new();
    for (k, (l, r)) in by_key {
        let status = match (l, r) {
            (Some(_), None) => DiffStatus::MissingRight,
            (None, Some(_)) => DiffStatus::MissingLeft,
            (Some(_), Some(_)) => DiffStatus::Modified,
            (None, None) => continue,
        };
        out.push(DiffRow {
            key: Value::from(k),
            left: None,
            right: None,
            status,
            confirmed: true,
        });
    }
    out.sort_by_key(|d| d.key.to_string());
    out
}

// ─── 摘要解析（两种 SQL 形态）────────────────────────────────────────────

fn parse_summary(rows: &[Vec<Value>], scheme: &str) -> Summary {
    match scheme {
        "gaussdb" => parse_parity_rows(rows),
        _ => parse_bitxor_rows(rows),
    }
}

/// MySQL/PolarDB-X/Oracle 形态：(grp, cell, cnt, key_xor, vx1..4)
fn parse_bitxor_rows(rows: &[Vec<Value>]) -> Summary {
    let mut out = HashMap::new();
    for row in rows {
        if row.len() < 7 {
            continue;
        }
        let grp = to_u64(&row[0]) as u8;
        let cell = to_u64(&row[1]);
        let c = Cell {
            cnt: to_i64(&row[2]),
            key_xor: to_u64(&row[3]),
            val_xor: [
                to_u64(&row[4]),
                to_u64(&row[5]),
                to_u64(&row[6]),
                to_u64(row.get(7).unwrap_or(&Value::Null)),
            ],
        };
        out.insert((grp, cell), c);
    }
    out
}

/// GaussDB 奇偶形态：(grp, cell, cnt, kx_0..63, vx1_0..31, vx2_*, vx3_*, vx4_*)
fn parse_parity_rows(rows: &[Vec<Value>]) -> Summary {
    let mut out = HashMap::new();
    for row in rows {
        if row.len() < 3 + 64 + 128 {
            continue;
        }
        let grp = to_u64(&row[0]) as u8;
        let cell = to_u64(&row[1]);
        let cnt = to_i64(&row[2]);
        let bits = |base: usize, n: usize| -> u64 {
            let mut v = 0u64;
            for b in 0..n {
                if to_u64(&row[3 + base + b]) % 2 == 1 {
                    v |= 1 << b;
                }
            }
            v
        };
        let c = Cell {
            cnt,
            key_xor: bits(0, 64),
            val_xor: [bits(64, 32), bits(96, 32), bits(128, 32), bits(160, 32)],
        };
        out.insert((grp, cell), c);
    }
    out
}

fn to_u64(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().map(|i| i as u64))
            .or_else(|| {
                n.as_f64().and_then(|f| {
                    if f.fract() == 0.0 && f >= 0.0 {
                        Some(f as u64)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(0),
        Value::String(s) => s
            .trim()
            .split('.')
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0),
        _ => 0,
    }
}

fn to_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Value::String(s) => s
            .trim()
            .split('.')
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0),
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn assemble(
    ctx: &DiffContext,
    diffs: Vec<DiffRow>,
    m: u64,
    queries: u64,
    t0: Instant,
    note: &str,
    left_total: u64,
    right_total: u64,
) -> DiffReport {
    let mut summary = DiffSummary {
        left_total,
        right_total,
        ..Default::default()
    };
    for d in &diffs {
        if !d.confirmed {
            continue;
        }
        match d.status {
            DiffStatus::MissingLeft => summary.missing_left += 1,
            DiffStatus::MissingRight => summary.missing_right += 1,
            DiffStatus::Modified => summary.modified += 1,
        }
    }
    let total = summary.left_total.max(summary.right_total);
    summary.diff_rate = if total > 0 {
        (summary.missing_left + summary.missing_right + summary.modified) as f64 / total as f64
    } else {
        0.0
    };

    let shard = ShardResult {
        shard_id: format!("iblt-k{}", 4 * m),
        key_range: (Value::Null, Value::Null),
        left_count: left_total,
        right_count: right_total,
        diff_count: (summary.missing_left + summary.missing_right + summary.modified),
        status: if diffs.is_empty() {
            ShardStatus::Match
        } else {
            ShardStatus::Diff
        },
        duration_ms: t0.elapsed().as_millis() as u64,
    };

    let mut warnings: Vec<String> = ctx
        .left
        .plan
        .warnings
        .iter()
        .chain(ctx.right.plan.warnings.iter())
        .chain(ctx.route_warnings.iter())
        .cloned()
        .collect();
    warnings.push(format!(
        "iblt: capacity={} cells={} note={}",
        ctx.iblt_capacity,
        4 * m,
        note
    ));

    DiffReport {
        started_at: Utc::now(),
        finished_at: Utc::now(),
        left: TableRef {
            connection: ctx.left.connection_name.clone(),
            schema: ctx.left.schema.clone(),
            table: ctx.left.table.clone(),
        },
        right: TableRef {
            connection: ctx.right.connection_name.clone(),
            schema: ctx.right.schema.clone(),
            table: ctx.right.table.clone(),
        },
        strategy: "iblt".into(),
        consistency: ctx.consistency.as_str().into(),
        hash_algorithm: "md5".into(),
        summary,
        perf: PerfMetrics {
            queries_total: queries,
            shard_duration_p50_ms: 0,
            shard_duration_p99_ms: 0,
        },
        shards: vec![shard],
        sample_diffs: diffs.into_iter().take(ctx.sample_limit).collect(),
        warnings,
    }
}
