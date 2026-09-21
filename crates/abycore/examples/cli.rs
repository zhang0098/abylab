//! A complete terminal front-end built on the SDK.
//!
//! It is an example of everything the SDK deliberately leaves to the host:
//! argument parsing, output policy, event rendering, snapshot storage, Ctrl-C
//! handling, pending-call resolution and background job control.
//!
//! ```sh
//! cargo run --example cli -- --help
//! cargo run --example cli -- "用一句话介绍这个仓库"
//! cargo run --example cli -- --workspace /tmp/sandbox --session /tmp/chat.json
//! ```
//!
//! `DEEPSEEK_API_KEY` is required. Assistant text goes to stdout, every CLI
//! notice goes to stderr, so `cli "..." > answer.txt` stays clean.

use abycore::{
    Agent, AgentEvent, BashJob, BashJobStatus, BashOutputCursor, CancellationToken, ClientConfig,
    DeepSeekClient, Error as SdkError, ErrorKind, Item, LocalToolConfig, LocalTools, MessageRole,
    ModelOptions, PendingCall, PendingState, PermissionMode, ReasoningEffort, RunOptions,
    RunOutcome, SessionSnapshot, StopReason, StreamEvent, ToolOutput, Usage,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    pin::Pin,
    process::ExitCode,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

#[tokio::main]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match Args::parse(argv) {
        Ok(Command::Run(args)) => *args,
        Ok(Command::Help) => {
            println!("{}", Args::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("abycore {} CLI 示例", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("参数错误：{message}\n\n{}", Args::USAGE);
            return ExitCode::from(2);
        }
    };
    match App::start(args) {
        Ok(mut app) => app.launch().await,
        Err(error) => {
            eprintln!("启动失败：{error}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------- arguments

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

enum Command {
    Run(Box<Args>),
    Help,
    Version,
}

#[derive(Clone)]
struct Args {
    prompt: Option<String>,
    workspace: PathBuf,
    session: Option<PathBuf>,
    api_key_env: String,
    base_url: Option<String>,
    model: String,
    reasoning: ReasoningEffort,
    max_tokens: u32,
    system: Option<String>,
    tools: bool,
    permission: PermissionMode,
    show_reasoning: bool,
    color: ColorChoice,
    run_timeout: Duration,
    tool_timeout: Duration,
    max_requests: usize,
}

impl Default for Args {
    fn default() -> Self {
        let env_text = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Self {
            prompt: None,
            workspace: PathBuf::from("."),
            session: Some(PathBuf::from(".abycore-cli-session.json")),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            base_url: env_text("DEEPSEEK_BASE_URL"),
            model: env_text("DEEPSEEK_MODEL").unwrap_or_else(|| "deepseek-flash".into()),
            reasoning: ReasoningEffort::High,
            max_tokens: 256_000,
            system: None,
            tools: true,
            permission: PermissionMode::WorkspaceWrite,
            show_reasoning: false,
            color: ColorChoice::Auto,
            run_timeout: Duration::from_secs(600),
            tool_timeout: Duration::from_secs(60),
            max_requests: 16,
        }
    }
}

impl Args {
    const USAGE: &'static str = "\
abycore CLI 示例 —— 终端里的 DeepSeek agent

用法:
  cli [选项] [提示词...]        一次性执行该提示词后退出
  cli [选项]                    进入交互式 REPL（管道输入时逐行执行）

选项:
  -w, --workspace <目录>      本地工具根目录（默认 .，必须已存在）
  -s, --session <文件>        会话快照路径（默认 .abycore-cli-session.json）
      --no-session            不读写会话快照
      --api-key-env <变量名>  API key 所在环境变量（默认 DEEPSEEK_API_KEY）
      --base-url <URL>        Messages 根地址（默认 https://api.deepseek.com/anthropic）
  -m, --model <名称>          模型（默认 deepseek-flash，可用 DEEPSEEK_MODEL）
      --reasoning <级别>      off | low | high | max（默认 high）
      --max-tokens <N>        单次响应输出上限（默认 256000）
      --system <文本>         系统提示词
      --no-tools              不注册 read/write/edit/bash
      --permission <模式>     read-only | workspace-write | full-access（默认 workspace-write）
      --show-reasoning        把思考增量以暗色写入 stderr
      --run-timeout <秒>      单个回合总时限（默认 600）
      --tool-timeout <秒>     单个工具时限（默认 60）
      --max-requests <N>      单个回合 HTTP 请求上限（默认 16）
      --color <模式>          auto | always | never（默认 auto，遵循 NO_COLOR）
  -h, --help                  显示本帮助
  -V, --version               显示版本

交互命令:
  /help  /new  /continue  /session  /history  /usage  /tools
  /jobs  /job <id>  /wait <id>  /kill <id>  /forget <id>
  /save [文件]  /load <文件>  /reasoning [on|off]  /version  /exit

运行中 Ctrl-C 取消当前回合，提示符下 Ctrl-C 退出；每个回合结束后保存快照，
任一回合未完成或出错时退出码为 1。";

    fn parse(argv: Vec<String>) -> Result<Command, String> {
        let mut args = Args::default();
        let mut prompt: Vec<String> = Vec::new();
        let mut index = 0;
        let mut positional_only = false;
        while index < argv.len() {
            let raw = argv[index].clone();
            index += 1;
            if positional_only {
                prompt.push(raw);
                continue;
            }
            if raw == "--" {
                positional_only = true;
                continue;
            }
            let (name, inline) = match raw.split_once('=') {
                Some((name, value)) if name.starts_with("--") => {
                    (name.to_owned(), Some(value.to_owned()))
                }
                _ => (raw.clone(), None),
            };
            let value = |index: &mut usize, flag: &str| -> Result<String, String> {
                if let Some(value) = inline.clone() {
                    return Ok(value);
                }
                let value = argv
                    .get(*index)
                    .cloned()
                    .ok_or_else(|| format!("{flag} 需要一个值"))?;
                *index += 1;
                Ok(value)
            };
            match name.as_str() {
                "-h" | "--help" => return Ok(Command::Help),
                "-V" | "--version" => return Ok(Command::Version),
                "-w" | "--workspace" => args.workspace = PathBuf::from(value(&mut index, &name)?),
                "-s" | "--session" => {
                    let path = value(&mut index, &name)?;
                    if path.trim().is_empty() {
                        return Err("--session 不能为空".into());
                    }
                    args.session = Some(PathBuf::from(path));
                }
                "--no-session" => args.session = None,
                "--api-key-env" => args.api_key_env = value(&mut index, &name)?,
                "--base-url" => args.base_url = Some(value(&mut index, &name)?),
                "-m" | "--model" => args.model = value(&mut index, &name)?,
                "--reasoning" => {
                    args.reasoning = match value(&mut index, &name)?.as_str() {
                        "off" => ReasoningEffort::Off,
                        "low" => ReasoningEffort::Low,
                        "high" => ReasoningEffort::High,
                        "max" => ReasoningEffort::Max,
                        other => return Err(format!("未知 reasoning 级别 {other}")),
                    }
                }
                "--max-tokens" => {
                    args.max_tokens = positive_u32(&value(&mut index, &name)?, &name)?
                }
                "--system" => args.system = Some(value(&mut index, &name)?),
                "--no-tools" => args.tools = false,
                "--tools" => args.tools = true,
                "--permission" => {
                    args.permission = match value(&mut index, &name)?.as_str() {
                        "read-only" => PermissionMode::ReadOnly,
                        "workspace-write" => PermissionMode::WorkspaceWrite,
                        "full-access" => PermissionMode::FullAccess,
                        other => return Err(format!("未知权限模式 {other}")),
                    }
                }
                "--show-reasoning" => args.show_reasoning = true,
                "--run-timeout" => args.run_timeout = seconds(&value(&mut index, &name)?, &name)?,
                "--tool-timeout" => args.tool_timeout = seconds(&value(&mut index, &name)?, &name)?,
                "--max-requests" => {
                    args.max_requests = positive_usize(&value(&mut index, &name)?, &name)?
                }
                "--color" => {
                    args.color = match value(&mut index, &name)?.as_str() {
                        "auto" => ColorChoice::Auto,
                        "always" => ColorChoice::Always,
                        "never" => ColorChoice::Never,
                        other => return Err(format!("未知颜色模式 {other}")),
                    }
                }
                other if other.starts_with('-') && other.len() > 1 => {
                    return Err(format!("未知选项 {other}"));
                }
                _ => prompt.push(raw),
            }
        }
        if !prompt.is_empty() {
            args.prompt = Some(prompt.join(" "));
        }
        Ok(Command::Run(Box::new(args)))
    }
}

fn positive(text: &str, flag: &str) -> Result<u64, String> {
    match text.parse::<u64>() {
        Ok(value) if value > 0 => Ok(value),
        _ => Err(format!("{flag} 需要一个正整数")),
    }
}

fn positive_u32(text: &str, flag: &str) -> Result<u32, String> {
    u32::try_from(positive(text, flag)?).map_err(|_| format!("{flag} 超出范围"))
}

fn positive_usize(text: &str, flag: &str) -> Result<usize, String> {
    usize::try_from(positive(text, flag)?).map_err(|_| format!("{flag} 超出范围"))
}

fn seconds(text: &str, flag: &str) -> Result<Duration, String> {
    positive(text, flag).map(Duration::from_secs)
}

// ------------------------------------------------------------------ output

#[derive(Clone, Copy)]
struct Style {
    color: bool,
}

impl Style {
    fn detect(choice: ColorChoice) -> Self {
        let color = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
            }
        };
        Self { color }
    }

    fn paint(self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }

    /// Ordinary host notice; never mixed into stdout.
    fn note(self, message: &str) {
        eprintln!("{}", self.dim(&format!("[cli] {message}")));
    }

    fn warn(self, message: &str) {
        eprintln!("{}", self.paint("33", &format!("[注意] {message}")));
    }

    fn error(self, message: &str) {
        eprintln!("{}", self.paint("31", &format!("[错误] {message}")));
    }

    fn trace(self, message: &str) {
        eprintln!("{}", self.dim(&format!("  {message}")));
    }
}

// ------------------------------------------------------------- status line

/// Serialises every in-place stderr rewrite (spinner frames vs. notices).
type ScreenLock = Arc<StdMutex<()>>;

/// Grok-style live status line, imitating `grok-build`'s spinner: braille
/// frames at 100 ms, the current label and elapsed seconds, redrawn in place.
/// Only active when stderr is a terminal (`--color auto`); `stop()` is
/// idempotent and re-checked under the screen lock, so a frame can never
/// overwrite text printed after the line was cleared.
#[derive(Clone)]
struct Spinner {
    state: Arc<SpinnerState>,
    screen: ScreenLock,
}

struct SpinnerState {
    stop: AtomicBool,
    label: StdMutex<String>,
}

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"];
const SPINNER_INTERVAL: Duration = Duration::from_millis(100);

impl Spinner {
    fn start(label: &str, screen: ScreenLock) -> Self {
        let state = Arc::new(SpinnerState {
            stop: AtomicBool::new(false),
            label: StdMutex::new(label.to_owned()),
        });
        tokio::spawn(spin_task(state.clone(), screen.clone()));
        Self { state, screen }
    }

    fn is_stopped(&self) -> bool {
        self.state.stop.load(Ordering::Relaxed)
    }

    fn set_label(&self, label: &str) {
        *self
            .state
            .label
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = label.to_owned();
    }

    /// Retire the spinner and erase its line once; later calls are no-ops.
    fn stop(&self) {
        if self.state.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        drop(
            self.screen
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    }
}

async fn spin_task(state: Arc<SpinnerState>, screen: ScreenLock) {
    let started = Instant::now();
    let mut frame = 0usize;
    loop {
        tokio::time::sleep(SPINNER_INTERVAL).await;
        if state.stop.load(Ordering::Relaxed) {
            return;
        }
        let label = state
            .label
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let guard = screen.lock().unwrap_or_else(|error| error.into_inner());
        // Re-check after taking the lock: a stop() may have run while waiting.
        if state.stop.load(Ordering::Relaxed) {
            return;
        }
        eprint!(
            "\r\x1b[2K{} {} · {} · Ctrl-C 中断",
            SPINNER_FRAMES[frame],
            label,
            elapsed_label(started.elapsed())
        );
        let _ = std::io::stderr().flush();
        drop(guard);
        frame = (frame + 1) % SPINNER_FRAMES.len();
    }
}

fn elapsed_label(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

// --------------------------------------------------------------- the agent

struct App {
    args: Args,
    style: Style,
    /// Kept so `/new` can build a second agent; cloning a client is supported.
    client: DeepSeekClient,
    agent: Agent,
    tools: Option<LocalTools>,
    cursors: HashMap<String, BashOutputCursor>,
    session_path: Option<PathBuf>,
    input: Lines<BufReader<Stdin>>,
    interactive: bool,
    restored: bool,
    system_prompt: String,
    model: ModelOptions,
    /// A run was cancelled by the user, so the unfinished turn is not replayed.
    cancelled: bool,
}

impl App {
    fn start(args: Args) -> Result<Self, Box<dyn std::error::Error>> {
        if args.tools && !args.workspace.is_dir() {
            return Err(format!("工作目录不存在或不是目录：{}", args.workspace.display()).into());
        }
        let style = Style::detect(args.color);
        let api_key = std::env::var(&args.api_key_env)
            .map_err(|_| format!("环境变量 {} 未设置", args.api_key_env))?;
        let mut config = ClientConfig::new(api_key);
        if let Some(base_url) = &args.base_url {
            config.base_url = base_url.clone();
        }
        let client = DeepSeekClient::new(config)?;

        let requested_model = ModelOptions {
            model: args.model.clone(),
            reasoning: args.reasoning,
            max_tokens: args.max_tokens,
            ..ModelOptions::default()
        };
        let requested_system = args
            .system
            .clone()
            .unwrap_or_else(|| default_system_prompt(args.tools));

        let session_path = args.session.clone();
        let restored = match &session_path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(json) => Some(SessionSnapshot::from_json(&json)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(format!("无法读取会话 {}：{error}", path.display()).into());
                }
            },
            None => None,
        };
        let was_restored = restored.is_some();
        // A restored snapshot owns the model and system prompt of that session.
        let (mut agent, system_prompt, model) = match restored {
            Some(snapshot) => {
                let system_prompt = snapshot.system_prompt.clone();
                let model = snapshot.model.clone();
                (
                    Agent::restore(client.clone(), snapshot)?,
                    system_prompt,
                    model,
                )
            }
            None => (
                Agent::new(
                    client.clone(),
                    requested_system.clone(),
                    requested_model.clone(),
                )?,
                requested_system,
                requested_model,
            ),
        };

        let tools = if args.tools {
            let mut config = LocalToolConfig::new(&args.workspace);
            config.permission_mode = args.permission;
            config.save_bash_output = true;
            let tools = LocalTools::with_config(config)?;
            tools.register(&mut agent)?;
            Some(tools)
        } else {
            None
        };

        let interactive = std::io::stdin().is_terminal();
        Ok(Self {
            args,
            style,
            client,
            agent,
            tools,
            cursors: HashMap::new(),
            session_path,
            input: BufReader::new(tokio::io::stdin()).lines(),
            interactive,
            restored: was_restored,
            system_prompt,
            model,
            cancelled: false,
        })
    }

    async fn launch(&mut self) -> ExitCode {
        if self.restored {
            let snapshot = self.agent.snapshot();
            self.style.note(&format!(
                "已恢复会话 {}（模型 {}，{} 条记录）",
                self.session_path
                    .as_deref()
                    .map_or_else(|| "-".into(), |path| path.display().to_string()),
                snapshot.model.model,
                snapshot.items.len()
            ));
        }
        // A restored snapshot may hold calls whose side effects are unknown.
        let settled = self.settle_pending().await;
        if !settled {
            self.style
                .error("待处理调用未能全部解决，会话保持原状后退出");
            self.save_session();
            self.shutdown_tools().await;
            return ExitCode::FAILURE;
        }
        self.prepare_turn().await;

        let code = match self.args.prompt.clone() {
            Some(prompt) => {
                if prompt.trim().is_empty() {
                    self.style.error("提示词为空");
                    ExitCode::from(2)
                } else {
                    match self.turn(Some(prompt)).await {
                        Ok(outcome) if outcome.stop_reason == StopReason::Completed => {
                            ExitCode::SUCCESS
                        }
                        Ok(outcome) => {
                            self.style.warn(&format!(
                                "回合未正常完成：{}",
                                stop_label(&outcome.stop_reason)
                            ));
                            ExitCode::FAILURE
                        }
                        Err(_) => ExitCode::FAILURE,
                    }
                }
            }
            None => {
                let failed = self.repl().await;
                if failed {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
        };
        self.save_session();
        self.shutdown_tools().await;
        code
    }

    /// Interactive loop. With piped stdin every line is one turn.
    /// Returns true when any turn did not complete, so the exit code reflects it.
    async fn repl(&mut self) -> bool {
        self.banner();
        let mut failures = 0;
        loop {
            if self.interactive {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(b"> ");
                let _ = stdout.flush();
            }
            let style = self.style;
            let line = tokio::select! {
                line = self.input.next_line() => match line {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        if self.agent.snapshot().needs_response {
                            style.warn("存在未完成的回合；快照已保存，下次启动可用 /continue 继续");
                        }
                        style.note("输入结束");
                        break;
                    }
                    Err(error) => { style.error(&format!("读取输入失败：{error}")); break; }
                },
                _ = tokio::signal::ctrl_c() => { style.note("已中断，退出"); break; }
            };
            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            if input.starts_with('/') {
                if matches!(self.command(input).await, Next::Exit) {
                    break;
                }
                continue;
            }
            if self.agent.snapshot().needs_response {
                if self.cancelled {
                    self.style.note(
                        "上一个回合已取消：输入 /continue 继续它，或 /new 开始新会话（也可直接用 /exit 退出）",
                    );
                    continue;
                }
                self.prepare_turn().await;
            }
            match self.turn(Some(input.to_string())).await {
                Ok(outcome) if outcome.stop_reason == StopReason::Completed => {}
                Ok(_) => failures += 1,
                Err(_) => failures += 1,
            }
        }
        failures > 0
    }

    fn banner(&self) {
        let tools = if self.tools.is_some() {
            format!(
                "read/write/edit/bash（{}，工作区 {}）",
                self.args.permission,
                self.args.workspace.display(),
            )
        } else {
            "未启用".to_string()
        };
        let session = self
            .session_path
            .as_deref()
            .map_or_else(|| "禁用".into(), |path| path.display().to_string());
        self.style
            .note(&format!("模型 {} · 工具 {}", self.model.model, tools));
        self.style.note(&format!("会话快照 {session}"));
        self.style
            .note("输入 /help 查看命令；Ctrl-C 取消当前回合，Ctrl-D 退出");
    }

    /// Cancel a user-interrupted turn, then finish whatever is still pending.
    async fn prepare_turn(&mut self) {
        if self.agent.snapshot().needs_response {
            self.style
                .note("存在未完成的回合，先继续它（Ctrl-C 可再次取消）");
            let _ = self.turn(None).await;
        }
    }

    /// Send one user message, or continue an unfinished turn when `input` is None.
    async fn turn(&mut self, input: Option<String>) -> Result<RunOutcome, SdkError> {
        let options = RunOptions {
            cancellation: CancellationToken::new(),
            timeout: self.args.run_timeout,
            max_requests: self.args.max_requests,
            tool_timeout: self.args.tool_timeout,
            ..RunOptions::default()
        };
        let cancellation = options.cancellation.clone();
        let style = self.style;
        let started = Instant::now();
        let screen: ScreenLock = Arc::new(StdMutex::new(()));
        let spinner = self
            .style
            .color
            .then(|| Spinner::start("思考中", screen.clone()));
        let mut printer = Printer::new(style, self.args.show_reasoning, screen.clone(), spinner);
        let interrupt_screen = screen;
        let mut interrupted = false;
        let result = {
            // `run` and `continue_run` return different opaque futures; box them
            // so the same select can drive either one.
            let mut run: Pin<Box<dyn Future<Output = Result<RunOutcome, SdkError>>>> = match &input
            {
                Some(text) => Box::pin(self.agent.run(text.clone(), options, |event| {
                    printer.handle(&event);
                    async { Ok(()) }
                })),
                None => Box::pin(self.agent.continue_run(options, |event| {
                    printer.handle(&event);
                    async { Ok(()) }
                })),
            };
            tokio::select! {
                result = &mut run => result,
                _ = tokio::signal::ctrl_c() => {
                    cancellation.cancel();
                    interrupted = true;
                    // Rewrite under the screen lock so the spinner frame and
                    // this notice can never interleave on one line.
                    let guard = interrupt_screen
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    eprint!("\r\x1b[2K{}", style.dim("[cli] 已请求取消，等待 SDK 收尾…\n"));
                    let _ = std::io::stderr().flush();
                    drop(guard);
                    run.await
                }
            }
        };
        let elapsed = started.elapsed();
        printer.finish();
        match &result {
            Ok(outcome) => {
                self.report(outcome, elapsed);
            }
            Err(error) => self.report_error(error),
        }
        if interrupted {
            self.cancelled = true;
        }
        // Only a cancelled or failed run can leave unresolved calls behind.
        if !self.settle_pending().await {
            self.style.warn("仍有待处理调用，可在下次启动时继续处理");
        }
        if !self.agent.snapshot().needs_response {
            self.cancelled = false;
        }
        self.save_session();
        result
    }

    fn report(&self, outcome: &RunOutcome, elapsed: Duration) {
        if !matches!(outcome.stop_reason, StopReason::Completed) {
            if let Some(reason) = &outcome.response.incomplete_reason {
                self.style.warn(&format!("响应未完成：{reason}"));
            }
            if let Some(message) = &outcome.response.error_message {
                self.style.warn(&format!("服务端错误：{message}"));
            }
        }
        let mut totals = Totals::default();
        for record in &outcome.requests {
            totals.add(record.usage.as_ref());
        }
        let unknown = if totals.unknown == 0 {
            String::new()
        } else {
            format!(" · 用量未知 {} 次", totals.unknown)
        };
        self.style.note(&format!(
            "{} · {:.1}s · 请求 {} 次 · 输入 {} / 输出 {} tokens（缓存 {} / 思考 {}）{unknown}",
            stop_label(&outcome.stop_reason),
            elapsed.as_secs_f64(),
            outcome.requests.len(),
            totals.input,
            totals.output,
            totals.cached,
            totals.reasoning,
        ));
    }

    fn report_error(&self, error: &SdkError) {
        let hint = match error.kind {
            ErrorKind::Authentication => "API key 无效或权限不足",
            ErrorKind::Quota => "配额不足",
            ErrorKind::RateLimit => "触发限流，SDK 已按退避策略重试",
            ErrorKind::Cancelled => "回合已取消；输入 /continue 继续该回合，或 /new 开始新会话",
            ErrorKind::ContextLimitExceeded => "上下文超出预算，SDK 不会自动压缩；请用 /new",
            ErrorKind::Timeout => "超过时限；可用 --run-timeout / --tool-timeout 调整",
            ErrorKind::Transport => "网络或代理错误",
            ErrorKind::EmptyResponse => "模型返回空响应，SDK 会在同一回合内重试",
            ErrorKind::Configuration => "配置错误，检查 --base-url 与模型名",
            ErrorKind::NeedsResolution => "工具副作用无法确认，已交由宿主处理",
            ErrorKind::Session => "会话协议或快照问题",
            _ => "",
        };
        let suffix = if hint.is_empty() {
            String::new()
        } else {
            format!("：{hint}")
        };
        self.style
            .error(&format!("{}（{:?}）{suffix}", error.message, error.kind));
    }

    // ------------------------------------------------------------ commands

    async fn command(&mut self, input: &str) -> Next {
        let mut parts = input.split_whitespace();
        let name = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();
        match name {
            "/help" | "/?" => eprintln!("{}", Args::USAGE),
            "/exit" | "/quit" | "/q" => {
                self.style.note("再见");
                return Next::Exit;
            }
            "/new" => {
                if let Err(error) = self.reset() {
                    self.style.error(&format!("无法新建会话：{error}"));
                }
            }
            "/continue" | "/cont" => {
                if self.agent.snapshot().needs_response {
                    let _ = self.turn(None).await;
                } else {
                    self.style.note("当前没有未完成的回合");
                }
            }
            "/session" => self.show_session(),
            "/history" => self.show_history(),
            "/usage" => self.show_usage(),
            "/tools" => self.show_tools(),
            "/jobs" => self.show_jobs(),
            "/job" => match rest.first() {
                Some(id) => self.show_job(id),
                None => self.style.error("用法：/job <id>"),
            },
            "/wait" => match rest.first() {
                Some(id) => self.wait_job(id).await,
                None => self.style.error("用法：/wait <id>"),
            },
            "/kill" => match rest.first() {
                Some(id) => self.kill_job(id).await,
                None => self.style.error("用法：/kill <id>"),
            },
            "/forget" => match rest.first() {
                Some(id) => self.forget_job(id),
                None => self.style.error("用法：/forget <id>"),
            },
            "/save" => self.save_to(rest.first().copied()),
            "/load" => match rest.first() {
                Some(path) => self.load_from(path),
                None => self.style.error("用法：/load <文件>"),
            },
            "/version" => self
                .style
                .note(&format!("abycore {} · CLI 示例", env!("CARGO_PKG_VERSION"))),
            "/reasoning" => self.toggle_reasoning(rest.first().copied()),
            other => {
                self.style.error(&format!("未知命令 {other}"));
                eprintln!("{}", Args::USAGE);
            }
        }
        Next::Continue
    }

    fn reset(&mut self) -> Result<(), SdkError> {
        let mut agent = Agent::new(
            self.client.clone(),
            self.system_prompt.clone(),
            self.model.clone(),
        )?;
        if let Some(tools) = &self.tools {
            tools.register(&mut agent)?;
        }
        self.agent = agent;
        self.cancelled = false;
        self.style
            .note("已开始新会话；旧快照将在下一次保存时被覆盖");
        Ok(())
    }

    fn show_session(&self) {
        let snapshot = self.agent.snapshot();
        let path = self
            .session_path
            .as_deref()
            .map_or_else(|| "禁用".into(), |path| path.display().to_string());
        self.style.note(&format!(
            "快照 {path} · 协议 {} v{} · 模型 {} · 记录 {} 条 · 请求 {} 次 · 待处理 {} · 待响应 {}",
            snapshot.protocol,
            snapshot.version,
            snapshot.model.model,
            snapshot.items.len(),
            snapshot.requests.len(),
            snapshot.pending.len(),
            snapshot.needs_response,
        ));
    }

    fn show_history(&self) {
        let snapshot = self.agent.snapshot();
        if snapshot.items.is_empty() {
            self.style.note("会话为空");
            return;
        }
        for (index, item) in snapshot.items.iter().enumerate() {
            let line = match item {
                Item::Message { role, content, .. } => {
                    let role = match role {
                        MessageRole::User => "user",
                        MessageRole::Assistant => "assistant",
                    };
                    let text: String = content.iter().map(|part| part.text()).collect();
                    format!("{role}: {}", snippet(&text, 100))
                }
                Item::Reasoning { content, .. } => {
                    let text: String = content.iter().map(|part| part.text()).collect();
                    format!("reasoning: {}", snippet(&text, 100))
                }
                Item::FunctionCall {
                    name,
                    arguments,
                    call_id,
                    ..
                } => format!("call {name}({}) [{call_id}]", snippet(arguments, 80)),
                Item::FunctionCallOutput {
                    call_id, output, ..
                } => {
                    format!("output [{call_id}]: {}", snippet(output, 80))
                }
            };
            eprintln!("{:>4}  {}", index + 1, self.style.dim(&line));
        }
    }

    fn show_usage(&self) {
        let records = self.agent.snapshot().requests;
        let mut totals = Totals::default();
        for record in &records {
            totals.add(record.usage.as_ref());
        }
        self.style.note(&format!(
            "累计请求 {} 次 · 输入 {} · 输出 {} · 缓存 {} · 思考 {} · 用量未知 {} 次",
            records.len(),
            totals.input,
            totals.output,
            totals.cached,
            totals.reasoning,
            totals.unknown,
        ));
    }

    fn show_tools(&self) {
        if self.tools.is_some() {
            self.style.note(&format!(
                "已注册 read、write、edit、bash，权限 {}，工作区 {}",
                self.args.permission,
                self.args.workspace.display(),
            ));
        } else {
            self.style.note("未启用本地工具（--no-tools）");
        }
    }

    fn show_jobs(&self) {
        let Some(tools) = &self.tools else {
            self.style.note("未启用本地工具");
            return;
        };
        let jobs = tools.jobs();
        if jobs.is_empty() {
            self.style.note("没有后台任务");
            return;
        }
        for job in jobs {
            eprintln!(
                "  {} {} {} {}",
                job.id,
                self.style.paint(
                    if job.status == BashJobStatus::Running {
                        "36"
                    } else {
                        "2"
                    },
                    status_label(job.status)
                ),
                snippet(&job.command, 60),
                self.style.dim(&job.description),
            );
        }
    }

    fn show_job(&mut self, id: &str) {
        let Some(tools) = &self.tools else {
            self.style.note("未启用本地工具");
            return;
        };
        let cursor = self.cursors.get(id).copied().unwrap_or_default();
        match tools.job_output(id, cursor) {
            Ok(output) => {
                self.cursors.insert(id.to_owned(), output.cursor);
                self.style
                    .trace(&format!("任务 {id} {}", status_label(output.status)));
                if output.lossy {
                    self.style
                        .warn("更早的输出已被内存尾部淘汰，可查看日志路径");
                }
                for (label, text, spill) in [
                    ("stdout", &output.stdout, &output.stdout_spill_path),
                    ("stderr", &output.stderr, &output.stderr_spill_path),
                ] {
                    if text.is_empty() && spill.is_none() {
                        continue;
                    }
                    self.style.trace(&format!("--- {label} ---"));
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                    if let Some(path) = spill {
                        self.style.trace(&format!("完整输出：{}", path.display()));
                    }
                }
            }
            Err(error) => self.style.error(&format!("无法读取任务 {id}：{error}")),
        }
    }

    async fn kill_job(&mut self, id: &str) {
        let Some(tools) = self.tools.clone() else {
            self.style.note("未启用本地工具");
            return;
        };
        match tools.kill_job(id).await {
            Ok(job) => self.style.note(&format!("任务 {id} {}", job_summary(&job))),
            Err(error) => self.style.error(&format!("无法终止任务 {id}：{error}")),
        }
    }

    fn forget_job(&mut self, id: &str) {
        let Some(tools) = &self.tools else {
            self.style.note("未启用本地工具");
            return;
        };
        match tools.forget_job(id) {
            Ok(()) => {
                self.cursors.remove(id);
                self.style.note(&format!("已释放任务记录 {id}"));
            }
            Err(error) => self.style.error(&format!("无法释放任务记录 {id}：{error}")),
        }
    }

    async fn wait_job(&mut self, id: &str) {
        let Some(tools) = self.tools.clone() else {
            self.style.note("未启用本地工具");
            return;
        };
        self.style
            .trace(&format!("等待任务 {id} …（Ctrl-C 中断等待）"));
        match tokio::select! {
            waited = tools.wait_job(id) => waited,
            _ = tokio::signal::ctrl_c() => {
                self.style.note("已停止等待，任务仍在运行");
                return;
            }
        } {
            Ok(job) => self.style.note(&format!("任务 {id} {}", job_summary(&job))),
            Err(error) => self.style.error(&format!("无法等待任务 {id}：{error}")),
        }
    }

    /// Write the snapshot to an explicit path, or to the configured session path.
    fn save_to(&mut self, path: Option<&str>) {
        let Some(path) = path
            .map(PathBuf::from)
            .or_else(|| self.session_path.clone())
        else {
            self.style
                .note("会话快照已禁用（--no-session），请指定一个路径：/save <文件>");
            return;
        };
        let json = match self.agent.snapshot().to_json() {
            Ok(json) => json,
            Err(error) => {
                self.style.warn(&format!("会话无法序列化：{error}"));
                return;
            }
        };
        match write_atomic(&path, json.as_bytes()) {
            Ok(()) => self.style.note(&format!("已保存会话 {}", path.display())),
            Err(error) => self
                .style
                .warn(&format!("会话保存失败 {}：{error}", path.display())),
        }
    }

    fn load_from(&mut self, path: &str) {
        if let Err(error) = self.try_load(path) {
            self.style.error(&format!("无法载入会话 {path}：{error}"));
        }
    }

    /// Adopt a snapshot from an arbitrary path, as `App::start` does at launch.
    fn try_load(&mut self, path: &str) -> Result<(), SdkError> {
        let json = std::fs::read_to_string(path)
            .map_err(|error| SdkError::new(ErrorKind::Session, format!("读取失败：{error}")))?;
        let snapshot = SessionSnapshot::from_json(&json)?;
        let system_prompt = snapshot.system_prompt.clone();
        let model = snapshot.model.clone();
        let mut agent = Agent::restore(self.client.clone(), snapshot)?;
        if let Some(tools) = &self.tools {
            tools.register(&mut agent)?;
        }
        self.agent = agent;
        self.system_prompt = system_prompt;
        self.model = model;
        self.session_path = Some(PathBuf::from(path));
        self.cursors.clear();
        self.cancelled = false;
        let snapshot = self.agent.snapshot();
        self.style.note(&format!(
            "已载入会话 {path}（模型 {}，{} 条记录，待响应 {}）",
            snapshot.model.model,
            snapshot.items.len(),
            snapshot.needs_response
        ));
        Ok(())
    }

    fn toggle_reasoning(&mut self, value: Option<&str>) {
        match value {
            Some("on") => self.args.show_reasoning = true,
            Some("off") => self.args.show_reasoning = false,
            Some(other) => {
                self.style
                    .error(&format!("未知开关 {other}，用法：/reasoning on|off"));
                return;
            }
            None => self.args.show_reasoning = !self.args.show_reasoning,
        }
        let state = if self.args.show_reasoning {
            "on"
        } else {
            "off"
        };
        self.style.note(&format!("思考增量显示：{state}"));
    }

    // -------------------------------------------------------- host services

    /// Resolve every pending call in transcript order. Returns false on failure.
    async fn settle_pending(&mut self) -> bool {
        loop {
            let Some(call) = self.agent.snapshot().pending.first().cloned() else {
                return true;
            };
            let output = match self.ask_resolution(&call).await {
                Some(text) => ToolOutput::text(text),
                None => ToolOutput::error(
                    "宿主在中断或重启后无法确认本次调用的副作用，未自动重放；请核实后自行处理",
                ),
            };
            if let Err(error) = self.agent.resolve_tool(&call.call_id, output) {
                self.style
                    .error(&format!("解决待处理调用 {} 失败：{error}", call.call_id));
                return false;
            }
            self.style
                .note(&format!("已解决待处理调用 {}({})", call.name, call.call_id));
        }
    }

    /// Ask the host operator for a verified result; empty input means "unknown".
    async fn ask_resolution(&mut self, call: &PendingCall) -> Option<String> {
        if !self.interactive {
            return None;
        }
        let state = match call.state {
            PendingState::Ready => "尚未执行",
            PendingState::Unknown => "副作用未知",
        };
        let style = self.style;
        style.warn(&format!(
            "待处理调用 {}({}) · {state} · 参数 {}",
            call.name,
            call.call_id,
            snippet(&call.arguments, 200),
        ));
        style.note("输入已核实的结果文本，或直接回车标记为未验证错误");
        match self.input.next_line().await {
            Ok(Some(line)) => {
                let text = line.trim();
                (!text.is_empty()).then(|| text.to_owned())
            }
            _ => None,
        }
    }

    fn save_session(&self) {
        let Some(path) = &self.session_path else {
            return;
        };
        let json = match self.agent.snapshot().to_json() {
            Ok(json) => json,
            Err(error) => {
                self.style.warn(&format!("会话无法序列化：{error}"));
                return;
            }
        };
        if let Err(error) = write_atomic(path, json.as_bytes()) {
            self.style
                .warn(&format!("会话保存失败 {}：{error}", path.display()));
        }
    }

    async fn shutdown_tools(&mut self) {
        let Some(tools) = self.tools.take() else {
            return;
        };
        let running = tools
            .jobs()
            .iter()
            .filter(|job| job.status == BashJobStatus::Running)
            .count();
        if running > 0 {
            self.style
                .note(&format!("正在终止 {running} 个后台任务并等待清理"));
        }
        tools.shutdown().await;
    }
}

enum Next {
    Continue,
    Exit,
}

// ------------------------------------------------------------- event output

struct Printer {
    style: Style,
    show_reasoning: bool,
    reasoning_open: bool,
    text_open: bool,
    text_progress: TextProgress,
    current_tool: Option<String>,
    /// Arguments of the latest `ItemDone` function call, for the tool line.
    pending_args: Option<(String, String)>,
    spinner: Option<Spinner>,
    spinner_screen: ScreenLock,
}

impl Printer {
    fn new(
        style: Style,
        show_reasoning: bool,
        screen: ScreenLock,
        spinner: Option<Spinner>,
    ) -> Self {
        Self {
            style,
            show_reasoning,
            reasoning_open: false,
            text_open: false,
            text_progress: TextProgress::default(),
            current_tool: None,
            pending_args: None,
            spinner,
            spinner_screen: screen,
        }
    }

    fn spinner_start(&mut self, label: &str) {
        self.spinner = match &self.spinner {
            Some(spinner) if !spinner.is_stopped() => {
                spinner.set_label(label);
                Some(spinner.clone())
            }
            _ => Some(Spinner::start(label, self.spinner_screen.clone())),
        };
    }

    fn spinner_stop(&mut self) {
        if let Some(spinner) = &self.spinner {
            spinner.stop();
        }
    }

    fn handle(&mut self, event: &AgentEvent) {
        if let AgentEvent::Model(event) = event {
            let missing = self.text_progress.observe(event);
            if !missing.is_empty() {
                self.spinner_stop();
                self.close_reasoning();
                print!("{missing}");
                let _ = std::io::stdout().flush();
                self.text_open = true;
            }
        }
        match event {
            AgentEvent::RunStarted { .. } => self.spinner_start("思考中"),
            AgentEvent::PlanChanged { plan } => {
                if let Some(plan) = plan
                    && !plan.todos.is_empty()
                {
                    self.style.trace(&format!(
                        "计划：{}/{} 已完成，{} 进行中，{} 待处理",
                        plan.counts.completed,
                        plan.total,
                        plan.counts.in_progress,
                        plan.counts.pending,
                    ));
                }
            }
            AgentEvent::Model(StreamEvent::TextDelta { delta, .. }) => {
                self.spinner_stop();
                self.close_reasoning();
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(delta.as_bytes());
                let _ = stdout.flush();
                self.text_open = true;
            }
            AgentEvent::Model(StreamEvent::ReasoningDelta { delta, .. }) => {
                if self.show_reasoning {
                    self.spinner_stop();
                    if !self.reasoning_open {
                        self.style.trace("思考：");
                        self.reasoning_open = true;
                    }
                    let mut stderr = std::io::stderr().lock();
                    let _ = stderr.write_all(self.style.dim(delta).as_bytes());
                    let _ = stderr.flush();
                }
            }
            AgentEvent::Model(StreamEvent::ItemDone {
                item: Item::FunctionCall {
                    name, arguments, ..
                },
                ..
            }) => {
                self.pending_args = Some((name.clone(), arguments.clone()));
            }
            AgentEvent::Model(_) => {}
            AgentEvent::ToolStarted { name, call_id } => {
                self.spinner_stop();
                self.close_reasoning();
                self.end_text_line();
                self.current_tool = Some(name.clone());
                let args = self
                    .pending_args
                    .take()
                    .filter(|(item_name, _)| item_name == name)
                    .map(|(_, arguments)| snippet(&arguments, 72));
                match args {
                    Some(arguments) => self.style.trace(&format!(
                        "→ {name} {} [{call_id}]",
                        self.style.dim(&arguments)
                    )),
                    None => self.style.trace(&format!("→ {name} [{call_id}]")),
                }
                self.spinner_start(&format!("执行 {name}"));
            }
            AgentEvent::ToolFinished { output, .. } => {
                self.spinner_stop();
                self.end_text_line();
                let name = self.current_tool.take().unwrap_or_else(|| "tool".into());
                self.style
                    .trace(&format!("← {name} {}", tool_summary(output)));
            }
            AgentEvent::RunFinished { .. } => self.spinner_stop(),
            AgentEvent::GoalChanged { goal } => {
                if let Some(goal) = goal {
                    self.style.trace(&format!("goal · {}", goal.summary()));
                }
            }
            AgentEvent::ViewChanged { change } => {
                self.spinner_stop();
                self.end_text_line();
                self.style.trace(&format!("view {change:?}"));
            }
        }
    }

    fn close_reasoning(&mut self) {
        if self.reasoning_open {
            eprintln!();
            self.reasoning_open = false;
        }
    }

    /// Terminate the assistant's stdout line so redirected output stays complete.
    fn end_text_line(&mut self) {
        if self.text_open {
            println!();
            self.text_open = false;
        }
    }

    fn finish(&mut self) {
        self.spinner_stop();
        self.close_reasoning();
        self.end_text_line();
    }
}

/// Track each response's emitted text parts, including completed-only items.
#[derive(Default)]
struct TextProgress {
    emitted: HashMap<(usize, usize), usize>,
}

impl TextProgress {
    fn observe(&mut self, event: &StreamEvent) -> String {
        match event {
            StreamEvent::Started { .. } => self.emitted.clear(),
            StreamEvent::TextDelta {
                output_index,
                content_index,
                delta,
                ..
            } => {
                *self
                    .emitted
                    .entry((*output_index, *content_index))
                    .or_default() += delta.len();
            }
            StreamEvent::ItemDone {
                output_index, item, ..
            } => return self.complete(*output_index, item),
            StreamEvent::Finished { response, .. } => {
                return response
                    .output
                    .iter()
                    .enumerate()
                    .map(|(i, item)| self.complete(i, item))
                    .collect();
            }
            _ => {}
        }
        String::new()
    }

    fn complete(&mut self, index: usize, item: &Item) -> String {
        let Item::Message {
            role: MessageRole::Assistant,
            content,
            ..
        } = item
        else {
            return String::new();
        };
        let mut missing = String::new();
        for (part, content) in content.iter().enumerate() {
            let text = content.text();
            let emitted = self.emitted.entry((index, part)).or_default();
            if let Some(suffix) = text.get(*emitted..) {
                missing.push_str(suffix);
            }
            *emitted = text.len();
        }
        missing
    }
}

fn tool_summary(output: &ToolOutput) -> String {
    let mut summary = if output.is_error { "错误" } else { "完成" }.to_string();
    if let Some(detail) = output.details.as_ref().and_then(detail_summary) {
        summary.push_str(" · ");
        summary.push_str(&detail);
    } else if let Some(line) = first_line(&output.content) {
        summary.push_str(" · ");
        summary.push_str(&line);
    }
    if output.truncated {
        summary.push_str(" · 输出已截断");
    }
    summary
}

/// Render the structured `ToolOutput.details` of the four local tools.
fn detail_summary(details: &Value) -> Option<String> {
    if let Some(job) = details.get("jobId").and_then(Value::as_str) {
        return Some(format!("后台任务 {job}"));
    }
    if details.get("exitCode").is_some() || details.get("timedOut").is_some() {
        let mut text = match details.get("exitCode").and_then(Value::as_i64) {
            Some(code) => format!("退出码 {code}"),
            None => "被信号终止".to_string(),
        };
        if details
            .get("timedOut")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            text.push_str("（超时）");
        }
        return Some(text);
    }
    let path = details.get("path").and_then(Value::as_str)?;
    if let Some(operation) = details.get("operation").and_then(Value::as_str) {
        return Some(format!("write {path} · {operation}"));
    }
    if let Some(replacements) = details.get("replacements").and_then(Value::as_u64) {
        return Some(format!("edit {path} · {replacements} 处替换"));
    }
    if let Some(total) = details.get("totalLines").and_then(Value::as_u64) {
        return Some(format!("read {path} · 共 {total} 行"));
    }
    Some(path.to_string())
}

// ----------------------------------------------------------------- helpers

#[derive(Default)]
struct Totals {
    input: u64,
    output: u64,
    cached: u64,
    reasoning: u64,
    unknown: usize,
}

impl Totals {
    fn add(&mut self, usage: Option<&Usage>) {
        let Some(usage) = usage.filter(|usage| usage.consistent) else {
            self.unknown += 1;
            return;
        };
        self.input += usage.input_tokens.unwrap_or(0);
        self.output += usage.output_tokens.unwrap_or(0);
        self.cached += usage.cached_tokens.unwrap_or(0);
        self.reasoning += usage.reasoning_tokens.unwrap_or(0);
    }
}

fn stop_label(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::Completed => "完成",
        StopReason::Incomplete => "未完成",
        StopReason::Failed => "失败",
        StopReason::Error(_) => "错误",
    }
}

fn status_label(status: BashJobStatus) -> &'static str {
    match status {
        BashJobStatus::Running => "运行中",
        BashJobStatus::Completed => "已完成",
        BashJobStatus::Cancelled => "已取消",
        BashJobStatus::Failed => "已失败",
    }
}

/// One-line rendering of a finished or running background job.
fn job_summary(job: &BashJob) -> String {
    let mut text = format!(
        "{} · {}",
        status_label(job.status),
        snippet(&job.command, 60)
    );
    if let Some(result) = &job.result {
        let tail = match result.exit_code {
            Some(code) => format!("退出码 {code}"),
            None => match result.signal {
                Some(signal) => format!("被信号 {signal} 终止"),
                None => "被终止".to_string(),
            },
        };
        text.push_str(&format!(" · {tail}"));
        if result.timed_out {
            text.push_str(" · 超时");
        }
    }
    if let Some(error) = &job.error {
        text.push_str(&format!(" · {error}"));
    }
    text
}

fn default_system_prompt(tools: bool) -> String {
    let mut prompt = String::from("你是运行在本地 CLI 中的助手，回答简洁、准确，不编造工具结果。");
    if tools {
        prompt.push_str(
            "你可以使用 read/write/edit/bash 操作工作目录：修改已有文件前先用 read 读取，\
             命令保持小步且可验证。",
        );
    } else {
        prompt.push_str("本次没有启用工具，请仅凭已有信息回答。");
    }
    prompt
}

/// Same-directory temp file + `sync_all` + atomic replace, as in `session.rs`.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

fn snippet(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let head: String = flat.chars().take(limit).collect();
    format!("{head}…")
}

fn first_line(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    Some(snippet(line, 120))
}

#[cfg(test)]
mod tests {
    use super::*;
    use abycore::{ContentPart, Response, ResponseStatus};

    fn message(parts: &[&str]) -> Item {
        Item::Message {
            id: Some("m".into()),
            role: MessageRole::Assistant,
            content: parts
                .iter()
                .map(|text| ContentPart::OutputText {
                    text: (*text).into(),
                })
                .collect(),
        }
    }
    fn delta(index: usize, part: usize, text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            output_index: index,
            content_index: part,
            item_id: "m".into(),
            delta: text.into(),
            sequence: 1,
        }
    }
    fn finished(items: Vec<Item>) -> StreamEvent {
        StreamEvent::Finished {
            sequence: 2,
            response: Box::new(Response {
                id: "r".into(),
                model: "test".into(),
                status: ResponseStatus::Completed,
                output: items,
                usage: None,
                incomplete_reason: None,
                error_code: None,
                error_message: None,
            }),
        }
    }

    #[test]
    fn final_answer_is_printed_after_an_earlier_streamed_round() {
        let mut progress = TextProgress::default();
        assert!(progress.observe(&delta(0, 0, "working")).is_empty());
        assert!(
            progress
                .observe(&finished(vec![message(&["working"])]))
                .is_empty()
        );
        progress.observe(&StreamEvent::Started {
            response_id: "next".into(),
            sequence: 0,
        });
        assert_eq!(
            progress.observe(&finished(vec![message(&["final answer"])])),
            "final answer"
        );
    }

    #[test]
    fn item_and_response_done_never_duplicate_text_or_drop_unstreamed_parts() {
        let mut progress = TextProgress::default();
        progress.observe(&delta(0, 0, "你好"));
        let item = message(&["你好", "第二段"]);
        assert_eq!(
            progress.observe(&StreamEvent::ItemDone {
                output_index: 0,
                item: item.clone(),
                sequence: 2
            }),
            "第二段"
        );
        assert_eq!(
            progress.observe(&finished(vec![item, message(&["新消息"])])),
            "新消息"
        );
    }

    #[test]
    fn incomplete_response_prints_only_the_unstreamed_utf8_suffix() {
        let mut progress = TextProgress::default();
        progress.observe(&delta(0, 0, "你"));
        let mut event = finished(vec![message(&["你好"])]);
        if let StreamEvent::Finished { response, .. } = &mut event {
            response.status = ResponseStatus::Incomplete;
        }
        assert_eq!(progress.observe(&event), "好");
    }
}
