**中文** · [English](README.en.md)

# abycore

可嵌入的 DeepSeek Messages 异步 Rust agent SDK。

abycore 把 agent 循环完全放进进程内：宿主自带 Tokio 运行时，按需注册工具，
通过 hooks 决定授权与上下文策略，自行选择持久化内容。在 client 或 agent
操作被 await 之前不会发出任何请求。

```rust,no_run
use abycore::{Agent, ClientConfig, DeepSeekClient, LocalTools, ModelOptions, RunOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = DeepSeekClient::new(ClientConfig::new(std::env::var("DEEPSEEK_API_KEY")?))?;
    let tools = LocalTools::new(".")?;
    let mut agent = Agent::new(
        client,
        "你是用户终端里的编码 agent。",
        ModelOptions::default(),
    )?;
    tools.register(&mut agent)?;
    let outcome = agent
        .run("列出这里的文件，然后总结这个项目。", RunOptions::default(), |_| async {
            Ok(())
        })
        .await?;
    println!("{}", outcome.response.output_text());
    Ok(())
}
```

## 能力

- **Agent 循环** — 基于流式 Messages 响应的多步骤回合；持久收件箱（回合进行中
  到达的提示词会排队跟在其后）、每轮预算（`max_requests`、`max_tool_calls`）、
  回合/工具截止时间，以及通过 `CancellationToken` 的协作式取消。
- **本地工具包** — `LocalTools` 原子注册 `read`、`write`、`edit`、`bash`、
  `glob`、`grep` 六个工具，也可单独注册：
  - `read`/`write`/`edit` 基于 cap-std 目录能力运行：版本守卫（拒绝改动已过期
    文件）、staging 目录原子发布、字节/行数上限。
  - `bash` 生成沙箱化子进程（Linux 上用 Landlock + seccomp），支持增量读取
    输出的后台任务，溢出输出自动落盘。
  - `glob` 与 `grep` 是 harness 搜索工具的进程内 Rust 移植，构建于 ripgrep
    家族 crate（`ignore`、`grep-regex`、`grep-searcher`）之上：无外部二进制、
    遵循 .gitignore、对二进制文件安全。
  - 每次工具调用都要经过 `AgentHooks::authorize`；三档权限
    （`read-only`、`workspace-write`、`danger-full-access`）约束变更操作。
- **子代理** — 一个 `Subagents` 负责人 spawn 或 fork 子代理
  （`subagent`、`subagent_fork`），父代理通过 `list_agents`、`send_message`、
  `interrupt_agent` 协调。子代理继承父级的指令基线，使用自己的预算。
- **会话** — 共享存储里每个会话一份 fsync 的 JSONL 日志，按工作区分目录；
  `SessionWriter` 追加 checkpoint，agent 可从快照恢复，未完成回合安全重放
  （挂起的工具调用在下一次请求前被结算为未核实错误）。
- **上下文压缩** — 宿主在每个模型请求边界回答 `AgentHooks::view_request`；
  SDK 压缩或修剪请求视图而不改写持久化记录，提供商确认的溢出会"硬压缩"
  （只留当前回合）并重试同一回合。
- **目标** — 每会话一个持久完成目标（`get_goal`/`create_goal`/`update_goal`），
  带版本号校验的完成协议；轮次由宿主驱动。
- **技能** — `SkillCatalog::discover` 读用户写的 `SKILL.md`（工作区的
  `.agents/skills/`，可选 frontmatter 写名字、描述、参数提示；正文上限 64 KiB，
  超出按字符边界截断并留标记），`expand` 把
  `/名字 [参数]` 渲染成注入正文的用户消息，`index_block` 给出可放进
  `AgentHooks::request_context` 的技能索引，`SkillTool` 让模型按需自己读正文。
  SDK 只读文本，不执行任何东西。
- **结构化事件** — `AgentEvent`/`StreamEvent` 把模型视图、计划视图、工具结果
  与用量记录分开，任何 UI 都能直接消费。

## 示例程序

`examples/` 提供可运行的示例：`basic.rs`（单次完成）、`todo.rs`（计划驱动的
回合）、`tools.rs`（工具包）、`local_tools.rs`（沙箱文件工具）、`session.rs`
（快照存储）、`cancel.rs`（取消）、`subagent.rs`（委托）、`web_search.rs`
（辅助搜索）以及 `cli.rs`（小型交互式终端 agent）。

环境变量里放好 key 后即可运行：

```sh
DEEPSEEK_API_KEY=sk-… cargo run --example basic
```

## 要求

Rust 1.90+（edition 2024）。abycore 不发布到 crates.io，请以 workspace 内
路径依赖的方式使用。
