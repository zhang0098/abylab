**English** · [中文](TECH.md)

# abylab technical notes

Internals only — install is in [README.en.md](README.en.md); commands, keys and
size numbers are on [abylab.ai](https://abylab.ai/).

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

## Skills

A skill is an instruction document you write yourself. The workspace is the
only root: `<workspace>/.agents/skills/`. Skills travel with the project they belong to
and never leak between projects, so nothing looks in the aby home.

A skill is either `<name>.md` or `<name>/SKILL.md`; the file name (the directory
name in the directory form) is the skill name, and it must be 1-64 characters of
`[A-Za-z0-9-_]` starting with a letter or digit. The optional `---` block at the
top is frontmatter: abylab reads `name`, `description` and `input-hint` (other
keys are kept but unused, `tools:` for instance), and falls back to the first
body line (minus markdown decoration) when `description` is missing. The parser
knows exactly enough YAML: one `key: value` per line plus the block scalar forms
`>` / `>-` / `|` / `|-`, which is how long descriptions are usually written — a
folded `>-` becomes one line rather than the literal string `>-`.

Bodies are capped at 64 KiB — the same budget the AGENTS.md baseline gets per
request, which a skill spends only when it is invoked. An oversized body is cut
on a char boundary, gets a closing `[Truncated: …]` line and one warning in the
UI: the skill still runs, and both the author and the model can see that the end
was dropped. An invalid name, more than 128 skills in a workspace, and two files
for one name are each skipped with the reason surfaced. Frontmatter without a
closing `---` stays body text, so a typo does not make the document disappear.

Only that one directory level is scanned: non-`.md` files, entries whose name
starts with a dot, and subdirectories without a `SKILL.md` are not skills, and
nothing recurses deeper. Two files for one name is a mistake: the first in path
order wins and the other is reported.

Skills are scanned once, at driver startup, and that one snapshot serves the
whole session: the `/skill` listing and its candidates, the `skill` tool and the
`/<name>` injection all read it. Adding a file takes a new session.

How they behave:

- `/name [args]` still ships as a prompt; the driver injects the body where the
  prompt arrives. The message opens with the line you typed, then a
  `<system-reminder>` (skill name, file path, the argument note, and the
  statement that the skill does not override system, developer or direct user
  instructions), then the body. The session title takes the first line of that
  message, so a resumed session still shows the command rather than the body.
- The body is an ordinary user message: it compacts with the history and lands
  in the snapshot.
- The request prefix carries a skill index (name + description, at most 32), sent
  every turn like the AGENTS.md baseline. That is how the model knows which
  skills exist; it can read one on demand with the `skill` tool (called with no
  name it lists them all). Children inherit the parent's request prefix and the
  `skill` tool, so a subagent sees the same skill set.
- The `skill` tool only reads text discovery already loaded, so no permission
  preset asks for approval.
- Builtins keep their precedence: a line like `//` or `/usr/bin` is never
  treated as a skill, and a skill never displaces a same-named builtin — a
  hand-typed `/<name>` reaches the builtin, so `/skill <name> [args]` is the way
  through; it resolves the name and ships `/<name> [args]` all the same.
- Skills never join the `/` menu: that column is the builtin command table.
  Skill names are whatever the user typed into a directory, and a catalog of
  them would bury the commands, so skill rows exist only under `/skill`.
- `/skill` is the one entry point — there is no separate listing command. Its
  candidate list is the catalog: the space opens every skill's row (name,
  argument hint, description), typing filters by prefix, and Tab completes
  without sending. Picking a skill that declares an `input-hint` only completes
  the line to `/skill <name> ` and leaves the argument to you; picking one that
  takes no arguments sends it.

With no skills in the workspace, `/skill` with no argument names the
`.agents/skills` directory; with none at all, the `skill` tool is not
registered, so no tool definition is paid for.

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
  share the run's cancellation and request budget, and honor the summary output
  cap; a summary is an ordinary request, so the transport's own timeouts bound
  it.
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
  summary failure leaves the view unchanged; cancellation, a request reaching
  its own deadline, and request-budget exhaustion stop the run. Automatic view changes pass a
  persistence barrier before the next model request.

Defaults: `ABY_COMPACT_AT=0.8`, `ABY_KEEP_RECENT=0.16`, `ABY_PRUNE_TOOL_OUTPUT=4096`,
summary cap 8192 output tokens. Pressure is priced with a valid provider
measurement when there is one, otherwise estimated at 3 bytes per token. A view
change invalidates the old measurement; restoring an already-compacted session
conservatively waits for a new one.

There is exactly one time limit, and it is a backstop rather than a budget:
`ABY_TOOL_TIMEOUT` (60 seconds by default) for a tool call that **declares no
budget of its own**.

There is no turn-level timeout. A turn does not exist on DeepSeek's side — the
API sees stateless requests one at a time while tools run locally — so "how long
this turn has been running" was never its rule, and it is not abylab's either:
harness's agent loop has no deadline over a step, and abylab sets none. What can
actually get stuck each has an owner:

- Every request belongs to the transport: 15s connect, 120s first byte, 60s
  stream idle (`ClientConfig`). The provider can only affect a *single* request —
  `max_tokens` truncation (a number we send), `Retry-After` on a 429, 5xx, or a
  gateway cutting one stream — and each of those becomes one failed request that
  `ABY_AUTO_CONTINUE` resumes inside the same turn.
- Every tool call belongs to its own budget: a `bash` call carrying `timeoutMs`
  runs under it (`bash_max_timeout` caps it, 600 seconds by default), a foreground
  delegation or `wait_agent` has no timer at all — the child turn ends on its own,
  so the wait simply lasts that long. Tools that declare nothing (`read`, `write`,
  `edit`) get `ABY_TOOL_TIMEOUT`.
- A runaway loop belongs to `ABY_MAX_REQUESTS` / `ABY_MAX_TOOL_CALLS` (1000 each
  by default, `0` for no cap).
- The user can press Esc at any point: it interrupts both execution and the wait
  between segments.

When a call reaches its budget it is asked to stop, gets its own `cleanup_grace`
to settle, and answers with a tool error the model can read (saying it was
stopped and that its result is unverified); the turn continues — the same answer
`dsh-tool-call-timeout-policy` gives, instead of failing the turn. A request or
tool call reaching its own deadline waits one second and continues the same open
turn, sharing the `ABY_AUTO_CONTINUE` allowance with budget stops and transient
failures (default 3; `0` stops on the first failure). Completed tool results
survive; a tool truncated mid-execution is marked "unverified — check before
repeating" and never replayed. Only once that allowance is spent does the error
surface, listing the current limits, and another message continues the session.
(These budgets and limits are abylab's own backstops, not DeepSeek limits —
usage is billed as usual.)

A host that wants a run which stops moving to fail on its own still has the
window (`RunOptions::timeout: Option<Duration>`, `TurnLimits::run_timeout`): it
measures a **lack of progress** — no bytes arriving, no request or tool call
finishing, no committed step, with every stream chunk and committed tool result
re-arming it. abylab's own front end sets none, because leaving your seat for ten
minutes should not turn an open permission ask into an error. Serialized input
has its own 4 MiB cap. The byte estimate is not a guaranteed upper bound.

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
- `/resume` lists the workspace store (newest first). The listing is a pure
  read and rides the driver's query channel, so the picker opens while a turn
  is still running; the load itself (`session/load`) waits for that turn to
  end. `/model`'s live catalog (the `/models` request) takes the same channel,
  so it fills the picker mid-turn too.
- A non-blocking `flock` on `session.lock` keeps it to a single writer.
- `/delete` permanently removes the active session: the driver drops its writer,
  `SessionStore::delete` takes the same `session.lock` (so a session another
  process holds open is refused) and removes the whole session directory; the
  queued-prompts file (`$ABYLAB_HOME/queued/…`) goes with it, and the driver
  mints a new id for the replacement session — the deleted log is never
  resurrected by the next message.

Changing permissions or the API key at runtime keeps the parent's runtime
identity, inbox and session writer. Existing children keep the credentials and
permissions they were delegated. Child callbacks own separate compaction and
repeat-reminder state, use the configured request/tool/time limits, and can't
hold on to the parent's session writer after a session switch. On exit, abylab
waits for subagents before shutting down local tools. Host goal changes and the
final run state (including incomplete/error exits) are saved before their settled
state is announced.

## Goal rounds

One durable objective per session, unlimited by default unless a round allowance is specified:

- `/goal <objective>` creates the goal and spends round 1 right away.
  `/goal @<n> <objective>` sets a positive total allowance, without a fixed
  100-round cap. Numeric allowances in older saved sessions remain in effect.
- `/goal status` prints it; `/goal pause|resume|complete|clear` controls it.
  Status and pause use a control channel serviced during execution. Status
  costs no model request; pause cancels the current turn and acknowledges the
  saved paused state after cleanup, without waiting for every round to finish.
- `/goal @<n> resume` sets the total allowance and continues the same goal,
  preserving its identity and spent rounds. `/goal rounds <n>` only changes
  the allowance; `/goal rounds off` removes it. Neither starts execution.
  A bare resume fails with an actionable message if the allowance is exhausted.
- Each round is one ordinary turn seeded with the objective; the model ends the
  loop by calling `update_goal` with `complete` or `blocked` (it has to quote the
  id and revision from `get_goal`, so a stale model can't overwrite a goal that
  moved on). Exhausting an explicit allowance stops the goal as blocked.
  Esc saves an active goal as paused; errors stop it as blocked and incomplete
  responses stop it as paused. Ordinary messages do not resume stopped goals.
- Continuation is armed by *you*: a goal the model creates through its tools
  starts paused and waits for `/goal resume`; the model cannot set it active.
  Restoring a session also saves previously active goals as paused before
  binding the session. The SDK never starts a turn on its own; the driver here does.


## Steering

While a turn is running, your next message takes one of two paths: queue, or
steer. `/enter` picks which one plain Enter takes and ctrl+enter always takes
the other; an idle session sends either way. This is deepseek-harness's
busy-Enter preference (the `ui-conversation` `resolveSubmitMode` policy), and
the default matches: queue.

A steer **cancels nothing**:

- The message enters the running agent's inbox (abycore `Agent::steer_handle`).
  The SDK drains that inbox before building each request — the boundary right
  after a completed tool batch — and appends the text as a user message. When
  the model had already finished its last step, the run is pulled back for one
  more step instead of a new turn being opened.
- The in-flight step's streamed output and its completed tool results all
  survive; esc remains the only interrupt (it rides the cancellation token on a
  separate channel from steering).
- Steering is best-effort: a message that misses the window (the turn ended
  first) is not a failure, it is delivered as the next waking turn. With no
  running turn at all (no `/login`, say) the driver answers
  `SteerSettled{deferred}` and the UI returns the item to the client queue with
  its queued tint back.
- An empty draft plus ctrl+enter steers every queued message, in FIFO order.
  Without a running turn the head goes out as an ordinary prompt and the rest
  stay queued.

A steer is visible in three states: the bubble carries a `steering` marker from
ctrl+enter, the inbox drain that spends it fires a checkpoint, and the driver
reports `SteerAdmitted`, which clears the marker; a rejected or late steer goes
back to `queued`. Messages the turn has not taken yet are painted as a tail
section (`⏳ N steering`) — harness's pending-steering rows: they belong after
the live output, not in the middle of the step they are about to interrupt, and
they drop back into their chronological slot once the agent takes them.

## The queue

The queue lives in the **driver**, not in a client: `drive` owns a
`VecDeque<QueuedItem>` and publishes the whole list through `CtlEvent::Queue`
on every change, so every client (the TUI today, a second process or a web UI
later) renders the same FIFO. Each row carries a `placement`: `Queued` (waiting
for the turn in flight to end) or `Steering` (the running turn already has it).

- Delivery order is decided in `next_command`: pending **commands first** (a
  removal, an edit, a new steer), then rows the turn took (`settle_taken`), and
  only then is the head shipped as the next prompt. A removal can therefore
  never lose a race with the boundary that drains the row it changes.
- Steering rides its own channel (`DriverHandle::steer`), checks the session
  identity, and takes the message at the next step boundary. Queue additions,
  edits and removals are processed and saved during running turns, questions,
  retries and compaction. Ordinary commands, including session switches, wait for
  that work to finish. The `⌥↑` list's enter / ctrl+enter / ctrl+d just send commands.
- Taken ids stay in a tombstone set: a client's "queue it, then steer it" pair
  can be in flight at once, and the late queue command must not turn a delivered
  row back into a queued one.
- The queue is process memory while the session snapshot is durable, so every
  mutation writes `$ABYLAB_HOME/queued/<session>.json` (one file per session,
  removed when the queue drains). A start or `/resume` reads it back as `held`
  rows: they were queued behind a turn that no longer exists, so they are not
  spent unbidden — the first turn this process actually runs releases them, and
  they ship in FIFO order after it. A missing, malformed or differently shaped
  file is just an empty queue, exactly like `settings.json`.

## Uninstall (`abylab --uninstall`)

`--uninstall` is the one maintenance command, and it is a CLI entry only (there
is no `/uninstall`: the thing being deleted is the program itself, so a TUI
still on screen would be pointless; there is no separate `/reset` either —
clearing the data is just its first question). One plan, two halves: the data
half lives in `data.rs` (`data::scan` / `data::wipe`), the program half in
`uninstall.rs` (`uninstall::scan` / `uninstall::wipe`).

- **The data half is a whitelist, not a directory scan**: `settings.json`,
  `abylab-modes.json`, `.credentials.yaml`, `sessions/`, `queued/`, plus the
  session store `--session-root` points at when it lies inside the home (the
  default `sessions/` is one of them; entries are deduped by path). Anything
  else in the home is not on it — and two cases are named as kept instead of
  silently skipped: a hand-written `AGENTS.md` (the workspace instructions
  abycore reads out of the home, which belongs to the user) and a session store
  `--session-root` put outside the home (the command cleans ABYHOME). The home
  itself is never a target, not even when someone points `--session-root` at
  it — that path is reported as kept, with the reason. Ownership checks resolve
  `..` and parent symlinks. Deletion uses the home directory handle opened by
  the scan, so replacing a parent with a symlink cannot redirect it outside.
  A final symlink is unlinked without deleting its target.
- **Measure first, then execute that list**: `scan` counts files and bytes with
  `lstat` (a symlink counts as the link itself, which is what `remove_dir_all`
  will unlink; one entry walks at most `MAX_WALK` files and then reads `N+`, so
  a directory someone filled by hand cannot stall the plan). The CLI prints
  exactly that measurement and `wipe` deletes exactly it — what was shown is
  what goes. A path that refuses to go is reported per path; one that vanished
  since the scan is not a failure.
- **The program list**: `std::env::current_exe` (so a binary started outside
  `$PATH` still finds itself), the `abylab` in every `$PATH` directory,
  `~/.local/bin/abylab`, and `$ABYLAB_BIN_DIR/abylab`. A candidate has to still
  be called `abylab` (a renamed file is left alone); a symlink enters the list
  together with the file it points at, and `link_target` is bounded to 20 hops
  so a link loop cannot hang. A path another package manager owns
  (`/opt/homebrew`, `/homebrew`, any `/Cellar/`, `/nix/store`,
  `/.nix-profile/`) is reported as kept. Copies in `$CARGO_HOME/bin` (default
  `~/.cargo/bin`) are marked as cargo's: the batch goes through
  `cargo uninstall abylab-tui` so the entry in `~/.cargo/.crates.toml` goes
  with it; when cargo is absent or fails, the files are unlinked directly and
  the report says cargo's list may still name the package.
- **Two halves, two questions**: the plan rows (`remove` / `keep`) cover the
  data and the program together, but the prompt asks twice — data first,
  program second. "Keep the data, remove the program" is therefore an answer
  the user gives on purpose, not a data wipe followed by hand-deleting a
  binary. `--keep-data` fixes the data answer to no, `--yes` fixes both answers
  to yes; the CLI asks `[y/N]` on `/dev/tty` (never on stdin, which may be
  carrying something else), and with no terminal to ask on the command errors
  out, never defaulting to yes. Two noes change nothing.
- **Shell startup files are not touched**: the PATH line install.sh added stays
  where it is; deleting the binary does not edit an rc file — that file belongs
  to the user, and the command is done once the binaries are gone.
- **Another abylab still running** is called out in the plan — it writes
  `settings.json` back on its next preference change, so a half-cleaned home
  would grow back.
- **What is deleted is the running program**: on Unix unlinking a live image is
  fine; the process finishes writing its report and exits. Both shipped targets
  (Linux musl, macOS) are in that world. In tests the directories and
  `current_exe` all come in through `ScanInput`, so a test run can never touch a
  real installation.

## Packaging and releases

Linux ships one kind of asset: a single musl static binary per architecture
(`x86_64`, `aarch64`) that needs no libc from the host, so the same download
runs on an old glibc distribution and on Alpine, and there is no glibc floor to
maintain. The binaries are built natively per architecture inside an Alpine
container with its musl gcc (see `.github/workflows/release.yml`), and
`scripts/check-glibc-floor.sh --static` asserts the result has no dynamic
section, no `PT_INTERP` and no `GLIBC_` symbol.

Static is not self-contained: two things still come from the host, which
matters on trimmed images.

- TLS roots, probed per distribution by `rustls-platform-verifier`:
  `/etc/ssl/certs/ca-certificates.crt` (Debian/Ubuntu),
  `/etc/pki/tls/certs/ca-bundle.crt` (RHEL family), `/etc/ssl/cert.pem`
  (Alpine) and friends, with `SSL_CERT_FILE`/`SSL_CERT_DIR` as overrides.
  A distroless or scratch image with no CA bundle fails the TLS handshake.
- DNS resolution uses the host's configuration (`/etc/hosts`,
  `/etc/resolv.conf`): the static binary carries our libc, but resolving is
  still the system's job.
