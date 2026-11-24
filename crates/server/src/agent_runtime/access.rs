//! The conversation access-mode policy.
//!
//! Muteki's four access modes are a product contract; every engine's native
//! permission control is narrower and shaped differently. Two mechanisms carry
//! them, and both come from data the adapters already publish:
//!
//! * the engine's own permission option (`agent::permission_control`), which
//!   decides what never reaches an approval request at all; and
//! * this server's answer to the approval requests that do arrive, which is
//!   where `supervised` and `auto-accept-edits` differ.
//!
//! A mode is honored only where one of those can express it. `auto` hands the
//! decision to the engine's own safety classifier, so it is offered only by
//! engines that have one; nothing here emulates a classifier.

use super::RuntimeError;
use agent::{
    ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind, OptionDescriptor,
    OptionSelection, ProviderKind, permission_control,
};

pub const SUPERVISED: &str = "supervised";
pub const AUTO_ACCEPT_EDITS: &str = "auto-accept-edits";
pub const AUTO: &str = "auto";
pub const FULL_ACCESS: &str = "full-access";

pub const ALL_MODES: [&str; 4] = [SUPERVISED, AUTO_ACCEPT_EDITS, AUTO, FULL_ACCESS];

/// Which approval kinds this server answers without asking the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AutoAllow {
    pub file_read: bool,
    pub file_change: bool,
    pub tool_use: bool,
    pub exec_command: bool,
}

impl AutoAllow {
    fn none() -> Self {
        Self::default()
    }

    fn edits() -> Self {
        Self {
            file_read: true,
            file_change: true,
            ..Self::default()
        }
    }

    fn everything() -> Self {
        Self {
            file_read: true,
            file_change: true,
            tool_use: true,
            exec_command: true,
        }
    }

    fn covers(&self, kind: &ApprovalKind) -> bool {
        match kind {
            ApprovalKind::FileRead { .. } => self.file_read,
            ApprovalKind::FileChange { .. } => self.file_change,
            ApprovalKind::ToolUse { .. } => self.tool_use,
            ApprovalKind::ExecCommand { .. } => self.exec_command,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccessPlan {
    pub mode: String,
    /// Option selections applied when the session starts.
    pub selections: Vec<OptionSelection>,
    pub auto_allow: AutoAllow,
}

struct Permission {
    id: String,
    asking: Option<String>,
    classifier: Option<String>,
    permissive: Option<String>,
}

/// The engine's permission option, split into the three values the access modes
/// need. A classifier value is one that is neither the asking default nor the
/// fully permissive value; where those collapse, the engine has no classifier.
fn permission(provider: ProviderKind) -> Option<Permission> {
    let OptionDescriptor::Select {
        id,
        default_value,
        recommended,
        permissive,
        ..
    } = permission_control(provider)?
    else {
        return None;
    };
    let classifier = recommended.filter(|value| {
        Some(value) != permissive.as_ref() && Some(value) != default_value.as_ref()
    });
    Some(Permission {
        id,
        asking: default_value,
        classifier,
        permissive,
    })
}

/// The access modes an adapter honors natively.
///
/// `approvals` says whether the adapter raises approval requests at all: an
/// adapter that never asks cannot be held at `supervised`, whatever its
/// permission option says.
pub fn honored(provider: ProviderKind, approvals: bool) -> Vec<&'static str> {
    let permission = permission(provider);
    let mut modes = Vec::new();
    if approvals {
        modes.push(SUPERVISED);
        modes.push(AUTO_ACCEPT_EDITS);
    }
    match provider {
        ProviderKind::Acp => {
            modes.push(AUTO);
            modes.push(FULL_ACCESS);
        }
        _ => {
            if permission
                .as_ref()
                .is_some_and(|permission| permission.classifier.is_some())
            {
                modes.push(AUTO);
            }
            modes.push(FULL_ACCESS);
        }
    }
    modes
}

/// The modes an adapter actually offers: what its engine's permission surface
/// can express, narrowed by what the adapter declares it honors.
pub fn offered(provider: ProviderKind, approvals: bool, declared: &[String]) -> Vec<&'static str> {
    let honored = honored(provider, approvals);
    ALL_MODES
        .into_iter()
        .filter(|mode| honored.contains(mode) && declared.iter().any(|value| value == mode))
        .collect()
}

/// Resolve `mode` into what the session start and the approval answers must do.
pub fn plan(
    provider: ProviderKind,
    adapter_id: &str,
    approvals: bool,
    declared: &[String],
    mode: &str,
) -> Result<AccessPlan, RuntimeError> {
    if matches!(adapter_id, "omp.rpc_v2" | "omp.rpc") {
        return Err(RuntimeError::unsupported(
            "omp.rpc_v2 does not honor Conversation access modes (omp.rpc.access_mode_not_honored); please use omp.acp",
        ));
    }
    let mode = if mode.trim().is_empty() {
        SUPERVISED
    } else {
        mode.trim()
    };
    if !ALL_MODES.contains(&mode) {
        return Err(RuntimeError::new(
            "runtime.access_mode_invalid",
            format!("unknown access mode {mode}"),
        ));
    }
    let offered = offered(provider, approvals, declared);
    if !offered.contains(&mode) {
        return Err(RuntimeError::unsupported(format!(
            "{adapter_id} does not honor the {mode} access mode; it offers {}",
            offered.join(", ")
        )));
    }
    let permission = permission(provider);
    let value = |chosen: Option<String>| -> Vec<OptionSelection> {
        match (permission.as_ref(), chosen) {
            (Some(permission), Some(value)) => vec![OptionSelection {
                id: permission.id.clone(),
                value: serde_json::Value::String(value),
            }],
            _ => Vec::new(),
        }
    };
    let asking = permission.as_ref().and_then(|p| p.asking.clone());
    let classifier = permission.as_ref().and_then(|p| p.classifier.clone());
    let permissive = permission.as_ref().and_then(|p| p.permissive.clone());
    let (selections, auto_allow) = match mode {
        SUPERVISED => (value(asking), AutoAllow::none()),
        AUTO_ACCEPT_EDITS => (value(asking), AutoAllow::edits()),
        AUTO => (value(classifier), AutoAllow::none()),
        _ => (value(permissive), AutoAllow::everything()),
    };
    Ok(AccessPlan {
        mode: mode.to_owned(),
        selections,
        auto_allow,
    })
}

fn is_internal_control_gateway(kind: &ApprovalKind) -> bool {
    const SERVERS: [&str; 6] = [
        "muteki-control",
        "muteki-collab",
        "muteki-browser",
        "muteki-computer",
        "muteki-platform",
        "muteki-packages",
    ];
    let ApprovalKind::ToolUse { name, .. } = kind else {
        return false;
    };
    SERVERS.iter().any(|server| {
        if name == server {
            return true;
        }
        let underscored = server.replace('-', "_");
        name.starts_with(&format!("mcp__{server}__"))
            || name.starts_with(&format!("mcp__{underscored}__"))
    })
}

/// The decision this server makes for one approval request, or `None` when the
/// request belongs to the user.
pub fn decide(
    plan: &AccessPlan,
    kind: &ApprovalKind,
    options: &[ApprovalOption],
) -> Option<ApprovalDecision> {
    let internal_gateway = is_internal_control_gateway(kind);
    if !internal_gateway && !plan.auto_allow.covers(kind) {
        return None;
    }
    let wanted: &[ApprovalOptionKind] = if plan.mode == FULL_ACCESS || internal_gateway {
        &[ApprovalOptionKind::AllowAlways, ApprovalOptionKind::AllowOnce]
    } else {
        &[ApprovalOptionKind::AllowOnce]
    };
    for kind in wanted {
        if let Some(option) = options.iter().find(|option| option.kind == *kind) {
            return Some(ApprovalDecision::Option(option.id.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared() -> Vec<String> {
        ALL_MODES.iter().map(|mode| (*mode).to_owned()).collect()
    }

    fn allow_once() -> Vec<ApprovalOption> {
        vec![ApprovalOption {
            id: "allow".into(),
            label: "Allow".into(),
            kind: ApprovalOptionKind::AllowOnce,
        }]
    }

    #[test]
    fn engines_with_a_classifier_offer_auto_and_others_do_not() {
        assert!(honored(ProviderKind::ClaudeCode, true).contains(&AUTO));
        assert!(honored(ProviderKind::Codex, true).contains(&AUTO));
        assert!(honored(ProviderKind::Grok, true).contains(&AUTO));
        assert!(!honored(ProviderKind::Cursor, false).contains(&AUTO));
        assert!(!honored(ProviderKind::OpenCode, true).contains(&AUTO));
    }

    #[test]
    fn cursor_sdk_versus_cursor_acp_access_modes() {
        assert_eq!(honored(ProviderKind::Cursor, false), vec![FULL_ACCESS]);
        assert_eq!(plan(ProviderKind::Cursor, "cursor.sdk", false, &declared(), SUPERVISED)
            .unwrap_err().code, "capability.unsupported");
        let acp_declared = vec![SUPERVISED.to_owned(), AUTO_ACCEPT_EDITS.to_owned(), FULL_ACCESS.to_owned()];
        assert_eq!(
            offered(ProviderKind::Acp, true, &acp_declared),
            vec![SUPERVISED, AUTO_ACCEPT_EDITS, FULL_ACCESS]
        );
        let error = plan(ProviderKind::Acp, "cursor.acp", true, &acp_declared, AUTO).unwrap_err();
        assert_eq!(error.code, "capability.unsupported");
    }

    #[test]
    fn omp_rpc_v2_rejects_conversation_access_modes_with_hint() {
        let error = plan(ProviderKind::Pi, "omp.rpc_v2", false, &[], SUPERVISED).unwrap_err();
        assert_eq!(error.code, "capability.unsupported");
        assert!(error.message.contains("omp.acp"));
    }

    #[test]
    fn supervised_starts_the_engine_asking_and_answers_nothing() {
        let plan = plan(ProviderKind::ClaudeCode, "claude.sdk", true, &declared(), SUPERVISED).unwrap();
        assert_eq!(plan.selections[0].id, "permissionMode");
        assert_eq!(plan.selections[0].value, serde_json::json!("default"));
        assert_eq!(
            decide(&plan, &ApprovalKind::FileRead { detail: String::new() }, &allow_once()),
            None
        );
    }

    #[test]
    fn auto_accept_edits_answers_edits_but_not_commands() {
        let plan = plan(ProviderKind::Codex, "codex.app_server", true, &declared(), AUTO_ACCEPT_EDITS).unwrap();
        assert_eq!(plan.selections[0].value, serde_json::json!("ask"));
        assert!(decide(
            &plan,
            &ApprovalKind::FileChange { changes: Vec::new(), reason: None },
            &allow_once()
        )
        .is_some());
        assert_eq!(
            decide(
                &plan,
                &ApprovalKind::ExecCommand { command: "rm -rf /".into(), cwd: None, reason: None },
                &allow_once()
            ),
            None
        );
    }

    #[test]
    fn auto_hands_the_decision_to_the_engine_classifier() {
        let plan = plan(ProviderKind::Codex, "codex.app_server", true, &declared(), AUTO).unwrap();
        assert_eq!(plan.selections[0].value, serde_json::json!("auto_review"));
        assert_eq!(
            decide(
                &plan,
                &ApprovalKind::ExecCommand { command: "curl x".into(), cwd: None, reason: None },
                &allow_once()
            ),
            None
        );
    }

    #[test]
    fn full_access_prefers_a_session_grant_and_answers_every_kind() {
        let plan = plan(ProviderKind::ClaudeCode, "claude.sdk", true, &declared(), FULL_ACCESS).unwrap();
        assert_eq!(plan.selections[0].value, serde_json::json!("bypassPermissions"));
        let options = vec![
            ApprovalOption { id: "once".into(), label: "Once".into(), kind: ApprovalOptionKind::AllowOnce },
            ApprovalOption { id: "always".into(), label: "Always".into(), kind: ApprovalOptionKind::AllowAlways },
        ];
        assert_eq!(
            decide(
                &plan,
                &ApprovalKind::ExecCommand { command: "ls".into(), cwd: None, reason: None },
                &options
            ),
            Some(ApprovalDecision::Option("always".into()))
        );
    }

    #[test]
    fn an_approval_with_no_allow_option_still_reaches_the_user() {
        let plan = plan(ProviderKind::ClaudeCode, "claude.sdk", true, &declared(), FULL_ACCESS).unwrap();
        let deny_only = vec![ApprovalOption {
            id: "no".into(),
            label: "Deny".into(),
            kind: ApprovalOptionKind::RejectOnce,
        }];
        assert_eq!(
            decide(&plan, &ApprovalKind::FileRead { detail: String::new() }, &deny_only),
            None
        );
    }

    #[test]
    fn a_declaration_narrows_what_the_engine_could_express() {
        let declared = vec![SUPERVISED.to_owned(), AUTO_ACCEPT_EDITS.to_owned(), FULL_ACCESS.to_owned()];
        assert_eq!(
            offered(ProviderKind::Acp, true, &declared),
            vec![SUPERVISED, AUTO_ACCEPT_EDITS, FULL_ACCESS]
        );
        let error = plan(ProviderKind::Acp, "omp.acp", true, &declared, AUTO).unwrap_err();
        assert_eq!(error.code, "capability.unsupported");
    }

    #[test]
    fn an_unknown_mode_is_rejected_before_a_session_starts() {
        let error = plan(ProviderKind::Codex, "codex.app_server", true, &declared(), "yolo").unwrap_err();
        assert_eq!(error.code, "runtime.access_mode_invalid");
    }
}
