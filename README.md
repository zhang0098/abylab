**中文** · [English](README.en.md)

# abylab

abylab 是个极简主义的 DeepSeek harness：装完就是一个可执行文件，
没有任何依赖，整个程序就是一个文件, 大约 11 MB，启动飞速,程序开源免费.

## 快速开始

1. 安装（支持 Linux、macOS ）, 在你的终端中运行：

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/zhang0098/abylab/main/install.sh)"
```

程序会装到 `~/.local/bin/abylab`, 并把这个目录写进你的 shell 启动文件(带注释
标记, 想撤销就删那一行); `--no-path` 只打印命令不落盘, `--path-file` 指定写到
哪个文件, `--bin-dir` 换安装目录, `--help` 看全部选项.

你可以直接让你的ai帮你安装.

2. 去 Deepseek官网 获取一个API Key, 点击这里 [https://platform.deepseek.com/](https://platform.deepseek.com/)

3. 在你的终端中输入: `abylab` ; 然后在程序中用`/login`命令输入你的API Key: 
```
/login sk-*****
```

4. 任意输入你的命令就开始工作了.


## 命令列表

`/help` · `/keys` · `/new` · `/resume [id]` · `/compact` · `/goal` · `/clear` · `/model [id]` ·
`/effort [off|low|high|max]` · `/permission [preset]` · `/plan [on|off]` ·
`/image <path> [text]` · `/clip [text]` · `/theme [dark|light|pack]` ·
`/session` · `/lang [zh|en]` · `/quit`

`/theme` 用来挑调色板包：内置的 DeepSeek 默认主题，外加九个主题（ayu ·
catppuccin · ember · everforest · iceberg · kanagawa · one · solarized ·
tomorrow）。`ctrl+t` 在当前包里切换深色/
浅色。

## 按键

enter 发送/排队 · ctrl+x 立即发送 · esc 中断 / 双击清空草稿 ·
ctrl+c 清空/退出 · ↑ 历史 · `/` 命令 · `@` 文件提及 ·
ctrl+z / ctrl+shift+z 撤销/重做 · ctrl+p 模型 · ctrl+t 主题

`/permission` 在 `read-only`、`workspace-write`、`danger-full-access`（默认）
之间切换实时会话。切换保留对话历史，并作用于后续的文件工具与 Bash 进程；
Shift+Tab 循环切换预设。

鼠标：滚轮滚动 · 点击工具卡片展开 · 拖动选择，松开复制
（原生工具 → tmux → OSC52）· `@` 打开文件浏览器。


# 下面是技术性的说明,可以不看

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


## 许可证

MIT，详见 [LICENSE](LICENSE)。
