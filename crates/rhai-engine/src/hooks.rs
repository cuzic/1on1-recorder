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
        call_hook(engine, scripts, scopes, *session_id, "on_session_end", rhai::Map::new());
    }

    call_hook(engine, scripts, scopes, session_id, hook_name, data);
}

pub fn trigger_manual_summary(
    engine: &Engine,
    scripts: &[rhai::AST],
    scopes: &ScopeStore,
    session_id: recorder_domain::SessionId,
) {
    let mut data = rhai::Map::new();
    data.insert("session_id".into(), rhai::Dynamic::from(session_id.to_string()));
    call_hook(engine, scripts, scopes, session_id, "on_manual_summary", data);
}

fn call_hook(
    engine: &Engine,
    scripts: &[rhai::AST],
    scopes: &ScopeStore,
    session_id: recorder_domain::SessionId,
    hook_name: &str,
    data: rhai::Map,
) {
    for (idx, ast) in scripts.iter().enumerate() {
        if !has_fn(engine, ast, hook_name) {
            continue;
        }
        let Some(scope_arc) = scopes.get(idx, session_id) else {
            continue;
        };
        let mut scope = scope_arc.lock().unwrap();
        match engine.call_fn::<rhai::Dynamic>(&mut scope, ast, hook_name, (data.clone(),)) {
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    idx = idx,
                    hook = hook_name,
                    %err,
                    "rhai: hook execution failed"
                );
            }
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