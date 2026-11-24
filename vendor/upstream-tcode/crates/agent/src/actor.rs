use smol::channel::{Receiver, RecvError, Sender};
use smol::future;

use crate::{AgentEvent, SessionCommand};

pub(crate) trait EventSenderExt {
    async fn emit(&self, event: AgentEvent);
}

impl EventSenderExt for Sender<AgentEvent> {
    async fn emit(&self, event: AgentEvent) {
        let _ = self.send(event).await;
    }
}

pub(crate) enum TransportOutcome {
    Continue,
    Closed(String),
    Fatal(String),
}

pub(crate) trait SessionActor {
    type TransportItem;

    fn transport(&self) -> &Receiver<Self::TransportItem>;
    fn events(&self) -> &Sender<AgentEvent>;
    fn command_failure_reason(&self) -> &'static str;

    async fn handle_command(&mut self, command: SessionCommand) -> Result<(), String>;
    async fn handle_transport(
        &mut self,
        item: Result<Self::TransportItem, RecvError>,
    ) -> TransportOutcome;
    fn wake_at(&self) -> Option<std::time::Instant> {
        None
    }
    async fn wake(&mut self) -> Result<(), String> {
        Ok(())
    }
    async fn settle_shutdown(&mut self) {}
    async fn teardown(self, reason: Option<String>) -> Option<String>;
}

pub(crate) async fn run<A: SessionActor>(mut actor: A, commands: &Receiver<SessionCommand>) {
    enum Input<T> {
        Command(Result<SessionCommand, RecvError>),
        Transport(Result<T, RecvError>),
        Wake,
    }

    let reason = loop {
        let deadline = actor.wake_at();
        let input = future::race(
            async { Input::Command(commands.recv().await) },
            future::race(
                async { Input::Transport(actor.transport().recv().await) },
                async {
                    match deadline {
                        Some(at) => {
                            smol::Timer::at(at).await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                    Input::Wake
                },
            ),
        )
        .await;

        match input {
            Input::Wake => {
                if let Err(message) = actor.wake().await {
                    actor
                        .events()
                        .emit(AgentEvent::Error {
                            message: message.clone(),
                            fatal: true,
                        })
                        .await;
                    break Some(message);
                }
            }
            Input::Command(Ok(SessionCommand::Shutdown)) | Input::Command(Err(_)) => {
                actor.settle_shutdown().await;
                break None;
            }
            Input::Command(Ok(command)) => {
                if let Err(message) = actor.handle_command(command).await {
                    let _ = actor
                        .events()
                        .send(AgentEvent::Error {
                            message,
                            fatal: true,
                        })
                        .await;
                    break Some(actor.command_failure_reason().into());
                }
            }
            Input::Transport(item) => match actor.handle_transport(item).await {
                TransportOutcome::Continue => {}
                TransportOutcome::Closed(reason) => break Some(reason),
                TransportOutcome::Fatal(message) => {
                    let _ = actor
                        .events()
                        .send(AgentEvent::Error {
                            message: message.clone(),
                            fatal: true,
                        })
                        .await;
                    break Some(message);
                }
            },
        }
    };

    let events = actor.events().clone();
    let reason = actor.teardown(reason).await;
    let _ = events.send(AgentEvent::SessionClosed { reason }).await;
}
