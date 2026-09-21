**中文** · [English](README.en.md) · [更新日志](CHANGELOG.md) · [abylab.ai](https://abylab.ai/)

# abylab

abylab 是个极简主义的 DeepSeek harness —— 终端里的 ratatui 画布，架在 abycore
的 DeepSeek SDK 上。所有东西都在同一个进程里跑：没有独立的 runtime 进程，没有
ACP，没有插件，没有 demo 模式。

## 快速开始

1. 安装（支持 Linux，macOS），在你的终端中运行：

```sh
/bin/bash -c "$(curl -fsSL https://abylab.ai/install.sh)"
```

你可以直接让你的 ai 帮你安装。

2. 去 DeepSeek 官网取一个 API Key：[https://platform.deepseek.com/](https://platform.deepseek.com/)

3. 在你的终端中输入 `abylab`；然后在程序中用 `/login` 命令输入你的 API Key
（存下就生效，不用重启）：

```
/login sk-********
```

key 写到 `~/.abylab/.credentials.yaml`（0600，仅属主可读）。凭据永远不从环境变量读：
abylab 不读 `DEEPSEEK_API_KEY`，也不读任何别的变量；`--api-key <key>` 只是本次运行的
覆盖项，永远不会落盘。

4. 任意输入你的命令就开始工作了。

你的偏好存在 `$ABYLAB_HOME/settings.json`：界面语言（默认中文）、模型、推理强度、
权限预设，以及外观（明暗 + 主题包）。命令行开关只覆盖本次运行。

## 按键

enter 发送 / 排队 · ctrl+enter 立即发送 · alt+↑（mac ⌥↑）编辑排队消息 ·
esc 中断 / 双击清空草稿 · ctrl+c 清空 / 退出 · ↑ 历史 · `/` 命令 · `@` 文件提及 ·
ctrl+z 撤销 · ctrl+p 模型 · ctrl+t 主题 · shift+tab 权限预设

`alt+↑` 只在有排队消息、草稿为空时打开列表：`↑/↓` 选择 · `enter` 编辑 ·
`ctrl+enter` 把这条插话进正在跑的轮次 · `ctrl+d` 删除（同一行按两次）。
完整映射见 TUI 里的 `/keys`。

## 许可与致谢

版权与许可：[MIT License](LICENSE)（© 2025 zhang@qimiao.org）。

本项目参考了两个项目：[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)
与 [pi.dev](https://pi.dev/)。
