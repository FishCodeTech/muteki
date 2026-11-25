//! SRC domain commands with the source accepted-event, outbox and result sequence.
use super::*;
use std::collections::HashSet;
mod outbox;

const PLATFORM_COMMANDS: &str = "platform_commands";
const KEYS: &str = "src_approval_command_keys";
const EVENTS: &str = "src_task_events";
const HEADS: &str = "src_task_event_heads";
const RESULTS: &str = "src_approval_command_results";
const SLICE_RESULTS: &str = "src_workbench_slice_command_results";
const FINDING_RESULTS: &str = "src_finding_command_results";
const ASSET_RESULTS: &str = "src_asset_observe_command_results";
const RESEARCH_RESULTS: &str = "src_research_plan_command_results";
const RESEARCH_AUTH_RESULTS: &str = "src_research_authorization_command_results";
const RESEARCH_SKILL_RESULTS: &str = "src_research_skill_command_results";
const RESEARCH_OPENAPI_RESULTS: &str = "src_research_openapi_command_results";
const RESEARCH_JAVASCRIPT_RESULTS: &str = "src_research_javascript_command_results";
const RESEARCH_OBSERVATION_RESULTS: &str = "src_research_observation_command_results";
const RESEARCH_BUSINESS_RESULTS: &str = "src_research_business_command_results";
const RESEARCH_WORKFLOW_RESULTS: &str = "src_research_workflow_command_results";
const COVERAGE_RESULTS: &str = "src_coverage_command_results";
const REPORT_RESULTS: &str = "src_report_command_results";
const EXCHANGE_RESULTS: &str = "src_exchange_command_results";
const CONNECTOR_RESULTS: &str = "src_connector_import_command_results";
const CONNECTOR_CONFIG_RESULTS: &str = "src_connector_config_command_results";
const CONNECTOR_GRANT_RESULTS: &str = "src_connector_grant_command_results";
const ASSET_JUDGEMENT_RESULTS: &str = "src_asset_judgement_command_results";
const CONTROL_RESULTS: &str = "src_control_command_results";
const TASK_LIFECYCLE_RESULTS: &str = "src_task_lifecycle_command_results";
const RUN_LIFECYCLE_RESULTS: &str = "src_run_lifecycle_command_results";
const RUN_DESTROY_RESULTS: &str = "src_run_destroy_command_results";
const TASK_DESTROY_RESULTS: &str = "src_task_destroy_command_results";
const ARTIFACT_DESTROY_RESULTS: &str = "src_artifact_destroy_command_results";
const ASSET_PUBLICATION_RESULTS: &str = "src_asset_publication_command_results";
const ASSET_QUERY_RESULTS: &str = "src_asset_query_command_results";
const WORKBENCH_RECORD_RESULTS: &str = "src_workbench_record_command_results";
const BROWSER_RESULTS: &str = "src_browser_command_results";
const STREAM_RESULTS: &str = "src_stream_command_results";
const MONITOR_RESULTS: &str = "src_monitor_command_results";
const INGESTION_RESULTS: &str = "src_ingestion_command_results";
const KNOWLEDGE_RESULTS: &str = "src_knowledge_command_results";
const OAST_RESULTS: &str = "src_oast_command_results";
const IDENTITY_RESULTS: &str = "src_identity_command_results";
const OBJECT_RESULTS: &str = "src_object_command_results";
const INSPECTION_RESULTS: &str = "src_inspection_command_results";
const PLATFORM_STATUS_RESULTS: &str = "src_platform_status_command_results";
const RETEST_RESULTS: &str = "src_retest_command_results";
const RUN_COMMAND_RESULTS: &str = "src_run_command_results";
const CONNECTOR_ONLINE_RESULTS: &str = "src_connector_online_command_results";
const CREDENTIAL_RESULTS: &str = "src_credential_command_results";
const COORDINATION_RESULTS: &str = "src_coordination_command_results";
const DOMAIN_APPLIED: &str = "src_domain_command_applied";

fn result_collection(kind: &str) -> &'static str {
    if matches!(
        kind,
        "src.task.archive" | "src.task.restore" | "src.task.pause" | "src.task.resume"
    ) {
        TASK_LIFECYCLE_RESULTS
    } else if matches!(kind, "src.run.archive" | "src.run.restore") {
        RUN_LIFECYCLE_RESULTS
    } else if kind == "src.run.destroy" {
        RUN_DESTROY_RESULTS
    } else if kind == "src.task.destroy" {
        TASK_DESTROY_RESULTS
    } else if kind == "src.artifact.destroy" {
        ARTIFACT_DESTROY_RESULTS
    } else if asset_publication_command(kind) {
        ASSET_PUBLICATION_RESULTS
    } else if asset_query_command(kind) {
        ASSET_QUERY_RESULTS
    } else if workbench_record_command(kind) {
        WORKBENCH_RECORD_RESULTS
    } else if kind == "src.approval.decide" {
        RESULTS
    } else if kind == "src.workbench.slice.save" {
        SLICE_RESULTS
    } else if matches!(
        kind,
        "src.policy.update" | "src.grant.revoke" | "src.task.authorization.renew"
    ) {
        CONTROL_RESULTS
    } else if asset_command(kind) {
        ASSET_RESULTS
    } else if kind == "src.research.plan.save" {
        RESEARCH_RESULTS
    } else if kind == "src.research.authorization.save" {
        RESEARCH_AUTH_RESULTS
    } else if kind == "src.research.skill.register" {
        RESEARCH_SKILL_RESULTS
    } else if kind == "src.research.openapi.ingest" {
        RESEARCH_OPENAPI_RESULTS
    } else if kind == "src.research.javascript.ingest"
        || kind == "src.research.javascript.collect"
    {
        RESEARCH_JAVASCRIPT_RESULTS
    } else if research_observation_command(kind) {
        RESEARCH_OBSERVATION_RESULTS
    } else if research_business_command(kind) {
        RESEARCH_BUSINESS_RESULTS
    } else if research_workflow_command(kind) {
        RESEARCH_WORKFLOW_RESULTS
    } else if coverage_command(kind) {
        COVERAGE_RESULTS
    } else if report_command(kind) {
        REPORT_RESULTS
    } else if exchange_command(kind) {
        EXCHANGE_RESULTS
    } else if kind == "src.connector.import" {
        CONNECTOR_RESULTS
    } else if connector_config_command(kind) {
        CONNECTOR_CONFIG_RESULTS
    } else if connector_grant_command(kind) {
        CONNECTOR_GRANT_RESULTS
    } else if kind == "src.connector.verify" {
        CONNECTOR_ONLINE_RESULTS
    } else if kind == "src.asset.judgement.save" {
        ASSET_JUDGEMENT_RESULTS
    } else if browser_command(kind) {
        BROWSER_RESULTS
    } else if stream_command(kind) {
        STREAM_RESULTS
    } else if monitor_command(kind) {
        MONITOR_RESULTS
    } else if ingestion_command(kind) {
        INGESTION_RESULTS
    } else if knowledge_command(kind) {
        KNOWLEDGE_RESULTS
    } else if oast_command(kind) {
        OAST_RESULTS
    } else if identity_command(kind) {
        IDENTITY_RESULTS
    } else if object_command(kind) {
        OBJECT_RESULTS
    } else if inspection_command(kind) {
        INSPECTION_RESULTS
    } else if platform_status_command(kind) {
        PLATFORM_STATUS_RESULTS
    } else if retest_command(kind) {
        RETEST_RESULTS
    } else if run_command(kind) {
        RUN_COMMAND_RESULTS
    } else if connector_online_command(kind) {
        CONNECTOR_ONLINE_RESULTS
    } else if credential_command(kind) {
        CREDENTIAL_RESULTS
    } else if coordination_command(kind) {
        COORDINATION_RESULTS
    } else {
        FINDING_RESULTS
    }
}

fn browser_async_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.browser.ensure"
            | "src.browser.context.ensure"
            | "src.browser.tab.open"
            | "src.browser.screenshot.capture"
            | "src.browser.task.close"
    )
}
fn browser_sync_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.browser.tab.release"
            | "src.browser.image.review"
            | "src.browser.execution.record"
            | "src.browser.run.finish"
    )
}
fn browser_command(kind: &str) -> bool {
    browser_async_command(kind) || browser_sync_command(kind)
}
fn stream_async_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.stream.websocket.connect"
            | "src.stream.websocket.send"
            | "src.stream.websocket.receive"
            | "src.stream.websocket.close"
            | "src.stream.sse.connect"
            | "src.stream.sse.read"
            | "src.stream.sse.close"
    )
}
fn stream_command(kind: &str) -> bool {
    stream_async_command(kind) || kind == "src.stream.websocket.acknowledge"
}
fn monitor_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.monitor.create"
            | "src.monitor.revise"
            | "src.monitor.state"
            | "src.monitor.wake"
            | "src.monitor.run_once"
            | "src.monitor.cycle.run"
            | "src.monitor.candidate.handle"
            | "src.monitor.external.reconcile"
            | "src.monitor.notifications.deliver"
    )
}
fn ingestion_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.ingestion.ingest"
            | "src.ingestion.retry"
            | "src.ingestion.reconcile"
            | "src.ingestion.external_event"
            | "src.ingestion.batch.revert"
    )
}
fn knowledge_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.knowledge.dataset.create"
            | "src.knowledge.publish"
            | "src.knowledge.revoke"
            | "src.knowledge.invalidate"
    )
}
fn oast_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.oast.allocate"
            | "src.oast.poll"
            | "src.oast.expire"
            | "src.oast.health"
            | "src.oast.local.start"
            | "src.oast.token.mint"
            | "src.oast.callback.ingest"
    )
}
fn identity_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.identity.ensure"
            | "src.identity.verify"
            | "src.identity.recover"
            | "src.identity.login_recipe.save"
            | "src.identity.login_recipe.replay"
            | "src.identity.session.import"
            | "src.identity.session.probe"
    )
}
fn proxy_bridge_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.proxy_bridge.start"
            | "src.proxy_bridge.stop"
            | "src.proxy_bridge.bookmark"
            | "src.proxypool.save"
            | "src.proxypool.reset_circuit"
    )
}
fn intel_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.intel.advisory.import"
            | "src.intel.match_run"
            | "src.workbench.attachment.autobind"
            | "src.notify.channel.save"
            | "src.notify.test"
            | "src.notify.flush_carryover"
            | "src.archive.export"
            | "src.archive.import"
            | "src.diagnostics.mcp_reset"
    )
}
fn mutator_command(kind: &str) -> bool {
    matches!(kind, "src.mutator.save" | "src.mutator.test")
}
fn object_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.object.lease.acquire"
            | "src.object.lease.release"
            | "src.object.register"
            | "src.object.experiment.begin"
            | "src.object.experiment.after"
            | "src.object.experiment.restore"
    )
}
fn inspection_command(kind: &str) -> bool {
    matches!(kind, "src.inspection.decide" | "src.inspection.grant.revoke")
}
fn platform_status_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.platform.status.record" | "src.feedback.platform_status.record"
    )
}
fn retest_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.retest.request" | "src.retest.dispatch" | "src.retest.complete"
    )
}
fn run_command(kind: &str) -> bool {
    matches!(kind, "src.run.ensure" | "src.run.start" | "src.run.intervene")
}
fn connector_online_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.connector.query" | "src.connector.repository" | "src.connector.external_event"
    )
}
fn credential_command(kind: &str) -> bool {
    matches!(kind, "src.credential.put" | "src.credential.revoke")
}
fn coordination_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.coordination.policy.save"
            | "src.coordination.decide"
            | "src.coordination.wake"
            | "src.coordination.notice.ack"
    )
}

fn coverage_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.coverage.item.save"
            | "src.coverage.item.claim"
            | "src.coverage.attempt.record"
            | "src.coverage.blocker.record"
            | "src.coverage.seed"
    )
}
fn report_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.report.freeze"
            | "src.report.reproduction.record"
            | "src.report.reproduction.auto"
            | "src.report.render"
            | "src.submission.export"
    )
}
fn exchange_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.exchange.record"
            | "src.exchange.draft.save"
            | "src.exchange.draft.execute"
            | "src.exchange.differential_probe"
            | "src.exchange.race_probe"
    )
}
fn asset_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.asset.observe"
            | "src.asset.correct"
            | "src.asset.relate"
            | "src.asset.derive"
            | "src.asset.lifecycle"
            | "src.asset.destroy"
            | "src.asset.merge"
            | "src.asset.split"
            | "src.asset.merge.revert"
    )
}
fn asset_publication_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.asset.publish" | "src.asset.reference.refresh" | "src.asset.publication.revoke"
    )
}
pub(super) fn asset_query_command(kind: &str) -> bool {
    matches!(kind, "src.query.save" | "src.query.delete" | "src.vocabulary.save"
        | "src.tag.save" | "src.tag.assign" | "src.note.save")
}
pub(super) fn workbench_record_command(kind: &str) -> bool {
    matches!(kind, "src.workbench.todo.save" | "src.workbench.question.status"
        | "src.workbench.history.record" | "src.workbench.history.revert"
        | "src.workbench.report.draft.save" | "src.workbench.report.draft.discard"
        | "src.workbench.suggestion.propose" | "src.workbench.suggestion.resolve"
        | "src.feedback.record" | "src.workbench.asset.importance"
        | "src.workbench.asset.link" | "src.workbench.object.link"
        | "src.workbench.asset.tasklink" | "src.workbench.duplicate.verdict"
        | "src.workbench.finding.merge" | "src.workbench.finding.unmerge"
        | "src.workbench.finding.meta.save" | "src.workbench.finding.stale"
        | "src.workbench.finding.stale.clear" | "src.asset.version.pin"
        | "src.project.memory.save")
}
pub(super) fn allowed_while_paused(kind: &str) -> bool {
    matches!(
        kind,
        "src.task.resume"
            | "src.task.archive"
            | "src.task.destroy"
            | "src.task.authorization.renew"
            | "src.coordination.policy.save"
            | "src.coordination.notice.ack"
            | "src.grant.revoke"
            | "src.approval.decide"
            | "src.workbench.slice.save"
            | "src.asset.merge"
            | "src.asset.merge.revert"
            | "src.asset.judgement.save"
    ) || workbench_record_command(kind)
        || asset_query_command(kind)
}
fn connector_config_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.connector.config.save" | "src.connector.config.revoke"
    )
}
fn connector_grant_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.connector.grant.save" | "src.connector.grant.revoke"
    )
}
fn is_external_side_effect_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.exchange.draft.execute"
            | "src.exchange.differential_probe"
            | "src.exchange.race_probe"
            | "src.connector.query"
            | "src.connector.repository"
            | "src.connector.verify"
            | "src.run.start"
            | "src.retest.dispatch"
            | "src.report.reproduction.auto"
    ) || browser_async_command(kind)
        || stream_async_command(kind)
}
fn research_observation_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.research.runtime.register"
            | "src.research.dataflow.register"
            | "src.research.page_action.register"
    )
}
fn research_business_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.research.business.save" | "src.research.business.derive"
    )
}
fn research_workflow_command(kind: &str) -> bool {
    matches!(
        kind,
        "src.research.permission_question.add"
            | "src.research.experiment.record"
            | "src.research.capability_chain.record"
            | "src.research.precondition_trial.record"
    )
}
fn research_actor_claims(_body: &Value, _actor_id: &str, _actor_kind: &str) -> Api<()> {
    Ok(())
}

fn finding_fields(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "src.finding.claim.create" => Some(("finding_claim", "claim_id")),
        "src.finding.review.record" | "src.review.record" => Some(("finding_review", "review_id")),
        "src.finding.deduplicate" => Some(("finding_dedup", "dedup_id")),
        "src.finding.invalidate" => Some(("finding_stale", "stale_id")),
        _ => None,
    }
}

pub(super) fn supported(kind: &str) -> bool {
    matches!(
        kind,
        "src.task.archive"
            | "src.task.restore"
            | "src.task.pause"
            | "src.task.resume"
            | "src.task.destroy"
            | "src.task.authorization.renew"
            | "src.run.archive"
            | "src.run.restore"
            | "src.run.destroy"
            | "src.artifact.destroy"
            | "src.approval.decide"
            | "src.policy.update"
            | "src.grant.revoke"
            | "src.workbench.slice.save"
    ) || asset_command(kind)
        || asset_publication_command(kind)
        || asset_query_command(kind)
        || workbench_record_command(kind)
        || research_observation_command(kind)
        || research_business_command(kind)
        || research_workflow_command(kind)
        || matches!(
            kind,
            "src.research.plan.save"
                | "src.research.authorization.save"
                | "src.research.skill.register"
                | "src.research.openapi.ingest"
                | "src.research.javascript.ingest"
                | "src.research.javascript.sourcemap_unpack"
                | "src.research.graphql_grpc.ingest"
                | "src.research.javascript.collect"
        )
        || kind == "src.connector.import"
        || connector_config_command(kind)
        || connector_grant_command(kind)
        || kind == "src.connector.verify"
        || kind == "src.asset.judgement.save"
        || connector_online_command(kind)
        || coverage_command(kind)
        || exchange_command(kind)
        || report_command(kind)
        || finding_fields(kind).is_some()
        || browser_command(kind)
        || stream_command(kind)
        || monitor_command(kind)
        || ingestion_command(kind)
        || knowledge_command(kind)
        || oast_command(kind)
        || identity_command(kind)
        || proxy_bridge_command(kind)
        || mutator_command(kind)
        || intel_command(kind)
        || object_command(kind)
        || inspection_command(kind)
        || platform_status_command(kind)
        || retest_command(kind)
        || run_command(kind)
        || credential_command(kind)
        || coordination_command(kind)
}

pub(super) fn agent_allowed(kind: &str) -> bool {
    !matches!(
        kind,
        "src.task.destroy"
            | "src.run.destroy"
            | "src.artifact.destroy"
            | "src.asset.destroy"
            | "src.task.archive"
            | "src.task.restore"
            | "src.run.archive"
            | "src.run.restore"
            | "src.archive.import"
            | "src.workbench.slice.save"
            | "src.approval.decide"
            | "src.grant.revoke"
            | "src.connector.grant.revoke"
    )
}

fn effect_was_applied(
    s: &AppState,
    id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    idempotency_key: Option<&str>,
) -> Api<bool> {
    if kind == "src.exchange.draft.execute" {
        let key = body["idempotency_key"]
            .as_str()
            .filter(|value| !value.is_empty())
            .or(idempotency_key);
        return key
            .map(|key| {
                s.store
                    .get(COMMANDS, &format!("{id}:src.exchange.draft.execute:{key}"))
                    .map_err(db)
            })
            .transpose()
            .map(|row| row.flatten().is_some());
    }
    let marker = if kind == "src.approval.decide" {
        format!("{id}:{}", require(body, "approval_id")?)
    } else if kind == "src.workbench.slice.save" {
        format!(
            "{id}:{}:{}",
            require(body, "slice")?,
            body["base_revision"].as_u64().unwrap_or(0)
        )
    } else {
        command_id.to_owned()
    };
    let collection = if matches!(
        kind,
        "src.task.archive" | "src.task.restore" | "src.task.pause" | "src.task.resume"
    ) {
        TASK_LIFECYCLE_APPLIED
    } else if matches!(kind, "src.run.archive" | "src.run.restore") {
        RUN_LIFECYCLE_APPLIED
    } else if kind == "src.run.destroy" {
        RUN_DESTROY_APPLIED
    } else if kind == "src.task.destroy" {
        TASK_DESTROY_APPLIED
    } else if kind == "src.artifact.destroy" {
        ARTIFACT_DESTROY_APPLIED
    } else if asset_publication_command(kind) {
        domains::ASSET_PUBLICATION_APPLIED
    } else if asset_query_command(kind) {
        domains::ASSET_QUERY_APPLIED
    } else if workbench_record_command(kind) {
        domains::WORKBENCH_RECORDS_APPLIED
    } else if kind == "src.approval.decide" {
        APPROVAL_APPLIED
    } else if kind == "src.workbench.slice.save" {
        SLICE_APPLIED
    } else if matches!(kind, "src.policy.update" | "src.task.authorization.renew") {
        control_command::POLICY_APPLIED
    } else if kind == "src.grant.revoke" {
        control_command::GRANT_APPLIED
    } else if matches!(kind, "src.asset.observe" | "src.asset.correct") {
        ASSET_OBSERVE_APPLIED
    } else if kind == "src.asset.relate" {
        ASSET_RELATE_APPLIED
    } else if kind == "src.asset.derive" {
        ASSET_DERIVE_APPLIED
    } else if matches!(kind, "src.asset.lifecycle" | "src.asset.destroy") {
        ASSET_LIFECYCLE_APPLIED
    } else if matches!(
        kind,
        "src.asset.merge" | "src.asset.split" | "src.asset.merge.revert"
    ) {
        ASSET_RESTRUCTURE_APPLIED
    } else if kind == "src.research.plan.save" {
        RESEARCH_PLAN_APPLIED
    } else if kind == "src.research.authorization.save" {
        RESEARCH_AUTH_APPLIED
    } else if kind == "src.research.skill.register" {
        RESEARCH_SKILL_APPLIED
    } else if kind == "src.research.openapi.ingest" {
        RESEARCH_OPENAPI_APPLIED
    } else if kind == "src.research.javascript.ingest" {
        RESEARCH_JAVASCRIPT_APPLIED
    } else if research_observation_command(kind) {
        RESEARCH_OBSERVATION_APPLIED
    } else if research_business_command(kind) {
        RESEARCH_BUSINESS_APPLIED
    } else if research_workflow_command(kind) {
        RESEARCH_WORKFLOW_APPLIED
    } else if coverage_command(kind) {
        COVERAGE_APPLIED
    } else if report_command(kind) {
        REPORT_APPLIED
    } else if exchange_command(kind) {
        domains::EXCHANGE_APPLIED
    } else if kind == "src.connector.import" {
        domains::CONNECTOR_IMPORT_APPLIED
    } else if connector_config_command(kind) {
        domains::CONNECTOR_CONFIG_APPLIED
    } else if connector_grant_command(kind) {
        domains::CONNECTOR_GRANT_APPLIED
    } else if kind == "src.asset.judgement.save" {
        domains::ASSET_JUDGEMENT_APPLIED
    } else if finding_fields(kind).is_some() {
        FINDING_APPLIED
    } else {
        DOMAIN_APPLIED
    };
    if s.store
        .get(collection, &marker)
        .map_err(db)?
        .is_some_and(|row| row["command_id"] == command_id)
    {
        return Ok(true);
    }
    Ok(s.store
        .get(DOMAIN_APPLIED, command_id)
        .map_err(db)?
        .is_some_and(|row| row["command_id"] == command_id))
}

fn effect_dispatched_in_prior_epoch(s: &AppState, command_id: &str) -> Api<bool> {
    let Some(key) = s.store.get("src_domain_outbox_keys", command_id).map_err(db)? else {
        return Ok(false);
    };
    let Some(effect_id) = key["effect_id"].as_str() else {
        return Ok(false);
    };
    let (_, effect_collection) = outbox::collections();
    let Some(effect) = s.store.get(effect_collection, effect_id).map_err(db)? else {
        return Ok(false);
    };
    Ok(effect["state"] == "dispatched"
        && effect["control_epoch"]
            .as_str()
            .is_some_and(|epoch| !epoch.is_empty() && epoch != s.chat.epoch()))
}

fn mark_effect_dispatched(s: &AppState, effect_row: &mut Value) -> Api<()> {
    let (_, effect_collection) = outbox::collections();
    let effect_id = require(effect_row, "effect_id")?.to_owned();
    let epoch = s.chat.epoch().to_owned();
    loop {
        let Some(current) = s.store.get(effect_collection, &effect_id).map_err(db)? else {
            return Ok(());
        };
        if current["state"] != "accepted" {
            *effect_row = current;
            return Ok(());
        }
        let mut next = current.clone();
        next["state"] = json!("dispatched");
        next["control_epoch"] = json!(epoch);
        next["attempts"] = json!(current["attempts"].as_u64().unwrap_or(0) + 1);
        next["updated_at"] = json!(now());
        if s.store
            .put_many_if(
                &[(effect_collection, &effect_id, Some(&current))],
                &[(effect_collection, &effect_id, &next)],
            )
            .map_err(db)?
        {
            *effect_row = next;
            return Ok(());
        }
    }
}

/// Source Reconciler lists unfinished receipts and outbox rows. Settle only
/// effects with a durable result or an attribution marker from the domain commit.
pub(super) async fn reconcile_once(s: Arc<AppState>, full_scan: bool) -> Api<usize> {
    let mut settled = 0;
    let pending:HashSet<String>=s.store.list(outbox::collections().0).map_err(db)?
        .into_iter().filter(|row|row["status"]=="pending")
        .filter_map(|row|row["command_id"].as_str().map(str::to_owned)).collect();
    let rows=if full_scan{s.store.list(PLATFORM_COMMANDS).map_err(db)?}else{
        let mut rows=Vec::with_capacity(pending.len());
        for command_id in &pending{
            if let Some(row)=s.store.get(PLATFORM_COMMANDS,command_id).map_err(db)?{rows.push(row);}
        }
        rows
    };
    for row in rows {
        let kind = row["request"]["command_type"].as_str().unwrap_or("");
        if !supported(kind) {
            continue;
        }
        let Some(command_id) = row["request"]["command_id"].as_str() else {
            continue;
        };
        let Some(id) = row["request"]["aggregate_id"].as_str() else {
            continue;
        };
        if row["response"]["receipt"]["state"] != "accepted"
            && !pending.contains(command_id) {
            continue;
        }
        let has_result = s
            .store
            .get(result_collection(kind), command_id)
            .map_err(db)?
            .is_some();
        let was_applied = if has_result {
            false
        } else {
            match effect_was_applied(
                &s,
                id,
                kind,
                &row["request"]["payload"],
                command_id,
                row["request"]["idempotency_key"].as_str(),
            ) {
                Ok(applied) => applied,
                Err(error) if error.status == StatusCode::BAD_REQUEST => false,
                Err(error) => return Err(error),
            }
        };
        let prior_epoch_dispatched = !has_result
            && !was_applied
            && is_external_side_effect_command(kind)
            && effect_dispatched_in_prior_epoch(&s, command_id)?;
        if !has_result
            && !was_applied
            && !prior_epoch_dispatched
            && !matches!(
                kind,
                "src.task.archive"
                    | "src.task.restore"
                    | "src.task.pause"
                    | "src.task.resume"
                    | "src.task.destroy"
                    | "src.run.archive"
                    | "src.run.restore"
                    | "src.run.destroy"
                    | "src.artifact.destroy"
            )
            && !asset_publication_command(kind)
            && !asset_query_command(kind)
            && !workbench_record_command(kind)
            && !connector_config_command(kind)
            && !connector_grant_command(kind)
            && kind != "src.asset.judgement.save"
        {
            continue;
        }
        let scope = row["scope"].as_str().unwrap_or("operator:local-user");
        match dispatch(s.clone(), id, &row["request"], scope).await {
            Ok(_) => settled += 1,
            Err(error) => tracing::warn!(command_id,code=error.code,%error.message,
                "SRC known domain effect awaits reconciliation"),
        }
    }
    Ok(settled)
}

fn error_value(error: &Error) -> Value {
    let category = match error.status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        StatusCode::FORBIDDEN => "policy",
        _ => "internal",
    };
    json!({"schema_version":1,"code":error.code,"message":error.message,
        "category":category,"retryable":false,"detail":error.detail})
}

fn accepted_receipt(command_id: &str, id: &str, cursor: &str) -> Value {
    json!({"schema_version":1,"command_id":command_id,
        "receipt_id":format!("rcpt-{}",uuid::Uuid::new_v4().simple()),
        "state":"accepted","run_id":null,"aggregate":{"type":"task","id":id},
        "event_cursor":cursor,"deduplicated":false,"submitted_at":now(),
        "completed_at":null,"next":["receipt","snapshot","events","wait"],
        "output":{},"effect_ids":[],"error":null})
}

fn applied_approval(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let approval_id = require(body, "approval_id")?;
    let row = rec(s, id, "execution_approval", approval_id)?;
    let base = body["base_revision"].as_u64().unwrap_or(0);
    if row["payload"]["status"] != "pending" {
        let marker = s
            .store
            .get(APPROVAL_APPLIED, &format!("{id}:{approval_id}"))
            .map_err(db)?;
        if marker
            .as_ref()
            .is_some_and(|marker| marker["command_id"] == command_id)
            && row["revision"].as_u64() == Some(base.saturating_add(1))
            && row["payload"]["decision"] == body["decision"]
        {
            return Ok(row["payload"].clone());
        }
        return Err(Error::conflict("approval was decided by another command"));
    }
    match decide_approval_marked(s, id, body, Some(command_id), Some(actor_id)) {
        Ok(output) => Ok(output),
        Err(error) if error.status == StatusCode::CONFLICT => {
            let marker = s
                .store
                .get(APPROVAL_APPLIED, &format!("{id}:{approval_id}"))
                .map_err(db)?;
            let current = rec(s, id, "execution_approval", approval_id)?;
            if marker
                .as_ref()
                .is_some_and(|marker| marker["command_id"] == command_id)
                && current["revision"].as_u64() == Some(base.saturating_add(1))
                && current["payload"]["decision"] == body["decision"]
            {
                Ok(current["payload"].clone())
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

async fn finalize(
    s: &Arc<AppState>,
    command_id: &str,
    result: &Value,
    outbox_row: &Value,
    effect_row: &Value,
) -> Api<Value> {
    loop {
        let current = s
            .store
            .get(PLATFORM_COMMANDS, command_id)
            .map_err(db)?
            .ok_or_else(|| Error::conflict("accepted approval command disappeared"))?;
        let (outbox_collection, effect_collection) = outbox::collections();
        let outbox_id = require(outbox_row, "outbox_id")?;
        let effect_id = require(effect_row, "effect_id")?;
        let current_outbox = s
            .store
            .get(outbox_collection, outbox_id)
            .map_err(db)?
            .ok_or_else(|| Error::conflict("SRC outbox entry disappeared"))?;
        let current_effect = s
            .store
            .get(effect_collection, effect_id)
            .map_err(db)?
            .ok_or_else(|| Error::conflict("SRC effect receipt disappeared"))?;
        if current["response"]["receipt"]["state"] != "accepted"
            && current_outbox["status"] != "pending"
        {
            return Ok(current["response"]["receipt"].clone());
        }
        let mut updated = current.clone();
        updated["response"]["receipt"]["state"] = result["state"].clone();
        updated["response"]["receipt"]["output"] = result["output"].clone();
        updated["response"]["receipt"]["error"] = result["error"].clone();
        if current["response"]["receipt"]["state"] == "accepted" {
            updated["response"]["receipt"]["completed_at"] = json!(now());
        }
        updated["response"]["receipt"]["effect_ids"] = json!([effect_id]);
        let receipt = updated["response"]["receipt"].clone();
        let (settled_outbox, settled_effect) =
            outbox::settled(&current_outbox, &current_effect, result);
        if s.store
            .put_many_if(
                &[
                    (PLATFORM_COMMANDS, command_id, Some(&current)),
                    (outbox_collection, outbox_id, Some(&current_outbox)),
                    (effect_collection, effect_id, Some(&current_effect)),
                ],
                &[
                    (PLATFORM_COMMANDS, command_id, &updated),
                    (outbox_collection, outbox_id, &settled_outbox),
                    (effect_collection, effect_id, &settled_effect),
                ],
            )
            .map_err(db)?
        {
            return Ok(receipt);
        }
    }
}

pub(super) async fn dispatch(
    s: Arc<AppState>,
    id: &str,
    envelope: &Value,
    scope: &str,
) -> Api<Value> {
    let task_row = task(&s, id)?;
    let kind = require(envelope, "command_type")?;
    if !supported(kind) {
        return Err(Error::invalid("unsupported SRC domain command"));
    }
    if !matches!(
        kind,
        "src.approval.decide"
            | "src.task.archive"
            | "src.task.restore"
            | "src.task.pause"
            | "src.task.resume"
            | "src.task.destroy"
            | "src.task.authorization.renew"
            | "src.run.archive"
            | "src.run.restore"
            | "src.run.destroy"
            | "src.artifact.destroy"
    ) && !asset_publication_command(kind)
        && !asset_query_command(kind)
        && !workbench_record_command(kind)
        && !connector_config_command(kind)
        && !connector_grant_command(kind)
        && kind != "src.asset.judgement.save"
        && kind != "src.grant.revoke"
        && matches!(
            task_row["lifecycle"]["state"].as_str(),
            Some("archived" | "destroyed")
        )
    {
        return Err(Error::conflict("SRC Task is not active"));
    }
    if (asset_publication_command(kind)
        || asset_query_command(kind)
        || workbench_record_command(kind)
        || connector_config_command(kind)
        || connector_grant_command(kind)
        || kind == "src.asset.judgement.save")
        && matches!(
            task_row["lifecycle"]["state"].as_str(),
            Some("archived" | "destroyed")
        )
    {
        let accepted = envelope["command_id"]
            .as_str()
            .map(|command_id| s.store.get(PLATFORM_COMMANDS, command_id))
            .transpose()
            .map_err(db)?
            .flatten()
            .is_some();
        if !accepted {
            return Err(Error::conflict("SRC Task is not active"));
        }
    }
    let payload_opt = envelope["payload"].as_object();
    let replay_key = envelope["idempotency_key"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            payload_opt
                .and_then(|p| p.get("idempotency_key"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        });
    let replay_cmd_id = envelope["command_id"].as_str().filter(|value| !value.is_empty());
    let has_replay_command = if let Some(cmd_id) = replay_cmd_id {
        s.store.get(PLATFORM_COMMANDS, cmd_id).map_err(db)?.is_some()
    } else if let Some(k) = replay_key {
        let kid = format!("{id}:{k}");
        if let Some(prior) = s.store.get(KEYS, &kid).map_err(db)? {
            prior["command_id"]
                .as_str()
                .map(|cid| s.store.get(PLATFORM_COMMANDS, cid).map_err(db))
                .transpose()?
                .flatten()
                .is_some()
        } else {
            false
        }
    } else {
        false
    };
    if !has_replay_command
        && task_row["lifecycle"]["state"].as_str() == Some("paused")
        && !allowed_while_paused(kind)
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.task_paused",
            "SRC Task 已暂停，请先恢复后再执行",
        ));
    }
    let actor_id = if let Some(operator) = scope.strip_prefix("operator:") {
        operator
    } else if let Some(sys) = scope.strip_prefix("system:") {
        sys
    } else {
        envelope["actor"]["id"].as_str().unwrap_or("")
    };
    let actor_kind = if scope.starts_with("operator:") {
        "operator"
    } else if scope.starts_with("system:") {
        "system"
    } else {
        envelope["actor"]["kind"].as_str().unwrap_or("agent")
    };
    let payload = envelope["payload"]
        .as_object()
        .ok_or_else(|| Error::invalid("SRC command payload must be an object"))?;
    let body = Value::Object(payload.clone());
    operation_contracts::validate(kind,&body,envelope["idempotency_key"].as_str())?;
    research_actor_claims(&body, actor_id, actor_kind)?;
    let expected = match envelope.get("expected_version") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| Error::invalid("expected_version must be an integer"))?,
        ),
    };
    let key = envelope["idempotency_key"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            body["idempotency_key"]
                .as_str()
                .filter(|value| !value.is_empty())
        });
    let key_id = key.map(|value| format!("{id}:{value}"));
    let fingerprint = if matches!(kind, "src.approval.decide") || finding_fields(kind).is_some() {
        digest(&json!([id, kind, body, expected, scope]))
    } else {
        json!([id, kind, body, expected, scope]).to_string()
    };
    let mut command_id = envelope["command_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4().simple()));
    let mut deduplicated = false;
    let mut stored_request = envelope.clone();
    loop {
        if let Some(key_id) = &key_id {
            if let Some(prior) = s.store.get(KEYS, key_id).map_err(db)? {
                if prior["request_fingerprint"] != fingerprint {
                    return Err(Error::conflict(format!(
                        "idempotency_key `{}` was already used for a different SRC command; resend identical arguments with the same key to dedupe, or use a new idempotency_key for a different request",
                        key.unwrap_or("")
                    )));
                }
                if let Some(prior_id) = prior["command_id"].as_str() {
                    if command_id != prior_id {
                        deduplicated = true;
                    }
                    command_id = prior_id.to_owned();
                }
            }
        }
        if let Some(existing) = s.store.get(PLATFORM_COMMANDS, &command_id).map_err(db)? {
            if existing["request_fingerprint"] != fingerprint {
                return Err(Error::conflict(format!(
                    "command_id `{command_id}` was already used for a different SRC command; resend identical arguments to dedupe, or use a new command_id for a different request"
                )));
            }
            deduplicated = true;
            break;
        }
        let head = s.store.get(HEADS, id).map_err(db)?;
        let version = head
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0);
        if expected.is_some_and(|value| value != version) {
            return Err(Error::conflict(format!(
                "expected Task event version {expected:?}, current {version}"
            )));
        }
        let seq = version + 1;
        let cursor = crate::operations_api::task_event_cursor(&s, id, seq, scope).map_err(db)?;
        let receipt = accepted_receipt(&command_id, id, &cursor);
        stored_request["command_id"] = json!(command_id);
        stored_request["command_type"] = json!(kind);
        stored_request["aggregate_type"] = json!("task");
        stored_request["aggregate_id"] = json!(id);
        stored_request["payload"] = body.clone();
        let event_key = format!("{id}:{seq}");
        let event = json!({"event_id":format!("evt-src-{}",uuid::Uuid::new_v4().simple()),
            "aggregate_type":"task","aggregate_id":id,"stream_seq":seq,
            "event_type":format!("{kind}.requested"),"occurred_at":now(),
            "producer":"builtin.src","actor_id":actor_id,
            "command_id":command_id,"causation_id":null,"correlation_id":command_id,
            "idempotency_key":key,"payload":body});
        let command = json!({"request":stored_request,"request_fingerprint":fingerprint,
            "response":{"receipt":receipt},"event_id":event["event_id"],"scope":scope,"status":200});
        let next_head = json!({"version":seq});
        let keyed = key_id
            .as_ref()
            .map(|_| json!({"command_id":command_id,"request_fingerprint":fingerprint}));
        let mut conditions = vec![
            (PLATFORM_COMMANDS, command_id.as_str(), None),
            (HEADS, id, head.as_ref()),
            (EVENTS, event_key.as_str(), None),
        ];
        let mut writes = vec![
            (PLATFORM_COMMANDS, command_id.as_str(), &command),
            (HEADS, id, &next_head),
            (EVENTS, event_key.as_str(), &event),
        ];
        if let (Some(key_id), Some(keyed)) = (key_id.as_ref(), keyed.as_ref()) {
            conditions.push((KEYS, key_id.as_str(), None));
            writes.push((KEYS, key_id.as_str(), keyed));
        }
        if s.store.put_many_if(&conditions, &writes).map_err(db)? {
            break;
        }
    }
    let accepted = s
        .store
        .get(PLATFORM_COMMANDS, &command_id)
        .map_err(db)?
        .ok_or_else(|| Error::conflict("SRC command could not be accepted"))?;
    if accepted["request_fingerprint"] != fingerprint {
        return Err(Error::conflict("SRC command content changed"));
    }
    let (outbox_row, mut effect_row) = outbox::ensure(&s, &command_id, id, kind, &accepted)?;
    let body = accepted["request"]["payload"].clone();
    let mut trusted = operation_contracts::normalize(kind,&body);
    if let Some(obj) = trusted.as_object_mut() {
        obj.insert("actor_id".to_owned(), json!(actor_id));
        obj.insert("actor_kind".to_owned(), json!(actor_kind));
        obj.insert("requester".to_owned(), json!(actor_id));
        obj.insert("principal_id".to_owned(), json!(actor_id));
        obj.insert("created_by".to_owned(), json!(actor_id));
        if !obj.contains_key("idempotency_key") || obj["idempotency_key"].is_null() {
            if let Some(key) = key {
                obj.insert("idempotency_key".to_owned(), json!(key));
            }
        }
        if object_command(kind) {
            let mut actor_obj = obj
                .get("actor")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            actor_obj.insert("id".to_owned(), json!(actor_id));
            actor_obj.insert("kind".to_owned(), json!(actor_kind));
            let has_run = actor_obj
                .get("run_id")
                .and_then(Value::as_str)
                .is_some_and(|v| !v.trim().is_empty());
            let has_thread = actor_obj
                .get("thread_id")
                .and_then(Value::as_str)
                .is_some_and(|v| !v.trim().is_empty());
            if !has_run && !has_thread {
                if let Some(run_id) = envelope["actor"]["run_id"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                {
                    actor_obj.insert("run_id".to_owned(), json!(run_id));
                    if let Some(worker_id) = envelope["actor"]["worker_id"]
                        .as_str()
                        .filter(|v| !v.is_empty())
                    {
                        actor_obj.insert("worker_id".to_owned(), json!(worker_id));
                    }
                } else {
                    let bound_thread = envelope["actor"]["thread_id"]
                        .as_str()
                        .filter(|v| !v.is_empty())
                        .or_else(|| task_row["binding"]["thread_id"].as_str())
                        .unwrap_or("");
                    if !bound_thread.is_empty() {
                        actor_obj.insert("thread_id".to_owned(), json!(bound_thread));
                    }
                }
            }
            obj.insert("actor".to_owned(), Value::Object(actor_obj));
        }
    }
    let body = if operation_contracts::auto_payload(kind) { trusted.clone() } else { body };
    let results = result_collection(kind);
    let result = if let Some(result) = s.store.get(results, &command_id).map_err(db)? {
        result
    } else if is_external_side_effect_command(kind)
        && effect_row["state"] == "dispatched"
        && effect_row["control_epoch"]
            .as_str()
            .is_some_and(|epoch| !epoch.is_empty() && epoch != s.chat.epoch())
        && !effect_was_applied(&s, id, kind, &body, &command_id, key)?
    {
        let interrupted = json!({
            "state": "outcome_unknown",
            "output": {},
            "error": {
                "code": "dispatch_interrupted",
                "category": "internal",
                "message": "外部执行在服务端重启或中断前已派发，无法确认远端执行结果",
                "retryable": false
            }
        });
        if s.store
            .put_if_absent(results, &command_id, &interrupted)
            .map_err(db)?
        {
            interrupted
        } else {
            s.store
                .get(results, &command_id)
                .map_err(db)?
                .ok_or_else(|| Error::conflict("SRC command result disappeared"))?
        }
    } else {
        if is_external_side_effect_command(kind) {
            mark_effect_dispatched(&s, &mut effect_row)?;
        }
        let applied = if matches!(
            kind,
            "src.task.archive" | "src.task.restore" | "src.task.pause" | "src.task.resume"
        ) {
            domains::apply_task_lifecycle_command(s.clone(), id, kind, &command_id, actor_id)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.task.destroy" {
            domains::apply_task_destroy_command(s.clone(), id, &command_id, actor_id)
                .await
                .map(|output| json!({"result":output}))
        } else if matches!(kind, "src.run.archive" | "src.run.restore") {
            domains::apply_run_lifecycle_command(s.clone(), id, kind, &body, &command_id, actor_id)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.run.destroy" {
            domains::apply_run_destroy_command(s.clone(), id, &body, &command_id, actor_id)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.artifact.destroy" {
            domains::apply_artifact_destroy_command(s.clone(), id, &body, &command_id, actor_id)
                .await
                .map(|output| json!({"result":output}))
        } else if asset_publication_command(kind) {
            domains::apply_asset_publication_command(&s, id, kind, &body, &command_id, actor_id)
                .map(|output| json!({"result":output}))
        } else if asset_query_command(kind) {
            domains::apply_asset_query_command(&s, id, kind, &body, &command_id, actor_id)
                .map(|output| json!({"result":output}))
        } else if workbench_record_command(kind) {
            domains::apply_workbench_record_command(&s, id, kind, &body, &command_id, actor_id)
                .map(|output| json!({"result":output}))
        } else if connector_config_command(kind) || connector_grant_command(kind) {
            domains::apply_connector_config_command(&s, id, kind, &body, &command_id, actor_id)
                .map(|output| json!({"result":output}))
        } else if kind == "src.asset.judgement.save" {
            domains::apply_asset_judgement_command(&s, id, &body, &command_id, actor_id)
                .map(|output| json!({"result":output}))
        } else if kind == "src.policy.update" {
            control_command::update_policy(s.clone(), id, &body, &command_id, actor_id)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.task.authorization.renew" {
            control_command::renew_authorization(s.clone(), id, &body, &command_id, actor_id)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.grant.revoke" {
            control_command::revoke_grant(&s, id, &body, &command_id)
                .map(|output| json!({"result":output}))
        } else if kind == "src.approval.decide" {
            applied_approval(&s, id, &body, &command_id, actor_id)
                .map(|output| json!({"result":output}))
        } else if kind == "src.workbench.slice.save" {
            save_slice_marked(&s, id, &body, Some(&command_id), Some(actor_id))
        } else if finding_fields(kind).is_some() {
            domains::apply_finding_command(&s, id, kind, &body, Some(&command_id))
                .map(|output| json!({"result":output}))
        } else if asset_command(kind) {
            domains::apply_asset_command(&s, id, kind, &body, &command_id, actor_id)
                .map(|output| json!({"result":output}))
        } else if kind == "src.research.plan.save" {
            domains::save_research_plan(&s, id, &body, Some(&command_id), actor_id)
                .map(|output| json!({"result":output}))
        } else if kind == "src.research.authorization.save" {
            domains::save_research_authorization(&s, id, &body, Some(&command_id), actor_id)
                .map(|output| json!({"result":output}))
        } else if kind == "src.research.skill.register" {
            domains::save_research_skill(&s, id, &body, Some(&command_id), actor_id)
                .map(|output| json!({"result":output}))
        } else if kind == "src.research.openapi.ingest" {
            domains::ingest_research_openapi(&s, id, &body, Some(&command_id))
                .map(|output| json!({"result":output}))
        } else if kind == "src.research.javascript.ingest" {
            domains::ingest_research_javascript(&s, id, &body, Some(&command_id))
                .map(|output| json!({"result":output}))
        } else if kind == "src.research.javascript.collect" {
            domains::collect_javascript(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.exchange.differential_probe" {
            domains::differential_probe(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.exchange.race_probe" {
            domains::apply_race_probe(s.clone(), id, &trusted, Some(&command_id))
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.report.reproduction.auto" {
            domains::auto_reproduce_finding(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if research_observation_command(kind) {
            research_actor_claims(&body, actor_id, actor_kind)
                .and_then(|_| {
                    domains::register_research_observation(&s, id, kind, &body, Some(&command_id))
                })
                .map(|output| json!({"result":output}))
        } else if research_business_command(kind) {
            research_actor_claims(&body, actor_id, actor_kind)
                .and_then(|_| {
                    domains::apply_research_business_command(&s, id, kind, &body, &command_id)
                })
                .map(|output| json!({"result":output}))
        } else if research_workflow_command(kind) {
            research_actor_claims(&body, actor_id, actor_kind)
                .and_then(|_| {
                    domains::apply_research_workflow_command(
                        &s,
                        id,
                        kind,
                        &body,
                        &command_id,
                        actor_id,
                        actor_kind,
                    )
                })
                .map(|output| json!({"result":output}))
        } else if coverage_command(kind) {
            domains::apply_coverage_command(&s, id, kind, &body, &command_id)
                .map(|output| json!({"result":output}))
        } else if report_command(kind) {
            domains::apply_report_command(&s, id, kind, &body, &command_id, actor_id, key)
                .map(|output| json!({"result":output}))
        } else if kind == "src.exchange.draft.execute" {
            let mut execution = body.clone();
            if execution["idempotency_key"].is_null() {
                if let Some(key) = key {
                    execution["idempotency_key"] = json!(key);
                }
            }
            domains::execute_draft(s.clone(), id, &execution)
                .await
                .map(|receipt| json!({"result":receipt["output"]}))
        } else if kind == "src.connector.import" {
            let mut imported = body.clone();
            if imported["idempotency_key"].is_null() {
                if let Some(key) = key {
                    imported["idempotency_key"] = json!(key);
                }
            }
            domains::apply_connector_import_command(&s, id, &imported, &command_id)
                .map(|output| json!({"result":output}))
        } else if exchange_command(kind) {
            if kind == "src.exchange.draft.save" && key.is_none() {
                Err(Error::invalid("idempotency_key is required"))
            } else {
                domains::apply_exchange_command(&s, id, kind, &trusted, &command_id)
                    .map(|output| json!({"result":output}))
            }
        } else if kind == "src.run.ensure" {
            super::ensure_run(&s, id, &trusted).map(|output| {
                let mut wrapped = output.clone();
                wrapped["result"] = output;
                wrapped
            })
        } else if kind == "src.run.start" {
            super::start_command(&s, id, &trusted).await.map(|receipt| {
                let mut wrapped = receipt["output"].clone();
                wrapped["result"] = receipt["output"].clone();
                wrapped
            })
        } else if kind == "src.retest.dispatch" {
            domains::dispatch_retest(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.connector.repository" {
            if trusted["idempotency_key"].as_str().is_none_or(str::is_empty) {
                trusted["idempotency_key"] = json!(command_id);
            }
            domains::repository(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.connector.query" {
            domains::query_connector(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.connector.verify" {
            domains::verify_connector(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.monitor.run_once" {
            domains::monitor_once(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.monitor.cycle.run" {
            domains::monitor_cycle(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.monitor.wake" {
            domains::monitor_wake(s.clone(), id)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.monitor.notifications.deliver" {
            domains::deliver_monitor_notifications(s.clone(), id)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.coordination.wake" {
            domains::coordination_wake(s.clone(), id)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.identity.login_recipe.replay" {
            domains::apply_login_recipe_replay(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.identity.session.probe" {
            domains::apply_session_probe(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.proxy_bridge.start" {
            domains::apply_proxy_bridge_start(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.mutator.test" {
            domains::apply_mutator_test(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.notify.test" {
            domains::apply_notify_test(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.notify.flush_carryover" {
            domains::apply_notify_flush_carryover(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if kind == "src.run.intervene" {
            domains::apply_run_intervene(s.clone(), id, &trusted)
                .await
                .map(|output| json!({"result":output}))
        }else if matches!(kind,"src.oast.allocate"|"src.oast.local.start"){
            domains::oast_command(s.clone(),id,kind,&trusted).await.map(|output|json!({"result":output}))
        } else if stream_async_command(kind) {
            domains::stream_command(s.clone(), id, kind, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if browser_async_command(kind) {
            let _guard = super::browser_mutation_lock().lock().await;
            domains::browser_command(s.clone(), id, kind, &trusted)
                .await
                .map(|output| json!({"result":output}))
        } else if supported(kind) {
            let _guard = s.chat.active.lock().await;
            let _chat = crate::chat::ChatLockFlag::enter();
            domains::command(&s, id, kind, &trusted).map(|output| json!({"result":output}))
        } else {
            Err(Error::invalid("unsupported SRC domain command"))
        };
        if let Err(error) = &applied {
            if (matches!(
                kind,
                "src.task.archive"
                    | "src.task.restore"
                    | "src.task.pause"
                    | "src.task.resume"
                    | "src.task.destroy"
                    | "src.run.archive"
                    | "src.run.restore"
                    | "src.run.destroy"
                    | "src.artifact.destroy"
            ) || asset_publication_command(kind)
                || asset_query_command(kind)
                || workbench_record_command(kind)
                || connector_config_command(kind)
                || connector_grant_command(kind)
                || kind == "src.asset.judgement.save")
                && error.status == StatusCode::INTERNAL_SERVER_ERROR
            {
                return Err(Error {
                    status: error.status,
                    code: error.code,
                    message: error.message.clone(),
                    detail: error.detail.clone(),
                });
            }
            if (matches!(
                kind,
                "src.task.archive"
                    | "src.task.restore"
                    | "src.task.pause"
                    | "src.task.resume"
                    | "src.task.destroy"
                    | "src.run.archive"
                    | "src.run.restore"
                    | "src.run.destroy"
                    | "src.artifact.destroy"
                    | "src.policy.update"
                    | "src.task.authorization.renew"
                    | "src.grant.revoke"
                    | "src.connector.import"
                    | "src.research.plan.save"
                    | "src.research.authorization.save"
                    | "src.research.skill.register"
                    | "src.research.openapi.ingest"
                    | "src.research.javascript.ingest"
            ) || research_observation_command(kind)
                || research_business_command(kind)
                || research_workflow_command(kind)
                || asset_command(kind)
                || asset_publication_command(kind)
                || asset_query_command(kind)
                || workbench_record_command(kind)
                || connector_config_command(kind)
                || connector_grant_command(kind)
                || kind == "src.asset.judgement.save"
                || exchange_command(kind))
                && error.status == StatusCode::INTERNAL_SERVER_ERROR
                && effect_was_applied(&s, id, kind, &body, &command_id, key)?
            {
                return Err(Error {
                    status: error.status,
                    code: error.code,
                    message: error.message.clone(),
                    detail: error.detail.clone(),
                });
            }
        }
        let result = match applied {
            Ok(output) => {
                let _ = s.store.put_if_absent(
                    DOMAIN_APPLIED,
                    &command_id,
                    &json!({"command_id":command_id}),
                );
                json!({"state":"completed","output":output,"error":null})
            }
            Err(error) => json!({"state":"failed","output":{},"error":error_value(&error)}),
        };
        if s.store
            .put_if_absent(results, &command_id, &result)
            .map_err(db)?
        {
            result
        } else {
            s.store
                .get(results, &command_id)
                .map_err(db)?
                .ok_or_else(|| Error::conflict("SRC command result disappeared"))?
        }
    };
    let mut receipt = finalize(&s, &command_id, &result, &outbox_row, &effect_row).await?;
    if kind == "src.approval.decide" && result["state"] == "completed" {
        if let Err(error) =
            domains::after_approval(s.clone(), id, &result["output"]["result"]).await
        {
            tracing::error!(task_id=id,command_id,%error,"approved SRC invocation did not settle");
        }
    }
    if result["state"] == "completed" && !deduplicated {
        let event = match kind {
            "src.finding.review.record" => Some("finding_review"),
            "src.retest.complete" => Some("retest_complete"),
            "src.monitor.run_once" | "src.monitor.cycle.run" => Some("monitor_change"),
            "src.execution.request" => Some("approval_pending"),
            _ => None,
        };
        if let Some(event) = event {
            if let Err(error) = domains::notify_event(&s, id, event, &command_id).await {
                receipt["notification_error"] = error_value(&error);
            }
        }
    }
    if deduplicated {
        receipt["deduplicated"] = json!(true);
    }
    Ok(receipt)
}
