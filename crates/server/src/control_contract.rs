//! Pinned control parameters and Muteki runtime configuration operations.
pub use tcode_control_mcp::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ControlThreadParams {
    pub changes: Vec<RuntimeChange>,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct RuntimeChange {
    pub thread_id: String,
    pub op: RuntimeOperation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RuntimeOperation {
    Runtime { set_runtime: RuntimePatch },
    Native(tcode_control_mcp::RunOp),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimePatch {
    pub adapter_id: Option<String>,
    pub instance_id: Option<String>,
    pub credential_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    pub interaction_mode: Option<String>,
    pub access_mode: Option<String>,
    pub options: Option<serde_json::Map<String, serde_json::Value>>,
}
