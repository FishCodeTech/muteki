//! A native stack's rebase, which GitHub has no endpoint for: every unmerged layer, bottom to
//! top, moves its own commits onto the new head of the layer below it (the bottom one onto the
//! base) in a scratch clone, and each moved branch is force-pushed only while it is still at
//! the head that was reviewed. The project's checkout is never touched.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tcode_core::pull_request::{StackRebaseFailure, StackRebaseGitStep, StackRebaseStep};

/// Each Git command of a rebase ends by this.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// One unmerged layer, bottom first, with the head that was reviewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseLayer {
    pub number: u64,
    pub branch: String,
    pub head: String,
}

/// The name and email the rebased commits are committed with: the host's own Git identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    name: String,
    email: String,
}

impl Identity {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }
}

/// The host's Git name and email, as a new repository on it would commit with.
pub fn identity() -> Option<Identity> {
    let dir = std::env::temp_dir();
    let read = |key: &str| match git(&dir, &[], &["config", "--get", key]) {
        Run::Exited {
            ok: true, stdout, ..
        } => Some(stdout.trim().to_owned()).filter(|value| !value.is_empty()),
        _ => None,
    };
    Some(Identity::new(read("user.name")?, read("user.email")?))
}

enum Run {
    Exited {
        ok: bool,
        stdout: String,
        stderr: String,
    },
    TimedOut,
    Failed(String),
}

fn git(dir: &Path, env: &[(String, String)], args: &[&str]) -> Run {
    let mut command = crate::process::command("git");
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        // The lease refusal is recognised by Git's own words, which a host locale translates.
        .env("LC_ALL", "C")
        .env("LANGUAGE", "")
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Run::Failed(error.to_string()),
    };
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = drain(child.stdout.take().map(|pipe| Box::new(pipe) as _));
    let stderr = drain(child.stderr.take().map(|pipe| Box::new(pipe) as _));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < GIT_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (stdout, stderr) = (
        stdout.join().unwrap_or_default(),
        stderr.join().unwrap_or_default(),
    );
    match status {
        Some(status) => Run::Exited {
            ok: status.success(),
            stdout,
            stderr,
        },
        None => Run::TimedOut,
    }
}

/// A directory removed with everything in it once the rebase ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Rebases `layers` of the stack on `base` at `remote`, telling `progress` each layer's step,
/// and answers every layer's final step. It stops at the first layer that fails: the scratch
/// rebase is aborted, that layer's branch is left as it was, the layers below it stay pushed
/// and those above it are not started. `token` rides only in this run's Git environment.
/// `identity` is the one the plan found before the user confirmed, passed in rather than left
/// to the scratch clone's Git, so a host without a name or email refuses the rebase before
/// anything is pushed instead of failing at the first layer's commit.
pub fn cascade(
    remote: &str,
    token: &str,
    identity: &Identity,
    base: &str,
    layers: &[RebaseLayer],
    mut progress: impl FnMut(usize, StackRebaseStep),
) -> Vec<StackRebaseStep> {
    let mut steps = vec![StackRebaseStep::Waiting; layers.len()];
    if layers.is_empty() {
        return steps;
    }
    let mut set = |steps: &mut Vec<StackRebaseStep>, index: usize, step: StackRebaseStep| {
        steps[index] = step.clone();
        progress(index, step);
    };
    let fail = |steps: &mut Vec<StackRebaseStep>,
                set: &mut dyn FnMut(&mut Vec<StackRebaseStep>, usize, StackRebaseStep),
                index: usize,
                reason: StackRebaseFailure| {
        set(steps, index, StackRebaseStep::Failed { reason });
        for above in index + 1..layers.len() {
            set(steps, above, StackRebaseStep::NotStarted);
        }
    };
    let git_failure = |step: StackRebaseGitStep, run: Run| StackRebaseFailure::Git {
        step,
        message: match run {
            Run::Exited { stderr, .. } => {
                stderr.trim().lines().last().unwrap_or_default().to_owned()
            }
            Run::TimedOut => "timed out".to_owned(),
            Run::Failed(message) => message,
        },
    };
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "tcode-stack-rebase-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )));
    if let Err(error) = std::fs::create_dir_all(&scratch.0) {
        fail(
            &mut steps,
            &mut set,
            0,
            StackRebaseFailure::Git {
                step: StackRebaseGitStep::Preparing,
                message: error.to_string(),
            },
        );
        return steps;
    }
    use base64::Engine as _;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    let config = [
        ("user.name".to_owned(), identity.name.clone()),
        ("user.email".to_owned(), identity.email.clone()),
        (
            format!("http.{remote}.extraheader"),
            format!("AUTHORIZATION: basic {basic}"),
        ),
    ];
    let mut env = vec![("GIT_CONFIG_COUNT".to_owned(), config.len().to_string())];
    for (index, (key, value)) in config.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value));
    }
    let run = |args: &[&str]| git(&scratch.0, &env, args);
    let ok = |run: &Run| matches!(run, Run::Exited { ok: true, .. });
    let output = |run: Run| match run {
        Run::Exited { stdout, .. } => stdout.trim().to_owned(),
        _ => String::new(),
    };

    for args in [
        &["init", "--quiet"][..],
        &["remote", "add", "origin", remote][..],
    ] {
        let result = run(args);
        if !ok(&result) {
            fail(
                &mut steps,
                &mut set,
                0,
                git_failure(StackRebaseGitStep::Preparing, result),
            );
            return steps;
        }
    }
    let mut refspecs = vec![format!("+refs/heads/{base}:refs/remotes/origin/{base}")];
    refspecs.extend(
        layers
            .iter()
            .map(|layer| format!("+refs/heads/{0}:refs/remotes/origin/{0}", layer.branch)),
    );
    let mut fetch = vec!["fetch", "--quiet", "--no-tags", "origin"];
    fetch.extend(refspecs.iter().map(String::as_str));
    let fetched = run(&fetch);
    if !ok(&fetched) {
        fail(
            &mut steps,
            &mut set,
            0,
            git_failure(StackRebaseGitStep::Fetching, fetched),
        );
        return steps;
    }

    let base_ref = format!("origin/{base}");
    let mut parent_old = base_ref.clone();
    let mut parent_new = base_ref;
    for (index, layer) in layers.iter().enumerate() {
        set(&mut steps, index, StackRebaseStep::Rebasing);
        // The bottom layer's own commits start where it forked from the base; above it they
        // start at the reviewed head of the layer below, whose commits it must not replay.
        let upstream = if index == 0 {
            let fork = run(&["merge-base", &parent_old, &layer.head]);
            if !ok(&fork) {
                fail(
                    &mut steps,
                    &mut set,
                    index,
                    git_failure(StackRebaseGitStep::ForkPoint, fork),
                );
                return steps;
            }
            output(fork)
        } else {
            parent_old.clone()
        };
        let checkout = run(&["checkout", "--quiet", "--detach", &layer.head]);
        if !ok(&checkout) {
            fail(
                &mut steps,
                &mut set,
                index,
                git_failure(StackRebaseGitStep::CheckingOut, checkout),
            );
            return steps;
        }
        match run(&["rebase", "--quiet", "--onto", &parent_new, &upstream]) {
            Run::Exited { ok: true, .. } => {}
            Run::Exited { ok: false, .. } => {
                let _ = run(&["rebase", "--abort"]);
                fail(&mut steps, &mut set, index, StackRebaseFailure::Conflict);
                return steps;
            }
            failed => {
                let _ = run(&["rebase", "--abort"]);
                fail(
                    &mut steps,
                    &mut set,
                    index,
                    git_failure(StackRebaseGitStep::Rebasing, failed),
                );
                return steps;
            }
        }
        let rebased = run(&["rev-parse", "HEAD"]);
        if !ok(&rebased) {
            fail(
                &mut steps,
                &mut set,
                index,
                git_failure(StackRebaseGitStep::Rebasing, rebased),
            );
            return steps;
        }
        let rebased = output(rebased);
        if rebased == layer.head {
            set(&mut steps, index, StackRebaseStep::AlreadyCurrent);
        } else {
            set(&mut steps, index, StackRebaseStep::Pushing);
            let lease = format!(
                "--force-with-lease=refs/heads/{}:{}",
                layer.branch, layer.head
            );
            let target = format!("{rebased}:refs/heads/{}", layer.branch);
            match run(&["push", "--quiet", &lease, "origin", &target]) {
                Run::Exited { ok: true, .. } => set(
                    &mut steps,
                    index,
                    StackRebaseStep::Pushed {
                        from: layer.head.clone(),
                        to: rebased.clone(),
                    },
                ),
                Run::Exited { stderr, .. } if stderr.contains("stale info") => {
                    fail(
                        &mut steps,
                        &mut set,
                        index,
                        StackRebaseFailure::LeaseRefused,
                    );
                    return steps;
                }
                Run::TimedOut => {
                    fail(
                        &mut steps,
                        &mut set,
                        index,
                        StackRebaseFailure::PushUnconfirmed,
                    );
                    return steps;
                }
                failed => {
                    fail(
                        &mut steps,
                        &mut set,
                        index,
                        git_failure(StackRebaseGitStep::Pushing, failed),
                    );
                    return steps;
                }
            }
        }
        parent_old = layer.head.clone();
        parent_new = rebased;
    }
    steps
}
