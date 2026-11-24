//! SRC frozen-report exports with Task-local artifact IDs.
use super::*;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;

fn selected_exchanges(report: &Value) -> Api<Vec<Value>> {
    let rows = report["document"]["exchanges"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "artifact_missing",
            "report has no selected HTTP exchanges",
        ));
    }
    Ok(rows)
}

fn artifact_bytes(s: &AppState, task_id: &str, reference: &Value) -> Api<Vec<u8>> {
    let saved = verify_ref(s, task_id, reference)?;
    base64::engine::general_purpose::STANDARD
        .decode(saved["data"].as_str().unwrap_or(""))
        .map_err(|_| {
            Error::new(
                StatusCode::CONFLICT,
                "src.artifact_corrupt",
                "evidence artifact bytes are unavailable",
            )
        })
}

fn rules(body: &Value) -> Api<Vec<(String, String)>> {
    body["redactions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|rule| {
            Ok((
                require(rule, "value")?.to_owned(),
                rule["replacement"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("[已脱敏]")
                    .to_owned(),
            ))
        })
        .collect()
}

fn redact_bytes(mut bytes: Vec<u8>, rules: &[(String, String)]) -> (Vec<u8>, Vec<usize>) {
    let mut counts = Vec::with_capacity(rules.len());
    for (needle, replacement) in rules {
        let needle = needle.as_bytes();
        let replacement = replacement.as_bytes();
        let mut output = Vec::with_capacity(bytes.len());
        let mut cursor = 0;
        let mut count = 0;
        while let Some(offset) = bytes[cursor..]
            .windows(needle.len())
            .position(|part| part == needle)
        {
            let position = cursor + offset;
            output.extend_from_slice(&bytes[cursor..position]);
            output.extend_from_slice(replacement);
            cursor = position + needle.len();
            count += 1;
        }
        output.extend_from_slice(&bytes[cursor..]);
        bytes = output;
        counts.push(count);
    }
    (bytes, counts)
}

fn redact_value(value: &mut Value, rules: &[(String, String)], counts: &mut [usize]) {
    match value {
        Value::String(text) => {
            for (index, (needle, replacement)) in rules.iter().enumerate() {
                counts[index] += text.matches(needle).count();
                *text = text.replace(needle, replacement);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item, rules, counts);
            }
        }
        Value::Object(items) => {
            let old = std::mem::take(items);
            for (mut key, mut item) in old {
                redact_value(&mut item, rules, counts);
                for (index, (needle, replacement)) in rules.iter().enumerate() {
                    counts[index] += key.matches(needle).count();
                    key = key.replace(needle, replacement);
                }
                items.insert(key, item);
            }
        }
        _ => {}
    }
}

fn body_bytes(bytes: &[u8]) -> &[u8] {
    bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|position| &bytes[position + 4..])
        .unwrap_or(bytes)
}

fn headers(value: &Value) -> Value {
    json!(
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                Some(json!({"name":pair.first()?.as_str()?,"value":pair.get(1)?.as_str()?}))
            })
            .collect::<Vec<_>>()
    )
}

fn raw_http(
    s: &AppState,
    task_id: &str,
    report: &Value,
    redactions: &[(String, String)],
) -> Api<Vec<u8>> {
    let mut chunks = Vec::new();
    for exchange in selected_exchanges(report)? {
        for field in ["request_raw_ref", "response_raw_ref"] {
            if exchange[field].is_object() {
                chunks.push(artifact_bytes(s, task_id, &exchange[field])?);
            }
        }
    }
    if chunks.is_empty() {
        return Err(Error::new(StatusCode::NOT_FOUND, "artifact_missing",
            "report exchanges have no frozen raw HTTP artifacts"));
    }
    let mut joined = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        if index > 0 {
            joined.extend_from_slice(b"\r\n\r\n");
        }
        joined.extend_from_slice(chunk);
    }
    let (content, counts) = redact_bytes(joined, redactions);
    for (index, count) in counts.iter().enumerate() {
        if *count == 0 {
            return Err(Error::conflict(format!(
                "redaction value was not found in raw HTTP: {}",
                redactions[index].0
            )));
        }
    }
    Ok(content)
}

fn har(s: &AppState, task_id: &str, report: &Value, redactions: &[(String, String)]) -> Api<Value> {
    let mut entries = Vec::new();
    let mut counts = vec![0; redactions.len()];
    let mut has_raw_artifact = false;
    for exchange in selected_exchanges(report)? {
        has_raw_artifact |= exchange["request_raw_ref"].is_object() || exchange["response_raw_ref"].is_object();
        let request = if exchange["request_raw_ref"].is_object() {
            artifact_bytes(s, task_id, &exchange["request_raw_ref"])?
        } else {
            Vec::new()
        };
        let response = if exchange["response_raw_ref"].is_object() {
            artifact_bytes(s, task_id, &exchange["response_raw_ref"])?
        } else {
            Vec::new()
        };
        let (request, request_counts) = redact_bytes(request, redactions);
        let (response, response_counts) = redact_bytes(response, redactions);
        for index in 0..redactions.len() {
            counts[index] += request_counts[index] + response_counts[index];
        }
        let request_body = body_bytes(&request);
        let response_body = body_bytes(&response);
        entries.push(json!({
            "startedDateTime":exchange["started_at"],"time":exchange["duration_ms"].as_u64().unwrap_or(0),
            "request":{"method":exchange["method"],"url":exchange["observed_url"],
                "httpVersion":exchange["protocol"].as_str().unwrap_or("http/1.1"),
                "headers":headers(&exchange["request_headers"]),"queryString":exchange["query"].as_array().cloned().unwrap_or_default(),
                "cookies":[],"headersSize":-1,"bodySize":request_body.len(),
                "postData":{"mimeType":"application/octet-stream","text":base64::engine::general_purpose::STANDARD.encode(request_body),"encoding":"base64"}},
            "response":{"status":exchange["status_code"].as_u64().unwrap_or(0),"statusText":"",
                "httpVersion":exchange["protocol"].as_str().unwrap_or("http/1.1"),
                "headers":headers(&exchange["response_headers"]),"cookies":[],"redirectURL":"",
                "headersSize":-1,"bodySize":response_body.len(),
                "content":{"size":response_body.len(),"mimeType":"application/octet-stream",
                    "text":base64::engine::general_purpose::STANDARD.encode(response_body),"encoding":"base64"}},
            "cache":{},"timings":{"send":0,"wait":exchange["duration_ms"].as_u64().unwrap_or(0),"receive":0},
            "comment":"HAR is an interchange projection. Refer to the original Artifact for wire-byte guarantees.",
            "_muteki":{"exchange_id":exchange["exchange_id"],"capture_state":exchange["capture_state"],
                "request_artifact_id":exchange["request_raw_ref"]["artifact_id"],
                "response_artifact_id":exchange["response_raw_ref"]["artifact_id"],
                "request_raw_base64":base64::engine::general_purpose::STANDARD.encode(&request),
                "response_raw_base64":if response.is_empty(){Value::Null}else{json!(base64::engine::general_purpose::STANDARD.encode(&response))},
                "derived_redaction":!redactions.is_empty()}
        }));
    }
    if !has_raw_artifact {
        return Err(Error::new(StatusCode::NOT_FOUND, "artifact_missing",
            "report exchanges have no frozen raw HTTP artifacts"));
    }
    let mut value = json!({"log":{"version":"1.2","creator":{"name":"Muteki SRC",
        "version":"src-submission-renderer.v1"},"entries":entries}});
    redact_value(&mut value, redactions, &mut counts);
    for (index, count) in counts.iter().enumerate() {
        if *count == 0 {
            return Err(Error::conflict(format!(
                "redaction value was not found in HAR: {}",
                redactions[index].0
            )));
        }
    }
    Ok(value)
}

fn jsonl(report: &Value, redactions: &[(String, String)]) -> Api<Vec<u8>> {
    let mut rows = vec![json!({"type":"frozen_report","object":report})];
    for claim in report["claims"].as_array().into_iter().flatten() {
        rows.push(json!({"type":"claim","report_id":report["report_id"],"object":claim}));
    }
    for exchange in report["document"]["exchanges"]
        .as_array()
        .into_iter()
        .flatten()
    {
        rows.push(
            json!({"type":"http_exchange","report_id":report["report_id"],"object":exchange}),
        );
    }
    for reference in report["evidence_refs"].as_array().into_iter().flatten() {
        rows.push(json!({"type":"artifact","report_id":report["report_id"],"object":reference}));
    }
    let mut output = Vec::new();
    for mut row in rows {
        let mut counts = vec![0; redactions.len()];
        redact_value(&mut row, redactions, &mut counts);
        output.extend_from_slice(&serde_json::to_vec(&row).map_err(super::db)?);
        output.push(b'\n');
    }
    Ok(output)
}

fn result(
    report: &Value,
    bytes: Vec<u8>,
    media_type: &str,
    filename: String,
    detail: Value,
) -> Value {
    let mut output = json!({"bytes":{"encoding":"base64",
        "data":base64::engine::general_purpose::STANDARD.encode(&bytes),"size":bytes.len()},
        "media_type":media_type,"filename":filename,"size":bytes.len(),
        "derived_from":report["report_id"]});
    if let Some(fields) = detail.as_object() {
        for (name, value) in fields {
            output[name] = value.clone();
        }
    }
    output
}

fn sanitize_xml_text(raw: &str) -> String {
    raw.chars()
        .filter(|&c| {
            matches!(c, '\t' | '\n' | '\r' | '\u{0020}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
        })
        .collect()
}

fn xml_escape(raw: &str) -> String {
    sanitize_xml_text(raw)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn report_retest_status(s: &AppState, task_id: &str, report: &Value) -> (bool, bool) {
    let report_id = report["report_id"].as_str().unwrap_or("");
    let finding_id = report["finding_id"].as_str().unwrap_or("");
    let frozen_at = report["frozen_at"].as_str().unwrap_or("");

    let has_concluded_retest = records(s, task_id, "finding_retest")
        .unwrap_or_default()
        .into_iter()
        .any(|row| {
            let p = &row["payload"];
            let target_match = p["report_id"].as_str() == Some(report_id)
                || (!finding_id.is_empty() && p["finding_id"].as_str() == Some(finding_id));
            target_match
                && p["status"].as_str() == Some("completed")
                && matches!(p["result"].as_str(), Some("fixed" | "still_vulnerable"))
        });

    let mut evidence_stale = false;
    if !finding_id.is_empty() {
        if let Ok(st) = super::finding::status(s, task_id, &json!({"finding_id": finding_id})) {
            if st["stale"] == true {
                evidence_stale = true;
            }
        }
    }
    if !evidence_stale && !frozen_at.is_empty() {
        for r in report["evidence_refs"].as_array().into_iter().flatten() {
            if let Some(ex_id) = r["exchange_id"].as_str() {
                if let Ok(row) = rec(s, task_id, "http_exchange", ex_id) {
                    if row["updated_at"].as_str().is_some_and(|u| u > frozen_at) {
                        evidence_stale = true;
                        break;
                    }
                }
            }
        }
    }
    (has_concluded_retest, evidence_stale)
}

struct DocxImage {
    rel_id: String,
    filename: String,
    ext: &'static str,
    bytes: Vec<u8>,
    cx: i64,
    cy: i64,
    caption: String,
}

fn collect_docx_images(
    s: &AppState,
    task_id: &str,
    report: &Value,
    redactions: &[(String, String)],
    allow_partial: bool,
) -> Api<Vec<DocxImage>> {
    let mut seen_ids = HashSet::new();
    let mut candidates: Vec<(Value, String)> = Vec::new();
    for reference in report["evidence_refs"].as_array().into_iter().flatten() {
        let mt = reference["media_type"].as_str().unwrap_or("");
        if mt.starts_with("image/") {
            let aid = reference["artifact_id"].as_str().unwrap_or("").to_owned();
            if !aid.is_empty() && seen_ids.insert(aid) {
                let caption = reference["name"].as_str().unwrap_or("Screenshot").to_owned();
                candidates.push((reference.clone(), caption));
            }
        }
    }
    let finding_id = report["finding_id"].as_str().unwrap_or("");
    if let Ok(shots) = records(s, task_id, "browser_screenshot") {
        for row in shots {
            let p = &row["payload"];
            let art = &p["artifact"];
            let aid = art["artifact_id"].as_str().unwrap_or("");
            if aid.is_empty() || !seen_ids.insert(aid.to_owned()) {
                continue;
            }
            let linked_finding = p["finding_id"].as_str().unwrap_or("");
            if !linked_finding.is_empty() && !finding_id.is_empty() && linked_finding != finding_id {
                continue;
            }
            if !art.is_object() {
                continue;
            }
            let caption = p["url"]
                .as_str()
                .filter(|u| !u.is_empty())
                .unwrap_or("Browser Screenshot")
                .to_owned();
            candidates.push((art.clone(), caption));
        }
    }

    let mut images = Vec::new();
    for (idx, (reference, caption)) in candidates.into_iter().enumerate() {
        let raw = match artifact_bytes(s, task_id, &reference) {
            Ok(bytes) => {
                if redactions.is_empty() {
                    bytes
                } else {
                    redact_bytes(bytes, redactions).0
                }
            }
            Err(_) if allow_partial => continue,
            Err(err) => return Err(err),
        };
        let reader = match image::ImageReader::new(std::io::Cursor::new(&raw)).with_guessed_format() {
            Ok(r) => r,
            Err(_) if allow_partial => continue,
            Err(err) => return Err(Error::invalid(format!("invalid image evidence: {err}"))),
        };
        let fmt = reader.format();
        let (ext, out_bytes, w_px, h_px) = match fmt {
            Some(image::ImageFormat::Png) => {
                let (w, h) = match reader.into_dimensions() {
                    Ok(dims) => dims,
                    Err(_) if allow_partial => continue,
                    Err(err) => return Err(Error::invalid(format!("corrupt png image: {err}"))),
                };
                ("png", raw, w, h)
            }
            Some(image::ImageFormat::Jpeg) => {
                let (w, h) = match reader.into_dimensions() {
                    Ok(dims) => dims,
                    Err(_) if allow_partial => continue,
                    Err(err) => return Err(Error::invalid(format!("corrupt jpeg image: {err}"))),
                };
                ("jpeg", raw, w, h)
            }
            _ => {
                let decoded = match reader.decode() {
                    Ok(img) => img,
                    Err(_) if allow_partial => continue,
                    Err(err) => return Err(Error::invalid(format!("unsupported image format: {err}"))),
                };
                let (w, h) = (decoded.width(), decoded.height());
                let mut buf = std::io::Cursor::new(Vec::new());
                if decoded.write_to(&mut buf, image::ImageFormat::Png).is_err() {
                    if allow_partial {
                        continue;
                    }
                    return Err(Error::conflict("failed to transcode evidence image to png"));
                }
                ("png", buf.into_inner(), w, h)
            }
        };
        let max_w_emu: i64 = 5_486_400;
        let w = i64::from(w_px.max(1));
        let h = i64::from(h_px.max(1));
        let cx = (w * 9525).min(max_w_emu);
        let cy = ((h * cx) / w).max(9525);
        let num = idx + 1;
        images.push(DocxImage {
            rel_id: format!("rIdImg{num}"),
            filename: format!("media/image{num}.{ext}"),
            ext,
            bytes: out_bytes,
            cx,
            cy,
            caption,
        });
    }
    Ok(images)
}

fn build_docx_from_parts(
    title: &str,
    report_id: &str,
    severity: &str,
    cvss: &str,
    markdown: &str,
    has_retest: bool,
    evidence_stale: bool,
    images: &[DocxImage],
) -> Api<Vec<u8>> {
    let mut body_xml = String::new();
    body_xml.push_str(&format!(
        "<w:p><w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr><w:r><w:t>{}</w:t></w:r></w:p>",
        xml_escape(title)
    ));
    body_xml.push_str(&format!(
        "<w:p><w:r><w:t xml:space=\"preserve\">Report ID: {} | Severity: {} | CVSS: {}</w:t></w:r></w:p>",
        xml_escape(report_id),
        xml_escape(severity),
        xml_escape(cvss),
    ));
    if !has_retest {
        body_xml.push_str(
            "<w:p><w:r><w:rPr><w:b/><w:color w:val=\"B45309\"/></w:rPr><w:t>【状态标注：未复测（待人工复核）】</w:t></w:r></w:p>",
        );
    }
    if evidence_stale {
        body_xml.push_str(
            "<w:p><w:r><w:rPr><w:b/><w:color w:val=\"B91C1C\"/></w:rPr><w:t>【提示：关联证据版本在冻结后发生变更】</w:t></w:r></w:p>",
        );
    }
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("## ") {
            body_xml.push_str(&format!(
                "<w:p><w:pPr><w:pStyle w:val=\"Heading2\"/></w:pPr><w:r><w:t>{}</w:t></w:r></w:p>",
                xml_escape(h)
            ));
        } else {
            body_xml.push_str(&format!(
                "<w:p><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                xml_escape(line)
            ));
        }
    }

    if !images.is_empty() {
        body_xml.push_str(
            "<w:p><w:pPr><w:pStyle w:val=\"Heading2\"/></w:pPr><w:r><w:t>截图证据</w:t></w:r></w:p>",
        );
        for (i, img) in images.iter().enumerate() {
            let docpr_id = i + 1;
            body_xml.push_str(&format!(
                "<w:p><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
                xml_escape(&img.caption)
            ));
            body_xml.push_str(&format!(
                "<w:p><w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\">\
<wp:extent cx=\"{cx}\" cy=\"{cy}\"/>\
<wp:docPr id=\"{docpr_id}\" name=\"{img_name}\"/>\
<wp:cNvGraphicFramePr><a:graphicFrameLocks noChangeAspect=\"1\"/></wp:cNvGraphicFramePr>\
<a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic><pic:nvPicPr><pic:cNvPr id=\"{docpr_id}\" name=\"{img_name}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip r:embed=\"{rel_id}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr>\
</pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>",
                cx = img.cx,
                cy = img.cy,
                docpr_id = docpr_id,
                img_name = xml_escape(&img.filename),
                rel_id = xml_escape(&img.rel_id),
            ));
        }
    }

    let document_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<w:body>{body_xml}<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/></w:sectPr></w:body></w:document>"
    );
    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Default Extension=\"png\" ContentType=\"image/png\"/>\
<Default Extension=\"jpeg\" ContentType=\"image/jpeg\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
</Types>";
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
</Relationships>";
    let img_rels: String = images
        .iter()
        .map(|img| {
            format!(
                "<Relationship Id=\"{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"{}\"/>",
                xml_escape(&img.rel_id),
                xml_escape(&img.filename)
            )
        })
        .collect();
    let doc_rels = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{img_rels}</Relationships>"
    );
    let styles_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/><w:rPr><w:b/><w:sz w:val=\"32\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading2\"><w:name w:val=\"heading 2\"/><w:rPr><w:b/><w:sz w:val=\"26\"/></w:rPr></w:style>\
</w:styles>";

    let buf = std::io::Cursor::new(Vec::new());
    let mut writer = ::zip::ZipWriter::new(buf);
    let opts = ::zip::write::SimpleFileOptions::default()
        .compression_method(::zip::CompressionMethod::Deflated);
    for (name, bytes) in [
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", rels.as_bytes()),
        ("word/_rels/document.xml.rels", doc_rels.as_bytes()),
        ("word/styles.xml", styles_xml.as_bytes()),
        ("word/document.xml", document_xml.as_bytes()),
    ] {
        writer.start_file(name, opts).map_err(super::db)?;
        writer.write_all(bytes).map_err(super::db)?;
    }
    for img in images {
        let entry_path = format!("word/{}", img.filename);
        writer.start_file(&entry_path, opts).map_err(super::db)?;
        writer.write_all(&img.bytes).map_err(super::db)?;
    }
    let cursor = writer.finish().map_err(super::db)?;
    Ok(cursor.into_inner())
}

fn docx_bytes(
    s: &AppState,
    task_id: &str,
    report: &Value,
    body: &Value,
    redactions: &[(String, String)],
) -> Api<(Vec<u8>, usize, bool, bool)> {
    let allow_partial = body["allow_partial"] == true;
    let rendered = report_render::render(s, task_id, report, body)?;
    let canonical = &rendered["canonical"];
    let title = canonical["title"].as_str().unwrap_or("SRC Security Finding Report");
    let markdown = rendered["markdown"].as_str().unwrap_or("");
    let severity = canonical["severity"].as_str().unwrap_or("");
    let cvss = canonical["cvss"]["score"]
        .as_f64()
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "—".to_owned());
    let (has_retest, evidence_stale) = report_retest_status(s, task_id, report);
    let images = collect_docx_images(s, task_id, report, redactions, allow_partial)?;
    let bytes = build_docx_from_parts(
        title,
        report["report_id"].as_str().unwrap_or(""),
        severity,
        &cvss,
        markdown,
        has_retest,
        evidence_stale,
        &images,
    )?;
    Ok((bytes, images.len(), has_retest, evidence_stale))
}

fn truncate_utf8_filename(raw: &str, max_chars: usize) -> String {
    let sanitized: String = sanitize_xml_text(raw)
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | ' ') || ('\u{4E00}'..='\u{9FFF}').contains(&c) {
                if c == ' ' { '-' } else { c }
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(['.', '-']);
    let base = if trimmed.is_empty() { "finding" } else { trimmed };
    let mut end = base.len();
    for (count, (byte_idx, _)) in base.char_indices().enumerate() {
        if count == max_chars {
            end = byte_idx;
            break;
        }
    }
    base[..end].trim_matches('-').to_owned()
}

fn docx_per_finding_zip(
    s: &AppState,
    task_id: &str,
    report: &Value,
    body: &Value,
    redactions: &[(String, String)],
) -> Api<Value> {
    let report_id = require(report, "report_id")?;
    let allow_partial = body["allow_partial"] == true;
    let all_reports = if body["all_frozen_reports"] == true {
        records(s, task_id, "frozen_report")?
            .into_iter()
            .map(|r| r["payload"].clone())
            .filter(|r| !r["review"]["readiness"].as_str().is_some_and(|v| v.eq_ignore_ascii_case("DRAFT")))
            .collect::<Vec<_>>()
    } else {
        vec![report.clone()]
    };

    let mut entries_meta = Vec::new();
    let mut output = ::zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = ::zip::write::SimpleFileOptions::default()
        .compression_method(::zip::CompressionMethod::Deflated);
    let mut used_names = HashSet::new();
    let mut total_idx = 0usize;

    for rep in &all_reports {
        let rep_id = rep["report_id"].as_str().unwrap_or(report_id);
        let rendered = report_render::render(s, task_id, rep, body)?;
        let canonical = &rendered["canonical"];
        let rep_title = canonical["title"].as_str().unwrap_or("SRC Finding");
        let rep_md = rendered["markdown"].as_str().unwrap_or("");
        let severity = canonical["severity"].as_str().unwrap_or("");
        let cvss = canonical["cvss"]["score"]
            .as_f64()
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "—".to_owned());
        let (has_retest, evidence_stale) = report_retest_status(s, task_id, rep);
        let images = collect_docx_images(s, task_id, rep, redactions, allow_partial)?;
        let claims = rep["claims"].as_array().cloned().unwrap_or_default();

        if claims.len() <= 1 {
            total_idx += 1;
            let short = truncate_utf8_filename(rep_title, 48);
            let mut fname = format!("{total_idx:02}-{short}.docx");
            while !used_names.insert(fname.clone()) {
                fname = format!("{total_idx:02}-{short}-{}.docx", used_names.len());
            }
            let bytes = build_docx_from_parts(
                rep_title,
                rep_id,
                severity,
                &cvss,
                rep_md,
                has_retest,
                evidence_stale,
                &images,
            )?;
            output.start_file(&fname, opts).map_err(super::db)?;
            output.write_all(&bytes).map_err(super::db)?;
            entries_meta.push(json!({"filename": fname, "report_id": rep_id, "title": rep_title, "embedded_images": images.len(), "size": bytes.len()}));
        } else {
            for claim in &claims {
                total_idx += 1;
                let c_title = claim["title"]
                    .as_str()
                    .or_else(|| claim["statement"].as_str())
                    .filter(|v| !v.is_empty())
                    .unwrap_or(rep_title);
                let short = truncate_utf8_filename(c_title, 48);
                let mut fname = format!("{total_idx:02}-{short}.docx");
                while !used_names.insert(fname.clone()) {
                    fname = format!("{total_idx:02}-{short}-{}.docx", used_names.len());
                }
                let claim_md = format!(
                    "## 漏洞主张详情\n{}\n\n{}",
                    serde_json::to_string_pretty(claim).unwrap_or_default(),
                    rep_md
                );
                let bytes = build_docx_from_parts(
                    c_title,
                    rep_id,
                    severity,
                    &cvss,
                    &claim_md,
                    has_retest,
                    evidence_stale,
                    &images,
                )?;
                output.start_file(&fname, opts).map_err(super::db)?;
                output.write_all(&bytes).map_err(super::db)?;
                entries_meta.push(json!({"filename": fname, "report_id": rep_id, "claim_id": claim["claim_id"], "title": c_title, "embedded_images": images.len(), "size": bytes.len()}));
            }
        }
    }

    let manifest = json!({
        "schema": "muteki-src-per-finding-docx.v1",
        "source_report_id": report_id,
        "docx_count": entries_meta.len(),
        "entries": entries_meta,
    });
    output.start_file("manifest.json", opts).map_err(super::db)?;
    output
        .write_all(&serde_json::to_vec_pretty(&manifest).map_err(super::db)?)
        .map_err(super::db)?;

    Ok(result(
        report,
        output.finish().map_err(super::db)?.into_inner(),
        "application/zip",
        format!("src-{report_id}-per-finding-docx.zip"),
        json!({"docx_count": total_idx, "manifest": manifest}),
    ))
}

pub(super) fn export(s: &AppState, task_id: &str, report: &Value, body: &Value) -> Api<Value> {
    let report_id = require(report, "report_id")?;
    if report["review"]["readiness"].as_str().is_some_and(|r| r.eq_ignore_ascii_case("DRAFT")) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "draft_cannot_export",
            "Draft finding report lacks baseline/quantitative evidence; complete supplement_evidence before export",
        ));
    }
    let format = body["format"].as_str().unwrap_or("zip");
    let redactions = rules(body)?;
    let redaction_detail = body["redactions"].as_array().cloned().unwrap_or_default();
    match format {
        "raw" => Ok(result(
            report,
            raw_http(s, task_id, report, &redactions)?,
            "message/http; charset=utf-8",
            format!("src-{report_id}.http"),
            json!({"derived_http1":true,"redactions":redaction_detail}),
        )),
        "har" => Ok(result(
            report,
            serde_json::to_vec(&har(s, task_id, report, &redactions)?).map_err(super::db)?,
            "application/json",
            format!("src-{report_id}.har"),
            json!({"wire_bytes_complete":false,"redactions":redaction_detail}),
        )),
        "jsonl" => Ok(result(
            report,
            jsonl(report, &redactions)?,
            "application/x-ndjson",
            format!("src-{report_id}.jsonl"),
            json!({"complete_objects":true,"redactions":redaction_detail}),
        )),
        "md" | "markdown" => {
            let rendered = report_render::render(s, task_id, report, body)?;
            Ok(result(
                report,
                rendered["markdown"]
                    .as_str()
                    .unwrap_or("")
                    .as_bytes()
                    .to_vec(),
                "text/markdown; charset=utf-8",
                format!("src-{report_id}.md"),
                json!({}),
            ))
        }
        "json" => {
            let rendered = report_render::render(s, task_id, report, body)?;
            let redacted = !redactions.is_empty() || body["redact_auth"] == true;
            Ok(result(
                report,
                serde_json::to_vec_pretty(&rendered["canonical"]).map_err(super::db)?,
                "application/json",
                format!("src-{report_id}.json"),
                json!({"redacted": redacted, "redactions": rendered["redactions"]}),
            ))
        }
        "zip" => zip(s, task_id, report, body, &redactions),
        "docx" => {
            let (bytes, embedded_images, has_retest, evidence_stale) =
                docx_bytes(s, task_id, report, body, &redactions)?;
            Ok(result(
                report,
                bytes,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                format!("src-{report_id}.docx"),
                json!({
                    "ooxml_valid": true,
                    "embedded_images": embedded_images,
                    "retested": has_retest,
                    "evidence_stale": evidence_stale,
                    "redacted": !redactions.is_empty() || body["redact_auth"] == true
                }),
            ))
        }
        "docx_per_finding" => docx_per_finding_zip(s, task_id, report, body, &redactions),
        _ => Err(Error::conflict(format!(
            "unsupported export format {format:?}"
        ))),
    }
}

fn filename(value: &str, fallback: &str) -> String {
    let normalized = value.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or("");
    let mut cleaned = String::new();
    for character in name.chars() {
        if character.is_alphanumeric() || matches!(character, '.' | '_' | '-') {
            cleaned.push(character);
        } else if !cleaned.ends_with('-') {
            cleaned.push('-');
        }
    }
    let cleaned = cleaned.trim_matches(['.', '-']);
    if cleaned.is_empty() {
        fallback.to_owned()
    } else {
        cleaned.to_owned()
    }
}

struct Entry {
    content: Vec<u8>,
    media_type: String,
    classification: &'static str,
    source_version: String,
    purpose: String,
    artifact_id: Option<String>,
}

fn add(files: &mut BTreeMap<String, Entry>, path: String, entry: Entry) -> Api<()> {
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part == "..") {
        return Err(Error::conflict("unsafe ZIP destination"));
    }
    if let Some(existing) = files.get(&path) {
        if existing.content != entry.content {
            return Err(Error::conflict(format!("ZIP path collision: {path}")));
        }
    }
    files.insert(path, entry);
    Ok(())
}

fn zip(
    s: &AppState,
    task_id: &str,
    report: &Value,
    body: &Value,
    redactions: &[(String, String)],
) -> Api<Value> {
    let report_id = require(report, "report_id")?;
    let allow_partial = body["allow_partial"] == true;
    let rendered = report_render::render(s, task_id, report, body)?;
    let mut issues = rendered["issues"].as_array().cloned().unwrap_or_default();
    let mut files = BTreeMap::new();
    let source_version = report["report_seq"].to_string();
    add(
        &mut files,
        "report.md".to_owned(),
        Entry {
            content: rendered["markdown"]
                .as_str()
                .unwrap_or("")
                .as_bytes()
                .to_vec(),
            media_type: "text/markdown".to_owned(),
            classification: "derived",
            source_version: source_version.clone(),
            purpose: "readable submission".to_owned(),
            artifact_id: None,
        },
    )?;
    add(
        &mut files,
        "report.json".to_owned(),
        Entry {
            content: serde_json::to_vec_pretty(&rendered["canonical"]).map_err(super::db)?,
            media_type: "application/json".to_owned(),
            classification: "derived",
            source_version: source_version.clone(),
            purpose: "canonical submission".to_owned(),
            artifact_id: None,
        },
    )?;
    let reviewed_images: HashSet<String> = records(s, task_id, "image_review")?
        .into_iter()
        .filter_map(|row| {
            let payload = &row["payload"];
            if payload["status"] == "reviewed"
                && payload["reviewer"].as_str().is_some_and(|v| !v.is_empty())
                && payload["reviewed_at"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty())
            {
                payload["artifact_id"]
                    .as_str()
                    .map(str::to_owned)
            } else {
                None
            }
        })
        .collect();
    let mut counts: Vec<usize> = rendered["redactions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| row["occurrences"].as_u64().unwrap_or(0) as usize)
        .collect();
    let mut references = report["evidence_refs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut seen: HashSet<String> = references
        .iter()
        .filter_map(|reference| {
            reference["artifact_id"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    for exchange in report["document"]["exchanges"]
        .as_array()
        .into_iter()
        .flatten()
    {
        for key in ["request_raw_ref", "response_raw_ref"] {
            let reference = &exchange[key];
            if let Some(id) = reference["artifact_id"]
                .as_str()
            {
                if seen.insert(id.to_owned()) {
                    references.push(reference.clone());
                }
            }
        }
    }
    for reference in references {
        let id = reference["artifact_id"]
            .as_str()
            .ok_or_else(|| Error::invalid("evidence reference has no artifact ID"))?;
        let name = reference["name"].as_str().unwrap_or("artifact.bin");
        let mut safe_name = name.to_owned();
        for (needle, replacement) in redactions {
            safe_name = safe_name.replace(needle, replacement);
        }
        let path = format!(
            "evidence/{}/{}",
            filename(id, "artifact"),
            filename(&safe_name, "artifact.bin")
        );
        let media_type = reference["media_type"]
            .as_str()
            .unwrap_or("application/octet-stream");
        let issue = if media_type.starts_with("image/") && !reviewed_images.contains(id) {
            Some(("image_not_reviewed", "图片没有实际查看记录".to_owned()))
        } else if media_type.starts_with("image/") && !redactions.is_empty() {
            Some((
                "image_redaction_required",
                "images require explicit pixel redaction before export".to_owned(),
            ))
        } else {
            None
        };
        if let Some((code, reason)) = issue {
            if !allow_partial {
                return Err(Error::new(StatusCode::CONFLICT, code, reason));
            }
            issues.push(json!({"path":path,"code":code,"reason":reason,"artifact_id":id}));
            continue;
        }
        match artifact_bytes(s, task_id, &reference) {
            Ok(content) => {
                let (content, found) = redact_bytes(content, redactions);
                for (index, count) in found.iter().enumerate() {
                    counts[index] += count;
                }
                add(
                    &mut files,
                    path,
                    Entry {
                        content,
                        media_type: media_type.to_owned(),
                        classification: if reference["original"] == true && redactions.is_empty() {
                            "original"
                        } else {
                            "derived"
                        },
                        source_version: reference["version"].as_u64().unwrap_or(1).to_string(),
                        purpose: safe_name,
                        artifact_id: Some(id.to_owned()),
                    },
                )?;
            }
            Err(error) if allow_partial => issues.push(json!({"path":path,"code":error.code,
                "reason":error.message,"artifact_id":id})),
            Err(error) => return Err(error),
        }
    }
    let poc = &rendered["canonical"]["document"]["poc"];
    if poc.is_object() && !poc.as_object().is_some_and(serde_json::Map::is_empty) {
        add(
            &mut files,
            "poc/dependencies.json".to_owned(),
            Entry {
                content: serde_json::to_vec_pretty(&json!({"dependencies":poc["dependencies"],
                "initialization":poc["initialization"],"success_assertion":poc["success_assertion"],
                "cleanup":poc["cleanup"],"command":poc["command"],
                "verification":poc["verification"].as_object().map(|_|poc["verification"].clone())
                    .unwrap_or(json!({"status":"not_verified"}))}))
                .map_err(super::db)?,
                media_type: "application/json".to_owned(),
                classification: "derived",
                source_version: source_version.clone(),
                purpose: "PoC execution instructions".to_owned(),
                artifact_id: None,
            },
        )?;
        if poc["content_ref"].is_object() {
            let path = format!(
                "poc/{}",
                filename(poc["filename"].as_str().unwrap_or("poc.py"), "poc.py")
            );
            match artifact_bytes(s, task_id, &poc["content_ref"]) {
                Ok(content) => {
                    let (content, found) = redact_bytes(content, redactions);
                    for (index, count) in found.iter().enumerate() {
                        counts[index] += count;
                    }
                    add(
                        &mut files,
                        path,
                        Entry {
                            content,
                            media_type: "text/x-python".to_owned(),
                            classification: if redactions.is_empty() {
                                "original"
                            } else {
                                "derived"
                            },
                            source_version: source_version.clone(),
                            purpose: "executable PoC".to_owned(),
                            artifact_id: poc["content_ref"]["artifact_id"]
                                .as_str()
                                .map(str::to_owned),
                        },
                    )?;
                }
                Err(error) if allow_partial => issues.push(json!({"path":"poc/content",
                    "code":error.code,"reason":error.message})),
                Err(error) => return Err(error),
            }
        }
    }
    for (index, count) in counts.iter().enumerate() {
        if *count == 0 {
            return Err(Error::conflict(format!(
                "redaction value was not found in the selected evidence: {}",
                redactions[index].0
            )));
        }
    }
    let mut unique_issues = Vec::new();
    for issue in issues {
        if !unique_issues.contains(&issue) {
            unique_issues.push(issue);
        }
    }
    let issues = unique_issues;
    let complete = issues.is_empty();
    let attachments: Vec<Value> = files
        .iter()
        .map(|(path, entry)| {
            let mut detail = json!({"path":path,"media_type":entry.media_type,
            "classification":entry.classification,"size":entry.content.len(),
            "source_version":entry.source_version,"purpose":entry.purpose});
            if let Some(id) = &entry.artifact_id {
                detail["artifact_id"] = json!(id);
            }
            detail
        })
        .collect();
    let verification = if poc["verification"].is_object() {
        poc["verification"].clone()
    } else {
        json!({"status":"not_verified"})
    };
    let mut manifest = json!({"schema":"muteki.src.evidence-bundle.v1","report_id":report_id,
        "report_seq":report["report_seq"],"renderer_version":"src-submission-renderer.v1",
        "template":report["template"],"source_watermarks":report["source_watermarks"],
        "complete":complete,"status":if complete{"complete"}else{"partial"},
        "expected_entry_count":attachments.len()+issues.len(),"actual_entry_count":attachments.len(),
        "omissions":issues,"redactions":rendered["redactions"],
        "poc_runnable":rendered["runnable"] == true && complete,
        "poc":{"command":poc["command"],"verification":verification},"attachments":attachments});
    let mut ignored = vec![0; redactions.len()];
    redact_value(&mut manifest, redactions, &mut ignored);
    let mut output = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (path, entry) in files {
        output.start_file(path, options).map_err(super::db)?;
        output.write_all(&entry.content).map_err(super::db)?;
    }
    output
        .start_file("manifest.json", options)
        .map_err(super::db)?;
    output
        .write_all(&serde_json::to_vec_pretty(&manifest).map_err(super::db)?)
        .map_err(super::db)?;
    Ok(result(
        report,
        output.finish().map_err(super::db)?.into_inner(),
        "application/zip",
        format!("src-{report_id}-evidence.zip"),
        json!({"complete":complete,"manifest":manifest}),
    ))
}
