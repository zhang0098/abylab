//! Paid probe: which tools the model actually reaches for on read-and-search
//! work, against this checkout.
//!
//! The prompts name no tool and no shell command, so the sequence is the
//! model's own choice under whatever system prompt the crate currently ships.
//! The per-tool counters and the bash classifier mirror the offline analysis of
//! `~/.abylab/sessions`, so the two are directly comparable.
//!
//! Since the search tools were removed, the question this probe answers is how
//! much of the work lands on `read` versus shelling out — the bash marks below
//! count how many of those shell calls are searching or paging files, which is
//! the behavior the shared prompt now asks for.
//!
//! Run one arm:
//!
//! ```sh
//! PROBE_ARM=v2 DEEPSEEK_API_KEY=... \
//!   cargo test -p abylab-backend --test live_tool_choice -- --ignored --nocapture
//! ```
//!
//! Every session is fresh and persists under a probe-owned store, so the
//! user's own sessions are untouched. Permission is the shipped default
//! (`danger-full-access`), so a bash call never waits on an approval that
//! nobody can answer.

use abylab_backend::driver::spawn;
use abylab_backend::{Cmd, CtlEvent, DriverConfig, Event, PermissionReply, TurnLimits, UiEvent};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Read-and-search tasks in this checkout: two "where is this mentioned", one
/// "what does this code say", one value lookup, one verbatim quote. Neutral
/// wording throughout — no tool name, no shell command.
const PROMPTS: [&str; 5] = [
    "这个仓库里哪些文件提到了 ABY_TOOL_TIMEOUT？列出文件路径，不要改任何文件。",
    "crates/abycore/src/local_tools.rs 里，LocalToolConfig 的 max_read_bytes 和 max_output_bytes 默认值分别是多少？给出具体数值。",
    "crates/abylab-backend/src/instructions.rs 里，工作区指令是从哪些文件名里发现的？用两三句话说明。",
    "crates/abycore/src/local_tools/bash.rs 里后台作业（run_in_background）是怎么实现的？简要说明关键函数。",
    "README.md 里的按键说明一节列了哪些键？把原文抄给我。",
];

struct Call {
    name: String,
    arguments: String,
}

#[test]
#[ignore = "paid probe; requires DEEPSEEK_API_KEY"]
fn which_tools_the_model_reaches_for() {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY explicitly");
    let base_url = std::env::var("DEEPSEEK_BASE_URL").ok();
    let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-flash".into());
    let arm = std::env::var("PROBE_ARM").unwrap_or_else(|_| "unlabelled".into());
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let store = std::env::temp_dir().join(format!("abylab-probe-{arm}-{}", std::process::id()));
    std::fs::create_dir_all(&store).expect("probe session store");

    println!(
        "\n=== abylab tool-choice probe: arm `{arm}`, model `{model}`, {} prompts ===",
        PROMPTS.len()
    );

    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    let mut bash_marks: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut bash_total = 0usize;
    let mut asks = 0usize;
    let mut sessions = 0usize;

    for (index, prompt) in PROMPTS.iter().enumerate() {
        let calls: Arc<Mutex<Vec<Call>>> = Arc::new(Mutex::new(Vec::new()));
        let turn_end = Arc::new(AtomicUsize::new(0));
        let failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let permission_asks = Arc::new(AtomicUsize::new(0));
        let sink = {
            let calls = Arc::clone(&calls);
            let turn_end = Arc::clone(&turn_end);
            let failures = Arc::clone(&failures);
            let permission_asks = Arc::clone(&permission_asks);
            move |event: Event| match event {
                Event::Ui(UiEvent::ToolCall {
                    name, arguments, ..
                }) => calls
                    .lock()
                    .expect("call lock")
                    .push(Call { name, arguments }),
                Event::Ui(UiEvent::TurnEnd { kind, .. }) => {
                    if kind != "completed" {
                        failures.lock().expect("failure lock").push(kind);
                    }
                    turn_end.fetch_add(1, Ordering::SeqCst);
                }
                Event::Ctl(CtlEvent::Error(message)) => {
                    failures.lock().expect("failure lock").push(message);
                    turn_end.fetch_add(1, Ordering::SeqCst);
                }
                Event::PermissionAsk { reply, .. } => {
                    permission_asks.fetch_add(1, Ordering::SeqCst);
                    let _ = reply.send(PermissionReply::Selected("allow".into()));
                }
                _ => {}
            }
        };
        let handle = spawn(
            DriverConfig {
                session_id: format!("probe-{arm}-{index}"),
                resume: None,
                sessions_root: Some(store.to_string_lossy().into_owned()),
                home: None,
                workspace: workspace.to_string_lossy().into_owned(),
                model: model.clone(),
                reasoning: "max".into(),
                permission: None,
                max_tokens: None,
                api_key: Some(key.clone()),
                base_url: base_url.clone(),
                limits: TurnLimits {
                    max_requests: 12,
                    max_tool_calls: 24,
                    continuations: 1,
                    run_timeout: Duration::from_secs(300),
                    tool_timeout: Duration::from_secs(60),
                },
                compaction: None,
            },
            sink,
        )
        .expect("driver spawns");
        handle.send(Cmd::Prompt {
            text: (*prompt).to_string(),
        });
        let deadline = Instant::now() + Duration::from_secs(420);
        while turn_end.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "prompt {index} never finished");
            std::thread::sleep(Duration::from_millis(100));
        }
        asks += permission_asks.load(Ordering::SeqCst);
        handle.shutdown();

        let calls = calls.lock().expect("call lock");
        for call in calls.iter() {
            *totals.entry(call.name.clone()).or_default() += 1;
            if call.name == "bash" {
                bash_total += 1;
                let command = serde_json::from_str::<Value>(&call.arguments)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("command")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                for (mark, hit) in bash_marks_of(&command) {
                    if hit {
                        *bash_marks.entry(mark).or_default() += 1;
                    }
                }
            }
        }
        let failures = failures.lock().expect("failure lock");
        println!("\n[{index}] {prompt}");
        for call in calls.iter() {
            println!("    {:<6} {}", call.name, preview(&call.arguments));
        }
        if !failures.is_empty() {
            println!("    !! {failures:?}");
        }
        sessions += 1;
    }

    let total: usize = totals.values().sum();
    println!(
        "\n--- arm `{arm}`: {sessions} sessions, {total} tool calls, {asks} approval asks ---"
    );
    for (name, count) in totals.iter().rev() {
        println!(
            "    {count:4}  {name:<14} {:5.1}%",
            *count as f64 * 100.0 / total.max(1) as f64
        );
    }
    println!("    bash marks (of {bash_total}): {bash_marks:?}");
    let reads = totals.get("read").copied().unwrap_or(0);
    println!(
        "    read share: {reads}/{total} = {:.1}%",
        reads as f64 * 100.0 / total.max(1) as f64
    );
    println!("    system prompt in force: {}", prompt_in_force(&store));
    let _ = std::fs::remove_dir_all(&store);
}

/// The same classifier the offline `~/.abylab/sessions` analysis used, so a
/// probed bash call and a historical one are counted the same way.
fn bash_marks_of(command: &str) -> [(&'static str, bool); 5] {
    let leading = command.trim_start();
    [
        (
            "leads with rg/grep/find/fd",
            ["rg ", "grep ", "find ", "fd "]
                .iter()
                .any(|bin| leading.starts_with(bin)),
        ),
        ("pipes into grep", command.contains("| grep")),
        ("pipes into head", command.contains("| head")),
        (
            "sed -n / awk",
            command.contains("sed -n") || command.contains("awk "),
        ),
        ("plain cat", leading.starts_with("cat ")),
    ]
}

/// The persisted `system_prompt`, so each arm is provably running the prompt it
/// claims to: sessions record it in their first JSONL line.
fn prompt_in_force(store: &std::path::Path) -> String {
    let mut stack = vec![store.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name == "session.jsonl") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some(line) = text.lines().next() else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if let Some(prompt) = value.get("system_prompt").and_then(Value::as_str) {
                    let head: String = prompt.chars().take(96).collect();
                    return format!("{}… ({} chars)", head, prompt.chars().count());
                }
            }
        }
    }
    "not persisted".into()
}

/// One-line argument preview so a run documents itself: the raw JSON with
/// newlines folded, cut to a width a terminal keeps on one line.
fn preview(arguments: &str) -> String {
    let folded = arguments.replace('\n', " ⏎ ");
    let cut: String = folded.chars().take(110).collect();
    if cut.chars().count() < folded.chars().count() {
        format!("{cut}…")
    } else {
        cut
    }
}
