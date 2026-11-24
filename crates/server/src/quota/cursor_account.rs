//! Cursor DashboardService account usage for the host's persisted CLI login.
//! No model request is made; raw events stay inside this read and never reach HTTP.

use super::*;
use chrono::{Duration as ChronoDuration, Offset, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, OnceLock},
};

const PAGE_SIZE: usize = 1000;
const HISTORY_DEADLINE: Duration = Duration::from_secs(60);
const RESULT_CACHE: Duration = Duration::from_secs(60);
const LIMIT_PATH: &str = "/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const EVENTS_PATH: &str = "/aiserver.v1.DashboardService/GetFilteredUsageEvents";
const LIMIT_WINDOWS: [(&str, [&str; 2], [&str; 2]); 3] = [
    (
        "totalPercentUsed",
        ["总体", "Overall"],
        [
            "两个额度池的合计用量，不是第三个额度。",
            "Combined usage across both pools, not a third quota.",
        ],
    ),
    (
        "autoPercentUsed",
        ["Cursor 模型", "Cursor models"],
        [
            "Grok 与 Composer 优先使用这个池；Auto 可使用任一池。",
            "Grok and Composer use this pool first. Auto can use either pool.",
        ],
    ),
    (
        "apiPercentUsed",
        ["其他模型", "Other models"],
        [
            "Claude、GPT、Gemini 使用这个池；Grok 与 Composer 用完自己的池后也会落到这里。",
            "Claude, GPT and Gemini use this pool. Grok and Composer fall back to it.",
        ],
    ),
];

#[derive(Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    state_dir: PathBuf,
    days: u32,
    zone: String,
    enabled: bool,
}

static CACHE: OnceLock<Mutex<HashMap<CacheKey, (Instant, Value)>>> = OnceLock::new();
static READ_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<CacheKey, (Instant, Value)>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn clear_cache(state_dir: &FsPath) {
    if let Ok(mut entries) = cache().lock() {
        entries.retain(|key, _| key.state_dir != state_dir);
    }
}

fn cached(key: &CacheKey) -> Option<Value> {
    let entries = cache().lock().ok()?;
    entries
        .get(key)
        .and_then(|(at, value)| (at.elapsed() < RESULT_CACHE).then(|| value.clone()))
}

fn bad_query(message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail":message.into()})),
    )
}

fn typed(code: &str, message: impl Into<String>) -> Value {
    json!({"code":code,"message":message.into()})
}

fn login_error(error: Fail) -> Value {
    let mut view = typed(error.code, error.message);
    if error.code == "keychain_disabled" {
        view["action"] = json!("enable_cursor_keychain");
    }
    view
}

struct UsageWindow {
    days: u32,
    time_zone: String,
    zone: Tz,
    since_ms: i64,
    until_ms: i64,
    resolution: &'static str,
    slots: Vec<String>,
}

impl UsageWindow {
    fn view(&self) -> Value {
        json!({"days":self.days,"time_zone":self.time_zone,"since_ms":self.since_ms,
            "until_ms":self.until_ms,"resolution":self.resolution})
    }
}

fn localize(local: chrono::NaiveDateTime, zone: Tz) -> Option<chrono::DateTime<Tz>> {
    if let Some(value) = zone.from_local_datetime(&local).earliest() {
        return Some(value);
    }
    let gap = chrono_tz::GapInfo::new(&local, &zone)?.begin?;
    local
        .and_local_timezone(gap.1.fix())
        .single()
        .map(|value| value.with_timezone(&zone))
}

fn make_window(
    days: u32,
    time_zone: &str,
    now: chrono::DateTime<Utc>,
) -> Result<UsageWindow, (StatusCode, Json<Value>)> {
    if ![1, 7, 30, 90].contains(&days) {
        return Err(bad_query("days 只能是 1, 7, 30, 90"));
    }
    let zone: Tz = time_zone
        .parse()
        .map_err(|_| bad_query(format!("无法识别的时区：{time_zone}")))?;
    let current = now.with_timezone(&zone);
    let (since, resolution, slots) = if days == 1 {
        let since = localize(current.naive_local() - ChronoDuration::hours(24), zone)
            .ok_or_else(|| bad_query("无法识别的时区边界"))?;
        let local_hour = since
            .naive_local()
            .with_minute(0)
            .and_then(|v| v.with_second(0))
            .and_then(|v| v.with_nanosecond(0))
            .unwrap();
        let mut slot = localize(local_hour, zone).ok_or_else(|| bad_query("无法识别的时区边界"))?;
        let mut slots = Vec::new();
        while slot <= current {
            slots.push(slot.format("%Y-%m-%dT%H:00").to_string());
            slot = (slot.with_timezone(&Utc) + ChronoDuration::hours(1)).with_timezone(&zone);
        }
        (since, "hour", slots)
    } else {
        let first = current.date_naive() - ChronoDuration::days(i64::from(days - 1));
        let since = localize(first.and_hms_opt(0, 0, 0).unwrap(), zone)
            .ok_or_else(|| bad_query("无法识别的时区边界"))?;
        let slots = (0..days)
            .map(|offset| (first + ChronoDuration::days(i64::from(offset))).to_string())
            .collect();
        (since, "day", slots)
    };
    Ok(UsageWindow {
        days,
        time_zone: time_zone.to_owned(),
        zone,
        since_ms: since.timestamp_millis(),
        until_ms: now.timestamp_millis(),
        resolution,
        slots,
    })
}

fn api_base() -> String {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("MUTEKI_CURSOR_USAGE_TEST_ENDPOINT")
        && let Ok(url) = reqwest::Url::parse(&value)
        && url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
    {
        return value.trim_end_matches('/').to_owned();
    }
    CURSOR_DEFAULT_ENDPOINT.to_owned()
}

async fn post(token: &str, path: &str, body: &Value) -> Result<Value, Value> {
    let response = http()
        .post(format!("{}{}", api_base(), path))
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("connect-protocol-version", "1")
        .header("x-cursor-client-type", "cli")
        .json(body)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                typed("request_timeout", "Cursor 账号接口请求超时。")
            } else {
                typed("request_failed", "无法连接 Cursor 账号接口。")
            }
        })?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(typed(
            "signed_out",
            format!(
                "Cursor 拒绝了当前登录（HTTP {}）。执行 cursor-agent login 重新登录后刷新。",
                status.as_u16()
            ),
        ));
    }
    if status.is_redirection() {
        return Err(typed(
            "request_redirected",
            format!(
                "Cursor 账号接口返回 HTTP {} 重定向，未转发登录凭据。",
                status.as_u16()
            ),
        ));
    }
    if status.as_u16() != 200 {
        return Err(typed(
            "request_failed",
            format!("Cursor 账号接口返回 HTTP {}。", status.as_u16()),
        ));
    }
    response
        .json::<Value>()
        .await
        .map_err(|_| typed("invalid_response", "Cursor 账号接口返回的不是 JSON。"))
}

async fn limits(token: &str) -> Result<Value, Value> {
    let body = post(token, LIMIT_PATH, &json!({})).await?;
    if !body.is_object() {
        return Err(typed(
            "invalid_response",
            "Cursor 额度接口返回的结构无法识别。",
        ));
    }
    let plan = &body["planUsage"];
    let resets_at = loose_number(&body["billingCycleEnd"])
        .filter(|value| *value > 0.0)
        .map(|value| value / 1000.0);
    let windows: Vec<Value> = LIMIT_WINDOWS
        .iter()
        .filter_map(|(id, label, description)| {
            let used = loose_number(&plan[*id])?;
            Some(
                json!({"id":id,"kind":"monthly","label":label,"description":description,
            "used_percent":used.clamp(0.0,100.0),"resets_at":resets_at}),
            )
        })
        .collect();
    if windows.is_empty() {
        return Err(typed(
            "limits_unsupported",
            "这个 Cursor 账号没有返回月度额度。",
        ));
    }
    Ok(json!({"windows":windows,"resets_at":resets_at}))
}

fn canonical(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut names: Vec<_> = map.keys().collect();
                names.sort();
                Value::Object(
                    names
                        .into_iter()
                        .map(|name| (name.clone(), sorted(&map[name])))
                        .collect(),
                )
            }
            Value::Array(rows) => Value::Array(rows.iter().map(sorted).collect()),
            _ => value.clone(),
        }
    }
    sorted(value).to_string()
}

fn boundary_overlap(previous: &[String], current: &[String]) -> usize {
    (1..=previous.len().min(current.len()))
        .rev()
        .find(|&count| previous[previous.len() - count..] == current[..count])
        .unwrap_or(0)
}

#[derive(Clone)]
struct Record {
    at_ms: f64,
    model: String,
    session: String,
    uncached_input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    reported_cost: Option<f64>,
}

fn tokens(value: &Value) -> u64 {
    loose_number(value)
        .filter(|value| *value > 0.0)
        .map(|value| value as u64)
        .unwrap_or(0)
}

async fn history(token: &str, window: &UsageWindow) -> Result<Vec<Record>, Value> {
    let deadline = Instant::now() + HISTORY_DEADLINE;
    let mut pages: Vec<Vec<Value>> = Vec::new();
    let mut total: Option<usize> = None;
    let mut page = 1usize;
    loop {
        let max_page = total.map_or(1000, |count| count.div_ceil(PAGE_SIZE) * 2 + 1);
        if page > max_page {
            return Err(typed(
                "invalid_response",
                "Cursor 账号历史的分页数量超出预期。",
            ));
        }
        if Instant::now() > deadline {
            return Err(typed("request_timeout", "读取 Cursor 账号历史超过 60 秒。"));
        }
        let body = post(
            token,
            EVENTS_PATH,
            &json!({"page":page,"pageSize":PAGE_SIZE,
            "startDate":window.since_ms.to_string(),"endDate":window.until_ms.to_string()}),
        )
        .await?;
        let Some(object) = body.as_object() else {
            return Err(typed("invalid_response", "Cursor 账号历史返回了错误结构。"));
        };
        if ["error", "message", "code"]
            .iter()
            .any(|key| object.contains_key(*key))
        {
            return Err(typed("invalid_response", "Cursor 账号历史返回了错误结构。"));
        }
        let count = if object.is_empty() {
            Some(0)
        } else if object
            .get("totalUsageEventsCount")
            .is_some_and(Value::is_null)
        {
            None
        } else if let Some(value) = object.get("totalUsageEventsCount") {
            Some(
                value
                    .as_u64()
                    .ok_or_else(|| typed("invalid_response", "Cursor 账号历史的分页前后不一致。"))?
                    as usize,
            )
        } else {
            None
        };
        let events = if object.is_empty()
            || (object.len() == 1 && object.contains_key("totalUsageEventsCount"))
        {
            Some(Vec::new())
        } else {
            object
                .get("usageEventsDisplay")
                .and_then(Value::as_array)
                .cloned()
        };
        let Some(events) = events else {
            return Err(typed(
                "invalid_response",
                "Cursor 账号历史的分页前后不一致。",
            ));
        };
        if (count.is_none() && !object.get("usageEventsDisplay").is_some_and(Value::is_array))
            || events.len() > PAGE_SIZE
            || total.is_some_and(|prior| count.is_some_and(|count| prior != count))
        {
            return Err(typed(
                "invalid_response",
                "Cursor 账号历史的分页前后不一致。",
            ));
        }
        total = count.or(total);
        let full = events.len() == PAGE_SIZE;
        pages.push(events);
        if !full {
            break;
        }
        page += 1;
    }
    let raw_count: usize = pages.iter().map(Vec::len).sum();
    if total.is_some_and(|count| raw_count < count) {
        return Err(typed("invalid_response", "Cursor 账号历史的分页不完整。"));
    }
    let mut removals = total.map_or(0, |count| raw_count - count);
    let mut previous: Vec<String> = Vec::new();
    let mut records = Vec::new();
    for events in pages {
        let keys: Vec<String> = if removals > 0 {
            events.iter().map(canonical).collect()
        } else {
            Vec::new()
        };
        let dropped = removals.min(boundary_overlap(&previous, &keys));
        removals -= dropped;
        previous = keys;
        for event in events.into_iter().skip(dropped) {
            let usage = &event["tokenUsage"];
            if usage.is_null() {
                continue;
            }
            let Some(fields) = usage.as_object() else {
                return Err(typed(
                    "invalid_response",
                    "Cursor 账号历史中的用量字段无法识别。",
                ));
            };
            for key in [
                "inputTokens",
                "outputTokens",
                "cacheReadTokens",
                "cacheWriteTokens",
                "totalCents",
            ] {
                if let Some(value) = fields.get(key)
                    && loose_number(value).is_none_or(|number| number < 0.0)
                {
                    return Err(typed(
                        "invalid_response",
                        "Cursor 账号历史中的用量数值无效。",
                    ));
                }
            }
            let at_ms = loose_number(&event["timestamp"]);
            let model = event["model"].as_str().filter(|value| !value.is_empty());
            let (Some(at_ms), Some(model)) = (at_ms, model) else {
                return Err(typed(
                    "invalid_response",
                    "Cursor 账号历史中有无法识别的事件。",
                ));
            };
            if at_ms < window.since_ms as f64 || at_ms > window.until_ms as f64 {
                continue;
            }
            records.push(Record {
                at_ms,
                model: model.to_owned(),
                session: event["conversationId"].as_str().unwrap_or("").to_owned(),
                uncached_input: tokens(&usage["inputTokens"]),
                cache_read: tokens(&usage["cacheReadTokens"]),
                cache_write: tokens(&usage["cacheWriteTokens"]),
                output: tokens(&usage["outputTokens"]),
                reported_cost: loose_number(&usage["totalCents"]).map(|value| value / 100.0),
            });
        }
    }
    if removals > 0 {
        return Err(typed(
            "invalid_response",
            "Cursor 账号历史的分页边界无法对齐。",
        ));
    }
    Ok(records)
}

#[derive(Clone, Default, Serialize)]
struct Totals {
    uncached_input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    total_tokens: u64,
    reported_cost: Option<f64>,
    events: u64,
    unpriced_events: u64,
    sessions: usize,
}

fn add(totals: &mut Totals, record: &Record) {
    totals.uncached_input += record.uncached_input;
    totals.cache_read += record.cache_read;
    totals.cache_write += record.cache_write;
    totals.output += record.output;
    totals.total_tokens +=
        record.uncached_input + record.cache_read + record.cache_write + record.output;
    totals.events += 1;
    if let Some(cost) = record.reported_cost {
        *totals.reported_cost.get_or_insert(0.0) += cost;
    } else {
        totals.unpriced_events += 1;
    }
}

fn summarize(records: &[Record], window: &UsageWindow) -> Value {
    let mut totals = Totals::default();
    let mut sessions = HashSet::new();
    let mut models: Vec<(String, Totals, HashSet<String>)> = Vec::new();
    let mut series: Vec<Value> = window
        .slots
        .iter()
        .map(|slot| json!({"slot":slot,"reported_cost":0.0,"total_tokens":0}))
        .collect();
    let indexes: HashMap<&str, usize> = window
        .slots
        .iter()
        .enumerate()
        .map(|(i, slot)| (slot.as_str(), i))
        .collect();
    for record in records {
        add(&mut totals, record);
        let index = models
            .iter()
            .position(|(model, _, _)| model == &record.model)
            .unwrap_or_else(|| {
                models.push((record.model.clone(), Totals::default(), HashSet::new()));
                models.len() - 1
            });
        add(&mut models[index].1, record);
        if !record.session.is_empty() {
            sessions.insert(record.session.clone());
            models[index].2.insert(record.session.clone());
        }
        if let Some(moment) = chrono::DateTime::from_timestamp_millis(record.at_ms as i64) {
            let local = moment.with_timezone(&window.zone);
            let slot = if window.resolution == "hour" {
                local.format("%Y-%m-%dT%H:00").to_string()
            } else {
                local.date_naive().to_string()
            };
            if let Some(&index) = indexes.get(slot.as_str()) {
                series[index]["reported_cost"] = json!(
                    series[index]["reported_cost"].as_f64().unwrap_or(0.0)
                        + record.reported_cost.unwrap_or(0.0)
                );
                series[index]["total_tokens"] = json!(
                    series[index]["total_tokens"].as_u64().unwrap_or(0)
                        + record.uncached_input
                        + record.cache_read
                        + record.cache_write
                        + record.output
                );
            }
        }
    }
    totals.sessions = sessions.len();
    for (_, entry, sessions) in &mut models {
        entry.sessions = sessions.len();
    }
    models.sort_by(|left, right| {
        right
            .1
            .reported_cost
            .unwrap_or(0.0)
            .total_cmp(&left.1.reported_cost.unwrap_or(0.0))
            .then_with(|| right.1.total_tokens.cmp(&left.1.total_tokens))
    });
    let models: Vec<Value> = models
        .into_iter()
        .map(|(model, entry, _)| {
            let mut value = serde_json::to_value(entry).unwrap_or(Value::Null);
            value["model"] = json!(model);
            value
        })
        .collect();
    json!({"totals":totals,"models":models,"series":series})
}

async fn payload(window: &UsageWindow, enabled: bool, state_dir: &FsPath) -> Value {
    let mut base = json!({"read_at":chrono::Utc::now().timestamp_millis() as f64/1000.0,
        "enabled":enabled,"window":window.view(),"source":null,"account":null,"error":null,
        "limits":null,"limits_error":null,"history":null,"history_error":null});
    let env = Env::default();
    let token = match cursor_token(&env, state_dir).await {
        Ok(token) => token,
        Err(error) => {
            base["error"] = login_error(error);
            return base;
        }
    };
    let subject = match jwt_subject(&token) {
        Ok(subject) => subject,
        Err(error) => {
            base["error"] = login_error(error);
            return base;
        }
    };
    base["source"] = json!(if uses_keychain() {
        "keychain"
    } else {
        "auth_file"
    });
    base["account"] = json!(sha(&subject).chars().take(12).collect::<String>());
    match limits(&token).await {
        Ok(value) => base["limits"] = value,
        Err(error) => base["limits_error"] = error,
    }
    match history(&token, window).await {
        Ok(records) => base["history"] = summarize(&records, window),
        Err(error) => base["history_error"] = error,
    }
    base
}

pub(crate) async fn read(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult {
    let days = query
        .get("days")
        .map(String::as_str)
        .unwrap_or("30")
        .parse::<u32>()
        .map_err(|_| bad_query("days 只能是 1, 7, 30, 90"))?;
    let zone = query.get("tz").map(String::as_str).unwrap_or("UTC");
    let refresh = match query.get("refresh").map(String::as_str).unwrap_or("false") {
        "true" | "True" | "1" | "yes" | "on" => true,
        "false" | "False" | "0" | "no" | "off" => false,
        _ => return Err(bad_query("refresh 必须是布尔值")),
    };
    let window = make_window(days, zone, Utc::now())?;
    let state_dir = native_chat::state_dir(&state);
    let enabled = cursor_settings_enabled(&state_dir);
    let key = CacheKey {
        state_dir: state_dir.clone(),
        days,
        zone: zone.to_owned(),
        enabled,
    };
    if !refresh && let Some(value) = cached(&key) {
        return Ok(Json(value));
    }
    let _guard = READ_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    if !refresh && let Some(value) = cached(&key) {
        return Ok(Json(value));
    }
    let value = payload(&window, enabled, &state_dir).await;
    if let Ok(mut entries) = cache().lock() {
        entries.insert(key, (Instant::now(), value.clone()));
    }
    Ok(Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_window_and_page_overlap_match_cursor_history() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-31T10:45:00+08:00")
            .unwrap()
            .with_timezone(&Utc);
        let window = make_window(7, "Asia/Shanghai", now).unwrap();
        assert_eq!(window.slots.len(), 7);
        assert_eq!(window.slots.first().map(String::as_str), Some("2026-10-25"));
        assert_eq!(window.slots.last().map(String::as_str), Some("2026-10-31"));
        assert_eq!(
            chrono::DateTime::from_timestamp_millis(window.since_ms)
                .unwrap()
                .with_timezone(&window.zone)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2026-10-25 00:00"
        );
        assert_eq!(
            boundary_overlap(
                &["a".into(), "b".into(), "c".into()],
                &["b".into(), "c".into(), "d".into()]
            ),
            2
        );
        assert_eq!(
            boundary_overlap(&["a".into(), "b".into()], &["a".into(), "c".into()]),
            0
        );
    }
}
