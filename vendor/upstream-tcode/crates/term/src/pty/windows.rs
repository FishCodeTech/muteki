use std::io::{Read as _, Write as _};

use portable_pty::{CommandBuilder, PtySize};

use super::*;

pub(super) fn spawn(
    cwd: &Path,
    program: String,
    args: Vec<String>,
    size: WindowSize,
    shared: Arc<Mutex<Shared>>,
    notifications: async_channel::Sender<PtyEvent>,
) -> io::Result<PtyCommandSender> {
    // PtyHandle retains this cwd for later splits; don't silently start elsewhere.
    if !cwd.metadata()?.is_dir() {
        return Err(io::Error::new(
            ErrorKind::NotADirectory,
            "PTY cwd is not a directory",
        ));
    }
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.width,
            pixel_height: size.height,
        })
        .map_err(io::Error::other)?;
    let mut command = CommandBuilder::new(program);
    command.args(args);
    command.cwd(cwd);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "tcode");
    let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
    let mut writer = pair.master.take_writer().map_err(io::Error::other)?;
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(io::Error::other)?;
    drop(pair.slave);
    let mut killer = child.clone_killer();
    let (sender, receiver) = mpsc::channel();
    let child_events = sender.clone();
    let reader_events = sender.clone();

    thread::Builder::new()
        .name("tcode-pty-io".into())
        .spawn(move || {
            let exit_code = thread::scope(|scope| {
                let output_notifications = &notifications;
                scope.spawn(move || {
                    let mut buffer = [0; 65536];
                    loop {
                        match reader.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(read) => {
                                let _ = output_notifications
                                    .try_send(PtyEvent::Output(buffer[..read].to_vec()));
                            }
                            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        }
                    }
                    let _ = reader_events.send(PtyCommand::OutputClosed);
                });
                let (input, writes) = mpsc::channel::<Vec<u8>>();
                scope.spawn(move || {
                    while let Ok(bytes) = writes.recv() {
                        if writer.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                });
                let waiter = scope.spawn(move || {
                    let exit_code = child.wait().ok().map(|status| status.exit_code() as i32);
                    let _ = child_events.send(PtyCommand::ChildExited);
                    exit_code
                });

                let mut master = Some(pair.master);
                loop {
                    match receiver.recv() {
                        Ok(PtyCommand::Input(bytes)) => {
                            let _ = input.send(bytes);
                        }
                        Ok(PtyCommand::Resize(size)) => {
                            if let Some(master) = &master {
                                let _ = master.resize(PtySize {
                                    rows: size.rows,
                                    cols: size.cols,
                                    pixel_width: size.width,
                                    pixel_height: size.height,
                                });
                            }
                        }
                        Ok(PtyCommand::Kill) => {
                            let _ = killer.kill();
                        }
                        Ok(PtyCommand::ChildExited | PtyCommand::Shutdown) | Err(_) => {
                            // ClosePseudoConsole can wait for a cursor-position reply
                            // on older Windows. Keep forwarding input while it closes.
                            if let Some(master) = master.take() {
                                scope.spawn(move || drop(master));
                            }
                        }
                        Ok(PtyCommand::OutputClosed) => {
                            let _ = killer.kill();
                            break;
                        }
                    }
                }
                drop(master);
                drop(input);
                waiter.join().expect("PTY child waiter panicked")
            });
            record_exit(&shared, &notifications, exit_code);
        })?;

    Ok(PtyCommandSender { sender })
}
