//! Rhai script loading and compilation.

use std::path::Path;

use rhai::{Engine, Scope};

use super::RhaiError;

pub struct CompiledScript {
    pub ast: rhai::AST,
}

pub fn load_plugins(
    engine: &mut Engine,
    plugin_dir: &Path,
) -> Result<Vec<CompiledScript>, RhaiError> {
    let mut scripts = Vec::new();

    let default_dir = plugin_dir.join("default");
    // `import "std" as std;` (used by every plugin, `default/` and `user/`
    // alike) resolves relative to whatever base path the module resolver was
    // given — the default `Engine::new()` resolver has none, so it fails with
    // "Module not found: std" regardless of the process's working directory
    // (confirmed: reproduces even from `default_dir` itself unless this is
    // set explicitly). `std.rhai` always lives under `default/`, never
    // `user/`, so that's the fixed base path for every script's imports.
    engine.set_module_resolver(rhai::module_resolvers::FileModuleResolver::new_with_path(&default_dir));

    if default_dir.is_dir() {
        load_dir(engine, &default_dir, &mut scripts)?;
    }

    let user_dir = plugin_dir.join("user");
    if user_dir.is_dir() {
        load_dir(engine, &user_dir, &mut scripts)?;
    }

    // Fire on_load for each script. `run_ast_with_scope` executes the script's
    // top-level statements (the `import "std" as std;`/`let ...;` lines the
    // hook functions below close over) into this throwaway scope, then
    // `call_fn` actually invokes `on_load()` itself — this scope is discarded
    // right after, since it's unrelated to any session (see `ScopeStore` for
    // the persistent, per-session equivalent of this same "run the AST into
    // the scope before calling into it" step).
    for script in &scripts {
        let mut scope = Scope::new();
        if has_fn(engine, &script.ast, "on_load") {
            let _ = engine.run_ast_with_scope(&mut scope, &script.ast);
            let _ = engine.call_fn::<rhai::Dynamic>(&mut scope, &script.ast, "on_load", ());
        }
    }

    Ok(scripts)
}

fn load_dir(
    engine: &mut Engine,
    dir: &Path,
    scripts: &mut Vec<CompiledScript>,
) -> Result<(), RhaiError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rhai"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let source = std::fs::read_to_string(&path)?;
        let ast = engine
            .compile(&source)
            .map_err(|e| RhaiError::Compile {
                path: path.display().to_string(),
                error: e.to_string(),
            })?;
        scripts.push(CompiledScript { ast });
    }

    Ok(())
}

/// Whether `ast` defines a function named `name`, regardless of its arity.
///
/// Reads the AST's function table (`AST::iter_functions`) rather than probing
/// by actually calling the function — an earlier version did the latter (with
/// a throwaway, uninitialized `Scope` and a single bogus `#{}` argument
/// regardless of the real hook's arity), which had two compounding bugs:
/// zero-arg hooks (`on_load`, `on_session_end`) always reported as "missing"
/// (Rhai's function resolution is by name *and* arity, so probing a 0-arg
/// function with 1 argument fails as "not found" exactly like it not existing
/// at all), and any hook whose body reads a module-scope variable (`turns`,
/// `seen` — the persistence pattern `plugins/default/*.rhai` and
/// `docs/rhai-plugin-summary.md` are built around) also reported as "missing"
/// once its body hit that variable in the probe's unrelated, un-initialized
/// scope — which additionally meant the probe call for a *matching*-arity
/// hook was a real, side-effecting invocation with garbage `#{}` data. Net
/// effect: `hooks::call_hook`'s `has_fn` gate silently skipped nearly every
/// hook a real plugin would define. This version has no side effects.
pub fn has_fn(_engine: &Engine, ast: &rhai::AST, name: &str) -> bool {
    ast.iter_functions().any(|f| f.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_zero_arg_hooks() {
        let engine = Engine::new();
        let ast = engine.compile("fn on_load() {} fn on_session_end() {}").unwrap();
        assert!(has_fn(&engine, &ast, "on_load"));
        assert!(has_fn(&engine, &ast, "on_session_end"));
    }

    #[test]
    fn detects_hooks_that_reference_module_scope_state() {
        // Regression case: the previous has_fn implementation probed by
        // actually calling the function with an empty, uninitialized scope,
        // which made a hook referencing `turns` (exactly the
        // summary.rhai/hint.rhai pattern) report as "missing".
        let engine = Engine::new();
        let ast = engine
            .compile("let turns = []; fn on_segment_finalized(data) { turns.push(data); }")
            .unwrap();
        assert!(has_fn(&engine, &ast, "on_segment_finalized"));
    }

    #[test]
    fn missing_function_is_not_detected() {
        let engine = Engine::new();
        let ast = engine.compile("fn on_load() {}").unwrap();
        assert!(!has_fn(&engine, &ast, "does_not_exist"));
    }

    /// Loads the real shipped plugins (`plugins/default/`, repo-relative to
    /// this crate) end to end — the closest thing to a compile check
    /// available for `.rhai` files, which `cargo check` never touches since
    /// they're loaded at runtime, not part of the Rust build.
    #[test]
    fn real_default_plugins_load_without_error() {
        let plugin_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let mut engine = Engine::new();
        engine.set_max_expr_depths(128, 128);
        engine.register_fn("call_async", |_name: &str, _args: rhai::Map| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
            Ok(rhai::Dynamic::UNIT)
        });
        engine.register_fn("log_info", |_msg: &str| {});
        engine.register_fn("log_warn", |_msg: &str| {});
        engine.register_fn("log_error", |_msg: &str| {});
        engine.register_fn("now_ms", || -> i64 { 0 });

        let scripts = load_plugins(&mut engine, &plugin_dir).expect("default plugins should compile");
        assert!(!scripts.is_empty(), "expected at least one plugin under plugins/default/");
    }
}