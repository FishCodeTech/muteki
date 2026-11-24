//! Read-only model discovery for an unsaved official Pi connection.
use super::{storage, text};
use crate::{AppState, agent_runtime, native_chat, runs};
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use std::{path::Path, process::Output, time::Duration};
use tokio::process::Command;

type Reply = Result<Value, (StatusCode, Json<Value>)>;

async fn runner_refresh(engine:&str,id:&str,body:&Value)->Option<Reply>{
    if text(body,"backend")!="container" {return None;}
    let socket=crate::runner::configured_socket()?;
    Some(crate::runner::exchange(&socket,"/v1/catalog/draft",Some(json!({"engine":engine,"credential_id":id,"provider":text(body,"provider")})))
        .await.map_err(|detail|(StatusCode::SERVICE_UNAVAILABLE,Json(json!({"detail":detail})))))
}

fn runtime_key(body: &Value) -> String {
    let requested = text(body, "runtime_instance");
    if requested.contains(':') {
        requested.to_owned()
    } else {
        format!(
            "cli.pi:{}",
            if requested.is_empty() {
                "default"
            } else {
                requested
            }
        )
    }
}

fn canonical_id(id: &str) -> String {
    if id.starts_with("account:") || id.starts_with("system:") {
        id.to_owned()
    } else {
        format!("account:{id}")
    }
}

fn isolated_env(command: &mut Command, agent_dir: &Path, home: &Path) {
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("PI_CODING_AGENT_DIR", agent_dir)
        .env("MUTEKI_HOST_DISCOVERY", "0")
        .env("NO_COLOR", "1");
    for key in [
        "OPENAI_API_KEY",
        "OPENAI_API_KEY_FILE",
        "OPENAI_BASE_URL",
        "MUTEKI_PI_PROVIDER",
        "MUTEKI_PI_MODEL",
        "PI_PROVIDER",
        "PI_MODEL",
    ] {
        command.env(key, "");
    }
}

async fn run(command: &mut Command, seconds: u64, input: Option<String>) -> Result<Output, agent_runtime::RuntimeError> {
    command.kill_on_drop(true);
    let operation = async {
        if let Some(input) = input {
            use tokio::io::AsyncWriteExt;
            command.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
            let mut child = command.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(input.as_bytes()).await?;
            }
            child.wait_with_output().await
        } else {
            command.output().await
        }
    };
    tokio::time::timeout(Duration::from_secs(seconds), operation)
        .await
        .map_err(|_| agent_runtime::RuntimeError::new("timeout", "模型发现超时"))?
        .map_err(|error| {
            agent_runtime::RuntimeError::new(
                if error.kind() == std::io::ErrorKind::NotFound {
                    "cli_missing"
                } else {
                    "catalog_request_failed"
                },
                error.to_string(),
            )
        })
}

async fn local(
    state: &AppState,
    body: &Value,
    script: &Path,
    scratch: &Path,
) -> Result<(Output, bool), agent_runtime::RuntimeError> {
    let descriptor = agent_runtime::descriptors::find("pi")
        .ok_or_else(|| agent_runtime::RuntimeError::new("cli_missing", "CLI 不存在"))?;
    let runtime = runtime_key(body);
    let configured = state
        .store
        .get(crate::runtime_api::INSTANCES, &runtime)
        .map_err(|error| {
            agent_runtime::RuntimeError::new("catalog_request_failed", error.to_string())
        })?;
    let explicit = configured
        .as_ref()
        .and_then(|row| row["binary_path"].as_str())
        .filter(|path| !path.is_empty())
        .map(Path::new);
    let binary = agent_runtime::binaries::resolve(descriptor, explicit)
        .await
        .map_err(|_| agent_runtime::RuntimeError::new("cli_missing", "CLI 不存在"))?;
    let agent_dir = scratch.join("agent");
    std::fs::create_dir_all(&agent_dir).map_err(|error| {
        agent_runtime::RuntimeError::new("catalog_request_failed", error.to_string())
    })?;
    let mut probe = crate::host_process::node_command();
    probe
        .arg(script)
        .arg(&binary.path)
        .stdin(std::process::Stdio::null());
    isolated_env(&mut probe, &agent_dir, scratch);
    if let Ok(path_env) = agent_runtime::binaries::launch_path(&binary.path) {
        probe.env("PATH", path_env);
    }
    match run(&mut probe, 35, None).await {
        Ok(output)
            if output.status.success()
                && output
                    .stdout
                    .iter()
                    .skip_while(|byte| byte.is_ascii_whitespace())
                    .next()
                    == Some(&b'{') =>
        {
            Ok((output, true))
        }
        Err(error) if error.code == "timeout" => Err(error),
        _ => {
            let mut fallback = crate::host_process::binary_command(&binary.path)?;
            fallback
                .arg("--list-models")
                .stdin(std::process::Stdio::null());
            isolated_env(&mut fallback, &agent_dir, scratch);
            run(&mut fallback, 45, None).await.map(|output| (output, false))
        }
    }
}

async fn container(script: &Path) -> Result<(Output, bool), agent_runtime::RuntimeError> {
    let image = runs::container_exec::resolve_worker_image();
    let projected = runs::container_exec::project_worker_network("bridge");
    let network = projected.effective.ok_or_else(|| {
        agent_runtime::RuntimeError::new(
            "catalog_request_failed",
            projected
                .error
                .unwrap_or_else(|| "Worker container network configuration is invalid".to_owned()),
        )
    })?;
    let source = std::fs::canonicalize(script).map_err(|error| {
        agent_runtime::RuntimeError::new("catalog_request_failed", error.to_string())
    })?;
    let mount = format!(
        "type=bind,source={},target=/tmp/muteki-pi-model-catalog.mjs,readonly",
        source.display()
    );
    let mut probe = crate::host_process::command("docker");
    probe
        .args([
            "run",
            "--rm",
            "--pull=never",
            "--network",
            &network,
            "--user",
            "kali",
            "--entrypoint",
            "node",
            "--mount",
            &mount,
            "--env",
            "HOME=/tmp/muteki-pi-draft",
            "--env",
            "PI_CODING_AGENT_DIR=/tmp/muteki-pi-draft/agent",
            "--env",
            "XDG_CONFIG_HOME=/tmp/muteki-pi-draft/.config",
            "--env",
            "MUTEKI_HOST_DISCOVERY=0",
            "--env",
            "OPENAI_API_KEY=",
            "--env",
            "OPENAI_API_KEY_FILE=",
            "--env",
            "OPENAI_BASE_URL=",
            "--env",
            "MUTEKI_PI_PROVIDER=",
            "--env",
            "MUTEKI_PI_MODEL=",
            "--env",
            "PI_PROVIDER=",
            "--env",
            "PI_MODEL=",
            &image,
            "/tmp/muteki-pi-model-catalog.mjs",
            "pi",
        ])
        .stdin(std::process::Stdio::null());
    run(&mut probe, 75, None).await.map(|output| (output, true))
}

async fn container_cli(engine: &str) -> Result<Output, agent_runtime::RuntimeError> {
    let descriptor = agent_runtime::descriptors::find(engine)
        .ok_or_else(|| agent_runtime::RuntimeError::new("cli_missing", "CLI 不存在"))?;
    let image = runs::container_exec::resolve_worker_image();
    let projected = runs::container_exec::project_worker_network("bridge");
    let network = projected.effective.ok_or_else(|| {
        agent_runtime::RuntimeError::new("catalog_request_failed", projected.error.unwrap_or_default())
    })?;
    let make_command = |argv: &[&str]| {
        let mut command = crate::host_process::command("docker");
        command.args(["run", "--rm", "--pull=never", "--network", &network,
            "--user", "kali", "--entrypoint", crate::native_credentials::container_binary(engine),
            "--env", "HOME=/tmp/muteki-cli-catalog-draft",
            "--env", "XDG_CONFIG_HOME=/tmp/muteki-cli-catalog-draft/.config",
            "--env", "XDG_DATA_HOME=/tmp/muteki-cli-catalog-draft/.local/share",
            "--env", "XDG_CACHE_HOME=/tmp/muteki-cli-catalog-draft/.cache",
            "--env", "MUTEKI_HOST_DISCOVERY=0", "--env", "NO_COLOR=1"]);
        for key in descriptor.public()["credentials"]["env_keys"].as_array().into_iter().flatten() {
            if let Some(key) = key.as_str() { command.arg("--env").arg(format!("{key}=")); }
        }
        match engine {
            "kimi" => { command.args(["--env", "KIMI_CODE_HOME=/tmp/muteki-cli-catalog-draft/.kimi-code"]); }
            "omp" => { command.args(["--env", "PI_CODING_AGENT_DIR=/tmp/muteki-cli-catalog-draft/.omp/agent"]); }
            "grok" => { command.args(["--env", "GROK_HOME=/tmp/muteki-cli-catalog-draft/.grok"]); }
            "opencode" => { command.args(["--env", "OPENCODE_CONFIG_DIR=/tmp/muteki-cli-catalog-draft/.config/opencode"]); }
            _ => {}
        }
        if agent_runtime::catalog::catalog_stdin(&descriptor.public()["models"]).is_some() { command.arg("--interactive"); }
        command.arg(&image).args(argv);
        command
    };
    let argv: Vec<&str> = descriptor.public()["models"]["argv"].as_array().into_iter().flatten()
        .filter_map(Value::as_str).collect();
    let mut command = make_command(&argv);
    let input = agent_runtime::catalog::catalog_stdin(&descriptor.public()["models"]);
    let output = run(&mut command, 75, input).await?;
    if engine == "opencode" && (!output.status.success()
        || agent_runtime::catalog::parse_cli_output("opencode_models", &String::from_utf8_lossy(&output.stdout), "opencode models --verbose")
            .map_or(true, |models| models.is_empty())) {
        let mut fallback = make_command(&["models"]);
        return run(&mut fallback, 75, None).await;
    }
    Ok(output)
}

fn parsed_models(stdout: &str, rpc: bool, provider: &str) -> Vec<Value> {
    let mut models = Vec::new();
    if rpc {
        let payload: Value = match serde_json::from_str(stdout) {
            Ok(value) => value,
            Err(_) => return models,
        };
        for row in payload["models"].as_array().into_iter().flatten() {
            let Some(id) = row["id"]
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            let selected_provider = row["provider"].as_str().unwrap_or("");
            if !provider.is_empty()
                && selected_provider != provider
                && !id.starts_with(&format!("{provider}/"))
            {
                continue;
            }
            if models.iter().any(|model: &Value| model["id"] == id) {
                continue;
            }
            let label = row["label"]
                .as_str()
                .filter(|v| !v.is_empty())
                .unwrap_or(id);
            let mut model = json!({"id":id,"label":label,
                "reasoning":agent_runtime::catalog::reasoning(&row["levels"], &row["default"], "pi_cli_rpc")});
            if !selected_provider.is_empty() {
                model["provider"] = json!(selected_provider);
            }
            if let Some(inputs) = row["input"]
                .as_array()
                .filter(|rows| !rows.is_empty() && rows.iter().all(Value::is_string))
            {
                let mut unique = Vec::<Value>::new();
                for input in inputs {
                    if !unique.contains(input) {
                        unique.push(input.clone());
                    }
                }
                model["input"] = json!(unique);
            }
            models.push(model);
        }
    } else {
        for line in stdout.lines().skip(1) {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 2 {
                continue;
            }
            let (selected_provider, id) = (fields[0], fields[1]);
            if !provider.is_empty()
                && selected_provider != provider
                && !id.starts_with(&format!("{provider}/"))
            {
                continue;
            }
            if models.iter().any(|model: &Value| model["id"] == id) {
                continue;
            }
            models.push(json!({"id":id,"label":format!("{id} ({selected_provider})"),
                "provider":selected_provider,
                "reasoning":agent_runtime::catalog::reasoning(&Value::Null, &Value::Null, "pi_names_only")}));
        }
    }
    models
}

fn detail(output: &Output) -> String {
    let code = output.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = [stderr.as_ref(), stdout.as_ref()]
        .into_iter()
        .find_map(|text| text.trim().lines().last().filter(|line| !line.is_empty()));
    match line {
        Some(line) => format!(
            "模型测试退出 {code}：{}",
            line.chars().take(300).collect::<String>()
        ),
        None => format!("模型测试退出 {code}"),
    }
}

pub(super) async fn refresh_pi(state: &AppState, id: &str, body: &Value) -> Reply {
    if let Some(reply)=runner_refresh("pi",id,body).await{return reply;}
    let root = native_chat::state_dir(state)
        .join("_tmp")
        .join("credential-drafts");
    std::fs::create_dir_all(&root).map_err(storage)?;
    let scratch = tempfile::Builder::new()
        .prefix("muteki-pi-catalog-draft-")
        .tempdir_in(&root)
        .map_err(storage)?;
    let script = scratch.path().join("pi-model-catalog.mjs");
    std::fs::write(
        &script,
        include_str!(
            "../../../../docker/worker-common/pi_model_catalog.mjs"
        ),
    )
    .map_err(storage)?;
    let result = if text(body, "backend") == "container" {
        container(&script).await
    } else {
        local(state, body, &script, scratch.path()).await
    };
    let (ok, models, detail, source, error_code) = match result {
        Ok((output, rpc)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let models = parsed_models(&stdout, rpc, text(body, "provider"));
            let success = output.status.success() && !models.is_empty();
            let code = if success {
                ""
            } else {
                let log = format!("{}\n{}", stdout, String::from_utf8_lossy(&output.stderr))
                    .to_lowercase();
                if [
                    "not logged in",
                    "login required",
                    "unauthorized",
                    "authentication",
                    "invalid api key",
                    "http 401",
                    "http 403",
                ]
                .iter()
                .any(|marker| log.contains(marker))
                {
                    "not_logged_in"
                } else if output.status.success() {
                    "model_catalog_unsupported"
                } else {
                    "catalog_request_failed"
                }
            };
            let message = if success {
                format!("发现 {} 个模型", models.len())
            } else {
                detail(&output)
            };
            (success, models, message, "pi_cli", code)
        }
        Err(error) => (false, Vec::new(), error.message, "cli", error.code),
    };
    let ids: Vec<Value> = models.iter().map(|model| model["id"].clone()).collect();
    Ok(json!({"ok":ok,"detail":detail,"source":source,"models":ids,
        "credential_id":canonical_id(id),
        "catalog":{"discovered_models":models,"draft":true},"error_code":error_code}))
}

fn cli_runtime_key(engine: &str, body: &Value) -> String {
    let requested = text(body, "runtime_instance");
    if requested.contains(':') {
        return requested.to_owned();
    }
    let default = crate::runtime_api::default_runtime_key(engine)
        .unwrap_or_else(|| format!("cli.{engine}:default"));
    if requested.is_empty() || requested == "default" {
        default
    } else {
        format!(
            "{}:{requested}",
            default
                .rsplit_once(':')
                .map(|(adapter, _)| adapter)
                .unwrap_or(&default)
        )
    }
}

async fn local_cli(
    state: &AppState,
    engine: &str,
    body: &Value,
    scratch: &Path,
) -> Result<Output, agent_runtime::RuntimeError> {
    let descriptor = agent_runtime::descriptors::find(engine)
        .ok_or_else(|| agent_runtime::RuntimeError::new("cli_missing", "CLI 不存在"))?;
    let runtime = cli_runtime_key(engine, body);
    let configured = state
        .store
        .get(crate::runtime_api::INSTANCES, &runtime)
        .map_err(|error| {
            agent_runtime::RuntimeError::new("catalog_request_failed", error.to_string())
        })?;
    let explicit = configured
        .as_ref()
        .and_then(|row| row["binary_path"].as_str())
        .filter(|path| !path.is_empty())
        .map(Path::new);
    let binary = agent_runtime::binaries::resolve(descriptor, explicit)
        .await
        .map_err(|_| agent_runtime::RuntimeError::new("cli_missing", "CLI 不存在"))?;
    let home = scratch.join("home");
    let native = match engine {
        "kimi" => home.join(".kimi-code"),
        "omp" => home.join(".omp/agent"),
        "grok" => home.join(".grok"),
        "opencode" => home.join(".config/opencode"),
        _ => home.join(".factory"),
    };
    std::fs::create_dir_all(&native).map_err(|error| {
        agent_runtime::RuntimeError::new("catalog_request_failed", error.to_string())
    })?;
    let make_command = |argv: &[&str]| {
        let mut command = crate::host_process::binary_command(&binary.path)
            .unwrap_or_else(|_| crate::host_process::command(&binary.path));
        command
            .args(argv)
            .stdin(std::process::Stdio::null())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("MUTEKI_HOST_DISCOVERY", "0")
            .env("NO_COLOR", "1");
        for key in descriptor.public()["credentials"]["env_keys"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(key) = key.as_str() {
                command.env(key, "");
            }
        }
        match engine {
            "kimi" => { command.env("KIMI_CODE_HOME", &native); }
            "omp" => { command.env("PI_CODING_AGENT_DIR", &native); }
            "grok" => { command.env("GROK_HOME", &native); }
            "opencode" => { command.env("OPENCODE_CONFIG_DIR", &native); }
            _ => {}
        }
        command
    };
    let argv: Vec<&str> = descriptor.public()["models"]["argv"].as_array().into_iter().flatten()
        .filter_map(Value::as_str).collect();
    let mut command = make_command(&argv);
    let input = agent_runtime::catalog::catalog_stdin(&descriptor.public()["models"]);
    let output = run(&mut command, 45, input).await?;
    if engine == "opencode" && (!output.status.success()
        || agent_runtime::catalog::parse_cli_output("opencode_models", &String::from_utf8_lossy(&output.stdout), "opencode models --verbose")
            .map_or(true, |models| models.is_empty())) {
        let mut fallback = make_command(&["models"]);
        return run(&mut fallback, 45, None).await;
    }
    Ok(output)
}

pub(super) async fn refresh_cli(state: &AppState, id: &str, body: &Value, engine: &str) -> Reply {
    if let Some(reply)=runner_refresh(engine,id,body).await{return reply;}
    let root = native_chat::state_dir(state)
        .join("_tmp")
        .join("credential-drafts");
    std::fs::create_dir_all(&root).map_err(storage)?;
    let scratch = tempfile::Builder::new()
        .prefix("muteki-cli-catalog-draft-")
        .tempdir_in(&root)
        .map_err(storage)?;
    let result = if text(body, "backend") == "container" {
        container_cli(engine).await
    } else {
        local_cli(state, engine, body, scratch.path()).await
    };
    let source = format!("{engine}_cli");
    let (ok, models, detail, source, error_code) = match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let parser = agent_runtime::descriptors::find(engine)
                .and_then(|descriptor| descriptor.public()["models"]["parser"].as_str().map(str::to_owned))
                .unwrap_or_default();
            let parsed = agent_runtime::catalog::parse_cli_output(&parser, &stdout, engine)
                .unwrap_or_default();
            let provider = text(body, "provider").trim().to_lowercase();
            let provider_scoped = agent_runtime::descriptors::find(engine)
                .is_some_and(|descriptor| descriptor.public()["models"]["provider_scoped"] == true);
            let mut models = Vec::<Value>::new();
            for row in parsed {
                let Some(model_id) = row["id"].as_str() else {
                    continue;
                };
                if provider_scoped && !provider.is_empty() {
                    let selected = row["provider"].as_str().unwrap_or("").trim().to_lowercase();
                    if selected != provider
                        && !model_id.to_lowercase().starts_with(&format!("{provider}/"))
                    {
                        continue;
                    }
                }
                if !models.iter().any(|model| model["id"] == model_id) {
                    models.push(row);
                }
            }
            let ok = output.status.success() && !models.is_empty();
            let log =
                format!("{}\n{}", stdout, String::from_utf8_lossy(&output.stderr)).to_lowercase();
            let code = if ok {
                ""
            } else if [
                "not logged in",
                "login required",
                "unauthorized",
                "authentication",
                "invalid api key",
                "http 401",
                "http 403",
            ]
            .iter()
            .any(|marker| log.contains(marker))
            {
                "not_logged_in"
            } else if output.status.success() {
                "model_catalog_unsupported"
            } else {
                "catalog_request_failed"
            };
            let message = if ok {
                format!("发现 {} 个模型", models.len())
            } else {
                detail(&output)
            };
            (ok, models, message, source, code.to_owned())
        }
        Err(error) => (
            false,
            Vec::new(),
            error.message,
            "cli".to_owned(),
            error.code.to_owned(),
        ),
    };
    let ids: Vec<Value> = models.iter().map(|model| model["id"].clone()).collect();
    Ok(json!({"ok":ok,"detail":detail,"source":source,"models":ids,
        "credential_id":canonical_id(id),
        "catalog":{"discovered_models":models,"draft":true},"error_code":error_code}))
}
