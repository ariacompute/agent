# agent

基于 OpenAI [`codex`](https://github.com/openai/codex) harness 构建的分层 agent
平台，向上提供 (a) **Rust + Postgres 云端 API** 与 (b) **Swift / Kotlin 原生
SDK**（通过 UniFFI 边界）。

## 模块

| 模块 | 说明 |
|------|------|
| **codex submodule** | `openai/codex`，位于 `codex/`（harness、sandboxing、memories）。 |
| **agent-memo** | 统一的**上下文记忆（memo）**。本地/嵌入式存储，**不使用 Postgres**。 |
| **agent-sandbox** | 可插拔 `Sandbox`：Docker（默认）/ Kata / Cube。 |
| **agent-core** | 统一的 agent 运行时：`recall → 模型 → memorize`，工具在 sandbox 中执行。 |
| **agent-sdk** | UniFFI `cdylib`（`libagent_sdk`）：`SdkAgent` / `SdkSession` / `create_agent`。 |
| **agent-cloud** | axum 服务，Postgres 仅存元数据，调用 OpenAI Agents API。`/v1/agents`、`/v1/runs`、SSE `/v1/runs/stream`。 |
| **bindings** | `bindings/swift`（SwiftPM）与 `bindings/kotlin`（Android）。 |

> **存储边界：** memo 是**唯一的**上下文存储，从不使用 Postgres；Postgres
> 仅由 `agent-cloud` 用于存储 `agents`/`runs` 元数据。

## 快速开始

```bash
git submodule update --init --depth 1 codex   # 1. codex 子模块
cargo build --workspace                        # 2. 构建
cargo test --workspace                         # 3. 测试
just ffi                                       # 4. 生成 Swift/Kotlin 绑定
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/agent
export OPENAI_API_KEY=sk-...
cargo run -p agent-cloud                       # 5. 运行云端服务
```

## 云端 API 示例

```bash
curl -X POST localhost:3000/v1/agents -d '{"name":"my-agent"}'
curl -X POST localhost:3000/v1/runs   -d '{"agent_id":"<id>","session":"s1","input":"hello"}'
```

## 原生 SDK

见 `bindings/swift/README.md` 与 `bindings/kotlin/agent-sdk/README.md`。

## 目录结构

```
crates/      agent-memo, agent-sandbox, agent-core, agent-sdk, agent-cloud, agent-ffigen
bindings/    swift, kotlin
codex/       openai/codex 子模块
docs/        architecture.md + adr/
migrations/  Postgres 元数据 schema
tests/       跨 crate 集成测试
```

## 许可证

MIT。
