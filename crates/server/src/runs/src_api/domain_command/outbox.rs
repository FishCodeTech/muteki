//! SRC domain delivery journal, kept separate from remote platform consumers.
use super::*;

const KEYS: &str = "src_domain_outbox_keys";
const OUTBOX: &str = "src_domain_outbox";
const EFFECTS: &str = "src_domain_effect_receipts";

pub(super) fn ensure(
    s: &AppState,
    command_id: &str,
    id: &str,
    kind: &str,
    accepted: &Value,
) -> Api<(Value, Value)> {
    loop {
        if let Some(key) = s.store.get(KEYS, command_id).map_err(db)? {
            let outbox_id = require(&key, "outbox_id")?;
            let effect_id = require(&key, "effect_id")?;
            let outbox = s
                .store
                .get(OUTBOX, outbox_id)
                .map_err(db)?
                .ok_or_else(|| Error::conflict("SRC outbox entry disappeared"))?;
            let effect = s
                .store
                .get(EFFECTS, effect_id)
                .map_err(db)?
                .ok_or_else(|| Error::conflict("SRC effect receipt disappeared"))?;
            return Ok((outbox, effect));
        }
        let outbox_id = format!("outbox-{}", uuid::Uuid::new_v4().simple());
        let effect_id = format!("effect-{}", uuid::Uuid::new_v4().simple());
        let stamp = now();
        let event_type = format!("{kind}.requested");
        let destination = if matches!(
            kind,
            "src.approval.decide" | "src.policy.update" | "src.grant.revoke" | "src.task.authorization.renew"
        ) {
            "src.control"
        } else if kind == "src.run.ensure" {
            "src.run.ensure"
        } else if kind == "src.run.start" {
            "src.run.start"
        } else {
            "src.domain"
        };
        let outbox = json!({"schema_version":1,"outbox_id":outbox_id,"command_id":command_id,
            "effect_id":effect_id,"correlation_id":command_id,"idempotency_key":command_id,
            "event_id":accepted["event_id"],"aggregate_type":"task","aggregate_id":id,
            "event_type":event_type,"destination":destination,"status":"pending",
            "attempts":0,"payload":{},"next_attempt_at":stamp,"last_error":null,
            "created_at":stamp,"updated_at":stamp,"delivered_at":null});
        let effect = json!({"effect_id":effect_id,"command_id":command_id,
            "acceptance_receipt_id":accepted["response"]["receipt"]["receipt_id"],
            "domain":"src","state":"accepted","aggregate":{"type":"task","id":id},
            "correlation_id":command_id,"outbox_id":outbox_id,"destination":destination,
            "idempotency_key":command_id,"attempts":0,"retry_after":null,
            "object_links":{},"output":{},"error":null,"created_at":stamp,
            "updated_at":stamp,"completed_at":null});
        let key = json!({"outbox_id":outbox_id,"effect_id":effect_id});
        if s.store
            .put_many_if(
                &[
                    (KEYS, command_id, None),
                    (OUTBOX, &outbox_id, None),
                    (EFFECTS, &effect_id, None),
                ],
                &[
                    (KEYS, command_id, &key),
                    (OUTBOX, &outbox_id, &outbox),
                    (EFFECTS, &effect_id, &effect),
                ],
            )
            .map_err(db)?
        {
            return Ok((outbox, effect));
        }
    }
}

pub(super) fn settled(outbox: &Value, effect: &Value, result: &Value) -> (Value, Value) {
    let mut next_outbox = outbox.clone();
    let mut next_effect = effect.clone();
    let stamp = now();
    let success = result["state"] == "completed";
    next_outbox["status"] = json!(if success { "delivered" } else { "failed" });
    next_outbox["updated_at"] = json!(stamp);
    next_outbox["delivered_at"] = if success { json!(stamp) } else { Value::Null };
    next_outbox["last_error"] = if success {
        Value::Null
    } else {
        result["error"]["message"].clone()
    };
    if !success {
        next_outbox["attempts"] = json!(outbox["attempts"].as_u64().unwrap_or(0) + 1);
    }
    next_outbox["next_attempt_at"] = Value::Null;
    next_effect["state"] = result["state"].clone();
    next_effect["attempts"] = json!(1);
    next_effect["output"] = result["output"].clone();
    next_effect["error"] = result["error"].clone();
    next_effect["updated_at"] = json!(stamp);
    next_effect["completed_at"] = json!(stamp);
    (next_outbox, next_effect)
}

pub(super) const fn collections() -> (&'static str, &'static str) {
    (OUTBOX, EFFECTS)
}
