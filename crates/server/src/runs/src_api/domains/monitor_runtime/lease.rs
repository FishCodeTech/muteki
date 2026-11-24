//! Durable SRC monitor lease with the source service's owner and fencing contract.
use super::*;

const LEASES: &str = "src_monitor_leases";

fn instance_id() -> &'static str {
    static INSTANCE: OnceLock<String> = OnceLock::new();
    INSTANCE.get_or_init(|| format!("monitor-{}", Uuid::new_v4().simple()))
}

fn key(id: &str, monitor_id: &str) -> String {
    format!("{id}:{monitor_id}")
}

fn active(value: &Value) -> bool {
    value["released_at"].is_null()
        && value["expires_at"].as_str().is_some_and(|expires| {
            chrono::DateTime::parse_from_rfc3339(expires)
                .ok()
                .is_some_and(|expires| expires > Utc::now())
        })
}

fn busy(message: impl Into<String>) -> Error {
    Error::new(StatusCode::CONFLICT, "resource_busy", message)
}

pub(super) struct Guard {
    state: Arc<AppState>,
    task_id: String,
    monitor_id: String,
    owner: String,
    token: u64,
}

impl Guard {
    pub(super) fn acquire(
        state: Arc<AppState>,
        id: &str,
        monitor_id: &str,
        cycle_id: &str,
        ttl_seconds: i64,
    ) -> Api<Self> {
        let resource_key = key(id, monitor_id);
        let previous = state
            .store
            .get(LEASES, &resource_key)
            .map_err(crate::runs::src_api::db)?;
        let owner = format!("{}:{cycle_id}", instance_id());
        if previous.as_ref().is_some_and(|row| active(&row["values"])&&row["values"]["owner"]!=owner) {
            return Err(busy("another monitor cycle still owns the lease"));
        }
        let version = previous
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0);
        let token = previous
            .as_ref()
            .and_then(|row| row["values"]["fencing_token"].as_u64())
            .unwrap_or(0)
            .checked_add(1).ok_or_else(||busy("monitor fencing token exhausted"))?;
        let ttl_seconds = ttl_seconds.max(1);
        let at = Utc::now();
        let value = json!({"resource_kind":"src.monitor.cycle","resource_key":resource_key,
            "owner":owner,"scope":format!("src:{id}"),"ttl_seconds":ttl_seconds,
            "fencing_token":token,"acquired_at":at.to_rfc3339(),
            "expires_at":(at+Duration::seconds(ttl_seconds)).to_rfc3339(),"released_at":null});
        match state
            .store
            .compare_and_put(LEASES, &resource_key, version, value)
            .map_err(crate::runs::src_api::db)?
        {
            Ok(_) => Ok(Self {
                state,
                task_id: id.to_owned(),
                monitor_id: monitor_id.to_owned(),
                owner,
                token,
            }),
            Err(_) => Err(busy("monitor lease changed during acquisition")),
        }
    }

    fn row(&self) -> Api<Value> {
        self.state
            .store
            .get(LEASES, &key(&self.task_id, &self.monitor_id))
            .map_err(crate::runs::src_api::db)?
            .ok_or_else(|| busy("monitor lease is missing"))
    }

    pub(super) fn assert_active(&self) -> Api<()> {
        let row = self.row()?;
        let value = &row["values"];
        if !active(value)
            || value["scope"] != format!("src:{}", self.task_id)
            || value["owner"] != self.owner
            || value["fencing_token"] != self.token
        {
            return Err(busy("monitor lease is expired, released, or fenced"));
        }
        Ok(())
    }

    pub(super) fn assert_fence(&self) -> Api<()> {
        let row = self.row()?;
        let value = &row["values"];
        if !value["released_at"].is_null()
            || value["scope"] != format!("src:{}", self.task_id)
            || value["owner"] != self.owner
            || value["fencing_token"] != self.token
        {
            return Err(busy("monitor lease was replaced before settlement"));
        }
        Ok(())
    }

    fn release(&self) -> Api<()> {
        let row = self.row()?;
        if row["values"]["owner"] != self.owner || row["values"]["fencing_token"] != self.token {
            return Ok(());
        }
        let mut value = row["values"].clone();
        value["released_at"] = json!(now());
        self.state
            .store
            .compare_and_put(
                LEASES,
                &key(&self.task_id, &self.monitor_id),
                row["version"].as_u64().unwrap_or(0),
                value,
            )
            .map_err(crate::runs::src_api::db)?
            .map_err(|_| busy("monitor lease changed during release"))?;
        Ok(())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            tracing::warn!(task_id=%self.task_id, monitor_id=%self.monitor_id,
                code=error.code, %error.message, "SRC monitor lease release failed");
        }
    }
}

pub(super) fn fence_for_recovery(state: &AppState, id: &str, monitor_id: &str) -> Api<bool> {
    let resource_key = key(id, monitor_id);
    let Some(row) = state
        .store
        .get(LEASES, &resource_key)
        .map_err(crate::runs::src_api::db)?
    else {
        return Ok(true);
    };
    if active(&row["values"]) {
        return Ok(false);
    }
    if !row["values"]["released_at"].is_null() {
        return Ok(true);
    }
    let mut value = row["values"].clone();
    value["released_at"] = json!(now());
    Ok(state
        .store
        .compare_and_put(
            LEASES,
            &resource_key,
            row["version"].as_u64().unwrap_or(0),
            value,
        )
        .map_err(crate::runs::src_api::db)?
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_monitor_lease_fences_the_previous_owner() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let first = Guard::acquire(state.clone(), "src-task", "monitor-1", "cycle-1", 5).unwrap();
        assert_eq!(first.token, 1);
        assert_eq!(
            Guard::acquire(state.clone(), "src-task", "monitor-1", "cycle-2", 5)
                .err()
                .unwrap()
                .code,
            "resource_busy"
        );
        first.release().unwrap();
        let second = Guard::acquire(state, "src-task", "monitor-1", "cycle-2", 5).unwrap();
        assert_eq!(second.token, 2);
        assert_eq!(first.assert_fence().unwrap_err().code, "resource_busy");
    }
}
