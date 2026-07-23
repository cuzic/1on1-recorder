// hint.rhai — Live conversation-hint plugin for 1on1 Recorder.
// Retrieves relevant context via a RAG provider (Cloudflare AI Search by
// default) and asks the configured summary/LLM provider to turn it into a
// short "what to talk about next" suggestion, published live during an
// active recording.
//
// Hooks:
//   on_session_start(session_id) — reset state
//   on_segment_finalized(data)   — collect turns only (cheap bookkeeping)
//   on_hint_timeout()            — generate + publish a hint
//
// Frequency: `on_hint_timeout` is NOT called from this script at all — it's
// triggered from Rust by `RhaiEngine::spawn_hint_debounce_driver`
// (`crates/rhai-engine/src/hint_debounce.rs`), which drives a real
// silence-detection debounce timer via the `timed-fsm` crate. Rhai itself
// has no timer/sleep primitive (see docs/rhai-plugin-summary.md §4.4's
// never-implemented `sleep_ms`), so a from-scratch throttle-on-each-event
// approximation used to live here — that's gone now; `on_segment_finalized`
// only collects turns.
//
// NOTE: `on_hint_timeout` reads `turns`/`seen` directly because it — like
// `on_segment_finalized` — is itself one of the functions the Rust host
// calls directly via `call_fn(..., hook_name, ...)`, which is what grants
// access to this script's module-scope `let` variables. A plain
// Rhai-internal call from one hook to a separate helper function does NOT
// inherit that visibility and fails at runtime with "Variable not found"
// (confirmed empirically) — do not factor shared logic between hooks into a
// helper function without re-verifying this; `summary.rhai`'s duplication
// between `on_session_end`/`on_manual_summary` is the same constraint, not
// an oversight.

import "std" as std;

let turns = [];
let seen = #{};

fn on_session_start(session_id) {
    turns = [];
    seen = #{};
}

fn on_segment_finalized(data) {
    if seen.contains(data.segment_id) {
        return;
    }
    seen[data.segment_id] = true;
    turns.push(#{ speaker: data.speaker_label, text: data.text });
}

fn on_hint_timeout() {
    if turns.is_empty() {
        return;
    }

    let provider = std::get_setting("hint_provider");
    if provider == () {
        provider = "cloudflare";
    }

    // Most recent ~12 turns as the RAG query / conversation context — enough
    // for relevance without an unbounded query as the session grows long.
    let window = [];
    let start = if turns.len() > 12 { turns.len() - 12 } else { 0 };
    for i in range(start, turns.len()) {
        window.push(turns[i]);
    }
    let query = std::format_turns(window, "text");

    try {
        let results = std::rag_search(provider, query, #{ max_results: 3 });
        if results.is_empty() {
            return;
        }

        let context = "";
        for r in results {
            context += "- " + r.text + "\n";
        }

        let hint_turns = window;
        hint_turns.push(#{ speaker: "参考情報 (RAG)", text: context });

        let model = std::get_selected_model();
        let system_prompt = "あなたは1on1ミーティングのアシスタントです。直近の会話と参考情報を踏まえて、"
            + "今このタイミングで話すと良い話題やフォローアップの質問を、日本語で1〜2文の短い提案として1つだけ出してください。"
            + "前置きや説明は不要です。";

        let hint_text = std::call_ai(model, system_prompt, hint_turns);
        std::publish_event(`hints.${session_id}.updated`, #{ session_id: session_id, text: hint_text, provider: provider });
        log_info(`hint generated for session ${session_id} via ${provider}`);
    } catch (err) {
        log_error("hint generation failed: " + err);
    }
}

fn on_load() {
    log_info("hint.rhai loaded");
}
