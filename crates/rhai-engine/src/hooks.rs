//! Hook dispatch: maps broker events to Rhai script function calls.

use rhai::Engine;

use crate::engine::has_fn;
use crate::scope::ScopeStore;

use transcript_event::TranscriptEvent;

pub fn dispatch(
    engine: &Engine,
    scripts: &[rhai::AST],
    scopes: &ScopeStore,
    event: &TranscriptEvent,
) {
    let (hook_name, session_id, data) = match event {
        TranscriptEvent::SegmentUpdated { session_id, data, .. } => {
            ("on_segment_update", *session_id, segment_data_to_map(data))
        }
        TranscriptEvent::SegmentFinalized { session_id, data } => {
            ("on_segment_finalized", *session_id, segment_data_to_map(data))
        }
        TranscriptEvent::UtteranceEnded { session_id, reason, .. } => {
            let reason_str = match reason {
                transcript_event::UtteranceEndReason::EndOfTurn => "EndOfTurn",
                transcript_event::UtteranceEndReason::SpeechPause => "SpeechPause",
                transcript_event::UtteranceEndReason::SessionEnd => "SessionEnd",
            };
            ("on_utterance_ended", *session_id, {
                let mut m = rhai::Map::new();
                m.insert("reason".into(), rhai::Dynamic::from(reason_str));
                m
            })
        }
    };

    if let TranscriptEvent::UtteranceEnded {
        reason: transcript_event::UtteranceEndReason::SessionEnd,
        session_id,
        ..
    } = event
    {
        // `on_session_end()` is documented (and defined by both
        // `summary.rhai`/`hint.rhai`... well, `hint.rhai` doesn't define it,
        // but `summary.rhai` does) as taking *zero* arguments — see
        // `call_hook`'s doc comment on why this must be `None`, not
        // `Some(rhai::Map::new())`.
        call_hook(engine, scripts, scopes, *session_id, "on_session_end", None);
    }

    call_hook(engine, scripts, scopes, session_id, hook_name, Some(data));
}

pub fn trigger_manual_summary(
    engine: &Engine,
    scripts: &[rhai::AST],
    scopes: &ScopeStore,
    session_id: recorder_domain::SessionId,
) {
    let mut data = rhai::Map::new();
    data.insert("session_id".into(), rhai::Dynamic::from(session_id.to_string()));
    call_hook(engine, scripts, scopes, session_id, "on_manual_summary", Some(data));
}

/// Fires `on_session_end()` outside the normal broker event loop — the same
/// zero-arg hook `dispatch` fires for a real `UtteranceEnded(SessionEnd)`.
/// `run_session`'s `session.{id}.stopped` branch calls this so that a
/// session's finalization logic (auto-summary, etc.) always runs exactly
/// once by the time the session's tasks are done, even on platforms where
/// `SessionEnd` is never published (see `crate`-level doc / the workspace's
/// `apps/desktop/src/recording.rs::stop` for why that subject exists).
pub fn trigger_session_end(
    engine: &Engine,
    scripts: &[rhai::AST],
    scopes: &ScopeStore,
    session_id: recorder_domain::SessionId,
) {
    call_hook(engine, scripts, scopes, session_id, "on_session_end", None);
}

/// Fires `on_hint_timeout()` (zero-arg) — called by
/// `RhaiEngine::spawn_hint_debounce_driver`'s `timed-fsm`-driven silence
/// timer, against the *same* per-session `Scope` `on_segment_finalized` has
/// been accumulating `turns` into (via `RhaiEngineInner::session_scopes`),
/// so `hint.rhai`'s `on_hint_timeout` sees the conversation collected so far.
pub fn trigger_hint_timeout(
    engine: &Engine,
    scripts: &[rhai::AST],
    scopes: &ScopeStore,
    session_id: recorder_domain::SessionId,
) {
    call_hook(engine, scripts, scopes, session_id, "on_hint_timeout", None);
}

/// `data`'s presence must match the hook's own declared arity exactly —
/// `None` for a zero-arg hook (`on_session_end()`), `Some(map)` for a
/// one-arg hook (`on_segment_update(data)`, `on_segment_finalized(data)`,
/// `on_utterance_ended(data)`, `on_manual_summary(data)`). Rhai resolves
/// script functions by name *and* arity (confirmed against rhai 1.20.0's
/// `call_fn` implementation), so calling a zero-arg hook with one argument
/// fails with the same "Function not found" error as the function not
/// existing at all — passing the wrong arity here silently disables that
/// hook exactly the way `has_fn`'s old side-effecting probe used to (see
/// `engine::has_fn`'s doc comment for that earlier bug); `has_fn` itself is
/// now arity-blind (it only checks the name), so it can't catch this at the
/// call site — `dispatch`/`trigger_manual_summary` above are the ones
/// responsible for passing the right shape for each hook name.
fn call_hook(
    engine: &Engine,
    scripts: &[rhai::AST],
    scopes: &ScopeStore,
    session_id: recorder_domain::SessionId,
    hook_name: &str,
    data: Option<rhai::Map>,
) {
    for (idx, ast) in scripts.iter().enumerate() {
        if !has_fn(engine, ast, hook_name) {
            continue;
        }
        let Some(scope_arc) = scopes.get(idx, session_id) else {
            continue;
        };
        let mut scope = scope_arc.lock().unwrap();
        let result = match &data {
            Some(d) => engine.call_fn::<rhai::Dynamic>(&mut scope, ast, hook_name, (d.clone(),)),
            None => engine.call_fn::<rhai::Dynamic>(&mut scope, ast, hook_name, ()),
        };
        if let Err(err) = result {
            tracing::warn!(
                idx = idx,
                hook = hook_name,
                %err,
                "rhai: hook execution failed"
            );
        }
    }
}

fn segment_data_to_map(data: &transcript_event::SegmentData) -> rhai::Map {
    let mut m = rhai::Map::new();
    m.insert("segment_id".into(), rhai::Dynamic::from(data.segment_id.clone()));
    m.insert("text".into(), rhai::Dynamic::from(data.text.clone()));
    m.insert("speaker_label".into(), rhai::Dynamic::from(data.speaker_label.clone()));
    m.insert("track".into(), rhai::Dynamic::from(format!("{:?}", data.track)));
    m.insert("start_ms".into(), rhai::Dynamic::from(data.start_ms.unwrap_or(0) as i64));
    m.insert("end_ms".into(), rhai::Dynamic::from(data.end_ms.unwrap_or(0) as i64));
    m
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use recorder_domain::{SessionId, TrackKind};
    use transcript_event::SegmentData;

    use super::*;
    use crate::engine::load_plugins;
    use crate::scope::ScopeStore;

    /// End-to-end proof that `plugins/default/hint.rhai` (loaded from the
    /// real repo-relative `plugins/` directory, same as the real app) can
    /// actually run a full `on_segment_finalized` (bookkeeping) →
    /// `on_hint_timeout` (triggered here the same way
    /// `RhaiEngine::spawn_hint_debounce_driver`'s real timer does, via
    /// `trigger_hint_timeout`) → `rag_search` → `call_ai` →
    /// `publish_event("hints.*.updated", ...)` chain, and that
    /// `on_hint_timeout` — despite being invoked from a completely separate
    /// trigger path than `on_segment_finalized` — still sees the `turns`
    /// that `on_segment_finalized` accumulated (both calls share the same
    /// `scopes`, exactly as `RhaiEngineInner::session_scopes` is designed to
    /// guarantee for the real timer-driven path). Every step of this chain
    /// was broken by one of several bugs this session found and fixed
    /// (invalid `std.rhai` syntax, the debug build's expression-depth limit,
    /// the never-initialized per-session scope, `has_fn`'s side-effecting
    /// false negatives, the missing module resolver for `import "std"`, and
    /// `call_hook`'s arity mismatch for zero-arg hooks), so this test exists
    /// specifically to prove the whole path now works together, not just
    /// each fix in isolation.
    #[test]
    fn hint_plugin_runs_end_to_end_on_segment_finalized() {
        let published: Arc<Mutex<Vec<(String, rhai::Map)>>> = Arc::new(Mutex::new(Vec::new()));

        let mut engine = Engine::new();
        engine.set_max_expr_depths(128, 128);
        engine.register_fn("now_ms", || -> i64 { 1_000_000 });
        engine.register_fn("log_info", |_msg: &str| {});
        engine.register_fn("log_warn", |_msg: &str| {});
        engine.register_fn("log_error", |msg: &str| eprintln!("[rhai test] log_error: {msg}"));

        let published_for_mock = published.clone();
        engine.register_fn("call_async", move |name: &str, args: rhai::Map| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
            match name {
                "get_setting" => Ok(rhai::Dynamic::UNIT), // hint_provider unset -> defaults to "cloudflare"
                "format_turns" => Ok(rhai::Dynamic::from("self: hello".to_string())),
                "rag_search" => {
                    let mut chunk = rhai::Map::new();
                    chunk.insert("text".into(), rhai::Dynamic::from("previous 1on1: discussed career goals".to_string()));
                    chunk.insert("score".into(), rhai::Dynamic::from(0.9_f64));
                    chunk.insert("source".into(), rhai::Dynamic::from("doc1".to_string()));
                    let arr: rhai::Array = vec![rhai::Dynamic::from_map(chunk)];
                    Ok(rhai::Dynamic::from_array(arr))
                }
                "ai_summarize" => Ok(rhai::Dynamic::from("キャリア目標の進捗を聞いてみましょう。".to_string())),
                "get_selected_model" => Ok(rhai::Dynamic::from("claude-sonnet-4-5".to_string())),
                "publish_event" => {
                    let subject = args.get("subject").unwrap().to_string();
                    let data = args.get("data").cloned().unwrap_or(rhai::Dynamic::UNIT).try_cast::<rhai::Map>().unwrap_or_default();
                    published_for_mock.lock().unwrap().push((subject, data));
                    Ok(rhai::Dynamic::UNIT)
                }
                other => panic!("unexpected call_async command in test mock: {other}"),
            }
        });

        let plugin_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let compiled = load_plugins(&mut engine, &plugin_dir).expect("plugins should load");
        let scripts: Vec<rhai::AST> = compiled.into_iter().map(|c| c.ast).collect();

        let scopes = ScopeStore::new();
        let session_id = SessionId::new();
        scopes.start_session_asts(&engine, &scripts, session_id);

        let event = TranscriptEvent::SegmentFinalized {
            session_id,
            data: SegmentData {
                segment_id: "seg1".to_string(),
                revision: 1,
                text: "hello".to_string(),
                speaker_label: "self".to_string(),
                track: TrackKind::SelfMic,
                start_ms: Some(0),
                end_ms: Some(1000),
            },
        };
        dispatch(&engine, &scripts, &scopes, &event);
        // Mirrors `spawn_hint_debounce_driver`'s real trigger: a separate
        // call, against the same `scopes`, once the (here: simulated)
        // silence timer fires.
        trigger_hint_timeout(&engine, &scripts, &scopes, session_id);

        let published = published.lock().unwrap();
        let hint_events: Vec<_> = published.iter().filter(|(subject, _)| subject.contains(".updated") && subject.starts_with("hints.")).collect();
        assert_eq!(hint_events.len(), 1, "expected exactly one hints.*.updated publish, got: {published:?}");
        let (subject, data) = hint_events[0];
        assert_eq!(*subject, format!("hints.{session_id}.updated"));
        assert_eq!(data.get("text").unwrap().to_string(), "キャリア目標の進捗を聞いてみましょう。");
        assert_eq!(data.get("provider").unwrap().to_string(), "cloudflare");
    }

    /// Regression test for a bug an external review caught in this same
    /// changeset: `on_session_end()` is a *zero*-argument hook (see
    /// `plugins/default/summary.rhai`), but `call_hook` used to always call
    /// with one argument regardless of hook name — rhai resolves script
    /// functions by name *and* arity, so that made `on_session_end` fail
    /// with "Function not found" on every single session end, silently
    /// warn-logged and never surfaced. `call_hook` now takes `Option<Map>`
    /// and `dispatch` passes `None` specifically for `on_session_end`; this
    /// drives the real `SessionEnd` dispatch path (not just
    /// `on_segment_finalized`, which the test above already covered) to
    /// prove the zero-arg hook actually executes.
    #[test]
    fn session_end_hook_runs_with_zero_arguments() {
        let published: Arc<Mutex<Vec<(String, rhai::Map)>>> = Arc::new(Mutex::new(Vec::new()));

        let mut engine = Engine::new();
        engine.set_max_expr_depths(128, 128);
        engine.register_fn("now_ms", || -> i64 { 1_000_000 });
        engine.register_fn("log_info", |_msg: &str| {});
        engine.register_fn("log_warn", |_msg: &str| {});
        engine.register_fn("log_error", |msg: &str| eprintln!("[rhai test] log_error: {msg}"));

        let published_for_mock = published.clone();
        engine.register_fn("call_async", move |name: &str, args: rhai::Map| -> Result<rhai::Dynamic, Box<rhai::EvalAltResult>> {
            match name {
                "get_setting" => Ok(rhai::Dynamic::UNIT),
                "format_turns" => Ok(rhai::Dynamic::from("self: hello".to_string())),
                "rag_search" => Ok(rhai::Dynamic::from_array(rhai::Array::new())),
                "get_selected_model" => Ok(rhai::Dynamic::from("claude-sonnet-4-5".to_string())),
                "ai_summarize" => Ok(rhai::Dynamic::from("要約テキスト".to_string())),
                "save_summary" => Ok(rhai::Dynamic::UNIT),
                "publish_event" => {
                    let subject = args.get("subject").unwrap().to_string();
                    let data = args.get("data").cloned().unwrap_or(rhai::Dynamic::UNIT).try_cast::<rhai::Map>().unwrap_or_default();
                    published_for_mock.lock().unwrap().push((subject, data));
                    Ok(rhai::Dynamic::UNIT)
                }
                other => panic!("unexpected call_async command in test mock: {other}"),
            }
        });

        let plugin_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let compiled = load_plugins(&mut engine, &plugin_dir).expect("plugins should load");
        let scripts: Vec<rhai::AST> = compiled.into_iter().map(|c| c.ast).collect();

        let scopes = ScopeStore::new();
        let session_id = SessionId::new();
        scopes.start_session_asts(&engine, &scripts, session_id);

        // Populate summary.rhai's `turns` first, exactly like a real session,
        // so `on_session_end` takes its main path rather than the
        // `list_segments` fallback (which this test's mock doesn't stub).
        dispatch(&engine, &scripts, &scopes, &TranscriptEvent::SegmentFinalized {
            session_id,
            data: SegmentData {
                segment_id: "seg1".to_string(),
                revision: 1,
                text: "hello".to_string(),
                speaker_label: "self".to_string(),
                track: TrackKind::SelfMic,
                start_ms: Some(0),
                end_ms: Some(1000),
            },
        });

        dispatch(&engine, &scripts, &scopes, &TranscriptEvent::UtteranceEnded {
            session_id,
            segment_id: None,
            reason: transcript_event::UtteranceEndReason::SessionEnd,
        });

        let published = published.lock().unwrap();
        let started = published.iter().any(|(subject, _)| *subject == format!("summary.{session_id}.started"));
        let completed = published.iter().any(|(subject, _)| *subject == format!("summary.{session_id}.completed"));
        assert!(started, "on_session_end should have published summary.*.started, got: {published:?}");
        assert!(completed, "on_session_end should have published summary.*.completed, got: {published:?}");
    }
}