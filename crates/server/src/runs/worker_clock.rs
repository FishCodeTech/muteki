//! Shared budget/conclusion/checkpoint clock; delivery and result semantics stay in each transport.
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone,Copy)]
enum Phase{Execute,Conclude,AwaitingExit,Checkpoint{deadline:Instant,suspended:f64}}
pub(super) struct WorkerClock{
    phase:Phase,
    pub checkpoint_sent:bool,
    pub total_deadline:Instant,
    pub soft_reminder_deadline:Option<Instant>,
}
impl WorkerClock{
    pub fn new(started:Instant,execute:u64,slice:Option<u64>)->Self{Self{phase:Phase::Execute,checkpoint_sent:false,total_deadline:started+Duration::from_secs(execute),soft_reminder_deadline:slice.filter(|slice|*slice<execute).map(|slice|started+Duration::from_secs(slice))}}
    pub fn concluding(&self)->bool{!matches!(self.phase,Phase::Execute)}
    pub fn conclude(&mut self){self.phase=Phase::Conclude;self.soft_reminder_deadline=None;}
    pub fn await_exit(&mut self){self.phase=Phase::AwaitingExit;self.soft_reminder_deadline=None;}
    pub fn awaiting_exit(&self)->bool{matches!(self.phase,Phase::AwaitingExit)}
    pub fn arm_checkpoint(&mut self,suspended:f64){self.phase=Phase::Checkpoint{deadline:self.total_deadline,suspended};self.soft_reminder_deadline=None;}
    pub fn checkpoint_remaining(&self,suspended:f64)->Option<f64>{match self.phase{Phase::Checkpoint{deadline,suspended:initial}=>{let now=Instant::now();Some(deadline.saturating_duration_since(now).as_secs_f64()-now.saturating_duration_since(deadline).as_secs_f64()+(suspended-initial).max(0.))},_=>None}}
    pub fn wait(&mut self,seconds:f64){self.total_deadline=Instant::now()+Duration::from_secs_f64(seconds.max(0.));}
}
