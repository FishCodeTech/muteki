use std::path::PathBuf;

use agent::FileChange;
use serde::{Deserialize, Serialize};

/// Read-only or result-bearing host operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum Query {
    /// Listener-owned operations, handled by the authenticated remote pipe.
    Hosting {
        action: HostingAction,
    },
    /// Read the agent host's system grants without requesting access.
    ComputerUsePermissions,
    Ping,
    /// Records strictly before the absolute event cursor, oldest first.
    SessionHistoryPage {
        session_id: String,
        before: u64,
        limit: u32,
    },
    ListActiveWorkspace {
        session_id: String,
    },
    ScanExternalHistory,
    GenerateCommitMessage {
        session_id: String,
        included: Option<Vec<String>>,
    },
    LoadGitDiff {
        cwd: PathBuf,
        scope: GitDiffScope,
        base: Option<String>,
        ignore_whitespace: bool,
    },
    BrowseIconImages {
        directory: PathBuf,
    },
    ReadIconImage {
        path: PathBuf,
    },
    /// A preview in the source's own shape: inside a `pixels` square, or with `cover` reaching
    /// both of its edges for the client to clip.
    ReadImagePreview {
        path: PathBuf,
        pixels: u32,
        cover: bool,
    },
    ReadProjectIcon {
        project_id: String,
        /// Physical display size, from 1 through 128 pixels.
        pixels: u32,
    },
    /// Complete file contents; network bridges cap the optional read budget.
    ReadFileBytes {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
    },
    /// Complete file contents for a preview, rejected above 512 KiB.
    ReadFilePreview {
        path: PathBuf,
    },
    SaveAttachment {
        dir: PathBuf,
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
        ext: String,
    },
    RemoveUserFile {
        path: PathBuf,
    },
    /// Render a stored thread into a transferable artifact. Rendering belongs to
    /// the host (it owns the event log); where the bytes land belongs to the
    /// client, so nothing is written here.
    RenderThreadExport {
        session_id: String,
        format: crate::ThreadExportFormat,
    },
    /// Full-text search over the host's own stored session logs. The host owns
    /// the index, the cache and the session order; clients supply no paths.
    SearchSessionContent {
        query: String,
        limit: u32,
    },
    /// The whole output of a tool call or command whose history record carried
    /// only [`OUTPUT_PREVIEW_BYTES`] of it.
    ReadItemOutput {
        session_id: String,
        item_id: String,
    },
    /// The turn of the entry a `tcode_control` anchor names: the last one the
    /// log first recorded at or before `position`, counted among every turn
    /// the whole log folds to. Only the host knows the row of each record, as
    /// windows merge them.
    ThreadAnchorTurn {
        session_id: String,
        position: u64,
    },
    /// Image bytes returned to the model by a read tool; omitted from timeline windows.
    ReadItemImage {
        session_id: String,
        item_id: String,
        image_index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<u64>,
    },
    /// A preview covering a `width`×`height` tile, in the image's own shape.
    ReadItemImagePreview {
        session_id: String,
        item_id: String,
        image_index: usize,
        width: u32,
        height: u32,
    },
    /// Every archived thread, most recently archived first.
    ArchivedSessions,
    /// The latest `tcode_control` writes across every thread, newest first.
    ControlActivity {
        limit: u32,
    },
    /// Review records stored by `record_review`, newest first.
    ReviewRecords {
        limit: u32,
    },
    /// A read of a pull request linked to the thread, answered from GitHub by the host.
    PullRequest {
        session_id: String,
        key: tcode_core::pull_request::PullRequestKey,
        read: crate::PullRequestRead,
    },
}

/// Widths the host will render command output at. Narrower than the low bound is
/// unreadable; wider is a client asking for a grid nobody can see.
pub const STORED_OUTPUT_COLS: std::ops::RangeInclusive<u16> = 20..=400;

/// Screen rows the host emulates command output into. Trailing blank rows are
/// trimmed, so a shorter output returns a shorter frame.
pub const STORED_OUTPUT_ROWS: u16 = 16;

/// Largest export the host will put on one response frame. The transport writes
/// a single WebSocket text frame per NDJSON line and peers read with
/// tungstenite's 16 MiB frame cap, so the base64 payload (4/3 of the raw bytes)
/// plus its envelope must stay well inside that.
pub const MAX_THREAD_EXPORT_BYTES: usize = 8 * 1024 * 1024;

/// Typed response paired with a [`Query`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum QueryResponse {
    Hosting(HostingState),
    ComputerUsePermissions(tcode_core::permissions::ComputerUsePermissions),
    Pong,
    /// Records standing for the log cursors `from..end`, with the same
    /// merging as [`crate::ServerEvent::SessionSnapshot`].
    SessionHistoryPage {
        records: Vec<crate::SessionEventRecord>,
        from: u64,
        end: u64,
        truncated: bool,
    },
    ActiveWorkspace(Vec<PathEntry>),
    IconImages {
        directory: PathBuf,
        parent: Option<PathBuf>,
        entries: Vec<IconImageEntry>,
    },
    ExternalHistory(Vec<RecentDir>),
    CommitMessage(String),
    GitDiff(GitDiffResult),
    FileBytes(#[serde(with = "crate::wire::base64_bytes")] Vec<u8>),
    SavedAttachment(PathBuf),
    UserFileRemoved,
    /// A rendered thread export. `suggested_name` is already safe for a file
    /// name on any client OS; the client picks the destination.
    ThreadExport {
        #[serde(with = "crate::wire::base64_bytes")]
        bytes: Vec<u8>,
        suggested_name: String,
        mime: String,
    },
    SessionContentHits(Vec<SessionSearchHit>),
    ItemOutput(String),
    /// `None` when no entry was recorded at or before the position.
    ThreadAnchorTurn(Option<usize>),
    ArchivedSessions(crate::ArchivedSessions),
    ControlActivity(Vec<crate::ControlActivityEntry>),
    ReviewRecords(Vec<tcode_core::control::ReviewRecord>),
    PullRequest {
        response: Box<crate::PullRequestReadResponse>,
        /// Unix seconds after which the host answers this read afresh.
        expires_at: u64,
    },
}

/// One content match in a stored session, addressed by the folded timeline
/// entry it came from. The sole owner of this shape: the search service builds
/// it and the palette renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub session_title: String,
    pub entry_id: String,
    pub turn: usize,
    pub snippet: String,
}

/// Scope used when loading a Git diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffScope {
    WorkingTree,
    Branch,
    #[serde(other)]
    Unknown,
}

/// Full base- and new-side text for one changed file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileText {
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Result of loading a Git diff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiffResult {
    pub changes: Vec<FileChange>,
    pub texts: Vec<GitFileText>,
    pub truncated: bool,
    pub error: Option<String>,
    pub branches: Vec<String>,
    pub default_base: Option<String>,
}

/// A picker entry on the host. Clients return `path` unchanged: their path
/// separator and drive semantics may differ from the host's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IconImageEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// One listable workspace entry (relative to the workspace root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathEntry {
    pub rel_path: String,
    pub basename: String,
    pub parent: String,
    pub is_dir: bool,
}

impl PathEntry {
    pub fn from_rel(rel_path: String, is_dir: bool) -> Self {
        let (parent, basename) = match rel_path.rfind('/') {
            Some(i) => (rel_path[..i].to_string(), rel_path[i + 1..].to_string()),
            None => (String::new(), rel_path.clone()),
        };
        Self {
            rel_path,
            basename,
            parent,
            is_dir,
        }
    }
}

/// External tool that owns an importable thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceTool {
    ClaudeCode,
    ClaudeDesktop,
    T3Code,
    CodexCli,
    CodexDesktop,
    #[serde(other)]
    Unknown,
}

impl SourceTool {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::ClaudeDesktop => "Claude Desktop",
            Self::T3Code => "T3 Code",
            Self::CodexCli => "Codex CLI",
            Self::CodexDesktop => "Codex Desktop",
            Self::Unknown => "Unknown",
        }
    }
}

/// One importable external thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalThread {
    pub source: SourceTool,
    pub file: PathBuf,
    pub external_id: String,
    pub title_hint: Option<String>,
    pub last_active_ms: u64,
}

/// Recently active directory containing external threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentDir {
    pub path: PathBuf,
    pub last_active_ms: u64,
    pub threads: Vec<ExternalThread>,
}

/// Fresh history and backwards pages are bounded independently of live cursors.
pub const SESSION_HISTORY_RECORDS: usize = 200;
/// The most one history reply may carry: a single record larger than this is
/// an error rather than a reply.
pub const MAX_SESSION_HISTORY_BYTES: usize = 8 * 1024 * 1024;
/// What one history reply aims for. A reply always carries at least one
/// record, so a larger record still arrives alone.
pub const SESSION_WINDOW_BYTES: usize = 512 * 1024;
/// Tool and command output a history record carries; the rest is read with
/// [`Query::ReadItemOutput`].
pub const OUTPUT_PREVIEW_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum HostingAction {
    State,
    SetEnabled(bool),
    /// Mint an invitation, replacing the current one.
    #[serde(alias = "new_code")]
    NewInvitation,
    /// Remove a device; from every machine of the group when it is a group
    /// member.
    RevokeDevice(String),
    Spaces(SpaceAction),
    Group(GroupAction),
}

/// Machine groups: machines that admit the devices any member admits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum GroupAction {
    /// Start a group with this machine and every full-access device paired
    /// with it.
    Create {
        name: String,
    },
    /// Join the group of another member with that machine's invitation,
    /// bringing every full-access device. `address` is one the user typed
    /// for that machine after an attempt found no path.
    Join {
        link: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
    },
    Leave,
    RemoveMachine(String),
    /// Admit here a group member declined while new devices were refused.
    Admit(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub id: String,
    pub name: String,
    /// Every member machine, this one included.
    pub machines: Vec<GroupMachineInfo>,
    /// Group members this machine has not admitted because new devices were
    /// refused when they arrived.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declined: Vec<HostedDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMachineInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub this_machine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum SpaceAction {
    Create {
        name: String,
    },
    Rename {
        id: String,
        name: String,
    },
    Delete {
        id: String,
    },
    SetProjects {
        id: String,
        project_ids: Vec<String>,
    },
    RegenerateLink {
        id: String,
    },
    SetLinkEnabled {
        id: String,
        enabled: bool,
    },
    MoveMember {
        device_id: String,
        space_id: String,
    },
    RemoveMember {
        device_id: String,
        regenerate_link: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceInfo {
    pub id: String,
    pub name: String,
    pub created_unix: u64,
    pub project_ids: Vec<String>,
    pub link: Option<String>,
    pub link_enabled: bool,
    pub link_dead: bool,
    pub members: Vec<HostedDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum DeviceAccess {
    #[default]
    Full,
    Space {
        space_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostingState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_space_id: Option<String>,
    pub enabled: bool,
    /// Seconds until `invite` expires; `0` without one.
    #[serde(default)]
    pub expires_in_secs: u64,
    pub host_id: String,
    pub host_name: String,
    /// The `tcode://pair?…` link for the active invitation, with where the
    /// machine is reachable right now. The link is the secret — there is no
    /// separate code. A client shows it as a QR or copies it; it never takes
    /// it apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
    pub devices: Vec<HostedDevice>,
    #[serde(default)]
    pub spaces: Vec<SpaceInfo>,
    /// How each Traverse source this machine publishes to is doing, in
    /// configured order; empty with none.
    #[serde(default)]
    pub traverse: Vec<TraverseSourceStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupInfo>,
}

/// One Traverse source a hosting machine publishes to, as it stands now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseSourceStatus {
    /// `official`, or the self-hosted instance's base URL, as invitations
    /// name it.
    pub source: String,
    pub manifest: TraverseManifestStatus,
    /// The relays the manifest in effect lists, in its order.
    pub relays: Vec<TraverseRelayStatus>,
    /// The pkarr URLs the manifest in effect lists, in its order.
    pub lookups: Vec<TraverseLookupStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseManifestStatus {
    pub state: TraverseManifestState,
    /// Unix seconds the copy in effect was fetched; absent for the bundled
    /// copy and without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_unix: Option<u64>,
    /// Why the last fetch failed, until one succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraverseManifestState {
    /// Fetched by this run of the machine.
    Live,
    /// Read from the copy an earlier run fetched.
    Cached,
    /// The official manifest built into this build.
    Bundled,
    /// No manifest: a self-hosted instance that has never answered.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseRelayStatus {
    pub url: String,
    /// The region the manifest gives the relay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The lowest round trip the machine's latest network report measured;
    /// absent until one has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// The machine's home relay: the one invitations name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub home: bool,
    /// Why the connection to this home relay last failed, while it is down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseLookupStatus {
    pub url: String,
    pub state: TraverseLookupState,
    /// Unix seconds of the last check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_unix: Option<u64>,
    /// Why the last check failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Whether the machine's own record reads back from a pkarr URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraverseLookupState {
    /// Not checked yet.
    Pending,
    Ok,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedDevice {
    #[serde(default)]
    pub access: DeviceAccess,
    pub id: String,
    pub name: String,
    pub created_unix: u64,
    /// Operating system name and version the device last reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// How the device reaches the machine while connected; `None` offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathInfo>,
}

/// How one live connection between a device and a machine is carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathInfo {
    /// The selected path is a direct UDP path rather than a relay.
    pub direct: bool,
    /// The relay URL carrying the connection when it is not direct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// The direct path's remote address is on this machine's own network
    /// (private, link-local or loopback) rather than punched across the
    /// internet. A peer that predates the flag reads as punched.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lan: bool,
    /// A relay carries the connection while a direct path is open but not
    /// yet the selected one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub probing_direct: bool,
}

/// The selected path, as the flags spell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind<'a> {
    Lan,
    Tunnel,
    Relay { url: Option<&'a str> },
}

impl PathInfo {
    pub fn kind(&self) -> PathKind<'_> {
        match (self.direct, self.lan) {
            (true, true) => PathKind::Lan,
            (true, false) => PathKind::Tunnel,
            (false, _) => PathKind::Relay {
                url: self.relay.as_deref(),
            },
        }
    }
}
