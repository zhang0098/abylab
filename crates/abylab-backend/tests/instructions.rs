mod common;

use common::{Reply, Scenario, text_reply};

#[test]
fn startup_resume_loads_current_workspace_instructions_before_continuing_unfinished_turn() {
    let run = common::drive(
        Scenario::new("instructions-resume", vec![Reply::sse(text_reply())])
            .resume("old-session")
            .seed(|workspace| {
                std::fs::write(workspace.join("AGENTS.md"), "CURRENT_WORKSPACE_RULE").unwrap();
                let mut snapshot =
                    abycore::SessionSnapshot::new("system", abycore::ModelOptions::default());
                snapshot
                    .items
                    .push(abycore::Item::user("unfinished user task"));
                snapshot.needs_response = true;
                let store = abycore::SessionStore::new(workspace).unwrap();
                let mut writer = store.create("old-session", &snapshot).unwrap();
                store.append_checkpoint(&mut writer, 1, &snapshot).unwrap();
            })
            .prompt("follow this correction"),
    );
    assert!(run.is_clean(), "{}", run.explain());
    assert_eq!(run.count(), 1);
    let body: serde_json::Value = serde_json::from_str(&run.requests[0]).unwrap();
    assert!(
        body["messages"]
            .to_string()
            .contains("CURRENT_WORKSPACE_RULE")
    );
    assert!(
        body["messages"]
            .to_string()
            .contains("follow this correction")
    );
    assert!(
        !body["system"]
            .to_string()
            .contains("CURRENT_WORKSPACE_RULE")
    );
}
