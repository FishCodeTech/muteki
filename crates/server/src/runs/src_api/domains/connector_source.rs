//! Connector source contracts precede asset normalization and retain source fields.
use super::*;
pub(super) struct Parsed{pub items:Vec<Value>,pub failures:Vec<Value>,pub mapping:Value,pub media_type:String,pub complete:bool}
fn error(index:usize,code:&str,message:impl Into<String>)->Value{json!({"index":index,"field":"","code":code,"message":message.into(),"recovery":"修正该项后仅重试失败项"})}
fn stable(raw:&Value,index:usize)->String{
    if raw.is_object(){if let(Some(url),Some(time))=(raw.get("url").filter(|v|!v.is_null()&&*v!=""),raw.get("started").or_else(||raw.get("time")).filter(|v|!v.is_null()&&*v!="")){return format!("url:{}@{}:{index}",prose(url),prose(time));}
        for key in ["uuid","id","asset_id","url","domain","ip","package","appid","AppID"]{if let Some(value)=raw.get(key).filter(|v|!v.is_null()&&*v!=""){return format!("{key}:{}",prose(value));}}
    }format!("sha256:{}:{index}",super::super::digest(raw))
}
fn prose(value:&Value)->String{value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string())}
fn object(raw:&Value,index:usize,mapping:&Value)->Value{
    let Some(fields)=raw.as_object()else{return json!({"input_key":format!("value:{index}"),"mapped":{"value":raw},"raw":raw})};
    let mut mapped=fields.clone();if let Some(mapping)=mapping.as_object(){for(source,target)in mapping{if let(Some(value),Some(target))=(fields.get(source),target.as_str()){mapped.insert(target.into(),value.clone());}}}
    json!({"input_key":stable(raw,index),"mapped":mapped,"raw":raw})
}
fn har(value:&Value,items:&mut Vec<Value>){
    for(index,entry)in value["log"]["entries"].as_array().into_iter().flatten().enumerate(){
        if !entry.is_object(){items.push(json!({"input_key":format!("invalid:{index}"),"error":error(index,"invalid_har_entry","entry must be an object"),"raw":entry}));continue;}
        let request=&entry["request"];let response=&entry["response"];let complete=request.as_object().is_some_and(|v|!v.is_empty())&&response.as_object().is_some_and(|v|!v.is_empty());
        let raw=json!({"request":request,"response":response,"startedDateTime":entry["startedDateTime"],"time":entry["time"],"timings":entry["timings"],"serverIPAddress":entry["serverIPAddress"],"comment":entry["comment"]});
        items.push(json!({"input_key":stable(&json!({"url":request["url"],"started":entry["startedDateTime"]}),index),"mapped":{"type":"http_exchange","method":request["method"],"url":request["url"],"status":response["status"],"capture_state":if complete{"complete"}else{"partial"},"source_kind":"import"},"raw":raw}));
    }
}
fn latin(bytes:&[u8])->String{bytes.iter().map(|byte|char::from(*byte)).collect()}
fn raw_http(bytes:&[u8])->Parsed{
    let boundary=[b"\r\n\r\nHTTP/".as_slice(),b"\n\nHTTP/".as_slice()].iter().filter_map(|marker|bytes.windows(marker.len()).position(|v|v==*marker).map(|at|at+marker.len()-5)).min();
    let(request,response)=boundary.map(|at|(&bytes[..at],&bytes[at..])).unwrap_or((bytes,&[]));
    let header_end=request.windows(4).position(|v|v==b"\r\n\r\n").or_else(||request.windows(2).position(|v|v==b"\n\n")).unwrap_or(request.len());
    let headers=latin(&request[..header_end]).replace("\r\n","\n");let mut lines=headers.lines();let first=lines.next().unwrap_or("");let parts=first.splitn(3,' ').collect::<Vec<_>>();
    let mut parsed=Parsed{items:Vec::new(),failures:Vec::new(),mapping:json!({"event_schema":"raw-http.v1","derived_view":true,"replayed_by_muteki":false}),media_type:"message/http".into(),complete:true};
    if parts.len()<2{parsed.failures.push(error(0,"invalid_raw_http","request line is malformed"));parsed.complete=false;return parsed;}
    let fields=lines.filter_map(|line|line.split_once(':').map(|(name,value)|json!([name,value.trim_start()]))).collect::<Vec<_>>();
    let status=latin(response).lines().next().and_then(|line|line.split_whitespace().nth(1)).and_then(|value|value.parse::<u16>().ok());
    parsed.items.push(json!({"input_key":format!("raw-http:{:x}",Sha256::digest(request)),"mapped":{"type":"http_exchange","method":parts[0],"url":parts[1],"status":status,"source_kind":"import","capture_state":if response.is_empty(){"partial"}else{"complete"},"headers":fields},"raw":{"request_base64":base64::engine::general_purpose::STANDARD.encode(request),"response_base64":if response.is_empty(){String::new()}else{base64::engine::general_purpose::STANDARD.encode(response)},"protocol":"raw-http/1.x"}}));parsed
}
fn burp(bytes:&[u8])->Api<Vec<Value>>{
    let text=std::str::from_utf8(bytes).map_err(|_|Error::invalid("Burp XML is not UTF-8"))?;let xml=roxmltree::Document::parse(text).map_err(|error|Error::invalid(error.to_string()))?;let mut items=Vec::new();
    for(index,item)in xml.descendants().filter(|node|node.has_tag_name("item")).enumerate(){
        let field=|name:&str|item.children().find(|node|node.has_tag_name(name)).and_then(|node|node.text()).unwrap_or("");let request=field("request");let response=field("response");
        for encoded in [request,response]{base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_|Error::invalid("Burp body is not base64"))?;}
        items.push(json!({"input_key":stable(&json!({"url":field("url"),"time":field("time")}),index),"mapped":{"type":"http_exchange","method":field("method"),"url":field("url"),"status":field("status"),"source_kind":"import","capture_state":if !request.is_empty()&&!response.is_empty(){"complete"}else{"partial"}},"raw":{"request_base64":request,"response_base64":response,"time":field("time"),"extension":field("extension")}}));
    }Ok(items)
}
pub(super) fn parse(bytes:&[u8],connector:&str,filename:&str,mapping:&Value)->Api<Parsed>{
    let first=bytes.split(|byte|*byte==b'\n').next().unwrap_or(&[]);let first=latin(first).trim_start().to_uppercase();
    if matches!(connector,"burp"|"caido"|"cli")&&["GET ","POST ","PUT ","DELETE ","PATCH ","OPTIONS ","HEAD ","TRACE ","CONNECT "].iter().any(|method|first.starts_with(method)){return Ok(raw_http(bytes));}
    let suffix=std::path::Path::new(filename).extension().and_then(|v|v.to_str()).unwrap_or("").to_ascii_lowercase();
    let mut parsed=Parsed{items:Vec::new(),failures:Vec::new(),mapping:import_formats::source_mapping(connector,mapping),media_type:"application/json".into(),complete:true};
    if connector=="burp"&&matches!(suffix.as_str(),"xml"|"burp"){
        parsed.media_type="application/xml".into();parsed.mapping=json!({"format":"burp-xml-v1"});match burp(bytes){Ok(items)=>parsed.items=items,Err(issue)=>{parsed.failures.push(error(0,"invalid_burp_xml",issue.message));parsed.complete=false;}}return Ok(parsed);
    }
    let mut rows=Vec::new();
    if suffix=="csv"{
        parsed.media_type="text/csv".into();let bytes=bytes.strip_prefix(&[0xef,0xbb,0xbf]).unwrap_or(bytes);let mut csv=csv::Reader::from_reader(bytes);let headers=csv.headers().map_err(|error|Error::invalid(error.to_string()))?.clone();
        for(index,row)in csv.records().enumerate(){match row{Ok(row)=>rows.push((index,Value::Object(headers.iter().zip(row.iter()).map(|(key,value)|(key.to_owned(),json!(value))).collect()))),Err(issue)=>parsed.failures.push(error(index,"invalid_source_item",issue.to_string()))}}
    }else if matches!(suffix.as_str(),"xml"|"burp"){parsed.media_type="application/xml".into();}
    else if let Ok(value)=serde_json::from_slice::<Value>(bytes.strip_prefix(&[0xef,0xbb,0xbf]).unwrap_or(bytes)){match value{Value::Array(items)=>rows.extend(items.into_iter().enumerate()),value=>rows.push((0,value))}}
    else{
        parsed.media_type="application/x-ndjson".into();for(index,line)in bytes.split(|byte|*byte==b'\n').enumerate(){if line.iter().all(u8::is_ascii_whitespace){continue;}match serde_json::from_slice(line){Ok(value)=>rows.push((index,value)),Err(issue)=>parsed.failures.push(error(index,"invalid_json",issue.to_string()))}}
    }
    for(index,row)in rows{
        if matches!(connector,"burp"|"caido")&&row.get("log").is_some(){har(&row,&mut parsed.items);continue;}
        if connector=="bbot"&&row["type"].as_str().is_none_or(|value|value.is_empty()){let mut issue=error(index,"invalid_bbot_event","BBOT event requires type");issue["field"]=json!("type");parsed.failures.push(issue);continue;}
        let nested=if connector=="scopesentry"{let object=if row["data"].is_object(){&row["data"]}else{&row};["list","records","items"].iter().find_map(|key|object.get(*key))}else if connector=="cloud_inventory"{row.get("resources")}else{None};
        if let Some(nested)=nested{if let Some(rows)=nested.as_array(){for row in rows{parsed.items.push(object(row,parsed.items.len(),mapping));}}else{parsed.failures.push(error(index,"invalid_source_item","source list must be an array"));}continue;}
        if connector=="scopesentry"&&row["data"].is_object(){continue;}
        let mut item=object(&row,index,mapping);
        if connector=="bbot"{item["mapped"]["event_type"]=row["type"].clone();item["mapped"]["uuid"]=row.get("uuid").or_else(||row.get("id")).cloned().unwrap_or(Value::Null);item["mapped"]["parent_uuid"]=row["parent_uuid"].clone();item["mapped"]["scope"]=row["scope"].clone();}
        parsed.items.push(item);
    }
    for item in &mut parsed.items{let raw=item["raw"].clone();if !raw.is_object(){continue;}let mapped=&mut item["mapped"];
        let kind=raw["type"].as_str().or_else(||raw["asset_type"].as_str()).or_else(||raw["type_name"].as_str()).unwrap_or("").to_lowercase();
        let product=matches!(kind.as_str(),"app"|"miniprogram"|"mini_program"|"mp"|"wechat"|"小程序")||["app","miniProgram","miniprogram","mp","package","bundle_id"].iter().any(|key|raw.get(*key).is_some());
        let missing=if connector=="scopesentry"&&product&&!["AppID","appid","package","bundle_id","store_id"].iter().any(|key|raw.get(*key).is_some_and(|v|!v.is_null()&&v!="")){Some("reliable_app_id")}else if connector=="cloud_inventory"&&!["resource_id","instance_id","arn","id"].iter().any(|key|raw.get(*key).is_some_and(|v|!v.is_null()&&v!="")){Some("provider_resource_id")}else{None};
        if let Some(field)=missing{mapped["identity_state"]=json!("candidate");mapped["missing_fields"]=json!([field]);}
        if connector=="bbrf"{mapped["program"]=raw.get("program").or_else(||raw.get("program_name")).cloned().unwrap_or(Value::Null);mapped["tags"]=raw.get("tags").or_else(||raw.get("tag")).cloned().unwrap_or(json!([]));}
    }
    parsed.complete=parsed.failures.is_empty();Ok(parsed)
}
