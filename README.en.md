<p align="center">
  <img src="docs/assets/readme-banner.svg" alt="abylab — a free, open-source, minimalist AI assistant" width="100%">
</p>

<p align="center">
  <strong>Your ideas. Your terminal. Let's build.</strong><br>
  One executable to write code, solve problems, and get things done with AI.
</p>

<p align="center">
  <strong>English</strong> · <a href="README.md">中文</a> · <a href="https://abylab.ai/en/">Website</a> · <a href="CHANGELOG.en.md">Changelog</a> · <a href="TECH.en.md">Technical docs</a>
</p>

---

abylab is a free, open-source, minimalist AI assistant: use it to write code, edit Excel files,
write reports, or adjust your computer's settings. A DeepSeek API key is all it takes to get to work.

- **Small** — One executable, ready to run.
- **Familiar** — A native terminal UI, file mentions, and themes.
- **Yours** — MIT licensed: read, modify, and build it yourself.

## Quick start

### 1 · Install abylab

Run the line below in your terminal:

```sh
curl -fsSL https://abylab.ai/install.sh | bash
```

Linux · macOS · installs to `~/.local/bin/abylab`

### 2 · Bring your API key

Grab an API key from [platform.deepseek.com](https://platform.deepseek.com/).

### 3 · Launch and log in

Run `abylab` in your terminal, then log in once from the composer:

```text
/login sk-xxxxxxxx
```

### 4 · Get to work

Type what you want done, and get to work.

> Building from source: `cargo build --release`. The installer also takes `--bin-dir`, `--version`, `--no-path`, `--path-file`, and `--help`.
>
> To uninstall abylab, run `abylab --uninstall`.

## Stay in your flow

| Key | Action |
| :--- | :--- |
| `enter` | Send / queue |
| `ctrl+enter` | Steer |
| `esc` twice | Interrupt / clear draft |
| `ctrl+c` | Clear / quit |
| `↑` | Prompt history |
| `ctrl+z` | Undo |
| `/` | Commands |
| `@` | File mention |
| `ctrl+p` | Switch model |
| `ctrl+t` | Switch theme |
| `alt+↑` (mac `⌥↑`) | Edit queued prompt |
| `shift+tab` | Switch permission preset |

The full key map lives in `/keys`; command help is in `/help`.

<details>
<summary><strong>Queued prompts and interactive questions</strong></summary>

`alt+↑` opens the list only when prompts are waiting and the draft is empty:
`↑/↓` select · `enter` edit · `ctrl+enter` steer that one into the running turn ·
`ctrl+d` delete it (twice on the same row).

During a task, abylab can pause to ask a question. Choose Yes/No, one or several
options, or type a custom answer. Use `↑/↓` to move, space to toggle multiple
choices, `enter` to answer, and `esc` to cancel the question.

</details>

<details>
<summary><strong>Preferences and credentials</strong></summary>

`/login` takes effect without a restart. The API key is written to
`~/.abylab/.credentials.yaml` (0600, owner-only). Credentials never come from the
environment: abylab does not read `DEEPSEEK_API_KEY` or any other credential
variable, and `--api-key <key>` is only a one-run override that is never persisted.

Your preferences live in `$ABYLAB_HOME/settings.json`: interface language
(Chinese by default), model, reasoning effort, permission preset, and
appearance (mode + palette). Flags override them for that run only.

</details>

## License and credits

[MIT License](LICENSE) · © 2025 zhang@qimiao.org

Built with reference to [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
and [pi.dev](https://pi.dev/).
