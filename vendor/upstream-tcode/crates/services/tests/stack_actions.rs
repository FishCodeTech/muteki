use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tcode_core::pull_request::{
    PullRequestKey, PullRequestMergeMethod, PullRequestState, StackRebaseFailure, StackRebaseStep,
};
use tcode_protocol::{PullRequestActionResult, PullRequestRejection, PullRequestStackHead};
use tcode_services::github::{
    GitHubApi,
    pull_request_reads::PullRequestReads,
    stack_actions::MergeSubmission,
    stack_rebase::{self, Identity, RebaseLayer},
};

#[path = "support/github.rs"]
#[allow(dead_code)] // tests/github.rs drives the fixture by hand as well.
mod fixture;
use fixture::{Fixture, Server, Store};

fn sha(digit: char) -> String {
    std::iter::repeat_n(digit, 40).collect()
}

/// The stack GitHub's stacks API lists for #3: #1 merged, then #2 and #3 open.
struct Stack {
    number: u64,
    layers: Vec<(u64, String, &'static str, bool)>,
}

impl Default for Stack {
    fn default() -> Self {
        Self {
            number: 50,
            layers: vec![
                (1, sha('a'), "merged", false),
                (2, sha('b'), "open", false),
                (3, sha('c'), "open", false),
            ],
        }
    }
}

impl Stack {
    /// The listing names the stack and its members, without their titles.
    fn listing(&self) -> Value {
        json!([{
            "number": self.number,
            "url": "https://api.github.com/repos/octo/repo/stacks/50",
            "base": {"ref": "main"},
            "pull_requests": self.layers.iter().map(|(number, head, ..)| json!({
                "number": number,
                "head": {"ref": format!("layer-{number}"), "sha": head},
            })).collect::<Vec<_>>(),
        }])
    }

    fn detail(&self) -> Value {
        json!({
            "number": self.number,
            "url": "https://api.github.com/repos/octo/repo/stacks/50",
            "base": {"ref": "main"},
            "pull_requests": self.layers.iter().map(|(number, head, state, draft)| json!({
                "number": number,
                "title": format!("Layer {number}"),
                "head": {"ref": format!("layer-{number}"), "sha": head},
                "state": if *state == "merged" { "closed" } else { state },
                "merged_at": (*state == "merged").then_some("2026-10-01T00:00:00Z"),
                "draft": draft,
            })).collect::<Vec<_>>(),
        })
    }
}

/// A GitHub listing `stack` for #3 that answers every other request with `other`, and the
/// request line of everything it was asked.
fn github(
    stack: Stack,
    other: impl Fn(&str, &[u8]) -> (u16, Value) + Send + Sync + 'static,
) -> (Arc<PullRequestReads>, Server, Arc<Mutex<Vec<String>>>) {
    let store = Store::new();
    let fixture = Fixture::new();
    let reads = PullRequestReads::new(GitHubApi::new(
        store.credentials(&[("GH_TOKEN", "fixture-token")]),
        fixture.builder(),
    ));
    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = asked.clone();
    let server: Server = fixture.serve(move |exchange| {
        let line = exchange.request.lines().next().unwrap().to_owned();
        seen.lock().unwrap().push(line.clone());
        let (status, reply) = if line.starts_with("GET /repos/octo/repo/stacks?pull_request=3 ") {
            (200, stack.listing())
        } else if line.starts_with(&format!("GET /repos/octo/repo/stacks/{} ", stack.number)) {
            (200, stack.detail())
        } else {
            other(&line, &exchange.body)
        };
        exchange.reply(status, "", reply.to_string().as_bytes());
    });
    (reads, server, asked)
}

fn heads(heads: &[(u64, String)]) -> Vec<PullRequestStackHead> {
    heads
        .iter()
        .map(|(number, head)| PullRequestStackHead {
            number: *number,
            head: head.clone(),
        })
        .collect()
}

/// Submits a merge of #3 at `heads` to a GitHub listing `stack` that answers the submission with
/// `put`, and returns what came of it with the body of every submission GitHub saw.
fn merge(
    stack: Stack,
    put: (u16, Value),
    confirmed: &[(u64, String)],
) -> (MergeSubmission, Vec<Value>) {
    let submitted: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = submitted.clone();
    let (reads, _server, _) = github(stack, move |line, body| {
        if line.starts_with("PUT /repos/octo/repo/pulls/3/merge-async ") {
            seen.lock()
                .unwrap()
                .push(serde_json::from_slice(body).unwrap());
            put.clone()
        } else {
            (404, json!({"message": "Not Found"}))
        }
    });
    let submission = reads.merge_stack(
        &PullRequestKey::new("github.com", "octo/repo", 3),
        50,
        &heads(confirmed),
        PullRequestMergeMethod::Squash,
    );
    let submitted = submitted.lock().unwrap().clone();
    (submission, submitted)
}

fn scope() -> Vec<(u64, String)> {
    vec![(2, sha('b')), (3, sha('c'))]
}

#[test]
fn a_stack_merge_is_one_submission_at_the_target_head_and_merged_queued_and_pending_differ() {
    let cases = [
        (
            json!({"status": "merged", "details": {"sha": sha('d')}}),
            MergeSubmission::Done(PullRequestActionResult::Applied),
        ),
        (
            json!({"status": "enqueued", "details": {}}),
            MergeSubmission::Done(PullRequestActionResult::Queued { position: None }),
        ),
        (
            json!({"status": "pending", "details": {"uuid": "op-1"}}),
            MergeSubmission::Following {
                id: "op-1".into(),
                adopted: false,
                layers: vec![2, 3],
            },
        ),
        (
            json!({"status": "failed", "details": {"message": "Required checks have not passed"}}),
            MergeSubmission::Done(PullRequestActionResult::Rejected(
                PullRequestRejection::Refused {
                    messages: vec!["Required checks have not passed".into()],
                },
            )),
        ),
    ];
    for (answer, expected) in cases {
        let (submission, submitted) = merge(Stack::default(), (202, answer), &scope());
        assert_eq!(submission, expected);
        assert_eq!(
            submitted,
            vec![json!({"merge_method": "squash", "merge_action": "default", "sha": sha('c')})]
        );
    }
}

#[test]
fn a_conflict_naming_a_running_merge_is_followed_and_one_naming_none_is_refused() {
    let (adopted, submitted) = merge(
        Stack::default(),
        (
            409,
            json!({"message": "A merge request already exists", "details": {"uuid": "op-0"}}),
        ),
        &scope(),
    );
    assert_eq!(
        adopted,
        MergeSubmission::Following {
            id: "op-0".into(),
            adopted: true,
            layers: vec![2, 3],
        }
    );
    assert_eq!(submitted.len(), 1);
    let (unnamed, submitted) = merge(
        Stack::default(),
        (409, json!({"message": "A merge request already exists"})),
        &scope(),
    );
    assert_eq!(
        unnamed,
        MergeSubmission::Done(PullRequestActionResult::Rejected(
            PullRequestRejection::MergeRunning
        ))
    );
    assert_eq!(submitted.len(), 1);
}

#[test]
fn a_moved_head_a_changed_scope_or_a_blocked_layer_sends_nothing() {
    let refused = |rejection| MergeSubmission::Done(PullRequestActionResult::Rejected(rejection));
    let with = |change: fn(&mut Stack)| {
        let mut stack = Stack::default();
        change(&mut stack);
        stack
    };
    let cases = [
        (
            Stack::default(),
            vec![(2, sha('e')), (3, sha('c'))],
            refused(PullRequestRejection::LayerChanged {
                number: 2,
                expected: sha('e'),
                actual: sha('b'),
            }),
        ),
        // A layer the confirmation did not show is not merged with the others.
        (
            Stack::default(),
            vec![(3, sha('c'))],
            refused(PullRequestRejection::StackChanged),
        ),
        (
            with(|stack| stack.number = 51),
            scope(),
            refused(PullRequestRejection::StackChanged),
        ),
        (
            with(|stack| stack.layers[1].3 = true),
            scope(),
            refused(PullRequestRejection::LayerDraft { number: 2 }),
        ),
        (
            with(|stack| stack.layers[1].2 = "closed"),
            scope(),
            refused(PullRequestRejection::LayerNotOpen {
                number: 2,
                state: PullRequestState::Closed,
            }),
        ),
        (
            with(|stack| stack.layers[2].2 = "merged"),
            vec![(2, sha('b'))],
            refused(PullRequestRejection::LayerNotOpen {
                number: 3,
                state: PullRequestState::Merged,
            }),
        ),
    ];
    for (stack, heads, expected) in cases {
        let (submission, submitted) = merge(
            stack,
            (202, json!({"status": "merged", "details": {}})),
            &heads,
        );
        assert_eq!(submission, expected);
        assert!(submitted.is_empty(), "nothing is submitted: {expected:?}");
    }
}

#[test]
fn a_layer_the_account_cannot_push_to_refuses_the_rebase_before_anything_runs() {
    let (reads, _server, asked) = github(Stack::default(), |line, _| {
        if line.starts_with("POST ") && line.contains("graphql") {
            (
                200,
                json!({"data": {"repository": {
                    "pr2": {"headRepository": {"viewerPermission": "WRITE"}, "maintainerCanModify": false},
                    "pr3": {"headRepository": {"viewerPermission": "READ"}, "maintainerCanModify": false},
                }}}),
            )
        } else {
            (404, json!({"message": "Not Found"}))
        }
    });
    let plan = reads.plan_stack_rebase(
        &PullRequestKey::new("github.com", "octo/repo", 3),
        50,
        &heads(&scope()),
    );
    assert_eq!(
        plan.err(),
        Some(PullRequestRejection::NoPushAccess { numbers: vec![3] })
    );
    // No plan, so no Git runs and nothing is pushed: GitHub was only read.
    let asked = asked.lock().unwrap();
    assert!(
        asked.iter().all(|line| line.starts_with("GET ")
            || (line.starts_with("POST ") && line.contains("graphql"))),
        "{asked:?}"
    );
}

/// A bare repository standing in for GitHub, holding a three-layer stack cut from `main`, with
/// `main` moved ahead since. Each layer changes its own file, so each has one commit of its own.
struct Remote {
    _root: tempdir::Dir,
    path: std::path::PathBuf,
    work: std::path::PathBuf,
}

mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = tcode_services::process::command("git")
        .current_dir(cwd)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

impl Remote {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "tcode-stack-remote-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("remote.git");
        let work = root.join("work");
        git(
            &root,
            &["init", "--quiet", "--bare", path.to_str().unwrap()],
        );
        git(
            &root,
            &[
                "clone",
                "--quiet",
                path.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        let remote = Self {
            _root: tempdir::Dir(root),
            path,
            work,
        };
        remote.commit("README.md", "start\n", "Initial");
        git(&remote.work, &["push", "--quiet", "origin", "HEAD:main"]);
        for (layer, parent) in [
            ("layer-1", "main"),
            ("layer-2", "layer-1"),
            ("layer-3", "layer-2"),
        ] {
            git(&remote.work, &["switch", "--quiet", "-c", layer, parent]);
            remote.commit(&format!("{layer}.txt"), "one\n", &format!("{layer}: add"));
        }
        git(&remote.work, &["switch", "--quiet", "main"]);
        remote.commit("README.md", "main moved ahead\n", "Docs");
        git(
            &remote.work,
            &[
                "push", "--quiet", "origin", "main", "layer-1", "layer-2", "layer-3",
            ],
        );
        remote
    }

    fn commit(&self, file: &str, text: &str, message: &str) {
        std::fs::write(self.work.join(file), text).unwrap();
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "--quiet", "-m", message]);
    }

    fn head(&self, branch: &str) -> String {
        git(&self.path, &["rev-parse", &format!("refs/heads/{branch}")])
    }

    fn layers(&self) -> Vec<RebaseLayer> {
        (1..=3)
            .map(|number| RebaseLayer {
                number,
                branch: format!("layer-{number}"),
                head: self.head(&format!("layer-{number}")),
            })
            .collect()
    }

    fn rebase(&self, layers: &[RebaseLayer]) -> Vec<StackRebaseStep> {
        stack_rebase::cascade(
            self.path.to_str().unwrap(),
            // An extra header keyed to a filesystem remote is never sent.
            "fixture-token",
            &Identity::new("Host", "host@example.test"),
            "main",
            layers,
            |_, _| {},
        )
    }
}

#[test]
fn each_layer_moves_only_its_own_commits_onto_the_rebased_layer_below() {
    let remote = Remote::new();
    let layers = remote.layers();
    let steps = remote.rebase(&layers);
    let main = remote.head("main");
    let heads: Vec<_> = (1..=3)
        .map(|n| remote.head(&format!("layer-{n}")))
        .collect();
    assert_eq!(
        steps,
        layers
            .iter()
            .zip(&heads)
            .map(|(layer, head)| StackRebaseStep::Pushed {
                from: layer.head.clone(),
                to: head.clone(),
            })
            .collect::<Vec<_>>()
    );
    let parents = [&main, &heads[0], &heads[1]];
    for (index, head) in heads.iter().enumerate() {
        assert_eq!(
            &git(&remote.path, &["rev-parse", &format!("{head}~1")]),
            parents[index]
        );
        assert_eq!(
            git(
                &remote.path,
                &["log", "--format=%s", &format!("{}..{head}", parents[index])]
            ),
            format!("layer-{}: add", index + 1)
        );
        assert_eq!(
            git(&remote.path, &["log", "-1", "--format=%cn <%ce>", head]),
            "Host <host@example.test>"
        );
    }
    // Rebased already, the stack has nothing to move.
    let again = remote.rebase(&remote.layers());
    assert_eq!(again, vec![StackRebaseStep::AlreadyCurrent; 3]);
}

#[test]
fn the_lease_keeps_a_branch_pushed_to_since_review_and_the_layer_below_stays_pushed() {
    let remote = Remote::new();
    let reviewed = remote.layers();
    git(&remote.work, &["switch", "--quiet", "layer-2"]);
    remote.commit("layer-2.txt", "two\n", "layer-2: pushed meanwhile");
    git(&remote.work, &["push", "--quiet", "origin", "layer-2"]);
    let concurrent = remote.head("layer-2");
    let steps = remote.rebase(&reviewed);
    assert!(matches!(steps[0], StackRebaseStep::Pushed { .. }));
    assert_eq!(
        steps[1..],
        [
            StackRebaseStep::Failed {
                reason: StackRebaseFailure::LeaseRefused
            },
            StackRebaseStep::NotStarted
        ]
    );
    assert_eq!(remote.head("layer-2"), concurrent);
    assert_eq!(remote.head("layer-3"), reviewed[2].head);
    assert_eq!(
        git(
            &remote.path,
            &["rev-parse", &format!("{}~1", remote.head("layer-1"))]
        ),
        remote.head("main")
    );
}

#[test]
fn a_conflict_stops_at_the_failing_layer_untouched_with_the_lower_layers_pushed() {
    let remote = Remote::new();
    git(&remote.work, &["switch", "--quiet", "main"]);
    remote.commit("layer-2.txt", "main's own\n", "main writes layer 2's file");
    git(&remote.work, &["push", "--quiet", "origin", "main"]);
    let reviewed = remote.layers();
    let steps = remote.rebase(&reviewed);
    assert!(matches!(steps[0], StackRebaseStep::Pushed { .. }));
    assert_eq!(
        steps[1..],
        [
            StackRebaseStep::Failed {
                reason: StackRebaseFailure::Conflict
            },
            StackRebaseStep::NotStarted
        ]
    );
    assert_ne!(remote.head("layer-1"), reviewed[0].head);
    assert_eq!(remote.head("layer-2"), reviewed[1].head);
    assert_eq!(remote.head("layer-3"), reviewed[2].head);
}
