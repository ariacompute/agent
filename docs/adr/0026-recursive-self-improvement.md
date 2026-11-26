# ADR-0026 — Recursive self-improvement: diagnose, propose, verify, version, reuse

- Status: accepted
- Date: 2026-11-26
- Supersedes: —
- Related: [ADR-0015](./0015-session-event-stream.md)（事件流是账本）、[ADR-0014](./0014-plugin-capability-seams.md)（capability seam）、[ADR-0016](./0016-tool-pipeline-and-approvals.md)（审批失败关闭）、[ADR-0017](./0017-subagents-teams-schedule.md)（周期作业）、[ADR-0019](./0019-dsh-runtime.md)（工具归属边界）

## Context

本仓库的能力沉淀是**手工**的：`.aria/skills/<name>/SKILL.md` 由人写，`FilesystemSkills` 只读扫描，模型在需要时自行 `skill_load`。没有任何机制把「上一次任务踩过的坑」变成「下一次任务的默认做法」，因此同一个低效（同一个工具反复失败、同一轮被拒两次、上下文反复压缩）会在每个租户、每个会话里重复发生。

参考材料（AGNES HARNESS 的 RSI 叙事 + `AgnesAI-Labs/agnes-harness` 的 human-in-the-loop 能力沉淀）给出四段闭环：执行留痕 → 复盘定位 → 提出改进 → **验证有效才沉淀** → 下次同类任务复用。其中两条警示是硬约束：

1. **先验证再沉淀**：偶然成功不是通用经验，未经验证的方案不得进入默认路径。
2. **跨租户必须适配**：一个客户的完整方案（阈值、字段名、审批口径）不能原样复制给另一个客户，否则会产出错误结论。

仓库这边还多两条自己的约束：

- **Reef 已被删除**（`requirements.md §7`）：`record → feedback → evolve → version → hot-serve` 那一套，包括 git `reef@<n>` 版本化与**热替换 harness**。因此本方案不得复活 hot-swap，也不得让系统提示词在运行中改变。
- **事件词表是冻结契约**（ADR-0015 / `seams/agent_loop.rs`）：`SessionEvent` 的变体集合被两种 harness 与云端 SSE 信封共同约束，加变体等于破协议。

## Decision

### 1. 三层落点，方向不变

```
aria-agent-core        纯折叠：SessionEvent → EfficiencyReport（零 I/O、零模型调用）
      ↑
aria-agent-rsi         引擎：账本 / 提案 / 离线重放 / 版本 / 技能落盘
      ↑
aria-agent-cloud       只有 HTTP + Postgres + 装配
```

- `core` 拥有 `SessionEvent`、`SessionRecord`、`SessionProjection`，所以**诊断就是一次折叠**，放在 `session/insight.rs`。它不写库、不调模型、不读文件，因此可 100% 覆盖且结果可复现。
- `aria-agent-rsi` 只依赖 `core`（与 `orchestrator` 同级），承载 `RsiLedger`、`DeterministicReflection`、`MemoryPlaybooks`、`FilesystemSkillSink`、`RsiSweep`。
- `cloud` 只做 `api/rsi.rs` 的薄 handler、`store/pg_rsi.rs` 的端口实现、`migrations/0009_rsi.sql` 的表与装配。

### 2. 事件契约只做加法

`SessionEvent::ItemDone` 增加 `#[serde(default)] latency_ms: Option<u64>`。

- 不新增变体：SSE 信封与两种 harness 的事件序列因此完全不变。
- 缺省即 `None`：旧 JSONL 记录与不测时的 harness（dsh）都能解析；报告退化为「步数 / 重试 / 审批」维度而不是报错。
- 时长只用于区分「工具慢」与「工具选错」，不参与任何授权判断。

### 3. 诊断是病理学，不是评分

`EfficiencyReport` 给出 `Finding { pathology, tool, occurrences, evidence }`，`pathology` 是封闭词表（`repeated_tool_failure` / `retry_storm` / `approval_denied_loop` / `compaction_churn` / `step_budget_exhausted` / `tool_argument_repair`），`evidence` 是**事件序号链**。评分只是把病态折算成一个可对比的整数（`health_score`，单调、封顶 100），存在的唯一理由是让「改进是否有效」能被一个数字比较。

折叠受三重上限约束（记录数、回合数、每条证据条数）并**显式报告截断**，长会话不会把一次请求拖死。

### 4. 提案来自规则表，验证来自重放

- `propose` 是**确定性规则表**：病理 → 可执行步骤，模板里带工具名与出现次数，不带任何租户特定阈值（否则无法跨租户适配）。模型改写可以藏在同一个 `ReflectionEngine` seam 后面，默认不启用。
- `validate` 是**离线重放**：用同一份 `health_score` 重算「把该病理解决掉之后的历史」，增益不达 `min_score_gain` 即 `BelowThreshold`，提案作废。因为评分函数只有一份，「已验证」不是自说自话。
- 真实重跑 (`RerunProbe`) 是**第二个可选闸门**：它花模型调用、有副作用，所以独立开关；开关打开但没有接线时 `validate` 返回 `Unavailable`，**绝不**静默降级成只做离线校验。

### 5. 闸门默认是人，且是现有审批抽象

`PromotionGate`：

| 取值 | 行为 |
| --- | --- |
| `Human`（默认） | 只有显式 `promote` 才发布 |
| `Auto` | 受控部署（`rsi.auto_promote = true`）验证通过即发布 |
| `Broker` | 询问 `ApprovalBroker`；**只有 `Allow` 发布**，`Deny` 与 `Unavailable` 一律留在 pending |

`Unavailable` 视为拒绝沿用 ADR-0016 的失败关闭语义：无人应答不等于同意。

### 6. 版本化照抄 revision guard，不复活 hot-swap

`PlaybookStore::promote(proposal_id, expected_revision)` 携带审批者「我认为当前是第几版」。不匹配 → `SeamError::Conflict` → HTTP `409`，候选**留在 pending**，绝不 last-writer-wins（与 ADR-0021 的 egress policy revision 同一套语义）。Postgres 侧再用 `UNIQUE (principal_id, domain, key, revision)` 兜底，并发发布在约束上失败而不是互相覆盖。

发布产出的是**下一轮读取的资产**：

- 手册正文按 `key` 线性版本化，`Playbook::as_prompt_section()` 带 `rN` 版本戳；
- 工具相关的手册额外蒸馏成 `<workspace>/.aria/skills/rsi-<pathology>-<tool>/SKILL.md`，由既有 `FilesystemSkills` 在下次 `skill_list` 时看到。

**不替换正在运行的系统提示词**。这与 Reef 的 hot-serve 是本质区别：RSI 增加可选资产，不改变已加载的 prompt。

### 7. 跨租户复用必须留下适配记录

`PlaybookCandidate` 区分 `origin`（证据来自哪里）与 `scope`（将要在哪里生效）。两者不同时强制要求 `Adaptation { from_*, to_*, rewritten, rationale }`：

- 缺失 → `SeamError::Unadapted` → HTTP `422`；
- 目标不符 / 未列出改写项 / 无依据 → 同样 `422`；
- 读另一个租户的手册需要 admin，否则 `403`——不能靠「404 与 403 的差异」探测别人的能力。

`POST /v1/rsi/playbooks/{key}/adapt` 只**产生 pending 提案**，适配不等于发布。

### 8. 触发不新增后台线程语义

- 入账：`SessionPersistence` 链。`ChainPersistence` 的**第一个 sink 是权威**（失败即整体失败，`SessionRecorder` 会重试），其后的 sink 是尽力而为（失败只告警）。于是云端 `TitleProjection` 仍是权威，`RsiLedger` 是可选的第二 sink，并且按 `(principal, session)` 分键，天然租户隔离。
- 复盘：`TurnCompleted` 之后按阈值累积信号；`RsiSweep` 单飞（并发调用返回 `busy`），单条会话失败只记录不中断。
- 周期兜底：进程内定时器默认关闭（`AGENT_RSI_SWEEP_SECS=0`），由 `POST /v1/rsi/sweep` 交给外部调度器驱动；仓库里**没有**现成的 tick 守护进程，不假装有。

### 9. 默认关闭

`RsiConfig::enabled = false`。关闭时：不建账本（`RsiState.ledger = None` → 链上只有一个 sink）、不启动定时器、`/v1/rsi/sweep` 返回 `503`。既有执行路径与成本零变化。

## Consequences

- **新增两个 seam key**：`rsi`（`ReflectionEngine`）与 `playbook`（`PlaybookStore`）。二者必须同时登记进 `plugin/manifest.rs` 的 `SeamKey::ALL`，否则插件声明会在启动期 `UnknownSeam`。未接线时 `NoReflection` / `NoPlaybooks` **失败关闭**：`analyze` 返回空报告，`propose`/`validate`/`submit`/`promote` 返回 `Unavailable`。
- **新增两个稳定错误码**：`conflict`（409）与 `unadapted`（422），进入 `SeamError::code()` 的公开契约（工具失败文本、HTTP 映射、审计记录共用）。
- **报告是持久事实，账本是缓存**：`rsi_reports` 存诊断与证据链（可追溯到 `seq`），`RsiLedger` 只是有界的内存窗口（每会话 `max_records`、全局 `max_sessions`，按最近写入淘汰）。重启后账本为空，报告与手册仍在——这是有意的取舍：账本的权威副本一直是既有的事件流。
- **dsh 会话能用到沉淀出的技能**：`DshConfig::skill_catalog()` 从 workspace 布局推导 `<root>/.aria/skills`，`Runtime::connect` 把它链进 `<cwd>/.aria/skills`（Unix symlink，其他平台递归复制）。失败只告警：缺一个技能是能力降级，不是回合失败。
- **可观测但不泄漏**：日志只记录计数、标识与版本号；手册正文、工具参数、用户输入一律不进日志。`ReportView`/`PlaybookView` 不出现 `principal`。
- **成本边界**：离线重放零成本，可在请求路径内同步完成；真实重跑是唯一有外部成本的动作，独立开关 + 独立超时。
- **覆盖率**：`pg_rsi.rs` 与其余 `store/pg*.rs` 同类排除（需要真库），其语义由 `MemoryStore`/`MemoryPlaybooks` 在 `rsi_tests.rs` 里覆盖，包括 `409`、`422`、`403` 三条规则。

## Alternatives considered

- **复活 Reef（含 hot-swap 与 git tag）**：被拒。热替换运行中的 harness 意味着同一会话的行为中途改变，不可审计也不可回滚；`requirements.md §7` 已把它列为删除项。RSI 用「显式版本 + 下一轮读取」达到同样的复用目的。
- **`rust 侧解析 cron`**：被拒。ADR-0017 刻意不引入 cron 解析器；周期触发留给外部调度器（或进程内固定间隔），RSI 不再自造一套。
- **让提案直接调模型生成修复**：默认不做。规则表可离线测试、可复现、免费，而且**失败模式正确**（没有规则的病理就不提案）；模型改写作为可选后端保留在同一 seam 上。
- **新增 `SessionEvent` 变体记录工具耗时**：被拒。事件词表是冻结契约，加变体破 SSE 兼容；加一个 `serde(default)` 字段达到同样目的。
- **用 `ApprovalBroker` 之外的第二套审批**：被拒。闸门复用了既有抽象，因此 `Unavailable` 的失败关闭语义自动继承。
