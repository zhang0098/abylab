use abycore::{
    Agent, ClientConfig, DeepSeekClient, LocalToolConfig, LocalTools, ModelOptions, PermissionMode,
    RunOptions,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The example grants access only after the host chooses a fresh workspace.
    let workspace = tempfile::tempdir()?;
    let client = DeepSeekClient::new(ClientConfig::new(std::env::var("DEEPSEEK_API_KEY")?))?;
    let mut agent = Agent::new(
        client,
        "在给定工作目录内完成用户要求，按顺序调用工具。",
        ModelOptions::default(),
    )?;
    let mut local = LocalToolConfig::new(workspace.path());
    local.permission_mode = PermissionMode::WorkspaceWrite;
    local.save_bash_output = true;
    local.max_bash_log_bytes = 8 * 1024 * 1024;
    let tools = LocalTools::with_config(local)?;
    tools.register(&mut agent)?;
    let outcome = agent.run(
        "用 write 创建 note.txt，内容为 alpha；用 read 读取；用 edit 将 alpha 改成 beta；最后用 bash 执行 cat note.txt 并报告结果。",
        RunOptions::default(), |_| async { Ok(()) },
    ).await?;
    println!("{}", outcome.response.output_text());
    // A background bash call returns a jobId in ToolOutput.details. The host owns it.
    for job in tools.jobs() {
        let output = tools.job_output(&job.id, Default::default())?;
        println!("{}: {:?} {}", job.id, output.status, output.stdout);
        tools.kill_job(&job.id).await?;
    }
    tools.shutdown().await;
    Ok(())
}
