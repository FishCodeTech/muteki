//! Where pending review comments sit in a pull request's diff, for hosts that read the whole
//! diff and file text at a revision, with no anchoring of their own.

use super::{Anchoring, ForgeError, Moved};
use tcode_core::{pull_request::PullRequestReviewDraftComment, session::ReviewSide};
use tcode_protocol::{PullRequestFileText, PullRequestFiles};

/// Whether the host would take a comment on those lines, by the diff `files` reads now.
pub(crate) fn anchoring(
    files: &PullRequestFiles,
    head: &str,
    path: &str,
    side: ReviewSide,
    lines: (u32, u32),
) -> Anchoring {
    if files.head != head {
        return Anchoring::Moved;
    }
    if crate::github::pull_request_actions::in_hunks(&files.files, path, side, lines) {
        Anchoring::InDiff
    } else {
        Anchoring::OutsideDiff
    }
}

/// The head `files` reads at, and the revision each pending comment's lines now read at: kept
/// where they are still in the diff and read the same as where they were written.
pub(crate) fn reanchor(
    files: PullRequestFiles,
    comments: &[PullRequestReviewDraftComment],
    text: impl Fn(&str, &str) -> Result<PullRequestFileText, ForgeError>,
) -> Result<(String, Vec<Moved>), ForgeError> {
    let lines = |revision: &str, comment: &PullRequestReviewDraftComment| {
        Ok::<_, ForgeError>(match text(revision, &comment.path)? {
            PullRequestFileText::Text(text) => {
                let lines: Vec<_> = text.lines().collect();
                lines
                    .get(comment.start_line as usize - 1..comment.end_line as usize)
                    .map(|lines| lines.join("\n"))
            }
            _ => None,
        })
    };
    let mut moved = Vec::new();
    for comment in comments {
        let revision = match comment.side {
            ReviewSide::Old => &files.base,
            ReviewSide::New => &files.head,
        };
        let kept = comment.placed
            && crate::github::pull_request_actions::in_hunks(
                &files.files,
                &comment.path,
                comment.side,
                (comment.start_line, comment.end_line),
            )
            && (comment.revision == *revision || {
                let before = lines(&comment.revision, comment)?;
                before.is_some() && before == lines(revision, comment)?
            });
        moved.push((comment.id, kept.then(|| revision.clone())));
    }
    Ok((files.head, moved))
}
