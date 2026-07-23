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
    ///
    /// Runs each script's top-level statements (`import "std" as std;`,
    /// module-scope `let turns = []; let seen = #{};`-style declarations —
    /// see `plugins/default/summary.rhai`/`hint.rhai`) into the *persistent*
    /// scope stored here, not a throwaway one — without this,
    /// `engine.call_fn` in `hooks::call_hook` fails every single call with
    /// "Variable not found: turns" the moment a hook function references a
    /// module-scope variable, since `call_fn` only looks up the named
    /// function and never runs the AST's own body on its own. This mirrors
    /// what `engine::load_plugins`'s `on_load` step already does for its own
    /// (unrelated, load-time-only) throwaway scope.
    pub fn start_session_asts(&self, engine: &rhai::Engine, asts: &[rhai::AST], session_id: SessionId) {
        for (idx, ast) in asts.iter().enumerate() {
            let mut scope = Scope::new();
            scope.push("session_id", session_id.to_string());
            if let Err(err) = engine.run_ast_with_scope(&mut scope, ast) {
                tracing::warn!(script_idx = idx, %session_id, %err, "rhai: failed to initialize script scope for session");
            }
            self.scopes.insert((idx, session_id), Arc::new(Mutex::new(scope)));
        }
    }

    pub fn get(&self, script_idx: usize, session_id: SessionId) -> Option<Arc<Mutex<Scope<'static>>>> {
        self.scopes.get(&(script_idx, session_id)).map(|r| r.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the bug this module's doc comment describes:
    /// before `start_session_asts` ran the AST into the stored scope, a hook
    /// function referencing a module-scope variable (exactly the
    /// `summary.rhai`/`hint.rhai` `let turns = [];` pattern) failed every
    /// single call with "Variable not found: turns" via `hooks::call_hook`'s
    /// `engine.call_fn` — silently logged as a warning, never surfaced.
    #[test]
    fn module_scope_state_persists_across_hook_calls() {
        let engine = rhai::Engine::new();
        let ast = engine
            .compile(
                r#"
                let turns = [];
                fn on_segment(data) { turns.push(data); }
                fn count() { turns.len() }
                "#,
            )
            .expect("compile");

        let store = ScopeStore::new();
        let session_id = SessionId::new();
        store.start_session_asts(&engine, std::slice::from_ref(&ast), session_id);

        let scope_arc = store.get(0, session_id).expect("scope was stored");
        {
            let mut scope = scope_arc.lock().unwrap();
            let _ = engine.call_fn::<rhai::Dynamic>(&mut scope, &ast, "on_segment", ("a".to_string(),)).expect("first call");
        }
        {
            let mut scope = scope_arc.lock().unwrap();
            let _ = engine.call_fn::<rhai::Dynamic>(&mut scope, &ast, "on_segment", ("b".to_string(),)).expect("second call");
        }
        let mut scope = scope_arc.lock().unwrap();
        let count: i64 = engine.call_fn(&mut scope, &ast, "count", ()).expect("count call");
        assert_eq!(count, 2, "turns should have accumulated across both on_segment calls");
    }
}