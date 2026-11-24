use super::GitHubError;
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tcode_core::pull_request::PullRequestKey;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ReadKey {
    pub(super) pull_request: PullRequestKey,
    /// The credential fingerprint: answers are the account's own and never cross to another.
    pub(super) account: String,
    pub(super) read: String,
}

/// An answer and when its reader should ask again.
#[derive(Debug)]
pub struct Fresh<V> {
    pub value: Arc<V>,
    pub expires_at: SystemTime,
}
impl<V> Clone for Fresh<V> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            expires_at: self.expires_at,
        }
    }
}

type Outcome<V> = Result<Fresh<V>, GitHubError>;

struct Flight<V> {
    outcome: Mutex<Option<Outcome<V>>>,
    done: Condvar,
}

enum Slot<V> {
    Ready {
        fresh: Fresh<V>,
        until: Instant,
        bytes: usize,
    },
    Loading(Arc<Flight<V>>),
}

/// Answers shared until their TTL, with one read in flight per key; a failure is handed to the
/// readers already waiting and kept for no one else. The byte budget bounds what is retained:
/// an answer larger than a quarter of it is served but not kept.
pub(super) struct ReadCache<V> {
    slots: Mutex<HashMap<ReadKey, Slot<V>>>,
    budget: usize,
}

impl<V> ReadCache<V> {
    pub(super) fn new(budget: usize) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            budget,
        }
    }

    pub(super) fn read(
        &self,
        key: ReadKey,
        load: impl FnOnce() -> Result<(V, Duration), GitHubError>,
        size: impl Fn(&V) -> usize,
    ) -> Outcome<V> {
        let flight = {
            let mut slots = self.slots.lock().unwrap();
            match slots.get(&key) {
                Some(Slot::Ready { fresh, until, .. }) if *until > Instant::now() => {
                    return Ok(fresh.clone());
                }
                Some(Slot::Loading(flight)) => {
                    let flight = flight.clone();
                    drop(slots);
                    let mut outcome = flight.outcome.lock().unwrap();
                    while outcome.is_none() {
                        outcome = flight.done.wait(outcome).unwrap();
                    }
                    return outcome.clone().unwrap();
                }
                _ => {}
            }
            // Another account's answers for this host go as soon as this one reads.
            slots.retain(|other, _| {
                other.pull_request.host != key.pull_request.host || other.account == key.account
            });
            let flight = Arc::new(Flight {
                outcome: Mutex::new(None),
                done: Condvar::new(),
            });
            slots.insert(key.clone(), Slot::Loading(flight.clone()));
            flight
        };
        // A load that panics still releases its waiters, with a failure no one keeps.
        let mut landing = Landing {
            cache: self,
            key: Some(key),
            flight,
        };
        match load() {
            Ok((value, ttl)) => {
                let bytes = size(&value);
                let fresh = Fresh {
                    value: Arc::new(value),
                    expires_at: SystemTime::now() + ttl,
                };
                let keep = (!ttl.is_zero() && bytes <= self.budget / 4).then(|| Slot::Ready {
                    fresh: fresh.clone(),
                    until: Instant::now() + ttl,
                    bytes,
                });
                landing.land(Ok(fresh), keep)
            }
            Err(error) => landing.land(Err(error), None),
        }
    }

    /// Answers already held, without reading.
    pub(super) fn held(&self, pull_request: &PullRequestKey) -> Vec<Arc<V>> {
        let now = Instant::now();
        self.slots
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(key, slot)| match slot {
                Slot::Ready { fresh, until, .. }
                    if key.pull_request == *pull_request && *until > now =>
                {
                    Some(fresh.value.clone())
                }
                _ => None,
            })
            .collect()
    }

    pub(super) fn invalidate(&self, pull_request: &PullRequestKey) {
        self.slots
            .lock()
            .unwrap()
            .retain(|key, _| key.pull_request != *pull_request);
    }

    fn trim(&self, slots: &mut HashMap<ReadKey, Slot<V>>) {
        let now = Instant::now();
        slots.retain(|_, slot| !matches!(slot, Slot::Ready { until, .. } if *until <= now));
        let held = |slots: &HashMap<ReadKey, Slot<V>>| {
            slots
                .values()
                .map(|slot| match slot {
                    Slot::Ready { bytes, .. } => *bytes,
                    Slot::Loading(_) => 0,
                })
                .sum::<usize>()
        };
        while held(slots) > self.budget {
            let Some(soonest) = slots
                .iter()
                .filter_map(|(key, slot)| match slot {
                    Slot::Ready { until, .. } => Some((key.clone(), *until)),
                    Slot::Loading(_) => None,
                })
                .min_by_key(|(_, until)| *until)
                .map(|(key, _)| key)
            else {
                break;
            };
            slots.remove(&soonest);
        }
    }
}

struct Landing<'a, V> {
    cache: &'a ReadCache<V>,
    key: Option<ReadKey>,
    flight: Arc<Flight<V>>,
}
impl<V> Landing<'_, V> {
    fn land(&mut self, outcome: Outcome<V>, keep: Option<Slot<V>>) -> Outcome<V> {
        let Some(key) = self.key.take() else {
            return outcome;
        };
        {
            let mut slots = self.cache.slots.lock().unwrap();
            // An invalidation during the read removed this flight; its answer is not kept.
            if matches!(slots.get(&key), Some(Slot::Loading(current)) if Arc::ptr_eq(current, &self.flight))
            {
                match keep {
                    Some(slot) => {
                        slots.insert(key, slot);
                        self.cache.trim(&mut slots);
                    }
                    None => {
                        slots.remove(&key);
                    }
                }
            }
        }
        *self.flight.outcome.lock().unwrap() = Some(outcome.clone());
        self.flight.done.notify_all();
        outcome
    }
}
impl<V> Drop for Landing<'_, V> {
    fn drop(&mut self) {
        let _ = self.land(Err(GitHubError::Request), None);
    }
}
