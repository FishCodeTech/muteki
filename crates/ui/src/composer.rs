//! Per-conversation drafts and explicit provider selection.
use serde_json::{Value, json};
#[derive(Clone, Default, PartialEq)]
pub struct Draft {
    pub text: String,
    pub credential: String,
    pub model: String,
}
pub fn runtime(credential: &str, model: &str) -> Value {
    json!({"adapter_id":"openai","instance_id":"text","credential_id":credential,"model":model})
}
pub fn supported(catalog: &[Value], credential: &str, model: &str) -> bool {
    !credential.is_empty()
        && !model.is_empty()
        && catalog.iter().any(|c| {
            c["credential_id"] == credential
                && c["http_compatible"] == true
                && c["models"]
                    .as_array()
                    .is_some_and(|m| m.iter().any(|m| m["id"] == model))
        })
}
pub fn default_choice(prefs: &Value, catalog: &[Value]) -> (String, String) {
    let default = &prefs["values"]["defaultModel"];
    let credential = default["credentialId"].as_str().unwrap_or("");
    let model = default["modelId"].as_str().unwrap_or("");
    if supported(catalog, credential, model) {
        (credential.into(), model.into())
    } else {
        (String::new(), String::new())
    }
}
pub fn credential_model(catalog: &[Value], credential: &str) -> String {
    catalog
        .iter()
        .find(|c| c["credential_id"] == credential)
        .and_then(|c| {
            let default = c["default_model"].as_str()?;
            c["models"]
                .as_array()?
                .iter()
                .any(|m| m["id"] == default)
                .then(|| default.to_owned())
        })
        .unwrap_or_default()
}
