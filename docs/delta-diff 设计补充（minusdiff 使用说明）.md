# `minusdiff` 使用说明

给操作者。路由、SQL 和失败语义的设计在 [delta-diff 设计补充（minusdiff 策略设计）.md](delta-diff%20设计补充（minusdiff%20策略设计）.md)（路由表、运算符表，文末「Auto 提升」是现行 auto）。本文不改 v2.1，也不改 scan-once 补充里「不扩展 joindiff 多列 `ON`」那一条。

`auto` 会在下面这张表成立时选 `minusdiff`。显式 `--strategy` 不被改写：点了 `keyeddiff` 或 `joindiff` 就还是那一个。

## auto 什么时候选

两侧 URL 字符串相等，并且两侧键都唯一（非空、`NOT NULL`、主键或非部分唯一索引）：

| 键 | 是否仅统计 | 服务器 | auto |
|---|---|---|---|
| 复合键，或不是整数的键 | 不论 | 全部，含 MySQL | minusdiff |
| 单列整数 | 是（`--summary-only` 生效） | Oracle、GaussDB、DuckDB | minusdiff |
| 单列整数 | 否（要行差） | 全部 | joindiff |
| 单列整数 | 是 | MySQL 系，含 PolarDB-X | joindiff |

无键仍是 bucketdiff。键不唯一、键可空、或两条 URL 不同，auto 不选 minusdiff。`--summary-only` 和 `--export` 一起用时，仅统计被忽略，单列整数仍是 joindiff。

MySQL 的单列整数即使只统计也留在 joindiff。这个服务器要执行时才知道有没有 `EXCEPT`；没有的话回退是 keyeddiff，PolarDB-X 会离开索引连接。复合键和字符串键的回退目标本来就是 keyeddiff，所以 MySQL 也会先试 minusdiff。

## 什么时候它更快

左右是同一条 URL，两侧键都唯一，键是复合列或字符串，差异打散在 keyeddiff 的哈希桶上。集合差只把差集传回客户端。仅统计时，Oracle / GaussDB / DuckDB 的唯一整数键用一条三计数语句，不再把 JOIN 的行体拉回来。

行差的同 URL 单列整数键仍是 joindiff，走索引。`minusdiff` 会对全列做哈希，也不是 joindiff 多列 `ON` 的另一种写法。

两侧表相同、校验和已经相等时，`minusdiff` 停在校验和，不比 keyeddiff 的提前停止更快。

## 命令

auto 已经会选时，不必再写 `--strategy`。仅统计加上 `--summary-only`。下面是显式点名，形状不合适时会带 warning 回退，而不是 exit 2：

```bash
hepta_dbcli delta-diff --left ora_dev --right ora_dev \
  --left-table LEFT_T --right-table RIGHT_T \
  --strategy minusdiff
```

两条 URL 必须字符串相等。Oracle 跨 schema、同一服务 URL 可以。MySQL 两个 database 是两条 URL，不行。查询只走左连接，右表用左会话里的 schema 和表名引用。

## 运算符

| 服务器 | 发出的关键字 |
|---|---|
| Oracle（`oracle` 与 `oracle_native`） | `MINUS` |
| GaussDB、DuckDB | `EXCEPT` |
| MySQL 8.0.31 及以上（普通版本串） | `EXCEPT` |
| MySQL 8.0.30 及更早 | 不发，回退 keyeddiff |
| 版本串含 `PXC`（本仓库用它代表 PolarDB-X） | 不发，不探测，回退 keyeddiff |
| MariaDB，或版本串含 `POLARDB` | 先执行 `SELECT 1 AS c EXCEPT SELECT 1 AS c`；失败则回退，不碰用户表 |

不发 `EXCEPT ALL` 或 `MINUS ALL`。键不唯一时集合差会丢掉重复行，所以那种形状直接回退，而不是改发 `ALL`。

## 显式 `--strategy minusdiff` 形状不对时

auto 选不上时不会写「falling back」warning，直接走上表里的策略。下面是点名 `minusdiff` 之后的回退：

| 情况 | 实际策略 |
|---|---|
| 没有键 | bucketdiff |
| 键不唯一，或键列可空 | keyeddiff |
| URL 不同，单列整数键 | hashdiff |
| URL 不同，复合键或非整数键 | keyeddiff |
| URL 相同，两侧键都唯一 | minusdiff（单列整数也保持 minusdiff，不改写成 joindiff） |

回退会写 warning，比对继续。列对不齐、集合差语句失败（含临时空间写满、语句超时）、结果里规范化键重复：exit 2，不回退。`--strict` 不参与这里。

## 预算、样本、超时、断点

`--minus-max-cells` 默认 `50000000`。执行期用 `max(两侧 COUNT) × 比对列数` 和它比较。超过则在发集合差之前回退 keyeddiff，warning 里带 `×`。`0` 表示不限制，只在确认临时空间够用时使用。MCP 省略 `minus_max_cells` 时用同一个默认值。

dry-run 能通过形状检查并打印 `strategy : minusdiff`，但不会探测版本，也不会做 `COUNT`。所有方言都会看到：`minusdiff checks capability and cell budget at execution; dry-run does not`。键域行是 `key domain : (not applicable — minusdiff)`，不发 `MIN`/`MAX`。

`--sample` 和 MCP `sample_limit` 只裁剪终端或返回样本。计数和集合差语句都是全量，样本变小不会减少服务器工作。

`--statement-timeout` 在 MySQL 上是 `max_execution_time`，在 GaussDB 上是 `statement_timeout`。Oracle 和 DuckDB 没有对应语句，这条旗标取消不了它们上面的集合差。临时空间写满或语句超时是 exit 2，不会改走 keyeddiff。

不写分片断点。传了 `--checkpoint` 会警告：`minusdiff does not record shard checkpoints; a cancelled statement restarts from scratch`。取消后整句从头再来。

`--recheck` 被忽略（snapshot 默认会打开复核，因此默认也会看到这条警告）：`minusdiff ignores --recheck: classification comes from one set-difference statement`。分类来自一条集合差语句，不再做当前读复核。

`--summary-only` 在有差异时用一条三计数语句代替行体。空的一侧在 summary 下不再额外 `SELECT`。
