# aria agent

[English](README.md) | [中文](README_cn.md)

将 Aria 组件接入 **DeepSeek Harness（`dsh`）** 的 out-of-tree 接入层。

- **LLM 后端**：`engine` 以进程内方式接入，由 `aria-engine` 插件（`@ariacompute/engine-ts` FFI SDK）接入。
- **Context memory**：`memo` 经 `aria-memo` 插件接入（tools `aria_memo_*`，可选 `autoInject`）。
- **Agent 沙盒**：[CubeSandbox](https://github.com/TencentCloud/CubeSandbox)（E2B 兼容）
  经 `aria-sandbox` 插件接入——每个 agent 拥有一个隔离的持久化工作空间。
- **自我改进**：`aria-reef`——Reef 式闭环：记录每个回合 → 绑定反馈 → 演化技能/提示词/规则（本地 engine FFI）
  或派发权重训练（ariapin）→ 评估保留胜者 → Git 版本化 → 热交付回运行中的 agent。**默认关闭**。

## 架构

```
dsh/plugins/
├── shared/          config / AriaError / spawn 助手（替代 aria-bridge）
├── aria-engine/     ctx.llm.registerAdapter(['aria'], …) → engine 进程内 FFI（支持 tool calls）
├── aria-memo/       aria_memo_add/search/get/list/forget + autoInject（默认关闭）
├── aria-sandbox/    sandbox_exec / sandbox_read_file / sandbox_write_file /
│                    sandbox_list_files / sandbox_sync_to_host / sandbox_sync_from_host /
│                    workspace_status（经 e2b SDK 连 CubeSandbox）
└── aria-reef/       Serve（签发 record_id）→ Observe（report/任务结果/rubric）→
                     Grow（skillclaw / prompt / rules / weight）→
                     Commit（git + LFS）→ Surface（热更新）
scripts/cube-sandbox-up.sh   本地一键拉起 CubeSandbox + 建模板
```

## 依赖

- [Bun](https://bun.com) >= 1.4（`bun install` / `bun test` / `bunx tsc`）。
- `aria-engine` bundle 路径 `ARIA_MODEL_BUNDLE` 与 原生库 `ARIA_FFI_LIB`（无需 HTTP 服务）。
- `aria-memo` CLI，路径 `ARIA_MEMO_BIN`（默认 `aria-memo`）。
- CubeSandbox：x86_64 Linux + KVM（`/dev/kvm`），见 [CubeSandbox](https://github.com/TencentCloud/CubeSandbox)。

## 安装

```sh
bun install          # 网络受限时先 export https_proxy=http://127.0.0.1:7897
scripts/cube-sandbox-up.sh   # 检查 KVM、安装 CubeSandbox、创建模板、写 .env
```

## 运行

```sh
export ARIA_MODEL_BUNDLE=/path/to/aria/model/bundle
export ARIA_FFI_LIB=/usr/lib/libaria_ffi.so
bun dsh web --patch /绝对路径/agent/dsh/cordis.patch.yml
```

选择 provider 路由 `aria`。沙盒模板 id 由 `CUBE_TEMPLATE_ID` 读取（env 或 `.env`）；未设置时 `sandbox_*` 工具会明确报错。

## 工作空间（隔离）

- 每个 agent 会话映射一个工作空间 id：工具参数 `workspace` → `sessionId` → 配置 → `default`。
- 宿主目录：`$ARIA_WORKSPACE_ROOT/<workspaceId>/`（默认 `~/.ariacompute/agent/workspaces`，0700）。
- 有 dsh `ctx.workspaceRegistry` 时注册（缺失回退纯目录）。
- 隔离三层：每工作空间独立 KVM MicroVM（CubeSandbox）+ 独立 0700 宿主目录 + 工具层路径校验（仅 `/workspace`，拒绝 `..` 逃逸）。
- 持久化：`sandbox_sync_to_host` / `sandbox_sync_from_host`；可选 `ARIA_WORKSPACE_SYNC_AFTER_EXEC=true` 每次 `sandbox_exec` 后自动拉回。

## 配置

完整环境变量表见 `requirements.md` §2（`ARIA_MODEL_BUNDLE`、`ARIA_FFI_LIB`、`ARIA_ENGINE_MODEL`、`ARIA_MEMO_*`、`E2B_API_URL`、`E2B_API_KEY`、`CUBE_TEMPLATE_ID`、`E2B_TIMEOUT_MS`、`ARIA_WORKSPACE_ROOT`、`ARIA_WORKSPACE_SYNC_AFTER_EXEC`、`ARIA_WORKSPACE_ID`、`ARIA_REEF_*`）。

## 自我改进（aria-reef）

```sh
export ARIA_REEF_ENABLED=on
export ARIA_REEF_RELEASE=v1            # 打在每条 record 上的 release id
export ARIA_REEF_BUNDLE=$ARIA_MODEL_BUNDLE   # harness recipe 用本地引擎提议编辑
export ARIA_REEF_AUTO_APPLY=on         # 候选胜出后自动发布
# 权重 recipe（可选）：export ARIA_REEF_RECIPES=skillclaw,prompt,rules,weight
#                    export ARIA_REEF_ARIAPIN_URL=http://127.0.0.1:8001
#                    export ARIA_REEF_ARIAPIN_KEY=... ARIA_REEF_BASE_MODEL=...
# 定时自动 cycle（默认关闭）：export ARIA_REEF_CYCLE=on
#                    export ARIA_REEF_CYCLE_INTERVAL_MS=900000 ARIA_REEF_CYCLE_MIN_SIGNALS=8
```

- 每个回合签发 `record_id`（回传为 `reef.recordId`）；落库为 detached promise，绝不阻塞 agent 主回路。
- 反馈三路：`aria_reef_report`（评分 0..1 或 1..5 / 文本 / 结构化 JSON）、任务结果信号、确定性 rubric 自动评分。
- `aria_reef_cycle` 手动跑一轮 Grow → Evaluate → Commit → Surface；`aria_reef_status` 查看计数器。
- 可选定时自动 cycle（`ARIA_REEF_CYCLE=on`）：距上次运行超过间隔**或**新信号达阈值即触发一轮；单飞互斥（在飞则跳过）、detached 执行不进主回路、失败指数退避；`aria_reef_status` 附 `autoCycle` 状态。
- 被接受的 harness 构件每回合实时读盘（热更新、无需重启）；权重经 ariapin 触发热重载（`POST /v1/models/{id}/reload`，失败且已知 agent 时回退 agent 重启）。
- 存储：`$ARIA_REEF_STORE_DIR`（JSONL）+ `$ARIA_REEF_ARTIFACT_REPO`（本地 Git，权重走 LFS）。

## 开发

```sh
bun test            # 离线单测（shared + engine + memo + sandbox + reef）
bun run typecheck   # bunx tsc --noEmit 各插件
```

注意：memo `search` 返回 `score\tcontent` 行（无 id）；`engine` 需先启动；`model/` 不在范围；GitHub/registry 访问可能需要 `export https_proxy=http://127.0.0.1:7897`。
