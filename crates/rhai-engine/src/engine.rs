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
    if default_dir.is_dir() {
        load_dir(engine, &default_dir, &mut scripts)?;
    }

    let user_dir = plugin_dir.join("user");
    if user_dir.is_dir() {
        load_dir(engine, &user_dir, &mut scripts)?;
    }

    // Fire on_load for each script
    for script in &scripts {
        let mut scope = Scope::new();
        if has_fn(engine, &script.ast, "on_load") {
            let _ = engine.run_ast_with_scope(&mut scope, &script.ast);
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

pub fn has_fn(engine: &Engine, ast: &rhai::AST, name: &str) -> bool {
    let mut scope = Scope::new();
    let empty = rhai::Map::new();
    match engine.call_fn::<rhai::Dynamic>(&mut scope, ast, name, (empty,)) {
        Ok(_) => true,
        Err(err) => {
            let msg = err.to_string();
            !msg.contains("not found") && !msg.contains("not defined")
        }
    }
}