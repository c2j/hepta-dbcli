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

/// `--iblt-auto-capacity` 的 `DiffContext::iblt_capacity` 哨兵值：
/// 0 表示两轮自协商容量（选项层负责翻译，strategy 路由不读该值）。
pub(crate) const IBLT_AUTO_CAPACITY: u64 = 0;

/// 非 auto 固定容量的下限：同时是用户显式传 `--iblt-capacity 0` 时与
/// AUTO 哨兵（0）冲突的防碰撞折迭值（评审 B6）。
pub(crate) const IBLT_MIN_CAPACITY: u64 = 16;

/// CLI 与 MCP/API 共用的容量归一：auto 走哨兵；固定容量折叠到
/// [`IBLT_MIN_CAPACITY`]，保证两个入口对同一输入语义一致。
pub(crate) fn normalize_iblt_capacity(auto: bool, requested: u64) -> u64 {
    if auto {
        IBLT_AUTO_CAPACITY
    } else {
        requested.max(IBLT_MIN_CAPACITY)
    }
}

/// 自适应协议第一轮子表桶数 m₁（最小可行；d ≤ ⌈4m/3⌉-1 时一轮完成）
const AUTO_MIN_CELLS: u64 = 64;

/// 自适应协议第二轮子表桶数上限 m_max（k = 4m 桶，覆盖旧默认容量 65536
/// 所对应的 k = 262144 再留 8× 余量）
const AUTO_MAX_CELLS: u64 = 65536 * 8;

/// 从相减后的摘要估计差异条数 d̂ = ⌈Σ|cnt| / 2⌉：
/// 每个幸存差异行给 4 个子表各贡献 ±1，XOR 抵消不掉的行对产生
/// |cnt| ≥ 1 的桶，故 Σ|cnt| 是 d 的 4 倍上界、通常 2–4 倍内。
/// 加载超载桶（|cnt| ≥ 2）一并计入，保证 d̂ ≥ 真实 d / 2。
fn estimate_diff_count(diff: &Summary) -> u64 {
    let total: i64 = diff.values().map(|c| c.cnt.abs()).sum();
    total.unsigned_abs().div_ceil(2)
}

/// 第二轮子表桶数 m₂ = clamp(3·d̂, m_min, m_max)，饱和乘法防溢出
fn auto_cells_for(dhat: u64) -> u64 {
    3u64.saturating_mul(dhat)
        .clamp(AUTO_MIN_CELLS, AUTO_MAX_CELLS)
}

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
        ctx.vlog(format!(
            "[delta-diff] strategy=iblt consistency={} key={} capacity={} strict={}",
            ctx.consistency.as_str(),
            ctx.key_column,
            if ctx.iblt_capacity == IBLT_AUTO_CAPACITY {
                "auto".to_string()
            } else {
                ctx.iblt_capacity.to_string()
            },
            ctx.strict
        ));
        match self.try_iblt(left, right, ctx).await {
            Ok(mut report) => {
                report.started_at = started;
                report.finished_at = Utc::now();
                ctx.vlog(format!(
                    "[shard] iblt {} left={} right={} diff={}",
                    if report.has_diff() { "diff" } else { "match" },
                    report.summary.left_total,
                    report.summary.right_total,
                    report.summary.missing_left
                        + report.summary.missing_right
                        + report.summary.modified
                ));
                Ok(report)
            }
            Err(IbltFailure::Capacity(diff)) if !ctx.strict => {
                // WP1 自适应两轮在 try_iblt 内完成（同一快照内重试）；
                // 两轮都失败才落到这里走既有 hashdiff 回退。
                let _ = diff;
                // 回退前先关掉快照事务：hashdiff 会开自己的快照，而
                // Oracle 在已开事务上 SET TRANSACTION READ ONLY 会
                // ORA-01453。main 的行为是先 COMMIT 再 result?（1951c96）。
                close_snapshot(left, right, ctx).await;
                ctx.vlog(format!(
                    "[delta-diff] iblt capacity exceeded (d > {}), falling back to hashdiff",
                    capacity_label(ctx)
                ));
                let mut report = hash_diff::HashDiffer.diff(left, right, ctx).await?;
                report.warnings.push(fallback_warning(ctx));
                Ok(report)
            }
            Err(IbltFailure::Capacity(diff)) => {
                // --strict 报错前同样先关快照，让调用方能直接重试或回退。
                close_snapshot(left, right, ctx).await;
                let detail = if ctx.iblt_capacity == IBLT_AUTO_CAPACITY {
                    // d̂/m₂ 从第一轮摘要确定性重算，与已尝试的第二轮一致
                    let dhat = estimate_diff_count(&diff);
                    format!(
                        "auto-capacity (round 1 m={AUTO_MIN_CELLS} and round 2 m={} both failed, d̂={dhat})",
                        auto_cells_for(dhat)
                    )
                } else {
                    ctx.iblt_capacity.to_string()
                };
                Err(DbError::query(format!(
                    "iblt decode failed (capacity exceeded, d > {}); use --strategy hashdiff or drop --strict",
                    detail
                )))
            }
            Err(IbltFailure::Db(e)) if !ctx.strict => {
                // §2.3 + §16.3-F8：方言能力不足（如 Oracle 19c 无 BIT_XOR_AGG）也透明回退；
                // --strict 下原样报错
                close_snapshot(left, right, ctx).await;
                ctx.vlog(format!(
                    "[delta-diff] iblt unavailable on this backend ({}), falling back to hashdiff",
                    e
                ));
                let mut report = hash_diff::HashDiffer.diff(left, right, ctx).await?;
                report.warnings.push(format!(
                    "fallback: hashdiff (iblt unavailable on this backend: {})",
                    e
                ));
                Ok(report)
            }
            Err(IbltFailure::Db(e)) => {
                close_snapshot(left, right, ctx).await;
                Err(e)
            }
        }
    }
}

/// 关闭两侧快照事务（snapshot 模式下的收尾/回退清理）。与 main
///（1951c96）一致：失败路径也要 COMMIT，否则 hashdiff 回退会在 Oracle
/// 已开事务上 SET TRANSACTION READ ONLY 而 ORA-01453。best-effort。
async fn close_snapshot(
    left: &mut (dyn DbConn + Send),
    right: &mut (dyn DbConn + Send),
    ctx: &DiffContext,
) {
    if ctx.consistency == ConsistencyMode::Snapshot {
        ctx.vlog("[sql] COMMIT");
        let _ = left.query_drop("COMMIT").await;
        let _ = right.query_drop("COMMIT").await;
    }
}

enum IbltFailure {
    /// 剥洋葱失败：携带相减后的摘要（自适应协议据此估计 d̂）
    Capacity(Summary),
    Db(DbError),
}

/// 日志/警告里的容量标签：哨兵 0 显示为 auto，固定容量原样
fn capacity_label(ctx: &DiffContext) -> String {
    if ctx.iblt_capacity == IBLT_AUTO_CAPACITY {
        "auto".to_string()
    } else {
        ctx.iblt_capacity.to_string()
    }
}

/// 既有回退警告文案；auto 模式注明两轮协议已尝试（行为红线：固定容量
/// 的字符串保持逐字节不变）
fn fallback_warning(ctx: &DiffContext) -> String {
    if ctx.iblt_capacity == IBLT_AUTO_CAPACITY {
        "fallback: hashdiff (iblt auto-capacity exhausted after 2 rounds)".to_string()
    } else {
        format!(
            "fallback: hashdiff (iblt capacity exceeded, d > {})",
            ctx.iblt_capacity
        )
    }
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
        // WP1 自适应：iblt_capacity 哨兵 0 = 两轮自协商（m₁=64 → 失败则
        // d̂=⌈Σ|cnt|/2⌉ → m₂=clamp(3·d̂)）；固定容量 = 单轮，行为不变。
        let auto = ctx.iblt_capacity == IBLT_AUTO_CAPACITY;
        let m1 = if auto {
            AUTO_MIN_CELLS
        } else {
            (3 * ctx.iblt_capacity / 4).max(16)
        };

        let t0 = Instant::now();
        let mut queries = 0u64;

        if ctx.consistency == ConsistencyMode::Snapshot {
            open_snapshot(left, ctx.verbose)
                .await
                .map_err(IbltFailure::Db)?;
            open_snapshot(right, ctx.verbose)
                .await
                .map_err(IbltFailure::Db)?;
            let _ = ctx.scns.set((
                hash_diff::capture_scn(left, ctx.verbose)
                    .await
                    .map_err(IbltFailure::Db)?,
                hash_diff::capture_scn(right, ctx.verbose)
                    .await
                    .map_err(IbltFailure::Db)?,
            ));
        }

        // 第一轮（auto 与固定容量共用）。
        let round1 = self
            .summarize_and_decode(left, right, ctx, m1, &mut queries)
            .await;
        if auto {
            if let Err(IbltFailure::Capacity(diff)) = &round1 {
                let dhat = estimate_diff_count(diff);
                let m2 = auto_cells_for(dhat);
                ctx.vlog(format!(
                    "[delta-diff] iblt auto-capacity: round 1 (m={m1}) failed, \
                     d̂={dhat}, retrying with m={m2}"
                ));
                // 第二轮复用同一（快照）连接：快照事务仍开启，重试读点一致。
                let round2 = self
                    .summarize_and_decode(left, right, ctx, m2, &mut queries)
                    .await;
                match round2 {
                    Ok((diffs, note, lt, rt)) => {
                        return self
                            .finish(left, right, ctx, diffs, m2, queries, t0, &note, lt, rt)
                            .await;
                    }
                    Err(IbltFailure::Capacity(diff2)) => {
                        ctx.vlog(format!(
                            "[delta-diff] iblt auto-capacity: round 2 (m={m2}) failed, \
                             falling back to hashdiff"
                        ));
                        return Err(IbltFailure::Capacity(diff2));
                    }
                    Err(e @ IbltFailure::Db(_)) => return Err(e),
                }
            }
        }

        let (diffs, note, left_total, right_total) = round1?;
        self.finish(
            left,
            right,
            ctx,
            diffs,
            m1,
            queries,
            t0,
            &note,
            left_total,
            right_total,
        )
        .await
    }

    /// 复核 + 报告组装（两轮共用的收尾；复核点查复用现有通道）
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
        mut diffs: Vec<DiffRow>,
        m: u64,
        mut queries: u64,
        t0: Instant,
        note: &str,
        left_total: u64,
        right_total: u64,
    ) -> Result<DiffReport, IbltFailure> {
        if ctx.consistency == ConsistencyMode::Snapshot {
            ctx.vlog("[sql] COMMIT");
            let _ = left.query_drop("COMMIT").await;
            let _ = right.query_drop("COMMIT").await;
        }

        // §8.3 复核：点查确认差异类型与真伪（复用现有通道）
        if ctx.recheck && !diffs.is_empty() {
            let lspec = hash_diff::keyset_spec(ctx, true, left.dialect())?;
            let rspec = hash_diff::keyset_spec(ctx, false, right.dialect())?;
            recheck_diffs(left, right, &lspec, &rspec, &mut diffs, ctx.verbose).await?;
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
    ) -> Result<(Vec<DiffRow>, String, u64, u64), IbltFailure> {
        let lsql = render_iblt(left, ctx, m, true).map_err(IbltFailure::Db)?;
        let rsql = render_iblt(right, ctx, m, false).map_err(IbltFailure::Db)?;
        ctx.vlog(format!("[sql:left] {lsql}"));
        ctx.vlog(format!("[sql:right] {rsql}"));

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
            return Ok((vec![], "decoded-empty".to_string(), left_total, right_total));
        }
        let entries = peel(&diff, m).map_err(|_| IbltFailure::Capacity(diff))?;
        Ok((
            classify(&entries),
            format!("decoded m={m}"),
            left_total,
            right_total,
        ))
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
    let spec = IbltSqlSpec {
        schema: side.schema.clone(),
        table: side.table.clone(),
        key_expr: dialect.quote_ident(&ctx.side_key_columns(is_left)[0]),
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

    // 可解性早退：能完成剥洋葱的摘要必须满足纯桶事件数 ≥ 3·Σ|cnt|/2
    //（每条差异剔除时引入 3 个新纯桶候选、自身消耗 1 次）。已处理事件数
    // 超过该下界还剩大摘要 → 必然卡死，立即放弃；避免超载摘要（大 d̂ 场景
    // 回退前）把空队列循环跑到百万级。
    let total_abs: u64 = cells.values().map(|c| c.cnt.unsigned_abs()).sum();
    let mut processed: u64 = 0;
    let event_budget = total_abs / 2 * 3 + 1;

    while let Some((grp, cell_idx)) = queue.pop_front() {
        processed += 1;
        if processed > event_budget {
            return Err(()); // 必然卡死，提前放弃
        }
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
        // GaussDB and DuckDB both render the per-bit parity column shape
        // (kx_0..63 / vx1..4_0..31, issue #49 phase 2).
        "gaussdb" | "duckdb" => parse_parity_rows(rows),
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
        shard_id: if ctx.iblt_capacity == IBLT_AUTO_CAPACITY {
            format!("iblt-auto-m{m}")
        } else {
            format!("iblt-k{}", 4 * m)
        },
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
    warnings.push(if ctx.iblt_capacity == IBLT_AUTO_CAPACITY {
        format!("iblt: capacity=auto cells={} m={} note={note}", 4 * m, m)
    } else {
        format!(
            "iblt: capacity={} cells={} note={}",
            ctx.iblt_capacity,
            4 * m,
            note
        )
    });

    let mut report = DiffReport {
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
        sample_diffs: diffs,
        warnings,
        row_payload: crate::delta_diff::report::RowPayload::Columns,
        key_columns: vec![],
        value_columns: vec![],
        column_data_types: vec![],
        ident_quote: '"',
        ident_scheme: String::new(),
        backslash_escape: false,
        modified_columns: None,
    };
    crate::delta_diff::report::stamp_columns_from_plan(&mut report, &ctx.left.plan);
    report
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::backend::mysql::dialect::MySqlDialect;
    use crate::backend::{DbPool, Dialect, QueryResult};
    use crate::delta_diff::strategy::SideCtx;

    // ── Pure estimation math ─────────────────────────────────────────

    #[test]
    fn capacity_normalizer_keeps_cli_and_api_consistent() {
        // 评审 B6：哨兵 0 只能来自 auto 标志；显式容量 0/1/16 在两个
        // 入口都折叠到 IBLT_MIN_CAPACITY，不会静默切换成两轮协议。
        assert_eq!(normalize_iblt_capacity(true, 0), IBLT_AUTO_CAPACITY);
        assert_eq!(normalize_iblt_capacity(true, 999), IBLT_AUTO_CAPACITY);
        assert_eq!(normalize_iblt_capacity(false, 0), IBLT_MIN_CAPACITY);
        assert_eq!(normalize_iblt_capacity(false, 1), IBLT_MIN_CAPACITY);
        assert_eq!(normalize_iblt_capacity(false, 16), 16);
        assert_eq!(normalize_iblt_capacity(false, 65_536), 65_536);
        // 哨兵不可达性：任何非 auto 输入都不产生 AUTO 哨兵。
        for req in [0u64, 1, 15, 16, 1024, u64::MAX] {
            assert_ne!(
                normalize_iblt_capacity(false, req),
                IBLT_AUTO_CAPACITY,
                "requested={req}"
            );
        }
    }

    #[test]
    fn should_estimate_zero_for_empty_summary() {
        let diff = Summary::new();
        assert_eq!(estimate_diff_count(&diff), 0);
    }

    #[test]
    fn should_estimate_half_of_absolute_cnt_sum() {
        // Two pure buckets: |+1| + |-1| = 2 → ceil(2/2) = 1
        let mut diff = Summary::new();
        diff.insert(
            (1, 0),
            Cell {
                cnt: 1,
                key_xor: 7,
                val_xor: [1, 2, 3, 4],
            },
        );
        diff.insert(
            (2, 1),
            Cell {
                cnt: -1,
                key_xor: 9,
                val_xor: [5, 6, 7, 8],
            },
        );
        assert_eq!(estimate_diff_count(&diff), 1);
    }

    #[test]
    fn should_estimate_from_overloaded_cells() {
        // Overloaded bucket with cnt=+5: ceil(5/2) = 3
        let mut diff = Summary::new();
        diff.insert(
            (1, 0),
            Cell {
                cnt: 5,
                key_xor: 0,
                val_xor: [0; 4],
            },
        );
        assert_eq!(estimate_diff_count(&diff), 3);
    }

    #[test]
    fn should_estimate_from_mixed_sign_overload() {
        // |+5| + |-3| = 8 → ceil(8/2) = 4
        let mut diff = Summary::new();
        diff.insert(
            (1, 0),
            Cell {
                cnt: 5,
                key_xor: 0,
                val_xor: [0; 4],
            },
        );
        diff.insert(
            (2, 2),
            Cell {
                cnt: -3,
                key_xor: 0,
                val_xor: [0; 4],
            },
        );
        assert_eq!(estimate_diff_count(&diff), 4);
    }

    #[test]
    fn should_floor_auto_m2_at_64() {
        // d̂=1 → 3·1=3, clamped up to 64
        assert_eq!(auto_cells_for(1), AUTO_MIN_CELLS);
        assert_eq!(auto_cells_for(0), AUTO_MIN_CELLS);
    }

    #[test]
    fn should_scale_auto_m2_with_estimate() {
        // d̂=100 → 3·100=300, inside [64, cap]
        assert_eq!(auto_cells_for(100), 300);
        // d̂=21 → 63 → clamped to 64 (just below floor)
        assert_eq!(auto_cells_for(21), 64);
        // d̂=22 → 66
        assert_eq!(auto_cells_for(22), 66);
    }

    #[test]
    fn should_cap_auto_m2_at_auto_max() {
        // 3·d̂ way beyond the cap → capped
        assert_eq!(auto_cells_for(10_000_000), AUTO_MAX_CELLS);
        assert_eq!(auto_cells_for(u64::MAX), AUTO_MAX_CELLS);
    }

    #[test]
    fn peel_aborts_early_when_impossible() {
        // 30 万条 entry 全部挤进同一组 cell（cnt=+30 万，无纯桶）：剥洋葱
        // 无从开始，旧实现会对空队列循环约 Σ|cnt|·3/2 次才以"剩余非零桶"
        // 判负；对这种超载摘要必须毫秒级早退。
        let entries: Vec<(u64, [u64; 4])> = (0..300_000u64).map(|_| (1, [1, 1, 1, 1])).collect();
        let m = 1024u64;
        let diff = aggregate_at(&entries, m);
        let t0 = std::time::Instant::now();
        assert!(peel(&diff, m).is_err());
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(200),
            "peel must abort early, took {:?}",
            t0.elapsed()
        );
    }

    // ── Two-round protocol via mock connections ──────────────────────

    /// Deterministic re-aggregation of raw per-row entries into an IBLT
    /// summary at arbitrary granularity m. Mirrors render_iblt_sql's
    /// placement: a row with val=[v0,v1,v2,v3] lands in subtable j at cell
    /// `v_{j-1} % m`, contributing cnt/key_xor/val_xor to that cell.
    fn aggregate_at(entries: &[(u64, [u64; 4])], m: u64) -> Summary {
        let mut out: Summary = HashMap::new();
        for &(key, val) in entries {
            for (i, &v) in val.iter().enumerate() {
                let j = i as u8 + 1;
                let cell_idx = v % m;
                let c = out.entry((j, cell_idx)).or_default();
                c.cnt += 1;
                c.key_xor ^= key;
                for (i2, x) in val.iter().enumerate() {
                    c.val_xor[i2] ^= x;
                }
            }
        }
        out
    }

    /// Mock DbConn serving IBLT summary rows. The SQL embeds
    /// `MOD(..., {m})` (render_iblt_sql), so the mock routes on the
    /// rendered SQL text to answer with a summary at the requested m.
    /// Also routes hashdiff fallback SQL (MIN/MAX key range, wide segment
    /// aggregate, keyset row pull) so two-failure-fallback tests drive the
    /// real HashDiffer path.
    type RawEntries = Vec<(u64, [u64; 4])>;

    struct MockIbltConn {
        dialect: MySqlDialect,
        /// (left_keys, right_keys) raw row entries per side.
        /// None → serve identical rows on both sides (equal summaries).
        entries: Option<(RawEntries, RawEntries)>,
        queries: std::sync::Arc<std::sync::atomic::AtomicU64>,
        /// 记录 query_drop 收到的语句（COMMIT 关快照测试用）。
        drops: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl MockIbltConn {
        fn identical() -> Self {
            Self {
                dialect: MySqlDialect,
                entries: None,
                queries: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                drops: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn with_entries(left: Vec<(u64, [u64; 4])>, right: Vec<(u64, [u64; 4])>) -> Self {
            Self {
                dialect: MySqlDialect,
                entries: Some((left, right)),
                queries: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                drops: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn summary_rows(&self, is_left: bool, m: u64) -> Vec<Vec<Value>> {
            let (l, r) = match &self.entries {
                None => return vec![], // identical → empty (all-zero) summaries
                Some((l, r)) => (l, r),
            };
            let mine = if is_left { l } else { r };
            let theirs = if is_left { r } else { l };
            // multiset difference as raw signed entries: rows present only
            // on this side count +1; only on the other side count -1.
            use std::collections::BTreeMap;
            let mut delta: BTreeMap<(u64, [u64; 4]), i64> = BTreeMap::new();
            for &e in mine {
                *delta.entry(e).or_default() += 1;
            }
            for &e in theirs {
                *delta.entry(e).or_default() -= 1;
            }
            // Fold into IBLT cells at granularity m: signed contributions.
            let mut cells: BTreeMap<(u8, u64), Cell> = BTreeMap::new();
            for (&(key, val), &sign) in delta.iter() {
                if sign == 0 {
                    continue;
                }
                for (i, &v) in val.iter().enumerate() {
                    let j = i as u8 + 1;
                    let cell_idx = v % m;
                    let c = cells.entry((j, cell_idx)).or_default();
                    c.cnt += sign;
                    c.key_xor ^= key;
                    for (i2, x) in val.iter().enumerate() {
                        c.val_xor[i2] ^= x;
                    }
                }
            }
            cells
                .into_iter()
                .map(|((grp, cell_idx), c)| {
                    vec![
                        json!(grp),
                        json!(cell_idx),
                        json!(c.cnt),
                        json!(c.key_xor),
                        json!(c.val_xor[0]),
                        json!(c.val_xor[1]),
                        json!(c.val_xor[2]),
                        json!(c.val_xor[3]),
                    ]
                })
                .collect()
        }

        fn iblt_m_from_sql(sql: &str) -> Option<u64> {
            // render_iblt_sql: "... MOD(CONV(SUBSTRING(h, g.grp * 8 - 7, 8), 16, 10), {m}) AS cell ..."
            let pos = sql.find("AS cell")?;
            let head = sql[..pos].trim_end().trim_end_matches(')');
            let digits: String = head
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            digits.parse().ok()
        }
    }

    #[async_trait::async_trait]
    impl DbConn for MockIbltConn {
        async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            use std::sync::atomic::Ordering;
            self.queries.fetch_add(1, Ordering::Relaxed);

            // IBLT summary round
            if sql.contains("CROSS JOIN") && sql.contains("AS cell") {
                let m = Self::iblt_m_from_sql(sql).expect("iblt sql must embed m");
                let is_left = sql.contains("FROM `lt`");
                let rows = self.summary_rows(is_left, m);
                let n = rows.len();
                return Ok(QueryResult {
                    columns: vec![],
                    rows,
                    row_count: n,
                    rows_affected: None,
                });
            }

            // hashdiff fallback routing (round 2 also failed)
            // 1) key range — right side empty: None lo/hi terminate bisection
            if sql.starts_with("SELECT MIN(") {
                return Ok(QueryResult {
                    columns: vec!["MIN(id)".into(), "MAX(id)".into()],
                    rows: vec![vec![json!(1), json!(200)]],
                    row_count: 1,
                    rows_affected: None,
                });
            }
            // 1b) snapshot-mode segment checksum (starts with the bare
            //     COUNT(*) projection; the wide UNION SQL starts with
            //     "SELECT seg_lo" so it must NOT match this branch):
            //     return identical zero tuples so bisection sees Match and
            //     the fallback completes without row pulls.
            if sql.starts_with("SELECT COUNT(*) AS cnt") && sql.contains("MD5(") {
                return Ok(QueryResult {
                    columns: vec![
                        "cnt".into(),
                        "s1".into(),
                        "s2".into(),
                        "s3".into(),
                        "s4".into(),
                    ],
                    rows: vec![vec![json!(0), json!(0), json!(0), json!(0), json!(0)]],
                    row_count: 1,
                    rows_affected: None,
                });
            }
            // 2) wide segment aggregate (WP3 none-mode first pass): each
            //    branch embeds "SELECT {lo} AS seg_lo" — echo those lo
            //    values with matching (cnt, s1..s4) tuples so segments
            //    become Match shards without further drilldown.
            if sql.contains("AS wide") {
                let mut los = Vec::new();
                let mut rest = sql;
                while let Some(p) = rest.find(" AS seg_lo") {
                    // walk back over the digits before the marker
                    let head = rest[..p].trim_end();
                    let digits: String = head
                        .chars()
                        .rev()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    if let Ok(lo) = digits.parse::<i64>() {
                        los.push(lo);
                    }
                    rest = &rest[p + " AS seg_lo".len()..];
                }
                let (cnt, s) = (2u64, [10u64, 0, 0, 0]);
                let rows: Vec<Vec<Value>> = los
                    .iter()
                    .map(|&lo| {
                        vec![
                            json!(lo),
                            json!(cnt),
                            json!(s[0]),
                            json!(0),
                            json!(0),
                            json!(0),
                        ]
                    })
                    .collect();
                let n = rows.len();
                return Ok(QueryResult {
                    columns: vec![],
                    rows,
                    row_count: n,
                    rows_affected: None,
                });
            }
            // 3) keyset row pull (fallback for genuinely differing rows)
            if sql.contains("ORDER BY") {
                return Ok(QueryResult {
                    columns: vec!["id".into()],
                    rows: vec![vec![json!(400001)]],
                    row_count: 1,
                    rows_affected: None,
                });
            }
            // 4) anything else (snapshot/VERSION probes etc.) → empty
            Ok(QueryResult::empty())
        }

        async fn exec(&mut self, _sql: &str, _params: &[Value]) -> Result<QueryResult, DbError> {
            Ok(QueryResult::empty())
        }

        async fn query_drop(&mut self, sql: &str) -> Result<(), DbError> {
            self.drops.lock().unwrap().push(sql.to_string());
            Ok(())
        }

        fn dialect(&self) -> &dyn Dialect {
            &self.dialect
        }
    }

    fn side_plan() -> crate::delta_diff::metadata::TablePlan {
        crate::delta_diff::metadata::TablePlan {
            url_scheme: "mysql".into(),
            key_columns: vec!["id".into()],
            compare_columns: vec!["id".into(), "v".into()],
            norm_specs: vec![
                crate::backend::ColumnNormSpec {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                    rtrim_fixed_char: false,
                },
                crate::backend::ColumnNormSpec {
                    name: "v".into(),
                    data_type: "int".into(),
                    nullable: false,
                    rtrim_fixed_char: false,
                },
            ],
            warnings: vec![],
        }
    }

    /// 仅供形态检查的占位池（hashdiff 回退不会用到它的测试）
    fn dummy_pool() -> std::sync::Arc<dyn DbPool> {
        struct Pool;
        #[async_trait::async_trait]
        impl DbPool for Pool {
            async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
                Err(DbError::unsupported("dummy"))
            }
        }
        std::sync::Arc::new(Pool)
    }

    /// 提供 MockIbltConn 克隆的池：hashdiff 回退会从两侧池各取 1 连接，
    /// 每个克隆共享同一原子查询计数。
    fn mock_pool(seed: &MockIbltConn) -> std::sync::Arc<dyn DbPool> {
        struct Pool(MockIbltConn);
        #[async_trait::async_trait]
        impl DbPool for Pool {
            async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
                Ok(Box::new(MockIbltConn {
                    dialect: MySqlDialect,
                    entries: self.0.entries.clone(),
                    queries: std::sync::Arc::clone(&self.0.queries),
                    drops: std::sync::Arc::clone(&self.0.drops),
                }))
            }
        }
        std::sync::Arc::new(Pool(MockIbltConn {
            dialect: MySqlDialect,
            entries: seed.entries.clone(),
            queries: std::sync::Arc::clone(&seed.queries),
            drops: std::sync::Arc::clone(&seed.drops),
        }))
    }

    fn qcount(c: &MockIbltConn) -> u64 {
        use std::sync::atomic::Ordering;
        c.queries.load(Ordering::Relaxed)
    }

    fn ctx_with_capacity(iblt_capacity: u64) -> DiffContext {
        let plan = side_plan();
        DiffContext {
            left: SideCtx {
                connection_name: "l".into(),
                schema: None,
                table: "lt".into(),
                plan: plan.clone(),
            },
            right: SideCtx {
                connection_name: "r".into(),
                schema: None,
                table: "rt".into(),
                plan: plan.clone(),
            },
            left_pool: dummy_pool(),
            right_pool: dummy_pool(),
            key_column: "id".into(),
            key_columns: vec!["id".into()],
            left_key_columns: vec!["id".into()],
            right_key_columns: vec!["id".into()],
            filter: None,
            incremental: None,
            bisection_factor: 32,
            bisection_threshold: 16_384,
            sample_limit: 20,
            threads: 1,
            consistency: ConsistencyMode::None,
            recheck: false,
            route_warnings: vec![],
            checkpoint: None,
            iblt_capacity,
            fetch_all_threshold: 4096,
            naive_max_rows: 4096,
            strict: false,
            scns: std::sync::OnceLock::new(),
            verbose: false,
        }
    }

    /// d diff rows on the left only (MissingRight), keys 1..=d.
    /// All four slices vary with k and stay collision-free mod 64 for
    /// d <= 64 (offsets 1000/2000/3000 are non-multiples of 64), so
    /// round 1 (m=64) decodes deterministically for small d.
    fn left_only_entries(d: u64) -> Vec<(u64, [u64; 4])> {
        (1..=d)
            .map(|k| (k, [k, k + 1000, k + 2000, k + 3000]))
            .collect()
    }

    /// d diff rows whose four slices are all constant: every entry lands
    /// in the SAME single cell of each of the 4 subtables at EVERY m, so
    /// that cell has cnt=+d (never ±1), the peel queue starts empty, and
    /// decode fails deterministically in both rounds (Σ|cnt| = 4·d intact
    /// for d̂). Values must not be all-zero (would read as empty diff).
    fn colliding_entries(d: u64) -> Vec<(u64, [u64; 4])> {
        (1..=d).map(|_| (0xdead, [1, 1, 1, 1])).collect()
    }

    async fn run_iblt(
        lconn: &mut (dyn DbConn + Send),
        rconn: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
    ) -> Result<DiffReport, DbError> {
        IbltDiffer.diff(lconn, rconn, ctx).await
    }

    #[tokio::test]
    async fn auto_mode_round1_decodes_small_diff() {
        // d=10 → round 1 (m=64) decodes directly; only 2 IBLT queries.
        let mut l = MockIbltConn::with_entries(left_only_entries(10), vec![]);
        let mut r = MockIbltConn::identical();
        let ctx = ctx_with_capacity(0);

        let report = run_iblt(&mut l, &mut r, &ctx).await.expect("decode");
        assert_eq!(report.summary.missing_right, 10);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("iblt: capacity=auto") && w.contains("m=64")));
        assert_eq!(qcount(&l), 1);
        assert_eq!(qcount(&r), 1);
    }

    #[tokio::test]
    async fn auto_mode_round2_resizes_from_estimate() {
        // d=200: round 1 (m=64) has guaranteed collisions (200 keys land
        // in 64 buckets per subtable) -> decode fails. Every entry
        // contributes exactly +1 per subtable (right side empty), so
        // Σ|cnt| = 4·200 = 800 and d̂ = 800/2 = 400 -> m₂ = 1200, which
        // separates the entries again -> round 2 decodes.
        let mut l = MockIbltConn::with_entries(left_only_entries(200), vec![]);
        let mut r = MockIbltConn::identical();
        let ctx = ctx_with_capacity(0);

        let report = run_iblt(&mut l, &mut r, &ctx)
            .await
            .expect("decode in round 2");
        assert_eq!(report.summary.missing_right, 200);
        assert_eq!(qcount(&l), 2);
        assert_eq!(qcount(&r), 2);
        let note = report
            .warnings
            .iter()
            .find(|w| w.contains("iblt: capacity=auto"))
            .expect("auto note present");
        assert!(
            note.contains("m=1200"),
            "note should record round-2 m: {note}"
        );
    }

    #[tokio::test]
    async fn auto_mode_falls_back_after_two_failures() {
        // Constant slices 3/4 keep one overloaded cell per subtable at
        // every m: round 1 (m=64) and round 2 (m₂=3·d̂, d̂=10000 here)
        // both fail -> transparent hashdiff fallback with auto note.
        let d = 5000;
        let mut l = MockIbltConn::with_entries(colliding_entries(d), vec![]);
        let mut r = MockIbltConn::identical();
        let mut ctx = ctx_with_capacity(0);
        ctx.left_pool = mock_pool(&l);
        ctx.right_pool = mock_pool(&r);

        let report = run_iblt(&mut l, &mut r, &ctx).await.expect("fallback");
        assert_eq!(report.strategy, "hashdiff");
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("fallback: hashdiff") && w.contains("auto-capacity")));
        // 2 IBLT rounds × 2 sides + fallback MIN/MAX per side
        assert!(qcount(&l) >= 3);
    }

    #[tokio::test]
    async fn auto_mode_strict_errors_after_two_failures() {
        let d = 5000;
        let mut l = MockIbltConn::with_entries(colliding_entries(d), vec![]);
        let mut r = MockIbltConn::identical();
        let mut ctx = ctx_with_capacity(0);
        ctx.strict = true;
        ctx.left_pool = mock_pool(&l);
        ctx.right_pool = mock_pool(&r);

        let err = run_iblt(&mut l, &mut r, &ctx)
            .await
            .expect_err("strict must surface capacity error");
        // d̂ = 4·5000/2 = 10000 -> m₂ = 30000 must appear in the message
        assert!(err.to_string().contains("auto-capacity"), "{err}");
        assert!(err.to_string().contains("30000"), "{err}");
    }

    #[tokio::test]
    async fn fixed_capacity_path_unchanged_single_round() {
        // capacity 1024 → m=768 in round 1, no second round, no auto note.
        let mut l = MockIbltConn::with_entries(left_only_entries(200), vec![]);
        let mut r = MockIbltConn::identical();
        let ctx = ctx_with_capacity(1024);

        let report = run_iblt(&mut l, &mut r, &ctx).await.expect("decode");
        assert_eq!(report.summary.missing_right, 200);
        assert_eq!(qcount(&l), 1);
        assert_eq!(qcount(&r), 1);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("iblt: capacity=1024 cells=3072")));
        assert!(!report.warnings.iter().any(|w| w.contains("capacity=auto")));
    }

    #[tokio::test]
    async fn fixed_capacity_fallback_warning_unchanged() {
        // capacity 64 → m=48 < 200 diffs → immediate hashdiff fallback with
        // the exact legacy warning text.
        let mut l = MockIbltConn::with_entries(left_only_entries(200), vec![]);
        let mut r = MockIbltConn::identical();
        let mut ctx = ctx_with_capacity(64);
        ctx.left_pool = mock_pool(&l);
        ctx.right_pool = mock_pool(&r);

        let report = run_iblt(&mut l, &mut r, &ctx).await.expect("fallback");
        assert_eq!(report.strategy, "hashdiff");
        assert!(report
            .warnings
            .iter()
            .any(|w| w == "fallback: hashdiff (iblt capacity exceeded, d > 64)"));
        let _ = qcount(&l);
    }

    #[tokio::test]
    async fn decode_success_notes_report_totals() {
        // equal sides → empty summaries → "decoded-empty" path.
        let mut l = MockIbltConn::identical();
        let mut r = MockIbltConn::identical();
        let ctx = ctx_with_capacity(0);

        let report = run_iblt(&mut l, &mut r, &ctx).await.expect("decode");
        assert!(report.shards.iter().all(|s| s.status == ShardStatus::Match));
        assert_eq!(
            report.summary.missing_left + report.summary.missing_right,
            0
        );
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("note=decoded-empty")));
    }

    // ── m extraction parser itself ───────────────────────────────────

    #[test]
    fn iblt_m_parser_reads_rendered_sql() {
        let d = MySqlDialect;
        let spec = crate::backend::IbltSqlSpec {
            schema: None,
            table: "t".into(),
            key_expr: "`id`".into(),
            normalized_exprs: vec!["CAST(`id` AS CHAR)".into()],
            cells_per_subtable: 64,
            filter: None,
            scn: None,
        };
        let sql = d.render_iblt_sql(&spec).expect("mysql render");
        assert_eq!(MockIbltConn::iblt_m_from_sql(&sql), Some(64));
    }

    // ── B1 回归：snapshot 失败路径必须先 COMMIT 再回退/报错 ──

    fn ctx_snapshot(capacity: u64) -> DiffContext {
        let mut ctx = ctx_with_capacity(capacity);
        ctx.consistency = ConsistencyMode::Snapshot;
        ctx
    }

    #[tokio::test]
    async fn snapshot_mode_capacity_fallback_commits_before_hashdiff() {
        // 评审 B1：WP1 之前（1951c96）失败路径也会 COMMIT 关掉快照，
        // hashdiff 回退才能在自己的连接上开新快照；Oracle 在已开事务上
        // SET TRANSACTION READ ONLY 会 ORA-01453。回退路径必须先 COMMIT。
        let d = 5000;
        let mut l = MockIbltConn::with_entries(colliding_entries(d), vec![]);
        let mut r = MockIbltConn::identical();
        let mut ctx = ctx_snapshot(0);
        ctx.left_pool = mock_pool(&l);
        ctx.right_pool = mock_pool(&r);

        let report = run_iblt(&mut l, &mut r, &ctx).await.expect("fallback");
        assert_eq!(report.strategy, "hashdiff");
        let drops_l = l.drops.lock().unwrap().clone();
        let drops_r = r.drops.lock().unwrap().clone();
        assert!(
            drops_l.iter().any(|s| s == "COMMIT"),
            "left must COMMIT before hashdiff fallback: {drops_l:?}"
        );
        assert!(
            drops_r.iter().any(|s| s == "COMMIT"),
            "right must COMMIT before hashdiff fallback: {drops_r:?}"
        );
        // 回退后（hashdiff 成功路径）各自再开一次快照；先 COMMIT 后 START。
        let start_before_commit = |drops: &[String]| {
            let commit = drops.iter().position(|s| s == "COMMIT").unwrap();
            drops[commit + 1..]
                .iter()
                .any(|s| s.contains("START TRANSACTION"))
        };
        assert!(start_before_commit(&drops_l));
        assert!(start_before_commit(&drops_r));
    }

    #[tokio::test]
    async fn snapshot_mode_strict_error_still_commits() {
        // --strict 下解码失败报错，但快照同样必须关闭（调用方可能直接
        // 重试同一连接）。
        let d = 5000;
        let mut l = MockIbltConn::with_entries(colliding_entries(d), vec![]);
        let mut r = MockIbltConn::identical();
        let mut ctx = ctx_snapshot(0);
        ctx.strict = true;

        let err = run_iblt(&mut l, &mut r, &ctx)
            .await
            .expect_err("strict must surface capacity error");
        assert!(err.to_string().contains("capacity exceeded"), "{err}");
        assert!(l.drops.lock().unwrap().iter().any(|s| s == "COMMIT"));
        assert!(r.drops.lock().unwrap().iter().any(|s| s == "COMMIT"));
    }

    #[tokio::test]
    async fn snapshot_mode_success_commits_exactly_once() {
        // 成功路径行为不变：finish() 里 COMMIT 一次；close_snapshot 在
        // 失败臂才会触发，不得重复提交。
        let mut l = MockIbltConn::with_entries(left_only_entries(10), vec![]);
        let mut r = MockIbltConn::identical();
        let ctx = ctx_snapshot(0);

        let report = run_iblt(&mut l, &mut r, &ctx).await.expect("decode");
        assert_eq!(report.summary.missing_right, 10);
        let commits_l = l
            .drops
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.as_str() == "COMMIT")
            .count();
        let commits_r = r
            .drops
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.as_str() == "COMMIT")
            .count();
        assert_eq!(commits_l, 1);
        assert_eq!(commits_r, 1);
    }
}
