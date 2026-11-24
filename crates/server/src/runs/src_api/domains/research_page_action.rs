//! Page actions and request-parameter observations from a Task-bound Exchange.
use super::*;

pub(super) fn register(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    task(s, id)?;
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_OBSERVATION_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let exchange = body.get("exchange").ok_or_else(|| {
        Error::new(
            StatusCode::BAD_REQUEST,
            "query.arguments_invalid",
            "missing required argument: exchange",
        )
    })?;
    if !exchange.is_object() || exchange["task_id"] != id {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "exchange belongs to another Task",
        ));
    }
    let observed_url = exchange["observed_url"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::new(
                StatusCode::BAD_REQUEST,
                "runtime_target_unknown",
                "page action requires an actually observed request URL",
            )
        })?;
    let action = body
        .get("action")
        .ok_or_else(|| Error::invalid("action is required"))?;
    let before_state = body
        .get("before_state")
        .ok_or_else(|| Error::invalid("before_state is required"))?;
    let after_state = body
        .get("after_state")
        .ok_or_else(|| Error::invalid("after_state is required"))?;
    let object_refs = body
        .get("object_refs")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !object_refs.is_object() {
        return Err(Error::invalid("object_refs must be an object"));
    }
    let tenant = object_refs["tenant"].clone();
    let payload = json!({"evidence_type":"Observation","action":action,
        "exchange_id":exchange["exchange_id"],"method":exchange["method"],
        "observed_url":observed_url,"identity_ref":exchange["context_id"],
        "identity_revision":exchange["identity_revision"],"tenant":tenant,
        "object_refs":object_refs,"before_state":before_state,"after_state":after_state,
        "observed_at":exchange["started_at"]});
    write_action(s, id, exchange, observed_url, payload, command_id)
}

fn write_action(
    s: &AppState,
    id: &str,
    exchange: &Value,
    observed_url: &str,
    payload: Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let record_id = format!("page-action-{}", Uuid::new_v4().simple());
    let timestamp = now();
    let record = json!({"record_id":record_id,"kind":"page_action","task_id":id,
        "revision":1,"parent_id":"","natural_key":super::super::digest(&payload),"generation":0,
        "created_at":timestamp,"updated_at":timestamp,"payload":payload});
    let mut documents = vec![(record_id.clone(), json!({"version":1,"values":record}))];
    let path = reqwest::Url::parse(observed_url)
        .map(|url| url.path().to_owned())
        .unwrap_or_default();
    for (occurrence, parameter) in research_ingest::request_parameters(s, id, exchange)
        .into_iter()
        .enumerate()
    {
        if parameter["name"].is_null() {
            continue;
        }
        let parameter_id = format!("action-parameter-{}", Uuid::new_v4().simple());
        let parameter_body = json!({"action_id":record_id,
            "exchange_id":exchange["exchange_id"],"method":exchange["method"],
            "path":path,"name":parameter["name"],"location":parameter["in"],
            "value":parameter["value"],"occurrence":occurrence,
            "identity_ref":exchange["context_id"],"tenant":record["payload"]["tenant"],
            "identity_revision":exchange["identity_revision"],"source":"runtime_request"});
        let timestamp = now();
        let child = json!({"record_id":parameter_id,"kind":"action_parameter",
            "task_id":id,"revision":1,"parent_id":record_id,
            "natural_key":parameter_id,"generation":0,"created_at":timestamp,
            "updated_at":timestamp,"payload":parameter_body});
        documents.push((parameter_id, json!({"version":1,"values":child})));
    }
    commit_action(s, &record, documents, command_id)
}

fn commit_action(
    s: &AppState,
    record: &Value,
    documents: Vec<(String, Value)>,
    command_id: Option<&str>,
) -> Api<Value> {
    s.store.document_scope(||commit_action_inner(s,record,documents,command_id)).map_err(db)?
}

fn commit_action_inner(s:&AppState,record:&Value,documents:Vec<(String,Value)>,command_id:Option<&str>)->Api<Value>{
    if let Some(command)=command_id&&let Some(marker)=s.store.get(RESEARCH_OBSERVATION_APPLIED,command).map_err(db)?{return Ok(marker["output"].clone());}
    for old in s.store.list(RECORDS).map_err(db)?{let old=&old["values"];if old["task_id"]==record["task_id"]&&old["kind"]=="page_action"&&old["payload"]==record["payload"]{return Err(Error::conflict("Page action content already exists in this Task"));}}
    let natural_key=json!([record["task_id"],"page_action",record["natural_key"]]).to_string();
    let identity=json!({"record_id":record["record_id"]});
    let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":record}));
    let mut expected = documents
        .iter()
        .map(|(key, _)| (RECORDS, key.as_str(), None))
        .collect::<Vec<_>>();
    let mut writes = documents
        .iter()
        .map(|(key, value)| (RECORDS, key.as_str(), value))
        .collect::<Vec<_>>();
    if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
        expected.push((RESEARCH_OBSERVATION_APPLIED, command_id, None));
        writes.push((RESEARCH_OBSERVATION_APPLIED, command_id, marker));
    }
    expected.push(("src_record_natural_keys",&natural_key,None));
    writes.push(("src_record_natural_keys",&natural_key,&identity));
    if s.store.put_many_if(&expected, &writes).map_err(db)? {
        return Ok(record.clone());
    }
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_OBSERVATION_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    Err(Error::conflict("SRC page action revision changed"))
}
