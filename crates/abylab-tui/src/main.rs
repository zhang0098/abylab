//! abylab — terminal-native agent harness UI.

mod app;
mod attachments;
mod bus;
mod clipboard;
mod controller;
mod credentials;

mod data;
mod events;
mod file_ref;
mod input;
mod locale;
mod markdown;
mod pet;
mod runtime;
mod slots;
mod theme;
mod transcript;
mod ui;
mod uninstall;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::SynchronizedUpdate;

use crate::app::App;
use crate::bus::{AppEvent, Cmd};
use crate::controller::Controller;
use crate::locale::UiSettings;
use crate::runtime::{aby_home, default_sessions_root, RuntimeConfig};

const HELP: &str = "\
abylab — terminal-native agent harness UI

USAGE:
  abylab [OPTIONS]

OPTIONS:
  -w, --workspace <dir>     agent workspace (default: cwd)
      --session-id <id>     resume/continue a durable session id
      --session-root <dir>  abycore session store root
                            (default: $ABYLAB_HOME/sessions)
      --model <id>          model id (default: $ABY_MODEL or deepseek-flash)
      --base-url <url>      sets DEEPSEEK_BASE_URL for the agent
      --api-key <key>       override the agent API key for this run
                            (/login persists one instead)
      --theme <dark|light>  DeepSeek Web UI palette (default: persisted, else dark)
      --uninstall           take abylab back off this machine: everything saved
                            under $ABYLAB_HOME (settings, stored key, session
                            logs, queued prompts) and every abylab binary.
                            Lists what goes and asks about the data and the
                            program separately; --keep-data removes the
                            program and leaves the data. Shell startup files
                            are not edited. Other launch options are ignored,
                            except --session-root, which names the store it
                            checks
      --keep-data           with --uninstall: keep the saved data and remove
                            only the program
      --yes                 with --uninstall: do not ask
  -V, --version             print version
  -h, --help                this help
";

struct Args {
    workspace: Option<String>,
    sessions_root: Option<String>,
    session_id: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    /// `--theme` override; absent falls back to the persisted appearance.
    theme: Option<String>,
    /// `--uninstall`: take back the data and the program itself, then exit.
    uninstall: bool,
    /// `--keep-data`: with `--uninstall`, leave the saved state alone.
    keep_data: bool,
    /// `--yes`: skip `--uninstall`'s terminal prompts.
    yes: bool,
}

fn parse_args() -> Result<Args> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut args_out = Args {
        workspace: None,
        sessions_root: None,
        session_id: None,
        model: None,
        base_url: None,
        api_key: None,
        theme: None,
        uninstall: false,
        keep_data: false,
        yes: false,
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut take = |name: &str| -> Result<String> {
            it.next().with_context(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-w" | "--workspace" => args_out.workspace = Some(take("--workspace")?),
            "--session-root" => args_out.sessions_root = Some(take("--session-root")?),
            "--session-id" => args_out.session_id = Some(take("--session-id")?),
            "--model" => args_out.model = Some(take("--model")?),
            "--base-url" => args_out.base_url = Some(take("--base-url")?),
            "--api-key" => args_out.api_key = Some(take("--api-key")?),
            "--theme" => args_out.theme = Some(take("--theme")?),
            "--uninstall" => args_out.uninstall = true,
            "--keep-data" => args_out.keep_data = true,
            "--yes" => args_out.yes = true,
            "-V" | "--version" => {
                println!("abylab {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other} (see --help)"),
        }
    }
    if args_out.keep_data && !args_out.uninstall {
        bail!("--keep-data only means something with --uninstall (see --help)");
    }
    if args_out.yes && !args_out.uninstall {
        bail!("--yes only means something with --uninstall (see --help)");
    }
    Ok(args_out)
}

fn build_config(args: &Args) -> Result<RuntimeConfig> {
    let workspace = match &args.workspace {
        Some(w) => std::fs::canonicalize(w)
            .with_context(|| format!("workspace not found: {w}"))?
            .to_string_lossy()
            .into_owned(),
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    let home = aby_home().to_string_lossy().into_owned();
    let sessions_root = args
        .sessions_root
        .clone()
        .unwrap_or_else(|| default_sessions_root().to_string_lossy().into_owned());
    std::fs::create_dir_all(&home).ok();
    let settings = UiSettings::load(&home);

    // `--api-key` overrides the key saved through `/login` for this run;
    // the environment is never consulted.
    let (api_key, key_origin) = RuntimeConfig::resolve_credentials(args.api_key.as_deref(), &home)
        .map_err(|err| anyhow::anyhow!("{err}"))?;

    Ok(RuntimeConfig {
        workspace,
        home,
        sessions_root,
        provider: "deepseek-official".into(),
        model: args
            .model
            .clone()
            .or_else(|| {
                std::env::var("ABY_MODEL")
                    .ok()
                    .filter(|raw| !raw.trim().is_empty())
            })
            .or_else(|| settings.model.clone().filter(|raw| !raw.trim().is_empty()))
            .unwrap_or_else(|| "deepseek-flash".into()),
        max_tokens: None,
        base_url: args.base_url.clone(),
        api_key,
        key_origin,
    })
}

/// One `remove` row per measured data entry: label, path, files and bytes, the
/// shape `--uninstall`'s plan prints for the data half.
fn data_item_rows(plan: &data::DataPlan) -> String {
    use crate::data::{human_bytes, human_files};

    let mut out = String::new();
    for item in &plan.items {
        let files = human_files(item.files, item.truncated);
        let plural = if item.files == 1 { "file" } else { "files" };
        out.push_str(&format!(
            "  remove  {}  ({} · {files} {plural} · {})\n",
            item.path.display(),
            item.what.label(crate::locale::Locale::En),
            human_bytes(item.bytes),
        ));
    }
    out
}

/// The data paths the wipe lists as out of scope.
fn data_kept_rows(plan: &data::DataPlan) -> String {
    let mut out = String::new();
    for keep in &plan.kept {
        out.push_str(&format!(
            "  keep    {}  ({})\n",
            keep.path.display(),
            keep.why.note(crate::locale::Locale::En),
        ));
    }
    out
}

/// Another abylab holds the home open; it writes settings and session logs
/// back, so the removal would half-undo itself.
const OTHER_INSTANCE_NOTE: &str =
    "  note    another abylab is running — quit it first, or it writes its\n\
     \x20         settings and session log back after this\n";

/// What the data wipe did, with every path that refused to go named on its own
/// line: a partial wipe is reported, never rounded up to a success.
fn data_outcome_text(outcome: &data::DataOutcome) -> String {
    let plural =
        |count: u64, one: &str, many: &str| if count == 1 { one } else { many }.to_string();
    let mut out = format!(
        "\nremoved {} {} — {} {} · {}\n",
        outcome.removed,
        plural(outcome.removed as u64, "entry", "entries"),
        outcome.files,
        plural(outcome.files, "file", "files"),
        data::human_bytes(outcome.bytes),
    );
    for (path, reason) in &outcome.failures {
        out.push_str(&format!(
            "warning could not remove {} ({reason})\n",
            path.display()
        ));
    }
    out
}

/// `abylab --uninstall`: the data half (everything this program saved under
/// the aby home) plus the program half — every binary this machine has. Both
/// halves are listed before anything goes, and the user is asked about them
/// separately, so "keep the data, remove the program" is one answer away. The
/// PATH block install.sh wrote into a shell startup file is left alone; that
/// file belongs to the user.
fn run_uninstall(args: &Args, home: &Path) -> Result<()> {
    let input = scan_input_from_env();
    run_uninstall_with(args, home, &input)
}

/// The environment half of the program scan: what this machine has and where.
/// `current_exe` covers a binary started from outside `$PATH`; the `$PATH`
/// entries, `~/.local/bin` and `$ABYLAB_BIN_DIR` cover the rest.
fn scan_input_from_env() -> uninstall::ScanInput {
    let user_home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.push(Path::new(&user_home).join(".local/bin"));
    if let Some(dir) = std::env::var_os("ABYLAB_BIN_DIR").filter(|value| !value.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&user_home).join(".cargo"));
    uninstall::ScanInput {
        current_exe: std::env::current_exe().ok(),
        dirs,
        cargo_bin: Some(cargo_home.join("bin")),
    }
}

/// The scan and prompts, with the environment passed in — which is also what
/// the tests pin, so a test run can never delete a real binary.
fn run_uninstall_with(
    args: &Args,
    home: &Path,
    program_input: &uninstall::ScanInput,
) -> Result<()> {
    let sessions_root = args
        .sessions_root
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(default_sessions_root);
    let data = data::scan(home, &sessions_root);
    let program = uninstall::scan(program_input);
    print!("{}", uninstall_plan_text(&data, &program, args.keep_data));

    if data.is_empty() && program.is_empty() {
        println!(
            "\nnothing to remove — no saved data under {} and no abylab binary found\n",
            home.display()
        );
        return Ok(());
    }

    // Two questions, each skippable: `--keep-data` answers the first with "no",
    // `--yes` answers both with "yes".
    let remove_data = if args.keep_data || data.is_empty() {
        false
    } else if args.yes {
        true
    } else {
        confirm_on_terminal("Remove the saved data above?")?
    };
    let remove_program = if program.is_empty() {
        false
    } else if args.yes {
        true
    } else {
        confirm_on_terminal("Remove the abylab program above?")?
    };

    if !remove_data && !remove_program {
        println!("\nnothing removed\n");
        return Ok(());
    }

    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    if remove_data {
        let outcome = data::wipe(&data);
        print!("{}", data_outcome_text(&outcome));
        failures.extend(outcome.failures);
    }
    if remove_program {
        let outcome = uninstall::wipe(&program);
        print!("{}", program_outcome_text(&outcome));
        failures.extend(outcome.failures);
    }
    if failures.is_empty() {
        if remove_program {
            print!("{}", uninstall_done_text(remove_data, home));
        }
        return Ok(());
    }
    bail!(
        "uninstall did not finish — {} path(s) could not be removed (see above)",
        failures.len()
    )
}

/// The plan as the terminal reads it: the data rows (or the home as kept,
/// under `--keep-data`), then one row per program path. Anything not listed
/// here is not touched either — the list is the contract.
fn uninstall_plan_text(
    data: &data::DataPlan,
    program: &uninstall::ProgramPlan,
    keep_data: bool,
) -> String {
    let mut out =
        String::from("abylab uninstall — the data this program saved and the program itself\n\n");
    if keep_data {
        out.push_str(&format!(
            "  keep    {}  (--keep-data)\n",
            data.home.display()
        ));
    } else {
        out.push_str(&data_item_rows(data));
        out.push_str(&data_kept_rows(data));
    }
    for bin in &program.bins {
        let through = if bin.cargo {
            " · through cargo uninstall abylab-tui"
        } else {
            ""
        };
        out.push_str(&format!(
            "  remove  {}  (binary{through} · {})\n",
            bin.path.display(),
            data::human_bytes(bin.bytes),
        ));
    }
    for keep in &program.kept {
        out.push_str(&format!(
            "  keep    {}  ({})\n",
            keep.path.display(),
            keep.why.note(),
        ));
    }
    if !data.is_empty() && other_instance_running() {
        out.push_str(OTHER_INSTANCE_NOTE);
    }
    out
}

/// What the program half's wipe did, with every path that refused to go named
/// on its own line and cargo's bookkeeping called out either way.
fn program_outcome_text(outcome: &uninstall::ProgramOutcome) -> String {
    let plural =
        |count: u64, one: &str, many: &str| if count == 1 { one } else { many }.to_string();
    let mut out = format!(
        "\nremoved {} {} — {}\n",
        outcome.removed,
        plural(outcome.removed as u64, "binary", "binaries"),
        data::human_bytes(outcome.bytes),
    );
    if outcome.cargo_uninstalled {
        out.push_str("  (the copies in cargo's bin went through cargo uninstall abylab-tui)\n");
    }
    if outcome.cargo_failed {
        out.push_str(
            "warning cargo uninstall abylab-tui did not work — the file was removed \
             directly, but cargo's list may still name the package\n",
        );
    }
    for (path, reason) in &outcome.failures {
        out.push_str(&format!(
            "warning could not remove {} ({reason})\n",
            path.display()
        ));
    }
    out
}

/// The last line of a finished uninstall, said only when the program half
/// really went. Shell startup files are deliberately left alone, so the note
/// says that instead of implying they were cleaned too.
fn uninstall_done_text(data_removed: bool, home: &Path) -> String {
    let data = if data_removed {
        "The data directory and the binaries are removed.".to_string()
    } else {
        format!("The data under {} was kept.", home.display())
    };
    format!("\nabylab is gone. {data} Shell startup files were left as they were.\n")
}

/// Ask on the terminal (not stdin: a caller may pipe something into abylab,
/// and the prompt must not eat it). No terminal to ask on is an error, not a
/// silent "yes".
fn confirm_on_terminal(prompt: &str) -> Result<bool> {
    let reader: Box<dyn std::io::BufRead> = match std::fs::File::open("/dev/tty") {
        Ok(tty) => Box::new(std::io::BufReader::new(tty)),
        Err(error) if cfg!(unix) => bail!(
            "there is no terminal to ask on ({error}) — re-run with --yes to remove \
             without a prompt"
        ),
        Err(_) => Box::new(std::io::BufReader::new(std::io::stdin())),
    };
    eprint!("\n{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    let mut reader = reader;
    reader.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Another abylab on this machine holds the home open and writes to it again
/// on its next preference change; the removal would half-undo itself. Best
/// effort: `pgrep` absent (or another process error) simply means no note.
fn other_instance_running() -> bool {
    let Ok(output) = std::process::Command::new("pgrep")
        .arg("-x")
        .arg("abylab")
        .output()
    else {
        return false;
    };
    let me = std::process::id();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .any(|pid| pid != me)
}

/// Resolve the per-turn limits: environment > abylab default.
///
/// The driver owns the semantics; abylab only decides the numbers. The two
/// budget values follow deepseek-harness's "runaway-loop backstop" idiom and
/// accept `0` as *no cap* — abycore's `RunOptions::validate` rejects a literal
/// zero, so unlimited maps onto `usize::MAX` here.
///
/// There is no run-level timeout to resolve: a turn is bounded by each request's
/// transport timeouts, each tool call's budget (`ABY_TOOL_TIMEOUT` for a tool
/// that declares none) and the budgets above, and a long turn that keeps working
/// is exactly what a run should do. Esc ends one at any point.
fn resolve_limits() -> Result<abylab_backend::TurnLimits> {
    let defaults = abylab_backend::TurnLimits::default();
    Ok(abylab_backend::TurnLimits {
        max_requests: cap_or_unlimited(pick_limit(
            "ABY_MAX_REQUESTS",
            std::env::var("ABY_MAX_REQUESTS").ok(),
            defaults.max_requests,
        )?),
        max_tool_calls: cap_or_unlimited(pick_limit(
            "ABY_MAX_TOOL_CALLS",
            std::env::var("ABY_MAX_TOOL_CALLS").ok(),
            defaults.max_tool_calls,
        )?),
        continuations: pick_limit(
            "ABY_AUTO_CONTINUE",
            std::env::var("ABY_AUTO_CONTINUE").ok(),
            defaults.continuations,
        )?,
        run_timeout: defaults.run_timeout,
        tool_timeout: Duration::from_secs(pick_positive(
            "ABY_TOOL_TIMEOUT",
            std::env::var("ABY_TOOL_TIMEOUT").ok(),
            defaults.tool_timeout.as_secs(),
        )?),
    })
}

/// `0` is the documented "no cap" sentinel; abycore rejects a literal zero.
fn cap_or_unlimited(value: usize) -> usize {
    if value == 0 {
        abylab_backend::TurnLimits::UNLIMITED
    } else {
        value
    }
}

/// Resolve the host compaction policy from the environment. Without a context
/// window the policy is `None`, so automatic compaction stays off and only
/// `/compact` condenses.
fn resolve_compaction() -> Result<Option<abylab_backend::CompactionConfig>> {
    let Some(raw) = std::env::var("ABY_CONTEXT_WINDOW")
        .ok()
        .filter(|raw| !raw.trim().is_empty())
    else {
        return Ok(None);
    };
    let context_window = raw
        .trim()
        .parse::<u64>()
        .with_context(|| "ABY_CONTEXT_WINDOW must be a positive token count")?;
    let compact_at = pick_ratio(
        "ABY_COMPACT_AT",
        std::env::var("ABY_COMPACT_AT").ok(),
        "0.8",
    )?;
    let keep_recent = pick_ratio(
        "ABY_KEEP_RECENT",
        std::env::var("ABY_KEEP_RECENT").ok(),
        "0.16",
    )?;
    let prune_tool_bytes = pick_limit(
        "ABY_PRUNE_TOOL_OUTPUT",
        std::env::var("ABY_PRUNE_TOOL_OUTPUT").ok(),
        abylab_backend::CompactionConfig::default().prune_tool_bytes,
    )?;
    compaction_policy(context_window, compact_at, keep_recent, prune_tool_bytes).map(Some)
}

/// Validate and assemble one compaction policy; the harness defaults are
/// condense at 80% of the window and keep the newest 16% verbatim.
fn compaction_policy(
    context_window: u64,
    compact_at: f64,
    keep_recent: f64,
    prune_tool_bytes: usize,
) -> Result<abylab_backend::CompactionConfig> {
    if context_window == 0 {
        bail!("ABY_CONTEXT_WINDOW must be at least 1 token");
    }
    if compact_at > 0.0 && keep_recent >= compact_at {
        bail!("ABY_KEEP_RECENT must be below ABY_COMPACT_AT");
    }
    if prune_tool_bytes > 0 && prune_tool_bytes < abylab_backend::MIN_PRUNE_BYTES {
        bail!("ABY_PRUNE_TOOL_OUTPUT must be 0 (disabled) or at least 64 bytes");
    }
    Ok(abylab_backend::CompactionConfig {
        context_window,
        compact_at,
        keep_recent,
        max_tokens: abylab_backend::CompactionConfig::default().max_tokens,
        prune_tool_bytes,
    })
}

/// A compaction ratio from the environment, else the documented default.
/// Ratios are strictly inside (0, 1]; `0` disables the retention floor or the
/// threshold.
fn pick_ratio(name: &str, env: Option<String>, default: &str) -> Result<f64> {
    let raw = env
        .filter(|raw| !raw.trim().is_empty())
        .unwrap_or_else(|| default.to_string());
    let value = raw
        .trim()
        .parse::<f64>()
        .with_context(|| format!("{name} must be a number between 0 and 1"))?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("{name} must be between 0 and 1");
    }
    Ok(value)
}

/// One positive-seconds setting: environment > default; zero is refused because
/// abycore rejects a zero deadline.
fn pick_positive(name: &str, env: Option<String>, default: u64) -> Result<u64> {
    let value = pick_limit(name, env, default as usize)?;
    if value == 0 {
        bail!("{name} must be at least 1 second");
    }
    Ok(value as u64)
}

/// One limit: a non-blank environment value, else the built-in default.
fn pick_limit(name: &str, env: Option<String>, default: usize) -> Result<usize> {
    match env {
        Some(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse()
            .with_context(|| format!("{name} must be a non-negative number")),
        _ => Ok(default),
    }
}

fn main() -> Result<()> {
    // Keep Rust's default SIGPIPE ignore behavior: a clipboard helper that
    // closes stdin must return BrokenPipe, letting copying fall back without
    // terminating the UI and its running agents.
    let args = parse_args()?;

    // `--uninstall` never reaches the screen: it lists what it would delete,
    // takes the answers on the terminal and exits.
    if args.uninstall {
        return run_uninstall(&args, &aby_home());
    }

    let cfg = build_config(&args)?;
    let limits = resolve_limits()?;
    let compaction = resolve_compaction()?;
    let session_id = args
        .session_id
        .clone()
        .unwrap_or_else(|| format!("aby-{}", app::timestamp()));

    let (bus_tx, bus_rx) = mpsc::channel::<AppEvent>();
    install_termination_handler(bus_tx.clone())?;

    // The abycore driver drives turns in-process.
    let controller = Controller::start_aby(
        cfg.clone(),
        session_id.clone(),
        bus_tx.clone(),
        limits,
        compaction,
    );
    // Appearance: flag > persisted > dark. The persisted palette pack is
    // applied by `App::new` from the same settings file.
    let home = aby_home().to_string_lossy().into_owned();
    let settings = UiSettings::load(&home);
    let theme = ui::theme_for(
        args.theme
            .as_deref()
            .or(settings.theme.as_deref())
            .unwrap_or("dark"),
    );
    let mut app = App::new(theme, cfg, session_id);
    // The launch splash: the wordmark, the project URL and the launch facts,
    // and nothing after them. The usage hint that used to trail the splash now
    // waits for a session the user opens (`/new`).
    app.push_banner();
    // No key at boot: guide the user to the platform and /login before the
    // first prompt (the driver's own error line stays as the short fact).
    if !app.cfg.has_credentials() {
        app.push_no_key_onboarding();
    }
    // Kitty-graphics image thumbnails in the chat scrollback (PNG only).
    let mut thumbnails = pet::Thumbnails::new();

    // input pump
    {
        let tx = bus_tx.clone();
        std::thread::Builder::new()
            .name("input".into())
            .spawn(move || loop {
                match crossterm::event::read() {
                    Ok(crossterm::event::Event::Key(key)) => {
                        // Recover terminal-lost physical modifiers at read
                        // time, before a quick key release can race the UI
                        // event queue.
                        let ev = crossterm::event::Event::Key(crate::input::rescue_key(key));
                        if tx.send(AppEvent::Term(ev)).is_err() {
                            break;
                        }
                    }
                    Ok(ev) => {
                        if tx.send(AppEvent::Term(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            })
            .expect("spawn input thread");
    }

    // terminal guard
    enter_tui()?;
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));

    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;

    if let Ok(auto) = std::env::var("ABYLAB_AUTOPROMPT") {
        if !auto.trim().is_empty() {
            app.auto_prompt(&auto, &controller);
        }
    }
    controller.send(bus::Cmd::FetchSkills);

    let run = (|| -> Result<()> {
        let mut last_tick = std::time::Instant::now();
        loop {
            if app.needs_redraw {
                // One atomic frame. Synchronized updates (DECSET 2026) make
                // supporting terminals apply the whole frame without tearing.
                // The caret itself is painted as buffer cells (ui::paint_caret)
                // and the hardware cursor stays hidden for the whole session
                // (enter_tui), so no frame-time Hide/Show is needed — the old
                // per-frame Hide kept the cursor off for the entire diff
                // write, which read as flicker in the input well whenever a
                // scroll redraw rewrote the transcript pane. The hidden
                // hardware cursor is still *repositioned* onto the caret cell
                // after every draw: terminals anchor IME candidate popups at
                // the cursor, and the frame diff would otherwise leave it
                // wherever the last cell write happened.
                std::io::stdout().sync_update(|out| -> Result<()> {
                    terminal.draw(|f| ui::draw(f, &mut app))?;
                    app.needs_redraw = false;
                    // Sync image thumbnails (chat + composer attachment strip)
                    // against the freshly drawn viewport.
                    let shots: Vec<pet::ThumbShot> = app
                        .chat_view
                        .images
                        .iter()
                        .chain(app.att_thumbs.iter())
                        .map(|t| pet::ThumbShot {
                            id: t.id,
                            rect: t.rect,
                            data: t.data.as_ref(),
                        })
                        .collect();
                    let _ = thumbnails.sync(out, &shots);
                    // Park the (hidden) hardware cursor on the caret cell so
                    // IME composition/candidate popups anchor at the caret.
                    // Last on purpose: the kitty writers above move the
                    // cursor around while placing images.
                    if let Some((col, row)) = app.caret_cell {
                        let _ = execute!(out, MoveTo(col, row));
                    }
                    Ok(())
                })??;
            }
            match bus_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) => {
                    app.handle(ev, &controller);
                    // drain whatever is queued to batch redraws
                    while let Ok(ev) = bus_rx.try_recv() {
                        app.handle(ev, &controller);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if last_tick.elapsed() >= Duration::from_millis(100) {
                app.tick();
                last_tick = std::time::Instant::now();
            }
            if app.quit {
                break;
            }
        }
        Ok(())
    })();

    controller.send(Cmd::Shutdown);
    restore_terminal();
    // Back on the normal screen: the session id and the two ways back, in a
    // ruled band. The app owns the wording (locale + the session it ended on),
    // the shell writes it, so it survives the alternate screen (see
    // `App::exit_notice`). The width is still the terminal's — this is the last
    // chance to ask before the process is gone.
    let width = crossterm::terminal::size().map_or(48, |(w, _)| w as usize);
    println!("{}", app.exit_notice(width));
    run
}

fn enter_tui() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Hide once up front: the caret is drawn as buffer cells, and the first
    // frame's diff must not paint with a visible hardware cursor.
    execute!(
        stdout,
        EnterAlternateScreen,
        Hide,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    // Kitty keyboard protocol, pushed blind: terminals that support it
    // (ghostty · kitty · wezterm · iterm2 3.5+) start reporting ⌘/⌥ chords
    // as real SUPER/ALT modifiers and make shift+enter distinguishable;
    // everything else ignores the sequence. (No capability query — the
    // input thread already owns the event stream, a query reply would race.)
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    Ok(())
}

#[cfg(unix)]
fn install_termination_handler(tx: mpsc::Sender<AppEvent>) -> Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ])?;
    std::thread::Builder::new()
        .name("termination-signal".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                let _ = tx.send(AppEvent::Terminate);
            }
        })
        .context("spawn termination signal handler")?;
    Ok(())
}

#[cfg(not(unix))]
fn install_termination_handler(_tx: mpsc::Sender<AppEvent>) -> Result<()> {
    Ok(())
}

fn restore_terminal() {
    let mut stdout = std::io::stdout();
    if pet::kitty_supported() {
        // Drop any kitty placement (panic-safe: also runs from the hook).
        let _ = stdout.write_all(pet::KITTY_DELETE_ALL.as_bytes());
    }
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    let _ = execute!(
        stdout,
        Show,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = stdout.flush();
}

#[cfg(test)]
mod cli_args_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn abylab_home_precedence_owns_the_default_home() {
        assert_eq!(
            crate::runtime::aby_home_from(Some("/opt/abylab"), "/Users/test"),
            PathBuf::from("/opt/abylab")
        );
        assert_eq!(
            crate::runtime::aby_home_from(None, "/Users/test"),
            PathBuf::from("/Users/test/.abylab")
        );
        assert_eq!(
            crate::runtime::aby_home_from(None, "/Users/test").join("sessions"),
            PathBuf::from("/Users/test/.abylab/sessions")
        );
    }

    #[test]
    fn removed_runtime_aliases_are_rejected() {
        for flag in ["--runtime-bin", "--cordis"] {
            let err = match parse_args_from([flag.into(), "legacy".into()]) {
                Ok(_) => panic!("{flag} unexpectedly remained accepted"),
                Err(err) => err,
            };
            assert!(
                err.to_string().contains("unknown argument"),
                "{flag} must not remain as a hidden legacy option: {err:#}"
            );
        }
    }

    /// Tuning is environment-only now: the former flags are plain errors.
    #[test]
    fn removed_tuning_flags_are_rejected() {
        for flag in [
            "--provider",
            "--max-tokens",
            "--max-requests",
            "--max-tool-calls",
            "--auto-continue",
            "--turn-timeout",
            "--tool-timeout",
            "--context-window",
            "--compact-at",
            "--keep-recent",
            "--prune-tool-output",
        ] {
            let err = match parse_args_from([flag.into(), "1".into()]) {
                Ok(_) => panic!("{flag} unexpectedly remained accepted"),
                Err(err) => err,
            };
            assert!(
                err.to_string().contains("unknown argument"),
                "{flag} must not remain as a hidden option: {err:#}"
            );
        }
    }

    #[test]
    fn limit_resolution_prefers_env_then_default() {
        assert_eq!(pick_limit("X", Some("9".into()), 1).unwrap(), 9);
        assert_eq!(pick_limit("X", Some("  ".into()), 1).unwrap(), 1);
        assert_eq!(pick_limit("X", None, 1).unwrap(), 1);
        let err = pick_limit("ABY_MAX_REQUESTS", Some("lots".into()), 1)
            .expect_err("non-numeric env is rejected");
        assert!(
            err.to_string().contains("ABY_MAX_REQUESTS"),
            "the failing variable is named: {err:#}"
        );
    }

    /// `0` is the documented "no cap" sentinel; abycore rejects a literal zero,
    /// so it must reach the driver as `usize::MAX`.
    #[test]
    fn zero_caps_mean_unlimited() {
        assert_eq!(cap_or_unlimited(0), abylab_backend::TurnLimits::UNLIMITED);
        assert_eq!(cap_or_unlimited(7), 7);
    }

    /// The one timeout abylab exposes is the backstop for a tool call that
    /// declares no budget of its own; a zero one is refused here rather than by
    /// abycore, and a run has no window at all.
    #[test]
    fn the_tool_backstop_resolves_from_env_with_zero_refused() {
        assert_eq!(pick_positive("ABY_TOOL_TIMEOUT", None, 60).unwrap(), 60);
        assert_eq!(
            pick_positive("ABY_TOOL_TIMEOUT", Some("600".into()), 60).unwrap(),
            600
        );
        assert!(
            pick_positive("ABY_TOOL_TIMEOUT", Some("0".into()), 60).is_err(),
            "a zero backstop is refused before abycore sees it"
        );
        assert_eq!(
            abylab_backend::TurnLimits::default().run_timeout,
            None,
            "no run window: requests, tool budgets and the caps above bound a turn"
        );
        assert_eq!(resolve_limits().unwrap().run_timeout, None);
    }

    /// The harness defaults: condense at 80% of the window and keep the newest
    /// 16% verbatim.
    #[test]
    fn compaction_policy_resolves_thresholds() {
        let policy = compaction_policy(64_000, 0.7, 0.2, 4096).expect("policy is enabled");
        assert_eq!(policy.context_window, 64_000);
        assert_eq!(policy.threshold_tokens(), 44_800);
        assert_eq!(policy.retained_tokens(), 12_800);
        assert_eq!(policy.max_tokens, 8_192);
    }

    #[test]
    fn a_retention_above_the_threshold_is_rejected() {
        let err = compaction_policy(1000, 0.5, 0.8, 4096)
            .expect_err("retention must stay below the threshold");
        assert!(err.to_string().contains("ABY_KEEP_RECENT"), "{err:#}");
    }

    /// `--uninstall` is the one maintenance mode: `--keep-data` and `--yes`
    /// belong to it and nothing else, and the help names them (the only place
    /// a user learns the flags exist).
    #[test]
    fn uninstall_flags_parse_and_only_pair_with_each_other() {
        assert!(
            HELP.contains("--uninstall") && HELP.contains("--keep-data"),
            "{HELP}"
        );
        let args = parse_args_from(["--uninstall".into()]).expect("--uninstall parses");
        assert!(
            args.uninstall && !args.keep_data && !args.yes,
            "the prompts are the default"
        );
        let args = parse_args_from(["--uninstall".into(), "--keep-data".into(), "--yes".into()])
            .expect("the pair parses");
        assert!(args.uninstall && args.keep_data && args.yes);
        let args = parse_args_from([
            "--uninstall".into(),
            "--session-root".into(),
            "/srv/store".into(),
        ])
        .expect("the store can be named");
        assert_eq!(args.sessions_root.as_deref(), Some("/srv/store"));
        // `--keep-data` alone is a typo, not a mode.
        let err = match parse_args_from(["--keep-data".into()]) {
            Ok(_) => panic!("--keep-data without --uninstall must be refused"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("--keep-data"), "{err:#}");
        // `--yes` alone is a typo too: it must not read as a launch.
        let err = match parse_args_from(["--yes".into()]) {
            Ok(_) => panic!("--yes without --uninstall must be refused"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("--yes"), "{err:#}");
    }

    /// The plan reads as a contract here too: the data half and the program
    /// half in one list, and `--keep-data` replaces the data rows with a keep
    /// row instead of hiding the decision.
    #[test]
    fn the_cli_uninstall_plan_names_both_halves() {
        let home = data_home("uninstall-plan");
        let root = std::env::temp_dir().join(format!(
            "aby-cli-uninstall-plan-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let bin_dir = root.join("bin");
        let _ = std::fs::create_dir_all(&bin_dir);
        std::fs::write(bin_dir.join("abylab"), b"fake binary").expect("bin");
        let program = uninstall::scan(&uninstall::ScanInput {
            current_exe: None,
            dirs: vec![bin_dir.clone()],
            cargo_bin: None,
        });
        let data = data::scan(&home, &home.join("sessions"));
        let text = uninstall_plan_text(&data, &program, false);
        assert!(text.contains("settings.json"), "{text}");
        assert!(text.contains("binary") && text.contains("abylab"), "{text}");
        assert!(text.contains("AGENTS.md"), "the kept file is named: {text}");

        let text = uninstall_plan_text(&data, &program, true);
        assert!(text.contains("--keep-data"), "{text}");
        assert!(
            !text.contains("settings.json"),
            "kept data is not listed as removal: {text}"
        );

        // The report names cargo's side either way.
        let outcome = uninstall::ProgramOutcome {
            removed: 2,
            bytes: 8 * 1024 * 1024,
            cargo_uninstalled: true,
            cargo_failed: false,
            failures: vec![],
        };
        let text = program_outcome_text(&outcome);
        assert!(text.contains("removed 2 binaries — 8.0 MB"), "{text}");
        assert!(text.contains("cargo uninstall abylab-tui"), "{text}");
        let outcome = uninstall::ProgramOutcome {
            cargo_failed: true,
            ..outcome
        };
        assert!(
            program_outcome_text(&outcome).contains("did not work"),
            "a cargo fallback is reported"
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `--yes` uninstall with the environment pinned: the data half and the
    /// program half both go, and `--keep-data` leaves the home standing while
    /// the program still goes. Nothing here reads the real environment.
    #[test]
    fn a_yes_uninstall_removes_both_halves_and_keep_data_only_the_program() {
        let home = data_home("uninstall-yes");
        let root = std::env::temp_dir().join(format!(
            "aby-cli-uninstall-yes-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let bin_dir = root.join("bin");
        let _ = std::fs::create_dir_all(&bin_dir);
        let program_input = uninstall::ScanInput {
            current_exe: None,
            dirs: vec![bin_dir.clone()],
            cargo_bin: None,
        };
        let arm = |home: &Path| {
            std::fs::write(bin_dir.join("abylab"), b"fake binary").expect("bin");
            let mut args = parse_args_from(["--uninstall".into(), "--yes".into()]).expect("args");
            args.sessions_root = Some(home.join("sessions").to_string_lossy().into_owned());
            args
        };

        let args = arm(&home);
        run_uninstall_with(&args, &home, &program_input).expect("--yes does not ask");
        assert!(!home.join("settings.json").exists(), "the data half ran");
        assert!(
            home.join("AGENTS.md").exists(),
            "the hand-written file stays"
        );
        assert!(home.exists(), "the home itself stays");
        assert!(!bin_dir.join("abylab").exists(), "the binary went");

        let home = data_home("uninstall-keep");
        let mut args = arm(&home);
        args.keep_data = true;
        run_uninstall_with(&args, &home, &program_input).expect("--keep-data does not ask");
        assert!(
            home.join("settings.json").exists(),
            "--keep-data leaves the saved data alone"
        );
        assert!(!bin_dir.join("abylab").exists(), "the program still goes");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The data half reads as a contract too: one `remove` row per entry with
    /// what is inside, the paths the wipe does not own, and a partial wipe
    /// reported per path.
    #[test]
    fn the_data_rows_and_report_name_what_goes_and_what_stays() {
        let home = data_home("plan");
        let plan = data::scan(&home, &home.join("sessions"));
        let rows = format!("{}{}", data_item_rows(&plan), data_kept_rows(&plan));
        assert!(
            rows.contains("remove") && rows.contains("settings.json"),
            "{rows}"
        );
        assert!(
            rows.contains("settings · 1 file · 17 B"),
            "the size is part of the plan: {rows}"
        );
        assert!(
            rows.contains("keep") && rows.contains("AGENTS.md"),
            "the hand-written file is reported: {rows}"
        );

        // An outside store is reported as kept instead of removed.
        let outside = std::env::temp_dir().join(format!("aby-cli-store-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&outside);
        let kept = data_kept_rows(&data::scan(&home, &outside));
        assert!(kept.contains(&outside.display().to_string()), "{kept}");
        assert!(
            kept.contains("session store outside the aby home"),
            "{kept}"
        );

        // A partial wipe is reported per path, never rounded up.
        let outcome = data::DataOutcome {
            removed: 1,
            files: 1,
            bytes: 27,
            failures: vec![(
                home.join("sessions"),
                "Permission denied (os error 13)".into(),
            )],
        };
        let text = data_outcome_text(&outcome);
        assert!(text.contains("removed 1 entry — 1 file · 27 B"), "{text}");
        assert!(text.contains("could not remove"), "{text}");
        assert!(text.contains("Permission denied"), "{text}");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A home with every entry this program owns, plus one file it does not.
    fn data_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "aby-cli-data-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("sessions/ws-abc/aby-1")).expect("home");
        std::fs::write(home.join("settings.json"), "{\"language\":\"en\"}").expect("settings");
        std::fs::write(home.join("abylab-modes.json"), "{}").expect("modes");
        std::fs::write(home.join(".credentials.yaml"), "version: 1\n").expect("key");
        std::fs::write(home.join("sessions/ws-abc/aby-1/session.jsonl"), "log").expect("log");
        std::fs::write(home.join("AGENTS.md"), "be terse").expect("instructions");
        home
    }

    /// The harness analogy: its agent loop has no turn budget, and the caps it
    /// does ship are explicit runaway backstops (workflow `maxTotalAgents`
    /// defaults to 1000). abylab's defaults must not be reachable by a normal
    /// turn, and a budget stop must still resume the turn.
    #[test]
    fn defaults_are_a_runaway_backstop_not_a_turn_limit() {
        let limits = abylab_backend::TurnLimits::default();
        assert!(limits.max_requests >= 1000, "{limits:?}");
        assert!(limits.max_tool_calls >= 1000, "{limits:?}");
        assert!(limits.continuations > 0, "{limits:?}");
        assert_eq!(limits, abylab_backend::TurnLimits::watchdog());
    }
}
