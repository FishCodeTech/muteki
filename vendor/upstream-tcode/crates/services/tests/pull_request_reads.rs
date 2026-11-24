use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    thread,
};
use tcode_core::{
    pull_request::{PullRequestKey, PullRequestMergeMethod, PullRequestReviewDraftComment},
    session::ReviewSide,
};
use tcode_protocol::{
    PullRequestAction, PullRequestActionResult, PullRequestFileText, PullRequestMedia,
    PullRequestPatch, PullRequestPermissions, PullRequestReactionContent, PullRequestRejection,
    PullRequestReviewAnchor, PullRequestReviewState, PullRequestReviewVerdict, PullRequestReviewer,
    PullRequestReviewerKind, PullRequestViewedState,
};
use tcode_services::github::{GitHubApi, GitHubError, pull_request_reads::PullRequestReads};

#[path = "support/github.rs"]
#[allow(dead_code)] // tests/github.rs drives the fixture by hand as well.
mod fixture;
use fixture::{Exchange, Fixture, Server, Store};

const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";

/// One request as GitHub would see it.
#[derive(Debug, Clone)]
struct Seen {
    line: String,
    host: String,
    accept: String,
    authorization: Option<String>,
    request: String,
    body: String,
}
impl Seen {
    fn from(exchange: &Exchange) -> Self {
        let mut seen = Self {
            line: exchange.request.lines().next().unwrap().to_owned(),
            host: String::new(),
            accept: String::new(),
            authorization: None,
            request: exchange.request.clone(),
            body: String::from_utf8_lossy(&exchange.body).into_owned(),
        };
        seen.host = seen.header("host").unwrap_or_default();
        seen.accept = seen.header("accept").unwrap_or_default();
        seen.authorization = seen.header("authorization");
        seen
    }
    fn header(&self, name: &str) -> Option<String> {
        self.request.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
    fn operation(&self) -> Option<String> {
        let query: Value = serde_json::from_str(&self.body).ok()?;
        let text = query["query"].as_str()?;
        Some(
            text.split_whitespace()
                .nth(1)?
                .split('(')
                .next()?
                .to_owned(),
        )
    }
    fn variables(&self) -> Value {
        serde_json::from_str::<Value>(&self.body).unwrap()["variables"].clone()
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// Answers each request with `answer(seen)` and keeps what it saw. A status of 0 closes the
/// connection unanswered, as a request lost after it reached GitHub would leave it.
fn serve(
    fixture: Fixture,
    answer: impl Fn(&Seen) -> (u16, String, Vec<u8>) + Send + 'static,
) -> (Server, Log) {
    let log: Log = Arc::default();
    let seen = log.clone();
    let server = fixture.serve(move |exchange| {
        let request = Seen::from(&exchange);
        let (status, headers, body) = answer(&request);
        seen.lock().unwrap().push(request);
        if status != 0 {
            exchange.reply(status, &headers, &body);
        }
    });
    (server, log)
}

fn json_reply(value: Value) -> (u16, String, Vec<u8>) {
    (200, String::new(), value.to_string().into_bytes())
}

fn key() -> PullRequestKey {
    PullRequestKey::new("github.com", "octo/repo", 7)
}

fn reads(fixture: &Fixture, store: &Store) -> Arc<PullRequestReads> {
    PullRequestReads::new(GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture-token")]),
        fixture.builder(),
    ))
}

fn revisions(changed_files: u64) -> (u16, String, Vec<u8>) {
    json_reply(json!({
        "number": 7,
        "base": {"sha": BASE},
        "head": {"sha": HEAD},
        "changed_files": changed_files,
        "merged_at": null,
        "node_id": "PR_node_7",
    }))
}

const WHOLE_DIFF: &str = "diff --git a/src/old name.rs b/src/new name.rs
similarity index 90%
rename from src/old name.rs
rename to src/new name.rs
index 3333333..4444444 100644
--- a/src/old name.rs
+++ b/src/new name.rs
@@ -1,3 +1,3 @@
 fn main() {
-    old();
+    new();
 }
diff --git a/docs/gone.md b/docs/gone.md
deleted file mode 100644
index 5555555..0000000
--- a/docs/gone.md
+++ /dev/null
@@ -1,2 +0,0 @@
-# Gone
--- a removed line that looks like a header
diff --git a/assets/logo.png b/assets/logo.png
index 6666666..7777777 100644
Binary files a/assets/logo.png and b/assets/logo.png differ
";

#[test]
fn whole_diff_keeps_renames_deletions_and_binaries_and_reads_text_at_a_revision() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, |seen| {
        match (
            seen.line.split_whitespace().nth(1).unwrap(),
            seen.accept.as_str(),
        ) {
            ("/repos/octo/repo/pulls/7", "application/vnd.github.diff") => {
                (200, String::new(), WHOLE_DIFF.as_bytes().to_vec())
            }
            ("/repos/octo/repo/pulls/7", _) => revisions(3),
            (path, "application/vnd.github.raw") => match path {
                p if p == format!("/repos/octo/repo/contents/src/new%20name.rs?ref={HEAD}") => {
                    (200, String::new(), b"fn main() {\n    new();\n}\n".to_vec())
                }
                p if p.starts_with("/repos/octo/repo/contents/assets/logo.png") => {
                    (200, String::new(), b"\x89PNG\r\n\x1a\n\0\0".to_vec())
                }
                p if p.starts_with("/repos/octo/repo/contents/big.txt") => {
                    (200, String::new(), vec![b'a'; 1024 * 1024 + 1])
                }
                _ => (404, String::new(), b"{\"message\":\"Not Found\"}".to_vec()),
            },
            _ => (500, String::new(), Vec::new()),
        }
    });

    let files = reads.files(&key(), None).unwrap().value;
    assert_eq!((files.base.as_str(), files.head.as_str()), (BASE, HEAD));
    assert!(files.complete && files.next_page.is_none());
    let summary: Vec<_> = files
        .files
        .iter()
        .map(|file| {
            (
                file.path.as_str(),
                file.previous_path.as_deref(),
                file.kind,
                file.additions,
                file.deletions,
            )
        })
        .collect();
    use agent::FileChangeKind::*;
    assert_eq!(
        summary,
        vec![
            ("src/new name.rs", Some("src/old name.rs"), Rename, 1, 1),
            ("docs/gone.md", None, Delete, 0, 2),
            ("assets/logo.png", None, Modify, 0, 0),
        ]
    );
    let PullRequestPatch::Hunks(hunks) = &files.files[0].patch else {
        panic!("a renamed file keeps its hunks")
    };
    assert!(hunks.starts_with("@@ -1,3 +1,3 @@\n") && hunks.ends_with(" }\n"));
    assert_eq!(files.files[2].patch, PullRequestPatch::Binary);

    assert_eq!(
        *reads
            .file_text(&key(), HEAD, "src/new name.rs")
            .unwrap()
            .value,
        PullRequestFileText::Text("fn main() {\n    new();\n}\n".into())
    );
    assert_eq!(
        *reads
            .file_text(&key(), HEAD, "assets/logo.png")
            .unwrap()
            .value,
        PullRequestFileText::Binary
    );
    assert_eq!(
        *reads.file_text(&key(), HEAD, "big.txt").unwrap().value,
        PullRequestFileText::Oversized
    );
    assert_eq!(
        *reads
            .file_text(&key(), BASE, "src/new name.rs")
            .unwrap()
            .value,
        PullRequestFileText::Missing,
        "a side the revision does not have is missing, not a failure"
    );
    let before = log.lock().unwrap().len();
    for (revision, path) in [
        ("main", "src/new name.rs"),
        (HEAD, "../../../user"),
        (HEAD, "src//x"),
        (HEAD, ""),
    ] {
        assert_eq!(
            reads.file_text(&key(), revision, path).unwrap_err(),
            GitHubError::InvalidInput
        );
    }
    assert_eq!(
        log.lock().unwrap().len(),
        before,
        "a branch name or a path leaving the repository never reaches GitHub"
    );
    let log = log.lock().unwrap();
    assert!(
        log.iter()
            .all(|seen| seen.authorization.as_deref() == Some("Bearer fixture-token"))
    );
}

fn listed(name: &str, status: &str, changes: (u64, u64), patch: Option<&str>) -> Value {
    let mut row = json!({
        "filename": name,
        "status": status,
        "additions": changes.0,
        "deletions": changes.1,
        "sha": "8888888888888888888888888888888888888888",
    });
    if status == "renamed" {
        row["previous_filename"] = json!(format!("old/{name}"));
    }
    if let Some(patch) = patch {
        row["patch"] = json!(patch);
    }
    row
}

#[test]
fn a_refused_whole_diff_pages_the_changed_files_and_names_withheld_patches() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let pages = Arc::new(Mutex::new(true));
    let pages_answer = pages.clone();
    // GitHub's count beside the revisions, one short of the files a later push left.
    let changed = Arc::new(Mutex::new(100));
    let changed_answer = changed.clone();
    let (_server, log) = serve(fixture, move |seen| {
        let path = seen.line.split_whitespace().nth(1).unwrap();
        if seen.accept == "application/vnd.github.diff" {
            return (
                406,
                String::new(),
                br#"{"message":"Sorry, the diff exceeded the maximum number of files (300)."}"#
                    .to_vec(),
            );
        }
        if !*pages_answer.lock().unwrap() && path.contains("/files?") {
            return (500, String::new(), b"{}".to_vec());
        }
        match path {
            "/repos/octo/repo/pulls/7" => revisions(*changed_answer.lock().unwrap()),
            "/repos/octo/repo/pulls/7/files?per_page=100&page=1" => {
                let mut rows = vec![
                    listed("huge.json", "modified", (40_000, 2), None),
                    listed("image.png", "added", (0, 0), None),
                    listed("moved.rs", "renamed", (0, 0), None),
                ];
                rows.extend((3..100).map(|index| {
                    listed(
                        &format!("src/{index}.rs"),
                        "modified",
                        (1, 0),
                        Some("@@ -1 +1,2 @@\n a\n+b"),
                    )
                }));
                json_reply(Value::Array(rows))
            }
            "/repos/octo/repo/pulls/7/files?per_page=100&page=2" => json_reply(json!([listed(
                "last.rs",
                "removed",
                (0, 3),
                Some("@@ -1,3 +0,0 @@\n-a\n-b\n-c")
            )])),
            _ => (500, String::new(), Vec::new()),
        }
    });

    let first = reads.files(&key(), None).unwrap().value;
    assert_eq!(first.files.len(), 100);
    assert_eq!(
        (first.next_page, first.complete),
        (Some(2), false),
        "a full page names the next one"
    );
    assert_eq!(first.files[0].patch, PullRequestPatch::Oversized);
    assert_eq!(
        first.files[1].patch,
        PullRequestPatch::Withheld,
        "the files listing cannot tell a binary from a file past GitHub's limits"
    );
    assert_eq!(
        (
            &first.files[2].patch,
            first.files[2].previous_path.as_deref()
        ),
        (
            &PullRequestPatch::Hunks(String::new()),
            Some("old/moved.rs")
        )
    );
    let second = reads.files(&key(), Some(2)).unwrap().value;
    assert_eq!(second.files[0].kind, agent::FileChangeKind::Delete);
    assert_eq!((second.next_page, second.complete), (None, true));
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.accept == "application/vnd.github.diff")
            .count(),
        1,
        "a page carries on from the files walk without asking for the whole diff again"
    );
    let revision_reads = || {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| {
                seen.line.starts_with("GET /repos/octo/repo/pulls/7 ")
                    && seen.accept != "application/vnd.github.diff"
            })
            .count()
    };
    assert_eq!(revision_reads(), 1);
    *changed.lock().unwrap() = 101;
    reads.files(&key(), None).unwrap();
    assert_eq!(
        revision_reads(),
        2,
        "files past the count read beside the revisions read the revisions again"
    );
    reads.files(&key(), None).unwrap();
    assert_eq!(
        revision_reads(),
        2,
        "revisions that agree with the files are shared"
    );

    // When the pages fail too, the refusal that explains the missing diff is reported.
    reads.invalidate(&key());
    *pages.lock().unwrap() = false;
    assert!(matches!(
        reads.files(&key(), None).unwrap_err(),
        GitHubError::Response { status: 406, .. }
    ));
}

fn conversation_reply(seen: &Seen) -> (u16, String, Vec<u8>) {
    let author =
        json!({"login": "octocat", "avatarUrl": "https://avatars.githubusercontent.com/u/1"});
    let comment = |id: &str, body: &str, at: &str| {
        json!({
            "id": id, "body": body, "createdAt": at, "lastEditedAt": null,
            "url": format!("https://github.com/octo/repo/pull/7#{id}"), "author": author,
            "reactionGroups": [
                {"content": "THUMBS_UP", "viewerHasReacted": true, "reactors": {"totalCount": 2}},
                {"content": "HEART", "viewerHasReacted": false, "reactors": {"totalCount": 0}},
            ],
            "commit": null,
            "originalCommit": {"oid": BASE},
            "diffHunk": "@@ -8,5 +8,5 @@\n a\n b\n-c\n+d\n e\n f",
        })
    };
    match seen.operation().as_deref() {
        Some("PullRequestConversation") => json_reply(
            json!({"data": {"repository": {"viewerPermission": "TRIAGE", "pullRequest": {
                "viewerCanUpdate": true, "viewerDidAuthor": true,
                "labels": {"nodes": [{"name": "bug", "color": "d73a4a", "description": null}]},
                "reviewRequests": {"nodes": [
                    {"requestedReviewer": {"slug": "core"}},
                    {"requestedReviewer": {"login": "monalisa", "avatarUrl": null}},
                ]},
                "latestReviews": {"nodes": [
                    {"state": "CHANGES_REQUESTED", "author": {"login": "monalisa"}},
                    {"state": "APPROVED", "author": {"login": "hubot", "avatarUrl": null}},
                    {"state": "DISMISSED", "author": {"login": "octocat"}},
                ]},
                "id": "PR_7", "body": "Screenshot: ![shot](https://github.com/user-attachments/assets/abc-123)\n<video src=\"https://github.com/user-attachments/assets/vid-1\">\n![](https://github.com/user-attachments/assets/loop) ![](https://github.com/user-attachments/assets/huge) ![](https://github.com/user-attachments/assets/wide) ![](https://github.com/user-attachments/assets/page) ![](https://github.com/user-attachments/assets/dated) ![](https://github.com/user-attachments/assets/unsized) ![](https://user-images.githubusercontent.com/1/legacy.png) Not an avatar: https://avatars.githubusercontent.com/u/5",
                "createdAt": "2026-10-01T00:00:00Z", "lastEditedAt": null,
                "url": "https://github.com/octo/repo/pull/7", "author": author, "reactionGroups": [],
                "mergedAt": null,
                "comments": {"pageInfo": {"hasNextPage": false, "endCursor": null},
                    "nodes": [comment("IC_2", "second", "2026-10-03T00:00:00Z")]},
                "reviews": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [
                    {"id": "PRR_1", "body": "", "state": "COMMENTED", "submittedAt": "2026-10-02T00:00:00Z",
                     "createdAt": "2026-10-02T00:00:00Z", "url": "", "author": author, "reactionGroups": []},
                    {"id": "PRR_2", "body": "", "state": "APPROVED", "submittedAt": "2026-10-02T12:00:00Z",
                     "createdAt": "2026-10-02T12:00:00Z", "url": "", "author": author, "reactionGroups": []},
                ]},
            }}}}),
        ),
        Some("PullRequestReviewThreads") => {
            let after = &seen.variables()["cursor"];
            if after.is_null() {
                json_reply(
                    json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                        "pageInfo": {"hasNextPage": false, "endCursor": null},
                        "nodes": [{
                            "id": "PRRT_outdated", "isResolved": true, "isOutdated": true,
                            "path": "src/lib.rs", "line": null, "startLine": null,
                            "originalLine": 12, "originalStartLine": 10,
                            "diffSide": "RIGHT", "startDiffSide": "RIGHT",
                            "viewerCanReply": true, "viewerCanResolve": false, "viewerCanUnresolve": true,
                            "comments": {"totalCount": 12,
                                "pageInfo": {"hasNextPage": true, "endCursor": "C10"},
                                "nodes": [comment("RC_1", "These three lines ![x](https://github.com/user-attachments/assets/in-thread)", "2026-10-02T01:00:00Z")]},
                        }],
                    }}}}}),
                )
            } else {
                json_reply(
                    json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                        "pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [],
                    }}}}}),
                )
            }
        }
        Some("PullRequestThreadReplies") => {
            let thread = seen.variables()["thread"].as_str().unwrap().to_owned();
            let mut reply = comment("RC_11", "eleventh", "2026-10-04T00:00:00Z");
            reply["author"] =
                json!({"login": "hubot", "avatarUrl": "https://avatars.githubusercontent.com/u/9"});
            json_reply(json!({"data": {
                "repository": {"pullRequest": {"id": "PR_7"}},
                "node": {
                    "pullRequest": {"id": if thread == "PRRT_outdated" { "PR_7" } else { "PR_other" }},
                    "comments": {"pageInfo": {"hasNextPage": false, "endCursor": null},
                        "nodes": [reply]},
                },
            }}))
        }
        _ => (500, String::new(), Vec::new()),
    }
}

#[test]
fn conversation_keeps_an_outdated_multiline_thread_and_pages_its_replies_within_the_pull_request() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, conversation_reply);

    let conversation = reads.conversation(&key()).unwrap().value;
    assert!(conversation.complete);
    assert!(conversation.description.body.starts_with("Screenshot:"));
    assert_eq!(
        conversation
            .comments
            .iter()
            .map(|comment| comment.id.as_str())
            .collect::<Vec<_>>(),
        vec!["PRR_2", "IC_2"],
        "a bodiless comment review is the threads' wrapper; an approval is the event itself"
    );
    assert_eq!(conversation.comments[1].reactions.len(), 1);
    assert!(conversation.comments[1].reactions[0].viewer_reacted);
    assert_eq!(
        conversation.permissions,
        PullRequestPermissions {
            update: true,
            verdicts: vec![PullRequestReviewVerdict::Comment],
            label: true,
            request_reviewers: false,
        },
        "the author may only comment, and triage labels without requesting reviews"
    );
    assert_eq!(
        conversation
            .reviewers
            .iter()
            .map(|state| (
                state.reviewer.login.as_str(),
                state.reviewer.kind,
                state.verdict
            ))
            .collect::<Vec<_>>(),
        vec![
            ("core", PullRequestReviewerKind::Team, None),
            ("monalisa", PullRequestReviewerKind::User, None),
            (
                "hubot",
                PullRequestReviewerKind::User,
                Some(PullRequestReviewState::Approved)
            ),
        ],
        "a request outstanding outranks the reviewer's last verdict; a dismissed review is none"
    );
    assert_eq!(conversation.labels[0].name, "bug");
    let thread = &conversation.threads[0];
    assert_eq!(thread.id, "PRRT_outdated");
    assert!(thread.outdated && thread.resolved);
    assert!(
        thread.viewer_can_reply && thread.viewer_can_resolve,
        "a resolved thread's right is to unresolve it"
    );
    assert_eq!(
        thread.anchor,
        Some(PullRequestReviewAnchor {
            revision: BASE.into(),
            path: "src/lib.rs".into(),
            side: ReviewSide::New,
            start_line: 10,
            end_line: 12,
        }),
        "an outdated thread keeps the lines it was left on, at the commit it was left on"
    );
    assert_eq!(
        thread.diff_hunk.as_deref(),
        Some("-c\n+d\n e\n f"),
        "the excerpt is the hunk's last lines"
    );
    assert_eq!(
        (thread.total_comments, thread.replies_after.as_deref()),
        (12, Some("C10"))
    );

    let replies = reads
        .thread_replies(&key(), "PRRT_outdated", "C10")
        .unwrap()
        .value;
    assert_eq!(replies.comments[0].id, "RC_11");
    assert_eq!(
        reads
            .thread_replies(&key(), "PRRT_elsewhere", "C10")
            .unwrap_err(),
        GitHubError::NotFound,
        "a thread of another pull request is not read through this one"
    );
    let log = log.lock().unwrap();
    let replies = log
        .iter()
        .find(|seen| seen.operation().as_deref() == Some("PullRequestThreadReplies"))
        .unwrap();
    assert_eq!(replies.variables()["cursor"], "C10");
}

#[test]
fn a_thread_list_past_ten_pages_is_reported_incomplete() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, |seen| {
        if seen.operation().as_deref() == Some("PullRequestReviewThreads") {
            // Every page names a fresh next page.
            let page = seen.variables()["cursor"]
                .as_str()
                .unwrap_or("0")
                .to_owned();
            return json_reply(
                json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                    "pageInfo": {"hasNextPage": true, "endCursor": format!("{page}+")}, "nodes": [],
                }}}}}),
            );
        }
        conversation_reply(seen)
    });
    let conversation = reads.conversation(&key()).unwrap().value;
    assert!(!conversation.complete);
    assert_eq!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.operation().as_deref() == Some("PullRequestReviewThreads"))
            .count(),
        10
    );
}

fn viewed_page(after: &Value) -> (u16, String, Vec<u8>) {
    let page = after
        .as_str()
        .map_or(0, |cursor| cursor.parse::<u32>().unwrap());
    let nodes: Vec<_> = (0..100)
        .map(|index| {
            json!({
                "path": format!("file-{page}-{index}"),
                "viewerViewedState": match index % 3 { 0 => "VIEWED", 1 => "DISMISSED", _ => "UNVIEWED" },
            })
        })
        .collect();
    json_reply(json!({"data": {"repository": {"pullRequest": {"files": {
        "pageInfo": {"hasNextPage": true, "endCursor": (page + 1).to_string()},
        "nodes": nodes,
    }}}}}))
}

#[test]
fn viewed_files_stop_at_five_pages_and_marking_rereads_only_the_viewed_state() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, |seen| match seen.operation().as_deref() {
        Some("PullRequestViewedFiles") => viewed_page(&seen.variables()["after"]),
        Some("SetPullRequestFilesViewed") => json_reply(json!({"data": {
            "f0": {"clientMutationId": null}, "f1": {"clientMutationId": null},
        }})),
        None if seen.line.starts_with("GET /repos/octo/repo/pulls/7 ") => revisions(2),
        _ => conversation_reply(seen),
    });
    let count = |operation: &str| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.operation().as_deref() == Some(operation))
            .count()
    };

    let viewed = reads.viewed_files(&key()).unwrap().value;
    assert_eq!(viewed.files.len(), 500);
    assert!(!viewed.complete, "a sixth page is unknown, not unviewed");
    assert_eq!(
        viewed.files[..3]
            .iter()
            .map(|(_, state)| *state)
            .collect::<Vec<_>>(),
        vec![
            PullRequestViewedState::Viewed,
            PullRequestViewedState::Dismissed,
            PullRequestViewedState::Unviewed
        ]
    );
    reads.conversation(&key()).unwrap();
    assert_eq!(
        (
            count("PullRequestViewedFiles"),
            count("PullRequestConversation")
        ),
        (5, 1)
    );

    reads
        .set_viewed(&key(), &["a.rs".into(), "b/c d.rs".into()], true)
        .unwrap();
    let mutation = log
        .lock()
        .unwrap()
        .iter()
        .find(|seen| seen.operation().as_deref() == Some("SetPullRequestFilesViewed"))
        .cloned()
        .unwrap();
    let body: Value = serde_json::from_str(&mutation.body).unwrap();
    assert_eq!(
        body["variables"],
        json!({"pullRequestId": "PR_node_7", "f0_path": "a.rs", "f1_path": "b/c d.rs"}),
        "paths travel as variables and the node id comes from the pull request's REST read"
    );

    reads.viewed_files(&key()).unwrap();
    reads.conversation(&key()).unwrap();
    assert_eq!(
        (
            count("PullRequestViewedFiles"),
            count("PullRequestConversation")
        ),
        (10, 1),
        "marking a file reads the viewed state again and leaves the conversation shared"
    );
}

#[test]
fn reads_are_shared_in_flight_and_within_their_ttl_but_never_a_failure_or_across_accounts() {
    let fixture = Fixture::new();
    let store = Store::new();
    let credentials = store.credentials(&[]);
    store
        .store
        .set_token(
            tcode_core::pull_request::HostKind::Github,
            "github.com",
            Some("first-account"),
        )
        .unwrap();
    let reads = PullRequestReads::new(GitHubApi::new(credentials.clone(), fixture.builder()));
    let fail = Arc::new(Mutex::new(false));
    let failing = fail.clone();
    // The first conversation answer is held until every other reader has asked for it too.
    let (started, readers_started) = std::sync::mpsc::channel::<()>();
    let held = Mutex::new(Some(readers_started));
    let (_server, log) = serve(fixture, move |seen| {
        if seen.operation().as_deref() == Some("PullRequestConversation")
            && let Some(readers) = held.lock().unwrap().take()
        {
            for _ in 1..4 {
                readers.recv().unwrap();
            }
        }
        if *failing.lock().unwrap() {
            return (502, String::new(), b"{}".to_vec());
        }
        match seen.operation().as_deref() {
            Some("PullRequestViewedFiles") => {
                json_reply(json!({"data": {"repository": {"pullRequest": {
                    "files": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []},
                }}}}))
            }
            _ => conversation_reply(seen),
        }
    });
    let count = |operation: &str| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.operation().as_deref() == Some(operation))
            .count()
    };

    thread::scope(|scope| {
        let first = scope.spawn(|| reads.conversation(&key()).unwrap().value);
        let readers: Vec<_> = (1..4)
            .map(|_| {
                let (started, reads) = (started.clone(), &reads);
                scope.spawn(move || {
                    started.send(()).unwrap();
                    reads.conversation(&key()).unwrap().value
                })
            })
            .chain([first])
            .collect();
        for reader in readers {
            reader.join().unwrap();
        }
    });
    assert_eq!(
        count("PullRequestConversation"),
        1,
        "one read in flight serves all"
    );
    reads.conversation(&key()).unwrap();
    assert_eq!(count("PullRequestConversation"), 1, "shared within the TTL");

    reads.invalidate(&key());
    *fail.lock().unwrap() = true;
    assert!(reads.conversation(&key()).is_err());
    *fail.lock().unwrap() = false;
    reads.conversation(&key()).unwrap();
    assert_eq!(
        count("PullRequestConversation"),
        3,
        "a failure is kept for no one; the next reader asks again"
    );

    let first = reads.conversation(&key()).unwrap().value.account.clone();
    store
        .store
        .set_token(
            tcode_core::pull_request::HostKind::Github,
            "github.com",
            Some("second-account"),
        )
        .unwrap();
    let second = reads.conversation(&key()).unwrap().value.account.clone();
    assert_eq!(
        count("PullRequestConversation"),
        4,
        "another account reads its own"
    );
    assert_ne!(
        first, second,
        "media a client keys by the account misses after a switch"
    );
    assert_eq!(
        log.lock().unwrap().last().unwrap().authorization.as_deref(),
        Some("Bearer second-account")
    );

    let viewed = reads.viewed_files(&key()).unwrap();
    reads.viewed_files(&key()).unwrap();
    assert_eq!(count("PullRequestViewedFiles"), 1);
    let left = |at: std::time::SystemTime| {
        at.duration_since(std::time::SystemTime::now())
            .unwrap_or_default()
            .as_secs_f64()
            .round()
    };
    assert_eq!(
        (
            left(viewed.expires_at),
            left(reads.conversation(&key()).unwrap().expires_at)
        ),
        (15., 60.),
        "each answer tells its reader when to ask again"
    );
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::RgbaImage::new(width, height)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

#[test]
fn media_is_read_only_from_github_hosts_the_conversation_names_and_keeps_the_token_on_github() {
    use tcode_services::github::media::{MediaSource, classify};
    let credentialed = |url: &str| Some(MediaSource::Credentialed(url::Url::parse(url).unwrap()));
    let public = |url: &str| Some(MediaSource::Public(url::Url::parse(url).unwrap()));
    for (source, expected) in [
        (
            "https://github.com/user-attachments/assets/abc-123",
            credentialed("https://github.com/user-attachments/assets/abc-123"),
        ),
        (
            "https://www.github.com/octo/repo/assets/42/legacy-id",
            credentialed("https://github.com/octo/repo/assets/42/legacy-id"),
        ),
        (
            "https://github.com/octo/repo/blob/main/docs/shot.png?raw=true",
            credentialed("https://raw.githubusercontent.com/octo/repo/main/docs/shot.png"),
        ),
        (
            "https://RAW.githubusercontent.com:443/octo/repo/main/a.png",
            credentialed("https://raw.githubusercontent.com/octo/repo/main/a.png"),
        ),
        (
            "https://user-images.githubusercontent.com/1/legacy.png",
            public("https://user-images.githubusercontent.com/1/legacy.png"),
        ),
        (
            "https://private-user-images.githubusercontent.com/1/2.png?jwt=signed",
            public("https://private-user-images.githubusercontent.com/1/2.png?jwt=signed"),
        ),
        (
            "https://camo.githubusercontent.com/abc/def",
            public("https://camo.githubusercontent.com/abc/def"),
        ),
        (
            "https://avatars.githubusercontent.com/u/1?v=4",
            Some(MediaSource::Avatar(
                url::Url::parse("https://avatars.githubusercontent.com/u/1?v=4").unwrap(),
            )),
        ),
        ("http://github.com/user-attachments/assets/abc", None),
        ("https://github.com/octo/repo/pull/7", None),
        ("https://example.com/user-attachments/assets/abc", None),
    ] {
        assert_eq!(classify(source), expected, "{source}");
    }

    const DATED: &str = "Wed, 21 Oct 2026 07:28:00 GMT";
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, move |seen| {
        let path = seen.line.split_whitespace().nth(1).unwrap();
        let redirect = |to: &str| (302, format!("Location: {to}\r\n"), Vec::new());
        match (seen.host.as_str(), path) {
            ("api.github.com", _) => conversation_reply(seen),
            ("github.com", "/user-attachments/assets/abc-123") => {
                redirect("https://objects.githubusercontent.com/signed/abc?X-Amz-Signature=s")
            }
            ("objects.githubusercontent.com", "/signed/abc?X-Amz-Signature=s") => {
                if seen.header("if-none-match").as_deref() == Some("\"v1\"") {
                    return (304, String::new(), Vec::new());
                }
                (
                    200,
                    "Content-Type: image/png\r\nETag: \"v1\"\r\n".into(),
                    png(3, 2),
                )
            }
            ("github.com", "/user-attachments/assets/dated") => {
                if seen.header("if-modified-since").as_deref() == Some(DATED) {
                    return (304, String::new(), Vec::new());
                }
                (
                    200,
                    format!("Content-Type: image/png\r\nLast-Modified: {DATED}\r\n"),
                    png(1, 1),
                )
            }
            ("github.com", "/user-attachments/assets/vid-1") => {
                (200, "Content-Type: video/mp4\r\n".into(), vec![0; 64])
            }
            ("github.com", "/user-attachments/assets/in-thread") => {
                redirect("http://objects.githubusercontent.com/plain")
            }
            ("github.com", "/user-attachments/assets/loop") => {
                redirect("https://objects.githubusercontent.com/hop/1")
            }
            ("objects.githubusercontent.com", hop) if hop.starts_with("/hop/") => {
                let next = hop[5..].parse::<u32>().unwrap() + 1;
                redirect(&format!("/hop/{next}"))
            }
            ("github.com", "/user-attachments/assets/huge") => (
                200,
                "Content-Type: image/png\r\n".into(),
                vec![0; tcode_protocol::MAX_PULL_REQUEST_MEDIA_BYTES + 1],
            ),
            // No length to refuse it by: the read itself stops at the cap.
            ("github.com", "/user-attachments/assets/unsized") => {
                let body = vec![0; tcode_protocol::MAX_PULL_REQUEST_MEDIA_BYTES + 1];
                let mut chunked = format!("{:x}\r\n", body.len()).into_bytes();
                chunked.extend(body);
                chunked.extend(b"\r\n0\r\n\r\n");
                (
                    200,
                    "Content-Type: image/png\r\nTransfer-Encoding: chunked\r\n".into(),
                    chunked,
                )
            }
            ("github.com", "/user-attachments/assets/wide") => {
                (200, "Content-Type: image/png\r\n".into(), png(9000, 1))
            }
            ("user-images.githubusercontent.com", "/1/legacy.png") => {
                (200, "Content-Type: image/png\r\n".into(), png(1, 1))
            }
            ("avatars.githubusercontent.com", "/u/1" | "/u/9") => {
                (200, "Content-Type: image/png\r\n".into(), png(2, 2))
            }
            ("github.com", "/user-attachments/assets/page") => (
                200,
                "Content-Type: text/html\r\n".into(),
                b"<html></html>".to_vec(),
            ),
            _ => (404, String::new(), Vec::new()),
        }
    });
    let media = |url: &str| reads.media(&key(), url, None);
    let asset = |id: &str| format!("https://github.com/user-attachments/assets/{id}");

    let PullRequestMedia::Image {
        bytes,
        mime,
        validator,
        ..
    } = media(&asset("abc-123")).unwrap()
    else {
        panic!("an uploaded screenshot is an image")
    };
    assert_eq!((bytes, mime.as_str()), (png(3, 2), "image/png"));
    assert_eq!(validator.as_deref(), Some("\"v1\""));
    {
        let log = log.lock().unwrap();
        let asset = log.iter().find(|seen| seen.host == "github.com").unwrap();
        let object = log
            .iter()
            .find(|seen| seen.host == "objects.githubusercontent.com")
            .unwrap();
        assert_eq!(asset.authorization.as_deref(), Some("Bearer fixture-token"));
        assert_eq!(
            object.authorization, None,
            "the signed hop never sees the token"
        );
    }
    assert!(
        matches!(
            reads.media(&key(), &asset("abc-123"), Some("\"v1\"")),
            Ok(PullRequestMedia::NotModified { .. })
        ),
        "an entity tag revalidates with If-None-Match"
    );
    let PullRequestMedia::Image { validator, .. } = media(&asset("dated")).unwrap() else {
        panic!("a dated upload is an image")
    };
    assert_eq!(validator.as_deref(), Some(DATED));
    assert!(
        matches!(
            reads.media(&key(), &asset("dated"), Some(DATED)),
            Ok(PullRequestMedia::NotModified { .. })
        ),
        "a Last-Modified date revalidates with If-Modified-Since"
    );
    assert!(matches!(
        media("https://user-images.githubusercontent.com/1/legacy.png").unwrap(),
        PullRequestMedia::Image { .. }
    ));
    assert_eq!(
        log.lock().unwrap().last().unwrap().authorization,
        None,
        "a public upload is read without the token"
    );
    assert!(matches!(
        media("https://avatars.githubusercontent.com/u/1").unwrap(),
        PullRequestMedia::Image { .. }
    ));
    assert_eq!(
        log.lock().unwrap().last().unwrap().authorization,
        None,
        "an author's avatar is public and read without the token"
    );
    assert_eq!(
        media("https://avatars.githubusercontent.com/u/5").unwrap_err(),
        GitHubError::InvalidInput,
        "an avatar is read for an author, not for a body that quotes its address"
    );
    assert_eq!(
        media("https://avatars.githubusercontent.com/u/9").unwrap_err(),
        GitHubError::InvalidInput,
        "an avatar no author of the pull request has is not read"
    );
    reads
        .thread_replies(&key(), "PRRT_outdated", "C10")
        .unwrap();
    assert!(
        matches!(
            media("https://avatars.githubusercontent.com/u/9").unwrap(),
            PullRequestMedia::Image { .. }
        ),
        "a reply's author is an author of the pull request once the reply is read"
    );
    assert_eq!(
        media(&asset("vid-1")).unwrap(),
        PullRequestMedia::External {
            mime: "video/mp4".into()
        }
    );
    assert_eq!(
        media(&asset("in-thread")).unwrap_err(),
        GitHubError::InvalidResponse,
        "a redirect off HTTPS is not followed, for media a review thread names"
    );
    let before = log.lock().unwrap().len();
    assert_eq!(
        media(&asset("loop")).unwrap_err(),
        GitHubError::InvalidResponse
    );
    assert_eq!(
        log.lock().unwrap().len() - before,
        4,
        "three redirects are followed and no fourth"
    );
    assert_eq!(
        media(&asset("huge")).unwrap_err(),
        GitHubError::BodyTooLarge
    );
    assert_eq!(
        media(&asset("unsized")).unwrap_err(),
        GitHubError::BodyTooLarge,
        "a body without a length is cut at the cap while it is read"
    );
    assert_eq!(
        media(&asset("wide")).unwrap_err(),
        GitHubError::BodyTooLarge
    );
    assert_eq!(
        media(&asset("page")).unwrap_err(),
        GitHubError::UnsupportedMedia
    );
    let before = log.lock().unwrap().len();
    assert_eq!(
        media(&asset("not-mentioned")).unwrap_err(),
        GitHubError::InvalidInput,
        "only media the pull request names is read"
    );
    assert_eq!(
        media("https://example.com/user-attachments/assets/abc-123").unwrap(),
        PullRequestMedia::Unsupported,
        "an image elsewhere is the client's to draw by its URL"
    );
    assert_eq!(log.lock().unwrap().len(), before);
}

/// Where a node id hangs, as `node(id:)` answers it: the conversation's comments, review, thread
/// and the pull request itself are this one's; `IC_foreign` is another pull request's comment.
fn subject_reply(seen: &Seen) -> (u16, String, Vec<u8>) {
    let subject = seen.variables()["subject"].as_str().unwrap().to_owned();
    let node = match subject.as_str() {
        "PR_node_7" => json!({"__typename": "PullRequest", "id": "PR_node_7"}),
        "IC_foreign" => {
            json!({"__typename": "IssueComment", "id": subject, "pullRequest": {"id": "PR_other"}})
        }
        _ => {
            let kind = match &subject[..subject.find('_').unwrap()] {
                "IC" => "IssueComment",
                "RC" => "PullRequestReviewComment",
                "PRR" => "PullRequestReview",
                _ => "PullRequestReviewThread",
            };
            json!({"__typename": kind, "id": subject, "pullRequest": {"id": "PR_node_7"}})
        }
    };
    json_reply(json!({"data": {
        "repository": {"pullRequest": {"id": "PR_node_7"}},
        "node": node,
    }}))
}

fn writes_reply(seen: &Seen) -> (u16, String, Vec<u8>) {
    match seen.operation().as_deref() {
        Some("PullRequestSubject") => subject_reply(seen),
        Some(_) if seen.body.contains("\"query\":\"mutation") => {
            json_reply(json!({"data": {"ok": {"clientMutationId": null}}}))
        }
        None if seen.line.starts_with("GET /repos/octo/repo/pulls/7 ") => revisions(1),
        _ => conversation_reply(seen),
    }
}

fn mutations(log: &Log) -> Vec<(String, Value)> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|seen| seen.body.contains("\"query\":\"mutation"))
        .map(|seen| (seen.operation().unwrap(), seen.variables()))
        .collect()
}

#[test]
fn conversation_writes_send_their_payloads_once_and_the_conversation_is_read_again() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, writes_reply);
    let count = |operation: &str| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.operation().as_deref() == Some(operation))
            .count()
    };
    reads.conversation(&key()).unwrap();

    let actions = [
        PullRequestAction::Comment {
            body: "Looks good".into(),
        },
        PullRequestAction::Edit {
            title: Some("Better title".into()),
            body: None,
        },
        PullRequestAction::ReplyToThread {
            thread_id: "PRRT_1".into(),
            body: "Fixed".into(),
        },
        PullRequestAction::ResolveThread {
            thread_id: "PRRT_1".into(),
            resolved: true,
        },
        PullRequestAction::EditComment {
            comment_id: "RC_1".into(),
            body: "Reworded".into(),
        },
        PullRequestAction::React {
            subject_id: "PRR_2".into(),
            content: PullRequestReactionContent::Rocket,
            reacted: true,
        },
        PullRequestAction::React {
            subject_id: "PR_node_7".into(),
            content: PullRequestReactionContent::ThumbsUp,
            reacted: false,
        },
    ];
    for action in &actions {
        assert_eq!(reads.act(&key(), action), PullRequestActionResult::Applied);
    }
    assert_eq!(
        mutations(&log),
        vec![
            (
                "AddPullRequestComment".into(),
                json!({"subjectId": "PR_node_7", "body": "Looks good"})
            ),
            (
                "EditPullRequest".into(),
                json!({"pullRequestId": "PR_node_7", "title": "Better title"}),
            ),
            (
                "ReplyToPullRequestThread".into(),
                json!({"threadId": "PRRT_1", "body": "Fixed"})
            ),
            (
                "ResolvePullRequestThread".into(),
                json!({"threadId": "PRRT_1"})
            ),
            (
                "EditPullRequestReviewComment".into(),
                json!({"commentId": "RC_1", "body": "Reworded"})
            ),
            (
                "AddPullRequestReaction".into(),
                json!({"subjectId": "PRR_2", "content": "ROCKET"})
            ),
            (
                "RemovePullRequestReaction".into(),
                json!({"subjectId": "PR_node_7", "content": "THUMBS_UP"})
            ),
        ],
        "an edit naming only the title leaves the body out, so GitHub keeps it"
    );
    let rest_reads = log
        .lock()
        .unwrap()
        .iter()
        .filter(|seen| seen.line.starts_with("GET /repos/octo/repo/pulls/7 "))
        .count();
    assert_eq!(
        rest_reads, 1,
        "the node id comes from one REST read of the pull request"
    );

    reads.conversation(&key()).unwrap();
    assert_eq!(
        count("PullRequestConversation"),
        2,
        "a write drops the conversation it changed"
    );
}

#[test]
fn a_subject_of_another_pull_request_is_refused_before_any_mutation() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, writes_reply);

    for action in [
        PullRequestAction::React {
            subject_id: "IC_foreign".into(),
            content: PullRequestReactionContent::Heart,
            reacted: true,
        },
        PullRequestAction::EditComment {
            comment_id: "IC_foreign".into(),
            body: "Not mine to change".into(),
        },
    ] {
        assert_eq!(
            reads.act(&key(), &action),
            PullRequestActionResult::Rejected(PullRequestRejection::ForeignSubject)
        );
    }
    assert!(mutations(&log).is_empty());
}

#[test]
fn labels_are_added_in_one_request_and_removed_one_per_request_until_one_fails() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, |seen| {
        if seen
            .line
            .starts_with("DELETE /repos/octo/repo/issues/7/labels/needs%2Freview ")
        {
            return (
                422,
                String::new(),
                br#"{"message":"Label does not exist"}"#.to_vec(),
            );
        }
        match seen.operation().as_deref() {
            Some("PullRequestLabelCandidates") => json_reply(json!({"data": {"repository": {
                "labels": {"pageInfo": {"hasNextPage": true}, "nodes": [
                    {"name": "bug", "color": "d73a4a", "description": "Something is broken"},
                    {"name": "docs", "color": "0075ca", "description": null},
                ]},
                "pullRequest": {"labels": {"nodes": [{"name": "docs"}, {"name": "retired"}]}},
            }}})),
            Some(_) => conversation_reply(seen),
            None => json_reply(json!([])),
        }
    });
    let lines = || {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.operation().is_none())
            .map(|seen| {
                let line = seen.line.rsplit_once(' ').unwrap().0.to_owned();
                (line, seen.body.clone())
            })
            .collect::<Vec<_>>()
    };

    let candidates = reads.label_candidates(&key()).unwrap().value;
    assert_eq!(
        candidates
            .labels
            .iter()
            .map(|label| (label.name.as_str(), label.applied))
            .collect::<Vec<_>>(),
        vec![("retired", true), ("bug", false), ("docs", true)],
        "a label the repository no longer lists leads, so it can still be taken off"
    );
    assert!(!candidates.complete);
    reads.conversation(&key()).unwrap();

    assert_eq!(
        reads.act(
            &key(),
            &PullRequestAction::SetLabels {
                add: vec!["bug".into(), "good first issue".into()],
                remove: vec!["docs".into(), "needs/review".into(), "retired".into()],
            }
        ),
        PullRequestActionResult::Partial {
            applied: vec!["bug".into(), "good first issue".into(), "docs".into()],
            unapplied: vec!["needs/review".into(), "retired".into()],
            failure: Box::new(PullRequestActionResult::Rejected(
                PullRequestRejection::Refused {
                    messages: vec!["Label does not exist".into()]
                }
            )),
        }
    );
    assert_eq!(
        lines(),
        vec![
            (
                "POST /repos/octo/repo/issues/7/labels".into(),
                r#"{"labels":["bug","good first issue"]}"#.into()
            ),
            (
                "DELETE /repos/octo/repo/issues/7/labels/docs".into(),
                String::new()
            ),
            (
                "DELETE /repos/octo/repo/issues/7/labels/needs%2Freview".into(),
                String::new()
            ),
        ],
        "nothing is sent after the label that failed"
    );
    reads.label_candidates(&key()).unwrap();
    reads.conversation(&key()).unwrap();
    let count = |operation: &str| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.operation().as_deref() == Some(operation))
            .count()
    };
    assert_eq!(
        (
            count("PullRequestLabelCandidates"),
            count("PullRequestConversation")
        ),
        (2, 2),
        "a label write drops the candidates and the conversation that shows the labels"
    );
}

#[test]
fn reviewers_are_offered_without_the_author_and_requested_by_kind() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let (_server, log) = serve(fixture, |seen| match seen.operation().as_deref() {
        None if seen.line.starts_with("DELETE ") => (
            422,
            String::new(),
            br#"{"message":"Reviews may only be requested from collaborators"}"#.to_vec(),
        ),
        Some("PullRequestReviewerCandidates") => json_reply(json!({"data": {"repository": {
            "assignableUsers": {"pageInfo": {"hasNextPage": false}, "nodes": [
                {"login": "octocat", "name": "The Author", "avatarUrl": null},
                {"login": "hubot", "name": null, "avatarUrl": "https://avatars.githubusercontent.com/u/9"},
                {"login": "monalisa", "name": "Mona", "avatarUrl": null},
            ]},
            "pullRequest": {"author": {"login": "octocat"}, "reviewRequests": {"nodes": [
                {"requestedReviewer": {"slug": "core", "name": "Core", "avatarUrl": null}},
                {"requestedReviewer": {"login": "monalisa", "name": "Mona", "avatarUrl": null}},
            ]}},
        }}})),
        _ => json_reply(json!({})),
    });

    let candidates = reads.reviewer_candidates(&key()).unwrap().value;
    assert_eq!(
        candidates
            .reviewers
            .iter()
            .map(|candidate| (
                candidate.reviewer.login.as_str(),
                candidate.reviewer.kind,
                candidate.requested
            ))
            .collect::<Vec<_>>(),
        vec![
            ("core", PullRequestReviewerKind::Team, true),
            ("monalisa", PullRequestReviewerKind::User, true),
            ("hubot", PullRequestReviewerKind::User, false),
        ]
    );
    let user = |login: &str| PullRequestReviewer {
        id: login.into(),
        login: login.into(),
        kind: PullRequestReviewerKind::User,
    };
    assert_eq!(
        reads.act(
            &key(),
            &PullRequestAction::SetReviewers {
                add: vec![user("hubot")],
                remove: vec![
                    user("monalisa"),
                    PullRequestReviewer {
                        id: "core".into(),
                        login: "core".into(),
                        kind: PullRequestReviewerKind::Team,
                    },
                ],
            }
        ),
        PullRequestActionResult::Partial {
            applied: vec!["hubot".into()],
            unapplied: vec!["monalisa".into(), "core".into()],
            failure: Box::new(PullRequestActionResult::Rejected(
                PullRequestRejection::Refused {
                    messages: vec!["Reviews may only be requested from collaborators".into()]
                }
            )),
        }
    );
    let sent: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .filter(|seen| seen.operation().is_none())
        .map(|seen| (seen.line.clone(), seen.body.clone()))
        .collect();
    assert_eq!(
        sent,
        vec![
            (
                "POST /repos/octo/repo/pulls/7/requested_reviewers HTTP/1.1".into(),
                r#"{"reviewers":["hubot"],"team_reviewers":[]}"#.into()
            ),
            (
                "DELETE /repos/octo/repo/pulls/7/requested_reviewers HTTP/1.1".into(),
                r#"{"reviewers":["monalisa"],"team_reviewers":["core"]}"#.into()
            ),
        ],
        "the additions go in one request and the removals in another"
    );
}

fn draft_comment(
    id: u64,
    revision: &str,
    lines: (u32, u32),
    side: ReviewSide,
) -> PullRequestReviewDraftComment {
    PullRequestReviewDraftComment {
        id,
        revision: revision.into(),
        path: "src/lib.rs".into(),
        side,
        start_line: lines.0,
        end_line: lines.1,
        body: format!("Comment {id}"),
        placed: true,
    }
}

#[test]
fn a_review_is_one_submission_at_the_head_read_fresh_and_a_moved_head_sends_nothing() {
    const MOVED: &str = "3333333333333333333333333333333333333333";
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let head = Arc::new(Mutex::new(HEAD));
    let current = head.clone();
    let (_server, log) = serve(fixture, move |seen| {
        if seen.line.starts_with("GET /repos/octo/repo/pulls/7 ") {
            let (status, headers, body) = revisions(1);
            let body = String::from_utf8(body)
                .unwrap()
                .replace(HEAD, &current.lock().unwrap());
            return (status, headers, body.into_bytes());
        }
        json_reply(json!({"id": 1}))
    });
    let reviews = || {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| {
                seen.line
                    .starts_with("POST /repos/octo/repo/pulls/7/reviews ")
            })
            .map(|seen| serde_json::from_str::<Value>(&seen.body).unwrap())
            .collect::<Vec<_>>()
    };
    let comments = [
        draft_comment(1, HEAD, (4, 4), ReviewSide::New),
        draft_comment(2, HEAD, (9, 12), ReviewSide::Old),
    ];
    // The files were read at HEAD, which the cached revisions still say.
    reads.files(&key(), Some(1)).ok();
    *head.lock().unwrap() = MOVED;

    assert_eq!(
        reads.submit_review(
            &key(),
            PullRequestReviewVerdict::RequestChanges,
            HEAD,
            "Two things",
            &comments
        ),
        PullRequestActionResult::Rejected(PullRequestRejection::StaleHead { head: MOVED.into() }),
        "a head that moved after the files were read is seen before anything is sent"
    );
    assert!(reviews().is_empty());

    let mut unplaced = draft_comment(2, MOVED, (9, 12), ReviewSide::Old);
    unplaced.placed = false;
    assert_eq!(
        reads.submit_review(
            &key(),
            PullRequestReviewVerdict::RequestChanges,
            MOVED,
            "Two things",
            &[draft_comment(1, MOVED, (4, 4), ReviewSide::New), unplaced]
        ),
        PullRequestActionResult::Rejected(PullRequestRejection::Invalid),
        "a comment whose lines changed is never sent"
    );
    assert!(reviews().is_empty());

    let comments = [
        draft_comment(1, MOVED, (4, 4), ReviewSide::New),
        draft_comment(2, MOVED, (9, 12), ReviewSide::Old),
    ];
    assert_eq!(
        reads.submit_review(
            &key(),
            PullRequestReviewVerdict::RequestChanges,
            MOVED,
            "Two things",
            &comments
        ),
        PullRequestActionResult::Applied
    );
    assert_eq!(
        reviews(),
        vec![json!({
            "commit_id": MOVED,
            "event": "REQUEST_CHANGES",
            "body": "Two things",
            "comments": [
                {"path": "src/lib.rs", "line": 4, "side": "RIGHT", "body": "Comment 1"},
                {"path": "src/lib.rs", "start_line": 9, "start_side": "LEFT", "line": 12,
                 "side": "LEFT", "body": "Comment 2"},
            ],
        })]
    );
}

#[test]
fn a_write_left_unanswered_is_uncertain_and_never_sent_again() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let model = Mutex::new(Lifecycle::default());
    let (_server, log) = serve(fixture, move |seen| {
        if seen
            .line
            .starts_with("POST /repos/octo/repo/issues/7/labels ")
        {
            return (502, String::new(), Vec::new());
        }
        match seen.operation().as_deref() {
            Some("AddPullRequestComment" | "MergePullRequest") => (0, String::new(), Vec::new()),
            Some("RevertPullRequest") => {
                json_reply(json!({"data": {"revertPullRequest": {"revertPullRequest": null}}}))
            }
            _ => lifecycle_reply(&model, seen),
        }
    });
    let sent = |prefix: &str| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|seen| {
                seen.line.starts_with(prefix) || seen.operation().as_deref() == Some(prefix)
            })
            .count()
    };

    assert_eq!(
        reads.act(
            &key(),
            &PullRequestAction::Comment {
                body: "Once".into()
            }
        ),
        PullRequestActionResult::Uncertain
    );
    assert_eq!(
        reads.act(
            &key(),
            &PullRequestAction::SetLabels {
                add: vec!["bug".into()],
                remove: Vec::new(),
            }
        ),
        PullRequestActionResult::Uncertain,
        "a server failure may have applied the write"
    );
    assert_eq!(
        reads.act(
            &key(),
            &PullRequestAction::Merge {
                head: HEAD.into(),
                method: PullRequestMergeMethod::Squash,
                auto: false,
                remove_credits: false,
            }
        ),
        PullRequestActionResult::Uncertain,
        "a merge whose answer was lost may have merged"
    );
    assert_eq!(
        reads.act(&key(), &PullRequestAction::Revert),
        PullRequestActionResult::Uncertain,
        "an answer that names no revert may still have opened one"
    );
    assert_eq!(
        (
            sent("AddPullRequestComment"),
            sent("POST /repos/octo/repo/issues/7/labels "),
            sent("MergePullRequest"),
            sent("RevertPullRequest"),
        ),
        (1, 1, 1, 1)
    );
}

/// What GitHub holds of the pull request that the lifecycle reads and writes see.
struct Lifecycle {
    head: &'static str,
    merge_state: &'static str,
    behind_by: u64,
    queue: bool,
    rebase_allowed: bool,
    message: &'static str,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            head: HEAD,
            merge_state: "CLEAN",
            behind_by: 2,
            queue: false,
            rebase_allowed: true,
            message: "Fix the parser\n\nCo-authored-by: Ada <ada@example.com>",
        }
    }
}

fn lifecycle_reply(model: &Mutex<Lifecycle>, seen: &Seen) -> (u16, String, Vec<u8>) {
    let model = model.lock().unwrap();
    match seen.operation().as_deref() {
        Some("PullRequestActionState") => json_reply(json!({"data": {"repository": {
            "viewerPermission": "WRITE",
            "mergeCommitAllowed": true, "squashMergeAllowed": true, "rebaseMergeAllowed": model.rebase_allowed,
            "autoMergeAllowed": true,
            "pullRequest": {
                "id": "PR_node_7", "headRefOid": model.head,
                "isMergeQueueEnabled": model.queue, "mergeStateStatus": model.merge_state,
                "viewerCanUpdate": true, "viewerCanUpdateBranch": true,
                "autoMergeRequest": null, "mergeQueueEntry": null,
                "baseRef": {"compare": {"behindBy": model.behind_by}},
                "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [
                    {"__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "FAILURE"},
                    {"__typename": "CheckRun", "name": "test", "status": "IN_PROGRESS", "conclusion": null},
                ]}}}}]},
            },
        }}})),
        Some("PullRequestMergeMessage") => {
            json_reply(json!({"data": {"repository": {"pullRequest": {
                "isMergeQueueEnabled": model.queue, "headRefOid": model.head,
                "viewerMergeBodyText": model.message,
            }}}}))
        }
        // GitHub merges at once, queues behind a merge queue, or arms auto-merge otherwise.
        Some("MergePullRequest") => {
            json_reply(json!({"data": {"mergePullRequest": {"pullRequest": {
                "merged": true, "mergeQueueEntry": null, "autoMergeRequest": null,
            }}}}))
        }
        Some("EnablePullRequestAutoMerge") => {
            let method = seen.variables()["input"]["mergeMethod"].clone();
            json_reply(
                json!({"data": {"enablePullRequestAutoMerge": {"pullRequest": if model.queue {
                    json!({"merged": false, "mergeQueueEntry": {"position": 2}, "autoMergeRequest": null})
                } else {
                    json!({"merged": false, "mergeQueueEntry": null, "autoMergeRequest": {"mergeMethod": method}})
                }}}}),
            )
        }
        Some("RevertPullRequest") => json_reply(json!({"data": {"revertPullRequest": {
            "revertPullRequest": {"number": 8, "url": "https://github.com/octo/repo/pull/8"},
        }}})),
        _ => writes_reply(seen),
    }
}

fn reads_of(log: &Log, operation: &str) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|seen| seen.operation().as_deref() == Some(operation))
        .count()
}

fn merge(method: PullRequestMergeMethod, auto: bool) -> PullRequestAction {
    PullRequestAction::Merge {
        head: HEAD.into(),
        method,
        auto,
        remove_credits: false,
    }
}

#[test]
fn a_merge_reads_the_head_fresh_and_tells_merged_queued_and_armed_apart() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let model = Arc::new(Mutex::new(Lifecycle::default()));
    let answering = model.clone();
    let (_server, log) = serve(fixture, move |seen| lifecycle_reply(&answering, seen));
    use PullRequestMergeMethod::*;

    let state = reads.action_state(&key()).unwrap().value;
    assert_eq!(
        (
            state.merge_methods.clone(),
            state.failing_checks.clone(),
            state.pending_checks,
            state.behind_by
        ),
        (
            vec![Merge, Squash, Rebase],
            vec!["lint".to_owned()],
            1,
            Some(2)
        )
    );

    assert_eq!(
        reads.act(&key(), &merge(Squash, false)),
        PullRequestActionResult::Applied
    );
    assert_eq!(
        reads.act(&key(), &merge(Merge, true)),
        PullRequestActionResult::Applied,
        "auto-merge asked of a pull request that can merge now merges it"
    );
    model.lock().unwrap().merge_state = "BLOCKED";
    assert_eq!(
        reads.act(&key(), &merge(Squash, true)),
        PullRequestActionResult::AutoMergeEnabled { method: Squash }
    );
    model.lock().unwrap().queue = true;
    assert_eq!(
        reads.act(&key(), &merge(Rebase, false)),
        PullRequestActionResult::Queued { position: Some(2) },
        "a merge queue takes the pull request; it is not merged"
    );
    model.lock().unwrap().rebase_allowed = false;
    assert_eq!(
        reads.act(&key(), &merge(Rebase, false)),
        PullRequestActionResult::Rejected(PullRequestRejection::Invalid),
        "a method the repository does not allow sends nothing"
    );
    model.lock().unwrap().head = BASE;
    assert_eq!(
        reads.act(&key(), &merge(Squash, false)),
        PullRequestActionResult::Rejected(PullRequestRejection::StaleHead { head: BASE.into() }),
        "a head that moved since the user looked sends nothing"
    );

    let input = |method: &str| json!({"input": {"pullRequestId": "PR_node_7", "mergeMethod": method, "expectedHeadOid": HEAD}});
    assert_eq!(
        mutations(&log),
        vec![
            ("MergePullRequest".into(), input("SQUASH")),
            ("MergePullRequest".into(), input("MERGE")),
            ("EnablePullRequestAutoMerge".into(), input("SQUASH")),
            ("EnablePullRequestAutoMerge".into(), input("REBASE")),
        ]
    );
}

#[test]
fn credits_leave_the_merge_message_only_when_it_has_some() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let model = Arc::new(Mutex::new(Lifecycle::default()));
    let answering = model.clone();
    let (_server, log) = serve(fixture, move |seen| lifecycle_reply(&answering, seen));
    let cleaning = |method| PullRequestAction::Merge {
        head: HEAD.into(),
        method,
        auto: false,
        remove_credits: true,
    };

    reads.act(&key(), &cleaning(PullRequestMergeMethod::Squash));
    model.lock().unwrap().message = "Fix the parser\n\nCo-authored-by: Ada <ada@example.com>\nCo-authored-by: Claude <noreply@anthropic.com>\n\n🤖 Generated with [Claude Code](https://claude.ai/code)\n";
    reads.act(&key(), &cleaning(PullRequestMergeMethod::Merge));
    reads.act(&key(), &cleaning(PullRequestMergeMethod::Rebase));

    let bodies: Vec<_> = mutations(&log)
        .into_iter()
        .map(|(_, variables)| variables["input"]["commitBody"].clone())
        .collect();
    assert_eq!(
        bodies,
        vec![
            Value::Null,
            json!("Fix the parser\n\nCo-authored-by: Ada <ada@example.com>"),
            Value::Null,
        ],
        "a message with no agent credit is GitHub's own; people stay credited"
    );
    assert_eq!(
        reads_of(&log, "PullRequestMergeMessage"),
        2,
        "rebasing keeps each commit's message, so none is read"
    );
}

#[test]
fn a_branch_update_carries_the_head_and_is_nothing_when_not_behind() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let model = Arc::new(Mutex::new(Lifecycle::default()));
    let answering = model.clone();
    let (_server, log) = serve(fixture, move |seen| lifecycle_reply(&answering, seen));
    let update = |head: &str, rebase| PullRequestAction::UpdateBranch {
        head: head.into(),
        rebase,
    };

    assert_eq!(
        reads.act(&key(), &update(HEAD, false)),
        PullRequestActionResult::Applied
    );
    assert_eq!(
        reads.act(&key(), &update(HEAD, true)),
        PullRequestActionResult::Applied
    );
    assert_eq!(
        reads.act(&key(), &update(BASE, false)),
        PullRequestActionResult::Rejected(PullRequestRejection::StaleHead { head: HEAD.into() })
    );
    model.lock().unwrap().behind_by = 0;
    assert_eq!(
        reads.act(&key(), &update(HEAD, false)),
        PullRequestActionResult::UpToDate
    );
    let update = |method: &str| {
        (
            "UpdatePullRequestBranch".to_owned(),
            json!({"pullRequestId": "PR_node_7", "expectedHeadOid": HEAD, "updateMethod": method}),
        )
    };
    assert_eq!(mutations(&log), vec![update("MERGE"), update("REBASE")]);
}

#[test]
fn lifecycle_writes_name_the_pull_request_and_a_revert_opens_one() {
    let fixture = Fixture::new();
    let store = Store::new();
    let reads = reads(&fixture, &store);
    let model = Arc::new(Mutex::new(Lifecycle::default()));
    let (_server, log) = serve(fixture, move |seen| lifecycle_reply(&model, seen));

    for action in [
        PullRequestAction::ReadyForReview,
        PullRequestAction::ConvertToDraft,
        PullRequestAction::Close,
        PullRequestAction::Reopen,
        PullRequestAction::DisableAutoMerge,
    ] {
        assert_eq!(reads.act(&key(), &action), PullRequestActionResult::Applied);
    }
    assert_eq!(
        reads.act(&key(), &PullRequestAction::Revert),
        PullRequestActionResult::Opened {
            number: 8,
            url: "https://github.com/octo/repo/pull/8".into()
        }
    );
    let named = json!({"pullRequestId": "PR_node_7"});
    assert_eq!(
        mutations(&log),
        [
            "MarkPullRequestReady",
            "ConvertPullRequestToDraft",
            "ClosePullRequest",
            "ReopenPullRequest",
            "DisablePullRequestAutoMerge",
            "RevertPullRequest",
        ]
        .map(|operation| (operation.to_owned(), named.clone()))
        .to_vec()
    );
}
