use super::{
    EV_FACT_ADDED, EV_FACT_CHALLENGED, EV_FACT_MERGED, EV_FACT_REJECTED,
    EV_FACT_REVALIDATED, EV_FACT_REVIEWED, EV_FACT_SUPERSEDED, EV_INTENT_CONCLUDED,
    EV_INTENT_PROPOSED, EV_INTENT_STATE_CHANGED, EV_OBSERVATION_ADDED, EV_OPERATOR_DIRECTIVE,
    EV_REVIEW_FINDING, EV_REVIEW_PROPOSAL, EV_REVIEW_PROPOSAL_DECISION, SharedGraph,
    canonical_fact_whitespace, normalize_route_hash, now_secs,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub fn normalize_review_marker(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "REVIEW_FINDING" | "NO_ACTION" => Some("REVIEW_FINDING"),
        "FACT_CHALLENGE" | "CHALLENGE_FACT" => Some("FACT_CHALLENGE"),
        "FACT_REVALIDATION" | "REVALIDATE_FACT" => Some("FACT_REVALIDATION"),
        "FACT_REJECT" | "REJECT_FACT" => Some("FACT_REJECT"),
        "FACT_MERGE" | "MERGE_FACTS" | "MERGE_FACT" => Some("FACT_MERGE"),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct ReviewTriggerState {
    pub last_reviewed_watermark: i64,
    pub last_review_dispatched_at: Option<Instant>,
    pub next_review_seq: usize,
}

impl Default for ReviewTriggerState {
    fn default() -> Self {
        Self {
            last_reviewed_watermark: 0,
            last_review_dispatched_at: None,
            next_review_seq: 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutoReviewRequest {
    pub intent_id: String,
    pub created_seq: i64,
    pub goal: String,
    pub triggers: Vec<String>,
    pub target_fact_seqs: Vec<i64>,
    pub route_hash: String,
    pub coverage_key: String,
    pub watermark: i64,
}

impl SharedGraph {
    pub(crate) fn with_immediate_tx<T>(
        &self,
        op: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let nested = !self.conn.is_autocommit();
        if nested {
            self.conn
                .execute_batch("SAVEPOINT muteki_review_tx")
                .map_err(|error| error.to_string())?;
        } else {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(|error| error.to_string())?;
        }
        match op() {
            Ok(val) => {
                if nested {
                    self.conn
                        .execute_batch("RELEASE SAVEPOINT muteki_review_tx")
                        .map_err(|error| error.to_string())?;
                } else {
                    self.conn
                        .execute_batch("COMMIT")
                        .map_err(|error| error.to_string())?;
                }
                Ok(val)
            }
            Err(error) => {
                if nested {
                    let _ = self.conn.execute_batch(
                        "ROLLBACK TO SAVEPOINT muteki_review_tx; RELEASE SAVEPOINT muteki_review_tx",
                    );
                } else {
                    let _ = self.conn.execute_batch("ROLLBACK");
                }
                Err(error)
            }
        }
    }

    fn review_event(&self, kind: &str, actor: &str, payload: &Value,
        dedupe: &str) -> Result<i64, String> {
        let existing = || self.conn.query_row(
            "SELECT seq FROM events WHERE dedupe_key=?1", [dedupe],
            |row| row.get::<_, i64>(0),
        ).optional().map_err(|error| error.to_string());
        if let Some(seq) = existing()? { return Ok(seq); }
        let seq = self.append_event(kind, actor, payload, None, true, 1.0, Some(dedupe));
        if seq > 0 { Ok(seq) }
        else { existing()?.ok_or_else(|| format!("Unable to record {kind}")) }
    }

    pub fn add_review_proposal(&self, actor: &str, marker: &str,
        payload: &Value) -> Result<i64, String> {
        let raw_trimmed = marker.trim();
        let Some(norm_marker) = normalize_review_marker(raw_trimmed) else {
            return Err(format!("marker is outside Review authority: {}", raw_trimmed.to_ascii_uppercase()));
        };
        let mut clean = payload.as_object().cloned().map(Value::Object)
            .ok_or("Review proposal payload must be an object")?;
        if raw_trimmed.eq_ignore_ascii_case("no_action") && clean.get("kind").is_none() {
            clean["kind"] = json!("no_action");
        }
        if let Some(route) = clean["route_hash"].as_str().filter(|route| !route.is_empty())
            .map(str::to_owned) {
            clean["route_hash"] = json!(normalize_route_hash(&route));
        }
        let confidence = clean["confidence"].as_f64().or_else(||
            clean["confidence"].as_str().and_then(|value| value.parse().ok()))
            .unwrap_or(1.0).clamp(0.0, 1.0);
        clean["confidence"] = json!(confidence);
        let event = json!({"marker":norm_marker,"tier":"tier1","payload":clean,
            "status":"pending"});
        let dedupe = format!("review-proposal::{norm_marker}::{clean}");
        self.review_event(EV_REVIEW_PROPOSAL, actor, &event, &dedupe)
    }

    fn pending_review_proposals(&self) -> Vec<Value> {
        let events = self.events();
        let decided: HashSet<i64> = events.iter()
            .filter(|event| event["kind"] == EV_REVIEW_PROPOSAL_DECISION)
            .filter_map(|event| event["payload"]["proposal_seq"].as_i64())
            .collect();
        events.into_iter().filter(|event| event["kind"] == EV_REVIEW_PROPOSAL
            && event["seq"].as_i64().is_some_and(|seq| !decided.contains(&seq)))
            .collect()
    }

    fn decide_review_proposal(&self, proposal_seq: i64, decision: &str,
        reason: &str, applied_seq: Option<i64>) -> Result<i64, String> {
        let mut payload = json!({"proposal_seq":proposal_seq,"decision":decision,
            "reason":reason});
        if let Some(seq) = applied_seq { payload["applied_seq"] = json!(seq); }
        self.review_event(EV_REVIEW_PROPOSAL_DECISION, "coordinator", &payload,
            &format!("review-proposal-decision::{proposal_seq}::{decision}"))
    }

    fn review_fact(&self, fact_seq: i64) -> Result<Value, String> {
        let raw: Option<String> = self.conn.query_row(
            "SELECT payload FROM events WHERE seq=?1 AND kind=?2",
            params![fact_seq, EV_FACT_ADDED], |row| row.get(0),
        ).optional().map_err(|error| error.to_string())?;
        raw.ok_or_else(|| format!("Unknown Fact event: {fact_seq}"))
            .and_then(|value| serde_json::from_str(&value).map_err(|error| error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_proposals_apply_once_to_real_sqlite_graph() {
        let dir = tempfile::tempdir().unwrap();
        let graph = SharedGraph::open_at(dir.path().join("shared_graph.db")).unwrap();
        let first = graph.append_event(EV_FACT_ADDED, "solver", &json!({"fact":"first claim"}),
            Some("art-1"), true, 1.0, None);
        let second = graph.append_event(EV_FACT_ADDED, "solver", &json!({"fact":"duplicate claim"}),
            Some("art-2"), true, 1.0, None);
        assert!(first > 0 && second > first);

        let finding = json!({"kind":"no_action","severity":"info","summary":"scope reviewed"});
        let proposal = graph.add_review_proposal("reviewer", "REVIEW_FINDING", &finding).unwrap();
        assert_eq!(graph.add_review_proposal("reviewer", "REVIEW_FINDING", &finding).unwrap(), proposal);
        let decisions = graph.drain_review_proposals(8);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["decision"], "accepted");
        assert_eq!(graph.drain_review_proposals(8).len(), 0);
        assert!(graph.events().iter().any(|event| event["kind"] == EV_REVIEW_FINDING
            && event["payload"]["finding_id"] == format!("rvw-{proposal}")));

        graph.add_review_proposal("reviewer", "FACT_CHALLENGE",
            &json!({"fact_seq":first,"reason":"reproduce","verification_goal":"Check claim"})).unwrap();
        assert_eq!(graph.drain_review_proposals(0)[0]["decision"], "deferred");
        graph.add_review_proposal("reviewer", "FACT_CHALLENGE",
            &json!({"fact_seq":first,"reason":"independent check","verification_goal":"Check claim"})).unwrap();
        assert_eq!(graph.drain_review_proposals(8)[0]["decision"], "accepted");
        assert!(!graph.active_facts().iter().any(|fact| fact["fact_seq"] == first));
        assert!(graph.all_intents().iter().any(|intent| intent["worker_class"] == "verifier"));

        graph.add_review_proposal("reviewer", "FACT_REVALIDATION",
            &json!({"fact_seq":first,"reason":"reproduced"})).unwrap();
        assert_eq!(graph.drain_review_proposals(8)[0]["decision"], "accepted");
        assert!(graph.active_facts().iter().any(|fact| fact["fact_seq"] == first));

        graph.add_review_proposal("reviewer", "FACT_MERGE",
            &json!({"from_fact_seq":second,"to_fact_seq":first,"reason":"same issue"})).unwrap();
        assert_eq!(graph.drain_review_proposals(8)[0]["decision"], "accepted");
        assert!(!graph.active_facts().iter().any(|fact| fact["fact_seq"] == second));

        graph.add_review_proposal("reviewer", "FACT_REJECT",
            &json!({"fact_seq":first,"reason":"contradicted"})).unwrap();
        assert_eq!(graph.drain_review_proposals(8)[0]["decision"], "accepted");
        assert!(!graph.active_facts().iter().any(|fact| fact["fact_seq"] == first));
    }

    #[test]
    fn automatic_review_to_verifier_to_fact_supersede_revalidate_and_unresolved_loop() {
        let dir = tempfile::tempdir().unwrap();
        let graph = SharedGraph::open_at(dir.path().join("shared_graph.db")).unwrap();
        graph.seed_origin_fact(&json!({"name":"crypto-box","category":"crypto"}));

        let (_, Some(fact_a)) = graph.add_evidence(
            "cli-claude#1",
            "AES key candidate",
            "Recovered key is 0011223344556677",
            Some("art-1"),
            true,
            0.9,
            Some("step_001"),
            Some("route-aes"),
            None,
        ) else {
            panic!("expected fact_a");
        };
        let (_, Some(fact_b)) = graph.add_evidence(
            "cli-claude#1",
            "IV candidate",
            "Initialization vector is aabbccddeeff0011",
            Some("art-2"),
            true,
            0.9,
            Some("step_001"),
            Some("route-aes"),
            None,
        ) else {
            panic!("expected fact_b");
        };
        let (_, Some(fact_c)) = graph.add_evidence(
            "cli-claude#1",
            "Padding oracle claim",
            "Endpoint /decrypt leaks PKCS7 padding errors",
            Some("art-3"),
            true,
            0.8,
            Some("step_001"),
            Some("route-aes"),
            None,
        ) else {
            panic!("expected fact_c");
        };

        assert!(
            graph.propose_intent(
                "reason",
                "step_dep_on_a",
                "Use key 0011223344556677 to decrypt flag",
                &json!({"worker_class":"code"}),
                Some(&[fact_a]),
            ) > 0
        );

        let mut trigger_state = ReviewTriggerState::default();
        let settings = json!({"review":{"enabled":true,"max_concurrent":0,"cooldown_seconds":0}});
        let auto_req = graph
            .schedule_auto_review(&mut trigger_state, &settings, true, 0, 4)
            .expect("expected auto review request");
        assert!(auto_req.triggers.contains(&"new_facts".to_owned()));
        assert!(auto_req.target_fact_seqs.contains(&fact_a));
        assert!(auto_req.target_fact_seqs.contains(&fact_b));
        assert!(
            graph
                .open_dispatchable_intents()
                .iter()
                .any(|i| i["intent_id"] == auto_req.intent_id && i["worker_class"] == "review")
        );

        assert!(graph.claim_intent("cli-reviewer#1", &auto_req.intent_id, 600.0));
        let prop_a = graph
            .add_review_proposal(
                "cli-reviewer#1",
                "challenge_fact",
                &json!({
                    "fact_seq": fact_a,
                    "reason": "Key bytes were not tested against ciphertext",
                    "verification_goal": "Test AES key against ciphertext.bin"
                }),
            )
            .unwrap();
        let prop_b = graph
            .add_review_proposal(
                "cli-reviewer#1",
                "challenge_fact",
                &json!({
                    "fact_seq": fact_b,
                    "reason": "Verify IV offset in binary",
                    "verification_goal": "Confirm IV bytes from binary"
                }),
            )
            .unwrap();
        let prop_c = graph
            .add_review_proposal(
                "cli-reviewer#1",
                "challenge_fact",
                &json!({
                    "fact_seq": fact_c,
                    "reason": "Check whether /decrypt status differs",
                    "verification_goal": "Replay /decrypt oracle probe"
                }),
            )
            .unwrap();
        graph.conclude_intent("cli-reviewer#1", &auto_req.intent_id, "done", "submitted proposals");

        let decisions = graph.drain_review_proposals(8);
        assert_eq!(decisions.len(), 3);
        assert!(decisions.iter().all(|d| d["decision"] == "accepted"));

        let verify_a = format!("I-verify-{fact_a}-{prop_a}");
        let verify_b = format!("I-verify-{fact_b}-{prop_b}");
        let verify_c = format!("I-verify-{fact_c}-{prop_c}");

        assert_eq!(
            graph.fact_state(fact_a).unwrap()["verification_intent_id"],
            verify_a
        );
        assert_eq!(
            graph.fact_review(fact_a).unwrap()["verification_intent_id"],
            verify_a
        );

        assert!(graph.claim_intent("cli-verifier#1", &verify_a, 600.0));
        let (_, Some(replacement_a)) = graph.add_evidence(
            "cli-verifier#1",
            "Contradicted key claim",
            "Claim is false: actual AES key in binary is ffeeddccbbaa9988",
            Some("art-10"),
            true,
            1.0,
            Some(&verify_a),
            Some("route-aes"),
            None,
        ) else {
            panic!("expected replacement fact");
        };
        graph.conclude_intent("cli-verifier#1", &verify_a, "done", "contradicted by binary dump");

        let state_a = graph.fact_state(fact_a).unwrap();
        assert_eq!(state_a["status"], "superseded");
        assert_eq!(state_a["superseded_by_seq"], replacement_a);
        assert_eq!(graph.fact_review(fact_a).unwrap()["verdict"], "superseded");
        assert!(!graph.active_facts().iter().any(|f| f["fact_seq"] == fact_a));
        assert!(graph.active_facts().iter().any(|f| f["fact_seq"] == replacement_a));
        let dep_intent = graph
            .all_intents()
            .into_iter()
            .find(|i| i["intent_id"] == "step_dep_on_a")
            .unwrap();
        assert_eq!(dep_intent["dispatch_state"], "retired");

        assert!(graph.claim_intent("cli-verifier#1", &verify_b, 600.0));
        let (_, Some(_confirm_b)) = graph.add_evidence(
            "cli-verifier#1",
            "Confirmed IV",
            "Reproduced and confirmed IV is aabbccddeeff0011 from binary rodata",
            Some("art-11"),
            true,
            1.0,
            Some(&verify_b),
            Some("route-aes"),
            None,
        ) else {
            panic!("expected confirming fact");
        };
        graph.conclude_intent("cli-verifier#1", &verify_b, "done", "confirmed IV");

        assert_eq!(graph.fact_state(fact_b).unwrap()["status"], "active");
        assert_eq!(graph.fact_review(fact_b).unwrap()["verdict"], "revalidated");
        assert!(graph.active_facts().iter().any(|f| f["fact_seq"] == fact_b));

        assert!(graph.claim_intent("cli-verifier#1", &verify_c, 600.0));
        graph.conclude_intent("cli-verifier#1", &verify_c, "timeout", "target timed out");

        assert_eq!(graph.fact_state(fact_c).unwrap()["status"], "unresolved");
        assert_eq!(graph.fact_review(fact_c).unwrap()["verdict"], "unresolved");
        assert!(!graph.active_facts().iter().any(|f| f["fact_seq"] == fact_c));
    }
}

impl SharedGraph {
    fn change_fact_state_with_intent(
        &self,
        fact_seq: i64,
        status: &str,
        verdict: &str,
        reason: &str,
        event_seq: i64,
        verification_intent_id: Option<&str>,
    ) -> Result<(), String> {
        let now = now_secs();
        let vid = verification_intent_id.unwrap_or("");
        self.conn.execute(
            "INSERT INTO fact_states
             (fact_seq,status,rationale,updated_by,updated_seq,updated_ts,verification_intent_id)
             VALUES (?1,?2,?3,'coordinator',?4,?5,?6)
             ON CONFLICT(fact_seq) DO UPDATE SET status=excluded.status,
             rationale=excluded.rationale,updated_by=excluded.updated_by,
             updated_seq=excluded.updated_seq,updated_ts=excluded.updated_ts,
             verification_intent_id=CASE WHEN excluded.verification_intent_id != ''
                 THEN excluded.verification_intent_id ELSE fact_states.verification_intent_id END",
            params![fact_seq,status,reason,event_seq,now,vid],
        ).map_err(|error| error.to_string())?;
        self.conn.execute(
            "INSERT INTO fact_reviews
             (fact_seq,verdict,reviewer,rationale,reviewed_seq,updated_ts,verification_intent_id)
             VALUES (?1,?2,'coordinator',?3,?4,?5,?6)
             ON CONFLICT(fact_seq) DO UPDATE SET verdict=excluded.verdict,
             reviewer=excluded.reviewer,rationale=excluded.rationale,
             reviewed_seq=excluded.reviewed_seq,updated_ts=excluded.updated_ts,
             verification_intent_id=CASE WHEN excluded.verification_intent_id != ''
                 THEN excluded.verification_intent_id ELSE fact_reviews.verification_intent_id END",
            params![fact_seq,verdict,reason,event_seq,now,vid],
        ).map_err(|error| error.to_string())?;
        Ok(())
    }

    fn change_fact_state(&self, fact_seq: i64, status: &str, verdict: &str,
        reason: &str, event_seq: i64) -> Result<(), String> {
        self.change_fact_state_with_intent(fact_seq, status, verdict, reason, event_seq, None)
    }

    fn apply_review_finding(&self, proposal: &Value) -> Result<i64, String> {
        self.with_immediate_tx(|| {
            let proposal_seq = proposal["seq"].as_i64().ok_or("Review proposal has no sequence")?;
            let payload = &proposal["payload"]["payload"];
            let kind = payload["kind"].as_str().unwrap_or("no_action").trim();
            let kind = if kind.is_empty() { "no_action" } else { kind };
            let summary = payload["summary"].as_str().unwrap_or("").trim();
            let route = payload["route_hash"].as_str().unwrap_or("");
            let severity = payload["severity"].as_str().unwrap_or("info");
            let severity = if matches!(severity, "info" | "warn" | "blocker") {
                severity
            } else {
                "warn"
            };
            let finding = json!({
                "finding_id":format!("rvw-{proposal_seq}"),"kind":kind,
                "severity":severity,"summary":summary,
                "evidence_seqs":payload["evidence_seqs"].as_array().cloned().unwrap_or_default(),
                "intent_ids":payload["intent_ids"].as_array().cloned().unwrap_or_default(),
                "route_hash":normalize_route_hash(route),
                "branch_id":payload["branch_id"].as_str().unwrap_or(""),
                "recommended_actions":payload["recommended_actions"].as_array().cloned().unwrap_or_default(),
                "worker":proposal["actor"],
            });
            let applied = self.review_event(
                EV_REVIEW_FINDING,
                "coordinator",
                &finding,
                &format!("review::{kind}::{summary}::{route}"),
            )?;
            self.decide_review_proposal(proposal_seq, "accepted", "", Some(applied))?;
            Ok(applied)
        })
    }

    pub fn apply_fact_challenge(&self, proposal: &Value) -> Result<i64, String> {
        self.with_immediate_tx(|| {
            let proposal_seq = proposal["seq"].as_i64().ok_or("Review proposal has no sequence")?;
            let payload = &proposal["payload"]["payload"];
            let fact_seq = payload["fact_seq"].as_i64().ok_or("fact_seq is required")?;
            let fact = self.review_fact(fact_seq)?;
            let reason = payload["reason"].as_str().unwrap_or("").trim();
            let goal = payload["verification_goal"].as_str().unwrap_or("").trim();
            let goal = if goal.is_empty() {
                format!("Verify Fact #{fact_seq}: {reason}")
            } else {
                goal.to_owned()
            };
            let intent_id = format!("I-verify-{fact_seq}-{proposal_seq}");
            let applied = self.review_event(
                EV_FACT_CHALLENGED,
                "coordinator",
                &json!({
                    "fact_seq":fact_seq,
                    "status":"challenged",
                    "reason":reason,
                    "challenged_by":"coordinator",
                    "verification_intent_id":intent_id
                }),
                &format!("fact-challenged::{fact_seq}::{reason}"),
            )?;
            self.change_fact_state_with_intent(
                fact_seq,
                "challenged",
                "challenged",
                reason,
                applied,
                Some(&intent_id),
            )?;
            let claim = fact["fact"].as_str().unwrap_or("");
            let verification_goal = format!(
                "{goal}\n\nUnverified claim under review (Fact event #{fact_seq}):\n{claim}"
            );
            let intent_seq = self.propose_intent(
                "coordinator",
                &intent_id,
                &verification_goal,
                &json!({
                    "worker_class":"verifier",
                    "priority":"high",
                    "challenged_fact_seq":fact_seq,
                    "proposal_seq":proposal_seq,
                    "rationale":format!("Review challenged fact #{fact_seq}: {reason}"),
                    "expected_observable":format!("Fresh tool output that confirms or contradicts fact #{fact_seq}"),
                    "stop_condition":"Stop after the bounded reproduction attempt and record verified evidence or a dead end"
                }),
                Some(&[fact_seq]),
            );
            if intent_seq <= 0
                && !self.all_intents().iter().any(|row| row["intent_id"] == intent_id)
            {
                return Err(format!("Unable to create verifier intent {intent_id}"));
            }
            self.decide_review_proposal(proposal_seq, "accepted", "", Some(applied))?;
            Ok(applied)
        })
    }

    pub fn apply_fact_reject(&self, proposal: &Value) -> Result<i64, String> {
        self.apply_fact_verdict(proposal, true)
    }

    pub fn apply_fact_revalidation(&self, proposal: &Value) -> Result<i64, String> {
        self.apply_fact_verdict(proposal, false)
    }

    fn apply_fact_verdict(&self, proposal: &Value, reject: bool) -> Result<i64, String> {
        self.with_immediate_tx(|| {
            let proposal_seq = proposal["seq"].as_i64().ok_or("Review proposal has no sequence")?;
            let payload = &proposal["payload"]["payload"];
            let fact_seq = payload["fact_seq"].as_i64().ok_or("fact_seq is required")?;
            self.review_fact(fact_seq)?;
            let reason = payload["reason"].as_str().unwrap_or("").trim();
            let (kind, status, verdict) = if reject {
                (EV_FACT_REJECTED, "rejected", "rejected")
            } else {
                (EV_FACT_REVALIDATED, "active", "revalidated")
            };
            let actor_key = if reject { "rejected_by" } else { "revalidated_by" };
            let mut event = json!({"fact_seq":fact_seq,"status":verdict,"reason":reason});
            event[actor_key] = json!("coordinator");
            let applied = self.review_event(
                kind,
                "coordinator",
                &event,
                &format!("{kind}::{fact_seq}::{reason}"),
            )?;
            self.change_fact_state(fact_seq, status, verdict, reason, applied)?;
            if reject {
                self.retire_rejected_fact_dependents(fact_seq)?;
            }
            self.decide_review_proposal(proposal_seq, "accepted", "", Some(applied))?;
            Ok(applied)
        })
    }

    pub fn apply_fact_merge(&self, proposal: &Value) -> Result<i64, String> {
        self.with_immediate_tx(|| {
            let proposal_seq = proposal["seq"].as_i64().ok_or("Review proposal has no sequence")?;
            let payload = &proposal["payload"]["payload"];
            let from = payload["fact_seq"]
                .as_i64()
                .or_else(|| payload["from_fact_seq"].as_i64())
                .ok_or("from_fact_seq is required")?;
            let to = payload["to_fact_seq"].as_i64().ok_or("to_fact_seq is required")?;
            if from == to {
                return Err("merge into self / invalid to_fact_seq".to_owned());
            }
            self.review_fact(from)?;
            self.review_fact(to)?;
            let reason = payload["reason"].as_str().unwrap_or("").trim();
            let applied = self.review_event(
                EV_FACT_MERGED,
                "coordinator",
                &json!({
                    "from_fact_seq":from,
                    "to_fact_seq":to,
                    "status":"merged",
                    "reason":reason,
                    "merged_by":"coordinator"
                }),
                &format!("fact-merged::{from}::{to}"),
            )?;
            self.change_fact_state(from, "merged", "merged", reason, applied)?;
            self.conn
                .execute(
                    "INSERT INTO fact_merges
                     (alias_fact_seq,canonical_fact_seq,merged_by,rationale,merged_seq,updated_ts)
                     VALUES (?1,?2,'coordinator',?3,?4,?5)
                     ON CONFLICT(alias_fact_seq) DO UPDATE SET
                     canonical_fact_seq=excluded.canonical_fact_seq,
                     rationale=excluded.rationale,merged_seq=excluded.merged_seq,
                     updated_ts=excluded.updated_ts",
                    params![from, to, reason, applied, now_secs()],
                )
                .map_err(|error| error.to_string())?;
            self.conn
                .execute(
                    "UPDATE fact_states SET canonical_fact_seq=?1 WHERE fact_seq=?2",
                    params![to, from],
                )
                .map_err(|error| error.to_string())?;
            self.decide_review_proposal(proposal_seq, "accepted", "", Some(applied))?;
            Ok(applied)
        })
    }

    fn retire_rejected_fact_dependents(&self, fact_seq: i64) -> Result<(), String> {
        let mut stmt = self.conn.prepare(
            "SELECT i.intent_id FROM intents i JOIN intent_sources s ON s.intent_id=i.intent_id
             WHERE s.fact_seq=?1 AND i.status='open' AND i.dispatch_state='active'
               AND i.worker_class != 'verifier'",
        ).map_err(|error| error.to_string())?;
        let ids = stmt.query_map([fact_seq], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?;
        for intent_id in ids {
            let seq = self.append_event(EV_INTENT_STATE_CHANGED, "coordinator",
                &json!({"intent_id":intent_id,"status":"done",
                    "dispatch_state":"retired","reason":format!("Prerequisite fact_{fact_seq} was rejected"),
                    "fact_seq":fact_seq}), None, true, 1.0, None);
            self.conn.execute(
                "UPDATE intents SET status='done',dispatch_state='retired',result='rejected_prerequisite',
                 concluded_seq=?1 WHERE intent_id=?2 AND status='open' AND dispatch_state='active'",
                params![seq,intent_id],
            ).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn apply_review_proposal(&self, proposal: &Value) -> Result<i64, String> {
        match proposal["payload"]["marker"].as_str().unwrap_or("") {
            "REVIEW_FINDING" => self.apply_review_finding(proposal),
            "FACT_CHALLENGE" => self.apply_fact_challenge(proposal),
            "FACT_REVALIDATION" => self.apply_fact_revalidation(proposal),
            "FACT_REJECT" => self.apply_fact_reject(proposal),
            "FACT_MERGE" => self.apply_fact_merge(proposal),
            marker => Err(format!("marker is outside Review authority: {marker}")),
        }
    }

    pub fn drain_review_proposals(&self, challenge_budget: usize) -> Vec<Value> {
        let mut used = 0;
        let mut results = Vec::new();
        for proposal in self.pending_review_proposals() {
            let proposal_seq = proposal["seq"].as_i64().unwrap_or(0);
            let marker = proposal["payload"]["marker"].as_str().unwrap_or("");
            if marker == "FACT_CHALLENGE" && used >= challenge_budget {
                let reason = format!("fan-out budget exhausted ({challenge_budget}/cycle)");
                if self
                    .decide_review_proposal(proposal_seq, "deferred", &reason, None)
                    .is_ok()
                {
                    results.push(json!({
                        "proposal_seq":proposal_seq,
                        "marker":marker,
                        "decision":"deferred",
                        "reason":reason,
                        "applied_seq":Value::Null
                    }));
                }
                continue;
            }
            match self.apply_review_proposal(&proposal) {
                Ok(seq) => {
                    if marker == "FACT_CHALLENGE" {
                        used += 1;
                    }
                    results.push(json!({
                        "proposal_seq":proposal_seq,
                        "marker":marker,
                        "decision":"accepted",
                        "reason":"",
                        "applied_seq":seq
                    }));
                }
                Err(error) => {
                    if self
                        .decide_review_proposal(proposal_seq, "rejected", &error, None)
                        .is_ok()
                    {
                        results.push(json!({
                            "proposal_seq":proposal_seq,
                            "marker":marker,
                            "decision":"rejected",
                            "reason":error,
                            "applied_seq":Value::Null
                        }));
                    }
                }
            }
        }
        results
    }

    /// Closes the Review -> Verifier -> Fact loop when a Verifier intent concludes.
    /// - If the Verifier produced evidence-backed Fact(s):
    ///   automatically revalidates or supersedes/retires the challenged Fact according
    ///   to the verification outcome and updates `fact_reviews` / `fact_states`.
    /// - If the Verifier produced no valid evidence or failed:
    ///   leaves the challenge `unresolved` and records the verification outcome.
    pub fn resolve_verifier_intent(
        &self,
        verifier_worker: &str,
        intent_id: &str,
        exit_result: &str,
        reason: &str,
    ) -> Result<Option<Value>, String> {
        self.with_immediate_tx(|| {
            let challenged_fact_seq: Option<i64> = self
                .conn
                .query_row(
                    "SELECT fact_seq FROM fact_states WHERE verification_intent_id = ?1 LIMIT 1",
                    params![intent_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?
                .or_else(|| {
                    self.conn
                        .query_row(
                            "SELECT fact_seq FROM fact_reviews WHERE verification_intent_id = ?1 LIMIT 1",
                            params![intent_id],
                            |row| row.get(0),
                        )
                        .optional()
                        .ok()
                        .flatten()
                })
                .or_else(|| {
                    intent_id
                        .strip_prefix("I-verify-")
                        .and_then(|rest| rest.split('-').next())
                        .and_then(|num| num.parse::<i64>().ok())
                });

            let Some(fact_seq) = challenged_fact_seq else {
                return Ok(None);
            };

            let current_status: Option<String> = self
                .conn
                .query_row(
                    "SELECT status FROM fact_states WHERE fact_seq = ?1",
                    params![fact_seq],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if !matches!(
                current_status.as_deref(),
                Some("challenged" | "unresolved")
            ) {
                return Ok(None);
            }

            let failed_exit = matches!(
                exit_result,
                "failed" | "timeout" | "vendor_error" | "oom" | "canceled" | "blocked"
            );

            let mut produced: Vec<(i64, String, String)> = {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT e.seq, e.payload, COALESCE(e.artifact_id, '')
                         FROM events e
                         WHERE e.kind = ?1 AND e.verified = 1
                           AND (
                               json_extract(e.payload, '$.intent_id') = ?2
                               OR EXISTS (
                                   SELECT 1 FROM intent_products p
                                   WHERE p.intent_id = ?2 AND p.fact_seq = e.seq
                               )
                           )
                         ORDER BY e.seq ASC",
                    )
                    .map_err(|error| error.to_string())?;
                stmt.query_map(params![EV_FACT_ADDED, intent_id], |row| {
                    let seq: i64 = row.get(0)?;
                    let raw: String = row.get(1)?;
                    let art: String = row.get(2)?;
                    let payload: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
                    let text = payload["fact"]
                        .as_str()
                        .or_else(|| payload["summary"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    Ok((seq, text, art))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
            };

            produced.retain(|(seq, _, art)| {
                if !art.is_empty() || *seq == fact_seq {
                    return true;
                }
                self.conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM fact_evidence WHERE fact_seq = ?1)",
                        params![seq],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(false)
            });

            let now = now_secs();
            if !failed_exit && !produced.is_empty() {
                let (latest_seq, latest_text, _) = produced.last().unwrap().clone();
                let lower_text = latest_text.to_ascii_lowercase();
                let lower_reason = reason.to_ascii_lowercase();
                let refutes = matches!(exit_result, "superseded" | "rejected" | "contradicted" | "refuted")
                    || [
                        "contradict",
                        "refut",
                        "false",
                        "invalid",
                        "incorrect",
                        "wrong",
                        "disprov",
                        "supersede",
                        "replace",
                        "instead",
                        "not reproducible",
                        "ruled out",
                    ]
                    .iter()
                    .any(|kw| lower_text.contains(kw) || lower_reason.contains(kw));
                let confirms = latest_seq == fact_seq
                    || matches!(exit_result, "revalidated" | "confirmed")
                    || (!refutes
                        && [
                            "revalidat",
                            "confirm",
                            "reproduc",
                            "verified",
                            "valid",
                            "support",
                            "accurate",
                            "holds",
                        ]
                        .iter()
                        .any(|kw| lower_text.contains(kw) || lower_reason.contains(kw)));

                if confirms && !refutes {
                    let rationale = if reason.trim().is_empty() {
                        format!("Verifier {verifier_worker} confirmed Fact #{fact_seq} via {intent_id}")
                    } else {
                        reason.trim().to_owned()
                    };
                    let event_payload = json!({
                        "fact_seq": fact_seq,
                        "status": "revalidated",
                        "reason": rationale,
                        "revalidated_by": verifier_worker,
                        "verification_intent_id": intent_id,
                        "evidence_fact_seq": latest_seq,
                    });
                    let seq = self.review_event(
                        EV_FACT_REVALIDATED,
                        verifier_worker,
                        &event_payload,
                        &format!("verifier-revalidated::{fact_seq}::{intent_id}"),
                    )?;
                    self.change_fact_state_with_intent(
                        fact_seq,
                        "active",
                        "revalidated",
                        &rationale,
                        seq,
                        Some(intent_id),
                    )?;
                    return Ok(Some(json!({
                        "fact_seq": fact_seq,
                        "outcome": "revalidated",
                        "verification_intent_id": intent_id,
                        "evidence_fact_seq": latest_seq,
                        "event_seq": seq,
                    })));
                } else {
                    let by_fact_seq = (latest_seq != fact_seq).then_some(latest_seq);
                    let rationale = if reason.trim().is_empty() {
                        format!(
                            "Verifier {verifier_worker} superseded Fact #{fact_seq} with Fact #{latest_seq}"
                        )
                    } else {
                        reason.trim().to_owned()
                    };
                    let mut event_payload = json!({
                        "fact_seq": fact_seq,
                        "status": "superseded",
                        "reason": rationale,
                        "superseded_by": verifier_worker,
                        "verification_intent_id": intent_id,
                    });
                    if let Some(by_seq) = by_fact_seq {
                        event_payload["by_fact_seq"] = json!(by_seq);
                    }
                    let seq = self.review_event(
                        EV_FACT_SUPERSEDED,
                        verifier_worker,
                        &event_payload,
                        &format!("verifier-superseded::{fact_seq}::{intent_id}"),
                    )?;
                    self.conn
                        .execute(
                            "INSERT INTO fact_states
                             (fact_seq,status,superseded_by_seq,rationale,updated_by,updated_seq,updated_ts,verification_intent_id)
                             VALUES (?1,'superseded',?2,?3,?4,?5,?6,?7)
                             ON CONFLICT(fact_seq) DO UPDATE SET status='superseded',
                             superseded_by_seq=excluded.superseded_by_seq,
                             rationale=excluded.rationale,updated_by=excluded.updated_by,
                             updated_seq=excluded.updated_seq,updated_ts=excluded.updated_ts,
                             verification_intent_id=excluded.verification_intent_id",
                            params![fact_seq, by_fact_seq, rationale, verifier_worker, seq, now, intent_id],
                        )
                        .map_err(|error| error.to_string())?;
                    self.conn
                        .execute(
                            "INSERT INTO fact_reviews
                             (fact_seq,verdict,reviewer,rationale,reviewed_seq,updated_ts,verification_intent_id)
                             VALUES (?1,'superseded',?2,?3,?4,?5,?6)
                             ON CONFLICT(fact_seq) DO UPDATE SET verdict='superseded',
                             reviewer=excluded.reviewer,rationale=excluded.rationale,
                             reviewed_seq=excluded.reviewed_seq,updated_ts=excluded.updated_ts,
                             verification_intent_id=excluded.verification_intent_id",
                            params![fact_seq, verifier_worker, rationale, seq, now, intent_id],
                        )
                        .map_err(|error| error.to_string())?;
                    self.retire_rejected_fact_dependents(fact_seq)?;
                    return Ok(Some(json!({
                        "fact_seq": fact_seq,
                        "outcome": "superseded",
                        "by_fact_seq": by_fact_seq,
                        "verification_intent_id": intent_id,
                        "event_seq": seq,
                    })));
                }
            }

            let rationale = if reason.trim().is_empty() {
                format!("Verifier {intent_id} exited ({exit_result}) without conclusive evidence")
            } else {
                reason.trim().to_owned()
            };
            let event_payload = json!({
                "fact_seq": fact_seq,
                "status": "unresolved",
                "verdict": "unresolved",
                "verification_intent_id": intent_id,
                "worker": verifier_worker,
                "result": exit_result,
                "reason": rationale,
            });
            let seq = self.review_event(
                EV_FACT_REVIEWED,
                verifier_worker,
                &event_payload,
                &format!("verifier-unresolved::{fact_seq}::{intent_id}"),
            )?;
            self.change_fact_state_with_intent(
                fact_seq,
                "unresolved",
                "unresolved",
                &rationale,
                seq,
                Some(intent_id),
            )?;
            Ok(Some(json!({
                "fact_seq": fact_seq,
                "outcome": "unresolved",
                "verification_intent_id": intent_id,
                "event_seq": seq,
            })))
        })
    }

    /// Evaluates whether graph activity warrants automatically scheduling a Review Worker Step,
    /// and if so, proposes a scoped `review` intent in `SharedGraph`.
    pub fn schedule_auto_review(
        &self,
        trigger_state: &mut ReviewTriggerState,
        settings: &Value,
        has_review_seat: bool,
        active_review_workers: usize,
        max_review_capacity: usize,
    ) -> Option<AutoReviewRequest> {
        if active_review_workers >= max_review_capacity || max_review_capacity == 0 {
            return None;
        }
        let all_intents = self.all_intents();
        if all_intents.iter().any(|intent| {
            intent["worker_class"] == "review"
                && intent["dispatch_state"] == "active"
                && matches!(intent["status"].as_str(), Some("open" | "claimed"))
        }) {
            return None;
        }

        let events = self.events();
        let watermark = self.semantic_graph_watermark();
        if watermark <= trigger_state.last_reviewed_watermark {
            return None;
        }

        let explicit_request = events.iter().any(|ev| {
            let seq = ev["seq"].as_i64().unwrap_or(0);
            seq > trigger_state.last_reviewed_watermark
                && (ev["kind"] == "review_requested"
                    || (ev["kind"] == EV_OPERATOR_DIRECTIVE
                        && (ev["payload"]["target"] == "review"
                            || ev["payload"]["action"] == "review")))
        });

        let review_enabled = settings["review"]["enabled"].as_bool()
            .or_else(||settings["stage_policy"]["coordinator"]["review"]["enabled"].as_bool())
            .unwrap_or(has_review_seat || explicit_request);
        if !review_enabled {
            return None;
        }

        let cooldown_secs = settings["review"]["cooldown_seconds"].as_u64().unwrap_or(10);
        if !explicit_request
            && cooldown_secs > 0
            && trigger_state
                .last_review_dispatched_at
                .is_some_and(|last| last.elapsed() < Duration::from_secs(cooldown_secs))
        {
            return None;
        }

        let mut triggers = Vec::new();
        if explicit_request {
            triggers.push("explicit_request".to_owned());
        }

        let reviewed_seqs: HashSet<i64> = self
            .conn
            .prepare("SELECT fact_seq FROM fact_reviews")
            .ok()
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get::<_, i64>(0))
                    .ok()
                    .map(|rows| rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();

        let mut target_fact_seqs: Vec<i64> = self
            .active_facts()
            .into_iter()
            .filter(|f| f["origin"] != true)
            .filter_map(|f| f["fact_seq"].as_i64())
            .filter(|seq| {
                *seq > trigger_state.last_reviewed_watermark && !reviewed_seqs.contains(seq)
            })
            .collect();

        let candidate_fact_seqs: Vec<i64> = events
            .iter()
            .filter(|ev| {
                ev["seq"].as_i64().unwrap_or(0) > trigger_state.last_reviewed_watermark
                    && ev["kind"] == EV_FACT_ADDED
                    && ev["verified"] == false
            })
            .filter_map(|ev| ev["seq"].as_i64())
            .collect();
        for seq in &candidate_fact_seqs {
            if !target_fact_seqs.contains(seq) {
                target_fact_seqs.push(*seq);
            }
        }
        if !target_fact_seqs.is_empty() {
            triggers.push("new_facts".to_owned());
        }

        let recent_obs_count = events
            .iter()
            .filter(|ev| {
                ev["seq"].as_i64().unwrap_or(0) > trigger_state.last_reviewed_watermark
                    && matches!(ev["kind"].as_str(), Some(EV_OBSERVATION_ADDED | EV_FACT_ADDED))
            })
            .count();
        if recent_obs_count >= 3 || candidate_fact_seqs.len() >= 2 {
            triggers.push("candidate_spike".to_owned());
        }

        let mut cov_counts: HashMap<String, usize> = HashMap::new();
        let mut route_counts: HashMap<String, usize> = HashMap::new();
        let mut repeated_cov = String::new();
        let mut repeated_route = String::new();
        for intent in &all_intents {
            if matches!(intent["worker_class"].as_str(), Some("review" | "verifier")) {
                continue;
            }
            let cov = canonical_fact_whitespace(intent["coverage_key"].as_str().unwrap_or(""))
                .to_ascii_lowercase();
            if !cov.is_empty() {
                let c = cov_counts.entry(cov.clone()).or_insert(0);
                *c += 1;
                if *c >= 2 && repeated_cov.is_empty() {
                    repeated_cov = cov;
                }
            }
            let route = normalize_route_hash(intent["route_hash"].as_str().unwrap_or(""));
            if !route.is_empty() {
                let r = route_counts.entry(route.clone()).or_insert(0);
                *r += 1;
                if *r >= 2 && repeated_route.is_empty() {
                    repeated_route = route;
                }
            }
        }
        if !repeated_cov.is_empty() || !repeated_route.is_empty() {
            let new_intent_since_wm = events.iter().any(|ev| {
                ev["seq"].as_i64().unwrap_or(0) > trigger_state.last_reviewed_watermark
                    && matches!(
                        ev["kind"].as_str(),
                        Some(EV_INTENT_PROPOSED | EV_INTENT_CONCLUDED)
                    )
            });
            if new_intent_since_wm {
                triggers.push("repeated_route_or_coverage".to_owned());
            }
        }

        let fruitless_streak: usize = {
            let mut stmt = match self.conn.prepare(
                "SELECT vr.realized_value, i.worker_class
                 FROM value_receipts vr
                 LEFT JOIN intents i ON i.intent_id = vr.intent_id
                 WHERE vr.concluded_seq > ?1
                 ORDER BY vr.concluded_seq DESC",
            ) {
                Ok(s) => s,
                Err(_) => return None,
            };
            let rows: Vec<(String, String)> = stmt
                .query_map(params![trigger_state.last_reviewed_watermark], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    ))
                })
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter(|(_, wc)| !matches!(wc.as_str(), "review" | "verifier"))
                .collect();
            rows.into_iter()
                .take_while(|(val, _)| val == "none" || val == "observation_only")
                .count()
        };
        if fruitless_streak >= 2 {
            triggers.push("fruitless_streak".to_owned());
        }

        if triggers.is_empty() {
            return None;
        }

        if target_fact_seqs.is_empty() {
            target_fact_seqs = self
                .active_facts()
                .into_iter()
                .filter(|f| f["origin"] != true)
                .filter_map(|f| f["fact_seq"].as_i64())
                .collect();
        }

        let review_idx = trigger_state.next_review_seq;
        let intent_id = format!("I-review-{review_idx:03}-{watermark}");
        let facts_label = if target_fact_seqs.is_empty() {
            "current graph state and fruitless routes".to_owned()
        } else {
            format!(
                "Facts {}",
                target_fact_seqs
                    .iter()
                    .map(|s| format!("#{s}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let goal = format!(
            "Review {facts_label} (triggers: {}). Inspect original tool artifacts and propose challenge-fact, revalidate-fact, reject-fact, merge-fact, or review-finding.",
            triggers.join(", ")
        );
        let cov_key = if !repeated_cov.is_empty() {
            format!("review:{repeated_cov}:{watermark}")
        } else {
            format!("review:wm:{watermark}")
        };
        let created_seq = self.propose_intent(
            "coordinator",
            &intent_id,
            &goal,
            &json!({
                "worker_class": "review",
                "priority": "high",
                "route_hash": repeated_route,
                "coverage_key": cov_key,
                "triggers": triggers,
                "target_fact_seqs": target_fact_seqs,
                "expected_observable": "Evidence-backed review proposal (challenge-fact, revalidate-fact, reject-fact, merge-fact, or review-finding)",
                "stop_condition": "Stop once review proposals are submitted and the review Step is committed",
            }),
            if target_fact_seqs.is_empty() {
                None
            } else {
                Some(&target_fact_seqs)
            },
        );
        if created_seq <= 0 {
            return None;
        }
        trigger_state.last_reviewed_watermark = watermark;
        trigger_state.last_review_dispatched_at = Some(Instant::now());
        trigger_state.next_review_seq += 1;

        Some(AutoReviewRequest {
            intent_id,
            created_seq,
            goal,
            triggers,
            target_fact_seqs,
            route_hash: repeated_route,
            coverage_key: cov_key,
            watermark,
        })
    }
}
