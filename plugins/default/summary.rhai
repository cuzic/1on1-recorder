// summary.rhai — Default summary generation plugin for 1on1 Recorder.
// Replaces the Rust SummaryConsumer with user-customizable Rhai logic.
//
// Hooks:
//   on_session_start(session_id)  — reset state
//   on_segment_finalized(data)    — collect turns
//   on_session_end()              — generate summary via AI
//   on_manual_summary(data)       — user-triggered summary (button)

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
    turns.push(#{
        speaker: data.speaker_label,
        text: data.text,
    });
}

fn on_session_end() {
    if turns.is_empty() {
        let segments = std::list_segments(session_id);
        for seg in segments {
            if seg.is_final && !seen.contains(seg.segment_id) {
                seen[seg.segment_id] = true;
                turns.push(#{ speaker: seg.speaker_label, text: seg.text });
            }
        }
    }
    if turns.is_empty() {
        return;
    }

    let model = std::get_selected_model();
    let system_prompt = std::get_setting("summary_template");
    if system_prompt == () {
        system_prompt = "You summarize 1-on-1 meeting transcripts. Produce a concise summary covering key discussion points, decisions, and action items.";
    }

    std::publish_event(`summary.${session_id}.started`, #{ session_id: session_id });

    try {
        let text = std::call_ai(model, system_prompt, turns);
        std::save_summary(session_id, text, model);
        std::publish_event(`summary.${session_id}.completed`, #{
            session_id: session_id,
            text: text,
            provider_model: model,
        });
        log_info(`summary generated for session ${session_id}`);
    } catch (err) {
        log_error("summary failed: " + err);
        std::publish_event(`summary.${session_id}.failed`, #{
            session_id: session_id,
            error: to_string(err),
        });
    }
}

fn on_manual_summary(data) {
    turns = [];
    seen = #{};
    let id = data.session_id;
    let segments = std::list_segments(id);
    for seg in segments {
        if seg.is_final && !seen.contains(seg.segment_id) {
            seen[seg.segment_id] = true;
            turns.push(#{ speaker: seg.speaker_label, text: seg.text });
        }
    }
    if turns.is_empty() {
        return;
    }

    let model = std::get_selected_model();
    let system_prompt = std::get_setting("summary_template");
    if system_prompt == () {
        system_prompt = "You summarize 1-on-1 meeting transcripts. Produce a concise summary covering key discussion points, decisions, and action items.";
    }

    std::publish_event(`summary.${id}.started`, #{ session_id: id });

    try {
        let text = std::call_ai(model, system_prompt, turns);
        std::save_summary(id, text, model);
        std::publish_event(`summary.${id}.completed`, #{
            session_id: id,
            text: text,
            provider_model: model,
        });
        log_info(`manual summary generated for session ${id}`);
    } catch (err) {
        log_error("manual summary failed: " + err);
        std::publish_event(`summary.${id}.failed`, #{
            session_id: id,
            error: to_string(err),
        });
    }
}

fn on_load() {
    log_info("summary.rhai loaded");
}