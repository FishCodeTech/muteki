//! Messages the host writes into a thread in the user's role. The model reads
//! exactly the text, and every client reads the notice back from that text, so
//! a pending steer, a queued turn, a reload and an older log all show the same
//! notice.

use crate::pull_request_watch::{self, WatchNotice};

/// Opens every `tcode_control` message the host writes into a thread: a watch firing,
/// or the answer to a grant request.
pub const CONTROL_CALLBACK_PREFIX: &str = "[control] ";

/// Opens every orchestrate child-thread callback.
pub const ORCHESTRATE_CALLBACK_PREFIX: &str = "[orchestrate] thread ";
/// Follows the callback header of a child waiting on an approval.
pub const ORCHESTRATE_APPROVAL_PHRASE: &str = "is waiting for approval: ";
/// Follows the callback header of a child asking its lead a question.
pub const ORCHESTRATE_QUESTION_PHRASE: &str = "is asking (";

/// Opens a callback body carrying the report the child chose with
/// `report_result`.
pub const ORCHESTRATE_REPORT_LABEL: &str = "Result (reported via report_result):\n";
/// Opens a callback body carrying a child's final message because it went
/// quiet twice without calling `report_result`.
pub const ORCHESTRATE_FALLBACK_LABEL: &str =
    "Final message (the child ended without report_result; this is a fallback, not its report):\n";
/// Opens the line before the tail of a final message too long to deliver whole.
pub const ORCHESTRATE_OUTPUT_TAIL_LABEL: &str = "Final output tail (";

/// Sent to a child that ended its turn owing a report, once per expectation.
pub const CHILD_REPORT_REMINDER: &str = include_str!("../../../assets/orchestrate/reminder.md");

/// `[orchestrate] thread {child_id} ("{title}")`, which every callback opens with.
pub fn callback_header(child_id: &str, title: &str) -> String {
    format!("{ORCHESTRATE_CALLBACK_PREFIX}{child_id} (\"{title}\")")
}

/// What the host told the thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostNotice {
    /// Callbacks that raced into one wake-up turn arrive as one message, in
    /// delivery order.
    Orchestrate(Vec<OrchestrateCallback>),
    PullRequest {
        number: u64,
        notice: WatchNotice,
    },
    /// [`CHILD_REPORT_REMINDER`].
    ReportReminder,
    /// `tcode_control` messages that raced into one wake, each without its prefix.
    Control(Vec<String>),
}

/// Read a user-role message as the notice the host wrote, or `None` for
/// anything else.
pub fn host_notice(text: &str) -> Option<HostNotice> {
    if let Some((number, notice)) = pull_request_watch::parse_message(text) {
        return Some(HostNotice::PullRequest { number, notice });
    }
    if text.trim() == CHILD_REPORT_REMINDER.trim() {
        return Some(HostNotice::ReportReminder);
    }
    if let Some(messages) = parse_control(text) {
        return Some(HostNotice::Control(messages));
    }
    parse_callbacks(text).map(HostNotice::Orchestrate)
}

/// The runtime joins control messages that race into one wake with a blank line, and
/// never merges them with orchestrate callbacks.
fn parse_control(text: &str) -> Option<Vec<String>> {
    text.split("\n\n")
        .map(|message| {
            message
                .strip_prefix(CONTROL_CALLBACK_PREFIX)
                .map(str::to_owned)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackEvent {
    /// The state word the host wrote: `reported`, `updated`, `unreported`,
    /// `failed`, or an older log's `completed`.
    Ended(String),
    Approval,
    Question,
}

/// One orchestrate child-thread callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestrateCallback {
    pub child_id: String,
    /// The child thread's title (may itself contain quotes).
    pub title: String,
    pub event: CallbackEvent,
    /// For an ending, everything after the header line; for a request,
    /// everything after the header.
    pub body: String,
}

impl OrchestrateCallback {
    /// The child's own words without the labels the callback frames them
    /// with: its report, or else its final message. `None` for a request.
    pub fn report(&self) -> Option<&str> {
        if !matches!(self.event, CallbackEvent::Ended(_)) {
            return None;
        }
        if let Some(report) = self.body.strip_prefix(ORCHESTRATE_REPORT_LABEL) {
            return Some(report);
        }
        let body = self
            .body
            .strip_prefix(ORCHESTRATE_FALLBACK_LABEL)
            .unwrap_or(&self.body);
        if body.starts_with(ORCHESTRATE_OUTPUT_TAIL_LABEL) {
            return Some(body.split_once('\n').map_or("", |(_, tail)| tail));
        }
        Some(body)
    }
}

/// The runtime joins callbacks that race into one wake with a blank line; a
/// blank line inside a report is not a boundary unless a callback header
/// follows it.
fn parse_callbacks(text: &str) -> Option<Vec<OrchestrateCallback>> {
    let separator = format!("\n\n{ORCHESTRATE_CALLBACK_PREFIX}");
    let mut callbacks = Vec::new();
    let mut start = 0;
    for (at, _) in text.match_indices(&separator) {
        if parse_callback(&text[at + 2..]).is_none() {
            continue;
        }
        callbacks.push(parse_callback(&text[start..at])?);
        start = at + 2;
    }
    callbacks.push(parse_callback(&text[start..])?);
    Some(callbacks)
}

/// `{header} {state}.{tokens}\n{body}` for an ending; `{header} is waiting for
/// approval: …` or `{header} is asking (…` for a request.
fn parse_callback(text: &str) -> Option<OrchestrateCallback> {
    let rest = text.strip_prefix(ORCHESTRATE_CALLBACK_PREFIX)?;
    // The id has no spaces, so the first ` ("` opens the title.
    let open = rest.find(" (\"")?;
    let child_id = &rest[..open];
    let after = &rest[open + 3..];
    if child_id.is_empty() || child_id.contains(char::is_whitespace) {
        return None;
    }
    for (phrase, event) in [
        (ORCHESTRATE_APPROVAL_PHRASE, CallbackEvent::Approval),
        (ORCHESTRATE_QUESTION_PHRASE, CallbackEvent::Question),
    ] {
        if let Some(close) = after.find(&format!("\") {phrase}")) {
            return Some(OrchestrateCallback {
                child_id: child_id.to_owned(),
                title: after[..close].to_owned(),
                event,
                body: after[close + 3..].to_owned(),
            });
        }
    }
    let (first_line, body) = after.split_once('\n').unwrap_or((after, ""));
    // Titles may contain quotes, but the state word after the last `") ` never does.
    let close = first_line.rfind("\") ")?;
    let state = first_line[close + 3..]
        .split('.')
        .next()
        .unwrap_or("")
        .trim();
    if state.is_empty() || state.contains(char::is_whitespace) {
        return None;
    }
    Some(OrchestrateCallback {
        child_id: child_id.to_owned(),
        title: first_line[..close].to_owned(),
        event: CallbackEvent::Ended(state.to_owned()),
        body: body.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callbacks(text: &str) -> Vec<OrchestrateCallback> {
        match host_notice(text) {
            Some(HostNotice::Orchestrate(callbacks)) => callbacks,
            other => panic!("not a callback: {other:?}"),
        }
    }

    #[test]
    fn endings_read_the_wire_format() {
        let [normal] = &callbacks(
            "[orchestrate] thread child-7 (\"Investigate zed terminal\") reported. tokens: input 5, output 3, total 8.\nResult (reported via report_result):\nHere is the report.\nSecond line.",
        )[..] else {
            panic!("one callback");
        };
        assert_eq!(normal.child_id, "child-7");
        assert_eq!(normal.title, "Investigate zed terminal");
        assert_eq!(normal.event, CallbackEvent::Ended("reported".into()));
        assert_eq!(normal.report(), Some("Here is the report.\nSecond line."));

        let [quoted] =
            &callbacks("[orchestrate] thread abc (\"He said \"hi\" twice\") failed.\nbody")[..]
        else {
            panic!("one callback");
        };
        assert_eq!(quoted.title, "He said \"hi\" twice");
        assert_eq!(quoted.event, CallbackEvent::Ended("failed".into()));
        assert_eq!(quoted.body, "body");

        let [no_body] = &callbacks("[orchestrate] thread c (\"Title\") completed.")[..] else {
            panic!("one callback");
        };
        assert_eq!(no_body.body, "");

        assert_eq!(host_notice("Please run the tests"), None);
        assert_eq!(host_notice("[orchestrate] thread only-a-header"), None);
    }

    /// Callbacks that raced into one wake are one message; a blank line in a
    /// report is not a boundary.
    #[test]
    fn merged_callbacks_split_at_each_header() {
        let merged = callbacks(
            "[orchestrate] thread a (\"First\") reported.\nResult (reported via report_result):\npara one\n\npara two\n\n[orchestrate] thread b (\"Second\") failed.\nthe provider closed",
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].child_id, "a");
        assert_eq!(merged[0].report(), Some("para one\n\npara two"));
        assert_eq!(merged[1].child_id, "b");
        assert_eq!(merged[1].body, "the provider closed");
    }

    /// Requests carry JSON after the header, whose `") ` must not be read as
    /// the end of the title.
    #[test]
    fn approvals_and_questions_are_requests_not_results() {
        let [approval] = &callbacks(
            "[orchestrate] thread kid (\"Fix (\"x\")\") is waiting for approval: Bash: rm -rf build (request_id: r1). Native options: [{\"id\":\"allow\",\"label\":\"Allow\") \"}]. The user answers in the child thread.",
        )[..] else {
            panic!("one callback");
        };
        assert_eq!(approval.title, "Fix (\"x\")");
        assert_eq!(approval.event, CallbackEvent::Approval);
        assert!(approval.body.starts_with("is waiting for approval: Bash"));
        assert_eq!(approval.report(), None);

        let [question] = &callbacks(
            "[orchestrate] thread kid (\"Fix\") is asking (request_id: q1; the child is blocked until it is answered). Questions: [{\"id\":\"a\"}]. The user answers in the child thread.",
        )[..] else {
            panic!("one callback");
        };
        assert_eq!(question.event, CallbackEvent::Question);
        assert_eq!(question.report(), None);
    }

    #[test]
    fn merged_control_messages_are_one_notice() {
        assert_eq!(
            host_notice(
                "[control] thread a (\"First\") turn_completed: completed\n\n[control] thread b (\"Second\") idle"
            ),
            Some(HostNotice::Control(vec![
                "thread a (\"First\") turn_completed: completed".into(),
                "thread b (\"Second\") idle".into(),
            ]))
        );
        assert_eq!(host_notice("[control]no space"), None);
    }

    #[test]
    fn the_reminder_and_watch_messages_are_notices() {
        assert_eq!(
            host_notice(CHILD_REPORT_REMINDER.trim()),
            Some(HostNotice::ReportReminder)
        );
        assert_eq!(
            host_notice(&pull_request_watch::merged_message(5, "https://x/pull/5")),
            Some(HostNotice::PullRequest {
                number: 5,
                notice: WatchNotice::Merged
            })
        );
    }
}
