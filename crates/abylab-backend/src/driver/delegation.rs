//! Session ownership of delegation trees. Retiring a session never cancels
//! background work; settled leaves are archived immediately or on completion.
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

#[derive(Default)]
pub(super) struct DelegationSessions {
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    roots: HashMap<String, String>,
    retired: HashSet<String>,
}
impl DelegationSessions {
    pub(super) fn attach(&self, root: &str, session: &str) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .roots
            .insert(root.into(), session.into());
    }
    pub(super) fn session(&self, parent: &str) -> String {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .roots
            .get(parent)
            .cloned()
            .unwrap_or_else(|| parent.into())
    }
    pub(super) fn retire(&self, root: &str, agents: &abycore::Subagents) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retired
            .insert(root.into());
        self.reap(agents);
    }
    pub(super) fn reap(&self, agents: &abycore::Subagents) {
        let roots: Vec<_> = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retired
            .iter()
            .cloned()
            .collect();
        for root in roots {
            agents.close_session(&root);
        }
    }
}
