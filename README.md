**中文** · [English](README.en.md) · [abylab.ai](https://abylab.ai/)

# abylab

abylab 是个极简主义的 DeepSeek harness：装完就是一个可执行文件，
没有任何依赖，整个程序就是一个文件, 大约 11 MB，启动飞速,程序开源免费.

## 快速开始

1. 安装（支持 Linux、macOS ）, 在你的终端中运行：

```sh
/bin/bash -c "$(curl -fsSL https://abylab.ai/install.sh)"
```

程序会装到 `~/.local/bin/abylab`, 并把这个目录写进你的 shell 启动文件(带注释
标记, 想撤销就删那一行); `--no-path` 只打印命令不落盘, `--path-file` 指定写到
哪个文件, `--bin-dir` 换安装目录, `--help` 看全部选项.

预编译的 Linux 二进制用 musl 静态链接, 就是一个不依赖宿主 libc 的文件: RHEL 7+、
Amazon Linux 2/2023、Debian/Ubuntu 全系、Alpine 都能直接跑, 不用管宿主的 glibc
版本. 静态不等于自包含 —— TLS 证书和 DNS 仍然读宿主的配置(见 [TECH.md](TECH.md)).
想自己编译需要一个 C 编译器(依赖里的 aws-lc-sys 会自己编译 BoringSSL):

```sh
cargo install --git https://github.com/zhang0098/abylab abylab-tui
```

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

代理用 `todo_write` 维护任务清单时，输入框上沿的提示行会实时显示进行中的
任务和完成进度（`任务 · 进行中: 修复登录 · 2/5 完成`）；操作反馈会短暂借用
这一行，几秒后回到任务进度。点击进度部分（`2/5 完成`）弹出完整清单对话框，
esc 关闭。空闲时这一行留空。

使用提示不常驻输入框：每次新会话（启动、`/new`）在对话区开头给出一条轮换的
提示（esc 中断、ctrl+enter steer、`/status` 里的 token 用量……），下一条会话换成
下一条；完整清单见 `/help` 和 `/keys`。

启动时对话区最上面是 ASCII wordmark 加项目地址 `https://abylab.ai`（居中），
再往下才是那条提示；`/new` 只给提示，不再重复 logo。

提示行右侧显示项目路径；工作区是个 Git 检出时，后面紧跟 `:分支`（冒号紧贴，
如 `/work/acme/abylab:main`）。分支直接读 `.git/HEAD`，不启动 git 进程，每 5 秒
在节拍里复查一次（代理的 shell 工具或另一个终端切了分支，标签会跟着变）；
路径太长或终端太窄时只留路径。

提示行右侧、项目路径后面的 `↥` 可以点击：每次点击往前跳一条你发过的
输入（从最新开始，到最早后回到最新），跳到的输入会滚动到顶部并高亮几秒。
`↥` 右边的 `⛶` 也是纯鼠标按钮：点一下把输入框固定到放大高度（约 5/8 屏），
再点一下回到自动高度。

鼠标：滚轮滚动 · 点击工具卡片展开 · 拖动选择，松开复制
（原生工具 → tmux → OSC52）· 输入框里点击定位光标、拖动选择
（`ctrl+x` 剪掉选中）· `@` 打开文件浏览器。

`@` 文件浏览器：输入即过滤（精确 > 前缀 > 包含 > 字母顺序模糊匹配），
当前目录没有匹配就自动跳到子目录里最近的一条（跳过 `.git`、`node_modules`
等重目录，最多三层）；↑↓ 移动 · ← 上级 · →/tab 进入目录 · enter 选择 ·
`ctrl+h` 显示/隐藏点文件 · esc 关闭（同样的文字再打开）。

## 技术说明

实现细节拆到 [TECH.md](TECH.md) 了:工作区指令(AGENTS.md 的加载顺序与优先级)、
上下文压缩(阈值、裁剪与溢出恢复)、会话持久化(JSONL 布局与锁)、目标轮次(配额
与续跑规则)。日常使用不需要读。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
