//! A checkout's repository on a host, from its git remotes, for hosts whose remote URLs each
//! kind reads in its own way.

use super::Repository;
use std::cell::OnceCell;
use std::path::Path;

/// One repository lookup, sharing the fetch remotes across host implementations.
/// A new lookup reads again so remote edits and a later `git init` are visible.
pub struct Checkout<'a> {
    cwd: &'a Path,
    remotes: OnceCell<Vec<(String, String)>>,
}

impl<'a> Checkout<'a> {
    pub fn new(cwd: &'a Path) -> Self {
        Self {
            cwd,
            remotes: OnceCell::new(),
        }
    }

    pub(crate) fn cwd(&self) -> &Path {
        self.cwd
    }

    pub(crate) fn remotes(&self) -> &[(String, String)] {
        self.remotes.get_or_init(|| {
            git(self.cwd, &["remote", "-v"])
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let mut parts = line.split_whitespace();
                    let name = parts.next()?;
                    let url = parts.next()?;
                    (parts.next()? == "(fetch)").then(|| (name.to_owned(), url.to_owned()))
                })
                .collect()
        })
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    crate::github::repository::git_read(cwd, args)
}

/// The fetch remotes of the checkout that `remote` reads as a repository, by remote name.
pub(crate) fn remotes<T>(
    checkout: &Checkout<'_>,
    remote: impl Fn(&str) -> Option<T>,
) -> Vec<(String, T)> {
    checkout
        .remotes()
        .iter()
        .filter_map(|(name, url)| Some((name.clone(), remote(url)?)))
        .collect()
}

/// The checkout's repository: `upstream`, then `origin`, then the first remote `remote` reads.
pub(crate) fn resolve(
    checkout: &Checkout<'_>,
    remote: impl Fn(&str) -> Option<Repository>,
) -> Option<Repository> {
    let remotes = remotes(checkout, remote);
    ["upstream", "origin"]
        .iter()
        .find_map(|name| remotes.iter().find(|(remote, _)| remote == name))
        .or_else(|| remotes.first())
        .map(|(_, repository)| repository.clone())
}
