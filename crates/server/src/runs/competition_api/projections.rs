//! Competition migration receipts, projection manager, and rebuild support.
use super::*;

const MIGRATIONS: &str = "competition_schema_migrations";
const WATERMARKS: &str = "competition_projection_watermarks";
const PROJECTION_SPECS: &str = "competition_projection_specs";
const PROJECTION_TABLES: &str = "competition_projection_tables";
pub(crate) const RUN_FLAG_INVALIDATION: &str = "run_flag_invalidation";

fn projection_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProjectionKind {
    RunFlagInvalidation,
    TableBacked { collection: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionDescriptor {
    pub(crate) name: String,
    pub(crate) kind: ProjectionKind,
}

fn empty_table_state(name: &str, collection: &str) -> Value {
    json!({
        "projection_name": name,
        "collection": collection,
        "event_count": 0,
        "by_event_type": {},
        "by_competition": {},
        "rows": {},
        "updated_at": now(),
    })
}

pub(super) fn register(state: &AppState) -> ApiResult<()> {
    if state.store.get(MIGRATIONS, "1").map_err(ApiError::internal)?.is_none() {
        let migration = json!({"version":1,"name":"competition_core_schema","applied_at":now()});
        state.store.put_many_if(&[(MIGRATIONS, "1", None)], &[(MIGRATIONS, "1", &migration)])
            .map_err(ApiError::internal)?;
    }
    if state.store.get(WATERMARKS, RUN_FLAG_INVALIDATION).map_err(ApiError::internal)?.is_none() {
        let created_at = now();
        let watermark = json!({
            "projection_name": RUN_FLAG_INVALIDATION,
            "last_event_seq": 0,
            "generation": 0,
            "schema_version": 1,
            "created_at": created_at,
            "updated_at": created_at,
            "payload": {"kind": "run_flag_invalidation"}
        });
        state.store.put_many_if(
            &[(WATERMARKS, RUN_FLAG_INVALIDATION, None)],
            &[(WATERMARKS, RUN_FLAG_INVALIDATION, &watermark)],
        ).map_err(ApiError::internal)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn register_table_projection(
    state: &AppState,
    name: &str,
    collection: &str,
) -> ApiResult<ProjectionDescriptor> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.projection.invalid_name",
            "projection name cannot be empty",
        ));
    }
    let collection = if collection.trim().is_empty() {
        format!("competition_projection_{name}")
    } else {
        collection.trim().to_owned()
    };
    let _guard = projection_lock().lock().unwrap();
    register(state)?;
    if name == RUN_FLAG_INVALIDATION
        || state
            .store
            .get(PROJECTION_SPECS, name)
            .map_err(ApiError::internal)?
            .is_some()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "competition.projection.already_registered",
            format!("projection already registered: {name}"),
        ));
    }
    let created_at = now();
    let spec = json!({
        "projection_name": name,
        "kind": "table",
        "collection": collection,
        "created_at": created_at,
    });
    let table_state = empty_table_state(name, &collection);
    let watermark = json!({
        "projection_name": name,
        "last_event_seq": 0,
        "generation": 0,
        "schema_version": 1,
        "created_at": created_at,
        "updated_at": created_at,
        "payload": {"kind": "table", "collection": collection}
    });
    let existing_wm = state
        .store
        .get(WATERMARKS, name)
        .map_err(ApiError::internal)?;
    let existing_tbl = state
        .store
        .get(PROJECTION_TABLES, name)
        .map_err(ApiError::internal)?;
    let mut expected = vec![(PROJECTION_SPECS, name, None)];
    let mut records = vec![(PROJECTION_SPECS, name, &spec)];
    if existing_wm.is_none() {
        expected.push((WATERMARKS, name, None));
        records.push((WATERMARKS, name, &watermark));
    }
    if existing_tbl.is_none() {
        expected.push((PROJECTION_TABLES, name, None));
        records.push((PROJECTION_TABLES, name, &table_state));
    }
    state
        .store
        .put_many_if(&expected, &records)
        .map_err(ApiError::internal)?;
    Ok(ProjectionDescriptor {
        name: name.to_owned(),
        kind: ProjectionKind::TableBacked { collection },
    })
}

#[cfg(test)]
pub(crate) fn table_projection_state(state: &AppState, name: &str) -> ApiResult<Option<Value>> {
    state
        .store
        .get(PROJECTION_TABLES, name.trim())
        .map_err(ApiError::internal)
}

pub(super) fn schema_version(state: &AppState) -> ApiResult<Option<u64>> {
    Ok(state
        .store
        .list(MIGRATIONS)
        .map_err(ApiError::internal)?
        .iter()
        .filter_map(|row| row["version"].as_u64())
        .max())
}

pub(crate) fn names(state: &AppState) -> ApiResult<Vec<String>> {
    register(state)?;
    names_registered(state)
}

fn names_registered(state: &AppState) -> ApiResult<Vec<String>> {
    let mut set = std::collections::BTreeSet::new();
    set.insert(RUN_FLAG_INVALIDATION.to_owned());
    for row in state
        .store
        .list(PROJECTION_SPECS)
        .map_err(ApiError::internal)?
    {
        if let Some(name) = row["projection_name"].as_str().filter(|s| !s.is_empty()) {
            set.insert(name.to_owned());
        }
    }
    for row in state.store.list(WATERMARKS).map_err(ApiError::internal)? {
        if let Some(name) = row["projection_name"].as_str().filter(|s| !s.is_empty()) {
            set.insert(name.to_owned());
        }
    }
    Ok(set.into_iter().collect())
}

pub(crate) fn get(state: &AppState, name: &str) -> ApiResult<ProjectionDescriptor> {
    register(state)?;
    get_registered(state, name)
}

fn get_registered(state: &AppState, name: &str) -> ApiResult<ProjectionDescriptor> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "competition.projection.name_required",
            "projection name is required",
        ));
    }
    if name == RUN_FLAG_INVALIDATION {
        return Ok(ProjectionDescriptor {
            name: RUN_FLAG_INVALIDATION.to_owned(),
            kind: ProjectionKind::RunFlagInvalidation,
        });
    }
    if let Some(spec) = state
        .store
        .get(PROJECTION_SPECS, name)
        .map_err(ApiError::internal)?
    {
        let collection = field(&spec, "collection")
            .to_owned()
            .if_empty(&format!("competition_projection_{name}"));
        return Ok(ProjectionDescriptor {
            name: name.to_owned(),
            kind: ProjectionKind::TableBacked { collection },
        });
    }
    if let Some(wm) = state
        .store
        .get(WATERMARKS, name)
        .map_err(ApiError::internal)?
    {
        let collection = field(&wm["payload"], "collection")
            .to_owned()
            .if_empty(&format!("competition_projection_{name}"));
        return Ok(ProjectionDescriptor {
            name: name.to_owned(),
            kind: ProjectionKind::TableBacked { collection },
        });
    }
    Err(ApiError::new(
        StatusCode::NOT_FOUND,
        "competition.projection.not_found",
        format!("unknown projection: {name}"),
    ))
}

pub(crate) fn watermark(state: &AppState, name: &str) -> ApiResult<u64> {
    let descriptor = get(state, name)?;
    Ok(state
        .store
        .get(WATERMARKS, &descriptor.name)
        .map_err(ApiError::internal)?
        .and_then(|row| row["last_event_seq"].as_u64())
        .unwrap_or(0))
}

pub(super) fn watermarks(state: &AppState) -> ApiResult<Value> {
    register(state)?;
    let mut values = serde_json::Map::new();
    for row in state.store.list(WATERMARKS).map_err(ApiError::internal)? {
        if let Some(name) = row["projection_name"].as_str() {
            values.insert(
                name.to_owned(),
                json!(row["last_event_seq"].as_u64().unwrap_or(0)),
            );
        }
    }
    Ok(Value::Object(values))
}

fn event_head(state: &AppState) -> ApiResult<u64> {
    Ok(state
        .store
        .list(EVENTS)
        .map_err(ApiError::internal)?
        .iter()
        .filter_map(|event| event["global_seq"].as_u64())
        .max()
        .unwrap_or(0))
}

pub(crate) fn lagging(state: &AppState) -> ApiResult<Vec<String>> {
    let head = event_head(state)?;
    let mut out = Vec::new();
    for name in names(state)? {
        if watermark(state, &name)? < head {
            out.push(name);
        }
    }
    Ok(out)
}

fn apply_run_flag_invalidation(state: &Arc<AppState>, event: &Value) -> ApiResult<()> {
    if event["event_type"] != "competition.platform_submission.state_changed"
        || event["payload"]["to"] != "wrong"
    {
        return Ok(());
    }
    let submission_id = field(&event["payload"], "submission_id");
    let Some(submission) = state
        .store
        .get(SUBMISSIONS, submission_id)
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    let Some(candidate) = state
        .store
        .get(CANDIDATES, field(&submission, "candidate_id"))
        .map_err(ApiError::internal)?
    else {
        return Ok(());
    };
    let run_id = field(&candidate, "source_run_id");
    if run_id.is_empty() || !super::super::graph::graph_db_path(state, run_id).is_file() {
        return Ok(());
    }
    let value = read_secret(state, field(&candidate, "answer_ref"))?;
    let mut graph =
        super::super::graph::SharedGraph::open(state, run_id).map_err(ApiError::internal)?;
    graph
        .reopen_after_false_positive(
            "competition",
            &value,
            &format!("remote verdict wrong (submission {submission_id})"),
        )
        .map_err(ApiError::internal)?;
    Ok(())
}

fn fold_table_event(mut table: Value, event: &Value) -> (Value, Option<(String, Value)>) {
    let count = table["event_count"].as_u64().unwrap_or(0) + 1;
    table["event_count"] = json!(count);
    let event_type = field(event, "event_type").to_owned();
    if !event_type.is_empty() {
        let prev = table["by_event_type"][&event_type].as_u64().unwrap_or(0);
        table["by_event_type"][&event_type] = json!(prev + 1);
    }
    let competition_id = field(event, "competition_id").to_owned();
    if !competition_id.is_empty() {
        let prev = table["by_competition"][&competition_id]
            .as_u64()
            .unwrap_or(0);
        table["by_competition"][&competition_id] = json!(prev + 1);
    }
    let aggregate_type = field(event, "aggregate_type");
    let aggregate_id = field(event, "aggregate_id");
    let seq = event["global_seq"].as_u64().unwrap_or(0);
    let row_key = if !aggregate_type.is_empty() && !aggregate_id.is_empty() {
        format!("{aggregate_type}:{aggregate_id}")
    } else {
        format!("event:{seq}")
    };
    let row_val = json!({
        "key": row_key,
        "aggregate_type": aggregate_type,
        "aggregate_id": aggregate_id,
        "competition_id": competition_id,
        "last_event_type": event_type,
        "last_global_seq": seq,
        "payload": event["payload"],
        "updated_at": now(),
    });
    table["rows"][&row_key] = row_val.clone();
    table["last_event_seq"] = json!(seq);
    table["updated_at"] = json!(now());
    (table, Some((row_key, row_val)))
}

fn run_locked(
    state: &Arc<AppState>,
    descriptor: &ProjectionDescriptor,
    limit: usize,
) -> ApiResult<usize> {
    let limit = limit.max(1);
    let name = descriptor.name.as_str();
    let mut applied = 0;
    loop {
        let Some(mut watermark_row) = state
            .store
            .get(WATERMARKS, name)
            .map_err(ApiError::internal)?
        else {
            return Ok(applied);
        };
        let after = watermark_row["last_event_seq"].as_u64().unwrap_or(0);
        let events = state
            .store
            .list_competition_events_after(after, limit)
            .map_err(ApiError::internal)?;
        if events.is_empty() {
            return Ok(applied);
        }
        let batch_len = events.len();
        let mut raced = false;
        for event in events {
            match &descriptor.kind {
                ProjectionKind::RunFlagInvalidation => {
                    apply_run_flag_invalidation(state, &event)?;
                    let mut next = watermark_row.clone();
                    next["last_event_seq"] = event["global_seq"].clone();
                    next["updated_at"] = json!(now());
                    if !state
                        .store
                        .put_many_if(
                            &[(WATERMARKS, name, Some(&watermark_row))],
                            &[(WATERMARKS, name, &next)],
                        )
                        .map_err(ApiError::internal)?
                    {
                        raced = true;
                        break;
                    }
                    watermark_row = next;
                    applied += 1;
                }
                ProjectionKind::TableBacked { collection } => {
                    let prior_table = state
                        .store
                        .get(PROJECTION_TABLES, name)
                        .map_err(ApiError::internal)?;
                    let current_table = prior_table
                        .clone()
                        .unwrap_or_else(|| empty_table_state(name, collection));
                    let (next_table, row_entry) = fold_table_event(current_table, &event);
                    let mut next_wm = watermark_row.clone();
                    next_wm["last_event_seq"] = event["global_seq"].clone();
                    next_wm["updated_at"] = json!(now());
                    let existing_row = if let Some((ref key, _)) = row_entry {
                        state
                            .store
                            .get(collection, key)
                            .map_err(ApiError::internal)?
                    } else {
                        None
                    };
                    let mut expected = vec![
                        (WATERMARKS, name, Some(&watermark_row)),
                        (PROJECTION_TABLES, name, prior_table.as_ref()),
                    ];
                    let mut records = vec![
                        (WATERMARKS, name, &next_wm),
                        (PROJECTION_TABLES, name, &next_table),
                    ];
                    if let Some((ref key, ref val)) = row_entry {
                        expected.push((collection.as_str(), key.as_str(), existing_row.as_ref()));
                        records.push((collection.as_str(), key.as_str(), val));
                    }
                    if !state
                        .store
                        .put_many_if(&expected, &records)
                        .map_err(ApiError::internal)?
                    {
                        raced = true;
                        break;
                    }
                    watermark_row = next_wm;
                    applied += 1;
                }
            }
        }
        if !raced && batch_len < limit {
            return Ok(applied);
        }
    }
}

#[cfg(test)]
pub(crate) fn run(state: &Arc<AppState>, name: &str) -> ApiResult<usize> {
    let descriptor = get(state, name)?;
    let _guard = projection_lock().lock().unwrap();
    run_locked(state, &descriptor, 1000)
}

pub(crate) fn run_all(state: &Arc<AppState>) -> ApiResult<Value> {
    register(state)?;
    let all_names = names_registered(state)?;
    let _guard = projection_lock().lock().unwrap();
    let mut results = serde_json::Map::new();
    for name in all_names {
        let descriptor = get_registered(state, &name)?;
        let count = run_locked(state, &descriptor, 1000)?;
        results.insert(name, json!(count));
    }
    Ok(Value::Object(results))
}

fn rebuild_locked(
    state: &Arc<AppState>,
    descriptor: &ProjectionDescriptor,
    limit: usize,
) -> ApiResult<usize> {
    let name = descriptor.name.as_str();
    loop {
        let prior_wm = state
            .store
            .get(WATERMARKS, name)
            .map_err(ApiError::internal)?;
        let next_gen = prior_wm
            .as_ref()
            .and_then(|row| row["generation"].as_u64())
            .unwrap_or(0)
            + 1;
        let timestamp = now();
        let mut reset_wm = prior_wm.clone().unwrap_or_else(|| {
            json!({
                "projection_name": name,
                "schema_version": 1,
                "created_at": timestamp,
                "payload": {},
            })
        });
        reset_wm["last_event_seq"] = json!(0);
        reset_wm["generation"] = json!(next_gen);
        reset_wm["updated_at"] = json!(timestamp);

        match &descriptor.kind {
            ProjectionKind::RunFlagInvalidation => {
                if state
                    .store
                    .put_many_if(
                        &[(WATERMARKS, name, prior_wm.as_ref())],
                        &[(WATERMARKS, name, &reset_wm)],
                    )
                    .map_err(ApiError::internal)?
                {
                    break;
                }
            }
            ProjectionKind::TableBacked { collection } => {
                let prior_table = state
                    .store
                    .get(PROJECTION_TABLES, name)
                    .map_err(ApiError::internal)?;
                let reset_table = empty_table_state(name, collection);
                let deletes: Vec<(String, String)> = prior_table
                    .as_ref()
                    .and_then(|table| table["rows"].as_object())
                    .into_iter()
                    .flat_map(|rows| rows.keys())
                    .map(|key| (collection.clone(), key.clone()))
                    .collect();
                if state
                    .store
                    .put_many_if_with_deletes(
                        &[
                            (WATERMARKS, name, prior_wm.as_ref()),
                            (PROJECTION_TABLES, name, prior_table.as_ref()),
                        ],
                        &[
                            (WATERMARKS, name, &reset_wm),
                            (PROJECTION_TABLES, name, &reset_table),
                        ],
                        |_| Ok(Some(deletes)),
                    )
                    .map_err(ApiError::internal)?
                {
                    break;
                }
            }
        }
    }
    run_locked(state, descriptor, limit)
}

pub(crate) fn rebuild(state: &Arc<AppState>, name: &str) -> ApiResult<usize> {
    let descriptor = get(state, name)?;
    let _guard = projection_lock().lock().unwrap();
    rebuild_locked(state, &descriptor, 1000)
}

pub(crate) fn rebuild_all(state: &Arc<AppState>) -> ApiResult<Value> {
    let all_names = names(state)?;
    let _guard = projection_lock().lock().unwrap();
    let mut results = serde_json::Map::new();
    for name in all_names {
        let descriptor = get(state, &name)?;
        let count = rebuild_locked(state, &descriptor, 1000)?;
        results.insert(name, json!(count));
    }
    Ok(Value::Object(results))
}

pub(super) fn consume(state: &Arc<AppState>) -> ApiResult<usize> {
    let counts = run_all(state)?;
    Ok(counts
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, v)| v.as_u64())
        .sum::<u64>() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_rebuild_rolls_back_watermark_when_row_delete_fails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("competition-projection.sqlite");
        let state = Arc::new(AppState::new(crate::store::Store::open(&path).unwrap()));
        register(&state).unwrap();
        register_table_projection(&state,"fixture", "competition_projection_fixture").unwrap();
        state.store.put(EVENTS,"fixture-event",&json!({"global_seq":1,
            "event_type":"competition.updated","competition_id":"comp-1",
            "aggregate_type":"competition","aggregate_id":"comp-1",
            "payload":{"title":"Fixture"}})).unwrap();
        assert_eq!(run(&state,"fixture").unwrap(),1);
        assert_eq!(watermark(&state,"fixture").unwrap(),1);
        let before = table_projection_state(&state,"fixture").unwrap().unwrap();
        assert_eq!(before["event_count"],1);
        assert!(state.store.get("competition_projection_fixture","competition:comp-1")
            .unwrap().is_some());

        let sql = rusqlite::Connection::open(&path).unwrap();
        sql.execute_batch("CREATE TRIGGER fail_projection_delete BEFORE DELETE ON documents
            WHEN OLD.collection='competition_projection_fixture'
            BEGIN SELECT RAISE(ABORT,'fixture delete failed'); END;").unwrap();
        assert!(rebuild(&state,"fixture").is_err());
        assert_eq!(watermark(&state,"fixture").unwrap(),1);
        assert_eq!(table_projection_state(&state,"fixture").unwrap().unwrap(),before);
        assert!(state.store.get("competition_projection_fixture","competition:comp-1")
            .unwrap().is_some());

        sql.execute_batch("DROP TRIGGER fail_projection_delete;").unwrap();
        assert_eq!(rebuild(&state,"fixture").unwrap(),1);
        assert_eq!(watermark(&state,"fixture").unwrap(),1);
        assert_eq!(table_projection_state(&state,"fixture").unwrap().unwrap()["event_count"],1);
        assert_eq!(rebuild_all(&state).unwrap()["fixture"],1);
    }
}
