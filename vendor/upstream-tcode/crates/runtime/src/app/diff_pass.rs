use std::collections::VecDeque;

use tcode_services::store::DiffPass;

use super::*;

/// The background pass that drops the diffs of superseded turn-changes
/// snapshots. Each thread is one store-writer operation, queued only once the one before it
/// finished, so the pass never holds more than one log in memory and never
/// delays the writes queued behind it by more than one thread.
pub(super) struct DiffPassRun {
    remaining: VecDeque<String>,
    started: Instant,
    dropped: usize,
    unchanged: usize,
    undecodable: usize,
    failed: usize,
    rows: usize,
    bytes_before: u64,
    bytes_after: u64,
    /// The longest any one thread held the store writer.
    longest: Duration,
}

impl AppState {
    /// List the threads the pass has not dealt with, off the mailbox, then
    /// pass them one at a time. A resident thread is passed like any other:
    /// its log already holds the snapshots without their diffs.
    pub(crate) fn start_diff_pass(&mut self, cx: &mut HostCx) {
        let store = self.store.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let threads = host_cx
                .unblock(move || store.threads_without_diff_pass())
                .await;
            host_cx.enqueue(move |state, cx| match threads {
                Ok(threads) => {
                    if threads.is_empty() {
                        log::info!("superseded diff cleanup complete: no work");
                        return;
                    }
                    state.diff_pass = Some(DiffPassRun {
                        remaining: threads.into(),
                        started: Instant::now(),
                        dropped: 0,
                        unchanged: 0,
                        undecodable: 0,
                        failed: 0,
                        rows: 0,
                        bytes_before: 0,
                        bytes_after: 0,
                        longest: Duration::ZERO,
                    });
                    state.continue_diff_pass(cx);
                }
                Err(error) => log::warn!("could not list the threads to pass: {error}"),
            });
        });
    }

    pub(super) fn stop_diff_pass(&mut self) {
        if let Some(run) = self.diff_pass.take() {
            log::info!(
                "superseded diff cleanup interrupted: {} changed, {} unchanged, {} undecodable, {} failed; incomplete",
                run.dropped,
                run.unchanged,
                run.undecodable,
                run.failed,
            );
        }
    }

    fn continue_diff_pass(&mut self, cx: &mut HostCx) {
        let Some(run) = self.diff_pass.as_mut() else {
            return;
        };
        let Some(session_id) = run.remaining.pop_front() else {
            let run = self.diff_pass.take().expect("checked above");
            let store = self.store.clone();
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let complete = match host_cx
                    .unblock(move || store.threads_without_diff_pass())
                    .await
                {
                    Ok(remaining) => remaining.is_empty(),
                    Err(error) => {
                        log::warn!("could not check diff cleanup completion: {error}");
                        false
                    }
                };
                log::info!(
                    "superseded diff cleanup {} in {:.1?}: {} thread(s) changed ({} rows, {} -> {} bytes), \
                 {} unchanged, {} left with an undecodable row, {} failed; the writer was held \
                 at most {:.1?}",
                    if complete { "complete" } else { "incomplete" },
                    run.started.elapsed(),
                    run.dropped,
                    run.rows,
                    run.bytes_before,
                    run.bytes_after,
                    run.unchanged,
                    run.undecodable,
                    run.failed,
                    run.longest,
                );
            });
            return;
        };
        let (completion, completed) = smol::channel::bounded(1);
        self.enqueue_store_write(
            StoreWrite::DropSupersededDiffs {
                id: session_id.clone(),
                completion,
            },
            cx,
        );
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let outcome = completed.recv().await;
            host_cx.enqueue(move |state, cx| {
                let Some(run) = state.diff_pass.as_mut() else {
                    return;
                };
                match outcome {
                    Ok(Ok((outcome, held))) => {
                        run.longest = run.longest.max(held);
                        match outcome {
                            DiffPass::Dropped {
                                rows,
                                before,
                                after,
                            } => {
                                run.dropped += 1;
                                run.rows += rows;
                                run.bytes_before += before;
                                run.bytes_after += after;
                            }
                            DiffPass::Unchanged => run.unchanged += 1,
                            DiffPass::Undecodable { position } => {
                                run.undecodable += 1;
                                log::warn!(
                                    "kept the superseded diffs of {session_id}: event {position} \
                                     does not decode"
                                );
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        run.failed += 1;
                        log::warn!("could not drop the superseded diffs of {session_id}: {error}");
                    }
                    Err(_) => {
                        state.stop_diff_pass();
                        return;
                    }
                }
                state.continue_diff_pass(cx);
            });
        });
    }
}
