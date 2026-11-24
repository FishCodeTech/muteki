//! Finished ordinary local Runs need not retain an open ownership-lock FD forever.
//! Keep their in-memory handle and history; a later lifecycle action claims the lock again.
use super::*;

pub(super) fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if let Err(error) = release(&state) {
                tracing::warn!(%error, "Idle Run ownership release is pending");
            }
        }
    });
}

fn release(state: &AppState) -> Result<(), String> {
    let bound: HashSet<String> = state.store.list("competition_bindings_live")
        .map_err(|e| e.to_string())?.iter()
        .filter_map(|row| row["run_id"].as_str().map(str::to_owned)).collect();
    let live: HashSet<String> = state.store.list("runner_commands")
        .map_err(|e| e.to_string())?.iter()
        .filter(|row| !crate::runner_execution::facts(row).stopped)
        .filter_map(|row| row["run_id"].as_str().map(str::to_owned)).collect();
    let handles: Vec<_> = state.runs.handles.lock().unwrap().iter()
        .map(|(id, handle)| (id.clone(), handle.clone())).collect();
    for (id, handle) in handles {
        if bound.contains(&id) || live.contains(&id) { continue; }
        let Ok(_lifecycle) = handle.delete_lock.try_lock() else { continue };
        let Some(run) = state.store.get("runs", &id).map_err(|e| e.to_string())? else { continue };
        if run["running"] != false || run["src_task_id"].is_string()
            || !matches!(run["status"].as_str(), Some("solved" | "finished"))
            || run["effective_configuration"]["worker_node_id"].as_str().is_some_and(|node| node != "local")
            || handle.deleting.load(Ordering::SeqCst)
            || handle.cleanup_watcher.load(Ordering::SeqCst)
            || !handle.workers.lock().unwrap().is_empty()
            || handle.coordinator_tasks.lock().unwrap().iter().any(|task| !task.is_finished())
            || handle.worker_tasks.lock().unwrap().iter().any(|task| !task.is_finished())
            || handle.coordinator_abort.lock().unwrap().as_ref().is_some_and(|task| !task.is_finished())
            || handle.followup_abort.lock().unwrap().as_ref().is_some_and(|task| !task.is_finished()) { continue; }
        handle.execution_owner.lock().unwrap().take();
    }
    Ok(())
}
