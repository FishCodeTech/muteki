//! What a pull request watch has told its agent, and what a fresh read adds to it.

use serde::{Deserialize, Serialize};

use crate::pull_request::{Mergeability, PullRequestState};

/// Wakes in a row that bring only comments. Check, conflict or push news resets the count, so
/// this only stops a chatty bot looping an agent that is replying to it.
pub const WAKE_LIMIT: u32 = 10;
/// Reads in a row that failed for a reason other than a rate limit before the watch ends.
pub const READ_FAILURE_LIMIT: u32 = 8;
const LISTED_ITEMS: usize = 10;
const SNIPPET_LENGTH: usize = 200;

/// A watch on one linked pull request. `started_at` names the generation: a stop and a new
/// start make a new one, and a read begun for an older generation is discarded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestWatch {
    pub started_at: String,
    pub head_sha: Option<String>,
    /// Checks the agent was last told failed on `head_sha`.
    pub failed_checks: Vec<String>,
    pub passed: bool,
    pub passed_checks: Vec<String>,
    /// Remarks are new when active after this time, or at it with an id not in `remark_ids`.
    pub remarks_through: String,
    pub remark_ids: Vec<String>,
    pub conflicting: bool,
    /// Comment-only wakes in a row.
    pub wakes: u32,
    /// Written with the watermark it reports and cleared when the provider accepts it, so a
    /// restart in between delivers it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_wake: Option<PendingWake>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWake {
    pub id: String,
    pub text: String,
    /// The watch ends once this message is delivered: it reports why it stopped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub last: bool,
}

impl PullRequestWatch {
    pub fn new(now_ms: u64) -> Self {
        let started_at = chrono::DateTime::from_timestamp_millis(now_ms as i64)
            .unwrap_or_default()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Self {
            remarks_through: started_at.clone(),
            started_at,
            head_sha: None,
            failed_checks: Vec::new(),
            passed: false,
            passed_checks: Vec::new(),
            remark_ids: Vec::new(),
            conflicting: false,
            wakes: 0,
            pending_wake: None,
        }
    }

    /// Still reading: an ending watch only waits for its last message to be delivered.
    pub fn active(&self) -> bool {
        self.pending_wake.as_ref().is_none_or(|wake| !wake.last)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pending,
    ActionRequired,
    Success,
    Failure,
    Skipped,
    Neutral,
    Cancelled,
}

impl CheckStatus {
    /// "action-required" is a finished check that needs someone, so the agent hears about it.
    pub fn failed(self) -> bool {
        matches!(self, Self::Failure | Self::Cancelled | Self::ActionRequired)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestCheck {
    pub name: String,
    pub status: CheckStatus,
    pub url: Option<String>,
    /// Only where the host says; absent is neither required nor advisory.
    pub required: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRemark {
    pub id: String,
    pub author: Option<String>,
    pub body: String,
    pub created_at: String,
    pub edited_at: Option<String>,
    pub url: Option<String>,
    pub path: Option<String>,
    pub review_state: Option<String>,
}

impl PullRequestRemark {
    /// An edit counts as new activity, so a bot that rewrites one summary still wakes the agent.
    fn active_at(&self) -> &str {
        self.edited_at.as_deref().unwrap_or(&self.created_at)
    }
}

/// The detail read a pass compares against the watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestWatchRead {
    pub state: PullRequestState,
    pub head_sha: Option<String>,
    pub base_branch: String,
    pub checks: Vec<PullRequestCheck>,
    pub mergeability: Mergeability,
    /// The authenticated account, whose own remarks never wake its agent.
    pub viewer: Option<String>,
    pub author: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchChange {
    ChecksFailed(Vec<PullRequestCheck>),
    ChecksPassed { count: usize, required: bool },
    Remarks(Vec<PullRequestRemark>),
    Conflicting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchChangeKind {
    ChecksFailed,
    ChecksPassed,
    NewComments,
    MergeConflict,
}

impl WatchChange {
    pub fn kind(&self) -> WatchChangeKind {
        match self {
            Self::ChecksFailed(_) => WatchChangeKind::ChecksFailed,
            Self::ChecksPassed { .. } => WatchChangeKind::ChecksPassed,
            Self::Remarks(_) => WatchChangeKind::NewComments,
            Self::Conflicting => WatchChangeKind::MergeConflict,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchReport {
    /// What the agent has not been told yet. Empty means no wake.
    pub changes: Vec<WatchChange>,
    /// The watch to record, whether or not anything is reported.
    pub next: PullRequestWatch,
    /// This report spends the last wake before the limit, so watching stops after it.
    pub exhausted: bool,
}

fn millis(at: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|at| at.timestamp_millis())
}

/// The checks that decide "passed": the required ones, or all of them where none is required;
/// and whether they are the required ones.
fn gate(checks: &[PullRequestCheck]) -> (Vec<&PullRequestCheck>, bool) {
    let required: Vec<_> = checks
        .iter()
        .filter(|check| check.required == Some(true))
        .collect();
    if required.is_empty() {
        (checks.iter().collect(), false)
    } else {
        (required, true)
    }
}

fn gate_passed(gate: &[&PullRequestCheck]) -> bool {
    gate.iter()
        .all(|check| check.status != CheckStatus::Pending && !check.status.failed())
}

/// Whether `checks` would be reported passed. A host consults the checks it requires but has
/// not reported only then: they can only hold that verdict back.
pub fn checks_pass(checks: &[PullRequestCheck]) -> bool {
    !checks.is_empty() && gate_passed(&gate(checks).0)
}

/// Compare a read with what the agent was last told. Each check is reported as soon as it fails,
/// so a check that never finishes cannot hold the news back. "Passed" is reported once the
/// required checks all passed, or all checks where none is required. Remarks count when someone
/// other than the agent's own account wrote them. `remarks` is `None` when the conversation could
/// not be read completely; remarks then wait for a later pass.
pub fn evaluate(
    watch: &PullRequestWatch,
    read: &PullRequestWatchRead,
    remarks: Option<&[PullRequestRemark]>,
) -> WatchReport {
    let mut changes = Vec::new();
    let head_moved = read.head_sha != watch.head_sha;
    let mut failed_checks = if head_moved {
        Vec::new()
    } else {
        watch.failed_checks.clone()
    };
    let mut passed = !head_moved && watch.passed;
    let mut passed_checks = if head_moved {
        Vec::new()
    } else {
        watch.passed_checks.clone()
    };
    // An empty list keeps the last state: a host can answer with one when its check read fails.
    if !read.checks.is_empty() {
        let failed: Vec<_> = read
            .checks
            .iter()
            .filter(|check| check.status.failed())
            .cloned()
            .collect();
        let newly_failed: Vec<_> = failed
            .iter()
            .filter(|check| !failed_checks.contains(&check.name))
            .cloned()
            .collect();
        if !newly_failed.is_empty() {
            changes.push(WatchChange::ChecksFailed(newly_failed));
        }
        // A check that runs again leaves the list, so a rerun that fails again is reported.
        failed_checks = failed.into_iter().map(|check| check.name).collect();

        let (gate, required) = gate(&read.checks);
        let passed_now = gate_passed(&gate);
        let gate_names: Vec<_> = gate.iter().map(|check| check.name.clone()).collect();
        // A watch saved before passed_checks existed takes the current names, so it does not wake.
        let told = if passed && passed_checks.is_empty() {
            &gate_names
        } else {
            &passed_checks
        };
        // A required job created and finished between two passes is never seen pending. Without
        // required checks any check counts, and advisory bots keep adding passed ones: no wake.
        let gate_grew = required && gate_names.iter().any(|name| !told.contains(name));
        if passed_now && (!passed || gate_grew) {
            changes.push(WatchChange::ChecksPassed {
                count: gate.len(),
                required,
            });
        }
        passed = passed_now;
        passed_checks = if passed_now { gate_names } else { Vec::new() };
    }

    let own = read
        .viewer
        .as_ref()
        .or(read.author.as_ref())
        .map(|login| login.to_lowercase());
    let through = millis(&watch.remarks_through);
    // GitHub times are per second, so remarks at the boundary time are told apart by id.
    let fresh: Vec<_> = remarks
        .unwrap_or_default()
        .iter()
        .filter(|remark| {
            let (Some(at), Some(through)) = (millis(remark.active_at()), through) else {
                return false;
            };
            (at > through || (at == through && !watch.remark_ids.contains(&remark.id)))
                && remark.author.as_ref().map(|login| login.to_lowercase()) != own
        })
        .cloned()
        .collect();
    let latest = fresh
        .iter()
        .filter_map(|remark| millis(remark.active_at()))
        .chain(through)
        .max();
    let at_latest: Vec<_> = fresh
        .iter()
        .filter(|remark| millis(remark.active_at()) == latest)
        .collect();
    let (remarks_through, remark_ids) = if latest == through {
        let mut ids = watch.remark_ids.clone();
        ids.extend(at_latest.iter().map(|remark| remark.id.clone()));
        (watch.remarks_through.clone(), ids)
    } else {
        (
            at_latest[0].active_at().to_owned(),
            at_latest.iter().map(|remark| remark.id.clone()).collect(),
        )
    };
    if !fresh.is_empty() {
        changes.push(WatchChange::Remarks(fresh));
    }

    if read.mergeability == Mergeability::Conflicting && !watch.conflicting {
        changes.push(WatchChange::Conflicting);
    }
    // A host still computing after a push, or one with no answer, says nothing of the conflict;
    // only a clean answer clears it.
    let conflicting = match read.mergeability {
        Mergeability::Pending | Mergeability::Unknown => watch.conflicting,
        Mergeability::Conflicting => true,
        Mergeability::Clean => false,
    };

    let comments_only = !changes.is_empty()
        && changes
            .iter()
            .all(|change| matches!(change, WatchChange::Remarks(_)));
    let progress = head_moved || (!changes.is_empty() && !comments_only);
    let wakes = if progress { 0 } else { watch.wakes } + u32::from(comments_only);
    WatchReport {
        changes,
        next: PullRequestWatch {
            started_at: watch.started_at.clone(),
            head_sha: read.head_sha.clone(),
            failed_checks,
            passed,
            passed_checks,
            remarks_through,
            remark_ids,
            conflicting,
            wakes,
            pending_wake: watch.pending_wake.clone(),
        },
        exhausted: comments_only && wakes >= WAKE_LIMIT,
    }
}

const HEADER_PREFIX: &str = "Update on pull request #";
const HEADER_SUFFIX: &str = "which Tcode is watching for you:";
const KEEP_WATCHING: &str = "Look into each item and act on it as your task requires. Tcode keeps watching and wakes you on the next change, so end your turn when you are done. When you hand the work back to the user, call unwatch_pull_request first.";
const EXHAUSTED_PREFIX: &str = "Tcode stopped watching after ";
const UNREADABLE_PREFIX: &str = "Tcode stopped watching pull request #";
const CLOSED_SUFFIX: &str =
    "was closed, so Tcode stopped watching it. Call watch_pull_request if it reopens.";
const MERGED_SUFFIX: &str = "was merged, so Tcode stopped watching it.";

fn snippet(body: &str) -> String {
    let mut text = String::new();
    let mut rest = body;
    // An unclosed `<!--` is no comment, so the rest of the body stays.
    while let Some((start, end)) = rest
        .find("<!--")
        .and_then(|start| Some((start, start + rest[start..].find("-->")? + 3)))
    {
        text.push_str(&rest[..start]);
        text.push(' ');
        rest = &rest[end..];
    }
    text.push_str(rest);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= SNIPPET_LENGTH {
        text
    } else {
        format!(
            "{}...",
            text.chars().take(SNIPPET_LENGTH - 3).collect::<String>()
        )
    }
}

fn listed<T>(items: &[T], line: impl Fn(&T) -> String) -> Vec<String> {
    let mut lines: Vec<_> = items.iter().take(LISTED_ITEMS).map(line).collect();
    if items.len() > LISTED_ITEMS {
        lines.push(format!("  - and {} more", items.len() - LISTED_ITEMS));
    }
    lines
}

/// The wake the agent reads.
pub fn update_message(number: u64, url: &str, base_branch: &str, report: &WatchReport) -> String {
    let commit = report
        .next
        .head_sha
        .as_deref()
        .map(|sha| format!(" on {}", sha.chars().take(7).collect::<String>()))
        .unwrap_or_default();
    let mut lines = vec![format!("{HEADER_PREFIX}{number} ({url}), {HEADER_SUFFIX}")];
    for change in &report.changes {
        match change {
            WatchChange::ChecksFailed(failed) => {
                lines.push(format!("- Checks failed{commit}:"));
                lines.extend(listed(failed, |check| {
                    let status = match check.status {
                        CheckStatus::Cancelled => " (cancelled)",
                        CheckStatus::ActionRequired => " (action-required)",
                        _ => "",
                    };
                    let url = check
                        .url
                        .as_ref()
                        .map(|url| format!(" {url}"))
                        .unwrap_or_default();
                    format!("  - {}{status}{url}", check.name)
                }));
            }
            WatchChange::ChecksPassed { count, required } => lines.push(format!(
                "- All {count} {}{} passed{commit}.",
                if *required { "required " } else { "" },
                if *count == 1 { "check" } else { "checks" }
            )),
            WatchChange::Remarks(remarks) => {
                lines.push(format!(
                    "- {} new {}:",
                    remarks.len(),
                    if remarks.len() == 1 {
                        "comment"
                    } else {
                        "comments"
                    }
                ));
                lines.extend(listed(remarks, |remark| {
                    let place = remark
                        .path
                        .as_ref()
                        .map(|path| format!(" on {path}"))
                        .unwrap_or_default();
                    let body = snippet(&remark.body);
                    let said = if body.is_empty() {
                        remark
                            .review_state
                            .clone()
                            .unwrap_or_else(|| "reviewed".into())
                    } else {
                        format!("\"{body}\"")
                    };
                    let url = remark
                        .url
                        .as_ref()
                        .map(|url| format!(" {url}"))
                        .unwrap_or_default();
                    format!(
                        "  - {}{place}: {said}{url}",
                        remark.author.as_deref().unwrap_or("someone")
                    )
                }));
            }
            WatchChange::Conflicting => {
                lines.push(format!("- The branch now conflicts with {base_branch}."))
            }
        }
    }
    lines.push(String::new());
    lines.push(if report.exhausted {
        format!(
            "{EXHAUSTED_PREFIX}{WAKE_LIMIT} comment-only updates in a row. Call watch_pull_request to watch it again."
        )
    } else {
        KEEP_WATCHING.into()
    });
    lines.join("\n")
}

pub fn closed_message(number: u64, url: &str) -> String {
    format!("Pull request #{number} ({url}) {CLOSED_SUFFIX}")
}

pub fn merged_message(number: u64, url: &str) -> String {
    format!("Pull request #{number} ({url}) {MERGED_SUFFIX}")
}

pub fn unreadable_message(number: u64, url: &str) -> String {
    format!(
        "{UNREADABLE_PREFIX}{number} ({url}) because it failed to read it from the host {READ_FAILURE_LIMIT} times in a row. Check it yourself, and call watch_pull_request to watch it again."
    )
}

/// What a watch message tells, for the user's notices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchNotice {
    /// `stopped` when this wake spent the comment-only limit.
    Update {
        kinds: Vec<WatchChangeKind>,
        stopped: bool,
    },
    Closed,
    Merged,
    Unreadable,
}

/// The pull request number and notice of a message a watch delivered.
pub fn parse_message(text: &str) -> Option<(u64, WatchNotice)> {
    let number = |rest: &str| -> Option<u64> {
        rest.split_once(' ')
            .and_then(|(number, _)| number.parse().ok())
    };
    if let Some(rest) = text.strip_prefix(HEADER_PREFIX) {
        let (header, body) = text.split_once('\n')?;
        if !header.ends_with(HEADER_SUFFIX) {
            return None;
        }
        let kinds = body
            .lines()
            .filter_map(|line| {
                let line = line.strip_prefix("- ")?;
                if line.starts_with("Checks failed") {
                    Some(WatchChangeKind::ChecksFailed)
                } else if line.starts_with("All ") {
                    Some(WatchChangeKind::ChecksPassed)
                } else if line.starts_with("The branch now conflicts") {
                    Some(WatchChangeKind::MergeConflict)
                } else {
                    line.contains(" new comment")
                        .then_some(WatchChangeKind::NewComments)
                }
            })
            .collect();
        let stopped = body
            .lines()
            .last()
            .is_some_and(|line| line.starts_with(EXHAUSTED_PREFIX));
        return Some((number(rest)?, WatchNotice::Update { kinds, stopped }));
    }
    if let Some(rest) = text.strip_prefix(UNREADABLE_PREFIX) {
        return Some((number(rest)?, WatchNotice::Unreadable));
    }
    let rest = text.strip_prefix("Pull request #")?;
    let notice = if text.ends_with(CLOSED_SUFFIX) {
        WatchNotice::Closed
    } else if text.ends_with(MERGED_SUFFIX) {
        WatchNotice::Merged
    } else {
        return None;
    };
    Some((number(rest)?, notice))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, status: CheckStatus, required: Option<bool>) -> PullRequestCheck {
        PullRequestCheck {
            name: name.into(),
            status,
            url: None,
            required,
        }
    }
    fn read(head: &str, checks: Vec<PullRequestCheck>) -> PullRequestWatchRead {
        PullRequestWatchRead {
            state: PullRequestState::Open,
            head_sha: Some(head.into()),
            base_branch: "main".into(),
            checks,
            mergeability: Mergeability::Clean,
            viewer: Some("tcode-bot".into()),
            author: Some("someone".into()),
        }
    }
    fn remark(id: &str, author: &str, at: &str, edited: Option<&str>) -> PullRequestRemark {
        PullRequestRemark {
            id: id.into(),
            author: Some(author.into()),
            body: format!("remark {id}"),
            created_at: at.into(),
            edited_at: edited.map(Into::into),
            url: None,
            path: None,
            review_state: None,
        }
    }
    fn watch() -> PullRequestWatch {
        let mut watch = PullRequestWatch::new(0);
        watch.started_at = "2026-10-08T10:00:00.000Z".into();
        watch.remarks_through = watch.started_at.clone();
        watch
    }
    fn kinds(report: &WatchReport) -> Vec<WatchChangeKind> {
        report.changes.iter().map(WatchChange::kind).collect()
    }

    #[test]
    fn required_gate_wakes_once_and_again_for_a_late_required_job() {
        use CheckStatus::*;
        let mut watch = watch();
        let first = evaluate(
            &watch,
            &read(
                "a",
                vec![
                    check("build", Success, Some(true)),
                    check("lint", Pending, Some(false)),
                ],
            ),
            None,
        );
        assert_eq!(
            first.changes,
            vec![WatchChange::ChecksPassed {
                count: 1,
                required: true
            }],
            "advisory work never holds the required gate"
        );
        watch = first.next;
        let quiet = evaluate(
            &watch,
            &read(
                "a",
                vec![
                    check("build", Success, Some(true)),
                    check("lint", Success, Some(false)),
                ],
            ),
            None,
        );
        assert!(quiet.changes.is_empty(), "an advisory job never re-wakes");
        watch = quiet.next;
        let late = evaluate(
            &watch,
            &read(
                "a",
                vec![
                    check("build", Success, Some(true)),
                    check("deploy", Success, Some(true)),
                ],
            ),
            None,
        );
        assert_eq!(
            late.changes,
            vec![WatchChange::ChecksPassed {
                count: 2,
                required: true
            }],
            "a required job that first appears already passed is news"
        );
    }

    #[test]
    fn failures_report_by_name_and_a_new_head_resets_them_without_waking() {
        use CheckStatus::*;
        let mut watch = watch();
        let failed = evaluate(
            &watch,
            &read(
                "a",
                vec![check("test", Failure, None), check("slow", Pending, None)],
            ),
            None,
        );
        assert_eq!(kinds(&failed), vec![WatchChangeKind::ChecksFailed]);
        watch = failed.next;
        assert!(
            evaluate(&watch, &read("a", vec![check("test", Failure, None)]), None)
                .changes
                .is_empty(),
            "a failure is told once"
        );
        assert!(
            evaluate(&watch, &read("a", Vec::new()), None)
                .next
                .failed_checks
                .contains(&"test".to_owned()),
            "an empty check read keeps what the agent was told"
        );
        let pushed = evaluate(&watch, &read("b", vec![check("test", Pending, None)]), None);
        assert!(pushed.changes.is_empty(), "a push alone is not news");
        assert!(pushed.next.failed_checks.is_empty());
        let rerun = evaluate(
            &pushed.next,
            &read("b", vec![check("test", Failure, None)]),
            None,
        );
        assert_eq!(
            kinds(&rerun),
            vec![WatchChangeKind::ChecksFailed],
            "the same check failing on the new head is reported again"
        );
    }

    #[test]
    fn remarks_by_others_wake_once_including_same_second_and_edits() {
        let watch = watch();
        let remarks = [
            remark("old", "reviewer", "2026-10-08T09:00:00Z", None),
            remark("mine", "TCODE-BOT", "2026-10-08T10:05:00Z", None),
            remark("a", "reviewer", "2026-10-08T10:05:00Z", None),
        ];
        let first = evaluate(&watch, &read("a", Vec::new()), Some(&remarks));
        let WatchChange::Remarks(fresh) = &first.changes[0] else {
            panic!("expected remarks: {:?}", first.changes);
        };
        assert_eq!(
            fresh.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["a"],
            "the viewer's own remark never wakes, even when the PR author is someone else"
        );
        let late = [
            remarks[2].clone(),
            remark("b", "reviewer", "2026-10-08T10:05:00Z", None),
        ];
        let second = evaluate(&first.next, &read("a", Vec::new()), Some(&late));
        let WatchChange::Remarks(fresh) = &second.changes[0] else {
            panic!("expected the same-second arrival");
        };
        assert_eq!(fresh[0].id, "b");
        assert_eq!(second.next.remark_ids, vec!["a", "b"]);
        let edited = [remark(
            "old",
            "summary-bot",
            "2026-10-08T09:00:00Z",
            Some("2026-10-08T10:07:00Z"),
        )];
        let third = evaluate(&second.next, &read("a", Vec::new()), Some(&edited));
        assert_eq!(kinds(&third), vec![WatchChangeKind::NewComments]);
        assert!(
            evaluate(&third.next, &read("a", Vec::new()), Some(&edited))
                .changes
                .is_empty(),
            "an edit wakes once"
        );
        assert_eq!(
            evaluate(&third.next, &read("a", Vec::new()), None).next,
            third.next,
            "an incomplete read never moves the watermark"
        );
    }

    #[test]
    fn conflict_wakes_on_entry_and_unknown_keeps_it() {
        let mut conflicting = read("a", Vec::new());
        conflicting.mergeability = Mergeability::Conflicting;
        let first = evaluate(&watch(), &conflicting, None);
        assert_eq!(kinds(&first), vec![WatchChangeKind::MergeConflict]);
        let mut unknown = conflicting.clone();
        unknown.mergeability = Mergeability::Unknown;
        let computing = evaluate(&first.next, &unknown, None);
        assert!(computing.changes.is_empty() && computing.next.conflicting);
        assert!(
            evaluate(&computing.next, &conflicting, None)
                .changes
                .is_empty()
        );
        let clean = evaluate(&computing.next, &read("a", Vec::new()), None);
        assert_eq!(
            kinds(&evaluate(&clean.next, &conflicting, None)),
            vec![WatchChangeKind::MergeConflict],
            "a clean answer lets the next conflict wake"
        );
    }

    #[test]
    fn comment_loop_ends_at_the_limit_and_check_news_or_a_push_resets_it() {
        let mut watch = watch();
        let at = |minute: u32| format!("2026-10-08T10:{minute:02}:00Z");
        let comment = |minute| [remark(&format!("c{minute}"), "bot", &at(minute), None)];
        for minute in 1..WAKE_LIMIT {
            let report = evaluate(&watch, &read("a", Vec::new()), Some(&comment(minute)));
            assert!(!report.exhausted);
            watch = report.next;
        }
        assert_eq!(watch.wakes, WAKE_LIMIT - 1);
        let checks = evaluate(
            &watch,
            &read("a", vec![check("ci", CheckStatus::Success, None)]),
            Some(&comment(30)),
        );
        assert_eq!(checks.next.wakes, 0, "check news resets the count");
        let mut pushed = watch.clone();
        pushed.head_sha = Some("old".into());
        assert_eq!(
            evaluate(&pushed, &read("a", Vec::new()), Some(&comment(31)))
                .next
                .wakes,
            1,
            "a push resets the count before this wake"
        );
        let last = evaluate(&watch, &read("a", Vec::new()), Some(&comment(32)));
        assert!(last.exhausted);
        let text = update_message(7, "https://github.com/sample/project/pull/7", "main", &last);
        assert_eq!(
            parse_message(&text),
            Some((
                7,
                WatchNotice::Update {
                    kinds: vec![WatchChangeKind::NewComments],
                    stopped: true
                }
            ))
        );
    }

    #[test]
    fn messages_list_ten_items_with_snippets_and_parse_back() {
        let failed: Vec<_> = (0..12)
            .map(|index| PullRequestCheck {
                name: format!("job {index}"),
                status: if index == 0 {
                    CheckStatus::Cancelled
                } else {
                    CheckStatus::Failure
                },
                url: Some(format!("https://ci.test/{index}")),
                required: None,
            })
            .collect();
        let mut long = remark("r", "reviewer", "2026-10-08T10:01:00Z", None);
        long.body = format!("<!-- hidden -->  {}\n\nend", "word ".repeat(60));
        long.path = Some("src/lib.rs".into());
        let mut review = remark("v", "lead", "2026-10-08T10:02:00Z", None);
        review.body.clear();
        review.review_state = Some("APPROVED".into());
        let mut unclosed = remark("u", "author", "2026-10-08T10:03:00Z", None);
        unclosed.body = "Needs a test <!-- unfinished".into();
        let report = WatchReport {
            changes: vec![
                WatchChange::ChecksFailed(failed),
                WatchChange::Remarks(vec![long, review, unclosed]),
                WatchChange::Conflicting,
            ],
            next: PullRequestWatch {
                head_sha: Some("0123456789".into()),
                ..watch()
            },
            exhausted: false,
        };
        let text = update_message(
            5,
            "https://github.com/sample/project/pull/5",
            "main",
            &report,
        );
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Update on pull request #5 (https://github.com/sample/project/pull/5), which Tcode is watching for you:"
        );
        assert_eq!(lines[1], "- Checks failed on 0123456:");
        assert_eq!(lines[2], "  - job 0 (cancelled) https://ci.test/0");
        assert_eq!(lines[12], "  - and 2 more");
        let snippet_line = lines[14];
        assert!(snippet_line.starts_with("  - reviewer on src/lib.rs: \"word word"));
        assert!(snippet_line.ends_with("...\""));
        assert_eq!(
            snippet_line.len(),
            "  - reviewer on src/lib.rs: \"\"".len() + 200
        );
        assert_eq!(lines[15], "  - lead: APPROVED");
        assert_eq!(
            lines[16], "  - author: \"Needs a test <!-- unfinished\"",
            "an unclosed comment marker hides nothing"
        );
        assert_eq!(lines[17], "- The branch now conflicts with main.");
        assert_eq!(lines.last(), Some(&KEEP_WATCHING));
        assert!(!text.contains("inbox"));
        assert_eq!(
            parse_message(&text),
            Some((
                5,
                WatchNotice::Update {
                    kinds: vec![
                        WatchChangeKind::ChecksFailed,
                        WatchChangeKind::NewComments,
                        WatchChangeKind::MergeConflict
                    ],
                    stopped: false
                }
            ))
        );
        let url = "https://github.com/sample/project/pull/5";
        assert_eq!(
            parse_message(&closed_message(5, url)),
            Some((5, WatchNotice::Closed))
        );
        assert_eq!(
            parse_message(&merged_message(5, url)),
            Some((5, WatchNotice::Merged))
        );
        assert_eq!(
            parse_message(&unreadable_message(5, url)),
            Some((5, WatchNotice::Unreadable))
        );
        assert_eq!(parse_message("Update on pull request #5 please"), None);
    }
}
