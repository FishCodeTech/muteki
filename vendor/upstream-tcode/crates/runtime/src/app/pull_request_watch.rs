use super::active_session::QueuedMessageKind;
use super::*;
use tcode_core::control::WatchEvent;
use tcode_core::pull_request::{self, PullRequestKey, PullRequestSource, PullRequestState};
use tcode_core::pull_request_watch::{
    self as watch, PendingWake, PullRequestRemark, PullRequestWatch, PullRequestWatchRead,
    WatchNotice,
};
use tcode_protocol::{CommandResponse, ProtocolError};
use tcode_services::forge::{Fingerprint, Forge, ForgeError, ForgeErrorKind, Tails};

/// Passes are this far apart, counted from the end of the previous one.
const PASS_SPACING: Duration = Duration::from_secs(2 * 60);
/// Without a host fingerprint, a pull request with nothing in flight is read again only when its
/// sync snapshot moves, or after this long.
const QUIET_REREAD: Duration = Duration::from_secs(10 * 60);
/// With a host fingerprint, how long until the activity is read again anyway: the fingerprint
/// does not see edits to comments inside review threads.
const FINGERPRINT_REREAD: Duration = Duration::from_secs(30 * 60);
/// How long mergeability the host is still working out keeps a pull request read every pass;
/// past it the host's answer is not coming soon, and the usual cadence resumes.
const PENDING_MERGEABILITY: Duration = Duration::from_secs(10 * 60);
const GROUP_CONCURRENCY: usize = 4;

/// Watch progress and pagination caches remain independent per project and pull request.
type GroupKey = (Option<String>, PullRequestKey);

/// The last successful read of a pull request, kept in memory: a restart reads each once.
pub(super) struct LastRead {
    /// When the activity was last read.
    at: Instant,
    snapshot: String,
    /// A check was still running, so the detail can move unannounced.
    checks_running: bool,
    /// The head whose mergeability the host was still working out, and since when.
    pending: Option<(Option<String>, Instant)>,
    remarks_complete: bool,
    /// The watch generations this read evaluated; one started since takes a full first look.
    watches: HashSet<(String, String)>,
    status: Option<String>,
    remarks: Option<String>,
}

impl LastRead {
    /// Whether the detail can move unannounced: a check is running, or the host has not yet
    /// worked out mergeability for long. A host that cannot work it out at all moves nothing.
    fn in_flight(&self, now: Instant) -> bool {
        self.checks_running
            || self.pending.as_ref().is_some_and(|(_, since)| {
                now.saturating_duration_since(*since) < PENDING_MERGEABILITY
            })
    }
}

pub(super) struct WatchRuntime {
    forge: Arc<dyn Forge>,
    last_reads: HashMap<GroupKey, LastRead>,
    failures: HashMap<GroupKey, u32>,
    /// Review-thread replies past each thread's first page.
    tails: HashMap<GroupKey, Tails>,
    passing: bool,
    /// Resumed subscriptions must read detail and activity even if fingerprints did not move.
    refresh: HashSet<(String, PullRequestKey)>,
    sweep_pending: bool,
    worker: Option<HostTask<()>>,
}

impl WatchRuntime {
    pub(super) fn new(forge: Arc<dyn Forge>) -> Self {
        Self {
            forge,
            last_reads: HashMap::new(),
            failures: HashMap::new(),
            tails: HashMap::new(),
            passing: false,
            refresh: HashSet::new(),
            sweep_pending: false,
            worker: None,
        }
    }
}

#[derive(Clone)]
struct Target {
    thread: String,
    started_at: String,
    url: String,
}

struct Group {
    key: GroupKey,
    targets: Vec<Target>,
    snapshot: String,
}

#[derive(Clone, Copy)]
struct Plan {
    detail: bool,
    activity: bool,
    /// The half-hour backstop is due: cached review-thread tails are paged again, so an edited
    /// reply past a thread's first page is seen though the thread's count is unchanged.
    reread_tails: bool,
}

type ActivityRead = (Result<Option<Vec<PullRequestRemark>>, ForgeError>, Tails);

type GroupRead = Result<(PullRequestWatchRead, Option<Option<Vec<PullRequestRemark>>>), ForgeError>;

fn snapshot_fingerprint(link: &pull_request::ThreadPullRequestLink) -> String {
    link.snapshot.as_ref().map_or_else(String::new, |snapshot| {
        format!(
            "{:?} {} {:?} {:?} {:?} {}",
            snapshot.state,
            snapshot.updated_at,
            snapshot.checks_state,
            snapshot.mergeability,
            snapshot.review_decision,
            snapshot.is_draft
        )
    })
}

fn rate_limited(error: &ForgeError) -> bool {
    matches!(
        error.kind,
        ForgeErrorKind::RateLimited { .. } | ForgeErrorKind::Paused { .. }
    )
}

/// Which reads a pass makes for one pull request. With a host fingerprint the detail is read
/// when anything moved or a check is still running, the activity only when the remarks moved;
/// without one, both whenever the sync snapshot moved or something is in flight.
fn plan(
    group: &Group,
    last: Option<&LastRead>,
    fingerprint: Option<&Fingerprint>,
    now: Instant,
) -> Plan {
    let full = Plan {
        detail: true,
        activity: true,
        reread_tails: false,
    };
    let Some(last) = last.filter(|last| {
        group.targets.iter().all(|target| {
            last.watches
                .contains(&(target.thread.clone(), target.started_at.clone()))
        })
    }) else {
        return full;
    };
    let elapsed = now.saturating_duration_since(last.at);
    let Some(fingerprint) = fingerprint else {
        let read = last.in_flight(now)
            || !last.remarks_complete
            || last.snapshot != group.snapshot
            || elapsed >= QUIET_REREAD;
        return Plan {
            detail: read,
            activity: read,
            reread_tails: false,
        };
    };
    let reread_tails = elapsed >= FINGERPRINT_REREAD;
    let activity = last.remarks.as_ref() != Some(&fingerprint.remarks)
        || !last.remarks_complete
        || reread_tails;
    Plan {
        detail: activity
            || last.status.as_ref() != Some(&fingerprint.status)
            || last.in_flight(now),
        activity,
        reread_tails,
    }
}

fn refused(message: &str) -> ProtocolError {
    ProtocolError {
        code: "pull_request_watch_refused".into(),
        message: message.into(),
    }
}

impl AppState {
    pub(super) fn start_pull_request_watch_worker(&mut self, cx: &mut HostCx) {
        let host = cx.clone();
        self.pull_request_watches.worker = Some(cx.spawn_background(async move {
            loop {
                let Ok(pass) = host
                    .enqueue_and_wait(|state, cx| state.sweep_pull_request_watches(cx))
                    .await
                else {
                    break;
                };
                pass.await;
                smol::Timer::after(PASS_SPACING).await;
            }
        }));
    }

    pub(super) fn stop_pull_request_watch_worker(&mut self) {
        self.pull_request_watches.worker = None;
    }

    /// One pass over every watched pull request: a batched fingerprint read, then the detail and
    /// activity reads it calls for, shared across projects only for identical inputs.
    pub(super) fn sweep_pull_request_watches(&mut self, cx: &mut HostCx) -> HostTask<()> {
        if std::mem::replace(&mut self.pull_request_watches.passing, true) {
            return cx.spawn_background(async {});
        }
        let mut groups: Vec<Group> = Vec::new();
        // What is kept for a pull request lasts while any thread watches it, read or not.
        let mut present = HashSet::new();
        let mut ending = Vec::new();
        let mut undelivered = Vec::new();
        for meta in &self.sessions {
            if meta.archived_at.is_some() || meta.pull_request_watches_paused {
                continue;
            }
            for link in pull_request::watched(&meta.pull_requests) {
                let watch = link.watch.as_ref().unwrap();
                let key = (meta.project_id.clone(), link.key.clone());
                present.insert(key.clone());
                if let Some(wake) = &watch.pending_wake
                    && (wake.last
                        || !self
                            .pull_request_watches
                            .refresh
                            .contains(&(meta.id.clone(), link.key.clone())))
                {
                    if wake.last {
                        self.pull_request_watches
                            .refresh
                            .remove(&(meta.id.clone(), link.key.clone()));
                    }
                    undelivered.push((meta.id.clone(), link.key.clone(), wake.clone()));
                    continue;
                }
                // Settled threads and subagents hold no watch. A merged pull request cannot
                // reopen, so its watch ends with the merge as its last wake.
                if meta.is_settled() || meta.parent_session_id.is_some() {
                    ending.push((
                        meta.id.clone(),
                        link.key.clone(),
                        watch.started_at.clone(),
                        None,
                    ));
                    continue;
                }
                if link
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.state == PullRequestState::Merged)
                {
                    let last = (
                        watch::merged_message(link.key.number, &link.url),
                        WatchNotice::Merged,
                    );
                    ending.push((
                        meta.id.clone(),
                        link.key.clone(),
                        watch.started_at.clone(),
                        Some(last),
                    ));
                    continue;
                }
                let target = Target {
                    thread: meta.id.clone(),
                    started_at: watch.started_at.clone(),
                    url: link.url.clone(),
                };
                match groups.iter_mut().find(|group| group.key == key) {
                    Some(group) => group.targets.push(target),
                    None => groups.push(Group {
                        key,
                        targets: vec![target],
                        snapshot: String::new(),
                    }),
                }
            }
        }
        for group in &mut groups {
            let mut snapshots: Vec<_> = group
                .targets
                .iter()
                .filter_map(|target| self.find_meta(&target.thread))
                .flat_map(|meta| meta.pull_requests)
                .filter(|link| link.key == group.key.1 && link.visible())
                .map(|link| snapshot_fingerprint(&link))
                .collect();
            snapshots.sort();
            snapshots.dedup();
            group.snapshot = snapshots.join("\n");
        }
        let watches = &mut self.pull_request_watches;
        watches.last_reads.retain(|key, _| present.contains(key));
        watches.failures.retain(|key, _| present.contains(key));
        watches.tails.retain(|key, _| present.contains(key));
        for (thread, key, started_at, last) in ending {
            self.record_pull_request_watch(&thread, &key, &started_at, None, last, cx);
        }
        for (thread, key, wake) in undelivered {
            self.deliver_pull_request_wake(&thread, &key, &wake, cx);
        }
        let forge = self.pull_request_watches.forge.clone();
        let host = cx.clone();
        cx.spawn_background(async move {
            let keys: Vec<_> = groups.iter().map(|group| group.key.1.clone()).collect();
            let mut fingerprints = {
                let forge = forge.clone();
                host.unblock(move || forge.fingerprints(&keys)).await
            };
            if fingerprints.len() != groups.len() {
                let error = ForgeError {
                    kind: ForgeErrorKind::Uncertain,
                    description: format!(
                        "{} fingerprints answered for {} pull requests",
                        fingerprints.len(),
                        groups.len()
                    ),
                };
                fingerprints = groups.iter().map(|_| Err(error.clone())).collect();
            }
            let mut fingerprinted = Vec::new();
            for (group, fingerprint) in groups.into_iter().zip(fingerprints) {
                match fingerprint {
                    Ok(fingerprint) => fingerprinted.push((group, fingerprint)),
                    // The host asks us to wait: skip this pull request, which is no failure.
                    Err(error) if rate_limited(&error) => {}
                    // The snapshot-gated reads report a host that cannot be read.
                    Err(error) => {
                        log::debug!("pull request watch fingerprint failed: {error}");
                        fingerprinted.push((group, None));
                    }
                }
            }
            let Ok(planned) = host
                .enqueue_and_wait(move |state, _| {
                    let watches = &mut state.pull_request_watches;
                    let now = Instant::now();
                    fingerprinted
                        .into_iter()
                        .filter_map(|(group, fingerprint)| {
                            let mut refresh = false;
                            for target in &group.targets {
                                refresh |= watches
                                    .refresh
                                    .remove(&(target.thread.clone(), group.key.1.clone()));
                            }
                            let plan = plan(
                                &group,
                                watches.last_reads.get(&group.key).filter(|_| !refresh),
                                fingerprint.as_ref(),
                                now,
                            );
                            let tails = plan.activity.then(|| {
                                watches
                                    .tails
                                    .remove(&group.key)
                                    .filter(|_| !plan.reread_tails && !refresh)
                                    .unwrap_or_default()
                            });
                            plan.detail.then_some((group, fingerprint, plan, tails))
                        })
                        .collect::<Vec<_>>()
                })
                .await
            else {
                return;
            };
            // Sharing lasts one pass; each project still applies its own watch generation
            // and watermarks. Activity is reusable only with identical input tails.
            let mut details: HashMap<_, Option<Result<PullRequestWatchRead, ForgeError>>> =
                HashMap::new();
            let mut activities: HashMap<_, Vec<(Tails, Option<ActivityRead>)>> = HashMap::new();
            let mut planned = planned.into_iter().peekable();
            while planned.peek().is_some() {
                let reading: Vec<_> = planned
                    .by_ref()
                    .take(GROUP_CONCURRENCY)
                    .map(|(group, fingerprint, plan, tails)| {
                        let key = group.key.1.clone();
                        let detail = match details.entry(key.clone()) {
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                entry.insert(None);
                                let forge = forge.clone();
                                let key = key.clone();
                                Some(host.unblock(move || forge.watch_detail(&key)))
                            }
                            std::collections::hash_map::Entry::Occupied(_) => None,
                        };
                        let activity = tails.map(|mut tails| {
                            let reads = activities.entry(key.clone()).or_default();
                            if let Some(index) = reads.iter().position(|(input, _)| input == &tails)
                            {
                                return (index, None);
                            }
                            let index = reads.len();
                            reads.push((tails.clone(), None));
                            let forge = forge.clone();
                            let task = host.unblock(move || {
                                let read = forge.activity(&key, &mut tails);
                                (read, tails)
                            });
                            (index, Some(task))
                        });
                        (group, fingerprint, plan, detail, activity)
                    })
                    .collect();
                for (group, fingerprint, plan, detail, activity) in reading {
                    let key = &group.key.1;
                    if let Some(task) = detail {
                        details.insert(key.clone(), Some(task.await));
                    }
                    let detail = details[key].as_ref().unwrap().clone();
                    let (activity, tails) = match activity {
                        Some((index, task)) => {
                            let cached = &mut activities.get_mut(key).unwrap()[index].1;
                            if let Some(task) = task {
                                *cached = Some(task.await);
                            }
                            let (read, tails) = cached.as_ref().unwrap().clone();
                            (Some(read), Some(tails))
                        }
                        None => (None, None),
                    };
                    let read: GroupRead = match (detail, activity) {
                        (Err(error), _) | (_, Some(Err(error))) => Err(error),
                        (Ok(detail), activity) => Ok((detail, activity.map(Result::unwrap))),
                    };
                    let _ = host
                        .enqueue_and_wait(move |state, cx| {
                            if let Some(tails) = tails {
                                state
                                    .pull_request_watches
                                    .tails
                                    .insert(group.key.clone(), tails);
                            }
                            state.finish_pull_request_watch_read(
                                group,
                                fingerprint,
                                plan,
                                read,
                                cx,
                            );
                        })
                        .await;
                }
            }
            let _ = host
                .enqueue_and_wait(|state, cx| {
                    state.pull_request_watches.passing = false;
                    state.refresh_host_credentials(cx);
                    if std::mem::take(&mut state.pull_request_watches.sweep_pending) {
                        state.sweep_pull_request_watches(cx).detach();
                    }
                })
                .await;
        })
    }

    fn finish_pull_request_watch_read(
        &mut self,
        group: Group,
        fingerprint: Option<Fingerprint>,
        plan: Plan,
        read: GroupRead,
        cx: &mut HostCx,
    ) {
        let Group {
            key,
            targets,
            snapshot,
        } = group;
        let (detail, activity) = match read {
            Ok(read) => read,
            Err(error) => {
                self.pull_request_watches.last_reads.remove(&key);
                for target in &targets {
                    if let Some(wake) = self.find_meta(&target.thread).and_then(|meta| {
                        meta.pull_requests
                            .into_iter()
                            .find(|link| link.key == key.1)
                            .and_then(|link| link.watch)
                            .and_then(|watch| watch.pending_wake)
                    }) {
                        self.deliver_pull_request_wake(&target.thread, &key.1, &wake, cx);
                    }
                }
                // The host's pause refuses later reads without a request, so waiting is free.
                if rate_limited(&error) {
                    return;
                }
                log::warn!("pull request watch read failed: {error}");
                let failures = self
                    .pull_request_watches
                    .failures
                    .entry(key.clone())
                    .or_default();
                *failures += 1;
                if *failures < watch::READ_FAILURE_LIMIT {
                    return;
                }
                // The count stays until every stop lands, so a failed stop is tried again.
                let mut landed = true;
                for target in &targets {
                    let text = watch::unreadable_message(key.1.number, &target.url);
                    landed &= self.record_pull_request_watch(
                        &target.thread,
                        &key.1,
                        &target.started_at,
                        None,
                        Some((text, WatchNotice::Unreadable)),
                        cx,
                    );
                }
                if landed {
                    self.pull_request_watches.failures.remove(&key);
                }
                return;
            }
        };
        self.pull_request_watches.failures.remove(&key);
        if detail.state != PullRequestState::Open {
            self.pull_request_watches.last_reads.remove(&key);
            for target in &targets {
                let last = if detail.state == PullRequestState::Closed {
                    (
                        watch::closed_message(key.1.number, &target.url),
                        WatchNotice::Closed,
                    )
                } else {
                    (
                        watch::merged_message(key.1.number, &target.url),
                        WatchNotice::Merged,
                    )
                };
                self.record_pull_request_watch(
                    &target.thread,
                    &key.1,
                    &target.started_at,
                    None,
                    Some(last),
                    cx,
                );
            }
            return;
        }
        // An incomplete read never advances the watermark; a pass without the activity has none.
        let remarks = activity.flatten();
        let previous = self.pull_request_watches.last_reads.get(&key);
        let now = Instant::now();
        let read = LastRead {
            at: if plan.activity {
                now
            } else {
                previous.map_or(now, |last| last.at)
            },
            snapshot,
            checks_running: detail
                .checks
                .iter()
                .any(|check| check.status == watch::CheckStatus::Pending),
            pending: (detail.mergeability == pull_request::Mergeability::Pending).then(|| {
                let since = previous
                    .and_then(|last| last.pending.as_ref())
                    .filter(|(head, _)| *head == detail.head_sha)
                    .map_or(now, |(_, since)| *since);
                (detail.head_sha.clone(), since)
            }),
            remarks_complete: if plan.activity {
                remarks.is_some()
            } else {
                previous.is_some_and(|last| last.remarks_complete)
            },
            watches: targets
                .iter()
                .map(|target| (target.thread.clone(), target.started_at.clone()))
                .collect(),
            status: fingerprint.as_ref().map(|print| print.status.clone()),
            remarks: fingerprint.map(|print| print.remarks),
        };
        self.pull_request_watches
            .last_reads
            .insert(key.clone(), read);
        for target in targets {
            let Some(current) = self.find_meta(&target.thread).and_then(|meta| {
                meta.pull_requests
                    .into_iter()
                    .find(|link| link.key == key.1 && link.visible())
                    .and_then(|link| link.watch)
            }) else {
                continue;
            };
            let report = watch::evaluate(&current, &detail, remarks.as_deref());
            if report.changes.is_empty() {
                if report.next != current {
                    self.record_pull_request_watch(
                        &target.thread,
                        &key.1,
                        &target.started_at,
                        Some(report.next),
                        None,
                        cx,
                    );
                }
                if let Some(wake) = &current.pending_wake {
                    self.deliver_pull_request_wake(&target.thread, &key.1, wake, cx);
                }
                continue;
            }
            let text =
                watch::update_message(key.1.number, &target.url, &detail.base_branch, &report);
            let notice = WatchNotice::Update {
                kinds: report
                    .changes
                    .iter()
                    .map(watch::WatchChange::kind)
                    .collect(),
                stopped: report.exhausted,
            };
            let next = (!report.exhausted).then_some(report.next);
            self.record_pull_request_watch(
                &target.thread,
                &key.1,
                &target.started_at,
                next,
                Some((text, notice)),
                cx,
            );
        }
    }

    /// Only the same subscription generation may consume a read. Persist news with its
    /// watermark; a resumed read combines it with any older undelivered news.
    fn record_pull_request_watch(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        started_at: &str,
        next: Option<PullRequestWatch>,
        wake: Option<(String, WatchNotice)>,
        cx: &mut HostCx,
    ) -> bool {
        let Some(mut meta) = self
            .find_meta(thread)
            .filter(|meta| meta.archived_at.is_none() && !meta.pull_request_watches_paused)
        else {
            return false;
        };
        let Some(link) = meta
            .pull_requests
            .iter_mut()
            .find(|link| &link.key == key && link.visible())
        else {
            return false;
        };
        let Some(current) = link
            .watch
            .as_ref()
            .filter(|current| current.started_at == started_at)
        else {
            return false;
        };
        let number = key.number;
        let delivered = match wake {
            None => {
                if next.is_none() {
                    log::info!("pull request watch ended: {thread} #{number}");
                }
                link.watch = next;
                None
            }
            Some((text, notice)) => {
                let last = next.is_none();
                let mut recorded = next.unwrap_or_else(|| current.clone());
                let pending = PendingWake {
                    id: current.pending_wake.as_ref().map_or_else(
                        || uuid::Uuid::new_v4().to_string(),
                        |pending| pending.id.clone(),
                    ),
                    text: current.pending_wake.as_ref().map_or_else(
                        || text.clone(),
                        |pending| format!("{}\n\n{text}", pending.text),
                    ),
                    last,
                };
                recorded.pending_wake = Some(pending.clone());
                link.watch = Some(recorded);
                if last {
                    log::info!("pull request watch ending: {thread} #{number} ({notice:?})");
                }
                Some((pending, notice))
            }
        };
        self.save_pull_request_meta(meta, cx);
        if let Some((pending, notice)) = delivered {
            self.fire_control_watches(thread, WatchEvent::PrChanged, cx);
            self.deliver_pull_request_wake(thread, key, &pending, cx);
            emit_runtime(
                cx,
                RuntimeEvent::Toast(RuntimeToast::PullRequestWatch {
                    session_id: thread.to_owned(),
                    number,
                    notice,
                }),
            );
        }
        true
    }

    /// Deliver a persisted wake: into the running turn when the provider can steer, so news
    /// about the pull request reaches the agent while it still works on it, otherwise queued
    /// behind that turn. A wake already waiting in the queue is not delivered twice.
    fn deliver_pull_request_wake(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        wake: &PendingWake,
        cx: &mut HostCx,
    ) {
        if self
            .find_meta(thread)
            .is_none_or(|meta| meta.pull_request_watches_paused)
        {
            return;
        }
        let kind = QueuedMessageKind::PullRequestWake {
            key: key.clone(),
            id: wake.id.clone(),
        };
        if self
            .resident(thread)
            .is_some_and(|session| session.queue.iter().any(|message| message.kind == kind))
        {
            return;
        }
        let can_steer = self
            .resident(thread)
            .is_some_and(|session| session.turn_in_flight && session.can_steer());
        if can_steer {
            let mut steer_cx = cx.clone();
            steer_cx.origin = Some(MessageOrigin::Server);
            steer_cx.author = None;
            steer_cx.delivery_key = Some(wake.id.clone());
            let request_id = self.record_steer_request(thread, &wake.text, &[], &mut steer_cx);
            let sent = self.resident_mut(thread).is_some_and(|session| {
                session
                    .steer_now(request_id, wake.text.clone(), Vec::new())
                    .is_ok()
            });
            if sent {
                self.acknowledge_pull_request_wake(thread, key, &wake.id, &wake.text, cx);
                return;
            }
            // Provider channel gone: the wake survives in the queue for the next turn.
        }
        let text = wake.text.clone();
        self.queue_automatic_turn(
            thread,
            move |session| {
                session.push_server_message(text, kind);
            },
            cx,
        );
    }

    /// The provider took a wake, steered or as a turn: it is delivered, and an ending watch
    /// ends here.
    pub(super) fn acknowledge_pull_request_wake(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        id: &str,
        text: &str,
        cx: &mut HostCx,
    ) {
        let Some(mut meta) = self.find_meta(thread) else {
            return;
        };
        let Some(link) = meta.pull_requests.iter_mut().find(|link| {
            &link.key == key
                && link
                    .watch
                    .as_ref()
                    .and_then(|watch| watch.pending_wake.as_ref())
                    .is_some_and(|wake| wake.id == id)
        }) else {
            return;
        };
        let watch = link.watch.as_mut().unwrap();
        let pending = watch.pending_wake.as_mut().unwrap();
        let remainder = if pending.text == text {
            let last = pending.last;
            watch.pending_wake = None;
            if last {
                link.watch = None;
            }
            None
        } else if let Some(rest) = pending
            .text
            .strip_prefix(text)
            .and_then(|rest| rest.strip_prefix("\n\n"))
        {
            // Catch-up can append news while the provider is accepting an older delivery.
            pending.text = rest.to_owned();
            pending.id = uuid::Uuid::new_v4().to_string();
            Some(pending.clone())
        } else {
            return;
        };
        self.save_pull_request_meta(meta, cx);
        if let Some(pending) = remainder {
            self.deliver_pull_request_wake(thread, key, &pending, cx);
        }
    }

    pub(super) fn resume_pull_request_watches(&mut self, thread: &str, cx: &mut HostCx) {
        let Some(mut meta) = self
            .find_meta(thread)
            .filter(|meta| meta.pull_request_watches_paused)
        else {
            return;
        };
        meta.pull_request_watches_paused = false;
        for link in pull_request::watched(&meta.pull_requests) {
            self.pull_request_watches
                .refresh
                .insert((thread.to_owned(), link.key.clone()));
        }
        self.save_pull_request_meta(meta, cx);
        if self.pull_request_watches.passing {
            self.pull_request_watches.sweep_pending = true;
        } else {
            self.sweep_pull_request_watches(cx).detach();
        }
    }

    /// Drop the thread's queued wakes, all of them or those of one pull request. A wake the
    /// provider is already accepting is not taken back.
    pub(super) fn discard_pull_request_wakes(
        &mut self,
        thread: &str,
        key: Option<&PullRequestKey>,
    ) {
        if let Some(session) = self.resident_mut(thread) {
            let in_flight = session.delivery_in_flight;
            session.queue.retain(|message| {
                Some(message.id) == in_flight
                    || !matches!(
                        &message.kind,
                        QueuedMessageKind::PullRequestWake { key: wake, .. }
                            if key.is_none_or(|key| key == wake)
                    )
            });
        }
    }

    pub(crate) fn set_pull_request_watches_paused(
        &mut self,
        thread: &str,
        paused: bool,
        cx: &mut HostCx,
    ) {
        if paused {
            self.pause_pull_request_watches(thread, cx);
        } else {
            self.resume_pull_request_watches(thread, cx);
        }
    }

    pub(super) fn pause_pull_request_watches(&mut self, thread: &str, cx: &mut HostCx) {
        self.discard_pull_request_wakes(thread, None);
        if let Some(mut meta) = self.find_meta(thread) {
            meta.pull_request_watches_paused = true;
            self.save_pull_request_meta(meta, cx);
        }
    }

    /// Settling or archiving a thread ends its watches in the same write.
    pub(super) fn end_pull_request_watches(&mut self, meta: &mut SessionMeta) {
        meta.pull_request_watches_paused = false;
        self.pull_request_watches
            .refresh
            .retain(|(thread, _)| thread != &meta.id);
        if clear_watches(meta) {
            self.discard_pull_request_wakes(&meta.id, None);
        }
    }

    /// An agent start may link the pull request in the same write.
    fn set_pull_request_watch(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        url: Option<String>,
        watching: bool,
        cx: &mut HostCx,
    ) -> Result<(bool, bool), ProtocolError> {
        let mut meta = self
            .find_meta(thread)
            .ok_or_else(|| refused("Unknown thread."))?;
        let was_watching = meta
            .pull_requests
            .iter()
            .any(|link| &link.key == key && link.visible() && link.watch.is_some());
        if !watching {
            let Some(link) = meta
                .pull_requests
                .iter_mut()
                .find(|link| &link.key == key && link.watch.is_some())
            else {
                return Ok((false, false));
            };
            link.watch = None;
            self.pull_request_watches
                .refresh
                .remove(&(thread.to_owned(), key.clone()));
            self.discard_pull_request_wakes(thread, Some(key));
            self.save_pull_request_meta(meta, cx);
            return Ok((false, was_watching));
        }
        if meta.parent_session_id.is_some() {
            return Err(refused(
                "This thread is a subagent, so it cannot watch pull requests. Its parent thread owns the pull request: finish your task and report back instead.",
            ));
        }
        if meta.archived_at.is_some() {
            return Err(refused(
                "This thread is archived, so it cannot watch pull requests.",
            ));
        }
        if meta.is_settled() {
            return Err(refused(
                "This thread is settled. Unsettle it before starting a new watch.",
            ));
        }
        let existing = meta
            .pull_requests
            .iter()
            .find(|link| &link.key == key && link.visible());
        // A merged pull request cannot reopen. A closed one can, so its first read decides.
        if existing
            .and_then(|link| link.snapshot.as_ref())
            .is_some_and(|snapshot| snapshot.state == PullRequestState::Merged)
        {
            return Err(refused(
                "The pull request is merged, so there is nothing to watch.",
            ));
        }
        if existing.is_some_and(|link| link.watch.as_ref().is_some_and(PullRequestWatch::active)) {
            return Ok((true, true));
        }
        let linked = match (existing.is_some(), url) {
            (true, _) => false,
            (false, Some(url)) => pull_request::link_pull_request(
                &mut meta.pull_requests,
                key.clone(),
                url,
                PullRequestSource::Agent,
                now_secs(),
                true,
            ),
            (false, None) => return Err(refused("This pull request is not linked.")),
        };
        if let Some(link) = meta
            .pull_requests
            .iter_mut()
            .find(|link| &link.key == key && link.visible())
        {
            link.watch = Some(PullRequestWatch::new(now_millis()));
        }
        self.save_pull_request_meta(meta, cx);
        if linked {
            self.request_pull_request_sync(key.clone(), cx);
        }
        Ok((true, false))
    }

    /// The user's Watch for changes / Stop watching.
    pub fn watch_pull_request(
        &mut self,
        thread: &str,
        key: &PullRequestKey,
        watching: bool,
        cx: &mut HostCx,
    ) -> Result<CommandResponse, ProtocolError> {
        self.set_pull_request_watch(thread, key, None, watching, cx)
            .map(|_| CommandResponse::Unit)
    }

    /// `watch_pull_request` and `unwatch_pull_request` from the thread's agent.
    pub(super) fn watch_pull_request_from_agent(
        &mut self,
        thread: &str,
        key: PullRequestKey,
        url: String,
        watching: bool,
        cx: &mut HostCx,
    ) -> Result<serde_json::Value, String> {
        let (now, before) = self
            .set_pull_request_watch(thread, &key, Some(url.clone()), watching, cx)
            .map_err(|error| error.message)?;
        Ok(serde_json::json!({
            "host": key.host,
            "repository": key.repository,
            "number": key.number,
            "url": url,
            "watching": now,
            "wasWatching": before,
            "paused": self.find_meta(thread).is_some_and(|meta| meta.pull_request_watches_paused),
        }))
    }
}

fn clear_watches(meta: &mut SessionMeta) -> bool {
    let mut cleared = false;
    for link in &mut meta.pull_requests {
        cleared |= link.watch.take().is_some();
    }
    if cleared {
        log::info!("pull request watches ended: {}", meta.id);
    }
    cleared
}

#[cfg(test)]
#[path = "pull_request_watch_tests.rs"]
mod tests;
