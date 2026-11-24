//! Source difflib-style longest-match alignment, preserving original line endings.
use std::collections::HashMap;

fn lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    let (mut start, mut index) = (0, 0);
    while index < bytes.len() {
        let width = if bytes[index..].starts_with(b"\r\n") {
            2
        } else if matches!(
            bytes[index],
            b'\n' | b'\r' | 0x0b | 0x0c | 0x1c | 0x1d | 0x1e
        ) {
            1
        } else if bytes[index..].starts_with(&[0xc2, 0x85]) {
            2
        } else if bytes[index..].starts_with(&[0xe2, 0x80, 0xa8])
            || bytes[index..].starts_with(&[0xe2, 0x80, 0xa9])
        {
            3
        } else {
            0
        };
        if width > 0 {
            index += width;
            result.push(&bytes[start..index]);
            start = index;
        } else {
            index += 1;
        }
    }
    if start < bytes.len() {
        result.push(&bytes[start..]);
    }
    result
}

#[derive(Clone, Copy)]
struct Span {
    equal: bool,
    a: usize,
    ae: usize,
    b: usize,
    be: usize,
}

fn spans(a: &[&[u8]], b: &[&[u8]]) -> Vec<Span> {
    let mut positions: HashMap<&[u8], Vec<usize>> = HashMap::new();
    for (index, line) in b.iter().enumerate() {
        positions.entry(line).or_default().push(index);
    }
    if b.len() >= 200 {
        positions.retain(|_, indexes| indexes.len() <= b.len() / 100 + 1);
    }
    let mut regions = vec![(0, a.len(), 0, b.len())];
    let mut matches = Vec::new();
    while let Some((lo, hi, blo, bhi)) = regions.pop() {
        let (mut best_a, mut best_b, mut size) = (lo, blo, 0);
        let mut prior: HashMap<usize, usize> = HashMap::new();
        for i in lo..hi {
            let mut current = HashMap::new();
            for &j in positions.get(a[i]).into_iter().flatten() {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let length = j
                    .checked_sub(1)
                    .and_then(|p| prior.get(&p))
                    .copied()
                    .unwrap_or(0)
                    + 1;
                current.insert(j, length);
                if length > size {
                    (best_a, best_b, size) = (i + 1 - length, j + 1 - length, length);
                }
            }
            prior = current;
        }
        while best_a > lo && best_b > blo && a[best_a - 1] == b[best_b - 1] {
            best_a -= 1;
            best_b -= 1;
            size += 1;
        }
        while best_a + size < hi && best_b + size < bhi && a[best_a + size] == b[best_b + size] {
            size += 1;
        }
        if size == 0 {
            continue;
        }
        matches.push((best_a, best_b, size));
        if lo < best_a && blo < best_b {
            regions.push((lo, best_a, blo, best_b));
        }
        if best_a + size < hi && best_b + size < bhi {
            regions.push((best_a + size, hi, best_b + size, bhi));
        }
    }
    matches.sort_unstable();
    let mut merged: Vec<(usize, usize, usize)> = Vec::new();
    for (i, j, n) in matches {
        if let Some(last) = merged.last_mut() {
            if last.0 + last.2 == i && last.1 + last.2 == j {
                last.2 += n;
                continue;
            }
        }
        merged.push((i, j, n));
    }
    merged.push((a.len(), b.len(), 0));
    let (mut ai, mut bi) = (0, 0);
    let mut result = Vec::new();
    for (i, j, n) in merged {
        if ai < i || bi < j {
            result.push(Span {
                equal: false,
                a: ai,
                ae: i,
                b: bi,
                be: j,
            });
        }
        if n > 0 {
            result.push(Span {
                equal: true,
                a: i,
                ae: i + n,
                b: j,
                be: j + n,
            });
        }
        (ai, bi) = (i + n, j + n);
    }
    result
}

fn range(start: usize, end: usize) -> String {
    match end - start {
        0 => format!("{start},0"),
        1 => (start + 1).to_string(),
        n => format!("{},{n}", start + 1),
    }
}

fn emit(output: &mut String, group: &[Span], a: &[&[u8]], b: &[&[u8]]) {
    let (first, last) = (group.first().unwrap(), group.last().unwrap());
    output.push_str(&format!(
        "@@ -{} +{} @@\n",
        range(first.a, last.ae),
        range(first.b, last.be)
    ));
    for span in group {
        for line in &a[span.a..span.ae] {
            output.push(if span.equal { ' ' } else { '-' });
            output.push_str(&String::from_utf8_lossy(line));
        }
        if !span.equal {
            for line in &b[span.b..span.be] {
                output.push('+');
                output.push_str(&String::from_utf8_lossy(line));
            }
        }
    }
}

pub(super) fn unified(left: &[u8], right: &[u8], side: &str) -> String {
    if left == right {
        return String::new();
    }
    let (a, b) = (lines(left), lines(right));
    let mut codes = spans(&a, &b);
    if let Some(first) = codes.first_mut().filter(|s| s.equal) {
        first.a = first.a.max(first.ae.saturating_sub(3));
        first.b = first.b.max(first.be.saturating_sub(3));
    }
    if let Some(last) = codes.last_mut().filter(|s| s.equal) {
        last.ae = last.ae.min(last.a + 3);
        last.be = last.be.min(last.b + 3);
    }
    let mut result = format!("--- baseline/{side}\n+++ experiment/{side}\n");
    let mut group = Vec::new();
    for mut span in codes {
        if span.equal && span.ae - span.a > 6 {
            group.push(Span {
                ae: span.a + 3,
                be: span.b + 3,
                ..span
            });
            emit(&mut result, &group, &a, &b);
            group.clear();
            span.a = span.ae - 3;
            span.b = span.be - 3;
        }
        group.push(span);
    }
    if group.iter().any(|span| !span.equal) {
        emit(&mut result, &group, &a, &b);
    }
    result
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn is_volatile_field(path: &str, left: &serde_json::Value, right: &serde_json::Value) -> bool {
    let key = path.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
    if matches!(
        key.as_str(),
        "timestamp" | "ts" | "nonce" | "trace_id" | "request_id" | "req_id" | "date" | "expires_at"
    ) {
        return true;
    }
    let hex_or_token = |v: &serde_json::Value| {
        v.as_str().is_some_and(|s| {
            s.len() >= 24 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    };
    hex_or_token(left) && hex_or_token(right)
}

fn walk_json_diff(
    path: &str,
    left: &serde_json::Value,
    right: &serde_json::Value,
    added: &mut Vec<String>,
    removed: &mut Vec<String>,
    type_changed: &mut Vec<serde_json::Value>,
    value_changed: &mut Vec<serde_json::Value>,
    volatile: &mut Vec<String>,
) {
    use serde_json::{Value, json};
    if left == right {
        return;
    }
    if json_type_name(left) != json_type_name(right) {
        type_changed.push(json!({
            "path": if path.is_empty() { "/" } else { path },
            "from_type": json_type_name(left),
            "to_type": json_type_name(right),
        }));
        return;
    }
    match (left, right) {
        (Value::Object(lm), Value::Object(rm)) => {
            for (k, lv) in lm {
                let next = format!("{path}/{k}");
                match rm.get(k) {
                    Some(rv) => walk_json_diff(&next, lv, rv, added, removed, type_changed, value_changed, volatile),
                    None => removed.push(next),
                }
            }
            for k in rm.keys() {
                if !lm.contains_key(k) {
                    added.push(format!("{path}/{k}"));
                }
            }
        }
        (Value::Array(la), Value::Array(ra)) => {
            let shared = la.len().min(ra.len());
            for i in 0..shared {
                walk_json_diff(&format!("{path}/{i}"), &la[i], &ra[i], added, removed, type_changed, value_changed, volatile);
            }
            for i in shared..ra.len() {
                added.push(format!("{path}/{i}"));
            }
            for i in shared..la.len() {
                removed.push(format!("{path}/{i}"));
            }
        }
        _ => {
            let ptr = if path.is_empty() { "/".to_owned() } else { path.to_owned() };
            let is_vol = is_volatile_field(&ptr, left, right);
            if is_vol {
                volatile.push(ptr.clone());
            }
            value_changed.push(json!({
                "path": ptr,
                "from": left,
                "to": right,
                "volatile": is_vol,
            }));
        }
    }
}

pub(super) fn json_structural_diff(left: &serde_json::Value, right: &serde_json::Value) -> serde_json::Value {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut type_changed = Vec::new();
    let mut value_changed = Vec::new();
    let mut volatile = Vec::new();
    walk_json_diff("", left, right, &mut added, &mut removed, &mut type_changed, &mut value_changed, &mut volatile);
    added.sort();
    removed.sort();
    serde_json::json!({
        "added_keys": added,
        "removed_keys": removed,
        "type_changed": type_changed,
        "value_changed": value_changed,
        "high_entropy_or_timestamp_fields": volatile,
        "structurally_identical": added.is_empty() && removed.is_empty() && type_changed.is_empty(),
    })
}
