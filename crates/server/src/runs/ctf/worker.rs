//! CTF Worker lifecycle, context and blackboard protocol.
use super::*;
use crate::runs as run;

pub(crate) fn blackboard_allowed(
    run: &Value,
    role: &str,
    campaign: bool,
    op: &str,
    restart_bound: bool,
) -> bool {
    let role = match role {
        "bootstrap" | "explore" | "solve" => "explore",
        "fact_verifier" | "verifier" | "verify" => "verifier",
        "review" | "reviewer" => "review",
        _ => return op == "context",
    };
    let op = match op {
        "fact" => "submit_fact",
        "mark_deadend" => "dead_end",
        other => other,
    };
    match op {
        "context" | "board_query" => true,
        "restart_target" => role == "explore" && restart_bound,
        "platform_verdict" => campaign || run["platform_confirmation_required"] == true,
        "mcp_tools" | "mcp_schema" | "mcp_call" => true,
        "recent_evidence" => false,
        "read_artifact" => matches!(role, "explore" | "verifier"),
        "need_input"
            if run["allow_operator_input"] == false
                || run["task_contract"]["interaction"]["allow_operator_input"] == false =>
        {
            false
        }
        _ => match role {
            "explore" => {
                matches!(
                    op,
                    "submit_fact" | "commit_step" | "dead_end" | "need_input" | "save_poc"
                ) || (op == "submit_flag")
            }
            "verifier" => matches!(
                op,
                "submit_fact" | "submit_report" | "commit_step" | "dead_end" | "save_poc"
            ),
            "review" => matches!(
                op,
                "commit_step"
                    | "dead_end"
                    | "review_finding"
                    | "no_action"
                    | "fact_challenge"
                    | "challenge_fact"
                    | "fact_revalidation"
                    | "revalidate_fact"
                    | "fact_reject"
                    | "reject_fact"
                    | "fact_merge"
                    | "merge_facts"
                    | "merge_fact"
            ),
            _ => false,
        },
    }
}

fn same_session_continuation(
    spec: &WorkerSpec,
    challenge: &Value,
    assignment: &IntentAssignment,
) -> bool {
    spec.profile["execution_mode"] == "campaign"
        && challenge["platform_confirmation_required"] == true
        && role_for_worker_class(&assignment.worker_class) == "explore"
}

fn blackboard_execution_mode(
    spec: &WorkerSpec,
    challenge: &Value,
    assignment: &IntentAssignment,
    finalize_on_exit: bool,
) -> &'static str {
    if finalize_on_exit && same_session_continuation(spec, challenge, assignment) {
        "campaign"
    } else {
        "step"
    }
}

pub fn stage_workspace(
    session_dir: &Path,
    solver_id: &str,
    norm_engine: &str,
    challenge: &Value,
) -> std::io::Result<(PathBuf, PathBuf, PathBuf)> {
    let uploads_dir = session_dir.join("uploads");
    let arts_dir = session_dir.join("arts");
    let shared_dir = session_dir.join("shared");
    let worker_dir = session_dir.join("workers").join(solver_id);
    std::fs::create_dir_all(shared_dir.join("live"))?;
    let skill_dir = session_dir.join(".muteki-skills").join("muteki-blackboard");
    let ingress_dir = session_dir
        .join(".muteki-blackboard-ingress")
        .join(solver_id);

    for d in [
        session_dir,
        &uploads_dir,
        &arts_dir,
        &shared_dir,
        &worker_dir,
        &skill_dir,
        &ingress_dir,
    ] {
        std::fs::create_dir_all(d)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&ingress_dir, std::fs::Permissions::from_mode(0o700));
    }

    let skill_md_path = skill_dir.join("SKILL.md");
    let blackboard_py_path = skill_dir.join("blackboard.py");
    std::fs::write(&skill_md_path, super::render_blackboard_skill("ctf", false, false))?;
    std::fs::write(&blackboard_py_path, BLACKBOARD_PY)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            std::fs::set_permissions(&blackboard_py_path, std::fs::Permissions::from_mode(0o755));
    }

    let mode = "ctf";
    let canonical_ingress_for_env =
        std::fs::canonicalize(&ingress_dir).unwrap_or_else(|_| ingress_dir.clone());
    let env_json = json!({
        "MUTEKI_BLACKBOARD_INGRESS_DIR": canonical_ingress_for_env.to_string_lossy(),
        "MUTEKI_WORKER_ID": solver_id,
        "MUTEKI_EXECUTION_GENERATION": worker_execution_generation(solver_id).to_string(),
        "MUTEKI_CHALLENGE_MODE": mode,
        "MUTEKI_BLACKBOARD_ROLE": "solve",
        "MUTEKI_BLACKBOARD_PROFILE": "solve",
        "MUTEKI_WORKER_EXECUTION_MODE": "step",
        "MUTEKI_PLATFORM_CONFIRMATION_REQUIRED": "0",
        "MUTEKI_TARGET_EPOCH": "1",
    });
    let _ = std::fs::write(skill_dir.join("env.json"), env_json.to_string());

    for rel in [
        ".agents/skills",
        ".claude/skills",
        ".codex/skills",
        ".cursor/skills",
        ".pi/skills",
        ".omp/skills",
        ".kimi/skills",
        ".grok/skills",
        ".opencode/skills",
    ] {
        let parent = session_dir.join(rel);
        let _ = std::fs::create_dir_all(&parent);
        let dst = parent.join("muteki-blackboard");
        if dst.symlink_metadata().is_ok() {
            let _ = std::fs::remove_file(&dst).or_else(|_| std::fs::remove_dir_all(&dst));
        }
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink("../../.muteki-skills/muteki-blackboard", &dst);
        }
    }

    if let Some(attachments) = challenge["attachments"].as_array() {
        for item in attachments {
            let src_str = item
                .as_str()
                .or_else(|| item["path"].as_str())
                .unwrap_or("")
                .trim();
            if src_str.is_empty() {
                continue;
            }
            let src = PathBuf::from(src_str);
            if !src.is_file() {
                continue;
            }
            if let Some(fname) = src.file_name() {
                let dst_root = session_dir.join(fname);
                let dst_shared = shared_dir.join(fname);
                if src.canonicalize().ok() != dst_root.canonicalize().ok() {
                    let _ = std::fs::copy(&src, &dst_root);
                }
                if src.canonicalize().ok() != dst_shared.canonicalize().ok() {
                    let _ = std::fs::copy(&src, &dst_shared);
                }
            }
        }
    }

    let sys_prompt = { CTF_WORKER_SYSTEM_PROMPT };
    let contract_body = format!("{ROLE_CONTRACT_MARKER}\n{sys_prompt}\n");
    let contract_file = if norm_engine == "claude" {
        "CLAUDE.md"
    } else {
        "AGENTS.md"
    };
    // Pi/OMP receive the role contract through their system-prompt channel.
    if !matches!(norm_engine, "pi" | "omp") {
        std::fs::write(session_dir.join(contract_file), &contract_body)?;
        if contract_file != "AGENTS.md" {
            std::fs::write(session_dir.join("AGENTS.md"), &contract_body)?;
        }
    }

    let canonical_session = std::fs::canonicalize(session_dir)?;
    let canonical_script = std::fs::canonicalize(&blackboard_py_path).unwrap_or(blackboard_py_path);
    let canonical_ingress = std::fs::canonicalize(&ingress_dir).unwrap_or(ingress_dir);
    Ok((canonical_session, canonical_script, canonical_ingress))
}

pub fn build_worker_prompt_for_intent(
    challenge: &Value,
    blackboard_script: &Path,
    assignment: Option<&IntentAssignment>,
    board_snapshot: Option<&str>,
    reopen_hint: Option<&str>,
) -> String {
    if assignment.is_some_and(|a| role_for_worker_class(&a.worker_class) == "review") {
        return build_review_prompt(
            challenge,
            blackboard_script,
            assignment,
            board_snapshot,
            reopen_hint,
        );
    }
    if let Some(assigned) =
        assignment.filter(|a| role_for_worker_class(&a.worker_class) == "verifier")
    {
        let mut header = format!(
            "Challenge: {} [{}]",
            challenge["name"].as_str().unwrap_or(""),
            challenge["category"].as_str().unwrap_or("")
        );
        if let Some(target) = challenge["target"].as_str().filter(|s| !s.is_empty()) {
            header.push_str(&format!("\nTarget: {target}"));
        }
        let files: Vec<_> = challenge["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().or_else(|| v["path"].as_str()))
            .filter_map(|p| Path::new(p).file_name())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if !files.is_empty() {
            header.push_str(&format!(
                "\nAttached files (already in your working directory — inspect them FIRST): {}",
                files.join(", ")
            ));
        }
        let instruction = challenge["task_contract"]["raw_instruction"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| challenge["description"].as_str())
            .unwrap_or("");
        let mut sections=vec![header,format!("\n## Goal\n{}",instruction.trim()),board_snapshot.unwrap_or("").to_owned(),"\n## Workspace contract\nYour current directory is this Worker's private cwd for independent work; it is not a permission boundary between Workers in the same Run. Put reusable scripts, outputs, and connection instructions in `./shared/` for explicit handoff, and preserve files other Workers already placed under `./shared/`. Attached inputs remain available in the current directory.\n".to_owned()];
        if let Some(hint) = reopen_hint.filter(|s| !s.trim().is_empty()) {
            sections.push(ctf_prompts::standing(hint));
        }
        let rejected = ctf_prompts::rejected(
            &challenge["rejected_flags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>(),
        );
        if !rejected.is_empty() {
            sections.push(rejected);
        }
        return ctf_prompts::VERIFY
            .replace("{ctx}", &sections.join("\n"))
            .replace("{kb}", "")
            .replace("{intent_goal}", &assigned.goal);
    }
    if assignment.is_some_and(|a| role_for_worker_class(&a.worker_class) == "explore") {
        let assignment = assignment.unwrap();
        let mut sections = vec![board_snapshot.unwrap_or("").to_owned()];
        let rejected = ctf_prompts::rejected(
            &challenge["rejected_flags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>(),
        );
        if !rejected.is_empty() {
            sections.push(rejected);
        }

        if let Some(hint) = reopen_hint.filter(|hint| !hint.trim().is_empty()) {
            sections.push(ctf_prompts::standing(hint));
        }
        sections.push(format!(
            "【你负责的 step】{}\n{}",
            assignment.intent_id, assignment.goal
        ));
        return sections
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
            .trim()
            .to_owned();
    }
    let name = challenge["name"].as_str().unwrap_or("CTF Challenge");
    let category = challenge["category"].as_str().unwrap_or("misc");
    let description = challenge["description"].as_str().unwrap_or("");
    let flag_format = challenge["flag_format"].as_str().unwrap_or("flag{...}");
    let target = challenge["target"].as_str().unwrap_or("");
    let mode = "ctf";
    let attachments: Vec<String> = challenge["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            v.as_str()
                .or_else(|| v["path"].as_str())
                .and_then(|p| Path::new(p).file_name())
                .map(|f| f.to_string_lossy().into_owned())
        })
        .collect();

    let mut prompt = format!(
        "# Challenge: {name}\n\
         - Mode: {mode}\n\
         - Category: {category}\n\
         - Expected Flag Format: `{flag_format}`\n"
    );
    if !target.is_empty() {
        prompt.push_str(&format!("- Target: `{target}`\n"));
    }
    if !attachments.is_empty() {
        prompt.push_str(&format!(
            "- Workspace Files (in `./` and `./shared/`): {}\n",
            attachments.join(", ")
        ));
    }
    if !description.trim().is_empty() {
        prompt.push_str(&format!("\n## Description\n{description}\n"));
    }
    if let Some(assign) = assignment {
        prompt.push_str(&format!(
            "\n## Assigned Step Contract (`{}`)\n\
             - Goal: {}\n\
             - Expected Observable: {}\n\
             - Stop Condition: {}\n\
             - Coverage Key: `{}`\n",
            assign.intent_id,
            assign.goal,
            assign.expected_observable,
            assign.stop_condition,
            assign.coverage_key,
        ));
    }
    if let Some(board) = board_snapshot.filter(|s| !s.trim().is_empty()) {
        prompt.push_str(&format!(
            "\n## Shared Graph / Blackboard Snapshot\n{board}\n"
        ));
    }
    if let Some(hint) = reopen_hint.filter(|s| !s.trim().is_empty()) {
        prompt.push_str(&format!("\n## Operator Guidance / Follow-up\n{hint}\n"));
    }
    let script = blackboard_script.display();

    let role = assignment
        .map(|assign| role_for_worker_class(&assign.worker_class))
        .unwrap_or("explore");
    if role == "review" {
        prompt.push_str(&format!(
            "\n## Review instructions\nRead current facts with `python3 \"{script}\" context`. \
             Propose one evidence-based review with `review-finding`, `challenge-fact`, \
             `merge-fact`, `reject-fact`, or `revalidate-fact`. Do not submit a flag.\n"
        ));
    } else if role == "verifier" {
        prompt.push_str(&format!(
            "\n## Verification instructions\nIndependently test the assigned claim. \
             Read original evidence with `python3 \"{script}\" read-artifact '<art-id>'`, \
             then record a supported Fact or a bounded dead end and commit the Step. \
             Do not submit a flag.\n"
        ));
    } else {
        prompt.push_str(&format!(
            "\n## Instructions\n\
             1. Inspect the challenge files in `./` (or target) and execute the assigned step.\n\
             2. Always record your verified mechanism/finding as a Fact on the shared board before finishing:\n\
                `python3 \"{script}\" submit-fact '<title>' '<fact>'`\n\
             3. As soon as you recover and verify the flag, submit it via:\n\
                `python3 \"{script}\" submit-flag '<flag>'`\n\
             4. Once `submit-flag` prints `SUBMITTED`, summarize your solution briefly and finish.\n"
        ));
    }
    prompt
}

pub fn drain_blackboard_ingress(
    state: &AppState,
    run_id: &str,
    solver_id: &str,
    ingress_dir: &Path,
    session_dir: &Path,
) -> (bool, bool) {
    let Ok(entries) = std::fs::read_dir(ingress_dir) else {
        return (false, false);
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("request-") && n.ends_with(".json"))
        })
        .collect();
    files.sort();

    let mut run_solved = false;
    let mut finish_worker = false;

    for req_path in files {
        let Ok(raw) = std::fs::read_to_string(&req_path) else {
            continue;
        };
        let Ok(req) = serde_json::from_str::<Value>(&raw) else {
            let _ = std::fs::remove_file(&req_path);
            continue;
        };
        if req["protocol"].as_str() != Some("muteki-blackboard-v2") {
            let _ = std::fs::remove_file(&req_path);
            continue;
        }
        let req_id = req["request_id"].as_str().unwrap_or("").to_owned();
        if req_id.is_empty() {
            let _ = std::fs::remove_file(&req_path);
            continue;
        }
        let op = req["operation"].as_str().unwrap_or("");
        let payload = req.get("payload").filter(|v| v.is_object()).unwrap_or(&req);
        let res_path = ingress_dir.join(format!("result-{req_id}.json"));

        let active_generation = state
            .store
            .get("runs", run_id)
            .ok()
            .flatten()
            .and_then(|run| run["execution_generation"].as_u64())
            .map(|generation| generation.max(1));
        let requested_generation = req["execution_generation"].as_u64().unwrap_or(1);
        if (active_generation.is_some() && active_generation != Some(requested_generation))
            || req["worker_id"]
                .as_str()
                .is_some_and(|worker| worker != solver_id)
        {
            let _ = write_atomic_json(
                &res_path,
                &json!({
                    "request_id":req_id,"ok":false,"code":"run.execution_generation_stale",
                    "error":"Worker request belongs to another execution generation"
                }),
            );
            let _ = std::fs::remove_file(&req_path);
            continue;
        }

        let (assigned_role, campaign_mode, active_intent_id) =
            resolve_server_worker_context(state, run_id, solver_id, session_dir, &req);
        let run = state
            .store
            .get("runs", run_id)
            .ok()
            .flatten()
            .unwrap_or(Value::Null);
        if !blackboard_allowed(
            &run,
            &assigned_role,
            campaign_mode,
            op,
            state.runs.has_target_restarter(run_id),
        ) {
            let _ = write_atomic_json(
                &res_path,
                &json!({
                    "request_id": req_id,
                    "ok": false,
                    "code": "blackboard.role_forbidden",
                    "role": assigned_role,
                    "operation": op,
                    "error": format!("Operation '{op}' is outside '{assigned_role}' role authority"),
                }),
            );
            let _ = std::fs::remove_file(&req_path);
            continue;
        }

        let response = match op {
            "platform_verdict" => {
                let flag = payload["flag"].as_str().unwrap_or("");
                let verdicts = run::graph::SharedGraph::open(state, run_id)
                    .ok()
                    .map(|graph| graph.platform_verdicts())
                    .unwrap_or_default();
                let verdict = if flag.is_empty() {
                    verdicts.iter().rev().next().cloned()
                } else {
                    verdicts
                        .iter()
                        .rev()
                        .find(|row| row["flag"] == flag)
                        .cloned()
                };
                json!({
                    "request_id": req_id,
                    "ok": true,
                    "status": verdict
                        .as_ref()
                        .and_then(|row| row["status"].as_str())
                        .unwrap_or("pending"),
                    "detail": verdict
                        .as_ref()
                        .map(|row| row["detail"].clone())
                        .unwrap_or(Value::Null),
                    "verdict": verdict.unwrap_or(Value::Null),
                    "verdicts": verdicts,
                })
            }
            "submit_report" => match run::worker_result::stage(
                state,
                run_id,
                solver_id,
                active_intent_id.as_deref(),
                &req_id,
                "vulnerability_report",
                &payload["report"],
            ) {
                Ok(()) => {
                    json!({"request_id":req_id,"ok":true,"message":"REPORT_DRAFTED; commit-step will atomically submit it with the cited Fact"})
                }
                Err(error) => json!({"request_id":req_id,"ok":false,"error":error}),
            },
            "submit_flag" => {
                let flag = payload["flag"].as_str().unwrap_or("").trim().to_owned();
                if flag.is_empty() {
                    json!({"request_id": req_id, "ok": false, "error": "flag must not be empty"})
                } else {
                    match record_flag(state, run_id, solver_id, &flag) {
                        Ok(solved_now) => {
                            if solved_now {
                                run_solved = true;
                                finish_worker = true;
                            }
                            json!({"request_id": req_id, "ok": true, "message": "SUBMITTED"})
                        }
                        Err(reason) => {
                            json!({"request_id": req_id, "ok": false, "error": reason})
                        }
                    }
                }
            }
            "submit_fact" | "fact" => {
                let title = payload["title"].as_str().unwrap_or("").trim();
                let content = payload["content"]
                    .as_str()
                    .or_else(|| payload["fact"].as_str())
                    .or_else(|| payload["text"].as_str())
                    .unwrap_or("")
                    .trim();
                let artifact_id = payload["evidence_artifact_id"]
                    .as_str()
                    .unwrap_or("")
                    .trim();
                if title.is_empty() || content.is_empty() {
                    json!({"request_id":req_id,"ok":false,"error":"Fact title and content are required"})
                } else {
                    let observation = json!({"title":title,"text":content,"claim_verified":true,"enables":payload["enables"],"supersedes":payload["supersedes"],"provenance":{"artifact_id":artifact_id},"subject":payload["subject"],"predicate":payload["predicate"],"object":payload["object_value"],"scope":payload["scope"],"canonical_key":payload["canonical_key"]});
                    match run::worker_result::stage(
                        state,
                        run_id,
                        solver_id,
                        active_intent_id.as_deref(),
                        &req_id,
                        "observations",
                        &observation,
                    ) {
                        Ok(()) => {
                            json!({"request_id":req_id,"ok":true,"message":"已记入草稿。核对无误即调 commit-step 定稿收束。"})
                        }
                        Err(error) => json!({"request_id":req_id,"ok":false,"error":error}),
                    }
                }
            }
            "commit_step" => {
                let keep_going = payload["keep_going"].as_bool().unwrap_or(false);
                let summary = payload["summary"]
                    .as_str()
                    .or_else(|| payload["reason"].as_str())
                    .or_else(|| payload["note"].as_str())
                    .unwrap_or("");
                let allow_campaign_finish = payload["campaign_conclude"].as_bool().unwrap_or(false)
                    || payload["reason"].as_str() == Some("campaign_boundary");
                let intent_hint = payload["intent_id"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .or(active_intent_id.as_deref());
                let finish = if campaign_mode {
                    run_solved || allow_campaign_finish
                } else {
                    true
                };
                let checkpoint = if finish { "" } else { req_id.as_str() };
                let result = if let Some(intent) = intent_hint {
                    let mut result = payload.clone();
                    result["summary"] = json!(summary);
                    result["status"] = json!(if run_solved { "solved" } else { "done" });
                    run::worker_result::commit(
                        state, run_id, solver_id, intent, checkpoint, &result, finish, run_solved,
                    )
                } else {
                    Err("WorkerResult requires its claimed Step identity".into())
                };
                match result {
                    Ok(receipt) => {
                        let should_finish =
                            receipt["finish_worker"].as_bool().unwrap_or(!keep_going);
                        if should_finish {
                            finish_worker = true;
                        }
                        emit_event(
                            state,
                            run_id,
                            Some(solver_id),
                            "blackboard.delta",
                            json!({
                                "kind": "worker_result_committed",
                                "worker": solver_id,
                                "intent_id": receipt["intent_id"],
                                "campaign_mode": campaign_mode,
                                "checkpoint": receipt["checkpoint"],
                                "finish_worker": should_finish,
                            }),
                        );
                        json!({
                            "request_id": req_id,
                            "ok": true,
                            "message": if receipt["checkpoint"] == true { "STEP_CHECKPOINT_COMMITTED" } else { "STEP_COMMITTED" },
                            "checkpoint": receipt["checkpoint"],
                            "fact_seq": receipt["fact_seqs"].as_array().and_then(|rows|rows.last()),
                            "superseded_fact_seqs":receipt["superseded_fact_seqs"],
                            "finish_worker": should_finish,
                            "intent_id": receipt["intent_id"],
                        })
                    }
                    Err(error) => json!({
                        "request_id": req_id,
                        "ok": false,
                        "code": "step.commit_rejected",
                        "error": error,
                    }),
                }
            }
            "dead_end" | "mark_deadend" => {
                let raw_reason = payload["reason"].as_str().unwrap_or("").trim();
                let clean = regex::Regex::new(r"(?i)^(?:\[[a-z0-9_.-]+\]\s*)+")
                    .unwrap()
                    .replace(raw_reason, "");
                let reason = if clean.trim().is_empty() {
                    raw_reason
                } else {
                    clean.trim()
                };
                let tested_scope = payload["tested_scope"].as_str().unwrap_or("").trim();
                let observed_result = payload["observed_result"].as_str().unwrap_or("").trim();
                if reason.is_empty() {
                    json!({"request_id": req_id, "ok": false, "error": "dead_end reason is empty"})
                } else {
                    let goal = run::graph::SharedGraph::open(state, run_id)
                        .ok()
                        .and_then(|graph| {
                            graph.all_intents().into_iter().find(|row| {
                                row["intent_id"].as_str() == active_intent_id.as_deref()
                            })
                        })
                        .and_then(|row| row["goal"].as_str().map(str::to_owned));
                    let dead = json!({"reason":reason,"tested_scope":if tested_scope.is_empty(){goal.as_deref().filter(|s|!s.is_empty()).or(active_intent_id.as_deref()).unwrap_or("current_step")}else{tested_scope},"observed_result":if observed_result.is_empty(){reason}else{observed_result}});
                    match run::worker_result::stage(
                        state,
                        run_id,
                        solver_id,
                        active_intent_id.as_deref(),
                        &req_id,
                        "dead_ends",
                        &dead,
                    ) {
                        Ok(()) => {
                            json!({"request_id":req_id,"ok":true,"message":"STAGED; call commit-step to conclude this Step with the recorded DeadEnd"})
                        }
                        Err(error) => json!({"request_id":req_id,"ok":false,"error":error}),
                    }
                }
            }
            "need_input" => {
                let need = payload["need"].as_str().unwrap_or("").trim();
                if need.is_empty() {
                    json!({"request_id":req_id,"ok":false,"error":"operator request is empty"})
                } else {
                    record_hitl_request(state, run_id, solver_id, need);
                    match run::worker_result::stage(
                        state,
                        run_id,
                        solver_id,
                        active_intent_id.as_deref(),
                        &req_id,
                        "need_input",
                        &json!({"need":need,"need_kind":"external_blocker"}),
                    ) {
                        Ok(()) => {
                            json!({"request_id":req_id,"ok":true,"message":"OPERATOR_REQUESTED"})
                        }
                        Err(error) => json!({"request_id":req_id,"ok":false,"error":error}),
                    }
                }
            }
            "save_poc" => {
                let saved = (|| -> Result<Value, String> {
                    let intent = active_intent_id
                        .as_deref()
                        .ok_or("PoC publication requires the current Step")?;
                    if let Some((kind, old)) = run::worker_result::staged_request(
                        state,
                        run_id,
                        solver_id,
                        Some(intent),
                        &req_id,
                    )? {
                        if kind != "pocs"
                            || old["source_path"] != payload["path"]
                            || old["entry_command"].as_str().unwrap_or("")
                                != payload["entry_command"].as_str().unwrap_or("")
                            || old["note"].as_str().unwrap_or("")
                                != payload["note"].as_str().unwrap_or("")
                        {
                            return Err(
                                "Worker request identity was reused with different PoC input"
                                    .into(),
                            );
                        }
                        run::worker_poc::verify(session_dir, &old)?;
                        return Ok(old);
                    }
                    let poc = run::worker_poc::save(
                        session_dir,
                        session_dir,
                        solver_id,
                        intent,
                        &payload,
                    )?;
                    run::worker_result::stage(
                        state,
                        run_id,
                        solver_id,
                        Some(intent),
                        &req_id,
                        "pocs",
                        &poc,
                    )?;
                    Ok(poc)
                })();
                match saved {
                    Ok(poc) => {
                        json!({"request_id":req_id,"ok":true,"message":format!("SAVED {}",poc["poc_id"].as_str().unwrap()),"poc_id":poc["poc_id"]})
                    }
                    Err(error) => json!({"request_id":req_id,"ok":false,"error":error}),
                }
            }
            "board_query" => {
                let result = (|| -> Result<Value,String> {
                    let run=state.store.get("runs",run_id).map_err(|e|e.to_string())?.ok_or("Run missing")?;
                    let graph=run::graph::SharedGraph::open(state,run_id)?;
                    let snapshot=run::ctf_planner::graph_views(&graph,&run,&run,0)? .1;
                    run::board_context::query_run(state,run_id,&snapshot,payload).and_then(|value|run::board_context::charge(state,run_id,solver_id,value))
                })();
                match result {
                    Ok(value)=>json!({"request_id":req_id,"ok":true,"content":value.to_string()}),
                    Err(error)=>json!({"request_id":req_id,"ok":false,"code":"context_read_failed","error":error}),
                }
            }
            "context" => {
                run::board_context::new_worker_round(state,run_id,solver_id);
                let run = state.store.get("runs", run_id);
                let context = match run {
                    Ok(Some(_)) => {
                        if assigned_role == "review" {
                            run::graph::SharedGraph::open(state, run_id).and_then(|graph| {
                                let intent = active_intent_id
                                    .as_deref()
                                    .ok_or("Review Step identity missing")?;
                                let row = graph
                                    .all_intents()
                                    .into_iter()
                                    .find(|row| row["intent_id"] == intent)
                                    .ok_or("Review Step missing")?;
                                graph.review_projection(intent, row["goal"].as_str().unwrap_or(""))
                            })
                        } else {
                            run::ctf_planner::worker_context(state, run_id, active_intent_id.as_deref().unwrap_or(""))
                        }
                    }
                    Ok(None) => Err("Run does not exist".to_owned()),
                    Err(error) => Err(error.to_string()),
                };
                match context {
                    Ok(content) => {
                        json!({"request_id":req_id,"ok":true,"message":content,"content":content})
                    }
                    Err(error) => {
                        json!({"request_id":req_id,"ok":false,"code":"shared_context_unready","error":error})
                    }
                }
            }
            "recent_evidence" => match run::evidence::recent(
                state,
                run_id,
                solver_id,
                active_intent_id.as_deref(),
                payload["limit"].as_u64().unwrap_or(10) as usize,
            ) {
                Ok(records) => {
                    json!({"request_id":req_id,"ok":true,"content":records.to_string(),"message":records.to_string()})
                }
                Err(error) => {
                    json!({"request_id":req_id,"ok":false,"code":"evidence.integrity_failed","detail":error})
                }
            },
            "read_artifact" => match run::evidence::read(
                state,
                run_id,
                solver_id,
                active_intent_id.as_deref(),
                payload["artifact_id"].as_str().unwrap_or(""),
                payload["include_command"] == true,
            ).and_then(|value|run::board_context::bounded_evidence(state,run_id,solver_id,payload["artifact_id"].as_str().unwrap_or(""),value)) {
                Ok(mut result) => {
                    result["request_id"] = json!(req_id);
                    result
                }
                Err(error) => {
                    json!({"request_id":req_id,"ok":false,"code":"evidence.unavailable","detail":error})
                }
            },
            "restart_target" => match run::worker_restart::dispatch(
                state, run_id, solver_id, &req, &req_path, &res_path,
            ) {
                Ok(()) => continue,
                Err(error) => {
                    json!({"request_id":req_id,"ok":false,"code":"restart_evidence_invalid","detail":error})
                }
            },
            "claim_intent" => {
                let intent_id = payload["intent_id"].as_str().unwrap_or("step_001").trim();
                let won = crate::runs::graph::SharedGraph::open(state, run_id)
                    .map(|g| g.claim_intent(solver_id, intent_id, 600.0))
                    .unwrap_or(true);
                json!({
                    "request_id": req_id,
                    "ok": true,
                    "won": won,
                    "message": if won { "WON" } else { "HELD" },
                })
            }
            "claim_activity" => {
                let key = payload["activity_key"]
                    .as_str()
                    .or_else(|| payload["key"].as_str())
                    .unwrap_or("default")
                    .trim();
                let intent_id = payload["intent_id"].as_str().unwrap_or("step_001").trim();
                let won = crate::runs::graph::SharedGraph::open(state, run_id)
                    .map(|g| g.try_claim_activity(key, solver_id, intent_id, 600.0))
                    .unwrap_or(true);
                json!({
                    "request_id": req_id,
                    "ok": true,
                    "won": won,
                    "message": if won { "WON" } else { "HELD" },
                })
            }
            "claim_resource" | "submission_lock" => {
                let key = payload["resource_key"]
                    .as_str()
                    .or_else(|| payload["key"].as_str())
                    .unwrap_or(if op == "submission_lock" {
                        "flag_submit"
                    } else {
                        "resource"
                    })
                    .trim();
                let intent_id = payload["intent_id"].as_str().unwrap_or("step_001").trim();
                let won = crate::runs::graph::SharedGraph::open(state, run_id)
                    .map(|g| g.request_resource_lock(key, solver_id, intent_id, 300.0))
                    .unwrap_or(true);
                json!({
                    "request_id": req_id,
                    "ok": true,
                    "won": won,
                    "message": if won { "WON" } else { "HELD" },
                })
            }
            "release_resource" => {
                let key = payload["resource_key"]
                    .as_str()
                    .or_else(|| payload["key"].as_str())
                    .unwrap_or("resource")
                    .trim();
                if let Ok(g) = crate::runs::graph::SharedGraph::open(state, run_id) {
                    g.release_resource_lock(key, solver_id, "worker_release");
                }
                json!({
                    "request_id": req_id,
                    "ok": true,
                    "won": true,
                    "message": "RELEASED",
                })
            }
            "propose_intent" => {
                let goal = payload["goal"].as_str().unwrap_or("").trim();
                let iid = payload["intent_id"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("step_{:03}", now_secs() as u64 % 1000));
                let seq = crate::runs::graph::SharedGraph::open(state, run_id)
                    .map(|g| g.propose_intent(solver_id, &iid, goal, payload, None))
                    .unwrap_or(-1);
                json!({
                    "request_id": req_id,
                    "ok": seq > 0,
                    "intent_id": iid,
                    "message": if seq > 0 { "PROPOSED" } else { "ERR" },
                })
            }
            "review_finding" | "no_action" | "fact_challenge" | "challenge_fact"
            | "fact_revalidation" | "revalidate_fact" | "fact_reject" | "reject_fact"
            | "fact_merge" | "merge_facts" | "merge_fact" => {
                let marker = run::graph::normalize_review_marker(op).unwrap_or("REVIEW_FINDING");
                let mut proposal_payload = payload.clone();
                if op == "no_action" && proposal_payload.get("kind").is_none() {
                    proposal_payload["kind"] = json!("no_action");
                }
                match run::graph::SharedGraph::open(state, run_id)
                    .and_then(|graph| graph.add_review_proposal(solver_id, op, &proposal_payload))
                {
                    Ok(seq) => {
                        emit_event(
                            state,
                            run_id,
                            Some(solver_id),
                            "blackboard.delta",
                            json!({
                                "kind": "review_proposal",
                                "proposal_seq": seq,
                                "marker": marker,
                                "worker": solver_id
                            }),
                        );
                        json!({
                            "request_id": req_id,
                            "ok": true,
                            "message": "PROPOSED",
                            "proposal_seq": seq
                        })
                    }
                    Err(error) => json!({"request_id": req_id, "ok": false, "error": error}),
                }
            }
            "mcp_tools" | "mcp_schema" | "mcp_call" => {
                match run::worker_mcp::dispatch(
                    state, run_id, solver_id, &req, &req_path, &res_path,
                ) {
                    Ok(()) => continue,
                    Err(error) => {
                        json!({"request_id":req_id,"ok":false,"code":"worker_mcp.unavailable","detail":error})
                    }
                }
            }
            _ => {
                json!({
                    "request_id": req_id,
                    "ok": true,
                    "message": "OK",
                })
            }
        };

        let _ = write_atomic_json(&res_path, &response);
        let _ = std::fs::remove_file(&req_path);
    }

    (run_solved, finish_worker)
}

pub fn spawn_local_worker(
    state: Arc<AppState>,
    run_id: String,
    spec: WorkerSpec,
    challenge: Value,
    reopen_hint: Option<String>,
    step_index: usize,
) -> tokio::task::AbortHandle {
    let name = challenge["name"].as_str().unwrap_or("challenge");
    let generation = state
        .store
        .get("runs", &run_id)
        .ok()
        .flatten()
        .and_then(|run| run["execution_generation"].as_u64())
        .unwrap_or(1);
    let intent_id = if generation <= 1 {
        format!("step_{step_index:03}")
    } else {
        format!("step_{step_index:03}_g{generation}")
    };
    let assignment = IntentAssignment {
        intent_id,
        goal: { format!("Analyze {name} and recover the flag") },
        expected_observable: { "Verified flag or concrete attack surface finding" }.to_owned(),
        stop_condition: { "Flag is submitted or initial surface inspection concludes" }.to_owned(),
        coverage_key: "initial_recon".to_owned(),
        reopen_hint,
        worker_class: "solve".to_owned(),
    };
    if let Ok(graph) = run::graph::SharedGraph::open(&state, &run_id) {
        let prior = graph
            .all_intents()
            .into_iter()
            .find(|intent| intent["intent_id"] == assignment.intent_id);
        if prior.is_none() {
            graph.propose_intent(
                "coordinator",
                &assignment.intent_id,
                &assignment.goal,
                &json!({"worker_class":"solve",
                    "expected_observable":assignment.expected_observable,
                    "stop_condition":assignment.stop_condition,
                    "coverage_key":assignment.coverage_key}),
                None,
            );
        }
        if prior.as_ref().is_none_or(|intent| {
            intent["status"] != "claimed" || intent["worker"] != spec.solver_id
        }) {
            graph.claim_intent(&spec.solver_id, &assignment.intent_id, 600.0);
        }
    }
    spawn_local_worker_with_intent(state, run_id, spec, challenge, assignment, true).abort
}

pub(super) async fn execute_worker_task(
    state: Arc<AppState>,
    run_id: String,
    spec: WorkerSpec,
    thread_id: String,
    challenge: Value,
    assignment: IntentAssignment,
    finalize_on_exit: bool,
    mut cancel_rx: Option<oneshot::Receiver<oneshot::Sender<bool>>>,
    host_cancel: Arc<AtomicBool>,
    exit_reason_slot: Arc<Mutex<Option<String>>>,
) {
    let solver_id = spec.solver_id.clone();
    let Some(_resource_reservation) = super::acquire_node_reservation(
        &state,
        &run_id,
        &solver_id,
        spec.backend == "container",
        false,
        &mut cancel_rx,
        &host_cancel,
        &exit_reason_slot,
    )
    .await
    else {
        return;
    };
    let sdir = session_dir(&state, &run_id);
    if spec.backend == "container"
        && run::launch_config::settings(&state, &run_id)
            .ok()
            .is_some_and(|config| config["worker_container_scope"] == "shared")
    {
        if let Err(error) = run::container_exec::prepare_shared_workspace(&state, &run_id) {
            *exit_reason_slot.lock().unwrap() = Some("failed".into());
            emit_event(
                &state,
                &run_id,
                Some(&solver_id),
                "worker.error",
                json!({"reason":"shared_workspace_failed","detail":error}),
            );
            state.runs.unregister_worker(&run_id, &solver_id);
            if finalize_on_exit {
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "runtime_failure",
                    Some(&error),
                );
            }
            return;
        }
    }

    let mut budget = match run::execution_budget::WorkerLease::start(
        &state,
        &run_id,
        &solver_id,
        &assignment.intent_id,
        &spec.profile,
        &challenge,
    ) {
        Ok(budget) => budget,
        Err(error) => {
            *exit_reason_slot.lock().unwrap() = Some("failed".into());
            state.runs.unregister_worker(&run_id, &solver_id);
            emit_event(
                &state,
                &run_id,
                Some(&solver_id),
                "worker.error",
                json!({"reason":"execution_budget_invalid","detail":error}),
            );
            if finalize_on_exit {
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "runtime_failure",
                    Some(&error),
                );
            }
            return;
        }
    };
    let Ok((workdir, blackboard_script, ingress_dir)) =
        stage_workspace(&sdir, &solver_id, &spec.norm_engine, &challenge)
    else {
        *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
        if finalize_on_exit {
            mark_run_finished(
                &state,
                &run_id,
                Some(&solver_id),
                false,
                "runtime_failure",
                Some("无法创建本地 Worker 工作区目录"),
            );
        }
        state.runs.unregister_worker(&run_id, &solver_id);
        return;
    };
    if spec.backend != "container" && host_cancel.load(Ordering::SeqCst) {
        *exit_reason_slot.lock().unwrap() = Some("canceled".to_owned());
        state.runs.unregister_worker(&run_id, &solver_id);
        return;
    }

    let worker_role = role_for_worker_class(&assignment.worker_class);
    let blackboard_role = if worker_role == "explore" {
        "solve"
    } else {
        worker_role
    };
    let execution_mode =
        blackboard_execution_mode(&spec, &challenge, &assignment, finalize_on_exit);
    let restart_bound = state.runs.has_target_restarter(&run_id);
    let campaign_mode = execution_mode == "campaign";
    let mut blackboard_env: Value =
        std::fs::read(sdir.join(".muteki-skills/muteki-blackboard/env.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_else(|| json!({}));
    blackboard_env["MUTEKI_BLACKBOARD_ROLE"] = json!(blackboard_role);
    blackboard_env["MUTEKI_BLACKBOARD_PROFILE"] = json!(blackboard_role);
    blackboard_env["MUTEKI_WORKER_EXECUTION_MODE"] = json!(execution_mode);
    blackboard_env["MUTEKI_PLATFORM_CONFIRMATION_REQUIRED"] =
        json!(if challenge["platform_confirmation_required"] == true {
            "1"
        } else {
            "0"
        });
    if restart_bound {
        blackboard_env["MUTEKI_TARGET_RESTART_AVAILABLE"] = json!("1");
    } else if let Some(map) = blackboard_env.as_object_mut() {
        map.remove("MUTEKI_TARGET_RESTART_AVAILABLE");
    }
    let _ = std::fs::write(
        sdir.join(".muteki-skills/muteki-blackboard/env.json"),
        blackboard_env.to_string(),
    );
    let _ = std::fs::write(
        sdir.join(".muteki-skills/muteki-blackboard/SKILL.md"),
        super::render_blackboard_skill("ctf", restart_bound, campaign_mode),
    );

    let intent_id = assignment.intent_id.as_str();
    let goal = assignment.goal.as_str();
    let model_str = spec.model.clone().unwrap_or_else(|| "default".to_owned());
    let generation = worker_execution_generation(&solver_id);
    write_worker_assignment_record(
        &sdir,
        &solver_id,
        worker_role,
        execution_mode,
        intent_id,
        generation,
    );
    let previous = run::read_jsonl_events(&state, &run_id);
    let anchor = previous.iter().rposition(|event| {
        matches!(
            event["event_type"].as_str(),
            Some("run.started" | "run.reopened")
        ) && event["payload"]["execution_generation"] == generation
    });
    let prior = &previous[anchor.map_or(0, |index| index + 1)..];

    if finalize_on_exit {
        emit_start_event_once(
            &state,
            &run_id,
            None,
            prior,
            "blackboard.delta",
            json!({
                "kind": "intent_proposed",
                "intent_id": intent_id,
                "goal": goal,
                "worker_class": "solve",
            }),
        );
    }
    emit_start_event_once(
        &state,
        &run_id,
        Some(&solver_id),
        prior,
        "blackboard.delta",
        json!({
            "kind": "intent_claimed",
            "intent_id": intent_id,
            "worker": solver_id,
        }),
    );
    emit_start_event_once(
        &state,
        &run_id,
        Some(&solver_id),
        prior,
        "blackboard.delta",
        json!({
            "kind": "worker_spawned",
            "actor": "coordinator",
            "worker": solver_id,
            "phase": worker_role,
            "engine": spec.raw_engine,
            "model": model_str,
            "intent_id": intent_id,
        }),
    );
    emit_start_event_once(
        &state,
        &run_id,
        Some(&solver_id),
        prior,
        "worker.lifecycle",
        json!({
            "phase": "spawned",
            "phase_label": if worker_role == "review" { "Review" }
                else if worker_role == "verifier" { "Verify" } else { "Explore & Solve" },
            "intent_id": intent_id,
            "worker_role": worker_role,
            "engine": spec.raw_engine,
            "model": model_str,
        }),
    );
    let started_at = now_secs();
    let is_container = spec.backend == "container";
    let prompt_script = if is_container {
        PathBuf::from(format!(
            "{}/.muteki-skills/muteki-blackboard/blackboard.py",
            run::container_exec::workspace_path(&state, &run_id)
        ))
    } else {
        blackboard_script.clone()
    };
    let runtime_cwd = if is_container {
        run::container_exec::workspace_path(&state, &run_id)
    } else {
        workdir.to_string_lossy().into_owned()
    };
    emit_start_event_once(
        &state,
        &run_id,
        Some(&solver_id),
        prior,
        "worker.status",
        json!({
            "online": true,
            "status": "online",
            "reason": "running",
            "engine": spec.raw_engine,
            "worker_role": "worker",
            "phase": "explore",
            "intent_id": intent_id,
            "profile_id": spec.profile_id,
            "profile_label": spec.profile_label,
            "model": model_str,
            "account_id": spec.account_id,
            "runtime": {
                "backend": spec.backend,
                "status": "running",
                "cwd": runtime_cwd,
                "started_at": started_at,
            },
        }),
    );

    let prompt = {
        let mut board_snapshot = {
            match if worker_role == "review" {
                run::graph::SharedGraph::open(&state, &run_id)
                    .and_then(|graph| graph.review_projection(intent_id, &assignment.goal))
            } else {
                run::ctf_planner::worker_context(&state, &run_id, intent_id)
            } {
                Ok(context) => context,
                Err(error) => {
                    *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
                    emit_event(
                        &state,
                        &run_id,
                        Some(&solver_id),
                        "worker.error",
                        json!({"reason":"shared_context_unready","detail":error}),
                    );
                    state.runs.unregister_worker(&run_id, &solver_id);
                    return;
                }
            }
        };
        let context = match run::control_context::bind_for_worker(
            &state,
            &run_id,
            &solver_id,
            &spec.raw_engine,
            intent_id,
        ) {
            Ok(value) => value,
            Err(error) => {
                *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
                emit_event(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    "worker.error",
                    json!({"reason":"runtime_failure","detail":error}),
                );
                state.runs.unregister_worker(&run_id, &solver_id);
                if finalize_on_exit {
                    mark_run_finished(
                        &state,
                        &run_id,
                        Some(&solver_id),
                        false,
                        "runtime_failure",
                        Some(&error),
                    );
                }
                return;
            }
        };
        board_snapshot.push_str(&context);
        let mut prompt_challenge = challenge.clone();
        prompt_challenge["rejected_flags"] = json!(
            state
                .runs
                .handle(&state, &run_id)
                .rejected_flags
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        );
        build_worker_prompt_for_intent(
            &prompt_challenge,
            &prompt_script,
            Some(&assignment),
            Some(&board_snapshot),
            assignment.reopen_hint.as_deref(),
        )
    };
    let (runtime_prompt, mut secret_values) =
        match run::control_context::materialize_for_worker(&state, &run_id, &solver_id, &prompt).and_then(|value| {
            if value.0.len()>run::board_context::WORKER_PROMPT_BYTES && worker_role!="review" {Err(format!("context.worker_prompt_budget_exceeded: {} bytes, limit {}",value.0.len(),run::board_context::WORKER_PROMPT_BYTES))}else{Ok(value)}
        }) {
            Ok(value) => value,
            Err(error) => {
                *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
                let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
                emit_event(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    "worker.error",
                    json!({"reason":"preflight_failed","detail":error}),
                );
                state.runs.unregister_worker(&run_id, &solver_id);
                if finalize_on_exit {
                    mark_run_finished(
                        &state,
                        &run_id,
                        Some(&solver_id),
                        false,
                        "preflight_failed",
                        Some(&error),
                    );
                }
                return;
            }
        };
    let prompt_id = format!("prompt-{}", uuid::Uuid::new_v4().simple());
    emit_start_event_once(
        &state,
        &run_id,
        Some(&solver_id),
        prior,
        "worker.prompt",
        json!({
            "prompt_id": prompt_id,
            "prompt": prompt,
            "kind": "execute",
            "status": "sent",
            "transport": "stdin",
            "engine": spec.raw_engine,
            "model": model_str,
            "intent_id": intent_id,
            "phase": "explore",
        }),
    );

    if is_container {
        execute_container_worker_task(
            state,
            run_id,
            spec,
            workdir,
            ingress_dir,
            sdir,
            challenge,
            assignment,
            runtime_prompt,
            secret_values,
            started_at,
            finalize_on_exit,
            cancel_rx,
            exit_reason_slot,
            budget,
            _resource_reservation.id().to_owned(),
        )
        .await;
        return;
    }

    let Some(engine_desc) = descriptors::find(&spec.norm_engine) else {
        *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
        let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
        state.runs.unregister_worker(&run_id, &solver_id);
        if finalize_on_exit {
            mark_run_finished(
                &state,
                &run_id,
                Some(&solver_id),
                false,
                "runtime_failure",
                Some("未知引擎描述符"),
            );
        }
        return;
    };

    let state_dir = native_chat::state_dir(&state);
    let resolved_cred = match credentials::resolve(
        &state.store,
        &state_dir,
        engine_desc,
        &spec.effective_credential_id,
        spec.model.as_deref(),
    )
    .await
    {
        Ok(c) => c,
        Err(err) => {
            *exit_reason_slot.lock().unwrap() = Some(
                classify_worker_exit_reason(false, false, false, false, Some(&err.message))
                    .to_owned(),
            );
            let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
            state.runs.unregister_worker(&run_id, &solver_id);
            if finalize_on_exit {
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "preflight_failed",
                    Some(&err.message),
                );
            }
            return;
        }
    };

    let instance_row = runtime_api::instance(&state.store, &spec.adapter_id, &spec.instance_id)
        .unwrap_or_else(|| json!({}));

    let mut env = resolved_cred.env;
    if let Some(obj) = instance_row["env"].as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                env.push((k.clone(), s.to_owned()));
            }
        }
    }
    match credentials::env_refs_with_platform(&state, &instance_row["env_refs"]) {
        Ok(refs) => env.extend(refs),
        Err(err) => {
            *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
            let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
            state.runs.unregister_worker(&run_id, &solver_id);
            if finalize_on_exit {
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "preflight_failed",
                    Some(&err.message),
                );
            }
            return;
        }
    }

    env.extend(runtime_api::transport_env(
        &spec.adapter_id,
        &instance_row["transport"],
        instance_row["adapter_endpoint"]
            .as_str()
            .or_else(|| instance_row["endpoint"].as_str())
            .unwrap_or(""),
    ));
    secret_values.extend(crate::network_proxy::active().secrets);
    let mode = "ctf";
    env.push((
        "MUTEKI_BLACKBOARD_SCRIPT".to_owned(),
        blackboard_script.to_string_lossy().into_owned(),
    ));
    env.push((
        "MUTEKI_BLACKBOARD_INGRESS_DIR".to_owned(),
        ingress_dir.to_string_lossy().into_owned(),
    ));
    env.push(("MUTEKI_WORKER_ID".to_owned(), solver_id.clone()));
    env.push((
        "MUTEKI_EXECUTION_GENERATION".to_owned(),
        worker_execution_generation(&solver_id).to_string(),
    ));
    env.push(("MUTEKI_CHALLENGE_MODE".to_owned(), mode.to_owned()));
    env.push((
        "MUTEKI_BLACKBOARD_ROLE".to_owned(),
        blackboard_role.to_owned(),
    ));
    env.push((
        "MUTEKI_BLACKBOARD_PROFILE".to_owned(),
        blackboard_role.to_owned(),
    ));
    env.push((
        "MUTEKI_WORKER_EXECUTION_MODE".to_owned(),
        execution_mode.to_owned(),
    ));
    if restart_bound {
        env.push(("MUTEKI_TARGET_RESTART_AVAILABLE".to_owned(), "1".to_owned()));
    }
    env.push((
        "MUTEKI_PLATFORM_CONFIRMATION_REQUIRED".to_owned(),
        if challenge["platform_confirmation_required"] == true {
            "1"
        } else {
            "0"
        }
        .to_owned(),
    ));
    let target_epoch = state
        .store
        .get("runs", &run_id)
        .ok()
        .flatten()
        .and_then(|run| {
            run["target_epoch"]
                .as_u64()
                .or_else(|| run["execution_generation"].as_u64())
        })
        .unwrap_or(1);
    env.push(("MUTEKI_TARGET_EPOCH".to_owned(), target_epoch.to_string()));
    {
        env.push((
            "MUTEKI_TMUX_SOCKET".to_owned(),
            sdir.join("shared/live/tmux.sock")
                .to_string_lossy()
                .into_owned(),
        ));
    }

    let mut extra_args: Vec<String> = instance_row["launch_args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    extra_args.extend(
        instance_row["extra_args"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned)),
    );

    extra_args.extend(runtime_api::transport_args(
        &spec.adapter_id,
        &instance_row["transport"],
    ));
    let binary_override = instance_row["binary_path"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let home = instance_row["home"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    let ctf_system = ctf_prompts::system(worker_role == "verifier");
    let sys_instructions = if worker_role == "review" {
        REVIEW_WORKER_SYSTEM_PROMPT
    } else {
        &ctf_system
    };

    let staged_plugins =
        super::stage_swarm_worker_plugins(&state, &workdir, &solver_id, &spec.norm_engine, mode)
            .unwrap_or_default();
    if spec.norm_engine == "pi" {
        extra_args.extend([
            "--no-skills".to_owned(),
            "--no-context-files".to_owned(),
            "--system-prompt".to_owned(),
            sys_instructions.to_owned(),
        ]);
        for skill in &staged_plugins.skills {
            extra_args.extend(["--skill".to_owned(), skill.to_string_lossy().into_owned()]);
        }
        for extension in &staged_plugins.extensions {
            extra_args.extend([
                "--extension".to_owned(),
                extension.to_string_lossy().into_owned(),
            ]);
        }
    }
    if spec.norm_engine == "omp" && worker_role != "review" {
        extra_args.extend(["--system-prompt".to_owned(), sys_instructions.to_owned()]);
    }
    if run::launch_config::settings(&state, &run_id)
        .is_ok_and(|settings| settings["offline"] == true)
    {
        match run::container_exec::local_web_tools_withholding_args(&spec.norm_engine) {
            Ok(withheld_args) => {
                env.push((
                    run::container_exec::WEB_TOOLS_ENV.to_owned(),
                    "disabled".to_owned(),
                ));
                extra_args.extend(withheld_args);
            }
            Err(err) => {
                *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
                let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
                emit_event(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    "worker.error",
                    json!({"reason":"preflight_failed","detail":err.message}),
                );
                state.runs.unregister_worker(&run_id, &solver_id);
                finish_worker_events(
                    &state,
                    &run_id,
                    &spec,
                    intent_id,
                    &workdir,
                    started_at,
                    false,
                    "preflight_failed",
                );
                if finalize_on_exit {
                    mark_run_finished(
                        &state,
                        &run_id,
                        Some(&solver_id),
                        false,
                        "preflight_failed",
                        Some(&err.message),
                    );
                }
                return;
            }
        }
    }
    let mut usage_observer = run::usage::Observer::new(&thread_id, worker_role);
    let start_req = StartRequest {
        thread_id: thread_id.clone(),
        adapter_id: spec.adapter_id.clone(),
        instance_id: spec.instance_id.clone(),
        model: spec.model.clone(),
        access_mode: access::FULL_ACCESS.to_owned(),
        credential_id: spec.effective_credential_id.clone(),
        cwd: workdir.clone(),
        binary_override,
        env,
        home,
        extra_args,
        resume: None,
        fork: false,
        instructions: (spec.norm_engine != "omp").then(|| sys_instructions.to_owned()),
        mcp_servers: Vec::new(),
    };

    #[cfg(test)]
    if let Some(gate) = HOST_START_GATE
        .get()
        .filter(|gate| gate.solver_id == solver_id)
    {
        gate.arrived.notify_one();
        gate.release.notified().await;
    }
    if host_cancel.load(Ordering::SeqCst) {
        *exit_reason_slot.lock().unwrap() = Some("canceled".to_owned());
        let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
        state.runs.unregister_worker(&run_id, &solver_id);
        finish_worker_events(
            &state,
            &run_id,
            &spec,
            intent_id,
            &workdir,
            started_at,
            false,
            "intent_dropped",
        );
        return;
    }
    if let Err(error) = record_external_start_request(&state, &run_id, &solver_id) {
        *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
        let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
        emit_event(
            &state,
            &run_id,
            Some(&solver_id),
            "worker.error",
            json!({"reason":"runtime_failure","detail":error}),
        );
        state.runs.unregister_worker(&run_id, &solver_id);
        finish_worker_events(
            &state,
            &run_id,
            &spec,
            intent_id,
            &workdir,
            started_at,
            false,
            "runtime_failure",
        );
        if finalize_on_exit {
            mark_run_finished(
                &state,
                &run_id,
                Some(&solver_id),
                false,
                "runtime_failure",
                Some(&error),
            );
        }
        return;
    }

    let (session, events) = match state.agents.start(start_req).await {
        Ok(pair) => pair,
        Err(err) => {
            let canceled = host_cancel.load(Ordering::SeqCst);
            let exit_class =
                classify_worker_exit_reason(false, canceled, false, false, Some(&err.message));
            *exit_reason_slot.lock().unwrap() = Some(exit_class.to_owned());
            let _ = run::control_context::unknown_for_worker(&state, &run_id, &solver_id);
            state.runs.unregister_worker(&run_id, &solver_id);
            finish_worker_events(
                &state,
                &run_id,
                &spec,
                intent_id,
                &workdir,
                started_at,
                false,
                if canceled {
                    "intent_dropped"
                } else {
                    "runtime_failure"
                },
            );
            if finalize_on_exit && !canceled {
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "runtime_failure",
                    Some(&err.message),
                );
            }
            return;
        }
    };

    match run::control_context::commit_for_worker(&state, &run_id, &solver_id) {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
            state.agents.close(&thread_id).await;
            state.runs.unregister_worker(&run_id, &solver_id);
            finish_worker_events(
                &state,
                &run_id,
                &spec,
                intent_id,
                &workdir,
                started_at,
                false,
                "context_delivery_unknown",
            );
            if finalize_on_exit {
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "context_delivery_unknown",
                    Some("Operator context changed before Worker delivery"),
                );
            }
            return;
        }
    }

    if host_cancel.load(Ordering::SeqCst) {
        *exit_reason_slot.lock().unwrap() = Some("canceled".to_owned());
        state.agents.close(&thread_id).await;
        state.runs.unregister_worker(&run_id, &solver_id);
        finish_worker_events(
            &state,
            &run_id,
            &spec,
            intent_id,
            &workdir,
            started_at,
            false,
            "intent_dropped",
        );
        return;
    }

    if let Err(err) = session
        .send(SessionCommand::SendTurn {
            delivery_id: 1,
            text: runtime_prompt,
            options: spec.effort.clone().map(|effort| TurnOptions {
                effort: Some(effort),
                ..Default::default()
            }),
            attachments: Vec::new(),
        })
        .await
    {
        let dropped = host_cancel.load(Ordering::SeqCst);
        let exit_class =
            classify_worker_exit_reason(false, dropped, false, false, Some(&err.message));
        *exit_reason_slot.lock().unwrap() = Some(exit_class.to_owned());
        state.agents.close(&thread_id).await;
        state.runs.unregister_worker(&run_id, &solver_id);
        finish_worker_events(
            &state,
            &run_id,
            &spec,
            intent_id,
            &workdir,
            started_at,
            false,
            if dropped {
                "intent_dropped"
            } else {
                "runtime_failure"
            },
        );
        if finalize_on_exit && !dropped {
            mark_run_finished(
                &state,
                &run_id,
                Some(&solver_id),
                false,
                "runtime_failure",
                Some(&err.message),
            );
        }
        return;
    }

    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let mut solved = false;
    let mut timed_out = false;
    let mut vendor_err = false;
    let mut last_error: Option<String> = None;
    let continue_session = same_session_continuation(&spec, &challenge, &assignment);
    let max_continuations = effective_max_continuations(&spec.profile);
    let mut continuations = 0u64;
    let mut slice_resume_prompt: Option<String> = None;
    let mut next_delivery_id = 2u64;

    let execute_started = tokio::time::Instant::now();
    let execute_timeout_secs = effective_execute_timeout_seconds(&spec.profile, &challenge);
    let suspended_at_start = run::control_freeze::worker_budget(&state, &run_id, &solver_id).0;
    let soft_reminder_secs = effective_soft_reminder_seconds(&spec.profile);
    let mut clock=WorkerClock::new(execute_started,execute_timeout_secs,first_execution_slice_seconds(soft_reminder_secs,execute_timeout_secs));

    loop {
        if host_cancel.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep_until(clock.total_deadline) => {
                let (suspended,frozen)=run::control_freeze::worker_budget(&state,&run_id,&solver_id);
                if frozen{clock.total_deadline=tokio::time::Instant::now()+Duration::from_millis(120);continue;}
                if let Some(left)=clock.checkpoint_remaining(suspended) {
                    if left>0. {clock.wait(left);continue;}
                    timed_out=true;last_error=Some("Worker conclusion budget expired".into());break;
                }
                let left=match budget.remaining(&state,&run_id,&solver_id,if clock.concluding(){"conclude"}else{"execute"}){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}};
                if left>0.{clock.wait(left);continue;}
                if !clock.concluding() && (match budget.remaining(&state,&run_id,&solver_id,"conclude"){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}})>0.{
                    clock.conclude();clock.checkpoint_sent=false;clock.soft_reminder_deadline=None;
                    let request_id=format!("budget-conclude-{}",uuid::Uuid::new_v4().simple());let(reminder_state,id,solver,reminder_session)=(state.clone(),run_id.clone(),solver_id.clone(),session.clone());
                    let conclude_limit=Duration::from_secs_f64(match budget.remaining(&state,&run_id,&solver_id,"conclude"){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}});
                    let task=tokio::spawn(async move{let result=tokio::time::timeout(conclude_limit,reminder_session.send_confirmed(SessionCommand::Steer{request_id:request_id.clone(),text:"Execution budget reached its conclusion phase. Stop target operations, retain actual evidence and commit the assigned Step now.".into(),attachments:Vec::new()},&request_id)).await.map_err(|_|"Conclusion delivery deadline expired".to_owned()).and_then(|result|result.map_err(|e|e.message));emit_event(&reminder_state,&id,Some(&solver),"worker.prompt",json!({"prompt_id":request_id,"kind":"conclude","status":if result.is_ok(){"delivered"}else{"delivery_unknown"}}));if result.is_err(){let _=reminder_session.send(SessionCommand::Interrupt).await;}});state.runs.handle(&state,&run_id).worker_tasks.lock().unwrap().push(task.abort_handle());
                    clock.total_deadline=tokio::time::Instant::now()+Duration::from_secs_f64(match budget.remaining(&state,&run_id,&solver_id,"conclude"){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}});continue;
                }
                timed_out = true;
                last_error = Some(format!("Worker execution timed out after {execute_timeout_secs}s"));
                break;
            }
            _ = async {
                match clock.soft_reminder_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            }, if clock.soft_reminder_deadline.is_some() => {
                let(suspended,frozen)=run::control_freeze::worker_budget(&state,&run_id,&solver_id);
                let delta=Duration::from_secs_f64((suspended-suspended_at_start).max(0.));
                clock.total_deadline=execute_started+Duration::from_secs(execute_timeout_secs)+delta;
                let expected=execute_started+Duration::from_secs(first_execution_slice_seconds(soft_reminder_secs,execute_timeout_secs).unwrap_or(execute_timeout_secs))+delta;
                if frozen{clock.soft_reminder_deadline=Some(tokio::time::Instant::now()+Duration::from_millis(120));continue;}
                if tokio::time::Instant::now()<expected{clock.soft_reminder_deadline=Some(expected);continue;}
                clock.soft_reminder_deadline = None;
                let now_inst = tokio::time::Instant::now();
                if now_inst < clock.total_deadline && !host_cancel.load(Ordering::SeqCst) {
                    let elapsed_secs = now_inst.saturating_duration_since(execute_started).as_secs();
                    let remaining_secs = clock.total_deadline.saturating_duration_since(now_inst).as_secs();
                    if remaining_secs > 0 {
                        let reminder_text = soft_reminder_prompt(
                            &assignment,
                            continue_session,
                            finalize_on_exit,
                            elapsed_secs,
                            remaining_secs,
                        );
                        slice_resume_prompt=Some(reminder_text);
                        if let Err(error)=session.send(SessionCommand::Interrupt).await {last_error=Some(error.message);break;}


                    }
                }
            }
            _ = tick.tick() => {
                if let Err(error)=budget.checkpoint(&state,&run_id,&solver_id){last_error=Some(error);break;}
                if (match budget.remaining(&state,&run_id,&solver_id,if clock.concluding(){"conclude"}else{"execute"}){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}})<=0.{clock.total_deadline=tokio::time::Instant::now();continue;}
                if host_cancel.load(Ordering::SeqCst) { break; }
                let (s_now, finish_now) = drain_blackboard_ingress(
                    &state,
                    &run_id,
                    &solver_id,
                    &ingress_dir,
                    &sdir,
                );
                if s_now {
                    solved = true;
                }
                if s_now || finish_now {
                    // Give the CLI a tiny moment to read its result-{request_id}.json before closing
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let _ = drain_blackboard_ingress(&state, &run_id, &solver_id, &ingress_dir, &sdir);
                    break;
                }
            }
            ev_res = events.recv() => {
                if host_cancel.load(Ordering::SeqCst) { break; }
                let Ok(event) = ev_res else {
                    break;
                };
                session.observe(&event);
                if let Some(resume)=session.resume_cursor(){let established=matches!(event.root_event(),Some(AgentEvent::TurnCompleted{status:TurnStatus::Completed,..}));if let Err(error)=run::continuation::save(&state,&run_id,&spec,&workdir,&resume,session.provider_session_id().as_deref(),established){last_error=Some(error);break;}}
                if let Err(error)=usage_observer.observe(&state,&run_id,&spec,&event){last_error=Some(error);break;}
                let Some(event)=event.into_root_event()else{continue};
                match event {
                    AgentEvent::SteerAccepted { request_id } => {
                        if let Some(reply) = state.runs.handle(&state,&run_id)
                            .control_steer_acks.lock().unwrap().remove(&request_id) {
                            let _ = reply.send(true);
                        }
                    }
                    AgentEvent::SteerRejected { request_id } => {
                        if let Some(reply) = state.runs.handle(&state,&run_id)
                            .control_steer_acks.lock().unwrap().remove(&request_id) {
                            let _ = reply.send(false);
                        }
                    }
                    AgentEvent::ApprovalRequested(req) => {
                        let decision = access::decide(&session.access, &req.kind, &req.options)
                            .or_else(|| {
                                req.options.first().map(|o| agent::ApprovalDecision::Option(o.id.clone()))
                            })
                            .unwrap_or(agent::ApprovalDecision::Cancel);
                        let _ = session
                            .send(SessionCommand::RespondApproval {
                                request_id: req.id,
                                decision,
                            })
                            .await;
                    }
                    AgentEvent::Delta { kind, text, .. } => match kind {
                        DeltaKind::AssistantText => {
                            if !text.is_empty() {
                                emit_event(
                                    &state,
                                    &run_id,
                                    Some(&solver_id),
                                    "text.delta",
                                    json!({"text": redact_operator_secrets(&text,&secret_values),
                                        "delta": redact_operator_secrets(&text,&secret_values)}),
                                );
                            }
                        }
                        DeltaKind::ReasoningText => {
                            if !text.is_empty() {
                                emit_event(
                                    &state,
                                    &run_id,
                                    Some(&solver_id),
                                    "reasoning.delta",
                                    json!({"text": redact_operator_secrets(&text,&secret_values),
                                        "delta": redact_operator_secrets(&text,&secret_values)}),
                                );
                            }
                        }
                        DeltaKind::CommandOutput => {
                            if !text.is_empty() {
                                emit_event(
                                    &state,
                                    &run_id,
                                    Some(&solver_id),
                                    "terminal.output",
                                    json!({"text": redact_operator_secrets(&text,&secret_values)}),
                                );
                            }
                        }
                    },
                    AgentEvent::ItemStarted(item) => {
                        if let Some((tool_label, _)) = summarize_tool_item(&item.content) {
                            emit_event(
                                &state,
                                &run_id,
                                Some(&solver_id),
                                "tool.start",
                                json!({
                                    "tool": redact_operator_secrets(&tool_label,&secret_values),
                                    "tool_call_id": item.id,
                                    "call_id": item.id,
                                }),
                            );
                        }
                    }
                    AgentEvent::ItemCompleted(item) => {
                        if let Some((tool_label, output_opt)) = summarize_tool_item(&item.content) {
                            let output = redact_operator_secrets(&output_opt.unwrap_or_default(),
                                &secret_values);
                            let tool_label = redact_operator_secrets(&tool_label,&secret_values);
                            let art_seq = state.runs.next_artifact_seq(&run_id);
                            let art_id = match store_artifact(&sdir, art_seq, &tool_label, &output) {
                                Ok(id) => id,
                                Err(error) => {
                                    last_error = Some(format!("工具证据保存失败：{error}"));
                                    break;
                                }
                            };
                            if !output.is_empty() {
                                emit_event(
                                    &state,
                                    &run_id,
                                    Some(&solver_id),
                                    "terminal.output",
                                    json!({"text": format!("$ {tool_label}\n{output}\n")}),
                                );
                            }
                            let condensed: String = output.chars().take(1200).collect();
                            emit_event(
                                &state,
                                &run_id,
                                Some(&solver_id),
                                "tool.result",
                                json!({
                                    "tool": tool_label,
                                    "tool_call_id": item.id,
                                    "call_id": item.id,
                                    "artifact_id": art_id,
                                    "result": {
                                        "condensed": format!("[artifact_id={art_id}] {condensed}"),
                                    },
                                }),
                            );
                        }
                        let (s_now, _) = drain_blackboard_ingress(
                            &state,
                            &run_id,
                            &solver_id,
                            &ingress_dir,
                            &sdir,
                        );
                        if s_now {
                            solved = true;
                        }
                    }
                    AgentEvent::TokenUsage(_) | AgentEvent::UsageDetail{..} => {},
                    AgentEvent::Error { message, fatal } => {
                        last_error = Some(message);
                        if fatal {
                            break;
                        }
                    }
                    AgentEvent::ProviderStartFailed { error } => {
                        vendor_err = true;
                        last_error = Some(error);
                        break;
                    }
                    AgentEvent::TurnCompleted { status, .. } => {
                        if status == TurnStatus::Failed && last_error.is_none() {
                            last_error = Some("Worker 轮次执行失败".to_owned());
                        }
                        let (solved_now, finished_now) = drain_blackboard_ingress(
                            &state, &run_id, &solver_id, &ingress_dir, &sdir);
                        if solved_now { solved = true; }
                        if let Some(text)=slice_resume_prompt.take() {
                            if !solved && !finished_now && !host_cancel.load(Ordering::SeqCst) && status != TurnStatus::Failed {
                                match session.send(SessionCommand::SendTurn {delivery_id:next_delivery_id,text:text.clone(),options:None,attachments:Vec::new()}).await {
                                    Ok(())=>{next_delivery_id+=1;emit_event(&state,&run_id,Some(&solver_id),"worker.prompt",json!({"kind":"soft_reminder","status":"sent","prompt":text,"transport":"same_session_resume"}));continue;},
                                    Err(error)=>{last_error=Some(error.message);break;}
                                }
                            }
                        }
                        if status == TurnStatus::Completed && !continue_session && !clock.concluding()
                            && !solved && !finished_now && !host_cancel.load(Ordering::SeqCst) {
                            let left=match budget.remaining(&state,&run_id,&solver_id,"conclude") {Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}}
                                .min(spec.profile["conclude_timeout"].as_f64().unwrap_or(120.));
                            if left>0. {
                                let text=run::worker_cli_checkpoint::prompt(&assignment);
                                match session.send(SessionCommand::SendTurn {delivery_id:next_delivery_id,text:text.clone(),options:None,attachments:Vec::new()}).await {
                                    Ok(())=>{
                                        next_delivery_id+=1;clock.conclude();clock.checkpoint_sent=true;clock.soft_reminder_deadline=None;
                                        clock.wait(left);
                                        clock.arm_checkpoint(run::control_freeze::worker_budget(&state,&run_id,&solver_id).0);
                                        emit_event(&state,&run_id,Some(&solver_id),"worker.prompt",json!({"kind":"checkpoint","status":"sent","prompt":text,"intent_id":intent_id}));
                                        continue;
                                    }
                                    Err(error)=>{last_error=Some(error.message);break;}
                                }
                            }
                        }
                        if (status == TurnStatus::Completed || clock.concluding() && status == TurnStatus::Interrupted) && (continue_session || clock.concluding())
                            && !solved && !finished_now && !clock.checkpoint_sent
                            && !host_cancel.load(Ordering::SeqCst)
                            && tokio::time::Instant::now() < clock.total_deadline
                        {
                            let boundary = clock.concluding() || continuation_boundary_reached(continuations, max_continuations);
                            let text = if boundary {
                                clock.checkpoint_sent = true;
                                emit_event(&state,&run_id,Some(&solver_id),"blackboard.delta",
                                    json!({"kind":"worker_campaign_boundary","worker":solver_id,
                                        "intent_id":intent_id,"continuations":continuations,
                                        "reason":"continuation_limit"}));
                                "Conclude this assigned work now. Record only confirmed evidence, \
                                 then commit the Step or report the current platform verdict."
                                    .to_owned()
                            } else {
                                continuations += 1;
                                campaign_continuation_prompt(&assignment, finalize_on_exit)
                            };
                            match session.send(SessionCommand::SendTurn {
                                delivery_id: next_delivery_id,
                                text,
                                options: spec.effort.clone().map(|effort| TurnOptions {
                                    effort: Some(effort), ..Default::default()
                                }),
                                attachments: Vec::new(),
                            }).await {
                                Ok(()) => {
                                    next_delivery_id += 1;
                                    if !boundary {
                                        emit_event(&state,&run_id,Some(&solver_id),"blackboard.delta",
                                            json!({"kind":"worker_campaign_continued",
                                                "worker":solver_id,"intent_id":intent_id,
                                                "continuation":continuations}));
                                    }
                                    continue;
                                }
                                Err(error) => last_error = Some(error.message),
                            }
                        }
                        break;
                    }
                    AgentEvent::SessionClosed { reason } => {
                        if last_error.is_none() {
                            last_error = reason;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    if !host_cancel.load(Ordering::SeqCst) {
        let (s_final, _) =
            drain_blackboard_ingress(&state, &run_id, &solver_id, &ingress_dir, &sdir);
        if s_final {
            solved = true;
        }
    }

    state.agents.close(&thread_id).await;
    state.runs.unregister_worker(&run_id, &solver_id);

    if !host_cancel.load(Ordering::SeqCst)
        && let Ok(Some(stored)) = state.store.get("runs", &run_id)
        && stored["solved"] == true
    {
        solved = true;
    }

    let canceled = host_cancel.load(Ordering::SeqCst);
    let classified_exit = if vendor_err && !solved && !canceled && !timed_out {
        "vendor_error"
    } else {
        classify_worker_exit_reason(solved, canceled, timed_out, false, last_error.as_deref())
    };
    *exit_reason_slot.lock().unwrap() = Some(classified_exit.to_owned());

    let reason = if canceled {
        "intent_dropped"
    } else if solved {
        "solved"
    } else if timed_out {
        "timeout"
    } else if last_error.is_some() {
        "runtime_failure"
    } else {
        "finished"
    };

    finish_worker_events(
        &state, &run_id, &spec, intent_id, &workdir, started_at, solved, reason,
    );

    if !canceled && (finalize_on_exit || solved) && state.runs.active_worker_count(&run_id) == 0 {
        mark_run_finished(
            &state,
            &run_id,
            Some(&solver_id),
            solved,
            reason,
            last_error.as_deref(),
        );
    }
}

fn finish_worker_events(
    state: &AppState,
    run_id: &str,
    spec: &WorkerSpec,
    intent_id: &str,
    workdir: &Path,
    started_at: f64,
    solved: bool,
    reason: &str,
) {
    let solver_id = &spec.solver_id;
    let model_str = spec.model.as_deref().unwrap_or("default");
    let finished_at = now_secs();
    let flags: Vec<String> = state
        .store
        .get("runs", run_id)
        .ok()
        .flatten()
        .and_then(|r| r["flags"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let first_flag = flags.first().cloned();

    let exit_class = match reason {
        "solved" => "solved",
        "intent_dropped" | "canceled" | "operator_stop" => "canceled",
        "timeout" => "timeout",
        "oom" => "oom",
        "vendor_error" => "vendor_error",
        "runtime_failure" | "preflight_failed" | "context_delivery_unknown" | "failed" => "failed",
        _ => {
            if solved {
                "solved"
            } else {
                "done"
            }
        }
    };

    let settled = match run::worker_result::salvage(
        state,
        run_id,
        solver_id,
        intent_id,
        if solved { "solved" } else { exit_class },
        reason,
    ) {
        Ok(_) => true,
        Err(error) => {
            emit_event(
                state,
                run_id,
                Some(solver_id),
                "worker.error",
                json!({"reason":"worker_result_salvage_failed","detail":error,"intent_id":intent_id}),
            );
            false
        }
    };

    // Decide may have concluded the Step before the Worker exit, including
    // when its cancellation request was not acknowledged. The Worker exit is
    // then a lifecycle receipt, not a second Step verdict.
    let superseded = reason == "intent_dropped";
    if settled && !superseded {
        emit_event(
            state,
            run_id,
            Some(solver_id),
            "blackboard.delta",
            json!({
                "kind": "intent_concluded",
                "intent_id": intent_id,
                "worker": solver_id,
                "result": exit_class,
            }),
        );
    }
    emit_event(
        state,
        run_id,
        Some(solver_id),
        "blackboard.delta",
        json!({
            "kind": "worker_finished",
            "worker": solver_id,
            "result": if solved { "solved" } else { "finished" },
            "exit_reason": exit_class,
        }),
    );
    emit_event(
        state,
        run_id,
        Some(solver_id),
        "worker.lifecycle",
        json!({
            "phase": "exited",
            "phase_label": if solved { "Solved" } else { "Finished" },
            "intent_id": intent_id,
            "worker_role": "worker",
            "engine": spec.raw_engine,
            "model": model_str,
            "exit_reason": exit_class,
        }),
    );
    let runtime_cwd = if spec.backend == "container" {
        run::container_exec::workspace_path(&state, &run_id)
    } else {
        workdir.to_string_lossy().into_owned()
    };
    emit_event(
        state,
        run_id,
        Some(solver_id),
        "worker.status",
        json!({
            "online": false,
            "status": "done",
            "reason": reason,
            "exit_reason": exit_class,
            "engine": spec.raw_engine,
            "worker_role": "worker",
            "phase": "explore",
            "intent_id": intent_id,
            "profile_id": spec.profile_id,
            "profile_label": spec.profile_label,
            "model": model_str,
            "account_id": spec.account_id,
            "runtime": {
                "backend": spec.backend,
                "status": "exited",
                "cwd": runtime_cwd,
                "started_at": started_at,
                "finished_at": finished_at,
            },
        }),
    );
    emit_event(
        state,
        run_id,
        Some(solver_id),
        "worker.finished",
        json!({
            "solved": solved,
            "flag": first_flag,
            "flags": flags,
            "reason": reason,
            "exit_reason": exit_class,
        }),
    );
}

async fn execute_container_worker_task(
    state: Arc<AppState>,
    run_id: String,
    spec: WorkerSpec,
    workdir: PathBuf,
    ingress_dir: PathBuf,
    sdir: PathBuf,
    challenge: Value,
    assignment: IntentAssignment,
    prompt: String,
    secret_values: Vec<String>,
    started_at: f64,
    finalize_on_exit: bool,
    mut cancel_rx: Option<oneshot::Receiver<oneshot::Sender<bool>>>,
    exit_reason_slot: Arc<Mutex<Option<String>>>,
    mut budget: run::execution_budget::WorkerLease,
    reservation_id: String,
) {
    let solver_id = spec.solver_id.clone();
    let intent_id = assignment.intent_id.as_str();
    let mode = "ctf";

    let mut saved_cfg = match run::launch_config::settings(&state, &run_id) {
        Ok(config) => config,
        Err(error) => {
            *exit_reason_slot.lock().unwrap() = Some("failed".into());
            emit_event(
                &state,
                &run_id,
                Some(&solver_id),
                "worker.error",
                json!({"reason":"configuration_unavailable","detail":error}),
            );
            state.runs.unregister_worker(&run_id, &solver_id);
            return;
        }
    };
    if let Ok(Some(run)) = state.store.get("runs", &run_id) {
        if let Some(network) = run["worker_network"].as_str().filter(|v| !v.is_empty()) {
            saved_cfg["worker_network"] = json!(network);
        }
    }
    saved_cfg["_runner_browser_enabled"] = json!(false);
    saved_cfg["_runner_solver_id"] = json!(solver_id);
    saved_cfg["_runner_material_key"] = json!(crate::runner_remote::controller::material_key(
        &spec.norm_engine,
        &spec.effective_credential_id,
        spec.model.as_deref()
    ));

    let mut container_ingress = format!(
        "{}/.muteki-blackboard-ingress/{solver_id}",
        run::container_exec::workspace_path(&state, &run_id)
    );
    let mut container_script = format!(
        "{}/.muteki-skills/muteki-blackboard/blackboard.py",
        run::container_exec::workspace_path(&state, &run_id)
    );
    let worker_role = role_for_worker_class(&assignment.worker_class);
    let blackboard_role = if worker_role == "explore" {
        "solve"
    } else {
        worker_role
    };
    let execution_mode =
        blackboard_execution_mode(&spec, &challenge, &assignment, finalize_on_exit);
    let restart_bound = state.runs.has_target_restarter(&run_id);
    let campaign_mode = execution_mode == "campaign";
    let platform_confirmation = if challenge["platform_confirmation_required"] == true {
        "1"
    } else {
        "0"
    };
    let mut env_json = json!({
        "MUTEKI_BLACKBOARD_INGRESS_DIR": container_ingress,
        "MUTEKI_BLACKBOARD_SCRIPT": container_script,
        "MUTEKI_WORKER_ID": solver_id,
        "MUTEKI_EXECUTION_GENERATION": worker_execution_generation(&solver_id).to_string(),
        "MUTEKI_CHALLENGE_MODE": mode,
        "MUTEKI_BLACKBOARD_ROLE": blackboard_role,
        "MUTEKI_BLACKBOARD_PROFILE": blackboard_role,
        "MUTEKI_WORKER_EXECUTION_MODE": execution_mode,
        "MUTEKI_PLATFORM_CONFIRMATION_REQUIRED": platform_confirmation,
        "MUTEKI_TARGET_EPOCH": "1",
    });
    if restart_bound {
        env_json["MUTEKI_TARGET_RESTART_AVAILABLE"] = json!("1");
    }

    let _ = std::fs::write(
        sdir.join(".muteki-skills")
            .join("muteki-blackboard")
            .join("env.json"),
        env_json.to_string(),
    );
    let _ = std::fs::write(
        sdir.join(".muteki-skills")
            .join("muteki-blackboard")
            .join("SKILL.md"),
        super::render_blackboard_skill("ctf", restart_bound, campaign_mode),
    );

    if let Err(error) = record_external_start_request(&state, &run_id, &solver_id) {
        *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
        let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
        emit_event(
            &state,
            &run_id,
            Some(&solver_id),
            "worker.error",
            json!({"reason":"runtime_failure","detail":error}),
        );
        state.runs.unregister_worker(&run_id, &solver_id);
        finish_worker_events(
            &state,
            &run_id,
            &spec,
            intent_id,
            &workdir,
            started_at,
            false,
            "runtime_failure",
        );
        if finalize_on_exit {
            mark_run_finished(
                &state,
                &run_id,
                Some(&solver_id),
                false,
                "runtime_failure",
                Some(&error),
            );
        }
        return;
    }

    let handle = match run::container_exec::ensure_run_container_owned(
        &state,
        &run_id,
        &workdir,
        &saved_cfg,
        Some(&run_id),
    )
    .await
    {
        Ok(h) => h,
        Err(err) => {
            *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
            let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
            emit_event(
                &state,
                &run_id,
                Some(&solver_id),
                "worker.error",
                json!({"reason":"runtime_failure","detail":err}),
            );
            state.runs.unregister_worker(&run_id, &solver_id);
            finish_worker_events(
                &state,
                &run_id,
                &spec,
                intent_id,
                &workdir,
                started_at,
                false,
                "runtime_failure",
            );
            if finalize_on_exit {
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "runtime_failure",
                    Some(&err),
                );
            }
            return;
        }
    };

    if handle.link.remote.is_some() {
        container_ingress = format!(
            "{}/.muteki-blackboard-ingress/{solver_id}",
            handle.container_workspace
        );
        container_script = format!(
            "{}/.muteki-skills/muteki-blackboard/blackboard.py",
            handle.container_workspace
        );
    }
    let mut extra_env = vec![
        ("MUTEKI_BLACKBOARD_SCRIPT".to_owned(), container_script),
        (
            "MUTEKI_BLACKBOARD_INGRESS_DIR".to_owned(),
            container_ingress,
        ),
        ("MUTEKI_WORKER_ID".to_owned(), solver_id.clone()),
        (
            "MUTEKI_EXECUTION_GENERATION".to_owned(),
            worker_execution_generation(&solver_id).to_string(),
        ),
        ("MUTEKI_CHALLENGE_MODE".to_owned(), mode.to_owned()),
        (
            "MUTEKI_BLACKBOARD_ROLE".to_owned(),
            blackboard_role.to_owned(),
        ),
        (
            "MUTEKI_BLACKBOARD_PROFILE".to_owned(),
            blackboard_role.to_owned(),
        ),
        (
            "MUTEKI_WORKER_EXECUTION_MODE".to_owned(),
            execution_mode.to_owned(),
        ),
        (
            "MUTEKI_PLATFORM_CONFIRMATION_REQUIRED".to_owned(),
            platform_confirmation.to_owned(),
        ),
        ("MUTEKI_TARGET_EPOCH".to_owned(), "1".to_owned()),
    ];
    if restart_bound {
        extra_env.push(("MUTEKI_TARGET_RESTART_AVAILABLE".to_owned(), "1".to_owned()));
    }

    let staged_plugins =
        super::stage_swarm_worker_plugins(&state, &workdir, &solver_id, &spec.norm_engine, mode)
            .unwrap_or_default();
    match spec.norm_engine.as_str() {
        "pi" => {
            extra_env.push((
                "MUTEKI_PI_SYSTEM_PROMPT".to_owned(),
                ctf_prompts::system(worker_role == "verifier"),
            ));
            let container_skills: Vec<String> = staged_plugins
                .skills
                .iter()
                .filter_map(|p| handle.to_container_path(p).ok())
                .collect();
            if !container_skills.is_empty() {
                extra_env.push((
                    "MUTEKI_PI_EXTRA_SKILLS".to_owned(),
                    json!(container_skills).to_string(),
                ));
            }
            let container_extensions: Vec<String> = staged_plugins
                .extensions
                .iter()
                .filter_map(|p| handle.to_container_path(p).ok())
                .collect();
            if !container_extensions.is_empty() {
                extra_env.push((
                    "MUTEKI_PI_EXTRA_EXTENSIONS".to_owned(),
                    json!(container_extensions).to_string(),
                ));
            }
        }
        "omp" => extra_env.push((
            "MUTEKI_OMP_SYSTEM_PROMPT".to_owned(),
            ctf_prompts::system(worker_role == "verifier"),
        )),
        _ => {}
    }
    let (env_map, _) = match run::container_exec::prepare_container_worker_env(
        &state,
        &handle,
        &solver_id,
        &spec.norm_engine,
        &spec.effective_credential_id,
        spec.model.as_deref(),
        &extra_env,
    )
    .await
    {
        Ok(pair) => pair,
        Err(err) => {
            *exit_reason_slot.lock().unwrap() = Some(
                classify_worker_exit_reason(false, false, false, false, Some(&err.message))
                    .to_owned(),
            );
            let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
            emit_event(
                &state,
                &run_id,
                Some(&solver_id),
                "worker.error",
                json!({"reason":"preflight_failed","detail":err.message}),
            );
            state.runs.unregister_worker(&run_id, &solver_id);
            finish_worker_events(
                &state,
                &run_id,
                &spec,
                intent_id,
                &workdir,
                started_at,
                false,
                "preflight_failed",
            );
            if finalize_on_exit {
                run::container_exec::teardown_run_container(&run_id).await;
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "preflight_failed",
                    Some(&err.message),
                );
            }
            return;
        }
    };

    let interactive_pi = spec.norm_engine == "pi";
    let (argv, stdin) = match run::worker_cli_checkpoint::initial(
        &spec.norm_engine,
        spec.model.as_deref(),
        spec.effort.as_deref(),
        &prompt,
        &env_map,
        interactive_pi,
        secret_values.is_empty(),
    ) {
        Ok(pair) => pair,
        Err(err) => {
            *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
            let _ = run::control_context::release_for_worker(&state, &run_id, &solver_id);
            emit_event(
                &state,
                &run_id,
                Some(&solver_id),
                "worker.error",
                json!({"reason":"runtime_failure","detail":err.message}),
            );
            state.runs.unregister_worker(&run_id, &solver_id);
            finish_worker_events(
                &state,
                &run_id,
                &spec,
                intent_id,
                &workdir,
                started_at,
                false,
                "runtime_failure",
            );
            if finalize_on_exit {
                run::container_exec::teardown_run_container(&run_id).await;
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "runtime_failure",
                    Some(&err.message),
                );
            }
            return;
        }
    };

    let mut usage_observer = run::usage::Observer::new(
        &format!("container:{}", uuid::Uuid::new_v4().simple()),
        worker_role,
    );
    let execute_timeout_secs = effective_execute_timeout_seconds(&spec.profile, &challenge);
    let cspec = run::container_exec::WorkerSpec {
        reservation_id: Some(reservation_id),
        owner_run_id: (handle.scope == "shared").then(|| run_id.clone()),
        owner_token: (handle.scope == "shared").then(|| handle.owner_token.clone()),
        owner_workspace: (handle.scope == "shared").then(|| handle.container_workspace.clone()),
        argv,
        cwd: handle.container_workspace.clone(),
        stdin,
        stdin_mode: interactive_pi.then(|| "stream".to_owned()),
        env: env_map,
        timeout_sec: 0,
        tag: Some(solver_id.clone()),
        output_limit_bytes: handle.limits.output_limit_bytes,
        disk_limit_bytes: handle.limits.disk_limit_bytes,
    };

    let (mut container_wid, mut rx) =
        match start_container_worker(&state, &run_id, &solver_id, &handle.link, &cspec, &budget)
            .await
        {
            Ok(pair) => pair,
            Err(err) => {
                *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
                let _ = run::control_context::unknown_for_worker(&state, &run_id, &solver_id);
                emit_event(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    "worker.error",
                    json!({"reason":"runtime_failure","detail":err}),
                );
                state.runs.unregister_worker(&run_id, &solver_id);
                finish_worker_events(
                    &state,
                    &run_id,
                    &spec,
                    intent_id,
                    &workdir,
                    started_at,
                    false,
                    "runtime_failure",
                );
                if finalize_on_exit {
                    run::container_exec::teardown_run_container(&run_id).await;
                    mark_run_finished(
                        &state,
                        &run_id,
                        Some(&solver_id),
                        false,
                        "runtime_failure",
                        Some(&err),
                    );
                }
                return;
            }
        };

    match run::control_context::commit_for_worker(&state, &run_id, &solver_id) {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
            let _ = handle.link.signal(&container_wid, "KILL", None, None).await;
            state.runs.unregister_worker(&run_id, &solver_id);
            finish_worker_events(
                &state,
                &run_id,
                &spec,
                intent_id,
                &workdir,
                started_at,
                false,
                "context_delivery_unknown",
            );
            if finalize_on_exit {
                run::container_exec::teardown_run_container(&run_id).await;
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "context_delivery_unknown",
                    Some("Operator context changed before Worker delivery"),
                );
            }
            return;
        }
    }

    state
        .runs
        .handle(&state, &run_id)
        .container_sessions
        .lock()
        .unwrap()
        .insert(
            solver_id.clone(),
            (handle.link.clone(), container_wid.clone()),
        );
    let initial_prompt_id = format!("worker-prompt-{}", uuid::Uuid::new_v4().simple());
    let mut pending_prompt_ids = HashSet::new();
    let mut pending_reminders = HashMap::<String, String>::new();
    if interactive_pi {
        let command = json!({"id":initial_prompt_id,"type":"prompt","message":prompt}).to_string();
        if handle.link.write_stdin(&container_wid, &command).await != Ok(true) {
            *exit_reason_slot.lock().unwrap() = Some("failed".to_owned());
            let _ = handle.link.signal(&container_wid, "KILL", None, None).await;
            state.runs.unregister_worker(&run_id, &solver_id);
            finish_worker_events(
                &state,
                &run_id,
                &spec,
                intent_id,
                &workdir,
                started_at,
                false,
                "runtime_failure",
            );
            if finalize_on_exit {
                run::container_exec::teardown_run_container(&run_id).await;
                mark_run_finished(
                    &state,
                    &run_id,
                    Some(&solver_id),
                    false,
                    "runtime_failure",
                    Some("Pi RPC initial prompt was not delivered"),
                );
            }
            return;
        }
        pending_prompt_ids.insert(initial_prompt_id.clone());
    }

    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let mut solved = false;
    let mut timed_out = false;
    let mut oom_killed = false;
    let mut last_error: Option<String> = None;
    let mut dropped_by_decide = false;
    let continue_session =
        interactive_pi && same_session_continuation(&spec, &challenge, &assignment);
    let max_continuations = effective_max_continuations(&spec.profile);
    let mut continuations = 0u64;
    let mut slice_resume_prompt: Option<String> = None;

    let mut cli_session =
        run::worker_cli_checkpoint::Session::invocation(&spec.norm_engine, &cspec.argv);
    let execute_started = tokio::time::Instant::now();
    let suspended_at_start = run::control_freeze::worker_budget(&state, &run_id, &solver_id).0;
    let soft_reminder_secs = effective_soft_reminder_seconds(&spec.profile);
    let mut clock=WorkerClock::new(execute_started,execute_timeout_secs,first_execution_slice_seconds(soft_reminder_secs,execute_timeout_secs));

    'worker: loop {
        tokio::select! {
            _ = tokio::time::sleep_until(clock.total_deadline) => {
                let (suspended,frozen)=run::control_freeze::worker_budget(&state,&run_id,&solver_id);
                if frozen{clock.total_deadline=tokio::time::Instant::now()+Duration::from_millis(120);continue;}
                if clock.awaiting_exit(){let _=handle.link.signal(&container_wid,"KILL",None,None).await;last_error=Some("Worker exit before same-session checkpoint was not observed".into());break;}
                if let Some(left)=clock.checkpoint_remaining(suspended) {
                    if left>0. {clock.wait(left);continue;}
                    timed_out=true;last_error=Some("Worker conclusion budget expired".into());break;
                }
                let left=match budget.remaining(&state,&run_id,&solver_id,if clock.concluding(){"conclude"}else{"execute"}){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}};
                if left>0.{clock.wait(left);continue;}
                if !clock.concluding() && interactive_pi && (match budget.remaining(&state,&run_id,&solver_id,"conclude"){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}})>0.{
                    clock.conclude();clock.checkpoint_sent=false;clock.soft_reminder_deadline=None;
                    let id=format!("budget-conclude-{}",uuid::Uuid::new_v4().simple());let message="Execution budget reached conclusion. Stop target operations, retain actual evidence and commit the assigned Step now.";
                    if handle.link.write_stdin(&container_wid,&json!({"id":id,"type":"steer","message":message}).to_string()).await==Ok(true){pending_reminders.insert(id.clone(),message.to_owned());}
                    emit_event(&state,&run_id,Some(&solver_id),"worker.prompt",json!({"prompt_id":id,"kind":"conclude","status":"pending_native_ack"}));
                    clock.total_deadline=tokio::time::Instant::now()+Duration::from_secs_f64(match budget.remaining(&state,&run_id,&solver_id,"conclude"){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}});continue;
                }
                if !clock.concluding()&&!interactive_pi&&secret_values.is_empty()&&cli_session.resumable().is_some(){
                    let left=match budget.remaining(&state,&run_id,&solver_id,"conclude"){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}};
                    if left>0.{clock.await_exit();clock.soft_reminder_deadline=None;timed_out=true;
                        if !handle.link.signal(&container_wid,"TERM",None,None).await.is_ok_and(|frame|frame.ok){last_error=Some("Worker stop before checkpoint was not acknowledged".into());break;}
                        clock.total_deadline=tokio::time::Instant::now()+Duration::from_secs_f64(left.min(5.));continue;
                    }
                }
                timed_out = true;
                last_error = Some(format!("Worker execution timed out after {execute_timeout_secs}s"));
                let _ = handle.link.signal(&container_wid, "TERM", None, None).await;
                break;
            }
            _ = async {
                match clock.soft_reminder_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            }, if clock.soft_reminder_deadline.is_some() => {
                let(suspended,frozen)=run::control_freeze::worker_budget(&state,&run_id,&solver_id);
                let delta=Duration::from_secs_f64((suspended-suspended_at_start).max(0.));
                clock.total_deadline=execute_started+Duration::from_secs(execute_timeout_secs)+delta;
                let expected=execute_started+Duration::from_secs(first_execution_slice_seconds(soft_reminder_secs,execute_timeout_secs).unwrap_or(execute_timeout_secs))+delta;
                if frozen{clock.soft_reminder_deadline=Some(tokio::time::Instant::now()+Duration::from_millis(120));continue;}
                if tokio::time::Instant::now()<expected{clock.soft_reminder_deadline=Some(expected);continue;}
                clock.soft_reminder_deadline = None;
                let now_inst = tokio::time::Instant::now();
                if now_inst < clock.total_deadline && !dropped_by_decide {
                    let elapsed_secs = now_inst.saturating_duration_since(execute_started).as_secs();
                    let remaining_secs = clock.total_deadline.saturating_duration_since(now_inst).as_secs();
                    if remaining_secs > 0 {
                        let reminder_text = soft_reminder_prompt(
                            &assignment,
                            continue_session,
                            finalize_on_exit,
                            elapsed_secs,
                            remaining_secs,
                        );
                        if interactive_pi {
                            slice_resume_prompt=Some(reminder_text);
                            let command=json!({"id":format!("slice-abort-{}",uuid::Uuid::new_v4().simple()),"type":"abort"}).to_string();
                            if handle.link.write_stdin(&container_wid,&command).await!=Ok(true){last_error=Some("Worker slice interruption was not delivered".into());break;}
                        }
                    }
                }
            }
            request = async {
                match &mut cancel_rx {
                    Some(receiver) => receiver.await.ok(),
                    None => std::future::pending().await,
                }
            }, if cancel_rx.is_some() => {
                cancel_rx = None;
                if let Some(reply) = request {
                    let stopped = handle.link.signal(&container_wid, "KILL", None, None)
                        .await.is_ok_and(|frame| frame.ok);
                    let _ = reply.send(stopped);
                    if stopped {
                        dropped_by_decide = true;
                        break;
                    }
                }
            }
            _ = tick.tick() => {
                if let Err(error)=budget.checkpoint(&state,&run_id,&solver_id){last_error=Some(error);break;}
                if (match budget.remaining(&state,&run_id,&solver_id,if clock.concluding(){"conclude"}else{"execute"}){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}})<=0.{clock.total_deadline=tokio::time::Instant::now();continue;}
                let (s_now, finish_now) = drain_blackboard_ingress(
                    &state,
                    &run_id,
                    &solver_id,
                    &ingress_dir,
                    &sdir,
                );
                if s_now {
                    solved = true;
                }
                if s_now || finish_now {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let _ = drain_blackboard_ingress(&state, &run_id, &solver_id, &ingress_dir, &sdir);
                    let _ = handle.link.signal(&container_wid, "TERM", None, None).await;
                    break;
                }
            }
            frame_opt = rx.recv() => {
                let Some(frame) = frame_opt else {
                    last_error = Some("Runner stream ended without a confirmed exit receipt; execution outcome is unknown".into());
                    break;
                };
                match frame.t.as_str() {
                    "transport_error" => {
                        last_error = Some("Runner transport was lost; Worker execution outcome is unknown".into());
                        break;
                    }
                    "out" => {
                        cli_session.observe(&frame.line);
                        if let Some(session)=cli_session.resumable(){if let Err(error)=run::continuation::save(&state,&run_id,&spec,&sdir,&json!({"session_id":session}),Some(session),true){last_error=Some(error);break;}}
                        if let Err(error)=usage_observer.container(&state,&run_id,&spec,&frame.line){last_error=Some(error);break;}

                        if interactive_pi {
                            if let Ok(rpc) = serde_json::from_str::<Value>(&frame.line) {
                                if rpc["type"] == "response" {
                                    let id = rpc["id"].as_str().unwrap_or("");
                                    let accepted = rpc["success"] == true;
                                    if let Some(message)=pending_reminders.remove(id){emit_event(&state,&run_id,Some(&solver_id),"worker.prompt",json!({"prompt_id":id,"prompt":message,"kind":"budget_control","status":if accepted{"delivered"}else{"rejected"},"error":rpc["error"]}));}
                                    if pending_prompt_ids.remove(id) && !accepted {
                                        last_error = Some(redact_operator_secrets(
                                            rpc["error"].as_str().unwrap_or("Pi RPC prompt failed"),
                                            &secret_values));
                                        let _ = handle.link.signal(&container_wid,"TERM",None,None).await;
                                        break;
                                    }
                                    if let Some(reply) = state.runs.handle(&state,&run_id)
                                        .control_steer_acks.lock().unwrap().remove(id) {
                                        let _ = reply.send(accepted);
                                    }
                                    continue;
                                }
                                if rpc["type"] == "agent_end" {
                                    let (solved_now, finished_now) = drain_blackboard_ingress(
                                        &state,&run_id,&solver_id,&ingress_dir,&sdir);
                                    if solved_now { solved = true; }
                                    if let Some(text)=slice_resume_prompt.take() {
                                        if !solved && !finished_now && !dropped_by_decide {
                                            let id=format!("slice-resume-{}",uuid::Uuid::new_v4().simple());
                                            if handle.link.write_stdin(&container_wid,&json!({"id":id,"type":"prompt","message":text}).to_string()).await!=Ok(true){last_error=Some("Worker slice resume was not delivered".into());break;}
                                            pending_prompt_ids.insert(id.clone());
                                            emit_event(&state,&run_id,Some(&solver_id),"worker.prompt",json!({"prompt_id":id,"kind":"soft_reminder","status":"sent","prompt":text,"transport":"same_session_resume"}));
                                            continue;
                                        }
                                    }
                                    if !continue_session && !clock.concluding() && !solved && !finished_now {
                                        let left=match budget.remaining(&state,&run_id,&solver_id,"conclude"){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}}
                                            .min(spec.profile["conclude_timeout"].as_f64().unwrap_or(120.));
                                        if left>0. {
                                            let text=run::worker_cli_checkpoint::prompt(&assignment);
                                            let id=format!("checkpoint-{}",uuid::Uuid::new_v4().simple());
                                            if handle.link.write_stdin(&container_wid,&json!({"id":id,"type":"prompt","message":text}).to_string()).await!=Ok(true){last_error=Some("Pi checkpoint was not delivered".into());break;}
                                            pending_prompt_ids.insert(id.clone());clock.conclude();clock.checkpoint_sent=true;clock.soft_reminder_deadline=None;
                                            clock.wait(left);
                                            clock.arm_checkpoint(run::control_freeze::worker_budget(&state,&run_id,&solver_id).0);
                                            emit_event(&state,&run_id,Some(&solver_id),"worker.prompt",json!({"prompt_id":id,"kind":"checkpoint","status":"sent","prompt":text,"intent_id":intent_id}));
                                            continue;
                                        }
                                    }
                                    if (continue_session || clock.concluding()) && !solved && !finished_now
                                        && !clock.checkpoint_sent
                                        && tokio::time::Instant::now() < clock.total_deadline
                                    {
                                        let boundary = clock.concluding() || continuation_boundary_reached(continuations, max_continuations);
                                        let text = if boundary {
                                            clock.checkpoint_sent = true;
                                            emit_event(&state,&run_id,Some(&solver_id),"blackboard.delta",
                                                json!({"kind":"worker_campaign_boundary",
                                                    "worker":solver_id,"intent_id":intent_id,
                                                    "continuations":continuations,
                                                    "reason":"continuation_limit"}));
                                            "Conclude this assigned work now. Record only confirmed \
                                             evidence, then commit the Step or report the current \
                                             platform verdict.".to_owned()
                                        } else {
                                            continuations += 1;
                                            campaign_continuation_prompt(&assignment,finalize_on_exit)
                                        };
                                        let prompt_id = format!("worker-prompt-{}",
                                            uuid::Uuid::new_v4().simple());
                                        let command = json!({"id":prompt_id,"type":"prompt",
                                            "message":text}).to_string();
                                        if handle.link.write_stdin(&container_wid,&command).await == Ok(true) {
                                            pending_prompt_ids.insert(prompt_id);
                                            if !boundary {
                                                emit_event(&state,&run_id,Some(&solver_id),"blackboard.delta",
                                                    json!({"kind":"worker_campaign_continued",
                                                        "worker":solver_id,"intent_id":intent_id,
                                                        "continuation":continuations}));
                                            }
                                            continue;
                                        }
                                        last_error = Some("Pi RPC continuation was not delivered".to_owned());
                                    }
                                    let _ = handle.link.signal(&container_wid,"TERM",None,None).await;
                                    break;
                                }
                            }
                        }
                        if let Some(ev) = run::container_exec::parse_container_cli_json_line(&frame.line) {
                            for text in ev.assistant_texts {
                                let text = redact_operator_secrets(&text,&secret_values);
                                emit_event(
                                    &state,
                                    &run_id,
                                    Some(&solver_id),
                                    "text.delta",
                                    json!({"text": format!("{text}\n"), "delta": format!("{text}\n")}),
                                );
                            }
                            for rtext in ev.reasoning_texts {
                                let rtext = redact_operator_secrets(&rtext,&secret_values);
                                emit_event(
                                    &state,
                                    &run_id,
                                    Some(&solver_id),
                                    "reasoning.delta",
                                    json!({"text": format!("{rtext}\n"), "delta": format!("{rtext}\n")}),
                                );
                            }
                            for (cid, cmd) in ev.commands_started {
                                let cmd = redact_operator_secrets(&cmd,&secret_values);
                                emit_event(
                                    &state,
                                    &run_id,
                                    Some(&solver_id),
                                    "tool.start",
                                    json!({
                                        "tool": cmd,
                                        "tool_call_id": cid,
                                        "call_id": cid,
                                    }),
                                );
                            }
                            for (cid, cmd, output, _exit) in ev.commands_completed {
                                let cmd = redact_operator_secrets(&cmd,&secret_values);
                                let output = redact_operator_secrets(&output,&secret_values);
                                let art_seq = state.runs.next_artifact_seq(&run_id);
                                let art_id = match store_artifact(&sdir, art_seq, &cmd, &output) {
                                    Ok(id) => id,
                                    Err(error) => {
                                        last_error = Some(format!("工具证据保存失败：{error}"));
                                        break 'worker;
                                    }
                                };
                                if !output.is_empty() {
                                    emit_event(
                                        &state,
                                        &run_id,
                                        Some(&solver_id),
                                        "terminal.output",
                                        json!({"text": format!("$ {cmd}\n{output}\n")}),
                                    );
                                }
                                let condensed: String = output.chars().take(1200).collect();
                                emit_event(
                                    &state,
                                    &run_id,
                                    Some(&solver_id),
                                    "tool.result",
                                    json!({
                                        "tool": cmd,
                                        "tool_call_id": cid,
                                        "call_id": cid,
                                        "artifact_id": art_id,
                                        "result": {
                                            "condensed": format!("[artifact_id={art_id}] {condensed}"),
                                        },
                                    }),
                                );
                                let (s_now, _) = drain_blackboard_ingress(
                                    &state,
                                    &run_id,
                                    &solver_id,
                                    &ingress_dir,
                                    &sdir,
                                );
                                if s_now {
                                    solved = true;
                                }
                            }

                            if let Some(err) = ev.error_message {
                                last_error = Some(redact_operator_secrets(&err,&secret_values));
                            }
                        } else if !frame.line.trim().is_empty() {
                            emit_event(
                                &state,
                                &run_id,
                                Some(&solver_id),
                                "terminal.output",
                                json!({"text": format!("{}\n",
                                    redact_operator_secrets(&frame.line,&secret_values))}),
                            );
                        }
                    }
                    "err" => {
                        if !frame.line.trim().is_empty() {
                            emit_event(
                                &state,
                                &run_id,
                                Some(&solver_id),
                                "terminal.output",
                                json!({"text": format!("{}\n",
                                    redact_operator_secrets(&frame.line,&secret_values))}),
                            );
                        }
                    }
                    "exit" => {
                        let (solved_now,finished_now)=drain_blackboard_ingress(&state,&run_id,&solver_id,&ingress_dir,&sdir);solved|=solved_now;
                        if !interactive_pi&&!solved&&!finished_now&&!dropped_by_decide&&!frame.oom&&!frame.output_limit&&!frame.disk_limit&&secret_values.is_empty()&&(!clock.concluding()||clock.awaiting_exit()){
                            if let Some(session)=cli_session.resumable(){
                                let left=match budget.remaining(&state,&run_id,&solver_id,"conclude"){Ok(left)=>left,Err(error)=>{last_error=Some(error);break;}};
                                if left>0.{
                                    let left=left.min(spec.profile["conclude_timeout"].as_f64().unwrap_or(120.));
                                    let checkpoint_prompt=run::worker_cli_checkpoint::prompt(&assignment);
                                    let invocation=run::worker_cli_checkpoint::resume(&spec.norm_engine,spec.model.as_deref(),spec.effort.as_deref(),&checkpoint_prompt,&cspec.env,session);
                                    let (argv,stdin)=match invocation{Ok(value)=>value,Err(error)=>{last_error=Some(error.message);break;}};
                                    let mut checkpoint=cspec.clone();checkpoint.argv=argv;checkpoint.stdin=stdin;checkpoint.timeout_sec=left.ceil() as i64;
                                    match start_container_worker(&state,&run_id,&solver_id,&handle.link,&checkpoint,&budget).await{Ok((wid,stream))=>{
                                        let prior=container_wid;container_wid=wid;rx=stream;clock.conclude();clock.soft_reminder_deadline=None;
                                        state.runs.handle(&state,&run_id).container_sessions.lock().unwrap().insert(solver_id.clone(),(handle.link.clone(),container_wid.clone()));
                                        usage_observer.checkpoint();
                                        emit_event(&state,&run_id,Some(&solver_id),"blackboard.delta",json!({"kind":"worker_checkpoint","worker":solver_id,"intent_id":intent_id,"session":session,"prior_container_worker":prior,"container_worker":container_wid,"status":"started","remaining_seconds":left}));
                                        clock.wait(left);
                                        clock.arm_checkpoint(run::control_freeze::worker_budget(&state,&run_id,&solver_id).0);continue;
                                    },Err(error)=>{last_error=Some(format!("Same-session checkpoint launch failed: {error}"));break;}}
                                }
                            }else{emit_event(&state,&run_id,Some(&solver_id),"blackboard.delta",json!({"kind":"worker_checkpoint","worker":solver_id,"intent_id":intent_id,"status":"skipped","reason":"no_established_native_session"}));}
                        }
                        if frame.oom {
                            oom_killed = true;
                        }
                        if frame.timed_out {
                            timed_out = true;
                        }
                        if frame.rc != 0 && !solved && last_error.is_none() {
                            last_error = Some(if !frame.error.is_empty() {
                                redact_operator_secrets(&frame.error,&secret_values)
                            } else {
                                format!("容器 Worker 退出 (rc={})", frame.rc)
                            });
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = handle.link.signal(&container_wid, "TERM", None, None).await;
    let (s_final, _) = drain_blackboard_ingress(&state, &run_id, &solver_id, &ingress_dir, &sdir);
    if s_final {
        solved = true;
    }

    state
        .runs
        .handle(&state, &run_id)
        .container_sessions
        .lock()
        .unwrap()
        .remove(&solver_id);
    state.runs.unregister_worker(&run_id, &solver_id);

    if let Ok(Some(stored)) = state.store.get("runs", &run_id)
        && stored["solved"] == true
    {
        solved = true;
    }

    let classified_exit = classify_worker_exit_reason(
        solved,
        dropped_by_decide,
        timed_out,
        oom_killed,
        last_error.as_deref(),
    );
    *exit_reason_slot.lock().unwrap() = Some(classified_exit.to_owned());

    let reason = if dropped_by_decide {
        "intent_dropped"
    } else if solved {
        "solved"
    } else if oom_killed {
        "oom"
    } else if timed_out {
        "timeout"
    } else if last_error.is_some() {
        "runtime_failure"
    } else {
        "finished"
    };

    if let Some(detail) = &last_error {
        emit_event(
            &state,
            &run_id,
            Some(&solver_id),
            "worker.error",
            json!({"reason":classified_exit,"detail":detail}),
        );
    }

    finish_worker_events(
        &state, &run_id, &spec, intent_id, &workdir, started_at, solved, reason,
    );

    if (finalize_on_exit || solved) && state.runs.active_worker_count(&run_id) == 0 {
        run::container_exec::teardown_run_container(&run_id).await;
        mark_run_finished(
            &state,
            &run_id,
            Some(&solver_id),
            solved,
            reason,
            last_error.as_deref(),
        );
    }
}

fn build_review_prompt(
    challenge: &Value,
    _blackboard_script: &Path,
    assignment: Option<&IntentAssignment>,
    board_snapshot: Option<&str>,
    reopen_hint: Option<&str>,
) -> String {
    let mut header = format!(
        "Challenge: {} [{}]",
        challenge["name"].as_str().unwrap_or(""),
        challenge["category"].as_str().unwrap_or("")
    );
    if let Some(target) = challenge["target"].as_str().filter(|s| !s.is_empty()) {
        header.push_str(&format!("\nTarget: {target}"));
    }
    let instruction = challenge["task_contract"]["raw_instruction"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| challenge["description"].as_str())
        .unwrap_or("");
    let mut context = format!("{header}\n\n## Goal\n{}", instruction.trim());
    if let Some(hint) = reopen_hint.filter(|s| !s.trim().is_empty()) {
        context.push_str(&format!(
            "\n\n## Operator standing guidance (applies to ALL your work):\n- {hint}\n"
        ));
    }
    let engagement = String::new();
    ctf_prompts::REVIEW
        .replace("{ctx}", &context)
        .replace("{engagement}", &engagement)
        .replace(
            "{intent_goal}",
            assignment
                .map(|a| a.goal.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("Audit the current swarm trajectory."),
        )
        .replace("{review_board}", board_snapshot.unwrap_or(""))
}
