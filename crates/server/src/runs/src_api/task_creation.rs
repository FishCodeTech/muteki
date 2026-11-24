//! SRC Task creation through the shared command contract.
use super::*;

const PLATFORM_COMMANDS: &str = "platform_commands";
const CREATE_KEYS: &str = "src_task_create_keys";

pub(super) struct Context {
    pub request: Value,
    pub command_id: String,
    pub key: Option<String>,
    pub semantic: Value,
    pub principal: String,
    pub actor_id: String,
}

impl Context {
    pub fn replay(&self, s: &AppState) -> Api<Option<Value>> {
        if let Some(stored) = s
            .store
            .get(PLATFORM_COMMANDS, &self.command_id)
            .map_err(db)?
        {
            if stored["request"] != self.request {
                return Err(Error::conflict("command_id 已用于其他请求"));
            }
            let mut receipt = stored["response"]["receipt"].clone();
            receipt["deduplicated"] = json!(true);
            return Ok(Some(receipt));
        }
        if let Some(key) = &self.key
            && let Some(stored) = s.store.get(CREATE_KEYS, key).map_err(db)?
        {
            if stored["semantic"] != self.semantic {
                return Err(Error::conflict("idempotency_key 已用于其他请求"));
            }
            let mut receipt = stored["receipt"].clone();
            receipt["deduplicated"] = json!(true);
            return Ok(Some(receipt));
        }
        Ok(None)
    }

    pub fn stored(&self, receipt: &Value) -> Value {
        json!({"request":self.request,"response":{"receipt":receipt},"status":200})
    }

    pub fn keyed(&self, receipt: &Value) -> Value {
        json!({"command_id":self.command_id,"semantic":self.semantic,"receipt":receipt})
    }

    fn fail_commit(&self, s: &AppState, message: &str) -> Api<Value> {
        let mut receipt = receipt(&self.command_id, "", json!({}), false);
        receipt["state"] = json!("failed");
        receipt["error"] =
            json!({"code":"command.local_commit_failed","category":"internal","message":message});
        let stored = self.stored(&receipt);
        let keyed = self.keyed(&receipt);
        let mut conditions = Vec::new();
        let mut writes = Vec::new();
        self.extend(&stored, &keyed, &mut conditions, &mut writes);
        if !s.store.put_many_if(&conditions, &writes).map_err(db)? {
            return self
                .replay(s)?
                .ok_or_else(|| Error::conflict("SRC Task creation conflicted"));
        }
        Ok(receipt)
    }

    pub fn extend<'a>(
        &'a self,
        stored: &'a Value,
        keyed: &'a Value,
        conditions: &mut Vec<(&'a str, &'a str, Option<&'a Value>)>,
        writes: &mut Vec<(&'a str, &'a str, &'a Value)>,
    ) {
        conditions.push((PLATFORM_COMMANDS, &self.command_id, None));
        writes.push((PLATFORM_COMMANDS, &self.command_id, stored));
        if let Some(key) = &self.key {
            conditions.push((CREATE_KEYS, key, None));
            writes.push((CREATE_KEYS, key, keyed));
        }
    }
}

pub(super) async fn execute(s: Arc<AppState>, envelope: &Value, principal: &str) -> Response {
    let result = async {
        let mut request = envelope.clone();
        if request.get("payload").is_none() {
            request["payload"] = json!({});
        }
        if !request["payload"].is_object() {
            return Err(Error::invalid("payload must be an object"));
        }
        if request["command_id"].as_str().is_none_or(str::is_empty) {
            request["command_id"] = json!(format!("cmd-{}", uuid::Uuid::new_v4().simple()));
        }
        let mut semantic = request.clone();
        semantic.as_object_mut().unwrap().remove("command_id");
        semantic.as_object_mut().unwrap().remove("idempotency_key");
        let context = Context {
            command_id: request["command_id"].as_str().unwrap().to_owned(),
            key: request["idempotency_key"]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(|key| json!([principal, key]).to_string()),
            semantic,
            principal: principal.to_owned(),
            actor_id: principal
                .split_once(':')
                .map(|(_, id)| id)
                .unwrap_or(principal)
                .to_owned(),
            request,
        };
        if let Some(receipt) = context.replay(&s)? {
            return Ok(json!({"receipt":receipt}));
        }
        if !context.request["expected_version"].is_null() {
            let receipt = context.fail_commit(
                &s,
                "expected_version requires events from exactly one aggregate stream",
            )?;
            return Ok(json!({"receipt":receipt}));
        }
        let receipt =
            match create_task_transaction(&s, &context.request["payload"], Some(&context)).await {
                Ok(receipt) => receipt,
                Err(error) if error.status.is_server_error() => {
                    context.fail_commit(&s, &error.message)?
                }
                Err(error) => return Err(error),
            };
        Ok(json!({"receipt":receipt}))
    }
    .await;
    respond(result)
}
