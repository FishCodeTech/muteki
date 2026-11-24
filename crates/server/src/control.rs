//! Project-scoped grants and thread lifecycle for `muteki-control`.
//!
//! Threads, including delegated children, use their project grant unless overridden.
//! A child cannot inspect or act on its ancestors.

use crate::{AppState, capabilities, chat};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

pub type Failure = (StatusCode, Json<Value>);
type Api = Result<Json<Value>, Failure>;

pub(crate) const SETTINGS: &str = "control_settings";
pub(crate) const THREADS: &str = "control_threads";
pub(crate) const ACTIVITY: &str = "control_activity";
pub(crate) const DISCUSSIONS: &str = "control_discussions";
pub(crate) const GLOBAL: &str = "global";
pub const OWN_THREAD: &str = "self";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Observe,
    Organize,
    Operate,
}

/// Dangerous capabilities that stay off until the user grants them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Answering another user thread's pending question in the user's place.
    AnswerInput,
    /// Importing credentials or host accounts.
    Credentials,
    /// Submitting an answer to a competition platform.
    CompetitionSubmit,
    /// Changing worker, identity, competition or control policy settings.
    SettingsWrite,
    /// Opening, typing into, interrupting or closing interactive PTY terminals.
    Terminal,
    /// Permanently deleting threads or worktrees.
    Delete,
    /// Pushing, pulling, publishing branches or merging worktrees.
    GitRemote,
    /// Commenting or replying on GitHub pull requests.
    PrWrite,
    /// Rewinding file/conversation history or dropping human-queued messages.
    HistoryEdit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Only threads of the caller's own project.
    Project,
    Projects(Vec<String>),
    Host,
}

impl Scope {
    pub fn reaches(&self, caller_project: &str, target_project: &str) -> bool {
        match self {
            Scope::Host => true,
            Scope::Project => caller_project == target_project,
            Scope::Projects(list) => {
                caller_project == target_project || list.iter().any(|id| id == target_project)
            }
        }
    }

    /// Whether everything `self` reaches, `granted` reaches too.
    pub fn within(&self, granted: &Scope) -> bool {
        match (self, granted) {
            (_, Scope::Host) => true,
            (Scope::Host, _) => false,
            (Scope::Project, _) => true,
            (Scope::Projects(wanted), Scope::Project) => wanted.is_empty(),
            (Scope::Projects(wanted), Scope::Projects(have)) => {
                wanted.iter().all(|id| have.contains(id))
            }
        }
    }

    fn widen(&self, other: &Scope) -> Scope {
        match (self, other) {
            (Scope::Host, _) | (_, Scope::Host) => Scope::Host,
            (Scope::Project, Scope::Project) => Scope::Project,
            (Scope::Projects(a), Scope::Projects(b)) => {
                let mut all: Vec<String> = a.iter().chain(b).cloned().collect();
                all.sort();
                all.dedup();
                Scope::Projects(all)
            }
            (Scope::Projects(list), Scope::Project) | (Scope::Project, Scope::Projects(list)) => {
                Scope::Projects(list.clone())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub scope: Scope,
    #[serde(default)]
    pub tiers: BTreeSet<Tier>,
    #[serde(default)]
    pub categories: BTreeSet<Category>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantDelta {
    pub added_tiers: Vec<Tier>,
    pub removed_tiers: Vec<Tier>,
    pub added_categories: Vec<Category>,
    pub removed_categories: Vec<Category>,
    pub scope_changed: bool,
}

impl Grant {
    /// Normal threads and delegated agents hold full internal control within the workspace
    /// (ancestor protection and same-SRC-Task routing still apply separately).
    pub fn standard() -> Self {
        Self {
            scope: Scope::Host,
            tiers: [Tier::Observe, Tier::Organize, Tier::Operate].into(),
            categories: [
                Category::AnswerInput,
                Category::Credentials,
                Category::CompetitionSubmit,
                Category::SettingsWrite,
                Category::Terminal,
                Category::Delete,
                Category::GitRemote,
                Category::PrWrite,
                Category::HistoryEdit,
            ]
            .into(),
        }
    }

    /// Subagents use the same internal tool grant (restricted only by ancestor & SRC Task boundaries).
    pub fn none() -> Self {
        Self::standard()
    }

    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty() && self.categories.is_empty() && self.scope == Scope::Project
    }

    /// A grant approval only ever widens: tiers and categories are united.
    pub fn merge(&self, other: &Grant) -> Grant {
        Grant {
            scope: self.scope.widen(&other.scope),
            tiers: self.tiers.union(&other.tiers).copied().collect(),
            categories: self.categories.union(&other.categories).copied().collect(),
        }
    }

    pub fn delta_from(&self, base: &Grant) -> GrantDelta {
        GrantDelta {
            added_tiers: self.tiers.difference(&base.tiers).copied().collect(),
            removed_tiers: base.tiers.difference(&self.tiers).copied().collect(),
            added_categories: self.categories.difference(&base.categories).copied().collect(),
            removed_categories: base.categories.difference(&self.categories).copied().collect(),
            scope_changed: self.scope != base.scope,
        }
    }

    pub fn describe(&self) -> String {
        let join = |names: Vec<String>| {
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            }
        };
        let scope = match &self.scope {
            Scope::Project => "own project".to_owned(),
            Scope::Projects(list) => format!("own project and {} other project(s)", list.len()),
            Scope::Host => "whole host".to_owned(),
        };
        format!(
            "tiers: {}; categories: {}; scope: {scope}",
            join(self.tiers.iter().map(|tier| tier_name(*tier).to_owned()).collect()),
            join(
                self.categories
                    .iter()
                    .map(|category| category_name(*category).to_owned())
                    .collect()
            ),
        )
    }
}

pub fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Observe => "observe",
        Tier::Organize => "organize",
        Tier::Operate => "operate",
    }
}

pub fn category_name(category: Category) -> &'static str {
    match category {
        Category::AnswerInput => "answer_input",
        Category::Credentials => "credentials",
        Category::CompetitionSubmit => "competition_submit",
        Category::SettingsWrite => "settings_write",
        Category::Terminal => "terminal",
        Category::Delete => "delete",
        Category::GitRemote => "git_remote",
        Category::PrWrite => "pr_write",
        Category::HistoryEdit => "history_edit",
    }
}


fn default_approval_mode() -> String { "always_allow".to_owned() }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPolicy {
    #[serde(default = "default_approval_mode")]
    pub approval_mode: String,
    #[serde(default)]
    pub child_worktrees: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_role: String,
}
impl Default for SubagentPolicy {
    fn default() -> Self {
        Self {
            approval_mode: default_approval_mode(),
            child_worktrees: false,
            default_role: String::new(),
        }
    }
}
impl SubagentPolicy {
    pub fn sanitized(mut self) -> Self {
        if self.approval_mode == "full-access" { self.approval_mode = "always_allow".into(); }
        self.default_role = self.default_role.trim().to_ascii_lowercase().replace(' ', "_");
        self
    }
}

fn default_true() -> bool {
    true
}
fn default_role_mode() -> String {
    "inherit".to_owned()
}
fn default_orchestration_preset() -> String {
    "balanced".to_owned()
}
fn default_orchestration_roles() -> Vec<OrchestrationRole> {
    vec![
        OrchestrationRole {
            id: "planner".to_owned(),
            label: "规划与架构".to_owned(),
            description: "需求拆解、技术选型与实现计划。".to_owned(),
            enabled: true,
            mode: "inherit".to_owned(),
            provider: String::new(),
            profile: String::new(),
            model: String::new(),
            effort: "high".to_owned(),
            guidelines: "输出清晰步骤、文件影响面和验收条件；不确定的架构决策先指出权衡。".to_owned(),
        },
        OrchestrationRole {
            id: "implementer".to_owned(),
            label: "编码实现".to_owned(),
            description: "编写功能、修复缺陷并运行检查。".to_owned(),
            enabled: true,
            mode: "inherit".to_owned(),
            provider: String::new(),
            profile: String::new(),
            model: String::new(),
            effort: "medium".to_owned(),
            guidelines: "先读现有调用链再做最小修改，保留无关代码与注释，改完必须跑对应检查。".to_owned(),
        },
        OrchestrationRole {
            id: "reviewer".to_owned(),
            label: "交叉审查与挑刺".to_owned(),
            description: "独立审查改动的正确性、回归风险与遗漏。".to_owned(),
            enabled: true,
            mode: "inherit".to_owned(),
            provider: String::new(),
            profile: String::new(),
            model: String::new(),
            effort: "high".to_owned(),
            guidelines: "只基于实际读取的文件和运行结果下结论；发现问题直接给出文件名、行号与修复建议。".to_owned(),
        },
        OrchestrationRole {
            id: "researcher".to_owned(),
            label: "代码检索与调研".to_owned(),
            description: "检索代码、定位调用链、总结资料。".to_owned(),
            enabled: true,
            mode: "inherit".to_owned(),
            provider: String::new(),
            profile: String::new(),
            model: String::new(),
            effort: "low".to_owned(),
            guidelines: "高信噪比汇报：只返回准确文件路径、符号名与关键源码摘录。".to_owned(),
        },
        OrchestrationRole {
            id: "verifier".to_owned(),
            label: "实机测试与验收".to_owned(),
            description: "启动服务、操作界面、查看日志并实机验收。".to_owned(),
            enabled: true,
            mode: "inherit".to_owned(),
            provider: String::new(),
            profile: String::new(),
            model: String::new(),
            effort: "medium".to_owned(),
            guidelines: "必须亲自跑命令或浏览器操作看到退出码与界面结果，严禁用推测代替验收。".to_owned(),
        },
    ]
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationRole {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_role_mode")]
    pub mode: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub guidelines: String,
}

pub const ORCHESTRATION_MODE_CONVERSATION: &str = "conversation";
pub const ORCHESTRATION_MODE_SRC: &str = "src";

pub fn normalize_orchestration_mode(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "default" | "chat" | "conversation" => Some(ORCHESTRATION_MODE_CONVERSATION),
        "src" => Some(ORCHESTRATION_MODE_SRC),
        _ => None,
    }
}

pub fn thread_orchestration_mode(s: &AppState, thread_id: &str) -> &'static str {
    if thread_id.is_empty() || thread_id.starts_with("prewarm:") {
        return ORCHESTRATION_MODE_CONVERSATION;
    }
    if capabilities::thread_mode(&s.store, thread_id) == ORCHESTRATION_MODE_SRC {
        ORCHESTRATION_MODE_SRC
    } else {
        ORCHESTRATION_MODE_CONVERSATION
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestrationSource {
    Default,
    ModeOverride,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedOrchestration {
    pub mode: &'static str,
    pub source: OrchestrationSource,
    pub profile: OrchestrationProfile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationProfile {
    #[serde(default = "default_orchestration_preset")]
    pub preset: String,
    #[serde(default = "default_true")]
    pub auto_inject_instructions: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_role: String,
    #[serde(default = "default_orchestration_roles")]
    pub roles: Vec<OrchestrationRole>,
}

impl Default for OrchestrationProfile {
    fn default() -> Self {
        Self {
            preset: default_orchestration_preset(),
            auto_inject_instructions: true,
            default_role: String::new(),
            roles: default_orchestration_roles(),
        }
    }
}

impl OrchestrationProfile {
    pub fn sanitized(mut self) -> Self {
        if self.preset.trim().is_empty() {
            self.preset = default_orchestration_preset();
        }
        self.default_role = self.default_role.trim().to_ascii_lowercase().replace(' ', "_");
        let mut seen = std::collections::HashSet::new();
        self.roles.retain_mut(|role| {
            let id = role.id.trim().to_ascii_lowercase().replace(' ', "_");
            if id.is_empty() || !seen.insert(id.clone()) {
                return false;
            }
            role.id = id;
            role.label = role.label.trim().to_owned();
            if role.label.is_empty() {
                role.label = role.id.clone();
            }
            role.description = role.description.trim().to_owned();
            role.provider = role.provider.trim().to_owned();
            role.profile = role.profile.trim().to_owned();
            role.model = role.model.trim().to_owned();
            role.effort = role.effort.trim().to_ascii_lowercase();
            if role.mode != "pinned" || role.provider.is_empty() {
                role.mode = "inherit".to_owned();
            }
            role.guidelines = role.guidelines.trim().to_owned();
            true
        });
        self
    }

    pub fn digest(&self) -> String {
        let encoded = serde_json::to_vec(self).unwrap_or_default();
        format!("{:x}", Sha256::digest(&encoded))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadLifecycle {
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub settled: bool,
    #[serde(default)]
    pub settled_at: Option<String>,
    #[serde(default)]
    pub settled_summary: Option<String>,
    #[serde(default)]
    pub follow_up: bool,
    #[serde(default)]
    pub follow_up_note: Option<String>,
    #[serde(default)]
    pub parked: bool,
    #[serde(default)]
    pub snoozed_until: Option<String>,
    #[serde(default)]
    pub unread: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadReference {
    pub target_thread_id: String,
    #[serde(default)]
    pub target_title: String,
    #[serde(default)]
    pub target_project_id: String,
    #[serde(default)]
    pub target_engine: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub anchors: Vec<String>,
    #[serde(default)]
    pub boundary_notice: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub actor: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRecord {
    pub summary: String,
    #[serde(default)]
    pub disposition: String,
    #[serde(default)]
    pub evidence_anchors: Vec<String>,
    #[serde(default)]
    pub reviewer_thread_id: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArmedSelfSettle {
    pub when: String,
    #[serde(default)]
    pub summary: String,
    pub consent_quote: String,
    pub armed_at: String,
    #[serde(default)]
    pub armed_turn_id: String,
}

/// What a tool needs before it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    Tier(Tier),
    Category(Category),
    /// Offered whatever the grant: it only asks the user.
    Always,
    /// Offered only to a thread another thread started as its subagent.
    Subagent,
}

/// Catalog tools that rearrange threads without running anything.
const ORGANIZE: [&str; 4] = [
    "muteki_create_thread",
    "muteki_rename_thread",
    "muteki_archive_thread",
    "muteki_unarchive_thread",
];

pub fn requirement(name: &str) -> Option<Requirement> {
    if let Some(requirement) = crate::control_tools::requirement(name) {
        return Some(requirement);
    }
    let category = match name {
        "muteki_import_host_account" | "muteki_import_credential_file" => Some(Category::Credentials),
        "muteki_submit_competition_answer" => Some(Category::CompetitionSubmit),
        "muteki_update_src_policy"
        | "muteki_update_src_coordination_policy"
        | "muteki_pause_src_task"
        | "muteki_resume_src_task"
        | "muteki_save_src_login_recipe"
        | "muteki_save_connector_config"
        | "muteki_revoke_connector_config"
        | "muteki_put_src_credential"
        | "muteki_revoke_src_credential"
        | "muteki_create_monitor"
        | "muteki_revise_monitor"
        | "muteki_set_monitor_state"
        | "muteki_update_worker_settings"
        | "muteki_update_worker_identity"
        | "muteki_update_policy"
        | "muteki_upsert_credential_account"
        | "muteki_upsert_model_endpoint"
        | "muteki_update_pricing"
        | "muteki_update_capability"
        | "muteki_update_ui_settings"
        | "muteki_scan_provider_history"
        | "muteki_import_provider_history"
        | "muteki_update_runner_resources"
        | "muteki_update_run_retention"
        | "muteki_manage_platform_effect"
        | "muteki_pull_worker_image" => Some(Category::SettingsWrite),
        "muteki_delete_credential_account"
        | "muteki_delete_model_endpoint"
        | "muteki_maintenance" => Some(Category::Delete),
        _ => None,
    };
    if let Some(category) = category {
        return Some(Requirement::Category(category));
    }
    if crate::chat_computer::handles(name) {
        return None;
    }
    let spec = capabilities::spec(name)?;
    Some(Requirement::Tier(if ORGANIZE.contains(&name) {
        Tier::Organize
    } else if spec.target_kind() == "command" {
        Tier::Operate
    } else {
        Tier::Observe
    }))
}

pub fn resolve_thread_id<'a>(caller: &'a str, raw: &'a str) -> &'a str {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == OWN_THREAD {
        caller
    } else {
        trimmed
    }
}

/// Refines a tool's base requirement when specific sub-actions escalate to a dangerous category.
pub fn dynamic_requirement(
    s: &AppState,
    caller: &str,
    name: &str,
    arguments: &Value,
) -> Option<Requirement> {
    match name {
        "muteki_git" => match arguments["action"].as_str().unwrap_or("") {
            "push" | "pull" | "commit_push" | "publish_branch" | "merge_worktree" => {
                Some(Requirement::Category(Category::GitRemote))
            }
            _ => Some(Requirement::Tier(Tier::Operate)),
        },
        "muteki_pull_request" if arguments.get("op").is_some() => {
            let write = serde_json::from_value::<crate::control_contract::PullRequestOp>(arguments["op"].clone()).is_ok_and(|op|op.writes());
            Some(if write { Requirement::Category(Category::PrWrite) } else { Requirement::Tier(Tier::Operate) })
        }
        "muteki_pull_request" => match arguments["action"].as_str().unwrap_or("status") {
            "comment" | "reply" => Some(Requirement::Category(Category::PrWrite)),
            "create" if arguments["push_first"].as_bool().unwrap_or(true) => {
                Some(Requirement::Category(Category::GitRemote))
            }
            "status" => Some(Requirement::Tier(Tier::Observe)),
            _ => Some(Requirement::Tier(Tier::Operate)),
        },
        "muteki_respond_input" => {
            let target = resolve_thread_id(caller, arguments["thread_id"].as_str().unwrap_or(""));
            if target != caller && crate::chat_subagents::is_descendant_of(s, target, caller) {
                Some(Requirement::Tier(Tier::Operate))
            } else {
                Some(Requirement::Category(Category::AnswerInput))
            }
        }
        "muteki_control_thread" => {
            if arguments["action"].as_str() == Some("drop_queued") {
                let target = resolve_thread_id(caller, arguments["thread_id"].as_str().unwrap_or(OWN_THREAD));
                let queue_id = arguments["queue_id"].as_str().unwrap_or("");
                let human_item = s
                    .store
                    .get(chat::COLLECTION, target)
                    .ok()
                    .flatten()
                    .and_then(|doc| {
                        doc["queue"].as_array().and_then(|q| {
                            q.iter()
                                .find(|item| item["queue_id"].as_str() == Some(queue_id))
                                .map(|item| item["actor"]["id"] != "muteki-control")
                        })
                    })
                    .unwrap_or(true);
                if human_item {
                    return Some(Requirement::Category(Category::HistoryEdit));
                }
            }
            Some(Requirement::Tier(Tier::Operate))
        }
        "muteki_update_runner_resources" if arguments["action"] == "delete" => Some(Requirement::Category(Category::Delete)),
        "muteki_update_ui_settings" if arguments["target"] == "host_credential" || arguments["target"] == "access" => Some(Requirement::Category(Category::Credentials)),
        "muteki_patch_settings" if arguments["patch"]["type"] == "host_credential" || arguments["patch"]["type"] == "access" => Some(Requirement::Category(Category::Credentials)),
        "muteki_update_local_pi" => match arguments["action"].as_str().unwrap_or("update") {
            "probe" => Some(Requirement::Tier(Tier::Operate)),
            _ => Some(Requirement::Category(Category::SettingsWrite)),
        },
        "muteki_refresh_quota" => match arguments["action"].as_str().unwrap_or("refresh") {
            "update_usage_settings" => Some(Requirement::Category(Category::SettingsWrite)),
            _ => Some(Requirement::Tier(Tier::Operate)),
        },
        "muteki_manage_runtime_instance" => {
            match arguments["action"].as_str().unwrap_or("probe") {
                "remove" | "delete" | "delete_profile" => Some(Requirement::Category(Category::Delete)),
                "upsert" | "create" | "update" | "login" | "logout" | "upsert_profile" => {
                    Some(Requirement::Category(Category::SettingsWrite))
                }
                _ => Some(Requirement::Tier(Tier::Operate)),
            }
        }
        "muteki_manage_chat_plugin" => match arguments["action"].as_str().unwrap_or("check") {
            "uninstall" => Some(Requirement::Category(Category::Delete)),
            "check" => Some(Requirement::Tier(Tier::Operate)),
            _ => Some(Requirement::Category(Category::SettingsWrite)),
        },
        "muteki_manage_extension" => {
            match arguments["action"].as_str().unwrap_or("health_refresh") {
                "uninstall" => Some(Requirement::Category(Category::Delete)),
                "health_refresh" => Some(Requirement::Tier(Tier::Operate)),
                _ => Some(Requirement::Category(Category::SettingsWrite)),
            }
        }
        "muteki_manage_workspace" => match arguments["action"].as_str().unwrap_or("") {
            "delete_project" | "delete_workspace" | "delete_worktree" => {
                Some(Requirement::Category(Category::Delete))
            }
            _ => Some(Requirement::Tier(Tier::Operate)),
        },
        "muteki_manage_folder" => match arguments["action"].as_str().unwrap_or("") {
            "delete" => Some(Requirement::Category(Category::Delete)),
            _ => Some(Requirement::Tier(Tier::Organize)),
        },
        "muteki_manage_thread_memory" => match arguments["action"].as_str().unwrap_or("") {
            "delete" => Some(Requirement::Category(Category::Delete)),
            _ => Some(Requirement::Category(Category::SettingsWrite)),
        },
        "muteki_manage_conversation_share" => match arguments["action"].as_str().unwrap_or("") {
            "revoke" => Some(Requirement::Tier(Tier::Organize)),
            _ => Some(Requirement::Category(Category::SettingsWrite)),
        },
        "muteki_manage_domain_module" => match arguments["action"].as_str().unwrap_or("") {
            "enable" | "disable" => Some(Requirement::Category(Category::SettingsWrite)),
            _ => Some(Requirement::Tier(Tier::Operate)),
        },
        _ => requirement(name),
    }
}

/// Whether `name` should be advertised in `muteki-collab`'s `tools/list` for `caller`.
pub fn visible_in_collab_list(s: &AppState, caller: &str, name: &str) -> bool {
    if name == "muteki_request_grant" {
        return false;
    }
    match requirement(name) {
        Some(Requirement::Subagent) => !caller.is_empty() && is_subagent(s, caller),
        _ => true,
    }
}


pub(crate) fn failure(status: StatusCode, code: &str, message: impl Into<String>) -> Failure {
    let category = match status {
        StatusCode::BAD_REQUEST => "validation",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::FORBIDDEN => "permission",
        StatusCode::INTERNAL_SERVER_ERROR => "internal",
        _ => "state",
    };
    (
        status,
        Json(json!({"error": {"code": code, "message": message.into(), "category": category}})),
    )
}

pub(crate) fn storage(error: impl std::fmt::Display) -> Failure {
    failure(
        StatusCode::INTERNAL_SERVER_ERROR,
        "control.storage_unavailable",
        error.to_string(),
    )
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn read_grant(value: &Value) -> Result<Option<Grant>, Failure> {
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone()).map(Some).map_err(|error| {
        failure(
            StatusCode::BAD_REQUEST,
            "control.grant_invalid",
            format!("grant is invalid: {error}"),
        )
    })
}

#[derive(Default, Clone, Debug)]
pub struct Settings {
    pub default_grant: Option<Grant>,
    pub project_overrides: HashMap<String, Grant>,
    pub subagent_policy: SubagentPolicy,
    pub orchestration: OrchestrationProfile,
    pub orchestration_overrides: BTreeMap<String, OrchestrationProfile>,
}

impl Settings {
    pub fn orchestration_for_mode(&self, raw_mode: &str) -> ResolvedOrchestration {
        let mode = normalize_orchestration_mode(raw_mode)
            .unwrap_or(ORCHESTRATION_MODE_CONVERSATION);
        if mode != ORCHESTRATION_MODE_CONVERSATION
            && let Some(profile) = self.orchestration_overrides.get(mode)
        {
            return ResolvedOrchestration {
                mode,
                source: OrchestrationSource::ModeOverride,
                profile: profile.clone(),
            };
        }
        ResolvedOrchestration {
            mode,
            source: OrchestrationSource::Default,
            profile: self.orchestration.clone(),
        }
    }

    pub fn effective_default_role(&self, resolved: &ResolvedOrchestration) -> Option<String> {
        let profile_role = resolved.profile.default_role.trim();
        if !profile_role.is_empty() {
            return Some(profile_role.to_owned());
        }
        if resolved.source == OrchestrationSource::Default {
            let legacy_role = self.subagent_policy.default_role.trim();
            if !legacy_role.is_empty() {
                return Some(legacy_role.to_owned());
            }
        }
        None
    }
}

pub fn settings(s: &AppState) -> Result<Settings, Failure> {
    let doc = s
        .store
        .get(SETTINGS, GLOBAL)
        .map_err(storage)?
        .unwrap_or(Value::Null);
    let mut out = Settings {
        default_grant: read_grant(&doc["default_grant"])?,
        subagent_policy: if doc["subagent_policy"].is_null() {
            SubagentPolicy::default()
        } else {
            serde_json::from_value::<SubagentPolicy>(doc["subagent_policy"].clone()).map_err(storage)?.sanitized()
        },
        orchestration: if doc.get("orchestration").is_some_and(|v| !v.is_null()) {
            serde_json::from_value::<OrchestrationProfile>(doc["orchestration"].clone())
                .map_err(storage)?.sanitized()
        } else {
            OrchestrationProfile::default()
        },
        ..Settings::default()
    };
    for (mode_key, raw_profile) in doc["orchestration_overrides"].as_object().into_iter().flatten() {
        if raw_profile.is_null() {
            continue;
        }
        if let Some(mode) = normalize_orchestration_mode(mode_key)
            && mode != ORCHESTRATION_MODE_CONVERSATION
        {
            let profile = serde_json::from_value::<OrchestrationProfile>(raw_profile.clone())
                .map_err(storage)?
                .sanitized();
            out.orchestration_overrides.insert(mode.to_owned(), profile);
        }
    }
    for (project, grant) in doc["project_overrides"].as_object().into_iter().flatten() {
        if let Some(grant) = read_grant(grant)? {
            out.project_overrides.insert(project.clone(), grant);
        }
    }
    Ok(out)
}

pub fn effective_orchestration(
    s: &AppState,
    thread_id: &str,
) -> Result<ResolvedOrchestration, Failure> {
    let cfg = settings(s)?;
    Ok(cfg.orchestration_for_mode(thread_orchestration_mode(s, thread_id)))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Default,
    Project,
    Thread,
    Subagent,
}

/// A thread's state in the control store: its own grant, requests, references, review, self-settle & lifecycle.
pub(crate) fn thread_doc(s: &AppState, thread_id: &str) -> Result<Value, Failure> {
    Ok(s.store
        .get(THREADS, thread_id)
        .map_err(storage)?
        .unwrap_or_else(|| {
            json!({
                "grant": null,
                "requests": [],
                "references": [],
                "review": null,
                "self_settle": null,
                "lifecycle": ThreadLifecycle::default(),
            })
        }))
}

pub(crate) fn save_thread_doc(s: &AppState, thread_id: &str, doc: &Value) -> Result<(), Failure> {
    s.store.put(THREADS, thread_id, doc).map_err(storage)
}

pub(crate) fn read_lifecycle(doc: &Value) -> ThreadLifecycle {
    serde_json::from_value(doc["lifecycle"].clone()).unwrap_or_default()
}

/// A delivered message reopens a settled or cancelled thread. Only human
/// messages withdraw self-settlement consent; every origin clears the manual
/// active/settled override, as the upstream message boundary does.
pub(crate) fn reactivate_message(s: &AppState, id: &str, thread: &mut Value, body: &Value) -> Result<Option<Value>, Failure> {
    if !matches!(body["command_type"].as_str(), Some("conversation.turn.send" | "conversation.turn.steer")) { return Ok(None); }
    let mut control = thread_doc(s, id)?;
    let before = control.clone();
    let mut lifecycle = read_lifecycle(&control);
    if lifecycle.settled { control["unsettled_at"] = json!(chrono::Utc::now().timestamp()); }
    lifecycle.settled = false;
    lifecycle.settled_at = None;
    control["manual_unsettle"] = json!(false);
    control["lifecycle"] = serde_json::to_value(&lifecycle).map_err(storage)?;
    if !matches!(body["actor"]["kind"].as_str(), Some("agent" | "system")) {
        control["self_settle"] = Value::Null;
        control["canonical_self_settle"] = Value::Null;
        if control["pull_request_watches_paused"] == true {
            control["pull_request_watches_paused"] = json!(false);
        }
    }
    thread["thread"]["control_lifecycle"] = serde_json::to_value(lifecycle).map_err(storage)?;
    thread["thread"]["state"]["delivery_settled"] = json!(false);
    Ok((before != control).then_some(control))
}

/// Mirror lightweight lifecycle flags onto `conversation_threads` so thread listings & SSE snapshots see them immediately.
pub(crate) fn sync_lifecycle_mirror(
    s: &AppState,
    thread_id: &str,
    lifecycle: &ThreadLifecycle,
) -> Result<(), Failure> {
    for _ in 0..8 {
        let Ok(mut doc) = chat::load(s, thread_id) else {
            return Ok(());
        };
        let before = doc.clone();
        doc["thread"]["control_lifecycle"] = serde_json::to_value(lifecycle).unwrap_or(Value::Null);
        doc["thread"]["pinned"] = json!(lifecycle.pinned);
        if let Some((events, watermark)) = s
            .store
            .chat_commit_checked(thread_id, &doc, Some(&before), None)
            .map_err(storage)?
        {
            chat::finish_save(s, thread_id, &mut doc, events, watermark);
            return Ok(());
        }
    }
    Ok(())
}

pub(crate) fn thread_meta(s: &AppState, thread_id: &str) -> Option<(String, String)> {
    let doc = s.store.get(chat::COLLECTION, thread_id).ok().flatten()?;
    Some((
        doc["thread"]["mode"]
            .as_str()
            .unwrap_or("conversation")
            .to_owned(),
        doc["thread"]["project_id"].as_str().unwrap_or("").to_owned(),
    ))
}

pub(crate) fn thread_engine(s: &AppState, thread_id: &str) -> String {
    let Some(doc) = s.store.get(chat::COLLECTION, thread_id).ok().flatten() else {
        return String::new();
    };
    let adapter = doc["runtime"]["adapter_id"].as_str().unwrap_or("");
    crate::agent_runtime::descriptors::for_adapter(adapter)
        .map(|(engine, _)| engine.engine().to_owned())
        .unwrap_or_else(|| adapter.to_owned())
}

pub fn is_subagent(s: &AppState, thread_id: &str) -> bool {
    s.store
        .get(chat::COLLECTION, thread_id)
        .ok()
        .flatten()
        .is_some_and(|doc| doc["thread"]["state"]["lineage"]["origin"] == "app_owned")
}

pub fn discussion_group_id(s: &AppState, thread_id: &str) -> Option<String> {
    s.store
        .get(chat::COLLECTION, thread_id)
        .ok()
        .flatten()
        .and_then(|doc| {
            doc["thread"]["state"]["lineage"]["discussion_group_id"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        })
}

pub fn is_read_only_subagent(s: &AppState, thread_id: &str) -> bool {
    s.store
        .get(chat::COLLECTION, thread_id)
        .ok()
        .flatten()
        .is_some_and(|doc| doc["thread"]["state"]["lineage"]["read_only"] == true)
}

/// The grant `thread_id` would inherit if it had no per-thread override.
pub fn inherited_grant(s: &AppState, thread_id: &str) -> Result<(Grant, Source), Failure> {
    let settings = settings(s)?;
    let project = thread_meta(s, thread_id)
        .map(|(_, project)| project)
        .unwrap_or_default();
    if let Some(grant) = settings.project_overrides.get(&project) {
        return Ok((grant.clone(), Source::Project));
    }
    Ok((
        settings.default_grant.unwrap_or_else(Grant::standard),
        Source::Default,
    ))
}

pub fn effective(s: &AppState, thread_id: &str) -> Result<(Grant, Source), Failure> {
    if let Some(grant) = read_grant(&thread_doc(s, thread_id)?["grant"])? {
        return Ok((grant, Source::Thread));
    }
    inherited_grant(s, thread_id)
}

/// Whether the gate applies to this thread. A missing thread or a thread of
/// another mode is not gated here.
pub fn gated(s: &AppState, thread_id: &str) -> bool {
    thread_meta(s, thread_id).is_some_and(|(mode, _)| matches!(mode.as_str(), "conversation" | "src"))
}

fn src_target_task_identity(s: &AppState, target: &str) -> Result<(Option<(String, String)>, bool), ()> {
    match crate::runs::src_api::resolve_authorized_src_caller(s, target) {
        Ok(Some(caller)) => Ok((Some((caller.task_id, caller.main_thread_id)), false)),
        Err(_) => {
            if let Some(doc) = s.store.get(chat::COLLECTION, target).ok().flatten() {
                let del = &doc["thread"]["src_delegation"];
                let task_id = del["task_id"].as_str().unwrap_or("").trim();
                let main_id = del["main_thread_id"].as_str().unwrap_or("").trim();
                if !task_id.is_empty() && !main_id.is_empty() {
                    return Ok((Some((task_id.to_owned(), main_id.to_owned())), true));
                }
                if doc["thread"]["mode"] == "src" {
                    return Err(());
                }
            }
            Err(())
        }
        Ok(None) => {
            let mode_is_src = thread_meta(s, target).is_some_and(|(mode, _)| mode == "src");
            if mode_is_src {
                Err(())
            } else {
                Ok((None, false))
            }
        }
    }
}

/// Strict same-SRC-Task isolation across control inspection and thread actions.
/// Non-SRC callers never see SRC threads; SRC callers never see non-SRC or other Tasks' threads;
/// revoked SRC subthreads cannot inspect or act on other threads.
pub(crate) fn src_isolation_allows(s: &AppState, caller: &str, target: &str) -> bool {
    if caller == target {
        return true;
    }
    let caller_mode = thread_meta(s, caller).map(|(mode, _)| mode).unwrap_or_default();
    let caller_identity = match crate::runs::src_api::resolve_authorized_src_caller(s, caller) {
        Ok(Some(found)) => Some((found.task_id, found.main_thread_id)),
        Err(_) => return false,
        Ok(None) => {
            if caller_mode == "src" {
                return false;
            }
            None
        }
    };
    let Ok((target_identity, _target_revoked)) = src_target_task_identity(s, target) else {
        return false;
    };
    match (caller_identity, target_identity) {
        (Some(c), Some(t)) => c == t,
        (None, None) => true,
        _ => false,
    }
}


pub fn denied(name: &str, requirement: Requirement, grant: &Grant) -> Value {
    let needs = match requirement {
        Requirement::Tier(tier) => format!("tier {}", tier_name(tier)),
        Requirement::Category(category) => format!("category {}", category_name(category)),
        Requirement::Subagent => "being started as a subagent by another thread".to_owned(),
        Requirement::Always => "nothing".to_owned(),
    };
    let hint = match requirement {
        Requirement::Subagent => String::new(),
        _ => " Call muteki_request_grant with the reason you need it; the user approves or declines in the chat.".to_owned(),
    };
    json!({
        "code": "control.grant.denied",
        "category": "permission",
        "message": format!("refused: `{name}` needs {needs}; your grant is {}.{hint}", grant.describe()),
        "detail": {"tool": name, "needs": needs, "grant": grant}
    })
}

pub fn allowed(s: &AppState, thread_id: &str, requirement: Requirement, grant: &Grant) -> bool {
    match requirement {
        Requirement::Tier(tier) => grant.tiers.contains(&tier),
        Requirement::Category(category) => grant.categories.contains(&category),
        Requirement::Always => true,
        Requirement::Subagent => is_subagent(s, thread_id),
    }
}

/// Extract target thread IDs referenced by a tool call (resolving `"self"` to `caller`).
fn referenced_targets(caller: &str, arguments: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if matches!(arguments["target"].as_str(), Some("conversation_preferences" | "thread_grant"))
        && let Some(id) = arguments["values"]["thread_id"].as_str().filter(|id| !id.trim().is_empty())
    {
        out.push(resolve_thread_id(caller, id).to_owned());
    }
    if arguments["patch"]["type"] == "thread_grant"
        && let Some(id) = arguments["patch"]["content"]["thread_id"].as_str().filter(|id| !id.trim().is_empty())
    {
        out.push(resolve_thread_id(caller, id).to_owned());
    }
    for key in ["thread_id", "target_thread_id", "source_thread_id"] {
        if let Some(raw) = arguments[key].as_str().map(str::trim).filter(|id| !id.is_empty()) {
            let resolved = resolve_thread_id(caller, raw).to_owned();
            if !out.contains(&resolved) {
                out.push(resolved);
            }
        }
    }
    if let Some(arr) = arguments["thread_ids"].as_array() {
        for item in arr.iter().filter_map(Value::as_str) {
            let trimmed = item.trim();
            if !trimmed.is_empty() {
                let resolved = resolve_thread_id(caller, trimmed).to_owned();
                if !out.contains(&resolved) {
                    out.push(resolved);
                }
            }
        }
    }
    for key in ["anchor", "from", "to"] {
        if let Some(anchor) = arguments[key].as_str().map(str::trim).filter(|s| !s.is_empty())
            && let Some((tid, _)) = anchor.split_once('#')
        {
            let resolved = resolve_thread_id(caller, tid).to_owned();
            if !out.contains(&resolved) {
                out.push(resolved);
            }
        }
    }
    if let Some(arr) = arguments["anchors"].as_array() {
        for anchor in arr.iter().filter_map(Value::as_str) {
            if let Some((tid, _)) = anchor.trim().split_once('#')
                && !tid.trim().is_empty()
            {
                let resolved = resolve_thread_id(caller, tid).to_owned();
                if !out.contains(&resolved) {
                    out.push(resolved);
                }
            }
        }
    }
    out
}

/// Base `authorize` — internal Muteki agent calls no longer require per-tool grant gates.
pub fn authorize(_s: &AppState, _thread_id: &str, _name: &str) -> Result<(), Value> {
    Ok(())
}

/// Per-call boundary check:
/// 1. Ancestor isolation (a subagent can NEVER inspect or act on its parent/ancestors).
/// 2. Same-SRC-Task routing isolation (via `check_target`).
/// All internal Tier/Category/Scope grant walls are removed.
pub fn authorize_call(
    s: &AppState,
    thread_id: &str,
    name: &str,
    arguments: &Value,
) -> Result<(), Value> {
    if !gated(s, thread_id) {
        return Ok(());
    }
    let targets = if name == "muteki_send_message" {
        Vec::new()
    } else {
        referenced_targets(thread_id, arguments)
    };
    let grant = Grant::standard();
    for target in &targets {
        check_target(s, thread_id, target, &grant)?;
    }
    Ok(())
}

/// Restrictions follow creation provenance as well as visible parentage. Persisted
/// copies keep a peer restricted when its creating child is later deleted.
pub fn ancestor_restrictions(s: &AppState, caller: &str) -> Result<BTreeSet<String>, Value> {
    let mut blocked = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut pending = vec![caller.to_owned()];
    while let Some(id) = pending.pop() {
        if !visited.insert(id.clone()) { continue; }
        let Some(doc) = s.store.get(chat::COLLECTION, &id).map_err(|error|json!({"code":"control.storage_failed","message":error.to_string()}))? else { continue; };
        blocked.extend(doc["thread"]["control_ancestor_restrictions"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned));
        if let Some(parent) = doc["thread"]["state"]["lineage"]["parent_thread_id"].as_str().filter(|id|!id.is_empty()) {
            blocked.insert(parent.to_owned());
            pending.push(parent.to_owned());
        }
        if let Some(creator) = doc["thread"]["started_by"]["thread_id"].as_str().filter(|id|!id.is_empty()) {
            pending.push(creator.to_owned());
        }
    }
    Ok(blocked)
}

/// A target thread the caller may act on: itself, one of its subagents, or a
/// thread the grant's scope reaches (never an ancestor).
pub fn check_target(s: &AppState, caller: &str, target: &str, grant: &Grant) -> Result<(), Value> {
    let target = resolve_thread_id(caller, target);
    if caller == target {
        return Ok(());
    }
    if ancestor_restrictions(s, caller)?.contains(target) {
        return Err(json!({
            "code": "control.scope.ancestor_forbidden",
            "category": "permission",
            "message": format!("thread {target} is an ancestor of {caller} and cannot be accessed by a subagent")
        }));
    }
    let Some((_, target_project)) = thread_meta(s, target) else {
        return Err(json!({
            "code": "control.thread.unknown",
            "category": "not_found",
            "message": format!("thread {target} does not exist")
        }));
    };
    if !src_isolation_allows(s, caller, target) {
        return Err(json!({
            "code": "control.scope.outside",
            "category": "permission",
            "message": format!("thread {target} is outside your SRC Task isolation scope.")
        }));
    }
    let caller_project = thread_meta(s, caller)
        .map(|(_, project)| project)
        .unwrap_or_default();
    if grant.scope.reaches(&caller_project, &target_project)
        || crate::chat_subagents::is_descendant_of(s, target, caller)
    {
        Ok(())
    } else {
        Err(json!({
            "code": "control.scope.outside",
            "category": "permission",
            "message": format!("thread {target} is outside your grant's scope ({}).", grant.describe())
        }))
    }
}


pub fn compute_boundary_notice(
    s: &AppState,
    caller_thread_id: &str,
    target_thread_id: &str,
) -> Option<String> {
    let caller_project = thread_meta(s, caller_thread_id)
        .map(|(_, p)| p)
        .unwrap_or_default();
    let target_project = thread_meta(s, target_thread_id)
        .map(|(_, p)| p)
        .unwrap_or_default();
    let caller_engine = thread_engine(s, caller_thread_id);
    let target_engine = thread_engine(s, target_thread_id);
    let mut parts = Vec::new();
    if !caller_project.is_empty() && !target_project.is_empty() && caller_project != target_project {
        parts.push(format!("跨项目引用（{target_project} → {caller_project}）"));
    }
    if !caller_engine.is_empty() && !target_engine.is_empty() && caller_engine != target_engine {
        parts.push(format!("跨模型引擎边界（{target_engine} → {caller_engine}）"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Store-only reference recorder for synchronous turn resolution (`composer::resolve`),
/// avoiding `conversation_threads` CAS contention while `chat::command_from_platform` stages a turn.
pub fn record_reference_in_store(
    s: &AppState,
    caller_thread_id: &str,
    target_thread_id: &str,
    reason: &str,
    actor: &str,
) -> Option<String> {
    let boundary_notice = compute_boundary_notice(s, caller_thread_id, target_thread_id);
    let Ok(target_doc) = chat::load(s, target_thread_id) else {
        return boundary_notice;
    };
    let record = ThreadReference {
        target_thread_id: target_thread_id.to_owned(),
        target_title: target_doc["thread"]["title"]
            .as_str()
            .unwrap_or(target_thread_id)
            .to_owned(),
        target_project_id: target_doc["thread"]["project_id"]
            .as_str()
            .unwrap_or("")
            .to_owned(),
        target_engine: thread_engine(s, target_thread_id),
        reason: reason.trim().to_owned(),
        anchors: Vec::new(),
        boundary_notice: boundary_notice.clone(),
        created_at: now(),
        actor: actor.to_owned(),
    };
    if let Ok(mut doc) = thread_doc(s, caller_thread_id) {
        let mut refs: Vec<ThreadReference> =
            serde_json::from_value(doc["references"].clone()).unwrap_or_default();
        if let Some(existing) = refs
            .iter_mut()
            .find(|r| r.target_thread_id == target_thread_id)
        {
            existing.boundary_notice = record.boundary_notice.clone();
        } else {
            refs.push(record);
        }
        if let Ok(val) = serde_json::to_value(&refs) {
            doc["references"] = val;
            let _ = save_thread_doc(s, caller_thread_id, &doc);
        }
    }
    boundary_notice
}

pub fn attach_reference(
    s: &Arc<AppState>,
    caller_thread_id: &str,
    target_thread_id: &str,
    reason: &str,
    anchors: Vec<String>,
    actor: &str,
) -> Result<ThreadReference, Failure> {
    let target_doc = chat::load(s, target_thread_id)?;
    let target_title = target_doc["thread"]["title"]
        .as_str()
        .unwrap_or(target_thread_id)
        .to_owned();
    let target_project_id = target_doc["thread"]["project_id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let target_engine = thread_engine(s, target_thread_id);
    let boundary_notice = compute_boundary_notice(s, caller_thread_id, target_thread_id);

    let record = ThreadReference {
        target_thread_id: target_thread_id.to_owned(),
        target_title,
        target_project_id,
        target_engine,
        reason: reason.trim().to_owned(),
        anchors,
        boundary_notice,
        created_at: now(),
        actor: actor.to_owned(),
    };

    let mut doc = thread_doc(s, caller_thread_id)?;
    let mut refs: Vec<ThreadReference> =
        serde_json::from_value(doc["references"].clone()).unwrap_or_default();
    if let Some(existing) = refs
        .iter_mut()
        .find(|r| r.target_thread_id == target_thread_id)
    {
        if !record.reason.is_empty() {
            existing.reason = record.reason.clone();
        }
        for a in &record.anchors {
            if !existing.anchors.contains(a) {
                existing.anchors.push(a.clone());
            }
        }
        existing.boundary_notice = record.boundary_notice.clone();
    } else {
        refs.push(record.clone());
    }
    doc["references"] = serde_json::to_value(&refs).map_err(storage)?;
    save_thread_doc(s, caller_thread_id, &doc)?;
    let _ = emit(
        s,
        caller_thread_id,
        "core.control.updated",
        json!({"thread_id": caller_thread_id, "kind": "reference_added", "target_thread_id": target_thread_id}),
    );
    audit(
        s,
        actor,
        caller_thread_id,
        "reference_thread",
        json!({
            "target_thread_id": target_thread_id,
            "boundary_notice": record.boundary_notice,
            "anchors": record.anchors,
        }),
    );
    Ok(record)
}


/// Record a write: one activity row for the settings page and one timeline
/// event in each thread involved, naming the acting thread.
pub fn audit(s: &Arc<AppState>, actor: &str, target: &str, action: &str, detail: Value) {
    let row = json!({
        "at": now(),
        "actor_thread_id": actor,
        "target_thread_id": target,
        "action": action,
        "detail": detail
    });
    let id = format!(
        "{:020}-{}",
        chrono::Utc::now().timestamp_millis(),
        uuid::Uuid::new_v4().simple()
    );
    if let Err(error) = s.store.put(ACTIVITY, &id, &row) {
        tracing::error!(%error, action, "control activity could not be recorded");
    }
    let mut threads = vec![target.to_owned()];
    if actor != target {
        threads.push(actor.to_owned());
    }
    for thread in threads
        .into_iter()
        .filter(|thread| !thread.is_empty() && thread != "user")
    {
        match s.store.get(chat::COLLECTION,&thread) {
            Ok(None) => continue,
            Ok(Some(_)) => {},
            Err(error) => { tracing::error!(%error,thread_id=%thread,"control audit target could not be read"); continue; }
        }
        if let Err((_, Json(body))) = emit(s, &thread, "core.control.action", row.clone()) {
            tracing::warn!(thread_id = %thread, error = %body, "control action could not be added to the thread timeline");
        }
    }
}

/// Append one event to a thread's timeline, retrying when the thread changed
/// between the read and the commit.
pub fn emit(s: &AppState, thread_id: &str, kind: &str, payload: Value) -> Result<(), Failure> {
    for _ in 0..8 {
        let mut doc = chat::load(s, thread_id)?;
        let before = doc.clone();
        chat::emit(s, thread_id, &mut doc, kind, payload.clone());
        if let Some((events, watermark)) = s
            .store
            .chat_commit_checked(thread_id, &doc, Some(&before), None)
            .map_err(storage)?
        {
            chat::finish_save(s, thread_id, &mut doc, events, watermark);
            return Ok(());
        }
    }
    Err(failure(
        StatusCode::CONFLICT,
        "control.thread_busy",
        "the thread kept changing while a control event was written",
    ))
}

/// Send `text` to a thread as a user-visible message: queued behind a running
/// turn, or starting a turn when the thread is idle. `key` makes a retry safe.
pub async fn deliver(
    s: &Arc<AppState>,
    thread_id: &str,
    key: &str,
    text: &str,
) -> Result<(), Failure> {
    let base_id = format!("control:{key}");
    let record_key = format!("thread:{thread_id}:{base_id}");
    let doc = chat::load(s, thread_id)?;
    let existing = s.store.get("conversation_commands", &record_key).map_err(storage)?;
    let restart_unaccepted = existing.is_some() && doc["turns"].as_array().into_iter().flatten().any(|turn|
        turn["command_id"] == base_id && turn["status"] == "interrupted" && turn["native_accepted_at"].is_null());
    let command_id = if restart_unaccepted { format!("{base_id}:restart:{}", s.chat.epoch()) } else { base_id.clone() };
    if existing.is_some() && doc["thread"]["state"]["queue_pause_reason"] == "process_restarted"
        && doc["queue"].as_array().is_some_and(|queue|queue.iter().any(|item|item["command_id"]==base_id) && queue.iter().all(|item|item["origin"].as_str().is_some_and(|origin|origin!="human"))) {
        let resume = json!({"command_id":format!("{base_id}:resume:{}",s.chat.epoch()),"command_type":"conversation.queue.resume","payload":{},"actor":{"kind":"system","id":"muteki-control"}});
        let _ = chat::command_from_platform(s.clone(),thread_id.to_owned(),resume,None).await?;
        return Ok(());
    }
    let body = if let Some(record) = existing.filter(|_|!restart_unaccepted) {
        record["request"].clone()
    } else {
        let adapter = doc["runtime"]["adapter_id"].as_str().unwrap_or("");
        let steer = doc["thread"]["mode"] != "src"
            && doc["thread"]["state"]["status"] == "running"
            && !crate::native_chat::turn_starting(s,thread_id,&doc)
            && crate::agent_runtime::descriptors::for_adapter(adapter).is_some_and(|(_, a)| a.capabilities.steer);
        json!({"command_id":command_id,
            "command_type": if steer { "conversation.turn.steer" } else { "conversation.turn.send" },
            "payload":{"text":text,"client_message_id":base_id,"expected_turn_id":if steer{doc["thread"]["state"]["running_turn_id"].clone()}else{Value::Null}},
            "actor":{"kind":"system","id":"muteki-control"}})
    };
    chat::command_from_platform(s.clone(), thread_id.to_owned(), body, None)
        .await
        .map(|_| ())
}

pub(crate) fn message_accepted(s:&AppState, thread:&str, key:&str)->Result<bool,Failure>{
    let id=format!("control:{key}");let doc=chat::load(s,thread)?;
    Ok(doc["turns"].as_array().into_iter().flatten().any(|turn|
        (turn["client_message_id"]==id || turn["command_id"]==id) && turn["native_accepted_at"].is_string())
        || doc["messages"].as_array().into_iter().flatten().any(|message|message["command_id"]==id))
}


/// `muteki_request_grant`: compatibility stub — internal tools are already granted by default.
pub async fn request_grant(
    _s: &Arc<AppState>,
    _thread_id: &str,
    _args: &Value,
) -> Result<Value, Value> {
    Ok(json!({
        "status": "approved",
        "note": "internal Muteki tool calls require no separate grant approval."
    }))
}

async fn respond(
    s: &Arc<AppState>,
    thread_id: &str,
    request_id: &str,
    approve: bool,
) -> Result<Value, Failure> {
    let was_subagent = is_subagent(s, thread_id);
    let mut doc = thread_doc(s, thread_id)?;
    let requests = doc["requests"].as_array_mut().ok_or_else(|| {
        failure(
            StatusCode::NOT_FOUND,
            "control.request.not_found",
            "no such request",
        )
    })?;
    let request = requests
        .iter_mut()
        .find(|row| row["request_id"] == request_id)
        .ok_or_else(|| {
            failure(
                StatusCode::NOT_FOUND,
                "control.request.not_found",
                "no such request",
            )
        })?;
    if request["status"] != "pending" {
        return Err(failure(
            StatusCode::CONFLICT,
            "control.request.settled",
            "the request was already answered",
        ));
    }
    let wanted: Grant = serde_json::from_value(request["grant"].clone()).map_err(storage)?;
    let reason = request["reason"].as_str().unwrap_or("").to_owned();
    request["status"] = json!(if approve { "approved" } else { "declined" });
    request["answered_at"] = json!(now());
    let message = if approve {
        let (held, _) = effective(s, thread_id)?;
        let merged = held.merge(&wanted);
        doc["grant"] = serde_json::to_value(&merged).map_err(storage)?;
        format!(
            "[control] grant approved: {} (you asked: {reason}). Your grant is now {}.",
            wanted.describe(),
            merged.describe()
        )
    } else {
        format!(
            "[control] grant declined: {} (you asked: {reason}). Do not ask again for the same reason; continue within your grant or tell the user what you could not do.",
            wanted.describe()
        )
    };
    save_thread_doc(s, thread_id, &doc)?;
    if approve && was_subagent {
        crate::native_chat::invalidate_all_capabilities(s).await;
    }
    let _ = emit(
        s,
        thread_id,
        "core.control.updated",
        json!({"thread_id": thread_id, "kind": "request_answered", "request_id": request_id, "approved": approve}),
    );
    audit(
        s,
        "user",
        thread_id,
        if approve {
            "grant_approved"
        } else {
            "grant_declined"
        },
        json!({"request_id": request_id, "grant": wanted}),
    );
    deliver(
        s,
        thread_id,
        &format!(
            "{request_id}:{}",
            if approve { "approved" } else { "declined" }
        ),
        &message,
    )
    .await?;
    Ok(json!({
        "request_id": request_id,
        "status": if approve { "approved" } else { "declined" }
    }))
}


fn thread_exists(s: &AppState, thread_id: &str) -> Result<(), Failure> {
    if thread_meta(s, thread_id).is_some() {
        Ok(())
    } else {
        Err(failure(
            StatusCode::NOT_FOUND,
            "control.thread.unknown",
            format!("thread {thread_id} does not exist"),
        ))
    }
}

pub(crate) async fn get_settings(State(s): State<Arc<AppState>>) -> Api {
    let cfg = settings(&s)?;
    let master_enabled = capabilities::collab_enabled(&s);
    Ok(Json(json!({
        "enabled": master_enabled,
        "standard_grant": Grant::standard(),
        "default_grant": cfg.default_grant,
        "project_overrides": cfg.project_overrides,
        "subagent_policy": cfg.subagent_policy,
        "orchestration": cfg.orchestration,
        "orchestration_overrides": cfg.orchestration_overrides,
    })))
}

fn enrich_orchestration_pricing(store: &crate::Store, inventory: &mut Value) {
    let mut seen = std::collections::HashSet::<(String, String, String)>::new();
    let mut requests = Vec::new();
    for rt in inventory["runtimes"].as_array().into_iter().flatten() {
        let engine = rt["engine"].as_str().unwrap_or("");
        let credential_id = rt["credential_id"].as_str().unwrap_or("");
        if engine.is_empty() {
            continue;
        }
        for model in rt["models"].as_array().into_iter().flatten() {
            let model_id = model["id"].as_str().unwrap_or("");
            if model_id.is_empty() {
                continue;
            }
            let key = (
                engine.to_owned(),
                credential_id.to_owned(),
                model_id.to_owned(),
            );
            if seen.insert(key) {
                let mut req = json!({
                    "engine": engine,
                    "model": model_id,
                });
                if !credential_id.is_empty() {
                    req["credential_id"] = json!(credential_id);
                }
                requests.push(req);
            }
        }
    }
    if requests.is_empty() {
        return;
    }
    let Ok(resolved) = crate::pricing_read::resolve(store, &requests) else {
        return;
    };
    let mut pricing_by_key = std::collections::HashMap::<(String, String, String), Value>::new();
    for item in resolved["items"].as_array().into_iter().flatten() {
        let engine = item["engine"].as_str().unwrap_or("").to_owned();
        let credential_id = item["credential_id"].as_str().unwrap_or("").to_owned();
        let model_id = item["model"].as_str().unwrap_or("").to_owned();
        let status = item["status"].as_str().unwrap_or("missing");
        let pricing = if matches!(status, "catalog" | "custom")
            && let Some(rates) = item.get("rates").and_then(Value::as_object)
        {
            json!({
                "status": status,
                "input_per_mtok": rates.get("input").cloned().unwrap_or(Value::Null),
                "output_per_mtok": rates.get("output").cloned().unwrap_or(Value::Null),
            })
        } else {
            json!({"status": status})
        };
        pricing_by_key.insert((engine, credential_id, model_id), pricing);
    }
    for list_key in ["runtimes", "providers"] {
        if let Some(list) = inventory
            .get_mut(list_key)
            .and_then(Value::as_array_mut)
        {
            for rt in list {
                let engine = rt["engine"].as_str().unwrap_or("").to_owned();
                let credential_id = rt["credential_id"].as_str().unwrap_or("").to_owned();
                if let Some(models) = rt.get_mut("models").and_then(Value::as_array_mut) {
                    for model in models {
                        let model_id = model["id"].as_str().unwrap_or("").to_owned();
                        if let Some(pricing) =
                            pricing_by_key.get(&(engine.clone(), credential_id.clone(), model_id))
                        {
                            model["pricing"] = pricing.clone();
                        }
                    }
                }
            }
        }
    }
}

async fn get_orchestration_providers(State(s): State<Arc<AppState>>) -> Api {
    let mut inventory = crate::chat_subagents::list_runtime_inventory(&s, &json!({})).await?;
    enrich_orchestration_pricing(&s.store, &mut inventory);
    Ok(Json(json!({
        "providers": inventory["providers"],
        "runtimes": inventory["runtimes"],
        "limits": inventory["limits"],
    })))
}

fn parse_orchestration_profile(val: &Value) -> Result<OrchestrationProfile, Failure> {
    serde_json::from_value::<OrchestrationProfile>(val.clone())
        .map(OrchestrationProfile::sanitized)
        .map_err(|e| {
            failure(
                StatusCode::BAD_REQUEST,
                "control.orchestration_invalid",
                format!("orchestration is invalid: {e}"),
            )
        })
}

pub(crate) async fn put_settings(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Api {
    save_settings(s, body, "user").await
}

pub(crate) async fn save_settings(s: Arc<AppState>, body: Value, actor: &str) -> Api {
    let mut doc = s
        .store
        .get(SETTINGS, GLOBAL)
        .map_err(storage)?
        .unwrap_or_else(|| json!({}));

    if body.get("default_grant").is_some() {
        read_grant(&body["default_grant"])?;
        doc["default_grant"] = body["default_grant"].clone();
    }

    if let Some(map) = body.get("project_overrides").and_then(Value::as_object) {
        let mut validated = serde_json::Map::new();
        for (pid, grant_val) in map {
            if let Some(g) = read_grant(grant_val)? {
                validated.insert(pid.clone(), serde_json::to_value(g).map_err(storage)?);
            }
        }
        doc["project_overrides"] = Value::Object(validated);
    }

    if let Some(project) = body["project_id"].as_str().filter(|id| !id.is_empty()) {
        let grant = read_grant(&body["project_grant"])?;
        if !doc["project_overrides"].is_object() {
            doc["project_overrides"] = json!({});
        }
        match grant {
            Some(grant) => {
                doc["project_overrides"][project] = serde_json::to_value(grant).map_err(storage)?;
            }
            None => {
                doc["project_overrides"]
                    .as_object_mut()
                    .map(|rows| rows.remove(project));
            }
        }
    }

    if let Some(policy_val) = body.get("subagent_policy") {
        let policy: SubagentPolicy = serde_json::from_value(policy_val.clone()).map_err(|e| {
            failure(
                StatusCode::BAD_REQUEST,
                "control.subagent_policy_invalid",
                format!("subagent_policy is invalid: {e}"),
            )
        })?;
        doc["subagent_policy"] = serde_json::to_value(policy.sanitized()).map_err(storage)?;
    }

    let mut orchestration_changed = false;
    if let Some(raw_mode) = body.get("orchestration_mode") {
        let mode_str = raw_mode.as_str().ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "control.orchestration_mode_invalid",
                "orchestration_mode must be a string",
            )
        })?;
        let mode = normalize_orchestration_mode(mode_str).ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "control.orchestration_mode_invalid",
                format!("unsupported orchestration_mode: {mode_str}"),
            )
        })?;
        let orch_val = body.get("orchestration").ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "control.orchestration_invalid",
                "orchestration field is required when orchestration_mode is specified",
            )
        })?;
        if mode == ORCHESTRATION_MODE_CONVERSATION {
            if orch_val.is_null() {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "control.orchestration_invalid",
                    "default conversation orchestration cannot be null",
                ));
            }
            let orch = parse_orchestration_profile(orch_val)?;
            doc["orchestration"] = serde_json::to_value(orch).map_err(storage)?;
        } else {
            if !doc["orchestration_overrides"].is_object() {
                doc["orchestration_overrides"] = json!({});
            }
            if orch_val.is_null() {
                doc["orchestration_overrides"]
                    .as_object_mut()
                    .map(|rows| rows.remove(mode));
            } else {
                let orch = parse_orchestration_profile(orch_val)?;
                doc["orchestration_overrides"][mode] =
                    serde_json::to_value(orch).map_err(storage)?;
            }
        }
        orchestration_changed = true;
    } else if let Some(orch_val) = body.get("orchestration") {
        let orch = parse_orchestration_profile(orch_val)?;
        doc["orchestration"] = serde_json::to_value(orch).map_err(storage)?;
        orchestration_changed = true;
    }

    if let Some(raw_overrides) = body.get("orchestration_overrides") {
        let map = raw_overrides.as_object().ok_or_else(|| {
            failure(
                StatusCode::BAD_REQUEST,
                "control.orchestration_invalid",
                "orchestration_overrides must be an object",
            )
        })?;
        let mut validated = serde_json::Map::new();
        for (raw_mode, val) in map {
            let Some(mode) = normalize_orchestration_mode(raw_mode)
                .filter(|m| *m != ORCHESTRATION_MODE_CONVERSATION)
            else {
                return Err(failure(
                    StatusCode::BAD_REQUEST,
                    "control.orchestration_mode_invalid",
                    format!("unsupported orchestration override mode: {raw_mode}"),
                ));
            };
            if !val.is_null() {
                let orch = parse_orchestration_profile(val)?;
                validated.insert(mode.to_owned(), serde_json::to_value(orch).map_err(storage)?);
            }
        }
        doc["orchestration_overrides"] = Value::Object(validated);
        orchestration_changed = true;
    }

    if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
        capabilities::set_enabled(&s, "mcp", capabilities::COLLAB_SERVER, enabled)?;
        capabilities::set_enabled(&s, "mcp", capabilities::CONTROL_SERVER, enabled)?;
        crate::native_chat::invalidate_all_capabilities(&s).await;
    }

    s.store.put(SETTINGS, GLOBAL, &doc).map_err(storage)?;
    if orchestration_changed {
        crate::native_chat::drain_warm_sessions(&s).await;
    }
    audit(
        &s,
        actor,
        "",
        "settings_changed",
        json!({
            "enabled": body.get("enabled"),
            "project_id": body.get("project_id"),
            "updated_subagent_policy": body.get("subagent_policy").is_some(),
            "updated_orchestration": orchestration_changed,
            "orchestration_mode": body.get("orchestration_mode"),
        }),
    );
    get_settings(State(s)).await
}

async fn get_thread(State(s): State<Arc<AppState>>, Path(thread_id): Path<String>) -> Api {
    thread_exists(&s, &thread_id)?;
    let (grant, source) = effective(&s, &thread_id)?;
    let (inherited, inherited_source) = inherited_grant(&s, &thread_id)?;
    let delta = grant.delta_from(&inherited);
    let doc = thread_doc(&s, &thread_id)?;
    let lifecycle = read_lifecycle(&doc);
    let discussions: Vec<Value> = s
        .store
        .list(DISCUSSIONS)
        .map_err(storage)?
        .into_iter()
        .filter(|row| row["parent_thread_id"] == thread_id)
        .collect();
    Ok(Json(json!({
        "thread_id": thread_id,
        "grant": grant,
        "inherited_grant": inherited,
        "inherited_source": inherited_source,
        "delta": delta,
        "source": source,
        "is_subagent": is_subagent(&s, &thread_id),
        "discussion_group_id": discussion_group_id(&s, &thread_id),
        "requests": doc["requests"],
        "pending": doc["requests"].as_array().into_iter().flatten()
            .filter(|row| row["status"] == "pending").cloned().collect::<Vec<_>>(),
        "references": doc["references"].as_array().cloned().unwrap_or_default(),
        "review": doc["review"],
        "self_settle": if doc["canonical_self_settle"].is_object() {json!({"when":if doc["canonical_self_settle"]["when"].as_str()==Some("end_of_turn"){json!("end_of_turn")}else if doc["canonical_self_settle"]["when"]["pr_merged"].is_object(){json!("pr_merged")}else{json!("pr_closed")},"consent_quote":doc["canonical_self_settle"]["consent_quote"],"armed_at":doc["canonical_self_settle"]["granted_at"]})}else{doc["self_settle"].clone()},
        "settle_suggestion":doc["settle_suggestion"],
        "native_subagent":chat::load(&s,&thread_id)?["thread"]["state"]["lineage"]["native_subagent"],
        "lifecycle": lifecycle,
        "discussions": discussions,
    })))
}

async fn put_thread_grant(
    State(s): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
    Json(body): Json<Value>,
) -> Api {
    save_thread_grant(s, thread_id, body, "user").await
}

pub(crate) async fn save_thread_grant(s: Arc<AppState>, thread_id: String, body: Value, actor: &str) -> Api {
    thread_exists(&s, &thread_id)?;
    let grant = read_grant(&body["grant"])?;
    let mut doc = thread_doc(&s, &thread_id)?;
    doc["grant"] = match &grant {
        Some(grant) => serde_json::to_value(grant).map_err(storage)?,
        None => Value::Null,
    };
    save_thread_doc(&s, &thread_id, &doc)?;
    if is_subagent(&s, &thread_id) {
        crate::native_chat::reload_thread_capabilities(&s, &thread_id).await;
    }
    emit(
        &s,
        &thread_id,
        "core.control.updated",
        json!({"thread_id": thread_id, "kind": "grant_set"}),
    )?;
    audit(&s, actor, &thread_id, "grant_set", json!({"grant": grant}));
    get_thread(State(s), Path(thread_id)).await
}

async fn answer_request(
    State(s): State<Arc<AppState>>,
    Path((thread_id, request_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Api {
    thread_exists(&s, &thread_id)?;
    let Some(approve) = body["approve"].as_bool() else {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "control.approve_required",
            "approve must be true or false",
        ));
    };
    Ok(Json(respond(&s, &thread_id, &request_id, approve).await?))
}

async fn post_thread_reference(
    State(s): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
    Json(body): Json<Value>,
) -> Api {
    thread_exists(&s, &thread_id)?;
    let target = body["target_thread_id"].as_str().map(str::trim).unwrap_or("");
    if target.is_empty() || target == thread_id {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "control.reference.invalid_target",
            "target_thread_id must name another existing thread",
        ));
    }
    let reason = body["reason"].as_str().unwrap_or("");
    let anchors: Vec<String> = body["anchors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let reference = attach_reference(&s, &thread_id, target, reason, anchors, "user")?;
    Ok(Json(json!({"reference": reference})))
}

async fn delete_thread_reference(
    State(s): State<Arc<AppState>>,
    Path((thread_id, target_id)): Path<(String, String)>,
) -> Api {
    thread_exists(&s, &thread_id)?;
    let mut doc = thread_doc(&s, &thread_id)?;
    let mut refs: Vec<ThreadReference> =
        serde_json::from_value(doc["references"].clone()).unwrap_or_default();
    let before = refs.len();
    refs.retain(|r| r.target_thread_id != target_id);
    doc["references"] = serde_json::to_value(&refs).map_err(storage)?;
    save_thread_doc(&s, &thread_id, &doc)?;
    let _ = emit(
        &s,
        &thread_id,
        "core.control.updated",
        json!({"thread_id": thread_id, "kind": "reference_removed", "target_thread_id": target_id}),
    );
    Ok(Json(json!({"removed": before != refs.len()})))
}

async fn delete_self_settle(
    State(s): State<Arc<AppState>>,
    Path(thread_id): Path<String>,
) -> Api {
    thread_exists(&s, &thread_id)?;
    let mut doc = thread_doc(&s, &thread_id)?;
    let had = !doc["self_settle"].is_null() || !doc["canonical_self_settle"].is_null();
    doc["self_settle"] = Value::Null;
    doc["canonical_self_settle"] = Value::Null;
    save_thread_doc(&s, &thread_id, &doc)?;
    if had {
        let _ = emit(
            &s,
            &thread_id,
            "core.control.updated",
            json!({"thread_id": thread_id, "kind": "self_settle_disarmed"}),
        );
        audit(&s, "user", &thread_id, "self_settle_disarmed", json!({}));
    }
    Ok(Json(json!({"disarmed": had})))
}

async fn pending_requests(State(s): State<Arc<AppState>>) -> Api {
    let pairs = s.store.entries(THREADS).map_err(storage)?;
    let mut rows = Vec::new();
    for (tid, doc) in pairs {
        let Some(chat_doc) = s.store.get(chat::COLLECTION, &tid).ok().flatten() else {
            continue;
        };
        let title = chat_doc["thread"]["title"].as_str().unwrap_or(&tid).to_owned();
        let project_id = chat_doc["thread"]["project_id"].as_str().unwrap_or("").to_owned();
        for req in doc["requests"].as_array().into_iter().flatten() {
            if req["status"] == "pending" {
                rows.push(json!({
                    "thread_id": tid,
                    "thread_title": title,
                    "project_id": project_id,
                    "request_id": req["request_id"],
                    "grant": req["grant"],
                    "reason": req["reason"],
                    "requested_at": req["requested_at"],
                }));
            }
        }
    }
    rows.sort_by(|a, b| b["requested_at"].as_str().cmp(&a["requested_at"].as_str()));
    Ok(Json(json!({"requests": rows, "total": rows.len()})))
}

async fn custom_thread_grants(State(s): State<Arc<AppState>>) -> Api {
    let pairs = s.store.entries(THREADS).map_err(storage)?;
    let mut rows = Vec::new();
    for (tid, doc) in pairs {
        let Ok(Some(grant)) = read_grant(&doc["grant"]) else {
            continue;
        };
        let Some(chat_doc) = s.store.get(chat::COLLECTION, &tid).ok().flatten() else {
            continue;
        };
        let (inherited, inherited_source) = inherited_grant(&s, &tid)?;
        let delta = grant.delta_from(&inherited);
        rows.push(json!({
            "thread_id": tid,
            "thread_title": chat_doc["thread"]["title"].as_str().unwrap_or(&tid),
            "project_id": chat_doc["thread"]["project_id"].as_str().unwrap_or(""),
            "updated_at": chat_doc["thread"]["updated_at"],
            "is_subagent": is_subagent(&s, &tid),
            "grant": grant,
            "inherited_grant": inherited,
            "inherited_source": inherited_source,
            "delta": delta,
        }));
    }
    rows.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
    Ok(Json(json!({"threads": rows, "total": rows.len()})))
}

async fn list_reviews(State(s): State<Arc<AppState>>) -> Api {
    let pairs = s.store.entries(THREADS).map_err(storage)?;
    let mut rows = Vec::new();
    for (tid, doc) in pairs {
        if doc["review"].is_null() {
            continue;
        }
        let Some(chat_doc) = s.store.get(chat::COLLECTION, &tid).ok().flatten() else {
            continue;
        };
        rows.push(json!({
            "thread_id": tid,
            "thread_title": chat_doc["thread"]["title"].as_str().unwrap_or(&tid),
            "project_id": chat_doc["thread"]["project_id"].as_str().unwrap_or(""),
            "review": doc["review"],
            "lifecycle": read_lifecycle(&doc),
        }));
    }
    rows.sort_by(|a, b| {
        b["review"]["updated_at"]
            .as_str()
            .cmp(&a["review"]["updated_at"].as_str())
    });
    Ok(Json(json!({"reviews": rows, "total": rows.len(),"canonical_count":s.store.list("control_reviews_v2").map_err(storage)?.len()})))
}

async fn list_discussions(
    State(s): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Api {
    let parent = query.get("parent_thread_id").map(String::as_str).unwrap_or("");
    let mut rows = s.store.list(DISCUSSIONS).map_err(storage)?;
    if !parent.is_empty() {
        rows.retain(|row| row["parent_thread_id"] == parent);
    }
    rows.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    Ok(Json(json!({"discussions": rows, "total": rows.len()})))
}

async fn get_discussion(State(s): State<Arc<AppState>>, Path(group_id): Path<String>) -> Api {
    let doc = s
        .store
        .get(DISCUSSIONS, &group_id)
        .map_err(storage)?
        .ok_or_else(|| {
            failure(
                StatusCode::NOT_FOUND,
                "control.discussion.not_found",
                format!("discussion {group_id} does not exist"),
            )
        })?;
    Ok(Json(doc))
}

async fn activity(
    State(s): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Api {
    let limit = query
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let thread = query.get("thread_id").map(String::as_str).unwrap_or("");
    let action_filter = query.get("action").map(String::as_str).unwrap_or("");
    let mut rows = s.store.list(ACTIVITY).map_err(storage)?;
    rows.retain(|row| {
        (thread.is_empty() || row["actor_thread_id"] == thread || row["target_thread_id"] == thread)
            && (action_filter.is_empty()
                || row["action"]
                    .as_str()
                    .is_some_and(|a| a.contains(action_filter)))
    });
    rows.sort_by(|a, b| b["at"].as_str().cmp(&a["at"].as_str()));
    let total = rows.len();
    rows.truncate(limit);
    Ok(Json(json!({"activity": rows, "total": total, "returned": rows.len()})))
}

pub(crate) async fn state_snapshot(
    s: Arc<AppState>,
    limit: usize,
    parent_thread_id: &str,
) -> Api {
    let Json(base) = get_settings(State(s.clone())).await?;
    let Json(pending) = pending_requests(State(s.clone())).await?;
    let Json(thread_grants) = custom_thread_grants(State(s.clone())).await?;
    let Json(reviews) = list_reviews(State(s.clone())).await?;
    let mut disc_query = HashMap::new();
    if !parent_thread_id.trim().is_empty() {
        disc_query.insert("parent_thread_id".into(), parent_thread_id.trim().to_owned());
    }
    let Json(discussions) = list_discussions(State(s.clone()), Query(disc_query)).await?;
    let mut act_query = HashMap::new();
    act_query.insert("limit".into(), limit.clamp(1, 200).to_string());
    let Json(act) = activity(State(s), Query(act_query)).await?;
    Ok(Json(json!({
        "settings": base,
        "pending_requests": pending["requests"],
        "custom_thread_grants": thread_grants["threads"],
        "reviews": reviews["reviews"],
        "discussions": discussions["discussions"],
        "recent_activity": act["activity"],
        "activity_total": act["total"],
    })))
}

async fn settle_from_ui(State(s): State<Arc<AppState>>, Path(thread_id): Path<String>) -> Api {
    crate::control_lifecycle::settle_thread_now(&s, "user", &thread_id, None).await
        .map(Json).map_err(|error| (StatusCode::CONFLICT, Json(json!({"error":error}))))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .merge(crate::control_settings_api::router())
        .merge(crate::control_lifecycle_api::router())
        .route("/api/control/settings", get(get_settings).put(put_settings))
        .route("/api/control/orchestration/providers", get(get_orchestration_providers))
        .route("/api/control/activity", get(activity))
        .route("/api/control/pending-requests", get(pending_requests))
        .route("/api/control/thread-grants", get(custom_thread_grants))
        .route("/api/control/reviews", get(list_reviews))
        .route("/api/control/discussions", get(list_discussions))
        .route("/api/control/discussions/{group_id}", get(get_discussion))
        .route("/api/control/threads/{thread_id}", get(get_thread))
        .route("/api/control/threads/{thread_id}/settle", post(settle_from_ui))
        .route("/api/control/threads/{thread_id}/grant", put(put_thread_grant))
        .route(
            "/api/control/threads/{thread_id}/requests/{request_id}",
            post(answer_request),
        )
        .route(
            "/api/control/threads/{thread_id}/references",
            post(post_thread_reference),
        )
        .route(
            "/api/control/threads/{thread_id}/references/{target_id}",
            delete(delete_thread_reference),
        )
        .route(
            "/api/control/threads/{thread_id}/self-settle",
            delete(delete_self_settle),
        )
}
