use super::*;
use crate::agent_runtime::credentials;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
pub(super) enum FollowupModelTransport {
    HttpProvider(crate::provider::TextProvider, worker::WorkerSpec),
    PiRpcReadonly {
        env: Vec<(String, String)>,
        spec: worker::WorkerSpec,
    },
}
impl FollowupModelTransport {
    fn spec(&self) -> &worker::WorkerSpec {
        match self {
            Self::HttpProvider(_, s) | Self::PiRpcReadonly { spec: s, .. } => s,
        }
    }
    pub(super) fn actor(&self) -> Value {
        json!({"engine":self.spec().norm_engine,"model":self.spec().model,"transport":match self{Self::HttpProvider(..)=>"HttpProvider",_=>"PiRpcReadonly"}})
    }
    pub(super) async fn execute(
        self,
        job: &Job,
        prompt: &str,
        settings: &Value,
    ) -> Result<String, Failure> {
        job.progress("investigating", None, "Inspecting Run evidence", 0)
            .await?;
        match self {
            Self::HttpProvider(provider, spec) => {
                http(job, prompt, settings, &provider, &spec).await
            }
            Self::PiRpcReadonly { env, spec } => pi(job, prompt, settings, env, &spec).await,
        }
    }
}
pub(super) async fn select(
    state: &Arc<AppState>,
    run: &Value,
    settings: &Value,
) -> Result<FollowupModelTransport, Failure> {
    if run["kind"] == "local_pi" || run["driver_kind"] == "local_pi" {
        let s = &run["local_pi_session"];
        let local = json!({"model":s["model"]["id"].as_str().or_else(||s["model"].as_str()),"credential_id":s["credential_id"].as_str().or_else(||s["model"]["account_id"].as_str()).or_else(||s["account_id"].as_str())});
        match resolve(state, &local).await {
            Ok(t) => return Ok(t),
            Err(e)
                if matches!(
                    e.code.as_str(),
                    "run.followup.planner_not_configured"
                        | "run.followup.credential_unavailable"
                        | "run.followup.auth_expired"
                        | "run.followup.model_not_available"
                ) => {}
            Err(e) => return Err(e),
        }
    }
    let profile = run["settings"]["llm_profiles"]
        .get("planner")
        .unwrap_or(&settings["llm_profiles"]["planner"]);
    resolve(state, profile).await
}
async fn resolve(
    state: &Arc<AppState>,
    profile: &Value,
) -> Result<FollowupModelTransport, Failure> {
    let model = profile["model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Failure::new("planner_not_configured", "Planner model is not configured"))?;
    let selected = profile["credential_id"]
        .as_str()
        .or_else(|| profile["endpoint_id"].as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Failure::new(
                "planner_not_configured",
                "Planner credential is not configured",
            )
        })?;
    let id = selected
        .strip_prefix("account:")
        .or_else(|| selected.strip_prefix("endpoint:"))
        .unwrap_or(selected);
    let mut account = crate::provider::matching_account(&state.store, selected)
        .map_err(storage)?
        .ok_or_else(|| {
            Failure::new(
                "credential_unavailable",
                "Selected credential is unavailable",
            )
        })?;
    if account["status"] == "expired" || account["auth_status"] == "expired" {
        return Err(Failure::new(
            "auth_expired",
            "Selected credential has expired",
        ));
    }
    let state_dir = native_chat::state_dir(state);
    let material_base = crate::account_material::base(&state_dir, id, &account).map_err(storage)?;
    let secret_dirs = [
        material_base,
        state_dir.join("_secrets").join("accounts").join(id),
        state_dir
            .join("imported-credentials")
            .join("accounts")
            .join(id),
    ];
    for dir in &secret_dirs {
        for (field, file) in [
            ("secret", "API_KEY"),
            ("base_url", "BASE_URL"),
            ("default_model", "DEFAULT_MODEL"),
        ] {
            if account[field].as_str().is_none_or(|s| s.trim().is_empty()) {
                match fs::read_to_string(dir.join(file)) {
                    Ok(v) if !v.trim().is_empty() => account[field] = json!(v.trim()),
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if account["models"].as_array().is_none_or(Vec::is_empty) {
            match fs::read(dir.join("MODELS.json")) {
                Ok(bytes) => {
                    let models: Value = serde_json::from_slice(&bytes).map_err(storage)?;
                    if let Some(arr) = models.as_array().or_else(|| models["models"].as_array()) {
                        account["models"] = Value::Array(arr.clone());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    if account["base_url"]
        .as_str()
        .is_some_and(|v| !v.trim().is_empty())
        && account["connection"].is_null()
    {
        account["connection"] = json!("custom_endpoint");
    }
    let listed = |row: &Value| {
        row["default_model"] == model
            || [
                "models",
                "configured_models",
                "discovered_models",
                "verified_models",
            ]
            .iter()
            .any(|key| {
                row[*key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|m| m.as_str() == Some(model) || m["id"] == model)
            })
    };
    let catalogs = state.store.list("model_catalogs").map_err(storage)?;
    let imported_catalogs: Vec<Value> = match fs::read_to_string(
        state_dir.join("_credential_model_catalog.json"),
    ) {
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|root| {
                root["catalogs"]
                    .as_object()
                    .map(|m| m.values().cloned().collect())
            })
            .unwrap_or_default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    if !listed(&account)
        && !catalogs.iter().chain(imported_catalogs.iter()).any(|c| {
            c["credential_id"].as_str().is_some_and(|key| {
                key.strip_prefix("account:")
                    .or_else(|| key.strip_prefix("endpoint:"))
                    .unwrap_or(key)
                    == id
            }) && listed(c)
        })
    {
        return Err(Failure::new(
            "model_not_available",
            "Model is not in the selected credential catalog",
        ));
    }
    let raw_account_engine = account["worker_engine"]
        .as_str()
        .or_else(|| account["details"]["target_engine"].as_str())
        .or_else(|| account["engine"].as_str())
        .unwrap_or("http");
    if profile["engine"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .is_some_and(|pe| pe != raw_account_engine && account["engine"].as_str() != Some(pe))
    {
        return Err(Failure::new(
            "credential_engine_mismatch",
            "Credential engine differs from planner engine",
        ));
    }
    let engine = raw_account_engine;
    let mut spec = worker::WorkerSpec {
        solver_id: "followup".into(),
        profile_id: "planner".into(),
        profile_label: "followup".into(),
        raw_engine: engine.into(),
        norm_engine: engine.into(),
        adapter_id: if engine == "pi" { "pi.rpc" } else { "http" }.into(),
        instance_id: "default".into(),
        model: Some(model.into()),
        effort: None,
        account_id: id.into(),
        effective_credential_id: selected.into(),
        backend: "local".into(),
        profile: profile.clone(),
    };
    usage::freeze_price(state, &mut spec);
    if engine == "pi" {
        let descriptor = crate::agent_runtime::descriptors::find("pi")
            .ok_or_else(|| Failure::new("protocol_unsupported", "Pi descriptor unavailable"))?;
        let resolved = credentials::resolve(
            &state.store,
            &native_chat::state_dir(state),
            descriptor,
            &format!("account:{id}"),
            Some(model),
        )
        .await
        .map_err(|e| {
            Failure::new(
                match e.code {
                    "credential.engine_mismatch" => "credential_engine_mismatch",
                    "credential.auth_expired" => "auth_expired",
                    "credential.model_unavailable" => "model_not_available",
                    _ => "credential_unavailable",
                },
                e.message,
            )
        })?;
        return Ok(FollowupModelTransport::PiRpcReadonly {
            env: resolved.env,
            spec,
        });
    }
    if let Some(wire) = account["wire_api"]
        .as_str()
        .or_else(|| account["endpoint"]["wire_api"].as_str())
        .or_else(|| profile["wire_api"].as_str())
    {
        if !matches!(
            wire,
            "chat_completions" | "chat-completions" | "openai-completions" | "openai"
        ) {
            return Err(Failure::new(
                "protocol_unsupported",
                format!("Unsupported tool protocol: {wire}"),
            ));
        }
    }
    if account["supports_tools"] == false {
        return Err(Failure::new(
            "protocol_unsupported",
            "Endpoint does not support function calling",
        ));
    }
    let provider = crate::provider::provider_from(&account, selected, Some(model))
        .map_err(|e| Failure::new("credential_unavailable", e.to_string()))?;
    if provider.protocol != "chat_completions" {
        return Err(Failure::new(
            "protocol_unsupported",
            "Followups require chat-completions function calling",
        ));
    }
    Ok(FollowupModelTransport::HttpProvider(provider, spec))
}
fn system_prompt_for(kind: &str) -> &'static str {
    if kind == "writeup" {
        "You are an experienced CTF player writing a clean, reproducible CTF Writeup in Markdown. Use only board_query, events_query, artifact_read, and workspace_read to inspect the challenge files, artifacts, and final exploit/solution. Do NOT output internal workflow terms (event seq, art-XX, step_00X, board_query, solvegraph, blackboard, Worker, Coordinator), timelines, or unfinished-step postmortems. Output ONLY the final complete CTF Writeup Markdown with no introductory chatter before tool calls."
    } else {
        "You are a read-only Run investigator. Use only board_query, events_query, artifact_read, and workspace_read to inspect evidence; distinguish host-confirmed outcome from assistant claims."
    }
}

async fn http(
    job: &Job,
    prompt: &str,
    settings: &Value,
    p: &crate::provider::TextProvider,
    spec: &worker::WorkerSpec,
) -> Result<String, Failure> {
    let client = crate::network_proxy::http_client_builder()
        .build()
        .map_err(|e| Failure::new("provider_failed", e.to_string()))?;
    let mut messages = vec![
        json!({"role":"system","content":system_prompt_for(&job.kind)}),
        json!({"role":"user","content":prompt}),
    ];
    let mut text = String::new();
    let mut turn = 0;
    loop {
        turn += 1;
        job.budget(settings)?;
        job.progress("investigating", None, "Requesting model analysis", turn)
            .await?;
        let mut request=client.post(&p.endpoint).json(&json!({"model":p.model,"messages":messages,"tools":readonly::schemas(),"stream":false}));
        if let Some(token) = &p.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| Failure::new("provider_failed", e.without_url().to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Failure::new(
                match status.as_u16() {
                    401 | 403 => "auth_expired",
                    404 => "model_not_available",
                    400 | 422 => "protocol_unsupported",
                    _ => "provider_failed",
                },
                format!("Provider HTTP {status}"),
            ));
        }
        let response: Value = response
            .json()
            .await
            .map_err(|e| Failure::new("protocol_unsupported", e.to_string()))?;
        let u = &response["usage"];
        let observed = json!({"input_tokens":u["prompt_tokens"],"output_tokens":u["completion_tokens"],"cache_read_tokens":u["prompt_tokens_details"]["cached_tokens"],"reported_cost":u["cost"],"input_includes_cache":true});
        usage::record(
            &job.state,
            &job.run,
            &job.id,
            spec,
            &job.kind,
            &turn.to_string(),
            "message",
            &turn.to_string(),
            &observed,
        )?;
        job.budget(settings)?;
        let message = &response["choices"][0]["message"];
        if !message.is_object() {
            return Err(Failure::new(
                "protocol_unsupported",
                "Provider omitted assistant message",
            ));
        }
        if response["choices"][0]["finish_reason"] == "length" {
            return Err(Failure::new(
                "output_limit",
                "Model output reached provider limit",
            ));
        }
        messages.push(message.clone());
        let calls = message["tool_calls"].as_array();
        let has_calls = calls.is_some_and(|c| !c.is_empty());
        if let Some(delta) = message["content"].as_str() {
            if !(job.kind == "writeup" && has_calls) {
                text.push_str(delta);
                job.publish("followup.delta", json!({"text":delta})).await?;
            }
        }
        if !has_calls {
            job.progress("writing", None, "Answer complete", turn)
                .await?;
            return Ok(if job.kind == "writeup" {
                text.trim_start().to_owned()
            } else {
                text
            });
        }
        for call in calls.unwrap() {
            let name = call["function"]["name"].as_str().unwrap_or("");
            let args = serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}"))
                .map_err(|e| {
                    Failure::new(
                        "protocol_unsupported",
                        format!("Invalid tool arguments: {e}"),
                    )
                })?;
            let result = job.tool(name, &args, turn).await?;
            messages.push(
                json!({"role":"tool","tool_call_id":call["id"],"content":result.to_string()}),
            );
        }
    }
}
struct Bridge(tokio::task::AbortHandle);
impl Drop for Bridge {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn pi(
    job: &Job,
    prompt: &str,
    settings: &Value,
    env: Vec<(String, String)>,
    spec: &worker::WorkerSpec,
) -> Result<String, Failure> {
    let scratch = tempfile::Builder::new()
        .prefix("muteki-followup-")
        .tempdir()?;
    let agent = scratch.path().join("agent");
    let cwd = scratch.path().join("cwd");
    fs::create_dir(&agent)?;
    fs::create_dir(&cwd)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(scratch.path(), fs::Permissions::from_mode(0o700))?;
    }
    if let Some((_, source)) = env.iter().rev().find(|(k, _)| k == "PI_CODING_AGENT_DIR") {
        for name in ["auth.json", "models.json", "settings.json"] {
            match fs::read(std::path::Path::new(source).join(name)) {
                Ok(bytes) => {
                    let value: Value = serde_json::from_slice(&bytes).map_err(storage)?;
                    fn dynamic(v: &Value) -> bool {
                        match v {
                            Value::String(s) => s.starts_with('!'),
                            Value::Array(a) => a.iter().any(dynamic),
                            Value::Object(o) => o.values().any(dynamic),
                            _ => false,
                        }
                    }
                    if dynamic(&value) {
                        return Err(Failure::new(
                            "credential_unavailable",
                            "Dynamic credential commands are not allowed for followups",
                        ));
                    }
                    let bytes = if name == "settings.json" {
                        let mut sanitized = serde_json::Map::new();
                        for key in ["retry", "httpIdleTimeoutMs", "requestTimeoutMs"] {
                            if let Some(item) = value.get(key) {
                                sanitized.insert(key.into(), item.clone());
                            }
                        }
                        serde_json::to_vec(&Value::Object(sanitized)).map_err(storage)?
                    } else {
                        bytes
                    };
                    credentials::private_file(&agent.join(name), &bytes)
                        .map_err(|e| Failure::new("credential_unavailable", e.message))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    let token = uuid::Uuid::new_v4().to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let context = (job.clone(), token.clone());
    let router = axum::Router::new()
        .route(
            "/tool",
            post(
                |State((job, token)): State<(Job, String)>,
                 headers: axum::http::HeaderMap,
                 Json(body): Json<Value>| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        != Some(token.as_str())
                    {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    match job
                        .tool(
                            body["name"].as_str().unwrap_or(""),
                            &body["args"],
                            body["turn"].as_u64().unwrap_or(0),
                        )
                        .await
                    {
                        Ok(v) => Json(v).into_response(),
                        Err(e) => e.response(StatusCode::UNPROCESSABLE_ENTITY),
                    }
                },
            ),
        )
        .with_state(context);
    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(error=%e,"Followup bridge failed");
        }
    });
    let _bridge = Bridge(server.abort_handle());
    let extension = scratch.path().join("readonly.ts");
    fs::write(&extension, include_str!("readonly.ts"))?;
    let instance =
        crate::runtime_api::instance(&job.state.store, "pi.rpc", "default").unwrap_or(Value::Null);
    let binary = instance["binary_path"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("pi");
    let resolved = crate::shell_env::resolve_program(std::ffi::OsStr::new(binary));
    // Intentionally clears the environment before applying the scoped followup env.
    #[allow(clippy::disallowed_methods)]
    let mut command = tokio::process::Command::new(resolved);
    command.env_clear();
    for key in [
        "PATH",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
    ] {
        if let Some(v) = std::env::var_os(key) {
            command.env(key, v);
        } else if key == "PATH"
            && let Some(path) = crate::shell_env::var("PATH")
        {
            command.env("PATH", path);
        }
    }
    for (k, v) in &env {
        if !matches!(k.as_str(), "PI_CODING_AGENT_DIR" | "HOME") {
            command.env(k, v);
        }
    }
    let mut no_proxy: Vec<String> = env
        .iter()
        .rfind(|(k, _)| k.eq_ignore_ascii_case("no_proxy"))
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("NO_PROXY").ok())
        .or_else(|| std::env::var("no_proxy").ok())
        .map(|raw| {
            raw.split(',')
                .map(|part| part.trim().to_owned())
                .filter(|part| !part.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for host in ["127.0.0.1", "localhost"] {
        if !no_proxy.iter().any(|existing| existing == host) {
            no_proxy.push(host.to_owned());
        }
    }
    let no_proxy_value = no_proxy.join(",");
    command
        .env("NO_PROXY", &no_proxy_value)
        .env("no_proxy", &no_proxy_value);
    command
        .env("HOME", scratch.path())
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("MUTEKI_READONLY_URL", format!("http://{address}/tool"))
        .env("MUTEKI_READONLY_TOKEN", token)
        .env("MUTEKI_READONLY_TOOLS", readonly::schemas().to_string());
    command
        .current_dir(&cwd)
        .args([
            "--mode",
            "rpc",
            "--no-session",
            "--no-builtin-tools",
            "--no-extensions",
            "--no-mcp",
            "--no-skills",
            "--no-context-files",
            "--no-prompt-templates",
            "--no-themes",
            "--system-prompt",
            system_prompt_for(&job.kind),
            "--extension",
        ])
        .arg(extension)
        .args([
            "--tools",
            "board_query,events_query,artifact_read,workspace_read",
            "--model",
            spec.model.as_deref().unwrap(),
        ]);
    if let Some((_, provider)) = env.iter().rev().find(|(k, _)| k == "MUTEKI_PI_PROVIDER") {
        command.args(["--provider", provider]);
    }
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(
            format!(
                "{}\n",
                json!({"id":job.id,"type":"prompt","message":prompt})
            )
            .as_bytes(),
        )
        .await?;
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stderr = child.stderr.take().unwrap();
    let errors = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut text = String::new();
        stderr.read_to_string(&mut text).await.map(|_| text)
    });
    let _errors = Bridge(errors.abort_handle());
    let mut observer = usage::Observer::new(&job.id, &job.kind);
    let mut text = String::new();
    while let Some(line) = lines.next_line().await? {
        job.budget(settings)?;
        observer.container(&job.state, &job.run, spec, &line)?;
        let event: Value = serde_json::from_str(&line).map_err(storage)?;
        if event["type"] == "response" && event["success"] == false {
            return Err(Failure::new(
                "provider_failed",
                event["error"].as_str().unwrap_or("Pi RPC failed"),
            ));
        }
        if job.kind == "writeup"
            && event["type"] == "message_start"
            && event["message"]["role"] == "assistant"
        {
            text.clear();
        }
        if event["type"] == "message_update"
            && event["assistantMessageEvent"]["type"] == "text_delta"
        {
            if let Some(delta) = event["assistantMessageEvent"]["delta"].as_str() {
                text.push_str(delta);
                job.publish("followup.delta", json!({"text":delta})).await?;
            }
        }
        if event["type"] == "agent_end" {
            if job.kind == "writeup" {
                let mut final_turn = String::new();
                for message in event["messages"].as_array().into_iter().flatten().rev() {
                    if message["role"] == "assistant" {
                        let mut combined = String::new();
                        for part in message["content"].as_array().into_iter().flatten() {
                            if part["type"] == "text" {
                                combined.push_str(part["text"].as_str().unwrap_or(""));
                            }
                        }
                        if !combined.trim().is_empty() {
                            final_turn = combined;
                            break;
                        }
                    }
                }
                if !final_turn.trim().is_empty() {
                    text = final_turn.trim_start().to_owned();
                } else {
                    text = text.trim_start().to_owned();
                }
            } else if text.is_empty() {
                for message in event["messages"].as_array().into_iter().flatten() {
                    if message["role"] == "assistant" {
                        for part in message["content"].as_array().into_iter().flatten() {
                            if part["type"] == "text" {
                                text.push_str(part["text"].as_str().unwrap_or(""));
                            }
                        }
                    }
                }
            }
            child.kill().await?;
            job.progress("writing", None, "Answer complete", 0).await?;
            return Ok(text);
        }
    }
    let status = child.wait().await?;
    let detail = errors.await.map_err(storage)??;
    Err(Failure::new(
        "provider_failed",
        format!("Pi exited ({status}): {detail}"),
    ))
}
