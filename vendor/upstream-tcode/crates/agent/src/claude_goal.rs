//! A Claude Code session's `/goal`, read from the `goal_status` records its
//! transcript keeps. stream-json drops the CLI's own `active_goal` event; the
//! transcript is also what the CLI restores a goal from on `--resume`.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{Goal, GoalStatus};

pub(crate) struct GoalTranscript {
    claude_dir: PathBuf,
    session_id: String,
    path: Option<PathBuf>,
    offset: u64,
    /// The next read starts with history this process did not write: a met
    /// or failed goal there is simply gone, as it is for the CLI.
    history: bool,
    goal: Option<Goal>,
}

impl GoalTranscript {
    pub(crate) fn new(claude_dir: PathBuf, session_id: String, resumed: bool) -> Self {
        Self {
            claude_dir,
            session_id,
            path: None,
            offset: 0,
            history: resumed,
            goal: None,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn goal(&self) -> Option<&Goal> {
        self.goal.as_ref()
    }

    /// Forget a goal the CLI has already let go of (met or found impossible).
    pub(crate) fn dismiss(&mut self) {
        self.goal = None;
    }

    /// Fold the records appended since the last read. Returns the goal when
    /// it changed.
    pub(crate) fn read_appended(&mut self) -> Option<Option<Goal>> {
        if self.path.is_none() {
            self.path = find_session_transcript(&self.claude_dir, &self.session_id);
        }
        let path = self.path.as_ref()?;
        let before = self.goal.clone();
        let live = !self.history;
        match read_lines_from(path, &mut self.offset, |line| {
            // Most records are large and never goals; skip parsing them.
            if !line.contains("\"goal_status\"") {
                return;
            }
            if let Ok(record) = serde_json::from_str::<Value>(line) {
                fold(&mut self.goal, &record, live);
            }
        }) {
            Ok(()) => self.history = false,
            Err(err) => log::debug!("claude goal transcript read failed: {err}"),
        }
        (self.goal != before).then(|| self.goal.clone())
    }
}

fn find_session_transcript(claude_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let name = format!("{session_id}.jsonl");
    std::fs::read_dir(claude_dir.join("projects"))
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(&name))
        .find(|path| path.is_file())
}

fn read_lines_from(
    path: &Path,
    offset: &mut u64,
    mut each: impl FnMut(&str),
) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() < *offset {
        *offset = 0;
    }
    file.seek(SeekFrom::Start(*offset))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        // A line still being written is read again once it is complete.
        if bytes == 0 || !line.ends_with('\n') {
            return Ok(());
        }
        *offset += bytes as u64;
        each(&line);
    }
}

/// One transcript record. Setting a goal writes a `sentinel` that is not
/// `met`, clearing it one that is; each check writes whether it was `met`
/// (or `failed`, judged impossible) with its reason. A met or failed goal is
/// cleared by the CLI, so only a live one stays to be shown.
fn fold(goal: &mut Option<Goal>, record: &Value, live: bool) {
    if record.get("type").and_then(Value::as_str) != Some("attachment") {
        return;
    }
    let Some(status) = record
        .get("attachment")
        .filter(|attachment| attachment.get("type").and_then(Value::as_str) == Some("goal_status"))
    else {
        return;
    };
    let flag = |field: &str| status.get(field).and_then(Value::as_bool) == Some(true);
    let Some(objective) = status.get("condition").and_then(Value::as_str) else {
        return;
    };
    let reason = status
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if flag("sentinel") {
        *goal = (!flag("met")).then(|| Goal {
            objective: objective.to_owned(),
            status: GoalStatus::Active,
            tokens_used: None,
            token_budget: None,
            time_used_secs: None,
            checks: Some(0),
            last_check: None,
        });
        return;
    }
    if flag("met") || flag("failed") {
        *goal = live.then(|| Goal {
            objective: objective.to_owned(),
            status: if flag("failed") {
                GoalStatus::Blocked
            } else {
                GoalStatus::Complete
            },
            tokens_used: status.get("tokens").and_then(Value::as_u64),
            token_budget: None,
            time_used_secs: status
                .get("durationMs")
                .and_then(Value::as_u64)
                .map(|ms| ms / 1000),
            checks: status
                .get("iterations")
                .and_then(Value::as_u64)
                .and_then(|count| u32::try_from(count).ok()),
            last_check: reason,
        });
        return;
    }
    let checks = goal
        .as_ref()
        .filter(|goal| goal.objective == objective)
        .and_then(|goal| goal.checks)
        .unwrap_or(0)
        + 1;
    *goal = Some(Goal {
        objective: objective.to_owned(),
        status: GoalStatus::Active,
        tokens_used: None,
        token_budget: None,
        time_used_secs: None,
        checks: Some(checks),
        last_check: reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = include_str!("../tests/fixtures/claude/goal_transcript.jsonl");

    fn claude_dir() -> PathBuf {
        std::env::temp_dir().join(format!("tcode-claude-goal-{}", uuid::Uuid::new_v4()))
    }

    fn transcript(dir: &Path, session_id: &str, contents: &str) {
        let project = dir.join("projects").join("-private-tmp-goal");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{session_id}.jsonl")), contents).unwrap();
    }

    /// Recorded from Claude Code 2.1.309: `/goal`, two unmet checks, then a
    /// met one, each appended while the session runs.
    #[test]
    fn live_records_follow_the_goal_to_completion() {
        let dir = claude_dir();
        let mut written = String::new();
        let mut reader = GoalTranscript::new(dir.clone(), "s".into(), false);
        assert_eq!(reader.read_appended(), None);
        let mut seen = Vec::new();
        for line in TRANSCRIPT.lines() {
            written.push_str(line);
            written.push('\n');
            transcript(&dir, "s", &written);
            if let Some(goal) = reader.read_appended() {
                seen.push(goal);
            }
        }
        let summary: Vec<_> = seen
            .iter()
            .map(|goal| {
                goal.as_ref()
                    .map(|goal| (goal.status, goal.checks, goal.last_check.is_some()))
            })
            .collect();
        assert_eq!(
            summary,
            [
                Some((GoalStatus::Active, Some(0), false)),
                Some((GoalStatus::Active, Some(1), true)),
                Some((GoalStatus::Active, Some(2), true)),
                Some((GoalStatus::Complete, Some(3), true)),
            ]
        );
        let done = seen.last().unwrap().as_ref().unwrap();
        assert_eq!(done.objective, "n.txt contains the number 3");
        assert_eq!(done.tokens_used, Some(862));
        assert_eq!(done.time_used_secs, Some(14));
    }

    /// The CLI drops a met goal when it resumes; so does the reader.
    #[test]
    fn resumed_history_keeps_only_a_goal_still_open() {
        let dir = claude_dir();
        transcript(&dir, "done", TRANSCRIPT);
        let mut reader = GoalTranscript::new(dir.clone(), "done".into(), true);
        assert_eq!(reader.read_appended(), None);

        let open: String = TRANSCRIPT
            .lines()
            .take(4)
            .flat_map(|line| [line, "\n"])
            .collect();
        transcript(&dir, "open", &open);
        let mut reader = GoalTranscript::new(dir.clone(), "open".into(), true);
        let goal = reader.read_appended().flatten().unwrap();
        assert_eq!((goal.status, goal.checks), (GoalStatus::Active, Some(2)));
    }
}
