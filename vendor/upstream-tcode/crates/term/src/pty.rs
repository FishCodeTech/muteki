//! Host-side pseudoterminal ownership and raw byte transport.

use std::{
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

#[cfg(windows)]
mod windows;

#[cfg(unix)]
use rio_vt::corcovado::unix::UnixReady;
use rio_vt::event::WindowSize;
#[cfg(unix)]
use rio_vt::{
    corcovado::{Events, Poll, PollOpt, Ready, Token, channel},
    teletypewriter::{
        self as tty, ChildEvent, EventedPty as _, ProcessReadWrite as _, WinsizeBuilder,
    },
};
#[cfg(unix)]
use std::{
    collections::VecDeque,
    io::{Read as _, Write as _},
    os::unix::process::ExitStatusExt as _,
};

use crate::{
    DEFAULT_CELL_HEIGHT_PX, DEFAULT_CELL_WIDTH_PX, DEFAULT_COLS, DEFAULT_ROWS, pty_info,
    sync::MutexExt as _,
};

#[cfg(unix)]
const READ_BUFFER_SIZE: usize = 0x10_0000;
#[cfg(unix)]
const COMMAND_TOKEN: Token = Token(0);

fn initial_window_size() -> WindowSize {
    WindowSize {
        rows: DEFAULT_ROWS as u16,
        cols: DEFAULT_COLS as u16,
        width: (DEFAULT_COLS * DEFAULT_CELL_WIDTH_PX as usize) as u16,
        height: (DEFAULT_ROWS * DEFAULT_CELL_HEIGHT_PX as usize) as u16,
    }
}

/// Data emitted by the host-side PTY.
///
/// Receivers see raw child output before terminal emulation. Foreground process
/// metadata and exit status are separate data events so they can cross the same
/// transport boundary without exposing platform process or PTY types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PtyEvent {
    Output(Vec<u8>),
    ProcessInfoChanged {
        name: String,
        working_directory: PathBuf,
    },
    Exited {
        exit_code: Option<i32>,
    },
}

struct Shared {
    process_name: Option<String>,
    working_directory: Option<PathBuf>,
    exited: bool,
    command_line: String,
    command_label: Option<String>,
    /// Grows with every input write and every output read, so a command
    /// started while the shell is inspected still changes it.
    activity: u64,
}

/// Host-side handle for a child process running in a pseudoterminal.
///
/// This type owns process lifecycle and byte I/O only. It has no terminal-grid
/// state and its public API contains no grid or parser types.
pub(crate) struct PtyHandle {
    sender: PtyCommandSender,
    notifications: async_channel::Sender<PtyEvent>,
    events: async_channel::Receiver<PtyEvent>,
    shared: Arc<Mutex<Shared>>,
    shell_name: String,
    cwd: PathBuf,
    pty_info: Arc<pty_info::PtyInfo>,
    refresh_running: Arc<AtomicBool>,
}

impl PtyHandle {
    /// Spawn the platform's default interactive shell in `cwd`.
    pub fn spawn(cwd: impl AsRef<Path>) -> io::Result<Self> {
        let (shell, args) = default_shell();
        let shell_name = shell_label(&shell);
        Self::spawn_command(cwd, shell, args, shell_name)
    }

    pub(crate) fn spawn_command(
        cwd: impl AsRef<Path>,
        program: String,
        args: Vec<String>,
        shell_name: String,
    ) -> io::Result<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let size = initial_window_size();
        #[cfg(unix)]
        let environment = Some(vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
            ("TERM_PROGRAM".to_string(), "tcode".to_string()),
        ]);
        #[cfg(unix)]
        let working_directory = Some(cwd.to_string_lossy().into_owned());
        #[cfg(unix)]
        let mut pty = with_pty_creation(|| {
            tty::create_pty_with_spawn(
                Some(&program),
                args,
                &working_directory,
                environment,
                size.cols,
                size.rows,
                size.width,
                size.height,
            )
        })?;
        #[cfg(unix)]
        let pty_info = Arc::new(pty_info::PtyInfo::new(
            pty.reader().try_clone()?,
            (*pty.child.pid) as u32,
        ));
        #[cfg(not(unix))]
        let pty_info = Arc::new(pty_info::PtyInfo::new());
        let shared = Arc::new(Mutex::new(Shared {
            process_name: None,
            working_directory: Some(cwd.clone()),
            exited: false,
            command_line: String::new(),
            command_label: None,
            activity: 0,
        }));
        let refresh_running = Arc::new(AtomicBool::new(false));
        let (notifications, events) = async_channel::unbounded();
        #[cfg(unix)]
        let command_sender = {
            let (sender, receiver) = channel::channel();
            let poll = Poll::new()?;
            // corcovado's channel drops the wakeup of a send that races its
            // registration, so register before the handle can send anything.
            poll.register(&receiver, COMMAND_TOKEN, Ready::readable(), PollOpt::edge())?;
            let mut tokens = (COMMAND_TOKEN.0 + 1..).map(Token::from);
            pty.register(&poll, &mut tokens, Ready::readable(), PollOpt::level())?;
            let command_sender = PtyCommandSender { sender };
            let event_loop = RawPtyEventLoop {
                pty,
                poll,
                receiver,
                notifications: notifications.clone(),
                shared: shared.clone(),
                pty_info: pty_info.clone(),
                refresh_running: refresh_running.clone(),
            };
            thread::Builder::new()
                .name("tcode-pty-io".into())
                .spawn(move || event_loop.run())?;
            command_sender
        };
        #[cfg(windows)]
        let command_sender = windows::spawn(
            &cwd,
            program,
            args,
            size,
            shared.clone(),
            notifications.clone(),
        )?;

        schedule_process_refresh(
            pty_info.clone(),
            shared.clone(),
            notifications.clone(),
            refresh_running.clone(),
        );

        Ok(Self {
            sender: command_sender,
            notifications,
            events,
            shared,
            shell_name,
            cwd,
            pty_info,
            refresh_running,
        })
    }

    /// Return the raw PTY event stream.
    ///
    /// Cloned receivers compete for events. A transport should install one
    /// draining task and fan out from there when multiple consumers are needed.
    pub fn events(&self) -> async_channel::Receiver<PtyEvent> {
        self.events.clone()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn working_directory(&self) -> PathBuf {
        self.shared
            .lock_recover()
            .working_directory
            .clone()
            .unwrap_or_else(|| self.cwd.clone())
    }

    pub fn label(&self) -> String {
        self.shared
            .lock_recover()
            .process_name
            .clone()
            .unwrap_or_else(|| self.shell_name.clone())
    }

    pub fn activity_mark(&self) -> u64 {
        self.shared.lock_recover().activity
    }

    /// Fresh inspection: the shell itself is in the foreground with no child
    /// process. `None` when the platform cannot tell.
    pub fn idle_shell(&self) -> Option<bool> {
        self.pty_info.idle_shell(&self.shell_name)
    }

    pub fn exited(&self) -> bool {
        self.shared.lock_recover().exited
    }

    pub(crate) fn write_input_inner(&self, bytes: Vec<u8>) -> io::Result<bool> {
        let label_changed = {
            let mut shared = self.shared.lock_recover();
            if !bytes.is_empty() {
                shared.activity += 1;
            }
            let previous_label = shared.command_label.clone();
            track_command_input(&mut shared, &bytes);
            shared.command_label != previous_label
        };
        let result = self.sender.send(PtyCommand::Input(bytes));
        schedule_process_refresh(
            self.pty_info.clone(),
            self.shared.clone(),
            self.notifications.clone(),
            self.refresh_running.clone(),
        );
        result.map(|()| label_changed)
    }

    /// Write terminal-protocol response bytes without tracking them as user input.
    pub fn write_raw(&self, bytes: impl Into<Vec<u8>>) -> io::Result<()> {
        self.sender.send(PtyCommand::Input(bytes.into()))
    }

    /// Resize the host PTY with row/column and total pixel dimensions.
    pub fn resize(&self, size: WindowSize) -> io::Result<()> {
        self.sender.send(PtyCommand::Resize(size))
    }

    /// Terminate the child process.
    pub fn kill(&self) -> io::Result<()> {
        self.sender.send(PtyCommand::Kill)
    }

    pub(crate) fn writer(&self) -> PtyWriter {
        PtyWriter {
            sender: self.sender.clone(),
        }
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        let _ = self.sender.send(PtyCommand::Shutdown);
    }
}

#[derive(Clone)]
pub(crate) struct PtyWriter {
    sender: PtyCommandSender,
}

impl PtyWriter {
    pub(crate) fn write_raw(&self, bytes: Vec<u8>) {
        let _ = self.sender.send(PtyCommand::Input(bytes));
    }
}

enum PtyCommand {
    Input(Vec<u8>),
    Resize(WindowSize),
    Kill,
    Shutdown,
    #[cfg(windows)]
    ChildExited,
    #[cfg(windows)]
    OutputClosed,
}

#[derive(Clone)]
struct PtyCommandSender {
    #[cfg(unix)]
    sender: channel::Sender<PtyCommand>,
    #[cfg(windows)]
    sender: mpsc::Sender<PtyCommand>,
}

impl PtyCommandSender {
    fn send(&self, command: PtyCommand) -> io::Result<()> {
        self.sender
            .send(command)
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "PTY event loop has stopped"))
    }
}

#[cfg(unix)]
struct RawPtyEventLoop {
    pty: tty::Pty,
    poll: Poll,
    receiver: channel::Receiver<PtyCommand>,
    notifications: async_channel::Sender<PtyEvent>,
    shared: Arc<Mutex<Shared>>,
    pty_info: Arc<pty_info::PtyInfo>,
    refresh_running: Arc<AtomicBool>,
}

#[cfg(unix)]
impl RawPtyEventLoop {
    fn run(mut self) {
        let poll_options = PollOpt::level();
        let mut events = Events::with_capacity(1024);
        let mut writes = VecDeque::new();
        let mut current_write = None;
        let mut buffer = vec![0; READ_BUFFER_SIZE];
        let mut last_interest = Ready::readable();

        'event_loop: loop {
            events.clear();
            match self.poll.poll(&mut events, None) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }

            if !self.drain_commands(&mut writes) {
                break;
            }

            for event in events.iter() {
                let token = event.token();
                if token == COMMAND_TOKEN {
                    continue;
                }
                if token == self.pty.child_event_token() {
                    if let Some(ChildEvent::Exited(exit_code)) = self.pty.next_child_event() {
                        let _ = self.read_output(&mut buffer);
                        // teletypewriter reports the raw waitpid status, including signal bits.
                        let exit_code = exit_code
                            .and_then(|status| std::process::ExitStatus::from_raw(status).code());
                        record_exit(&self.shared, &self.notifications, exit_code);
                        break 'event_loop;
                    }
                } else if token == self.pty.read_token() || token == self.pty.write_token() {
                    #[cfg(unix)]
                    if UnixReady::from(event.readiness()).is_hup() {
                        continue;
                    }
                    if event.readiness().is_readable()
                        && let Err(_error) = self.read_output(&mut buffer)
                    {
                        #[cfg(target_os = "linux")]
                        if _error.raw_os_error() == Some(libc::EIO) {
                            continue;
                        }
                        break 'event_loop;
                    }
                    if event.readiness().is_writable()
                        && Self::write_pending(&mut self.pty, &mut writes, &mut current_write)
                            .is_err()
                    {
                        break 'event_loop;
                    }
                }
            }

            let needs_write = current_write.is_some() || !writes.is_empty();
            let mut interest = Ready::readable();
            if needs_write {
                interest.insert(Ready::writable());
            }
            if interest != last_interest {
                if self
                    .pty
                    .reregister(&self.poll, interest, poll_options)
                    .is_err()
                {
                    break;
                }
                last_interest = interest;
            }
        }

        let _ = self.poll.deregister(&self.receiver);
        let _ = self.pty.deregister(&self.poll);
    }

    fn drain_commands(&mut self, writes: &mut VecDeque<Vec<u8>>) -> bool {
        loop {
            match self.receiver.try_recv() {
                Ok(PtyCommand::Input(bytes)) => {
                    if !bytes.is_empty() {
                        writes.push_back(bytes);
                    }
                }
                Ok(PtyCommand::Resize(size)) => {
                    let _ = self.pty.set_winsize(WinsizeBuilder::from(size));
                }
                Ok(PtyCommand::Kill) => {
                    let _ = self.pty.child.terminate();
                }
                Ok(PtyCommand::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => return false,
                Err(mpsc::TryRecvError::Empty) => return true,
            }
        }
    }

    fn read_output(&mut self, buffer: &mut [u8]) -> io::Result<()> {
        let mut read_any = false;
        loop {
            match self.pty.reader().read(buffer) {
                Ok(0) => break,
                Ok(read) => {
                    read_any = true;
                    self.shared.lock_recover().activity += 1;
                    let _ = self
                        .notifications
                        .try_send(PtyEvent::Output(buffer[..read].to_vec()));
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
                {
                    if error.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if read_any {
            schedule_process_refresh(
                self.pty_info.clone(),
                self.shared.clone(),
                self.notifications.clone(),
                self.refresh_running.clone(),
            );
        }
        Ok(())
    }

    fn write_pending(
        pty: &mut tty::Pty,
        writes: &mut VecDeque<Vec<u8>>,
        current: &mut Option<PendingWrite>,
    ) -> io::Result<()> {
        if current.is_none() {
            *current = writes.pop_front().map(PendingWrite::new);
        }
        while let Some(pending) = current.as_mut() {
            match pty.writer().write(pending.remaining()) {
                Ok(0) => return Ok(()),
                Ok(written) => {
                    pending.written += written;
                    if pending.remaining().is_empty() {
                        *current = writes.pop_front().map(PendingWrite::new);
                    }
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
                {
                    if error.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn record_exit(
    shared: &Mutex<Shared>,
    notifications: &async_channel::Sender<PtyEvent>,
    exit_code: Option<i32>,
) {
    let mut shared = shared.lock_recover();
    shared.exited = true;
    shared.command_label = None;
    drop(shared);
    let _ = notifications.try_send(PtyEvent::Exited { exit_code });
}

#[cfg(unix)]
struct PendingWrite {
    bytes: Vec<u8>,
    written: usize,
}

#[cfg(unix)]
impl PendingWrite {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, written: 0 }
    }

    fn remaining(&self) -> &[u8] {
        &self.bytes[self.written..]
    }
}

fn refresh_process_info(
    info: &pty_info::PtyInfo,
    shared: &Mutex<Shared>,
    notifications: &async_channel::Sender<PtyEvent>,
) {
    if let Some(process) = info.load() {
        let mut shared = shared.lock_recover();
        let changed = shared.process_name.as_deref() != Some(&process.name)
            || shared.working_directory.as_ref() != Some(&process.cwd);
        shared.process_name = Some(process.name.clone());
        shared.working_directory = Some(process.cwd.clone());
        drop(shared);
        if changed {
            let _ = notifications.try_send(PtyEvent::ProcessInfoChanged {
                name: process.name,
                working_directory: process.cwd,
            });
        }
    }
}

fn schedule_process_refresh(
    info: Arc<pty_info::PtyInfo>,
    shared: Arc<Mutex<Shared>>,
    notifications: async_channel::Sender<PtyEvent>,
    running: Arc<AtomicBool>,
) {
    if !info.should_refresh() || running.swap(true, Ordering::AcqRel) {
        return;
    }
    let running_on_error = running.clone();
    let spawn_result = thread::Builder::new()
        .name("tcode-terminal-pty-info".into())
        .spawn(move || {
            refresh_process_info(&info, &shared, &notifications);
            // Input/output can precede the shell applying `cd` or launching
            // its foreground process. One bounded trailing refresh catches
            // that transition even when the command itself is silent.
            thread::sleep(Duration::from_millis(300));
            refresh_process_info(&info, &shared, &notifications);
            running.store(false, Ordering::Release);
        });
    if spawn_result.is_err() {
        running_on_error.store(false, Ordering::Release);
    }
}

/// Derive the compact tab label used after a command is submitted.
fn derive_command_label(command: &str) -> Option<String> {
    let first = command.split_whitespace().next()?;
    let first = first.rsplit('/').next().unwrap_or(first);
    (!first.is_empty()).then(|| first.to_string())
}

fn track_command_input(shared: &mut Shared, bytes: &[u8]) {
    for &byte in bytes {
        match byte {
            3 => {
                shared.command_line.clear();
                shared.command_label = None;
            }
            b'\r' | b'\n' => {
                shared.command_label = derive_command_label(&shared.command_line);
                shared.command_line.clear();
            }
            0x7f | 0x08 => {
                shared.command_line.pop();
            }
            0x20..=0x7e => shared.command_line.push(byte as char),
            _ => {}
        }
    }
}

/// The interactive shell to spawn, as `(program, args)`.
pub(crate) fn default_shell() -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string());
        (shell, Vec::new())
    }
    #[cfg(not(windows))]
    {
        let shell = unix_shell(std::env::var("SHELL").ok().as_deref());
        (shell, vec!["-l".to_string()])
    }
}

#[cfg(not(windows))]
pub(crate) fn unix_shell(configured_shell: Option<&str>) -> String {
    configured_shell
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or(if cfg!(target_os = "macos") {
            "/bin/zsh"
        } else {
            "/bin/sh"
        })
        .to_string()
}

pub(crate) fn shell_label(shell: &str) -> String {
    let name = shell
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("shell");
    name.rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .to_string()
}

#[cfg(target_os = "macos")]
pub(crate) fn with_pty_creation<T>(create: impl FnOnce() -> T) -> T {
    // Concurrent macOS openpty(3) calls can return an invalid negative errno.
    // Serialize only PTY creation process-wide; the terminal lifetime remains unlocked.
    static PTY_CREATION_LOCK: Mutex<()> = Mutex::new(());
    let guard = PTY_CREATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = create();
    drop(guard);
    result
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn with_pty_creation<T>(create: impl FnOnce() -> T) -> T {
    create()
}
