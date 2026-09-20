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
