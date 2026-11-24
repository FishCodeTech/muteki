//! Scoped online queries for the source connector catalog.
use super::{
    Api, Error, connector_config, connectors_online, credential_store, now, policy,
    put_artifact, rec, record_exchange, require, save_exchange_draft, save_rec,
};
use crate::AppState;
use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

struct PageSpec {
    method: &'static str,
    url: String,
    body: Vec<u8>,
    bindings: Value,
}

fn binding(settings: &Value, field: &str, location: &str, name: &str, format: &str) -> Api<Value> {
    let reference = require(settings, field)?;
    Ok(json!({"location":location,"name":name,"format":format,"reference":reference}))
}

#[allow(dead_code)]
fn page_spec(provider: &str, settings: &Value, query: &str, position: u64) -> Api<PageSpec> {
    page_spec_with_extra(provider, settings, query, position, &Value::Null)
}

fn page_spec_with_extra(
    provider: &str,
    settings: &Value,
    query: &str,
    position: u64,
    extra: &Value,
) -> Api<PageSpec> {
    let size = settings["page_size"]
        .as_u64()
        .or_else(|| settings["max_size"].as_u64())
        .filter(|v| *v > 0)
        .unwrap_or(100);
    let base_override = |default: &str| -> String {
        settings["base_url"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(default)
            .trim_end_matches('/')
            .to_owned()
    };
    let (method, mut url, body, bindings) = match provider {
        "fofa" => {
            let mut url = reqwest::Url::parse(&format!("{}/api/v1/search/all", base_override("https://fofa.info")))
                .map_err(|_| Error::invalid("fofa base_url invalid"))?;
            let fields = settings["fields"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "host,ip,port,protocol,title,domain,icp".into());
            url.query_pairs_mut()
                .append_pair(
                    "qbase64",
                    &base64::engine::general_purpose::STANDARD.encode(query),
                )
                .append_pair("page", &position.to_string())
                .append_pair("size", &size.to_string())
                .append_pair("fields", &fields);
            (
                "GET",
                url,
                Vec::new(),
                json!([
                    binding(settings, "email_ref", "query", "email", "raw")?,
                    binding(settings, "key_ref", "query", "key", "raw")?
                ]),
            )
        }
        "quake" => {
            let url = reqwest::Url::parse(&format!(
                "{}/api/v3/search/quake_service",
                base_override("https://quake.360.net")
            ))
            .map_err(|_| Error::invalid("quake base_url invalid"))?;
            let body = serde_json::to_vec(&json!({"query":query,"start":position,"size":size}))
                .map_err(crate::runs::src_api::db)?;
            (
                "POST",
                url,
                body,
                json!([binding(
                    settings,
                    "token_ref",
                    "header",
                    "X-QuakeToken",
                    "raw"
                )?]),
            )
        }
        "shodan" => {
            let mut url = reqwest::Url::parse(&format!(
                "{}/shodan/host/search",
                base_override("https://api.shodan.io")
            ))
            .map_err(|_| Error::invalid("shodan base_url invalid"))?;
            url.query_pairs_mut()
                .append_pair("query", query)
                .append_pair("page", &position.to_string());
            (
                "GET",
                url,
                Vec::new(),
                json!([binding(settings, "key_ref", "query", "key", "raw")?]),
            )
        }
        "zoomeye" => {
            let mut url = reqwest::Url::parse(&format!(
                "{}/host/search",
                base_override("https://api.zoomeye.org")
            ))
            .map_err(|_| Error::invalid("zoomeye base_url invalid"))?;
            url.query_pairs_mut()
                .append_pair("query", query)
                .append_pair("page", &position.to_string());
            (
                "GET",
                url,
                Vec::new(),
                json!([binding(settings, "key_ref", "header", "API-KEY", "raw")?]),
            )
        }
        "scopesentry" => {
            if settings["query_enabled"] != true
                || !settings["endpoints"]
                    .as_object()
                    .is_some_and(|m| !m.is_empty())
            {
                return Err(Error::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "capability.unavailable",
                    "scopesentry does not support online query with this configuration",
                ));
            }
            let asset_type = extra["asset_type"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or("domain");
            let path = settings["endpoints"][asset_type]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    Error::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "capability.unavailable",
                        format!("no verified ScopeSentry endpoint for {asset_type}"),
                    )
                })?;
            let base = settings["base_url"]
                .as_str()
                .map(|v| v.trim().trim_end_matches('/'))
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    Error::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "capability.unavailable",
                        "ScopeSentry base_url is required for online query",
                    )
                })?;
            let full = if path.starts_with('/') {
                format!("{base}{path}")
            } else {
                format!("{base}/{path}")
            };
            let mut url = reqwest::Url::parse(&full)
                .map_err(|_| Error::invalid("ScopeSentry endpoint URL is invalid"))?;
            {
                let mut pairs = url.query_pairs_mut();
                if let Some(pid) = extra.get("project_id").filter(|v| !v.is_null()) {
                    let text = pid
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| pid.to_string());
                    if !text.is_empty() {
                        pairs.append_pair("project_id", &text);
                    }
                }
                if let Some(tid) = extra.get("source_task_id").filter(|v| !v.is_null()) {
                    let text = tid
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| tid.to_string());
                    if !text.is_empty() {
                        pairs.append_pair("task_id", &text);
                    }
                }
                pairs
                    .append_pair("page", &position.to_string())
                    .append_pair("size", &size.to_string());
            }
            let bindings = if let Some(reference) = settings["token_ref"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                json!([{"location":"header","name":"Authorization","format":"bearer","reference":reference}])
            } else {
                json!([])
            };
            ("GET", url, Vec::new(), bindings)
        }
        _ => {
            return Err(Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "capability.unavailable",
                format!("{provider} has no scoped online query adapter"),
            ));
        }
    };
    url.set_fragment(None);
    Ok(PageSpec {
        method,
        url: url.to_string(),
        body,
        bindings,
    })
}

fn draft(s: &AppState, id: &str, spec: &PageSpec) -> Api<String> {
    let url = reqwest::Url::parse(&spec.url).map_err(crate::runs::src_api::db)?;
    let host = url
        .host_str()
        .ok_or_else(|| Error::invalid("connector URL host missing"))?;
    let authority = if let Some(port) = url.port() {
        format!("{host}:{port}")
    } else {
        host.to_owned()
    };
    let target = if let Some(query) = url.query() {
        format!("{}?{query}", url.path())
    } else {
        url.path().to_owned()
    };
    let mut raw=format!("{} {} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nUser-Agent: Muteki-SRC/0.4.8\r\n",spec.method,target,authority).into_bytes();
    if !spec.body.is_empty() {
        raw.extend_from_slice(
            format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                spec.body.len()
            )
            .as_bytes(),
        );
    }
    raw.extend_from_slice(b"\r\n");
    raw.extend_from_slice(&spec.body);
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    let exchange = record_exchange(
        s,
        id,
        &json!({"request_raw_b64":encoded,"observed_url":spec.url,
        "source_kind":"import","execution_state":"created"}),
    )?;
    let source = require(&exchange, "exchange_id")?;
    let saved = save_exchange_draft(
        s,
        id,
        &json!({"source_exchange_id":source,"base_revision":1,
        "raw_request_b64":encoded,"editor":"connector:query"}),
    )?;
    Ok(require(&saved["draft"], "draft_id")?.to_owned())
}

fn upstream_error_message(document: &Value) -> Option<String> {
    if !document.is_object() {
        return None;
    }
    let has_error_flag = document["error"] == true;
    let errmsg = document["errmsg"]
        .as_str()
        .or_else(|| document["error_message"].as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let non_ok_code = document["code"].as_i64().filter(|&c| c != 0 && c != 200);
    let non_ok_str_code = document["code"]
        .as_str()
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != "0" && *c != "200" && !c.eq_ignore_ascii_case("ok") && !c.eq_ignore_ascii_case("success"));
    let http_err = document["status_code"].as_u64().filter(|&c| c >= 400);
    if has_error_flag || errmsg.is_some() || non_ok_code.is_some() || non_ok_str_code.is_some() || http_err.is_some() {
        let msg = errmsg
            .or_else(|| document["message"].as_str().map(str::trim).filter(|v| !v.is_empty()))
            .or_else(|| document["msg"].as_str().map(str::trim).filter(|v| !v.is_empty()))
            .or_else(|| document["error"].as_str().map(str::trim).filter(|v| !v.is_empty()))
            .unwrap_or("connector reported an upstream error");
        if let Some(code) = non_ok_code {
            return Some(format!("[{code}] {msg}"));
        }
        if let Some(code) = non_ok_str_code {
            return Some(format!("[{code}] {msg}"));
        }
        if let Some(code) = http_err {
            return Some(format!("HTTP {code}: {msg}"));
        }
        return Some(msg.to_owned());
    }
    None
}

fn parse_page(
    provider: &str,
    settings: &Value,
    document: &Value,
    position: u64,
) -> Api<(Vec<Value>, Option<u64>, Option<u64>, Value)> {
    let size = if provider == "shodan" {
        100
    } else {
        settings["page_size"]
            .as_u64()
            .or_else(|| settings["max_size"].as_u64())
            .filter(|v| *v > 0)
            .unwrap_or(100)
    };
    if let Some(err_msg) = upstream_error_message(document) {
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "src.connector_upstream_error",
            err_msg,
        ));
    }
    if provider == "scopesentry" {
        let data_node = if document.is_object() {
            document.get("data").filter(|v| !v.is_null()).unwrap_or(document)
        } else {
            document
        };
        let rows_vec: Vec<Value> = if let Some(arr) = data_node.as_array() {
            arr.clone()
        } else if data_node.is_object() {
            ["list", "records", "items"]
                .iter()
                .find_map(|k| data_node.get(*k).and_then(Value::as_array))
                .cloned()
                .unwrap_or_default()
        } else {
            return Err(Error::new(
                StatusCode::BAD_GATEWAY,
                "src.connector_upstream_invalid",
                "ScopeSentry response data is not an object or array",
            ));
        };
        let total = if data_node.is_object() {
            data_node["total"]
                .as_u64()
                .or_else(|| document["total"].as_u64())
        } else {
            document["total"].as_u64()
        };
        let items = rows_vec
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let id_part = row["id"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .or_else(|| row["id"].as_u64().map(|v| v.to_string()))
                    .unwrap_or_else(|| {
                        format!(
                            "{position}:{index}:{}",
                            &crate::runs::src_api::digest(row)[..24]
                        )
                    });
                json!({
                    "input_key": format!("scopesentry:{id_part}"),
                    "mapped": row,
                    "raw": row,
                })
            })
            .collect::<Vec<_>>();
        let next = if rows_vec.is_empty() {
            None
        } else {
            let page = position + 1;
            total.filter(|total| position * size < *total).map(|_| page)
        };
        return Ok((items, next, total, json!({})));
    }
    if !document.is_object() {
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "src.connector_upstream_invalid",
            "connector response is not an object",
        ));
    }
    if let Some(err_msg) = upstream_error_message(document) {
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "src.connector_upstream_error",
            err_msg,
        ));
    }
    let key = match provider {
        "fofa" => "results",
        "quake" | "zoomeye" => "data",
        "shodan" => "matches",
        _ => "",
    };
    let rows = document[key].as_array().ok_or_else(|| {
        Error::new(
            StatusCode::BAD_GATEWAY,
            "src.connector_upstream_invalid",
            "connector result array is missing",
        )
    })?;
    let total = if provider == "quake" {
        document["meta"]["pagination"]["total"].as_u64()
    } else if provider == "fofa" {
        document["size"].as_u64()
    } else {
        document["total"].as_u64()
    };
    let fields = settings["fields"]
        .as_array()
        .map(|v| v.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["host", "ip", "port", "protocol", "title", "domain", "icp"]);
    let items=rows.iter().enumerate().map(|(index,row)|{
        let mapped=if provider=="fofa" && row.is_array(){
            let mut object=serde_json::Map::new();
            for (name,value) in fields.iter().zip(row.as_array().unwrap()) { object.insert((*name).to_owned(),value.clone()); }
            Value::Object(object)
        } else {row.clone()};
        json!({"input_key":format!("{provider}:{position}:{index}:{}", &crate::runs::src_api::digest(row)[..24]),
            "mapped":mapped,"raw":row})
    }).collect::<Vec<_>>();
    let next = if rows.is_empty() {
        None
    } else if provider == "quake" {
        let start = position + rows.len() as u64;
        total.filter(|total| start < *total).map(|_| start)
    } else {
        let page = position + 1;
        total.filter(|total| position * size < *total).map(|_| page)
    };
    let mut quota = document.clone();
    quota.as_object_mut().unwrap().remove(key);
    Ok((items, next, total, quota))
}

async fn capture_output(command:&mut Command,timeout:Duration,output_limit:usize,error_limit:usize)->Api<std::process::Output>{
    use tokio::io::AsyncReadExt;
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let mut child=command.spawn().map_err(|error|Error::new(StatusCode::SERVICE_UNAVAILABLE,"capability.unavailable",format!("AWS CLI launch failed: {error}")))?;
    let stdout=child.stdout.take().ok_or_else(||Error::invalid("AWS stdout unavailable"))?;let stderr=child.stderr.take().ok_or_else(||Error::invalid("AWS stderr unavailable"))?;
    async fn drain(mut reader:impl tokio::io::AsyncRead+Unpin,limit:usize)->std::io::Result<(Vec<u8>,bool)>{let mut output=Vec::new();let mut exceeded=false;let mut chunk=[0u8;8192];loop{let n=reader.read(&mut chunk).await?;if n==0{break}let available=limit.saturating_sub(output.len());output.extend_from_slice(&chunk[..n.min(available)]);exceeded|=n>available;}Ok((output,exceeded))}
    let outcome=tokio::time::timeout(timeout,async{let(status,out,err)=tokio::try_join!(child.wait(),drain(stdout,output_limit),drain(stderr,error_limit))?;Ok::<_,std::io::Error>((status,out,err))}).await;
    match outcome{Ok(Ok((status,(stdout,out_limit),(stderr,err_limit))))=>{if out_limit||err_limit{return Err(Error::new(StatusCode::BAD_GATEWAY,"src.connector_capture_limit","AWS CLI exceeded capture limit"))}Ok(std::process::Output{status,stdout,stderr})},other=>{let _=child.kill().await;let _=child.wait().await;Err(Error::new(StatusCode::GATEWAY_TIMEOUT,"external_outcome_unknown",if other.is_err(){"AWS CLI timed out"}else{"AWS CLI capture failed"}))}}
}
async fn aws_ec2(s: Arc<AppState>, id: &str, body: &Value, config: &Value) -> Api<Value> {
    let settings = &config["config"];
    let region = require(settings, "region")?;
    let domain = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    let endpoint = settings["endpoint_url"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("https://ec2.{region}.{domain}/"));
    let p = policy(&s, id)?;
    if p["excluded_targets"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|v| v.as_str().is_some_and(|ex| crate::runs::src_api::target_matches(&endpoint, ex)))
    }) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "AWS EC2 endpoint is in excluded_targets",
        ));
    }
    super::connector_config::enforce_grant_for_purpose(&s, id, "aws_ec2", "query", true)?;
    let mut version=Command::from(agent::supervised_command("aws"));version.arg("--version");
    if !capture_output(&mut version,Duration::from_secs(10),65536,65536).await?.status.success(){return Err(Error::new(StatusCode::SERVICE_UNAVAILABLE,"capability.unavailable","AWS CLI version discovery failed"));}
    let credentials = if let Some(reference) = settings["credential_ref"]
        .as_str()
        .filter(|v| !v.is_empty())
    {
        let secret = credential_store::resolve(&s, id, reference, None)?;
        let parsed: Value = serde_json::from_str(&secret).map_err(|_| {
            Error::invalid("AWS credential reference must contain a JSON environment object")
        })?;
        parsed.as_object().cloned().ok_or_else(|| {
            Error::invalid("AWS credential reference must contain a JSON environment object")
        })?
    } else {
        serde_json::Map::new()
    };
    let key = body["idempotency_key"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let acquisition_id = format!(
        "sconnacq_{}",
        &crate::runs::src_api::digest(&json!([id, "aws_ec2", key]))[..24]
    );
    let batch_id = format!(
        "sconnbatch_{}",
        &crate::runs::src_api::digest(&json!([id, "aws_ec2", key]))[..24]
    );
    let request_digest = crate::runs::src_api::digest(&json!([
        region,
        config["version"],
        body["max_pages"],
        body["max_items"]
    ]));
    if let Ok(row) = rec(&s, id, "connector_batch", &batch_id) {
        if row["payload"]["request_digest"] != request_digest {
            return Err(Error::conflict(
                "connector idempotency key used for different input",
            ));
        }
        let mut output = row["payload"].clone();
        output["deduplicated"] = json!(true);
        return Ok(output);
    }
    if let Ok(row) = rec(&s, id, "connector_acquisition", &acquisition_id) {
        if row["payload"]["request_digest"] != request_digest {
            return Err(Error::conflict(
                "connector idempotency key used for different input",
            ));
        }
    } else {
        save_rec(
            &s,
            id,
            "connector_acquisition",
            &acquisition_id,
            json!({"acquisition_id":acquisition_id,"connector_id":"aws_ec2",
                "config_version":config["version"],"request_digest":request_digest}),
            Some(0),
        )?;
    }
    let max_pages = body["max_pages"]
        .as_u64()
        .or_else(|| settings["max_pages"].as_u64())
        .unwrap_or(10)
        .clamp(1, 100);
    let max_items = body["max_items"]
        .as_u64()
        .or_else(|| settings["max_items"].as_u64())
        .unwrap_or(10_000)
        .max(1) as usize;
    let page_size = settings["page_size"]
        .as_u64()
        .filter(|v| *v > 0)
        .unwrap_or(100);
    let mut items = Vec::<Value>::new();
    let mut source_refs = Vec::<Value>::new();
    let mut next_token = String::new();
    let mut pages = 0;
    let mut errors=Vec::new();
    while pages<max_pages&&items.len()<max_items{
        pages+=1;let call_key=format!("{acquisition_id}:{pages}");
        let page:Api<(Vec<Value>,String,Value)>=async{
            if let Ok(row)=rec(&s,id,"connector_aws_call",&call_key){
                if row["payload"]["state"]!="completed"{return Err(Error::new(StatusCode::BAD_GATEWAY,"external_outcome_unknown","previous AWS call has no completed local receipt"));}
                let reference=row["payload"]["source_ref"].clone();let artifact=super::verify_ref(&s,id,&reference)?;
                let bytes=base64::engine::general_purpose::STANDARD.decode(artifact["data"].as_str().unwrap_or("")).map_err(|_|Error::invalid("AWS source artifact invalid"))?;
                let document:Value=serde_json::from_slice(&bytes).map_err(|_|Error::invalid("AWS source JSON invalid"))?;let(mut rows,next)=aws_items(&document,region)?;
                assign_account_tenant(&mut rows,&settings["account_id"]);return Ok((rows,next,reference));
            }
            save_rec(&s,id,"connector_aws_call",&call_key,json!({"state":"dispatched","region":region,"next_token_present":!next_token.is_empty(),"credential_ref":settings["credential_ref"]}),Some(0))?;
            let mut cmd=Command::from(agent::supervised_command("aws"));cmd.arg("ec2").arg("describe-instances").arg("--region").arg(region).arg("--endpoint-url").arg(&endpoint).arg("--output").arg("json").arg("--max-items").arg(page_size.to_string()).env("AWS_PAGER","");
            if let Some(profile)=settings["profile"].as_str().filter(|v|!v.is_empty()){cmd.arg("--profile").arg(profile);}
            if !next_token.is_empty(){cmd.arg("--starting-token").arg(&next_token);}
            for(name,value)in &credentials{cmd.env(name,value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string()));}
            let timeout=settings["timeout"].as_u64().filter(|v|*v>0).unwrap_or(45);
            let output=capture_output(&mut cmd,Duration::from_secs(timeout),32*1024*1024,1024*1024).await?;
            let bytes=output.stdout;
            let reference=put_artifact(&s,id,&bytes,&format!("aws-ec2-{call_key}.json"),"application/json")?;
            if !output.status.success(){let row=rec(&s,id,"connector_aws_call",&call_key)?;save_rec(&s,id,"connector_aws_call",&call_key,json!({"state":"failed","source_ref":reference,"error_code":"src.connector_upstream_failed"}),row["revision"].as_u64())?;return Err(Error::new(StatusCode::BAD_GATEWAY,"src.connector_upstream_failed","AWS CLI describe-instances failed"));}
            let row=rec(&s,id,"connector_aws_call",&call_key)?;save_rec(&s,id,"connector_aws_call",&call_key,json!({"state":"completed","region":region,"source_ref":reference,"credential_ref":settings["credential_ref"]}),row["revision"].as_u64())?;
            let document:Value=serde_json::from_slice(&bytes).map_err(|_|Error::new(StatusCode::BAD_GATEWAY,"src.connector_upstream_invalid","AWS CLI response is not JSON"))?;
            let(mut rows,next)=aws_items(&document,region)?;assign_account_tenant(&mut rows,&settings["account_id"]);Ok((rows,next,reference))
        }.await;
        match page{Ok((rows,next,reference))=>{items.extend(rows);source_refs.push(reference);next_token=next;if next_token.is_empty(){break;}},Err(error)=>{if let Ok(row)=rec(&s,id,"connector_aws_call",&call_key){if row["payload"]["source_ref"].is_object(){source_refs.push(row["payload"]["source_ref"].clone());}}errors.push(json!({"page":pages,"code":error.code,"message":error.message}));break;}}
    }
    let truncated = !next_token.is_empty() || items.len() > max_items;
    items.truncate(max_items);
    let batch_status = if !errors.is_empty() && items.is_empty() {
        "failed"
    } else if !errors.is_empty() || truncated {
        "partial"
    } else {
        "completed"
    };
    let outcome = if batch_status == "failed" {
        "failed"
    } else if batch_status == "partial" {
        "partial"
    } else if items.is_empty() {
        "empty"
    } else {
        "completed"
    };
    let unit_cost = settings["cost_per_call"]
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0);
    let mut batch = json!({"schema":"muteki.src.connector-batch.v1","acquisition_id":acquisition_id,
        "batch_id":batch_id,"connector_id":provider_id(),"config_version":config["version"],"task_id":id,
        "product_mode":"src","request_digest":request_digest,"items":items,"source_refs":source_refs,
        "errors":errors,"error":errors.first().cloned().unwrap_or(Value::Null),
        "acquisition":{"query":body["query"],"pages":pages,"next_token":if truncated{json!(next_token)}else{Value::Null},
            "complete":!truncated&&errors.is_empty(),"truncated":truncated},
        "cost":{"state":if unit_cost.is_some(){"estimated"}else{"call_counted"},
            "actual":unit_cost.unwrap_or(0.0)*(pages as f64),"unknown":false},
        "external_state":batch_status,
        "status":batch_status,"outcome":outcome,"reconciliation_state":"unreconciled",
        "created_at":now(),"deduplicated":false});
    if let Ok(ingested) = super::ingestion::ingest_connector_batch_observations(&s, id, &batch, None) {
        batch["observation_counts"] = ingested["counts"].clone();
        batch["observations_ingested"] = json!(true);
    }
    save_rec(&s, id, "connector_batch", &batch_id, batch.clone(), Some(0))?;
    Ok(batch)
}

fn provider_id() -> &'static str {
    "aws_ec2"
}

fn assign_account_tenant(rows: &mut [Value], account_id: &Value) {
    for row in rows {
        row["mapped"]["tenant"] = account_id.clone();
    }
}
fn aws_items(document: &Value, region: &str) -> Api<(Vec<Value>, String)> {
    let mut items = Vec::new();
    for reservation in document["Reservations"].as_array().into_iter().flatten() {
        for instance in reservation["Instances"].as_array().into_iter().flatten() {
            let instance_id = instance["InstanceId"].as_str().unwrap_or("");
            items.push(
                json!({"input_key":format!("aws:ec2:{region}:{instance_id}"),
                "mapped":{"type":"cloud_resource","provider":"aws","service":"ec2",
                    "region":region,"resource_id":instance_id,"state":instance["State"]["Name"],
                    "public_read":false,"active_operation_authorized":false},"raw":instance}),
            );
        }
    }
    Ok((
        items,
        document["NextToken"].as_str().unwrap_or("").to_owned(),
    ))
}

pub(super) async fn query(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let provider = require(body, "connector_id")?;
    if provider == "aws_ec2" {
        let config = connector_config(&s, id, body)?;
        return aws_ec2(s, id, body, &config).await;
    }
    if !matches!(
        provider,
        "fofa" | "quake" | "shodan" | "zoomeye" | "scopesentry"
    ) {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability.unavailable",
            format!("{provider} has no scoped online query adapter"),
        ));
    }
    let query = if provider == "scopesentry" {
        body["query"].as_str().unwrap_or("")
    } else {
        require(body, "query")?
    };
    let config = connector_config(&s, id, body)?;
    let version = config["version"].as_u64().unwrap_or(0);
    if version == 0 {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.connector_config_missing",
            "saved connector config required",
        ));
    }
    let settings = &config["config"];
    if provider == "scopesentry"
        && (settings["query_enabled"] != true
            || !settings["endpoints"]
                .as_object()
                .is_some_and(|m| !m.is_empty()))
    {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability.unavailable",
            "scopesentry does not support online query with this configuration",
        ));
    }
    let key = body["idempotency_key"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let acquisition_id = format!(
        "sconnacq_{}",
        &crate::runs::src_api::digest(&json!([id, provider, key]))[..24]
    );
    let batch_id = format!(
        "sconnbatch_{}",
        &crate::runs::src_api::digest(&json!([id, provider, key]))[..24]
    );
    let request_digest = crate::runs::src_api::digest(&json!([
        provider,
        query,
        version,
        body["max_pages"],
        body["max_items"],
        body["extra"]
    ]));
    if let Ok(old) = rec(&s, id, "connector_batch", &batch_id) {
        if old["payload"]["request_digest"] != request_digest {
            return Err(Error::conflict(
                "connector idempotency key used for different input",
            ));
        }
        let mut batch = old["payload"].clone();
        batch["deduplicated"] = json!(true);
        return Ok(batch);
    }
    let acquisition = rec(&s, id, "connector_acquisition", &acquisition_id).ok();
    if let Some(old) = &acquisition {
        if old["payload"]["request_digest"] != request_digest {
            return Err(Error::conflict(
                "connector idempotency key used for different input",
            ));
        }
    } else {
        save_rec(
            &s,
            id,
            "connector_acquisition",
            &acquisition_id,
            json!({"acquisition_id":acquisition_id,"connector_id":provider,"request_digest":request_digest,
                "config_version":version,"query":query}),
            Some(0),
        )?;
    }
    let max_pages = body["max_pages"]
        .as_u64()
        .or_else(|| settings["max_pages"].as_u64())
        .unwrap_or(10)
        .clamp(1, 100);
    let max_items = body["max_items"]
        .as_u64()
        .or_else(|| settings["max_items"].as_u64())
        .unwrap_or(10_000)
        .max(1) as usize;
    let mut position = if provider == "quake" { 0 } else { 1 };
    let mut items = Vec::<Value>::new();
    let mut source_refs = Vec::<Value>::new();
    let mut quotas = Vec::<Value>::new();
    let mut total = None;
    let mut pages = 0;
    let mut more = false;
    let mut errors=Vec::new();
    while pages < max_pages && items.len() < max_items {
        pages += 1;
        let page_result:Api<Value>=async{
        let spec = page_spec_with_extra(provider, settings, query, position, &body["extra"])?;
        let page_key = format!("{acquisition_id}:{pages}");
        let page_row = rec(&s, id, "connector_query_page", &page_key).ok();
        let draft_id = if let Some(row) = page_row {
            if row["payload"]["url"] != spec.url || row["payload"]["position"] != position {
                return Err(Error::conflict("connector page changed before replay"));
            }
            require(&row["payload"], "draft_id")?.to_owned()
        } else {
            let draft_id = draft(&s, id, &spec)?;
            save_rec(
                &s,
                id,
                "connector_query_page",
                &page_key,
                json!({"draft_id":draft_id,"url":spec.url,"position":position,"page":pages}),
                Some(0),
            )?;
            draft_id
        };
        let manifest_doc = super::connector_config::manifest(provider)?;
        let allowed_base = super::connector_config::connector_base_url(&manifest_doc, Some(settings));
        let output = super::egress::execute_connector_draft(
            s.clone(),
            id,
            &json!({
                "draft_id": draft_id,
                "draft_revision": 1,
                "authorization_version": super::policy(&s, id)?["version"],
                "idempotency_key": format!("query:{page_key}"),
                "identity_ref": format!("connector:{provider}"),
                "context_id": format!("connector:{provider}"),
                "credential_bindings": spec.bindings,
                "budget": {"max_requests": 1}
            }),
            provider,
            &allowed_base,
            "query",
        )
        .await?["output"]
            .clone();
        if output["execution_state"] != "completed" {
            return Ok(json!({"waiting":
                {"schema":"muteki.src.connector-batch.v1","acquisition_id":acquisition_id,
                "batch_id":batch_id,"connector_id":provider,"config_version":version,"task_id":id,
                "product_mode":"src","items":items,"source_refs":source_refs,"errors":[],
                "acquisition":{"query":query,"pages":pages,"complete":false,"truncated":false},
                "cost":{"state":"unknown","actual":null},"external_state":output["execution_state"],
                "execution_id":output["execution_id"],"approval":output["approval"],"receipt":output["receipt"]}}));
        }
        let (document, reference) = connectors_online::body_bytes(&s, id, &output)?;
        source_refs.push(reference.clone());
        let (page_items, next, page_total, quota) =
            parse_page(provider, settings, &document, position)?;
        Ok(json!({"items":page_items,"next":next,"total":page_total,"quota":quota}))
        }.await;
        let page = match page_result {
            Ok(page) => page,
            Err(error) => {
                if pages == 1
                    && (error.status == StatusCode::FORBIDDEN
                        || error.status == StatusCode::TOO_MANY_REQUESTS)
                {
                    return Err(error);
                }
                errors.push(json!({"page":pages,"code":error.code,"message":error.message,"detail":error.detail}));
                more = !items.is_empty();
                break;
            }
        };
        if let Some(waiting)=page.get("waiting"){return Ok(waiting.clone());}
        let page_items=page["items"].as_array().cloned().unwrap_or_default();let next=page["next"].as_u64();let page_total=page["total"].as_u64();let quota=page["quota"].clone();
        items.extend(page_items);
        quotas.push(quota);
        total = page_total.or(total);
        more = next.is_some();
        let Some(next) = next else { break };
        position = next;
    }
    let truncated = more || items.len() > max_items;
    items.truncate(max_items);
    let batch_status = if !errors.is_empty() && items.is_empty() {
        "failed"
    } else if !errors.is_empty() || truncated {
        "partial"
    } else {
        "completed"
    };
    let outcome = if batch_status == "failed" {
        "failed"
    } else if batch_status == "partial" {
        "partial"
    } else if items.is_empty() {
        "empty"
    } else {
        "completed"
    };
    let reconciliation_state = if batch_status == "failed" && source_refs.is_empty() {
        "not_applicable"
    } else {
        "unreconciled"
    };
    let unit_cost = settings["cost_per_call"]
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0);
    let mut batch = json!({"schema":"muteki.src.connector-batch.v1","acquisition_id":acquisition_id,
        "batch_id":batch_id,"connector_id":provider,"config_version":version,"task_id":id,
        "product_mode":"src","request_digest":request_digest,"items":items,"source_refs":source_refs,
        "errors":errors,"error":errors.first().cloned().unwrap_or(Value::Null),
        "unknown_fields":[],"field_mapping":{},"acquisition":{"query":query,
            "pages":pages,"total":total,"next_token":if more{json!(position)}else{Value::Null},
            "complete":!truncated&&errors.is_empty(),"truncated":truncated},
        "counts":{"processed":items.len(),"new":0,"updated":0,"merged":0,"skipped":0,"failed":errors.len(),
            "truncated":usize::from(truncated)},
        "cost":{"state":if unit_cost.is_some(){"estimated"}else{"call_counted"},
            "actual":unit_cost.unwrap_or(0.0)*(pages as f64),"unknown":false,"quota":quotas},
        "external_state":batch_status,"status":batch_status,"outcome":outcome,
        "reconciliation_state":reconciliation_state,"created_at":now(),"deduplicated":false});
    if let Ok(ingested) = super::ingestion::ingest_connector_batch_observations(&s, id, &batch, None) {
        batch["counts"]["new"] = ingested["counts"]["created_assets"].clone();
        batch["counts"]["merged"] = ingested["counts"]["merged_assets"].clone();
        batch["observation_counts"] = ingested["counts"].clone();
        batch["observations_ingested"] = json!(true);
    }
    save_rec(&s, id, "connector_batch", &batch_id, batch.clone(), Some(0))?;
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_query_specs_keep_credentials_as_references() {
        let settings = json!({"email_ref":"secret://src/task/email","key_ref":"secret://src/task/key",
            "token_ref":"secret://src/task/token","page_size":25});
        let fofa = page_spec("fofa", &settings, "domain=example.com", 1).unwrap();
        assert!(fofa.url.starts_with("https://fofa.info/api/v1/search/all?"));
        assert_eq!(fofa.bindings[0]["name"], "email");
        assert_eq!(fofa.bindings[1]["name"], "key");
        assert!(!fofa.url.contains("secret://"));
        let quake = page_spec("quake", &settings, "example.com", 0).unwrap();
        assert_eq!(quake.method, "POST");
        assert_eq!(quake.bindings[0]["name"], "X-QuakeToken");
        assert_eq!(
            serde_json::from_slice::<Value>(&quake.body).unwrap()["size"],
            25
        );
        let shodan = page_spec("shodan", &settings, "hostname:example.com", 1).unwrap();
        assert_eq!(shodan.bindings[0]["location"], "query");
        assert_eq!(shodan.bindings[0]["name"], "key");
        let zoom = page_spec("zoomeye", &settings, "app:nginx", 1).unwrap();
        assert_eq!(zoom.bindings[0]["location"], "header");
        assert_eq!(zoom.bindings[0]["name"], "API-KEY");
        let (instances, token) = aws_items(
            &json!({"Reservations":[{"Instances":[{"InstanceId":"i-test",
            "State":{"Name":"running"}}]}],"NextToken":"page-2"}),
            "us-east-1",
        )
        .unwrap();
        assert_eq!(instances[0]["input_key"], "aws:ec2:us-east-1:i-test");
        assert_eq!(token, "page-2");
        let fixture = json!({"Reservations":[{"Instances":[{"InstanceId":"i-i65iso","State":{"Name":"running"}}]}]});
        let (mut rows, next) = aws_items(&fixture, "ap-east-1").unwrap();
        assert!(next.is_empty());
        assign_account_tenant(&mut rows, &json!("210987654321"));
        assert_eq!(rows[0]["mapped"]["tenant"], "210987654321");
        assert_eq!(rows[0]["mapped"]["resource_id"], "i-i65iso");
    }
}
