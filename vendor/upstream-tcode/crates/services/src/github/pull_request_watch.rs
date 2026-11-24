//! The reads a pull request watch makes: a batched fingerprint, the detail and the activity.

use super::{
    GitHubApi, GitHubError, RequestOptions,
    api::{Authentication, RestRequest},
    graphql::{self, AliasItem, Document, Variables},
    pull_request_reads::percent_encode,
    repository::Repository,
};
use crate::forge::{Fingerprint, Tail, Tails};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use tcode_core::{
    pull_request::{Mergeability, PullRequestKey, PullRequestState},
    pull_request_watch::{
        self as watch, CheckStatus, PullRequestCheck, PullRequestRemark, PullRequestWatchRead,
    },
};

/// Past this many requests a list is reported incomplete rather than read on.
const MAX_PAGES: usize = 10;
const FINGERPRINT_BATCH: usize = 25;

/// What a watch must notice, priced by GitHub at one point for twenty-five pull requests.
const FINGERPRINT_SELECTION: &str = "state mergeable headRefOid comments(first: 100, orderBy: { field: UPDATED_AT, direction: DESC }) { totalCount nodes { lastEditedAt } } reviews(last: 100) { totalCount nodes { lastEditedAt } } reviewThreads { totalCount } commits(last: 1) { nodes { commit { statusCheckRollup { contexts { checkRunCountsByState { state count } statusContextCountsByState { state count } } } } } }";
const REMARK: &str = "id body createdAt lastEditedAt url author { login }";

fn options(
    api: &GitHubApi,
    host: &str,
    operation: &'static str,
) -> Result<RequestOptions, GitHubError> {
    Ok(RequestOptions {
        authentication: Authentication::Pinned(api.credentials().get(host)?),
        operation,
        ..Default::default()
    })
}

fn query(
    api: &GitHubApi,
    key: &PullRequestKey,
    options: &RequestOptions,
    query: String,
    variables: BTreeMap<String, Value>,
) -> Result<Value, GitHubError> {
    let document = Document { query, variables };
    api.graphql(&key.host, &document, options)?.json()
}

fn repository_variables(key: &PullRequestKey) -> Result<BTreeMap<String, Value>, GitHubError> {
    let repository = Repository::from_key(key).ok_or(GitHubError::Request)?;
    Ok(BTreeMap::from([
        ("owner".into(), json!(repository.owner)),
        ("name".into(), json!(repository.name)),
        ("number".into(), json!(key.number)),
    ]))
}

/// One fingerprint per key, read in batches of twenty-five per host. `Ok(None)` is a pull
/// request the host gave no fingerprint for; it takes the reads gated by its sync snapshot.
pub fn fingerprints(
    api: &GitHubApi,
    keys: &[PullRequestKey],
) -> Vec<Result<Option<Fingerprint>, GitHubError>> {
    let mut results: Vec<_> = keys.iter().map(|_| Ok(None)).collect();
    let mut by_host: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut positions: HashMap<&PullRequestKey, Vec<usize>> = HashMap::new();
    for (index, key) in keys.iter().enumerate() {
        let copies = positions.entry(key).or_default();
        if copies.is_empty() {
            by_host.entry(key.host.as_str()).or_default().push(index);
        }
        copies.push(index);
    }
    for (host, indexes) in by_host {
        for chunk in indexes.chunks(FINGERPRINT_BATCH) {
            let read = read_fingerprints(api, host, chunk.iter().map(|index| &keys[*index]));
            for (position, index) in chunk.iter().enumerate() {
                for index in &positions[&keys[*index]] {
                    results[*index] = match &read {
                        Ok(rows) => Ok(rows.get(&position).cloned()),
                        Err(error) => Err(error.clone()),
                    };
                }
            }
        }
    }
    results
}

fn read_fingerprints<'a>(
    api: &GitHubApi,
    host: &str,
    keys: impl Iterator<Item = &'a PullRequestKey>,
) -> Result<HashMap<usize, Fingerprint>, GitHubError> {
    let items: Option<Vec<_>> = keys
        .enumerate()
        .map(|(index, key)| {
            let repository = Repository::from_key(key)?;
            Some(AliasItem {
                key: index,
                variables: Variables::from([
                    ("owner".into(), ("String!".into(), json!(repository.owner))),
                    ("name".into(), ("String!".into(), json!(repository.name))),
                    ("number".into(), ("Int!".into(), json!(key.number))),
                ]),
            })
        })
        .collect();
    let items = items.ok_or(GitHubError::Request)?;
    let selection = if items.len() > 1 {
        "...WatchFingerprint"
    } else {
        FINGERPRINT_SELECTION
    };
    let mut document = graphql::aliases(
        "query",
        "PullRequestWatchFingerprints",
        "w",
        &items,
        &Variables::new(),
        |v| {
            format!(
                "repository(owner: {}, name: {}) {{ pullRequest(number: {}) {{ {selection} }} }}",
                v["owner"], v["name"], v["number"]
            )
        },
        |fields| fields,
    )
    .ok_or(GitHubError::Request)?;
    if items.len() > 1 {
        document.query.push_str(&format!(
            "\nfragment WatchFingerprint on PullRequest {{ {FINGERPRINT_SELECTION} }}"
        ));
    }
    let response: Value = api
        .graphql(
            host,
            &document,
            &options(api, host, "PullRequestWatchFingerprints")?,
        )?
        .json()?;
    let data = response["data"]
        .as_object()
        .ok_or(GitHubError::InvalidResponse)?;
    Ok(data
        .iter()
        .filter_map(|(alias, value)| {
            let index = alias.strip_prefix('w')?.parse().ok()?;
            Some((index, decode_fingerprint(&value["pullRequest"])?))
        })
        .collect())
}

fn decode_fingerprint(pr: &Value) -> Option<Fingerprint> {
    // An edit only ever moves `lastEditedAt` forward, so the newest one stands for them all.
    let newest_edit = |connection: &Value| -> Option<String> {
        Some(
            connection["nodes"]
                .as_array()?
                .iter()
                .filter_map(|node| node["lastEditedAt"].as_str())
                .max()
                .unwrap_or_default()
                .to_owned(),
        )
    };
    let state_counts = |counts: &Value| -> String {
        let mut parts: Vec<_> = counts
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| Some((row["state"].as_str()?, row["count"].as_i64()?)))
            .filter(|(_, count)| *count > 0)
            .map(|(state, count)| format!("{state}:{count}"))
            .collect();
        parts.sort();
        parts.join(",")
    };
    let contexts = &pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
    let count = |connection: &Value| connection["totalCount"].as_i64();
    Some(Fingerprint {
        status: [
            pr["state"].as_str()?.to_owned(),
            pr["mergeable"].as_str().unwrap_or_default().to_owned(),
            pr["headRefOid"].as_str()?.to_owned(),
            state_counts(&contexts["checkRunCountsByState"]),
            state_counts(&contexts["statusContextCountsByState"]),
        ]
        .join(" "),
        remarks: [
            count(&pr["comments"])?.to_string(),
            newest_edit(&pr["comments"])?,
            count(&pr["reviews"])?.to_string(),
            newest_edit(&pr["reviews"])?,
            count(&pr["reviewThreads"])?.to_string(),
        ]
        .join(" "),
    })
}

/// State, head, checks, mergeability and the authenticated account, in one point.
pub fn detail(api: &GitHubApi, key: &PullRequestKey) -> Result<PullRequestWatchRead, GitHubError> {
    let options = options(api, &key.host, "PullRequestWatchDetail")?;
    // GitHub Enterprise Server has no per-pull-request required flag, so nothing there is known
    // to be required.
    let github_com = key.host == "github.com";
    let (required, rules) = if github_com {
        (
            " isRequired(pullRequestNumber: $number)",
            " baseRef { rules(first: 100) { nodes { parameters { ... on RequiredStatusChecksParameters { requiredStatusChecks { context } } } } } }",
        )
    } else {
        ("", "")
    };
    let document = format!(
        "query PullRequestWatchDetail($owner: String!, $name: String!, $number: Int!, $checksAfter: String) {{ viewer {{ login }} repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ number state mergedAt mergeable headRefOid baseRefName author {{ login }}{rules} commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ contexts(first: 100, after: $checksAfter) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ __typename ... on StatusContext {{ context state targetUrl createdAt{required} }} ... on CheckRun {{ name status conclusion startedAt completedAt detailsUrl{required} checkSuite {{ workflowRun {{ workflow {{ name }} }} }} }} }} }} }} }} }} }} }} }} }}"
    );
    let mut variables = repository_variables(key)?;
    let mut contexts = Vec::new();
    let mut first: Option<Value> = None;
    let mut seen = HashSet::new();
    let mut complete = false;
    for _ in 0..MAX_PAGES {
        let response = query(api, key, &options, document.clone(), variables.clone())?;
        let pr = &response["data"]["repository"]["pullRequest"];
        if pr.is_null() {
            return Err(GitHubError::NotFound);
        }
        if pr["number"].as_u64() != Some(key.number) {
            return Err(GitHubError::InvalidResponse);
        }
        // A page of a newer head would mix two commits' checks.
        if let Some(first) = &first
            && first["repository"]["pullRequest"]["headRefOid"] != pr["headRefOid"]
        {
            return Err(GitHubError::InvalidResponse);
        }
        let page = &pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"];
        contexts.extend(page["nodes"].as_array().into_iter().flatten().cloned());
        let next = page["pageInfo"]["endCursor"]
            .as_str()
            .filter(|_| page["pageInfo"]["hasNextPage"].as_bool() == Some(true))
            .map(str::to_owned);
        first.get_or_insert_with(|| response["data"].clone());
        match next {
            Some(cursor) if seen.insert(cursor.clone()) => {
                variables.insert("checksAfter".into(), json!(cursor));
            }
            Some(_) => break,
            None => {
                complete = true;
                break;
            }
        }
    }
    let data = first.ok_or(GitHubError::InvalidResponse)?;
    let pr = &data["repository"]["pullRequest"];
    let state = if pr["mergedAt"].is_string() {
        PullRequestState::Merged
    } else {
        match pr["state"].as_str() {
            Some("OPEN") => PullRequestState::Open,
            Some("CLOSED") => PullRequestState::Closed,
            Some("MERGED") => PullRequestState::Merged,
            _ => return Err(GitHubError::InvalidResponse),
        }
    };
    let base_branch = pr["baseRefName"].as_str().unwrap_or_default().to_owned();
    // A list cut short could hide a pending check behind a passing gate; an empty one keeps
    // what the agent was told.
    let mut checks = if complete {
        checks(&contexts)
    } else {
        Vec::new()
    };
    if github_com && state == PullRequestState::Open && watch::checks_pass(&checks) {
        checks.extend(unreported_required(
            api,
            key,
            &base_branch,
            &pr["baseRef"],
            &contexts,
        )?);
    }
    let login = |value: &Value| value["login"].as_str().map(str::to_owned);
    Ok(PullRequestWatchRead {
        state,
        head_sha: pr["headRefOid"].as_str().map(str::to_owned),
        base_branch,
        checks,
        mergeability: match pr["mergeable"].as_str() {
            Some("MERGEABLE") => Mergeability::Clean,
            Some("CONFLICTING") => Mergeability::Conflicting,
            // GitHub's word for a computation it has not finished.
            Some("UNKNOWN") => Mergeability::Pending,
            _ => Mergeability::Unknown,
        },
        viewer: login(&data["viewer"]),
        author: login(&pr["author"]),
    })
}

/// The checks the base branch requires that have not reported on the head yet, as pending.
/// GitHub creates a job that waits on others only once they finish, so until then the checks
/// read do not show it. Rulesets come with the detail; branch protection is readable through
/// REST alone, and a token that may not read it leaves those checks unknown.
fn unreported_required(
    api: &GitHubApi,
    key: &PullRequestKey,
    base_branch: &str,
    base_ref: &Value,
    contexts: &[Value],
) -> Result<Vec<PullRequestCheck>, GitHubError> {
    let repository = Repository::from_key(key).ok_or(GitHubError::Request)?;
    let path = format!(
        "/repos/{}/{}/branches/{}",
        repository.owner,
        repository.name,
        percent_encode(base_branch)
    );
    let branch: Value = match api.rest(
        &key.host,
        RestRequest {
            answers: &[403],
            ..RestRequest::get(&path)
        },
        &options(api, &key.host, "PullRequestWatchRequiredChecks")?,
    ) {
        Ok(response) if response.status == 403 => Value::Null,
        Ok(response) => response.json()?,
        Err(GitHubError::NotFound) => Value::Null,
        Err(error) => return Err(error),
    };
    let reported: HashSet<String> = contexts
        .iter()
        .filter_map(|raw| trimmed(&raw["name"]).or_else(|| trimmed(&raw["context"])))
        .collect();
    let mut expected: Vec<String> = Vec::new();
    let protection = &branch["protection"];
    let protected = protection["required_status_checks"]["contexts"]
        .as_array()
        .filter(|_| protection["enabled"].as_bool() != Some(false))
        .into_iter()
        .flatten()
        .filter_map(trimmed);
    let ruled = base_ref["rules"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|rule| {
            rule["parameters"]["requiredStatusChecks"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|check| trimmed(&check["context"]));
    for context in protected.chain(ruled) {
        if !reported.contains(&context) && !expected.contains(&context) {
            expected.push(context);
        }
    }
    Ok(expected
        .into_iter()
        .map(|name| PullRequestCheck {
            name,
            status: CheckStatus::Pending,
            url: None,
            required: Some(true),
        })
        .collect())
}

fn trimmed(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn check_status(raw: &Value) -> CheckStatus {
    // Commit statuses report one `state`; check runs report `status` plus a `conclusion`
    // that only exists once the run has completed.
    if raw["status"]
        .as_str()
        .map(|status| status.trim().to_ascii_uppercase())
        .is_some_and(|status| !status.is_empty() && status != "COMPLETED")
    {
        return CheckStatus::Pending;
    }
    let verdict = raw["conclusion"]
        .as_str()
        .or_else(|| raw["state"].as_str())
        .map(|value| value.trim().to_ascii_uppercase());
    match verdict.as_deref() {
        Some("SUCCESS") => CheckStatus::Success,
        Some("ACTION_REQUIRED") => CheckStatus::ActionRequired,
        Some("FAILURE" | "ERROR" | "TIMED_OUT" | "STARTUP_FAILURE") => CheckStatus::Failure,
        Some("CANCELLED") => CheckStatus::Cancelled,
        Some("SKIPPED") => CheckStatus::Skipped,
        Some("PENDING" | "EXPECTED") => CheckStatus::Pending,
        _ => CheckStatus::Neutral,
    }
}

/// One check per name and workflow, keeping its newest run: a rerun replaces the run before it
/// where it stood. Survivors sharing a name across workflows are written `workflow / name`.
pub(super) fn checks(contexts: &[Value]) -> Vec<PullRequestCheck> {
    let at = |raw: &Value, field: &str| {
        trimmed(&raw[field]).filter(|value| value != "0001-01-01T00:00:00Z")
    };
    let mut order: Vec<(String, PullRequestCheck, Option<String>)> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    for raw in contexts {
        let Some(name) = trimmed(&raw["name"]).or_else(|| trimmed(&raw["context"])) else {
            continue;
        };
        let workflow = trimmed(&raw["checkSuite"]["workflowRun"]["workflow"]["name"]);
        let when = at(raw, "completedAt").or_else(|| at(raw, "startedAt"));
        let check = PullRequestCheck {
            name: name.clone(),
            status: check_status(raw),
            url: trimmed(&raw["detailsUrl"]).or_else(|| trimmed(&raw["targetUrl"])),
            required: raw["isRequired"].as_bool(),
        };
        let key = format!("{} {name}", workflow.clone().unwrap_or_default());
        match positions.get(&key) {
            Some(&position) => {
                let kept = &order[position].2;
                let newer = match (&when, kept) {
                    (None, kept) => kept.is_none(),
                    (Some(_), None) => true,
                    (Some(when), Some(kept)) => when >= kept,
                };
                if newer {
                    order[position] = (workflow.unwrap_or_default(), check, when);
                }
            }
            None => {
                positions.insert(key, order.len());
                order.push((workflow.unwrap_or_default(), check, when));
            }
        }
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (_, check, _) in &order {
        *counts.entry(check.name.clone()).or_default() += 1;
    }
    order
        .into_iter()
        .map(|(workflow, mut check, _)| {
            if !workflow.is_empty() && counts[&check.name] > 1 {
                check.name = format!("{workflow} / {}", check.name);
            }
            check
        })
        .collect()
}

fn remark(raw: &Value, path: Option<&str>) -> Option<PullRequestRemark> {
    Some(PullRequestRemark {
        id: raw["id"].as_str()?.to_owned(),
        author: raw["author"]["login"].as_str().map(str::to_owned),
        body: raw["body"].as_str().unwrap_or_default().to_owned(),
        created_at: raw["createdAt"].as_str()?.to_owned(),
        edited_at: raw["lastEditedAt"].as_str().map(str::to_owned),
        url: trimmed(&raw["url"]),
        path: path.map(str::to_owned),
        review_state: None,
    })
}

/// A review with no body is kept only when its state is the event itself. GitHub also opens a
/// bodiless `COMMENTED` review around line comments, which are read from the review threads.
fn review(raw: &Value) -> Option<PullRequestRemark> {
    let state = trimmed(&raw["state"]);
    let verdict = state.as_deref().is_some_and(|state| {
        matches!(
            state.to_ascii_uppercase().as_str(),
            "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
        )
    });
    if raw["body"].as_str().unwrap_or_default().trim().is_empty() && !verdict {
        return None;
    }
    Some(PullRequestRemark {
        id: raw["id"].as_str()?.to_owned(),
        author: raw["author"]["login"].as_str().map(str::to_owned),
        body: raw["body"].as_str().unwrap_or_default().to_owned(),
        created_at: trimmed(&raw["submittedAt"])?,
        edited_at: raw["lastEditedAt"].as_str().map(str::to_owned),
        url: trimmed(&raw["url"]),
        path: None,
        review_state: state,
    })
}

fn next_cursor(connection: &Value) -> Option<String> {
    connection["pageInfo"]["endCursor"]
        .as_str()
        .filter(|_| connection["pageInfo"]["hasNextPage"].as_bool() == Some(true))
        .map(str::to_owned)
}

/// Every remark on the pull request, or `Ok(None)` when one could be missing: a list longer
/// than ten pages, or a review thread whose replies could not all be read. Such a read never
/// advances the watermark.
pub fn activity(
    api: &GitHubApi,
    key: &PullRequestKey,
    tails: &mut Tails,
) -> Result<Option<Vec<PullRequestRemark>>, GitHubError> {
    let options = options(api, &key.host, "PullRequestWatchActivity")?;
    let base = repository_variables(key)?;
    let mut remarks = Vec::new();
    let mut complete = true;

    let mut variables = base.clone();
    let (mut comments, mut reviews) = (true, true);
    let (mut comments_after, mut reviews_after) = (Value::Null, Value::Null);
    for _ in 0..MAX_PAGES {
        variables.extend([
            ("withComments".into(), json!(comments)),
            ("commentsAfter".into(), comments_after.clone()),
            ("withReviews".into(), json!(reviews)),
            ("reviewsAfter".into(), reviews_after.clone()),
        ]);
        let response = query(
            api,
            key,
            &options,
            format!(
                "query PullRequestWatchActivity($owner: String!, $name: String!, $number: Int!, $withComments: Boolean!, $commentsAfter: String, $withReviews: Boolean!, $reviewsAfter: String) {{ repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ comments(first: 100, after: $commentsAfter) @include(if: $withComments) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {REMARK} }} }} reviews(first: 100, after: $reviewsAfter) @include(if: $withReviews) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {REMARK} state submittedAt }} }} }} }} }}"
            ),
            variables.clone(),
        )?;
        let pr = &response["data"]["repository"]["pullRequest"];
        if pr.is_null() {
            return Err(GitHubError::NotFound);
        }
        if comments {
            remarks.extend(
                pr["comments"]["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|raw| remark(raw, None)),
            );
            comments_after = next_cursor(&pr["comments"]).map_or(Value::Null, Value::String);
            comments = !comments_after.is_null();
        }
        if reviews {
            remarks.extend(
                pr["reviews"]["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(review),
            );
            reviews_after = next_cursor(&pr["reviews"]).map_or(Value::Null, Value::String);
            reviews = !reviews_after.is_null();
        }
        if !comments && !reviews {
            break;
        }
    }
    complete &= !comments && !reviews;

    let mut threads = Vec::new();
    let mut cursor = Value::Null;
    let mut pages = 0;
    loop {
        let mut variables = base.clone();
        variables.insert("cursor".into(), cursor.clone());
        let response = query(
            api,
            key,
            &options,
            format!(
                "query PullRequestWatchThreads($owner: String!, $name: String!, $number: Int!, $cursor: String) {{ repository(owner: $owner, name: $name) {{ pullRequest(number: $number) {{ reviewThreads(first: 100, after: $cursor) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ id path comments(first: 10) {{ totalCount pageInfo {{ hasNextPage endCursor }} nodes {{ {REMARK} }} }} }} }} }} }} }}"
            ),
            variables,
        )?;
        let connection = &response["data"]["repository"]["pullRequest"]["reviewThreads"];
        threads.extend(
            connection["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
        pages += 1;
        match next_cursor(connection) {
            Some(next) if pages < MAX_PAGES => cursor = Value::String(next),
            Some(_) => {
                complete = false;
                break;
            }
            None => break,
        }
    }

    let mut live = HashSet::new();
    for thread in &threads {
        let (Some(id), path) = (thread["id"].as_str(), thread["path"].as_str()) else {
            continue;
        };
        let first_page: Vec<_> = thread["comments"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|raw| remark(raw, path))
            .collect();
        let count = thread["comments"]["totalCount"].as_u64().unwrap_or(0);
        remarks.extend(first_page.iter().cloned());
        let Some(cursor) = next_cursor(&thread["comments"]) else {
            continue;
        };
        live.insert(id.to_owned());
        let cached = tails.get(id).filter(|tail| tail.count == count);
        let tail = match cached {
            Some(tail) => tail.comments.clone(),
            None => match read_tail(api, key, &options, id, cursor, path) {
                Some(comments) if first_page.len() + comments.len() >= count as usize => {
                    tails.insert(
                        id.to_owned(),
                        Tail {
                            count,
                            comments: comments.clone(),
                        },
                    );
                    comments
                }
                _ => {
                    complete = false;
                    continue;
                }
            },
        };
        remarks.extend(tail);
    }
    tails.retain(|id, _| live.contains(id));
    remarks.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    Ok(complete.then_some(remarks))
}

/// A thread's replies after its first page, deduplicated by id; `None` when they could not all
/// be read.
fn read_tail(
    api: &GitHubApi,
    key: &PullRequestKey,
    options: &RequestOptions,
    thread: &str,
    cursor: String,
    path: Option<&str>,
) -> Option<Vec<PullRequestRemark>> {
    let mut comments: Vec<PullRequestRemark> = Vec::new();
    let mut ids = HashSet::new();
    let mut cursors = HashSet::new();
    let mut cursor = Some(cursor);
    while let Some(after) = cursor.take() {
        if !cursors.insert(after.clone()) || cursors.len() > MAX_PAGES {
            return None;
        }
        let response = query(
            api,
            key,
            options,
            format!(
                "query PullRequestWatchThreadComments($thread: ID!, $cursor: String) {{ node(id: $thread) {{ ... on PullRequestReviewThread {{ comments(first: 100, after: $cursor) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ {REMARK} }} }} }} }} }}"
            ),
            BTreeMap::from([
                ("thread".into(), json!(thread)),
                ("cursor".into(), json!(after)),
            ]),
        );
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                log::warn!("pull request watch comment pagination failed: {error}");
                return None;
            }
        };
        let page = &response["data"]["node"]["comments"];
        for comment in page["nodes"].as_array().into_iter().flatten() {
            if let Some(comment) = remark(comment, path)
                && ids.insert(comment.id.clone())
            {
                comments.push(comment);
            }
        }
        cursor = next_cursor(page);
    }
    Some(comments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_keep_the_newest_run_and_name_twins_by_workflow() {
        let run = |workflow: &str, name: &str, conclusion: &str, completed: &str| {
            json!({
                "__typename": "CheckRun",
                "name": name,
                "status": "COMPLETED",
                "conclusion": conclusion,
                "completedAt": completed,
                "checkSuite": {"workflowRun": {"workflow": {"name": workflow}}},
            })
        };
        let contexts = [
            run("CI", "test", "FAILURE", "2026-10-08T10:00:00Z"),
            run("Release", "test", "SUCCESS", "2026-10-08T10:00:00Z"),
            run("CI", "test", "SUCCESS", "2026-10-08T10:05:00Z"),
            json!({"__typename": "CheckRun", "name": "lint", "status": "IN_PROGRESS", "conclusion": "FAILURE"}),
            json!({"__typename": "StatusContext", "context": "deploy", "state": "ERROR", "targetUrl": "https://ci.test"}),
            json!({"__typename": "StatusContext", "state": "SUCCESS"}),
        ];
        let checks = checks(&contexts);
        assert_eq!(
            checks
                .iter()
                .map(|check| (check.name.as_str(), check.status))
                .collect::<Vec<_>>(),
            vec![
                ("CI / test", CheckStatus::Success),
                ("Release / test", CheckStatus::Success),
                ("lint", CheckStatus::Pending),
                ("deploy", CheckStatus::Failure),
            ],
            "a rerun replaces the failed run where it stood"
        );
    }
}
