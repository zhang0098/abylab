//! Explicit paid probe. All effects stay in a fresh temporary workspace.
#![cfg(unix)]
use abycore::*;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

#[tokio::test]
#[ignore = "paid local-tool probe; requires DEEPSEEK_API_KEY"]
async fn four_local_tools_live() {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = ClientConfig::new(
        std::env::var("DEEPSEEK_API_KEY").expect("set DEEPSEEK_API_KEY explicitly"),
    );
    if let Ok(url) = std::env::var("DEEPSEEK_BASE_URL") {
        config.base_url = url;
    }
    let model = ModelOptions {
        model: std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
        ..Default::default()
    };
    let mut agent = Agent::new(DeepSeekClient::new(config).unwrap(),
        "Follow the exact requested sequence. Only access abycore_local_probe.txt and the .abycore log returned by Bash. The only allowed Bash commands are: for ((i=0;i<200;i++)); do cat abycore_local_probe.txt; done AND printf background-ok. Do not inspect environment variables or any other files. The host manages background jobs, so after starting the requested background job simply report its id.",model).unwrap();
    let tools = LocalTools::new(workspace.path()).unwrap();
    tools.register(&mut agent).unwrap();
    let details = Arc::new(Mutex::new(vec![]));
    let observed = details.clone();
    let outcome = agent.run(
        r#"Call write with {"file_path":"abycore_local_probe.txt","content":"alpha alpha\n"}. Call read to read that file. Call edit with {"file_path":"abycore_local_probe.txt","old_string":"alpha","new_string":"beta","replace_all":true}. Call bash with command "for ((i=0;i<200;i++)); do cat abycore_local_probe.txt; done", a description and timeoutMs 5000. Call read on its full-output log with offset 1 and limit 2. Finally call bash with command "printf background-ok", a description and run_in_background true. Report the file content and background job id; do not run further commands."#,
        RunOptions {max_requests:12,max_tool_calls:12,max_tool_output_bytes:512,..Default::default()},
        move |event| {
            if let AgentEvent::ToolFinished{output,..} = event { observed.lock().unwrap().push(output.details); }
            async {Ok(())}
        },
    ).await.unwrap();
    assert_eq!(outcome.stop_reason, StopReason::Completed);
    let names: HashSet<_> = outcome
        .new_items
        .iter()
        .filter_map(|item| match item {
            Item::FunctionCall { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    for name in ["read", "write", "edit", "bash"] {
        assert!(names.contains(name), "model did not use {name}");
    }
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("abycore_local_probe.txt")).unwrap(),
        "beta beta\n"
    );
    let outputs: Vec<_> = details.lock().unwrap().iter().flatten().cloned().collect();
    assert!(
        outputs
            .iter()
            .any(|value| value["replacements"] == 2
                && !value["diffs"].as_array().unwrap().is_empty())
    );
    let result: BashResult = serde_json::from_value(
        outputs
            .iter()
            .find(|value| value.get("stdout").is_some())
            .unwrap()
            .clone(),
    )
    .unwrap();
    let log = result.stdout.spill_path.unwrap();
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "beta beta\n".repeat(200)
    );
    assert!(outcome.new_items.iter().any(|item| match item {
        Item::FunctionCall {
            name, arguments, ..
        } if name == "read" => {
            let args: serde_json::Value = serde_json::from_str(arguments).unwrap();
            args["file_path"]
                .as_str()
                .is_some_and(|p| std::path::Path::new(p) == log || workspace.path().join(p) == log)
        }
        _ => false,
    }));
    let id = tools
        .jobs()
        .first()
        .expect("model started background job")
        .id
        .clone();
    let job = tools.wait_job(&id).await.unwrap();
    assert_eq!(job.status, BashJobStatus::Completed);
    assert_eq!(job.result.unwrap().stdout.text, "background-ok");
    tools.shutdown().await;
    assert!(agent.snapshot().pending.is_empty());
    eprintln!(
        "Verified Harness tool arguments, replace_all, structured diff, log readback and host-managed background job; {} model requests",
        outcome.requests.len()
    );
}
