//! Standalone property-test verifier for core Muteki server invariants.
//! Invoked via: `cargo run --profile local -p muteki-server --features verify-props --bin muteki-verify-props`

use std::{env, process::ExitCode};

use muteki_server::{
    agent_runtime::RuntimeError,
    attachment_delivery,
    browser_headless::browser_policy,
    chat_core,
    control::{self, OrchestrationProfile, OrchestrationSource, Settings, SubagentPolicy},
    local_origin_accepted,
    native_chat,
    runs::container_exec,
    runs::src_api::debug_logs::{DebugExportError, build_debug_archive},
    store::Store,
    workbench_settings,
    workspace_read_api,
};
use std::io::Read as _;
use proptest::{
    prelude::*,
    test_runner::{Config, TestRunner},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn cases_per_suite() -> u32 {
    env::var("MUTEKI_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(128)
}

fn runner(name: &'static str) -> TestRunner {
    let mut cfg = Config::default();
    cfg.cases = cases_per_suite();
    cfg.failure_persistence = None;
    let _ = name;
    TestRunner::new(cfg)
}

fn run_prop<S: Strategy>(
    name: &'static str,
    strategy: S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) -> Result<(), String> {
    let mut r = runner(name);
    r.run(&strategy, test)
        .map(|()| println!("PASS  [rust-prop] {name} ({} cases)", cases_per_suite()))
        .map_err(|err| format!("FAIL  [rust-prop] {name}: {err}"))
}

fn safe_text_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just('\n'),
            Just('\r'),
            Just('\t'),
            proptest::char::range(' ', '~'),
            proptest::char::range('\u{4e00}', '\u{9fff}'),
        ],
        0..120,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn filename_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        safe_text_strategy(),
        Just("".to_owned()),
        Just("..".to_owned()),
        Just("../etc/passwd".to_owned()),
        Just("a/b\\c:d\0.txt".to_owned()),
        Just("BEGIN ATTACHED FILE CONTENTS.md".to_owned()),
    ]
}

fn json_leaf_strategy() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        safe_text_strategy().prop_map(Value::String),
    ]
}

fn json_value_strategy() -> impl Strategy<Value = Value> {
    json_leaf_strategy().prop_recursive(3, 24, 6, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            proptest::collection::btree_map("[a-z_]{1,12}", inner, 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

fn main() -> ExitCode {
    let mut errors = Vec::new();

    let res = run_prop(
        "attachment_content_and_delivery_separation",
        (
            safe_text_strategy(),
            proptest::collection::vec((filename_strategy(), safe_text_strategy()), 0..4),
        ),
        |(user_text, files_spec)| {
            let files: Vec<(Value, Vec<u8>)> = files_spec
                .iter()
                .enumerate()
                .map(|(idx, (name, body))| {
                    let bytes = body.as_bytes().to_vec();
                    (
                        json!({
                            "artifact_id": format!("att-{idx}"),
                            "name": name,
                            "media_type": "text/plain",
                        }),
                        bytes,
                    )
                })
                .collect();

            let (content, delivery) = chat_core::attachment_content_and_delivery(&user_text, files)
                .map_err(|e| TestCaseError::fail(format!("unexpected error: {e}")))?;

            if files_spec.is_empty() {
                prop_assert_eq!(content, json!(user_text));
                prop_assert!(delivery.is_empty());
                return Ok(());
            }

            let parts = content.as_array().ok_or_else(|| TestCaseError::fail("expected parts array"))?;
            prop_assert_eq!(delivery.len(), files_spec.len());

            if user_text.is_empty() {
                prop_assert_eq!(parts.len(), files_spec.len());
            } else {
                prop_assert_eq!(parts.len(), files_spec.len() + 1);
                prop_assert_eq!(&parts[0], &json!({"type": "text", "text": user_text}));
            }

            let att_offset = usize::from(!user_text.is_empty());
            for (idx, (_, body)) in files_spec.iter().enumerate() {
                let block_text = parts[att_offset + idx]["text"]
                    .as_str()
                    .ok_or_else(|| TestCaseError::fail("attachment block must be text"))?;
                prop_assert!(block_text.starts_with("BEGIN ATTACHED FILE CONTENTS"));
                prop_assert!(block_text.ends_with("END ATTACHED FILE CONTENTS"));
                prop_assert_eq!(delivery[idx]["channel"].as_str(), Some("text_blocks"));
                prop_assert_eq!(delivery[idx]["size"].as_u64(), Some(body.len() as u64));
            }
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "attachment_content_rejects_control_bytes_and_unsupported_mime",
        (safe_text_strategy(), filename_strategy(), 0u8..31u8),
        |(user_text, name, ctrl)| {
            prop_assume!(!matches!(ctrl, b'\n' | b'\r' | b'\t'));
            let bad_text_file = vec![(
                json!({"artifact_id": "id-1", "name": name, "media_type": "text/plain"}),
                vec![b'a', ctrl, b'b'],
            )];
            prop_assert!(chat_core::attachment_content_and_delivery(&user_text, bad_text_file).is_err());

            let bad_mime_file = vec![(
                json!({"artifact_id": "id-2", "name": name, "media_type": "application/pdf"}),
                b"%PDF-1.4".to_vec(),
            )];
            prop_assert!(chat_core::attachment_content_and_delivery(&user_text, bad_mime_file).is_err());
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "attachment_delivery_prepare_containment_and_handoff_integrity",
        (
            filename_strategy(),
            proptest::collection::vec(any::<u8>(), 0..256),
            safe_text_strategy(),
        ),
        |(name, bytes, message_text)| {
            let dir = tempfile::tempdir().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let cwd_canon = dir.path().canonicalize().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let att_root = cwd_canon.join(".muteki").join("attachments");
            let id = uuid::Uuid::new_v4().to_string();
            let digest = format!("{:x}", Sha256::digest(&bytes));
            let meta = json!({
                "artifact_id": id,
                "name": name,
                "media_type": "application/octet-stream",
                "size": bytes.len(),
                "sha256": digest,
            });

            let (attachments, delivery) = attachment_delivery::prepare(dir.path(), vec![(meta.clone(), bytes.clone())], false)
                .map_err(|e| TestCaseError::fail(format!("prepare failed: {e}")))?;

            prop_assert_eq!(attachments.len(), 1);
            prop_assert_eq!(delivery.len(), 1);
            let staged_path = attachments[0]
                .source_path
                .as_deref()
                .ok_or_else(|| TestCaseError::fail("missing source_path"))?;
            let staged_canon = std::path::Path::new(staged_path)
                .canonicalize()
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert!(staged_canon.starts_with(&att_root));
            let disk_bytes = std::fs::read(&staged_canon).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(disk_bytes, bytes.clone());

            let mut bad_digest = meta.clone();
            bad_digest["sha256"] = json!("0".repeat(64));
            if digest != "0".repeat(64) {
                prop_assert!(attachment_delivery::prepare(dir.path(), vec![(bad_digest, bytes.clone())], false).is_err());
            }
            let mut bad_size = meta;
            bad_size["size"] = json!(bytes.len() + 1);
            prop_assert!(attachment_delivery::prepare(dir.path(), vec![(bad_size, bytes)], false).is_err());

            let msg = json!({
                "message_id": "m1",
                "text": message_text,
                "source_thread_id": "t0",
                "attachments": [id],
            });
            let refs = attachment_delivery::handoff_refs([&msg], |_| true);
            prop_assert_eq!(refs.len(), 1);
            prop_assert_eq!(&refs[0]["attachments"], &json!([id]));
            prop_assert_eq!(&msg["text"], &json!(message_text));
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "local_origin_and_browser_policy_boundaries",
        (
            1025u16..60000u16,
            prop_oneof![Just("127.0.0.1"), Just("localhost"), Just("[::1]")],
            "[a-z0-9]{1,10}\\.example",
            any::<bool>(),
        ),
        |(port, loopback_host, ext_host, exposed)| {
            let host_header = format!("{ext_host}:{port}");
            prop_assert!(local_origin_accepted(None, Some(&host_header), exposed));

            let lb_origin = format!("http://{loopback_host}:{port}");
            prop_assert!(local_origin_accepted(Some(&lb_origin), Some(&host_header), exposed));

            let ext_origin = format!("http://{ext_host}:{port}");
            let spoof_origin = format!("http://127.0.0.1.{ext_host}:{port}");
            prop_assert_eq!(
                local_origin_accepted(Some(&ext_origin), Some(&host_header), exposed),
                exposed
            );
            prop_assert!(!local_origin_accepted(
                Some(&spoof_origin),
                Some(&host_header),
                exposed
            ));
            prop_assert!(!local_origin_accepted(
                Some("javascript:alert(1)"),
                Some(&host_header),
                exposed
            ));

            let ui_port = if port == 3101 || port == 3103 { port + 1 } else { port };
            let ui_origin = format!("http://{loopback_host}:{ui_port}");
            let api_port = ui_port.saturating_add(1).max(1025);
            let ports = browser_policy::protected_ports(api_port, Some(&ui_origin));
            prop_assert!(ports.windows(2).all(|w| w[0] < w[1]));
            prop_assert!(ports.contains(&api_port));
            prop_assert!(ports.contains(&3101));
            prop_assert!(ports.contains(&3103));
            prop_assert!(ports.contains(&ui_port));

            let origins = browser_policy::browser_origins(api_port, Some(&ui_origin));
            prop_assert!(origins.windows(2).all(|w| w[0] < w[1]));
            if api_port != 3101 {
                prop_assert!(origins.contains(&"http://127.0.0.1:3101".to_owned()));
                prop_assert!(origins.contains(&"http://localhost:3101".to_owned()));
            }
            let valid_target = reqwest::Url::parse(&format!("{ui_origin}/chat/abc?x=1")).unwrap();
            prop_assert!(browser_policy::allowed(&valid_target, &origins));

            let with_creds = reqwest::Url::parse(&format!("http://user:pass@{loopback_host}:{ui_port}/")).unwrap();
            prop_assert!(!browser_policy::allowed(&with_creds, &origins));

            let bad_scheme = reqwest::Url::parse("file:///etc/passwd").unwrap();
            prop_assert!(!browser_policy::allowed(&bad_scheme, &origins));
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "store_roundtrip_cas_legacy_fold_and_redaction",
        (
            "[a-z]{1,12}",
            json_value_strategy(),
            json_value_strategy(),
            safe_text_strategy(),
            safe_text_strategy(),
        ),
        |(id, val1, val2, user_msg, assistant_msg)| {
            let store = Store::open(":memory:").map_err(|e| TestCaseError::fail(e.to_string()))?;
            store.put("prop_col", &id, &val1).map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(store.get("prop_col", &id).unwrap(), Some(val1.clone()));

            let first = store.put_if_absent("prop_once", &id, &val1).unwrap();
            let second = store.put_if_absent("prop_once", &id, &val2).unwrap();
            prop_assert!(first);
            prop_assert!(!second);
            prop_assert_eq!(store.get("prop_once", &id).unwrap(), Some(val1.clone()));

            prop_assert!(store.compare_and_put("prop_cas", &id, 1, val1.clone()).unwrap().is_err());
            let ok = store.compare_and_put("prop_cas", &id, 0, val1.clone()).unwrap();
            prop_assert!(ok.is_ok());
            prop_assert_eq!(ok.unwrap()["version"].as_u64(), Some(1));
            prop_assert!(store.compare_and_put("prop_cas", &id, 0, val2.clone()).unwrap().is_err());
            let stored = store.get("prop_cas", &id).unwrap().unwrap();
            prop_assert_eq!(&stored["values"], &val1);

            let legacy = vec![
                json!({"message_id": "m1", "role": "user", "text": user_msg}),
                json!({"message_id": "m2", "role": "assistant", "text": assistant_msg}),
                json!({"message_id": "m3", "role": "unknown_legacy_role"}),
            ];
            let folded = chat_core::fold_messages(&legacy);
            prop_assert_eq!(folded.len(), 2);
            prop_assert_eq!(&folded[0]["text"], &json!(user_msg));
            prop_assert_eq!(&folded[1]["text"], &json!(assistant_msg));

            let secret_key = "api_key";
            let mut payload = json!({
                "public_field": val1,
                secret_key: val2,
                "nested": {
                    "token": "top-secret-token",
                    "safe_label": "keep-me",
                    "items": [{"password": "1234", "name": "ok"}],
                }
            });
            workbench_settings::redact_request(&mut payload);
            prop_assert_eq!(&payload[secret_key], &json!("[REDACTED]"));
            prop_assert_eq!(&payload["nested"]["token"], &json!("[REDACTED]"));
            prop_assert_eq!(&payload["nested"]["items"][0]["password"], &json!("[REDACTED]"));
            prop_assert_eq!(&payload["public_field"], &val1);
            prop_assert_eq!(&payload["nested"]["safe_label"], &json!("keep-me"));
            prop_assert_eq!(&payload["nested"]["items"][0]["name"], &json!("ok"));

            let once = payload.clone();
            workbench_settings::redact_request(&mut payload);
            prop_assert_eq!(payload, once);
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "byte_size_limits_and_typed_error_contract",
        (
            1u64..1024u64,
            prop_oneof![Just(""), Just("b"), Just("k"), Just("kb"), Just("m"), Just("mb"), Just("g"), Just("gb")],
            any::<bool>(),
            safe_text_strategy(),
        ),
        |(amount, unit, retryable, message)| {
            let input = json!(format!(" {amount}{} ", unit.to_ascii_uppercase()));
            let (normalized, bytes) = container_exec::parse_byte_size(&input, "worker_output_limit")
                .map_err(|e| TestCaseError::fail(format!("valid size rejected: {e}")))?;
            let expected_scale = match unit {
                "" | "b" => 1u64,
                "k" | "kb" => 1024u64,
                "m" | "mb" => 1024u64 * 1024,
                "g" | "gb" => 1024u64 * 1024 * 1024,
                _ => unreachable!(),
            };
            prop_assert_eq!(bytes, amount * expected_scale);
            prop_assert!(!normalized.contains(' '));

            for invalid in [json!(true), json!(false), json!(""), json!("0m"), json!("-10g"), json!("12z")] {
                prop_assert!(container_exec::parse_byte_size(&invalid, "worker_output_limit").is_err());
            }

            let err = if retryable {
                RuntimeError::retryable("runtime.output_limit_exceeded", message.clone())
            } else {
                RuntimeError::new("capability.unsupported", message.clone())
            };
            let (status, axum::Json(body)) = native_chat::failure(&err);
            prop_assert!(status.is_client_error());
            prop_assert_eq!(body["error"]["code"].as_str(), Some(err.code));
            prop_assert_eq!(body["error"]["message"].as_str(), Some(message.as_str()));
            prop_assert_eq!(body["error"]["retryable"].as_bool(), Some(retryable));
            prop_assert!(!body["error"]["category"].as_str().unwrap_or("").is_empty());
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    // 5. Two-layer orchestration mode isolation, inheritance & digest invariants
    let res = run_prop(
        "orchestration_mode_isolation_and_inheritance",
        (
            prop_oneof![Just("codex"), Just("claude"), Just("gemini"), Just("pi")],
            prop_oneof![Just("gpt-5"), Just("claude-sonnet"), Just("gemini-2.5-pro")],
            prop_oneof![Just(""), Just("low"), Just("medium"), Just("high")],
            prop_oneof![Just(""), Just("planner"), Just("reviewer"), Just("researcher")],
            prop_oneof![Just(""), Just("planner"), Just("reviewer"), Just("implementer")],
            prop_oneof![Just(""), Just("verifier"), Just("reviewer")],
            any::<bool>(),
        ),
        |(src_provider, src_model, src_effort, chat_default, src_default, legacy_default, has_override)| {
            prop_assert_eq!(control::normalize_orchestration_mode(""), Some("conversation"));
            prop_assert_eq!(control::normalize_orchestration_mode("Chat"), Some("conversation"));
            prop_assert_eq!(control::normalize_orchestration_mode("conversation"), Some("conversation"));
            prop_assert_eq!(control::normalize_orchestration_mode(" SRC "), Some("src"));
            prop_assert_eq!(control::normalize_orchestration_mode("ctf"), None);
            prop_assert_eq!(control::normalize_orchestration_mode("pentest"), None);

            let mut base = OrchestrationProfile::default();
            base.default_role = format!("  {chat_default}  ");
            base = base.sanitized();
            let base_once = base.clone().sanitized();
            prop_assert_eq!(&base, &base_once);
            prop_assert_eq!(base.digest(), base_once.digest());

            let mut src_profile = OrchestrationProfile::default();
            src_profile.default_role = format!(" {src_default} ");
            if let Some(reviewer) = src_profile.roles.iter_mut().find(|r| r.id == "reviewer") {
                reviewer.mode = "pinned".to_owned();
                reviewer.provider = src_provider.to_owned();
                reviewer.model = src_model.to_owned();
                reviewer.effort = src_effort.to_owned();
            }
            src_profile = src_profile.sanitized();
            prop_assert_ne!(src_profile.digest(), base.digest());

            let mut cfg = Settings {
                subagent_policy: SubagentPolicy {
                    default_role: legacy_default.to_owned(),
                    ..SubagentPolicy::default()
                },
                orchestration: base.clone(),
                ..Settings::default()
            };
            if has_override {
                cfg.orchestration_overrides
                    .insert(control::ORCHESTRATION_MODE_SRC.to_owned(), src_profile.clone());
            }

            let resolved_chat = cfg.orchestration_for_mode("conversation");
            let resolved_src = cfg.orchestration_for_mode("src");
            prop_assert_eq!(resolved_chat.mode, "conversation");
            prop_assert_eq!(resolved_chat.source, OrchestrationSource::Default);
            prop_assert_eq!(&resolved_chat.profile, &base);
            prop_assert_eq!(resolved_src.mode, "src");

            if has_override {
                prop_assert_eq!(resolved_src.source, OrchestrationSource::ModeOverride);
                prop_assert_eq!(&resolved_src.profile, &src_profile);
                prop_assert_eq!(
                    cfg.effective_default_role(&resolved_src),
                    (!src_default.is_empty()).then(|| src_default.to_owned())
                );
            } else {
                prop_assert_eq!(resolved_src.source, OrchestrationSource::Default);
                prop_assert_eq!(&resolved_src.profile, &base);
                let expected_chat_role = if !chat_default.is_empty() {
                    Some(chat_default.to_owned())
                } else if !legacy_default.is_empty() {
                    Some(legacy_default.to_owned())
                } else {
                    None
                };
                prop_assert_eq!(cfg.effective_default_role(&resolved_src), expected_chat_role);
            }

            // Reverting SRC override immediately restores inheritance from chat base
            cfg.orchestration_overrides.remove(control::ORCHESTRATION_MODE_SRC);
            let reverted = cfg.orchestration_for_mode("src");
            prop_assert_eq!(reverted.source, OrchestrationSource::Default);
            prop_assert_eq!(reverted.profile, base);
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "capability_truthfulness_and_skill_scoping",
        (any::<bool>(), any::<bool>(), any::<bool>()),
        |(restart_bound, campaign, use_pentest)| {
            let prompt = muteki_server::runs::ctf_step_system_prompt(restart_bound);
            prop_assert!(!prompt.contains("/var/lib/muteki-hosted"));
            prop_assert!(!prompt.contains("/proc"));
            if restart_bound {
                prop_assert!(prompt.contains("restart_target"));
            } else {
                prop_assert!(!prompt.contains("restart_target"));
            }

            let mode = if use_pentest { "pentest" } else { "ctf" };
            let skill = muteki_server::runs::worker::render_blackboard_skill(
                mode,
                restart_bound,
                campaign,
            );
            if use_pentest {
                prop_assert!(!skill.contains("submit-flag"));
                prop_assert!(!skill.contains("restart-target"));
                prop_assert!(skill.contains("submit-report"));
                prop_assert!(skill.contains("recent-evidence"));
            } else {
                prop_assert!(skill.contains("submit-flag"));
                prop_assert!(!skill.contains("submit-report"));
                prop_assert_eq!(skill.contains("restart-target"), restart_bound);
                prop_assert_eq!(
                    skill.contains("MUTEKI_WORKER_EXECUTION_MODE=campaign"),
                    campaign
                );
            }

            let mut available = vec!["blackboard".to_owned(), "submit-fact".to_owned()];
            if restart_bound {
                available.push("restart_target".to_owned());
            }
            prop_assert!(
                muteki_server::runs::worker::validate_requested_capabilities(
                    &["blackboard".to_owned()],
                    &available
                )
                .is_ok()
            );
            let req_restart = vec!["restart_target".to_owned()];
            prop_assert_eq!(
                muteki_server::runs::worker::validate_requested_capabilities(
                    &req_restart,
                    &available
                )
                .is_ok(),
                restart_bound
            );
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "step_tracker_syncs_live_claimed_running",
        (1u64..1000u64, any::<bool>(), "[a-z][a-z0-9-]{2,12}"),
        |(claim_seq, concluded, worker_name)| {
            let dir = tempfile::tempdir().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let mut snapshot = json!({
                "open_steps": [{
                    "id": "step_001",
                    "action": "probe target",
                    "running": false,
                    "worker_id": "",
                    "has_live_context": false
                }],
                "done_steps": []
            });
            let mut events = vec![json!({
                "seq": claim_seq,
                "event_type": "blackboard.delta",
                "ts": 100.0,
                "payload": {
                    "kind": "intent_claimed",
                    "intent_id": "step_001",
                    "worker": worker_name
                }
            })];
            if concluded {
                events.push(json!({
                    "seq": claim_seq + 1,
                    "event_type": "blackboard.delta",
                    "ts": 105.0,
                    "payload": {
                        "kind": "intent_concluded",
                        "intent_id": "step_001"
                    }
                }));
            }
            muteki_server::runs::apply_step_tracker_from_events(
                &mut snapshot,
                dir.path(),
                &events,
            );
            let step = &snapshot["open_steps"][0];
            prop_assert_eq!(step["running"].as_bool(), Some(!concluded));
            prop_assert_eq!(step["worker_id"].as_str(), Some(worker_name.as_str()));
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "run_titler_parse_and_overwrite_guard",
        ("[a-z0-9]{3,8}", "[a-zA-Z0-9 _-]{3,20}", 85usize..130usize),
        |(host, custom, over_len)| {
            let url_target = format!("https://{host}.example.test:8080/login");
            let same_target = json!({"name": url_target, "target": url_target});
            let empty_name = json!({"name": "", "target": url_target});
            let auto_same = muteki_server::runs::titler::should_auto_title(None, &same_target);
            let auto_empty = muteki_server::runs::titler::should_auto_title(None, &empty_name);
            let auto_custom = muteki_server::runs::titler::should_auto_title(
                Some(custom.trim()),
                &same_target,
            );
            prop_assert!(auto_same);
            prop_assert!(auto_empty);
            prop_assert!(!auto_custom || custom.trim().is_empty());

            let fenced = format!("<think>reasoning</think>\n```json\n{{\"title\":\"{custom} Title\"}}\n```");
            let parsed = muteki_server::runs::titler::parse_title_response(&fenced)
                .ok_or_else(|| TestCaseError::fail("expected fenced JSON title"))?;
            prop_assert!(!parsed.contains('\n'));
            prop_assert!(parsed.ends_with("Title"));

            let long_raw = "x".repeat(over_len);
            let truncated = muteki_server::runs::titler::parse_title_response(&long_raw)
                .ok_or_else(|| TestCaseError::fail("expected plain title"))?;
            prop_assert!(truncated.chars().count() <= 80);

            let dir = tempfile::tempdir().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let store = muteki_server::store::Store::open(dir.path().join("muteki.sqlite"))
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let state = muteki_server::AppState::new(store);
            state
                .store
                .put(
                    "runs",
                    "run-auto",
                    &json!({"id": "run-auto", "name": url_target, "name_autogen": true}),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let updated = muteki_server::runs::titler::apply_generated_title(
                &state,
                "run-auto",
                "Auto Named Task",
            )
            .map_err(|e| TestCaseError::fail(e))?;
            prop_assert!(updated);
            let doc = state
                .store
                .get("runs", "run-auto")
                .map_err(|e| TestCaseError::fail(e.to_string()))?
                .ok_or_else(|| TestCaseError::fail("missing run-auto"))?;
            prop_assert_eq!(doc["name"].as_str(), Some("Auto Named Task"));

            state
                .store
                .put(
                    "runs",
                    "run-custom",
                    &json!({"id": "run-custom", "name": "Keep Me", "custom_name": "Keep Me", "name_autogen": false}),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let overwritten = muteki_server::runs::titler::apply_generated_title(
                &state,
                "run-custom",
                "Should Not Overwrite",
            )
            .map_err(|e| TestCaseError::fail(e))?;
            prop_assert!(!overwritten);
            let custom_doc = state
                .store
                .get("runs", "run-custom")
                .map_err(|e| TestCaseError::fail(e.to_string()))?
                .ok_or_else(|| TestCaseError::fail("missing run-custom"))?;
            prop_assert_eq!(custom_doc["name"].as_str(), Some("Keep Me"));
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "worker_plugin_staging_idempotent",
        "[a-z][a-z0-9-]{2,10}",
        |skill_slug| {
            let dir = tempfile::tempdir().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let pkg_dir = dir.path().join("pkg");
            let skill_dir = pkg_dir.join("skills").join(&skill_slug);
            std::fs::create_dir_all(&skill_dir).map_err(|e| TestCaseError::fail(e.to_string()))?;
            std::fs::write(
                pkg_dir.join("plugin.json"),
                json!({
                    "name": "test-plugin",
                    "version": "0.1.0",
                    "skills": format!("./skills/{skill_slug}")
                })
                .to_string(),
            )
            .map_err(|e| TestCaseError::fail(e.to_string()))?;
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {skill_slug}\n---\n# Demo\n"),
            )
            .map_err(|e| TestCaseError::fail(e.to_string()))?;

            let registry = muteki_server::plugins::Registry::open(&dir.path().join("chat-plugins"))
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let installed = registry
                .install(
                    &json!({"kind": "local-dir", "path": pkg_dir.to_string_lossy()}),
                    Some(&json!(["ctf"])),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let pkg_id = installed["id"]
                .as_str()
                .ok_or_else(|| TestCaseError::fail("missing installed plugin id"))?;
            registry
                .update(pkg_id, Some(true), false, None, "", Some(&json!(["ctf"])))
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            let worker_ws = dir.path().join("worker-ws");
            std::fs::create_dir_all(&worker_ws).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let first = registry
                .stage_worker_assets(&worker_ws, "pi", "ctf")
                .map_err(|e| TestCaseError::fail(format!("first stage failed: {e}")))?;
            let second = registry
                .stage_worker_assets(&worker_ws, "pi", "ctf")
                .map_err(|e| TestCaseError::fail(format!("second stage failed: {e}")))?;
            prop_assert_eq!(first.skills.len(), 1);
            prop_assert_eq!(first.skills, second.skills);
            prop_assert_eq!(first.extensions, second.extensions);
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "terminal_settlement_and_multi_flag_solved_invariants",
        (
            any::<bool>(),
            any::<bool>(),
            1u64..=4u64,
            any::<bool>(),
            0usize..=3usize,
            any::<bool>(),
            any::<bool>(),
            0usize..7usize,
        ),
        |(
            is_pentest,
            multi_flag,
            expected_flags,
            platform_confirmation_required,
            flags_count,
            doc_solved,
            requested_solved,
            reason_idx,
        )| {
            let mode = if is_pentest { "pentest" } else { "ctf" };
            let reasons = [
                "operator_stop",
                "solved",
                "finished",
                "no_progress",
                "budget_exhausted",
                "preflight_failed",
                "runtime_failure",
            ];
            let requested_reason = reasons[reason_idx % reasons.len()];
            let flags: Vec<String> = (0..flags_count).map(|i| format!("flag{{p-{i}}}")).collect();
            let doc = json!({
                "mode": mode,
                "multi_flag": multi_flag,
                "expected_flags": expected_flags,
                "platform_confirmation_required": platform_confirmation_required,
                "solved": doc_solved,
                "flags": flags,
            });
            let (out_solved, out_reason) = muteki_server::runs::settle_terminal_outcome(
                &doc,
                requested_solved,
                requested_reason,
            );
            let hard_failure = matches!(requested_reason, "preflight_failed" | "runtime_failure");
            if mode == "ctf"
                && multi_flag
                && expected_flags <= 1
                && !platform_confirmation_required
                && flags_count > 0
                && !hard_failure
            {
                prop_assert!(out_solved);
                if matches!(requested_reason, "operator_stop" | "no_progress" | "budget_exhausted") {
                    prop_assert_eq!(out_reason.as_str(), "solved");
                }
            }
            if (platform_confirmation_required || mode == "pentest" || flags_count == 0)
                && !doc_solved
                && !requested_solved
            {
                prop_assert!(!out_solved);
            }
            if expected_flags > 1 && !doc_solved && !requested_solved {
                prop_assert!(!out_solved);
            }
            if hard_failure && !doc_solved && !requested_solved {
                prop_assert!(!out_solved);
                prop_assert_eq!(out_reason.as_str(), requested_reason);
            }
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "local_pi_usage_attribution_and_exclusive_cache_pricing",
        (
            "[a-z0-9-]{4,16}",
            1u64..100_000u64,
            1u64..50_000u64,
            150_000u64..2_000_000u64,
        ),
        |(run_id, input_tok, output_tok, cache_read_tok)| {
            let tid = format!("ctf-pi-{run_id}");
            let mut row = json!({
                "id": format!("turn-{run_id}"),
                "engine": "pi",
                "model": "provider/model",
                "source": "runtime",
                "workspace_kind": "conversation",
                "thread_id": tid,
                "run_id": tid,
                "input_tokens": input_tok,
                "output_tokens": output_tok,
                "cache_read_tokens": cache_read_tok,
                "cache_write_tokens": 0,
            });
            muteki_server::usage_history::normalize_local_pi_ctf_row(&mut row);
            prop_assert_eq!(row["run_id"].as_str(), Some(run_id.as_str()));
            prop_assert_eq!(row["thread_id"].as_str(), Some(tid.as_str()));
            prop_assert_eq!(row["actor_kind"].as_str(), Some("solve"));
            prop_assert_eq!(row["billing_scope"].as_str(), Some("solve"));
            prop_assert_eq!(row["worker_id"].as_str(), Some("cli-pi#1"));
            prop_assert_eq!(row["workspace_kind"].as_str(), Some("ctf"));

            let store = Store::open(":memory:").map_err(|e| TestCaseError::fail(e.to_string()))?;
            store
                .put(
                    "pricing_live_cache",
                    "public_model_prices",
                    &json!({
                        "document": {
                            "_meta": { "schema_version": 1, "generated_at": "2026-10-29T00:00:00Z" },
                            "provider/model": {
                                "input_cost_per_token": 0.000002,
                                "output_cost_per_token": 0.000004,
                                "cache_read_input_token_cost": 0.0000005,
                                "cache_creation_input_token_cost": 0.0000025
                            }
                        },
                        "fetched_at": 1_800_000_000.0
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let mut rows = vec![row];
            muteki_server::usage_history::price_rows(&store, &mut rows)
                .map_err(TestCaseError::fail)?;
            let cost = rows[0]["cost"].as_f64();
            prop_assert!(cost.is_some_and(|c| c > 0.0));
            prop_assert_eq!(rows[0]["amount_source"].as_str(), Some("catalog"));
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "local_pi_session_file_uses_latest_step_context_not_sum",
        proptest::collection::vec((100u64..50_000u64, 10u64..10_000u64), 2..6),
        |steps| {
            let dir = tempfile::tempdir().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let path = dir.path().join("session.jsonl");
            let mut lines = String::new();
            let mut last_total = 0u64;
            for (inp, out) in &steps {
                let total = inp + out;
                last_total = total;
                lines.push_str(
                    &json!({
                        "message": {
                            "role": "assistant",
                            "usage": {
                                "input": inp,
                                "output": out,
                                "totalTokens": total
                            }
                        }
                    })
                    .to_string(),
                );
                lines.push('\n');
            }
            lines.push_str(&json!({"message": {"role": "user", "content": "hi"}}).to_string());
            lines.push('\n');
            std::fs::write(&path, lines).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let read = muteki_server::runs::local_pi::read_latest_pi_session_context_used(&path);
            prop_assert_eq!(read, Some(last_total));
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "ctf_pi_threads_excluded_from_chat_inbox_snapshot",
        (
            proptest::collection::vec("[a-z0-9]{4,10}", 1..6),
            proptest::collection::vec("[a-z0-9]{4,10}", 1..4),
            proptest::collection::vec("[a-z0-9]{4,10}", 1..4),
            proptest::collection::vec("[a-z0-9]{4,10}", 0..3),
            any::<bool>(),
        ),
        |(normal_tags, ctf_tagged, ctf_prefixed, side_tags, compact)| {
            let store = Store::open(":memory:").map_err(|e| TestCaseError::fail(e.to_string()))?;
            let mut expected_ids = std::collections::BTreeSet::new();
            let mk_doc = |tid: &str, extra: Value| {
                let mut thread = json!({
                    "thread_id": tid,
                    "title": format!("Thread {tid}"),
                    "mode": "conversation",
                    "project_id": null,
                    "updated_at": "2026-11-22T00:00:00Z",
                    "archived": false,
                    "state": {
                        "status": "idle",
                        "running_turn_id": null,
                        "queue_count": 0,
                        "queue_paused": false,
                        "unread": false
                    }
                });
                if let Some(obj) = extra.as_object() {
                    for (k, v) in obj {
                        thread[k] = v.clone();
                    }
                }
                json!({
                    "thread": thread,
                    "queue": [],
                    "runtime": {"adapter_id": "openai", "instance_id": "text"},
                    "messages": [],
                    "turns": [],
                    "event_seq": 1
                })
            };

            for (i, s) in normal_tags.iter().enumerate() {
                let tid = format!("chat-{i}-{s}");
                let doc = mk_doc(&tid, json!({}));
                prop_assert!(!muteki_server::chat::is_ctf_thread(&doc["thread"]));
                store
                    .put("conversation_threads", &tid, &doc)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
                expected_ids.insert(tid);
            }
            for (i, s) in ctf_tagged.iter().enumerate() {
                let tid = format!("custom-ctf-{i}-{s}");
                let doc = mk_doc(&tid, json!({"ctf_run_id": format!("run-tagged-{i}-{s}")}));
                prop_assert!(muteki_server::chat::is_ctf_thread(&doc["thread"]));
                store
                    .put("conversation_threads", &tid, &doc)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
            }
            for (i, s) in ctf_prefixed.iter().enumerate() {
                let tid = format!("ctf-pi-run-{i}-{s}");
                let doc = mk_doc(&tid, json!({}));
                prop_assert!(muteki_server::chat::is_ctf_thread(&doc["thread"]));
                store
                    .put("conversation_threads", &tid, &doc)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
            }
            for (i, s) in side_tags.iter().enumerate() {
                let tid = format!("side-{i}-{s}");
                let doc = mk_doc(&tid, json!({"side_chat": {"parent_thread_id": "root"}}));
                prop_assert!(!muteki_server::chat::is_ctf_thread(&doc["thread"]));
                store
                    .put("conversation_threads", &tid, &doc)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
            }

            let snap = muteki_server::chat::inbox_snapshot_from_store(
                &store, "epoch-1", "", 7, compact,
            )
            .map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert_eq!(snap["count"].as_u64(), Some(expected_ids.len() as u64));

            let thread_ids: std::collections::BTreeSet<String> = snap["threads"]
                .as_array()
                .ok_or_else(|| TestCaseError::fail("threads must be array"))?
                .iter()
                .filter_map(|t| t["thread_id"].as_str().map(str::to_owned))
                .collect();
            let summary_ids: std::collections::BTreeSet<String> = snap["summaries"]
                .as_array()
                .ok_or_else(|| TestCaseError::fail("summaries must be array"))?
                .iter()
                .filter_map(|s| s["thread_id"].as_str().map(str::to_owned))
                .collect();

            prop_assert_eq!(thread_ids, expected_ids.clone());
            prop_assert_eq!(summary_ids, expected_ids);
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "src_debug_logs_bundle_isolates_task_and_redacts_secrets",
        (
            "[a-zA-Z0-9]{12,24}",
            10u64..100_000u64,
            10u64..100_000u64,
            10usize..240usize,
        ),
        |(secret_suffix, input_tokens, output_tokens, extra_chars)| {
            let dir = tempfile::tempdir().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let store = Store::open(dir.path().join("muteki.sqlite"))
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            let secret_bearer = format!("br_{secret_suffix}");
            let secret_sk = format!("sk-{secret_suffix}");
            let secret_ghp = format!("ghp_{secret_suffix}");
            let secret_pass = format!("pw_{secret_suffix}");
            let secret_api_key = format!("ak_{secret_suffix}");
            let secret_evt_tok = format!("et_{secret_suffix}");

            let long_traffic_body = "中".repeat(4096 + extra_chars);
            let long_chat_msg = "聊".repeat(4096 + extra_chars);

            store
                .put(
                    "src_tasks",
                    "task-a",
                    &json!({
                        "task_id": "task-a",
                        "title": format!("Alpha Research Bearer {secret_bearer}"),
                        "project_id": "proj-a",
                        "binding": {"task_id": "task-a", "thread_id": "thr-a-main"}
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            store
                .put(
                    "src_tasks",
                    "task-b",
                    &json!({
                        "task_id": "task-b",
                        "title": "TASK_B_TITLE_MARKER",
                        "project_id": "proj-b",
                        "binding": {"task_id": "task-b", "thread_id": "thr-b-main"}
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            store
                .put(
                    "src_live_records",
                    "srec:task-a:traffic:t1",
                    &json!({
                        "version": 1,
                        "values": {
                            "task_id": "task-a",
                            "kind": "traffic_exchange",
                            "record_id": "t1",
                            "payload": {
                                "headers": [
                                    ["Authorization", format!("Bearer {secret_bearer}")],
                                    ["X-Custom", "safe-header"]
                                ],
                                "body": long_traffic_body,
                                "api_key": secret_api_key,
                                "usage": {
                                    "input_tokens": input_tokens,
                                    "output_tokens": output_tokens
                                }
                            }
                        }
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            store
                .put(
                    "src_live_records",
                    "srec:task-b:asset:b1",
                    &json!({
                        "version": 1,
                        "values": {
                            "task_id": "task-b",
                            "kind": "asset",
                            "record_id": "b1",
                            "payload": {"marker": "TASK_B_ONLY_MARKER"}
                        }
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            store
                .put(
                    "src_task_events",
                    "evt-a-1",
                    &json!({
                        "task_id": "task-a",
                        "command_id": "cmd-a-1",
                        "token": secret_evt_tok
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            store
                .put(
                    "src_source_watermarks",
                    "[\"task-a\",\"src1\"]",
                    &json!({"task_id": "task-a", "source_id": "src1", "next_seq": 3}),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            store
                .put(
                    "src_domain_outbox",
                    "outbox-a-1",
                    &json!({
                        "outbox_id": "outbox-a-1",
                        "command_id": "cmd-a-1",
                        "effect_id": "eff-a-1",
                        "aggregate_type": "task",
                        "aggregate_id": "task-a"
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            store
                .put(
                    "src_domain_effect_receipts",
                    "eff-a-1",
                    &json!({
                        "effect_id": "eff-a-1",
                        "command_id": "cmd-a-1",
                        "state": "completed"
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            store
                .put(
                    "runs",
                    "run-a-1",
                    &json!({
                        "run_id": "run-a-1",
                        "src_task_id": "task-a",
                        "proxy_url": format!("https://alice:{secret_pass}@proxy.example.com:8080")
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            store
                .put(
                    "runs",
                    "run-b-1",
                    &json!({
                        "run_id": "run-b-1",
                        "src_task_id": "task-b",
                        "note": "TASK_B_RUN_MARKER"
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            let mk_event = |tid: &str, seq: u64, eid: &str, text: &str| {
                json!({
                    "event_type": "turn.delta",
                    "event_id": eid,
                    "stream_seq": seq,
                    "payload": {
                        "turn_id": format!("turn-{tid}"),
                        "text": text,
                        "token": secret_evt_tok
                    }
                })
            };

            store
                .chat_commit(
                    "thr-a-main",
                    &json!({
                        "thread": {
                            "thread_id": "thr-a-main",
                            "title": "Main A",
                            "mode": "src",
                            "state": {
                                "status": "idle",
                                "running_turn_id": null,
                                "subagents": [{"agent_id": "thr-a-child"}]
                            }
                        },
                        "messages": [{"text": long_chat_msg}],
                        "turns": [{"turn_id": "turn-1", "status": "completed"}],
                        "_pending_events": [mk_event("thr-a-main", 1, "e-a-1", &format!("Bearer {secret_bearer}"))]
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            store
                .chat_commit(
                    "thr-a-child",
                    &json!({
                        "thread": {
                            "thread_id": "thr-a-child",
                            "title": "Child A",
                            "mode": "src",
                            "src_delegation": {
                                "task_id": "task-a",
                                "parent_thread_id": "thr-a-main"
                            },
                            "state": {
                                "status": "completed",
                                "lineage": {"parent_thread_id": "thr-a-main"},
                                "subagents": [{"thread_id": "thr-a-grandchild"}]
                            }
                        },
                        "messages": [],
                        "turns": [],
                        "_pending_events": [mk_event("thr-a-child", 1, "e-a-2", &secret_sk)]
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            store
                .chat_commit(
                    "thr-a-grandchild",
                    &json!({
                        "thread": {
                            "thread_id": "thr-a-grandchild",
                            "title": "Grandchild A",
                            "mode": "src",
                            "state": {
                                "status": "completed",
                                "lineage": {"parent_thread_id": "thr-a-child"}
                            }
                        },
                        "messages": [],
                        "turns": [],
                        "_pending_events": [mk_event("thr-a-grandchild", 1, "e-a-3", "grandchild ok")]
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            store
                .chat_commit(
                    "thr-a-side",
                    &json!({
                        "thread": {
                            "thread_id": "thr-a-side",
                            "title": "Side A",
                            "mode": "src",
                            "side_chat": {
                                "parent_task_id": "task-a",
                                "bound_run_id": "run-a-1"
                            },
                            "state": {"status": "idle"}
                        },
                        "messages": [],
                        "turns": [],
                        "_pending_events": [mk_event("thr-a-side", 1, "e-a-4", "side ok")]
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            store
                .chat_commit(
                    "thr-b-main",
                    &json!({
                        "thread": {
                            "thread_id": "thr-b-main",
                            "title": "TASK_B_THREAD_MARKER",
                            "mode": "src",
                            "state": {"status": "idle"}
                        },
                        "messages": [{"text": "TASK_B_THREAD_MARKER"}],
                        "turns": [],
                        "_pending_events": [mk_event("thr-b-main", 1, "e-b-1", "TASK_B_EVENT_MARKER")]
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            std::fs::write(
                dir.path().join("desktop-startup.log"),
                format!("boot ok\nAuthorization: Bearer {secret_bearer}\nkey={secret_sk}\ntoken={secret_ghp}\n"),
            )
            .map_err(|e| TestCaseError::fail(e.to_string()))?;
            std::fs::create_dir_all(dir.path().join("runner"))
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            std::fs::write(
                dir.path().join("runner/runner.log"),
                format!("proxy=https://alice:{secret_pass}@example.com/path\n"),
            )
            .map_err(|e| TestCaseError::fail(e.to_string()))?;

            prop_assert!(matches!(
                build_debug_archive(&store, Some(dir.path()), "nonexistent-task"),
                Err(DebugExportError::NotFound(_))
            ));

            let archive = build_debug_archive(&store, Some(dir.path()), "task-a")
                .map_err(|e| TestCaseError::fail(e.to_string()))?;

            let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(&archive.bytes));
            let mut tar_in = tar::Archive::new(gz);
            let mut entries = std::collections::BTreeMap::<String, String>::new();
            for entry in tar_in
                .entries()
                .map_err(|e| TestCaseError::fail(e.to_string()))?
            {
                let mut entry = entry.map_err(|e| TestCaseError::fail(e.to_string()))?;
                let path = entry
                    .path()
                    .map_err(|e| TestCaseError::fail(e.to_string()))?
                    .to_string_lossy()
                    .into_owned();
                let mut text = String::new();
                entry
                    .read_to_string(&mut text)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
                entries.insert(path, text);
            }

            for expected_file in [
                "summary.json",
                "src_documents.json",
                "conversation_threads.json",
                "chat_events.jsonl",
                "related_documents.json",
                "desktop-startup-tail.log",
                "runner-tail.log",
            ] {
                prop_assert!(
                    entries.contains_key(expected_file),
                    "missing archive entry {expected_file}"
                );
            }

            for (name, content) in &entries {
                for secret in [
                    &secret_bearer,
                    &secret_sk,
                    &secret_ghp,
                    &secret_pass,
                    &secret_api_key,
                    &secret_evt_tok,
                ] {
                    prop_assert!(
                        !content.contains(secret),
                        "secret leaked in {name}"
                    );
                }
                for foreign in [
                    "task-b",
                    "TASK_B_TITLE_MARKER",
                    "TASK_B_ONLY_MARKER",
                    "TASK_B_THREAD_MARKER",
                    "TASK_B_EVENT_MARKER",
                    "TASK_B_RUN_MARKER",
                ] {
                    prop_assert!(
                        !content.contains(foreign),
                        "foreign task marker {foreign} leaked in {name}"
                    );
                }
            }

            let src_docs: Value = serde_json::from_str(&entries["src_documents.json"])
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            for col in [
                "src_tasks",
                "src_live_records",
                "src_task_events",
                "src_source_watermarks",
                "src_domain_outbox",
                "src_domain_effect_receipts",
            ] {
                prop_assert!(src_docs[col].is_array() && !src_docs[col].as_array().unwrap().is_empty());
            }
            let traffic = &src_docs["src_live_records"][0]["value"]["values"]["payload"];
            prop_assert_eq!(traffic["usage"]["input_tokens"].as_u64(), Some(input_tokens));
            prop_assert_eq!(traffic["usage"]["output_tokens"].as_u64(), Some(output_tokens));
            let truncated_body = traffic["body"].as_str().unwrap_or("");
            prop_assert!(truncated_body.contains("...<truncated:"));
            prop_assert_eq!(traffic["headers"][0][1].as_str(), Some("<REDACTED>"));
            prop_assert_eq!(traffic["headers"][1][1].as_str(), Some("safe-header"));

            let threads: Value = serde_json::from_str(&entries["conversation_threads.json"])
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            for tid in ["thr-a-main", "thr-a-child", "thr-a-grandchild", "thr-a-side"] {
                prop_assert!(threads[tid].is_object(), "missing thread {tid}");
            }
            prop_assert!(threads["thr-b-main"].is_null());
            let main_msg = threads["thr-a-main"]["messages"][0]["text"].as_str().unwrap_or("");
            prop_assert_eq!(main_msg.chars().count(), 4096 + extra_chars);

            let mut prev_seq = -1i64;
            let mut event_tids = std::collections::BTreeSet::new();
            for line in entries["chat_events.jsonl"].lines() {
                let row: Value = serde_json::from_str(line)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
                let seq = row["seq"].as_i64().unwrap_or(-1);
                prop_assert!(seq > prev_seq);
                prev_seq = seq;
                if let Some(tid) = row["thread_id"].as_str() {
                    event_tids.insert(tid.to_owned());
                }
            }
            prop_assert_eq!(event_tids.len(), 4);

            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    let res = run_prop(
        "workspace_preview_opens_only_recorded_external_reads",
        ("[a-z][a-z0-9_-]{2,10}", "[a-z][a-z0-9_-]{2,10}"),
        |(read_name, hidden_name)| {
            prop_assume!(read_name != hidden_name);
            let dir = tempfile::tempdir().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let outside = dir.path().join("outside");
            std::fs::create_dir_all(&outside).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let read_file = outside.join(format!("{read_name}.txt"));
            let hidden_file = outside.join(format!("{hidden_name}.txt"));
            std::fs::write(&read_file, b"read by a tool")
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            std::fs::write(&hidden_file, b"never read by a tool")
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let alias = outside.join(format!("{read_name}-alias.txt"));
            std::os::unix::fs::symlink(&read_file, &alias)
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let canonical = |path: &std::path::Path| {
                path.canonicalize()
                    .map_err(|e| TestCaseError::fail(e.to_string()))
            };
            let read_canonical = canonical(&read_file)?;
            let hidden_canonical = canonical(&hidden_file)?;

            let store = Store::open(dir.path().join("muteki.sqlite"))
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let tool_event = |eid: &str, seq: u64, input: &std::path::Path, resources: Value| {
                json!({
                    "event_type": "core.tool.completed",
                    "event_id": eid,
                    "stream_seq": seq,
                    "payload": {
                        "turn_id": "turn-1",
                        "tool_call_id": eid,
                        "name": "read",
                        "kind": "read",
                        "status": "completed",
                        "input": {"path": input.to_string_lossy()},
                        "resources": resources,
                    }
                })
            };
            store
                .chat_commit(
                    "thr-recorded",
                    &json!({
                        "thread": {"thread_id": "thr-recorded", "mode": "conversation"},
                        "_pending_events": [
                            tool_event("e-read", 1, &alias, json!([
                                {"kind": "workspace", "name": format!("{read_name}-alias.txt"),
                                 "path": alias.to_string_lossy(), "line": null}
                            ])),
                            tool_event("e-artifact", 2, &hidden_file, json!([
                                {"kind": "artifact", "name": "output.txt",
                                 "artifact_id": "art-1", "media_type": "text/plain"}
                            ])),
                        ]
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            store
                .chat_commit(
                    "thr-unprojected",
                    &json!({
                        "thread": {"thread_id": "thr-unprojected", "mode": "conversation"},
                        "_pending_events": [tool_event("e-legacy", 1, &hidden_file, Value::Null)]
                    }),
                )
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let state = muteki_server::AppState::new(store);

            let alias_request = alias.to_string_lossy().into_owned();
            let hidden_request = hidden_file.to_string_lossy().into_owned();
            let read_request = read_file.to_string_lossy().into_owned();
            prop_assert!(workspace_read_api::recorded_read(
                &state,
                "thr-recorded",
                &alias_request,
                &read_canonical
            ));
            prop_assert!(!workspace_read_api::recorded_read(
                &state,
                "thr-recorded",
                &hidden_request,
                &hidden_canonical
            ));
            prop_assert!(!workspace_read_api::recorded_read(
                &state,
                "thr-absent",
                &alias_request,
                &read_canonical
            ));
            prop_assert!(workspace_read_api::recorded_read(
                &state,
                "thr-unprojected",
                &hidden_request,
                &hidden_canonical
            ));
            prop_assert!(!workspace_read_api::recorded_read(
                &state,
                "thr-unprojected",
                &read_request,
                &read_canonical
            ));
            Ok(())
        },
    );
    if let Err(e) = res {
        errors.push(e);
    }

    if errors.is_empty() {
        println!("ALL RUST PROPERTY SUITES PASSED");
        ExitCode::SUCCESS
    } else {
        for err in errors {
            eprintln!("{err}");
        }
        ExitCode::FAILURE
    }
}
