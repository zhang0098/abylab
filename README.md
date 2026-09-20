**中文** · [English](README.en.md) · [更新日志](CHANGELOG.md) · [abylab.ai](https://abylab.ai/)

# abylab

abylab 是个极简主义的 DeepSeek harness —— 终端里的 ratatui 画布，架在 abycore
的 DeepSeek SDK 上。所有东西都在同一个进程里跑：没有独立的 runtime 进程，没有
ACP，没有插件，没有 demo 模式。

```
crates/abycore           agent SDK（工作区成员）：Agent、会话快照、工具
crates/abylab-backend    abycore Agent 驱动：Cmd ⇄ Agent::run、hooks → 权限询问
crates/abylab-tui        画布：输入卡片、markdown、主题包、@文件、图片缩略图
```

## 极简、单文件、快

装完就是一个可执行文件：没有 Node、没有 Python、没有依赖目录、没有后台进程。
x86_64 Linux 上的发布构建（默认 LTO + strip）大约 11 MB，压缩包约 4.9 MB，
冷启动按毫秒算（在那台编译机上 `abylab --version` 约 2 ms）。流式输出、工具执行、
会话持久化都发生在同一个进程里：没有 IPC 往返，没有 GC 停顿。

## 快速开始

1. 安装（支持 Linux，macOS），在你的终端中运行：

```sh
/bin/bash -c "$(curl -fsSL https://abylab.ai/install.sh)"
```

预编译的二进制覆盖 Linux（x86_64、aarch64）和 macOS（Intel、Apple Silicon）。
安装脚本会把下载的文件和这个 release 的 `SHA256SUMS` 对一遍，然后装到
`~/.local/bin/abylab`；`--bin-dir` 换安装目录、`--version <tag>` 固定某个版本、
`--mirror <url>` 换下载源，`--help` 看全部选项。它还会把这个目录写进你的 shell
启动文件（也就是 PATH，带注释标记，想撤销就删那一行）；`--no-path` 只打印那一行
不落盘，`--path-file` 指定写到哪个文件。想从源码构建：`cargo build --release`。

预编译的 Linux 二进制用 musl 静态链接，就是一个不依赖宿主 libc 的文件：RHEL 7+、
Amazon Linux 2/2023、Debian/Ubuntu 全系、Alpine 都能直接跑，不用管宿主的 glibc
版本。静态不等于自包含 —— TLS 证书和 DNS 仍然读宿主的配置（见 [TECH.md](TECH.md)）。
想自己编译需要一个 C 编译器（依赖里的 `aws-lc-sys` 会自己编译 BoringSSL）：

```sh
cargo install --git https://github.com/zhang0098/abylab abylab-tui
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

选项：

```
-w, --workspace <dir>     代理工作区（默认：当前目录）
    --session-root <dir>  会话 JSONL 根目录（默认 $ABYLAB_HOME/sessions）
    --session-id <id>     恢复/继续一个持久会话 id
    --model <id>          模型 id（默认 $ABY_MODEL，其次持久化的模型，
                          最后 deepseek-flash）
    --base-url <url>      给代理设置 DEEPSEEK_BASE_URL
    --api-key <key>       只覆盖本次运行的 API key（长期保存用 /login）
    --theme <dark|light>  明暗模式（默认：持久化的值，其次 dark）
```

细调没有开关，只用环境变量：`ABY_MAX_REQUESTS`、`ABY_MAX_TOOL_CALLS`、
`ABY_AUTO_CONTINUE`、`ABY_TURN_TIMEOUT`、`ABY_TOOL_TIMEOUT`、`ABY_CONTEXT_WINDOW`
（开压缩）、`ABY_COMPACT_AT`、`ABY_KEEP_RECENT`、`ABY_PRUNE_TOOL_OUTPUT`，以及
`ABYLAB_HOME` 和 `ABY_MODEL`。

你的偏好存在 `$ABYLAB_HOME/settings.json`：界面语言（默认中文）、模型、推理强度、
权限预设，以及外观（明暗 + 主题包）。命令行开关只覆盖本次运行。

## 命令列表

`/help` · `/keys` · `/new` · `/resume [id]` · `/compact` · `/goal` · `/clear` · `/model [id]` ·
`/effort [off|low|high|max]` · `/permission [preset]` · `/plan [on|off]` ·
`/vim [on|off]` · `/image <path> [text]` · `/clip [text]` · `/theme [dark|light|pack]` ·
`/status` · `/lang [zh|en]` · `/quit`

`/theme` 用来挑调色板包：缺省是 one 的深色（`--theme dark` 只是换明暗，
不动包）；另有内置的 DeepSeek 主题和七个主题（ayu · catppuccin · everforest ·
iceberg · kanagawa · solarized · tomorrow）。
选择器里 ↑/↓ 会就地预览高亮的那一款，`enter` 才应用、`esc` 还原；
`ctrl+t` 在当前包里切换深色/浅色。

`/status`（运行状态、模型、token/轮次/耗时统计，以及会话 id/标题和
凭据来源）是弹窗：浮在对话上、esc 关闭、↑↓/滚轮滚动，
不会写进对话记录 —— 和 `/help`、`/keys` 一样属于界面本身。

## 按键

enter 发送/排队 · ctrl+enter 立即发送 · esc 中断（空闲时清空草稿）·
ctrl+c 清空/退出 · ↑ 历史（空草稿时召回，草稿首行也能拉起）· `/` 命令 · `@` 文件提及 ·
ctrl+z / ctrl+shift+z 撤销/重做 · ctrl+p 模型 · ctrl+t 主题

`/permission` 在 `read-only`、`workspace-write`、`danger-full-access`（默认）
之间切换实时会话。切换保留对话历史，并作用于后续的文件工具与 Bash 进程；
Shift+Tab 循环切换预设。

当前轮次运行时 enter 排的队由客户端持有（不是丢给后端）：`⌥↑` 打开队列选择器
挑一条编辑 —— enter 保存回原位置、ctrl+d 删除（按两次）、esc 取消，三者都不会
把它发出去；编辑期间队列暂停，不会被回合结束抽走。草稿为空时 enter 会把队首
立即送进当前轮次（等于对该条按 ctrl+enter）。

代理用 `todo_write` 维护任务清单时，输入框上沿的提示行会实时显示进行中的
任务和完成进度（`任务 · 进行中: 修复登录 · 2/5 完成`）；操作反馈会短暂借用
这一行，几秒后回到任务进度。点击进度部分（`2/5 完成`）弹出完整清单对话框，
esc 关闭。空闲时这一行留空。

使用提示不常驻输入框：每次新会话（启动、`/new`）在对话区开头给出一条轮换的
提示（esc 中断、ctrl+enter steer、`/status` 里的 token 用量……），下一条会话换成
下一条；完整清单见 `/help` 和 `/keys`。

启动时对话区最上面是 ASCII wordmark 加项目地址 `https://abylab.ai`（居中），
下面紧跟四行启动信息：版本号、工作目录、权限预设、模型（设了 effort 就带在
模型那行后面）；再往下才是那条提示；`/new` 只给提示，不再重复 logo。

提示行右侧显示项目路径；工作区是个 Git 检出时，后面紧跟 `:分支`（冒号紧贴，
如 `/work/acme/abylab:main`）。分支直接读 `.git/HEAD`，不启动 git 进程，每 5 秒
在节拍里复查一次（代理的 shell 工具或另一个终端切了分支，标签会跟着变）；
路径太长或终端太窄时只留路径。

提示行右侧、项目路径后面的 `↥` 可以点击：每次点击往前跳一条你发过的
输入（从最新开始，到最早后回到最新），跳到的输入会滚动到顶部并高亮几秒。
`↥` 右边的 `⛶` 也是纯鼠标按钮：点一下把输入框固定到放大高度（约 5/8 屏），
再点一下回到自动高度。往上翻过对话后，底栏左侧的 `↓ N`（N 是离底部多少行）
同样可以点：一点就翻回最新的行、重新跟随尾部；指针停在上面时它会变亮。

鼠标：滚轮滚动 · 点击工具卡片展开 · 拖动选择，松开复制
（原生工具 → tmux → OSC52）· 输入框里点击定位光标、拖动选择
（`ctrl+x` 剪掉选中）· `@` 打开文件浏览器。

`@` 文件浏览器：输入即过滤（精确 > 前缀 > 包含 > 字母顺序模糊匹配），
当前目录没有匹配就自动跳到子目录里最近的一条（跳过 `.git`、`node_modules`
等重目录，最多三层）；↑↓ 移动 · ← 上级 · →/tab 进入目录 · enter 选择 ·
`ctrl+h` 显示/隐藏点文件 · esc 关闭（同样的文字再打开）。

## 轮次预算

abycore 始终对每次运行有一个请求/工具上限（对话请求、重试和 `web_search` 的
辅助请求都算在内）。abylab 把这些数字当兜底，而不是正常结束一轮的方式：

- `ABY_MAX_REQUESTS` / `ABY_MAX_TOOL_CALLS` 默认各 **1000**。把任意一个设成 `0`
  就没有上限：回合会一直跑到模型自己结束、你按 esc、超时续跑次数用完，或者撞上
  上下文上限。
- 一段因为预算上限、超时（包括整段的截止时间）或临时故障（连接断开、限流、
  服务端错误）停下时，abylab 会把还没结算的工具调用结算成错误结果：执行中被中断
  的标记为“未核实”，并写明重复执行前先检查；已经完成的工具结果留在历史里。它会
  打一条 `info` 提示，提供商给了 `Retry-After` 就等完，然后接着跑同一个未完成的
  回合，最多 `ABY_AUTO_CONTINUE` 次（默认 3）。
- 这些余量用完，回合才以错误结束，会话仍然可续：再发一条消息就从断点接着跑。
- 卡住的循环得到的是提醒，不是打断：同一个工具调用带完全相同的参数连着重复第 3、
  5、8 次时，宿主会给模型排一条简短提醒。调用本身从不被阻塞或延迟，`todo_write`
  豁免，新提示或不同的调用会重新计数。这条护栏在
  `abycore::AgentHooks::tool_reminder`，abylab 通过自己的 hooks 装上。

预算是 abylab 自己的兜底，不是 DeepSeek 的限制 —— 用量照常计费。时限也是 abylab
自己的：`ABY_TURN_TIMEOUT` 默认 **每段 600 秒**，`ABY_TOOL_TIMEOUT` 默认
**每个工具调用 60 秒**。超时后等 1 秒再接着跑同一个未完成的回合，和预算耗尽、
其他重试共用 `ABY_AUTO_CONTINUE` 的次数；把它设成 `0` 就是第一次失败就停。
次数用尽后，错误会列出当前的限额；再发一条消息可以继续，或者改完这些环境变量
重启 abylab。Esc 既能中断执行，也能中断段与段之间的等待。

`incomplete` 指模型撞上了单次请求的输出上限。驱动会报出这个上限，并暂停自动的
目标轮次，等你动手。你的下一条消息会接进这个未完成回合的下一次请求，所以修正
立刻能到模型手里。部分回答留在对话区可见，但永远不会写进可执行的请求视图；被
截断的回答里的工具调用也永远不会执行。想要短一点的回答？直接说。新会话用 SDK 的
输出上限（256,000 token）；恢复的会话沿用当初存下的上限。

## 技术说明

实现细节拆到 [TECH.md](TECH.md) 了:工作区指令(AGENTS.md 的加载顺序与优先级)、
上下文压缩(阈值、裁剪与溢出恢复)、会话持久化(JSONL 布局与锁)、目标轮次(配额
与续跑规则)。日常使用不需要读。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
