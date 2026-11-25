//! Task-scoped SRC research, asset, connector and reporting records.
mod browser_runtime;
mod artifact_destroy;
mod asset_query;
mod asset_publication;
mod connector_config;
mod connector_file_import;
mod connector_source;
mod connector_query;
mod connectors_online;
mod coordination;
mod coverage;
mod credential_store;
mod egress;
mod exchange_queries;
mod external_events;
mod finding;
mod graphql_grpc;
mod guardrails;
mod handoff_archive;
mod identity;
mod import_formats;
mod ingestion;
mod inspection;
mod knowledge_queries;
mod local_parity;
mod monitor_runtime;
mod net_guard;
mod notify_gateway;
mod oast;
mod objects;
mod protocol_stream;
mod proxy_bridge;
mod proxypool;
mod publication;
mod race_probe;
mod report_export;
mod report_render;
mod report_rules;
mod request_mutator;
mod vuln_intel;
mod workspace_explorer;
mod research_business;
mod research_ingest;
mod research_observations;
mod research_page_action;
mod research_views;
mod research_workflow;
mod run_lifecycle;
mod run_destroy;
mod task_destroy;
mod task_lifecycle;
mod retest;
mod traffic_index;
mod worker_gateway;
mod workbench_records;
pub(super) fn monitor_health_error(s:&AppState,id:&str,error:&Error)->Api<()>{monitor_runtime::health_result(s,id,&json!([]),Some(json!({"code":error.code,"message":error.message})))}
pub(super) fn cleanup_destroyed_task_cas(s: &AppState, id: &str) -> Api<()> { task_destroy::cleanup_cas(s, id) }
pub(super) fn sync_inspection(s:&AppState,id:&str)->Api<Vec<String>>{inspection::sync_task(s,id)}
pub(super) fn src_thread_instructions(s:&AppState,tid:&str)->String{coordination::src_thread_instructions(s,tid)}
pub(super) fn sync_session_files_before_turn(s:&AppState,tid:&str,cwd:&std::path::Path){coordination::sync_session_files_before_turn(s,tid,cwd)}
pub(super) fn sync_session_files_after_turn(s:&AppState,tid:&str,cwd:&std::path::Path){coordination::sync_session_files_after_turn(s,tid,cwd)}
pub(super) fn enqueue_run_milestone(s:&AppState,id:&str,run_id:&str,seq:u64,record_kind:&str,record_id:&str,data:&Value,candidate_record_id:Option<&str>){coordination::enqueue_run_milestone(s,id,run_id,seq,record_kind,record_id,data,candidate_record_id)}
pub(super) async fn coordination_wake(s:std::sync::Arc<AppState>,id:&str)->Api<Value>{coordination::reconcile_and_wake(s,id).await}
pub(super) fn coordination_policy_get(s:&AppState,id:&str)->Api<Value>{coordination::policy_get(s,id)}
pub(super) fn record_duplicate_run_prevented(s:&AppState,id:&str){coordination::record_duplicate_run_prevented(s,id)}
pub(super) async fn apply_login_recipe_replay(s:std::sync::Arc<AppState>,id:&str,body:&Value)->Api<Value>{identity::replay_login_recipe(s,id,body).await}
pub(super) async fn apply_session_probe(s:std::sync::Arc<AppState>,id:&str,body:&Value)->Api<Value>{identity::probe_session(s,id,body).await}
pub(super) async fn apply_proxy_bridge_start(s:std::sync::Arc<AppState>,id:&str,body:&Value)->Api<Value>{proxy_bridge::start(s,id,body).await}
pub(super) async fn apply_mutator_test(s:std::sync::Arc<AppState>,id:&str,body:&Value)->Api<Value>{request_mutator::test_run(&s,id,body).await}
pub(super) async fn apply_race_probe(s:std::sync::Arc<AppState>,id:&str,body:&Value,command_id:Option<&str>)->Api<Value>{race_probe::race_probe(s,id,body,command_id).await}
pub(super) async fn apply_notify_test(s:std::sync::Arc<AppState>,id:&str,body:&Value)->Api<Value>{notify_gateway::test_channel(&s,id,body).await}
pub(super) async fn apply_notify_flush_carryover(s:std::sync::Arc<AppState>,id:&str,body:&Value)->Api<Value>{notify_gateway::flush_carryover(&s,id,body).await}
pub(super) async fn apply_run_intervene(s:std::sync::Arc<AppState>,id:&str,body:&Value)->Api<Value>{coordination::run_intervene(s,id,body).await}
pub(super) fn guardrails_banner(s: &AppState, task_id: &str) -> Api<String> {
    guardrails::platform_policy_banner(s, task_id)
}
pub(super) fn guardrails_refusal(
    s: &AppState,
    task_id: &str,
    chain_key: &str,
    stop_reason: &str,
    model_id: &str,
) -> Api<(bool, Option<String>)> {
    guardrails::record_refusal_and_maybe_clarify(s, task_id, chain_key, stop_reason, model_id)
}
pub(super) fn guardrails_run_coverage(
    s: &AppState,
    task_id: &str,
    run_id: &str,
) -> Api<(bool, Value)> {
    guardrails::run_coverage_gate_met(s, task_id, run_id)
}
pub(super) fn policy_compare(name:&str,actual:&Value,operator:&str,expected:&Value)->Api<bool>{egress::scope_compare(name,actual,operator,expected)}
pub(super) fn policy_raw_path(raw:&str)->String{raw_endpoint_path(raw)}
pub(super) fn recover_retests(s:&AppState,id:&str)->Api<Value>{retest::recover(s,id)}

pub(super) fn atomic_records(s:&AppState,id:&str,updates:&[(&str,&str,Value,Option<u64>)])->Api<bool>{
    let mut staged=Vec::new();
    for(kind,key,payload,revision)in updates{
        let(storage,version,row)=stage_rec(s,id,kind,key,payload.clone(),*revision)?;
        let before=s.store.get(RECORDS,&storage).map_err(crate::runs::src_api::db)?;
        if before.as_ref().and_then(|row|row["version"].as_u64()).unwrap_or(0)!=version{return Ok(false);}
        staged.push((storage,before,json!({"version":version+1,"values":row})));
    }
    let expected=staged.iter().map(|(key,before,_)|(RECORDS,key.as_str(),before.as_ref())).collect::<Vec<_>>();
    let records=staged.iter().map(|(key,_,after)|(RECORDS,key.as_str(),after)).collect::<Vec<_>>();
    s.store.put_many_if(&expected,&records).map_err(crate::runs::src_api::db)
}
pub(super) fn recover_browser(s:&AppState,id:&str)->Api<Value>{browser_runtime::recover(s,id)}

const ASSET_SOURCE_EVENTS: &str = "src_asset_observation_event_keys";
pub(super) const EXCHANGE_APPLIED: &str = "src_exchange_command_applied";
pub(super) const CONNECTOR_IMPORT_APPLIED: &str = connector_file_import::APPLIED;
pub(super) const ASSET_QUERY_APPLIED: &str = asset_query::APPLIED;
pub(super) const WORKBENCH_RECORDS_APPLIED: &str = workbench_records::APPLIED;
const RESEARCH_PLAN_HEADS: &str = "src_research_plan_heads";
const RESEARCH_AUTH_HEADS: &str = "src_research_authorization_heads";
const RESEARCH_SKILL_HEADS: &str = "src_research_skill_heads";
const RESEARCH_BUSINESS_MODEL_HEADS: &str = "src_research_business_model_heads";
const RESEARCH_BUSINESS_CONTEXT_HEADS: &str = "src_research_business_context_heads";
const RESEARCH_QUESTION_HEADS: &str = "src_research_question_heads";
use super::{
    ASSET_DERIVE_APPLIED, ASSET_LIFECYCLE_APPLIED, ASSET_OBSERVE_APPLIED, ASSET_RELATE_APPLIED,
    ASSET_RESTRUCTURE_APPLIED, Api, Error, FINDING_APPLIED, RECORDS, REPORT_APPLIED, REPORT_HEADS,
    RESEARCH_AUTH_APPLIED, RESEARCH_BUSINESS_APPLIED, RESEARCH_OBSERVATION_APPLIED,
    RESEARCH_PLAN_APPLIED, RESEARCH_SKILL_APPLIED, RESEARCH_WORKFLOW_APPLIED, advance_source, db,
    in_policy, now, parse_url, policy, rec, rec_key, records, records_with_rowid, require, save_rec, stage_rec,
    task, ARTIFACT_DESTROY_APPLIED, RUN_DESTROY_APPLIED, RUN_LIFECYCLE_APPLIED, TASKS, TASK_DESTROY_APPLIED, TASK_LIFECYCLE_APPLIED,
};
use crate::AppState;
use axum::http::StatusCode;
use base64::Engine;
use sha2::{Digest, Sha256};
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use uuid::Uuid;

async fn runner_environment_request(id:&str,operation:&str,body:&Value)->Option<Api<Value>>{
    let socket=crate::runner::configured_socket()?;
    Some(crate::runner::exchange(&socket,"/v1/src/environment",Some(json!({"task_id":id,"operation":operation,"body":body})))
        .await.map_err(|message|Error::new(StatusCode::SERVICE_UNAVAILABLE,"runner.operation_failed",message)))
}
pub(super) fn worker_gateway_status(s: &AppState, id: &str) -> Api<Value> {
    worker_gateway::status(s, id)
}
pub(super) fn apply_coverage_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
) -> Api<Value> {
    coverage::apply_command(s, id, kind, body, command_id)
}
pub(super) fn apply_asset_publication_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    asset_publication::apply_command(s, id, kind, body, command_id, actor_id)
}
pub(super) fn apply_asset_query_command(
    s: &AppState, id: &str, kind: &str, body: &Value, command_id: &str, actor_id: &str,
) -> Api<Value> {
    asset_query::apply_command(s, id, kind, body, command_id, actor_id)
}
pub(super) fn apply_workbench_record_command(
    s: &AppState, id: &str, kind: &str, body: &Value, command_id: &str, actor_id: &str,
) -> Api<Value> {
    workbench_records::apply_command(s, id, kind, body, command_id, actor_id)
}
pub(super) fn workbench_record_query(s: &AppState, id: &str, kind: &str, body: &Value) -> Api<Value> {
    workbench_records::query(s, id, kind, body)
}
pub(super) const ASSET_PUBLICATION_APPLIED: &str = asset_publication::APPLIED;
pub(super) async fn apply_task_lifecycle_command(
    s: std::sync::Arc<AppState>,
    id: &str,
    kind: &str,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    task_lifecycle::apply(s, id, kind, command_id, actor_id).await
}
pub(super) async fn apply_artifact_destroy_command(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    artifact_destroy::apply(s, id, body, command_id, actor_id).await
}
pub(super) async fn apply_run_lifecycle_command(
    s: std::sync::Arc<AppState>,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    run_lifecycle::apply(s, id, kind, body, command_id, actor_id).await
}
pub(super) async fn apply_run_destroy_command(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    run_destroy::apply(s, id, body, command_id, actor_id).await
}
pub(super) async fn apply_task_destroy_command(
    s: std::sync::Arc<AppState>,
    id: &str,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    task_destroy::apply(s, id, command_id, actor_id).await
}
pub(super) fn apply_exchange_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
) -> Api<Value> {
    if let Some(marker) = s
        .store
        .get(EXCHANGE_APPLIED, command_id)
        .map_err(super::db)?
    {
        if kind == "src.exchange.record" {
            advance_exchange_source(
                s,
                id,
                marker["output"]["exchange_id"].as_str().unwrap_or(""),
            )?;
        }
        return Ok(marker["output"].clone());
    }
    match kind {
        "src.exchange.record" => record_exchange_inner(s, id, body, Some(command_id)),
        "src.exchange.draft.save" => save_exchange_draft_inner(s, id, body, Some(command_id)),
        _ => Err(Error::invalid("unsupported SRC exchange command")),
    }
}
pub(super) fn apply_connector_import_command(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: &str,
) -> Api<Value> {
    let mut batch = connector_file_import::import_marked(s, id, body, command_id)?;
    if batch["external_state"] != "failed"
        && batch["items"].as_array().is_some_and(|v| !v.is_empty())
        && batch["observations_ingested"] != true
        && let Ok(ingested) = ingestion::ingest_connector_batch_observations(s, id, &batch, Some(command_id))
    {
        batch["counts"]["new"] = ingested["counts"]["created_assets"].clone();
        batch["counts"]["merged"] = ingested["counts"]["merged_assets"].clone();
        batch["observation_counts"] = ingested["counts"].clone();
        batch["observations_ingested"] = json!(true);
        if let Some(bid) = batch["batch_id"].as_str()
            && let Ok(row) = rec(s, id, "connector_batch", bid)
        {
            let _ = save_rec(s, id, "connector_batch", bid, batch.clone(), row["revision"].as_u64());
        }
    }
    Ok(batch)
}
pub(super) const CONNECTOR_CONFIG_APPLIED: &str = connector_config::APPLIED;
pub(super) const CONNECTOR_GRANT_APPLIED: &str = connector_config::GRANT_APPLIED;
pub(super) const ASSET_JUDGEMENT_APPLIED: &str = "src_asset_judgement_applied_commands";
pub(super) fn apply_connector_config_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    connector_config::apply_command(s, id, kind, body, command_id, actor_id)
}
pub(super) async fn verify_connector(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    connector_config::verify(s, id, body).await
}
pub(super) fn apply_asset_judgement_command(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    save_asset_judgement(s, id, body, Some(command_id), actor_id)
}
pub(super) fn browser_verified_status(s: &AppState, id: &str) -> Api<Option<Value>> {
    browser_runtime::verified_service(s, id)
}

pub(super) async fn ensure_worker_gateway(s: &AppState, id: &str) -> Api<Value> {
    if let Some(result)=runner_environment_request(id,"worker_gateway.ensure",&json!({})).await{return result;}
    worker_gateway::ensure(s, id).await
}
pub(super) fn revoke_inspection_grant(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    inspection::revoke_grant(s, id, body)
}
pub(super) fn revoke_inspection_grant_marked(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: &str,
    marker_collection: &str,
) -> Api<Value> {
    inspection::revoke_grant_marked(s, id, body, command_id, marker_collection)
}
pub(super) fn revoke_incompatible_inspection_grants(
    s: &AppState,
    id: &str,
    digest: &Value,
) -> Api<()> {
    inspection::revoke_incompatible_grants(s, id, digest)
}

/// Saved, integrity-checked input for the scoped HTTP egress gateway.
pub(crate) struct PreparedExchange {
    pub source_exchange_id: String,
    pub url: String,
    pub method: String,
    pub raw_request: Vec<u8>,
    pub raw_request_artifact_id: String,
    pub policy_digest: String,
}

pub(super) fn prepare_exchange_dispatch(
    s: &AppState,
    id: &str,
    draft_id: &str,
    revision: u64,
    policy_version: u64,
) -> Result<PreparedExchange, String> {
    let t = task(s, id).map_err(|e| e.message)?;
    if t["lifecycle"]["state"] != "active" {
        return Err("SRC Task is not active".to_owned());
    }
    let p = policy(s, id).map_err(|e| e.message)?;
    if p["version"] != policy_version {
        return Err("SRC authorization version is stale".to_owned());
    }
    let row = rec(s, id, "request_draft", draft_id).map_err(|e| e.message)?;
    let draft = &row["payload"];
    if draft["revision"] != revision {
        return Err("SRC draft revision is stale".to_owned());
    }
    let target = draft["target_url"]
        .as_str()
        .ok_or("SRC draft target missing")?;
    if !in_policy(&p, target)
        || !t["input"]["authorization"]["targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| {
                    v.as_str()
                        .is_some_and(|allowed| super::target_matches(target, allowed))
                })
            })
    {
        return Err("SRC target is outside current task authorization".to_owned());
    }
    let ref_data = &draft["raw_request_ref"];
    let raw = verify_ref(s, id, ref_data).map_err(|e| e.message)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw["data"].as_str().unwrap_or(""))
        .map_err(|_| "SRC draft artifact is invalid".to_owned())?;
    let scheme = parse_url(target)
        .map_err(|e| e.message)?
        .scheme()
        .to_owned();
    let (method, url, _) = parse_raw_request(&bytes, &scheme).map_err(|e| e.message)?;
    if url != target || draft["method"] != method {
        return Err("SRC draft raw request differs from saved target".to_owned());
    }
    let source = draft["source_exchange_id"]
        .as_str()
        .ok_or("SRC source exchange missing")?;
    rec(s,id,"http_exchange",source).or_else(|error|if error.status==StatusCode::NOT_FOUND{rec(s,id,"import_exchange",source)}else{Err(error)}).map_err(|e|e.message)?;
    Ok(PreparedExchange {
        source_exchange_id: source.to_owned(),
        url,
        method,
        raw_request: bytes,
        raw_request_artifact_id: raw["artifact_id"].as_str().unwrap_or("").to_owned(),
        policy_digest: p["digest"].as_str().unwrap_or("").to_owned(),
    })
}

pub(super) fn command(s: &AppState, id: &str, kind: &str, body: &Value) -> Api<Value> {
    s.store.document_scope(||command_inner(s,id,kind,body)).map_err(db)?
}
pub(super) fn command_inner(s: &AppState, id: &str, kind: &str, body: &Value) -> Api<Value> {
    let task=task(s,id)?;
    if super::domain_command::workbench_record_command(kind)||super::domain_command::asset_query_command(kind){
        let key=body["idempotency_key"].as_str().map(str::to_owned).unwrap_or_else(||Uuid::new_v4().to_string());let command_id=format!("{id}:{kind}:{key}");let actor=task["binding"]["principal_id"].as_str().unwrap_or("local-operator");
        return if super::domain_command::workbench_record_command(kind){workbench_records::apply_command(s,id,kind,body,&command_id,actor)}else{asset_query::apply_command(s,id,kind,body,&command_id,actor)};
    }
    match kind {
        "src.exchange.record" => record_exchange(s, id, body),
        "src.exchange.draft.save" => save_exchange_draft(s, id, body),
        "src.exchange.draft.execute" => Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability.unavailable",
            "SRC 请求回放需要已验证的出口与审批网关",
        )),
        "src.asset.observe" => observe_asset(s, id, body, None),
        "src.asset.correct" => correct_asset(
            s,
            id,
            body,
            None,
            body["actor_id"].as_str().unwrap_or("local-operator"),
        ),
        "src.asset.lifecycle" => asset_lifecycle(s, id, body, None, "local-operator"),
        "src.asset.merge" => asset_merge(s, id, body, None, "local-operator"),
        "src.asset.merge.revert" => asset_merge_revert(s, id, body, None, "local-operator"),
        "src.asset.split" => split_asset(s, id, body, None, "local-operator"),
        "src.asset.relate" => asset_relate(s, id, body, None),
        "src.asset.derive" => derive_asset_relations(s, id, body, None),
        "src.asset.destroy" => asset_destroy(s, id, body, None, "local-operator", None),
        "src.knowledge.dataset.create" => create_dataset(s, id, body),
        "src.knowledge.publish" => knowledge_publish(s, id, body),
        "src.knowledge.revoke" => revoke_publication(s, id, body),
        "src.knowledge.invalidate" => invalidate_knowledge(s, id, body),
        "src.research.plan.save" => save_research_plan(s, id, body, None, "local-operator"),
        "src.research.authorization.save" => {
            save_research_authorization(s, id, body, None, "local-operator")
        }
        "src.research.business.derive" => research_business::derive(s, id, body, None),
        "src.research.openapi.ingest" => ingest_research_openapi(s, id, body, None),
        "src.research.javascript.ingest" => ingest_research_javascript(s, id, body, None),
        "src.research.javascript.sourcemap_unpack" => {
            research_ingest::unpack_sourcemap(s, id, body)
        }
        "src.research.graphql_grpc.ingest" => graphql_grpc::ingest(s, id, body),
        "src.research.skill.register" => save_research_skill(s, id, body, None, "local-operator"),
        "src.research.permission_question.add" => {
            research_workflow::question(s, id, body, None, "local-user", "operator")
        }
        "src.research.business.save" => research_business::save(s, id, body, None),
        "src.research.experiment.record" => research_workflow::experiment(s, id, body, None),
        "src.research.capability_chain.record" => {
            research_workflow::capability_chain(s, id, body, None)
        }
        "src.research.precondition_trial.record" => {
            research_workflow::precondition_trial(s, id, body, None)
        }
        "src.coverage.item.save" => coverage::save_item(s, id, body),
        "src.coverage.item.claim" => coverage::claim(s, id, body),
        "src.coverage.attempt.record" => coverage::record_attempt(s, id, body),
        "src.coverage.blocker.record" => coverage::record_blocker(s, id, body),
        "src.coverage.seed" => coverage::seed(s, id, body),
        "src.coordination.policy.save" => coordination::policy_save(s, id, body),
        "src.coordination.decide" => coordination::decide(s, id, body),
        "src.coordination.notice.ack" => coordination::ack_notice(s, id, body),
        "src.object.lease.acquire" => objects::acquire(s, id, body),
        "src.object.lease.release" => objects::release(s, id, body),
        "src.object.register" => objects::register(s, id, body),
        "src.object.experiment.begin" => objects::begin(s, id, body),
        "src.object.experiment.after" => objects::after(s, id, body),
        "src.object.experiment.restore" => objects::restore(s, id, body),
        "src.oast.poll" => oast::poll(s, id, body),
        "src.oast.expire" => oast::expire(s, id, body),
        "src.oast.health" => oast::health(s, id, body),
        "src.oast.token.mint" => oast::token_mint(s, id, body),
        "src.oast.callback.ingest" => oast::callback_ingest(s, id, body),
        "src.identity.ensure" => identity::ensure(s, id, body),
        "src.identity.verify" => identity::verify(s, id, body),
        "src.identity.recover" => identity::recover(s, id, body),
        "src.identity.login_recipe.save" => identity::save_login_recipe(s, id, body),
        "src.stream.websocket.acknowledge" => protocol_stream::acknowledge(s, id, body),
        "src.inspection.decide" => inspection::decide(s, id, body),
        "src.inspection.grant.revoke" => inspection::revoke_grant(s, id, body),
        "src.browser.tab.release" => browser_runtime::release_tab(s, id, body),
        "src.browser.image.review" => browser_runtime::review_image(s, id, body),
        "src.browser.execution.record" => browser_runtime::execution_record(s, id, body),
        "src.browser.run.finish" => browser_runtime::run_finished(s, id, body),
        "src.research.runtime.register" => {
            research_observations::register_runtime(s, id, body, None)
        }
        "src.research.dataflow.register" => {
            research_observations::register_dataflow(s, id, body, None)
        }
        "src.research.page_action.register" => research_page_action::register(s, id, body, None),
        "src.finding.claim.create"
        | "src.finding.review.record"
        | "src.review.record"
        | "src.finding.invalidate"
        | "src.finding.deduplicate" => finding::apply(s, id, kind, body, None),
        "src.report.freeze" => freeze_report(s, id, body, None),
        "src.report.render" => render_report(s, id, body),
        "src.submission.export" => export_report(s, id, body),
        "src.report.reproduction.record" => record_reproduction(s, id, body, None),
        "src.platform.status.record" | "src.feedback.platform_status.record" => {
            record_status(s, id, body)
        }
        "src.retest.request" => retest::request(s, id, body),
        "src.retest.complete" => retest::complete(s, id, body),
        "src.connector.config.save" => save_connector_config(s, id, body),
        "src.connector.config.revoke" => revoke_connector_config(s, id, body),
        "src.connector.grant.save" | "src.connector.grant.revoke" => {
            connector_config::apply_command(
                s,
                id,
                kind,
                body,
                &format!("{id}:{kind}:{}", Uuid::new_v4().simple()),
                body["actor_id"].as_str().unwrap_or("local-operator"),
            )
        }
        "src.asset.judgement.save" => save_asset_judgement(
            s,
            id,
            body,
            None,
            body["actor_id"].as_str().unwrap_or("local-operator"),
        ),
        "src.credential.put" => credential_store::put(s, id, body),
        "src.credential.revoke" => credential_store::revoke(s, id, body),
        "src.monitor.create" => create_monitor(s, id, body),
        "src.monitor.revise" => revise_monitor(s, id, body),
        "src.monitor.state" => monitor_state(s, id, body),
        "src.monitor.candidate.handle" => monitor_runtime::handle_candidate(s, id, body),
        "src.monitor.external.reconcile" => monitor_runtime::reconcile_external(s, id, body),
        "src.ingestion.ingest" => ingest(s, id, body),
        "src.ingestion.external_event" => external_events::ingestion_event(s, id, body),
        "src.ingestion.reconcile" => external_events::reconcile_ingestion(s, id, body),
        "src.ingestion.batch.revert" => revert_ingestion(s, id, body),
        "src.ingestion.retry" => retry_ingestion(s, id, body),
        "src.identity.session.import" => identity::import_session(s, id, body),
        "src.proxy_bridge.stop" => proxy_bridge::stop(s, id),
        "src.proxy_bridge.bookmark" => proxy_bridge::bookmark(s, id, body),
        "src.proxypool.save" => proxypool::save(s, id, body),
        "src.proxypool.reset_circuit" => proxypool::reset_circuit(s, id, body),
        "src.mutator.save" => request_mutator::save(s, id, body),
        "src.intel.advisory.import" => vuln_intel::advisory_import(s, id, body),
        "src.intel.match_run" => vuln_intel::match_run(s, id, body),
        "src.workbench.attachment.autobind" => workspace_explorer::attachment_autobind(s, id, body),
        "src.notify.channel.save" => notify_gateway::save_channel(s, id, body),
        "src.archive.export" => handoff_archive::export_archive(s, id, body),
        "src.archive.import" => handoff_archive::import_archive(s, id, body),
        "src.diagnostics.mcp_reset" => traffic_index::mcp_reset(s, body),
        "src.connector.import" => {
            let mut batch = connector_file_import::import(s, id, body)?;
            if batch["external_state"] != "failed"
                && batch["items"].as_array().is_some_and(|v| !v.is_empty())
                && batch["observations_ingested"] != true
                && let Ok(ingested) = ingestion::ingest_connector_batch_observations(s, id, &batch, None)
            {
                batch["counts"]["new"] = ingested["counts"]["created_assets"].clone();
                batch["counts"]["merged"] = ingested["counts"]["merged_assets"].clone();
                batch["observation_counts"] = ingested["counts"].clone();
                batch["observations_ingested"] = json!(true);
                if let Some(bid) = batch["batch_id"].as_str()
                    && let Ok(row) = rec(s, id, "connector_batch", bid)
                {
                    let _ = save_rec(s, id, "connector_batch", bid, batch.clone(), row["revision"].as_u64());
                }
            }
            Ok(batch)
        }
        "src.connector.external_event" => external_events::connector_event(s, id, body),
        _ => Err(Error::new(
            StatusCode::NOT_IMPLEMENTED,
            "capability.unsupported",
            format!("SRC 命令 `{kind}` 尚未接入"),
        )),
    }
}
pub(in crate::runs::src_api::domains) fn create_next_draft(
    s: &AppState,
    id: &str,
    source_exchange_id: &str,
    raw_request_b64: Option<&str>,
    changes: Value,
    use_original_capture: bool,
) -> Api<Value> {
    s.store
        .document_scope(|| {
            let ex = rec(s, id, "http_exchange", source_exchange_id)?;
            let current = records(s, id, "request_draft")?
                .into_iter()
                .filter(|v| v["payload"]["source_exchange_id"] == source_exchange_id)
                .filter_map(|v| v["payload"]["revision"].as_u64())
                .max()
                .unwrap_or_else(|| ex["revision"].as_u64().unwrap_or(1));
            let mut body = json!({
                "source_exchange_id": source_exchange_id,
                "base_revision": current,
                "changes": changes,
                "use_original_capture":use_original_capture,
            });
            if let Some(b64) = raw_request_b64 {
                body["raw_request_b64"] = json!(b64);
            }
            save_exchange_draft_commit(s, id, &body, None)
        })
        .map_err(db)?
}
pub(super) async fn collect_javascript(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    research_ingest::collect_javascript(s, id, body).await
}
pub(super) async fn differential_probe(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    exchange_queries::differential_probe(s, id, body).await
}
pub(super) async fn auto_reproduce_finding(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    let finding_id = require(body, "finding_id")?;
    let exchange_ids = body["exchange_ids"]
        .as_array()
        .filter(|rows| !rows.is_empty())
        .ok_or_else(|| Error::invalid("exchange_ids must be a non-empty array"))?;
    let reviewer = body["reviewer"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("src.independent-reproducer");
    let original = body["original_researcher"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("src.explorer");
    let target_version = body["target_version"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("live-current");
    let expected_status = body["expected_status"].as_u64();
    let current_policy = policy(&s, id)?;
    let auth_ver = current_policy["version"].as_u64().unwrap_or(1);

    let mut executed_steps = Vec::new();
    let mut receipt_ids = Vec::new();
    let mut evidence_refs = Vec::new();
    let mut blockers = Vec::new();
    let mut identity_refs = Vec::new();
    let mut replay_completed = true;
    let mut replay_observations = Vec::new();

    for (idx, eid_val) in exchange_ids.iter().enumerate() {
        let Some(src_eid) = eid_val.as_str().filter(|v| !v.is_empty()) else {
            continue;
        };
        let src_ex = rec(&s, id, "http_exchange", src_eid)?["payload"].clone();
        let ctx_id = src_ex["context_id"].as_str().unwrap_or("default").to_owned();
        if !identity_refs.iter().any(|x| x == &json!(ctx_id)) {
            identity_refs.push(json!(ctx_id));
        }
        let draft_out = create_next_draft(
            &s, id, src_eid, None,
            json!({"independent_reproduction_step":idx + 1,"source_request_ref":src_ex["request_raw_ref"]}),
            true,
        )?;
        let draft_id = draft_out["draft"]["draft_id"].as_str().unwrap_or("");
        let draft_rev = draft_out["draft"]["revision"].as_u64().unwrap_or(1);
        let idem = format!("autorepro-{finding_id}-{idx}-{}", Uuid::new_v4().simple());
        match execute_draft(
            s.clone(),
            id,
            &json!({
                "draft_id": draft_id,
                "draft_revision": draft_rev,
                "authorization_version": auth_ver,
                "context_id": ctx_id,
                "identity_ref": ctx_id,
                "actor_ref": reviewer,
                "idempotency_key": idem,
            }),
        )
        .await
        {
            Ok(exec_receipt) => {
                let out = &exec_receipt["output"];
                let completed = out["execution_state"] == "completed"
                    && out["receipt"]["outcome_known"] == true;
                if completed {
                    if let Some(inv_id) = out["execution_id"].as_str().filter(|v| !v.is_empty()) {
                        receipt_ids.push(json!(inv_id));
                    }
                }
                let replayed = &out["exchange"];
                let status_code = replayed["status_code"].as_u64();
                replay_observations.push(json!({
                    "execution_id": out["execution_id"],
                    "execution_state": out["execution_state"],
                    "status_code": status_code,
                    "expected_status": expected_status,
                    "status_matches_expectation": expected_status.map(|expected| status_code == Some(expected)),
                }));
                if replayed["request_raw_ref"].is_object() {
                    evidence_refs.push(replayed["request_raw_ref"].clone());
                }
                if replayed["response_raw_ref"].is_object() {
                    evidence_refs.push(replayed["response_raw_ref"].clone());
                }
                executed_steps.push(json!(format!(
                    "Step {}: replay {} {} as `{}` -> HTTP {}",
                    idx + 1,
                    replayed["method"].as_str().unwrap_or("GET"),
                    replayed["observed_url"].as_str().unwrap_or(""),
                    ctx_id,
                    status_code.map(|c| c.to_string()).unwrap_or_else(|| "none".to_owned())
                )));
                if !completed {
                    replay_completed = false;
                    blockers.push(json!(format!("step {}: replay execution is {}", idx + 1, out["execution_state"])));
                }
            }
            Err(err) => {
                replay_completed = false;
                blockers.push(json!(format!("step {}: {} ({})", idx + 1, err.message, err.code)));
            }
        }
    }

    let replay_completed = replay_completed && !executed_steps.is_empty()
        && !receipt_ids.is_empty() && !evidence_refs.is_empty();
    // A transport receipt (even HTTP 200) does not establish finding impact.
    // Leave confirmation to the independent researcher's evidence assessment.
    blockers.push(json!("Finding impact has not been assessed. Review the replay evidence and explicitly record an independent reproduction verdict."));
    record_reproduction(
        &s,
        id,
        &json!({
            "report_id": body["report_id"],
            "finding_id": finding_id,
            "reviewer": reviewer,
            "original_researcher": original,
            "executed_steps": executed_steps,
            "skipped_steps": [],
            "success": false,
            "replay": {"completed": replay_completed, "observations": replay_observations,
                "confirmation_required": true},
            "evidence_refs": evidence_refs,
            "execution_receipt_ids": receipt_ids,
            "identity_refs": identity_refs,
            "target_version": target_version,
            "blockers": blockers,
            "execution": {
                "command": "src.exchange.draft.execute (independent live replay)",
                "exit_code": if replay_completed { 0 } else { 1 }
            }
        }),
        None,
    )
}
pub(super) async fn execute_draft(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    egress::execute_draft(s, id, body).await
}
pub(super) async fn after_approval(
    s: std::sync::Arc<AppState>,
    id: &str,
    approval: &Value,
) -> Result<(), String> {
    if approval["request_snapshot"]["adapter_id"].is_string()||approval["request_snapshot"]["tool_name"]=="src.http.transparent_proxy"{return Ok(());}
    if matches!(
        approval["request_snapshot"]["tool_name"].as_str(),
        Some("src_websocket_connect" | "src_sse_connect")
    ) {
        return protocol_stream::after_approval(s, id, approval).await;
    }
    egress::after_approval(s, id, approval).await
}
pub(super) fn recover_http(s:&AppState,id:&str)->Api<()>{egress::recover(s,id)}
pub(super) async fn recover_oast_receiver(s:std::sync::Arc<AppState>)->Api<()>{oast::recover_local_receiver(s).await}
pub(super) async fn oast_command(s:std::sync::Arc<AppState>,id:&str,kind:&str,body:&Value)->Api<Value>{match kind{"src.oast.allocate"=>oast::allocate(s,id,body).await,"src.oast.local.start"=>oast::local_start(s,id,body).await,_=>Err(Error::invalid("Unknown asynchronous OAST operation"))}}
pub(super) async fn stream_command(
    s: std::sync::Arc<AppState>,
    id: &str,
    kind: &str,
    body: &Value,
) -> Api<Value> {
    match kind {
        "src.stream.websocket.connect" => protocol_stream::connect(s, id, "websocket", body).await,
        "src.stream.websocket.send" => protocol_stream::send_ws(s, id, body).await,
        "src.stream.websocket.receive" => protocol_stream::receive_ws(s, id, body).await,
        "src.stream.websocket.close" => protocol_stream::close(s, id, body, "websocket").await,
        "src.stream.sse.connect" => protocol_stream::connect(s, id, "sse", body).await,
        "src.stream.sse.read" => protocol_stream::read_sse(s, id, body).await,
        "src.stream.sse.close" => protocol_stream::close(s, id, body, "sse").await,
        _ => Err(Error::invalid("unknown stream command")),
    }
}
pub(super) async fn dispatch_retest(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    retest::dispatch(s, id, body).await
}
pub(super) async fn repository(s: std::sync::Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    connectors_online::repository(s, id, body).await
}
pub(super) async fn query_connector(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    connector_query::query(s, id, body).await
}
pub(super) async fn monitor_cycle(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    monitor_runtime::run_cycle(s, id, body).await
}
pub(super) async fn monitor_once(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    monitor_runtime::run_once(s, id, body).await
}
#[cfg(test)]
pub(super) fn monitor_preflight(s: &AppState, id: &str, definition: &Value) -> Api<Value> {
    monitor_runtime::preflight(s, id, definition)
}
pub(super) async fn monitor_wake(s: std::sync::Arc<AppState>, id: &str) -> Api<Value> {
    monitor_runtime::wake(s, id).await
}
pub(super) async fn recover_monitors(s: std::sync::Arc<AppState>, id: &str) -> Api<Value> {
    monitor_runtime::recover(s, id).await
}
pub(super) async fn deliver_monitor_notifications(
    s: std::sync::Arc<AppState>,
    id: &str,
) -> Api<Value> {
    monitor_runtime::deliver_notifications(s, id).await
}
pub(super) async fn quiesce_streams(
    s: std::sync::Arc<AppState>,
    id: &str,
    reason: &str,
) -> Api<()> {
    protocol_stream::quiesce(s, id, reason).await
}
pub(super) async fn recover_streams(s: std::sync::Arc<AppState>, id: &str) -> Api<()> {
    protocol_stream::recover(s, id).await
}
pub(super) async fn browser_command(
    s: std::sync::Arc<AppState>,
    id: &str,
    kind: &str,
    body: &Value,
) -> Api<Value> {
    if let Some(result)=runner_environment_request(id,kind,body).await{return result;}
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let output=match kind {
        "src.browser.ensure" => browser_runtime::ensure(s.clone(), id).await,
        "src.browser.context.ensure" => {
            browser_runtime::ensure_context(s.clone(), id, body["context_id"].as_str().unwrap_or("default"))
                .await
        }
        "src.browser.tab.open" => browser_runtime::open_tab(s.clone(), id, body).await,
        "src.browser.screenshot.capture" => browser_runtime::screenshot(s.clone(), id, body).await,
        "src.browser.task.close" => browser_runtime::close_task(s.clone(), id).await,
        _ => Err(Error::invalid("unsupported asynchronous browser command")),
    }?;
    match kind{
        "src.browser.ensure"|"src.browser.task.close"=>rec(&s,id,"browser_service","task"),
        "src.browser.context.ensure"=>rec(&s,id,"browser_context",require(&output,"context_id")?),
        "src.browser.tab.open"=>rec(&s,id,"browser_tab",require(&output,"tab_id")?),
        "src.browser.screenshot.capture"=>rec(&s,id,"browser_screenshot",require(&output,"screenshot_id")?),
        _=>Ok(output)
    }
}

pub(super) fn oast_callback(
    s: &AppState,
    token: &str,
    method: &str,
    path: &str,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Api<()> {
    oast::callback(s, token, method, path, headers, body)
}
pub(super) async fn quiesce_browser(s: std::sync::Arc<AppState>, id: &str) -> Api<()> {
    if let Some(result)=runner_environment_request(id,"browser.quiesce",&json!({})).await{return result.map(|_|());}
    browser_runtime::quiesce(s, id).await
}

pub(super) fn query(s: &AppState, id: &str, kind: &str, p: &Value) -> Api<Value> {
    task(s, id)?;
    match kind {
        "src.exchange.query" => exchange_queries::query_exchanges(s, id, p),
        "src.exchange.read" => read_exchange(s, id, p),
        "src.exchange.draft.read" => exchange_queries::read_draft(s, id, p),
        "src.exchange.compare" => exchange_queries::compare_exchanges(s, id, p),
        "src.exchange.diff" => exchange_queries::compare_exchanges(s, id, p),
        "src.exchange.curl_export" => exchange_queries::curl_export(s, id, p),
        "src.exchange.export" => exchange_queries::export_exchanges(s, id, p),
        "src.traffic.search" => traffic_index::search(s, id, p),
        "src.traffic.read_bytes" => traffic_index::read_bytes(s, id, p),
        "src.diagnostics.wire" => traffic_index::wire_diagnostics(s, id, p),
        "src.diagnostics.mcp_circuits" => traffic_index::mcp_circuits(s),
        "src.coverage.audit" => coverage::audit(s, id),
        "src.coordination.status" => coordination::status(s, id),
        "src.object.history" => objects::history(s, id, p),
        "src.oast.events" => oast::events(s, id, p),
        "src.identity.get" => identity::get(s, id, p),
        "src.identity.verifications" => identity::verifications(s, id, p),
        "src.identity.compare" => identity::compare(s, id, p),
        "src.identity.login_recipe.get" => identity::get_login_recipe(s, id, p),
        "src.identity.session.status" => identity::session_status(s, id, p),
        "src.identity.contexts" => identity::list_contexts(s, id),
        "src.proxy_bridge.status" => proxy_bridge::status(s, id),
        "src.proxy_bridge.ca_cert" => proxy_bridge::ca_cert(s, id),
        "src.proxypool.status" => proxypool::status(s, id),
        "src.mutator.list" => request_mutator::list(s, id),
        "src.intel.list" => vuln_intel::list(s, id),
        "src.intel.status" => vuln_intel::status(s, id),
        "src.workspace.tables" => workspace_explorer::tables(s, id, p),
        "src.workspace.sample" => workspace_explorer::sample(s, id, p),
        "src.notify.channel.list" => notify_gateway::list_channels(s, id),
        "src.inspection.requests" => inspection::requests(s, id),
        "src.browser.service" => rec(s,id,"browser_service","task"),
        "src.browser.context" => rec(s,id,"browser_context",require(p,"context_id")?),
        "src.browser.generation" => browser_runtime::generation(s, id),
        "src.browser.image.read" => browser_runtime::image_read(s, id, p),
        "src.stream.export" => protocol_stream::export(s, id, p),
        "src.research.records" => super::research_record_list(s, id, p, false),
        "src.research.record.get" => rec(s, id, require(p, "kind")?, require(p, "record_id")?),
        "src.research.plan.current" => research_plan_current(s, id, p),
        "src.research.backlog" => research_views::backlog(s, id),
        "src.research.openapi.read" => research_ingest::read_openapi(s, id, p),
        "src.research.openapi.compare" => research_ingest::compare_openapi(s, id, p),
        "src.research.javascript.read" => research_ingest::read_javascript(s, id, p),
        "src.research.skill.read" => research_ingest::read_skill(s, id, p),
        "src.research.graphql_grpc.auth_matrix" => graphql_grpc::auth_matrix(s, id, p),
        "src.research.precondition.summary" => research_workflow::precondition_summary(s, id),
        "src.research.history" => research_views::history(s, id, p),
        "src.asset.detail" => asset_detail(s, id, p),
        "src.asset.version.list" => local_parity::versions(s, id, p),
        "src.asset.query" | "src.asset.full_text" | "src.asset.pivot"
        | "src.asset.export" | "src.asset.saved_query.read" | "src.asset.fold_routes" => {
            asset_query::query(s, id, kind, p)
        }
        "src.asset.published" => asset_publication::published(s, id, require(p, "publication_id")?),
        "src.knowledge.search" => knowledge_search(s, id, p),
        "src.knowledge.read" => knowledge_read(s, id, p),
        "src.knowledge.source.read" => knowledge_source(s, id, p),
        "src.knowledge.history" => knowledge_history(s, id, p),
        "src.report.get" => report(s, id, require(p, "report_id")?),
        "src.workbench.report.diff" => local_parity::report_diff(s, id, p),
        "src.workbench.duplicates" => local_parity::duplicates(s, id),
        "src.report.list" => {
            let mut out = Vec::new();
            for row in records(s, id, "frozen_report")? {
                if p["finding_id"]
                    .as_str()
                    .is_some_and(|fid| row["payload"]["finding_id"] != fid)
                {
                    continue;
                }
                out.push(report(s, id, row["record_id"].as_str().unwrap_or(""))?);
            }
            Ok(json!(out))
        }
        "src.report.templates" => Ok(report_templates()),
        "src.finding.claims" => finding::claims(s, id, p),
        "src.finding.claim.get" => finding::claim(s, id, p),
        "src.finding.reviews" => finding::reviews(s, id, p),
        "src.finding.deduplications" => finding::deduplications(s, id, p),
        "src.finding.status" => finding::status(s, id, p),
        "src.feedback.platform_status" => platform_status(s, id, p),
        "src.feedback.history" => Ok(json!(
            payloads(s, id, "platform_status")?
                .into_iter()
                .filter(|v| v["report_id"] == p["report_id"])
                .collect::<Vec<_>>()
        )),
        "src.feedback.diff" => report_diff(s, id, p),
        "src.retest.recover" => retest::recover(s,id),
        "src.connector.manifests" => Ok(connector_manifests()),
        "src.connector.manifest" => connector_manifest(require(p, "connector_id")?),
        "src.connector.config" => connector_config(s, id, p),
        "src.connector.status" => connector_config::status(s, id),
        "src.connector.batch.list" => connector_config::batch_list(s, id, p),
        "src.connector.batch.get" => connector_config::batch_get(s, id, p),
        "src.ingestion.preview" => ingestion::preview(s, id, p),
        "src.credential.list" => credential_store::list(s, id),
        "src.monitor.get" => monitor_get(s, id, p),
        "src.monitor.preflight" => monitor_runtime::preflight_query(s, id, p),
        "src.monitor.changes" => monitor_runtime::changes(s, id, p),
        "src.monitor.notifications" => monitor_runtime::notifications(s, id, p),
        "src.monitor.scheduler_health" => monitor_runtime::scheduler_health(s, id),
        "src.artifact.read" => read_artifact(s, id, p),
        _ => Err(Error::new(
            StatusCode::NOT_IMPLEMENTED,
            "capability.unsupported",
            format!("SRC 查询 `{kind}` 尚未接入"),
        )),
    }
}

fn payloads(s: &AppState, id: &str, kind: &str) -> Api<Vec<Value>> {
    Ok(records(s, id, kind)?
        .into_iter()
        .map(|v| v["payload"].clone())
        .collect())
}
fn record_payload(s: &AppState, id: &str, kind: &str, rid: &str) -> Api<Value> {
    Ok(rec(s, id, kind, rid)?["payload"].clone())
}
fn research_plan_current(s: &AppState, id: &str, params: &Value) -> Api<Value> {
    let plan_id = require(params, "plan_id")?;
    records(s, id, "research_plan")?
        .into_iter()
        .filter(|row| row["parent_id"] == plan_id)
        .max_by_key(|row| row["payload"]["version"].as_u64().unwrap_or(0))
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "research plan is unavailable",
            )
        })
}
fn research_base_revision(body: &Value) -> Api<Option<u64>> {
    Ok(match body.get("base_revision") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| Error::invalid("base_revision must be an integer"))?,
        ),
    })
}
fn save_research_versioned(
    s: &AppState,
    id: &str,
    kind: &str,
    parent_id: &str,
    base_revision: Option<u64>,
    mut payload: Value,
    heads: &str,
    applied: &str,
    command_id: Option<&str>,
    extra_writes: &[(&str, String, Value)],
) -> Api<Value> {
    let head_key = format!("{id}:{parent_id}");
    loop {
        if let Some(command_id) = command_id {
            if let Some(marker) = s.store.get(applied, command_id).map_err(db)? {
                return Ok(marker["output"].clone());
            }
        }
        let head = s.store.get(heads, &head_key).map_err(db)?;
        let latest = records(s, id, kind)?
            .into_iter()
            .filter(|row| row["parent_id"] == parent_id)
            .max_by_key(|row| row["payload"]["version"].as_u64().unwrap_or(0));
        let current_version = latest
            .as_ref()
            .and_then(|row| row["payload"]["version"].as_u64())
            .unwrap_or(0);
        if base_revision.is_some_and(|base| base != current_version) {
            return Err(Error {
                status: StatusCode::CONFLICT,
                code: "version_conflict",
                message: format!("{kind} base revision is stale"),
                detail: json!({"expected":base_revision,"actual":current_version}),
            });
        }
        let version = current_version + 1;
        let record_id = format!("{parent_id}:v{version}");
        payload["version"] = json!(version);
        payload["supersedes"] = latest
            .as_ref()
            .map(|row| row["record_id"].clone())
            .unwrap_or(Value::Null);
        if kind != "skill_material" {
            payload.as_object_mut().unwrap().remove("digest");
        }
        let (storage_id, storage_version, mut record) =
            match stage_rec(s, id, kind, &record_id, payload.clone(), Some(0)) {
                Ok(staged) => staged,
                Err(error) if error.status == StatusCode::CONFLICT => continue,
                Err(error) => return Err(error),
            };
        record["record_id"] = json!(record_id);
        record["parent_id"] = json!(parent_id);
        record["natural_key"] = json!(record_id);
        let document = json!({"version":storage_version+1,"values":record});
        let next_head = json!({"version":version,"record_id":record_id});
        let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":record}));
        let mut expected = vec![
            (RECORDS, storage_id.as_str(), None),
            (heads, head_key.as_str(), head.as_ref()),
        ];
        let mut writes = vec![
            (RECORDS, storage_id.as_str(), &document),
            (heads, head_key.as_str(), &next_head),
        ];
        if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
            expected.push((applied, command_id, None));
            writes.push((applied, command_id, marker));
        }
        for (collection, key, value) in extra_writes {
            expected.push((*collection, key.as_str(), None));
            writes.push((*collection, key.as_str(), value));
        }
        if s.store.put_many_if(&expected, &writes).map_err(db)? {
            return Ok(record);
        }
    }
}
pub(super) fn save_research_plan(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    let input = body["payload"]
        .as_object()
        .ok_or_else(|| Error::invalid("payload must be an object"))?;
    let missing = ["question", "rationale", "gaps", "actions"]
        .into_iter()
        .filter(|field| !input.contains_key(*field))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "plan_incomplete",
            format!("plan is missing: {}", missing.join(", ")),
        ));
    }
    let plan_id = body["plan_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("research-plan-{}", Uuid::new_v4().simple()));
    let mut payload = Value::Object(input.clone());
    payload["created_by"] = json!(actor_id);
    payload["created_at"] = json!(now());
    payload["evidence_type"] = json!("Plan");
    payload["scheduler"] = json!(false);
    save_research_versioned(
        s,
        id,
        "research_plan",
        &plan_id,
        research_base_revision(body)?,
        payload,
        RESEARCH_PLAN_HEADS,
        RESEARCH_PLAN_APPLIED,
        command_id,
        &[],
    )
}
pub(super) fn save_research_authorization(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    if let Some(command_id) = command_id {
        if let Some(marker) = s.store.get(RESEARCH_AUTH_APPLIED, command_id).map_err(db)? {
            return Ok(marker["output"].clone());
        }
    }
    let source_refs = body["source_refs"]
        .as_array()
        .ok_or_else(|| Error::invalid("source_refs is required"))?;
    let clauses = body["clauses"]
        .as_array()
        .ok_or_else(|| Error::invalid("clauses is required"))?;
    let unknowns = body["unknowns"]
        .as_array()
        .ok_or_else(|| Error::invalid("unknowns is required"))?;
    let raw = body
        .get("raw_artifact")
        .ok_or_else(|| Error::invalid("raw_artifact is required"))?;
    let raw_artifact = if raw.is_null() {
        Value::Null
    } else {
        if raw["task_id"] != id {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.artifact_scope",
                "证据工件不属于该 Task",
            ));
        }
        let artifact_id = require(raw, "artifact_id")?;
        s.store
            .get("src_artifacts", &format!("{id}:{artifact_id}"))
            .map_err(db)?
            .ok_or_else(|| {
                Error::new(
                    StatusCode::NOT_FOUND,
                    "src.artifact_missing",
                    "证据工件不存在",
                )
            })?;
        let mut reference = raw.clone();
        if let Some(object) = reference.as_object_mut() {
            for field in ["sha256", "sha512", "digest", "content_digest"] {
                object.remove(field);
            }
        }
        verify_ref(s, id, &reference)?;
        reference
    };
    let snapshot_id = body["snapshot_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("authorization-snapshot-{}", Uuid::new_v4().simple()));
    let payload = json!({"source_refs":source_refs,"raw_artifact":raw_artifact,
        "clauses":clauses,"unknowns":unknowns,"created_by":actor_id,
        "created_at":now(),"authorization_is_not_execution_grant":true});
    save_research_versioned(
        s,
        id,
        "authorization_snapshot",
        &snapshot_id,
        research_base_revision(body)?,
        payload,
        RESEARCH_AUTH_HEADS,
        RESEARCH_AUTH_APPLIED,
        command_id,
        &[],
    )
}
pub(super) fn save_research_skill(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_SKILL_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let source_url = require(body, "source_url")?;
    let source_commit = require(body, "source_commit")?;
    let distillation_version = require(body, "distillation_version")?;
    let encoded = body["content_b64"]
        .as_str()
        .ok_or_else(|| Error::invalid("content_b64 is required"))?;
    let content = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Error::invalid("content_b64 must be base64"))?;
    let required = body["required_capabilities"]
        .as_object()
        .ok_or_else(|| Error::invalid("required_capabilities must be an object"))?;
    let available = body["available_capabilities"]
        .as_object()
        .ok_or_else(|| Error::invalid("available_capabilities must be an object"))?;
    let fragments = body["selected_fragments"]
        .as_array()
        .ok_or_else(|| Error::invalid("selected_fragments must be an array"))?;
    let mut mappings = Vec::new();
    let mut missing = Vec::new();
    for (capability, required_version) in required {
        let actual = available.get(capability).cloned().unwrap_or(Value::Null);
        let present = !actual.is_null();
        let mut mapping = json!({"capability":capability,"required_version":required_version,
            "actual_version":actual,"available":present});
        if !present {
            mapping["code"] = json!("capability_unavailable");
            missing.push(json!(capability));
        }
        mappings.push(mapping);
    }
    let artifact_id = format!("sart_{}", Uuid::new_v4().simple());
    let artifact = json!({"artifact_id":artifact_id,"version":1,"task_id":id,
        "size":content.len(),"name":"skill-source","media_type":"text/markdown",
        "original":true,"complete":"complete"});
    let saved_artifact = json!({"artifact_id":artifact_id,"version":1,"task_id":id,
        "size":content.len(),"name":"skill-source","media_type":"text/markdown",
        "data":base64::engine::general_purpose::STANDARD.encode(&content),"created_at":now()});
    let material_id = body["material_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("skill-source:{source_url}"));
    let payload = json!({"digest":format!("{:x}",Sha256::digest(&content)),"content_digest":format!("{:x}",Sha256::digest(&content)),"source_url":source_url,"source_commit":source_commit,
        "distillation_version":distillation_version,"selected_fragments":fragments,
        "capability_mappings":mappings,"missing_capabilities":missing,
        "read_scope":body["read_scope"].as_str().unwrap_or("task"),
        "artifact":artifact,"registered_by":actor_id,"registered_at":now(),
        "status":if missing.is_empty(){"available"}else{"partial"}});
    let extra = [(
        "src_artifacts",
        format!("{id}:{artifact_id}"),
        saved_artifact,
    )];
    save_research_versioned(
        s,
        id,
        "skill_material",
        &material_id,
        research_base_revision(body)?,
        payload,
        RESEARCH_SKILL_HEADS,
        RESEARCH_SKILL_APPLIED,
        command_id,
        &extra,
    )
}
pub(super) fn ingest_research_openapi(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    research_ingest::ingest_openapi(s, id, body, command_id)
}
pub(super) fn ingest_research_javascript(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    research_ingest::ingest_javascript(s, id, body, command_id)
}
pub(super) fn register_research_observation(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    match kind {
        "src.research.runtime.register" => {
            research_observations::register_runtime(s, id, body, command_id)
        }
        "src.research.dataflow.register" => {
            research_observations::register_dataflow(s, id, body, command_id)
        }
        "src.research.page_action.register" => {
            research_page_action::register(s, id, body, command_id)
        }
        _ => Err(Error::invalid("unsupported SRC research observation")),
    }
}
pub(super) fn apply_research_business_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
) -> Api<Value> {
    match kind {
        "src.research.business.save" => research_business::save(s, id, body, Some(command_id)),
        "src.research.business.derive" => research_business::derive(s, id, body, Some(command_id)),
        _ => Err(Error::invalid("unsupported SRC business command")),
    }
}
pub(super) fn apply_research_workflow_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
    actor_kind: &str,
) -> Api<Value> {
    match kind {
        "src.research.permission_question.add" => {
            research_workflow::question(s, id, body, Some(command_id), actor_id, actor_kind)
        }
        "src.research.experiment.record" => {
            research_workflow::experiment(s, id, body, Some(command_id))
        }
        "src.research.capability_chain.record" => {
            research_workflow::capability_chain(s, id, body, Some(command_id))
        }
        "src.research.precondition_trial.record" => {
            research_workflow::precondition_trial(s, id, body, Some(command_id))
        }
        _ => Err(Error::invalid("unsupported SRC research workflow command")),
    }
}
fn normalized_asset_host(raw: &str) -> Api<String> {
    let host = raw.trim().trim_end_matches('.');
    let unbracketed = host.strip_prefix('[').and_then(|value| value.strip_suffix(']')).unwrap_or(host);
    if let Ok(address) = unbracketed.parse::<IpAddr>() {
        return Ok(address.to_string());
    }
    if host.is_empty() || host.chars().any(|c| matches!(c, '/' | ':' | '@' | '?' | '#')) {
        return Err(Error::new(StatusCode::BAD_REQUEST,"asset_identity_invalid","invalid asset host"));
    }
    reqwest::Url::parse(&format!("http://{host}/"))
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST,"asset_identity_invalid","invalid asset host"))
}

fn normalized_asset_path(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return "/".to_owned();
    }
    let absolute = raw.starts_with('/');
    let candidate = format!("http://asset.invalid/{}", raw.trim_start_matches('/'));
    let normalized = reqwest::Url::parse(&candidate)
        .map(|url| url.path().to_owned())
        .unwrap_or_else(|_| raw.to_owned());
    if absolute {
        normalized
    } else {
        normalized.strip_prefix('/').unwrap_or(&normalized).to_owned()
    }
}

fn normalized_identity(asset_type: &str, identity: &Value, source: Option<(&str, &str)>) -> Api<Value> {
    let normalized=identity.as_object().map(|rows|Value::Object(rows.iter().map(|(key,value)|(key.clone(),value.as_str().map(|value|json!(value.trim())).unwrap_or_else(||value.clone()))).collect())).unwrap_or_else(||identity.clone());let identity=&normalized;
    if !identity.is_object() {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "asset_identity_invalid",
            "asset identity must be an object",
        ));
    }
    let text = |field: &str| identity[field].as_str().unwrap_or("").trim();
    match asset_type {
        "root_domain" | "domain" | "subdomain" | "dns" => {
            let raw = ["name", "domain", "host"]
                .iter()
                .find_map(|k| identity[*k].as_str().filter(|v| !v.trim().is_empty()))
                .unwrap_or("")
                .trim()
                .trim_end_matches('.');
            if raw.is_empty()
                || raw
                    .chars()
                    .any(|c| matches!(c, '/' | ':' | '@' | '?' | '#'))
            {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "asset_identity_invalid",
                    "invalid domain",
                ));
            }
            let host = normalized_asset_host(raw)?;
            if asset_type == "dns" {
                Ok(
                    json!({"name":host,"record_type":require(identity,"record_type")?.to_uppercase(),"value":require(identity,"value")?}),
                )
            } else {
                Ok(json!({"name":host}))
            }
        }
        "ip" => {
            let raw = ["address", "cidr", "ip"]
                .iter()
                .find_map(|k| identity[*k].as_str().filter(|value|!value.is_empty()))
                .unwrap_or("");
            let address: IpAddr = raw.parse().map_err(|_| {
                Error::new(
                    StatusCode::BAD_REQUEST,
                    "asset_identity_invalid",
                    "invalid IP address",
                )
            })?;
            Ok(json!({"address":address.to_string(),"family":if address.is_ipv4(){4}else{6}}))
        }
        "cidr" => {
            let raw = ["address", "cidr", "ip"]
                .iter()
                .find_map(|k| identity[*k].as_str().filter(|value|!value.is_empty()))
                .unwrap_or("");
            let (ip, prefix) = raw.split_once('/').map(|(ip, prefix)| (ip, Some(prefix)))
                .unwrap_or((raw, None));
            let address: IpAddr = ip
                .parse()
                .map_err(|_| Error::invalid("invalid CIDR address"))?;
            let mask_field = ["mask", "netmask", "inverse_mask", "wildcard_mask"]
                .iter()
                .find_map(|k| identity[*k].as_str().filter(|value| !value.trim().is_empty()));
            let prefix: u8 = match (prefix, mask_field) {
                (Some(value), _) => cidr_prefix(value, address.is_ipv4())?,
                (None, Some(value)) => cidr_prefix(value, address.is_ipv4())?,
                (None, None) => {
                    if address.is_ipv4() {
                        32
                    } else {
                        128
                    }
                }
            };
            if prefix > if address.is_ipv4() { 32 } else { 128 } {
                return Err(Error::invalid("invalid CIDR prefix"));
            }
            let network = match address {
                IpAddr::V4(ip) => {
                    let mask = if prefix == 0 {
                        0
                    } else {
                        u32::MAX << (32 - prefix)
                    };
                    IpAddr::V4(Ipv4Addr::from(u32::from(ip) & mask))
                }
                IpAddr::V6(ip) => {
                    let mask = if prefix == 0 {
                        0
                    } else {
                        u128::MAX << (128 - prefix)
                    };
                    IpAddr::V6(Ipv6Addr::from(u128::from(ip) & mask))
                }
            };
            Ok(
                json!({"address":format!("{network}/{prefix}"),"family":if address.is_ipv4(){4}else{6}}),
            )
        }
        "port" | "port_service" | "web_service" | "non_web_service" => {
            let raw = require(identity, "host")?;
            let host = normalized_asset_host(raw)?;
            let port = identity["port"]
                .as_u64().or_else(|| identity["port"].as_str().and_then(|v| v.parse().ok()))
                .filter(|v| (1..=65535).contains(v))
                .ok_or_else(|| Error::invalid("valid service port required"))?;
            let protocol = identity["protocol"]
                .as_str().filter(|value|!value.is_empty())
                .unwrap_or(if matches!(asset_type, "port" | "port_service") { "tcp" } else { "http" })
                .to_ascii_lowercase();
            let mut normalized = json!({"host":host,"port":port,"protocol":protocol});
            if matches!(asset_type, "port" | "port_service") {
                normalized["transport"] = json!(identity["transport"].as_str()
                    .filter(|value| !value.trim().is_empty()).unwrap_or("tcp").trim().to_ascii_lowercase());
            }
            Ok(normalized)
        }
        "web_origin" => {
            if let (None, Some(scheme), Some(host)) = (
                identity["url"]
                    .as_str()
                    .or_else(|| identity["origin"].as_str())
                    .filter(|value| !value.is_empty()),
                identity["scheme"]
                    .as_str()
                    .filter(|value| !value.trim().is_empty()),
                identity["host"]
                    .as_str()
                    .filter(|value| !value.trim().is_empty()),
            ) {
                let scheme = scheme.trim().to_ascii_lowercase();
                let default_port = if scheme == "http" { 80 } else if scheme == "https" { 443 } else { 0 };
                let port = match &identity["port"] {
                    Value::Null => default_port,
                    Value::String(v) if v.is_empty() => default_port,
                    Value::Number(v) if v.as_i64() == Some(0) => default_port,
                    Value::String(v) => v.parse::<i64>().map_err(|_| Error::invalid("origin port must be an integer"))?,
                    v => v.as_i64().ok_or_else(|| Error::invalid("origin port must be an integer"))?,
                };
                Ok(json!({"scheme": scheme, "host": normalized_asset_host(host)?, "port": port}))
            } else {
                let raw = identity["url"]
                    .as_str()
                    .or_else(|| identity["origin"].as_str())
                    .or_else(|| identity["host"].as_str())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Error::invalid("origin URL required"))?;
                let complete = if raw.contains("://") { raw.to_owned() } else { format!("https://{raw}") };
                let url = parse_url(&complete)?;
                Ok(json!({
                    "scheme": url.scheme(),
                    "host": normalized_asset_host(url.host_str().unwrap_or(""))?,
                    "port": url.port().filter(|value| *value > 0).unwrap_or(if url.scheme() == "http" { 80 } else if url.scheme() == "https" { 443 } else { 0 })
                }))
            }
        }
        "http_endpoint" | "http_route" | "api_endpoint" | "graphql_operation"
        | "websocket_endpoint" | "parameter" => {
            let mut out = if let (None, Some(scheme), Some(host)) = (
                identity["url"].as_str().filter(|value| !value.is_empty()),
                identity["scheme"]
                    .as_str()
                    .filter(|value| !value.trim().is_empty()),
                identity["host"]
                    .as_str()
                    .filter(|value| !value.trim().is_empty()),
            ) {
                let scheme = scheme.trim().to_ascii_lowercase();
                let default_port=if matches!(scheme.as_str(),"http"|"ws"){80}else if matches!(scheme.as_str(),"https"|"wss"){443}else{0};
                let port=match &identity["port"]{Value::Null=>default_port,Value::String(value)if value.is_empty()=>default_port,Value::Number(value)if value.as_i64()==Some(0)=>default_port,Value::String(value)=>value.parse::<i64>().map_err(|_|Error::invalid("endpoint port must be an integer"))?,value=>value.as_i64().ok_or_else(||Error::invalid("endpoint port must be an integer"))?};
                json!({"scheme":scheme,"host":normalized_asset_host(host)?,"port":port,
                    "path":normalized_asset_path(identity["path"].as_str().unwrap_or("/")),
                    "query":identity["query"].as_str().unwrap_or(""),
                    "method":identity["method"].as_str().filter(|value|!value.is_empty()).unwrap_or("GET").to_uppercase()})
            } else {
                let raw = identity["url"]
                    .as_str().filter(|value|!value.is_empty())
                    .or_else(|| identity["endpoint"].as_str().filter(|value|!value.is_empty()))
                    .ok_or_else(|| Error::invalid("endpoint URL required"))?;
                let default_scheme = if asset_type == "websocket_endpoint" { "wss" } else { "https" };
                let complete = if raw.contains("://") { raw.to_owned() } else { format!("{default_scheme}://{raw}") };
                let url = if asset_type == "websocket_endpoint" {
                    reqwest::Url::parse(&complete).map_err(|_| Error::invalid("invalid websocket endpoint URL"))?
                } else {
                    parse_url(&complete)?
                };
                if asset_type == "websocket_endpoint" && !matches!(url.scheme(), "ws" | "wss" | "http" | "https") {
                    return Err(Error::invalid("websocket_endpoint scheme must be ws or wss"));
                }
                let scheme = url.scheme();
                let default_port = if matches!(scheme, "http" | "ws") { 80 } else if matches!(scheme, "https" | "wss") { 443 } else { 0 };
                json!({"scheme":scheme,"host":normalized_asset_host(url.host_str().unwrap_or(""))?,
                    "port":url.port().filter(|value|*value>0).unwrap_or(default_port),
                    "path":url.path(),"query":complete.split_once('?').filter(|_|complete.find('#').is_none_or(|fragment|complete.find('?').unwrap()<fragment)).map(|(_,query)|query.split('#').next().unwrap_or("")).unwrap_or(""),
                    "method":identity["method"].as_str().filter(|value|!value.is_empty()).unwrap_or("GET").to_uppercase()})
            };
            if asset_type == "graphql_operation" {
                let op_name = identity["operation_name"]
                    .as_str()
                    .or_else(|| identity["operation"].as_str())
                    .unwrap_or("")
                    .trim();
                if !op_name.is_empty() {
                    out["operation_name"] = json!(op_name);
                }
                if let Some(op_type) = identity["operation_type"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
                    out["operation_type"] = json!(op_type.to_ascii_lowercase());
                }
            }
            if asset_type == "parameter" {
                out["location"] = json!(
                    require(identity, "location")
                        .or_else(|_| require(identity, "param_location"))?
                );
                out["name"] =
                    json!(require(identity, "name").or_else(|_| require(identity, "param_name"))?);
                for key in ["identity_ref", "tenant", "action"] {
                    out[key] = json!(identity[key].as_str().unwrap_or("").trim());
                }
            }
            Ok(out)
        }
        "app" | "miniprogram" | "mobile_package" => {
            let platform = require(identity, "platform")?.to_ascii_lowercase();
            let stable = identity["stable_id"]
                .as_str()
                .or_else(|| identity["package_id"].as_str())
                .or_else(|| identity["bundle_id"].as_str())
                .or_else(|| identity["appid"].as_str())
                .unwrap_or("")
                .trim();
            if stable.is_empty() {
                let name = identity["name"].as_str().unwrap_or("").trim();
                let (source_kind, source_id) = source.unwrap_or(("", ""));
                let marker = if source.is_some() {
                    json!({"source_kind":source_kind,"source_id":source_id})
                } else { json!({}) };
                let candidate = json!({"name":name,"source":marker});
                let key = format!("{:x}", Sha256::digest(python_json_default(&candidate).as_bytes()));
                Ok(json!({"platform":platform,"stable_id":"","candidate_key":key}))
            } else {
                Ok(json!({"platform":platform,"stable_id":stable}))
            }
        }
        "cloud" | "cloud_resource" => Ok(
            json!({"provider":require(identity,"provider")?,"resource_id":require(identity,"resource_id")?,
            "tenant":text("tenant"),"region":text("region")}),
        ),
        "repository" | "code_repository" => {
            let stable = identity["stable_id"].as_str().map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
                .or_else(|| identity["stable_id"].as_i64().map(|value| value.to_string()))
                .or_else(|| identity["stable_id"].as_u64().map(|value| value.to_string()))
                .or_else(|| require(identity, "repository").ok().map(str::to_owned))
                .or_else(|| require(identity, "canonical").ok().map(str::to_owned))
                .ok_or_else(|| Error::invalid("stable_id is required"))?;
            Ok(json!({"platform":require(identity,"platform").or_else(|_|require(identity,"host"))?.to_ascii_lowercase(),
                "stable_id":stable}))
        }
        "cdn" => {
            let provider = ["provider", "cdn", "vendor"]
                .iter()
                .find_map(|k| identity[*k].as_str().map(str::trim).filter(|v| !v.is_empty()))
                .unwrap_or("")
                .to_ascii_lowercase();
            let raw_cname = ["cname", "target"]
                .iter()
                .find_map(|k| identity[*k].as_str().map(str::trim).filter(|v| !v.is_empty()))
                .unwrap_or("")
                .trim_end_matches('.');
            let raw_host = ["host", "domain", "name"]
                .iter()
                .find_map(|k| identity[*k].as_str().map(str::trim).filter(|v| !v.is_empty()))
                .unwrap_or("")
                .trim_end_matches('.');
            if provider.is_empty() && raw_cname.is_empty() && raw_host.is_empty() {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "asset_identity_invalid",
                    "cdn identity requires provider, cname, or host",
                ));
            }
            let cname = if raw_cname.is_empty() {
                String::new()
            } else {
                normalized_asset_host(raw_cname)?
            };
            let host = if raw_host.is_empty() {
                String::new()
            } else {
                normalized_asset_host(raw_host)?
            };
            Ok(json!({"provider": provider, "cname": cname, "host": host}))
        }
        "certificate" => {
            let sha256 = ["sha256", "fingerprint_sha256", "fingerprint", "cert_sha256"]
                .iter()
                .find_map(|k| identity[*k].as_str().map(str::trim).filter(|v| !v.is_empty()))
                .unwrap_or("")
                .replace(':', "")
                .to_ascii_lowercase();
            let serial = ["serial_number", "serial"]
                .iter()
                .find_map(|k| identity[*k].as_str().map(str::trim).filter(|v| !v.is_empty()))
                .unwrap_or("")
                .to_ascii_lowercase();
            let common_name = ["common_name", "cn", "subject", "domain", "name"]
                .iter()
                .find_map(|k| identity[*k].as_str().map(str::trim).filter(|v| !v.is_empty()))
                .unwrap_or("")
                .to_ascii_lowercase();
            let issuer = identity["issuer"].as_str().unwrap_or("").trim().to_owned();
            if sha256.is_empty() && serial.is_empty() && common_name.is_empty() {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "asset_identity_invalid",
                    "certificate identity requires sha256, serial_number, or common_name",
                ));
            }
            Ok(json!({
                "sha256": sha256,
                "serial_number": serial,
                "common_name": common_name,
                "issuer": issuer
            }))
        }
        "organization_hint" => {
            let name = ["name", "organization", "org", "company"]
                .iter()
                .find_map(|k| identity[*k].as_str().map(str::trim).filter(|v| !v.is_empty()))
                .unwrap_or("")
                .to_owned();
            let icp = identity["icp"].as_str().unwrap_or("").trim().to_owned();
            let domain = identity["domain"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(normalized_asset_host)
                .transpose()?
                .unwrap_or_default();
            if name.is_empty() && icp.is_empty() && domain.is_empty() {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "asset_identity_invalid",
                    "organization_hint identity requires name, icp, or domain",
                ));
            }
            Ok(json!({"name": name, "icp": icp, "domain": domain}))
        }
        "account_subject" => {
            let subject = ["subject", "username", "email", "account", "account_id", "id"]
                .iter()
                .find_map(|k| identity[*k].as_str().map(str::trim).filter(|v| !v.is_empty()))
                .ok_or_else(|| {
                    Error::new(
                        StatusCode::BAD_REQUEST,
                        "asset_identity_invalid",
                        "account_subject identity requires subject",
                    )
                })?;
            let platform = ["platform", "provider", "tenant"]
                .iter()
                .find_map(|k| identity[*k].as_str().map(str::trim).filter(|v| !v.is_empty()))
                .unwrap_or("")
                .to_ascii_lowercase();
            Ok(json!({"platform": platform, "subject": subject}))
        }
        _ => Err(Error::new(
            StatusCode::BAD_REQUEST,
            "asset_type_invalid",
            format!("unsupported asset type {asset_type}"),
        )),
    }
}

pub(super) fn preview_asset_identity(
    asset_type: &str,
    identity: &Value,
    source: Option<(&str, &str)>,
) -> Api<(Value, String, String)> {
    let norm = normalized_identity(asset_type, identity, source)?;
    let key = asset_identity_key(asset_type, &norm);
    let label = asset_display_label(asset_type, &norm);
    Ok((norm, key, label))
}

fn asset_display_label(asset_type: &str, identity: &Value) -> String {
    match asset_type {
        "domain" | "subdomain" | "root_domain" | "dns" => {
            identity["name"].as_str().unwrap_or("").to_owned()
        }
        "ip" | "cidr" => identity["address"].as_str().unwrap_or("").to_owned(),
        "port" | "port_service" | "web_service" | "non_web_service" => {
            format!(
                "{}:{}",
                identity["host"].as_str().unwrap_or(""),
                identity["port"].as_u64().unwrap_or(0)
            )
        }
        "web_origin" => {
            let scheme = identity["scheme"].as_str().unwrap_or("https");
            let host = identity["host"].as_str().unwrap_or("");
            let port = identity["port"].as_u64().unwrap_or(443);
            let default_p = if scheme == "http" { 80 } else { 443 };
            if port == default_p || port == 0 {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}:{port}")
            }
        }
        "http_endpoint" | "http_route" | "api_endpoint" | "graphql_operation"
        | "websocket_endpoint" | "parameter" => {
            let scheme = identity["scheme"].as_str().unwrap_or("https");
            let host = identity["host"].as_str().unwrap_or("");
            let path = identity["path"].as_str().unwrap_or("/");
            format!("{scheme}://{host}{path}")
        }
        "code_repository" | "repository" | "app" | "miniprogram" | "mobile_package" => identity
            ["stable_id"]
            .as_str()
            .filter(|v| !v.is_empty())
            .or_else(|| identity["candidate_key"].as_str())
            .unwrap_or("")
            .to_owned(),
        "cloud" | "cloud_resource" => identity["resource_id"].as_str().unwrap_or("").to_owned(),
        "cdn" => identity["cname"]
            .as_str()
            .filter(|v| !v.is_empty())
            .or_else(|| identity["host"].as_str().filter(|v| !v.is_empty()))
            .or_else(|| identity["provider"].as_str())
            .unwrap_or("")
            .to_owned(),
        "certificate" => identity["common_name"]
            .as_str()
            .filter(|v| !v.is_empty())
            .or_else(|| identity["sha256"].as_str().filter(|v| !v.is_empty()))
            .or_else(|| identity["serial_number"].as_str())
            .unwrap_or("")
            .to_owned(),
        "organization_hint" => identity["name"]
            .as_str()
            .filter(|v| !v.is_empty())
            .or_else(|| identity["icp"].as_str().filter(|v| !v.is_empty()))
            .or_else(|| identity["domain"].as_str())
            .unwrap_or("")
            .to_owned(),
        "account_subject" => identity["subject"].as_str().unwrap_or("").to_owned(),
        _ => String::new(),
    }
}

pub(super) fn asset_query_url_of(asset: &Value) -> String {
    let identity = &asset["identity"];
    if let Some(url) = identity["url"].as_str().filter(|v| !v.is_empty()) {
        return url.to_owned();
    }
    let Some(host) = identity["host"].as_str().filter(|v| !v.is_empty()) else {
        return String::new();
    };
    let scheme = identity["scheme"].as_str().unwrap_or("https");
    let default_port = if scheme == "http" { 80 } else { 443 };
    let port = identity["port"]
        .as_u64()
        .filter(|p| *p > 0 && *p != default_port)
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    let path = identity["path"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("/");
    format!("{scheme}://{host}{port}{path}")
}

pub(super) fn find_exchange_record(s: &AppState, id: &str, exchange_id: &str) -> Api<Value> {
    match rec(s, id, "http_exchange", exchange_id) {
        Ok(row) => Ok(row),
        Err(error) if error.status == StatusCode::NOT_FOUND => rec(s, id, "import_exchange", exchange_id),
        Err(error) => Err(error),
    }
}

fn save_asset_judgement(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    if let Some(cmd) = command_id
        && let Some(marker) = s.store.get(ASSET_JUDGEMENT_APPLIED, cmd).map_err(db)?
    {
        return Ok(marker["output"].clone());
    }
    let asset_id = require(body, "asset_id")?;
    let base_revision = body.get("base_revision").and_then(Value::as_u64);
    let updated_by = body["updated_by"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(actor_id)
        .to_owned();

    loop {
        if let Some(cmd) = command_id
            && let Some(marker) = s.store.get(ASSET_JUDGEMENT_APPLIED, cmd).map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
        let asset_rec = asset_row(s, id, asset_id)?;
        let mut asset_payload = asset_rec["payload"].clone();
        if asset_payload["lifecycle"] == "destroyed" {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "Asset was destroyed",
            ));
        }
        let j_key = rec_key(id, "asset_judgement", asset_id);
        let j_doc = s.store.get(RECORDS, &j_key).map_err(db)?;
        let j_rev = j_doc.as_ref().and_then(|d| d["version"].as_u64()).unwrap_or(0);
        if let Some(expected) = base_revision
            && j_rev != expected
        {
            return Err(Error::conflict("asset judgement revision is stale"));
        }
        let prev_j = j_doc
            .as_ref()
            .map(|d| d["values"]["payload"].clone())
            .unwrap_or_else(|| json!({}));

        let judgement_state = body
            .get("judgement_state")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                prev_j["judgement_state"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "uncertain".to_owned());
        let system_name = match body.get("system_name") {
            Some(v) => v.clone(),
            None => prev_j.get("system_name").cloned().unwrap_or(Value::Null),
        };
        let uncertainty_note = match body.get("uncertainty_note") {
            Some(v) => v.clone(),
            None => prev_j
                .get("uncertainty_note")
                .cloned()
                .unwrap_or(Value::Null),
        };
        let priority = match body.get("priority") {
            Some(v) => v.clone(),
            None => prev_j.get("priority").cloned().unwrap_or(Value::Null),
        };
        let updated_at = now();
        let j_payload = json!({
            "asset_id": asset_id,
            "judgement_state": judgement_state,
            "system_name": system_name,
            "uncertainty_note": uncertainty_note,
            "priority": priority,
            "updated_by": updated_by,
            "updated_at": updated_at
        });

        asset_payload["judgement_state"] = json!(judgement_state);
        asset_payload["system_name"] = system_name.clone();
        asset_payload["uncertainty_note"] = uncertainty_note.clone();
        asset_payload["priority"] = priority.clone();
        asset_payload["judgement_updated_by"] = json!(updated_by);
        asset_payload["judgement_updated_at"] = json!(updated_at);
        if !asset_payload["judgements"].is_object() {
            asset_payload["judgements"] = json!({});
        }
        asset_payload["judgements"]["researcher_judgement"] = j_payload.clone();

        let (staged_j_key, _, staged_j_row) =
            stage_rec(s, id, "asset_judgement", asset_id, j_payload.clone(), Some(j_rev))?;
        let asset_nk = asset_rec["natural_key"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or(asset_id);
        let asset_store_key = rec_key(id, "asset", asset_nk);
        let asset_doc = s.store.get(RECORDS, &asset_store_key).map_err(db)?;
        let asset_ver = asset_doc
            .as_ref()
            .and_then(|d| d["version"].as_u64())
            .unwrap_or(0);
        let (staged_a_key, _, staged_a_row) =
            stage_rec(s, id, "asset", asset_nk, asset_payload.clone(), Some(asset_ver))?;

        let j_write = json!({"version": j_rev + 1, "values": staged_j_row});
        let a_write = json!({"version": asset_ver + 1, "values": staged_a_row});
        let output = json!({
            "asset_id": asset_id,
            "judgement": j_payload,
            "revision": j_rev + 1,
            "asset": asset_payload
        });
        let mut conds = vec![
            (RECORDS, staged_j_key.as_str(), j_doc.as_ref()),
            (RECORDS, staged_a_key.as_str(), asset_doc.as_ref()),
        ];
        let mut writes = vec![
            (RECORDS, staged_j_key.as_str(), &j_write),
            (RECORDS, staged_a_key.as_str(), &a_write),
        ];
        let marker = command_id.map(|c| json!({"command_id": c, "output": output}));
        if let (Some(c), Some(m)) = (command_id, marker.as_ref()) {
            conds.push((ASSET_JUDGEMENT_APPLIED, c, None));
            writes.push((ASSET_JUDGEMENT_APPLIED, c, m));
        }
        if s.store.put_many_if(&conds, &writes).map_err(db)? {
            return Ok(output);
        }
    }
}

fn python_json_default(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            format!("{{{}}}", keys.into_iter().map(|key| format!("{}: {}",
                serde_json::to_string(key).unwrap(), python_json_default(&map[key]))
            ).collect::<Vec<_>>().join(", "))
        }
        Value::Array(items) => format!("[{}]", items.iter().map(python_json_default)
            .collect::<Vec<_>>().join(", ")),
        _ => value.to_string(),
    }
}

fn asset_identity_key(asset_type: &str, identity: &Value) -> String {
    let canonical = browser_runtime::canonical_json(identity);
    format!("asset-key.v1:{asset_type}:{:x}", Sha256::digest(canonical.as_bytes()))
}

fn observe_asset(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    let asset_type = require(body, "asset_type")?;
    let source_kind = require(body, "source_kind")?;
    let source_id = require(body, "source_id")?;
    let identity = normalized_identity(asset_type, &body["identity"], Some((source_kind, source_id)))?;
    if let Some(run_id) = body["source_run_id"].as_str().filter(|v| !v.is_empty()) {
        if !records(s, id, "run_binding")?
            .into_iter()
            .any(|v| v["payload"]["run_id"] == run_id)
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.run_not_bound",
                "资产来源 Run 不属于该 Task",
            ));
        }
    }
    for reference in body["artifact_refs"].as_array().into_iter().flatten() {
        verify_ref(s, id, reference)?;
    }
    let attrs = body["attributes"].as_object().cloned().unwrap_or_default();
    let key_identity = normalized_identity(asset_type, &identity, Some((source_kind, source_id)))?;
    let key = asset_identity_key(asset_type, &key_identity);
    let storage_id = rec_key(id, "asset", &key);
    let source_key = body["source_event_id"]
        .as_str()
        .filter(|event| !event.is_empty())
        .map(|event| format!("{id}:{source_kind}:{source_id}:{event}"));
    let observed_at = projected_observed_at(&body["observed_at"]);
    let source_payload = json!({"task_id":id,"asset_type":asset_type,"identity":identity,
        "source_kind":source_kind,"source_id":source_id,"source_run_id":body["source_run_id"],
        "source_event_seq":body["source_event_seq"],"observed_at":observed_at,
        "attributes":attrs,"raw_values":body["raw_values"].as_object().cloned().unwrap_or_default(),
        "parser_version":body["parser_version"].as_str().unwrap_or("asset-observation.v1"),
        "source_event_id":body["source_event_id"]});
    loop {
        if let Some(command_id) = command_id {
            if let Some(marker) = s.store.get(ASSET_OBSERVE_APPLIED, command_id).map_err(db)? {
                return Ok(marker["output"].clone());
            }
        }
        if let Some(source_key) = source_key.as_ref() {
            if let Some(prior) = s.store.get(ASSET_SOURCE_EVENTS, source_key).map_err(db)? {
                let mut prior_payload = prior["payload"].clone();
                prior_payload["observed_at"] = projected_observed_at(&prior_payload["observed_at"]);
                if prior_payload != source_payload {
                    return Err(Error::conflict(
                        "source event ID already refers to a different observation",
                    ));
                }
                let output = prior["output"].clone();
                if let Some(command_id) = command_id {
                    s.store
                        .put_if_absent(
                            ASSET_OBSERVE_APPLIED,
                            command_id,
                            &json!({"command_id":command_id,"output":output}),
                        )
                        .map_err(db)?;
                    return Ok(s
                        .store
                        .get(ASSET_OBSERVE_APPLIED, command_id)
                        .map_err(db)?
                        .ok_or_else(|| Error::conflict("asset observation marker disappeared"))?
                        ["output"]
                        .clone());
                }
                return Ok(output);
            }
        }
        let existing = s.store.get(RECORDS, &storage_id).map_err(db)?;
        let aid = existing
            .as_ref()
            .and_then(|row| row["values"]["payload"]["asset_id"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("asset-{}", Uuid::new_v4().simple()));
        if let Some(event) = body["source_event_id"].as_str().filter(|v| !v.is_empty()) {
            if let Some(prior) = records(s, id, "asset_observation")?
                .into_iter()
                .map(|v| v["payload"].clone())
                .find(|v| {
                    v["source_kind"] == source_kind
                        && v["source_id"] == source_id
                        && v["source_event_id"] == event
                })
            {
                if prior["asset_id"] != aid || prior["attributes"] != json!(attrs) {
                    return Err(Error::conflict(
                        "source event ID already refers to a different observation",
                    ));
                }
                let asset = asset_row(s, id, &aid)?;
                let output =
                    json!({"action":"deduplicated","asset":asset["payload"],"observation":prior});
                if let Some(command_id) = command_id {
                    s.store
                        .put_if_absent(
                            ASSET_OBSERVE_APPLIED,
                            command_id,
                            &json!({"command_id":command_id,"output":output}),
                        )
                        .map_err(db)?;
                    let marker = s
                        .store
                        .get(ASSET_OBSERVE_APPLIED, command_id)
                        .map_err(db)?
                        .ok_or_else(|| Error::conflict("asset observation marker disappeared"))?;
                    return Ok(marker["output"].clone());
                }
                return Ok(output);
            }
        }
        let version = existing
            .as_ref()
            .and_then(|v| v["version"].as_u64())
            .unwrap_or(0);
        let mut asset = existing
            .as_ref()
            .map(|v| v["values"]["payload"].clone())
            .unwrap_or_else(|| json!({
                "asset_id":aid,"type":asset_type,"key_version":"asset-key.v1","natural_key":key,
                "identity":identity,"original_identifiers":[identity],"aliases":[],"lifecycle":"active",
                "judgements":{"company_attribution":{"state":"unknown"},"program_coverage":{"state":"unknown"},"active_test_conditions":{"state":"unknown"}},
                "current_profile":{},"profile_sources":{},"conflicts":{},"observation_count":0,
            }));
        if version==0{
            if let Some(judgements)=body["judgements"].as_object(){asset["judgements"]=Value::Object(judgements.clone());}
            if body.get("active_operation_authorized").is_some() {
                asset["active_operation_authorized"] = body["active_operation_authorized"].clone();
            }
            if body["observation_role"].is_string() {
                asset["observation_role"] = body["observation_role"].clone();
            }
            if body["source_batch_id"].is_string() {
                asset["source_batch_id"] = body["source_batch_id"].clone();
            }
        }
        if matches!(asset["lifecycle"].as_str(), Some("merged" | "destroyed")) {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "version_conflict",
                "asset was merged or destroyed",
            ));
        }
        asset["lifecycle"] = json!("active");
        if let Some(aliases) = body["aliases"].as_array() {
            let rows = asset["aliases"]
                .as_array_mut()
                .ok_or_else(|| Error::invalid("asset aliases invalid"))?;
            for alias in aliases {
                if alias.is_string() && !rows.contains(alias) {
                    rows.push(alias.clone());
                }
            }
        }
        let observation_id = format!("sobs-{}", Uuid::new_v4().simple());
        let mut observation = json!({"observation_id":observation_id,"task_id":id,"asset_id":aid,
        "asset_type":asset_type,"source_kind":source_kind,"source_id":source_id,
        "source_run_id":body["source_run_id"],"source_event_seq":body["source_event_seq"],
        "source_event_id":body["source_event_id"],"artifact_refs":body["artifact_refs"].as_array().cloned().unwrap_or_default(),
        "observed_at":observed_at,"ingested_at":now(),"attributes":attrs,
        "raw_values":if body["raw_values"].as_object().is_some_and(|raw|!raw.is_empty())
            {body["raw_values"].clone()}else{identity.clone()},
        "parser_version":body["parser_version"].as_str().unwrap_or("asset-observation.v1"),
        "completeness":body["completeness"].as_str().unwrap_or("complete"),"success":body["success"],
        "manual":body["manual"].as_bool().unwrap_or(false),"correction_of":body["correction_of"]});
        if body.get("active_operation_authorized").is_some() {
            observation["active_operation_authorized"] = body["active_operation_authorized"].clone();
        }
        if body["observation_role"].is_string() {
            observation["observation_role"] = body["observation_role"].clone();
        }
        if body["source_batch_id"].is_string() {
            observation["source_batch_id"] = body["source_batch_id"].clone();
        }
        let mut asset_observations: Vec<_> = records(s, id, "asset_observation")?
            .into_iter()
            .map(|row| row["payload"].clone())
            .filter(|row| row["asset_id"] == aid && row["revoked_at"].is_null())
            .collect();
        asset_observations.push(observation.clone());
        asset = reproject_asset(asset, &asset_observations);
        let output = json!({"action":if version==0{"created"}else{"merged"},
        "asset":asset,"observation":observation});
        let (observation_key, observation_version, observation_row) = stage_rec(
            s,
            id,
            "asset_observation",
            &observation_id,
            observation,
            Some(0),
        )?;
        let (asset_key, asset_version, asset_row) = stage_rec(s, id, "asset", &key, asset, None)?;
        if asset_version != version {
            continue;
        }
        let observation_document =
            json!({"version":observation_version+1,"values":observation_row});
        let asset_document = json!({"version":asset_version+1,"values":asset_row});
        let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":output}));
        let mut conditions = vec![
            (RECORDS, observation_key.as_str(), None),
            (RECORDS, asset_key.as_str(), existing.as_ref()),
        ];
        let mut writes = vec![
            (RECORDS, observation_key.as_str(), &observation_document),
            (RECORDS, asset_key.as_str(), &asset_document),
        ];
        if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
            conditions.push((ASSET_OBSERVE_APPLIED, command_id, None));
            writes.push((ASSET_OBSERVE_APPLIED, command_id, marker));
        }
        let source_record = source_key
            .as_ref()
            .map(|_| json!({"payload":source_payload,"output":output}));
        if let (Some(source_key), Some(source_record)) =
            (source_key.as_ref(), source_record.as_ref())
        {
            conditions.push((ASSET_SOURCE_EVENTS, source_key.as_str(), None));
            writes.push((ASSET_SOURCE_EVENTS, source_key.as_str(), source_record));
        }
        if s.store.put_many_if(&conditions, &writes).map_err(db)? {
            return Ok(output);
        }
    }
}

pub(super) fn apply_asset_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    match kind {
        "src.asset.observe" => observe_asset(s, id, body, Some(command_id)),
        "src.asset.correct" => correct_asset(s, id, body, Some(command_id), actor_id),
        "src.asset.relate" => asset_relate(s, id, body, Some(command_id)),
        "src.asset.derive" => derive_asset_relations(s, id, body, Some(command_id)),
        "src.asset.lifecycle" => asset_lifecycle(s, id, body, Some(command_id), actor_id),
        "src.asset.destroy" => asset_destroy(s, id, body, Some(command_id), actor_id, None),
        "src.asset.merge" => asset_merge(s, id, body, Some(command_id), actor_id),
        "src.asset.split" => split_asset(s, id, body, Some(command_id), actor_id),
        "src.asset.merge.revert" => asset_merge_revert(s, id, body, Some(command_id), actor_id),
        _ => Err(Error::invalid("unsupported asset command")),
    }
}
fn asset_row(s: &AppState, id: &str, aid: &str) -> Api<Value> {
    rec(s, id, "asset", aid)
}
fn correct_asset(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    let aid = require(body, "asset_id")?;
    let asset = asset_row(s, id, aid)?;
    if !body["attributes"].is_object() {
        return Err(Error::invalid("attributes must be an object"));
    }
    let reason = body["reason"]
        .as_str()
        .ok_or_else(|| Error::invalid("reason is required"))?;
    observe_asset(
        s,
        id,
        &json!({"asset_type":asset["payload"]["type"],"identity":asset["payload"]["identity"],
        "source_kind":"manual","source_id":actor_id,
        "attributes":body["attributes"],"raw_values":{"reason":reason},"success":true,
        "manual":true,"correction_of":body["correction_of"]}),
        command_id,
    )
}
fn lifecycle_prior(s: &AppState, command_id: Option<&str>) -> Api<Option<Value>> {
    command_id
        .map(|command_id| {
            s.store
                .get(ASSET_LIFECYCLE_APPLIED, command_id)
                .map_err(db)
                .map(|marker| marker.map(|marker| marker["output"].clone()))
        })
        .transpose()
        .map(Option::flatten)
}
fn asset_lifecycle(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    let aid = require(body, "asset_id")?;
    let action = require(body, "action")?;
    let expected_revision = match body.get("expected_revision") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| Error::invalid("expected_revision must be an integer"))?,
        ),
    };
    if action == "destroy" {
        return asset_destroy(s, id, body, command_id, actor_id, expected_revision);
    }
    if !matches!(action, "detach" | "archive" | "restore") {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "asset_lifecycle_invalid",
            format!("unsupported lifecycle action {action}"),
        ));
    }
    loop {
        if let Some(output) = lifecycle_prior(s, command_id)? {
            return Ok(output);
        }
        let asset_record = asset_row(s, id, aid)?;
        let asset_revision = asset_record["revision"].as_u64().unwrap_or(0);
        if expected_revision.is_some_and(|revision| revision != asset_revision) {
            return Err(Error::conflict("asset revision is stale"));
        }
        let mut conditions = Vec::new();
        let mut writes = Vec::new();
        let mut documents = Vec::new();
        let output = if action == "detach" {
            let link_key = rec_key(id, "task_asset_link", aid);
            let old_link = s.store.get(RECORDS, &link_key).map_err(db)?;
            let mut link = old_link
                .as_ref()
                .map(|record| record["values"]["payload"].clone())
                .unwrap_or_else(|| json!({"asset_id":aid}));
            link["state"] = json!("detached");
            link["actor_id"] = json!(actor_id);
            link["updated_at"] = json!(now());
            let (_, version, link_row) =
                stage_rec(s, id, "task_asset_link", aid, link.clone(), None)?;
            documents.push(json!({"version":version+1,"values":link_row}));
            conditions.push((RECORDS, link_key, old_link));
            writes.push((RECORDS, conditions.last().unwrap().1.clone(), 0usize));
            link
        } else {
            let asset_key = rec_key(
                id,
                "asset",
                asset_record["natural_key"].as_str().unwrap_or(""),
            );
            let old_asset = s.store.get(RECORDS, &asset_key).map_err(db)?;
            let mut asset = asset_record["payload"].clone();
            asset["lifecycle"] = json!(if action == "archive" {
                "archived"
            } else {
                "active"
            });
            asset["lifecycle_actor"] = json!(actor_id);
            let (_, version, next_asset) = stage_rec(
                s,
                id,
                "asset",
                asset_record["natural_key"].as_str().unwrap_or(""),
                asset.clone(),
                Some(asset_revision),
            )?;
            documents.push(json!({"version":version+1,"values":next_asset}));
            conditions.push((RECORDS, asset_key.clone(), old_asset));
            writes.push((RECORDS, asset_key, 0usize));

            let history_id = format!("asset-lifecycle-{}", Uuid::new_v4().simple());
            let history = json!({"asset_id":aid,"action":action,"actor_id":actor_id,
                "before":asset_record["payload"],"after":asset,"at":now()});
            let (history_key, history_version, mut history_row) =
                stage_rec(s, id, "asset_lifecycle", &history_id, history, Some(0))?;
            history_row["record_id"] = json!(history_id);
            history_row["natural_key"] = json!("");
            documents.push(json!({"version":history_version+1,"values":history_row}));
            conditions.push((RECORDS, history_key.clone(), None));
            writes.push((RECORDS, history_key, 1usize));
            asset
        };
        let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":output}));
        if let Some(command_id) = command_id {
            conditions.push((ASSET_LIFECYCLE_APPLIED, command_id.to_owned(), None));
            writes.push((
                ASSET_LIFECYCLE_APPLIED,
                command_id.to_owned(),
                documents.len(),
            ));
        }
        if let Some(marker) = marker {
            documents.push(marker);
        }
        let expected: Vec<_> = conditions
            .iter()
            .map(|(collection, key, value)| (*collection, key.as_str(), value.as_ref()))
            .collect();
        let records: Vec<_> = writes
            .iter()
            .map(|(collection, key, index)| (*collection, key.as_str(), &documents[*index]))
            .collect();
        if s.store.put_many_if(&expected, &records).map_err(db)? {
            return Ok(output);
        }
    }
}

fn asset_destroy(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
    expected_revision: Option<u64>,
) -> Api<Value> {
    let aid = require(body, "asset_id")?;
    loop {
        if let Some(output) = lifecycle_prior(s, command_id)? {
            return Ok(output);
        }
        let asset_record = match asset_row(s, id, aid) {
            Ok(record) => record,
            Err(error) => {
                if let Some(output) = lifecycle_prior(s, command_id)? {
                    return Ok(output);
                }
                return Err(error);
            }
        };
        if expected_revision.is_some_and(|revision| asset_record["revision"] != revision) {
            return Err(Error::conflict("asset revision is stale"));
        }
        let natural_key = asset_record["natural_key"].as_str().unwrap_or("");
        let tombstone = json!({"asset_id":aid,"type":asset_record["payload"]["type"],
            "natural_key":natural_key,"lifecycle":"destroyed","destroyed_by":actor_id,
            "destroyed_at":now(),"identity":asset_record["payload"]["identity"]});
        let (tombstone_key, version, tombstone_row) = stage_rec(
            s,
            id,
            "asset_tombstone",
            natural_key,
            tombstone.clone(),
            Some(0),
        )?;
        let tombstone_doc = json!({"version":version+1,"values":tombstone_row});
        let asset_key = rec_key(id, "asset", natural_key);
        let old_asset = s.store.get(RECORDS, &asset_key).map_err(db)?;
        let marker =
            command_id.map(|command_id| json!({"command_id":command_id,"output":tombstone}));
        let mut expected = vec![
            (RECORDS, asset_key.as_str(), old_asset.as_ref()),
            (RECORDS, tombstone_key.as_str(), None),
        ];
        let mut writes = vec![(RECORDS, tombstone_key.as_str(), &tombstone_doc)];
        if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
            expected.push((ASSET_LIFECYCLE_APPLIED, command_id, None));
            writes.push((ASSET_LIFECYCLE_APPLIED, command_id, marker));
        }
        let mut protected = Vec::new();
        let committed = s
            .store
            .put_many_if_with_deletes(&expected, &writes, |transaction| {
                let mut statement =
                    transaction.prepare("SELECT id,value FROM documents WHERE collection=?1")?;
                let rows = statement.query_map([RECORDS], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let mut deletes = Vec::new();
                for row in rows {
                    let (storage_id, serialized) = row?;
                    let document: Value = serde_json::from_str(&serialized)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    let record = &document["values"];
                    let kind = record["kind"].as_str().unwrap_or("");
                    let task_id = record["task_id"].as_str().unwrap_or("");
                    let payload = &record["payload"];
                    let intrinsic = matches!(
                        kind,
                        "asset"
                            | "asset_observation"
                            | "asset_tombstone"
                            | "asset_alias"
                            | "asset_lifecycle"
                    );
                    if !intrinsic
                        && !(kind == "task_asset_link" && task_id == id)
                        && payload.to_string().contains(aid)
                    {
                        protected.push(json!({"task_id":task_id,"kind":kind,
                        "record_id":record["record_id"]}));
                    }
                    if task_id == id
                        && (kind == "asset" && record["record_id"] == aid
                            || matches!(
                                kind,
                                "asset_observation" | "asset_relation" | "task_asset_link"
                            ) && (payload["asset_id"] == aid
                                || payload["from_asset_id"] == aid
                                || payload["to_asset_id"] == aid))
                    {
                        deletes.push((RECORDS.to_owned(), storage_id));
                    }
                }
                Ok(protected.is_empty().then_some(deletes))
            })
            .map_err(db)?;
        if !protected.is_empty() {
            return Err(Error {
                status: StatusCode::BAD_REQUEST,
                code: "artifact_in_use",
                message: "asset is protected by shared, report, relation or baseline references"
                    .into(),
                detail: json!({"references":protected}),
            });
        }
        if committed {
            return Ok(tombstone);
        }
    }
}
fn restructure_prior(s: &AppState, command_id: Option<&str>) -> Api<Option<Value>> {
    command_id
        .map(|command_id| {
            s.store
                .get(ASSET_RESTRUCTURE_APPLIED, command_id)
                .map_err(db)
                .map(|marker| marker.map(|marker| marker["output"].clone()))
        })
        .transpose()
        .map(Option::flatten)
}
fn stage_asset_change(
    s: &AppState,
    id: &str,
    kind: &str,
    record: &Value,
    payload: Value,
) -> Api<(String, Option<Value>, Value)> {
    let key = if kind == "asset_observation" {
        record["payload"]["observation_id"].as_str().unwrap_or("")
    } else {
        record["natural_key"].as_str().unwrap_or("")
    };
    let (storage_id, version, staged) =
        stage_rec(s, id, kind, key, payload, record["revision"].as_u64())?;
    Ok((
        storage_id,
        Some(json!({"version":record["revision"],"values":record})),
        json!({"version":version+1,"values":staged}),
    ))
}
fn asset_snapshot_keys(id: &str, kind: &str, records: &[Value]) -> Vec<String> {
    let mut keys = records
        .iter()
        .map(|record| {
            let key = if kind == "asset_observation" {
                record["payload"]["observation_id"].as_str().unwrap_or("")
            } else {
                record["natural_key"].as_str().unwrap_or("")
            };
            rec_key(id, kind, key)
        })
        .collect::<Vec<_>>();
    keys.sort();
    keys
}
fn commit_restructure(
    s: &AppState,
    id: &str,
    changes: &[(String, Option<Value>, Value)],
    read_only: &[(String, Value)],
    snapshots: &[(&str, Vec<String>)],
    delete_alias: Option<&(String, Option<Value>)>,
    command_id: Option<&str>,
    output: &Value,
) -> Api<bool> {
    let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":output}));
    let mut expected = changes
        .iter()
        .map(|(key, old, _)| (RECORDS, key.as_str(), old.as_ref()))
        .collect::<Vec<_>>();
    expected.extend(
        read_only
            .iter()
            .map(|(key, old)| (RECORDS, key.as_str(), Some(old))),
    );
    let mut writes = changes
        .iter()
        .map(|(key, _, next)| (RECORDS, key.as_str(), next))
        .collect::<Vec<_>>();
    if let Some((key, old)) = delete_alias {
        expected.push((RECORDS, key.as_str(), old.as_ref()));
    }
    if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
        expected.push((ASSET_RESTRUCTURE_APPLIED, command_id, None));
        writes.push((ASSET_RESTRUCTURE_APPLIED, command_id, marker));
    }
    s.store
        .put_many_if_with_deletes(&expected, &writes, |transaction| {
            for (kind, keys) in snapshots {
                let mut statement = transaction.prepare(
                    "SELECT id FROM documents WHERE collection=?1 \
                 AND json_extract(value,'$.values.task_id')=?2 \
                 AND json_extract(value,'$.values.kind')=?3 ORDER BY id",
                )?;
                let current = statement
                    .query_map(rusqlite::params![RECORDS, id, kind], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if current != *keys {
                    return Ok(None);
                }
            }
            Ok(Some(
                delete_alias
                    .map(|(key, _)| vec![(RECORDS.to_owned(), key.clone())])
                    .unwrap_or_default(),
            ))
        })
        .map_err(db)
}
fn asset_merge(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    let source = require(body, "source_asset_id")?;
    let target = require(body, "target_asset_id")?;
    let reason = body["reason"]
        .as_str()
        .ok_or_else(|| Error::invalid("reason is required"))?;
    if source == target {
        return Err(Error::conflict("cannot merge an asset into itself"));
    }
    loop {
        if let Some(output) = restructure_prior(s, command_id)? {
            return Ok(output);
        }
        let from = asset_row(s, id, source)?;
        let to = asset_row(s, id, target)?;
        if from["payload"]["type"] != to["payload"]["type"] {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "asset_type_invalid",
                "only assets of the same type can be merged",
            ));
        }
        let observations = records(s, id, "asset_observation")?;
        let relations = records(s, id, "asset_relation")?;
        let snapshots = [
            (
                "asset_observation",
                asset_snapshot_keys(id, "asset_observation", &observations),
            ),
            (
                "asset_relation",
                asset_snapshot_keys(id, "asset_relation", &relations),
            ),
        ];
        let mut changes = Vec::new();
        let mut target_observations = Vec::new();
        for row in &observations {
            let current_asset = row["payload"]["asset_id"].as_str().unwrap_or("");
            if current_asset == source {
                let mut observation = row["payload"].clone();
                observation["asset_id"] = json!(target);
                if observation["original_asset_id"].is_null()
                    || observation["original_asset_id"] == ""
                {
                    observation["original_asset_id"] = json!(source);
                }
                changes.push(stage_asset_change(
                    s,
                    id,
                    "asset_observation",
                    row,
                    observation.clone(),
                )?);
                if observation["revoked_at"].is_null() {
                    target_observations.push(observation);
                }
            } else if current_asset == target && row["payload"]["revoked_at"].is_null() {
                target_observations.push(row["payload"].clone());
            }
        }
        for row in &relations {
            let mut relation = row["payload"].clone();
            let mut changed = false;
            for field in ["from_asset_id", "to_asset_id"] {
                if relation[field] == source {
                    relation[field] = json!(target);
                    changed = true;
                }
            }
            if changed {
                relation["remapped_from"] = json!(source);
                changes.push(stage_asset_change(s, id, "asset_relation", row, relation)?);
            }
        }
        let mut merged = from["payload"].clone();
        merged["lifecycle"] = json!("merged");
        merged["merged_into"] = json!(target);
        merged["merge_reason"] = json!(reason);
        merged["merged_by"] = json!(actor_id);
        changes.push(stage_asset_change(s, id, "asset", &from, merged.clone())?);

        let source_key = from["natural_key"].as_str().unwrap_or("");
        let alias = json!({"source_asset_id":source,"target_asset_id":target,
            "natural_key":source_key,"created_at":now()});
        let alias_key = rec_key(id, "asset_alias", source_key);
        let alias_old = s.store.get(RECORDS, &alias_key).map_err(db)?;
        let (alias_key, alias_version, alias_row) =
            stage_rec(s, id, "asset_alias", source_key, alias, None)?;
        changes.push((
            alias_key,
            alias_old,
            json!({"version":alias_version+1,"values":alias_row}),
        ));

        let mut combined = to["payload"].clone();
        let mut aliases = combined["aliases"].as_array().cloned().unwrap_or_default();
        for alias in from["payload"]["aliases"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned()
            .chain(std::iter::once(json!(source_key)))
        {
            if !aliases.contains(&alias) {
                aliases.push(alias);
            }
        }
        combined["aliases"] = json!(aliases);
        let mut identifiers = combined["original_identifiers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        identifiers.extend(
            from["payload"]["original_identifiers"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
        combined["original_identifiers"] = json!(identifiers);
        let projected = reproject_asset(combined, &target_observations);
        changes.push(stage_asset_change(s, id, "asset", &to, projected.clone())?);
        let output = json!({"source":merged,"target":projected});
        if commit_restructure(s, id, &changes, &[], &snapshots, None, command_id, &output)? {
            return Ok(output);
        }
    }
}
fn asset_merge_revert(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    let source_id = require(body, "source_asset_id")?;
    loop {
        if let Some(output) = restructure_prior(s, command_id)? {
            return Ok(output);
        }
        let source_row = asset_row(s, id, source_id)?;
        if source_row["payload"]["lifecycle"] != "merged" {
            return Err(Error::conflict(
                "asset is not in the merged lifecycle state",
            ));
        }
        let target_id = source_row["payload"]["merged_into"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::new(
                    StatusCode::NOT_FOUND,
                    "artifact_missing",
                    "merged asset has no merge target",
                )
            })?;
        let target_row = asset_row(s, id, target_id)?;
        let observations = records(s, id, "asset_observation")?;
        let relations = records(s, id, "asset_relation")?;
        let snapshots = [
            (
                "asset_observation",
                asset_snapshot_keys(id, "asset_observation", &observations),
            ),
            (
                "asset_relation",
                asset_snapshot_keys(id, "asset_relation", &relations),
            ),
        ];
        let mut changes = Vec::new();
        let mut source_observations = Vec::new();
        let mut target_observations = Vec::new();
        for row in &observations {
            let current_asset = row["payload"]["asset_id"].as_str().unwrap_or("");
            if current_asset == target_id && row["payload"]["original_asset_id"] == source_id {
                let mut observation = row["payload"].clone();
                observation["asset_id"] = json!(source_id);
                observation["unmerged_from"] = json!(target_id);
                observation
                    .as_object_mut()
                    .unwrap()
                    .remove("original_asset_id");
                changes.push(stage_asset_change(
                    s,
                    id,
                    "asset_observation",
                    row,
                    observation.clone(),
                )?);
                if observation["revoked_at"].is_null() {
                    source_observations.push(observation);
                }
            } else if current_asset == source_id && row["payload"]["revoked_at"].is_null() {
                source_observations.push(row["payload"].clone());
            } else if current_asset == target_id && row["payload"]["revoked_at"].is_null() {
                target_observations.push(row["payload"].clone());
            }
        }
        for row in &relations {
            if row["payload"]["remapped_from"] != source_id {
                continue;
            }
            let mut relation = row["payload"].clone();
            for field in ["from_asset_id", "to_asset_id"] {
                if relation[field] == target_id {
                    relation[field] = json!(source_id);
                }
            }
            relation.as_object_mut().unwrap().remove("remapped_from");
            changes.push(stage_asset_change(s, id, "asset_relation", row, relation)?);
        }
        let source_key = source_row["natural_key"].as_str().unwrap_or("");
        let alias_key = rec_key(id, "asset_alias", source_key);
        let alias_old = s.store.get(RECORDS, &alias_key).map_err(db)?;
        let delete_alias = (alias_key, alias_old);

        let mut restored = source_row["payload"].clone();
        for field in ["merged_into", "merge_reason", "merged_by"] {
            restored.as_object_mut().unwrap().remove(field);
        }
        restored["lifecycle"] = json!("active");
        restored["unmerged_by"] = json!(actor_id);
        restored = reproject_asset(restored, &source_observations);
        changes.push(stage_asset_change(
            s,
            id,
            "asset",
            &source_row,
            restored.clone(),
        )?);

        let mut target = target_row["payload"].clone();
        let source_aliases = source_row["payload"]["aliases"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(aliases) = target["aliases"].as_array_mut() {
            aliases.retain(|alias| !source_aliases.contains(alias) && alias != source_key);
        }
        let source_identifiers = source_row["payload"]["original_identifiers"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(identifiers) = target["original_identifiers"].as_array_mut() {
            identifiers.retain(|value| !source_identifiers.contains(value));
        }
        target = reproject_asset(target, &target_observations);
        changes.push(stage_asset_change(
            s,
            id,
            "asset",
            &target_row,
            target.clone(),
        )?);
        let output = json!({"source":restored,"target":target});
        if commit_restructure(
            s,
            id,
            &changes,
            &[],
            &snapshots,
            Some(&delete_alias),
            command_id,
            &output,
        )? {
            return Ok(output);
        }
    }
}
fn observed_instant(value: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Some(seconds) = value.as_f64() {
        return chrono::DateTime::from_timestamp_micros((seconds * 1_000_000.0).round() as i64);
    }
    let raw = value.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.with_timezone(&chrono::Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|value| value.and_utc())
        })
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|value| value.and_utc())
        })
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
                .map(|value| value.and_utc())
        })
}
fn projected_observed_at(value: &Value) -> Value {
    observed_instant(value)
        .map(|at| {
            let at = chrono::DateTime::from_timestamp_micros(at.timestamp_micros()).unwrap();
            let format = if at.timestamp_subsec_micros() == 0 {
                chrono::SecondsFormat::Secs
            } else {
                chrono::SecondsFormat::Micros
            };
            json!(at.to_rfc3339_opts(format, false))
        })
        .unwrap_or_else(|| {
            if value.is_null() || value == "" {
                Value::Null
            } else {
                value.clone()
            }
        })
}
fn relation_iso(value: &Value) -> Api<Value> {
    if value.is_null() || value == "" {
        return Ok(Value::Null);
    }
    if observed_instant(value).is_none() {
        return Err(Error::invalid("invalid asset relation date"));
    }
    Ok(projected_observed_at(value))
}
fn reproject_asset(mut asset: Value, observations: &[Value]) -> Value {
    let mut ordered: Vec<_> = observations.iter().collect();
    ordered.sort_by(|left, right| {
        let left_time = projected_observed_at(&left["observed_at"]);
        let right_time = projected_observed_at(&right["observed_at"]);
        left_time
            .as_str()
            .unwrap_or("")
            .cmp(right_time.as_str().unwrap_or(""))
            .then(
                left["observation_id"]
                    .as_str()
                    .cmp(&right["observation_id"].as_str()),
            )
    });
    asset["current_profile"] = json!({});
    asset["profile_sources"] = json!({});
    let mut first_observed: Option<String> = None;
    let mut latest_observed: Option<String> = None;
    let mut last_success: Option<String> = None;
    let mut last_failure: Option<String> = None;
    for observation in ordered {
        let observed_at = projected_observed_at(&observation["observed_at"]);
        let observed = observed_at.as_str().filter(|value| !value.is_empty());
        if let Some(observed) = observed {
            if first_observed
                .as_deref()
                .is_none_or(|first| observed < first)
            {
                first_observed = Some(observed.to_owned());
            }
            if latest_observed
                .as_deref()
                .is_none_or(|latest| observed > latest)
            {
                latest_observed = Some(observed.to_owned());
            }
            if observation["success"] == true
                && last_success.as_deref().is_none_or(|last| observed > last)
            {
                last_success = Some(observed.to_owned());
            }
            if observation["success"] == false
                && last_failure.as_deref().is_none_or(|last| observed > last)
            {
                last_failure = Some(observed.to_owned());
            }
        }
        if let Some(attrs) = observation["attributes"].as_object() {
            for (field, value) in attrs {
                let prior = &asset["profile_sources"][field];
                let manual = observation["manual"] == true;
                let replace = prior.is_null()
                    || manual
                    || (prior["manual"] != true
                        && prior["value"] != *value
                        && observed_instant(&observation["observed_at"]).is_some_and(|at| {
                            observed_instant(&prior["observed_at"])
                                .is_none_or(|previous| at > previous)
                        }));
                if replace {
                    asset["current_profile"][field] = value.clone();
                    asset["profile_sources"][field] = json!({
                        "observation_id":observation["observation_id"],"value":value,
                        "observed_at":observed_at,
                        "source_kind":observation["source_kind"],"source_id":observation["source_id"],
                        "manual":manual
                    });
                }
            }
        }
    }
    let mut conflicts = serde_json::Map::new();
    for observation in observations {
        let observed_at = projected_observed_at(&observation["observed_at"]);
        if let Some(attrs) = observation["attributes"].as_object() {
            for (field, value) in attrs {
                if value.is_null() {
                    continue;
                }
                let entries = conflicts.entry(field.clone()).or_insert_with(|| json!([]));
                let entries = entries.as_array_mut().unwrap();
                if !entries
                    .iter()
                    .any(|item| item["observation_id"] == observation["observation_id"])
                {
                    entries.push(json!({
                        "observation_id": observation["observation_id"],
                        "value": value,
                        "observed_at": observed_at,
                        "source_kind": observation["source_kind"],
                        "source_id": observation["source_id"],
                        "artifact_refs": observation["artifact_refs"].as_array().cloned().unwrap_or_default(),
                        "manual": observation["manual"] == true,
                        "reason": observation["reason"].as_str()
                            .or_else(|| observation["correction_reason"].as_str())
                            .unwrap_or("")
                    }));
                }
            }
        }
    }
    conflicts.retain(|_, entries| {
        let entries = entries.as_array().unwrap();
        entries
            .iter()
            .any(|row| row["value"] != entries[0]["value"])
    });
    asset["conflicts"] = Value::Object(conflicts);
    asset["first_observed"] = json!(first_observed);
    asset["latest_observed"] = json!(latest_observed);
    asset["last_success"] = json!(last_success);
    asset["last_failure"] = json!(last_failure);
    asset["observation_count"] = json!(observations.len());
    asset
}
fn split_asset(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    let original_id = require(body, "merged_asset_id")?;
    let ids = body["observation_ids"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::invalid("split requires selected observations"))?;
    loop {
        if let Some(output) = restructure_prior(s, command_id)? {
            return Ok(output);
        }
        let original = asset_row(s, id, original_id)?;
        let observations = records(s, id, "asset_observation")?;
        let snapshots = [(
            "asset_observation",
            asset_snapshot_keys(id, "asset_observation", &observations),
        )];
        let mut selected = std::collections::HashSet::new();
        for oid in ids {
            let oid = oid
                .as_str()
                .ok_or_else(|| Error::invalid("invalid observation ID"))?;
            let row = observations.iter().find(|row| {
                row["record_id"] == oid
                    || row["natural_key"] == oid
                    || row["payload"]["observation_id"] == oid
            });
            let Some(row) = row.filter(|row| row["payload"]["asset_id"] == original_id) else {
                return Err(Error::new(
                    StatusCode::NOT_FOUND,
                    "src.artifact_missing",
                    "one or more selected observations do not belong to this asset",
                ));
            };
            selected.insert(row["record_id"].as_str().unwrap_or("").to_owned());
        }
        let new_id = format!("asset-{}", Uuid::new_v4().simple());
        let asset_type = original["payload"]["type"].as_str().unwrap_or("");
        let split_identity = normalized_identity(asset_type, &original["payload"]["identity"], None)?;
        let key = format!(
            "{}:split:{new_id}",
            asset_identity_key(asset_type, &split_identity)
        );
        let mut asset = original["payload"].clone();
        asset["asset_id"] = json!(new_id);
        asset["natural_key"] = json!(key);
        asset["key_version"] = json!("asset-key.v1");
        asset["lifecycle"] = json!("active");
        asset["split_from"] = json!(original_id);
        asset["split_by"] = json!(actor_id);
        asset["aliases"] = json!([]);
        asset.as_object_mut().unwrap().remove("merged_into");

        let (new_storage_id, _, new_row) = stage_rec(s, id, "asset", &key, asset.clone(), Some(0))?;
        let mut expected = vec![(new_storage_id.clone(), None)];
        let mut writes = vec![(
            new_storage_id.clone(),
            json!({"version":1,"values":new_row.clone()}),
        )];
        let mut remaining = Vec::new();
        let mut moved = Vec::new();
        for row in &observations {
            if row["payload"]["asset_id"] != original_id {
                continue;
            }
            let rid = rec_key(
                id,
                "asset_observation",
                row["payload"]["observation_id"].as_str().unwrap_or(""),
            );
            expected.push((
                rid.clone(),
                Some(json!({"version":row["revision"],"values":row})),
            ));
            if selected.contains(row["record_id"].as_str().unwrap_or("")) {
                let mut observation = row["payload"].clone();
                observation["asset_id"] = json!(new_id);
                observation["split_from_observation"] = row["payload"]["observation_id"]
                    .as_str()
                    .map(|v| json!(v))
                    .unwrap_or_else(|| row["record_id"].clone());
                if observation["revoked_at"].is_null() {
                    moved.push(observation.clone());
                }
                let (_, version, staged) = stage_rec(
                    s,
                    id,
                    "asset_observation",
                    row["payload"]["observation_id"].as_str().unwrap_or(""),
                    observation,
                    None,
                )?;
                writes.push((rid, json!({"version":version+1,"values":staged})));
            } else if row["payload"]["revoked_at"].is_null() {
                remaining.push(row["payload"].clone());
            }
        }
        let original_projected = reproject_asset(original["payload"].clone(), &remaining);
        let (original_storage_id, original_version, original_row) = stage_rec(
            s,
            id,
            "asset",
            original["natural_key"].as_str().unwrap_or(""),
            original_projected,
            None,
        )?;
        expected.push((
            original_storage_id.clone(),
            Some(json!({"version":original["revision"],"values":original})),
        ));
        writes.push((
            original_storage_id,
            json!({"version":original_version+1,"values":original_row}),
        ));
        let projected = reproject_asset(asset, &moved);
        let mut projected_row = new_row;
        projected_row["revision"] = json!(2);
        projected_row["updated_at"] = json!(now());
        projected_row["payload"] = projected.clone();
        writes.push((new_storage_id, json!({"version":2,"values":projected_row})));
        let mut changes = Vec::new();
        let mut read_only = Vec::new();
        for (key, old) in expected {
            if let Some((_, next)) = writes.iter().rev().find(|(write_key, _)| *write_key == key) {
                changes.push((key, old, next.clone()));
            } else if let Some(old) = old {
                read_only.push((key, old));
            }
        }
        if commit_restructure(
            s, id, &changes, &read_only, &snapshots, None, command_id, &projected,
        )? {
            return Ok(projected);
        }
    }
}
fn asset_relate(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(ASSET_RELATE_APPLIED, command_id)
            .map_err(super::db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let from = require(body, "from_asset_id")?;
    let to = require(body, "to_asset_id")?;
    asset_row(s, id, from)?;
    asset_row(s, id, to)?;
    let relation_type = require(body, "relation_type")?;
    let origin = require(body, "origin")?;
    if !matches!(origin, "observed" | "derived" | "inferred") {
        return Err(Error::invalid("invalid relation origin"));
    }
    let valid_from = relation_iso(&body["valid_from"])?;
    let valid_until = relation_iso(&body["valid_until"])?;
    let key = format!("rel:{from}:{to}:{relation_type}:{origin}");
    loop {
        if let Ok(prior) = rec(s, id, "asset_relation", &key) {
            let relation = prior["payload"].clone();
            if let Some(command_id) = command_id {
                s.store
                    .put_if_absent(
                        ASSET_RELATE_APPLIED,
                        command_id,
                        &json!({"command_id":command_id,"output":relation}),
                    )
                    .map_err(super::db)?;
                return Ok(s
                    .store
                    .get(ASSET_RELATE_APPLIED, command_id)
                    .map_err(super::db)?
                    .ok_or_else(|| Error::conflict("asset relation marker disappeared"))?["output"]
                    .clone());
            }
            return Ok(relation);
        }
        let relation = json!({"relation_id":format!("srel-{}",Uuid::new_v4().simple()),"task_id":id,
            "from_asset_id":from,"to_asset_id":to,"relation_type":relation_type,"origin":origin,
            "evidence_refs":body["evidence_refs"].as_array().cloned().unwrap_or_default(),
            "valid_from":valid_from,"valid_until":valid_until,"version":1});
        let (record_key, version, row) =
            stage_rec(s, id, "asset_relation", &key, relation.clone(), None)?;
        if version != 0 {
            continue;
        }
        let document = json!({"version":1,"values":row});
        let marker =
            command_id.map(|command_id| json!({"command_id":command_id,"output":relation}));
        let mut conditions = vec![(RECORDS, record_key.as_str(), None)];
        let mut writes = vec![(RECORDS, record_key.as_str(), &document)];
        if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
            conditions.push((ASSET_RELATE_APPLIED, command_id, None));
            writes.push((ASSET_RELATE_APPLIED, command_id, marker));
        }
        if s.store
            .put_many_if(&conditions, &writes)
            .map_err(super::db)?
        {
            return Ok(relation);
        }
    }
}
fn derive_asset_relations(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(ASSET_DERIVE_APPLIED, command_id)
            .map_err(super::db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let endpoint_id = require(body, "endpoint_asset_id")?;
    let endpoint = asset_row(s, id, endpoint_id)?;
    if !matches!(
        endpoint["payload"]["type"].as_str(),
        Some("http_endpoint" | "parameter")
    ) {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "asset_type_invalid",
            "endpoint asset required",
        ));
    }
    let identity = &endpoint["payload"]["identity"];
    let host = require(identity, "host")?;
    let host_type = if host.parse::<IpAddr>().is_ok() {
        "ip"
    } else {
        "domain"
    };
    let host_identity = if host_type == "ip" {
        json!({"address":host})
    } else {
        json!({"name":host})
    };
    let host_observation = json!({"asset_type":host_type,"identity":host_identity,
        "source_kind":"derived","source_id":endpoint_id,
        "raw_values":{"derived_from":endpoint_id}});
    let host_marker = command_id.map(|command_id| format!("{command_id}:host"));
    let host_asset = observe_asset(s, id, &host_observation, host_marker.as_deref())?;
    let service_observation = json!({"asset_type":"web_service",
        "identity":{"host":host,"port":identity["port"],"protocol":identity["scheme"]},
        "source_kind":"derived","source_id":endpoint_id,
        "raw_values":{"derived_from":endpoint_id}});
    let service_marker = command_id.map(|command_id| format!("{command_id}:service"));
    let service = observe_asset(s, id, &service_observation, service_marker.as_deref())?;
    let host_id = host_asset["asset"]["asset_id"].as_str().unwrap_or("");
    let service_id = service["asset"]["asset_id"].as_str().unwrap_or("");
    let a = asset_relate(
        s,
        id,
        &json!({"from_asset_id":endpoint_id,"to_asset_id":host_id,"relation_type":"hosted_by","origin":"derived",
            "evidence_refs":[endpoint_id]}),
        None,
    )?;
    let b = asset_relate(
        s,
        id,
        &json!({"from_asset_id":endpoint_id,"to_asset_id":service_id,"relation_type":"served_by","origin":"derived",
            "evidence_refs":[endpoint_id]}),
        None,
    )?;
    let output = json!([a, b]);
    if let Some(command_id) = command_id {
        s.store
            .put_if_absent(
                ASSET_DERIVE_APPLIED,
                command_id,
                &json!({"command_id":command_id,"output":output}),
            )
            .map_err(super::db)?;
        return Ok(s
            .store
            .get(ASSET_DERIVE_APPLIED, command_id)
            .map_err(super::db)?
            .ok_or_else(|| Error::conflict("asset derivation marker disappeared"))?["output"]
            .clone());
    }
    Ok(output)
}
fn find_dataset_record(s: &AppState, dataset_id: &str) -> Api<Value> {
    let row = s
        .store
        .list(RECORDS)
        .map_err(super::db)?
        .into_iter()
        .map(|stored| stored["values"].clone())
        .find(|row| {
            row["kind"] == "knowledge_dataset"
                && (row["record_id"] == dataset_id
                    || row["natural_key"] == dataset_id
                    || row["payload"]["dataset_id"] == dataset_id)
        })
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "knowledge dataset is unavailable",
            )
        })?;
    if !row["payload"]["archived_at"].is_null() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "source_revoked",
            "knowledge dataset is archived",
        ));
    }
    Ok(row)
}

fn check_dataset_acl(
    _s: &AppState,
    _dataset: &Value,
    _task_id: &str,
    _principal_id: &str,
    _action: &str,
) -> Api<()> {
    Ok(())
}

fn create_dataset(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let name = require(body, "name")?;
    let did = body["dataset_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("skdata-{}", Uuid::new_v4().simple()));
    let principal_id = body["principal_id"]
        .as_str()
        .or_else(|| body["actor_id"].as_str())
        .filter(|v| !v.is_empty())
        .unwrap_or("local-user");
    let parse_str_set = |field: &str, default_self: bool| -> Vec<String> {
        let mut set = std::collections::BTreeSet::new();
        if let Some(arr) = body[field].as_array().filter(|v|!v.is_empty()) {
            for item in arr.iter().filter_map(Value::as_str) {
                if !item.trim().is_empty() {
                    set.insert(item.trim().to_owned());
                }
            }
        } else if default_self {
            set.insert(id.to_owned());
        }
        set.into_iter().collect()
    };
    let reader_task_ids = parse_str_set("reader_task_ids", true);
    let contributor_task_ids = parse_str_set("contributor_task_ids", true);
    let reader_principal_ids = parse_str_set("reader_principal_ids", false);
    knowledge_queries::authorize(s, id, principal_id)?;
    if !reader_task_ids.iter().any(|t| t == id) || !contributor_task_ids.iter().any(|t| t == id) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "policy_denied",
            "dataset owner must retain read and publish access",
        ));
    }
    if find_dataset_record(s, &did).is_ok() {
        return Err(Error::conflict("knowledge dataset already exists"));
    }
    let value = json!({
        "dataset_id": did,
        "owner_task_id": id,
        "name": name,
        "purpose": body["purpose"],
        "reader_task_ids": reader_task_ids,
        "contributor_task_ids": contributor_task_ids,
        "reader_principal_ids": reader_principal_ids,
        "revision": 1,
        "created_by": principal_id,
        "access": "task",
        "authorization_expanded": false,
        "created_at": now(),
        "archived_at": null,
        "invalidated_at": null
    });
    save_rec(s, id, "knowledge_dataset", &did, value.clone(), Some(0))?;
    Ok(value)
}

fn ensure_private_dataset(s: &AppState, id: &str, principal_id: &str) -> Api<Value> {
    let did = format!("private:{id}");
    if let Ok(row) = find_dataset_record(s, &did) {
        return Ok(row["payload"].clone());
    }
    create_dataset(
        s,
        id,
        &json!({
            "dataset_id": did,
            "name": "Task 私有案例",
            "principal_id": principal_id,
            "reader_task_ids": [id],
            "contributor_task_ids": [id],
        }),
    )
}

fn knowledge_publish(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let idem=require(body,"idempotency_key")?;
    let request=json!({"source_kind":body["source_kind"],"source_id":body["source_id"],"source_revision":body["source_revision"],"selected_source_fields":body["selected_source_fields"],"entry":body["entry"],"evidence_refs":body["evidence_refs"],"dataset_id":body["dataset_id"].as_str().unwrap_or(""),"validity":body.get("validity").filter(|value|value.is_object()).cloned().unwrap_or(json!({})),"entry_id":body["entry_id"].as_str().unwrap_or("")});
    let idem_key=json!([id,idem]).to_string();
    if let Some(prior)=s.store.get("src_knowledge_publish_keys",&idem_key).map_err(db)?{if prior["request"]!=request{return Err(Error::conflict("knowledge idempotency key was reused with different input"));}return Ok(prior["output"].clone());}
    let principal_id = body["principal_id"]
        .as_str()
        .or_else(|| body["actor_id"].as_str())
        .filter(|v| !v.is_empty())
        .unwrap_or("local-user");
    let source_kind = require(body, "source_kind")?;
    let source_id = require(body, "source_id")?;
    let source_revision = body["source_revision"]
        .as_u64()
        .ok_or_else(|| Error::invalid("source_revision is required"))?;
    let selected_fields: Vec<String> = body["selected_source_fields"]
        .as_array()
        .filter(|arr| !arr.is_empty())
        .ok_or_else(|| Error::conflict("selected source fields are empty or unavailable"))?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if selected_fields.is_empty() {
        return Err(Error::conflict(
            "selected source fields are empty or unavailable",
        ));
    }
    let entry_obj = body["entry"]
        .as_object()
        .ok_or_else(|| Error::invalid("entry must be an object"))?;
    let kind = entry_obj
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !matches!(kind, "case" | "counterexample") {
        return Err(Error::conflict(
            "knowledge kind must be case or counterexample",
        ));
    }
    let source = rec(s, id, source_kind, source_id)?;
    if !source["payload"]["revoked_at"].is_null()
        || matches!(
            source["payload"]["status"].as_str(),
            Some("revoked" | "deleted")
        )
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "source_revoked",
            "a revoked source cannot be published",
        ));
    }
    if source["revision"].as_u64() != Some(source_revision) {
        return Err(Error::conflict("source revision changed before publication"));
    }
    let mut source_snapshot = serde_json::Map::new();
    for field in &selected_fields {
        let Some(val) = source["payload"].get(field) else {
            return Err(Error::conflict(
                "selected source fields are empty or unavailable",
            ));
        };
        source_snapshot.insert(field.clone(), val.clone());
    }
    let dataset_id_param = body["dataset_id"].as_str().map(str::trim).unwrap_or("");
    let dataset = if dataset_id_param.is_empty() {
        ensure_private_dataset(s, id, principal_id)?
    } else {
        find_dataset_record(s, dataset_id_param)?["payload"].clone()
    };
    check_dataset_acl(s, &dataset, id, principal_id, "publish")?;
    let dataset_id = dataset["dataset_id"].as_str().unwrap_or("");
    let dataset_owner = dataset["owner_task_id"].as_str().unwrap_or(id);

    let evidence_refs = body["evidence_refs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut pinned = Vec::with_capacity(evidence_refs.len());
    let mut dependencies = vec![json!({
        "task_id": id,
        "kind": source_kind,
        "record_id": source_id,
        "revision": source_revision,
    })];
    for reference in &evidence_refs {
        let verified = verify_ref(s, id, reference)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(verified["data"].as_str().unwrap_or(""))
            .map_err(|_| Error::invalid("knowledge evidence artifact data invalid"))?;
        let name = reference["name"].as_str().unwrap_or("knowledge-evidence");
        let media = reference["media_type"]
            .as_str()
            .unwrap_or("application/octet-stream");
        let copy = if dataset_owner == id {
            let mut r = reference.clone();
            if r["artifact_id"].is_null() {
                r["artifact_id"] = verified["artifact_id"].clone();
            }
            if r["version"].is_null() {
                r["version"] = json!(1);
            }
            r
        } else {
            put_artifact(s, dataset_owner, &bytes, name, media)?
        };
        dependencies.push(json!({
            "task_id": id,
            "kind": "artifact",
            "artifact_id": verified["artifact_id"],
            "revision": reference["version"].as_u64().unwrap_or(1),
        }));
        pinned.push(copy);
    }
    let entry_id = body["entry_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("sknowledge-{}", Uuid::new_v4().simple()));
    let existing_entry = rec(s, id, "knowledge_entry", &entry_id).ok();
    if let Some(existing) = &existing_entry {
        if existing["payload"]["current_dataset_id"] != dataset_id {
            return Err(Error::conflict(
                "knowledge entry is bound to a different dataset",
            ));
        }
    }
    let version = records(s, dataset_owner, "knowledge_publication")?
        .into_iter()
        .filter(|row| row["payload"]["entry_id"] == entry_id)
        .filter_map(|row| row["payload"]["version"].as_u64())
        .max()
        .unwrap_or(0)
        + 1;
    let publication_id = format!("skpub-{}", Uuid::new_v4().simple());
    let mut snapshot = Value::Object(entry_obj.clone());
    snapshot["kind"] = json!(kind);
    snapshot["reference_only"] = json!(true);
    snapshot["does_not_create_fact"] = json!(true);
    let validity = body["validity"]
        .as_object()
        .cloned()
        .map(Value::Object)
        .unwrap_or_else(|| json!({}));
    let publication = json!({
        "publication_id": publication_id,
        "entry_id": entry_id,
        "version": version,
        "dataset_id": dataset_id,
        "dataset_owner_task_id": dataset_owner,
        "owner_task_id": id,
        "task_id": id,
        "kind": kind,
        "entry_snapshot": snapshot,
        "source": {
            "kind": source_kind,
            "record_id": source_id,
            "revision": source_revision,
            "task_id": id,
        },
        "source_snapshot": Value::Object(source_snapshot),
        "dependencies": dependencies,
        "evidence": pinned,
        "validity": validity,
        "status": "active",
        "reason": "",
        "published_by": principal_id,
        "published_at": now(),
        "revoked_at": null,
        "revoked_by": "",
    });
    let entry_payload = json!({
        "entry_id": entry_id,
        "kind": kind,
        "current_publication_id": publication_id,
        "current_dataset_id": dataset_id,
        "current_dataset_owner_task_id": dataset_owner,
        "current_version": version,
        "status": "active",
        "reason": "",
    });
    let staged_publication=stage_rec(s,dataset_owner,"knowledge_publication",&publication_id,publication.clone(),Some(0))?;
    let staged_entry=stage_rec(s,id,"knowledge_entry",&entry_id,entry_payload,Some(existing_entry.as_ref().and_then(|row|row["revision"].as_u64()).unwrap_or(0)))?;
    let entry_before=s.store.get(RECORDS,&staged_entry.0).map_err(db)?;
    if entry_before.as_ref().and_then(|row|row["version"].as_u64()).unwrap_or(0)!=staged_entry.1{return Err(Error::conflict("knowledge entry changed"));}
    let pub_doc=json!({"version":staged_publication.1+1,"values":staged_publication.2});let entry_doc=json!({"version":staged_entry.1+1,"values":staged_entry.2});let remembered=json!({"request":request,"output":publication});
    let source_key=rec_key(id,source_kind,source["natural_key"].as_str().unwrap_or(source_id));let source_before=s.store.get(RECORDS,&source_key).map_err(db)?;
    if source_before.as_ref().and_then(|row|row["version"].as_u64())!=Some(source_revision){return Err(Error::conflict("source revision changed"));}
    if !s.store.put_many_if(&[(RECORDS,&staged_publication.0,None),(RECORDS,&staged_entry.0,entry_before.as_ref()),(RECORDS,&source_key,source_before.as_ref()),("src_knowledge_publish_keys",&idem_key,None)],&[(RECORDS,&staged_publication.0,&pub_doc),(RECORDS,&staged_entry.0,&entry_doc),("src_knowledge_publish_keys",&idem_key,&remembered)]).map_err(db)?{
        if let Some(prior)=s.store.get("src_knowledge_publish_keys",&idem_key).map_err(db)?{if prior["request"]==request{return Ok(prior["output"].clone());}}
        return Err(Error::conflict("knowledge publication changed during commit"));
    }
    Ok(publication)
}

fn revoke_publication(s:&AppState,id:&str,body:&Value)->Api<Value>{knowledge_status(s,id,body,"revoked")}
fn invalidate_knowledge(s:&AppState,id:&str,body:&Value)->Api<Value>{
    if require(body,"reason")?.is_empty(){return Err(Error::invalid("invalidation reason is required"));}
    knowledge_status(s,id,body,"stale")
}
fn knowledge_status(s:&AppState,id:&str,body:&Value,status:&str)->Api<Value>{
    let pid=require(body,"publication_id")?;let did=require(body,"dataset_id")?;let reason=body["reason"].as_str().ok_or_else(||Error::invalid("reason must be a string"))?;
    let principal_id=body["principal_id"].as_str().unwrap_or("local-user");let dataset=find_dataset_record(s,did)?["payload"].clone();check_dataset_acl(s,&dataset,id,principal_id,"revoke")?;
    let owner=require(&dataset,"owner_task_id")?;let row=rec(s,owner,"knowledge_publication",pid)?;let revision=body["expected_revision"].as_u64().ok_or_else(||Error::invalid("expected_revision is required"))?;
    if row["revision"].as_u64()!=Some(revision){return Err(Error::conflict("publication revision is stale"));}
    let mut value=row["payload"].clone();if value["dataset_id"]!=did{return Err(Error::new(StatusCode::FORBIDDEN,"policy_denied","publication does not belong to this dataset"));}
    value["status"]=json!(status);value["reason"]=json!(reason);if status=="revoked"{value["revoked_at"]=json!(now());value["revoked_by"]=json!(principal_id);}
    let mut staged=vec![stage_rec(s,owner,"knowledge_publication",row["natural_key"].as_str().filter(|key|!key.is_empty()).unwrap_or(pid),value.clone(),Some(revision))?];
    let entry_owner=require(&value,"owner_task_id")?;let entry_id=require(&value,"entry_id")?;
    match rec(s,entry_owner,"knowledge_entry",entry_id){Ok(entry)if entry["payload"]["current_publication_id"]==pid=>{let mut payload=entry["payload"].clone();payload["status"]=json!(status);payload["reason"]=json!(reason);staged.push(stage_rec(s,entry_owner,"knowledge_entry",entry["natural_key"].as_str().filter(|key|!key.is_empty()).unwrap_or(entry_id),payload,entry["revision"].as_u64())?);},Ok(_)=>{},Err(error)if error.status==StatusCode::NOT_FOUND=>{},Err(error)=>return Err(error)}
    let mut docs=Vec::new();for(key,version,row)in staged{let before=s.store.get(RECORDS,&key).map_err(db)?;if before.as_ref().and_then(|row|row["version"].as_u64()).unwrap_or(0)!=version{return Err(Error::conflict("knowledge state changed"));}docs.push((key,before,json!({"version":version+1,"values":row})));}
    let expected=docs.iter().map(|(key,before,_)|(RECORDS,key.as_str(),before.as_ref())).collect::<Vec<_>>();let writes=docs.iter().map(|(key,_,after)|(RECORDS,key.as_str(),after)).collect::<Vec<_>>();
    if !s.store.put_many_if(&expected,&writes).map_err(db)?{return Err(Error::conflict("knowledge state changed during commit"));}Ok(value)
}

fn knowledge_availability(s: &AppState, publication: &Value, context: Option<&Value>) -> Value {
    if publication["status"] == "revoked" || publication["revoked_at"].is_string() {
        return json!({
            "state": "revoked",
            "reason": publication["reason"].as_str().filter(|r| !r.is_empty()).unwrap_or("publication revoked")
        });
    }
    for dep in publication["dependencies"].as_array().into_iter().flatten() {
        if dep["kind"] == "artifact" {
            continue;
        }
        let dep_task = dep["task_id"].as_str().unwrap_or("");
        let dep_kind = dep["kind"].as_str().unwrap_or("");
        let dep_id = dep["record_id"].as_str().unwrap_or("");
        let Ok(record) = rec(s, dep_task, dep_kind, dep_id) else {
            return json!({"state": "unreadable", "reason": "source_missing"});
        };
        let payload = &record["payload"];
        if !payload["revoked_at"].is_null()
            || matches!(payload["status"].as_str(), Some("revoked" | "deleted"))
        {
            return json!({"state": "revoked", "reason": "source_revoked"});
        }
        let invalidated: Vec<String> = payload["invalidates"]
            .as_array()
            .or_else(|| payload["invalidated_refs"].as_array())
            .into_iter()
            .flatten()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            })
            .collect();
        let dep_rev = dep["revision"].to_string();
        if invalidated.iter().any(|r| r == &dep_rev || r == dep_id) {
            return json!({"state": "stale", "reason": "source_invalidated"});
        }
    }
    if let (Some(validity), Some(ctx)) = (
        publication["validity"].as_object(),
        context.and_then(Value::as_object),
    ) {
        let mut mismatches = Vec::new();
        for (k, expected) in validity {
            if ctx.get(k) != Some(expected) {
                mismatches.push(format!("{k}: expected {expected}, got {:?}", ctx.get(k)));
            }
        }
        if !mismatches.is_empty() {
            return json!({"state": "stale", "reason": mismatches.join("; ")});
        }
    }
    if publication["status"] == "stale" {
        return json!({"state": "stale", "reason": publication["reason"].as_str().unwrap_or("")});
    }
    json!({"state": "active", "reason": ""})
}

fn knowledge_search(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    knowledge_queries::search(s, id, p)
}
fn knowledge_read(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    knowledge_queries::read(s, id, p)
}
fn knowledge_source(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    knowledge_queries::source(s, id, p)
}
fn knowledge_history(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    knowledge_queries::history(s, id, p)
}
fn asset_detail(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    let aid = require(p, "asset_id")?;
    let row = asset_row(s, id, aid).map_err(|error| {
        if error.status == StatusCode::NOT_FOUND {
            Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                format!("asset/{aid} is unavailable"),
            )
        } else {
            error
        }
    })?;
    let inserted = s.store.list_inserted(RECORDS).map_err(db)?;
    let observations: Vec<Value> = inserted
        .iter()
        .map(|stored| &stored["values"])
        .filter(|record| record["task_id"] == id && record["kind"] == "asset_observation")
        .map(|record| record["payload"].clone())
        .filter(|payload| payload["asset_id"] == aid && payload["revoked_at"].is_null())
        .collect();
    let sources: std::collections::BTreeSet<(String, String)> = observations
        .iter()
        .map(|value| {
            (
                value["source_kind"].as_str().unwrap_or("").to_owned(),
                value["source_id"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let sources: Vec<Value> = sources
        .into_iter()
        .map(|(source_kind, source_id)| {
            json!({"source_kind":source_kind,"source_id":source_id})
        })
        .collect();
    let relations: Vec<Value> = inserted
        .iter()
        .map(|stored| &stored["values"])
        .filter(|record| record["task_id"] == id && record["kind"] == "asset_relation")
        .map(|record| record["payload"].clone())
        .filter(|payload| payload["from_asset_id"] == aid || payload["to_asset_id"] == aid)
        .collect();
    Ok(json!({"asset":row["payload"],"observations":observations,
        "sources":sources,"history":observations,"relations":relations}))
}
pub(super) fn asset_bulk(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    let mut rows = records(s, id, "asset")?;
    rows.sort_by(|a, b| a["record_id"].as_str().cmp(&b["record_id"].as_str()));
    let mut merged_by_target: std::collections::HashMap<String, Vec<Value>> =
        std::collections::HashMap::new();
    for row in &rows {
        let payload = &row["payload"];
        if payload["lifecycle"] == "merged"
            && let Some(target_aid) = payload["merged_into"].as_str().filter(|v| !v.is_empty())
        {
            let at = payload["type"].as_str().unwrap_or("");
            merged_by_target.entry(target_aid.to_owned()).or_default().push(json!({
                "asset_id": payload["asset_id"],
                "display_label": asset_display_label(at, &payload["identity"]),
                "type": at,
                "reason": payload["merge_reason"].as_str().unwrap_or("")
            }));
        }
    }
    for tomb in payloads(s, id, "asset_tombstone").unwrap_or_default() {
        if let Some(target_aid) = tomb["merged_into"]
            .as_str()
            .or_else(|| tomb["target_asset_id"].as_str())
            .filter(|v| !v.is_empty())
            && let Some(source_aid) = tomb["asset_id"].as_str()
        {
            let entry = merged_by_target.entry(target_aid.to_owned()).or_default();
            if !entry.iter().any(|m| m["asset_id"] == source_aid) {
                let at = tomb["type"].as_str().unwrap_or("");
                entry.push(json!({
                    "asset_id": source_aid,
                    "display_label": tomb["display_label"].as_str().filter(|v| !v.is_empty()).map(str::to_owned)
                        .unwrap_or_else(|| asset_display_label(at, &tomb["identity"])),
                    "type": at,
                    "reason": tomb["reason"].as_str().or_else(|| tomb["merge_reason"].as_str()).unwrap_or("")
                }));
            }
        }
    }
    let judgements_by_asset: std::collections::HashMap<String, Value> =
        payloads(s, id, "asset_judgement")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|j| Some((j["asset_id"].as_str()?.to_owned(), j)))
            .collect();

    let mut items = Vec::new();
    let observations = payloads(s, id, "asset_observation")?;
    let relations = payloads(s, id, "asset_relation")?;
    for row in rows.into_iter().filter(|v| {
        matches!(
            v["payload"]["lifecycle"].as_str(),
            None | Some("" | "active" | "archived")
        )
    }) {
        let mut asset = row["payload"].clone();
        let aid = asset["asset_id"].clone();
        let aid_str = aid.as_str().unwrap_or("");
        let obs: Vec<_> = observations
            .iter()
            .filter(|v| v["asset_id"] == aid && v["revoked_at"].is_null())
            .cloned()
            .collect();
        let rel: Vec<_> = relations
            .iter()
            .filter(|v| v["from_asset_id"] == aid || v["to_asset_id"] == aid)
            .cloned()
            .collect();
        let sources: std::collections::BTreeSet<(String, String)> = obs
            .iter()
            .map(|v| {
                (
                    v["source_kind"].as_str().unwrap_or("").to_owned(),
                    v["source_id"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        asset["observations"] = json!(obs);
        asset["relations"] = json!(rel);
        asset["sources"] = json!(
            sources
                .into_iter()
                .map(|(source_kind, source_id)| json!({"source_kind":source_kind,"source_id":source_id}))
                .collect::<Vec<_>>()
        );
        asset["merged_items"] = json!(merged_by_target.remove(aid_str).unwrap_or_default());
        if let Some(j) = judgements_by_asset.get(aid_str) {
            asset["judgement"] = j.clone();
            asset["judgement_state"] = j["judgement_state"].clone();
            asset["system_name"] = j["system_name"].clone();
            asset["uncertainty_note"] = j["uncertainty_note"].clone();
            asset["priority"] = j["priority"].clone();
            asset["judgement_updated_by"] = j["updated_by"].clone();
            asset["judgement_updated_at"] = j["updated_at"].clone();
        }
        asset["_record_revision"] = row["revision"].clone();
        items.push(asset);
    }
    let offset = p["cursor"]
        .as_u64()
        .or_else(|| p["cursor"].as_str().and_then(|v| v.parse().ok()))
        .unwrap_or(0) as usize;
    let limit = p["limit"].as_u64().unwrap_or(200).clamp(1, 500) as usize;
    let total = items.len();
    let page: Vec<_> = items.into_iter().skip(offset).take(limit).collect();
    Ok(
        json!({"items":page,"total":total,"next_cursor":if offset+page.len()<total{Some(offset+page.len())}else{None},"complete":offset+page.len()>=total}),
    )
}
fn raw_bytes(value: &Value, field: &str) -> Api<Vec<u8>> {
    let encoded = require(value, field)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Error::invalid(format!("{field} must be valid base64")))?;
    Ok(bytes)
}
fn parse_raw_request(
    raw: &[u8],
    default_scheme: &str,
) -> Api<(String, String, Vec<(String, String)>)> {
    let boundary = raw
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .map(|n| (n, 4))
        .or_else(|| raw.windows(2).position(|v| v == b"\n\n").map(|n| (n, 2)))
        .ok_or_else(|| {
            Error::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "capture_partial",
                "HTTP request headers incomplete",
            )
        })?;
    let head=raw[..boundary.0].iter().map(|byte|char::from(*byte)).collect::<String>();
    let mut lines = head.lines();
    let start = lines
        .next()
        .ok_or_else(|| Error::invalid("HTTP request line missing"))?
        .trim_end_matches('\r');
    let parts: Vec<_> = start.split(' ').collect();
    if parts.len() != 3
        || !matches!(parts[2], "HTTP/1.0" | "HTTP/1.1")
        || parts[0].is_empty()
        || !parts[0].bytes().all(|b| b.is_ascii_uppercase())
    {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "capture_unavailable",
            "unsupported HTTP request line",
        ));
    }
    let mut headers = Vec::new();
    let mut host = None;
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.starts_with([' ', '\t']) {
            return Err(Error::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "capture_unavailable",
                "folded HTTP headers are unsupported",
            ));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Error::invalid("malformed HTTP header"))?;
        if name.is_empty() || name.chars().any(|value|(value as u32)<=32||value=='\u{7f}') {
            return Err(Error::invalid("invalid HTTP header name"));
        }
        let value = value.trim_start_matches([' ','\t']).to_owned();
        if name.eq_ignore_ascii_case("host") {
            if host.is_some() {
                return Err(Error::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "capture_unavailable",
                    "multiple Host headers unsupported",
                ));
            }
            host = Some(value.clone());
        }
        headers.push((name.to_owned(), value));
    }
    let target = parts[1];
    let observed_url=if target.starts_with("http://")||target.starts_with("https://"){target.to_owned()}else{format!("{default_scheme}://{}{target}",host.as_deref().unwrap_or(""))};
    let url = if target.starts_with("http://") || target.starts_with("https://") {
        let parsed = parse_url(target)?;
        if let Some(host) = host {
            let host_url = parse_url(&format!("{}://{host}/", parsed.scheme()))?;
            if host_url.host_str() != parsed.host_str()
                || host_url.port_or_known_default() != parsed.port_or_known_default()
            {
                return Err(Error::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "capture_unavailable",
                    "Host header differs from absolute request target",
                ));
            }
        }
        parsed
    } else {
        if !target.starts_with('/') || target.starts_with("//") {
            return Err(Error::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "capture_unavailable",
                "HTTP request target must be origin or absolute form",
            ));
        }
        let host = host.ok_or_else(|| Error::invalid("Host header required"))?;
        parse_url(&format!("{default_scheme}://{host}{target}"))?
    };
    let _=url;
    Ok((parts[0].to_owned(), observed_url, headers))
}
fn put_capture(s: &AppState, id: &str, bytes: &[u8], name: &str) -> Api<Value> {
    put_artifact(s, id, bytes, name, "message/http")
}
fn safe_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let sensitive = matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization"
                    | "proxy-authorization"
                    | "cookie"
                    | "cookie2"
                    | "set-cookie"
                    | "x-token"
                    | "x-auth-token"
                    | "x-api-key"
                    | "satoken"
                    | "x-quaketoken"
                    | "api-key"
                    | "private-token"
            );
            (
                name.clone(),
                if sensitive {
                    "[redacted]".to_owned()
                } else {
                    value.clone()
                },
            )
        })
        .collect()
}
fn record_exchange(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    record_exchange_inner(s, id, body, None)
}
pub(super) fn backfill_traffic_index(s: &AppState, id: &str) -> Api<()> {
    traffic_index::backfill_missing(s, id)
}

fn advance_exchange_source(s: &AppState, id: &str, exchange_id: &str) -> Api<()> {
    let key = rec_key(id, "http_exchange", exchange_id);
    let seq = s
        .store
        .document_rowid(RECORDS, &key)
        .map_err(super::db)?
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.record_not_found",
                "Exchange record is unavailable",
            )
        })?;
    advance_source(
        s,
        id,
        "http-exchanges",
        0,
        seq as u64,
        "HttpExchange.v1",
        "ready",
        "",
    )?;
    Ok(())
}
fn record_exchange_inner(s:&AppState,id:&str,body:&Value,command_id:Option<&str>)->Api<Value>{
    s.store.document_scope(||record_exchange_commit(s,id,body,command_id)).map_err(db)?
}
fn record_exchange_commit(s:&AppState,id:&str,body:&Value,command_id:Option<&str>)->Api<Value>{
    if let Some(command)=command_id{if let Some(marker)=s.store.get(EXCHANGE_APPLIED,command).map_err(db)?{return Ok(marker["output"].clone());}}
    if let Some(run_id)=body["source_run_id"].as_str().filter(|v|!v.is_empty()){if !records(s,id,"run_binding")?.iter().any(|row|row["payload"]["run_id"]==run_id){return Err(Error::new(StatusCode::FORBIDDEN,"src.run_not_bound","request source Run is outside this Task"));}}
    let raw=raw_bytes(body,"request_raw_b64").or_else(|_|raw_bytes(body,"raw_request_b64"))?;
    let scheme=body["observed_url"].as_str().and_then(|url|parse_url(url).ok()).map(|url|url.scheme().to_owned()).unwrap_or("http".into());
    let parsed=exchange_queries::parsed_request(&raw,&scheme)?;let method=require(&parsed,"method")?;let url=require(&parsed,"url")?;
    if body["observed_url"].as_str().filter(|value|!value.is_empty()).is_some_and(|value|value!=url)||body["method"].as_str().is_some_and(|value|!value.eq_ignore_ascii_case(method)){return Err(Error::new(StatusCode::UNPROCESSABLE_ENTITY,"capture_unavailable","observed URL or method differs from raw request"));}
    let eid=body["exchange_id"].as_str().filter(|value|!value.is_empty()).map(str::to_owned).unwrap_or_else(||format!("sexchange-{}",Uuid::new_v4().simple()));
    let prior=match rec(s,id,"http_exchange",&eid){Ok(row)=>Some(row),Err(error)if error.status==StatusCode::NOT_FOUND=>None,Err(error)=>return Err(error)};
    let read=|reference:&Value|->Api<Vec<u8>>{let saved=verify_ref(s,id,reference)?;base64::engine::general_purpose::STANDARD.decode(saved["data"].as_str().unwrap_or("")).map_err(|_|Error::invalid("capture artifact is unreadable"))};
    if let Some(row)=&prior{if read(&row["payload"]["request_raw_ref"])?!=raw{return Err(Error::conflict("original request bytes are immutable"));}}
    let mut response=body["response_raw_b64"].as_str().map(|_|raw_bytes(body,"response_raw_b64")).transpose()?;
    if response.is_none()&&body["response_raw_ref"].is_object(){response=Some(read(&body["response_raw_ref"])?);}
    if let Some(row)=&prior{let original=&row["payload"]["response_raw_ref"];if original.is_object(){let bytes=read(original)?;if response.as_ref().is_some_and(|value|*value!=bytes){return Err(Error::conflict("original response bytes are immutable"));}response=Some(bytes);}}
    let request_ref=if let Some(row)=&prior{row["payload"]["request_raw_ref"].clone()}else{put_capture(s,id,&raw,&format!("{eid}-request.http"))?};
    let response_ref=if let Some(row)=prior.as_ref().filter(|row|row["payload"]["response_raw_ref"].is_object()){row["payload"]["response_raw_ref"].clone()}else if body["response_raw_b64"].is_null()&&body["response_raw_ref"].is_object(){body["response_raw_ref"].clone()}else{response.as_ref().map(|bytes|put_capture(s,id,bytes,&format!("{eid}-response.http"))).transpose()?.unwrap_or(Value::Null)};
    let mut exchange=prior.as_ref().map(|row|row["payload"].clone()).unwrap_or(json!({"exchange_id":eid,"task_id":id,"source_kind":"import","actor_ref":"human","context_id":"default","identity_revision":0,"browser_generation":0,"protocol":"http/1.1","execution_state":"completed","capture_limitations":[],"tags":[],"metadata":{},"started_at":now()}));
    for field in ["source_run_id","source_event_seq","source_kind","actor_ref","context_id","identity_revision","browser_generation","declared_target","started_at","finished_at","status_code","protocol","protocol_stream_ref","redirect_parent","replay_of","execution_state","capture_state","capture_limitations","authorization_ref","decision_ref","tags","metadata"]{if let Some(value)=body.get(field){exchange[field]=value.clone();}}
    exchange["request_raw_ref"]=request_ref;exchange["response_raw_ref"]=response_ref;exchange["observed_url"]=json!(url);exchange["method"]=json!(method);exchange["request_headers"]=parsed["headers"].clone();
    if body.get("capture_state").is_none() && (prior.is_none() || prior.as_ref().is_some_and(|row| row["payload"]["response_raw_ref"].is_null()) && response.is_some()) {
        exchange["capture_state"] = json!(if response.is_some() { "complete" } else { "partial" });
    }
    if exchange["capture_state"] == "complete" && response.is_none() { return Err(Error::invalid("complete capture requires response bytes")); }
    if !matches!(exchange["capture_state"].as_str(),Some("complete"|"partial"|"unavailable")){return Err(Error::invalid("invalid capture_state"));}
    if let Some(row)=&prior{let current=&row["payload"];for field in ["source_kind","source_run_id","source_event_seq","replay_of","redirect_parent"]{if current[field]!=exchange[field]{return Err(Error::conflict("exchange provenance is immutable"));}}if matches!(current["execution_state"].as_str(),Some("completed"|"failed"|"outcome_unknown"|"cancelled"|"denied"))&&current["execution_state"]!=exchange["execution_state"]{return Err(Error::conflict("terminal exchange cannot be reopened"));}}
    if !exchange["metadata"].is_object(){return Err(Error::invalid("exchange metadata must be an object"));}
    if !matches!(exchange["source_kind"].as_str(),Some("browser"|"http_tool"|"proxy"|"import")){return Err(Error::invalid("invalid exchange source_kind"));}
    if !matches!(exchange["execution_state"].as_str(),Some("created"|"waiting_approval"|"denied"|"ready"|"dispatched"|"completed"|"failed"|"outcome_unknown"|"cancelled")){return Err(Error::invalid("invalid exchange execution_state"));}
    for field in ["identity_revision","browser_generation"]{if !exchange[field].is_u64(){return Err(Error::invalid(format!("exchange {field} must be nonnegative integer")));}}
    for field in ["tags","capture_limitations"]{if !exchange[field].as_array().is_some_and(|rows|rows.iter().all(Value::is_string)){return Err(Error::invalid(format!("exchange {field} must be text array")));}}
    for field in ["source_run_id","redirect_parent","replay_of","finished_at"]{if !exchange[field].is_null()&&!exchange[field].is_string(){return Err(Error::invalid(format!("exchange {field} must be text or null")));}}
    for field in ["source_event_seq","status_code"]{if !exchange[field].is_null()&&!exchange[field].is_i64(){return Err(Error::invalid(format!("exchange {field} must be integer or null")));}}
    for field in ["started_at","finished_at"]{if let Some(value)=exchange[field].as_str(){chrono::DateTime::parse_from_rfc3339(value).map_err(|_|Error::invalid(format!("exchange {field} must be RFC3339")))?;}}

    exchange["metadata"]["request_target"]=parsed["target"].clone();exchange["metadata"]["request_body_sha256"]=parsed["body"]["sha256"].clone();exchange["metadata"]["request_body_size"]=parsed["body"]["size"].clone();exchange["metadata"]["query_parameters"]=parsed["query_parameters"].clone();exchange["metadata"]["raw_representation"]=json!(if matches!(exchange["protocol"].as_str(),Some("http/1.0"|"http/1.1")){"original"}else{"collector-fields"});
    let resp_slice = response.clone();
    if let Some(response)=response{match exchange_queries::parsed_response(&response,method){Ok(parsed)=>{
        exchange["response_headers"]=parsed["headers"].clone();if exchange["status_code"].is_null(){exchange["status_code"]=parsed["status_code"].clone();}
        let(decoded,limitations)=exchange_queries::decoded_body(&parsed,method)?;
        let raw_body=base64::engine::general_purpose::STANDARD.decode(parsed["body"]["base64"].as_str().unwrap_or("")).map_err(|_|Error::invalid("response body encoding invalid"))?;
        if decoded!=raw_body{exchange["metadata"]["decoded_body_ref"]=put_artifact(s,id,&decoded,&format!("{eid}-decoded-response"),"application/octet-stream")?;}
        if !exchange["capture_limitations"].is_array(){exchange["capture_limitations"]=json!([]);}for limitation in limitations{exchange["capture_limitations"].as_array_mut().unwrap().push(json!(limitation));}
        exchange["metadata"]["response_body_sha256"]=parsed["body"]["sha256"].clone();exchange["metadata"]["response_body_size"]=parsed["body"]["size"].clone();
        exchange["metadata"]["media_type"]=parsed["headers"].as_array().into_iter().flatten().find(|row|row[0].as_str().is_some_and(|name|name.eq_ignore_ascii_case("content-type"))).and_then(|row|row[1].as_str()).map(|value|json!(value.split(';').next().unwrap_or("").trim().to_ascii_lowercase())).unwrap_or(json!(""));
    },Err(error)if exchange["capture_state"]!="complete"=>exchange["metadata"]["response_parse_error"]=json!(error.code),Err(error)=>return Err(error)}}
    save_rec(s,id,"http_exchange",&eid,exchange.clone(),Some(prior.as_ref().and_then(|row|row["revision"].as_u64()).unwrap_or(0)))?;
    traffic_index::index_exchange(s, id, &exchange, &raw, resp_slice.as_deref())?;
    advance_exchange_source(s,id,&eid)?;
    if let Some(command)=command_id{s.store.put_if_absent(EXCHANGE_APPLIED,command,&json!({"command_id":command,"output":exchange})).map_err(db)?;}
    Ok(exchange)
}
fn read_exchange(s:&AppState,id:&str,p:&Value)->Api<Value>{
    let eid=require(p,"exchange_id")?;let row=find_exchange_record(s,id,eid)?;if p["revision"].as_u64().is_some_and(|revision|row["revision"].as_u64()!=Some(revision)){return Err(Error::conflict("exchange revision is stale"));}
    let exchange=&row["payload"];let request=verify_ref(s,id,&exchange["request_raw_ref"])?;let request_bytes=base64::engine::general_purpose::STANDARD.decode(request["data"].as_str().unwrap_or("")).map_err(|_|Error::invalid("request bytes are unreadable"))?;
    let response=if exchange["response_raw_ref"].is_object(){Some(verify_ref(s,id,&exchange["response_raw_ref"])?) }else{None};
    let structured_request=if exchange["metadata"]["source_format"]=="har"{serde_json::from_slice(&request_bytes).ok().unwrap_or(Value::Null)}else{exchange_queries::parsed_request(&request_bytes,parse_url(require(exchange,"observed_url")?)?.scheme())?};
    let structured_response=if exchange["capture_state"]=="complete" && exchange["metadata"]["source_format"]!="har"{response.as_ref().map(|response|{let raw=base64::engine::general_purpose::STANDARD.decode(response["data"].as_str().unwrap_or("")).map_err(|_|Error::invalid("response bytes are unreadable"))?;exchange_queries::parsed_response(&raw,exchange["method"].as_str().unwrap_or(""))}).transpose()?}else if exchange["metadata"]["source_format"]=="har"{response.as_ref().and_then(|response|{let raw=base64::engine::general_purpose::STANDARD.decode(response["data"].as_str().unwrap_or("")).ok()?;serde_json::from_slice(&raw).ok()})}else{None};
    let mut chain=Vec::new();let mut seen=std::collections::HashSet::new();let mut next=Some(eid.to_owned());while let Some(current)=next{if !seen.insert(current.clone()){return Err(Error::conflict("redirect_parent contains a cycle"));}let row=find_exchange_record(s,id,&current)?;next=row["payload"]["redirect_parent"].as_str().filter(|value|!value.is_empty()).map(str::to_owned);chain.push(current);}chain.reverse();
    let mut result=json!({"exchange":exchange,"record_revision":row["revision"],"structured":{"request":structured_request,"response":structured_response},"redirect_chain":chain});
    if p["include_bytes"]!=false{result["request_raw"]=json!({"encoding":"base64","data":request["data"],"size":request["size"]});result["response_raw"]=response.map(|response|json!({"encoding":"base64","data":response["data"],"size":response["size"]})).unwrap_or(Value::Null);}else{result["request_raw_base64"]=request["data"].clone();result["response_raw_base64"]=response.map(|response|response["data"].clone()).unwrap_or(Value::Null);}Ok(result)
}
fn save_exchange_draft(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    save_exchange_draft_inner(s, id, body, None)
}
fn save_exchange_draft_inner(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    s.store.document_scope(||save_exchange_draft_commit(s,id,body,command_id)).map_err(db)?
}
fn save_exchange_draft_commit(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let source = require(body, "source_exchange_id")?;
    let exchange = match rec(s, id, "http_exchange", source) {
        Ok(exchange) => exchange,
        Err(error) if error.status == StatusCode::NOT_FOUND => records(s, id, "import_exchange")?
            .into_iter()
            .find(|row| row["record_id"] == source || row["payload"]["exchange_id"] == source)
            .ok_or_else(|| {
                Error::new(
                    StatusCode::NOT_FOUND,
                    "artifact_missing",
                    format!("source exchange {source} is unavailable"),
                )
            })?,
        Err(error) => return Err(error),
    };
    let prior: Vec<_> = records(s, id, "request_draft")?
        .into_iter()
        .filter(|v| v["payload"]["source_exchange_id"] == source)
        .collect();
    let latest = prior
        .iter()
        .max_by_key(|row| row["payload"]["revision"].as_u64().unwrap_or(0));
    let current = prior
        .iter()
        .map(|v| v["payload"]["revision"].as_u64().unwrap_or(0))
        .max()
        .unwrap_or_else(|| exchange["revision"].as_u64().unwrap_or(1));
    if body["base_revision"].as_u64() != Some(current) {
        return Err(Error::conflict("request draft base revision is stale"));
    }
    let read_artifact = |reference: &Value| -> Api<Vec<u8>> {
        let saved = verify_ref(s, id, reference).map_err(|error| {
            if error.code == "src.artifact_missing" {
                Error::new(
                    StatusCode::NOT_FOUND,
                    "artifact_missing",
                    "source HTTP request artifact is unavailable",
                )
            } else {
                error
            }
        })?;
        base64::engine::general_purpose::STANDARD
            .decode(saved["data"].as_str().unwrap_or(""))
            .map_err(|_| {
                Error::new(
                    StatusCode::CONFLICT,
                    "src.artifact_corrupt",
                    "source HTTP request artifact is corrupt",
                )
            })
    };
    let mut derived=false;
    let raw = if let Some(encoded) = body["raw_request_b64"].as_str() {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| Error::invalid("raw_request_b64 must be valid base64"))?
    } else if let Some(latest) = latest.filter(|_| body["use_original_capture"] != true) {
        read_artifact(&latest["payload"]["raw_request_ref"])?
    } else {
        let raw=read_artifact(&exchange["payload"]["request_raw_ref"])?;
        if parse_raw_request(&raw,"http").is_ok(){raw}else{derived=true;structured_draft_request(&raw,&exchange["payload"])?}
    };
    let scheme = exchange["payload"]["observed_url"]
        .as_str()
        .and_then(|v| parse_url(v).ok())
        .map(|v| v.scheme().to_owned())
        .unwrap_or_else(|| "http".to_owned());
    let (method, url, _) = parse_raw_request(&raw, &scheme)?;
    let revision = if prior.is_empty() { 1 } else { current + 1 };
    let did = format!("sdraft-{}", Uuid::new_v4().simple());
    let artifact = put_capture(s, id, &raw, &format!("{did}.http"))?;
    let mut changes = body["changes"].as_object().cloned().unwrap_or_default();
    changes.insert(
        "derived_from_structured_capture".to_owned(),
        latest.filter(|_| body["use_original_capture"] != true)
            .map(|row| row["payload"]["changes"]["derived_from_structured_capture"] == true)
            .unwrap_or(derived)
            .into(),
    );
    let metadata = body["metadata"]
        .as_object()
        .cloned()
        .or_else(|| latest.and_then(|row| row["payload"]["metadata"].as_object().cloned()))
        .unwrap_or_default();
    let draft = json!({"draft_id":did,"task_id":id,"source_exchange_id":source,
        "revision":revision,"raw_request_ref":artifact,"target_url":url,"method":method,
        "changes":changes,"metadata":metadata,
        "editor":body["editor"].as_str().unwrap_or("local-operator"),"created_at":now()});
    if let Some(command_id) = command_id {
        let (record_key, _, row) = stage_rec(s, id, "request_draft", &did, draft.clone(), Some(0))?;
        let output = json!({"draft":draft,"record_revision":row["revision"]});
        let record_doc = json!({"version":1,"values":row});
        let marker = json!({"command_id":command_id,"output":output});
        if s.store
            .put_many_if(
                &[
                    (RECORDS, &record_key, None),
                    (EXCHANGE_APPLIED, command_id, None),
                ],
                &[
                    (RECORDS, &record_key, &record_doc),
                    (EXCHANGE_APPLIED, command_id, &marker),
                ],
            )
            .map_err(super::db)?
        {
            Ok(output)
        } else {
            Ok(s.store
                .get(EXCHANGE_APPLIED, command_id)
                .map_err(super::db)?
                .ok_or_else(|| Error::conflict("draft command commit was not found"))?["output"]
                .clone())
        }
    } else {
        let row = save_rec(s, id, "request_draft", &did, draft.clone(), Some(0))?;
        Ok(json!({"draft":draft,"record_revision":row["revision"]}))
    }
}
pub(super) fn apply_finding_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    finding::apply(s, id, kind, body, command_id)
}
fn report_templates() -> Value {
    json!([
        {"template_id":"generic-zh-v1","language":"zh","platform":"generic","template_version":"1.1.0","mapping_version":"1.1.0","source_url":"builtin://src/generic-zh-v1","source_fetched_at":"2026-10-26T00:00:00Z","field_mapping":{"title":"漏洞标题","target":"受影响资产与端点","attacker":"攻击者最低权限前提","function":"业务功能边界","mechanism":"漏洞根因与机制","normal_behavior":"预期安全行为","actual_behavior":"实际越界行为","steps":"逐步复现","decisive_evidence":"决定性证据","confirmed_impact":"已证实危害","conditional_impact":"条件性扩展影响","cvss":"CVSS 3.1 评估","remediation":"代码级修复方案","counterevidence":"已排除替代解释","limitations":"测试边界与限制","restoration":"环境恢复确认"},"constraints":{"required":["title","target","mechanism","steps","confirmed_impact","remediation"]}},
        {"template_id":"enterprise-src-v1","language":"zh","platform":"enterprise_src","template_version":"1.1.0","mapping_version":"1.1.0","source_url":"builtin://src/enterprise-src-v1","source_fetched_at":"2026-10-26T00:00:00Z","field_mapping":{"title":"漏洞名称","target":"受影响系统与接口","attacker":"利用条件","mechanism":"漏洞成因分析","normal_behavior":"正常逻辑","actual_behavior":"实际观察结果","steps":"详细验证过程","decisive_evidence":"关键数据包与工件","confirmed_impact":"已验证业务风险","conditional_impact":"潜在连锁风险","cvss":"风险评分","remediation":"具体整改方案","limitations":"验证范围说明","restoration":"数据清理与恢复"},"constraints":{"required":["title","target","attacker","mechanism","steps","confirmed_impact","remediation"]}},
        {"template_id":"edusrc-v1","language":"zh","platform":"edusrc","template_version":"1.1.0","mapping_version":"1.1.0","source_url":"builtin://src/edusrc-v1","source_fetched_at":"2026-10-26T00:00:00Z","field_mapping":{"title":"漏洞标题","target":"漏洞位置","attacker":"利用前提","mechanism":"漏洞成因","steps":"无害化测试过程","confirmed_impact":"危害说明","cvss":"CVSS","remediation":"修复方案","restoration":"恢复说明"},"constraints":{"required":["title","target","mechanism","steps","confirmed_impact"]}},
        {"template_id":"hackerone-en-v1","language":"en","platform":"hackerone","template_version":"1.0.0","mapping_version":"1.0.0","source_url":"builtin://src/hackerone-en-v1","source_fetched_at":"2026-11-12T00:00:00Z","field_mapping":{"title":"Title","target":"Asset & Endpoint","attacker":"Attacker Preconditions & Role","function":"Affected Component","mechanism":"Vulnerability Details & Root Cause","normal_behavior":"Expected Behavior","actual_behavior":"Observed Behavior","steps":"Steps to Reproduce","decisive_evidence":"Supporting Material / References","confirmed_impact":"Verified Impact","conditional_impact":"Conditional Impact","cvss":"CVSS 3.1","remediation":"Remediation","counterevidence":"Ruled-Out Benign Explanations","limitations":"Testing Scope & Limitations","restoration":"Cleanup & State Restoration"},"constraints":{"required":["title","target","mechanism","steps","confirmed_impact","remediation"]}},
        {"template_id":"bugcrowd-en-v1","language":"en","platform":"bugcrowd","template_version":"1.0.0","mapping_version":"1.0.0","source_url":"builtin://src/bugcrowd-en-v1","source_fetched_at":"2026-11-12T00:00:00Z","field_mapping":{"title":"Submission Title","target":"Target URL / VRT Scope","attacker":"Prerequisites","function":"Application Function","mechanism":"Technical Root Cause","normal_behavior":"Expected Control","actual_behavior":"Actual Response","steps":"Technical Walkthrough (PoC)","decisive_evidence":"HTTP Evidence Artifacts","confirmed_impact":"Business Impact","conditional_impact":"Escalation Preconditions","cvss":"CVSS 3.1 Vector","remediation":"Fix Recommendation","limitations":"Scope Notes"},"constraints":{"required":["title","target","mechanism","steps","confirmed_impact","remediation"]}},
        {"template_id":"bilingual-src-v1","language":"zh-en","platform":"global_src","template_version":"1.0.0","mapping_version":"1.0.0","source_url":"builtin://src/bilingual-src-v1","source_fetched_at":"2026-11-12T00:00:00Z","field_mapping":{"title":"漏洞标题 / Title","target":"受影响端点 / Target Endpoint","attacker":"前置条件 / Preconditions","function":"所属模块 / Function","mechanism":"根因机制 / Root Cause & Mechanism","normal_behavior":"预期行为 / Expected Behavior","actual_behavior":"实际行为 / Observed Behavior","steps":"复现步骤 / Steps to Reproduce","decisive_evidence":"决定性证据 / Decisive Evidence","confirmed_impact":"已证实影响 / Confirmed Impact","conditional_impact":"条件性影响 / Conditional Impact","cvss":"CVSS 3.1","remediation":"修复方案 / Concrete Remediation","counterevidence":"排除项 / Addressed Alternatives","limitations":"限制说明 / Limitations","restoration":"恢复说明 / Cleanup"},"constraints":{"required":["title","target","mechanism","steps","confirmed_impact","remediation"]}}
    ])
}
fn template_for(spec: &Value) -> Api<Value> {
    if let Some(name) = spec.as_str() {
        let mapped = match name {
            "general_src" => "generic-zh-v1",
            "bsrc" | "butian" => "enterprise-src-v1",
            "edusrc" => "edusrc-v1",
            "hackerone" => "hackerone-en-v1",
            "bugcrowd" => "bugcrowd-en-v1",
            "bilingual" => "bilingual-src-v1",
            other => other,
        };
        return report_templates()
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["template_id"] == mapped)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    StatusCode::NOT_FOUND,
                    "artifact_missing",
                    format!("unknown report template {name}"),
                )
            });
    }
    let Some(template) = spec.as_object() else {
        return Err(Error::invalid("report template must be an ID or object"));
    };
    for field in [
        "template_id",
        "platform",
        "template_version",
        "mapping_version",
        "source_url",
        "source_fetched_at",
        "field_mapping",
        "constraints",
    ] {
        if !template.contains_key(field) {
            return Err(Error::conflict(format!(
                "custom template is incomplete: {field}"
            )));
        }
    }
    if !spec["field_mapping"].is_object() || !spec["constraints"].is_object() {
        return Err(Error::conflict(
            "template mapping and constraints must be objects",
        ));
    }
    let mut normalized = spec.clone();
    normalized["schema"] = json!("muteki.src.report-template.v1");
    Ok(normalized)
}
pub(in crate::runs::src_api::domains) fn cas_file(
    s: &AppState,
    task_id: &str,
    sha256: &str,
) -> Option<std::path::PathBuf> {
    if task_id.is_empty()
        || sha256.len() != 64
        || !sha256.bytes().all(|b| b.is_ascii_hexdigit())
        || task_id.contains(['/', '\\', '\0'])
    {
        return None;
    }
    let state_dir = s.store.get("system", "state_dir").ok().flatten()?;
    let root = state_dir.as_str().filter(|v| !v.is_empty())?;
    Some(
        std::path::PathBuf::from(root)
            .join("src-cas")
            .join(task_id)
            .join(&sha256[..2])
            .join(sha256),
    )
}
pub(in crate::runs::src_api::domains) fn write_cas(
    s: &AppState,
    task_id: &str,
    sha256: &str,
    bytes: &[u8],
) -> Api<bool> {
    let Some(path) = cas_file(s, task_id, sha256) else {
        return Ok(false);
    };
    if path.is_file() {
        return Ok(true);
    }
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    std::fs::create_dir_all(parent).map_err(super::db)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(super::db)?;
    use std::io::Write;
    temp.write_all(bytes).map_err(super::db)?;
    temp.as_file().sync_all().map_err(super::db)?;
    let _ = temp.persist(&path).map_err(|e| super::db(e.error))?;
    Ok(true)
}
pub(in crate::runs::src_api::domains) fn remove_cas_if_unreferenced(
    s: &AppState,
    task_id: &str,
    sha256: &str,
) {
    let Some(path) = cas_file(s, task_id, sha256) else {
        return;
    };
    let still_used = s
        .store
        .list("src_artifacts")
        .ok()
        .is_some_and(|rows| {
            rows.iter().any(|row| {
                row["task_id"] == task_id
                    && (row["cas_sha256"].as_str() == Some(sha256)
                        || row["sha256"].as_str() == Some(sha256))
            })
        });
    if !still_used {
        let _ = std::fs::remove_file(path);
    }
}
fn artifact_record(s: &AppState, id: &str, reference: &Value) -> Api<(String, Value)> {
    let artifact_id = reference["artifact_id"]
        .as_str()
        .filter(|value| !value.is_empty());
    if let Some(artifact_id) = artifact_id {
        let key = format!("{id}:{artifact_id}");
        if let Some(mut row) = s.store.get("src_artifacts", &key).map_err(super::db)? {
            if row["data"].as_str().is_none() {
                if let Some(sha) = row["cas_sha256"]
                    .as_str()
                    .or_else(|| row["sha256"].as_str())
                    .filter(|v| !v.is_empty())
                {
                    let path = cas_file(s, id, sha).ok_or_else(|| {
                        Error::new(
                            StatusCode::NOT_FOUND,
                            "src.artifact_missing",
                            "CAS 证据存储目录不可用",
                        )
                    })?;
                    let bytes = std::fs::read(&path).map_err(|_| {
                        Error::new(
                            StatusCode::NOT_FOUND,
                            "src.artifact_missing",
                            "CAS 证据工件原件不存在",
                        )
                    })?;
                    row["data"] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
                }
            }
            return Ok((key, row));
        }
    }
    if let Ok(Some(hist_root_val)) = s.store.get("system", "history_root") {
        if let Some(hist_root) = hist_root_val.as_str() {
            for candidate in [
                artifact_id,
                reference["sha256"].as_str().filter(|value| !value.is_empty()),
            ]
            .into_iter()
            .flatten()
            {
                if !crate::import::valid_id(id) || !crate::import::valid_id(candidate) {
                    continue;
                }
                let path = std::path::PathBuf::from(hist_root)
                    .join("_platform/src/tasks")
                    .join(id)
                    .join("cas")
                    .join(candidate);
                if let Ok(bytes) = std::fs::read(&path) {
                    let resolved_id = artifact_id.unwrap_or(candidate);
                    let row = json!({
                        "artifact_id": resolved_id,
                        "sha256": reference["sha256"],
                        "version": reference["version"].as_u64().unwrap_or(1),
                        "task_id": id,
                        "size": bytes.len(),
                        "name": reference["name"].as_str().unwrap_or(""),
                        "media_type": reference["media_type"].as_str().unwrap_or("application/octet-stream"),
                        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    });
                    return Ok((format!("{id}:{resolved_id}"), row));
                }
            }
        }
    }
    Err(Error::new(
        StatusCode::NOT_FOUND,
        "src.artifact_missing",
        "证据工件不存在",
    ))
}
fn verify_ref(s: &AppState, id: &str, reference: &Value) -> Api<Value> {
    if reference["task_id"] != id {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.artifact_scope",
            "证据工件不属于该 Task",
        ));
    }
    let (_, saved) = artifact_record(s, id, reference)?;
    if reference["artifact_id"]
        .as_str()
        .is_some_and(|v| saved["artifact_id"] != v)
        || reference["media_type"]
            .as_str()
            .is_some_and(|v| saved["media_type"] != v)
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.artifact_ref_mismatch",
            "证据引用与存储工件不匹配",
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(saved["data"].as_str().unwrap_or(""))
        .map_err(|_| {
            Error::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "src.artifact_corrupt",
                "证据工件损坏",
            )
        })?;
    let size=reference["size"].as_u64().ok_or_else(||Error::invalid("artifact reference size is required"))?;
    let version=reference["version"].as_u64().filter(|value|*value>0).ok_or_else(||Error::invalid("artifact reference version is required"))?;
    let hash=format!("{:x}",Sha256::digest(&bytes));
    if size!=bytes.len() as u64||saved["size"].as_u64()!=Some(size)||saved["version"].as_u64().unwrap_or(1)!=version
        ||saved["sha256"].as_str().is_some_and(|expected|expected!=hash)||reference["sha256"].as_str().is_some_and(|expected|expected!=hash){
        return Err(Error::new(StatusCode::CONFLICT,"src.artifact_ref_mismatch","artifact content, size or version differs from the frozen reference"));
    }
    Ok(json!({"encoding":"base64","data":saved["data"],"size":size,"version":version,"sha256":hash,
        "artifact_id":saved["artifact_id"],"media_type":saved["media_type"],"task_id":id}))
}
fn read_artifact(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    publication::readable_ref(s, id, &p["ref_data"])
}
pub(super) fn verify_workbench_evidence(
    s: &AppState,
    id: &str,
    body: &Value,
    actor_id: &str,
) -> Api<Value> {
    task(s, id)?;
    let reference = &body["ref_data"];
    let readable = match publication::readable_ref(s, id, reference) {
        Ok(readable) => readable,
        Err(error) => {
            return Ok(json!({"state":"unreadable","code":error.code,
                "reason":error.message}));
        }
    };
    let evidence_id = body["evidence_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            reference["evidence_id"]
                .as_str()
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            readable["artifact_id"]
                .as_str()
                .filter(|value| !value.is_empty())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| format!("swev_{}", Uuid::new_v4().simple()));
    let key = json!([id, "workbench_evidence", evidence_id]).to_string();
    loop {
        let current = s.store.get(RECORDS, &key).map_err(super::db)?;
        let version = current
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0);
        let existing = current.as_ref().map(|row| &row["values"]);
        let timestamp = now();
        let payload = json!({"evidence_id":evidence_id,
            "artifact_id":readable["artifact_id"],"state":"verified",
            "checked_by":actor_id,"checked_at":timestamp,"size":readable["size"]});
        let row = json!({"record_id":existing
                .and_then(|row| row["record_id"].as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("srec_{}", Uuid::new_v4().simple())),
            "kind":"workbench_evidence","task_id":id,"revision":version+1,
            "parent_id":"","natural_key":evidence_id,"generation":0,
            "created_at":existing.and_then(|row| row["created_at"].as_str())
                .unwrap_or(&timestamp),"updated_at":timestamp,"payload":payload});
        if s.store
            .compare_and_put(RECORDS, &key, version, row)
            .map_err(super::db)?
            .is_ok()
        {
            return Ok(json!({"state":"verified","size":readable["size"],
                "artifact_id":readable["artifact_id"]}));
        }
    }
}
fn report(s: &AppState, id: &str, rid: &str) -> Api<Value> {
    record_payload(s, id, "frozen_report", rid)
}
fn render_report(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let mut frozen = report(s, id, require(body, "report_id")?)?;
    if let Some(tpl) = body.get("template").filter(|v| !v.is_null()) {
        frozen["template"] = template_for(tpl)?;
    }
    report_render::render(s, id, &frozen, body)
}
fn export_report(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let frozen = report(s, id, require(body, "report_id")?)?;
    report_export::export(s, id, &frozen, body)
}
fn record_reproduction(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    if let Some(command_id) = command_id {
        if let Some(marker) = s.store.get(REPORT_APPLIED, command_id).map_err(super::db)? {
            return Ok(marker["output"].clone());
        }
    }
    let report_id = body["report_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let supplied_finding = body["finding_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let resolved_finding = if let Some(report_id) = report_id {
        let frozen = report(s, id, report_id)?;
        let report_finding = frozen["finding_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if let Some(expected) = supplied_finding {
            if expected != report_finding {
                return Err(Error::conflict(
                    "reproduction finding_id does not match the referenced report",
                ));
            }
        }
        report_finding
    } else if let Some(finding_id) = supplied_finding {
        if records(s, id, "finding_claim")?
            .iter()
            .all(|row| row["payload"]["finding_id"] != finding_id)
        {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "reproduction finding has no persisted claims in this Task",
            ));
        }
        finding_id.to_owned()
    } else {
        return Err(Error::invalid(
            "report_id or finding_id is required to record reproduction",
        ));
    };
    let reviewer = body["reviewer"].as_str().unwrap_or("");
    let original = body["original_researcher"].as_str().unwrap_or("");
    if reviewer == original {
        return Err(Error::conflict(
            "independent reviewer must differ from the original researcher",
        ));
    }
    let evidence = body["evidence_refs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for reference in &evidence { verify_ref(s,id,reference)?; }
    let receipt_ids = body["execution_receipt_ids"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for receipt_id in &receipt_ids {
        let receipt_id = receipt_id.as_str().unwrap_or("");
        let receipt = rec(s, id, "execution_receipt", receipt_id)?;
        let receipt = &receipt["payload"];
        if receipt["dispatch_state"] != "completed"
            || receipt["outcome_known"] != true
            || (receipt["result_ref"].is_null() && receipt["result"].is_null())
        {
            return Err(Error::conflict(
                "reproduction execution is not complete with a known result",
            ));
        }
    }
    let executed = body["executed_steps"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let skipped = body["skipped_steps"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let blockers = body["blockers"].as_array().cloned().unwrap_or_default();
    let execution = body["execution"].as_object().cloned().unwrap_or_default();
    let target_version = body["target_version"].as_str().unwrap_or("");
    let success = body["success"] == true;
    if success
        && (executed.is_empty()
            || !skipped.is_empty()
            || !blockers.is_empty()
            || evidence.is_empty()
            || receipt_ids.is_empty()
            || target_version.is_empty()
            || execution
                .get("command")
                .is_none_or(|value| value.is_null() || value == "" || value == false)
            || execution.get("exit_code").and_then(Value::as_i64) != Some(0))
    {
        return Err(Error::conflict(
            "successful reproduction requires steps, evidence, real execution receipts, target version and a zero-exit command record",
        ));
    }
    let reproduction_id = format!("srepro-{}", Uuid::new_v4().simple());
    let value = json!({"reproduction_id":reproduction_id,"report_id":report_id,
        "finding_id":resolved_finding,"reviewer":reviewer,"original_researcher":original,
        "executed_steps":executed,"skipped_steps":skipped,"success":success,
        "execution":execution,"execution_receipt_ids":receipt_ids,"target_version":target_version,
        "identity_refs":body["identity_refs"].as_array().cloned().unwrap_or_default(),
        "evidence_refs":evidence,"blockers":blockers,"created_at":now()});
    let mut value = value;
    if let Some(replay) = body.get("replay") { value["replay"] = replay.clone(); }
    if let Some(command_id) = command_id {
        let (key, _, row) = stage_rec(
            s,
            id,
            "report_reproduction",
            &reproduction_id,
            value.clone(),
            Some(0),
        )?;
        let document = json!({"version":1,"values":row});
        let marker = json!({"command_id":command_id,"output":value});
        if s.store
            .put_many_if(
                &[(RECORDS, &key, None), (REPORT_APPLIED, command_id, None)],
                &[
                    (RECORDS, &key, &document),
                    (REPORT_APPLIED, command_id, &marker),
                ],
            )
            .map_err(super::db)?
        {
            return Ok(value);
        }
        return Ok(s
            .store
            .get(REPORT_APPLIED, command_id)
            .map_err(super::db)?
            .ok_or_else(|| {
                Error::conflict("report reproduction was committed by another command")
            })?["output"]
            .clone());
    }
    save_rec(
        s,
        id,
        "report_reproduction",
        &reproduction_id,
        value.clone(),
        Some(0),
    )?;
    Ok(value)
}
pub(super) fn apply_report_command(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
    idempotency_key: Option<&str>,
) -> Api<Value> {
    if let Some(marker) = s.store.get(REPORT_APPLIED, command_id).map_err(super::db)? {
        return Ok(marker["output"].clone());
    }
    match kind {
        "src.report.freeze" => {
            if idempotency_key.is_none() {
                return Err(Error::invalid("idempotency_key is required"));
            }
            let mut trusted = body.clone();
            trusted["actor_id"] = json!(actor_id);
            freeze_report(s, id, &trusted, Some(command_id))
        }
        "src.report.reproduction.record" => record_reproduction(s, id, body, Some(command_id)),
        "src.report.render" | "src.submission.export" => {
            let output = if kind == "src.report.render" {
                render_report(s, id, body)?
            } else {
                export_report(s, id, body)?
            };
            let marker = json!({"command_id":command_id,"output":output});
            if s.store
                .put_if_absent(REPORT_APPLIED, command_id, &marker)
                .map_err(super::db)?
            {
                Ok(output)
            } else {
                Ok(s.store
                    .get(REPORT_APPLIED, command_id)
                    .map_err(super::db)?
                    .ok_or_else(|| Error::conflict("SRC report result disappeared"))?["output"]
                    .clone())
            }
        }
        _ => Err(Error::invalid("unsupported SRC report command")),
    }
}
fn freeze_report(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    s.store.document_scope(||freeze_report_inner(s,id,body,command_id)).map_err(db)?
}
fn freeze_report_inner(s: &AppState, id: &str, body: &Value, command_id: Option<&str>) -> Api<Value> {
    if let Some(command_id) = command_id {
        if let Some(marker) = s.store.get(REPORT_APPLIED, command_id).map_err(super::db)? {
            return Ok(marker["output"].clone());
        }
    }
    let finding = require(body, "finding_id")?;
    let source = require(body, "source_run_id")?;
    let canonical = report_rules::canonical_document(&body["document"])?;
    let evidence = body["evidence_refs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for item in &evidence {
        verify_ref(s, id, item)?;
    }
    report_rules::scope(s, id, &canonical)?;
    report_rules::evidence_contract(s, id, &canonical)?;
    let raw_readiness = &body["review"]["readiness"];
    let readiness = raw_readiness
        .as_str()
        .or_else(|| raw_readiness["status"].as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    if readiness == "DRAFT" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "draft_evidence_incomplete",
            "Cannot freeze report while readiness is DRAFT; supplement baseline/quantitative evidence first",
        ));
    }
    if body["residual_object_waiver_reason"].as_str().is_none_or(|v| v.trim().is_empty()) {
        for row in records(s, id, "object_experiment")? {
            let exp = &row["payload"];
            let matches_scope = exp["finding_id"] == finding
                || exp["actor"]["run_id"] == source
                || (exp["finding_id"].is_null() && exp["actor"]["run_id"].is_null());
            if !matches_scope {
                continue;
            }
            let st = exp["restoration"]["status"].as_str().unwrap_or("");
            if !matches!(st, "restored" | "not_applicable") {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    "uncleaned_test_objects_remaining",
                    format!(
                        "Cannot freeze report: experiment '{}' retains uncleaned state ('{}'); run src.object.experiment.restore first",
                        row["record_id"].as_str().unwrap_or("unknown"),
                        if st.is_empty() { "unrestored" } else { st }
                    ),
                ));
            }
        }
    }
    if readiness == "GREEN" {
        report_rules::green(s, id, finding, &body["review"], &body["cvss"], &evidence)?;
    } else {
        if body["cvss"]["metric_reasons"].is_object() {
            finding::assess_cvss(&body["cvss"])?;
        }
        if records(s, id, "finding_review")?
            .iter()
            .any(|row| row["payload"]["finding_id"] == finding)
        {
            finding::refresh_staleness(s, id, finding, body["review"].get("dependencies"))?;
            let status = finding::status(s, id, &json!({"finding_id": finding}))?;
            if status["stale"] == true {
                return Err(Error::conflict(
                    "cannot freeze report against a stale Finding review",
                ));
            }
        }
    }
    if let Some(supersedes) = body["supersedes"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        if report(s, id, supersedes)?["finding_id"] != finding {
            return Err(Error::conflict(
                "a report version cannot supersede another Finding",
            ));
        }
    }
    let template_spec = body
        .get("template")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!("generic-zh-v1"));
    let template = template_for(&template_spec)?;
    let required = template["constraints"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let template_missing: Vec<_> = required
        .into_iter()
        .filter(|v| {
            v.as_str().is_some_and(|key| {
                canonical[key].is_null() || canonical[key] == "" || canonical[key] == json!([])
            })
        })
        .collect();
    let template_unmapped: Vec<_> = report_rules::CANONICAL_FIELDS
        .iter()
        .filter(|field| template["field_mapping"].get(**field).is_none())
        .map(|field| json!(field))
        .collect();
    let coverage = coverage::audit(s, id)?;
    let source_run_ids = report_rules::source_run_ids(source, &evidence, &canonical);
    let effective_severity = report_rules::effective_severity(&body["review"], &body["cvss"]);
    let mut auth_snapshot = body["authorization_snapshot"].clone();
    let mut plan_snapshot = body["plan_snapshot"].clone();
    let bindings = records(s, id, "run_binding")?;
    let contributing = bindings.iter().map(|r| &r["payload"]).filter(|b|
        source_run_ids.as_array().is_some_and(|ids| ids.contains(&b["run_id"])));
    let mut plans = std::collections::BTreeSet::new();
    for binding in contributing {
        if let Some(plan) = binding["plan_ref"].as_str().filter(|v| !v.is_empty()) {
            plans.insert(plan.to_owned());
        }
    }
    let stored_plans = records(s, id, "research_plan")?;
    let latest_plan = stored_plans
        .iter()
        .max_by_key(|row| row["revision"].as_u64().unwrap_or(0))
        .map(|row| &row["payload"]);
    if plan_snapshot.is_null() || plan_snapshot.as_object().is_some_and(|v| v.is_empty()) {
        if let Some(plan) = latest_plan {
            plan_snapshot = json!({
                "present": true,
                "plan_id": plan["plan_id"],
                "version": plan["version"],
                "source_run_plan_refs": plans
            });
        } else if !plans.is_empty() {
            plan_snapshot = json!({"present": true, "source_run_plan_refs": plans});
            if plans.len() == 1 {
                plan_snapshot["record_id"] = json!(plans.first());
            }
        } else {
            plan_snapshot = json!({"present": false});
        }
    }
    if auth_snapshot.is_null() || auth_snapshot.as_object().is_some_and(|v| v.is_empty()) {
        let current = policy(s, id)?;
        auth_snapshot = json!({
            "record_id": format!("{}:v{}", require(&current, "policy_id")?, current["version"]),
            "at": "report_freeze",
            "policy_version": current["version"],
            "policy_digest": current["digest"],
            "allowed_targets": current["authorized_targets"],
            "authorized_targets": current["authorized_targets"],
            "excluded_targets": current["excluded_targets"]
        });
    } else if auth_snapshot.is_object() && auth_snapshot["allowed_targets"].is_null() {
        auth_snapshot["allowed_targets"] = auth_snapshot["authorized_targets"].clone();
    }

    let counterevidence = if !body["counterevidence"].is_null() {
        body["counterevidence"].clone()
    } else if !canonical["counterevidence"].is_null() {
        canonical["counterevidence"].clone()
    } else {
        let mut refs = Vec::new();
        for claim in body["claims"].as_array().into_iter().flatten() {
            for r in claim["counterevidence_refs"].as_array().into_iter().flatten() {
                if !refs.contains(r) {
                    refs.push(r.clone());
                }
            }
        }
        json!({"items": refs, "checked": !refs.is_empty()})
    };

    let head_key = format!("{id}:{finding}");
    loop {
        let head = s.store.get(REPORT_HEADS, &head_key).map_err(super::db)?;
        let siblings = payloads(s, id, "frozen_report")?
            .into_iter()
            .filter(|v| v["finding_id"] == finding)
            .filter_map(|v| v["report_seq"].as_u64())
            .max()
            .unwrap_or(0);
        let report_seq = head
            .as_ref()
            .and_then(|v| v["seq"].as_u64())
            .unwrap_or(0)
            .max(siblings)
            + 1;
        let rid = format!("sreport-{}", Uuid::new_v4().simple());
        let mut value = json!({"schema":"muteki.src.submission.v1","product_mode":"src","task_id":id,
        "report_id":rid,"report_seq":report_seq,"finding_id":finding,"source_run_id":source,
        "source_run_ids":source_run_ids,"document":canonical,"claims":body["claims"].as_array().cloned().unwrap_or_default(),
        "evidence_refs":evidence,"counterevidence":counterevidence,"review":body["review"],"cvss":body["cvss"],
        "deduplication":body["deduplication"],"effective_severity":effective_severity,
        "coverage":coverage,"completion_status":coverage["status"],"template":template,
        "renderer_version":"src-submission-renderer.v1","plan_snapshot":plan_snapshot,
        "authorization_snapshot":auth_snapshot,"source_watermarks":body["source_watermarks"],
        "template_missing":template_missing,"template_unmapped":template_unmapped,
        "supersedes":body["supersedes"],"invalidates":[],
        "feedback_responses":body["feedback_responses"].as_array().cloned().unwrap_or_default(),
        "frozen_by":body["actor_id"].as_str().unwrap_or("local-operator"),"frozen_at":now()});
        let rendered = report_render::render(s, id, &value, &json!({}))?;
        value["rendered_markdown"] = rendered["markdown"].clone();
        let (record_key, _, row) = stage_rec(s, id, "frozen_report", &rid, value.clone(), Some(0))?;
        let record_doc = json!({"version":1,"values":row});
        let next_head = json!({"seq":report_seq,"finding_id":finding,"last_report_id":rid});
        let saved = if let Some(command_id) = command_id {
            let marker = json!({"command_id":command_id,"output":value});
            s.store
                .put_many_if(
                    &[
                        (REPORT_HEADS, &head_key, head.as_ref()),
                        (RECORDS, &record_key, None),
                        (REPORT_APPLIED, command_id, None),
                    ],
                    &[
                        (REPORT_HEADS, &head_key, &next_head),
                        (RECORDS, &record_key, &record_doc),
                        (REPORT_APPLIED, command_id, &marker),
                    ],
                )
                .map_err(super::db)?
        } else {
            s.store
                .put_many_if(
                    &[
                        (REPORT_HEADS, &head_key, head.as_ref()),
                        (RECORDS, &record_key, None),
                    ],
                    &[
                        (REPORT_HEADS, &head_key, &next_head),
                        (RECORDS, &record_key, &record_doc),
                    ],
                )
                .map_err(super::db)?
        };
        if saved {
            return Ok(value);
        }
        if let Some(command_id) = command_id {
            if let Some(marker) = s.store.get(REPORT_APPLIED, command_id).map_err(super::db)? {
                return Ok(marker["output"].clone());
            }
        }
    }
}
const PLATFORM_STATUS_HEADS: &str = "src_platform_status_heads";
const PLATFORM_STATES: &[&str] = &[
    "not_submitted",
    "submitted",
    "pending_info",
    "accepted",
    "duplicate",
    "rejected",
    "reward_pending",
    "reward_confirmed",
    "reward_paid",
];

fn record_status(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let actor_kind = body["actor_kind"].as_str().unwrap_or("operator");
    let rid = require(body, "report_id")?;
    let frozen = report(s, id, rid)?;
    let status = require(body, "status")?;
    if !PLATFORM_STATES.contains(&status) {
        return Err(Error::conflict(format!(
            "unsupported platform status {status:?}"
        )));
    }
    let explicit_amount = match body.get("amount") {
        None => None,
        Some(Value::Null) => Some(Value::Null),
        Some(Value::Bool(_)) => return Err(Error::conflict("reward amount must be non-negative")),
        Some(Value::Number(n)) => {
            let val = n
                .as_f64()
                .ok_or_else(|| Error::conflict("reward amount must be non-negative"))?;
            if !val.is_finite() || val < 0.0 {
                return Err(Error::conflict("reward amount must be non-negative"));
            }
            Some(json!(val))
        }
        _ => return Err(Error::conflict("reward amount must be non-negative")),
    };
    let head_key = format!("{id}:{rid}");
    loop {
        let head = s.store.get(PLATFORM_STATUS_HEADS, &head_key).map_err(super::db)?;
        let prev = payloads(s, id, "platform_status")?
            .into_iter()
            .filter(|v| v["report_id"] == rid)
            .max_by_key(|v| v["revision"].as_u64().unwrap_or(0));
        let version = head
            .as_ref()
            .and_then(|v| v["revision"].as_u64())
            .unwrap_or(0)
            .max(
                prev.as_ref()
                    .and_then(|v| v["revision"].as_u64())
                    .unwrap_or(0),
            );
        if body["base_revision"].as_u64() != Some(version) {
            let mut err = Error::conflict("提交台账版本已变化");
            err.detail = json!({"expected": body["base_revision"], "actual": version});
            return Err(err);
        }
        let actor_id = body["actor_id"].as_str().unwrap_or("local-operator");
        let amount = explicit_amount.clone().unwrap_or_else(|| {
            prev.as_ref()
                .map(|v| v["amount"].clone())
                .unwrap_or(Value::Null)
        });
        let platform_reference = body
            .get("platform_reference")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                prev.as_ref()
                    .and_then(|v| v["platform_reference"].as_str())
                    .unwrap_or("")
                    .to_owned()
            });
        let notes = body
            .get("notes")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                prev.as_ref()
                    .and_then(|v| v["notes"].as_str())
                    .unwrap_or("")
                    .to_owned()
            });
        let currency = body
            .get("currency")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                prev.as_ref()
                    .and_then(|v| v["currency"].as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "CNY".to_owned());
        let platform = match body.get("platform") {
            Some(Value::Null) => Value::Null,
            Some(v) => v
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| json!(s))
                .unwrap_or_else(|| {
                    prev.as_ref()
                        .map(|p| p["platform"].clone())
                        .unwrap_or(Value::Null)
                }),
            None => prev
                .as_ref()
                .map(|p| p["platform"].clone())
                .unwrap_or(Value::Null),
        };
        let paid_at = match body.get("paid_at") {
            Some(Value::Null) => Value::Null,
            Some(v) => v
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| json!(s))
                .unwrap_or(Value::Null),
            None => prev
                .as_ref()
                .map(|p| p["paid_at"].clone())
                .unwrap_or(Value::Null),
        };
        let recorded_at = now();
        let status_id = format!("splatform-{}", Uuid::new_v4().simple());
        let old_value = prev.clone().unwrap_or(Value::Null);
        let new_value = json!({
            "status": status,
            "platform_reference": platform_reference,
            "platform": platform,
            "amount": amount,
            "currency": currency,
            "paid_at": paid_at,
            "notes": notes,
        });
        let entry = json!({
            "status_id": status_id,
            "task_id": id,
            "report_id": rid,
            "finding_id": frozen["finding_id"],
            "revision": version + 1,
            "status": status,
            "at": recorded_at,
            "recorded_at": recorded_at,
            "recorded_by": actor_id,
            "actor_id": actor_id,
            "actor_kind": actor_kind,
            "platform_reference": platform_reference,
            "platform": platform,
            "amount": amount,
            "currency": currency,
            "paid_at": paid_at,
            "notes": notes,
            "old_value": old_value,
            "new_value": new_value,
            "from_status": prev.as_ref().map(|v| v["status"].clone()).unwrap_or(json!("not_submitted")),
            "verification": "用户记录，未与平台核验"
        });
        let (record_key, _, row) =
            stage_rec(s, id, "platform_status", &status_id, entry.clone(), Some(0))?;
        let record_doc = json!({"version": 1, "values": row});
        let next_head = json!({"revision": version + 1});
        if s.store
            .put_many_if(
                &[
                    (PLATFORM_STATUS_HEADS, &head_key, head.as_ref()),
                    (RECORDS, &record_key, None),
                ],
                &[
                    (PLATFORM_STATUS_HEADS, &head_key, &next_head),
                    (RECORDS, &record_key, &record_doc),
                ],
            )
            .map_err(super::db)?
        {
            return Ok(entry);
        }
    }
}
fn platform_status(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    let rid = require(p, "report_id")?;
    report(s, id, rid)?;
    Ok(payloads(s,id,"platform_status")?.into_iter().filter(|v|v["report_id"]==rid)
        .max_by_key(|v|v["revision"].as_u64().unwrap_or(0))
        .unwrap_or_else(||json!({"task_id":id,"report_id":rid,"revision":0,"status":"not_submitted","verification":"用户记录，未与平台核验"})))
}
fn report_diff(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    let old = report(s, id, require(p, "old_report_id")?)?;
    let new = report(s, id, require(p, "new_report_id")?)?;
    if old["finding_id"] != new["finding_id"] {
        return Err(Error::conflict("reports belong to different Findings"));
    }
    let mut changes = Vec::new();
    for (field, path) in [
        ("claims", "/claims"),
        ("impact", "/document/confirmed_impact"),
        ("conditional_impact", "/document/conditional_impact"),
        ("steps", "/document/steps"),
        ("cvss", "/cvss"),
        ("evidence", "/evidence_refs"),
        ("readiness", "/review/readiness"),
        ("template", "/template"),
        ("document", "/document"),
    ] {
        let before = old.pointer(path).unwrap_or(&Value::Null);
        let after = new.pointer(path).unwrap_or(&Value::Null);
        if before != after {
            changes.push(json!({"field":field,"path":path.trim_start_matches('/').replace('/', "."),"before":before,"after":after}));
        }
    }
    let responses: std::collections::HashMap<String, Value> = new["feedback_responses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row["feedback_id"].as_str()?.to_owned(), row.clone())))
        .collect();
    let new_seq = new["report_seq"].as_u64().unwrap_or(0);
    let mut finding_report_ids: std::collections::HashSet<String> = payloads(s, id, "frozen_report")?
        .into_iter()
        .filter(|row| row["finding_id"] == old["finding_id"] && row["report_seq"].as_u64().unwrap_or(0) <= new_seq)
        .filter_map(|row| row["report_id"].as_str().map(str::to_owned))
        .collect();
    if let Some(oid) = old["report_id"].as_str() {
        finding_report_ids.insert(oid.to_owned());
    }
    if let Some(nid) = new["report_id"].as_str() {
        finding_report_ids.insert(nid.to_owned());
    }
    let mut feedback = Vec::new();
    for mut item in payloads(s, id, "platform_feedback")? {
        if item["report_id"]
            .as_str()
            .is_some_and(|rid| finding_report_ids.contains(rid))
        {
            let resp = item["feedback_id"]
                .as_str()
                .and_then(|fid| responses.get(fid))
                .cloned()
                .unwrap_or_else(|| json!({"status": "unknown"}));
            item["response"] = resp;
            feedback.push(item);
        }
    }
    let unresolved: Vec<Value> = feedback
        .iter()
        .filter(|row| row["response"]["status"] != "answered")
        .cloned()
        .collect();
    Ok(json!({
        "finding_id": old["finding_id"],
        "old_report_id": old["report_id"],
        "new_report_id": new["report_id"],
        "changes": changes,
        "feedback": feedback,
        "unresolved": unresolved,
        "note": "包含主张、影响、步骤、CVSS、证据、准备度和模板变化；不仅是正文文本 diff"
    }))
}
fn connector_manifests() -> Value {
    connector_config::manifests()
}
fn connector_manifest(id: &str) -> Api<Value> {
    connector_config::manifest(id)
}
fn save_connector_config(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    connector_config::apply_command(
        s,
        id,
        "src.connector.config.save",
        body,
        &format!("compat-cmd-{}", Uuid::new_v4().simple()),
        "local-operator",
    )
    .map(|record| record["payload"].clone())
}
fn connector_config(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    connector_config::config(s, id, p)
}
fn revoke_connector_config(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    connector_config::apply_command(
        s,
        id,
        "src.connector.config.revoke",
        body,
        &format!("compat-cmd-{}", Uuid::new_v4().simple()),
        "local-operator",
    )
    .map(|record| record["payload"].clone())
}
fn monitor_payload(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    task(s, id)?;
    let payload = body.get("payload").unwrap_or(body);
    if !payload.is_object() {
        return Err(Error::invalid("monitor payload must be an object"));
    }
    let source = &payload["source"];
    require(source, "connector_id")?;
    require(source, "capability_manifest_ref")?;
    require(payload, "authorization_ref")?;
    if !matches!(payload["chain_type"].as_str(), Some("asset" | "rule")) {
        return Err(Error::invalid("monitor chain_type must be asset or rule"));
    }
    let interval = payload["schedule"]["interval_seconds"]
        .as_u64()
        .unwrap_or(0);
    if interval < 1 {
        return Err(Error::invalid("monitor interval_seconds must be positive"));
    }
    if payload["schedule"]["timezone"]
        .as_str()
        .is_some_and(|v| v.parse::<chrono_tz::Tz>().is_err())
    {
        return Err(Error::invalid("monitor timezone is unknown"));
    }
    let total = payload["budget"]["total"].as_f64().unwrap_or(0.0);
    let per_cycle = payload["budget"]["per_cycle"].as_f64().filter(|value|*value!=0.0).unwrap_or(total);
    if !total.is_finite() || !per_cycle.is_finite() || total < 0.0 || per_cycle <= 0.0 {
        return Err(Error::invalid(
            "monitor budget must be positive and bounded",
        ));
    }
    if payload["budget"]["call_limit"]
        .as_i64()
        .is_some_and(|v| v < 0)
        || payload["notification"]["merge_window_seconds"]
            .as_i64()
            .is_some_and(|v| v < 0)
    {
        return Err(Error::invalid(
            "monitor call limit and merge window cannot be negative",
        ));
    }
    let mut normalized = payload.clone();
    normalized["budget"]["per_cycle"]=json!(per_cycle);
    if !matches!(source["connector_id"].as_str(), Some("github" | "gitlab")) {
        if !normalized["baseline_policy"].is_object() {
            normalized["baseline_policy"] = json!({});
        }
        if !normalized["baseline_policy"]["denoise"].is_object() {
            normalized["baseline_policy"]["denoise"] = json!({});
        }
        if normalized["baseline_policy"]["denoise"]["version"].is_null() {
            normalized["baseline_policy"]["denoise"]["version"] = json!("v1");
        }
        if normalized["budget"]["call_limit"].is_null() {
            normalized["budget"]["call_limit"] = json!((per_cycle.ceil() as u64).max(1));
        }
        normalized["plan_revision"] = payload["plan_ref"]
            .as_str()
            .and_then(|reference| rec(s, id, "research_plan", reference).ok())
            .map(|row| row["revision"].clone())
            .unwrap_or(Value::Null);
    }
    Ok(normalized)
}
fn create_monitor(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let payload = monitor_payload(s, id, body)?;
    if payload["continuous"] != true {
        return Err(Error::invalid(
            "continuous monitoring requires explicit confirmation",
        ));
    }
    monitor_runtime::preflight_for_create(s, id, &payload)?;
    let activation = monitor_runtime::active_monitor(s, id, &payload)?;
    let mid = format!("smon-{}", Uuid::new_v4().simple());
    let mut definition = payload.clone();
    definition["monitor_id"] = json!(mid);
    definition["task_id"] = json!(id);
    definition["version"] = json!(1);
    let mut value = json!({"monitor_id":mid,"task_id":id,"definition":definition,"name":payload["name"],
        "chain_type":payload["chain_type"],"source":payload["source"],
        "authorization_ref":payload["authorization_ref"],"version":1,"revision":1,
        "created_at":now(),"updated_at":now(),"cycles":[],"baseline":null});
    for (key, entry) in activation.as_object().unwrap() {
        value[key] = entry.clone();
    }
    value["current_version"]=json!(1);
    if !atomic_records(s,id,&[("monitor",&mid,value.clone(),Some(0)),("monitor_definition",&format!("{mid}:v1"),definition,Some(0))])?{return Err(Error::conflict("monitor was created concurrently"));}
    Ok(value)
}
fn revise_monitor(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let mid = require(body, "monitor_id")?;
    let row = rec(s, id, "monitor", mid)?;
    let revision = row["revision"].as_u64().unwrap_or(0);
    if body["expected_revision"].as_u64() != Some(revision) {
        return Err(Error::conflict("monitor revision changed"));
    }
    let mut merged = row["payload"]["definition"].clone();
    let patch = body.get("payload").unwrap_or(body);
    if let Some(entries) = patch.as_object() {
        for (key, value) in entries {
            merged[key] = value.clone();
        }
    }
    merged["continuous"] = json!(true);
    let mut payload = monitor_payload(s, id, &json!({"payload":merged}))?;
    let mut value = row["payload"].clone();
    let old_version = value["version"].as_u64().unwrap_or(1);
    let new_version = old_version + 1;
    payload["monitor_id"] = json!(mid);
    payload["task_id"] = json!(id);
    payload["version"] = json!(new_version);
    value["definition"] = payload.clone();
    value["name"] = payload["name"].clone();
    value["source"] = payload["source"].clone();
    value["authorization_ref"] = payload["authorization_ref"].clone();
    value["version"] = json!(new_version);
    value["revision"] = json!(revision + 1);
    let unresolved = matches!(
        value["blocked_reason"].as_str(),
        Some("cost_unknown" | "external_outcome_unknown")
    );
    value["status"] = json!("active");
    if !unresolved {
        value["blocked_reason"] = json!("");
        value["activation_reason"] = Value::Null;
    }
    value["next_wake_at"] = if unresolved {
        Value::Null
    } else {
        json!(now())
    };
    value["updated_at"] = json!(now());
    value["current_version"]=json!(new_version);
    if !atomic_records(s,id,&[("monitor",row["natural_key"].as_str().unwrap_or(mid),value.clone(),Some(revision)),("monitor_definition",&format!("{mid}:v{new_version}"),payload,Some(0))])?{return Err(Error::conflict("monitor revision changed"));}
    Ok(value)
}
fn monitor_state(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let mid = require(body, "monitor_id")?;
    let row = rec(s, id, "monitor", mid)?;
    let revision = row["revision"].as_u64().unwrap_or(0);
    if body["expected_revision"].as_u64() != Some(revision) {
        return Err(Error::conflict("monitor revision changed"));
    }
    let state = require(body, "state")?;
    if !matches!(state, "active" | "paused" | "cancelled") {
        return Err(Error::invalid("invalid monitor state"));
    }
    let mut value = row["payload"].clone();
    value["next_wake_at"] = if state == "active" {
        json!(now())
    } else {
        Value::Null
    };
    value["status"] = json!(state);
    if state == "cancelled" {
        value["blocked_reason"] = json!("cancelled_by_user");
        if let Some(cycle_id) = value["current_cycle_id"].as_str().filter(|v| !v.is_empty()) {
            let cycle_row = rec(s, id, "monitor_cycle", cycle_id)?;
            if matches!(
                cycle_row["payload"]["status"].as_str(),
                Some("scheduled" | "running")
            ) {
                let mut cycle = cycle_row["payload"].clone();
                cycle["cancel_requested"] = json!(true);
                save_rec(
                    s,
                    id,
                    "monitor_cycle",
                    cycle_id,
                    cycle,
                    cycle_row["revision"].as_u64(),
                )?;
            }
        }
    }
    value["revision"] = json!(revision + 1);
    value["updated_at"] = json!(now());
    save_rec(
        s,
        id,
        "monitor",
        row["natural_key"].as_str().unwrap_or(mid),
        value.clone(),
        Some(revision),
    )?;
    Ok(value)
}
pub(super) fn monitor_get(s: &AppState, id: &str, p: &Value) -> Api<Value> {
    let mid = require(p, "monitor_id")?;
    let row = rec(s, id, "monitor", mid)?;
    let mut value = row["payload"].clone();
    value["revision"] = row["revision"].clone();
    value["current_version"]=value["version"].clone();
    let mut cycles=records_with_rowid(s,id,"monitor_cycle")?;cycles.retain(|(_,row)|row["payload"]["monitor_id"]==mid);cycles.sort_by_key(|(rowid,_)|std::cmp::Reverse(*rowid));
    value["cycles"]=json!(cycles.into_iter().take(20).map(|(_,row)|row["payload"].clone()).collect::<Vec<_>>());
    let mut baselines=records_with_rowid(s,id,"monitor_baseline")?;baselines.retain(|(_,row)|row["payload"]["monitor_id"]==mid);baselines.sort_by_key(|(rowid,_)|std::cmp::Reverse(*rowid));
    value["baseline"]=baselines.first().map(|(_,row)|row["payload"].clone()).unwrap_or(Value::Null);
    Ok(value)
}

fn put_artifact(s: &AppState, id: &str, bytes: &[u8], name: &str, media_type: &str) -> Api<Value> {
    let artifact_id = format!("sart_{}", Uuid::new_v4().simple());
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let in_cas = write_cas(s, id, &sha256, bytes)?;
    let value = if in_cas {
        json!({"artifact_id":artifact_id,"version":1,"task_id":id,"size":bytes.len(),"sha256":sha256,"cas_sha256":sha256,"name":name,"media_type":media_type,"data":null,"created_at":now()})
    } else {
        json!({"artifact_id":artifact_id,"version":1,"task_id":id,"size":bytes.len(),"sha256":sha256,"name":name,"media_type":media_type,
            "data":base64::engine::general_purpose::STANDARD.encode(bytes),"created_at":now()})
    };
    s.store
        .put_if_absent("src_artifacts", &format!("{id}:{artifact_id}"), &value)
        .map_err(super::db)?;
    Ok(
        json!({"artifact_id":artifact_id,"version":1,"task_id":id,"size":bytes.len(),"sha256":sha256,"name":name,
        "media_type":media_type,"original":true,"complete":"complete"}),
    )
}
fn record_import_exchange(
    s: &AppState,
    id: &str,
    batch_id: &str,
    index: usize,
    row: &Value,
) -> Api<Option<String>> {
    let raw_http = row["attributes"]["raw_http"] == true;
    let har = row["attributes"]["har_derived"] == true;
    let burp = row["attributes"]["burp_xml"] == true;
    if !raw_http && !har && !burp {
        return Ok(None);
    }
    let source_key = format!("{batch_id}:{index}");
    if let Ok(prior) = rec(s, id, "import_exchange", &source_key) {
        return Ok(prior["payload"]["exchange_id"].as_str().map(str::to_owned));
    }
    let url = require(&row["identity"], "url")?;
    let method = row["identity"]["method"].as_str().unwrap_or("GET");
    let (request_bytes, response_bytes, media_type, limitations) = if raw_http || burp {
        let request = base64::engine::general_purpose::STANDARD
            .decode(row["raw"]["request_base64"].as_str().unwrap_or(""))
            .map_err(|_| Error::invalid("imported HTTP request base64 invalid"))?;
        let response = base64::engine::general_purpose::STANDARD
            .decode(row["raw"]["response_base64"].as_str().unwrap_or(""))
            .map_err(|_| Error::invalid("imported HTTP response base64 invalid"))?;
        let limitation = if response.is_empty() {
            "import contains request only"
        } else {
            "imported source was not live replayed"
        };
        (request, response, "message/http", limitation)
    } else {
        (
            serde_json::to_vec(&row["raw"]["request"]).map_err(super::db)?,
            serde_json::to_vec(&row["raw"]["response"]).map_err(super::db)?,
            "application/json",
            "HAR provides structured capture, not raw wire bytes",
        )
    };
    let request_ref = put_artifact(
        s,
        id,
        &request_bytes,
        &format!("{batch_id}-{index}-request"),
        media_type,
    )?;
    let response_ref = if response_bytes.is_empty() {
        Value::Null
    } else {
        put_artifact(
            s,
            id,
            &response_bytes,
            &format!("{batch_id}-{index}-response"),
            media_type,
        )?
    };
    let exchange_id = format!("simportexchange-{}", Uuid::new_v4().simple());
    let capture_state = if har {
        "partial"
    } else if response_ref.is_null() {
        "partial"
    } else {
        "complete"
    };
    let value = json!({"exchange_id":exchange_id,"task_id":id,"source_kind":"import",
        "observed_url":url,"declared_target":url,"method":method,"source_run_id":null,
        "protocol":row["raw"]["request"]["httpVersion"].as_str()
            .or_else(||row["attributes"]["protocol"].as_str()).unwrap_or("unknown"),
        "request_headers":row["raw"]["request"]["headers"].as_array()
            .or_else(||row["attributes"]["request_headers"].as_array()).cloned().unwrap_or_default(),
        "response_headers":row["raw"]["response"]["headers"].as_array().cloned().unwrap_or_default(),
        "request_raw_ref":request_ref,"response_raw_ref":response_ref,
        "status_code":row["attributes"]["status_code"],"capture_state":capture_state,
        "capture_limitations":[limitations],"execution_state":"completed","actor_ref":"human-import",
        "metadata":{"live_replay":false,"batch_id":batch_id,"source_format":if raw_http{"raw_http"}else if burp{"burp_xml"}else{"har"}}});
    save_rec(
        s,
        id,
        "import_exchange",
        &source_key,
        value,
        Some(0),
    )?;
    let _ = traffic_index::backfill_missing(s, id);
    Ok(Some(exchange_id))
}
fn ingest(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    ingestion::ingest(s, id, body)
}

fn revert_ingestion(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    s.store.document_scope(||revert_ingestion_inner(s,id,body)).map_err(db)?
}
fn revert_ingestion_inner(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let bid = require(body, "batch_id")?;
    let actor_id = body["actor_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("local-operator");
    let row = rec(s, id, "import_batch", bid)
        ?;
    let batch_kind = row["kind"].as_str().unwrap_or("import_batch").to_owned();
    if row["payload"]["reverted_at"].is_string() {
        return Ok(json!({
            "batch_id": bid,
            "status": "reverted",
            "reverted_at": row["payload"]["reverted_at"],
            "removed_observations": 0,
            "removed_assets": 0,
            "preserved_assets": row["payload"]["revert"]["preserved_assets"].as_array().cloned().unwrap_or_default(),
            "already_reverted": true,
        }));
    }
    let batch = &row["payload"];
    let batch_obs_ids: std::collections::HashSet<String> = batch["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["observation_id"].as_str().map(str::to_owned))
        .collect();
    let batch_source_kind = batch["source_kind"].as_str().unwrap_or("");
    let batch_source_id = batch["source_id"].as_str().unwrap_or(bid);
    let prefix = format!("{bid}:");

    let all_observations = records(s, id, "asset_observation")?;
    let mut obs_to_remove = Vec::new();
    let mut touched_assets = std::collections::BTreeSet::new();
    for aid in batch["created_asset_ids"].as_array().into_iter().flatten() {
        if let Some(aid) = aid.as_str().filter(|v| !v.is_empty()) {
            touched_assets.insert(aid.to_owned());
        }
    }
    for obs_row in &all_observations {
        let obs = &obs_row["payload"];
        let oid = obs["observation_id"]
            .as_str()
            .or_else(|| obs_row["record_id"].as_str())
            .unwrap_or("");
        let matches_item = !oid.is_empty() && batch_obs_ids.contains(oid);
        let matches_event = obs["source_id"] == batch_source_id
            && (obs["source_kind"] == batch_source_kind
                || (batch_source_kind != "external_event" && obs["source_kind"] == "import"))
            && obs["source_event_id"]
                .as_str()
                .is_some_and(|ev| ev.starts_with(&prefix) || ev.parse::<usize>().is_ok());
        if matches_item || matches_event {
            if let Some(aid) = obs["asset_id"].as_str().filter(|v| !v.is_empty()) {
                touched_assets.insert(aid.to_owned());
            }
            obs_to_remove.push(obs_row.clone());
        }
    }
    let removed_obs_ids: std::collections::HashSet<String> = obs_to_remove
        .iter()
        .filter_map(|r| {
            r["payload"]["observation_id"]
                .as_str()
                .or_else(|| r["record_id"].as_str())
                .map(str::to_owned)
        })
        .collect();
    let removed_observations = obs_to_remove.len();

    let all_task_records = s.store.list(RECORDS).map_err(super::db)?;
    let mut delete_keys = Vec::new();
    for obs_row in &obs_to_remove {
        let oid = obs_row["payload"]["observation_id"]
            .as_str()
            .or_else(|| obs_row["record_id"].as_str())
            .unwrap_or("");
        let storage_key = rec_key(id, "asset_observation", oid);
        delete_keys.push((RECORDS.to_owned(), storage_key));
        if let Some(ev) = obs_row["payload"]["source_event_id"]
            .as_str()
            .filter(|v| !v.is_empty())
        {
            let source_key = format!(
                "{id}:{}:{}:{ev}",
                obs_row["payload"]["source_kind"].as_str().unwrap_or(""),
                obs_row["payload"]["source_id"].as_str().unwrap_or("")
            );
            delete_keys.push((ASSET_SOURCE_EVENTS.to_owned(), source_key));
        }
    }

    let mut removed_assets = 0usize;
    let mut preserved_assets = Vec::new();
    for aid in &touched_assets {
        let Ok(asset_record) = asset_row(s, id, aid) else {
            continue;
        };
        let remaining: Vec<Value> = all_observations
            .iter()
            .map(|r| r["payload"].clone())
            .filter(|p| {
                p["asset_id"] == aid.as_str()
                    && p["revoked_at"].is_null()
                    && !p["observation_id"]
                        .as_str()
                        .is_some_and(|oid| removed_obs_ids.contains(oid))
            })
            .collect();
        let natural_key = asset_record["natural_key"].as_str().unwrap_or(aid);
        if remaining.is_empty() && asset_record["payload"]["lifecycle"] == "active" {
            let mut protected = Vec::new();
            for stored in &all_task_records {
                let record = &stored["values"];
                let rkind = record["kind"].as_str().unwrap_or("");
                let rtask = record["task_id"].as_str().unwrap_or("");
                let intrinsic = matches!(
                    rkind,
                    "asset"
                        | "asset_observation"
                        | "asset_tombstone"
                        | "asset_alias"
                        | "asset_lifecycle"
                        | "import_batch"
                        | "import_batch_revert"
                );
                if !intrinsic
                    && !(rkind == "task_asset_link" && rtask == id)
                    && record["payload"].to_string().contains(aid.as_str())
                {
                    protected.push(json!({
                        "task_id": rtask,
                        "kind": rkind,
                        "record_id": record["record_id"],
                    }));
                }
            }
            if !protected.is_empty() {
                preserved_assets.push(json!({
                    "asset_id": aid,
                    "references": protected,
                }));
                let projected = reproject_asset(asset_record["payload"].clone(), &remaining);
                save_rec(
                    s,
                    id,
                    "asset",
                    natural_key,
                    projected,
                    asset_record["revision"].as_u64(),
                )?;
            } else {
                let asset_storage_key = rec_key(id, "asset", natural_key);
                delete_keys.push((RECORDS.to_owned(), asset_storage_key));
                let tombstone = json!({
                    "asset_id": aid,
                    "type": asset_record["payload"]["type"],
                    "natural_key": natural_key,
                    "lifecycle": "destroyed",
                    "destroyed_by": actor_id,
                    "destroyed_at": now(),
                    "identity": asset_record["payload"]["identity"],
                    "destroy_reason": format!("import batch {bid} reverted"),
                });
                save_rec(s, id, "asset_tombstone", natural_key, tombstone, None)?;
                removed_assets += 1;
            }
        } else {
            let projected = reproject_asset(asset_record["payload"].clone(), &remaining);
            save_rec(
                s,
                id,
                "asset",
                natural_key,
                projected,
                asset_record["revision"].as_u64(),
            )?;
        }
    }

    s.store
        .put_many_if_with_deletes(&[], &[], |_| Ok(Some(delete_keys.clone())))
        .map_err(super::db)?;

    let reverted_at = now();
    let mut value = row["payload"].clone();
    value["reverted_at"] = json!(reverted_at);
    value["reverted_by"] = json!(actor_id);
    value["status"] = json!("reverted");
    value["revert"] = json!({
        "removed_observations": removed_observations,
        "removed_assets": removed_assets,
        "preserved_assets": preserved_assets,
    });
    save_rec(
        s,
        id,
        &batch_kind,
        row["natural_key"].as_str().filter(|v| !v.is_empty()).unwrap_or(bid),
        value,
        row["revision"].as_u64(),
    )?;
    let revert_audit_id = format!("sbatchrev_{}", Uuid::new_v4().simple());
    save_rec(
        s,
        id,
        "import_batch_revert",
        &revert_audit_id,
        json!({
            "batch_id": bid,
            "reverted_by": actor_id,
            "reverted_at": reverted_at,
            "removed_observations": removed_observations,
            "removed_assets": removed_assets,
        }),
        Some(0),
    )?;
    Ok(json!({
        "batch_id": bid,
        "status": "reverted",
        "reverted_at": reverted_at,
        "removed_observations": removed_observations,
        "removed_assets": removed_assets,
        "preserved_assets": preserved_assets,
    }))
}

fn retry_ingestion(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    ingestion::retry(s, id, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn frozen_artifact_ref_rejects_same_size_byte_replacement() {
        let state = AppState::new(crate::store::Store::open(":memory:").unwrap());
        let task_id = "src-artifact-integrity";
        let reference = put_artifact(&state, task_id, b"alpha", "request.http", "message/http").unwrap();
        assert_eq!(reference["sha256"], format!("{:x}", Sha256::digest(b"alpha")));
        verify_ref(&state, task_id, &reference).unwrap();

        let key = format!("{task_id}:{}", reference["artifact_id"].as_str().unwrap());
        let mut stored = state.store.get("src_artifacts", &key).unwrap().unwrap();
        stored["data"] = json!(base64::engine::general_purpose::STANDARD.encode(b"omega"));
        state.store.put("src_artifacts", &key, &stored).unwrap();
        assert_eq!(verify_ref(&state, task_id, &reference).unwrap_err().code, "src.artifact_ref_mismatch");
    }

    #[test]
    fn asset_natural_keys_match_source_normalization() {
        let cases = [
            ("root_domain", json!({"domain":"Example.COM."}),
                "asset-key.v1:root_domain:ace40fd9bfb3c70c5edba30af6677cfe6dcf9386207c8d9fb4ac6df20559cc5a"),
            ("domain", json!({"name":"Exämple.COM."}),
                "asset-key.v1:domain:d0a7f7d2dfb52c6e7c12468e7d05c12ef77cc3533f9bcd77f057123a876dd69f"),
            ("dns", json!({"host":"Example.COM.","record_type":"a","value":"1.2.3.4"}),
                "asset-key.v1:dns:4af0afbf1384c7e517be7c336d6b25b6f14dd05e5d74e64dffd0b6826a13e13c"),
            ("ip", json!({"cidr":"2001:db8::1"}),
                "asset-key.v1:ip:178b268d5738498a62346a44481dd506443a2e54e88560f37ab039659cd24471"),
            ("cidr", json!({"ip":"10.2.3.4"}),
                "asset-key.v1:cidr:79b8352c69bded8bd5c5957e9c00d93ba6d7bcec1d1ac1758ad2f08b2529185f"),
            ("port", json!({"host":"[2001:db8::1]","port":443}),
                "asset-key.v1:port:b10b7d8c196900ade741fa041c755b392d492b6a10580e14e6a6561300717fc1"),
            ("web_service", json!({"host":"Exämple.COM.","port":"8080"}),
                "asset-key.v1:web_service:c9ca97e0078c28e4b1718f675916d427ec5d9d07aea629c0e4756cb6b2fb9e80"),
            ("non_web_service", json!({"host":"10.0.0.1","port":22,"protocol":"ssh"}),
                "asset-key.v1:non_web_service:b569a393db2e343b5ebd0c6fb8ca5bb073085f642c6ea6d1e8c45bf9ce9cd53f"),
            ("http_endpoint", json!({"url":"example.com/login?x=1"}),
                "asset-key.v1:http_endpoint:64631b344da3fc203d81475daee1174043aef041a28171ca163245e8397daab5"),
            ("http_endpoint", json!({"url":"ws://example.com/socket"}),
                "asset-key.v1:http_endpoint:e73ef85f2433e8d3e720df79e1f747d505d88c04866430b15eff68f5a69b806b"),
            ("http_endpoint", json!({"scheme":"HTTPS","host":"Exämple.COM.",
                "path":"login","query":"q=1","method":"post"}),
                "asset-key.v1:http_endpoint:7dee69da202ca65084b98fcff0ebb70a70ab0df0becca76e62e28df7dd86d063"),
            ("parameter", json!({"url":"https://Exämple.COM./search?q=1","method":"post",
                "param_location":"query","param_name":"q","tenant":"tenant-1",
                "identity_ref":"alice","action":"read"}),
                "asset-key.v1:parameter:76e4ae216f2c1eb1d6df74daa108da93fd7a758c4584ebd72f6449e9c1ff9515"),
            ("app", json!({"platform":"Android","name":"测试应用"}),
                "asset-key.v1:app:62f0bf8866d9fae8eeacde097b2449380df77a00124914e1d7d42cc51503a184"),
            ("miniprogram", json!({"platform":"WeChat","appid":"wx123"}),
                "asset-key.v1:miniprogram:7c9a3f45c88a3badcdaff4f800dcb0af45dd06118b62f3cef558acde922511f1"),
            ("repository", json!({"host":"GitHub","canonical":"Owner/Repo"}),
                "asset-key.v1:repository:9a5103a6c3eb34c2ea68fa5570f32664345422c27bfbbcac4685189f52383324"),
            ("cloud", json!({"provider":"AWS","resource_id":"i-123","region":"cn-north-1"}),
                "asset-key.v1:cloud:d3ab581902a48e3d5e85b643a634caa80dd59c94a972ba2cdd441a47215306b7"),
        ];
        for (asset_type, raw, expected) in cases {
            let source = Some(("manual", "source-1"));
            let stored = normalized_identity(asset_type, &raw, source).unwrap();
            let keyed = normalized_identity(asset_type, &stored, source).unwrap();
            assert_eq!(asset_identity_key(asset_type, &keyed), expected, "{asset_type}");
            if asset_type == "app" {
                assert_eq!(stored["candidate_key"],
                    "dd5eb71312f5a400411f88142e24021e9d8b0e896491eaa9caa1edd3a439681c");
            }
        }
        let numeric = normalized_identity("repository", &json!({"platform":"GitHub","stable_id":9001001}), None).unwrap();
        assert_eq!(numeric["platform"], "github");
        assert_eq!(numeric["stable_id"], "9001001");
        let text = normalized_identity("repository", &json!({"platform":"github","stable_id":"stable-text-i65"}), None).unwrap();
        assert_eq!(text["stable_id"], "stable-text-i65");
    }

    #[test]
    fn cidr_identity_accepts_prefix_netmask_and_inverse_mask() {
        let cases = [
            (json!({"ip":"10.2.3.4","mask":"255.255.255.0"}), "10.2.3.0/24"),
            (json!({"address":"10.2.3.4","netmask":"255.255.0.0"}), "10.2.0.0/16"),
            (json!({"ip":"10.2.3.4","inverse_mask":"0.0.0.255"}), "10.2.3.0/24"),
            (json!({"ip":"10.2.3.4","wildcard_mask":"0.0.255.255"}), "10.2.0.0/16"),
            (json!({"cidr":"10.2.3.4/24"}), "10.2.3.0/24"),
        ];
        for (identity, expected) in cases {
            let stored = normalized_identity("cidr", &identity, None).unwrap();
            assert_eq!(stored["address"], expected, "{identity}");
        }
        assert!(normalized_identity("cidr", &json!({"ip":"10.2.3.4","mask":"255.0.255.0"}), None).is_err());
    }

    #[test]
    fn asset_endpoint_paths_remove_dot_segments() {
        let from_url = normalized_identity(
            "http_endpoint",
            &json!({"url":"http://127.0.0.1:4280/a/./b/../login.php","method":"GET"}),
            None,
        )
        .unwrap();
        assert_eq!(from_url["path"], "/a/login.php");

        let structured = normalized_identity(
            "http_endpoint",
            &json!({"scheme":"http","host":"127.0.0.1","port":4280,
                "path":"/a/./b/../login.php","method":"GET"}),
            None,
        )
        .unwrap();
        assert_eq!(structured["path"], "/a/login.php");
        assert_eq!(
            normalized_identity("http_endpoint", &structured, None).unwrap(),
            structured
        );
    }

    #[tokio::test]
    async fn asset_observation_uses_source_key() {
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        let task = super::super::create_task_inner(&state,
            &json!({"title":"Asset keys","goal":"Observe owned assets",
                "idempotency_key":"src-asset-key-parity"})).await.unwrap();
        let task_id = task["output"]["task_id"].as_str().unwrap();
        let source = |name: &str| json!({"asset_type":"domain","identity":{"name":name},
            "source_kind":"manual","source_id":"source-1"});
        let created = observe_asset(&state,task_id,&source("example.com"),None).unwrap();
        let expected = asset_identity_key("domain",&json!({"name":"example.com"}));
        assert_eq!(created["asset"]["natural_key"],expected);
        assert_eq!(created["asset"]["key_version"],"asset-key.v1");
        let repeated = observe_asset(&state,task_id,&source("example.com"),None).unwrap();
        assert_eq!(repeated["action"],"merged");
        assert_eq!(repeated["asset"]["asset_id"],created["asset"]["asset_id"]);

        assert_eq!(records(&state,task_id,"asset").unwrap().len(),1);
    }

    #[tokio::test]
    async fn asset_query_cursors_are_signed_and_bound_to_the_task() {
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        let created = super::super::create_task_inner(&state,
            &json!({"title":"Query cursor","goal":"Review local assets",
                "idempotency_key":"src-query-cursor"})).await.unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let other = super::super::create_task_inner(&state,
            &json!({"title":"Other task","goal":"Separate local assets",
                "idempotency_key":"src-query-cursor-other"})).await.unwrap();
        let other_id = other["output"]["task_id"].as_str().unwrap();
        for name in ["one.example", "two.example"] {
            observe_asset(&state,id,&json!({"asset_type":"domain","identity":{"name":name},
                "source_kind":"manual","source_id":"operator",
                "attributes":{"title":"cursor-needle"}}),None).unwrap();
        }
        for (operation, input) in [
            ("src.asset.query",json!({"expression":"type:domain","limit":1})),
            ("src.asset.full_text",json!({"needle":"cursor-needle","limit":1})),
        ] {
            let first = asset_query::query(&state,id,operation,&input).unwrap();
            assert_eq!(first["total"],2);
            let cursor = first["next_cursor"].as_str().unwrap();
            assert!(cursor.contains(":v1."));
            let next = asset_query::query(&state,id,operation,
                &json!({"cursor":cursor,"limit":1,"needle":"cursor-needle"})).unwrap();
            assert_eq!(next["returned"],1);
            assert!(next["next_cursor"].is_null());
            assert_eq!(asset_query::query(&state,id,operation,
                &json!({"cursor":format!("{cursor}x"),"needle":"cursor-needle"}))
                .unwrap_err().status,StatusCode::BAD_REQUEST);
            assert!(asset_query::query(&state,other_id,operation,
                &json!({"cursor":cursor,"needle":"cursor-needle"})).is_err());
        }
    }

    #[tokio::test]
    async fn external_event_is_idempotent_and_searchable_with_artifact_id() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = super::super::create_task_inner(
            &state,
            &json!({"title":"External event",
            "goal":"Review inbound asset evidence","idempotency_key":"src-external-event-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let event = json!({"event_id":"upstream-1","source_id":"local-feed","assets":[{
            "asset_type":"domain","identity":{"name":"example.com"},"attributes":{"title":"distinctive finding"}}]});
        let first = external_events::ingestion_event(&state, id, &event).unwrap();
        assert!(
            first["artifact"]["artifact_id"]
                .as_str()
                .is_some_and(|v| v.starts_with("sart_"))
        );
        assert!(first["artifact"].get("sha512").is_none());
        assert_eq!(
            external_events::ingestion_event(&state, id, &event).unwrap()["deduplicated"],
            true
        );
        assert_eq!(records(&state, id, "asset_observation").unwrap().len(), 1);
        let mut changed = event.clone();
        changed["assets"][0]["attributes"]["title"] = json!("changed");
        assert_eq!(
            external_events::ingestion_event(&state, id, &changed)
                .unwrap_err()
                .status,
            StatusCode::CONFLICT
        );
        let asset = records(&state, id, "asset").unwrap().remove(0)["payload"].clone();
        let _aid = asset["asset_id"].as_str().unwrap();
        observe_asset(&state,id,&json!({"asset_type":"domain","identity":{"name":"other.example"},
            "source_kind":"manual","source_id":"operator","attributes":{"title":"distinctive finding"}}), None).unwrap();
        let result = asset_query::query(&state,id,"src.asset.full_text",
            &json!({"needle":"distinctive finding","limit":1})).unwrap();
        assert_eq!(result["total"], 2);
        assert_eq!(result["items"][0]["kind"], "asset_observation");
        let cursor = result["next_cursor"].as_str().unwrap();
        let next = asset_query::query(&state,id,"src.asset.full_text",
            &json!({"needle":"distinctive finding","cursor":cursor,"limit":1})).unwrap();
        assert_eq!(next["items"].as_array().unwrap().len(), 1);
        assert!(next["next_cursor"].is_null());
        let tampered = format!("{cursor}x");
        assert_eq!(
            asset_query::query(&state,id,"src.asset.full_text",
                &json!({"needle":"distinctive finding","cursor":tampered}))
            .unwrap_err()
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    #[tokio::test]
    async fn assets_project_corrections_and_record_query_is_paged() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = super::super::create_task_inner(
            &state,
            &json!({"title":"Research","goal":"Inventory",
            "idempotency_key":"asset-domain-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let observed=observe_asset(&state,id,&json!({"asset_type":"domain","identity":{"name":"EXAMPLE.COM."},
            "source_kind":"manual","source_id":"operator","attributes":{"title":"Original"},"manual":true}), None).unwrap();
        let aid = observed["asset"]["asset_id"].as_str().unwrap();
        assert_eq!(observed["asset"]["identity"]["name"], "example.com");
        assert_eq!(
            observe_asset(
                &state,
                id,
                &json!({"asset_type":"domain","identity":{"name":"spoof.example"},
            "source_kind":"worker","source_id":"worker","source_run_id":"other-run"}),
                None,
            )
            .unwrap_err()
            .status,
            StatusCode::FORBIDDEN
        );
        let corrected = correct_asset(
            &state,
            id,
            &json!({"asset_id":aid,"attributes":{"title":"Corrected"},"reason":"review"}),
            None,
            "local-operator",
        )
        .unwrap();
        let detail = asset_detail(&state, id, &json!({"asset_id":aid})).unwrap();
        let original_observation = observed["observation"]["observation_id"].as_str().unwrap();
        let correction_observation = corrected["observation"]["observation_id"].as_str().unwrap();
        let (selected_id, selected_title) = if original_observation > correction_observation {
            (original_observation, "Original")
        } else {
            (correction_observation, "Corrected")
        };
        assert_eq!(detail["asset"]["current_profile"]["title"], selected_title);
        assert_eq!(
            detail["asset"]["profile_sources"]["title"]["observation_id"],
            selected_id
        );
        assert_eq!(
            detail["asset"]["conflicts"]["title"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(detail["observations"].as_array().unwrap().len(), 2);
        let page = super::super::query(
            &state,
            id,
            "src.record.list",
            &json!({"kind":"asset","limit":1}),
        )
        .unwrap();
        assert_eq!(page["total"], 1);
        assert_eq!(page["records"].as_array().unwrap().len(), 1);
        let bulk = asset_bulk(&state, id, &json!({})).unwrap();
        assert_eq!(
            bulk["items"][0]["observations"].as_array().unwrap().len(),
            2
        );
        let child_page = super::super::query(
            &state,
            id,
            "src.record.list",
            &json!({"kind":"asset_observation","parent_id":aid}),
        )
        .unwrap();
        assert_eq!(child_page["total"], 2);
        let searched = asset_query::query(&state,id,"src.asset.query",
            &json!({"expression":"type:domain"})).unwrap();
        assert_eq!(searched["total"], 1);
        let csv = asset_query::query(&state,id,"src.asset.export",
            &json!({"export_format":"csv"})).unwrap();
        assert_eq!(csv[1], "text/csv; charset=utf-8");
        let csv_bytes = base64::engine::general_purpose::STANDARD
            .decode(csv[0]["data"].as_str().unwrap())
            .unwrap();
        assert!(String::from_utf8_lossy(&csv_bytes).contains("example.com"));
        let zipped = asset_query::query(&state,id,"src.asset.export",
            &json!({"export_format":"zip","include_artifacts":false})).unwrap();
        let zip_bytes = base64::engine::general_purpose::STANDARD
            .decode(zipped[0]["data"].as_str().unwrap())
            .unwrap();
        assert!(zip_bytes.starts_with(b"PK"));
    }
    #[tokio::test]
    async fn report_green_requires_evidence_and_reads_by_record_id() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = super::super::create_task_inner(
            &state,
            &json!({"title":"Research","goal":"Report",
            "authorization":{"targets":["https://example.com/"]},
            "idempotency_key":"report-domain-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let claim = finding::apply(
            &state,
            id,
            "src.finding.claim.create",
            &json!({"status":"unverified","text":"Possible issue"}),
            None,
        )
        .unwrap();
        let fid = claim["claim_id"].as_str().unwrap();
        let base = json!({"finding_id":fid,"source_run_id":"workbench-operator",
            "document":{"title":"Draft","target":"https://example.com/","mechanism":"Observed","steps":["Step"],"confirmed_impact":"Unverified"},
            "review":{"readiness":"RED"},"template":"generic-zh-v1","evidence_refs":[]});
        let mut green = base.clone();
        green["review"]["readiness"] = json!("GREEN");
        assert_eq!(
            freeze_report(&state, id, &green, None).unwrap_err().status,
            StatusCode::CONFLICT
        );
        let frozen = freeze_report(&state, id, &base, None).unwrap();
        let rid = frozen["report_id"].as_str().unwrap();
        assert!(frozen.get("digest").is_none());
        assert!(frozen.get("rendered_sha512").is_none());
        assert!(report(&state, id, rid).is_ok());
        let row = rec(&state, id, "frozen_report", rid).unwrap();
        let mut tampered = row["payload"].clone();
        tampered["document"]["title"] = json!("Changed");
        save_rec(
            &state,
            id,
            "frozen_report",
            rid,
            tampered,
            row["revision"].as_u64(),
        )
        .unwrap();
        assert_eq!(
            report(&state, id, rid).unwrap()["document"]["title"],
            "Changed"
        );
    }
    #[tokio::test]
    async fn workbench_frozen_report_can_request_and_dispatch_retest() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = super::super::create_task_inner(
            &state,
            &json!({"title":"Research","goal":"Workbench retest",
            "authorization":{"targets":["https://example.com/"]},
            "idempotency_key":"workbench-retest-domain-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let claim = finding::apply(
            &state,
            id,
            "src.finding.claim.create",
            &json!({"status":"unverified","text":"Workbench finding"}),
            None,
        )
        .unwrap();
        let fid = claim["claim_id"].as_str().unwrap();
        let frozen = freeze_report(
            &state,
            id,
            &json!({"finding_id":fid,"source_run_id":"workbench-operator",
            "document":{"title":"Draft","target":"https://example.com/","mechanism":"Observed","steps":["Step"],"confirmed_impact":"Unverified"},
            "review":{"readiness":"RED"},"template":"generic-zh-v1","evidence_refs":[]}),
            None,
        )
        .unwrap();
        let rid = frozen["report_id"].as_str().unwrap();
        let p = policy(&state, id).unwrap();
        let auth = format!(
            "{}:v{}",
            p["policy_id"].as_str().unwrap(),
            p["version"].as_u64().unwrap_or(1)
        );
        let requested = retest::request(
            &state,
            id,
            &json!({
                "report_id": rid,
                "finding_id": fid,
                "repair_statement": "No code change; register workbench retest path",
                "expected_target_version": "example-v1",
                "authorization_ref": auth,
                "requester": "local-operator",
                "idempotency_key": "wb-retest-unit-1",
            }),
        )
        .unwrap();
        let retest_id = requested["retest_id"].as_str().unwrap();
        assert!(retest_id.starts_with("sretest-"));
        assert_eq!(requested["status"], "requested");
        assert_eq!(requested["source_run_id"], "workbench-operator");
        assert_eq!(
            rec(&state, id, "finding_retest", retest_id).unwrap()["payload"]["status"],
            "requested"
        );
        let dispatched = retest::dispatch(
            state.clone(),
            id,
            &json!({"retest_id": retest_id}),
        )
        .await
        .unwrap();
        assert_eq!(dispatched["status"], "dispatched");
        assert_eq!(dispatched["dispatch_execution_state"], "workbench_manual");
        assert_eq!(dispatched["recovery_required"], false);
        assert_eq!(dispatched["directive_receipt"]["source"], "workbench_manual");
        assert_eq!(
            rec(&state, id, "finding_retest", retest_id).unwrap()["payload"]["status"],
            "dispatched"
        );
    }
    #[tokio::test]
    async fn connector_secrets_are_rejected_and_import_artifacts_are_task_scoped() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = super::super::create_task_inner(
            &state,
            &json!({"title":"Research","goal":"Import",
            "idempotency_key":"import-domain-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        assert_eq!(
            save_connector_config(
                &state,
                id,
                &json!({"connector_id":"fofa","config":{"token":"secret"}})
            )
            .unwrap_err()
            .code,
            "validation_error"
        );
        save_connector_config(
            &state,
            id,
            &json!({"connector_id":"fofa","config":{"email_ref":"account:email","key_ref":"account:foo"}}),
        )
        .unwrap();
        let bytes=serde_json::to_vec(&json!({"items":[{"asset_type":"domain","identity":{"name":"example.com"},"attributes":{"title":"Imported"}}]})).unwrap();
        let batch=ingest(&state,id,&json!({"source_kind":"json","content_b64":base64::engine::general_purpose::STANDARD.encode(&bytes)})).unwrap();
        assert_eq!(batch["imported"], 1);
        assert!(
            batch["source_artifact"]["artifact_id"]
                .as_str()
                .is_some_and(|v| v.starts_with("sart_"))
        );
        assert!(batch["source_artifact"].get("sha512").is_none());
        assert!(verify_ref(&state, id, &batch["source_artifact"]).is_ok());
        assert_eq!(
            verify_ref(&state, "another-task", &batch["source_artifact"])
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        let reverted =
            revert_ingestion(&state, id, &json!({"batch_id":batch["batch_id"]})).unwrap();
        assert_eq!(reverted["status"], "reverted");
    }
    #[tokio::test]
    async fn request_draft_is_task_scoped_and_never_dispatches_without_gateway() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = super::super::create_task_inner(
            &state,
            &json!({"title":"Research","goal":"Inspect requests",
            "authorization":{"targets":["https://example.com/app"]},
            "idempotency_key":"exchange-domain-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let raw = b"GET /app HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let exchange = record_exchange(
            &state,
            id,
            &json!({"request_raw_b64":base64::engine::general_purpose::STANDARD.encode(raw),
            "observed_url":"https://example.com/app","source_kind":"import"}),
        )
        .unwrap();
        let eid = exchange["exchange_id"].as_str().unwrap();
        let read = read_exchange(&state, id, &json!({"exchange_id":eid})).unwrap();
        assert_eq!(read["exchange"]["observed_url"], "https://example.com/app");
        assert_eq!(
            read["exchange"]["request_raw_ref"]["artifact_id"],
            exchange["request_raw_ref"]["artifact_id"]
        );
        assert!(read["request_raw"].get("sha512").is_none());
        assert!(read_exchange(&state, "another-task", &json!({"exchange_id":eid})).is_err());
        let draft=save_exchange_draft(&state,id,&json!({"source_exchange_id":eid,"base_revision":1,
            "raw_request_b64":base64::engine::general_purpose::STANDARD.encode(b"GET /app/next HTTP/1.1\r\nHost: example.com\r\n\r\n"),
            "editor":"operator"})).unwrap();
        assert_eq!(draft["draft"]["target_url"], "https://example.com/app/next");
        let did = draft["draft"]["draft_id"].as_str().unwrap();
        assert!(exchange_queries::read_draft(&state, id, &json!({"draft_id":did})).is_ok());
        let prepared = prepare_exchange_dispatch(&state, id, did, 1, 1).unwrap();
        assert_eq!(prepared.url, "https://example.com/app/next");
        assert!(prepare_exchange_dispatch(&state, id, did, 2, 1).is_err());
        super::super::update_policy(
            &state,
            id,
            &json!({"base_version":1,"authorized_targets":[]}),
        )
        .unwrap();
        assert!(prepare_exchange_dispatch(&state, id, did, 1, 1).is_err());
        assert_eq!(
            command(
                &state,
                id,
                "src.exchange.draft.execute",
                &json!({"draft_id":did})
            )
            .unwrap_err()
            .status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert!(
            parse_raw_request(
                b"GET / HTTP/1.1\r\nHost: example.com\r\nHost: evil.com\r\n\r\n",
                "https"
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn monitor_file_connector_records_missing_adapter_on_cycle() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = super::super::create_task_inner(
            &state,
            &json!({"title":"Research","goal":"Monitor",
            "idempotency_key":"monitor-domain-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        save_connector_config(&state, id, &json!({"connector_id":"cli","config":{}})).unwrap();
        let definition = json!({"name":"Daily","continuous":true,"chain_type":"asset",
            "source":{"connector_id":"cli","config_version":1,"capability_manifest_ref":"source-file.v1"},
            "authorization_ref":format!("src-policy-{id}:v1"),"schedule":{"interval_seconds":3600,"timezone":"UTC"},
            "budget":{"total":10,"per_cycle":1}});
        let monitor = create_monitor(&state, id, &json!({"payload":definition})).unwrap();
        assert_eq!(monitor["status"], "active");
        let mid = monitor["monitor_id"].as_str().unwrap();
        let first = monitor_runtime::run_once(state.clone(), id, &json!({"monitor_id":mid}))
            .await
            .unwrap();
        assert_eq!(first["status"], "failed");
        assert_eq!(first["task_id"], id);
        assert_eq!(
            monitor_runtime::run_cycle(
                state.clone(),
                id,
                &json!({"monitor_id":mid,"cycle_id":first["cycle_id"]})
            )
            .await
            .unwrap()["cycle_id"],
            first["cycle_id"]
        );
        assert_eq!(
            monitor_runtime::run_cycle(
                state.clone(),
                id,
                &json!({"monitor_id":mid,"cycle_id":"missing"})
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            monitor_get(&state, id, &json!({"monitor_id":mid})).unwrap()["cycles"][0]["status"],
            "failed"
        );
    }

    #[tokio::test]
    async fn skill_material_retains_content_digest_after_versioned_save() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = super::super::create_task_inner(
            &state,
            &json!({"title":"Research","goal":"Skill digest",
            "idempotency_key":"i54-skill-digest"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let content = b"# i54 skill body\n";
        let sha = format!("{:x}", Sha256::digest(content));
        let saved = save_research_skill(
            &state,
            id,
            &json!({
                "material_id": "i54-skill",
                "source_url": "https://example.invalid/skill/i54",
                "source_commit": "abc123",
                "distillation_version": "v1",
                "content_b64": base64::engine::general_purpose::STANDARD.encode(content),
                "required_capabilities": {"http": "1"},
                "available_capabilities": {"http": "1"},
                "selected_fragments": [],
            }),
            None,
            "local-operator",
        )
        .unwrap();
        let record_id = saved["record_id"].as_str().unwrap();
        assert_eq!(saved["payload"]["digest"], sha);
        assert_eq!(saved["payload"]["content_digest"], sha);
        let stored = rec(&state, id, "skill_material", record_id).unwrap();
        assert_eq!(stored["payload"]["digest"], sha);
        assert_eq!(stored["payload"]["content_digest"], sha);
        let read = super::research_ingest::read_skill(
            &state,
            id,
            &json!({"record_id": record_id}),
        )
        .unwrap();
        assert_eq!(read["record"]["digest"], sha);
        assert_eq!(read["record"]["content_digest"], sha);
        assert_eq!(read["record"]["digest"], read["record"]["content_digest"]);

        let plan = save_research_plan(
            &state,
            id,
            &json!({
                "plan_id": "i54-plan",
                "payload": {
                    "question": "q",
                    "rationale": "r",
                    "gaps": ["g"],
                    "actions": ["a"],
                    "digest": "caller-supplied",
                }
            }),
            None,
            "local-operator",
        )
        .unwrap();
        assert!(plan["payload"].get("digest").is_none());
    }
}

fn cidr_prefix(raw:&str,ipv4:bool)->Api<u8>{
    if let Ok(prefix)=raw.parse(){return Ok(prefix);}
    if ipv4{if let Ok(mask)=raw.parse::<Ipv4Addr>(){let mut mask=u32::from(mask);if mask!=0&&mask.leading_ones()==0{mask=!mask;}let prefix=mask.leading_ones();let expected=if prefix==0{0}else{u32::MAX<<(32-prefix)};if mask==expected{return Ok(prefix as u8);}}}
    Err(Error::invalid("invalid CIDR prefix or mask"))
}
fn raw_endpoint_path(raw:&str)->String{
    let tail=raw.split_once("://").map(|(_,tail)|tail).unwrap_or(raw);let boundary=tail.find(['/', '?', '#']).unwrap_or(tail.len());let tail=&tail[boundary..];let path=tail.split(['?', '#']).next().unwrap_or("");if path.is_empty(){"/".into()}else{path.into()}
}

fn structured_draft_request(bytes:&[u8],exchange:&Value)->Api<Vec<u8>>{
    let document:Value=serde_json::from_slice(bytes).map_err(|_|Error::new(StatusCode::UNPROCESSABLE_ENTITY,"capture_unavailable","imported request has no replayable HTTP representation"))?;
    let method=document["method"].as_str().or_else(||exchange["method"].as_str()).unwrap_or("GET").to_uppercase();let target=document["url"].as_str().or_else(||exchange["observed_url"].as_str()).ok_or_else(||Error::invalid("imported request target missing"))?;let url=parse_url(target)?;
    let path=format!("{}{}",raw_endpoint_path(target),url.query().map(|query|format!("?{query}")).unwrap_or_default());let mut raw=format!("{method} {path} HTTP/1.1\r\n").into_bytes();let mut host=false;
    for row in document["headers"].as_array().into_iter().flatten(){let name=row["name"].as_str().or_else(||row.get(0).and_then(Value::as_str)).unwrap_or("");let value=row["value"].as_str().or_else(||row.get(1).and_then(Value::as_str)).unwrap_or("");if name.is_empty(){continue;}if name.contains(['\r','\n'])||value.contains(['\r','\n']){return Err(Error::invalid("structured request contains a header line break"));}host|=name.eq_ignore_ascii_case("host");for ch in format!("{name}: {value}\r\n").chars(){let byte=u8::try_from(ch as u32).map_err(|_|Error::invalid("structured header is not ISO-8859-1"))?;raw.push(byte);}}
    if !host{let authority=target.split_once("://").map(|(_,tail)|tail.split(['/','?','#']).next().unwrap_or("")).unwrap_or("");raw.extend_from_slice(format!("Host: {authority}\r\n").as_bytes());}raw.extend_from_slice(b"\r\n");
    let post=&document["postData"];let text=post["text"].as_str().unwrap_or("");if post["encoding"]=="base64"{raw.extend(base64::engine::general_purpose::STANDARD.decode(text).map_err(|_|Error::invalid("structured request body has invalid base64"))?);}else{raw.extend_from_slice(text.as_bytes());}parse_raw_request(&raw,url.scheme())?;Ok(raw)
}

pub(super) fn gateway_replacement_ready(s:&AppState,id:&str)->Api<()>{
    for row in records(s,id,"run_binding")?{let run_id=row["payload"]["run_id"].as_str().unwrap_or("");let run=s.store.get("runs",run_id).map_err(db)?;if s.runs.active_worker_count(run_id)>0||s.runs.handle(s,run_id).coordinator_abort.lock().unwrap().as_ref().is_some_and(|task|!task.is_finished())||run.is_some_and(|run|matches!(run["status"].as_str(),Some("running"|"starting"|"stopping"|"paused"|"quota_paused"|"frozen"))){return Err(Error::new(StatusCode::CONFLICT,"authorization_stale",format!("stop dependent Run {run_id} before replacing this Task gateway")));}}
    Ok(())
}

pub(super) async fn notify_event(s: &AppState, task_id: &str, event: &str, event_id: &str) -> Api<()> {
    notify_gateway::notify_event(s, task_id, event, event_id).await
}
