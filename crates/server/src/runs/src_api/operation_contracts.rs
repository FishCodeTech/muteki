//! Fixed-source business argument contracts, shared by commands and queries.
use super::*;
use std::collections::HashMap;
struct Contract { validator:jsonschema::Validator, aliases:Value, schema:Value }
fn fold_auto_payload(contract:&Contract,input:&mut serde_json::Map<String,Value>){
    if contract.schema["auto_payload"]!=true||input.contains_key("payload_b64"){return;}
    let declared=contract.schema["properties"].as_object();
    let business=input.keys().filter(|key|{
        !declared.is_some_and(|properties|properties.contains_key(*key))
            && !matches!(key.as_str(),"task_id"|"command_id"|"expected_version"|"correlation_id"|"actor_id"|"requester"|"principal_id"|"created_by"|"actor_kind"|"actor"|"idempotency_key")
    }).cloned().collect::<Vec<_>>();
    if business.is_empty(){return;}
    let mut folded=serde_json::Map::new();
    for key in business{if let Some(value)=input.remove(&key){folded.insert(key,value);}}
    match input.get_mut("payload"){
        Some(Value::Object(existing))=>{for(key,value)in folded{existing.entry(key).or_insert(value);}}
        Some(_)=>{for(key,value)in folded{input.insert(key,value);}}
        None=>{input.insert("payload".into(),Value::Object(folded));}
    }
}
fn contracts()-> &'static HashMap<String,Contract>{
    static CONTRACTS:OnceLock<HashMap<String,Contract>>=OnceLock::new();
    CONTRACTS.get_or_init(||{
        let rows:Value=serde_json::from_str(include_str!("operation_contracts.json")).expect("embedded SRC contracts");
        rows.as_object().expect("contract map").iter().map(|(kind,row)|{
            let mut schema=row.clone();let aliases=schema["binary_aliases"].clone();
            if let Some(required)=schema["required"].as_array_mut(){required.retain(|name|!aliases.as_object().is_some_and(|rows|rows.values().any(|value|value==name)));}
            let validator=jsonschema::draft202012::options().build(&schema).expect("embedded SRC argument schema");
            (kind.clone(),Contract{validator,aliases,schema:row.clone()})
        }).collect()
    })
}
pub(super) fn validate(kind:&str,input:&Value,idempotency_key:Option<&str>)->Api<()>{
    let Some(contract)=contracts().get(kind)else{return Ok(())};
    let mut input=input.as_object().cloned().ok_or_else(||arguments("SRC arguments must be an object"))?;
    for key in ["task_id","command_id","expected_version","correlation_id","actor_id","requester","principal_id","created_by","actor_kind","actor"]{input.remove(key);}
    if contract.schema.get("properties").and_then(|p|p.get("idempotency_key")).is_some(){
        let key=idempotency_key.or_else(||input.get("idempotency_key").and_then(Value::as_str)).filter(|key|!key.is_empty()).ok_or_else(||arguments("idempotency_key is required"))?;
        input.insert("idempotency_key".into(),json!(key));
    }else{input.remove("idempotency_key");}
    if let Some(aliases)=contract.aliases.as_object(){
        let mut provided=std::collections::HashSet::new();
        for(alias,name)in aliases {
            let name=name.as_str().unwrap();
            if let Some(encoded)=input.get(alias){
                if input.contains_key(name)||!provided.insert(name){return Err(arguments(format!("provide one representation of {name}")));}
                let encoded=encoded.as_str().ok_or_else(||arguments(format!("{alias} must be base64 text")))?;
                use base64::Engine; base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_|arguments(format!("{alias} contains invalid base64")))?;
            }
        }
        for name in aliases.values().filter_map(Value::as_str) {
            let required=contract.schema["required"].as_array().is_some_and(|rows|rows.iter().any(|value|value==name));
            if required&&!input.contains_key(name)&&!provided.contains(name){return Err(arguments(format!("{name} or its base64 representation is required")));}
        }
    }
    fold_auto_payload(contract,&mut input);
    let value=Value::Object(input);
    if let Some(error)=contract.validator.iter_errors(&value).next(){return Err(arguments(format!("{}: {}",error.instance_path,error)));}
    Ok(())
}
fn arguments(message:impl Into<String>)->Error{Error::new(StatusCode::BAD_REQUEST,"query.arguments_invalid",message)}
pub(super) fn auto_payload(kind:&str)->bool{contracts().get(kind).is_some_and(|contract|contract.schema["auto_payload"]==true)}

pub(super) fn normalize(kind:&str,input:&Value)->Value{
    let mut value=input.clone();
    if let Some(contract)=contracts().get(kind){
        if let Some(fields)=value.as_object_mut(){fold_auto_payload(contract,fields);}
        if let Some(aliases)=contract.aliases.as_object().filter(|_|kind!="src.stream.websocket.send"){
            for(alias,name)in aliases{
                let name=name.as_str().unwrap();
                if !aliases.iter().any(|(key,target)|target.as_str()==Some(name)&&value.get(key).is_some()){
                    if let Some(text)=value.get(name).and_then(Value::as_str){use base64::Engine;value[alias]=json!(base64::engine::general_purpose::STANDARD.encode(text.as_bytes()));}
                }
            }
        }
    }
    value
}
#[cfg(test)]
mod tests{
    use super::*;
    #[test]
    fn flat_saved_query_is_folded_into_business_payload(){
        let input=json!({"name":"DVWA HTTP 端点验收","expression":"type=http_endpoint","idempotency_key":"query-once"});
        validate("src.query.save",&input,Some("query-once")).unwrap();
        let value=normalize("src.query.save",&input);
        assert_eq!(value["payload"],json!({"name":"DVWA HTTP 端点验收","expression":"type=http_endpoint"}));
        assert_eq!(value["idempotency_key"],"query-once");
        assert!(value.get("name").is_none());
        assert!(value.get("expression").is_none());
    }
}
