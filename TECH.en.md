**English** · [中文](TECH.md)

# abylab technical notes

Internals only — install, keys and commands live in [README.en.md](README.en.md).

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

