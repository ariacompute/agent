# 沙箱运行时特性与 DeepSeek Harness 容器化可行性评估

> 评估对象：`crates/aria-agent-sandbox`、`crates/aria-agent-core`（seam 层）、`crates/aria-agent-tools`、
> `crates/aria-agent-dsh`、`crates/aria-agent-cloud`（装配）、`Dockerfile`。
> 本文只读代码，不修改任何源文件、依赖或配置；所有结论标注置信度：
>
> - **【证】** 代码已证：给出 `file:line`。
> - **【推】** 基于已证事实的推断：写明前提，未被运行时验证。
> - **【测】** 需实测：代码无法回答（内存占用、启动延迟、kata 下 `commit` 是否可用等）。
>
> 核对基线：`ab8f60a`（`feat(rsi): diagnose, verify and version reusable playbooks`）＋当前工作区未提交改动。
> 本轮核对结论：`aria-agent-sandbox` 自上一版报告以来**没有改动**（仍无 attach / mount / pull / timeout 的实现），
> 两条路径的阻塞点清单与 provider 矩阵结论不变；变化集中在 RSI（改为默认开启、新增 tool-free re-run）与
> skills catalog 的统一，已反映在 §1.7 / §5.1 / §5.4。
>
> **状态更新（ADR-0027 已落地）**：本文仍是当时的只读评估快照，§1–§2 的缺口清单保留为决策依据。
> 路径 A 已按 **形态 F2（keep-alive 容器 + attached exec）**实现并**默认开启**：`Sandbox` seam 补上
> `exec_attached` / `copy_in` / `has_image`，`ExecSpec` 补上 `mounts` / `hardening` / `resources`，
> `Dockerfile` 增加 `AS sandbox` 阶段提供容器内 dsh。§0 第 1 条与 §2.5 的"不可行"结论**已被取代**；
> 仍然成立的限制（`network` 强制模式下的 `HTTPS_PROXY` 语义、`cap_drop=ALL` 与 dsh 自带 sandbox 的交互、
> 默认 1 GiB 的内存包络、`docker inspect` 可见凭据、harness 容器不计入 `QuotaGuardSandbox`）见
> [ADR-0027](./adr/0027-dsh-sandbox-runtime.md) 的风险表。

---

## 0. 结论摘要

1. **路径 A（dsh 及其自带工具在沙箱容器内运行）当前不可直接落地**（*已被 ADR-0027 取代*：seam 已补齐，
   容器化默认开启）：`Sandbox` seam 只提供**一次性、缓冲式**的
   `spawn` / `exec` / `destroy`，缺三样 dsh 运行契约必需的能力——**带命令的 spawn**（现硬编码 `sleep 3600`）、
   **双向 stdio（stdin attach）**、**工作区持久化（挂载/拷贝/常驻容器）**；此外沙箱镜像里没有 dsh 二进制。
   补齐后**有条件可行，且只在 `docker` / `kata` 两个后端上成立**；`cube` 因结构化拒绝 `network`/`labels`
   且无 attach 被直接排除，`browser` 不是通用执行后端，同样排除。
2. **路径 B（aria 工具族真正落到沙箱）当前是"半沙箱"**：`shell` / `jobs` 已走容器，但**每条命令一个新容器**，
   跨命令不留状态；`fs` / `skills` / `web` 都在宿主进程内，`terminal` 是宿主 PTY（网络治理开启时被直接禁用）。
   因此"工具在沙箱里跑"目前只对单条 shell 命令成立，不构成可用工作区。
3. **零改造现状的准确定性**：默认 `AGENT_HARNESS=dsh`（`crates/aria-agent-cloud/src/main.rs:282-287`）时，
   dsh 作为**宿主子进程**执行模型调用与它自己的工具，隔离级别是 dsh 的**权限档** `workspace-write`
   （逻辑约束，不是文件系统边界）；aria 的 `Sandbox` 对这条会话**完全空闲**
   （`docs/adr/0019-dsh-runtime.md:131-138`、`docs/architecture.md:84-88`）。
   所以现状**不能表述为"工具在沙箱中运行"**，只能表述为"dsh 在自己的权限档下运行，容器沙箱仅覆盖 native 循环的 shell/jobs"。
4. **推荐**：短期不改架构，把"dsh 会话无容器边界"写成显式风险并配 `danger-full-access` 禁用策略；
   中期做路径 B 的**会话级沙箱**（成本低、与既有 egress/quota 治理栈同构）；路径 A 只在需要"运行时级隔离"
   （多租户不可信代码）时启动，且第一步必须先落 **镜像 + attach + workspace** 三件套。

---

## 1. 沙箱运行时事实表

### 1.1 `Sandbox` seam 契约

| 项 | 事实 | 证据 |
| --- | --- | --- |
| 必备方法 | `spawn(&ExecSpec) -> SandboxHandle`、`exec(&handle, &[String]) -> ExecOutput`、`destroy(handle)` | `crates/aria-agent-sandbox/src/lib.rs:222-232` |
| 可选方法 | `capabilities` / `status` / `pause` / `resume` / `suspend` / `checkpoint` / `restore` | `lib.rs:239-292` |
| 缺省行为 | 未实现即 `SandboxError::Unsupported`（fail-closed，不假装成功）；`capabilities` 默认 `none()` | `lib.rs:239-276` |
| 句柄 | 仅 `{ id: String }`，无进程/流句柄 | `lib.rs:200-203` |
| 输出 | `ExecOutput { exit_code, stdout, stderr }`，**整段字符串**，无增量流 | `lib.rs:192-197` |

### 1.2 `ExecSpec` 字段与缺口

现有字段（`lib.rs:156-175`）：`image` / `command` / `workdir` / `env` / `timeout_ms`（默认 60_000，`lib.rs:184`）/ `network` / `labels`。

缺口（**路径 A 的核心阻塞**，全部【证】）：

| 缺失能力 | 影响 | 证据 |
| --- | --- | --- |
| `stdin` / 双向流 | dsh 的 JSON-RPC 长连接无处安放 | `exec` 只 `attach_stdout/stderr`，全仓无 `attach_stdin`：`lib.rs:686-691` |
| 流式回调 / 增量输出 | 调用方必须等命令退出才拿到任何字节 | `lib.rs:706-716` |
| `timeout_ms` 的实际执行 | 该字段**没有任何后端读取**（仅定义、默认值、测试断言、browser 字面量） | 全仓 4 处：`lib.rs:167`、`lib.rs:184`、`crates/aria-agent-browser/src/sandbox.rs:88`、`lib.rs:1169` |
| 容器命令 | `spawn` 硬编码 `cmd = ["sleep","3600"]`，无法把 dsh 作为容器 1 号进程 | `lib.rs:651`（`restore` 同 `lib.rs:869`） |
| 卷 / 绑定挂载 | 宿主工作区、`DSH_HOME`、skills 无法进容器 | `build_host_config` 只设 cpus/memory/swap/pids/runtime/network_mode：`lib.rs:78-89`；全仓无 `Mounts`/`binds`/`volumes` 调用 |
| 文件拷入/拷出 | 无 `copy_to_container` 类 API（全仓无匹配） | — |
| 镜像拉取 | 无 `pull_image`；镜像必须预先存在于 daemon | 全仓无匹配，`spawn` 直接 `create_container`：`lib.rs:643-665` |
| 运行用户 / 只读根 / cap 裁剪 | 容器以镜像默认用户运行（`alpine` 即 root）；隔离强度不可表达 | `Config` 无 `user`/`readonly_rootfs` 字段：`lib.rs:649-661` |

> 唯一"存在但无效"的字段是 `timeout_ms`：它是 seam 声明的契约缺口，不是调用方误用【证】。

### 1.3 Docker 后端实现语义（默认路径）

| 行为 | 事实 | 证据 |
| --- | --- | --- |
| 传输 | Docker **Engine API**（bollard），不依赖容器内 `docker` CLI | `lib.rs:541-556` |
| 资源限制 | `SANDBOX_CPUS` / `SANDBOX_MEMORY` / `SANDBOX_PIDS_LIMIT`，缺省 **1.0 CPU / 512 MiB / 256 pids**；解析失败回落缺省而非报错 | `lib.rs:59-65`、`lib.rs:73-90` |
| 网络 | 设置 `network_mode` 即替换默认 bridge；配合 ADR-0024 的 `internal` 网络 = 无默认路由 | `lib.rs:84-88` |
| 身份 | 容器**创建时**写入 `aria.tenant_id` / `aria.session_id` label，供 egress proxy 归因 | `lib.rs:655`、`lib.rs:897-906`、`egress_net.rs:24-33` |
| exec 语义 | `sh -c <shell_join(argv)>`，读完整个 raw-stream 再 `inspect_exec` 取退出码；流中断 = 错误 | `lib.rs:681-731` |
| 退出码丢失 | `inspect_exec` 失败时降级为 `-1`（不报错） | `lib.rs:717-721` |
| 工作目录 | `spec.workdir` 直接写进容器 `working_dir`；宿主路径语义被照搬进容器 | `lib.rs:654` |

### 1.4 生命周期语义

| 状态 | 语义 | 证据 |
| --- | --- | --- |
| `Creating` / `Running` / `Paused` / `Suspended` / `Destroyed` / `Failed` | 与任务状态正交；`accepts_exec` 仅 `Running` 为真 | `lifecycle.rs:24-73` |
| `pause` | Docker pause：内存保留、进程冻结 | `lib.rs:782-790` |
| `suspend` | Docker stop：**文件系统保留、进程消失** | `lib.rs:813-824` |
| `resume` | 从 paused 走 `unpause`；从 suspended 走 `start_container`，重新拉起 `sleep 3600` | `lib.rs:792-811` |
| `checkpoint` | `commit_container(pause=true)` → 镜像引用 `CheckpointRef` | `lib.rs:826-851` |
| `restore` | 用该镜像**新建**容器（不是原地回滚），重新走一遍 `spawn` 参数 | `lib.rs:853-889` |
| 能力声明 | docker 声明 pause/suspend/checkpoint/restore 全真 | `lib.rs:749-751`，断言见 `lib.rs:1967-1969` |

> **对 dsh 的直接含义**【推，前提为 §1.2 的 attach 缺口被补齐】：`suspend` 之后容器内进程全部消失，
> 只能靠容器可写层里的 `DSH_HOME` 重新 `exec` 一个 dsh 并用同一 `sessionId` 续接——
> 这与当前宿主上"池里留一个 runtime"的模型不同，需要新的"容器→runtime"恢复路径。

### 1.5 网络与配额治理

| 机制 | 事实 | 证据 |
| --- | --- | --- |
| 代理环境 | `proxy_env()` = `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` + `NO_PROXY=localhost,127.0.0.1,::1` | `egress_net.rs:40-51` |
| 网络拓扑 | internal bridge `aria-egress` + 代理容器 `aria-egress-proxy:8080`；沙箱只到代理 | `egress_net.rs:24-33`、`egress_net.rs:221-401` |
| 沙箱侧代理地址 | 用**容器名**寻址：`http://aria-egress-proxy:8080` | `egress_net.rs:104-106` |
| 身份归因 | 按源 IP + label 反查租户；无 label 一律不可归因（代理拒绝） | `egress_net.rs:164-202`、`egress_net.rs:447-480` |
| 启动即强制 | `internal_egress::bootstrap()` 失败 → 进程启动失败（不允许"看似强制"） | `crates/aria-agent-cloud/src/main.rs:392-406` |
| 装饰器 | `NetworkingSandbox` 只填补调用方留空的 `network`/`labels`/`env`，显式值优先 | `egress_net.rs:520-566` |
| 第二道装饰 | `AgentConfig` 侧 `governed_sandbox()` 也包一层，防止新调用方遗忘 | `crates/aria-agent-core/src/lib.rs:1290-1308` |
| 配置校验 | `egress_enforcement=network` 要求 `egress_network` 与 `egress_proxy_url` 均非空 | `crates/aria-agent-core/src/lib.rs:1176-1184` |
| 配额 | `QuotaGuardSandbox` 在 `spawn`/`restore` 前 `reserve`，拒绝则回滚 destroy | `crates/aria-agent-governance/src/sandbox.rs:16-70` |

### 1.6 镜像与二进制可得性

| 项 | 事实 | 证据 |
| --- | --- | --- |
| 沙箱默认镜像 | `alpine:latest`（musl） | `lib.rs:638-641` |
| 云镜像 | `debian:bookworm-slim`，非 root 用户 `aria`（uid 10001） | `Dockerfile:120`、`Dockerfile:163-166`、`Dockerfile:179` |
| dsh 安装 | `python3 -m venv /opt/dsh && pip install deepseek-harness-sdk`，`DSH_BIN=/opt/dsh/bin/dsh`、`DSH_HOME_ROOT=/app/.aria/dsh` | `Dockerfile:151-161` |
| 结论 | **沙箱容器不是云服务镜像**，容器内没有 dsh；且 dsh 是 glibc 原生二进制，不能直接塞进 alpine【推，前提：SDK 分发的是 glibc 构建，`Dockerfile` 选 debian 基础镜像即为此】 | 同上 |

### 1.7 当前"工具在哪个世界"（native 循环）

| 工具族 | 实际执行位置 | 证据 |
| --- | --- | --- |
| `shell` / `jobs` | 沙箱容器，**每条命令 spawn→exec→destroy** | `crates/aria-agent-core/src/seams/shell.rs:96-121`（`destroy` 结果被丢弃，`shell.rs:113`） |
| `shell` 的 workdir | `SandboxPolicy::root()`（`workspace_root` 为空则进程 cwd，云容器内即 `/app`），**宿主路径原样写进容器** | `crates/aria-agent-core/src/lib.rs:1311-1320`、`shell.rs:103`、`lib.rs:654` |
| `fs_*` | **宿主本地** `LocalFs`（`tokio::fs`），不经过 `Sandbox` | `crates/aria-agent-core/src/seams/fs.rs:35-51`、`fs.rs:79-85`、`crates/aria-agent-tools/src/lib.rs:83` |
| `skills` | 宿主目录 `shared_skill_catalog()`（`AGENT_RSI_SKILLS_DIR` → `$ARIA_COMPUTE_HOME/skills` → `$HOME/.ariacompute/skills`） | `crates/aria-agent-core/src/seams/skills.rs:54-98`、`tools/src/lib.rs:89` |
| `web_*` | 宿主进程内 `reqwest`（受 `EgressSeam` 策略约束，不受沙箱网络约束） | `crates/aria-agent-tools/src/web.rs:216-230`、`web.rs:266-269` |
| `term_*` | 宿主本地 PTY；**网络强制开启时整个 family 不注册** | `crates/aria-agent-tools/src/terminal.rs:176-215`、`tools/src/lib.rs:117-125` |
| 云端装配 | `ToolSet::local(policy, sandbox, context).with_egress(...).with_sandbox_transport(...)` | `crates/aria-agent-cloud/src/app.rs:352-355` |
| RSI（技能来源） | 现在**默认开启**（`AGENT_RSI_ENABLED=false` 才关闭），蒸馏结果写入 `shared_skill_catalog()`；真实 re-run 探针被设计成 **tool-free**，不触碰任何 sandbox | `docs/architecture.md:95`、`crates/aria-agent-cloud/src/main.rs:460`、`crates/aria-agent-cloud/src/rsi_rerun.rs:18-19`、`.env.example:143-144` |

> **关键不一致**【推，前提：容器内 `/app` 与宿主 `/app` 不是同一目录】：`fs_write` 写的文件在宿主 `/app`，
> 紧接着 `shell cat /app/...` 在容器 `/app` 里看不到——两套工具看到的是两个文件系统。
> 这不是新缺陷，但它决定了"路径 B 能否声称工具族进了沙箱"的答案：**不能**。

---

## 2. 路径 A：dsh 及其自带工具进容器

### 2.1 dsh 的运行契约（必须同时满足）

| # | 契约 | 证据 |
| --- | --- | --- |
| A1 | 长驻子进程 `dsh --profile sdk`，**双向 NDJSON JSON-RPC over stdio** | `crates/aria-agent-dsh/src/transport.rs:3-5`、`transport.rs:136-183` |
| A2 | 握手 `initialize` 30 s 上限，`session/prompt` 120 s 上限 | `crates/aria-agent-dsh/src/runtime.rs:107-108`、`runtime.rs:318-330`、`runtime.rs:338-349` |
| A3 | 单帧 ≤ 8 MiB，超限视为协议错误 | `transport.rs:24-25`、`transport.rs:175-180` |
| A4 | 每 session 独立 `DSH_HOME` 与工作区（宿主目录） | `runtime.rs:97-113`、`runtime.rs:189-212` |
| A5 | skills 通过 `<cwd>/.aria/skills` 符号链接共享 | `runtime.rs:215-253` |
| A6 | 出网：`DEEPSEEK_BASE_URL`（`/anthropic/v1`）+ `DEEPSEEK_API_KEY`，默认指向宿主 loopback 的 thinking filter | `runtime.rs:132-141`、`thinking_proxy.rs:293-295`、`crates/aria-agent-cloud/src/main.rs:305-336` |
| A7 | 审批/提问经 stdio 回传 aria 侧 bridge，缺 provider 一律 deny | `docs/adr/0019-dsh-runtime.md:105-112`、`crates/aria-agent-dsh/src/approval.rs` |

### 2.2 逐条对照 → 阻塞点

| 契约 | 沙箱现状 | 判定 |
| --- | --- | --- |
| A1（双向 stdio） | `exec` 无 `attach_stdin`、无增量输出，必须读到 EOF | **阻塞**（seam 缺口）【证】`lib.rs:686-691`、`lib.rs:706-716` |
| A1（进程如何起） | `spawn` 容器 cmd 固定 `sleep 3600`；只能用 `exec` 起临时进程 | **阻塞**（缺"带命令的 spawn"）【证】`lib.rs:651` |
| A2（超时） | `timeout_ms` 无人读取；请求超时只覆盖 RPC，不覆盖僵死流 | **阻塞**（需看门狗）【证】+【推】 |
| A3（帧上限） | 传输层有 8 MiB 限制，但容器 raw-stream 逐帧读取，语义不变 | 可满足【推】 |
| A4（`DSH_HOME`/workspace） | 无挂载、无拷贝，只能落容器可写层；跨轮续跑依赖容器常驻或 checkpoint | **阻塞**（需持久化策略）【证】 |
| A5（skills 链接） | `symlink` 在宿主侧执行（`std::fs`），容器内路径不存在 | **阻塞**（需拷入或挂载）【证】 |
| A6（出网到网关） | `internal` 网络下只有代理可达；需把 `proxy_env()` 注入容器 env | **有条件**：dsh 必须尊重 `HTTPS_PROXY`【测】 |
| A6（thinking filter） | 过滤器监听宿主 `127.0.0.1:0`，容器内 `127.0.0.1` 是自身 | **阻塞**（需改拓扑）【证】 |
| A7（审批回传） | RPC 客户端仍在 aria 进程内，stdout 从容器取即可 | 可满足【推】 |
| 二进制 | 沙箱镜像内无 dsh；默认 alpine/musl | **阻塞**（需专用镜像 + 预置）【证】 |

### 2.3 四种落地形态（按改动量从小到大）

| 形态 | 做法 | 优点 | 硬伤 | 结论 |
| --- | --- | --- | --- | --- |
| F1 每轮短生命周期 | 每轮 `exec` 一个 `dsh --profile sdk` 跑单次 prompt 后退出 | 不需要常驻句柄 | 握手 30 s + 会话历史需每次载入；`initialize` 一次也就意味着 dsh 必须支持"一次性 prompt"模式，与现池化模型冲突 | 不推荐【推】 |
| F2 `exec` + `attach_stdin` | `create_exec(attach_stdin/stdout/stderr)` 起 dsh，保持流 | 复用现有容器与网络/配额；最小改动面 | 需要新的 `Sandbox` 方法返回**双向流**而非 `ExecOutput`；容器重建即断流；`exec` 进程随容器 `stop` 一起消失 | **首选**（需扩 seam）【推】 |
| F3 容器 1 号进程 + `attach_container` | 容器 cmd 直接是 `dsh --profile sdk`，attach 主进程 stdio | 语义最干净（容器生命周期 = runtime 生命周期） | 需要"带命令的 spawn"，并绕过现在的 `sleep 3600` 假设（`status`/日志/退出码语义都要改） | 备选（改动更大）【推】 |
| F4 checkpoint + restore 续会话 | 用 `commit` 冻结工作区，恢复时新建容器再起 dsh | 与治理/计费（ADR-0022）对齐 | `commit` 是**文件系统**快照，不含进程；恢复后仍需重连；kata 下是否可用未验证 | 与 F2/F3 组合使用【测】 |

### 2.4 路径 A 所需新增能力（只列能力，不写实现）

按优先级：

1. **P0 双向 exec**：`Sandbox` 增加"起进程并返回可读写的字节流 + 可关闭句柄"的能力（含 stdin、增量 read、独立 stderr、显式 kill）。
2. **P0 超时/看门狗**：让 `ExecSpec.timeout_ms` 真正生效（或由新能力自带 deadline），否则 8 MiB 帧 + 长连接会无限挂起。
3. **P0 专用沙箱镜像**：内置 dsh 二进制与其运行时依赖（glibc 基座），并由部署保证镜像已存在于 daemon（无 pull）。
4. **P1 工作区持久化**：卷/绑定挂载，或"拷入/拷出"API；至少要能把 `DSH_HOME`、workspace、`.aria/skills` 放进去。
5. **P1 出网改造**：thinking filter 放到容器可达地址（同网络 sidecar / 宿主网关 IP），或显式允许 `DSH_THINKING_FILTER=disable` 的降级路径。
6. **P2 安全加固**：非 root、只读根、cap 裁剪、`pids`/内存重新标定（`workspace-write` 只是逻辑档，容器内 root 时 `danger-full-access` 等于全容器）。

### 2.5 结论

- **当前代码：不可行**（*历史结论*）。不是配置问题，是 seam 缺能力（stdin/命令/spawn 命令/持久化/超时/镜像六项）。
  这六项已在 ADR-0027 中补齐：`exec_attached`、`copy_in`、`has_image`、`ExecSpec.mounts`、
  keep-alive 命令与 `SandboxShell` 自持超时。
- **补齐 P0 后：在 `docker` 与 `kata` 上可行**，代价是每个活跃会话一个容器（`DSH_POOL_MAX` 默认 8 → 每租户最多 8 个容器），
  且必须在容器生命周期上重新实现池化（`suspend` 会杀进程、`destroy` 会丢工作区）。
- **`cube` / `browser`：不可行**（见 §4）。

---

## 3. 路径 B：aria 工具族进沙箱

### 3.1 首要阻塞：`SandboxShell` 每命令一容器

`SandboxShell::exec` 的固定序列是 `spawn → exec → destroy`（`crates/aria-agent-core/src/seams/shell.rs:107-113`），
`destroy` 的错误还被丢弃（`shell.rs:113`）。后果【证】：

- 命令之间**没有任何文件系统延续**：`shell ["sh","-c","echo hi > f"]` 之后 `shell ["cat","f"]` 必然失败；
- 每次调用都重新付容器创建成本（cold start）【测：具体延迟】；
- 无法承载"编译—测试—修复"这类多步工作流，也无法与 `fs_*` 共享同一工作区。

### 3.2 各 family 的真实归属与改造量

| family | 现状 | 要进沙箱需要 | 改造量 |
| --- | --- | --- | --- |
| `shell` | 容器（一次性） | 会话级沙箱句柄 | 中（见 3.3） |
| `jobs` | 复用同一 `ShellSeam`（`tools/src/lib.rs:130-133`） | 跟随 shell 一起改 | 小 |
| `fs` | 宿主 `LocalFs`（`seams/fs.rs:35-51`） | `FsSeam` 走容器（exec/base64）或改挂载 + 宿主直读 | 大（失败模式、编码、性能都变） |
| `skills` | 宿主目录（`seams/skills.rs:54-98`） | 目录注入容器 | 小（随 A5） |
| `web` | 宿主 `reqwest`（`web.rs:216-230`） | 要么留在宿主（受 `EgressSeam`），要么容器内 `curl` | 不建议改（会丢策略投影） |
| `term_*` | 宿主 PTY，网络强制下禁用（`tools/src/lib.rs:117-125`） | 容器内 PTY（exec tty + 流） | 大（依赖 P0 双向流） |

### 3.3 需要一个"会话级沙箱"抽象

现在 `LoopContext` 只拿 `Arc<dyn Sandbox>`（无句柄），而 `ShellSeam` 的签名是 `exec(argv, cwd)`
（`seams/shell.rs:38-42`），天然只够表达"一次性命令"。要真正可用的工作区，需要二者之一：

- `Sandbox` 增加 `session()`（返回持有容器句柄的会话对象，带 `exec`/`read`/`write`/`close`）；或
- 新增 `SandboxSession` seam，由 `Agent`/`LoopContext` 在 turn 开始时创建、turn 结束按策略释放。

同时要处理的既有语义：

- `SandboxPolicy::root()` 是宿主路径（`core/lib.rs:1311-1320`），容器里需要同名目录；
- `fs` 的写后读、`shell` 的 `cwd` 参数、`jobs` 的后台生命周期都要绑到同一个句柄；
- 网络治理下要在会话创建时一次性注入 `network`/`labels`/`proxy_env`（`tools/src/lib.rs:139-149` 已有雏形）。

### 3.4 与 dsh 同时启用的边界

`harness = "dsh"` 时 dsh 自己执行工具，aria 的 `Sandbox` / pipeline / handlers 全部空闲
（`docs/adr/0019-dsh-runtime.md:131-138`）。因此路径 B 只在 `harness = "native"` 或"dsh 只做模型、工具回传 aria"
（当前**不存在**这种模式）时有意义；**两条管道同时跑会双执行副作用**，ADR 明确不支持。

### 3.5 结论

- **现状：部分可行**——`shell`/`jobs` 确实在容器里，但是"一次性、无状态、无双工"的；
- **要做到"工具族真正进沙箱"：需改 4 个 seam + 1 个会话抽象**，工作量大于路径 A 的 P0；
- 收益是**立刻可用**（不依赖 dsh 的传输模型），且与既有 egress/quota/checkpoint 治理天然同构；
- 若目标只是"让不可信命令不外溢"，路径 B 的性价比高于路径 A。

---

## 4. 四 provider 可行性矩阵

| 能力 | docker | kata | cube | browser |
| --- | --- | --- | --- | --- |
| 一次性 `exec` | ✅ Engine API，`sh -c`，缓冲式（`lib.rs:673-732`） | ✅ 同路径，`runtime=kata`（`lib.rs:908-971`） | ✅ CLI `cube exec <id> sh -c`（`lib.rs:424-447`） | ⚠️ 仅内部使用：`node /runner/runner.mjs` 交给 inner（`aria-agent-browser/src/sandbox.rs:118-123`） |
| 双向 stdio / attach | ❌（seam 未暴露，bollard 支持） | ❌（同） | ❌（`Command::output()` 一次性收集，`lib.rs:411-421`） | ❌ |
| 自定义 network | ✅ `network_mode`（`lib.rs:84-88`） | ✅（同字段） | ❌ **结构化拒绝** `spec.network`（`lib.rs:394-400`） | 继承 inner |
| 租户 labels | ✅（`lib.rs:897-906`） | ✅ | ❌ **结构化拒绝** `spec.labels`（`lib.rs:403-407`） | 继承 inner |
| `pause`/`resume` | ✅（`lib.rs:782-811`） | ✅ 委派（`lib.rs:952-957`） | ❌ 默认 `Unsupported`（`lib.rs:1010-1024`） | ❌ 默认 |
| `suspend` | ✅ `stop`（`lib.rs:813-824`） | ✅ 委派 | ❌ | ❌ |
| `checkpoint`/`restore` | ✅ `commit` + 新建（`lib.rs:826-889`） | ⚠️ 委派到 `commit_container`（`lib.rs:961-970`），**kata 下是否可用未验证**（ADR-0022:26-27 只断言委派，无测试）【测】 | ❌ | ❌ |
| 工作区挂载 | ❌ | ❌ | ❌ | ❌ |
| 镜像可得性 | 需预置（本仓无拉取） | 需预置且 **kata 兼容镜像** | 由 CLI 自管（本仓不管） | 六种浏览器镜像由 catalog 定义（`requirements.md:95-110`，本仓无拉取） |
| 非 root / 只读根 | ❌ 不可表达 | ❌ | ❌ | ❌ |
| 结论 | **路径 A/B 唯一可行后端** | **有条件可行**（需实测 commit、镜像与冷启动成本） | **排除**：与网络治理结构性冲突、无生命周期、无 attach | **排除**：不是通用执行后端，`from_provider` 直接 `NotConfigured`（`lib.rs:1055-1066`） |

补充：

- kata 的隔离强度（VM 级）正是"多租户跑不可信工具"想要的东西，但**同一条 bollard 路径**意味着 §1.2 的缺口一个都不少【证】`lib.rs:908-971`。
- cube 的容器 id 靠 stdout 解析（`lib.rs:411-421`），本身就没有可用的进程句柄，接入 dsh 需要重写后端。

---

## 5. 四条主轴分析

### 5.1 安全隔离

**事实**

- dsh 会话（默认配置）在宿主进程内执行工具，隔离只到 `read-only | workspace-write | danger-full-access`
  三档权限（`crates/aria-agent-dsh/src/runtime.rs:30-34`、`crates/aria-agent-cloud/src/main.rs:221-228`）；
  这是 dsh 自己的策略判断，不是内核/容器边界【证】。
- 云进程以非 root `aria` 运行（`Dockerfile:163-179`），所以 dsh 的工具调用至少不是 root——但能读该用户的全部可读文件
  （含 `DSH_HOME` 之外的 `/app`、挂载的 docker socket 视权限而定）【推】。
- 沙箱容器以镜像默认用户运行，`alpine` 下即 root，且无 `readonly_rootfs`/cap 裁剪（`lib.rs:649-661`）【证】。
- 两类沙箱不可混用：harness=dsh 时 aria pipeline 空闲，harness=native 时 dsh 不参与（ADR-0019 §6）【证】。
- "校验期不得执行工具"已经是仓库自己的判断：RSI 的真实 re-run 被刻意做成 tool-free，理由写在
  `crates/aria-agent-cloud/src/rsi_rerun.rs:18-19`（"a re-run that executed shell or file tools would turn a
  validation into a second production incident"）【证】。这与本报告"工具执行必须有边界"的结论同向。

**影响**：现状的"隔离强度"取决于你是否信任模型产出的命令。多租户互不可信时，`workspace-write` 不足以构成边界；
而容器路径即使补齐 attach，也只是"root-in-container"，需要额外加固才算真隔离。

**结论**：**现状不满足"工具在容器边界内"**；路径 A 补齐后仍建议叠加非 root + 只读根 + cap 裁剪，否则只是把风险从宿主搬进容器。

### 5.2 协议与长连接

**事实**

- dsh 必须长驻：`initialize` 30 s、`session/prompt` 120 s、单帧 8 MiB、stderr 只采样 2 KiB（`transport.rs:24-28`、`runtime.rs:107-108`）。
- `Sandbox::exec` 是"起进程 → 读完 → 返回字符串"，没有 stdin、没有增量读、没有 kill 句柄（`lib.rs:673-732`、`lib.rs:192-197`）。
- `timeout_ms` 不生效；`request_timeout` 只覆盖一次 JSON-RPC 调用，不覆盖"流还开着但对端已僵死"【证】+【推】。
- 断线语义在宿主侧已写好：runtime 关闭且无 `turn/end` → 以 `CoreError::Model` 失败该 turn（`crates/aria-agent-dsh/src/lib.rs:250-261`）。

**影响**：容器化后，"断线"会多一层来源（容器被回收、`suspend`、daemon 重启），需要把"容器死亡"映射成同一套失败语义，
否则会出现"turn 一直挂到 120 s 超时"的体验。

**结论**：**协议层可复用**（RPC 与事件映射与传输位置无关），**传输层必须新增能力**（双向流 + deadline + 断开检测）。这是路径 A 的第一优先级。

### 5.3 出网与凭据

**事实**

- 宿主侧：`StdioTransport` 从宿主进程转发 `HTTP(S)_PROXY`/`ALL_PROXY`，并强制 `NO_PROXY` 含 loopback（`transport.rs:76-88`、`transport.rs:189-207`）。
- 容器侧：env 由 `ExecSpec.env` 显式注入；`proxy_env()` 与 `proxy_url_for_sandbox()` 已按"从容器内可达"设计（`egress_net.rs:40-51`、`egress_net.rs:104-106`）。
- 凭据：`GATEWAY_API_KEY` 经 `with_gateway_env()` 变成子进程的 `DEEPSEEK_API_KEY`（`runtime.rs:132-141`）；
  放进容器 env 意味着它能被 `docker inspect` 看到（容器内同用户可读）【推】。
- thinking filter 监听宿主 `127.0.0.1:0`（`thinking_proxy.rs:293-295`），容器内 loopback 是自身 → **不可达**【证】；
  关掉过滤器则 dsh 直连网关，第二轮回放 `redacted_thinking` 会 `UNSUPPORTED_CONTENT`（ADR-0019 §7）【证】。
- egress proxy 是唯一出路，按 label 归因租户（`egress_net.rs:447-480`）；`networking` 模式下无默认路由（`lib.rs:84-88`）。

**影响**：容器化后有三件事必须同时成立——(a) dsh 尊重 `HTTPS_PROXY`；(b) thinking filter 换到容器可达地址；
(c) 凭据不进镜像、尽量不落容器 env（否则隔离收益被凭据暴露抵消）。

**结论**：**有条件可行**，但 thinking filter 是**具体拓扑改动**而不是开关；凭据面需要新的注入方式（文件/一次性 token/代理侧替换）。

### 5.4 状态与生命周期

**事实**

- dsh 状态在 `DSH_HOME`（`runtime.rs:97-113`）；云端为每 principal 一个池、每 session 一个 runtime（`main.rs:337-380`）；
  池满直接报错（`runtime.rs:432-437`）。
- 路径推导存在**二次嵌套**：`DshConfig::for_session(root/tenant, session)` 得到 `<root>/<tenant>/.aria/dsh/<leaf>`，
  而 `DshPool::acquire` 用 `cfg.home.parent()` 再推一次（`runtime.rs:439-447`），
  实际 home 落在 `<DSH_HOME_ROOT>/<tenant>/.aria/dsh/.aria/dsh/<leaf>`【推：按代码字面推导，未在运行环境核对】。
- skills：三处已统一到 `agent_core::seams::shared_skill_catalog()`——RSI sink（`main.rs:457-460`）、
  skills 工具族（`tools/src/lib.rs:89`）、dsh 的 `skill_catalog()`（`runtime.rs:176-187`，可被
  `with_skills_catalog` 覆盖，`runtime.rs:169-174`）；dsh 侧再把它 symlink 到 `<cwd>/.aria/skills`
  （`runtime.rs:215-237`）。**对齐已由构造保证**【证】；剩余风险是**容器可见性**——该目录是宿主路径，
  容器化后必须挂载/拷入，否则 dsh 一个技能也看不到【推】。
  由于 RSI 现在**默认开启**（`docs/architecture.md:95`），容器里看不到 catalog 已不是"某个可选功能失效"，
  而是默认路径下技能蒸馏整体落空【推】。
- 另一处细节：`DshPool::acquire` 重建 per-session 配置时只覆盖 `home`/`cwd`（`runtime.rs:445-447`），
  而 `for_session` 把 `skills` 置为 `None`（`runtime.rs:111`）⇒ 部署用 `with_skills_catalog()` 钉住的目录
  在进池后会被丢弃，回落到进程环境解析【证】。
- 沙箱侧：`suspend` 保留文件系统、杀进程；`resume` 重启 `sleep 3600`（`lib.rs:792-824`）；
  `checkpoint` 是镜像快照（`lib.rs:826-851`）；`restore` 新建容器（`lib.rs:853-889`）。
- 配额：`spawn`/`restore` 都要过 metering（`governance/sandbox.rs:66-70`）。

**影响**：容器化后"会话 = 容器"会把 `DSH_POOL_MAX`（默认 8）从"8 个进程"变成"8 个容器/租户"，
计费与回收策略必须跟着改；跨轮续跑只能靠"容器常驻"或"checkpoint + 重新握手"。

**结论**：**可行但成本最高**：状态必须容器化，且要新增"会话级沙箱"与"空闲回收 = suspend/checkpoint"的策略。

---

## 6. 取舍与建议

### 6.1 推荐路线

| 阶段 | 动作 | 理由 |
| --- | --- | --- |
| 立即（零代码） | 把"dsh 会话无容器边界"写入部署风险清单；生产禁用 `danger-full-access`；对多租户场景显式要求 `AGENT_HARNESS=native` 或专租户部署 | 现状语义不能被误读为"已沙箱化"（ADR-0019 §6 已写死，需要运维层承接） |
| 短期 | 路径 B 的**会话级沙箱**（`shell`/`jobs` 共用一个容器，`fs` 至少与沙箱共享根） | 改动集中在 core seam，不触碰 dsh；立刻消除"fs 写宿主、shell 读容器"的割裂 |
| 中期（可选） | 路径 A 的 P0：双向 exec + 命令化 spawn + 专用镜像 + 超时看门狗 | 只有"运行不可信代码"这一诉求才值得付这个代价 |
| 中期（配套） | thinking filter 拓扑改造、凭据注入改造、skills 目录注入容器 | 三者是路径 A 的前置条件，不是可选项 |

### 6.2 零改造现状（必须写清）

- 默认 `AGENT_HARNESS=dsh`（`main.rs:282-287`）：**工具在宿主上跑**，隔离 = dsh 权限档。
- `AGENT_HARNESS=native`：**shell/jobs 在容器里跑**（一次性、无状态），`fs`/`skills`/`web` 仍在该宿主的进程内。
- 两种模式都**不能**声称"agent 的工具在沙箱容器中执行"。这是本报告最重要的一句话。

### 6.3 风险清单

| 风险 | 触发条件 | 后果 | 缓解 |
| --- | --- | --- | --- |
| dsh 会话越权读写宿主 | 默认配置 + 不可信输入 | 读到部署用户可读的所有内容 | 专租户部署 / `read-only` 档 / 路径 B |
| 容器内 root | 任何走 `sandbox` 的命令 | 容器逃逸面放大 | 非 root 镜像 + cap 裁剪（当前不可表达） |
| 凭据暴露 | 容器化 dsh 时注入 `DEEPSEEK_API_KEY` | `docker inspect` 可见 | 代理侧替换 / 短期 token / 文件挂载 |
| 无超时 | 任何长命令/僵死流 | 请求挂到 120 s 或永不返回 | 落地 `timeout_ms` |
| 镜像缺失 | 新环境首次运行 | `create_container` 失败 | 部署预置 + 启动自检 |
| 容器数量爆炸 | 每会话一容器 + 池上限 8 | 资源告警 | 会话 TTL + suspend/checkpoint 策略 |
| 容器内 skills 目录缺失 | 容器化且未注入 `shared_skill_catalog()` 目录 | dsh 一个技能也看不到 | 挂载/拷入 catalog + 启动自检 |

---

## 7. 待实测清单

| # | 待验证 | 方法 | 预期 / 失败含义 |
| --- | --- | --- | --- |
| M1 | 容器内 dsh 的内存/CPU 峰值（默认 512 MiB / 1.0 CPU 是否够） | 专用镜像跑一次真实 turn，`docker stats` 采样 | 超限 → 需重新标定 `SANDBOX_*`，否则 OOM 杀进程 |
| M2 | bollard `create_exec(attach_stdin)` 的读写并发与 EOF 语义 | 最小 demo：echo 服务 + 双工收发 | 不通 → 只能走 F3（容器 1 号进程 attach） |
| M3 | dsh 是否尊重 `HTTPS_PROXY`（含 CONNECT） | `internal` 网络 + 仅代理可达，观察代理日志 | 不尊重 → 需 sidecar/透明代理 |
| M4 | thinking filter 放到同网络 sidecar 后是否仍能剥离 JSON/SSE 两侧 | 起 sidecar，跑两轮（触发 redacted_thinking 回放） | 失败 → 只能 `disable` + 接受第二轮 400，或换 provider |
| M5 | kata 下 `commit_container` 是否可用、镜像能否被 `create_container` 复用 | kata runtime 上 `checkpoint` → `restore` | 不可用 → kata 只能用"容器常驻"，不能做工作区快照 |
| M6 | 每租户 8 容器的实际资源占用与冷启动 | 并发 8 会话压测 | 超预算 → 降 `DSH_POOL_MAX` 或改共享容器 |
| M7 | `suspend` 后重新 `exec` dsh 能否用同一 `sessionId` 续上历史 | 停容器 → `resume` → `session/prompt` | 不能 → 必须实现 checkpoint 或禁止 suspend |
| M8 | 容器化后 `shared_skill_catalog()` 目录能否被 dsh 看到 | 容器内 `ls <cwd>/.aria/skills`（symlink 目标） | 看不到 → 路径 A 必须先落挂载/拷入，否则技能全丢 |
| M9 | 沙箱内工作目录（policy root）在镜像里是否已存在 | 镜像内 `ls /app` | 不存在 → 每次首次命令前需 `mkdir -p` |

---

## 附录 A：代码位置索引

| 主题 | 位置 |
| --- | --- |
| `Sandbox` trait / 可选生命周期 | `crates/aria-agent-sandbox/src/lib.rs:222-292` |
| `ExecSpec` | `crates/aria-agent-sandbox/src/lib.rs:156-189` |
| 资源限制与 `HostConfig` | `crates/aria-agent-sandbox/src/lib.rs:46-90` |
| Docker `spawn` / `exec` | `crates/aria-agent-sandbox/src/lib.rs:636-732` |
| Docker `status`/`pause`/`resume`/`suspend` | `crates/aria-agent-sandbox/src/lib.rs:753-824` |
| `checkpoint` / `restore` | `crates/aria-agent-sandbox/src/lib.rs:826-889` |
| Kata 委派 | `crates/aria-agent-sandbox/src/lib.rs:908-971` |
| Cube 拒绝 network/labels 与 CLI 语义 | `crates/aria-agent-sandbox/src/lib.rs:369-447`、`:1010-1024` |
| `from_provider`（browser 例外） | `crates/aria-agent-sandbox/src/lib.rs:1055-1066` |
| 生命周期状态机 | `crates/aria-agent-sandbox/src/lifecycle.rs:24-73` |
| egress 拓扑 / `proxy_env` / `NetworkingSandbox` | `crates/aria-agent-sandbox/src/egress_net.rs:24-51`、`:164-202`、`:520-623` |
| `SandboxShell`（每命令一容器） | `crates/aria-agent-core/src/seams/shell.rs:96-121` |
| `LocalFs` | `crates/aria-agent-core/src/seams/fs.rs:35-85` |
| `shared_skill_catalog` | `crates/aria-agent-core/src/seams/skills.rs:54-98` |
| `build_policy`（root 来源） | `crates/aria-agent-core/src/lib.rs:1311-1320` |
| `governed_sandbox` | `crates/aria-agent-core/src/lib.rs:1290-1308` |
| `ToolSet::local` / `with_sandbox_transport` | `crates/aria-agent-tools/src/lib.rs:74-149` |
| `QuotaGuardSandbox` | `crates/aria-agent-governance/src/sandbox.rs:16-70` |
| dsh 传输（NDJSON / 白名单 / 代理） | `crates/aria-agent-dsh/src/transport.rs:3-5`、`:55-133`、`:136-207` |
| dsh 配置与路径推导 | `crates/aria-agent-dsh/src/runtime.rs:97-113`、`:176-212` |
| dsh 池与二次路径推导 | `crates/aria-agent-dsh/src/runtime.rs:385-479` |
| dsh 事件与失败语义 | `crates/aria-agent-dsh/src/lib.rs:130-264` |
| thinking filter | `crates/aria-agent-dsh/src/thinking_proxy.rs:291-340` |
| 云端 harness 装配 | `crates/aria-agent-cloud/src/main.rs:282-380` |
| 云端 sandbox 装配与 provider 分派 | `crates/aria-agent-cloud/src/app.rs:219-238`（`governed`）、`:297-344`（provider 分派）、`:352-355`（`ToolSet` 装配） |
| egress bootstrap（失败即拒绝启动） | `crates/aria-agent-cloud/src/main.rs:392-406` |
| RSI 的 tool-free re-run（不触碰 sandbox） | `crates/aria-agent-cloud/src/rsi_rerun.rs:18-19`、`.env.example:143-144` |
| 镜像与 dsh 安装 | `Dockerfile:120`、`:151-179` |
| 工具归属边界（权威表述） | `docs/adr/0019-dsh-runtime.md:131-138`、`docs/architecture.md:84-88` |

## 附录 B：术语

| 术语 | 含义 |
| --- | --- |
| `Sandbox` seam | 工具执行的唯一抽象；provider 可换，调用方不感知后端 |
| 一次性 exec | `spawn` 一个容器 → 跑一条 `sh -c` → `destroy`；无 stdin、无增量输出 |
| `DSH_HOME` | dsh 的会话状态目录；必须每 session 独立 |
| attach | 把宿主进程的双向字节流接到容器内进程的 stdio |
| `workspace-write` | dsh 的权限档（逻辑约束），不是内核沙箱 |
| checkpoint | 用 `docker commit` 生成文件系统快照；不含进程 |
