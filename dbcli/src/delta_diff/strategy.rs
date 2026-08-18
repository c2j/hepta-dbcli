// ─── delta-diff strategy: 策略 trait 与比对上下文（§5.2）────────────────

use std::sync::Arc;

use crate::backend::{DbConn, DbError, DbPool};
use crate::delta_diff::metadata::TablePlan;
use crate::delta_diff::report::DiffReport;

/// 单侧上下文
#[derive(Debug, Clone)]
pub(crate) struct SideCtx {
    pub(crate) connection_name: String,
    pub(crate) schema: Option<String>,
    pub(crate) table: String,
    pub(crate) plan: TablePlan,
}

/// 比对上下文（MVP：单列整型键，§6.4）
pub(crate) struct DiffContext {
    pub(crate) left: SideCtx,
    pub(crate) right: SideCtx,
    /// none 模式的并行快筛连接池（snapshot 模式不使用——快照绑定单连接）
    pub(crate) left_pool: Arc<dyn DbPool>,
    pub(crate) right_pool: Arc<dyn DbPool>,
    pub(crate) key_column: String,
    pub(crate) filter: Option<String>,
    /// 增量比对（--update-column/--update-since）：(列, 窗口表达式)，
    /// 谓词由 side_filter 按方言渲染
    pub(crate) incremental: Option<(String, String)>,
    pub(crate) bisection_factor: usize,
    pub(crate) bisection_threshold: u64,
    pub(crate) sample_limit: usize,
    pub(crate) threads: usize,
    pub(crate) consistency: ConsistencyMode,
    /// 差异行二次复核（§8.3）
    pub(crate) recheck: bool,
    /// 路由层告警（键降级/回退/语义提示），并入报告 warnings
    pub(crate) route_warnings: Vec<String>,
    /// 断点续传（§13.2；Arc<Mutex> 以穿越二分递归的借用链）
    pub(crate) checkpoint:
        Option<std::sync::Arc<tokio::sync::Mutex<crate::delta_diff::progress::CheckpointManager>>>,
    /// IBLT 预期差异容量 d（k=3d 桶，Addendum §2.4）
    pub(crate) iblt_capacity: u64,
    /// IBLT 解码失败时报错（exit 2）而非透明回退
    pub(crate) strict: bool,
    /// Oracle AS OF SCN 锚点（左, 右），快照开启后由策略捕获（§8.2）
    pub(crate) scns: std::sync::OnceLock<(Option<u64>, Option<u64>)>,
}

impl DiffContext {
    /// 本侧 SCN（若已捕获且为 Oracle 快照模式）
    pub(crate) fn scn_of(&self, is_left: bool) -> Option<u64> {
        self.scns
            .get()
            .and_then(|(l, r)| if is_left { *l } else { *r })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsistencyMode {
    Snapshot,
    None,
}

impl ConsistencyMode {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            ConsistencyMode::Snapshot => "snapshot",
            ConsistencyMode::None => "none",
        }
    }
}

/// 比对策略（§5.2 DiffStrategy）
#[async_trait::async_trait]
pub(crate) trait DiffStrategy: Send + Sync {
    fn name(&self) -> &'static str;

    async fn diff(
        &self,
        left: &mut (dyn DbConn + Send),
        right: &mut (dyn DbConn + Send),
        ctx: &DiffContext,
    ) -> Result<DiffReport, DbError>;
}

/// 单侧有效过滤条件：--where 原样；增量窗口按方言渲染
/// （字面时间三方言通用；"N day(s)/hour(s)" 分别展开）。
pub(crate) fn side_filter(ctx: &DiffContext, scheme: &str) -> Option<String> {
    if let Some(w) = &ctx.filter {
        return Some(w.clone());
    }
    let (col, since) = ctx.incremental.as_ref()?;
    let parts: Vec<&str> = since.split_whitespace().collect();
    if parts.len() == 2 && parts[0].chars().all(|c| c.is_ascii_digit()) {
        let n = parts[0];
        let (mysql, gauss, oracle) = match parts[1].trim_end_matches('s') {
            "day" => (
                format!("{col} >= DATE_SUB(NOW(), INTERVAL {n} DAY)"),
                format!("{col} >= NOW() - INTERVAL '{n} days'"),
                format!("{col} >= SYSDATE - {n}"),
            ),
            "hour" => (
                format!("{col} >= DATE_SUB(NOW(), INTERVAL {n} HOUR)"),
                format!("{col} >= NOW() - INTERVAL '{n} hours'"),
                format!("{col} >= SYSDATE - {n}/24"),
            ),
            _ => return Some(literal(col, since)),
        };
        return Some(match scheme {
            "gaussdb" => gauss,
            "oracle" => oracle,
            _ => mysql,
        });
    }
    Some(literal(col, since))
}

fn literal(column: &str, since: &str) -> String {
    format!("{column} >= '{}'", since.replace('\'', "''"))
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    fn ctx(filter: Option<&str>, inc: Option<(&str, &str)>) -> DiffContext {
        DiffContext {
            left: dummy_side(),
            right: dummy_side(),
            left_pool: dummy_pool(),
            right_pool: dummy_pool(),
            key_column: "id".into(),
            filter: filter.map(str::to_string),
            incremental: inc.map(|(a, b)| (a.to_string(), b.to_string())),
            bisection_factor: 32,
            bisection_threshold: 16384,
            sample_limit: 1000,
            threads: 4,
            consistency: ConsistencyMode::None,
            recheck: false,
            route_warnings: vec![],
            checkpoint: None,
            iblt_capacity: 65536,
            strict: false,
            scns: std::sync::OnceLock::new(),
        }
    }

    fn dummy_side() -> SideCtx {
        SideCtx {
            connection_name: "x".into(),
            schema: None,
            table: "t".into(),
            plan: crate::delta_diff::metadata::TablePlan {
                key_columns: vec![],
                compare_columns: vec![],
                norm_specs: vec![],
                warnings: vec![],
            },
        }
    }

    fn dummy_pool() -> std::sync::Arc<dyn DbPool> {
        struct P;
        #[async_trait::async_trait]
        impl DbPool for P {
            async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
                Err(DbError::unsupported("dummy"))
            }
        }
        std::sync::Arc::new(P)
    }

    #[test]
    fn where_takes_precedence() {
        let c = ctx(Some("a > 1"), Some(("ts", "1 day")));
        assert_eq!(side_filter(&c, "mysql"), Some("a > 1".into()));
    }

    #[test]
    fn relative_day_per_dialect() {
        let c = ctx(None, Some(("ts", "3 days")));
        assert!(side_filter(&c, "mysql")
            .unwrap()
            .contains("DATE_SUB(NOW(), INTERVAL 3 DAY)"));
        assert!(side_filter(&c, "gaussdb")
            .unwrap()
            .contains("NOW() - INTERVAL '3 days'"));
        assert!(side_filter(&c, "oracle").unwrap().contains("SYSDATE - 3"));
    }

    #[test]
    fn literal_datetime_quoted() {
        let c = ctx(None, Some(("ts", "2026-08-01 00:00:00")));
        assert_eq!(
            side_filter(&c, "mysql"),
            Some("ts >= '2026-08-01 00:00:00'".into())
        );
    }
}
