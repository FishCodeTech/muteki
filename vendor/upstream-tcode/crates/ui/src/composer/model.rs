use agent::{
    FileChangeKind, ModelSpec, OptionDescriptor, ProviderCommand, ProviderCommandKind, TokenUsage,
    UserInputQuestion,
};
use chrono::{DateTime, Local, NaiveDate, TimeZone as _};
use tcode_core::ui::ConversationDestination;

use crate::context_meter;
use crate::palette::fuzzy_score;

pub(super) enum SlashIntent {
    Model,
    Compact,
    Goal(GoalIntent),
}

/// What a `/goal` the composer runs itself asks for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GoalIntent {
    Set(String),
    Clear,
    Pause,
    Resume,
    /// Bare `/goal` or `/goal edit`: put the current objective up for editing.
    Edit,
}

/// The harness's goal control when `/goal` is Tcode's to run. A harness whose
/// goal change is a turn of its own takes it only between turns; otherwise the
/// text reaches the provider as typed.
pub(super) fn goal_offered(state: &crate::store::ComposerState) -> Option<agent::GoalControl> {
    let control = state.provider?.caps().goals?;
    (!state.conversation_read_only && (control.live || !state.turn_running && !state.stopping))
        .then_some(control)
}

/// `pause` and `resume` are subcommands only where the goal can be paused;
/// elsewhere they are an objective like any other, as the harness reads them.
pub(super) fn parse_goal(text: &str, control: agent::GoalControl) -> Option<GoalIntent> {
    let rest = text.trim().strip_prefix("/goal")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(match rest.trim() {
        "" | "edit" => GoalIntent::Edit,
        "clear" => GoalIntent::Clear,
        "pause" if control.pause => GoalIntent::Pause,
        "resume" if control.pause => GoalIntent::Resume,
        objective => GoalIntent::Set(objective.to_owned()),
    })
}

/// Whether `/compact` is Tcode's to run: the provider compacts on request and
/// no turn is running. Otherwise the text reaches the provider as typed.
pub(super) fn compact_offered(state: &crate::store::ComposerState) -> bool {
    !state.conversation_read_only
        && !state.turn_running
        && !state.stopping
        && state
            .provider
            .is_some_and(|provider| provider.caps().manual_compaction)
}

/// Strip a leading `/orchestrate` command token while rejecting lookalikes
/// such as `/orchestrated`.
pub(super) fn strip_orchestrate_prefix(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("/orchestrate")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// User-facing validation failures for a syntactically recognized `/later`.
pub(super) enum LaterError {
    MissingTime,
    MissingMessage,
    InvalidTime,
}

/// Parse a scheduled-message command against an injectable local clock. A
/// non-command returns `None`, keeping provider commands and lookalikes on the
/// ordinary submit path; recognized but malformed commands return an error.
pub(super) fn parse_later(
    text: &str,
    now: DateTime<Local>,
) -> Option<Result<(u64, String), LaterError>> {
    let text = text.trim();
    let rest = text.strip_prefix("/later")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Some(Err(LaterError::MissingTime));
    }
    let split = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let time_spec = &rest[..split];
    let message = rest[split..].trim();
    if message.is_empty() {
        return Some(Err(LaterError::MissingMessage));
    }

    let fire_at = if let Some((hour, minute)) = parse_wall_clock(time_spec) {
        let today = local_occurrence(now.date_naive(), hour, minute);
        let candidate = today.filter(|candidate| *candidate > now).or_else(|| {
            now.date_naive()
                .succ_opt()
                .and_then(|date| local_occurrence(date, hour, minute))
        });
        candidate.ok_or(LaterError::InvalidTime)
    } else {
        parse_relative(time_spec)
            .and_then(|duration| now.checked_add_signed(duration))
            .ok_or(LaterError::InvalidTime)
    };
    Some(fire_at.and_then(|fire_at| {
        u64::try_from(fire_at.timestamp())
            .map(|timestamp| (timestamp, message.to_string()))
            .map_err(|_| LaterError::InvalidTime)
    }))
}

pub(super) fn parse_wall_clock(spec: &str) -> Option<(u32, u32)> {
    let (hour, minute) = spec.split_once(':')?;
    if hour.is_empty()
        || hour.len() > 2
        || minute.len() != 2
        || !hour.bytes().all(|byte| byte.is_ascii_digit())
        || !minute.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let hour = hour.parse().ok()?;
    let minute = minute.parse().ok()?;
    (hour < 24 && minute < 60).then_some((hour, minute))
}

pub(super) fn local_occurrence(date: NaiveDate, hour: u32, minute: u32) -> Option<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_hms_opt(hour, minute, 0)?)
        .earliest()
}

pub(super) fn parse_relative(spec: &str) -> Option<chrono::Duration> {
    let (digits, unit_seconds) = if let Some(digits) = spec.strip_suffix("min") {
        (digits, 60_i64)
    } else if let Some(digits) = spec.strip_suffix('s') {
        (digits, 1_i64)
    } else {
        (spec.strip_suffix('h')?, 3_600_i64)
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let count: i64 = digits.parse().ok()?;
    if count < 1 {
        return None;
    }
    count
        .checked_mul(unit_seconds)
        .and_then(chrono::Duration::try_seconds)
}

pub(super) fn slash_command(
    text: &str,
    compact_offered: bool,
    goal_offered: Option<agent::GoalControl>,
) -> Option<SlashIntent> {
    match text.trim() {
        "/model" => Some(SlashIntent::Model),
        "/compact" if compact_offered => Some(SlashIntent::Compact),
        text => goal_offered
            .and_then(|control| parse_goal(text, control))
            .map(SlashIntent::Goal),
    }
}

/// The stable destination for unsent composer text. Draft session ids are
/// deliberately excluded: opening a project's New thread page allocates a new
/// transient session id each time.
pub(crate) type ComposerDestination = ConversationDestination;

pub(crate) fn composer_destination(
    is_draft: bool,
    session_id: &str,
    project_id: Option<&str>,
) -> Option<ComposerDestination> {
    if is_draft {
        project_id.map(|id| ComposerDestination::ProjectDraft(id.to_string()))
    } else {
        Some(ComposerDestination::Thread(session_id.to_string()))
    }
}

#[derive(Clone, Copy)]
/// Which glyph a trigger-menu row shows.
pub(super) enum MenuIcon {
    File,
    Folder,
    Command,
    /// Skill rows (`$`), populated from the provider's skills feed.
    Skill,
    Thread,
}

#[derive(Clone)]
/// What accepting a trigger-menu row does.
pub(super) enum MenuAccept {
    /// Insert the serialized `[basename](path)` mention for this relative path.
    InsertPath(String),
    /// Reference this thread from the active one: a chip, not text.
    ReferenceThread(String),
    /// Insert `$<name> ` for this provider skill.
    InsertSkill(String),
    /// Insert `/<name> ` for this provider slash command.
    InsertCommand(String),
    /// Insert the native orchestration command without consuming following text.
    InsertOrchestrate,
    /// Insert the scheduled-message command prefix.
    InsertLater,
    /// Insert the goal command prefix.
    InsertGoal,
    /// Strip the `/model` command and open the model picker.
    OpenModelPicker,
}

#[derive(Clone)]
/// One selectable row in a trigger (`@`/`/`/`$`) menu.
pub(super) struct MenuRow {
    /// Bold primary text (basename / command label).
    pub(super) primary: String,
    /// Muted secondary text (parent path / description).
    pub(super) secondary: String,
    pub(super) icon: MenuIcon,
    pub(super) accept: MenuAccept,
    /// Group heading emitted before this row when its group changes.
    /// `None` leaves the file-mention menu ungrouped.
    pub(super) group: Option<&'static str>,
}

/// Select and rank one provider-command kind with the same fuzzy subsequence
/// matcher as the command palette. Empty queries preserve provider order;
/// non-empty queries put the strongest matches first and never truncate.
pub(super) fn filter_provider_commands<'a>(
    commands: &'a [ProviderCommand],
    kind: ProviderCommandKind,
    query: &str,
) -> Vec<&'a ProviderCommand> {
    let mut matched: Vec<(i32, usize, &ProviderCommand)> = commands
        .iter()
        .enumerate()
        .filter(|(_, command)| command.kind == kind)
        .filter_map(|(index, command)| {
            fuzzy_score(query, &command.name).map(|score| (score, index, command))
        })
        .collect();
    if !query.is_empty() {
        matched.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    matched.into_iter().map(|(_, _, command)| command).collect()
}

pub(super) fn option_selection_str<'a>(
    selections: &'a [agent::OptionSelection],
    id: &str,
) -> Option<&'a str> {
    selections
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.value.as_str())
}

pub(super) fn option_selection_bool(
    selections: &[agent::OptionSelection],
    id: &str,
) -> Option<bool> {
    selections
        .iter()
        .find(|s| s.id == id)
        .and_then(|s| s.value.as_bool())
}

/// The resolved value of a select descriptor: an accepted persisted selection,
/// else the descriptor default.
pub(super) fn resolved_select_value(
    id: &str,
    options: &[agent::SelectOption],
    default_value: &Option<String>,
    selections: &[agent::OptionSelection],
) -> Option<String> {
    option_selection_str(selections, id)
        .filter(|v| options.iter().any(|o| &o.value == v))
        .map(str::to_string)
        .or_else(|| default_value.clone())
}

/// The traits chip label: every resolved descriptor label joined with " · "
/// (e.g. "High", "High · Fast", "Thinking Off"). `None` when the
/// model has no descriptors.
pub(super) fn traits_chip_label(
    spec: &ModelSpec,
    selections: &[agent::OptionSelection],
) -> Option<String> {
    if spec.options.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for descriptor in &spec.options {
        match descriptor {
            OptionDescriptor::Select {
                id,
                label,
                options,
                default_value,
                ..
            } => {
                let part = resolved_select_value(id, options, default_value, selections)
                    .and_then(|value| options.iter().find(|o| o.value == value))
                    .map(|option| option.label.clone())
                    .unwrap_or_else(|| label.clone());
                parts.push(part);
            }
            OptionDescriptor::Boolean {
                id,
                label,
                default_value,
                ..
            } => {
                let on = option_selection_bool(selections, id).unwrap_or(*default_value);
                if id == "fastMode" {
                    parts.push(
                        crate::tr!(if on {
                            "composer.trait_fast"
                        } else {
                            "composer.trait_normal"
                        })
                        .into_owned(),
                    );
                } else {
                    let state = crate::tr!(if on { "composer.on" } else { "composer.off" });
                    parts.push(format!("{label} {state}"));
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Build the answers map for a user-input request: keyed by question id, with a
/// string (single-select / free-text) or string-array (multi-select) value. A
/// non-empty custom answer overrides the current question's selections.
/// A question counts as answered once it carries at least one selection (or a
/// recorded custom-text answer, which is stored the same way).
pub(super) fn user_input_answered(
    question: &UserInputQuestion,
    selections: &std::collections::HashMap<String, Vec<String>>,
) -> bool {
    selections
        .get(&question.id)
        .is_some_and(|selected| !selected.is_empty())
}

pub(super) fn user_input_all_answered(
    questions: &[UserInputQuestion],
    selections: &std::collections::HashMap<String, Vec<String>>,
) -> bool {
    questions
        .iter()
        .all(|question| user_input_answered(question, selections))
}

/// The next unanswered question after `from`, wrapping around to earlier ones
/// (and back to `from` itself) so a skipped question is always revisited.
pub(super) fn next_unanswered_question(
    questions: &[UserInputQuestion],
    selections: &std::collections::HashMap<String, Vec<String>>,
    from: usize,
) -> Option<usize> {
    let total = questions.len();
    if total == 0 {
        return None;
    }
    (1..=total)
        .map(|step| (from + step) % total)
        .find(|&i| !user_input_answered(&questions[i], selections))
}

pub(super) fn assemble_user_input_answers(
    questions: &[UserInputQuestion],
    selections: &std::collections::HashMap<String, Vec<String>>,
    current_index: usize,
    custom_current: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (i, question) in questions.iter().enumerate() {
        if i == current_index
            && let Some(text) = custom_current.map(str::trim).filter(|t| !t.is_empty())
        {
            map.insert(
                question.id.clone(),
                serde_json::Value::String(text.to_string()),
            );
            continue;
        }
        let selected = selections.get(&question.id).cloned().unwrap_or_default();
        let value = if question.multi_select {
            serde_json::Value::Array(
                selected
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            )
        } else {
            serde_json::Value::String(selected.into_iter().next().unwrap_or_default())
        };
        map.insert(question.id.clone(), value);
    }
    map
}

pub(super) fn current_model_name(catalog: &[ModelSpec], model: Option<&str>) -> String {
    match model {
        Some(id) => catalog
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| id.to_string()),
        None => crate::tr!("composer.default_model").into_owned(),
    }
}

/// The picker button's label: the resolved row's name (so a custom slug shows
/// its own name), else the catalog's, else the raw id.
pub(super) fn current_model_name_resolved(
    resolved: &[tcode_core::provider_models::ResolvedModel],
    catalog: &[ModelSpec],
    model: Option<&str>,
) -> String {
    let Some(id) = model else {
        return crate::tr!("composer.default_model").into_owned();
    };
    resolved
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| current_model_name(catalog, Some(id)))
}

/// The context chip label: "42k / 200k" when both known, "200k" when only the
/// window is known, "Context" when nothing is known.
pub(super) fn context_label(usage: Option<TokenUsage>) -> String {
    match usage {
        Some(u) => {
            let window = u.context_window;
            let used = u.used_tokens.or(u.input_tokens);
            match (used, window) {
                (Some(used), Some(window)) => {
                    format!(
                        "{} / {}",
                        context_meter::format_tokens(Some(used)),
                        context_meter::format_tokens(Some(window))
                    )
                }
                (Some(used), None) => context_meter::format_tokens(Some(used)),
                (None, Some(window)) => context_meter::format_tokens(Some(window)),
                (None, None) => crate::tr!("composer.context").into_owned(),
            }
        }
        None => crate::tr!("composer.context").into_owned(),
    }
}

pub(super) fn file_change_kind_label(kind: FileChangeKind) -> &'static str {
    match kind {
        FileChangeKind::Create => "create",
        FileChangeKind::Modify => "modify",
        FileChangeKind::Delete => "delete",
        FileChangeKind::Rename => "rename",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike as _;

    #[test]
    fn orchestrate_prefix_requires_a_complete_command_token() {
        assert_eq!(strip_orchestrate_prefix("/orchestrate"), Some(""));
        assert_eq!(
            strip_orchestrate_prefix("/orchestrate   split the work"),
            Some("split the work")
        );
        assert_eq!(strip_orchestrate_prefix("/orchestrated"), None);
        assert_eq!(strip_orchestrate_prefix("hello /orchestrate"), None);
    }

    fn local_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn parsed_later(text: &str, now: DateTime<Local>) -> (DateTime<Local>, String) {
        let (timestamp, message) = parse_later(text, now).unwrap().unwrap();
        (
            Local.timestamp_opt(timestamp as i64, 0).single().unwrap(),
            message,
        )
    }

    #[test]
    fn later_parses_schedules_and_rejects_invalid_or_overflowing_input() {
        let now = local_time(2026, 8, 8, 12, 0);
        let (later_today, message) = parsed_later("/later 23:59 continue here", now);
        assert_eq!(later_today.date_naive(), now.date_naive());
        assert_eq!((later_today.hour(), later_today.minute()), (23, 59));
        assert_eq!(message, "continue here");

        let (tomorrow, message) = parsed_later("/later 5:10 first line\nsecond line", now);
        assert_eq!(tomorrow.date_naive(), now.date_naive().succ_opt().unwrap());
        assert_eq!((tomorrow.hour(), tomorrow.minute()), (5, 10));
        assert_eq!(message, "first line\nsecond line");
        let (same_time_tomorrow, _) = parsed_later("/later 12:00 next day", now);
        assert_eq!(
            same_time_tomorrow.date_naive(),
            now.date_naive().succ_opt().unwrap()
        );

        for (command, seconds, message) in [
            ("/later 5min multi word", 300, "multi word"),
            ("/later 30s soon", 30, "soon"),
            ("/later 2h much later", 7_200, "much later"),
        ] {
            let (fire_at, parsed_message) = parsed_later(command, now);
            assert_eq!((fire_at - now).num_seconds(), seconds);
            assert_eq!(parsed_message, message);
        }
        for (command, error) in [
            ("/later", LaterError::MissingTime),
            ("/later 5min", LaterError::MissingMessage),
            ("/later 25:00 x", LaterError::InvalidTime),
            ("/later 5:99 x", LaterError::InvalidTime),
            ("/later abc x", LaterError::InvalidTime),
            ("/later 0s x", LaterError::InvalidTime),
            ("/later -1h x", LaterError::InvalidTime),
            ("/later 9223372036854775s x", LaterError::InvalidTime),
            ("/later 9223372036854775807s x", LaterError::InvalidTime),
            ("/later 9223372036854775807h x", LaterError::InvalidTime),
            ("/later 999999999999999999999min x", LaterError::InvalidTime),
        ] {
            assert_eq!(parse_later(command, now), Some(Err(error)), "{command}");
        }
        assert_eq!(parse_later("/laters 5min x", now), None);
    }

    /// Codex's `/goal` takes `pause` and `resume`; Claude Code's reads any
    /// word as the condition, so there they are sent on as one.
    #[test]
    fn goal_subcommands_follow_what_the_harness_can_do() {
        let [claude, codex] = [agent::ProviderKind::ClaudeCode, agent::ProviderKind::Codex]
            .map(|provider| provider.caps().goals.unwrap());
        let goal = |text, control| parse_goal(text, control);
        assert_eq!(
            goal("/goal  ship the strip ", codex),
            Some(GoalIntent::Set("ship the strip".into()))
        );
        assert_eq!(goal("/goal clear", claude), Some(GoalIntent::Clear));
        assert_eq!(goal("/goal", claude), Some(GoalIntent::Edit));
        assert_eq!(goal("/goal pause", codex), Some(GoalIntent::Pause));
        assert_eq!(goal("/goal resume", codex), Some(GoalIntent::Resume));
        assert_eq!(
            goal("/goal pause", claude),
            Some(GoalIntent::Set("pause".into()))
        );
        assert_eq!(goal("/goals", codex), None);
        assert_eq!(goal("ship /goal", codex), None);
    }

    #[test]
    fn provider_menu_filter_is_fuzzy_kind_aware_and_uncapped() {
        let mut commands: Vec<ProviderCommand> = (0..60)
            .map(|index| ProviderCommand {
                name: format!("command-{index:02}"),
                description: Some(format!("Command {index}")),
                kind: ProviderCommandKind::Command,
            })
            .collect();
        commands.push(ProviderCommand {
            name: "deep-review".into(),
            description: None,
            kind: ProviderCommandKind::Skill,
        });

        let all = filter_provider_commands(&commands, ProviderCommandKind::Command, "");
        assert_eq!(all.len(), 60);
        assert_eq!(all[0].name, "command-00");
        assert_eq!(all[59].name, "command-59");

        // Fuzzy subsequence rather than prefix-only matching.
        let fuzzy = filter_provider_commands(&commands, ProviderCommandKind::Skill, "dprv");
        assert_eq!(fuzzy.len(), 1);
        assert_eq!(fuzzy[0].name, "deep-review");
        assert!(
            filter_provider_commands(&commands, ProviderCommandKind::Command, "dprv").is_empty()
        );
    }

    #[test]
    fn context_label_variants() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        assert_eq!(context_label(None), "Context");
        assert_eq!(
            context_label(Some(TokenUsage {
                used_tokens: Some(42_000),
                context_window: Some(200_000),
                ..Default::default()
            })),
            "42k / 200k"
        );
        assert_eq!(
            context_label(Some(TokenUsage {
                context_window: Some(200_000),
                ..Default::default()
            })),
            "200k"
        );
    }

    #[test]
    fn traits_chip_joins_descriptor_labels() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // Fast Mode boolean → Fast/Normal; a plain boolean → "<Label> On/Off".
        let fast = agent::ModelSpec {
            id: "m".into(),
            display_name: "m".into(),
            is_default: false,
            options: vec![agent::OptionDescriptor::Boolean {
                id: "fastMode".into(),
                label: "Fast Mode".into(),
                default_value: false,
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
            }],
        };
        assert_eq!(traits_chip_label(&fast, &[]), Some("Normal".into()));
        let thinking = agent::ModelSpec {
            id: "h".into(),
            display_name: "h".into(),
            is_default: false,
            options: vec![agent::OptionDescriptor::Boolean {
                id: "thinking".into(),
                label: "Thinking".into(),
                default_value: false,
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
            }],
        };
        assert_eq!(
            traits_chip_label(&thinking, &[]),
            Some("Thinking Off".into())
        );
        let unresolved = agent::ModelSpec {
            id: "u".into(),
            display_name: "u".into(),
            is_default: false,
            options: vec![agent::OptionDescriptor::Select {
                id: "reasoningEffort".into(),
                label: "Thinking".into(),
                options: vec![agent::SelectOption {
                    value: "high".into(),
                    label: "High".into(),
                    description: None,
                    unavailable: None,
                }],
                default_value: None,
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
            }],
        };
        assert_eq!(traits_chip_label(&unresolved, &[]), Some("Thinking".into()));
        let bare = agent::ModelSpec {
            id: "b".into(),
            display_name: "b".into(),
            is_default: false,
            options: Vec::new(),
        };
        assert_eq!(traits_chip_label(&bare, &[]), None);
    }

    #[test]
    fn user_input_answer_flow_advances_to_unanswered_and_detects_completion() {
        let question = |id: &str| UserInputQuestion {
            id: id.into(),
            header: id.into(),
            question: id.into(),
            options: vec![agent::UserInputOption {
                label: "A".into(),
                description: String::new(),
            }],
            multi_select: false,
            prefill: None,
        };
        let questions = vec![question("q1"), question("q2"), question("q3")];
        let mut selections = std::collections::HashMap::new();

        assert!(!user_input_all_answered(&questions, &selections));
        assert_eq!(
            next_unanswered_question(&questions, &selections, 0),
            Some(1)
        );

        // q1 and q3 answered: from q1 the hop skips answered q3 and lands on q2;
        // an empty selection does not count as an answer.
        selections.insert("q1".into(), vec!["A".into()]);
        selections.insert("q2".into(), Vec::new());
        selections.insert("q3".into(), vec!["A".into()]);
        assert!(!user_input_all_answered(&questions, &selections));
        assert_eq!(
            next_unanswered_question(&questions, &selections, 2),
            Some(1)
        );

        // The wrap search revisits a skipped earlier question from the end.
        assert_eq!(
            next_unanswered_question(&questions, &selections, 0),
            Some(1)
        );

        selections.insert("q2".into(), vec!["custom text".into()]);
        assert!(user_input_all_answered(&questions, &selections));
        assert_eq!(next_unanswered_question(&questions, &selections, 0), None);

        assert_eq!(next_unanswered_question(&[], &selections, 0), None);
    }

    #[test]
    fn user_input_answers_assemble_with_multi_and_custom_override() {
        let questions = vec![
            UserInputQuestion {
                id: "q1".into(),
                header: "H1".into(),
                question: "Pick one".into(),
                options: vec![
                    agent::UserInputOption {
                        label: "A".into(),
                        description: String::new(),
                    },
                    agent::UserInputOption {
                        label: "B".into(),
                        description: String::new(),
                    },
                ],
                multi_select: false,
                prefill: None,
            },
            UserInputQuestion {
                id: "q2".into(),
                header: "H2".into(),
                question: "Pick many".into(),
                options: vec![
                    agent::UserInputOption {
                        label: "X".into(),
                        description: String::new(),
                    },
                    agent::UserInputOption {
                        label: "Y".into(),
                        description: String::new(),
                    },
                ],
                multi_select: true,
                prefill: None,
            },
        ];
        let mut selections = std::collections::HashMap::new();
        selections.insert("q1".to_string(), vec!["A".to_string()]);
        selections.insert("q2".to_string(), vec!["X".to_string(), "Y".to_string()]);

        let answers = assemble_user_input_answers(&questions, &selections, 0, None);
        assert_eq!(answers["q1"], serde_json::json!("A"));
        assert_eq!(answers["q2"], serde_json::json!(["X", "Y"]));

        let answers = assemble_user_input_answers(&questions, &selections, 0, Some("  freehand  "));
        assert_eq!(answers["q1"], serde_json::json!("freehand"));
        assert_eq!(answers["q2"], serde_json::json!(["X", "Y"]));

        let answers = assemble_user_input_answers(&questions, &selections, 0, Some("   "));
        assert_eq!(answers["q1"], serde_json::json!("A"));

        let answers =
            assemble_user_input_answers(&questions, &std::collections::HashMap::new(), 0, None);
        assert_eq!(answers["q1"], serde_json::json!(""));
        assert_eq!(answers["q2"], serde_json::json!([]));
    }
}
