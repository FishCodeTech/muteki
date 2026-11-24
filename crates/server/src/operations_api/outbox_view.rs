//! Read-only Operations projection across platform and competition effect journals.
use super::*;

pub(super) struct OutboxView {
    platform: Vec<Value>,
    competition: Vec<Value>,
    src: Vec<Value>,
}

fn normalized(row: &Value, domain: &str, destination: &str) -> Value {
    json!({
        "domain": domain,
        "outbox_id": row["outbox_id"],
        "command_id": row["command_id"],
        "destination": destination,
        "status": row["status"],
        "attempts": row["attempts"].as_u64().unwrap_or(0),
        "created_at": row["created_at"],
        "updated_at": row["updated_at"],
        "delivered_at": row["delivered_at"],
        "next_attempt_at": row["next_attempt_at"],
        "last_error": row["last_error"],
    })
}

impl OutboxView {
    pub(super) fn load(state: &AppState) -> Result<Self, Failure> {
        let effects = state
            .store
            .list("platform_effect_receipts")
            .map_err(storage)?;
        let domains: std::collections::HashMap<&str, &str> = effects
            .iter()
            .filter_map(|effect| Some((effect["effect_id"].as_str()?, effect["domain"].as_str()?)))
            .collect();
        let mut platform = Vec::new();
        let mut competition = Vec::new();
        let mut src = Vec::new();
        for row in state.store.list("platform_outbox_live").map_err(storage)? {
            let destination = row["destination"].as_str().unwrap_or("");
            let domain = row["effect_id"]
                .as_str()
                .and_then(|id| domains.get(id).copied())
                .unwrap_or(
                    if matches!(destination, "competition.live" | "run.gateway") {
                        "competition"
                    } else {
                        "platform"
                    },
                );
            let item = normalized(&row, domain, destination);
            if domain == "competition" {
                competition.push(item);
            } else {
                platform.push(item);
            }
        }
        for row in state.store.list("src_domain_outbox").map_err(storage)? {
            src.push(normalized(&row, "src", row["destination"].as_str().unwrap_or("src.domain")));
        }
        let leases = state
            .store
            .list("competition_leases_live")
            .map_err(storage)?;
        let lease_connections: std::collections::HashMap<&str, &str> = leases
            .iter()
            .filter_map(|lease| {
                Some((
                    lease["lease_id"].as_str()?,
                    lease["connection_id"].as_str()?,
                ))
            })
            .collect();
        let connections = state.store.list("platform_connections").map_err(storage)?;
        let platform_kinds: std::collections::HashMap<&str, &str> = connections
            .iter()
            .filter_map(|connection| {
                Some((
                    connection["connection_id"].as_str()?,
                    connection["platform_kind"].as_str()?,
                ))
            })
            .collect();
        for row in state
            .store
            .list("competition_lease_outbox_live")
            .map_err(storage)?
        {
            let destination = row["lease_id"]
                .as_str()
                .and_then(|id| lease_connections.get(id).copied())
                .and_then(|id| platform_kinds.get(id).copied())
                .map(|kind| format!("platform.{kind}"))
                .unwrap_or_else(|| "competition.lease".to_owned());
            competition.push(normalized(&row, "competition", &destination));
        }
        for row in state
            .store
            .list("competition_submission_outbox_live")
            .map_err(storage)?
        {
            let destination = row["destination"].as_str().unwrap_or("competition.submit");
            competition.push(normalized(&row, "competition", destination));
        }
        for row in state
            .store
            .list("competition_binding_outbox_live")
            .map_err(storage)?
        {
            competition.push(normalized(&row, "competition", "run_gateway"));
        }
        for row in state.store.list("competition_connection_outbox_live").map_err(storage)? {
            competition.push(normalized(&row, "competition",
                row["destination"].as_str().unwrap_or("competition.connection")));
        }
        Ok(Self {
            platform,
            competition,
            src,
        })
    }

    pub(super) fn summary(&self, domain: &str) -> Value {
        let rows = match domain {
            "competition" => &self.competition,
            "src" => &self.src,
            _ => &self.platform,
        };
        let mut by_state = serde_json::Map::new();
        let mut oldest = 0.0_f64;
        let mut pending_count = 0_u64;
        for row in rows {
            let status = row["status"].as_str().unwrap_or("unknown");
            *by_state
                .entry(status.to_owned())
                .or_insert_with(|| json!(0)) =
                json!(by_state.get(status).and_then(Value::as_u64).unwrap_or(0) + 1);
            let active = matches!(status, "pending" | "processing" | "paused")
                || status == "failed" && domain != "src" && !row["next_attempt_at"].is_null();
            if active {
                pending_count += 1;
                let age = row["created_at"]
                    .as_str()
                    .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                    .map(|created| {
                        (chrono::Utc::now() - created.with_timezone(&chrono::Utc))
                            .num_milliseconds()
                            .max(0) as f64
                            / 1000.0
                    })
                    .unwrap_or(0.0);
                oldest = oldest.max(age);
            }
        }
        json!({"by_state":by_state,"pending_count":pending_count,"oldest_pending_seconds":(oldest*1000.0).round()/1000.0})
    }

    pub(super) fn recovery_pending(&self, domain: &str) -> usize {
        let rows = match domain {
            "competition" => &self.competition,
            "src" => &self.src,
            _ => &self.platform,
        };
        if domain == "platform" {
            return rows
                .iter()
                .filter(|row| matches!(row["status"].as_str(), Some("pending" | "processing" | "paused"))
                    || row["status"] == "failed" && !row["next_attempt_at"].is_null())
                .count();
        }
        rows.iter()
            .filter(|row| {
                row["status"] == "pending"
                    || domain != "src" && row["status"] == "failed"
                        && row["next_attempt_at"]
                            .as_str()
                            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                            .is_some_and(|at| at <= chrono::Utc::now())
            })
            .take(1000)
            .count()
    }

    pub(super) fn items(&self, limit: usize) -> Vec<Value> {
        let mut rows = self
            .platform
            .iter()
            .chain(self.competition.iter())
            .chain(self.src.iter())
            .cloned()
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| {
            b["created_at"]
                .as_str()
                .cmp(&a["created_at"].as_str())
                .then_with(|| b["outbox_id"].as_str().cmp(&a["outbox_id"].as_str()))
        });
        rows.truncate(limit.clamp(1, 1000));
        rows
    }

    pub(super) fn for_command(&self, command_id: &str) -> Vec<Value> {
        self.platform
            .iter()
            .chain(self.competition.iter())
            .chain(self.src.iter())
            .filter(|row| row["command_id"] == command_id)
            .cloned()
            .collect()
    }
}

pub(super) fn competition_receipts(state: &AppState) -> Result<Value, Failure> {
    let mut by_state = serde_json::Map::new();
    let mut pending_by_type = serde_json::Map::new();
    let mut pending = 0_u64;
    for row in state.store.list("platform_commands").map_err(storage)? {
        let kind = row["request"]["command_type"].as_str().unwrap_or("");
        if !competition_api::supports_command(kind) {
            continue;
        }
        let Some(status) = row["response"]["receipt"]["state"].as_str() else {
            continue;
        };
        let count = by_state.get(status).and_then(Value::as_u64).unwrap_or(0) + 1;
        by_state.insert(status.to_owned(), json!(count));
        if matches!(status, "accepted" | "running" | "waiting") {
            pending += 1;
            let count = pending_by_type
                .get(kind)
                .and_then(Value::as_u64)
                .unwrap_or(0)
                + 1;
            pending_by_type.insert(kind.to_owned(), json!(count));
        }
    }
    Ok(json!({"pending":pending,"by_state":by_state,"pending_by_type":pending_by_type}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn competition_projection_merges_lease_and_generic_effects() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("state.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(dir.path()))
            .unwrap();
        let state = AppState::new(store);
        let old = (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
        state
            .store
            .put(
                "platform_effect_receipts",
                "effect-c",
                &json!({"effect_id":"effect-c","domain":"competition"}),
            )
            .unwrap();
        state
            .store
            .put(
                "platform_effect_receipts",
                "effect-p",
                &json!({"effect_id":"effect-p","domain":"platform"}),
            )
            .unwrap();
        state.store.put("platform_outbox_live","generic-c",&json!({"outbox_id":"generic-c","command_id":"cmd-c","effect_id":"effect-c","destination":"competition.live","status":"failed","next_attempt_at":old,"created_at":old})).unwrap();
        state.store.put("platform_outbox_live","generic-p",&json!({"outbox_id":"generic-p","command_id":"cmd-p","effect_id":"effect-p","destination":"platform.remote","status":"delivered","created_at":old})).unwrap();
        state
            .store
            .put(
                "platform_connections",
                "conn-c",
                &json!({"connection_id":"conn-c","platform_kind":"gzctf"}),
            )
            .unwrap();
        state
            .store
            .put(
                "competition_leases_live",
                "lease-c",
                &json!({"lease_id":"lease-c","connection_id":"conn-c"}),
            )
            .unwrap();
        state.store.put("competition_lease_outbox_live","lease-outbox",&json!({"outbox_id":"lease-outbox","lease_id":"lease-c","command_id":"cmd-lease","status":"pending","attempts":0,"created_at":old})).unwrap();
        let older = (chrono::Utc::now() - chrono::Duration::hours(5)).to_rfc3339();
        state.store.put("competition_lease_outbox_live","dead-lease",&json!({"outbox_id":"dead-lease","status":"dead_letter","created_at":older})).unwrap();
        state.store.put("src_domain_outbox","failed-src",&json!({"outbox_id":"failed-src","status":"failed","next_attempt_at":old,"created_at":older})).unwrap();
        let view = OutboxView::load(&state).unwrap();
        assert_eq!(view.summary("competition")["by_state"]["pending"], 1);
        assert_eq!(view.summary("competition")["by_state"]["failed"], 1);
        assert_eq!(view.summary("platform")["by_state"]["delivered"], 1);
        assert_eq!(view.recovery_pending("competition"), 2);
        assert_eq!(view.summary("competition")["by_state"]["dead_letter"], 1);
        assert!(view.summary("competition")["oldest_pending_seconds"].as_f64().unwrap() < 300.0);
        assert_eq!(view.summary("src")["oldest_pending_seconds"], 0.0);
        assert_eq!(view.summary("src")["pending_count"], 0);
        assert_eq!(view.recovery_pending("src"), 0);
        assert_eq!(
            view.for_command("cmd-lease")[0]["destination"],
            "platform.gzctf"
        );
        assert_eq!(view.items(10).len(), 5);
    }
}
