//! Source file parsing for SRC ingestion and connector imports.
use super::{Api, Error};
use base64::Engine;
use serde_json::{Value, json};
use std::path::Path;

pub(super) struct Parsed {
    pub rows: Vec<(usize, Value)>,
    pub failures: Vec<Value>,
    pub media_type: &'static str,
}

fn failed(index: usize, code: &str, message: &str, item: Value) -> Value {
    json!({"index":index,"code":code,"message":message,"item":item})
}

fn first<'a>(source: &'a Value, fields: &[&str]) -> Option<&'a Value> {
    fields
        .iter()
        .find_map(|field| field.split('.').try_fold(source, |value, key| value.get(key))
            .filter(|v| !v.is_null() && *v != ""))
}

pub(super) fn normalize(raw: &Value, mapping: &Value) -> Value {
    let source = raw;
    let mut projected = raw.clone();
    if let Some(fields) = mapping.as_object() {
        for (from, target) in fields {
            let Some(target) = target.as_str() else {
                continue;
            };
            if let Some(value) = first(source, &[from]) {
                projected[target] = value.clone();
            }
            if matches!(
                from.as_str(),
                "asset_type"
                    | "type"
                    | "name"
                    | "domain"
                    | "address"
                    | "url"
                    | "identity"
                    | "attributes"
            ) {
                if let Some(value) = first(source, &[target]) {
                    projected[from] = value.clone();
                }
            }
        }
    }
    let raw = &projected;
    let mapped = |field: &str| mapping[field].as_str().unwrap_or(field).to_owned();
    let type_field = mapped("asset_type");
    let mut kind = first(raw, &[&type_field, "type", "event_type"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if kind.is_empty() {
        kind = if raw["url"].is_string() {
            "http_endpoint"
        } else if raw["domain"].is_string() {
            "domain"
        } else if raw["ip"].is_string() {
            "ip"
        } else {
            ""
        }
        .to_owned();
    }
    kind = match kind.as_str() {
        "dns_name" => "domain",
        "ip_address" => "ip",
        "url" | "endpoint" => "http_endpoint",
        "open_tcp_port" => "port",
        "service" => "web_service",
        "repo" => "repository",
        "mini_program" => "miniprogram",
        "param" => "parameter",
        _ => &kind,
    }
    .to_owned();
    let mut identity = if raw["identity"].is_object() {
        raw["identity"].clone()
    } else if mapping["identity"].is_object() {
        mapping["identity"].clone()
    } else {
        match kind.as_str() {
            "domain" | "subdomain" | "root_domain" | "dns" => {
                json!({"name":first(raw,&["name","domain","host","data"]),
                "record_type":first(raw,&["record_type","dns_type"]),"value":first(raw,&["value","record_value"])})
            }
            "ip" | "cidr" => json!({"address":first(raw,&["address","ip","host","data"])}),
            "port" | "port_service" | "web_service" | "non_web_service" => {
                json!({"host":first(raw,&["host","ip","domain"]),
                "port":raw["port"].as_u64().or_else(||raw["port"].as_str().and_then(|v|v.parse::<u64>().ok())),
                "protocol":first(raw,&["protocol","scheme"]),"transport":raw["transport"]})
            }
            "web_origin" => json!({"url":first(raw,&["url","origin","uri","data"]),
                "scheme":raw["scheme"],"host":first(raw,&["host","domain"]),"port":raw["port"]}),
            "http_endpoint" | "http_route" | "api_endpoint" | "graphql_operation"
            | "websocket_endpoint" | "parameter" => json!({"url":first(raw,&["url","uri","endpoint","data"]),
                "method":raw["method"].as_str().unwrap_or("GET"),"location":first(raw,&["location","param_location","in"]),
                "name":first(raw,&["param_name","parameter","key"]),
                "operation_name":first(raw,&["operation_name","operation"]),
                "operation_type":raw["operation_type"],
                "identity_ref":first(raw,&["identity_ref","account"]),
                "tenant":raw["tenant"],"action":raw["action"]}),
            "app" | "miniprogram" | "mobile_package" => json!({"platform":raw["platform"],
                "stable_id":first(raw,&["stable_id","package_id","bundle_id","appid"]),
                "name":first(raw,&["name","product_name"])}),
            "cloud" | "cloud_resource" => json!({"provider":raw["provider"],"tenant":raw["tenant"],
                "region":raw["region"],"resource_id":first(raw,&["resource_id","arn","instance_id","id"])}),
            "repository" | "code_repository" => json!({"platform":first(raw,&["platform","host","provider"]),
                "stable_id":first(raw,&["stable_id","repository","canonical","full_name"])}),
            "cdn" => json!({"provider":first(raw,&["provider","cdn","vendor"]),
                "cname":first(raw,&["cname","target"]),
                "host":first(raw,&["host","domain","name"])}),
            "certificate" => json!({"sha256":first(raw,&["sha256","fingerprint_sha256","fingerprint","cert_sha256"]),
                "serial_number":first(raw,&["serial_number","serial"]),
                "common_name":first(raw,&["common_name","cn","subject","domain","name"]),
                "issuer":raw["issuer"]}),
            "organization_hint" => json!({"name":first(raw,&["name","organization","org","company","icp"]),
                "icp":raw["icp"],"domain":raw["domain"]}),
            "account_subject" => json!({"platform":first(raw,&["platform","provider"]),
                "subject":first(raw,&["subject","account","username","email","account_id","id"])}),
            _ => json!({}),
        }
    };
    if let Some(url_fields) = identity["url"].as_object().cloned() {
        let fields = identity.as_object_mut().unwrap();
        fields.remove("url");
        fields.extend(url_fields);
    }
    let mut attributes = raw["attributes"].as_object()
        .or_else(||mapping["attributes"].as_object()).cloned().unwrap_or_default();
    for key in [
        "title",
        "status_code",
        "content_length",
        "technology",
        "favicon",
        "ttl",
        "version",
        "description",
        "subject",
        "issuer",
        "record_type",
    ] {
        if let Some(value) = raw.get(key).filter(|v| !v.is_null()) {
            attributes.insert(key.to_owned(), value.clone());
        }
    }
    json!({"asset_type":kind,"identity":identity,"attributes":attributes,"raw":source})
}

fn push_object(
    rows: &mut Vec<(usize, Value)>,
    failures: &mut Vec<Value>,
    index: usize,
    item: &Value,
    mapping: &Value,
) {
    if item.is_object() {
        rows.push((index, normalize(item, mapping)))
    } else {
        failures.push(failed(
            index,
            "import_item_invalid",
            "entry must be an object",
            item.clone(),
        ));
    }
}

fn decorate(kind: &str, row: &mut Value) {
    let raw = row["raw"].clone();
    if kind == "bbot" {
        row["event_type"] = raw["type"].clone();
        row["uuid"] = raw["uuid"]
            .as_str()
            .or_else(|| raw["id"].as_str())
            .map_or(Value::Null, |v| json!(v));
        row["parent_uuid"] = raw["parent_uuid"].clone();
        row["scope"] = raw["scope"].clone();
    }
    if kind == "scopesentry" {
        let source_kind = raw["type"]
            .as_str()
            .or_else(|| raw["asset_type"].as_str())
            .or_else(|| raw["type_name"].as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let product = matches!(
            source_kind.as_str(),
            "app" | "miniprogram" | "mini_program" | "mp" | "wechat" | "小程序"
        ) || [
            "app",
            "miniProgram",
            "miniprogram",
            "mp",
            "package",
            "bundle_id",
        ]
        .iter()
        .any(|field| raw.get(*field).is_some());
        let reliable = ["AppID", "appid", "package", "bundle_id", "store_id"]
            .iter()
            .any(|field| raw.get(*field).is_some_and(|v| !v.is_null() && v != ""));
        if product && !reliable {
            row["identity_state"] = json!("candidate");
            row["missing_fields"] = json!(["reliable_app_id"]);
        }
    }
    if kind == "cloud_inventory" {
        let reliable = ["resource_id", "instance_id", "arn", "id"]
            .iter()
            .any(|field| raw.get(*field).is_some_and(|v| !v.is_null() && v != ""));
        if !reliable {
            row["identity_state"] = json!("candidate");
            row["missing_fields"] = json!(["provider_resource_id"]);
        }
    }
    if kind == "bbrf" {
        row["program"] = raw["program"]
            .as_str()
            .or_else(|| raw["program_name"].as_str())
            .map_or(Value::Null, |v| json!(v));
        row["tags"] = raw
            .get("tags")
            .or_else(|| raw.get("tag"))
            .cloned()
            .unwrap_or_else(|| json!([]));
    }
}

pub(super) fn source_mapping(kind: &str, field_mapping: &Value) -> Value {
    let mut mapping = match kind {
        "bbot" => json!({"event_schema":"bbot.typed-events.v3","parent_chain":"uuid,parent_uuid"}),
        "reconftw" => {
            json!({"event_schema":"reconftw.assets.jsonl","quickrescan_is_not_monitoring":true})
        }
        "burp" | "caido" => {
            json!({"event_schema":"har.http-archive.or.source-json","replayed_by_muteki":false})
        }
        "raw_http" => {
            json!({"event_schema":"raw-http.v1","derived_view":true,"replayed_by_muteki":false})
        }
        _ => json!({"event_schema":format!("{kind}.source-json.v1")}),
    };
    if field_mapping.is_object() {
        mapping["fields"] = field_mapping.clone();
    }
    mapping
}

fn push_source(
    kind: &str,
    rows: &mut Vec<(usize, Value)>,
    failures: &mut Vec<Value>,
    index: usize,
    item: &Value,
    mapping: &Value,
) {
    if kind == "bbot" && item["type"].as_str().is_none_or(str::is_empty) {
        failures.push(failed(
            index,
            "invalid_bbot_event",
            "BBOT event requires type",
            item.clone(),
        ));
        return;
    }
    let start = rows.len();
    let nested = if kind == "scopesentry" {
        let source = if item["data"].is_object() {
            &item["data"]
        } else {
            item
        };
        ["list", "records", "items"]
            .iter()
            .find_map(|key| source.get(*key))
    } else if kind == "cloud_inventory" {
        item.get("resources")
    } else {
        None
    };
    if let Some(nested) = nested {
        if let Some(children) = nested.as_array() {
            for child in children {
                let child_index = rows.len();
                push_object(rows, failures, child_index, child, mapping);
            }
        } else {
            failures.push(failed(
                index,
                "invalid_source_item",
                "nested source items must be an array",
                item.clone(),
            ));
        }
    } else if !(kind=="scopesentry"&&item["data"].is_object()) {
        push_object(rows, failures, index, item, mapping);
    }
    for (_, row) in rows.iter_mut().skip(start) {
        decorate(kind, row);
    }
}

fn parse_har(rows: &mut Vec<(usize, Value)>, failures: &mut Vec<Value>, value: &Value) {
    for (index, entry) in value["log"]["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if !entry.is_object() {
            failures.push(failed(
                index,
                "invalid_har_entry",
                "entry must be an object",
                entry.clone(),
            ));
            continue;
        }
        let request = &entry["request"];
        let url = request["url"].as_str().unwrap_or("");
        if reqwest::Url::parse(url).is_err() {
            failures.push(failed(
                index,
                "import_http_invalid",
                "HAR request URL is required",
                entry.clone(),
            ));
            continue;
        }
        let method = request["method"].as_str().unwrap_or("GET");
        rows.push((index,json!({"asset_type":"http_endpoint","identity":{"url":url,"method":method},
            "attributes":{"status_code":entry["response"]["status"],"status_text":entry["response"]["statusText"],
                "har_derived":true,"live_replay":false,"capture_state":"partial"},"raw":entry})));
    }
}

fn raw_http(bytes: &[u8], metadata: &Value) -> Api<Value> {
    let separator = if bytes.windows(4).any(|v| v == b"\r\n\r\n") {
        b"\r\n\r\n".as_slice()
    } else {
        b"\n\n".as_slice()
    };
    let header_end = bytes
        .windows(separator.len())
        .position(|v| v == separator)
        .ok_or_else(|| Error::invalid("raw HTTP request headers incomplete"))?;
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let marker = [separator, b"HTTP/"].concat();
    let split = bytes
        .windows(marker.len())
        .position(|v| v == marker)
        .map(|v| v + separator.len());
    let (request, response) = if let Some(index) = split {
        (&bytes[..index], &bytes[index..])
    } else {
        (bytes, &[][..])
    };
    let mut lines = header.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| Error::invalid("raw HTTP request line missing"))?;
    let parts = request_line.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 3 || !parts[2].starts_with("HTTP/") {
        return Err(Error::invalid("raw HTTP request line invalid"));
    }
    let mut host = "";
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push(json!([name,value.trim_start()]));
            if name.eq_ignore_ascii_case("Host") {
                host = value.trim();
            }
        }
    }
    let url = if parts[1].contains("://") {
        parts[1].to_owned()
    } else {
        let scheme = metadata["scheme"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                Error::invalid("origin-form HTTP import requires source_metadata.scheme")
            })?;
        if host.is_empty() {
            return Err(Error::invalid("origin-form HTTP import requires Host"));
        }
        format!("{scheme}://{host}{}", parts[1])
    };
    reqwest::Url::parse(&url).map_err(|_| Error::invalid("raw HTTP import URL invalid"))?;
    Ok(
        json!({"asset_type":"http_endpoint","identity":{"url":url,"method":parts[0].to_uppercase()},
        "attributes":{"raw_http":true,"live_replay":false,"capture_state":if response.is_empty(){"partial"}else{"complete"},"protocol":parts[2],"request_headers":headers},
        "raw":{"request_base64":base64::engine::general_purpose::STANDARD.encode(request),
            "response_base64":base64::engine::general_purpose::STANDARD.encode(response),
            "method":parts[0],"url":url}}),
    )
}

fn burp_xml(bytes: &[u8]) -> Api<Parsed> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::invalid("Burp XML is not UTF-8"))?;
    let document =
        roxmltree::Document::parse(text).map_err(|_| Error::invalid("Burp XML is invalid"))?;
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for (index, item) in document
        .descendants()
        .filter(|n| n.has_tag_name("item"))
        .enumerate()
    {
        let field = |name: &str| {
            item.children()
                .find(|n| n.has_tag_name(name))
                .and_then(|n| n.text())
                .unwrap_or("")
        };
        let url = field("url");
        if reqwest::Url::parse(url).is_err() {
            failures.push(failed(
                index,
                "import_http_invalid",
                "Burp item URL is invalid",
                Value::Null,
            ));
            continue;
        }
        let request = field("request");
        let response = field("response");
        if (!request.is_empty()
            && base64::engine::general_purpose::STANDARD
                .decode(request)
                .is_err())
            || (!response.is_empty()
                && base64::engine::general_purpose::STANDARD
                    .decode(response)
                    .is_err())
        {
            failures.push(failed(
                index,
                "invalid_burp_xml",
                "Burp request or response is not base64",
                Value::Null,
            ));
            continue;
        }
        rows.push((index,json!({"asset_type":"http_endpoint","identity":{"url":url,
            "method":if field("method").is_empty(){"GET"}else{field("method")}},
            "attributes":{"status_code":field("status"),"capture_state":if response.is_empty(){"partial"}else{"complete"},
                "live_replay":false,"burp_xml":true},
            "raw":{"request_base64":request,"response_base64":response,"url":url,"time":field("time")}})));
    }
    Ok(Parsed {
        rows,
        failures,
        media_type: "application/xml",
    })
}

pub(super) fn parse(
    bytes: &[u8],
    kind: &str,
    filename: &str,
    metadata: &Value,
    mapping: &Value,
) -> Api<Parsed> {
    if bytes.is_empty() {
        return Err(Error::invalid("cannot import an empty source"));
    }
    let extension = Path::new(filename)
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = kind.to_ascii_lowercase();
    let looks_http = [
        b"GET ".as_slice(),
        b"POST ",
        b"PUT ",
        b"DELETE ",
        b"PATCH ",
        b"HEAD ",
        b"OPTIONS ",
    ]
    .iter()
    .any(|method| bytes.starts_with(method));
    if (kind == "burp" && matches!(extension.as_str(), "xml" | "burp")) || kind == "burp_xml" {
        return burp_xml(bytes);
    }
    if matches!(kind.as_str(), "raw" | "raw_http" | "http")
        || matches!(extension.as_str(), "http" | "request")
        || (matches!(kind.as_str(), "burp" | "caido" | "cli") && looks_http)
    {
        return Ok(Parsed {
            rows: vec![(0, raw_http(bytes, metadata)?)],
            failures: vec![],
            media_type: "message/http",
        });
    }
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    if kind == "csv" || extension == "csv" {
        let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(bytes);
        let headers = reader
            .headers()
            .map_err(|_| Error::invalid("CSV header is invalid"))?
            .clone();
        if headers.is_empty() {
            return Err(Error::invalid("CSV header is required"));
        }
        for (index, record) in reader.records().enumerate() {
            match record {
                Ok(record) => {
                    let object = headers
                        .iter()
                        .zip(record.iter())
                        .map(|(key, value)| (key.to_owned(), json!(value)))
                        .collect::<serde_json::Map<String, Value>>();
                    let mut row = normalize(&Value::Object(object), mapping);
                    decorate(&kind, &mut row);
                    rows.push((index, row));
                }
                Err(_) => failures.push(failed(
                    index,
                    "import_csv_invalid",
                    "CSV row is invalid",
                    Value::Null,
                )),
            }
        }
        return Ok(Parsed {
            rows,
            failures,
            media_type: "text/csv",
        });
    }
    if kind == "jsonl"
        || matches!(extension.as_str(), "jsonl" | "ndjson")
        || (matches!(kind.as_str(), "bbot" | "reconftw" | "bbrf")
            && serde_json::from_slice::<Value>(bytes).is_err())
    {
        let text = std::str::from_utf8(bytes).map_err(|_| Error::invalid("JSONL is not UTF-8"))?;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(item) => push_source(&kind, &mut rows, &mut failures, index, &item, mapping),
                Err(_) => failures.push(failed(
                    index,
                    "import_item_invalid",
                    "JSONL row is invalid",
                    Value::Null,
                )),
            }
        }
        return Ok(Parsed {
            rows,
            failures,
            media_type: "application/x-ndjson",
        });
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| Error::invalid("import JSON is invalid"))?;
    if value["log"]["entries"].is_array() {
        parse_har(&mut rows, &mut failures, &value);
    } else if let Some(items) = value["items"].as_array() {
        for (index, item) in items.iter().enumerate() {
            push_source(&kind, &mut rows, &mut failures, index, item, mapping);
        }
    } else if let Some(items) = value.as_array() {
        for (index, item) in items.iter().enumerate() {
            push_source(&kind, &mut rows, &mut failures, index, item, mapping);
        }
    } else {
        push_source(&kind, &mut rows, &mut failures, 0, &value, mapping);
    }
    Ok(Parsed {
        rows,
        failures,
        media_type: "application/json",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_file_formats_project_assets_and_keep_row_errors() {
        let csv = parse(
            b"type,domain,title\ndomain,example.com,CSV asset\n",
            "csv",
            "assets.csv",
            &Value::Null,
            &Value::Null,
        )
        .unwrap();
        assert_eq!(csv.media_type, "text/csv");
        assert_eq!(csv.rows[0].1["identity"]["name"], "example.com");
        assert_eq!(csv.rows[0].1["attributes"]["title"], "CSV asset");
        let mapped = parse(
            b"kind,hostname\ndomain,mapped.example.test\n",
            "csv",
            "mapped.csv",
            &Value::Null,
            &json!({"kind":"asset_type","hostname":"domain"}),
        )
        .unwrap();
        assert_eq!(mapped.rows[0].1["asset_type"], "domain");
        assert_eq!(mapped.rows[0].1["identity"]["name"], "mapped.example.test");
        let jsonl = parse(
            b"{\"type\":\"IP_ADDRESS\",\"ip\":\"192.0.2.8\"}\nnot-json\n",
            "bbot",
            "events.jsonl",
            &Value::Null,
            &Value::Null,
        )
        .unwrap();
        assert_eq!(jsonl.rows[0].1["asset_type"], "ip");
        assert_eq!(jsonl.failures.len(), 1);
        let har=parse(br#"{"log":{"entries":[{"request":{"method":"GET","url":"https://example.com/login"},"response":{"status":200}}]}}"#,
            "har","capture.har",&Value::Null,&Value::Null).unwrap();
        assert_eq!(har.rows[0].1["asset_type"], "http_endpoint");
        assert_eq!(har.rows[0].1["attributes"]["status_code"], 200);
        let raw = parse(
            b"GET /login HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "raw_http",
            "request.http",
            &json!({"scheme":"https"}),
            &Value::Null,
        )
        .unwrap();
        assert_eq!(
            raw.rows[0].1["identity"]["url"],
            "https://example.com/login"
        );
        let burp=parse(b"<items><item><url>https://example.com/login</url><method>GET</method><request>R0VU</request></item></items>",
            "burp","capture.xml",&Value::Null,&Value::Null).unwrap();
        assert_eq!(burp.rows[0].1["attributes"]["burp_xml"], true);
        let nested = parse(
            br#"{"data":{"list":[{"type":"domain","domain":"nested.example.test"}]}}"#,
            "scopesentry",
            "scope.json",
            &Value::Null,
            &Value::Null,
        )
        .unwrap();
        assert_eq!(nested.rows[0].1["identity"]["name"], "nested.example.test");
    }
}

/// Decode only the derived parsing view; the caller keeps the original bytes.
pub(super) fn parse_import(bytes:&[u8],kind:&str,filename:&str,metadata:&Value,mapping:&Value,media_type:&str,encoding:&str)->Api<Parsed>{
    let raw=matches!(kind,"raw"|"raw_http"|"http"|"burp"|"caido")&&(bytes.windows(4).any(|v|v==b"\r\n\r\n")||bytes.windows(2).any(|v|v==b"\n\n"));
    if raw{return parse(bytes,"raw_http",filename,metadata,mapping);}
    let codec=if encoding.trim().is_empty(){"utf-8-sig"}else{encoding.trim()};
    let fail=||Error::new(axum::http::StatusCode::BAD_REQUEST,"import_encoding_invalid",format!("cannot decode import bytes as {codec}"));
    let decoded=match codec.to_ascii_lowercase().replace('_',"-").as_str(){
        "utf-8"|"utf8"=>std::str::from_utf8(bytes).map(str::to_owned).map_err(|_|fail())?,
        "utf-8-sig"=>std::str::from_utf8(bytes.strip_prefix(&[0xef,0xbb,0xbf]).unwrap_or(bytes)).map(str::to_owned).map_err(|_|fail())?,
        "latin-1"|"latin1"|"iso-8859-1"=>bytes.iter().map(|byte|char::from(*byte)).collect(),
        "ascii"|"us-ascii"=>{if !bytes.is_ascii(){return Err(fail());}String::from_utf8(bytes.to_vec()).map_err(|_|fail())?},
        label=>{let codec=encoding_rs::Encoding::for_label(label.as_bytes()).ok_or_else(fail)?;let(text,had_errors)=codec.decode_without_bom_handling(bytes);if had_errors{return Err(fail());}text.into_owned()}
    };
    let kind=if media_type=="text/csv"{"csv"}else if matches!(media_type,"application/jsonl"|"application/x-ndjson"){"jsonl"}else{kind};
    parse(decoded.as_bytes(),kind,filename,metadata,mapping)
}
