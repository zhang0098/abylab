**English** · [中文](CHANGELOG.md)

# Changelog

What each abylab release changed, from the user's side. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions follow
[Semantic Versioning](https://semver.org/). Internals (workspace instructions,
context compaction, session persistence, goal rounds, uninstall) live in
[TECH.en.md](TECH.en.md); day-to-day usage is in [README.en.md](README.en.md).

## [Unreleased]

### Changed

- `ctrl+q` no longer quits on a single press: the first press only shows "press ctrl+q again to
  exit" in the tip row, and a second press within 1.5s really quits — the same chord `ctrl+c`
  wears on an empty draft, so a stray keypress cannot end the session. Typed `/quit` is still
  immediate.
- The notice printed after leaving the alternate screen no longer draws its two `─` rules; the
  session id, the `abylab --session-id` command and the `/resume` hint remain.
- New `/delete`: permanently deletes the current session — the snapshot log under `.abycore` and
  the queued-prompts file go together — then opens a fresh session. A confirmation card guards the
  removal (cancel starts selected, Esc closes), and only choosing "Delete permanently" and pressing
  enter acts. If the files cannot be removed (another process holds the session), they stay, and the
  reason lands as a notice on the new session.
- `/enter` is no longer one of abylab's own commands: it is gone from the `/` menu, tab completion
  and `/help`, and a hand-typed `/enter …` resolves like any other unknown `/name` (skill injection,
  else the unknown-command notice). The busy-Enter preference itself is untouched — the `enter` value
  in `settings.json` still decides whether plain Enter queues or steers while a turn runs (queue by
  default; ctrl+enter is always the other), there is just no command left that changes it.

## [0.1.18] - 2026-09-26

### Changed

- `/plan` is no longer one of abylab's own commands. It is gone from the `/` menu, tab completion (its
  `on`/`off` argument list with it), `/help`, and the command list on abylab.ai. Where that line goes has
  not changed: hand-typed it still ships as a prompt, and a workspace skill carrying the name is injected
  like every other `/name`. Host plan mode is untouched — the transcript still reports the `plan mode`
  facts the driver sends — so what is gone is the client's own offer of a command the skill path already
  covered. `/help` and `/keys` list one command less.
- The queue goes by one name: what `alt+↑` opens is 排队命令 in Chinese, not 排队消息 — the list header,
  the edit and delete tips, `/help`, the `/data` panel and both READMEs. `shift+enter` and `ctrl+j` moved
  into the composer's editing group in `/keys`, beside the draft's own keys: they type a newline, they
  never sent anything, and they sat under send.
- An empty composer hints `Enter any command` / 输入你的任何命令, where it used to say "describe what you
  want to build…" / 描述你想构建的内容….
- The Chinese interface caught up with a few English phrasings: 轮换 becomes 切换 for `shift+tab` and
  `ctrl+o`, `/resume` is described as 恢复当前工作区之前的会话, steer is translated as 插话 instead of
  being left in English (`已插话，Agent 下一步会处理`), and the Enter-while-busy setting reads
  运行中按 Enter rather than 繁忙时 Enter — in `/help`, `/keys`, the tips and the READMEs.

### Fixed

- An image-only prompt sent while nothing was running no longer lands in the transcript twice. It used to
  take the queue path: an item was enrolled with its own optimistic echo, and the driver then delivered
  the prompt, so a `/clip` send painted the picture and painted it again once the turn started. It now
  leaves as one prompt with one row — and only if the session it was composed in is still the active one:
  switching sessions (or `/resume`) between staging the image and pressing enter is refused with a
  `prompt rejected: the active session changed` notice instead of being handed to whichever session is
  open by then.
- The `ctrl+q` row in `/keys` called abylab `martty`, the name this code carried before it was abylab. It
  now reads `quit abylab` / 退出 abylab.

## [0.1.17] - 2026-09-25

### Fixed

- A clipboard helper that exits early and closes its input pipe no longer terminates abylab and
  its running tasks with SIGPIPE. A failed copy continues through the fallback routes.

## [0.1.16] - 2026-09-25

### Changed

- A finished thought no longer keeps a row. Each one used to end with `✻ thought · 9.1s · 91 lines` —
  when the thinking happened, how long it ran, how many lines came out. None of it is anything you
  read the transcript for, and a turn that thinks a dozen times paid a dozen rows of height for it. The
  row is gone, and so is the blank line above it: thoughts take up no room at all in summary and
  preview. `ctrl+o` (full) still opens them — each thought then leads with a plain `✻ thought` divider
  (the seconds and the line count do not come back; the lines under the divider are the count). The
  live row stays, since it is what tells you the model is quiet rather than stuck, and it now follows
  the interface language: `✻ 思考中… 3s` in Chinese, `✻ thinking… 3s` in English — it used to say
  `thinking…` in every language.
- abylab introduces itself differently now. The READMEs and the homepage used to say what it is built
  from — "a minimalist DeepSeek harness, built with Rust, ratatui, and the abycore DeepSeek SDK" — and
  now say what you can do with it: a free, open-source, minimalist AI assistant for writing code,
  editing Excel files, writing reports, or adjusting your computer's settings, with nothing to set up
  beyond a DeepSeek API key. The site's title, meta description and `og:title`, and the README banner's
  alt text and SVG description, carry the same line. The implementation pedigree (`abycore`, `ratatui`)
  moves off the front page into [TECH.en.md](TECH.en.md), where it already lived; the credit to DeepSeek
  Harness at the end of the README stays.

## [0.1.15] - 2026-09-24

### Added

- The composer border's top-left now carries `abylab.ai`, like a window title. That cell used to stay
  empty when the app was idle; a tip or the todo checklist takes the row while it is up, and the brand
  returns when it expires.
- The preview card of a staged image now pops up by itself after `/clip` (and ctrl+v, or an `@`-picked
  image) and stays on screen. It used to appear only while the mouse hovered the chip or the text cursor
  sat in or right after it, so the first character of the caption you typed took it away — you could not
  look at the picture while writing about it. A freshly staged image is now pinned: the card lives
  exactly as long as that image does (⌫ on the chip, esc clearing the draft, or sending all take it
  with them), while hovering another chip still previews that one. The `/help` line for `/clip` says so.

### Fixed

- With an empty draft the caret block sat on the first glyph of the placeholder hint, boxing that
  character. The hint is now ghost text one cell right of the caret, which keeps a blank cell of its
  own (reversed into a solid block) and looks like the caret over a real draft.
- The caret stayed in the composer while the `ask_user_question` dialog's "Other" field was selected.
  A card owns the whole keyboard while it is up (every key reaches it first, the draft gets none), yet the
  composer still painted its caret block on the draft — and the IME popup anchored there too, so typing
  into the dialog looked like typing into the input box. The composer now paints no caret while any card
  owns the keyboard; the question dialog paints one in its own `Other:` field — right after the label on
  an empty field, which no longer carries a "type an answer" hint (the caret is the affordance), then
  riding the tail of what you type — and a permission card stacked over the dialog leaves no caret on
  either side.

### Changed

- Interrupting the running turn now takes two `esc` presses. One press used to cancel outright, so a
  stray keypress threw away the turn in flight; the first press now only arms — a tip says "press esc
  again to interrupt", live for 1.5s, the same chord shape `ctrl+c` quit wears — and the second one
  cancels. Neither touches the draft. Closing a card, cancelling a queue edit and dismissing suggestions
  stay single-press, and `/help`, `/keys` and both READMEs say so.
- A dot now separates the workspace title from the `↥` glyph at the right end of the composer cap, in the
  same pale tone as the title itself: `· ~/project/abylab:main · ↥`.
- The install one-liner is now `curl -fsSL https://abylab.ai/install.sh | bash` — the same script at the
  same URL, without the `/bin/bash -c "$(…)"` wrapper. Both READMEs, the site hero, `install.sh --help`
  and the note in `site/_redirects` all switched. The script never reads stdin (the `read` behind the
  PATH step consumes its own heredoc), so piping it into `bash` cannot eat the script.
- Dropped the `⛶` expand button at the right end of the composer cap. It was mouse-only: one click pinned
  the input well to 5/8 of the frame, another restored it. The well now only grows with the draft
  (half the screen, capped at 14 rows), and the `click ⛶` row is gone from `/keys`. The right end keeps
  just `↥` (jump back to your previous prompt).
- `ctrl+o` cycles three tool-output modes instead of toggling two: **summary** (the default — one line per
  call), **preview** (one card per call with a 4-line tail), **full** (every thought and result expanded),
  then back. Summary folds consecutive calls into one block headed by a count — `13 tool calls · 13
  commands · 1 failed`, with the command and failure segments dropped when they are zero — and hangs each
  call off the head as a branch, `├─ Ran ls -la …`, the last one closing it with `└─`. A finished call
  carries no bullet, since the line is the call; only one still running keeps its spinner, after the
  branch. A whole turn's tool work then fits on a screen instead of drowning the
  conversation in output. A failed call leads with its own message rather than a made-up verb
  (`Failed shell request · 1 invalid field`) and keeps the rest behind a click; a successful call opens
  its full output on one. The header line ignores clicks. An empty assistant message between steps does
  not break a block, so a multi-step turn still reads as one; an assistant reply or a thought with text in
  it splits them as before. The `ctrl+o` row in `/keys` says so. The verb opening each call stays
  English in every locale (`Ran` / `Read` / `Wrote` / `Edited` / `Searched`): a command or a path follows
  it, and the tools we cannot name already show their English tool names. The count header, the click
  hints and a failure's own message still follow the interface language.

## [0.1.14] - 2026-09-24

### Added

- `ask_user_question` lets the agent pause during a task for a Yes/No choice, a single or multiple selection, or a written answer. The same turn continues after the answer; `esc` cancels the question.

### Fixed

- Concurrent asks queue in arrival order. When a second permission ask or task
  question arrived while one was on screen, the new one used to replace it
  outright: the replaced asker got no answer (its reply channel went with the
  overlay, so the tool or subagent waiting on it saw only a cancellation), and
  the user never saw what it asked. Later asks now wait their turn, one is
  answered and the next appears; an ask whose asker has already gone (an
  interrupted subagent, say) is skipped instead of shown. When a permission ask
  and a task question are both up, the permission card draws on top and owns the
  keyboard, so a paste cannot land in the question hidden behind it; a parent
  turn going idle no longer clears a still-running child's question. Keys that
  some terminals deliver wrapped as a paste are handled as keys too — esc and
  ctrl+c still cancel a question instead of being typed into its answer.
- Image prompts actually reach the model. An image staged in the composer used
  to be a chip and an echo: the in-process transport carried text only, the
  bytes never left the terminal, and the prompt the model received did not
  contain it. Images now travel the whole way with the prompt — queueing,
  editing a queued prompt, Send Now steering, subagent steering, the session
  snapshot — and appear in the request as base64 image blocks, in order with the
  text. png / jpeg / gif / webp; a prompt's inline images total at most 32 MiB
  (more is refused with a tip), and a conversation carrying images gets its
  request budget raised from 4 MiB to 48 MiB.
- The durable queue is split by workspace. It used to live at
  `$ABYLAB_HOME/queued/<session id>.json`: the file name knew only the session
  id (which is user-supplied), so two workspaces with the same id shared one
  file and a restart could show workspace A's waiting prompts in workspace B;
  the old path also folded every non-alphanumeric character to `_`, so `a/b` and
  `a_b` landed on the same file; and it stored text alone, so a queued image
  died at the first restart. The path is now
  `queued/<workspace hash>/<escaped session id>.json`, any byte of an id outside
  `[A-Za-z0-9_-]` is escaped losslessly as `~XX`, and records keep text and
  image blocks. A damaged or unknown-version store is no longer silently read as
  an empty queue: loading reports the error to the user, and saving refuses to
  overwrite it (so anything still salvageable by hand survives); a queue file in
  the old location is reported with its path for the user to move after checking
  it. Saves go through a temp file and a rename, and a queue change that cannot
  be written is rolled back and reported instead of pretending to succeed.
- The installer verifies before it replaces. Upgrading used to `install`
  straight over the installed binary, so a truncated download, a
  wrong-architecture asset or a binary that would not start on this system
  overwrote a working abylab. The new binary is now staged inside the target
  directory and has to answer `--version` before it is moved onto the
  destination; a failed check removes the staged file, keeps the previous
  version, and exits non-zero.

## [0.1.13] - 2026-09-23

### Added

- `abylab --uninstall` uninstalls abylab: it lists one plan in two
  halves — the data this program saved under `$ABYLAB_HOME` (`settings.json`:
  language, model, effort, permission preset, appearance; `abylab-modes.json`:
  the per-workspace mode cache; `.credentials.yaml`: the key `/login` stored;
  `sessions/`: the session logs; `queued/`: prompts that were waiting) and the
  program itself (the running executable, every `abylab` on `$PATH`, the
  installer's default `~/.local/bin`, and `$ABYLAB_BIN_DIR`). Each `remove` and
  `keep` row carries its size and file count, and the confirmation comes in two
  questions — data first, program second — so "keep the data, remove the
  program" is answering no to the first question; `--keep-data` fixes that
  answer and `--yes` skips both (in a script, `--yes` is the full clear and
  `--keep-data --yes` is the program only). Two noes remove nothing. There is no
  separate `/reset`: clearing the data is just the first question here.
- The list is a whitelist, not a directory scan: a hand-written
  `~/.abylab/AGENTS.md` (your file — abylab only reads it) and a session store
  `--session-root` put outside the aby home are listed as kept, and the home
  itself is never a target. The program half only takes a file still called
  `abylab`; a symlink goes with the file it points at, link following is
  bounded; a Homebrew or Nix copy is listed as kept; a copy in `~/.cargo/bin`
  goes through `cargo uninstall abylab-tui`, so cargo's bookkeeping goes with
  it. Shell startup files are left byte for byte alone — the PATH line the
  installer added stays, and can be removed by hand.
- It prints the plan line by line, then asks `[y/N]` on `/dev/tty` — never on
  stdin, which may be carrying something else — and refuses to run at all when
  there is no terminal to ask on, so saying yes takes an explicit `--yes`. The
  wipe reports per path and exits non-zero if anything refused to go. The flag
  never opens the UI and ignores the other launch options: only `--session-root`
  matters, because it names the store being checked. Another abylab still
  running is called out in the plan — it writes its settings and session log
  back, so quit it first.

### Fixed

- `ABY_TURN_TIMEOUT` is gone. It used to cap each segment's **total duration**
  (600 seconds by default): model thinking, streamed output and every tool call
  counted against it. A turn that had been running for fifteen minutes with every
  step making progress was interrupted at the 600-second mark — the running tool
  became "unverified", text already streamed was lost — and once three automatic
  continuations were spent, the screen showed `· turn end · error` with
  `timed out (ABY_TURN_TIMEOUT=600, ABY_TOOL_TIMEOUT=60 seconds; ABY_AUTO_CONTINUE=3)`.
  A turn never should have had one: it is a local loop, DeepSeek only ever sees
  stateless requests one at a time, so "how long this turn has been running" was
  never its rule — and harness's agent loop has **no** deadline over a step
  either. The only time limit left is `ABY_TOOL_TIMEOUT` (next entry), while
  everything that can actually get stuck has an owner: each request belongs to
  the transport (15s connect, 120s first byte, 60s stream idle), each tool call to
  its own budget, a runaway loop to `ABY_MAX_REQUESTS` / `ABY_MAX_TOOL_CALLS`
  (1000 each by default), and the user can press Esc at any point.
- A tool's deadline no longer fails the whole turn, and no longer overrides the
  tool's own setting. `ABY_TOOL_TIMEOUT` (60 seconds by default) used to apply to
  every tool call and outrank a budget the tool had declared itself: `bash`'s
  `timeoutMs` could ask for 600 seconds (`bash_max_timeout` caps it) and the
  executor still killed the command at 60; the same held for a foreground
  `subagent` waiting on a child turn, where a child that took a little longer had
  its wait cut short. The tool's own declared budget now wins — the way harness
  reads each tool's own `timeoutMs` instead of capping every call at one
  host-wide number — and `ABY_TOOL_TIMEOUT` only backs up the tools that declare
  nothing (`read`/`write`/`edit` and friends), while a foreground delegation and
  `wait_agent` carry no timer at all: the child turn ends on its own. On expiry a
  call is asked to stop, gets its own cleanup grace to settle, and the outcome
  reaches the model as a tool error (saying it was stopped and that its result is
  unverified) while the turn keeps going — the answer
  `dsh-tool-call-timeout-policy` gives, not an `error` turn end.
- The timeout error line no longer talks around the point: a request or tool call
  reaching its own deadline says so (a tool declaring no budget stops at
  `ABY_TOOL_TIMEOUT` seconds), and it still lists the current environment values
  with "send a message to continue it".

## [0.1.12] - 2026-09-23

### Changed

- `shift+tab` says when a permission switch is waiting. The driver serializes
  turns — it reads its command channel only between them — so a switch taken
  mid-turn lands at the turn's end rather than failing. Until now the chord went
  quiet after the ask: the chip cannot move before the host's durable echo, so a
  busy session showed no tip and no change, and the key read as broken. It now
  prints `permission → Read Only · applies after this turn` (a switch staged
  before the first prompt keeps `permission → Read Only …`), and the tip spells
  the preset the way the chip does instead of leaking the wire id.
- `shift+tab` steps from the last *request* instead of the chip. Two presses
  inside one turn used to compute the same target — the second one died in the
  driver's equal-preset branch, so N presses moved the cycle one step, and the
  harder you pressed the less it moved. Every press now advances one step and the
  host applies them in order; asking twice for the same preset (say `/permission`
  naming the one already on its way) no longer re-sends it, which would have
  rebuilt the local tools a second time at the boundary. The docs were wrong
  too: `/help` and `/keys` said "workspace-write ⇄ full access", but the chord
  walks all three presets, and the first press from the `danger-full-access`
  default lands on read only — the parenthetical that made that press look lost.
- The permission chip no longer highlights `Full access`: the chip on the meta
  row's left now paints all three presets in one tone (`fg_tertiary`), the same
  one the approval chip behind it uses. `danger-full-access` used to go warn
  (amber), but a trusted-directory session sits in that preset all day — a colour
  that is always on carries nothing, and it drowned out the queue count on the
  same row, which does change and uses warn too. The label still follows the
  preset (Read Only / Workspace Write / Full access), and the preset's full
  description still lives in the `/permission` picker and `/status`.
- `--help` no longer lists the tuning environment variables: the trailing
  `ENVIRONMENT (advanced tuning; no flags):` block (`ABYLAB_HOME`, `ABY_MODEL`,
  `ABY_MAX_REQUESTS`, `ABY_CONTEXT_WINDOW`, …) is gone, so the help ends at the
  option table. Those variables are advanced tuning only, and what a day-to-day
  run touches is the matching option; the defaults the flags read (`$ABYLAB_HOME`,
  `$ABY_MODEL`) are still named in the table, and the compaction and timeout
  knobs are still documented one by one in [TECH.en.md](TECH.en.md).
- The status bar carries the queue count now: while prompts are waiting, the
  meta row (the composer's bottom border) leads with `· 2 queued`, plus the `⌥↑`
  chord whenever the composer is free. Until now "how many are queued" showed up
  in three places, and each one vanished exactly when it was needed: the
  transcript's tail line skips its own `· N queued` while the model streams
  (which is when most queuing happens), the `⏎ send queue head` hint only exists
  while a turn runs with an empty draft, and the tip at enqueue time lives for 4
  seconds. The count no longer depends on the draft, so it is always there; the
  `⌥↑` half appears only when the selector would really open (empty draft, no
  staged image), because the list needs a free composer — chrome should not
  advertise a key that would refuse. With the chip in place the `⏎ send queue
  head` hint is gone from the row: it no longer restates the queue, since an
  empty enter still ships the head and `/keys` documents that.
- The timeline has no usage hints any more: a launch keeps the wordmark, the
  project URL and the four launch facts (version, working directory, permission,
  model), and `/new` no longer greets a session with one either. The seven-hint
  rotation (`esc` interrupts, `enter` queues, `@` mentions a file, …) went with
  the counter that walked it — the keys and commands themselves are unchanged,
  and `/keys`, `/help` and the slash menu still list them.
- The no-key onboarding reads differently: one sentence, the `/login sk-xxxxxxxx`
  command alone in a framed code block (it is the only thing the reader has to
  *do*), then a rule with the storage path and the key-management commands under
  it. The numbered steps it replaces tucked the command into prose.
- The lines printed on exit are a ruled band now: a `─` rule above and below
  (as wide as the terminal, capped at 72 columns), `abylab · session closed ·
  <session id>` inside, the `abylab --session-id <id>` command alone on its line
  (selecting that line copies the command), and the `/resume` route underneath.
  The three flat `label: value` lines it replaces are gone.

## [0.1.11] - 2026-09-22

### Changed

- The built-in `glob` and `grep` tools are gone: workspace search is bash again
  (`rg`/`grep`/`find`), which also drops about 1.6KB from every request prefix
  and one more pair of search semantics to maintain. What sank them was the
  output contract: `grep` returns line numbers with matching lines capped at
  100 and no count, so counting or listing files means shelling out anyway, and
  `glob` lists files only — never directories, no size, no time beyond a
  modification-time order. The paired probe had already shown the model
  preferring the shell: bash was 16.7% of calls, more than half of them
  searching or paging files. The cost, stated plainly: under `read-only` and
  `workspace-write` every search now asks for approval, since `read` is the only
  read tool left that never does; `danger-full-access` is unaffected. The
  system prompt follows: it no longer names glob/grep, it names `rg`, `grep` and
  `find` instead.

## [0.1.10] - 2026-09-22

### Changed

- The system prompt now names the tool split **and the shell command each read
  tool replaces** (harness's one-section-per-tool shape): "Read files with read
  — not cat or sed. Search them with glob and grep — not shell find or rg". It
  previously named only read/write/edit/todo_write — "search" never appeared —
  so the model reached for bash by default (measured on this machine: bash was
  78% of 2112 tool calls across 15 sessions, with 781 `| head` and 453
  `sed -n`/`awk` used as a pager, against 2 `glob` and 15 `grep` calls). Naming
  the commands rather than a vague "no pipelines" keeps command-output piping
  legitimate (`cargo test | tail -20`) while paging and searching files is not.
  The three named read tools are workspace-rooted and never ask for approval,
  while bash asks every time under read-only / workspace-write. New sessions
  only: a session restored with `/resume` or `--session-id` keeps the prompt it
  was created with.

## [0.1.9] - 2026-09-21

### Changed

- ctrl+enter steers instead of cancelling and re-sending: the message enters the
  running agent's inbox, and the SDK appends it as an ordinary user message at
  the next completed tool-batch boundary (or runs one more step when the model
  had already finished), so the current step's streamed output and its tool
  results survive — and the "interrupted — turn cancelled" notice is gone from
  this path. esc is now the only interrupt. A steer that misses its window is
  not an error: it is delivered as the next waking turn, and with no running turn
  at all (no `/login`, say) the item returns to the client queue with its queued
  tint back.
- New `/enter queue|steer` busy-state preference: it picks whether plain enter
  queues or steers while the agent runs, ctrl+enter always takes the other one,
  and an idle session sends either way. Queue stays the default; the choice is
  written to `settings.json` and survives a restart. The composer's shortcut
  hints, `/help` and `/keys` follow it.
- Empty draft plus ctrl+enter steers every queued message, in FIFO order (it used
  to be a no-op); without a running turn the head ships as an ordinary prompt and
  the rest stay queued.
- A steer is now visible in stages on the timeline: the bubble carries a
  `steering` marker from ctrl+enter until the agent actually takes it at a
  boundary (the inbox drain reports `SteerAdmitted`), and a rejected or late
  steer goes back to `queued`. Before, only the "steered" tip said anything —
  it could not show whether the message had landed.
- The `⌥↑` queue list steers rows one at a time: ctrl+enter hands the
  highlighted row to the running turn (the list rebuilds in place, the
  highlight follows the row that moved up, and it closes when the queue is
  empty) — the same chord the composer uses. The list title, `/help` and `/keys`
  name the key.
- Queued prompts survive a restart: every mutation writes
  `$ABYLAB_HOME/queued/<session>.json` (one file per session, removed when the
  queue drains). A start or `/resume` onto that session paints the items back as
  queued bubbles and **holds** them — they were queued behind a turn that no
  longer exists, so they are not spent unbidden; the first turn this process
  actually runs releases them, and they ship in FIFO order after it. A missing
  or malformed file is just an empty queue, exactly like `settings.json`.
- Messages the turn has not taken yet render as a tail section (`⏳ N steering`):
  they sit below the live output until the agent takes them at a boundary, and
  then drop back into their chronological slot (harness's pending-steering
  rows).
- The queue moved into the driver, and clients render snapshots: `CtlEvent::Queue`
  publishes every row on every change, so any client (the TUI today, a second
  process or a web UI later) sees the same FIFO. Rows carry a placement —
  queueing and steering are two states of one list.
- Delivery order is decided at the driver's idle wait: pending commands first
  (removals, edits, new steers), then rows the turn took, and only then the head
  ships — a removal can never lose a race with the boundary that drains the row
  it changes. Taken ids stay as tombstones so a late queue command cannot turn a
  delivered row back into a queued one.
- Queue persistence moved with it: the driver writes
  `$ABYLAB_HOME/queued/<session>.json` and reads rows back as held, released by
  the first turn this process actually runs (same promise, new owner).

### Fixed

- "steered — lands at the next agent step" now means it: the old gesture actually
  cancelled the active segment and re-sent the prompt, and the UI reported an
  interrupt alongside it.
- The website's key row and both READMEs say what the keymap implements again:
  send-now is ctrl+enter (the old line promised ctrl+x, which cuts the composer
  selection), and the queue list's `alt+↑` (mac `⌥↑`) is in them now. 0.1.8 had
  already fixed the TUI's own hints; the two pages were what it missed.

## [0.1.8] - 2026-09-21

### Added

- The `⌥↑` queue list deletes too: besides ↑/↓ and enter-edit, `ctrl+d` drops the
  highlighted row (two presses, and the armed row says "ctrl+d deletes" while it
  waits). The list rebuilds in place with the highlight on the row that moved up,
  so several queued prompts can go in a row; deleting the last one closes the
  dialog. The key is in the dialog's title, in `/help` and in `/keys`, and popups
  now widen to fit their title instead of clipping the hint.
- Quitting prints the session id and the way back: once the alternate screen is
  restored, the shell gets three lines — `Session id: aby-…`,
  `Resume it later: abylab --session-id aby-…` and `Or pick it with /resume in
  the app`. One idea per line, so the resume command can be selected on its own.
  The id is the session that was on screen at exit — a mid-run `/resume` switch
  moves it — and each interface language words it its own way.

### Fixed

- An edited queued prompt repaints its echo: saving an edit in the `⌥↑` editor
  replaced the queue item but left the timeline showing the old wording. The
  run of bubbles is now repainted in place when the edit is saved — the queue
  order stays the order on screen — block count and kind changes included, with
  the queue, the `↥` jump and every other item's indices recalculated.
- A deleted queued prompt no longer lingers in the timeline: after `ctrl+d` in
  the `⌥↑` editor the echo bubble used to keep saying `queued` even though the
  prompt never left the client. Deleting the message now deletes its bubbles,
  the items behind it are remapped onto their new cell indices (the `↥` jump and
  any waiting steer are recalculated too), and a runtime exit drops a dead
  queue's echoes the same way.
- Tool cards are no longer blank: for a call the agent answers itself, the UI
  used to show the tool's placeholder output (an empty string for `get_goal`),
  leaving a bare `get_goal {}` line with nothing under it. The result event, the
  checkpoint and the next request now all carry the committed result — a goal
  read shows the goal snapshot, a mutation shows `goal → …` — and a call with no
  arguments no longer trails a `{}` that says nothing.

## [0.1.7] - 2026-09-21

### Fixed

- Session files no longer grow with every checkpoint: each checkpoint used to
  append the whole snapshot again, which wrote a 64 MB log for one README task.
  A log is now one header, one title, one anchor and the deltas appended after
  it: a delta carries only the divergent tail of items/requests plus the small
  fields wholesale, and every 64 deltas (or when a single delta dwarfs the
  anchor) the writer re-anchors through a synced temp file and an atomic rename,
  so a failed rewrite never damages the previous state. Loading folds the deltas
  onto the last anchor and validates the result, leaving resume semantics and
  every caller signature unchanged.
- Recovery and switching stop losing history: a session's write lock is taken
  before it is restored, so a second process cannot displace a session that is
  being written — it waits for the lock and then reads the latest snapshot; a
  resume intent recorded while no key was set survives until `/login`, after
  which the latest history and title are still there. A failed switch keeps the
  UI on the old session: view, draft and queue stay put, the switch is committed
  only once the driver confirms the new session is ready, and a prompt meant for
  the old session can no longer land in the new one.
- Workspace storage partitions by the SHA-256 digest of the canonical path, so
  different paths whose names collide no longer share a directory; legacy
  directories stay resumable in place after checking the recorded `cwd`, and
  they keep the same lock file.
- Configuration a session carries (model options, system prompt) is written with
  the checkpoint and survives replay; every folded delta is validated on load, so
  a damaged record is rejected instead of being read as ordinary history.

## [0.1.6] - 2026-09-20

### Added

- Skills: drop a markdown instruction document into the workspace's `.agents/skills/`
  directory (`<name>.md` or `<name>/SKILL.md`) and `/skill` can run it. Running
  `/name [args]` — typed by hand or sent through `/skill` — makes the agent inject
  that file's body into the turn; the body is an ordinary user message that
  compacts with the history and lands in the snapshot, while the session title
  still takes the command line you typed. Skills never join the `/` menu: that
  column is the builtin command table, and skills appear only under `/skill`.
- `/skill <name> [args]` invokes a skill by name, which also works when a
  builtin (`/plan`, say) shadows that name. There is no separate listing
  command, and no skill row in the `/` menu: a space after `/skill` opens the
  whole catalog as candidates (name, argument hint, description, source file),
  filtering as you type, and Tab only completes. Picking a skill that declares
  an argument hint waits for the argument; picking one that takes none sends it.
  With no argument `/skill` opens that same listing, and with no skills it names
  `.agents/skills/` instead of showing nothing.
- The model side gains a `skill` tool: the request prefix carries a skill index,
  so the model can read one skill's body on demand, or list them all when called
  without a name. One workspace holds at most 128 skills, they are read from the
  workspace's `.agents/skills/` only — never from the aby home — and they are
  scanned once at startup, so adding a file takes a new session.
- Skill bodies are capped at 64 KiB, the budget the AGENTS.md baseline gets per
  request. Over the cap a body is cut on a char boundary with a closing
  `[Truncated: …]` line and one warning in the UI instead of being skipped: the
  skill still runs, and the dropped tail is visible to the author and the model.

## [0.1.5] - 2026-09-20

### Added

- The launch splash now carries four facts under the ASCII wordmark and the
  project URL: build version, working directory, permission preset and model
  (with its effort when one is set). Labels follow the interface language; the
  values do not.
- The meta row's `↓ N` (N = lines above the bottom) is a button: one click
  returns to the newest line and follows the tail again, and the pointer
  resting on it brightens it. It only exists while you are scrolled up.
- `/resume` lists sessions while a turn is running: the listing is a pure store
  read and rides the driver's query channel instead of queuing behind the
  active turn. Picking one still loads it after that turn ends, which the tip
  now says ("(after this turn)"). `/model`'s live catalog shares that channel,
  so it fills the picker mid-turn too (and follows a `/login` key rotation).

### Fixed

- `/resume` during a running task waited for the turn to end: the listing sat
  in the driver's single command queue, so the picker never opened until the
  turn was over. The same queue held `/model`'s live catalog back.

### Changed

- The README is down to one section in both languages: quick start (install, key,
  `/login`, options, environment variables, `settings.json`). The
  minimal/single-file, commands, keys, turn-budget, technical-note and license
  sections are gone — commands, keys, features and the crate table already live
  on abylab.ai, and the TECH pair keeps the internals. Cross-references that
  pointed at the dropped README sections and anchors now point at TECH or the
  site.
- Quick start no longer explains the install mechanics either: the prebuilt
  platform matrix, `SHA256SUMS` check, install directory, PATH handling, musl
  static linking and source builds are gone, leaving one install line (those
  options live in the site's hint and `install.sh --help`).
- Chinese/English parity: the Chinese README gained the options table, the
  environment variables and `settings.json`; the English README gained the
  "ask your AI to install it" line. The two site pages were checked to be
  structurally identical.
- Client-owned chrome now follows `/lang` everywhere: timeline notices and the
  tool-card footer, subagent/agent rails, the plan chip, permission-preset
  meanings and the `· current`/`· default` markers, slash-argument hints,
  command feedback and state notes, the attachment preview, the approval card's
  title, and the too-small-terminal notice.

## [0.1.4] - 2026-09-20

### Changed

- Linux ships one kind of asset: a single musl static binary per architecture,
  so there is no glibc build and no glibc floor to maintain. The binaries are
  built natively per architecture inside an Alpine container.
- Two more composer input rows and a quieter meta row: modes and model stay
  readable without crowding the draft.

### Fixed

- `scripts/check-glibc-floor.sh`'s objdump fallback answers the interpreter
  question honestly, so static artifacts are no longer misreported.

## [0.1.3] - 2026-09-20

### Added

- The startup splash (ASCII wordmark over the project URL).
- `/status`, `/help` and `/keys` are dialogs floating over the conversation —
  they never land in the transcript.

### Changed

- Quieter chrome around the composer: modes, model and state stop competing
  with the draft.
- Linux binaries are built on bullseye with `scripts/check-glibc-floor.sh`
  asserting the floor, and releases mirror the tarballs onto
  abylab.ai/downloads, which the installer now prefers.
- `https://abylab.ai/install.sh` is served by the site itself instead of
  bouncing through GitHub.

## [0.1.2] - 2026-09-20

### Added

- The `@` file browser: Tab on a directory drills into it, typing filters, and
  heavyweight trees (`.git`, `node_modules`, …) are skipped.
- The `todo_write` checklist on the composer's cap row with its progress chip —
  click it for the full list.
- The project path in the composer row now carries `:branch`, read straight
  from `.git/HEAD` (no git process).
- The abylab.ai site (Pico CSS on Cloudflare Pages), and the docs split into
  README/TECH pairs in both languages.

### Changed

- A fresh install starts on the One palette, dark.
- The installer puts its directory on PATH in your shell startup file
  (`--no-path` only prints it) and points users at `/login` for the key.

### Fixed

- Turns interrupted by a timeout can be resumed: unsettled tool calls are
  marked unverified and the next message continues the same turn.

## [0.1.1] - 2026-09-19

### Fixed

- The publish job never checked out the repo, so no release was ever published.
- README wording and layout.

## [0.1.0] - 2026-09-19

First release: the abycore agent SDK, the abylab-backend driver and the
abylab-tui canvas (composer card, markdown, palettes, `@` file mentions, image
thumbnails), plus the one-line installer and the release pipeline.

[Unreleased]: https://github.com/zhang0098/abylab/compare/v0.1.18...HEAD
[0.1.18]: https://github.com/zhang0098/abylab/compare/v0.1.17...v0.1.18
[0.1.17]: https://github.com/zhang0098/abylab/compare/v0.1.16...v0.1.17
[0.1.16]: https://github.com/zhang0098/abylab/compare/v0.1.15...v0.1.16
[0.1.15]: https://github.com/zhang0098/abylab/compare/v0.1.14...v0.1.15
[0.1.14]: https://github.com/zhang0098/abylab/compare/v0.1.13...v0.1.14
[0.1.13]: https://github.com/zhang0098/abylab/compare/v0.1.12...v0.1.13
[0.1.12]: https://github.com/zhang0098/abylab/compare/v0.1.11...v0.1.12
[0.1.11]: https://github.com/zhang0098/abylab/compare/v0.1.10...v0.1.11
[0.1.10]: https://github.com/zhang0098/abylab/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/zhang0098/abylab/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/zhang0098/abylab/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/zhang0098/abylab/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/zhang0098/abylab/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/zhang0098/abylab/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/zhang0098/abylab/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/zhang0098/abylab/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/zhang0098/abylab/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/zhang0098/abylab/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/zhang0098/abylab/releases/tag/v0.1.0
