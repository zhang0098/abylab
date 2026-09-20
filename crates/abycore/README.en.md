**English** · [中文](README.md)

# abycore

An embeddable, asynchronous Rust agent SDK for the DeepSeek Messages API.

abycore owns the agent loop in-process: the host brings its own Tokio runtime,
registers the tools it wants, decides authorization and context policy through
hooks, and persists what it chooses. No requests are made until a client or
agent operation is awaited.

```rust,no_run
use abycore::{Agent, ClientConfig, DeepSeekClient, LocalTools, ModelOptions, RunOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = DeepSeekClient::new(ClientConfig::new(std::env::var("DEEPSEEK_API_KEY")?))?;
    let tools = LocalTools::new(".")?;
    let mut agent = Agent::new(
        client,
        "You are a coding agent in the user's terminal.",
        ModelOptions::default(),
    )?;
    tools.register(&mut agent)?;
    let outcome = agent
        .run("List the files here, then summarize the project.", RunOptions::default(), |_| async {
            Ok(())
        })
        .await?;
    println!("{}", outcome.response.output_text());
    Ok(())
}
```

## Features

- **Agent loop** — multi-step turns over streaming Messages responses, with a
  durable inbox (a prompt sent mid-turn queues behind the active turn),
  per-run budgets (`max_requests`, `max_tool_calls`), run/tool deadlines, and
  cooperative cancellation through `CancellationToken`.
- **Local tool bundle** — `LocalTools` registers `read`, `write`, `edit`,
  `bash`, `glob` and `grep` atomically, or individually:
  - `read`/`write`/`edit` run on a cap-std directory capability with version
    guards (stale files are rejected), atomic staging + publication, and
    byte/line caps.
  - `bash` spawns sandboxed children (Landlock + seccomp on Linux), supports
    background jobs with incremental output, and saves overflowing output.
  - `glob` and `grep` are in-process ports of harness's search tools, built on
    the ripgrep-family crates (`ignore`, `grep-regex`, `grep-searcher`): no
    external binary, gitignore-aware discovery, binary-safe line scanning.
  - Every tool call passes `AgentHooks::authorize`; three permission presets
    (`read-only`, `workspace-write`, `danger-full-access`) gate mutations.
- **Subagents** — one `Subagents` owner spawns or forks children
  (`subagent`, `subagent_fork`), and coordinates them from the parent
  (`list_agents`, `send_message`, `interrupt_agent`). Children inherit the
  parent's instruction baseline and use their own budgets.
- **Sessions** — one fsync'd JSONL log per session in a shared store,
  partitioned per workspace; `SessionWriter` appends checkpoints, and an agent
  restores from a snapshot, replaying unfinished turns safely (pending tool
  calls settle as unverified errors before the next request).
- **Context compaction** — the host answers `AgentHooks::view_request` at
  every model request boundary; the SDK condenses or prunes the request view
  without rewriting the durable transcript, and provider-confirmed overflow
  recovers by condensing hard and retrying the same open turn.
- **Goals** — one durable completion objective per session
  (`get_goal`/`create_goal`/`update_goal`) with a revision-checked completion
  protocol; the host drives rounds.
- **Skills** — `SkillCatalog::discover` reads the `SKILL.md` documents a user
  wrote under the workspace's `.agents/skills/` directory (optional frontmatter
  for name, description and argument hint; bodies cap at 64 KiB and are
  truncated with a marker rather than dropped), `expand` renders a `/name [args]`
  line into the user message that carries the body, `index_block` is the skill
  index a host puts in `AgentHooks::request_context`, and `SkillTool` lets the
  model read one body on demand. The SDK reads text only; nothing here executes.
- **Structured events** — `AgentEvent`/`StreamEvent` keep the model-facing
  view, plan views, tool results, and usage records separable for any UI.

## Example programs

`examples/` ships runnable snippets: `basic.rs` (one completion), `todo.rs`
(plan-driven turn), `tools.rs` (tool bundle), `local_tools.rs` (sandboxed
filesystem tools), `session.rs` (snapshot store), `cancel.rs` (cancellation),
`subagent.rs` (delegation), `web_search.rs` (auxiliary search), and `cli.rs`
(a small interactive terminal agent).

Run one with your key in the environment:

```sh
DEEPSEEK_API_KEY=sk-… cargo run --example basic
```

## Requirements

Rust 1.90+ (edition 2024). abycore is not published to crates.io; depend on it
by path from your workspace.
