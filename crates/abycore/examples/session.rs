mod support;
use abycore::{Agent, ClientConfig, DeepSeekClient, ModelOptions, RunOptions, SessionSnapshot};
use std::{io::Write, path::Path};

/// The storage policy belongs to the host. Temp file and destination share a directory.
fn save(path: &Path, snapshot: &SessionSnapshot) -> Result<(), Box<dyn std::error::Error>> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(snapshot.to_json()?.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    #[cfg(unix)]
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = DeepSeekClient::new(ClientConfig::new(std::env::var("DEEPSEEK_API_KEY")?))?;
    let path = std::env::var("SESSION_PATH").unwrap_or_else(|_| "session.json".into());
    let mut agent = match std::fs::read_to_string(&path) {
        Ok(json) => Agent::restore(client, SessionSnapshot::from_json(&json)?)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Agent::new(client, "使用工具处理用户的文字。", ModelOptions::default())?
        }
        Err(error) => return Err(error.into()),
    };
    agent.register_tool(support::Uppercase)?;
    let result = if agent.snapshot().needs_response {
        agent
            .continue_run(RunOptions::default(), |_| async { Ok(()) })
            .await
    } else {
        agent
            .run(
                "请用 uppercase 处理 hello",
                RunOptions::default(),
                |_| async { Ok(()) },
            )
            .await
    };
    // Save on failure too, including completed outputs and unresolved calls.
    save(Path::new(&path), &agent.snapshot())?;
    println!("{}", result?.response.output_text());
    Ok(())
}
