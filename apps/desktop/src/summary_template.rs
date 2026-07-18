//! Built-in "要約プロンプトテンプレート" presets for
//! `app_settings::AppSettings::summary_template` (task tracked alongside #38/#67).
//!
//! `AppSettings::summary_template` itself only stores a resolved freeform string
//! (or `None` for "no override — use `summarize::DEFAULT_SYSTEM_PROMPT`"). This
//! module is where the *preset name → prompt text* mapping and the settings-UI
//! sentinels for "no override"/"custom" live, so `settings.rs` (the picker UI)
//! and `ui.rs` (`on_generate_summary`, which resolves the stored value into a
//! [`summarize::SummarizeOptions`]) share one definition instead of duplicating
//! prompt text or sentinel values.
//!
//! Selecting a preset in the settings UI writes that preset's [`SummaryTemplatePreset::prompt`]
//! text verbatim into `AppSettings::summary_template`; selecting [`CUSTOM_TEMPLATE`]
//! instead lets the user type their own freeform system prompt, which is stored
//! as-is. Either way, once saved, `summary_template` is just an `Option<String>`
//! — this module doesn't need to be consulted again at summarize-call time
//! (see [`summarize_options_for`]), only when the settings screen needs to
//! decide which `<select>` entry to preselect (see [`select_key_for`]).

/// One built-in preset. Initial scope is a handful of built-ins plus one
/// freeform custom slot ([`CUSTOM_TEMPLATE`]) — managing multiple named custom
/// templates is out of scope for now (see design note referenced above).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryTemplatePreset {
    /// This app's own namesake use case — the default worth investing the most
    /// care in. Structured around progress-since-last-time / issues raised /
    /// decisions / next action items, the shape a manager skimming a 1on1
    /// summary actually needs.
    OneOnOne,
    SalesCall,
    Standup,
}

impl SummaryTemplatePreset {
    pub(crate) const ALL: [SummaryTemplatePreset; 3] =
        [SummaryTemplatePreset::OneOnOne, SummaryTemplatePreset::SalesCall, SummaryTemplatePreset::Standup];

    /// Stable identifier used as the `<select>` `value` and stored nowhere else
    /// (the preset's *text*, not its key, is what ends up in
    /// `AppSettings::summary_template` — see this module's doc comment).
    pub(crate) fn key(self) -> &'static str {
        match self {
            SummaryTemplatePreset::OneOnOne => "one_on_one",
            SummaryTemplatePreset::SalesCall => "sales_call",
            SummaryTemplatePreset::Standup => "standup",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            SummaryTemplatePreset::OneOnOne => "1on1用",
            SummaryTemplatePreset::SalesCall => "営業通話用",
            SummaryTemplatePreset::Standup => "スタンドアップ用",
        }
    }

    pub(crate) fn prompt(self) -> &'static str {
        match self {
            SummaryTemplatePreset::OneOnOne => ONE_ON_ONE_PROMPT,
            SummaryTemplatePreset::SalesCall => SALES_CALL_PROMPT,
            SummaryTemplatePreset::Standup => STANDUP_PROMPT,
        }
    }

    pub(crate) fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.key() == key)
    }
}

const ONE_ON_ONE_PROMPT: &str = "\
あなたは1on1-recorderというアプリで録音・文字起こしされた、マネージャーとメンバーの\
1on1ミーティングの文字起こしを要約するアシスタントです。マネージャーが後から短時間で\
振り返り、次回の1on1に活かせるように、以下の構成で日本語の要約を作成してください。

## 前回からの進捗
前回までに合意していたタスクや目標に対する進捗状況(完了/継続中/未着手)を整理する。

## 話し合われた課題・懸念事項
メンバーが挙げた課題、悩み、ブロッカーを具体的に列挙する。

## 決定事項
その場で合意・決定した内容を簡潔にまとめる。

## Next Action
誰が・何を・いつまでに行うかが分かる形で、次回までのアクションアイテムを箇条書きにする。

該当する情報が文字起こしに含まれない項目は「特になし」と明記し、憶測で内容を補わないでください。";

const SALES_CALL_PROMPT: &str = "\
あなたは営業通話(商談)の文字起こしを要約するアシスタントです。営業担当者がCRMに\
記録しやすいように、以下の構成で日本語の要約を作成してください。

## 顧客の課題・ニーズ
顧客が言及した課題、要件、予算感、意思決定プロセスを整理する。

## 提案内容と反応
自社から提示した提案・デモ内容と、それに対する顧客の反応・懸念を整理する。

## 決定事項・合意事項
商談中に合意した内容(価格、範囲、日程など)を簡潔にまとめる。

## Next Step
次回のアクション(誰が・何を・いつまでに)を箇条書きにする。フォローアップの期限や\
次回商談の予定があれば明記する。

該当する情報が文字起こしに含まれない項目は「特になし」と明記し、憶測で内容を補わないでください。";

const STANDUP_PROMPT: &str = "\
あなたはチームのスタンドアップ(デイリー)ミーティングの文字起こしを要約するアシスタント\
です。以下の構成で日本語の要約を作成してください。

## 各メンバーの状況
発言した各メンバーごとに、昨日やったこと・今日やること・困っていること(ブロッカー)を\
箇条書きで整理する。話者が特定できない発言は無理に割り当てず「話者不明」としてまとめる。

## ブロッカー・エスカレーション事項
チーム全体で対応が必要な課題やエスカレーションが必要な事項を整理する。

## Next Action
誰が・何を・いつまでに対応するかを箇条書きにする。

該当する情報が文字起こしに含まれない項目は「特になし」と明記し、憶測で内容を補わないでください。";

/// Sentinel `<select>` value meaning "no override — leave
/// `AppSettings::summary_template` as `None`, so `summarize::DEFAULT_SYSTEM_PROMPT`
/// applies". Mirrors `settings::CUSTOM_MODEL`'s sentinel-value pattern for the
/// model picker, just for the opposite ("nothing selected") end of this picker.
pub(crate) const NO_TEMPLATE: &str = "__none__";

/// Sentinel `<select>` value meaning "not a built-in preset — show the freeform
/// textarea instead". Same pattern as `settings::CUSTOM_MODEL`.
pub(crate) const CUSTOM_TEMPLATE: &str = "__custom__";

/// Which `<select>` entry should be preselected for a given stored
/// `AppSettings::summary_template` value: [`NO_TEMPLATE`] for `None`, the
/// matching preset's [`SummaryTemplatePreset::key`] if `stored`'s text equals
/// that preset's [`SummaryTemplatePreset::prompt`] verbatim, or
/// [`CUSTOM_TEMPLATE`] for any other non-preset text.
///
/// Known, accepted limitation (Codex review, `AppSettings::summary_template`
/// stores resolved text only, no separate preset-vs-custom tag): if a user's
/// custom prompt happens to be byte-for-byte identical to a built-in preset's
/// text, this resolves to that preset's key on reload instead of
/// [`CUSTOM_TEMPLATE`] — the settings screen then shows it as that preset
/// selected rather than "カスタム..." with the text prefilled. The *stored*
/// value and the text actually sent to the summarizer are unaffected either
/// way (both cases use the identical prompt text); only which `<select>`
/// entry looks selected on redisplay is affected. Disambiguating would need a
/// `{ preset_key: Option<String>, text: String }`-shaped field instead of a
/// bare `Option<String>` — not worth the schema change for this edge case.
pub(crate) fn select_key_for(stored: &Option<String>) -> &'static str {
    match stored {
        None => NO_TEMPLATE,
        Some(text) => match SummaryTemplatePreset::ALL.into_iter().find(|p| p.prompt() == text) {
            Some(preset) => preset.key(),
            None => CUSTOM_TEMPLATE,
        },
    }
}

/// Resolves a stored `AppSettings::summary_template` value into the
/// [`summarize::SummarizeOptions`] `ui.rs`'s `on_generate_summary` passes to
/// `Summarizer::summarize` — `Some(template)` overrides the system prompt,
/// `None` leaves it unset so `crates/summarize`'s own
/// `DEFAULT_SYSTEM_PROMPT` fallback (`build_chat_request`/`build_cli_prompt`,
/// shared by every `Summarizer` impl regardless of provider) applies.
pub(crate) fn summarize_options_for(model: impl Into<String>, template: Option<String>) -> summarize::SummarizeOptions {
    match template {
        Some(template) => summarize::SummarizeOptions::new(model).with_system_prompt(template),
        None => summarize::SummarizeOptions::new(model),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_key_round_trips_through_from_key() {
        for preset in SummaryTemplatePreset::ALL {
            assert_eq!(SummaryTemplatePreset::from_key(preset.key()), Some(preset));
        }
    }

    #[test]
    fn from_key_returns_none_for_unknown_key() {
        assert_eq!(SummaryTemplatePreset::from_key("not-a-real-preset"), None);
    }

    #[test]
    fn one_on_one_preset_is_structured_around_progress_issues_and_next_actions() {
        let prompt = SummaryTemplatePreset::OneOnOne.prompt();
        assert!(prompt.contains("進捗"), "expected the 1on1 preset to ask about progress");
        assert!(prompt.contains("課題"), "expected the 1on1 preset to ask about issues/concerns");
        assert!(prompt.contains("Next Action"), "expected the 1on1 preset to ask for action items");
        assert!(prompt.contains("1on1-recorder"), "expected the 1on1 preset to reference this app by name");
    }

    #[test]
    fn every_preset_has_a_distinct_non_empty_prompt_and_label() {
        let prompts: Vec<&str> = SummaryTemplatePreset::ALL.iter().map(|p| p.prompt()).collect();
        for prompt in &prompts {
            assert!(!prompt.is_empty());
        }
        let mut unique_prompts = prompts.clone();
        unique_prompts.sort_unstable();
        unique_prompts.dedup();
        assert_eq!(unique_prompts.len(), prompts.len(), "expected every preset prompt to be distinct");

        let labels: Vec<&str> = SummaryTemplatePreset::ALL.iter().map(|p| p.label()).collect();
        let mut unique_labels = labels.clone();
        unique_labels.sort_unstable();
        unique_labels.dedup();
        assert_eq!(unique_labels.len(), labels.len(), "expected every preset label to be distinct");
    }

    #[test]
    fn select_key_for_none_is_no_template() {
        assert_eq!(select_key_for(&None), NO_TEMPLATE);
    }

    #[test]
    fn select_key_for_preset_text_returns_that_presets_key() {
        for preset in SummaryTemplatePreset::ALL {
            let stored = Some(preset.prompt().to_string());
            assert_eq!(select_key_for(&stored), preset.key());
        }
    }

    #[test]
    fn select_key_for_freeform_text_returns_custom_template() {
        let stored = Some("私だけのカスタムプロンプト".to_string());
        assert_eq!(select_key_for(&stored), CUSTOM_TEMPLATE);
    }

    #[test]
    fn summarize_options_for_none_leaves_system_prompt_unset_falling_back_to_builtin_default() {
        let options = summarize_options_for("claude-sonnet-4-5", None);
        assert_eq!(options.model, "claude-sonnet-4-5");
        assert!(options.system_prompt.is_none(), "None should not override summarize::DEFAULT_SYSTEM_PROMPT");
    }

    #[test]
    fn summarize_options_for_some_overrides_system_prompt() {
        let options = summarize_options_for("gpt-4o-mini", Some("custom prompt text".to_string()));
        assert_eq!(options.system_prompt.as_deref(), Some("custom prompt text"));
    }
}
