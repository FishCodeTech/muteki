//! Role-scoped Review projection of shared FGS evidence.
use super::*;
fn t(v: &Value) -> String {
    match v {
        Value::String(v) => v.clone(),
        Value::Number(v) => v.to_string(),
        _ => String::new(),
    }
}
impl SharedGraph {
    pub fn review_projection(&self, intent: &str, directive: &str) -> Result<String, String> {
        let events = self.events();
        let proposal = events
            .iter()
            .find(|e| e["kind"] == EV_INTENT_PROPOSED && e["payload"]["intent_id"] == intent);
        let explicit = proposal
            .map(|e| &e["payload"]["target_fact_seqs"])
            .unwrap_or(&Value::Null);
        let wanted: HashSet<i64> = explicit
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect();
        let since = proposal
            .and_then(|e| e["payload"]["since_seq"].as_i64())
            .unwrap_or(0);
        let states = self.fact_states_map();
        let rows: Vec<_> = events
            .iter()
            .rev()
            .filter(|e| {
                matches!(e["kind"].as_str(), Some(EV_FACT_ADDED | EV_FACT_ORIGIN))
                    && e["seq"].as_i64().is_some_and(|seq| {
                        if wanted.is_empty() {
                            seq > since
                        } else {
                            wanted.contains(&seq)
                        }
                    })
            })
            .collect();
        let scope: HashSet<i64> = rows.iter().filter_map(|e| e["seq"].as_i64()).collect();
        let mut lines = vec![
            if directive.trim().is_empty() {
                "# Review scope".to_owned()
            } else {
                format!("# Review scope — trigger: {}", directive.trim())
            },
            "\n## Facts under review".into(),
        ];
        let mut producers = HashSet::new();
        for event in &rows {
            let seq = event["seq"].as_i64().unwrap();
            let p = &event["payload"];
            let state = states.get(&seq).map(String::as_str).unwrap_or("active");
            let lifecycle = if state == "active" {
                "unresolved"
            } else {
                state
            };
            let verified = event["verified"] == true && state != "challenged";
            let mut verdict = if verified { "verified" } else { "candidate" }.to_owned();
            if lifecycle != "unresolved" {
                verdict.push_str(&format!("/{lifecycle}"));
            }
            lines.push(format!(
                "- [#{seq}] ({verdict}, confidence={:.2}) {}",
                event["confidence"].as_f64().unwrap_or(0.),
                t(&p["fact"])
            ));
            let prov = &p["evidence_provenance"];
            let mut bits = Vec::new();
            for (key, values) in [
                (
                    "worker",
                    vec![&prov["worker_id"], &p["source_solver"], &event["actor"]],
                ),
                ("intent", vec![&prov["intent_id"], &p["intent_id"]]),
                ("epoch", vec![&prov["target_epoch"], &p["target_epoch"]]),
                (
                    "artifact",
                    vec![&prov["artifact_id"], &event["artifact_id"]],
                ),
                ("tool_event", vec![&prov["tool_event_id"]]),
            ] {
                if let Some(value) = values.into_iter().map(t).find(|s| !s.trim().is_empty()) {
                    bits.push(format!("{key}={value}"));
                }
            }
            if !bits.is_empty() {
                lines.push(format!("  provenance: {}", bits.join(" ")));
            }
            if let Some(id) = p["intent_id"].as_str().filter(|s| !s.is_empty()) {
                producers.insert(id.to_owned());
            }
            for observation in events
                .iter()
                .filter(|e| e["kind"] == "fact_observed" && e["payload"]["fact_seq"] == seq)
            {
                lines.push(format!(
                    "  - obs [#{}] by {} at {:.0}",
                    t(&observation["seq"]),
                    t(&observation["actor"]),
                    observation["ts"].as_f64().unwrap_or(0.)
                ));
            }
        }
        if rows.is_empty() {
            lines.push("- (no facts in scope)".into());
        }
        for (id, products) in self.intent_products()? {
            if products.iter().any(|seq| scope.contains(seq)) {
                producers.insert(id);
            }
        }
        let related: Vec<_> = self
            .all_intents()
            .into_iter()
            .filter(|i| {
                i["intent_id"]
                    .as_str()
                    .is_some_and(|id| producers.contains(id))
            })
            .collect();
        if !related.is_empty() {
            lines.push("\n## Related intents".into());
        }
        for row in &related {
            let mut meta = Vec::new();
            for (key, field) in [("route", "route_hash"), ("coverage", "coverage_key")] {
                if !t(&row[field]).is_empty() {
                    meta.push(format!("{key}={}", t(&row[field])));
                }
            }
            let suffix = if meta.is_empty() {
                String::new()
            } else {
                format!(" ({})", meta.join(", "))
            };
            let result = t(&row["result"]);
            let tail = if result.is_empty() {
                String::new()
            } else {
                format!(" → {result}")
            };
            lines.push(format!(
                "- {} [{}]: {}{suffix}{tail}",
                t(&row["intent_id"]),
                t(&row["status"]),
                t(&row["goal"])
            ));
        }
        for (statuses, title) in [
            (
                vec!["challenged"],
                "\n## Challenged facts (DO NOT treat as verified until revalidated)",
            ),
            (vec!["revalidated"], "\n## Revalidated facts"),
            (
                vec!["rejected", "merged", "superseded"],
                "\n## Retired facts (rejected/merged/superseded — do NOT use as evidence)",
            ),
        ] {
            let selected: Vec<_> = events
                .iter()
                .filter(|e| {
                    matches!(e["kind"].as_str(), Some(EV_FACT_ADDED | EV_FACT_ORIGIN))
                        && e["seq"]
                            .as_i64()
                            .and_then(|seq| states.get(&seq))
                            .is_some_and(|s| statuses.contains(&s.as_str()))
                })
                .collect();
            if !selected.is_empty() {
                lines.push(title.into());
            }
            for event in selected {
                let seq = event["seq"].as_i64().unwrap();
                let status = states[&seq].as_str();
                let update = events
                    .iter()
                    .rev()
                    .find(|e| {
                        e["payload"]["fact_seq"] == seq || e["payload"]["from_fact_seq"] == seq
                    })
                    .map(|e| &e["payload"])
                    .unwrap_or(&Value::Null);
                let fact = t(&event["payload"]["fact"]);
                let reason = t(&update["reason"]);
                lines.push(if status == "challenged" {
                    format!(
                        "- [#{seq}] {fact} :: {reason} (verify via {})",
                        t(&update["verification_intent_id"])
                    )
                } else if status == "revalidated" {
                    format!("- [#{seq}] {fact} :: {reason}")
                } else {
                    let tag = if status == "merged" && !update["to_fact_seq"].is_null() {
                        format!("merged→#{}", t(&update["to_fact_seq"]))
                    } else {
                        status.to_owned()
                    };
                    format!("- [#{seq}] ({tag}) {fact} :: {reason}")
                });
            }
        }
        let mut dead: Vec<_> = self
            .dead_ends()
            .into_iter()
            .filter(|d| {
                related.iter().any(|i| {
                    d["intent_id"] == i["intent_id"]
                        || !t(&d["coverage_key"]).is_empty()
                            && d["coverage_key"] == i["coverage_key"]
                        || !t(&d["route_hash"]).is_empty() && d["route_hash"] == i["route_hash"]
                })
            })
            .collect();
        dead.sort_by_key(|d| std::cmp::Reverse(d["seq"].as_i64().unwrap_or(0)));
        if !dead.is_empty() {
            lines.push("\n## Relevant dead ends".into());
        }
        for row in dead {
            let mut tags = Vec::new();
            for (key, field) in [
                ("intent", "intent_id"),
                ("route", "route_hash"),
                ("coverage", "coverage_key"),
                ("epoch", "target_epoch"),
            ] {
                if !t(&row[field]).is_empty() {
                    tags.push(format!("{key}={}", t(&row[field])));
                }
            }
            let suffix = if tags.is_empty() {
                String::new()
            } else {
                format!(" ({})", tags.join(", "))
            };
            let mut detail = t(&row["reason"]);
            for (key, field) in [("tested", "tested_scope"), ("observed", "observed_result")] {
                if !t(&row[field]).is_empty() {
                    detail.push_str(&format!(" | {key}: {}", t(&row[field])));
                }
            }
            lines.push(format!("- {detail}{suffix}"));
        }
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_keeps_full_scoped_facts_and_their_producers() {
        let dir = tempfile::tempdir().unwrap();
        let graph = SharedGraph::open_at(dir.path().join("graph.db")).unwrap();
        for (id, worker) in [("step-a", "worker-a"), ("step-b", "worker-b")] {
            graph.propose_intent("reason", id, id, &json!({}), None);
            assert!(graph.claim_intent(worker, id, 600.));
        }
        let (_, first) = graph
            .add_evidence_checked(
                "worker-a",
                "A",
                "A\nComplete evidence line one\nComplete evidence line two",
                None,
                true,
                1.,
                Some("step-a"),
                None,
                Some(&json!({"mode":"ctf","target_epoch":1})),
            )
            .unwrap();
        graph
            .add_evidence_checked(
                "worker-b",
                "B",
                "Unrelated evidence B",
                None,
                true,
                1.,
                Some("step-b"),
                None,
                Some(&json!({"mode":"ctf","target_epoch":1})),
            )
            .unwrap();
        graph.propose_intent(
            "reason",
            "review-a",
            "Audit A",
            &json!({"worker_class":"review","target_fact_seqs":[first.unwrap()]}),
            None,
        );
        let rendered = graph.review_projection("review-a", "Audit A").unwrap();
        assert!(rendered.contains("Complete evidence line one\nComplete evidence line two"));
        assert!(rendered.contains("step-a [claimed]"));
        assert!(!rendered.contains("Unrelated evidence B"));
        assert!(!rendered.contains("step-b [claimed]"));
    }
}
