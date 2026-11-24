//! A pull request's changed files as Azure DevOps lists them for an iteration, and each file's
//! hunks, built from its text at both commits: Azure serves no patch of its own.

use serde_json::Value;
use tcode_protocol::{PullRequestFile, PullRequestFileText, PullRequestPatch};

/// One changed file of an iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Change {
    pub(super) path: String,
    pub(super) previous_path: Option<String>,
    pub(super) kind: agent::FileChangeKind,
    /// The file's blob at the head; `None` for a deleted file.
    pub(super) object: Option<String>,
    /// Azure's id for the file across the pull request's pushes, which a thread on its lines
    /// names.
    pub(super) tracking: Option<u64>,
}

/// Azure writes paths from the repository root with a leading slash.
fn relative(path: &str) -> String {
    path.trim_start_matches('/').to_owned()
}

/// An iteration's change entry, `None` for a folder.
pub(super) fn change(entry: &Value) -> Option<Change> {
    let item = &entry["item"];
    if item["isFolder"].as_bool() == Some(true) || item["gitObjectType"].as_str() == Some("tree") {
        return None;
    }
    let path = relative(item["path"].as_str()?);
    let change_type = entry["changeType"].as_str().unwrap_or_default();
    let has = |kind: &str| change_type.split(',').any(|part| part.trim() == kind);
    let kind = if has("add") {
        agent::FileChangeKind::Create
    } else if has("delete") {
        agent::FileChangeKind::Delete
    } else if has("rename") {
        agent::FileChangeKind::Rename
    } else {
        agent::FileChangeKind::Modify
    };
    let previous_path = entry["originalPath"]
        .as_str()
        .or_else(|| entry["sourceServerItem"].as_str())
        .map(relative)
        .filter(|previous| *previous != path && kind == agent::FileChangeKind::Rename);
    Some(Change {
        tracking: entry["changeTrackingId"].as_u64(),
        object: (kind != agent::FileChangeKind::Delete)
            .then(|| item["objectId"].as_str().map(str::to_ascii_lowercase))
            .flatten(),
        path,
        previous_path,
        kind,
    })
}

/// The file with its hunks between `old` and `new`, its text at the two commits; `None` on a
/// side the change does not have.
pub(super) fn file(
    change: &Change,
    old: Option<PullRequestFileText>,
    new: Option<PullRequestFileText>,
) -> PullRequestFile {
    let side = |text: Option<PullRequestFileText>| match text {
        None | Some(PullRequestFileText::Missing) => Ok(String::new()),
        Some(PullRequestFileText::Text(text)) => Ok(text),
        Some(other) => Err(other),
    };
    let (patch, additions, deletions) = match (side(old), side(new)) {
        (Err(PullRequestFileText::Oversized), _) | (_, Err(PullRequestFileText::Oversized)) => {
            (PullRequestPatch::Oversized, 0, 0)
        }
        (Err(_), _) | (_, Err(_)) => (PullRequestPatch::Binary, 0, 0),
        (Ok(old), Ok(new)) => hunks(&old, &new),
    };
    PullRequestFile {
        path: change.path.clone(),
        previous_path: change.previous_path.clone(),
        kind: change.kind,
        additions,
        deletions,
        patch,
    }
}

/// Lines of context around each change, as git writes them.
const CONTEXT: u32 = 3;

/// Unified hunks with three lines of context, as git writes them, and the lines added and
/// removed. imara-diff 0.2.0's own unified printer is not used: its first hunk's header starts
/// one line after the context it prints when the change is within six lines of the top.
fn hunks(old: &str, new: &str) -> (PullRequestPatch, u64, u64) {
    use imara_diff::{Algorithm, Diff, InternedInput};
    let input = InternedInput::new(
        imara_diff::sources::lines(old),
        imara_diff::sources::lines(new),
    );
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let changes: Vec<_> = diff.hunks().collect();
    let (mut additions, mut deletions) = (0, 0);
    for change in &changes {
        deletions += u64::from(change.before.end - change.before.start);
        additions += u64::from(change.after.end - change.after.start);
    }
    // Changes whose context would touch or overlap go in one hunk.
    let mut groups: Vec<Vec<&imara_diff::Hunk>> = Vec::new();
    for change in &changes {
        match groups.last_mut() {
            Some(group)
                if change.before.start - group.last().unwrap().before.end <= 2 * CONTEXT =>
            {
                group.push(change)
            }
            _ => groups.push(vec![change]),
        }
    }
    let line = |text: &mut String, sign: char, token: imara_diff::Token| {
        let content = input.interner[token];
        text.push(sign);
        text.push_str(content);
        if !content.ends_with('\n') {
            text.push_str("\n\\ No newline at end of file\n");
        }
    };
    // Git points an empty range at the line before it and writes a one-line range as its
    // number alone.
    let range = |start: u32, length: u32| match length {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{length}", start + 1),
    };
    let mut text = String::new();
    for group in groups {
        let (first, last) = (group[0], group[group.len() - 1]);
        let old_start = first.before.start.saturating_sub(CONTEXT);
        let old_end = (last.before.end + CONTEXT).min(input.before.len() as u32);
        let new_start = first.after.start - (first.before.start - old_start);
        let new_end = last.after.end + (old_end - last.before.end);
        text.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(old_start, old_end - old_start),
            range(new_start, new_end - new_start)
        ));
        let mut at = old_start;
        for change in group {
            for index in at..change.before.start {
                line(&mut text, ' ', input.before[index as usize]);
            }
            for index in change.before.clone() {
                line(&mut text, '-', input.before[index as usize]);
            }
            for index in change.after.clone() {
                line(&mut text, '+', input.after[index as usize]);
            }
            at = change.before.end;
        }
        for index in at..old_end {
            line(&mut text, ' ', input.before[index as usize]);
        }
    }
    (PullRequestPatch::Hunks(text), additions, deletions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An iteration's entries read as files: a rename keeps the path it came from, a deleted
    /// file has no blob at the head, and a folder is no file. Each file's hunks are the change
    /// between its two texts with three lines of context; a binary or oversized side shows none.
    #[test]
    fn changes_read_as_files_with_hunks_between_their_texts() {
        let renamed = change(&json!({
            "changeType": "rename, edit",
            "changeTrackingId": 4,
            "originalPath": "/src/old.rs",
            "item": {"path": "/src/new.rs", "objectId": "AB12"},
        }))
        .unwrap();
        assert_eq!(renamed.path, "src/new.rs");
        assert_eq!(renamed.previous_path.as_deref(), Some("src/old.rs"));
        assert_eq!(renamed.kind, agent::FileChangeKind::Rename);
        assert_eq!(renamed.object.as_deref(), Some("ab12"));
        assert_eq!(renamed.tracking, Some(4));
        let deleted = change(&json!({
            "changeType": "delete",
            "item": {"path": "/gone.txt", "objectId": "CD34"},
        }))
        .unwrap();
        assert_eq!(
            (deleted.kind, deleted.object),
            (agent::FileChangeKind::Delete, None)
        );
        assert!(
            change(&json!({"changeType": "add", "item": {"path": "/dir", "isFolder": true}}))
                .is_none()
        );

        let old: String = (1..=10).map(|line| format!("line {line}\n")).collect();
        let new = old.replace("line 5\n", "line five\n");
        let text = |text: &str| Some(PullRequestFileText::Text(text.to_owned()));
        let read = file(&renamed, text(&old), text(&new));
        assert_eq!(
            read.patch,
            PullRequestPatch::Hunks(
                "@@ -2,7 +2,7 @@\n line 2\n line 3\n line 4\n-line 5\n+line five\n line 6\n line 7\n line 8\n"
                    .into()
            )
        );
        assert_eq!((read.additions, read.deletions), (1, 1));
        let created = file(&renamed, None, text("a\nb"));
        assert_eq!(
            created.patch,
            PullRequestPatch::Hunks(
                "@@ -0,0 +1,2 @@\n+a\n+b\n\\ No newline at end of file\n".into()
            )
        );
        assert_eq!((created.additions, created.deletions), (2, 0));
        assert_eq!(
            file(&renamed, text(&old), Some(PullRequestFileText::Binary)).patch,
            PullRequestPatch::Binary
        );
        assert_eq!(
            file(&renamed, Some(PullRequestFileText::Oversized), text(&new)).patch,
            PullRequestPatch::Oversized
        );
    }
}
