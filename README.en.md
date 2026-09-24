**English** · [中文](README.md) · [Changelog](CHANGELOG.en.md) · [abylab.ai](https://abylab.ai/)

# abylab

abycore agent harness in your terminal — a ratatui canvas over the abycore
DeepSeek SDK. Everything runs in one process: no separate runtime process, no
ACP, no plugins, no demo mode.

## Quick start

```sh
curl -fsSL https://abylab.ai/install.sh | bash
abylab
```

You can also just ask your AI to install it.

Then store your key (from <https://platform.deepseek.com/> → API keys) by
typing this into the composer — it takes effect without a restart:

```
/login sk-xxxxxxxx
```

It is written to `~/.abylab/.credentials.yaml` (0600, owner-only). Credentials
never come from the environment: abylab does not read `DEEPSEEK_API_KEY` or any
other variable, and `--api-key <key>` is only a one-run override that is never
persisted.

Your preferences live in `$ABYLAB_HOME/settings.json`: interface language
(Chinese by default), model, reasoning effort, permission preset, and
appearance (mode + palette). Flags override them for that run only.

To uninstall abylab, run `abylab --uninstall`.

## Keys

enter send / queue · ctrl+enter send now · alt+↑ (mac ⌥↑) edit queued prompt ·
esc 2× interrupt / clear draft · ctrl+c clear / quit · ↑ history · `/` commands ·
`@` file mention · ctrl+z undo · ctrl+p model · ctrl+t theme · shift+tab permission preset

`alt+↑` opens the list only when prompts are waiting and the draft is empty:
`↑/↓` select · `enter` edit · `ctrl+enter` steer that one into the running turn ·
`ctrl+d` delete it (twice on the same row). The full map lives in `/keys`.

During a task, abylab can pause to ask a question. Choose Yes/No, one or several
options, or type a custom answer. Use `↑/↓` to move, space to toggle multiple
choices, `enter` to answer, and `esc` to cancel the question.

## License and credits

Copyright and license: [MIT License](LICENSE) (© 2025 zhang@qimiao.org).

The project was built with reference to two projects:
[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) and
[pi.dev](https://pi.dev/).
