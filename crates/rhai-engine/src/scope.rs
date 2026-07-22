//! Per-script, per-session Scope management.

use std::sync::{Arc, Mutex};

use rhai::Scope;
use recorder_domain::SessionId;

pub struct ScopeStore {
    scopes: dashmap::DashMap<(usize, SessionId), Arc<Mutex<Scope<'static>>>>,
}

impl ScopeStore {
    pub fn new() -> Self {
        Self { scopes: dashmap::DashMap::new() }
    }

    /// Initialize scopes from ASTs (no CompiledScript needed).
    pub fn start_session_asts(&self, asts: &[rhai::AST], session_id: SessionId) {
        for (idx, _) in asts.iter().enumerate() {
            let mut scope = Scope::new();
            scope.push("session_id", session_id.to_string());
            self.scopes.insert((idx, session_id), Arc::new(Mutex::new(scope)));
        }
    }

    pub fn get(&self, script_idx: usize, session_id: SessionId) -> Option<Arc<Mutex<Scope<'static>>>> {
        self.scopes.get(&(script_idx, session_id)).map(|r| r.clone())
    }
}