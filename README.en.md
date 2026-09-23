**English** · [中文](README.md) · [Changelog](CHANGELOG.en.md) · [abylab.ai](https://abylab.ai/)

# abylab

abycore agent harness in your terminal — a ratatui canvas over the abycore
DeepSeek SDK. Everything runs in one process: no separate runtime process, no
ACP, no plugins, no demo mode.

## Quick start

```sh
/bin/bash -c "$(curl -fsSL https://abylab.ai/install.sh)"
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

To get back to an as-installed state, `/reset` deletes everything this program
saved under `$ABYLAB_HOME` (settings, API key, session logs, queued prompts) and
starts a new session — two Enters confirm it. The same wipe from a shell is
`abylab --reset`. A hand-written `AGENTS.md` and a session store that
`--session-root` put outside the aby home are not part of it.

To take the program itself off the machine, `abylab --uninstall` lists the
saved data, every `abylab` binary and the PATH block the installer wrote into a
shell startup file, then asks about the data and the program separately —
`--keep-data` removes only the program and `--yes` skips both questions. A copy
Homebrew or Nix owns is reported, not removed.

## Keys

enter send / queue · ctrl+enter send now · alt+↑ (mac ⌥↑) edit queued prompt ·
esc interrupt / 2× clear draft · ctrl+c clear / quit · ↑ history · `/` commands ·
`@` file mention · ctrl+z undo · ctrl+p model · ctrl+t theme · shift+tab permission preset

`alt+↑` opens the list only when prompts are waiting and the draft is empty:
`↑/↓` select · `enter` edit · `ctrl+enter` steer that one into the running turn ·
`ctrl+d` delete it (twice on the same row). The full map lives in `/keys`.

## License and credits

Copyright and license: [MIT License](LICENSE) (© 2025 zhang@qimiao.org).

The project was built with reference to two projects:
[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) and
[pi.dev](https://pi.dev/).
