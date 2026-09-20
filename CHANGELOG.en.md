**English** · [中文](CHANGELOG.md)

# Changelog

What each abylab release changed, from the user's side. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the versions follow
[Semantic Versioning](https://semver.org/). Internals (workspace instructions,
context compaction, session persistence, goal rounds) live in
[TECH.en.md](TECH.en.md); day-to-day usage is in [README.en.md](README.en.md).

## [Unreleased]

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

[Unreleased]: https://github.com/zhang0098/abylab/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/zhang0098/abylab/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/zhang0098/abylab/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/zhang0098/abylab/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/zhang0098/abylab/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/zhang0098/abylab/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/zhang0098/abylab/releases/tag/v0.1.0
