use super::*;
use agent::DeltaKind;
use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;
use tcode_core::session::{TurnSnapshots, drop_turn_diffs};
use tcode_protocol::{
    CommandResponse, HostMessage, MAX_SESSION_HISTORY_BYTES, OUTPUT_PREVIEW_BYTES,
    SESSION_HISTORY_RECORDS, SESSION_WINDOW_BYTES, Subscription,
};
use tcode_services::store::{EventLog, LogRead, TurnIndex};

/// Count serialized bytes without allocating a copy of a large record.
struct ByteCount(usize);

impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn wire_len(record: &SessionEventRecord) -> usize {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, record).expect("serializable record");
    count.0
}

fn output_tail(text: &str) -> &str {
    let mut start = text.len().saturating_sub(OUTPUT_PREVIEW_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn oversized_output(event: &AgentEvent) -> bool {
    match event {
        AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) => match &item.content {
            ItemContent::ToolCall {
                output,
                image_reads,
                ..
            } => {
                image_reads
                    .iter()
                    .any(|image| !image.data_base64.is_empty())
                    || output
                        .as_ref()
                        .is_some_and(|output| output.len() > OUTPUT_PREVIEW_BYTES)
            }
            ItemContent::ImageRead {
                image: Some(image), ..
            } => !image.data_base64.is_empty(),
            ItemContent::CommandExecution { output, .. } => output.len() > OUTPUT_PREVIEW_BYTES,
            _ => false,
        },
        AgentEvent::Delta {
            kind: DeltaKind::CommandOutput,
            text,
            ..
        } => text.len() > OUTPUT_PREVIEW_BYTES,
        _ => false,
    }
}

const HISTORY_PREVIEW_POLICY: u32 = 1;
static HISTORY_PREVIEW_VERSION: LazyLock<String> =
    LazyLock::new(|| format!("history-preview-{HISTORY_PREVIEW_POLICY}-{OUTPUT_PREVIEW_BYTES}"));
const HISTORY_PREVIEW_CACHE_BYTES: usize = 16 * 1024;

/// A record as clients receive it: tool and command output beyond
/// [`OUTPUT_PREVIEW_BYTES`] stays on the host, read back with
/// `Query::ReadItemOutput`.
pub(super) fn wire_record(record: &SessionEventRecord) -> Cow<'_, SessionEventRecord> {
    let Some((event, elided)) = wire_event(&record.event) else {
        return Cow::Borrowed(record);
    };
    Cow::Owned(SessionEventRecord {
        origin: record.origin,
        author: record.author.clone(),
        ts: record.ts,
        event,
        elided,
    })
}

fn wire_event(event: &AgentEvent) -> Option<(AgentEvent, Option<u64>)> {
    if !oversized_output(event) {
        return None;
    }
    if let AgentEvent::Delta {
        item_id,
        kind: DeltaKind::CommandOutput,
        text,
    } = event
    {
        return Some((
            AgentEvent::Delta {
                item_id: item_id.clone(),
                kind: DeltaKind::CommandOutput,
                text: output_tail(text).to_owned(),
            },
            Some(text.len() as u64),
        ));
    }
    let item = match event {
        AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) => item,
        _ => return None,
    };
    let (content, elided) = match &item.content {
        ItemContent::CommandExecution {
            command,
            output,
            exit_code,
            status,
        } => (
            ItemContent::CommandExecution {
                command: command.clone(),
                output: output_tail(output).to_owned(),
                exit_code: *exit_code,
                status: *status,
            },
            Some(output.len() as u64),
        ),
        ItemContent::ToolCall {
            presentation,
            image_reads,
            name,
            input,
            output,
            status,
        } => {
            let full = output.as_ref().map(|output| output.len() as u64);
            let mut preview = output.as_deref().map(Cow::Borrowed);
            let mut removed = false;
            for image in image_reads {
                if !image.data_base64.is_empty()
                    && let Some(text) = &mut preview
                {
                    let mut matches = text.match_indices(image.data_base64.as_str());
                    if let Some((first, matched)) = matches.next() {
                        let mut replaced = String::with_capacity(text.len() - matched.len() + 7);
                        replaced.push_str(&text[..first]);
                        replaced.push_str("[image]");
                        let mut end = first + matched.len();
                        for (start, matched) in matches {
                            replaced.push_str(&text[end..start]);
                            replaced.push_str("[image]");
                            end = start + matched.len();
                        }
                        replaced.push_str(&text[end..]);
                        removed |= replaced.len() != text.len();
                        *text = Cow::Owned(replaced);
                    }
                }
            }
            let mut elided = if removed { full } else { None };
            let output = preview.map(|text| {
                if text.len() > OUTPUT_PREVIEW_BYTES {
                    elided = full;
                    text[..text.floor_char_boundary(OUTPUT_PREVIEW_BYTES)].to_owned()
                } else {
                    text.into_owned()
                }
            });
            (
                ItemContent::ToolCall {
                    presentation: presentation.clone(),
                    image_reads: image_reads.iter().map(wire_image).collect(),
                    name: name.clone(),
                    input: input.clone(),
                    output,
                    status: *status,
                },
                elided,
            )
        }
        ItemContent::ImageRead {
            path,
            image: Some(image),
        } => (
            ItemContent::ImageRead {
                path: path.clone(),
                image: Some(wire_image(image)),
            },
            None,
        ),
        _ => return None,
    };
    let preview = agent::ThreadItem {
        id: item.id.clone(),
        parent_item_id: item.parent_item_id.clone(),
        content,
    };
    let event = match event {
        AgentEvent::ItemStarted(_) => AgentEvent::ItemStarted(preview),
        AgentEvent::ItemUpdated(_) => AgentEvent::ItemUpdated(preview),
        _ => AgentEvent::ItemCompleted(preview),
    };
    Some((event, elided))
}

pub(super) fn append_projection(
    bytes: usize,
    ts: u64,
    event: &AgentEvent,
    author: Option<&Author>,
    origin: Option<MessageOrigin>,
) -> Option<(String, Vec<u8>)> {
    if bytes < HISTORY_PREVIEW_CACHE_BYTES || !oversized_output(event) {
        return None;
    }
    let record = SessionEventRecord {
        origin,
        author: author.cloned(),
        ts: Some(ts),
        event: event.clone(),
        elided: None,
    };
    let Cow::Owned(preview) = wire_record(&record) else {
        return None;
    };
    match serde_json::to_vec(&preview) {
        Ok(body) if body.len() < HISTORY_PREVIEW_CACHE_BYTES => {
            Some((HISTORY_PREVIEW_VERSION.to_string(), body))
        }
        Ok(_) => None,
        Err(error) => {
            log::warn!("could not encode appended history preview: {error}");
            None
        }
    }
}

fn wire_image(image: &agent::Attachment) -> agent::Attachment {
    agent::Attachment {
        media_type: image.media_type.clone(),
        data_base64: String::new(),
        source_path: image.source_path.clone(),
    }
}

/// The records that stand for the log cursors in `range`.
struct Window {
    range: Range<usize>,
    records: Vec<SessionEventRecord>,
}

/// Choose the contiguous cursors a reply covers and the records it sends for
/// them. Starting from the `backwards` end of `requested`, records are taken
/// while the reply fits [`SESSION_WINDOW_BYTES`], and always at least one.
/// Consecutive deltas of one item are merged into one, which folds the same
/// because nothing between them moves the turn.
fn wire_window(
    records: &[SessionEventRecord],
    requested: Range<usize>,
    backwards: bool,
    overhead: usize,
) -> Result<Window, tcode_protocol::ProtocolError> {
    let budget = SESSION_WINDOW_BYTES.saturating_sub(overhead);
    let mut used = 0;
    let mut count = 0;
    for offset in 0..requested.len() {
        let index = if backwards {
            requested.end - 1 - offset
        } else {
            requested.start + offset
        };
        // Separator; also leaves room for an empty array.
        let size = wire_len(&wire_record(&records[index])) + 1;
        if count == 0 && size > MAX_SESSION_HISTORY_BYTES.saturating_sub(overhead) {
            return Err(tcode_protocol::ProtocolError {
                code: "history_record_too_large".into(),
                message: "A history record exceeds the 8 MiB response limit.".into(),
            });
        }
        if count > 0 && used + size > budget {
            break;
        }
        used += size;
        count += 1;
    }
    let range = if backwards {
        requested.end - count..requested.end
    } else {
        requested.start..requested.start + count
    };
    let mut merged: Vec<SessionEventRecord> = Vec::with_capacity(range.len());
    for index in range.clone() {
        let record = &records[index];
        if let AgentEvent::Delta {
            item_id,
            kind,
            text,
        } = &record.event
            && index > range.start
            && let Some(last) = merged.last_mut()
            && let AgentEvent::Delta {
                item_id: last_id,
                kind: last_kind,
                text: last_text,
            } = &mut last.event
            && matches!(&records[index - 1].event, AgentEvent::Delta { .. })
            && last_id == item_id
            && last_kind == kind
        {
            if *kind == DeltaKind::CommandOutput {
                let full = last.elided.unwrap_or(last_text.len() as u64)
                    + record.elided.unwrap_or(text.len() as u64);
                if record.elided.unwrap_or(text.len() as u64) >= OUTPUT_PREVIEW_BYTES as u64 {
                    last_text.clear();
                    last_text.push_str(output_tail(text));
                } else {
                    last_text.push_str(text);
                    let start = last_text.len() - output_tail(last_text).len();
                    last_text.drain(..start);
                }
                last.elided = (full > OUTPUT_PREVIEW_BYTES as u64).then_some(full);
            } else {
                last_text.push_str(text);
            }
            continue;
        }
        merged.push(wire_record(record).into_owned());
    }
    Ok(Window {
        range,
        records: merged,
    })
}

/// What a window is cut from: the records of the rows before `end` of a log,
/// all of them or only its last ones, with the turns of the whole log.
///
/// Cursors are row positions. A blank or undecodable row holds no record, so
/// a range of cursors can hold fewer records than it spans.
struct LogView<'a> {
    records: &'a [SessionEventRecord],
    /// Lower cursor covered by this read, including blank or undecodable rows.
    from: u64,
    /// The row each record is in, ascending.
    rows: &'a [u64],
    end: u64,
    covered_end: u64,
    turn_starts: &'a [u64],
    turns: u64,
}

impl LogView<'_> {
    /// Move a window start back to the row that opens its turn. The client
    /// folds only the records it holds, so a window that begins mid-turn
    /// renders a partial first turn whose entries shift as earlier pages
    /// arrive, while a window that begins where a turn begins opens its turns
    /// exactly where the full log does. Rows that open no turn leave the
    /// start unchanged.
    fn turn_aligned_start(&self, start: u64) -> u64 {
        let opened_before = self.turn_starts.partition_point(|&row| row <= start);
        opened_before
            .checked_sub(1)
            .map_or(start, |last| self.turn_starts[last])
    }

    /// The index of the first record at or after row `position`.
    fn index(&self, position: u64) -> usize {
        self.rows.partition_point(|row| *row < position)
    }

    /// The row a window whose first record is `index` starts at, when it
    /// was asked to start at `requested`, the row of record `first`.
    fn start(&self, index: usize, first: usize, requested: u64) -> u64 {
        if index == first {
            requested.max(self.from)
        } else {
            self.rows[index]
        }
    }

    /// The window answering a subscription: the records from its cursor on,
    /// or without one a baseline: the tail from a turn start about
    /// [`BASELINE_RECORDS`] rows back, cut to the byte budget from its newest
    /// end.
    fn events_window(&self, subscription: &Subscription) -> ServerEvent {
        let empty = HostMessage::Event(EventEnvelope {
            request_id: Some(u64::MAX),
            topic: subscription.topic.clone(),
            event: ServerEvent::SessionSnapshot {
                from: u64::MAX,
                end: u64::MAX,
                records: vec![],
                total: u64::MAX,
                total_turns: u64::MAX,
                truncated: false,
            },
        });
        let total = self.end;
        let after = subscription.after.filter(|after| *after <= total);
        let from = after
            .unwrap_or_else(|| self.turn_aligned_start(total.saturating_sub(BASELINE_RECORDS)));
        let first = self.index(from);
        match wire_window(
            self.records,
            first..self.records.len(),
            after.is_none(),
            wire_overhead(&empty),
        ) {
            Ok(window) => {
                let start = self.start(window.range.start, first, from);
                let end = if window.range.end == self.records.len() {
                    self.covered_end
                } else {
                    self.rows[window.range.end]
                };
                ServerEvent::SessionSnapshot {
                    from: start,
                    end,
                    truncated: start > from || end < total,
                    records: window.records,
                    total,
                    total_turns: self.turns,
                }
            }
            Err(error) => ServerEvent::SessionHistoryError(error),
        }
    }

    fn page_start(&self, end: u64, limit: u32) -> u64 {
        let count = (limit as u64).clamp(1, SESSION_HISTORY_RECORDS as u64);
        self.turn_aligned_start(end.saturating_sub(count))
    }

    fn history_page(
        &self,
        before: u64,
        limit: u32,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        let end = before.min(self.end);
        let start = self.page_start(end, limit);
        let requested = self.index(start)..self.index(end);
        let empty = HostMessage::QueryResult {
            id: u64::MAX,
            result: Ok(QueryResponse::SessionHistoryPage {
                records: vec![],
                from: u64::MAX,
                end: u64::MAX,
                truncated: false,
            }),
        };
        let window = wire_window(self.records, requested.clone(), true, wire_overhead(&empty))?;
        Ok(QueryResponse::SessionHistoryPage {
            from: self.start(window.range.start, requested.start, start),
            end,
            truncated: self.from > start || window.range.len() < requested.len(),
            records: window.records,
        })
    }
}

/// The bytes a reply spends besides its records, measured on `empty`, a reply
/// without records whose numbers are as long as any reply's.
fn wire_overhead(empty: &HostMessage) -> usize {
    serde_json::to_vec(empty).expect("serializable reply").len() + 1
}

/// How far back a baseline reaches before its turn start and byte budget.
const BASELINE_RECORDS: u64 = 400;

/// The complete event log of one session, held in memory while the session
/// is resident so history windows cost the page rather than a re-read of the
/// stored log, plus the fold that decides where each window may start.
///
/// Memory policy: [`AppState::event_records`] holds a log for every live or
/// parked session (bounded by the resident LRU) and for nothing else. A log
/// is read off the mailbox ([`Hydration`]) when a client opens the session or
/// when the session first appends in this process, and is dropped once the
/// session leaves residency and the store writer has flushed every append
/// queued from it ([`AppState::release_stale_session_logs`]): until then the
/// log, not the store, is the whole conversation.
///
/// A turn-changes snapshot that a later one supersedes is held without its
/// diffs, as the store keeps it, so no window sends them.
#[derive(Clone)]
pub(super) struct SessionLog {
    records: Vec<SessionEventRecord>,
    retained_bytes: usize,
    /// The stored row each record is in, ascending.
    rows: Vec<u64>,
    /// The rows of the records that opened a turn in `fold`, ascending.
    turn_starts: Vec<u64>,
    /// `Timeline::fold_events(records)`, extended by every push so an append
    /// can tell whether it opens a turn, and cloned by timeline loads instead
    /// of folding the records again.
    fold: Timeline,
    /// The record index of each turn's latest snapshot.
    snapshots: TurnSnapshots<usize>,
    /// The stored row the next append takes.
    next_row: u64,
    /// The end flushed by the release barrier in flight, if any.
    release_barrier: Option<u64>,
}

impl SessionLog {
    pub(super) fn new(log: EventLog) -> Self {
        let mut session_log = Self {
            records: Vec::new(),
            retained_bytes: 0,
            rows: Vec::new(),
            turn_starts: Vec::new(),
            fold: Timeline::default(),
            snapshots: TurnSnapshots::default(),
            next_row: log.next_row,
            release_barrier: None,
        };
        for (record, row) in log.records.into_iter().zip(log.rows) {
            session_log.push_at(record, row);
        }
        session_log
    }

    pub(super) fn push(&mut self, record: SessionEventRecord) {
        let row = self.next_row;
        self.next_row += 1;
        self.push_at(record, row);
    }

    fn push_at(&mut self, record: SessionEventRecord, row: u64) {
        let turns = self.fold.turns.len();
        let index = self.records.len();
        let superseded = self
            .snapshots
            .apply_at(&mut self.fold, record.ts, &record.event, index);
        if self.fold.turns.len() > turns {
            self.turn_starts.push(row);
        }
        self.retained_bytes += wire_len(&record) + 1;
        self.records.push(record);
        self.rows.push(row);
        if let Some(earlier) = superseded {
            self.retained_bytes -= wire_len(&self.records[earlier]);
            drop_turn_diffs(&mut self.records[earlier].event);
            self.retained_bytes += wire_len(&self.records[earlier]);
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub(super) fn records(&self) -> &[SessionEventRecord] {
        &self.records
    }

    /// The records in rows from `position` on.
    pub(super) fn records_from(&self, position: u64) -> &[SessionEventRecord] {
        &self.records[self.rows.partition_point(|row| *row < position)..]
    }

    /// The pure fold of every record, before any `mark_idle`.
    pub(super) fn fold(&self) -> &Timeline {
        &self.fold
    }

    /// Every accepted record with its row, as a store read of the same rows returns them.
    pub(super) fn event_log(&self) -> EventLog {
        EventLog {
            records: self.records.clone(),
            rows: self.rows.clone(),
            next_row: self.next_row,
            undecodable: 0,
        }
    }

    /// The row just past the last record: the log's end cursor.
    pub(super) fn end(&self) -> u64 {
        self.next_row
    }

    fn view(&self) -> LogView<'_> {
        LogView {
            records: &self.records,
            from: 0,
            rows: &self.rows,
            end: self.next_row,
            covered_end: self.next_row,
            turn_starts: &self.turn_starts,
            turns: self.fold.turns.len() as u64,
        }
    }

    pub(super) fn events_window(&self, subscription: &Subscription) -> ServerEvent {
        self.view().events_window(subscription)
    }

    pub(super) fn history_page(
        &self,
        before: u64,
        limit: u32,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        self.view().history_page(before, limit)
    }

    fn image_attachment(
        &self,
        item_id: &str,
        image_index: usize,
    ) -> Result<&agent::Attachment, ProtocolError> {
        self.fold
            .entries
            .iter()
            .rev()
            .find(|entry| entry.id == item_id)
            .and_then(|entry| match &entry.content {
                EntryContent::Item(ItemContent::ToolCall { image_reads, .. }) => {
                    image_reads.get(image_index)
                }
                EntryContent::Item(ItemContent::ImageRead { image, .. }) if image_index == 0 => {
                    image.as_ref()
                }
                _ => None,
            })
            .ok_or_else(|| ProtocolError::out_of_scope("image is outside this item"))
    }

    pub(super) fn original_image(
        &self,
        item_id: &str,
        image_index: usize,
    ) -> Result<(u64, &agent::Attachment), ProtocolError> {
        let image = self.image_attachment(item_id, image_index)?;
        let row = self
            .records
            .iter()
            .zip(&self.rows)
            .rev()
            .find_map(|(record, row)| match &record.event {
                AgentEvent::ItemStarted(item)
                | AgentEvent::ItemUpdated(item)
                | AgentEvent::ItemCompleted(item)
                    if item.id == item_id =>
                {
                    Some(*row)
                }
                _ => None,
            })
            .ok_or_else(|| ProtocolError::out_of_scope("image has no stored item"))?;
        Ok((row, image))
    }

    pub(super) fn item_image(
        &self,
        item_id: &str,
        image_index: usize,
        max_bytes: Option<u64>,
    ) -> Result<tcode_services::image_bytes::ImageRead, ProtocolError> {
        let image = self.image_attachment(item_id, image_index)?;
        tcode_services::image_bytes::ImageRead::complete(&image.data_base64, max_bytes)
            .map_err(image_read_error)
    }

    /// The whole output of one item, as the full log folds it.
    pub(super) fn item_output(
        &self,
        session_id: &str,
        item_id: &str,
    ) -> Result<QueryResponse, tcode_protocol::ProtocolError> {
        let output = self
            .fold
            .entries
            .iter()
            .rev()
            .find(|entry| entry.id == item_id)
            .and_then(|entry| match &entry.content {
                EntryContent::Item(ItemContent::ToolCall { output, .. }) => output.clone(),
                EntryContent::Item(ItemContent::CommandExecution { output, .. }) => {
                    Some(output.clone())
                }
                _ => None,
            })
            .ok_or_else(|| tcode_protocol::ProtocolError {
                code: "unknown_item_output".into(),
                message: format!("no output for item {item_id} in {session_id}"),
            })?;
        if output.len() > MAX_SESSION_HISTORY_BYTES {
            return Err(tcode_protocol::ProtocolError {
                code: "item_output_too_large".into(),
                message: "The output exceeds the 8 MiB response limit.".into(),
            });
        }
        Ok(QueryResponse::ItemOutput(output))
    }
}

/// The last rows of a log, as many as a baseline window can use, read with
/// the turn index of the whole log so the window is the one the whole log
/// would give.
pub(super) struct Tail {
    records: Vec<SessionEventRecord>,
    rows: Vec<u64>,
    from: u64,
    end: u64,
    covered_end: u64,
    index: TurnIndex,
}

impl Tail {
    fn read_cut(
        read: &LogRead,
        after: Option<u64>,
        page: Option<(u64, u32)>,
    ) -> std::io::Result<Option<Self>> {
        let Some(index) = read.index.clone() else {
            return Ok(None);
        };
        let end = page.map_or(read.end, |(before, _)| before.min(read.end));
        let after = after.filter(|after| *after <= end);
        let mut tail = Self {
            records: Vec::new(),
            rows: Vec::new(),
            from: 0,
            end: read.end,
            covered_end: end,
            index,
        };
        let start = after.unwrap_or_else(|| match page {
            Some((_, count)) => tail.view().page_start(end, count),
            None => tail
                .view()
                .turn_aligned_start(end.saturating_sub(BASELINE_RECORDS)),
        });
        tail.from = start;
        let mut bytes = 0;
        read.visit_projected_rows(
            start..end,
            after.is_none(),
            &HISTORY_PREVIEW_VERSION,
            HISTORY_PREVIEW_CACHE_BYTES,
            wire_record,
            |row, record| {
                bytes += wire_len(&record) + 1;
                tail.records.push(record);
                tail.rows.push(row);
                if bytes > SESSION_WINDOW_BYTES {
                    if after.is_some() {
                        tail.covered_end = row + 1;
                    } else {
                        tail.from = row;
                    }
                    return false;
                }
                true
            },
        )?;
        if after.is_none() {
            tail.records.reverse();
            tail.rows.reverse();
        }
        Ok(Some(tail))
    }

    fn view(&self) -> LogView<'_> {
        LogView {
            records: &self.records,
            from: self.from,
            rows: &self.rows,
            end: self.end,
            covered_end: self.covered_end,
            turn_starts: &self.index.starts,
            turns: self.index.turns,
        }
    }
}

/// A session's log being read and folded off the mailbox, and what waits for
/// it. A session has at most one, and none while its log is resident.
///
/// The read covers the rows before the position the store writer found next
/// in its queue order, so it holds every record whose append was queued
/// before the hydration began and none queued after. Every record accepted
/// for the session from then on is held in `pending`, in order, and takes the
/// rows after that end.
pub(super) struct Hydration {
    live: bool,
    pending: Vec<SessionEventRecord>,
    /// Subscription replies; one without a request id answers no request.
    replies: Vec<(Option<u64>, Subscription)>,
    /// Queries answered from the whole log.
    queries: Vec<LogQuery>,
    /// The timeline load waiting for the log, with how many `pending` records
    /// had been accepted when it was requested.
    timeline: Option<(TimelineLoad, usize)>,
}

type LogQuery = Box<dyn FnOnce(Result<&SessionLog, &str>) + Send>;

type ItemQuery<T> = Box<dyn FnOnce(&SessionLog) -> Result<T, ProtocolError> + Send>;

struct LogAnswer<T> {
    query: std::sync::Mutex<Option<ItemQuery<T>>>,
    sender: smol::channel::Sender<Result<T, ProtocolError>>,
}

impl<T> LogAnswer<T> {
    fn answer_log(&self, log: Result<&SessionLog, ProtocolError>) {
        if let Some(query) = self.query.lock().unwrap().take() {
            let _ = self.sender.try_send(log.and_then(query));
        }
    }

    fn complete(&self, result: Result<T, ProtocolError>) {
        if self.query.lock().unwrap().take().is_some() {
            let _ = self.sender.try_send(result);
        }
    }
}

impl AppState {
    /// The session's hydration, begun unless its log is being read already.
    /// Callers check first that the log is not resident.
    pub(super) fn hydrate_log(
        &mut self,
        session_id: &str,
        timeline: Option<TimelineLoad>,
        cx: &mut HostCx,
    ) -> &mut Hydration {
        if !self.log_hydrations.contains_key(session_id) {
            let (snapshot, taken) = smol::channel::bounded(1);
            self.enqueue_store_write(
                StoreWrite::SnapshotLog {
                    id: session_id.to_string(),
                    end: snapshot,
                },
                cx,
            );
            let store = self.store.clone();
            let read_id = session_id.to_string();
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let read = match taken.recv().await {
                    Ok(Ok(read)) => read,
                    Ok(Err(error)) => {
                        host_cx.enqueue(move |state, cx| {
                            state.finish_hydration(read_id, Err(error), false, cx)
                        });
                        return;
                    }
                    Err(_) => {
                        host_cx.enqueue(move |state, cx| {
                            state.finish_hydration(
                                read_id,
                                Err("the session store writer has stopped".into()),
                                false,
                                cx,
                            )
                        });
                        return;
                    }
                };
                let full_id = read_id.clone();
                let (log, indexed) = host_cx
                    .unblock(move || {
                        let (end, indexed) = (read.end, read.index.is_some());
                        drop(read);
                        let log = store
                            .read_log_until(&full_id, end)
                            .map(SessionLog::new)
                            .map_err(|error| error.to_string());
                        (log, indexed)
                    })
                    .await;
                host_cx.enqueue(move |state, cx| state.finish_hydration(read_id, log, indexed, cx));
            });
            self.log_hydrations.insert(
                session_id.to_string(),
                Hydration {
                    live: false,
                    pending: Vec::new(),
                    replies: Vec::new(),
                    queries: Vec::new(),
                    timeline: None,
                },
            );
        }
        let hydration = self
            .log_hydrations
            .get_mut(session_id)
            .expect("inserted above");
        if let Some(load) = timeline {
            hydration.timeline = Some((load, hydration.pending.len()));
        }
        hydration
    }

    /// Serve everything that waited for the log, in the order of its
    /// records, and keep the log only if its session is resident.
    fn finish_hydration(
        &mut self,
        session_id: String,
        log: Result<SessionLog, String>,
        indexed: bool,
        cx: &mut HostCx,
    ) {
        let Some(hydration) = self.log_hydrations.remove(&session_id) else {
            return;
        };
        let mut log = match log {
            Ok(log) => log,
            Err(error) => {
                log::error!("could not read the log of {session_id}: {error}");
                let failure = ServerEvent::SessionHistoryError(tcode_protocol::ProtocolError {
                    code: "history_unavailable".into(),
                    message: format!("could not read the history of {session_id}: {error}"),
                });
                for (request_id, subscription) in &hydration.replies {
                    self.reply_with(failure.clone(), *request_id, subscription, cx);
                }
                for query in hydration.queries {
                    query(Err(&error));
                }
                if hydration.timeline.is_some() {
                    self.report_error(
                        RuntimeError::External(format!(
                            "could not load thread {session_id}: {error}"
                        )),
                        cx,
                    );
                }
                return;
            }
        };
        let retained = self.resident(&session_id).is_some();
        let topic = Topic::SessionEvents {
            session_id: session_id.clone(),
        };
        let live = hydration.live || (indexed && !hydration.replies.is_empty());
        // Records a live log accepted are out already: a waiting subscription
        // gets a window that holds them. Otherwise they follow the windows.
        if !live {
            self.reply_from_log(&log, &hydration.replies, cx);
        }
        let timeline = hydration.timeline.filter(|_| retained);
        let fold_at = timeline.as_ref().map(|(_, accepted)| *accepted);
        let mut folded = None;
        for (index, record) in hydration.pending.into_iter().enumerate() {
            if fold_at == Some(index) {
                folded = Some((log.end(), log.fold().clone()));
            }
            if !live {
                self.emit_domain(
                    topic.clone(),
                    ServerEvent::SessionEvent(wire_record(&record).into_owned()),
                    cx,
                );
            }
            log.push(record);
        }
        if live {
            self.reply_from_log(&log, &hydration.replies, cx);
        }
        for query in hydration.queries {
            query(Ok(&log));
        }
        if !retained {
            return;
        }
        let folded = timeline.map(|(load, _)| {
            let (cursor, fold) = folded.unwrap_or_else(|| (log.end(), log.fold().clone()));
            (load, cursor, fold)
        });
        self.event_records.insert(session_id.clone(), log);
        if let Some((load, cursor, fold)) = folded {
            self.fold_timeline(session_id, load, cursor, fold, cx);
        }
    }

    fn reply_from_log(
        &self,
        log: &SessionLog,
        replies: &[(Option<u64>, Subscription)],
        cx: &mut HostCx,
    ) {
        for (request_id, subscription) in replies {
            self.reply_with(
                log.events_window(subscription),
                *request_id,
                subscription,
                cx,
            );
        }
    }

    /// Send `window` as the reply to a subscription unless nobody is
    /// subscribed any more, and acknowledge the request.
    fn reply_with(
        &self,
        window: ServerEvent,
        request_id: Option<u64>,
        subscription: &Subscription,
        cx: &mut HostCx,
    ) {
        if self.subscriptions.contains(&subscription.topic) {
            cx.emit(HostEvent::Domain(EventEnvelope {
                request_id,
                topic: subscription.topic.clone(),
                event: window,
            }));
        }
        if let Some(id) = request_id {
            cx.send_message(HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            });
        }
    }

    fn read_bounded<T: Send + 'static>(
        &mut self,
        session_id: &str,
        read: impl FnOnce(&LogRead) -> std::io::Result<Option<T>> + Send + 'static,
        finish: impl FnOnce(&mut Self, &mut HostCx, Option<T>, usize) + Send + 'static,
        cx: &mut HostCx,
    ) {
        let accepted = self.hydrate_log(session_id, None, cx).pending.len();
        let (snapshot, taken) = smol::channel::bounded(1);
        self.enqueue_store_write(
            StoreWrite::SnapshotLog {
                id: session_id.to_owned(),
                end: snapshot,
            },
            cx,
        );
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let (result, publication) = host_cx
                .unblock(move || {
                    let read_cut = taken.recv_blocking().ok().and_then(Result::ok)?;
                    let result = read(&read_cut)
                        .inspect_err(|error| log::warn!("bounded history read: {error}"))
                        .ok()
                        .flatten();
                    Some((result, read_cut.take_publication()))
                })
                .await
                .map_or((None, None), |(result, publication)| {
                    (result, Some(publication))
                });
            host_cx.enqueue(move |state, cx| finish(state, cx, result, accepted));
            if let Some(publication) = publication {
                host_cx
                    .unblock(move || {
                        if let Err(error) = publication.publish() {
                            log::warn!("could not publish history previews: {error}");
                        }
                    })
                    .await;
            }
        });
    }

    pub(crate) fn reply_to_subscription(
        &mut self,
        request_id: Option<u64>,
        subscription: Subscription,
        cx: &mut HostCx,
    ) {
        if let Topic::CommandOutput { .. } = &subscription.topic {
            self.reply_command_output(request_id, subscription.topic, cx);
            return;
        }
        if let Topic::SessionEvents { session_id } = &subscription.topic {
            if let Some(log) = self.event_records.get(session_id) {
                self.reply_with(
                    log.events_window(&subscription),
                    request_id,
                    &subscription,
                    cx,
                );
                return;
            }
            let session_id = session_id.clone();
            let after = subscription.after;
            self.hydrate_log(&session_id, None, cx)
                .replies
                .push((request_id, subscription.clone()));
            let read_id = session_id.clone();
            self.read_bounded(
                &read_id,
                move |read| Tail::read_cut(read, after, None),
                move |state, cx, tail, _| {
                    let Some(hydration) = state.log_hydrations.get_mut(&session_id) else {
                        return;
                    };
                    if !hydration
                        .replies
                        .iter()
                        .any(|reply| reply.0 == request_id && reply.1 == subscription)
                    {
                        return;
                    }
                    if let Some(tail) = tail {
                        if !hydration.pending.is_empty() {
                            hydration.live = true;
                            return;
                        }
                        if let Some(position) = hydration
                            .replies
                            .iter()
                            .position(|reply| reply.0 == request_id && reply.1 == subscription)
                        {
                            hydration.replies.remove(position);
                            hydration.live = true;
                            log::debug!(
                                "bounded history for {session_id}: {} records in rows {}..{} of {}, before hydration",
                                tail.records.len(), tail.from, tail.covered_end, tail.end,
                            );
                            state.reply_with(
                                tail.view().events_window(&subscription),
                                request_id,
                                &subscription,
                                cx,
                            );
                        }
                    }
                },
                cx,
            );
            return;
        }
        if let Some(mut snapshot) = self.subscription_snapshot(&subscription, &cx.principal) {
            snapshot.request_id = request_id;
            cx.emit(HostEvent::Domain(snapshot));
        }
        if let Some(id) = request_id {
            cx.send_message(HostMessage::Ack {
                id,
                result: Ok(CommandResponse::Unit),
            });
        }
    }

    /// Accept a record for a session whose log is not resident. It is held
    /// while the log is read, since its row comes after whatever the read
    /// holds, and sent at once when the log is already live.
    pub(super) fn hold_record(
        &mut self,
        session_id: &str,
        record: SessionEventRecord,
        cx: &mut HostCx,
    ) {
        let hydration = self.hydrate_log(session_id, None, cx);
        let live = hydration.live;
        let wire = live.then(|| wire_record(&record).into_owned());
        hydration.pending.push(record);
        if let Some(wire) = wire {
            self.emit_domain(
                Topic::SessionEvents {
                    session_id: session_id.to_string(),
                },
                ServerEvent::SessionEvent(wire),
                cx,
            );
        }
    }

    fn query_session_log<T: Send + 'static>(
        &mut self,
        session_id: &str,
        query: impl FnOnce(&SessionLog) -> Result<T, ProtocolError> + Send + 'static,
        cx: &mut HostCx,
    ) -> (
        std::sync::Arc<LogAnswer<T>>,
        HostTask<Result<T, ProtocolError>>,
    ) {
        let (sender, answered) = smol::channel::bounded(1);
        let answer = std::sync::Arc::new(LogAnswer {
            query: std::sync::Mutex::new(Some(Box::new(query))),
            sender,
        });
        if let Some(log) = self.event_records.get(session_id) {
            answer.answer_log(Ok(log));
        } else {
            let pending = answer.clone();
            let read_id = session_id.to_owned();
            self.hydrate_log(session_id, None, cx)
                .queries
                .push(Box::new(move |log| {
                    pending.answer_log(log.map_err(|error| ProtocolError {
                        code: "history_unavailable".into(),
                        message: format!("could not read the history of {read_id}: {error}"),
                    }));
                }));
        }
        let task = cx.spawn_background(async move {
            answered.recv().await.unwrap_or_else(|_| {
                Err(ProtocolError {
                    code: "host_stopped".into(),
                    message: "host stopped during history read".into(),
                })
            })
        });
        (answer, task)
    }

    pub(crate) fn session_history_page(
        &mut self,
        session_id: &str,
        before: u64,
        limit: u32,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, tcode_protocol::ProtocolError>> {
        let resident = self.event_records.contains_key(session_id);
        let (answer, task) =
            self.query_session_log(session_id, move |log| log.history_page(before, limit), cx);
        if resident {
            return task;
        }
        self.read_bounded(
            session_id,
            move |read| Tail::read_cut(read, None, Some((before, limit))),
            move |_, _, tail, _| {
                if let Some(tail) = tail.filter(|tail| before <= tail.end) {
                    answer.complete(tail.view().history_page(before, limit));
                }
            },
            cx,
        );
        task
    }

    pub(super) fn query_item<T: Send + 'static>(
        &mut self,
        session_id: &str,
        item_id: String,
        query: impl FnOnce(&SessionLog) -> Result<T, ProtocolError> + Send + 'static,
        cx: &mut HostCx,
    ) -> HostTask<Result<T, ProtocolError>> {
        let resident = self.event_records.contains_key(session_id);
        let (answer, task) = self.query_session_log(session_id, query, cx);
        if resident {
            return task;
        }
        let read_id = session_id.to_owned();
        let addressed_id = item_id.clone();
        self.read_bounded(
            session_id,
            move |read| read.read_item(&item_id).map(|log| log.map(SessionLog::new)),
            move |state, _, log, accepted| {
                if accepted > 0 {
                    return;
                }
                let suffix = state
                    .log_hydrations
                    .get(&read_id)
                    .map(|hydration| &hydration.pending[accepted..])
                    .unwrap_or_default();
                // Turn starts do not describe the final turn's closing state.
                // A later mention could reuse the id in a newly opened turn.
                if suffix.iter().any(|record| {
                    record.item_id() == Some(addressed_id.as_str())
                        || matches!(record.event, AgentEvent::RewindCompleted { .. })
                }) {
                    return;
                }
                if let Some(log) = log {
                    answer.answer_log(Ok(&log));
                }
            },
            cx,
        );
        task
    }

    pub(crate) fn item_image(
        &mut self,
        session_id: &str,
        item_id: String,
        image_index: usize,
        max_bytes: Option<u64>,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, ProtocolError>> {
        let read = self.query_item(
            session_id,
            item_id.clone(),
            move |log| log.item_image(&item_id, image_index, max_bytes),
            cx,
        );
        decode_image_read(read, cx)
    }

    pub(crate) fn image_preview(
        &self,
        path: std::path::PathBuf,
        pixels: u32,
        cover: bool,
        cx: &HostCx,
    ) -> HostTask<Result<QueryResponse, ProtocolError>> {
        let data_dir = self.store.root().clone();
        let task = cx
            .unblock(move || tcode_services::image_previews::read(&path, pixels, cover, &data_dir));
        cx.spawn_background(async move {
            task.await
                .map(QueryResponse::FileBytes)
                .map_err(|error| ProtocolError {
                    code: "preview_unavailable".into(),
                    message: error.to_string(),
                })
        })
    }

    pub(crate) fn item_image_preview(
        &mut self,
        session_id: &str,
        item_id: String,
        image_index: usize,
        width: u32,
        height: u32,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, ProtocolError>> {
        let data_dir = self.store.root().clone();
        let read = self.query_item(
            session_id,
            item_id.clone(),
            move |log| {
                let image = log.image_attachment(&item_id, image_index)?;
                tcode_services::image_bytes::ImageRead::preview(
                    &image.data_base64,
                    width,
                    height,
                    &data_dir,
                )
                .map_err(image_read_error)
            },
            cx,
        );
        decode_image_read(read, cx)
    }

    pub(crate) fn item_output(
        &mut self,
        session_id: &str,
        item_id: String,
        cx: &mut HostCx,
    ) -> HostTask<Result<QueryResponse, tcode_protocol::ProtocolError>> {
        let read_id = session_id.to_string();
        let scoped = matches!(cx.principal, tcode_protocol::Principal::Space { .. });
        self.query_item(
            session_id,
            item_id.clone(),
            move |log| {
                log.item_output(&read_id, &item_id).map_err(|error| {
                    if scoped && error.code == "unknown_item_output" {
                        ProtocolError::out_of_scope("item is outside this session")
                    } else {
                        error
                    }
                })
            },
            cx,
        )
    }

    pub(crate) fn stored_command_output(
        &mut self,
        session_id: &str,
        item_id: String,
        cx: &mut HostCx,
    ) -> HostTask<Result<String, ProtocolError>> {
        let scoped = matches!(cx.principal, tcode_protocol::Principal::Space { .. });
        let read_id = session_id.to_owned();
        self.query_item(
            session_id,
            item_id.clone(),
            move |log| {
                log.fold
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| entry.id == item_id)
                    .and_then(|entry| match &entry.content {
                        EntryContent::Item(ItemContent::CommandExecution { output, .. }) => {
                            Some(output.clone())
                        }
                        _ => None,
                    })
                    .ok_or_else(|| {
                        if scoped {
                            ProtocolError::out_of_scope("item is outside this session")
                        } else {
                            ProtocolError {
                                code: "unknown_stored_output".into(),
                                message: format!("no stored command output {item_id} in {read_id}"),
                            }
                        }
                    })
            },
            cx,
        )
    }

    /// Queue a store-writer barrier for every cached log whose session left
    /// residency, and drop the log once the barrier confirms every earlier
    /// write committed: a cold read after that sees every append the log had
    /// accepted. Appends that race the barrier re-arm it. A barrier that
    /// reports a failed write, or a writer that is gone, keeps the log: it is
    /// then the only complete copy of the conversation.
    pub(super) fn release_stale_session_logs(&mut self, cx: &mut HostCx) {
        let stale: Vec<(String, u64)> = self
            .event_records
            .iter()
            .filter(|(id, log)| log.release_barrier.is_none() && self.resident(id).is_none())
            .map(|(id, log)| (id.clone(), log.end()))
            .collect();
        for (session_id, flushed) in stale {
            self.event_records
                .get_mut(&session_id)
                .expect("stale log collected above")
                .release_barrier = Some(flushed);
            let (completion, completed) = smol::channel::bounded(1);
            self.enqueue_store_write(StoreWrite::Flush(completion), cx);
            let host_cx = cx.clone();
            HostCx::spawn_detached(cx, async move {
                let flushed_ok = match completed.recv().await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err("the session store writer has stopped".to_owned()),
                };
                host_cx.enqueue(move |state, cx| {
                    let Some(log) = state.event_records.get_mut(&session_id) else {
                        return;
                    };
                    if let Err(error) = flushed_ok {
                        // The barrier stays armed, so the log is never offered
                        // for release again.
                        log::warn!("keeping the in-memory history of {session_id}: {error}");
                        return;
                    }
                    log.release_barrier = None;
                    let appended_since = log.end() != flushed;
                    if state.resident(&session_id).is_some() {
                        return;
                    }
                    if appended_since {
                        state.release_stale_session_logs(cx);
                    } else {
                        state.event_records.remove(&session_id);
                    }
                });
            });
        }
    }
}

fn image_read_error(error: std::io::Error) -> ProtocolError {
    ProtocolError {
        code: if error.kind() == std::io::ErrorKind::FileTooLarge {
            "file_too_large"
        } else {
            "invalid_image"
        }
        .into(),
        message: error.to_string(),
    }
}

fn decode_image_read(
    read: HostTask<Result<tcode_services::image_bytes::ImageRead, ProtocolError>>,
    cx: &mut HostCx,
) -> HostTask<Result<QueryResponse, ProtocolError>> {
    let host_cx = cx.clone();
    cx.spawn_background(async move {
        let read = read.await?;
        host_cx
            .unblock(move || read.read())
            .await
            .map_err(image_read_error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_index_matches_the_full_session_log_fixture() {
        let root =
            std::env::temp_dir().join(format!("turn-boundary-fixture-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        for (row, line) in include_str!("../../../services/src/store/fixtures/turn-index.jsonl")
            .lines()
            .enumerate()
        {
            let event = serde_json::from_str::<AgentEvent>(line).unwrap();
            store.append_event("fixture", row as u64, &event).unwrap();
        }
        let log = SessionLog::new(store.read_log("fixture").unwrap());
        assert_eq!(
            store.turn_index("fixture").unwrap(),
            Some(TurnIndex {
                turns: log.fold.turns.len() as u64,
                starts: log.turn_starts,
            })
        );
        store.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cold_continuation_covers_only_the_rows_it_read() {
        let root = std::env::temp_dir().join(format!("cold-cursor-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::open_at(root.clone()).unwrap();
        let oversized = serde_json::to_string(&AgentEvent::Warning {
            message: "x".repeat(SESSION_WINDOW_BYTES + 1),
        })
        .unwrap();
        let fixture = format!(
            "{oversized}\n\n{{\"type\":\"delta\",\"item_id\":\"answer\",\"kind\":\"assistant_text\",\"text\":\"a\"}}\n{{\"type\":\"delta\",\"item_id\":\"answer\",\"kind\":\"assistant_text\",\"text\":\"b\"}}\n{{\"type\":\"warning\",\"message\":\"last\"}}\n"
        );
        store
            .apply(&[tcode_services::store::Mutation::replace_event_log(
                "thread",
                fixture.into_bytes(),
            )])
            .unwrap();
        let mut meta = SessionMeta::new(ProviderKind::Codex, root.clone(), None);
        meta.id = "thread".into();
        store.upsert_meta(&meta).unwrap();
        store.backfill_thread_activity(&["thread".into()]).unwrap();
        let cut = store.snapshot_log("thread").unwrap();
        let mut subscription = Subscription {
            topic: Topic::SessionEvents {
                session_id: "thread".into(),
            },
            after: Some(0),
            known_index: None,
        };
        let resident = SessionLog::new(store.read_log("thread").unwrap());
        let mut cold_records = Vec::new();
        let mut resident_records = Vec::new();
        for cold in [false, true] {
            subscription.after = Some(0);
            let collected = if cold {
                &mut cold_records
            } else {
                &mut resident_records
            };
            loop {
                let window = if cold {
                    Tail::read_cut(&cut, subscription.after, None)
                        .unwrap()
                        .unwrap()
                        .view()
                        .events_window(&subscription)
                } else {
                    resident.events_window(&subscription)
                };
                let ServerEvent::SessionSnapshot {
                    from,
                    end,
                    total,
                    records,
                    ..
                } = window
                else {
                    panic!("snapshot");
                };
                assert_eq!(from, subscription.after.unwrap());
                assert!(end > from && end <= total);
                collected.extend(records);
                if end == total {
                    break;
                }
                subscription.after = Some(end);
            }
        }
        assert_eq!(cold_records, resident_records);
        assert_eq!(cold_records.len(), 3);
        assert!(matches!(&cold_records[1].event, AgentEvent::Delta { text, .. } if text == "ab"));
        assert!(
            matches!(&cold_records[2].event, AgentEvent::Warning { message } if message == "last")
        );
        drop(cut);
        store.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn history_preview_policy_golden() {
        let image = agent::Attachment {
            media_type: "image/png".into(),
            data_base64: "AQID".repeat(10_000),
            source_path: Some("/fixture/image.png".into()),
        };
        let command = ItemContent::CommandExecution {
            command: "make".into(),
            output: format!("{}tail", "é".repeat(20_000)),
            exit_code: Some(0),
            status: agent::ItemStatus::Completed,
        };
        let tool = ItemContent::ToolCall {
            presentation: None,
            image_reads: vec![image.clone()],
            name: "Read".into(),
            input: serde_json::json!({"path": "/fixture/image.png"}),
            output: Some(format!("{}{}suffix", image.data_base64, "é".repeat(20_000))),
            status: agent::ItemStatus::Completed,
        };
        let attachment = ItemContent::ImageRead {
            path: "/fixture/image.png".into(),
            image: Some(image),
        };
        let item = |id: &str, content| agent::ThreadItem {
            id: id.into(),
            parent_item_id: Some("parent".into()),
            content,
        };
        let events = vec![
            AgentEvent::ItemStarted(item("command-start", command.clone())),
            AgentEvent::ItemUpdated(item("command-update", command.clone())),
            AgentEvent::ItemCompleted(item("command", command)),
            AgentEvent::Delta {
                item_id: "command".into(),
                kind: DeltaKind::CommandOutput,
                text: format!("{}tail", "é".repeat(20_000)),
            },
            AgentEvent::ItemCompleted(item("tool", tool)),
            AgentEvent::ItemCompleted(item("image", attachment)),
        ];
        let records: Vec<_> = events
            .into_iter()
            .map(|event| SessionEventRecord {
                origin: Some(MessageOrigin::Agent),
                author: Some(Author {
                    device_id: "fixture-device".into(),
                    name: "Fixture".into(),
                }),
                ts: Some(1234),
                event,
                elided: None,
            })
            .collect();
        let previews: Vec<_> = records
            .iter()
            .map(|record| wire_record(record).into_owned())
            .collect();
        let serialized = serde_json::to_vec(&previews).unwrap();
        let hash = serialized
            .iter()
            .fold(0xcbf29ce484222325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
        assert_eq!(
            hash, 0x97dd8e2f15bd29c0,
            "wire previews changed: bump HISTORY_PREVIEW_POLICY and update this golden for an intentional policy change"
        );
        for (record, preview) in records.iter().zip(previews) {
            let (_, body) = append_projection(
                wire_len(record) + 1,
                record.ts.unwrap(),
                &record.event,
                record.author.as_ref(),
                record.origin,
            )
            .unwrap();
            assert_eq!(
                serde_json::from_slice::<SessionEventRecord>(&body).unwrap(),
                preview
            );
        }
    }
}
