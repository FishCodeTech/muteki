//! Server-only HTTP provider resolution. Credentials never enter the public catalog.
use crate::store::Store;
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub struct TextProvider {
    pub endpoint: String,
    pub model: String,
    pub token: Option<String>,
    pub credential_id: String,
    pub configuration_revision: Option<String>,
    pub protocol: String,
}
fn string<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
fn endpoint(base: &str, exact: bool) -> Result<String> {
    let mut url =
        reqwest::Url::parse(base).map_err(|_| anyhow::anyhow!("Invalid provider endpoint"))?;
    if !["http", "https"].contains(&url.scheme())
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "Provider endpoint must be an HTTP URL without embedded credentials or query parameters"
        );
    }
    if !exact
        && !url
            .path()
            .trim_end_matches('/')
            .ends_with("/chat/completions")
    {
        let path = format!("{}/chat/completions", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url.to_string())
}
fn account_id<'a>(value: &'a str) -> &'a str {
    value
        .strip_prefix("account:")
        .or_else(|| value.strip_prefix("endpoint:"))
        .or_else(|| value.strip_prefix("secret://credential-accounts/"))
        .unwrap_or(value)
}
fn model_id(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| string(value, "id"))
        .or_else(|| string(value, "model"))
}
fn model_values(row: &Value) -> Vec<Value> {
    let mut values = Vec::new();
    for key in [
        "models",
        "configured_models",
        "discovered_models",
        "verified_models",
    ] {
        for value in row[key].as_array().into_iter().flatten() {
            if let Some(id) = model_id(value) {
                if !values.iter().any(|m: &Value| m["id"] == id) {
                    let mut model = json!({"id":id,"label":string(value,"label").unwrap_or(id)});
                    for field in [
                        "provider",
                        "input",
                        "reasoning",
                        "service_tiers",
                        "variant",
                        "default_service_tier",
                    ] {
                        if let Some(v) = value.get(field) {
                            model[field] = v.clone();
                        }
                    }
                    values.push(model);
                }
            }
        }
    }
    if let Some(id) = string(row, "default_model") {
        if !values.iter().any(|m| m["id"] == id) {
            values.push(json!({"id":id,"label":id}));
        }
    }
    values
}
pub(crate) fn matching_account(store: &Store, selected: &str) -> Result<Option<Value>> {
    if let Some(id) = selected.strip_prefix("endpoint:") {
        return Ok(store
            .get("model_endpoints", id)?
            .or(store.get("credential_accounts", id)?));
    }
    let id = account_id(selected);
    if let Some(account) = store.get("credential_accounts", id)? {
        return Ok(Some(account));
    }
    if let Some(credential) = store.get("credentials", selected)? {
        if let Some(reference) = string(&credential, "secret_ref") {
            return Ok(store.get("credential_accounts", account_id(reference))?);
        }
    }
    Ok(None)
}
pub(crate) fn provider_from(
    account: &Value,
    selected: &str,
    model: Option<&str>,
) -> Result<TextProvider> {
    let base = string(account,"base_url").or_else(||account.get("endpoint").and_then(|e|string(e,"base_url"))).ok_or_else(||anyhow::anyhow!("Selected credential has no HTTP endpoint; its CLI login was preserved but cannot serve HTTP text chat"))?;
    let protocol = string(account, "wire_api")
        .or_else(|| account.get("endpoint").and_then(|e| string(e, "wire_api")))
        .unwrap_or("chat_completions");
    if ![
        "",
        "chat_completions",
        "chat-completions",
        "openai",
        "openai-completions",
    ]
    .contains(&protocol)
    {
        bail!("Selected credential requires an unsupported HTTP protocol");
    }
    let model = model
        .or_else(|| string(account, "default_model"))
        .or_else(|| {
            account["models"]
                .as_array()
                .and_then(|rows| rows.first())
                .and_then(model_id)
        })
        .ok_or_else(|| anyhow::anyhow!("Select a model for this credential"))?;
    let token = string(account, "secret").map(str::to_owned);
    Ok(TextProvider {
        endpoint: endpoint(base, false)?,
        model: model.to_owned(),
        token,
        credential_id: selected.to_owned(),
        configuration_revision: string(account,"revision").map(str::to_owned),
        protocol: "chat_completions".into(),
    })
}
/// Explicit selections never fall back to another credential or environment key.
/// Runtime selection accepts credential_id/credentialId/endpoint_id and model/model_id/modelId.
pub fn resolve(store: &Store, runtime: &Value) -> Result<TextProvider> {
    let selected = string(runtime, "credential_id")
        .or_else(|| string(runtime, "credentialId"))
        .or_else(|| string(runtime, "endpoint_id"));
    let model = string(runtime, "model")
        .or_else(|| string(runtime, "model_id"))
        .or_else(|| string(runtime, "modelId"));
    if let Some(selected) = selected {
        if selected != "system:openai" && selected != "environment" {
            let account = matching_account(store, selected)?
                .ok_or_else(|| anyhow::anyhow!("Selected credential does not exist"))?;
            return provider_from(&account, selected, model);
        }
    }
    let base = std::env::var("MUTEKI_CHAT_ENDPOINT").map_err(|_| {
        anyhow::anyhow!("Select an imported HTTP credential or configure MUTEKI_CHAT_ENDPOINT")
    })?;
    let env_model = std::env::var("MUTEKI_CHAT_MODEL").ok();
    let model = model
        .or(env_model.as_deref().filter(|s| !s.trim().is_empty()))
        .ok_or_else(|| anyhow::anyhow!("Select a model or configure MUTEKI_CHAT_MODEL"))?;
    Ok(TextProvider {
        endpoint: endpoint(&base, true)?,
        model: model.to_owned(),
        token: std::env::var("MUTEKI_CHAT_TOKEN")
            .ok()
            .filter(|s| !s.is_empty()),
        credential_id: "system:openai".into(),
        configuration_revision: None,
        protocol: "chat_completions".into(),
    })
}
/// Whitelisted metadata for the browser. Presence means configured, never connectivity-tested.
pub fn public_catalog(store: &Store) -> Result<Vec<Value>> {
    public_catalog_for_environment(store, "local")
}

pub fn public_catalog_for_environment(store: &Store, environment: &str) -> Result<Vec<Value>> {
    let catalogs = store.list("model_catalogs")?;
    let tests = store.list("model_test_results")?;
    let matches_environment = |row: &Value, key: &str| {
        row[key].as_str().unwrap_or("local") == environment
    };
    let mut result = Vec::new();
    for account in store.list("credential_accounts")? {
        let Some(id) = string(&account, "account_id").or_else(|| string(&account, "id")) else {
            continue;
        };
        let credential_id = format!("account:{id}");
        let mut models = model_values(&account);
        for catalog in &catalogs {
            if matches_environment(catalog, "environment")
                && string(catalog, "credential_id").is_some_and(|c| account_id(c) == id) {
                for model in model_values(catalog) {
                    if !models.iter().any(|m| m["id"] == model["id"]) {
                        models.push(model);
                    }
                }
            }
        }
        let usable =
            provider_from(&account, &credential_id, models.first().and_then(model_id)).is_ok();
        result.push(json!({"id":credential_id,"account_id":id,"credential_id":credential_id,"label":string(&account,"label").unwrap_or(id),"engine":string(&account,"engine").unwrap_or("api"),"provider":account["provider"],"models":models,"default_model":account["default_model"],"present":account["present"].as_bool().unwrap_or_else(||string(&account,"secret").is_some()),"http_compatible":usable,"status":if usable{"configured"}else{"unavailable"},"status_detail":if usable{"HTTP text configuration imported; connectivity has not been tested"}else{"Credential files preserved; this login requires a CLI runtime"}}));
    }
    for account in store.list("model_endpoints")? {
        if let Some(raw_id) = string(&account, "id").map(account_id) {
            if store
                .get("credential_accounts", raw_id)?
                .as_ref()
                .is_some_and(|a| crate::store::same_http_identity(a, &account))
            {
                continue;
            }
        }
        if let Some(id) = string(&account, "id") {
            result.push(json!({"id":id,"credential_id":id,"label":string(&account,"label").unwrap_or(id),"engine":"api","models":model_values(&account),"default_model":account["default_model"],"http_compatible":provider_from(&account,id,None).is_ok(),"status":"configured"}));
        }
    }
    if let Ok(provider) = resolve(store, &json!({})) {
        result.push(json!({"id":"system:openai","credential_id":"system:openai","label":"Environment HTTP provider","engine":"openai","models":[{"id":provider.model,"label":provider.model}],"default_model":provider.model,"http_compatible":true,"present":true,"status":"configured"}));
    }
    for row in &mut result {
        let id = row["id"].as_str().unwrap_or("").to_owned();
        let mut public_catalogs = serde_json::Map::new();
        for catalog in &catalogs {
            if catalog["credential_id"] != id || !matches!(catalog["source"].as_str(), Some("http_openai_models" | "http_anthropic_models" | "pi_cli_rpc"))
                || !matches_environment(catalog, "environment") {
                continue;
            }
            let mut public = json!({});
            for field in [
                "credential_id",
                "engine",
                "environment",
                "runtime_instance",
                "configured_models",
                "discovered_models",
                "verified_models",
                "default_model",
                "source",
                "refresh_status",
                "refreshed_at",
                "expires_at",
                "last_error",
                "error_code",
            ] {
                if let Some(value) = catalog.get(field) {
                    public[field] = value.clone();
                }
            }
            public_catalogs.insert(
                string(catalog, "runtime_instance")
                    .unwrap_or("default")
                    .into(),
                public,
            );
            for model in model_values(catalog) {
                if !row["models"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|v| v["id"] == model["id"]))
                {
                    if let Some(items) = row["models"].as_array_mut() {
                        items.push(model);
                    }
                }
            }
        }
        row["model_catalogs"] = json!(public_catalogs);
        let mut public_tests = serde_json::Map::new();
        let mut latest = None::<Value>;
        for test in &tests {
            if test["credential_id"] != id || !matches_environment(test, "backend") {
                continue;
            }
            public_tests.insert(
                string(test, "runtime_instance").unwrap_or("default").into(),
                test.clone(),
            );
            if latest.as_ref().is_none_or(|v| {
                v["tested_at"].as_f64().unwrap_or(0.0) < test["tested_at"].as_f64().unwrap_or(0.0)
            }) {
                latest = Some(test.clone());
            }
        }
        row["last_tests"] = json!(public_tests);
        row["last_test"] = latest.unwrap_or(Value::Null);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_selected_account_and_redacts_catalog() {
        let store = Store::open(":memory:").unwrap();
        store.put("credential_accounts","sample",&json!({"id":"sample","account_id":"sample","secret":"synthetic-test-key","base_url":"https://example.invalid/v1","models":["test-model"],"default_model":"test-model","engine":"pi"})).unwrap();
        store
            .put("credentials", "custom", &json!({"secret_ref":"sample"}))
            .unwrap();
        let p = resolve(
            &store,
            &json!({"credential_id":"custom","model":"chosen-model"}),
        )
        .unwrap();
        assert_eq!(p.endpoint, "https://example.invalid/v1/chat/completions");
        assert_eq!(p.model, "chosen-model");
        assert_eq!(p.token.as_deref(), Some("synthetic-test-key"));
        assert!(
            resolve(
                &store,
                &json!({"credential_id":"missing","model":"chosen-model"})
            )
            .is_err()
        );
        let catalog = public_catalog(&store).unwrap();
        assert_eq!(catalog[0]["http_compatible"], true);
        assert!(
            !serde_json::to_string(&catalog)
                .unwrap()
                .contains("synthetic-test-key")
        );
        assert!(
            !serde_json::to_string(&catalog)
                .unwrap()
                .contains("base_url")
        );
    }
    #[test]
    fn endpoint_rejects_embedded_credentials() {
        assert!(endpoint("https://user:password@example.invalid/v1", false).is_err());
        assert!(endpoint("file:///tmp/provider", false).is_err());
        assert!(endpoint("https://example.invalid/v1?token=test", false).is_err());
        assert_eq!(
            endpoint("http://localhost:8080/v1/chat/completions", false).unwrap(),
            "http://localhost:8080/v1/chat/completions"
        );
    }
}
