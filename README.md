**中文** · [English](README.en.md)

# abylab

终端里的 abycore agent harness —— 一块 ratatui 画布，驱动 abycore DeepSeek
SDK。所有东西都跑在一个进程里：没有单独的服务进程，没有 ACP，没有插件，
也没有演示模式。

```
crates/abycore           agent SDK（workspace 成员）：Agent、会话快照、工具
crates/abylab-backend    abycore Agent 驱动：Cmd ⇄ Agent::run，hooks → 权限询问
crates/abylab-tui        画布：输入卡片、markdown、调色板、@文件、图片缩略图
```

## 极简、单文件、快

abylab 是个极简主义的 DeepSeek harness：装完就是一个可执行文件，不用 Node
也不用 Python，没有依赖目录，更没有后台进程。x86_64 Linux 上 release 构建
（默认开 LTO + strip）大约 11 MB，压成发布包约 4.9 MB；冷启动也是毫秒级的
（本机实测 `abylab --version` 约 2 ms）。流式渲染、工具执行、会话落盘全在
同一个进程里完成，没有 IPC 往返，也没有 GC 停顿。

## 快速开始

```sh
cargo build --release
export DEEPSEEK_API_KEY=sk-…
./target/release/abylab
```

常用参数：

```
-w, --workspace <dir>     agent 工作区（默认：当前目录）
    --session-root <dir>  会话 JSONL 根目录（默认：$ABYLAB_HOME/sessions）
    --session-id <id>     恢复/继续一个持久会话 id
    --model <id>          模型 id（默认：$ABY_MODEL，其次持久化设置，
                          最后 deepseek-flash）
    --base-url <url>      为 agent 设置 DEEPSEEK_BASE_URL
    --api-key <key>       为 agent 设置 DEEPSEEK_API_KEY
    --theme <dark|light>  外观模式（默认：持久化设置，其次 dark）
```

细调参数没有命令行开关，用环境变量就行：`ABY_MAX_REQUESTS`、
`ABY_MAX_TOOL_CALLS`、`ABY_AUTO_CONTINUE`、`ABY_TURN_TIMEOUT`、
`ABY_TOOL_TIMEOUT`、`ABY_CONTEXT_WINDOW`（启用压缩）、`ABY_COMPACT_AT`、
`ABY_KEEP_RECENT`、`ABY_PRUNE_TOOL_OUTPUT`，另外还有 `ABYLAB_HOME` 和
`ABY_MODEL`。

偏好设置存在 `$ABYLAB_HOME/settings.json`：界面语言（默认中文）、模型、推理
强度、权限预设、外观（明暗模式 + 主题包）。命令行参数只在当次运行里盖过它们。

## 回合预算

abycore 每个轮段都会硬性执行请求/工具上限（对话请求、重试和 `web_search`
辅助请求共用同一份额度）。abylab 把这些数字当兜底，而不是正常的收尾方式：

- `ABY_MAX_REQUESTS` / `ABY_MAX_TOOL_CALLS` 默认都是 **1000**。填 `0` 就完全
  不限：回合会一直跑，直到模型自己收尾、你按 esc、回合时限到了，或者撞上
  上下文上限。
- 如果某个轮段是撞上限停的，或者是瞬时故障（断连、限流、服务端错误）停的，
  abylab 会把还没执行的工具调用结算成错误结果——模型不会以为写入已经落盘
  ——然后打一条 `info` 通知；提供商带了 `Retry-After` 就先等一等，再在同一个
  回合里续跑，最多 `ABY_AUTO_CONTINUE` 次（默认 3）。
- 只有这点续跑额度也用光了，回合才会以错误收场，而会话依然能恢复：再发一条
  消息就接着跑。
- 卡住的循环只会被劝，不会被杀：同一个工具调用、同样的参数连着出现 3、5、8
  次之后，宿主会给模型排队一条简短提醒。调用本身不会被拦也不会被拖慢，
  `todo_write` 不算；新提示词或换一个调用都会重新计数。这道守卫在
  `abycore::AgentHooks::tool_reminder`，由 abylab 通过 hooks 装上。

预算是 abylab 自己的兜底，不是 DeepSeek 的限制——用量照常计费。时限也一样：
预算都兜住之后，真正卡时间的是 `ABY_TURN_TIMEOUT`（回合仍可恢复，发条消息
就能继续），单个工具调用则由 `ABY_TOOL_TIMEOUT` 约束。

`incomplete` 表示模型撞到了单次请求的输出上限。驱动会把这个上限报出来，并
暂停自动目标轮次，等你下一步操作。你发的新消息会并进未完成回合的下一次请求，
所以修正能立刻到达模型。被截断的响应在会话记录里还看得到，但不会进可执行
历史；被截断响应里的工具调用永远不会执行。想要更短的回答就直接说：新会话用
SDK 的输出上限（256,000 token），恢复的会话沿用当初存下的上限。

## 工作区指令

第一次向模型发请求之前，abylab 会先读 `$ABYLAB_HOME/AGENTS.md`（默认
`~/.abylab/AGENTS.md`），再从最近的 Git 根目录一路往下读到启动工作区。目录
里没有 `.git` 标记时，就只搜工作区自己。Git 目录和 worktree 的 `.git` 文件
都算数。

每个目录按顺序读 `AGENTS.md`、`CLAUDE.md`、`AGENTS.local.md`、
`CLAUDE.local.md`。只要文件在就会被考虑；同一目录里裁剪后内容完全一样的
只收一份。目录越具体，指令优先级越高。Linux 上文件名区分大小写。

指令以 user 上下文发送，排在 system、developer 和直接用户指令之下。合并后
的上下文上限 64 KiB：优先留下更具体的文件，实在放不下就在 UTF-8 边界截断
最后一个。超过 1 MiB 的文件直接跳过。读取出错、缺失、截断都会报告；文件
不存在就安静跳过。

新会话和恢复的会话都会重新从磁盘加载。子代理继承父级已加载的基线，历史压缩
也会留着它。这条基线是运行时的请求前缀：不会变成用户记录条目，也不会变成
会话标题。abylab 只加载启动目录链；改完指令，新开会话或恢复当前会话就能
重新加载。

## 上下文压缩

长会话会被压掉，而不是直接失败，行为对齐 harness 的 `dsh-compaction-basic`
（到窗口 80% 就压，最新 16% 原样留着）：

- 设了 `ABY_CONTEXT_WINDOW=<tokens>` 才会启用。只有宿主知道路由模型的窗口有
  多大；abycore 没有模型表，不给这个数就没法衡量压力。
- 策略在**每一次模型请求**时都会问一遍，而不是只在回合之间
  （`abycore::AgentHooks::view_request`），所以一个很长的回合在步骤之间也会
  被压缩。agent 发问，abylab 决策，SDK 负责总结并换上新的视图。
- 估计值到窗口的 `ABY_COMPACT_AT` 时，abylab 先把旧的工具结果一个个裁到
  `ABY_PRUNE_TOOL_OUTPUT` 字节。光靠裁就能回到阈值以下的话，根本不发模型
  调用；否则就把最老的安全区间总结掉，最新的 `ABY_KEEP_RECENT` 原样留着。
  总结基于当前可见的历史，包括已有的 checkpoint 和裁过的输出。反复压缩会把
  旧 checkpoint 合并起来，而不是一层层叠上去。自动总结调用共用本轮的取消、
  时限和请求预算，也要守总结输出上限。
- 提供商确认的上下文溢出（`context_length_exceeded`，或 abycore 自己的输入
  预算拒绝）不会结束回合：abylab 会硬压——只留当前回合——然后重试同一个
  回合。只有这一步也失败，错误才会冒出来，并指向 `/compact` 和 `/new`。
- `/compact` 想压就压，低于阈值也行。Esc 一样能取消手动压缩和溢出压缩。视图
  变更先落盘，再显示成功。
- 持久化记录永远不会被改写：压缩记录只在**请求视图**里遮住某个区间，会话
  JSONL、`/resume` 重放和原始条目都是完整的。SDK 会拒绝把工具调用和它的
  结果、或者已有 checkpoint 劈开的区间；被截断的总结、工具调用回复，以及
  没让当前视图变小的总结，也都会被拒。普通总结失败就保持视图不变；取消、
  超时、请求预算耗尽则会停掉本轮。自动视图变更会在下一次模型请求前过一遍
  持久化屏障。

默认值：`ABY_COMPACT_AT=0.8`、`ABY_KEEP_RECENT=0.16`、
`ABY_PRUNE_TOOL_OUTPUT=4096`，总结输出上限 8192 token。压力优先用提供商给的
有效测量值，没有就按每 token 3 字节估。视图一变，旧测量就作废；恢复一个已经
压过的会话时，会保守地等新测量。

比 harness 更严的地方，以及实际真正卡人的限制：abycore 每个轮段 10 分钟运行
时限，和 4 MiB 的序列化输入上限。字节估算不保证是上界。

## 命令

`/help` · `/keys` · `/new` · `/resume [id]` · `/compact` · `/goal` · `/clear` · `/model [id]` ·
`/effort [off|low|high|max]` · `/permission [preset]` · `/plan [on|off]` ·
`/image <path> [text]` · `/clip [text]` · `/theme [dark|light|pack]` ·
`/session` · `/lang [zh|en]` · `/quit`

`/theme` 用来挑调色板包：内置的 DeepSeek 默认主题，外加九个画廊包（ayu ·
catppuccin · ember · everforest · iceberg · kanagawa · one · solarized ·
tomorrow）——全都编在二进制里，不需要外部文件。`ctrl+t` 在当前包里切换深色/
浅色。

## 会话持久化

abycore 给每个会话在共享存储里存一份 JSONL 日志，按工作区 slug 分目录：

```
~/.abylab/sessions/<workspace-slug>/<id>/session.jsonl
#   header + title + snapshot 行 · slug：/home/x/proj → --home-x-proj--
```

- 日志在第一个 checkpoint 时创建；每次追加都 fsync，追加失败就回滚——不会
  留下半行数据。
- `--session-id <id>` 会重放存下来的历史，没跑完的回合等你下一条消息继续。
  挂起的工具调用在那条消息发出前会被结算成“未核实”错误，不会重放。
- `/resume` 列出当前工作区存过的会话（新的在前）。
- `session.lock` 上有一把非阻塞 `flock`，保证同一时间只有一个写入者。

运行中改权限或 API key，会保留父级的运行时身份、收件箱和会话写入器。已有的
子代理继续用它们被委托的凭据和权限。子代理回调有自己的压缩和重复提醒状态，
用配置里的请求/工具/时间限额；切换会话后就不再持有父级的会话写入器。退出时
会先等子代理收尾，再关掉本地工具。宿主的目标变更和最终运行状态（包括
incomplete/错误退出）都会在对外公布结算状态之前存好。

## 目标轮次

每个会话一个持久目标，带明确的轮次配额：

- `/goal <目标>` 创建目标，并且当场用掉第 1 轮。`/goal @<n> <目标>` 设置配额
  （默认 10，最大 100）。
- `/goal status` 看状态；`/goal pause|resume|complete|clear` 控制它。
  `/goal resume` 也能让空闲的目标继续跑；目标激活期间你发的任何提示，都会在
  后面接着跑轮次。
- 每一轮就是一个用目标做种子的普通回合；模型靠调用 `update_goal` 并传
  `complete` 或 `blocked` 来结束循环（必须引用 `get_goal` 返回的 id 和
  revision，所以过期的模型覆盖不了已经前进的目标）。配额用尽会记下阻塞原因，
  而不是无限循环；esc 依然能中断某一轮。
- 续跑是*你*说了算：模型用工具建的目标不会自己开跑，直到你执行
  `/goal resume`。SDK 从不自己开回合，开回合的是这里的驱动。

## 按键

enter 发送/排队 · ctrl+x 立即发送 · esc 中断 / 双击清空草稿 ·
ctrl+c 清空/退出 · ↑ 历史 · `/` 命令 · `@` 文件提及 ·
ctrl+z / ctrl+shift+z 撤销/重做 · ctrl+p 模型 · ctrl+t 主题

`/permission` 在 `read-only`、`workspace-write`、`danger-full-access`（默认）
之间切换实时会话。切换保留对话历史，并作用于后续的文件工具与 Bash 进程；
Shift+Tab 循环切换预设。

鼠标：滚轮滚动 · 点击工具卡片展开 · 拖动选择，松开复制
（原生工具 → tmux → OSC52）· `@` 打开文件浏览器。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
