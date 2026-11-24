//! GitHub pull requests explicitly requested by the workspace operator.
use crate::{
    AppState,
    git_api::{self, PushBody},
    git_process::{Failure, WRITES, failure, redact, run},
    workspace_read_api::{bound_root, inspect_git_status},
};
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use reqwest::{Client, Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::Path as FsPath,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
type Api = Result<Json<Value>, Failure>;
const ERROR: &str = "conversation.git.error";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/threads/{id}/pull-requests", get(list))
        .route("/api/threads/{id}/git/pull-request", post(create))
}

#[derive(Clone)]
struct GitHub {
    client: Client,
    #[cfg(test)]
    endpoint: Option<Url>,
    #[cfg(test)]
    credential: Option<String>,
}
impl GitHub {
    fn production() -> Self {
        Self {
            client: crate::network_proxy::http_client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("GitHub HTTP client configuration"),
            #[cfg(test)]
            endpoint: None,
            #[cfg(test)]
            credential: None,
        }
    }
    fn select(injected: Option<Extension<Arc<Self>>>) -> Arc<Self> {
        injected
            .map(|v| v.0)
            .unwrap_or_else(|| Arc::new(Self::production()))
    }
    fn url(&self, owner: &str, repo: &str, pulls: bool) -> Url {
        #[cfg(test)]
        let mut url = self
            .endpoint
            .clone()
            .unwrap_or_else(|| Url::parse("https://api.github.com").unwrap());
        #[cfg(not(test))]
        let mut url = Url::parse("https://api.github.com").unwrap();
        {
            let mut path = url.path_segments_mut().expect("GitHub API is a base URL");
            path.clear().extend(["repos", owner, repo]);
            if pulls {
                path.push("pulls");
            }
        }
        url
    }
    async fn token(&self) -> String {
        #[cfg(test)]
        if let Some(token) = &self.credential {
            return token.clone();
        }
        for name in ["MUTEKI_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
            if let Ok(token) = std::env::var(name) {
                if !token.trim().is_empty() {
                    return token.trim().to_owned();
                }
            }
        }
        let mut command = crate::host_process::command("gh");
        command
            .args(["auth", "token", "--hostname", "github.com"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(5), command.output()).await {
            Ok(Ok(output)) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            }
            _ => String::new(),
        }
    }
    async fn request(
        &self,
        method: Method,
        url: Url,
        token: &str,
        payload: Option<Value>,
        seconds: u64,
    ) -> Result<Value, Failure> {
        let mut request = self
            .client
            .request(method, url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "muteki/c15")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(Duration::from_secs(seconds));
        if !token.is_empty() {
            request = request.bearer_auth(token);
        }
        if let Some(payload) = payload {
            request = request.json(&payload);
        }
        let response = request.send().await.map_err(|error| {
            github_error(
                StatusCode::BAD_GATEWAY,
                "conversation.github.api_failed",
                if error.is_timeout() {
                    "GitHub API request timed out"
                } else {
                    "Could not connect to GitHub API"
                },
                "",
            )
        })?;
        let status = response.status();
        let raw = response.text().await.map_err(|_| {
            github_error(
                StatusCode::BAD_GATEWAY,
                "conversation.github.api_failed",
                "Could not read GitHub API response",
                "",
            )
        })?;
        let parsed = serde_json::from_str::<Value>(&raw);
        if !status.is_success() {
            let mut details = Vec::new();
            if let Ok(value) = &parsed {
                if let Some(message) = value["message"].as_str() {
                    details.push(message.to_owned());
                }
                if let Some(errors) = value["errors"].as_array() {
                    for item in errors {
                        if let Some(message) = item["message"].as_str().or_else(|| item.as_str()) {
                            details.push(message.to_owned());
                        } else if item.is_object() {
                            details.push(
                                ["resource", "field", "code"]
                                    .iter()
                                    .filter_map(|key| item[*key].as_str())
                                    .collect::<Vec<_>>()
                                    .join(" "),
                            );
                        }
                    }
                }
            }
            let detail = details.join("; ");
            let detail = if token.is_empty() {
                detail
            } else {
                detail.replace(token, "[redacted]")
            };
            let message = format!(
                "GitHub API HTTP {}{}{}",
                status.as_u16(),
                if detail.is_empty() { "" } else { ": " },
                detail
            );
            let (status, code) = match status.as_u16() {
                422 => (StatusCode::CONFLICT, "conversation.github.pr_rejected"),
                401 | 403 | 404 => (StatusCode::FORBIDDEN, "conversation.github.forbidden"),
                _ => (StatusCode::BAD_GATEWAY, "conversation.github.api_failed"),
            };
            return Err(github_error(status, code, &message, ""));
        }
        parsed.map_err(|_| {
            github_error(
                StatusCode::BAD_GATEWAY,
                "conversation.github.api_failed",
                "GitHub API returned invalid JSON",
                "",
            )
        })
    }
}
fn github_error(status: StatusCode, code: &str, message: &str, hint: &str) -> Failure {
    let message = redact(message);
    (
        status,
        Json(
            json!({"detail":message,"error":{"code":code,"message":message,"recovery_hint":hint}}),
        ),
    )
}
#[derive(Debug, Default, PartialEq)]
struct Remote {
    url: String,
    owner: String,
    repo: String,
}
impl Remote {
    fn github(&self) -> bool {
        !self.owner.is_empty() && !self.repo.is_empty()
    }
}
fn parse_remote(raw: &str) -> Remote {
    let mut result = Remote {
        url: redact(raw.trim()),
        ..Remote::default()
    };
    let path = if let Some(path) = raw.trim().strip_prefix("git@github.com:") {
        Some(path.to_owned())
    } else {
        Url::parse(raw.trim())
            .ok()
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str() == Some("github.com")
                    && url.port().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            })
            .map(|url| url.path().trim_start_matches('/').to_owned())
    };
    if let Some(path) = path {
        if let Some((owner, repo)) = path.split_once('/') {
            let repo = repo.strip_suffix(".git").unwrap_or(repo);
            if !owner.is_empty()
                && !repo.is_empty()
                && !repo.contains('/')
                && !matches!(owner, "." | "..")
                && !matches!(repo, "." | "..")
            {
                result.owner = owner.to_owned();
                result.repo = repo.to_owned();
            }
        }
    }
    result
}
async fn remote(root: &FsPath, name: &str) -> Result<Remote, Failure> {
    let output = run(root, &["remote", "get-url", "--", name], 30, ERROR, false).await?;
    Ok(if output.status.success() {
        parse_remote(output.stdout.trim())
    } else {
        Remote::default()
    })
}
fn listing(state: &str, branch: Option<&str>, remote: &Remote) -> Value {
    json!({"state":state,"branch":branch,"remote_url":remote.url,"owner":remote.owner,"repo":remote.repo,"pull_requests":[]})
}
fn pr_state(pr: &Value) -> Value {
    if !pr["merged_at"].is_null() {
        json!("merged")
    } else if pr["draft"] == true {
        json!("draft")
    } else {
        pr["state"].clone()
    }
}
fn workspace(s: &AppState, id: &str) -> Result<Option<Value>, Failure> {
    let thread = s
        .store
        .get("conversation_threads", id)
        .map_err(|_| failure(ERROR, "Could not read thread"))?
        .ok_or_else(|| {
            github_error(
                StatusCode::NOT_FOUND,
                "conversation.thread.not_found",
                "Thread not found",
                "",
            )
        })?;
    let wid = thread["thread"]["workspace_id"].as_str().unwrap_or("");
    s.store
        .get("workspaces", wid)
        .map_err(|_| failure(ERROR, "Could not read workspace"))
}
async fn list(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    injected: Option<Extension<Arc<GitHub>>>,
) -> Api {
    let workspace = workspace(&s, &id)?;
    if workspace
        .as_ref()
        .and_then(|w| w["root_path"].as_str())
        .is_none_or(|root| root.trim().is_empty())
    {
        return Ok(Json(listing("no_workspace", None, &Remote::default())));
    }
    let root = match bound_root(&s, &id) {
        Ok(root) => root,
        Err(_) => return Ok(Json(listing("no_git", None, &Remote::default()))),
    };
    let status = inspect_git_status(&root).await?;
    if status["is_repo"] != true {
        return Ok(Json(listing("no_git", None, &Remote::default())));
    }
    let branch = status["current_branch"].as_str().unwrap_or("");
    let remote = remote(&root, "origin").await?;
    let state = if remote.url.is_empty() {
        "no_remote"
    } else if !remote.github() {
        "not_github"
    } else {
        "no_pr"
    };
    let mut result = listing(state, Some(branch), &remote);
    if state != "no_pr" {
        return Ok(Json(result));
    }
    let github = GitHub::select(injected);
    let token = github.token().await;
    let mut url = github.url(&remote.owner, &remote.repo, true);
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("state", if branch.is_empty() { "open" } else { "all" });
        if !branch.is_empty() {
            query.append_pair("head", &format!("{}:{branch}", remote.owner));
        }
        query.append_pair("per_page", "10");
    }
    match github.request(Method::GET, url, &token, None, 8).await {
        Ok(raw) if raw.is_array() => {
            let records = raw.as_array().unwrap().iter().filter(|pr| pr.is_object()).map(|pr| json!({
                "number":pr["number"],"title":pr["title"],"state":pr_state(pr),"html_url":pr["html_url"],
                "user_login":pr["user"]["login"],"created_at":pr["created_at"],"updated_at":pr["updated_at"],
                "base_ref":pr["base"]["ref"],"head_ref":pr["head"]["ref"],"mergeable_state":pr["mergeable_state"]
            })).collect::<Vec<_>>();
            result["state"] = json!(if records.is_empty() { "no_pr" } else { "ok" });
            result["pull_requests"] = json!(records);
        }
        error => {
            result["state"] = json!("error");
            result["error"] = match error {
                Err(error) => error.1.0["error"]["message"].clone(),
                _ => json!("GitHub API returned an invalid pull request listing"),
            };
        }
    }
    Ok(Json(result))
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct CreateBody {
    title: String,
    body: String,
    base: String,
    draft: bool,
    push_first: bool,
}
impl CreateBody {
    fn validate(&self) -> Result<(), Failure> {
        if self.title.trim().is_empty() {
            return Err(github_error(
                StatusCode::BAD_REQUEST,
                "conversation.github.empty_title",
                "PR title must not be empty",
                "",
            ));
        }
        if self.title.chars().count() > 1000
            || self.body.chars().count() > 65000
            || self.base.chars().count() > 255
            || [&self.title, &self.body, &self.base]
                .iter()
                .any(|s| s.contains('\0'))
        {
            return Err(github_error(
                StatusCode::BAD_REQUEST,
                "conversation.github.invalid_input",
                "PR title, body or base exceeds its allowed size or contains a null character",
                "",
            ));
        }
        Ok(())
    }
}
fn needs_push(status: &Value) -> bool {
    status["upstream"].as_str().is_none_or(str::is_empty)
        || status["ahead"].as_i64().unwrap_or(0) != 0
}
fn head_ref(origin: &Remote, head_remote: &Remote, remote_branch: &str) -> String {
    if head_remote.github() && head_remote.owner != origin.owner {
        format!("{}:{remote_branch}", head_remote.owner)
    } else {
        remote_branch.to_owned()
    }
}
async fn create(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    injected: Option<Extension<Arc<GitHub>>>,
    Json(body): Json<CreateBody>,
) -> Api {
    body.validate()?;
    let _guard = WRITES.lock().await;
    if workspace(&s, &id)?
        .as_ref()
        .and_then(|w| w["root_path"].as_str())
        .is_none_or(|root| root.trim().is_empty())
    {
        return Err(github_error(
            StatusCode::NOT_FOUND,
            "conversation.workspace.not_found",
            "This conversation has no workspace",
            "",
        ));
    }
    let root = git_api::thread_root(&s, &id)?;
    let status = inspect_git_status(&root).await?;
    if status["is_repo"] != true {
        return Err(failure(
            "conversation.git.not_repo",
            "Workspace is not a Git repository",
        ));
    }
    let branch = git_api::branch(&root).await?;
    let origin = remote(&root, "origin").await?;
    if origin.url.is_empty() {
        return Err(failure(
            "conversation.git.no_remote",
            "Repository has no origin remote",
        ));
    }
    if !origin.github() {
        return Err(github_error(
            StatusCode::CONFLICT,
            "conversation.github.not_github",
            "The origin remote is not a supported GitHub repository",
            "",
        ));
    }
    let github = GitHub::select(injected);
    let token = github.token().await;
    if token.is_empty() {
        return Err(github_error(
            StatusCode::FORBIDDEN,
            "conversation.github.token_missing",
            "GitHub token is not configured",
            "Set MUTEKI_GITHUB_TOKEN / GH_TOKEN / GITHUB_TOKEN, or run gh auth login on the server",
        ));
    }
    let mut pushed = false;
    if needs_push(&status) {
        if !body.push_first {
            return Err(github_error(
                StatusCode::CONFLICT,
                "conversation.git.push_required",
                "Push this branch and its new commits before creating a PR",
                "Push first, or submit with push_first=true",
            ));
        }
        git_api::push(&root, PushBody::default()).await?;
        pushed = true;
    }
    let remote_name = run(
        &root,
        &["config", "--get", &format!("branch.{branch}.remote")],
        30,
        ERROR,
        false,
    )
    .await?;
    let merge = run(
        &root,
        &["config", "--get", &format!("branch.{branch}.merge")],
        30,
        ERROR,
        false,
    )
    .await?;
    let name = remote_name.stdout.trim();
    let head_remote = if name.is_empty() || name == "origin" {
        remote(&root, "origin").await?
    } else {
        remote(&root, name).await?
    };
    let merge = merge.stdout.trim();
    let remote_branch = if merge.is_empty() {
        &branch
    } else {
        merge.strip_prefix("refs/heads/").unwrap_or(merge)
    };
    let head = head_ref(&origin, &head_remote, remote_branch);
    let mut base = body.base.trim().to_owned();
    if base.is_empty() {
        let info = github
            .request(
                Method::GET,
                github.url(&origin.owner, &origin.repo, false),
                &token,
                None,
                15,
            )
            .await?;
        base = info["default_branch"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_owned();
        if base.is_empty() {
            return Err(github_error(
                StatusCode::BAD_REQUEST,
                "conversation.github.base_unknown",
                "Could not determine the default branch; specify a base branch",
                "",
            ));
        }
    }
    let created = github.request(Method::POST, github.url(&origin.owner, &origin.repo, true), &token,
        Some(json!({"title":body.title.trim(),"body":body.body,"base":base,"head":head,"draft":body.draft})), 15).await?;
    if !created.is_object()
        || created["number"].as_u64().is_none()
        || created["html_url"].as_str().is_none_or(str::is_empty)
    {
        return Err(github_error(
            StatusCode::BAD_GATEWAY,
            "conversation.github.api_failed",
            "GitHub API returned an invalid pull request",
            "",
        ));
    }
    Ok(Json(
        json!({"number":created["number"],"url":created["html_url"],"state":pr_state(&created),"title":created.get("title").cloned().unwrap_or(json!(body.title.trim())),"base":base,"head":head,"owner":origin.owner,"repo":origin.repo,"pushed":pushed}),
    ))
}

pub(crate) async fn inspect_thread_prs(
    s: &Arc<AppState>,
    id: &str,
    number: Option<u64>,
    include_comments: bool,
    include_checks: bool,
) -> Result<Value, Failure> {
    let Json(mut listing_val) = list(State(s.clone()), Path(id.to_owned()), None).await?;
    let owner = listing_val["owner"].as_str().unwrap_or("").to_owned();
    let repo_name = listing_val["repo"].as_str().unwrap_or("").to_owned();
    if owner.is_empty() || repo_name.is_empty() {
        return Ok(listing_val);
    }
    let chosen_number = number.or_else(|| {
        listing_val["pull_requests"]
            .as_array()
            .and_then(|rows| rows.first())
            .and_then(|pr| pr["number"].as_u64())
    });
    let Some(pr_num) = chosen_number else {
        return Ok(listing_val);
    };
    let github = GitHub::production();
    let token = github.token().await;
    let mut pr_url = github.url(&owner, &repo_name, true);
    if let Ok(mut segs) = pr_url.path_segments_mut() {
        segs.push(&pr_num.to_string());
    }
    if let Ok(detail) = github.request(Method::GET, pr_url, &token, None, 10).await {
        let head_sha = detail["head"]["sha"].as_str().unwrap_or("").to_owned();
        let mut enriched = json!({
            "number": detail["number"],
            "title": detail["title"],
            "body": detail["body"],
            "state": pr_state(&detail),
            "html_url": detail["html_url"],
            "user_login": detail["user"]["login"],
            "base_ref": detail["base"]["ref"],
            "head_ref": detail["head"]["ref"],
            "head_sha": head_sha,
            "mergeable": detail["mergeable"],
            "mergeable_state": detail["mergeable_state"],
            "additions": detail["additions"],
            "deletions": detail["deletions"],
            "changed_files": detail["changed_files"],
            "comments_count": detail["comments"],
            "review_comments_count": detail["review_comments"],
            "created_at": detail["created_at"],
            "updated_at": detail["updated_at"],
            "merged_at": detail["merged_at"],
        });
        if include_checks && !head_sha.is_empty() {
            let mut checks_url = github.url(&owner, &repo_name, false);
            if let Ok(mut segs) = checks_url.path_segments_mut() {
                segs.extend(["commits", &head_sha, "check-runs"]);
            }
            if let Ok(checks) = github.request(Method::GET, checks_url, &token, None, 10).await {
                let runs: Vec<Value> = checks["check_runs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(30)
                    .map(|run| json!({
                        "name": run["name"],
                        "status": run["status"],
                        "conclusion": run["conclusion"],
                        "details_url": run["details_url"],
                    }))
                    .collect();
                enriched["check_runs"] = json!(runs);
            }
        }
        if include_comments {
            let mut comments_url = github.url(&owner, &repo_name, false);
            if let Ok(mut segs) = comments_url.path_segments_mut() {
                segs.extend(["issues", &pr_num.to_string(), "comments"]);
            }
            comments_url.query_pairs_mut().append_pair("per_page", "30");
            if let Ok(Value::Array(items)) = github.request(Method::GET, comments_url, &token, None, 10).await {
                let comments: Vec<Value> = items.iter().map(|c| json!({
                    "id": c["id"],
                    "author": c["user"]["login"],
                    "body": c["body"],
                    "created_at": c["created_at"],
                    "html_url": c["html_url"],
                })).collect();
                enriched["issue_comments"] = json!(comments);
            }
        }
        listing_val["selected_pr"] = enriched;
    }
    Ok(listing_val)
}

pub(crate) async fn create_thread_pr(
    s: &Arc<AppState>,
    id: &str,
    title: String,
    body: String,
    base: String,
    draft: bool,
    push_first: bool,
) -> Result<Value, Failure> {
    let Json(value) = create(
        State(s.clone()),
        Path(id.to_owned()),
        None,
        Json(CreateBody { title, body, base, draft, push_first }),
    ).await?;
    Ok(value)
}

pub(crate) async fn comment_thread_pr(
    s: &Arc<AppState>,
    id: &str,
    number: u64,
    body_text: String,
    reply_to_review_comment_id: Option<u64>,
) -> Result<Value, Failure> {
    let trimmed = body_text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 65000 {
        return Err(github_error(
            StatusCode::BAD_REQUEST,
            "conversation.github.invalid_input",
            "Comment body must be 1..=65000 characters",
            "",
        ));
    }
    let root = bound_root(s, id).map_err(|_| {
        github_error(StatusCode::NOT_FOUND, "conversation.workspace.not_found", "This conversation has no workspace", "")
    })?;
    let origin = remote(&root, "origin").await?;
    if !origin.github() {
        return Err(github_error(StatusCode::CONFLICT, "conversation.github.not_github", "The origin remote is not a GitHub repository", ""));
    }
    let github = GitHub::production();
    let token = github.token().await;
    if token.is_empty() {
        return Err(github_error(
            StatusCode::FORBIDDEN,
            "conversation.github.token_missing",
            "GitHub token is not configured",
            "Set MUTEKI_GITHUB_TOKEN / GH_TOKEN / GITHUB_TOKEN, or run gh auth login on the server",
        ));
    }
    let mut url = github.url(&origin.owner, &origin.repo, false);
    if let Some(comment_id) = reply_to_review_comment_id {
        if let Ok(mut segs) = url.path_segments_mut() {
            segs.extend(["pulls", &number.to_string(), "comments", &comment_id.to_string(), "replies"]);
        }
    } else if let Ok(mut segs) = url.path_segments_mut() {
        segs.extend(["issues", &number.to_string(), "comments"]);
    }
    let created = github.request(Method::POST, url, &token, Some(json!({"body": trimmed})), 15).await?;
    Ok(json!({
        "id": created["id"],
        "number": number,
        "html_url": created["html_url"],
        "created_at": created["created_at"],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        extract::Query,
        http::{HeaderMap, Request},
        response::IntoResponse,
    };
    use std::{collections::HashMap, sync::Mutex};
    use tower::ServiceExt;

    #[test]
    fn remote_parsing_head_resolution_and_input_limits() {
        let origin = parse_remote("git@github.com:team/project.git");
        assert_eq!(
            (origin.owner.as_str(), origin.repo.as_str()),
            ("team", "project")
        );
        let https = parse_remote("https://user:secret@github.com/team/project.git");
        assert!(https.github());
        assert!(!https.url.contains("secret"));
        for url in [
            "https://github.com.evil.invalid/team/project",
            "ssh://git@github.com/team/project",
            "https://github.com/team/project/extra",
            "https://gitlab.com/team/project",
            "https://github.com/team/project?token=x",
        ] {
            assert!(!parse_remote(url).github(), "{url}");
        }
        assert_eq!(
            head_ref(
                &origin,
                &parse_remote("git@github.com:fork/project.git"),
                "remote/feature"
            ),
            "fork:remote/feature"
        );
        assert_eq!(head_ref(&origin, &origin, "tracking-name"), "tracking-name");
        assert!(CreateBody::default().validate().is_err());
        assert!(
            CreateBody {
                title: "x".repeat(1001),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            CreateBody {
                title: "Valid".into(),
                body: "x".repeat(65001),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(needs_push(&json!({"upstream":null,"ahead":0})));
        assert!(needs_push(&json!({"upstream":"origin/feature","ahead":1})));
        assert!(!needs_push(&json!({"upstream":"origin/feature","ahead":0})));
    }
    #[derive(Default)]
    struct Stub {
        error: Mutex<Option<u16>>,
        pulls: Mutex<Vec<Value>>,
        requests: Mutex<Vec<(String, Value)>>,
    }
    async fn stub_get(
        State(stub): State<Arc<Stub>>,
        Query(query): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer synthetic-only"
        );
        stub.requests
            .lock()
            .unwrap()
            .push(("GET pulls".into(), json!(query)));
        if let Some(code) = *stub.error.lock().unwrap() {
            let body = json!({"message":"Fixture failure for synthetic-only"});
            return (StatusCode::from_u16(code).unwrap(), Json(body)).into_response();
        }
        Json(json!(*stub.pulls.lock().unwrap())).into_response()
    }
    async fn stub_create(
        State(stub): State<Arc<Stub>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer synthetic-only"
        );
        stub.requests
            .lock()
            .unwrap()
            .push(("POST pulls".into(), body.clone()));
        if body["title"] == "Reject fixture" {
            return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"message":"Fixture rejection","errors":[{"message":"No commits between base and head"}]}))).into_response();
        }
        let mut pulls = stub.pulls.lock().unwrap();
        let created = json!({"number":pulls.len()+1,"title":body["title"],"body":body["body"],"draft":body["draft"],"state":"open","html_url":"https://github.com/fixture/project/pull/1","user":{"login":"fixture"},"created_at":"2026-10-29T00:00:00Z","updated_at":"2026-10-29T00:00:00Z","base":{"ref":body["base"]},"head":{"ref":body["head"]},"merged_at":null});
        pulls.push(created.clone());
        (StatusCode::CREATED, Json(created)).into_response()
    }
    async fn stub() -> (Arc<GitHub>, Arc<Stub>, tokio::task::JoinHandle<()>) {
        let state = Arc::new(Stub::default());
        let routes = Router::new()
            .route(
                "/repos/{owner}/{repo}",
                get(|| async { Json(json!({"default_branch":"main"})) }),
            )
            .route(
                "/repos/{owner}/{repo}/pulls",
                get(stub_get).post(stub_create),
            )
            .route(
                "/redirect",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [("location", "https://example.invalid/credential-leak")],
                    )
                }),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        let mut client = GitHub::production();
        client.endpoint = Some(endpoint);
        client.credential = Some("synthetic-only".into());
        (Arc::new(client), state, task)
    }
    async fn fixture() -> (tempfile::TempDir, Arc<AppState>) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let bare = directory.path().join("remote.git");
        std::fs::create_dir(&bare).unwrap();
        run(&bare, &["init", "--bare"], 30, ERROR, true)
            .await
            .unwrap();
        run(
            &bare,
            &["config", "core.hooksPath", "/dev/null"],
            30,
            ERROR,
            true,
        )
        .await
        .unwrap();
        for args in [
            vec!["init", "-b", "feature/manual-pr"],
            vec!["config", "user.name", "PR fixture"],
            vec!["config", "user.email", "fixture@example.invalid"],
            vec!["config", "core.hooksPath", "/dev/null"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "tag.gpgsign", "false"],
        ] {
            run(&root, &args, 30, ERROR, true).await.unwrap();
        }
        std::fs::write(root.join("example.txt"), "A local manual PR fixture\n").unwrap();
        run(&root, &["add", "example.txt"], 30, ERROR, true)
            .await
            .unwrap();
        run(
            &root,
            &["commit", "-m", "Manual PR fixture"],
            30,
            ERROR,
            true,
        )
        .await
        .unwrap();
        run(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/fixture/project.git",
            ],
            30,
            ERROR,
            true,
        )
        .await
        .unwrap();
        run(
            &root,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                bare.to_str().unwrap(),
            ],
            30,
            ERROR,
            true,
        )
        .await
        .unwrap();
        git_api::push(&root, PushBody::default()).await.unwrap();
        let state = Arc::new(AppState::new(
            crate::store::Store::open(directory.path().join("fixture.sqlite")).unwrap(),
        ));
        state
            .store
            .put("system", "state_dir", &json!(directory.path()))
            .unwrap();
        state.store.put("projects", "pr-project", &json!({"project_id":"pr-project","name":"Manual PR fixture","root_path":root,"created_at":"2026-10-29T00:00:00Z","updated_at":"2026-10-29T00:00:00Z"})).unwrap();
        state.store.put("workspaces", "pr-workspace", &json!({"workspace_id":"pr-workspace","project_id":"pr-project","root_path":root,"kind":"local","settings":{"mode":"shared_checkout"}})).unwrap();
        state.store.put("conversation_threads", "pr-fixture", &json!({"thread":{"thread_id":"pr-fixture","title":"Manual PR fixture","title_source":"user","mode":"conversation","project_id":"pr-project","workspace_id":"pr-workspace","created_at":"2026-10-29T00:00:00Z","updated_at":"2026-10-29T00:00:00Z","archived":false,"state":{"status":"idle","running_turn_id":null,"queue_count":0,"queue_paused":false,"queue_revision":0}},"queue":[],"runtime":{"adapter_id":"openai","instance_id":"text"},"messages":[],"turns":[],"event_seq":0})).unwrap();
        (directory, state)
    }
    async fn call(app: Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    fn test_router(state: Arc<AppState>, client: Arc<GitHub>) -> Router {
        router().layer(Extension(client)).with_state(state)
    }
    #[tokio::test]
    async fn manual_pr_routes_preserve_payloads_states_errors_and_push_guard() {
        let (_directory, state) = fixture().await;
        let (client, stub, server) = stub().await;
        let app = test_router(state.clone(), client);
        let (status, body) = call(
            app.clone(),
            "GET",
            "/api/threads/pr-fixture/pull-requests",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "no_pr");
        let query = stub.requests.lock().unwrap()[0].1.clone();
        assert_eq!(query["head"], "fixture:feature/manual-pr");
        assert_eq!(query["state"], "all");
        let (status, created) = call(
            app.clone(),
            "POST",
            "/api/threads/pr-fixture/git/pull-request",
            json!({"title":"  Fixture title  ","body":"Full\nbody","draft":true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(created["state"], "draft");
        assert_eq!(created["base"], "main");
        assert_eq!(created["pushed"], false);
        let (_, body) = call(
            app.clone(),
            "GET",
            "/api/threads/pr-fixture/pull-requests",
            json!({}),
        )
        .await;
        assert_eq!(body["pull_requests"][0]["state"], "draft");
        let payload = stub
            .requests
            .lock()
            .unwrap()
            .iter()
            .find(|(name, _)| name == "POST pulls")
            .unwrap()
            .1
            .clone();
        assert_eq!(
            payload,
            json!({"title":"Fixture title","body":"Full\nbody","base":"main","head":"feature/manual-pr","draft":true})
        );
        let (status, rejected) = call(
            app.clone(),
            "POST",
            "/api/threads/pr-fixture/git/pull-request",
            json!({"title":"Reject fixture"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(rejected["error"]["code"], "conversation.github.pr_rejected");
        *stub.error.lock().unwrap() = Some(403);
        let (_, failed) = call(
            app.clone(),
            "GET",
            "/api/threads/pr-fixture/pull-requests",
            json!({}),
        )
        .await;
        assert_eq!(failed["state"], "error");
        assert!(!failed.to_string().contains("synthetic-only"));
        *stub.error.lock().unwrap() = None;
        let root = bound_root(&state, "pr-fixture").unwrap();
        run(
            &root,
            &["config", "--unset", "branch.feature/manual-pr.remote"],
            30,
            ERROR,
            true,
        )
        .await
        .unwrap();
        let before = stub.requests.lock().unwrap().len();
        let (status, blocked) = call(
            app.clone(),
            "POST",
            "/api/threads/pr-fixture/git/pull-request",
            json!({"title":"Must not push"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(blocked["error"]["code"], "conversation.git.push_required");
        assert_eq!(stub.requests.lock().unwrap().len(), before);
        let (status, created) = call(
            app.clone(),
            "POST",
            "/api/threads/pr-fixture/git/pull-request",
            json!({"title":"Push only to local bare fixture","push_first":true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(created["pushed"], true);
        state.store.put("conversation_threads", "other", &json!({"thread":{"thread_id":"other","workspace_id":"pr-workspace","state":{"status":"active"}}})).unwrap();
        let (status, blocked) = call(
            app,
            "POST",
            "/api/threads/pr-fixture/git/pull-request",
            json!({"title":"Occupied"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(blocked["error"]["code"], "conversation.git.root_occupied");
        server.abort();
    }
    #[tokio::test]
    async fn missing_workspace_repository_remote_and_credentials_are_explicit() {
        let (_directory, state) = fixture().await;
        let (client, _, server) = stub().await;
        let app = test_router(state.clone(), client.clone());
        let (status, body) = call(
            app.clone(),
            "GET",
            "/api/threads/missing/pull-requests",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "conversation.thread.not_found");
        state
            .store
            .put(
                "conversation_threads",
                "unbound",
                &json!({"thread":{"thread_id":"unbound"}}),
            )
            .unwrap();
        let (_, body) = call(
            app.clone(),
            "GET",
            "/api/threads/unbound/pull-requests",
            json!({}),
        )
        .await;
        assert_eq!(body["state"], "no_workspace");
        let root = bound_root(&state, "pr-fixture").unwrap();
        run(
            &root,
            &[
                "remote",
                "set-url",
                "origin",
                "https://gitlab.com/fixture/project.git",
            ],
            30,
            ERROR,
            true,
        )
        .await
        .unwrap();
        let (_, body) = call(
            app.clone(),
            "GET",
            "/api/threads/pr-fixture/pull-requests",
            json!({}),
        )
        .await;
        assert_eq!(body["state"], "not_github");
        run(&root, &["remote", "remove", "origin"], 30, ERROR, true)
            .await
            .unwrap();
        let (_, body) = call(
            app.clone(),
            "GET",
            "/api/threads/pr-fixture/pull-requests",
            json!({}),
        )
        .await;
        assert_eq!(body["state"], "no_remote");
        run(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/fixture/project.git",
            ],
            30,
            ERROR,
            true,
        )
        .await
        .unwrap();
        let mut no_token = (*client).clone();
        no_token.credential = Some(String::new());
        let (status, body) = call(
            test_router(state.clone(), Arc::new(no_token)),
            "POST",
            "/api/threads/pr-fixture/git/pull-request",
            json!({"title":"No credentials"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "conversation.github.token_missing");
        std::fs::rename(root.join(".git"), root.join("hidden-git")).unwrap();
        let (_, body) = call(
            app,
            "GET",
            "/api/threads/pr-fixture/pull-requests",
            json!({}),
        )
        .await;
        assert_eq!(body["state"], "no_git");
        server.abort();
    }
    #[tokio::test]
    async fn github_client_does_not_follow_redirects_or_report_token_contents() {
        let (client, _, server) = stub().await;
        let url = client.endpoint.as_ref().unwrap().join("redirect").unwrap();
        let error = client
            .request(Method::GET, url, "synthetic-only", None, 2)
            .await
            .unwrap_err();
        assert_eq!(error.1.0["error"]["code"], "conversation.github.api_failed");
        assert!(!error.1.0.to_string().contains("synthetic-only"));
        server.abort();
    }
    /// Full app with isolated metadata and a loopback GitHub stub, never host GitHub credentials.
    /// Run with --ignored --nocapture. Ctrl-C ends the disposable fixture process.
    #[tokio::test]
    #[ignore]
    async fn manual_ui_fixture() {
        let (directory, state) = fixture().await;
        let (client, _, stub_server) = stub().await;
        let access = crate::access_api::Access::from_env(
            directory.path(),
            state.store.installation_id().unwrap(),
            false,
        )
        .unwrap();
        let port = std::env::var("MUTEKI_PR_UI_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(18100);
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        println!(
            "Synthetic PR UI fixture: http://127.0.0.1:{port}; thread pr-fixture; root {}",
            directory.path().display()
        );
        println!(
            "Title 'Reject fixture' returns a local API rejection; other titles create in-memory fixture PRs. Push writes only to local bare repo. Do not fetch/pull."
        );
        let app = crate::app(state, access).layer(Extension(client));
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
        stub_server.abort();
    }
}
