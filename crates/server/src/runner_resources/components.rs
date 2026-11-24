//! Managed components reserve memory once per Run generation, including idle daemons.
use super::*;
use std::collections::HashSet;

pub(super) const BROWSER_BYTES: u64 = 512 * 1024 * 1024;
pub(super) type Owners = HashSet<(String, u64)>;
pub(super) fn key(row: &Value) -> Option<(String,u64)> {
    Some((row["run_id"].as_str()?.to_owned(), row["generation"].as_u64().filter(|value|*value>0)?))
}
pub(super) struct Inventory {
    pub browsers: Owners,
    pub runs: HashSet<String>,
}
pub(super) fn inventory(store: &crate::store::Store, dispatches: &[Value]) -> Result<Inventory,String> {
    inventory_node(store,dispatches,"local")
}
pub(super) fn inventory_node(store:&crate::store::Store,dispatches:&[Value],node:&str)->Result<Inventory,String>{
    let mut value=Inventory{browsers:Owners::new(),runs:HashSet::new()};
    for row in dispatches {
        if matches!(row["status"].as_str(),Some("running"|"detached")) {
            if let Some(run)=row["run_id"].as_str(){value.runs.insert(run.to_owned());}
            if row["browser"]==true {if let Some(key)=key(row){value.browsers.insert(key);}}
        }
    }
    for row in store.list(if node=="local"{"runner_runs"}else{"runner_controller_runs"}).map_err(|e|e.to_string())? {
        if node!="local"&&row["node_id"]!=node{continue;}
        if row["state"]!="stopped" {
            if let Some(run)=row["owner_run_id"].as_str().or_else(||row["run_id"].as_str()) {value.runs.insert(run.to_owned());}
            if row["browser_reserved"]==true {if let Some(key)=key(&row){value.browsers.insert(key);}}
        }
    }
    Ok(value)
}

pub(super) fn required_memory(owners:&Owners, candidate:&Value, active_workers:usize, worker_bytes:u64)->u64 {
    let additional=usize::from(candidate["browser"]==true && key(candidate).is_none_or(|key|!owners.contains(&key)));
    (active_workers as u64).saturating_add(1).saturating_mul(worker_bytes)
        .saturating_add((owners.len().saturating_add(additional) as u64).saturating_mul(BROWSER_BYTES))
}
pub(super) fn waiting(store:&crate::store::Store, current:&Value, reason:Value)->Result<bool,String>{
    if current["waiting"]!=reason {
        let mut row=current.clone();row["waiting"]=reason;
        store.put(DISPATCHES,row["id"].as_str().ok_or("Dispatch ID missing")?,&row).map_err(|e|e.to_string())?;
    }
    Ok(false)
}
