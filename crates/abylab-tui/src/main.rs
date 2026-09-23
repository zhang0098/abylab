//! abylab — terminal-native agent harness UI.

mod app;
mod attachments;
mod bus;
mod clipboard;
mod controller;
mod credentials;

mod events;
mod file_ref;
mod input;
mod locale;
mod markdown;
mod pet;
mod reset;
mod runtime;
mod slots;
mod theme;
mod transcript;
mod ui;

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
      --reset               delete everything abylab saved under $ABYLAB_HOME
                            (settings, stored key, session logs, queued prompts)
                            and exit; lists what goes and asks on the terminal
                            first. Other launch options are ignored, except
                            --session-root, which names the store it checks
      --yes                 with --reset: do not ask
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
    /// `--reset`: wipe the saved state under the aby home and exit.
    reset: bool,
    /// `--yes`: skip `--reset`'s terminal prompt.
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
        reset: false,
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
            "--reset" => args_out.reset = true,
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
    if args_out.yes && !args_out.reset {
        bail!("--yes only means something with --reset (see --help)");
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

/// `abylab --reset`: the CLI side of `/reset`, for a shell that never opened
/// the TUI (a script, an upgrade, a machine being handed on).
///
/// It lists what it would remove — the plan is the prompt — and removes
/// nothing before a "y" at the terminal, unless `--yes` says the caller
/// already asked (uninstall.sh's shape, for the same reason). The report is
/// English, like every other line this binary prints before the TUI starts.
fn run_reset(args: &Args, home: &Path) -> Result<()> {
    let sessions_root = args
        .sessions_root
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(default_sessions_root);
    let plan = reset::scan(home, &sessions_root);
    print!("{}", reset_plan_text(&plan));
    if plan.is_empty() {
        println!(
            "\nnothing to remove — {} holds nothing this program saved\n",
            home.display()
        );
        return Ok(());
    }
    if !args.yes && !confirm_on_terminal(home)? {
        println!("\nnothing removed\n");
        return Ok(());
    }
    let outcome = reset::wipe(&plan);
    print!("{}", reset_outcome_text(&outcome));
    if outcome.failures.is_empty() {
        return Ok(());
    }
    bail!(
        "reset did not finish — {} path(s) could not be removed (see above)",
        outcome.failures.len()
    );
}

/// The plan as the terminal reads it: one `remove` row per entry with what is
/// inside, then the paths this command does *not* own. Anything not listed
/// here is not touched either — the list is the contract.
fn reset_plan_text(plan: &reset::ResetPlan) -> String {
    use crate::reset::{human_bytes, human_files};

    let mut out = format!(
        "abylab reset — everything this program saved under {}\n\n",
        plan.home.display()
    );
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
    for keep in &plan.kept {
        out.push_str(&format!(
            "  keep    {}  ({})\n",
            keep.path.display(),
            keep.why.note(crate::locale::Locale::En),
        ));
    }
    if !plan.is_empty() && other_instance_running() {
        out.push_str(
            "  note    another abylab is running — quit it first, or it writes its\n\
             \x20         settings and session log back after this\n",
        );
    }
    out
}

/// What the wipe did, with every path that refused to go named on its own
/// line: a partial reset is reported, never rounded up to a success.
fn reset_outcome_text(outcome: &reset::ResetOutcome) -> String {
    let plural =
        |count: u64, one: &str, many: &str| if count == 1 { one } else { many }.to_string();
    let mut out = format!(
        "\nremoved {} {} — {} {} · {}\n",
        outcome.removed,
        plural(outcome.removed as u64, "entry", "entries"),
        outcome.files,
        plural(outcome.files, "file", "files"),
        reset::human_bytes(outcome.bytes),
    );
    for (path, reason) in &outcome.failures {
        out.push_str(&format!(
            "warning could not remove {} ({reason})\n",
            path.display()
        ));
    }
    out
}

/// Ask on the terminal (not stdin: a caller may pipe something into abylab,
/// and the prompt must not eat it). No terminal to ask on is an error, not a
/// silent "yes".
fn confirm_on_terminal(home: &Path) -> Result<bool> {
    let reader: Box<dyn std::io::BufRead> = match std::fs::File::open("/dev/tty") {
        Ok(tty) => Box::new(std::io::BufReader::new(tty)),
        Err(error) if cfg!(unix) => bail!(
            "there is no terminal to ask on ({error}) — re-run with --yes if {} should \
             be emptied without a prompt",
            home.display()
        ),
        Err(_) => Box::new(std::io::BufReader::new(std::io::stdin())),
    };
    eprint!("\nRemove everything above? [y/N] ");
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
        run_timeout: Duration::from_secs(pick_positive(
            "ABY_TURN_TIMEOUT",
            std::env::var("ABY_TURN_TIMEOUT").ok(),
            defaults.run_timeout.as_secs(),
        )?),
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
    // Die quietly on closed pipes (dsh-tui --dump-frame | head) instead of
    // panicking in println!.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args = parse_args()?;

    // `--reset` never reaches the screen: it lists what it would delete, takes
    // the answer on the terminal and exits (the TUI's `/reset` does the same
    // inside the app).
    if args.reset {
        return run_reset(&args, &aby_home());
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

    /// The deadlines abylab exposes are the limits that bind in practice once
    /// the request/tool budgets are backstops.
    #[test]
    fn deadlines_resolve_from_env_with_zero_refused() {
        assert_eq!(
            pick_positive("ABY_TURN_TIMEOUT", Some("120".into()), 600).unwrap(),
            120
        );
        assert_eq!(pick_positive("ABY_TOOL_TIMEOUT", None, 60).unwrap(), 60);
        assert!(
            pick_positive("ABY_TURN_TIMEOUT", Some("0".into()), 600).is_err(),
            "a zero deadline is refused before abycore sees it"
        );
        assert_eq!(
            abylab_backend::TurnLimits::default().run_timeout,
            Duration::from_secs(600),
            "abycore's SDK default stays the fallback"
        );
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

    /// `--reset` is a mode of its own, `--yes` only belongs to it, and the
    /// help names both (the only place a user learns the flag exists).
    #[test]
    fn reset_flags_parse_and_only_pair_with_each_other() {
        assert!(HELP.contains("--reset") && HELP.contains("--yes"), "{HELP}");
        let args = parse_args_from(["--reset".into()]).expect("--reset parses");
        assert!(args.reset && !args.yes, "the prompt is the default");
        let args = parse_args_from(["--reset".into(), "--yes".into()]).expect("both parse");
        assert!(args.reset && args.yes);
        let args = parse_args_from([
            "--reset".into(),
            "--session-root".into(),
            "/srv/store".into(),
        ])
        .expect("the store can be named");
        assert_eq!(args.sessions_root.as_deref(), Some("/srv/store"));
        // `--yes` alone is a typo, not a mode: it must not read as a launch.
        let err = match parse_args_from(["--yes".into()]) {
            Ok(_) => panic!("--yes without --reset must be refused"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("--yes"), "{err:#}");
    }

    /// The plan reads as a contract: one `remove` row per entry with what is
    /// inside, then the paths this command does not own.
    #[test]
    fn the_cli_plan_names_what_goes_and_what_stays() {
        let home = reset_home("plan");
        let plan = reset::scan(&home, &home.join("sessions"));
        let text = reset_plan_text(&plan);
        assert!(
            text.contains(&format!("under {}", home.display())),
            "the home is named: {text}"
        );
        assert!(
            text.contains("remove") && text.contains("settings.json"),
            "{text}"
        );
        assert!(
            text.contains("settings · 1 file · 17 B"),
            "the size is part of the prompt: {text}"
        );
        assert!(
            text.contains("keep") && text.contains("AGENTS.md"),
            "the hand-written file is reported: {text}"
        );

        // An outside store is reported as kept instead of removed.
        let outside = std::env::temp_dir().join(format!("aby-cli-store-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&outside);
        let text = reset_plan_text(&reset::scan(&home, &outside));
        assert!(text.contains(&outside.display().to_string()), "{text}");
        assert!(
            text.contains("session store outside the aby home"),
            "{text}"
        );

        // A partial wipe is reported per path, never rounded up.
        let outcome = reset::ResetOutcome {
            removed: 1,
            files: 1,
            bytes: 27,
            failures: vec![(
                home.join("sessions"),
                "Permission denied (os error 13)".into(),
            )],
        };
        let text = reset_outcome_text(&outcome);
        assert!(text.contains("removed 1 entry — 1 file · 27 B"), "{text}");
        assert!(text.contains("could not remove"), "{text}");
        assert!(text.contains("Permission denied"), "{text}");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// `--yes` is the scripted path: no terminal, no question, and every entry
    /// the plan listed is gone while the rest of the home stays.
    #[test]
    fn a_yes_reset_wipes_without_a_terminal() {
        let home = reset_home("yes");
        let mut args = parse_args_from(["--reset".into(), "--yes".into()]).expect("args");
        args.sessions_root = Some(home.join("sessions").to_string_lossy().into_owned());
        run_reset(&args, &home).expect("--yes does not ask");
        for name in [
            "settings.json",
            "abylab-modes.json",
            ".credentials.yaml",
            "sessions",
            "queued",
        ] {
            assert!(!home.join(name).exists(), "{name} should be gone");
        }
        assert!(home.join("AGENTS.md").exists(), "the home's own file stays");
        assert!(home.exists(), "the home itself stays");

        // A second run has nothing to do and still exits clean.
        run_reset(&args, &home).expect("an empty home is not an error");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A home with every entry this program owns, plus one file it does not.
    fn reset_home(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!(
            "aby-cli-reset-{tag}-{}-{:x}",
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
