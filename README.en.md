**English** · [中文](README.md) · [abylab.ai](https://abylab.ai/)

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
/bin/bash -c "$(curl -fsSL https://abylab.ai/install.sh)"
abylab
```

Prebuilt binaries cover Linux (x86_64, aarch64) and macOS (Intel, Apple
Silicon). The script installs `~/.local/bin/abylab` and checks the download
against the release's `SHA256SUMS`; `--bin-dir` moves it, `--version <tag>`
pins a release, `--help` lists the rest. It also puts that directory on PATH
in your shell startup file (marked with a comment; `--no-path` only prints the
line, `--path-file` picks the file). To build from source instead:
`cargo build --release`.

The Linux binaries are statically linked against musl: one file per
architecture that needs no libc from the host, so RHEL 7+, Amazon Linux 2 and
2023, every Debian and Ubuntu, and Alpine all run the same download — there is
no glibc version to match. Static is not self-contained, though: TLS
certificates and DNS still come from the host (see
[TECH.en.md](TECH.en.md)). A source build wants a C compiler (`aws-lc-sys`
builds its own BoringSSL):

```sh
cargo install --git https://github.com/zhang0098/abylab abylab-tui
```

Then store your key (from <https://platform.deepseek.com/> → API keys) by
typing this into the composer — it takes effect without a restart:

```
/login sk-xxxxxxxx
```

It is written to `~/.abylab/.credentials.yaml` (0600, owner-only). Credentials
never come from the environment: abylab does not read `DEEPSEEK_API_KEY` or any
other variable, and `--api-key <key>` is only a one-run override that is never
persisted.

Options:

```
-w, --workspace <dir>     agent workspace (default: cwd)
    --session-root <dir>  session JSONL root (default: $ABYLAB_HOME/sessions)
    --session-id <id>     resume/continue a durable session id
    --model <id>          model id (default: $ABY_MODEL, else the persisted
                          model, else deepseek-flash)
    --base-url <url>      sets DEEPSEEK_BASE_URL for the agent
    --api-key <key>       override the agent API key for this run
                          (/login persists one instead)
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
  esc, timeout continuations are exhausted, or you hit the context limit.
- When a segment stops on a cap, a timeout (including the total segment
  deadline), or a transient failure (connection lost, rate limit, server error),
  abylab settles unresolved tool calls as error results. Calls interrupted during
  execution are marked unverified, with instructions to check before repeating;
  completed tool results stay in history. It prints an `info`
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
usual. The deadlines are abylab's too: `ABY_TURN_TIMEOUT` defaults to **600
seconds per segment**, and `ABY_TOOL_TIMEOUT` to **60 seconds per tool call**.
Timeouts wait one second before continuing the same open turn, sharing the
`ABY_AUTO_CONTINUE` allowance with budget stops and other retries. Set that
allowance to `0` to stop on the first failure. Once it is spent, the error names
the current limits; send a message to continue, or adjust those environment
variables before starting abylab again. Esc can interrupt both execution and
the wait between segments.

`incomplete` means the model hit its per-request output limit. The driver reports
the limit and pauses automatic goal rounds until you do something. Your next
message joins the unfinished turn's next request, so corrections reach the model
right away. Partial responses stay visible in the transcript but are never
committed to executable history, and tool calls from a truncated response never
run. Want a shorter answer? Just ask. A fresh session uses the SDK output cap
(256,000 tokens); a resumed one keeps the limit it was stored with.

## Commands

`/help` · `/keys` · `/new` · `/resume [id]` · `/compact` · `/goal` · `/clear` · `/model [id]` ·
`/effort [off|low|high|max]` · `/permission [preset]` · `/plan [on|off]` ·
`/vim [on|off]` · `/image <path> [text]` · `/clip [text]` · `/theme [dark|light|pack]` ·
`/status` · `/lang [zh|en]` · `/quit`

`/theme` picks a palette pack — a fresh install starts on **one**, dark
(`--theme dark` only moves between light and dark, it never changes the pack).
The built-in DeepSeek pack and the other seven gallery packs (ayu ·
catppuccin · everforest · iceberg · kanagawa · solarized · tomorrow) are all
embedded in the binary, no external files.
Arrows in the picker preview the highlighted pack in place; `enter` applies it
and `esc` reverts. `ctrl+t` toggles dark/light inside the active pack.

`/status` (run state, model, token/turn/timing counters, plus the session
id/title and the credential source) opens a dialog:
it floats over the conversation, esc closes it, ↑↓/wheel scroll, and nothing
lands in the transcript — same chrome as `/help` and `/keys`.

## Keys

enter send/queue · ctrl+enter send-now · esc interrupt / 2×clear draft ·
ctrl+c clear/quit · ↑ history · `/` commands · `@` file mention ·
ctrl+z / ctrl+shift+z undo / redo · ctrl+p model · ctrl+t theme

`/permission` switches the live abycore session between `read-only`,
`workspace-write`, and `danger-full-access` (default). The switch keeps the
conversation history and applies to later file tools and Bash processes;
Shift+Tab cycles the presets.

While the agent keeps a checklist with `todo_write`, the cap row on the
composer's top border shows the task in progress and the completed/total
progress (`Todo · now: fix login · 2/5 done`). Transient action feedback
borrows that row for a few seconds, then the checklist returns. Clicking the
progress chip (`2/5 done`) opens the full checklist in a dialog; esc closes it.
With neither, the row stays blank.

Usage hints no longer live in that row: a new session (launch, `/new`) opens
the transcript with one rotating hint (esc interrupts, ctrl+enter steers, the token
counters in `/status`, …), and the next session moves on to the next one. The
full list stays in `/help` and `/keys`.

A launch opens the transcript with a centered ASCII wordmark over the project
URL `https://abylab.ai`, then that hint; `/new` shows the hint alone and never
repeats the mark.

That row names the project path; when the workspace is a Git checkout the
current branch rides along colon-tight (`/work/acme/abylab:main`). The branch
is read straight from `.git/HEAD` (no git process) and re-checked on a 5 s
tick, so a checkout made by the agent's shell tool or another terminal
reaches the label; a long path or a narrow terminal keeps the path alone.

The `↥` right of the project path in that same row walks your prompts: each
click jumps one prompt back (newest first, wrapping from the oldest), scrolls
it to the top of the pane and highlights it for a few seconds.

Mouse: wheel scrolls · click a tool card expands it · drag selects, release
copies (native tool → tmux → OSC52) · inside the input box a click places the
caret and a drag selects (`ctrl+x` cuts it) · `@` opens the file browser.

The `@` browser filters as you type (exact > prefix > contains > fuzzy
subsequence) and, when the current directory has no match, follows the query
into the closest subdirectory (skipping heavyweight trees like `.git` and
`node_modules`, three levels deep). ↑↓ move · ← parent · →/tab open a
directory · enter pick · `ctrl+h` shows/hides dotfiles · esc closes it until
the token text changes.

## Technical notes

The internals — workspace instructions (AGENTS.md load order and precedence),
context compaction (thresholds, pruning, overflow recovery), session persistence
(JSONL layout and locking) and goal rounds (allowance and continuation) — live in
[TECH.en.md](TECH.en.md). None of it is needed for day-to-day use.

## License

MIT — see [LICENSE](LICENSE).
