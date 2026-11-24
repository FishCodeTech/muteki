//! Viewed marks Tcode keeps for hosts that have none it can read, on the machine, so every
//! device connected to it sees the same marks. A mark holds the file's revision when it was
//! viewed; a file at another revision now was viewed before a later push changed it.

use std::{collections::BTreeMap, path::PathBuf, sync::Mutex};
use tcode_core::pull_request::PullRequestKey;
use tcode_protocol::PullRequestViewedState;

/// Marks by account, then pull request, then path, each with the revision it was viewed at.
type Marks = BTreeMap<String, BTreeMap<String, BTreeMap<String, String>>>;

/// Each changed path's blob at the head side, from the diff's `index` lines.
pub(crate) fn revisions(diff: &str) -> BTreeMap<String, String> {
    let mut revisions = BTreeMap::new();
    let mut path: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            path = rest.rsplit_once(" b/").map(|(_, path)| path.to_owned());
        } else if let Some(renamed) = line.strip_prefix("rename to ") {
            path = Some(renamed.to_owned());
        } else if let (Some(range), Some(path)) = (line.strip_prefix("index "), &path) {
            let range = range.split_whitespace().next().unwrap_or_default();
            if let Some((_, head)) = range.split_once("..") {
                revisions.insert(path.clone(), head.to_owned());
            }
        }
    }
    revisions
}

pub(crate) struct ViewedMarks {
    path: PathBuf,
    marks: Mutex<Option<Marks>>,
}

fn pull_request(key: &PullRequestKey) -> String {
    format!("{}/{}#{}", key.host, key.repository, key.number)
}

impl ViewedMarks {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            marks: Mutex::new(None),
        }
    }

    fn with<T>(&self, change: impl FnOnce(&mut Marks) -> T) -> T {
        let mut marks = self.marks.lock().unwrap();
        let marks = marks.get_or_insert_with(|| {
            std::fs::read(&self.path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default()
        });
        change(marks)
    }

    /// Each current file's state, given the revision each path is at now.
    pub(crate) fn states(
        &self,
        account: &str,
        key: &PullRequestKey,
        revisions: &BTreeMap<String, String>,
    ) -> Vec<(String, PullRequestViewedState)> {
        self.with(|marks| {
            let held = marks
                .get(account)
                .and_then(|pulls| pulls.get(&pull_request(key)));
            revisions
                .iter()
                .map(|(path, revision)| {
                    let state = match held.and_then(|held| held.get(path)) {
                        Some(viewed) if viewed == revision => PullRequestViewedState::Viewed,
                        Some(_) => PullRequestViewedState::Dismissed,
                        None => PullRequestViewedState::Unviewed,
                    };
                    (path.clone(), state)
                })
                .collect()
        })
    }

    pub(crate) fn set(
        &self,
        account: &str,
        key: &PullRequestKey,
        revisions: &BTreeMap<String, String>,
        paths: &[String],
        viewed: bool,
    ) -> std::io::Result<()> {
        self.with(|marks| {
            let pulls = marks.entry(account.to_owned()).or_default();
            let held = pulls.entry(pull_request(key)).or_default();
            for path in paths {
                match revisions.get(path).filter(|_| viewed) {
                    Some(revision) => {
                        held.insert(path.clone(), revision.clone());
                    }
                    None => {
                        held.remove(path);
                    }
                }
            }
            if held.is_empty() {
                pulls.remove(&pull_request(key));
            }
            if pulls.is_empty() {
                marks.remove(account);
            }
            let bytes = serde_json::to_vec(marks).map_err(std::io::Error::other)?;
            let partial = self.path.with_extension("json.tmp");
            std::fs::write(&partial, bytes)?;
            std::fs::rename(partial, &self.path)
        })
    }
}
