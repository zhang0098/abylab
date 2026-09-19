**English** · [中文](README.md)

# abylab

abycore agent harness in your terminal — a ratatui canvas over the abycore
DeepSeek SDK. Everything runs in one process: no separate runtime process, no
ACP, no plugins, no demo mode.

```
crates/abycore           agent SDK (workspace member): Agent, session snapshots, tools
crates/abylab-backend    abycore Agent driver: Cmd ⇄ Agent::run, hooks → permission ask
crates/abylab-tui        canvas: composer card, markdown, gallery palettes, @file, image thumbs
```

## Minimal, single file, fast

abylab is a minimalist DeepSeek harness: what you install is one executable. No
Node, no Python, no dependency directory, no background process. A release build
on x86_64 Linux (LTO + strip by default) is about 11 MB, about 4.9 MB as a
compressed archive, and a cold start is measured in milliseconds (~2 ms for
`abylab --version` on the machine that built it). Streaming, tool execution and
session persistence all happen inside that one process: no IPC round trips, no
GC pauses.

## Quick start

```sh
cargo build --release
export DEEPSEEK_API_KEY=sk-…
./target/release/abylab
```

Options:

```
-w, --workspace <dir>     agent workspace (default: cwd)
    --session-root <dir>  session JSONL root (default: $ABYLAB_HOME/sessions)
    --session-id <id>     resume/continue a durable session id
    --model <id>          model id (default: $ABY_MODEL, else the persisted
                          model, else deepseek-flash)
    --base-url <url>      sets DEEPSEEK_BASE_URL for the agent
    --api-key <key>       sets DEEPSEEK_API_KEY for the agent
    --theme <dark|light>  appearance mode (default: persisted, else dark)
```

There are no flags for the fine tuning — just set the environment:
`ABY_MAX_REQUESTS`, `ABY_MAX_TOOL_CALLS`, `ABY_AUTO_CONTINUE`,
`ABY_TURN_TIMEOUT`, `ABY_TOOL_TIMEOUT`, `ABY_CONTEXT_WINDOW` (enables
compaction), `ABY_COMPACT_AT`, `ABY_KEEP_RECENT`, `ABY_PRUNE_TOOL_OUTPUT`,
plus `ABYLAB_HOME` and `ABY_MODEL`.

Your preferences live in `$ABYLAB_HOME/settings.json`: interface language
(Chinese by default), model, reasoning effort, permission preset, and
appearance (mode + palette). Flags override them for that run only.

## Turn budgets

abycore always enforces a per-run request/tool ceiling (conversation requests,
retries and `web_search` auxiliary requests all share it). abylab treats those
numbers as a backstop, never as a normal way to end a turn:

- `ABY_MAX_REQUESTS` / `ABY_MAX_TOOL_CALLS` default to **1000** each. Set either
  to `0` and the cap is gone: the turn runs until the model finishes, you press
  esc, the run deadline passes, or you hit the context limit.
- When a segment stops on a cap, or on a transient failure (connection lost,
  rate limit, server error), abylab settles any tool call it left unexecuted as
  an error result — the model never assumes a write landed — prints an `info`
  notice, waits out `Retry-After` when the provider sent one, and resumes the
  same open turn, up to `ABY_AUTO_CONTINUE` times (default 3).
- The turn ends with an error only once that headroom is spent, and the session
  stays resumable: send another message and it picks up where it left off.
- A stuck loop gets advice, not a kill: once the same tool call with identical
  arguments repeats 3, 5 and 8 times in a row, the host queues a short reminder
  for the model. The call itself is never blocked or delayed, `todo_write` is
  exempt, and a new prompt or a different call restarts the count. That guard
  lives in `abycore::AgentHooks::tool_reminder`; abylab installs it through its
  hooks.

The budgets are abylab's own backstops, not DeepSeek limits — usage is billed as
usual. The deadlines are abylab's too: once the budgets are out of the way, what
actually binds is `ABY_TURN_TIMEOUT` (the turn stays resumable, so send a
message to continue it), and `ABY_TOOL_TIMEOUT` bounds a single tool call.

`incomplete` means the model hit its per-request output limit. The driver reports
the limit and pauses automatic goal rounds until you do something. Your next
message joins the unfinished turn's next request, so corrections reach the model
right away. Partial responses stay visible in the transcript but are never
committed to executable history, and tool calls from a truncated response never
run. Want a shorter answer? Just ask. A fresh session uses the SDK output cap
(256,000 tokens); a resumed one keeps the limit it was stored with.

## Workspace instructions

Before the first model request, abylab reads `$ABYLAB_HOME/AGENTS.md` (default
`~/.abylab/AGENTS.md`), then walks from the nearest Git root down to the startup
workspace. With no `.git` marker it searches the workspace directory only. Both
Git directories and worktree `.git` files count.

Each directory loads `AGENTS.md`, `CLAUDE.md`, `AGENTS.local.md`, and
`CLAUDE.local.md`, in that order. Anything present gets considered; identical
trimmed content in the same directory is included once. More specific directory
instructions take precedence. On Linux, file names are case-sensitive.

Instructions go out as user context, below system, developer and direct user
instructions. The combined context is capped at 64 KiB: more specific files win,
and the last one is truncated at a UTF-8 boundary if it comes to that. Files over
1 MiB are skipped. Read errors, omissions and truncation are reported; missing
files stay silent.

New and resumed sessions both reload from disk. Children inherit the parent's
loaded baseline, and compaction keeps it around. That baseline is a runtime
request prefix: it never turns into a user transcript entry or a session title.
abylab loads the startup directory chain only; after editing instructions, start
a new session or resume the current one to reload them.

## Context compaction

Long sessions get condensed instead of failing, matching harness's
`dsh-compaction-basic` (condense at 80% of the window, keep the newest 16%
verbatim):

- `ABY_CONTEXT_WINDOW=<tokens>` turns it on. Only the host knows the routed
  model's window; abycore has no model table, and without the number there is no
  way to price pressure.
- The policy is asked at **every model request**, not just between turns
  (`abycore::AgentHooks::view_request`), so one long turn gets condensed between
  its steps too. The agent asks, abylab decides, and the SDK summarizes and swaps
  the view.
- When the estimate reaches `ABY_COMPACT_AT` of the window, abylab first trims
  each older tool result to `ABY_PRUNE_TOOL_OUTPUT` bytes. If trimming alone gets
  the request back under the threshold, no model call happens at all; otherwise
  the oldest safe span is summarized and the newest `ABY_KEEP_RECENT` stays
  verbatim. Summaries are built from the current visible history, including
  existing checkpoints and pruned outputs. Repeated compaction merges older
  checkpoints instead of stacking overlapping spans. Automatic summary calls
  share the run's cancellation, deadline and request budget, and honor the
  summary output cap.
- A provider-confirmed context overflow (`context_length_exceeded`, or abycore's
  own input-budget refusal) doesn't end the turn: abylab condenses hard — keeping
  only the current turn — and retries the same open turn. Only if that fails does
  the error surface, pointing at `/compact` and `/new`.
- `/compact` condenses on demand, even below the threshold. Esc cancels manual
  and overflow summaries alike. A changed view is saved before success is shown.
- The durable transcript is never rewritten: compaction records shadow a span in
  the *request view* only, so the session JSONL, `/resume` replay and the raw
  items all stay complete. The SDK refuses a span that would split a tool call
  from its result or an existing checkpoint, and it rejects truncated summaries,
  tool-call replies and summaries that don't shrink the current view. An ordinary
  summary failure leaves the view unchanged; cancellation, deadline and
  request-budget exhaustion stop the run. Automatic view changes pass a
  persistence barrier before the next model request.

Defaults: `ABY_COMPACT_AT=0.8`, `ABY_KEEP_RECENT=0.16`, `ABY_PRUNE_TOOL_OUTPUT=4096`,
summary cap 8192 output tokens. Pressure is priced with a valid provider
measurement when there is one, otherwise estimated at 3 bytes per token. A view
change invalidates the old measurement; restoring an already-compacted session
conservatively waits for a new one.

Where abylab is stricter than harness, and the limits that bind in practice:
abycore's 10-minute run deadline per segment and its 4 MiB serialized input cap.
The byte estimate is not a guaranteed upper bound.

## Commands

`/help` · `/keys` · `/new` · `/resume [id]` · `/compact` · `/goal` · `/clear` · `/model [id]` ·
`/effort [off|low|high|max]` · `/permission [preset]` · `/plan [on|off]` ·
`/image <path> [text]` · `/clip [text]` · `/theme [dark|light|pack]` ·
`/session` · `/lang [zh|en]` · `/quit`

`/theme` picks a palette pack: the built-in DeepSeek default plus the nine
gallery packs (ayu · catppuccin · ember · everforest · iceberg · kanagawa ·
one · solarized · tomorrow) — all embedded in the binary, no external files.
`ctrl+t` toggles dark/light inside the active pack.

## Session persistence

abycore keeps one JSONL log per session in a shared store, partitioned by
workspace slug:

```
~/.abylab/sessions/<workspace-slug>/<id>/session.jsonl
#   header + title + snapshot lines · slug: /home/x/proj → --home-x-proj--
```

- The log appears at the first checkpoint; every append is fsync'd, and a failed
  append rolls back — torn lines never survive.
- `--session-id <id>` replays the stored history, and your next message continues
  an unfinished turn. Pending tool calls are settled as unverified errors before
  that message goes out; they are not replayed.
- `/resume` lists the workspace store (newest first).
- A non-blocking `flock` on `session.lock` keeps it to a single writer.

Changing permissions or the API key at runtime keeps the parent's runtime
identity, inbox and session writer. Existing children keep the credentials and
permissions they were delegated. Child callbacks own separate compaction and
repeat-reminder state, use the configured request/tool/time limits, and can't
hold on to the parent's session writer after a session switch. On exit, abylab
waits for subagents before shutting down local tools. Host goal changes and the
final run state (including incomplete/error exits) are saved before their settled
state is announced.

## Goal rounds

One durable objective per session, with an explicit round allowance:

- `/goal <objective>` creates the goal and spends round 1 right away.
  `/goal @<n> <objective>` sets the allowance (default 10, max 100).
- `/goal status` prints it; `/goal pause|resume|complete|clear` controls it.
  `/goal resume` also gets an idle goal moving again, and any prompt you send
  while the goal is armed continues the rounds after it.
- Each round is one ordinary turn seeded with the objective; the model ends the
  loop by calling `update_goal` with `complete` or `blocked` (it has to quote the
  id and revision from `get_goal`, so a stale model can't overwrite a goal that
  moved on). Running out of allowance records a blocker instead of looping
  forever, and esc still interrupts a round.
- Continuation is armed by *you*: a goal the model creates through its tools
  doesn't start any rounds until you run `/goal resume`. The SDK never starts a
  turn on its own; the driver here does.

## Keys

enter send/queue · ctrl+x send-now · esc interrupt / 2×clear draft ·
ctrl+c clear/quit · ↑ history · `/` commands · `@` file mention ·
ctrl+z / ctrl+shift+z undo / redo · ctrl+p model · ctrl+t theme

`/permission` switches the live abycore session between `read-only`,
`workspace-write`, and `danger-full-access` (default). The switch keeps the
conversation history and applies to later file tools and Bash processes;
Shift+Tab cycles the presets.

Mouse: wheel scrolls · click a tool card expands it · drag selects, release
copies (native tool → tmux → OSC52) · `@` opens the file browser.

## License

MIT — see [LICENSE](LICENSE).
