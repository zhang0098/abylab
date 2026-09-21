**English** · [中文](CHANGELOG.md)

# Changelog

What each abylab release changed, from the user's side. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions follow
[Semantic Versioning](https://semver.org/). Internals (workspace instructions,
context compaction, session persistence, goal rounds) live in
[TECH.en.md](TECH.en.md); day-to-day usage is in [README.en.md](README.en.md).

## [Unreleased]

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

[Unreleased]: https://github.com/zhang0098/abylab/compare/v0.1.7...HEAD
[0.1.7]: https://github.com/zhang0098/abylab/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/zhang0098/abylab/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/zhang0098/abylab/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/zhang0098/abylab/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/zhang0098/abylab/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/zhang0098/abylab/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/zhang0098/abylab/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/zhang0098/abylab/releases/tag/v0.1.0
