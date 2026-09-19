use abycore::{
    Agent, AgentEvent, ClientConfig, DeepSeekClient, ModelOptions, PlanView, RunOptions,
    SessionSnapshot, StreamEvent, TodoStatus, TodoWriteTool,
};
use std::io::{Write, stdout};

fn show_plan(plan: Option<&PlanView>) {
    let Some(plan) = plan.filter(|plan| !plan.todos.is_empty()) else {
        // A GUI host would hide or clear its task panel here.
        return;
    };
    eprintln!("\n计划：{}/{} 已完成", plan.counts.completed, plan.total);
    for todo in &plan.todos {
        let status = match todo.status {
            TodoStatus::Pending => "待处理",
            TodoStatus::InProgress => "进行中",
            TodoStatus::Completed => "已完成",
        };
        eprintln!("  [{status}] {}", todo.content);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = ClientConfig::new(std::env::var("DEEPSEEK_API_KEY")?);
    if let Ok(url) = std::env::var("DEEPSEEK_BASE_URL") {
        config.base_url = url;
    }
    let mut model = ModelOptions::default();
    if let Ok(name) = std::env::var("DEEPSEEK_MODEL") {
        model.model = name;
    }
    let mut agent = Agent::new(
        DeepSeekClient::new(config)?,
        "多步骤任务先用 todo_write 建立计划；开始或完成步骤时更新整份列表。完成后给出简短总结。",
        model,
    )?;
    // false permits at most one active step; use true for concurrent delegated work.
    agent.register_tool(TodoWriteTool::new(false))?;
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let prompt = if prompt.is_empty() {
        "用三个步骤解释二分查找：先说明适用条件，再推导复杂度，最后用有序数组举例；逐步更新计划进度。".into()
    } else {
        prompt
    };
    let result = agent
        .run(prompt, RunOptions::default(), |event| async move {
            match event {
                AgentEvent::PlanChanged { plan } => show_plan(plan.as_ref()),
                AgentEvent::Model(StreamEvent::TextDelta { delta, .. }) => {
                    print!("{delta}");
                    let _ = stdout().flush();
                }
                _ => {}
            }
            Ok(())
        })
        .await;
    // The snapshot carries the current checklist even on an interrupted run.
    // A host can store this JSON and render it without executing tools again.
    let json = agent.snapshot().to_json()?;
    let saved = SessionSnapshot::from_json(&json)?;
    assert_eq!(saved.plan_view(), agent.plan_view());
    let outcome = result?;
    println!("\n{:?}", outcome.stop_reason);
    Ok(())
}
