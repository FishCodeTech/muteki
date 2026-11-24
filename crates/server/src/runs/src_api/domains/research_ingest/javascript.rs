//! JavaScript lexical research records for a SRC Task.
use super::verify_ref;
use crate::AppState;
use sha2::{Digest,Sha256};
use crate::runs::src_api::{
    Api, Error, RECORDS, RESEARCH_JAVASCRIPT_APPLIED, db, now, rec, records, require, save_rec, stage_rec,
};
use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

const EXTRACTOR: &str = "src.javascript-static.v1";
const HEADS: &str = "src_research_javascript_heads";
const ARTIFACTS: &str = "src_artifacts";

#[derive(Clone)]
struct Token {
    kind: &'static str,
    value: String,
    line: usize,
    column: usize,
    end_line: usize,
    end_column: usize,
}
fn advance(ch: char, line: &mut usize, column: &mut usize) {
    if ch == '\n' {
        *line += 1;
        *column = 1;
    } else {
        *column += 1;
    }
}
fn code_tokens(segment: &str, first_line: usize, first_column: usize, out: &mut Vec<Token>) {
    let chars: Vec<char> = segment.chars().collect();
    let multi = [
        "===", "!==", ">>>", "=>", "==", "!=", "<=", ">=", "&&", "||", "??", "?.", "...",
    ];
    let (mut i, mut line, mut column) = (0, first_line, first_column);
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            advance(ch, &mut line, &mut column);
            i += 1;
            continue;
        }
        if let Some(op) = multi.iter().find(|op| {
            let value: Vec<char> = op.chars().collect();
            chars
                .get(i..i + value.len())
                .is_some_and(|slice| slice == value)
        }) {
            let length = op.chars().count();
            out.push(Token {
                kind: "symbol",
                value: (*op).to_owned(),
                line,
                column,
                end_line: line,
                end_column: column + length,
            });
            i += length;
            column += length;
            continue;
        }
        if ch.is_alphabetic() || matches!(ch, '_' | '$') {
            let first = i;
            let start = column;
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$')) {
                i += 1;
            }
            column += i - first;
            out.push(Token {
                kind: "symbol",
                value: chars[first..i].iter().collect(),
                line,
                column: start,
                end_line: line,
                end_column: column,
            });
            continue;
        }
        if ch.is_numeric() {
            let first = i;
            let start = column;
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '.' | '_')) {
                i += 1;
            }
            column += i - first;
            out.push(Token {
                kind: "number",
                value: chars[first..i].iter().collect(),
                line,
                column: start,
                end_line: line,
                end_column: column,
            });
            continue;
        }
        out.push(Token {
            kind: "symbol",
            value: ch.to_string(),
            line,
            column,
            end_line: line,
            end_column: column + 1,
        });
        i += 1;
        column += 1;
    }
}
fn emit_code(buffer: &mut String, line: usize, column: usize, out: &mut Vec<Token>) {
    if !buffer.is_empty() {
        code_tokens(buffer, line, column, out);
        buffer.clear();
    }
}
fn can_start_regex(buffer: &str, out: &[Token]) -> bool {
    let trimmed = buffer.trim_end();
    if let Some(last_ch) = trimmed.chars().last() {
        if last_ch.is_alphanumeric() || matches!(last_ch, '_' | '$' | ')' | ']') {
            let word: String = trimmed
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || matches!(*c, '_' | '$'))
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            return matches!(
                word.as_str(),
                "return" | "case" | "throw" | "typeof" | "void" | "delete" | "in" | "of" | "instanceof" | "new" | "await" | "yield" | "else" | "do"
            );
        }
        return true;
    }
    match out.last() {
        None => true,
        Some(tok) if tok.kind != "symbol" => false,
        Some(tok) => matches!(
            tok.value.as_str(),
            "(" | "," | "=" | ":" | "[" | "!" | "&" | "|" | "?" | "{" | "}" | ";" | "&&" | "||" | "??" | "=>" | "==" | "===" | "!=" | "!==" | "+" | "-" | "*" | "%" | "<" | ">" | "<=" | ">=" | "return" | "case" | "throw" | "typeof" | "void" | "delete" | "new" | "await" | "yield"
        ),
    }
}

fn tokens(content: &str) -> Vec<Token> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Code,
        LineComment,
        BlockComment,
        String,
        Template,
        Regex,
    }
    let chars: Vec<char> = content.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line, mut column) = (0, 1, 1);
    let (mut start_line, mut start_column) = (1, 1);
    let (mut state, mut quote) = (State::Code, '\0');
    let mut in_regex_class = false;
    let mut buffer = String::new();
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied().unwrap_or('\0');
        match state {
            State::Code => {
                if matches!(ch, '"' | '\'' | '\u{60}') {
                    emit_code(&mut buffer, start_line, start_column, &mut out);
                    state = if ch == '\u{60}' {
                        State::Template
                    } else {
                        State::String
                    };
                    quote = ch;
                    start_line = line;
                    start_column = column;
                } else if ch == '/' && next == '/' {
                    emit_code(&mut buffer, start_line, start_column, &mut out);
                    state = State::LineComment;
                } else if ch == '/' && next == '*' {
                    emit_code(&mut buffer, start_line, start_column, &mut out);
                    state = State::BlockComment;
                } else if ch == '/' && can_start_regex(&buffer, &out) {
                    emit_code(&mut buffer, start_line, start_column, &mut out);
                    state = State::Regex;
                    in_regex_class = false;
                    start_line = line;
                    start_column = column;
                } else {
                    if buffer.is_empty() {
                        start_line = line;
                        start_column = column;
                    }
                    buffer.push(ch);
                }
            }
            State::Regex => {
                if ch == '\\' && next != '\0' && next != '\n' {
                    advance(ch, &mut line, &mut column);
                    i += 1;
                    advance(next, &mut line, &mut column);
                    i += 1;
                    continue;
                } else if ch == '[' {
                    in_regex_class = true;
                } else if ch == ']' {
                    in_regex_class = false;
                } else if ch == '\n' {
                    state = State::Code;
                    start_line = line + 1;
                    start_column = 1;
                } else if ch == '/' && !in_regex_class {
                    while chars.get(i + 1).is_some_and(|f| f.is_ascii_alphabetic()) {
                        advance(chars[i], &mut line, &mut column);
                        i += 1;
                    }
                    out.push(Token {
                        kind: "regex",
                        value: String::new(),
                        line: start_line,
                        column: start_column,
                        end_line: line,
                        end_column: column + 1,
                    });
                    state = State::Code;
                    start_line = line;
                    start_column = column + 1;
                }
            }
            State::LineComment => {
                if ch == '\n' {
                    state = State::Code;
                    start_line = line + 1;
                    start_column = 1;
                }
            }
            State::BlockComment => {
                if ch == '*' && next == '/' {
                    state = State::Code;
                    advance(ch, &mut line, &mut column);
                    i += 1;
                    advance('/', &mut line, &mut column);
                    i += 1;
                    start_line = line;
                    start_column = column;
                    continue;
                }
            }
            State::String | State::Template => {
                if ch == '\\' {
                    buffer.push(ch);
                    if next != '\0' {
                        buffer.push(next);
                        advance(ch, &mut line, &mut column);
                        i += 1;
                        advance(next, &mut line, &mut column);
                        i += 1;
                        continue;
                    }
                } else if ch == quote {
                    out.push(Token {
                        kind: "string",
                        value: std::mem::take(&mut buffer),
                        line: start_line,
                        column: start_column,
                        end_line: line,
                        end_column: column,
                    });
                    state = State::Code;
                    start_line = line;
                    start_column = column + 1;
                } else {
                    buffer.push(ch);
                }
            }
        }
        advance(ch, &mut line, &mut column);
        i += 1;
    }
    if matches!(state, State::String | State::Template) {
        out.push(Token {
            kind: "unterminated_string",
            value: buffer,
            line: start_line,
            column: start_column,
            end_line: line,
            end_column: column,
        });
    } else {
        emit_code(&mut buffer, start_line, start_column, &mut out);
    }
    out
}

#[derive(Clone)]
struct Scope {
    name: String,
    depth: usize,
    start: usize,
    end: usize,
}
fn identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_alphabetic() || matches!(first, '_' | '$'))
        && chars.all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '$'))
}
fn scopes(tokens: &[Token]) -> Vec<Scope> {
    let mut result = Vec::new();
    let mut active = Vec::new();
    let mut pending: Option<String> = None;
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != "symbol" {
            continue;
        }
        match token.value.as_str() {
            "function" => {
                let name = tokens
                    .iter()
                    .skip(index + 1)
                    .take(3)
                    .map(|token| token.value.as_str())
                    .find(|value| identifier(value) && !matches!(*value, "function" | "async"))
                    .unwrap_or("<anonymous>");
                pending = Some(name.to_owned());
            }
            "=>" => {
                let mut name = "<arrow>".to_owned();
                for previous in (0..index).rev() {
                    let value = tokens[previous].value.as_str();
                    if matches!(value, "=" | ":")
                        && previous > 0
                        && identifier(&tokens[previous - 1].value)
                    {
                        name = tokens[previous - 1].value.clone();
                        break;
                    }
                    if matches!(value, ";" | "{" | "}") {
                        break;
                    }
                }
                pending = Some(name);
            }
            "{" => {
                depth += 1;
                if let Some(name) = pending.take() {
                    active.push(Scope {
                        name,
                        depth,
                        start: index,
                        end: tokens.len(),
                    });
                }
            }
            "}" => {
                if active.last().is_some_and(|scope| scope.depth == depth) {
                    let mut scope = active.pop().unwrap();
                    scope.end = index;
                    result.push(scope);
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    result.extend(active);
    result
}
fn function_name(index: usize, scopes: &[Scope]) -> &str {
    scopes
        .iter()
        .filter(|scope| scope.start <= index && index <= scope.end)
        .min_by_key(|scope| scope.end - scope.start)
        .map(|scope| scope.name.as_str())
        .unwrap_or("<top-level>")
}
fn looks_like_endpoint(value: &str) -> bool {
    let text = value.trim();
    if text.is_empty() || text.chars().any(char::is_whitespace) {
        return false;
    }
    if text.starts_with('/') && text.len() > 1 && !text.starts_with("//") {
        return true;
    }
    if let Some(index) = text.find("://") {
        let scheme = &text[..index];
        if scheme.chars().next().is_some_and(|ch| ch.is_alphabetic())
            && scheme
                .chars()
                .skip(1)
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '+' | '.' | '-'))
        {
            return text.len() > index + 3;
        }
    }
    false
}
fn looks_like_callsite_api_path(value: &str) -> bool {
    if looks_like_endpoint(value) {
        return true;
    }
    let text = value.trim();
    if text.is_empty()
        || text.len() > 256
        || text.chars().any(char::is_whitespace)
        || text.starts_with('.')
        || text.starts_with("//")
    {
        return false;
    }
    let clean = text.split(['?', '#']).next().unwrap_or(text).to_ascii_lowercase();
    const STATIC_EXTS: &[&str] = &[
        ".js", ".mjs", ".cjs", ".css", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".ico",
        ".woff", ".woff2", ".ttf", ".eot", ".map", ".webp", ".wasm", ".html",
    ];
    if STATIC_EXTS.iter().any(|ext| clean.ends_with(ext)) {
        return false;
    }
    if matches!(
        clean.as_str(),
        "true" | "false" | "null" | "undefined" | "get" | "post" | "put" | "patch" | "delete"
            | "head" | "options" | "json" | "text" | "blob" | "arraybuffer" | "document"
            | "application/json"
    ) {
        return false;
    }
    text.chars().next().is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '{')
        && text.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '/' | '_' | '-' | '.' | '{' | '}' | ':' | '?' | '&' | '=' | '%')
        })
}
fn decode_js_string_token(token: &Token) -> String {
    let mut value = token.value.clone();
    if token.kind == "string" && value.contains('\\') {
        let quoted = format!("\"{}\"", value.replace('"', "\\\""));
        if let Ok(decoded) = serde_json::from_str::<String>(&quoted) {
            value = decoded;
        }
    }
    value
}
fn fold_redundant_dup_segments(url: &reqwest::Url) -> Option<String> {
    let mut segs: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None;
    }
    let mut changed = false;
    loop {
        let mut folded_once = false;
        let max_k = segs.len() / 2;
        'outer: for k in 1..=max_k {
            for i in 0..=segs.len() - 2 * k {
                if segs[i..i + k] == segs[i + k..i + 2 * k] {
                    segs.drain(i + k..i + 2 * k);
                    folded_once = true;
                    changed = true;
                    break 'outer;
                }
            }
        }
        if !folded_once {
            break;
        }
    }
    if !changed {
        return None;
    }
    let mut next = url.clone();
    next.set_path(&format!("/{}", segs.join("/")));
    Some(next.to_string())
}
fn normalize_endpoint_template(raw: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && chars.get(i + 1) == Some(&'{') {
            i += 2;
            let mut depth = 1usize;
            while i < chars.len() && depth > 0 {
                if chars[i] == '{' {
                    depth += 1;
                } else if chars[i] == '}' {
                    depth -= 1;
                }
                i += 1;
            }
            out.push_str("{id}");
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out.split('/')
        .map(|seg| {
            if seg.starts_with(':') && seg.len() > 1 {
                "{id}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn join_base_url(base: &str, rel: &str) -> String {
    if rel.contains("://") {
        return rel.to_owned();
    }
    let b = base.trim_end_matches('/');
    let r = rel.trim_start_matches('/');
    if b.is_empty() {
        format!("/{r}")
    } else if !b.starts_with('/') && !b.contains("://") {
        format!("/{b}/{r}")
    } else if r.starts_with(b.trim_start_matches('/')) {
        format!("/{r}")
    } else {
        format!("{b}/{r}")
    }
}

fn extract_object_keys(tokens: &[Token], open_brace: usize) -> (Vec<String>, Vec<String>, Vec<String>) {
    const CONFIG_KEYS: &[&str] = &[
        "headers", "timeout", "withCredentials", "credentials", "signal", "mode", "cache",
        "redirect", "referrer", "referrerPolicy", "integrity", "keepalive", "responseType",
        "baseURL", "prefixUrl", "method", "url", "adapter", "transformRequest", "transformResponse",
        "validateStatus", "maxRedirects", "onUploadProgress", "onDownloadProgress", "retry",
    ];
    let mut depth = 0usize;
    let mut top_keys = Vec::new();
    let mut nested_params = Vec::new();
    let mut auth_signals = Vec::new();
    let mut in_params_depth: Option<usize> = None;
    let end_limit = (open_brace + 160).min(tokens.len());
    for idx in open_brace..end_limit {
        let tok = &tokens[idx];
        if tok.kind == "string" {
            let lower = tok.value.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "authorization" | "x-token" | "x-auth-token" | "x-api-key" | "x-csrf-token" | "satoken"
            ) || lower.starts_with("bearer ")
            {
                if !auth_signals.contains(&tok.value) {
                    auth_signals.push(tok.value.clone());
                }
            }
        }
        if tok.kind != "symbol" {
            continue;
        }
        match tok.value.as_str() {
            "{" => {
                depth += 1;
            }
            "}" => {
                if in_params_depth == Some(depth) {
                    in_params_depth = None;
                }
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            val if identifier(val) => {
                let next_sym = tokens.get(idx + 1).map(|t| t.value.as_str()).unwrap_or("");
                if depth == 1 && matches!(next_sym, ":" | "," | "}") {
                    if matches!(val, "params" | "query" | "searchParams")
                        && tokens.get(idx + 2).map(|t| t.value.as_str()) == Some("{")
                    {
                        in_params_depth = Some(2);
                    } else if matches!(val, "data" | "body" | "json")
                        && tokens.get(idx + 2).map(|t| t.value.as_str()) == Some("{")
                    {
                        let sub_open = idx + 2;
                        let (sub_keys, _, _) = extract_object_keys(tokens, sub_open);
                        for k in sub_keys {
                            if !top_keys.contains(&k) {
                                top_keys.push(k);
                            }
                        }
                    } else if !CONFIG_KEYS.contains(&val) && !top_keys.contains(&val.to_owned()) {
                        top_keys.push(val.to_owned());
                    }
                } else if in_params_depth == Some(depth)
                    && matches!(next_sym, ":" | "," | "}")
                    && !nested_params.contains(&val.to_owned())
                {
                    nested_params.push(val.to_owned());
                }
            }
            _ => {}
        }
    }
    (top_keys, nested_params, auth_signals)
}

fn extract_scoped_clients_and_contracts(
    tokens: &[Token],
    source_url: &str,
) -> (BTreeMap<String, String>, Vec<Value>) {
    let mut clients = BTreeMap::<String, String>::new();
    for i in 0..tokens.len() {
        if tokens[i].kind == "symbol" && matches!(tokens[i].value.as_str(), "baseURL" | "prefixUrl") {
            if tokens.get(i + 1).map(|t| t.value.as_str()) == Some(":")
                && let Some(val_tok) = tokens.get(i + 2).filter(|t| t.kind == "string")
            {
                let base = val_tok.value.trim();
                if base.is_empty() {
                    continue;
                }
                let mut client_name = "<module_default>".to_owned();
                for back in (i.saturating_sub(18)..i).rev() {
                    if tokens[back].kind == "symbol"
                        && tokens[back].value == "="
                        && back > 0
                        && identifier(&tokens[back - 1].value)
                    {
                        client_name = tokens[back - 1].value.clone();
                        break;
                    }
                }
                clients.insert(client_name, base.to_owned());
            }
        }
    }

    let mut contracts = Vec::new();
    for i in 0..tokens.len() {
        if tokens[i].kind != "symbol" {
            continue;
        }
        let sym = tokens[i].value.as_str();
        if matches!(sym, "useQuery" | "useMutation") && i >= 4 && tokens[i - 1].value == "." {
            let mut chain = Vec::new();
            let mut cur = i - 1;
            while cur > 0 && tokens[cur].value == "." && identifier(&tokens[cur - 1].value) {
                chain.push(tokens[cur - 1].value.clone());
                if cur < 2 {
                    break;
                }
                cur -= 2;
            }
            chain.reverse();
            if chain.first().is_some_and(|root| matches!(root.as_str(), "trpc" | "api")) && chain.len() >= 2 {
                let proc_path = format!("/api/trpc/{}", chain[1..].join("."));
                let method = if sym == "useMutation" { "POST" } else { "GET" };
                contracts.push(json!({
                    "method": method,
                    "raw_path": proc_path,
                    "resolved_path": proc_path,
                    "client": chain[0],
                    "query_params": [],
                    "body_params": [],
                    "auth_headers": [],
                    "module": source_url,
                    "line": tokens[i].line,
                }));
                continue;
            }
        }

        let next_is_paren = tokens.get(i + 1).map(|t| t.value.as_str()) == Some("(");
        if !next_is_paren {
            continue;
        }
        let receiver = if i >= 2 && tokens[i - 1].value == "." && identifier(&tokens[i - 2].value) {
            tokens[i - 2].value.as_str()
        } else {
            ""
        };
        let http_method = match sym.to_ascii_lowercase().as_str() {
            "get" => Some("GET"),
            "post" => Some("POST"),
            "put" => Some("PUT"),
            "patch" => Some("PATCH"),
            "delete" => Some("DELETE"),
            "fetch" | "$fetch" | "ofetch" | "usefetch" => Some("FETCH"),
            "open" if !receiver.is_empty() => Some("XHR_OPEN"),
            _ => None,
        };
        let Some(mut method) = http_method else {
            continue;
        };
        let (path_tok_idx, arg_search_start) = if method == "XHR_OPEN" {
            let Some(m_tok) = tokens.get(i + 2).filter(|t| t.kind == "string") else {
                continue;
            };
            let upper = m_tok.value.to_ascii_uppercase();
            method = match upper.as_str() {
                "POST" => "POST",
                "PUT" => "PUT",
                "PATCH" => "PATCH",
                "DELETE" => "DELETE",
                _ => "GET",
            };
            (i + 4, i + 5)
        } else {
            (i + 2, i + 3)
        };
        let Some(path_tok) = tokens.get(path_tok_idx).filter(|t| t.kind == "string") else {
            continue;
        };
        let norm_path = normalize_endpoint_template(path_tok.value.trim());
        if !looks_like_callsite_api_path(&norm_path) {
            continue;
        }
        let mut query_params = Vec::new();
        let mut body_params = Vec::new();
        let mut auth_headers = Vec::new();
        let mut declared_method = None::<String>;
        for scan in arg_search_start..(arg_search_start + 24).min(tokens.len()) {
            if tokens[scan].kind == "symbol" && tokens[scan].value == ")" {
                break;
            }
            if tokens[scan].kind == "symbol" && tokens[scan].value == "{" {
                if method == "FETCH" {
                    let mut depth = 0usize;
                    for option in scan..tokens.len() {
                        match (tokens[option].kind, tokens[option].value.as_str()) {
                            ("symbol", "{") => depth += 1,
                            ("symbol", "}") => { depth -= 1; if depth == 0 { break; } },
                            (_, "method") if depth == 1 && tokens.get(option + 1).is_some_and(|token| token.value == ":") => {
                                declared_method = Some(tokens.get(option + 2).filter(|token| token.kind == "string")
                                    .map(|token| token.value.to_ascii_uppercase()).unwrap_or_else(|| "UNKNOWN".to_owned()));
                            }
                            _ => {}
                        }
                    }
                }
                let (top, nested, auth) = extract_object_keys(tokens, scan);
                auth_headers = auth;
                query_params = nested;
                if matches!(declared_method.as_deref().unwrap_or(method), "GET" | "DELETE" | "FETCH") && query_params.is_empty() {
                    if method != "FETCH" {
                        query_params = top;
                    }
                } else {
                    body_params = top;
                }
                break;
            }
        }
        let method = declared_method.as_deref().unwrap_or(if method == "FETCH" { "GET" } else { method });
        let resolved_path = if !receiver.is_empty()
            && let Some(base) = clients.get(receiver)
        {
            join_base_url(base, &norm_path)
        } else if matches!(receiver, "axios" | "http" | "request" | "client")
            && let Some(base) = clients.get("<module_default>")
        {
            join_base_url(base, &norm_path)
        } else {
            norm_path.clone()
        };
        contracts.push(json!({
            "method": method,
            "raw_path": norm_path,
            "resolved_path": resolved_path,
            "client": receiver,
            "query_params": query_params,
            "body_params": body_params,
            "auth_headers": auth_headers,
            "module": source_url,
            "line": path_tok.line,
        }));
    }
    (clients, contracts)
}

fn lexical_rows(
    content: &str,
    source_url: &str,
) -> (Vec<Value>, Vec<Value>, Vec<Value>, BTreeMap<String, String>, Vec<Value>, Vec<Value>) {
    let tokens = tokens(content);
    let scopes = scopes(&tokens);
    let (scoped_clients, api_contracts) = extract_scoped_clients_and_contracts(&tokens, source_url);
    let chunk_refs = extract_chunk_specifiers(&tokens, source_url);
    let keywords = [
        "if", "for", "while", "switch", "catch", "function", "return", "typeof", "delete", "void",
        "new", "await", "yield",
    ];
    let mut candidates = Vec::new();
    let mut calls = Vec::new();
    let mut errors = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let function = function_name(index, &scopes);
        if matches!(token.kind, "string" | "unterminated_string") {
            let value = decode_js_string_token(token);
            if looks_like_endpoint(&value) {
                let normalized = normalize_endpoint_template(&value);
                let matched_contract = api_contracts
                    .iter()
                    .find(|c| c["line"] == token.line && c["raw_path"] == normalized);
                candidates.push(json!({"candidate_type":"endpoint_string","value":value,
                    "raw_path":normalized,
                    "normalized_path":normalized,
                    "resolved_path":matched_contract.and_then(|c| c["resolved_path"].as_str()).unwrap_or(&normalized),
                    "client":matched_contract.map(|c| c["client"].clone()).unwrap_or(Value::Null),
                    "method":matched_contract.and_then(|c| c["method"].as_str()).unwrap_or("GET"),
                    "query_params":matched_contract.map(|c| c["query_params"].clone()).unwrap_or_else(|| json!([])),
                    "body_params":matched_contract.map(|c| c["body_params"].clone()).unwrap_or_else(|| json!([])),
                    "auth_headers":matched_contract.map(|c| c["auth_headers"].clone()).unwrap_or_else(|| json!([])),
                    "line":token.line,"column":token.column,
                    "end_line":token.end_line,"end_column":token.end_column,
                    "module":source_url,"function":function,
                    "evidence_state":"static_candidate","reachability":"unknown"}));
            }
        }
        if token.kind == "unterminated_string" {
            errors.push(json!({"location":format!("{}:{}",token.line,token.column),
                "code":"javascript_unterminated_string",
                "message":"string literal is unterminated"}));
        }
        if token.kind == "symbol"
            && identifier(&token.value)
            && !keywords.contains(&token.value.as_str())
        {
            let before = index
                .checked_sub(1)
                .filter(|previous| tokens[*previous].kind == "symbol")
                .map(|previous| tokens[previous].value.as_str())
                .unwrap_or("");
            let receiver = if before == "." && index > 1 && tokens[index - 2].kind == "symbol" {
                tokens[index - 2].value.as_str()
            } else {
                before
            };
            let after = tokens
                .get(index + 1)
                .filter(|next| next.kind == "symbol")
                .map(|next| next.value.as_str())
                .unwrap_or("");
            if matches!(after, "(" | "=") || before == "." {
                calls.push(
                    json!({"name":token.value,"receiver":receiver,"next_token":after,
                    "line":token.line,"column":token.column,"module":source_url,
                    "function":function,"evidence_state":"static_call_site",
                    "reachability":"unknown"}),
                );
            }
        }
    }
    for contract in &api_contracts {
        let rpath = contract["resolved_path"].as_str().unwrap_or("");
        if !rpath.is_empty() && !candidates.iter().any(|c| c["resolved_path"] == rpath && c["method"] == contract["method"]) {
            candidates.push(json!({
                "candidate_type": "api_contract",
                "value": rpath,
                "raw_path": contract["raw_path"],
                "normalized_path": rpath,
                "resolved_path": rpath,
                "client": contract["client"],
                "method": contract["method"],
                "query_params": contract["query_params"],
                "body_params": contract["body_params"],
                "auth_headers": contract["auth_headers"],
                "line": contract["line"],
                "column": 1,
                "end_line": contract["line"],
                "end_column": 1,
                "module": source_url,
                "function": "<contract>",
                "evidence_state": "static_candidate",
                "reachability": "unknown"
            }));
        }
    }
    (candidates, calls, errors, scoped_clients, api_contracts, chunk_refs)
}
fn vlq(segment: &str, offset: &mut usize) -> Result<i64, String> {
    const ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = segment.as_bytes();
    let (mut result, mut shift) = (0i64, 0usize);
    loop {
        let digit = bytes
            .get(*offset)
            .and_then(|byte| ALPHABET.as_bytes().iter().position(|item| item == byte))
            .ok_or_else(|| "invalid base64 VLQ mapping".to_owned())? as i64;
        *offset += 1;
        result += (digit & 31) << shift;
        if digit & 32 == 0 {
            let sign = if result & 1 == 1 { -1 } else { 1 };
            return Ok(sign * (result >> 1));
        }
        shift += 5;
    }
}
fn source_positions(mappings: &str) -> Result<BTreeMap<(i64, i64), (i64, i64, i64, i64)>, String> {
    let mut positions = BTreeMap::new();
    let (mut source_index, mut original_line, mut original_column, mut name_index) = (0, 0, 0, 0);
    for (generated_line, raw_line) in mappings.split(';').enumerate() {
        let mut generated_column = 0;
        for segment in raw_line.split(',').filter(|value| !value.is_empty()) {
            let mut fields = Vec::new();
            let mut offset = 0;
            while offset < segment.len() {
                fields.push(vlq(segment, &mut offset)?);
            }
            if !matches!(fields.len(), 1 | 4 | 5) {
                return Err("mapping segment has an invalid field count".to_owned());
            }
            generated_column += fields[0];
            if fields.len() >= 4 {
                source_index += fields[1];
                original_line += fields[2];
                original_column += fields[3];
                if fields.len() == 5 {
                    name_index += fields[4];
                }
                positions.insert(
                    (generated_line as i64 + 1, generated_column + 1),
                    (
                        source_index,
                        original_line + 1,
                        original_column + 1,
                        name_index,
                    ),
                );
            }
        }
    }
    Ok(positions)
}
fn map_candidates(candidates: &mut [Value], source_map: &[u8], errors: &mut Vec<Value>) -> usize {
    let parsed: Result<Value, _> = serde_json::from_slice(source_map);
    let source_map = match parsed {
        Ok(value) => value,
        Err(error) => {
            errors.push(json!({"location":"$.sourceMap","code":"source_map_invalid",
                "message":error.to_string()}));
            return 0;
        }
    };
    let (Some(sources), Some(mappings)) = (
        source_map["sources"].as_array(),
        source_map["mappings"].as_str(),
    ) else {
        errors.push(json!({"location":"$.sourceMap","code":"source_map_invalid",
            "message":"sources and mappings are required"}));
        return 0;
    };
    let positions = match source_positions(mappings) {
        Ok(positions) => positions,
        Err(error) => {
            errors.push(json!({"location":"$.sourceMap","code":"source_map_invalid",
                "message":error}));
            return 0;
        }
    };
    let mut mapped = 0;
    for candidate in candidates {
        let (Some(line), Some(column)) = (candidate["line"].as_i64(), candidate["column"].as_i64())
        else {
            continue;
        };
        let nearest = positions.range((line, 0)..=(line, column)).next_back();
        let Some((_, &(source_index, original_line, original_column, _))) = nearest else {
            continue;
        };
        let Ok(source_index) = usize::try_from(source_index) else {
            continue;
        };
        let Some(source) = sources.get(source_index) else {
            continue;
        };
        let available = source_map["sourcesContent"]
            .as_array()
            .and_then(|rows| rows.get(source_index))
            .is_some_and(|value| !value.is_null());
        candidate["original"] = json!({"source":source,"line":original_line,
            "column":original_column,"content_available":available});
        mapped += 1;
    }
    mapped
}
fn extract_chunk_specifiers(tokens: &[Token], source_url: &str) -> Vec<Value> {
    let mut refs = Vec::new();
    let mut seen = HashSet::new();
    let base = reqwest::Url::parse(source_url).ok();
    let origin_root = base.as_ref().map(|u| {
        let mut r = u.clone();
        r.set_path("/");
        r.set_query(None);
        r.set_fragment(None);
        r
    });
    for token in tokens {
        if !matches!(token.kind, "string" | "unterminated_string") {
            continue;
        }
        let decoded = decode_js_string_token(token);
        let value = decoded.trim();
        if value.is_empty()
            || value.chars().any(char::is_whitespace)
            || value.contains("${")
            || value.contains('{')
            || value.contains('*')
        {
            continue;
        }
        let clean = value.split(['?', '#']).next().unwrap_or(value);
        if clean.len() <= 3 || !(clean.ends_with(".js") || clean.ends_with(".mjs")) {
            continue;
        }
        if value.contains("://") && !value.starts_with("http://") && !value.starts_with("https://") {
            continue;
        }
        let is_bare_relative = !value.contains("://")
            && !value.starts_with('/')
            && !value.starts_with("./")
            && !value.starts_with("../");
        let (resolved, dup_fallback, alt_root) = if let Some(b) = base.as_ref() {
            if let Ok(joined) = b.join(value) {
                if let Some(folded) = fold_redundant_dup_segments(&joined) {
                    (folded, Some(joined.to_string()), None)
                } else {
                    let alt = if is_bare_relative && value.contains('/') {
                        origin_root
                            .as_ref()
                            .and_then(|root| root.join(value).ok())
                            .map(|u| u.to_string())
                            .filter(|u| *u != joined.as_str())
                    } else {
                        None
                    };
                    (joined.to_string(), None, alt)
                }
            } else {
                (value.to_owned(), None, None)
            }
        } else {
            (value.to_owned(), None, None)
        };
        if seen.insert(resolved.clone()) {
            let mut row = json!({
                "url": resolved,
                "source_value": value,
                "source_location": {
                    "line": token.line,
                    "column": token.column,
                    "module": source_url
                },
                "discovery_state": "static_reference_not_fetched"
            });
            if let Some(dup) = dup_fallback {
                row["dup_fallback_url"] = json!(dup);
            }
            if let Some(alt) = alt_root {
                row["alt_root_url"] = json!(alt);
            }
            refs.push(row);
        }
    }
    refs
}
fn artifact(
    s: &AppState,
    task_id: &str,
    bytes: &[u8],
    name: &str,
    media_type: &str,
    source_run_id: &Value,
) -> Api<(String, Value, Value)> {
    let artifact_id = format!("sart_{}", Uuid::new_v4().simple());
    let key = format!("{task_id}:{artifact_id}");
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let in_cas = super::super::write_cas(s, task_id, &sha256, bytes)?;
    let reference = json!({"artifact_id":artifact_id,"version":1,"task_id":task_id,
        "size":bytes.len(),"sha256":sha256,"name":name,"media_type":media_type,
        "source_run_id":source_run_id,"source_event_seq":null,
        "original":true,"complete":"complete"});
    let saved = if in_cas {
        json!({"artifact_id":artifact_id,"version":1,"task_id":task_id,
            "size":bytes.len(),"sha256":sha256,"cas_sha256":sha256,"name":name,"media_type":media_type,
            "source_run_id":source_run_id,"source_event_seq":null,
            "data":null,"created_at":now()})
    } else {
        json!({"artifact_id":artifact_id,"version":1,"task_id":task_id,
            "size":bytes.len(),"sha256":sha256,"name":name,"media_type":media_type,
            "source_run_id":source_run_id,"source_event_seq":null,
            "data":base64::engine::general_purpose::STANDARD.encode(bytes),
            "created_at":now()})
    };
    Ok((key, reference, saved))
}
fn saved_bytes(s: &AppState, task_id: &str, reference: &Value) -> Option<Vec<u8>> {
    let saved = verify_ref(s, task_id, reference).ok()?;
    base64::engine::general_purpose::STANDARD
        .decode(saved["data"].as_str()?)
        .ok()
}
fn same_content(
    s: &AppState,
    task_id: &str,
    content: &[u8],
    source_map: Option<&[u8]>,
) -> Api<Option<Value>> {
    for row in records(s, task_id, "research_document")? {
        if row["payload"]["extractor_version"] != EXTRACTOR {
            continue;
        }
        let original = saved_bytes(s, task_id, &row["payload"]["artifact"]);
        if original.as_deref() != Some(content) {
            continue;
        }
        let prior_map = if row["payload"]["source_map_artifact"].is_null() {
            Some(Vec::new())
        } else {
            saved_bytes(s, task_id, &row["payload"]["source_map_artifact"])
        };
        if prior_map.as_deref() == Some(source_map.unwrap_or_default()) {
            return Ok(Some(row));
        }
    }
    Ok(None)
}
pub(in crate::runs::src_api::domains) fn ingest(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_JAVASCRIPT_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let encoded = body["content_b64"]
        .as_str()
        .ok_or_else(|| Error::invalid("content_b64 is required"))?;
    let content = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Error::invalid("content_b64 must be base64"))?;
    let source_url = body["source_url"]
        .as_str()
        .ok_or_else(|| Error::invalid("source_url is required"))?;
    let source_map =
        if let Some(value) = body.get("source_map_b64").filter(|value| !value.is_null()) {
            Some(
                base64::engine::general_purpose::STANDARD
                    .decode(
                        value
                            .as_str()
                            .ok_or_else(|| Error::invalid("source_map_b64 must be base64"))?,
                    )
                    .map_err(|_| Error::invalid("source_map_b64 must be base64"))?,
            )
        } else {
            body.get("source_map")
                .and_then(Value::as_str)
                .map(|value| value.as_bytes().to_vec())
        };
    let (mut candidates, call_sites, mut errors, scoped_clients, api_contracts, chunk_references, decode_error) =
        match std::str::from_utf8(&content) {
            Ok(text) => {
                let (candidates, calls, errors, clients, contracts, chunks) =
                    lexical_rows(text, source_url);
                (candidates, calls, errors, clients, contracts, chunks, None)
            }
            Err(error) => (
                Vec::new(),
                Vec::new(),
                vec![json!({"location":"$","code":"javascript_not_utf8",
                    "message":error.to_string()})],
                BTreeMap::new(),
                Vec::new(),
                Vec::new(),
                Some(error.to_string()),
            ),
        };
    let mapped = source_map
        .as_ref()
        .map(|source_map| map_candidates(&mut candidates, source_map, &mut errors))
        .unwrap_or(0);
    for candidate in &mut candidates {
        let tier = classify_bundle_candidate_tier(candidate);
        annotate_evidence_tier(candidate, tier);
    }
    let status = if decode_error.is_some() {
        "invalid"
    } else if errors.is_empty() {
        "available"
    } else {
        "partial"
    };
    loop {
        if let Some(command_id) = command_id {
            if let Some(marker) = s
                .store
                .get(RESEARCH_JAVASCRIPT_APPLIED, command_id)
                .map_err(db)?
            {
                return Ok(marker["output"].clone());
            }
        }
        let head = s.store.get(HEADS, task_id).map_err(db)?;
        if let Some(existing) = same_content(s, task_id, &content, source_map.as_deref())? {
            if let Some(command_id) = command_id {
                let marker = json!({"command_id":command_id,"output":existing});
                if s.store
                    .put_many_if(
                        &[
                            (HEADS, task_id, head.as_ref()),
                            (RESEARCH_JAVASCRIPT_APPLIED, command_id, None),
                        ],
                        &[(RESEARCH_JAVASCRIPT_APPLIED, command_id, &marker)],
                    )
                    .map_err(db)?
                {
                    return Ok(existing);
                }
                continue;
            }
            return Ok(existing);
        }
        let source_id = format!("javascript-source-{}", Uuid::new_v4().simple());
        let record_id = format!("javascript:{source_id}:{EXTRACTOR}");
        let (artifact_key, artifact_ref, artifact_row) = artifact(
            s,
            task_id,
            &content,
            "javascript-source",
            "text/javascript",
            &body["source_run_id"],
        )?;
        let source_map_artifact = source_map
            .as_ref()
            .map(|source_map| {
                artifact(
                    s,
                    task_id,
                    source_map,
                    "javascript-source-map",
                    "application/json",
                    &body["source_run_id"],
                )
            })
            .transpose()?;
        let payload = json!({"document_type":"javascript","extractor_version":EXTRACTOR,"source_digest":format!("{:x}",Sha256::digest(&content)),"content_digest":format!("{:x}",Sha256::digest(&content)),
            "source_url":source_url,"source_run_id":body["source_run_id"],
            "artifact":artifact_ref,
            "source_map_artifact":source_map_artifact.as_ref().map(|(_,reference,_)|reference),
            "candidates":candidates,"call_sites":call_sites,
            "scoped_clients":scoped_clients,"api_contracts":api_contracts,
            "chunk_references":chunk_references,
            "source_map":{"mapped_candidates":mapped,
                "unmapped_candidates":candidates.len()-mapped},
            "errors":errors,
            "parser_limitations":[
                "lexical extraction does not prove syntax validity, execution reachability or source-to-sink flow",
                "template interpolation and dynamic property access require runtime evidence",
                "source map positions identify original locations, not original runtime behavior"
            ],
            "status":status,"ingested_at":now()});
        let (storage_id, version, mut record) = stage_rec(
            s,
            task_id,
            "research_document",
            &record_id,
            payload,
            Some(0),
        )?;
        record["record_id"] = json!(record_id);
        record["parent_id"] = json!(source_id);
        record["natural_key"] = json!(record_id);
        let document = json!({"version":version+1,"values":record});
        let mut candidate_documents = Vec::new();
        for (index, candidate) in candidates.iter().enumerate() {
            let candidate_id = format!("{record_id}:{index}");
            let natural_key = format!(
                "{record_id}:{}:{}:{}",
                candidate["line"].as_u64().unwrap_or(0),
                candidate["column"].as_u64().unwrap_or(0),
                candidate["value"].as_str().unwrap_or("")
            );
            let mut candidate_payload = candidate.clone();
            candidate_payload["document_id"] = json!(record_id);
            let (storage_id, version, mut row) = stage_rec(
                s,
                task_id,
                "static_candidate",
                &candidate_id,
                candidate_payload,
                Some(0),
            )?;
            row["record_id"] = json!(candidate_id);
            row["parent_id"] = json!(record_id);
            row["natural_key"] = json!(natural_key);
            candidate_documents.push((storage_id, json!({"version":version+1,"values":row})));
        }
        let next_head = json!({"version":head.as_ref()
            .and_then(|row|row["version"].as_u64()).unwrap_or(0)+1});
        let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":record}));
        let mut expected = vec![
            (HEADS, task_id, head.as_ref()),
            (RECORDS, storage_id.as_str(), None),
            (ARTIFACTS, artifact_key.as_str(), None),
        ];
        let mut writes = vec![
            (HEADS, task_id, &next_head),
            (RECORDS, storage_id.as_str(), &document),
            (ARTIFACTS, artifact_key.as_str(), &artifact_row),
        ];
        if let Some((key, _, row)) = source_map_artifact.as_ref() {
            expected.push((ARTIFACTS, key.as_str(), None));
            writes.push((ARTIFACTS, key.as_str(), row));
        }
        for (key, value) in &candidate_documents {
            expected.push((RECORDS, key.as_str(), None));
            writes.push((RECORDS, key.as_str(), value));
        }
        if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
            expected.push((RESEARCH_JAVASCRIPT_APPLIED, command_id, None));
            writes.push((RESEARCH_JAVASCRIPT_APPLIED, command_id, marker));
        }
        if s.store.put_many_if(&expected, &writes).map_err(db)? {
            if let Some(sm_bytes) = source_map.as_ref() {
                if let Ok(unpacked) = unpack_sourcemap_bytes(
                    s,
                    task_id,
                    sm_bytes,
                    &record_id,
                    source_url,
                    &body["source_run_id"],
                ) {
                    record["payload"]["sourcemap_unpacked"] = json!({
                        "file_count": unpacked["file_count"],
                        "endpoint_count": unpacked["endpoint_count"],
                        "package_observation_count": unpacked["package_observation_count"],
                    });
                }
            }
            return Ok(record);
        }
    }
}
fn normalize_sourcemap_path(raw: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_start_matches("webpack:///")
        .trim_start_matches("webpack://")
        .trim_start_matches("vite://")
        .trim_start_matches("file://");
    let mut parts: Vec<&str> = Vec::new();
    for seg in trimmed.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        "unknown.js".to_owned()
    } else {
        parts.join("/")
    }
}

fn annotate_evidence_tier(candidate: &mut Value, tier: &str) {
    if let Some(obj) = candidate.as_object_mut() {
        obj.insert("evidence_tier".to_owned(), json!(tier));
    }
}

fn classify_bundle_candidate_tier(candidate: &Value) -> &'static str {
    let raw = candidate["raw_path"]
        .as_str()
        .or_else(|| candidate["value"].as_str())
        .unwrap_or("");
    if raw.contains("${") || raw.contains(" + ") || candidate["evidence_state"] == "dynamic_candidate" {
        "dynamic_concat_candidate"
    } else if candidate["original"]["content_available"] == true {
        "sourcemap_verified"
    } else {
        "ast_literal"
    }
}

fn unpack_sourcemap_bytes(
    s: &AppState,
    task_id: &str,
    source_map: &[u8],
    document_id: &str,
    source_url: &str,
    source_run_id: &Value,
) -> Api<Value> {
    let parsed: Value = serde_json::from_slice(source_map)
        .map_err(|err| Error::invalid(format!("source map JSON invalid: {err}")))?;
    let sources = parsed["sources"]
        .as_array()
        .ok_or_else(|| Error::invalid("source map sources array is required"))?;
    let contents = parsed["sourcesContent"].as_array();
    let mut files = Vec::new();
    let mut verified_endpoints = Vec::new();
    let mut packages = Vec::new();

    for (idx, src_val) in sources.iter().enumerate() {
        let Some(raw_path) = src_val.as_str() else {
            continue;
        };
        let norm_path = normalize_sourcemap_path(raw_path);
        let text = contents
            .and_then(|arr| arr.get(idx))
            .and_then(Value::as_str);
        let Some(code) = text else {
            files.push(json!({
                "source_path": norm_path,
                "content_available": false,
            }));
            continue;
        };
        let (sha256, vault_rel) =
            super::super::traffic_index::put_vault_object(s, task_id, code.as_bytes())?;
        let file_id = format!("jsm_{}", &sha256[..20]);
        let mut endpoint_count = 0usize;

        if norm_path.ends_with("package.json") {
            if let Ok(pkg_json) = serde_json::from_str::<Value>(code) {
                for section in ["dependencies", "devDependencies", "peerDependencies"] {
                    if let Some(deps) = pkg_json[section].as_object() {
                        for (pkg_name, ver_val) in deps {
                            let Some(raw_ver) = ver_val.as_str() else {
                                continue;
                            };
                            let obs_id = format!(
                                "pkg_{}",
                                &crate::runs::src_api::digest(&json!([
                                    "npm",
                                    pkg_name.to_ascii_lowercase(),
                                    raw_ver,
                                    norm_path
                                ]))[..20]
                            );
                            let obs_doc = json!({
                                "observation_id": obs_id,
                                "ecosystem": "npm",
                                "package_name": pkg_name.to_ascii_lowercase(),
                                "version": raw_ver,
                                "section": section,
                                "source_ref": format!("sourcemap:{document_id}:{norm_path}"),
                                "observed_at": now(),
                            });
                            let rev = rec(s, task_id, "pkg_version_observation", &obs_id)
                                .ok()
                                .and_then(|r| r["revision"].as_u64());
                            save_rec(s, task_id, "pkg_version_observation", &obs_id, obs_doc.clone(), rev)?;
                            packages.push(obs_doc);
                        }
                    }
                }
            }
        } else if matches!(
            norm_path.rsplit('.').next(),
            Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "vue" | "svelte")
        ) && !norm_path.contains("node_modules/")
        {
            let (mut cands, _, _, _, _, _) = lexical_rows(code, source_url);
            for mut cand in cands.drain(..) {
                annotate_evidence_tier(&mut cand, "sourcemap_verified");
                cand["source_file"] = json!(norm_path);
                endpoint_count += 1;
                if verified_endpoints.len() < 500 {
                    verified_endpoints.push(cand);
                }
            }
        }

        let file_doc = json!({
            "file_id": file_id,
            "document_id": document_id,
            "source_url": source_url,
            "source_path": norm_path,
            "vault_sha256": sha256,
            "vault_rel": vault_rel,
            "byte_len": code.len(),
            "endpoint_count": endpoint_count,
            "source_run_id": source_run_id,
            "unpacked_at": now(),
        });
        let rev = rec(s, task_id, "research_sourcemap_file", &file_id)
            .ok()
            .and_then(|r| r["revision"].as_u64());
        save_rec(s, task_id, "research_sourcemap_file", &file_id, file_doc.clone(), rev)?;
        files.push(file_doc);
    }

    Ok(json!({
        "document_id": document_id,
        "source_url": source_url,
        "file_count": files.len(),
        "endpoint_count": verified_endpoints.len(),
        "package_observation_count": packages.len(),
        "files": files,
        "verified_endpoints": verified_endpoints,
        "packages": packages,
    }))
}

pub(in crate::runs::src_api::domains) fn unpack_sourcemap(
    s: &AppState,
    task_id: &str,
    body: &Value,
) -> Api<Value> {
    let source_url = body["source_url"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("https://sourcemap.local/bundle.js.map");
    let doc_id = body["document_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("sourcemap-{}", Uuid::new_v4().simple()));
    let bytes = if let Some(b64) = body["source_map_b64"].as_str().filter(|v| !v.is_empty()) {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| Error::invalid("source_map_b64 must be valid base64"))?
    } else if let Some(raw) = body["source_map"].as_str().filter(|v| !v.is_empty()) {
        raw.as_bytes().to_vec()
    } else if body["source_map"].is_object() {
        serde_json::to_vec(&body["source_map"]).map_err(db)?
    } else if let Some(doc_rec) = rec(s, task_id, "research_document", &doc_id).ok() {
        let art_ref = &doc_rec["payload"]["source_map_artifact"];
        let art = verify_ref(s, task_id, art_ref)?;
        base64::engine::general_purpose::STANDARD
            .decode(art["data"].as_str().unwrap_or(""))
            .map_err(|_| Error::invalid("stored source_map artifact invalid"))?
    } else {
        return Err(Error::invalid(
            "source_map_b64, source_map, or existing document_id with source_map_artifact is required",
        ));
    };
    unpack_sourcemap_bytes(
        s,
        task_id,
        &bytes,
        &doc_id,
        source_url,
        &body["source_run_id"],
    )
}

pub(in crate::runs::src_api::domains) fn read(
    s: &AppState,
    task_id: &str,
    params: &Value,
) -> Api<Value> {
    let record_id = require(params, "record_id")?;
    let row = rec(s, task_id, "research_document", record_id)?;
    if row["payload"]["document_type"] != "javascript" {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "javascript_type_mismatch",
            "research document is not JavaScript",
        ));
    }
    let original = verify_ref(s, task_id, &row["payload"]["artifact"])?;
    let mut result = json!({"record_id":row["record_id"],"kind":row["kind"],
        "revision":row["revision"],"parent_id":row["parent_id"],
        "version":row["payload"]["version"].as_u64()
            .unwrap_or(row["revision"].as_u64().unwrap_or(0))});
    for (key, value) in row["payload"].as_object().into_iter().flatten() {
        result[key] = value.clone();
    }
    result["original"] = json!({"encoding":"base64","data":original["data"],
        "size":original["size"]});
    if !row["payload"]["source_map_artifact"].is_null() {
        let source_map = verify_ref(s, task_id, &row["payload"]["source_map_artifact"])?;
        result["source_map_original"] = json!({"encoding":"base64",
            "data":source_map["data"],"size":source_map["size"]});
    }
    Ok(result)
}

fn extract_html_script_urls(html: &str, base: &reqwest::Url) -> Vec<String> {
    let mut out = Vec::new();
    let script_re = regex::Regex::new(r#"(?i)<(?:script[^>]+src|link[^>]+rel=["']?modulepreload["']?[^>]+href|link[^>]+href=["']([^"']+\.m?js[^"']*)["'][^>]+rel=["']?modulepreload["']?)\s*=?\s*["']?([^"'\s>]+)"#).unwrap();
    for cap in script_re.captures_iter(html) {
        let raw = cap.get(1).or_else(|| cap.get(2)).map(|m| m.as_str()).unwrap_or("");
        if let Ok(resolved) = base.join(raw) {
            out.push(resolved.to_string());
        }
    }
    let dyn_re = regex::Regex::new(r#"import\(\s*["']([^"']+\.m?js(?:\?[^"']*)?)["']\s*\)"#).unwrap();
    for cap in dyn_re.captures_iter(html) {
        if let Some(m) = cap.get(1)
            && let Ok(resolved) = base.join(m.as_str())
        {
            out.push(resolved.to_string());
        }
    }
    out
}

fn extract_js_from_json_value(val: &Value, base: &reqwest::Url, out: &mut Vec<String>) {
    match val {
        Value::String(s) => {
            let clean = s.split(['?', '#']).next().unwrap_or("");
            if (clean.ends_with(".js") || clean.ends_with(".mjs")) && !clean.contains(char::is_whitespace) {
                let candidate = if s.starts_with("./") || s.starts_with("../") || s.starts_with('/') || s.contains("://") {
                    base.join(s).ok()
                } else {
                    base.join(&format!("/{s}")).ok()
                };
                if let Some(u) = candidate {
                    out.push(u.to_string());
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                extract_js_from_json_value(item, base, out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                extract_js_from_json_value(&Value::String(k.clone()), base, out);
                extract_js_from_json_value(v, base, out);
            }
        }
        _ => {}
    }
}

// Live collection uses the same persisted HTTP execution and approval path as
// other SRC tools. No process-local client may bypass Task HTTP policy.
async fn collect_resource(s: std::sync::Arc<AppState>, task_id: &str, url: &reqwest::Url, body: &Value)
    -> Api<(reqwest::Url, u16, Vec<u8>)>
{
    super::super::browser_runtime::target_scope(&s, task_id, url.as_str())?;
    let key = format!("js-collect-{:x}", Sha256::digest(serde_json::to_vec(&json!([
        task_id, require(body, "idempotency_key")?, url.as_str(),
    ])).map_err(db)?));
    let source_id = format!("{key}-source");
    let target = format!("{}{}", url.path(), url.query().map(|query| format!("?{query}")).unwrap_or_default());
    let raw = format!("GET {target} HTTP/1.1\r\nHost: {}\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n", url.authority());
    super::super::record_exchange(&s, task_id, &json!({"exchange_id":source_id,
        "source_kind":"import","actor_ref":"javascript:collector","context_id":"anonymous",
        "observed_url":url.as_str(),"method":"GET","execution_state":"created",
        "request_raw_b64":base64::engine::general_purpose::STANDARD.encode(raw),
        "source_run_id":body["source_run_id"]}))?;
    let prior = records(&s, task_id, "request_draft")?.into_iter().find(|row|
        row["payload"]["source_exchange_id"] == source_id && row["payload"]["revision"] == 1);
    let draft = match prior {
        Some(row) => row["payload"].clone(),
        None => super::super::create_next_draft(&s, task_id, &source_id, None,
            json!({"javascript_collection":true}), true)?["draft"].clone(),
    };
    let policy = super::super::policy(&s, task_id)?;
    let executed = super::super::execute_draft(s.clone(), task_id, &json!({
        "draft_id":draft["draft_id"],"draft_revision":draft["revision"],
        "authorization_version":policy["version"],"context_id":"anonymous","identity_ref":"anonymous",
        "actor_ref":"javascript:collector","source_run_id":body["source_run_id"],"idempotency_key":key,
        "wait_for_approval":true,"budget":{"max_redirects":3,"timeout_seconds":12,"capture_limit":25*1024*1024},
    })).await?;
    let output = &executed["output"];
    if output["execution_state"] != "completed" {
        let mut error = Error::new(StatusCode::CONFLICT, "src.javascript.http_execution_failed",
            format!("JavaScript collection HTTP execution is {}", output["execution_state"]));
        error.detail = json!({"execution_id":output["execution_id"],"receipt":output["receipt"]});
        return Err(error);
    }
    let exchange = &output["exchange"];
    let response = verify_ref(&s, task_id, &exchange["response_raw_ref"])?;
    let raw = base64::engine::general_purpose::STANDARD.decode(response["data"].as_str().unwrap_or("")).map_err(db)?;
    let parsed = super::super::exchange_queries::parsed_response(&raw, "GET")?;
    let (bytes, limitations) = super::super::exchange_queries::decoded_body(&parsed, "GET")?;
    if !limitations.is_empty() || bytes.len() > 24*1024*1024 {
        return Err(Error::new(StatusCode::UNPROCESSABLE_ENTITY, "capture_partial",
            format!("JavaScript collection body is incomplete or too large: {limitations:?}")));
    }
    Ok((reqwest::Url::parse(require(exchange, "observed_url")?).map_err(db)?,
        parsed["status_code"].as_u64().unwrap_or(0) as u16, bytes))
}

async fn collect_attempt(s: std::sync::Arc<AppState>, task_id: &str, url: &reqwest::Url,
    body: &Value, skipped: &mut Vec<Value>) -> Option<(reqwest::Url, u16, Vec<u8>)> {
    match collect_resource(s, task_id, url, body).await {
        Ok(resource) => Some(resource),
        Err(error) => {
            skipped.push(json!({"url":url.as_str(),"code":error.code,"error":error.message,"detail":error.detail}));
            None
        }
    }
}

#[derive(Clone)]
struct CollectItem {
    url: String,
    alt_urls: Vec<String>,
}

fn is_vendor_chunk(url: &str) -> bool {
    let path = reqwest::Url::parse(url)
        .ok()
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_else(|| url.to_ascii_lowercase());
    [
        "/vendor", "vendor-", "vendor.", "/node_modules/", "polyfill",
        "/runtime", "runtime-", "runtime.", "framework-", "webpack-",
        "chunks/webpack", "chunks/framework",
    ]
    .iter()
    .any(|marker| path.contains(marker))
}

fn enqueue_collect_item(
    item: CollectItem,
    business_queue: &mut std::collections::VecDeque<CollectItem>,
    vendor_queue: &mut std::collections::VecDeque<CollectItem>,
) {
    if is_vendor_chunk(&item.url) {
        vendor_queue.push_back(item);
    } else {
        business_queue.push_back(item);
    }
}

fn requeue_front_item(
    item: CollectItem,
    business_queue: &mut std::collections::VecDeque<CollectItem>,
    vendor_queue: &mut std::collections::VecDeque<CollectItem>,
) {
    if is_vendor_chunk(&item.url) {
        vendor_queue.push_front(item);
    } else {
        business_queue.push_front(item);
    }
}

async fn collect_with_fallbacks(
    s: std::sync::Arc<AppState>,
    task_id: &str,
    item: &CollectItem,
    body: &Value,
    visited: &mut HashSet<String>,
    skipped_errors: &mut Vec<Value>,
) -> Api<Option<(reqwest::Url, Vec<u8>)>> {
    let mut primary_errors = Vec::new();
    let requested = reqwest::Url::parse(&item.url).map_err(db)?;
    if let Some((final_url, status, bytes)) =
        collect_attempt(s.clone(), task_id, &requested, body, &mut primary_errors).await
    {
        if (200..300).contains(&status) {
            for alt in &item.alt_urls {
                visited.insert(alt.clone());
            }
            return Ok(Some((final_url, bytes)));
        }
        primary_errors.push(json!({"url": item.url, "status": status}));
    }
    for alt in &item.alt_urls {
        if !visited.insert(alt.clone())
            || super::super::browser_runtime::target_scope(&s, task_id, alt).is_err()
        {
            continue;
        }
        let Ok(alt_url) = reqwest::Url::parse(alt) else { continue };
        let mut alt_errors = Vec::new();
        if let Some((final_url, status, bytes)) =
            collect_attempt(s.clone(), task_id, &alt_url, body, &mut alt_errors).await
            && (200..300).contains(&status)
        {
            return Ok(Some((final_url, bytes)));
        }
    }
    skipped_errors.extend(primary_errors);
    Ok(None)
}

fn backfill_row_endpoint(row: &mut Value, shared_clients: &BTreeMap<String, String>) -> bool {
    let client = row["client"].as_str().unwrap_or("");
    let raw_path = row["raw_path"].as_str().unwrap_or("");
    let cur_resolved = row["resolved_path"].as_str().unwrap_or("");
    let Some(base) = shared_clients.get(client) else {
        return false;
    };
    if raw_path.is_empty() || cur_resolved != raw_path {
        return false;
    }
    let next_resolved = join_base_url(base, raw_path);
    if next_resolved == cur_resolved {
        return false;
    }
    row["resolved_path"] = json!(next_resolved);
    row["base_url_source"] = json!("cross_bundle_client");
    if row["candidate_type"] == "api_contract" {
        row["value"] = json!(next_resolved);
        row["normalized_path"] = json!(next_resolved);
    }
    true
}

fn backfill_cross_bundle_clients(s: &AppState, task_id: &str) -> Api<usize> {
    let docs = records(s, task_id, "research_document")?;
    let mut client_bases: BTreeMap<String, Option<String>> = BTreeMap::new();
    for doc in &docs {
        if doc["payload"]["document_type"] != "javascript" {
            continue;
        }
        for (name, val) in doc["payload"]["scoped_clients"]
            .as_object()
            .into_iter()
            .flatten()
        {
            if name == "<module_default>" {
                continue;
            }
            let Some(base) = val.as_str().map(str::trim).filter(|v| !v.is_empty()) else {
                continue;
            };
            match client_bases.get(name) {
                None => {
                    client_bases.insert(name.clone(), Some(base.to_owned()));
                }
                Some(Some(existing)) if existing == base => {}
                Some(_) => {
                    client_bases.insert(name.clone(), None);
                }
            }
        }
    }
    let shared_clients: BTreeMap<String, String> = client_bases
        .into_iter()
        .filter_map(|(k, v)| v.map(|b| (k, b)))
        .collect();
    if shared_clients.is_empty() {
        return Ok(0);
    }

    let mut backfilled = 0usize;
    for doc in docs {
        if doc["payload"]["document_type"] != "javascript" {
            continue;
        }
        let Some(doc_id) = doc["record_id"].as_str() else { continue };
        let mut payload = doc["payload"].clone();
        let mut doc_changed = false;
        if let Some(arr) = payload["api_contracts"].as_array_mut() {
            for item in arr {
                if backfill_row_endpoint(item, &shared_clients) {
                    doc_changed = true;
                    backfilled += 1;
                }
            }
        }
        if let Some(arr) = payload["candidates"].as_array_mut() {
            for item in arr {
                if backfill_row_endpoint(item, &shared_clients) {
                    doc_changed = true;
                }
            }
        }
        if doc_changed {
            save_rec(s, task_id, "research_document", doc_id, payload, doc["revision"].as_u64())?;
        }
    }
    for cand in records(s, task_id, "static_candidate")? {
        let Some(cand_id) = cand["record_id"].as_str() else { continue };
        let mut payload = cand["payload"].clone();
        if backfill_row_endpoint(&mut payload, &shared_clients) {
            save_rec(s, task_id, "static_candidate", cand_id, payload, cand["revision"].as_u64())?;
        }
    }
    Ok(backfilled)
}

pub(in crate::runs::src_api::domains) async fn collect(
    s: std::sync::Arc<AppState>,
    task_id: &str,
    body: &Value,
) -> Api<Value> {
    let entry_raw = require(body, "entry_url")?;
    super::super::browser_runtime::target_scope(&s, task_id, entry_raw)?;
    let entry_url = reqwest::Url::parse(entry_raw)
        .map_err(|_| Error::invalid("entry_url must be a valid HTTP(S) URL"))?;
    let max_files = body["max_files"].as_u64().unwrap_or(20).clamp(1, 50) as usize;
    let mut ingested_docs = Vec::new();
    let mut skipped_errors = Vec::new();
    let mut optional_resources_unavailable = Vec::new();

    let mut business_queue = std::collections::VecDeque::<CollectItem>::new();
    let mut vendor_queue = std::collections::VecDeque::<CollectItem>::new();
    let mut visited = HashSet::<String>::new();
    for seed in body["extra_js_urls"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if super::super::browser_runtime::target_scope(&s, task_id, seed).is_ok() {
            enqueue_collect_item(
                CollectItem { url: seed.to_owned(), alt_urls: Vec::new() },
                &mut business_queue,
                &mut vendor_queue,
            );
        }
    }

    if !(entry_url.path().ends_with(".js") || entry_url.path().ends_with(".mjs")) {
        if let Some((final_url, status, bytes)) = collect_attempt(s.clone(), task_id, &entry_url, body, &mut skipped_errors).await {
            if (200..300).contains(&status) && let Ok(html) = std::str::from_utf8(&bytes) {
                for js_url in extract_html_script_urls(html, &final_url) {
                    if super::super::browser_runtime::target_scope(&s, task_id, &js_url).is_ok() {
                        enqueue_collect_item(
                            CollectItem { url: js_url, alt_urls: Vec::new() },
                            &mut business_queue,
                            &mut vendor_queue,
                        );
                    }
                }
            } else if !(200..300).contains(&status) {
                skipped_errors.push(json!({"url":entry_url.as_str(),"status":status,"resource":"entry"}));
            }
        }
        let origin_root = {
            let mut r = entry_url.clone();
            r.set_path("/");
            r.set_query(None);
            r.set_fragment(None);
            r
        };
        let entry_dir = entry_url.join("./").ok().filter(|d| d.path() != "/");
        let mut probed_manifests = HashSet::new();
        for manifest_path in [
            "/.vite/manifest.json",
            "/vite-manifest.json",
            "/asset-manifest.json",
            "/_next/static/build-manifest.json",
            "/_next/static/app-build-manifest.json",
            "/_nuxt/builds/latest.json",
            "/ngsw.json",
            "/build/manifest.json",
            "/build/.vite/manifest.json",
            "/webpack-manifest.json",
        ] {
            let mut bases = vec![origin_root.clone()];
            if let Some(dir) = &entry_dir {
                bases.push(dir.clone());
            }
            for base_url in bases {
                let rel = if base_url.path() == "/" { manifest_path } else { manifest_path.trim_start_matches('/') };
                let Ok(m_url) = base_url.join(rel) else { continue };
                if !probed_manifests.insert(m_url.to_string()) {
                    continue;
                }
                if super::super::browser_runtime::target_scope(&s, task_id, m_url.as_str()).is_err() {
                    continue;
                }
                let Some((_, status, bytes)) = collect_attempt(s.clone(), task_id, &m_url, body, &mut optional_resources_unavailable).await else { continue };
                if (200..300).contains(&status) && let Ok(json_val) = serde_json::from_slice::<Value>(&bytes) {
                    let mut found = Vec::new();
                    extract_js_from_json_value(&json_val, &base_url, &mut found);
                    for js_url in found {
                        if super::super::browser_runtime::target_scope(&s, task_id, &js_url).is_ok() {
                            enqueue_collect_item(
                                CollectItem { url: js_url, alt_urls: Vec::new() },
                                &mut business_queue,
                                &mut vendor_queue,
                            );
                        }
                    }
                } else if !(200..300).contains(&status) {
                    optional_resources_unavailable.push(json!({"url":m_url.as_str(),"status":status,"resource":"manifest"}));
                }
            }
        }
    } else {
        enqueue_collect_item(
            CollectItem { url: entry_url.to_string(), alt_urls: Vec::new() },
            &mut business_queue,
            &mut vendor_queue,
        );
    }

    while let Some(item) = business_queue.pop_front().or_else(|| vendor_queue.pop_front()) {
        if ingested_docs.len() >= max_files {
            requeue_front_item(item, &mut business_queue, &mut vendor_queue);
            break;
        }
        if !visited.insert(item.url.clone()) {
            continue;
        }
        if super::super::browser_runtime::target_scope(&s, task_id, &item.url).is_err() {
            continue;
        }
        let Some((final_js_url, bytes)) =
            collect_with_fallbacks(s.clone(), task_id, &item, body, &mut visited, &mut skipped_errors).await?
        else {
            continue;
        };
        let mut map_b64: Option<String> = None;
        if let Ok(text) = std::str::from_utf8(&bytes) {
            let mut start = text.len().saturating_sub(512);
            while !text.is_char_boundary(start) { start += 1; }
            let tail = &text[start..];
            if let Some(idx) = tail.rfind("sourceMappingURL=") {
                let map_ref = tail[idx + "sourceMappingURL=".len()..]
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches("*/")
                    .trim();
                if !map_ref.is_empty()
                    && !map_ref.starts_with("data:")
                    && let Ok(map_url) = final_js_url.join(map_ref)
                    && super::super::browser_runtime::target_scope(&s, task_id, map_url.as_str()).is_ok()
                {
                    if let Some((_, status, m_bytes)) = collect_attempt(s.clone(), task_id, &map_url, body, &mut optional_resources_unavailable).await {
                        if (200..300).contains(&status) {
                            map_b64 = Some(base64::engine::general_purpose::STANDARD.encode(&m_bytes));
                        } else {
                            optional_resources_unavailable.push(json!({"url":map_url.as_str(),"status":status,"resource":"source_map"}));
                        }
                    }
                }
            }
        }
        let ingest_body = json!({
            "source_url": final_js_url.as_str(),
            "content_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "source_map_b64": map_b64,
            "source_run_id": body["source_run_id"],
        });
        match ingest(&s, task_id, &ingest_body, None) {
        Ok(doc) => {
            for chunk in doc["payload"]["chunk_references"].as_array().into_iter().flatten() {
                if let Some(next_url) = chunk["url"].as_str()
                    && !visited.contains(next_url)
                    && super::super::browser_runtime::target_scope(&s, task_id, next_url).is_ok()
                {
                    let alt_urls = ["alt_root_url", "dup_fallback_url"]
                        .into_iter()
                        .filter_map(|k| chunk[k].as_str().map(str::to_owned))
                        .collect();
                    enqueue_collect_item(
                        CollectItem { url: next_url.to_owned(), alt_urls },
                        &mut business_queue,
                        &mut vendor_queue,
                    );
                }
            }
            ingested_docs.push(json!({
                "record_id": doc["record_id"],
                "source_url": final_js_url.as_str(),
                "candidate_count": doc["payload"]["candidates"].as_array().map_or(0, Vec::len),
                "contract_count": doc["payload"]["api_contracts"].as_array().map_or(0, Vec::len),
                "chunk_count": doc["payload"]["chunk_references"].as_array().map_or(0, Vec::len),
            }));
        }
        Err(error) => skipped_errors.push(json!({"url":final_js_url.as_str(),"code":error.code,"error":error.message,"detail":error.detail})),
        }
    }

    let backfilled_contracts = backfill_cross_bundle_clients(&s, task_id)?;
    let remaining_queue = business_queue.len() + vendor_queue.len();
    let derived = if body["derive_business_context"].as_bool().unwrap_or(true) {
        match super::super::research_business::derive(&s, task_id, &json!({}), None) {
            Ok(context) => Some(context),
            Err(error) => { skipped_errors.push(json!({"resource":"business_context","code":error.code,"error":error.message,"detail":error.detail})); None }
        }
    } else {
        None
    };

    Ok(json!({
        "entry_url": entry_url.as_str(),
        "ingested_count": ingested_docs.len(),
        "backfilled_contracts": backfilled_contracts,
        "complete": skipped_errors.is_empty() && remaining_queue == 0,
        "remaining_queue": remaining_queue,
        "documents": ingested_docs,
        "skipped": skipped_errors,
        "optional_resources_unavailable": optional_resources_unavailable,
        "business_context": derived.map(|r| r["payload"].clone()),
    }))
}
