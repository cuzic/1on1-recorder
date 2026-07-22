// std.rhai — Standard library for 1on1 Recorder Rhai plugins.
// Provides curry wrappers around call_async() for common operations.
//
// Plugin authors can import this module and use the friendly function names:
//   import "std" as std;
//   std::call_ai("claude-sonnet-4-5", prompt, turns);

// ── AI ──

fn call_ai(model, system_prompt, turns) {
    call_async("ai_summarize", #{ model, system_prompt, turns })
}

// ── Data Access ──

fn list_segments(session_id) {
    call_async("list_segments", #{ session_id })
}

fn save_summary(session_id, text, provider_model) {
    call_async("save_summary", #{ session_id, text, provider_model })
}

fn get_setting(key) {
    call_async("get_setting", #{ key })
}

fn get_selected_model() {
    call_async("get_selected_model", #{})
}

fn get_session_metadata(session_id) {
    call_async("get_session_metadata", #{ session_id })
}

// ── Formatting ──

fn format_turns(turns, format) {
    call_async("format_turns", #{ turns, format })
}

// ── Event Publishing ──

fn publish_event(subject, data) {
    call_async("publish_event", #{ subject, data })
}

// ── RAG (Retrieval-Augmented Generation) ──

// Search across indexed documents.
// provider: "vertex" | "bedrock"
// query: natural language query
// options: #{ data_store, max_results, ... }
// Returns: array of #{ title, snippet, text, score, source }
fn rag_search(provider, query, options) {
    call_async("rag_search", #{ provider, query, options })
}

// Index documents for future search.
// provider: "vertex" | "bedrock"
// documents: array of strings or segment objects
// options: #{ data_store, ... }
// Returns: #{ indexed, errors }
fn rag_index(provider, documents, options) {
    call_async("rag_index", #{ provider, documents, options })
}