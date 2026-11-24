use android_activity::AndroidAppWaker;
use gpui::{PlatformDispatcher, Priority, RunnableVariant};
use parking_lot::{Condvar, Mutex};
use std::{collections::VecDeque, sync::Arc, thread, time::Duration};

const MIN_BACKGROUND_THREADS: usize = 2;

pub(crate) struct AndroidDispatcher {
    main_queue: Mutex<RunnableQueues>,
    background_queue: Arc<SharedQueue>,
    main_thread: thread::ThreadId,
    waker: AndroidAppWaker,
    _workers: Vec<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct RunnableQueues {
    realtime: VecDeque<RunnableVariant>,
    high: VecDeque<RunnableVariant>,
    medium: VecDeque<RunnableVariant>,
    low: VecDeque<RunnableVariant>,
}

impl RunnableQueues {
    fn push(&mut self, priority: Priority, runnable: RunnableVariant) {
        match priority {
            Priority::RealtimeAudio => self.realtime.push_back(runnable),
            Priority::High => self.high.push_back(runnable),
            Priority::Medium => self.medium.push_back(runnable),
            Priority::Low => self.low.push_back(runnable),
        }
    }

    fn pop(&mut self) -> Option<RunnableVariant> {
        self.realtime
            .pop_front()
            .or_else(|| self.high.pop_front())
            .or_else(|| self.medium.pop_front())
            .or_else(|| self.low.pop_front())
    }
}

#[derive(Default)]
struct SharedQueue {
    runnables: Mutex<RunnableQueues>,
    ready: Condvar,
}

impl SharedQueue {
    fn push(&self, priority: Priority, runnable: RunnableVariant) {
        self.runnables.lock().push(priority, runnable);
        self.ready.notify_one();
    }

    fn pop(&self) -> RunnableVariant {
        let mut runnables = self.runnables.lock();
        loop {
            if let Some(runnable) = runnables.pop() {
                return runnable;
            }
            self.ready.wait(&mut runnables);
        }
    }
}

impl AndroidDispatcher {
    pub(crate) fn new(waker: AndroidAppWaker) -> Arc<Self> {
        let background_queue = Arc::new(SharedQueue::default());
        let thread_count = thread::available_parallelism()
            .map_or(MIN_BACKGROUND_THREADS, |count| {
                count.get().max(MIN_BACKGROUND_THREADS)
            });

        let workers = (0..thread_count)
            .map(|index| {
                let queue = background_queue.clone();
                thread::Builder::new()
                    .name(format!("gpui-worker-{index}"))
                    .spawn(move || {
                        loop {
                            let runnable = queue.pop();
                            runnable.run();
                        }
                    })
                    .expect("failed to spawn GPUI background worker")
            })
            .collect();

        Arc::new(Self {
            main_queue: Mutex::new(RunnableQueues::default()),
            background_queue,
            main_thread: thread::current().id(),
            waker,
            _workers: workers,
        })
    }

    pub(crate) fn drain_main_queue(&self) {
        loop {
            let runnable = self.main_queue.lock().pop();
            let Some(runnable) = runnable else {
                break;
            };
            runnable.run();
        }
    }
}

impl PlatformDispatcher for AndroidDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        self.background_queue.push(priority, runnable);
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        self.main_queue.lock().push(priority, runnable);
        self.waker.wake();
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        thread::Builder::new()
            .name("gpui-timer".into())
            .spawn(move || {
                thread::sleep(duration);
                runnable.run();
            })
            .expect("failed to spawn GPUI timer thread");
    }

    fn spawn_realtime(&self, callback: Box<dyn FnOnce() + Send>) {
        thread::Builder::new()
            .name("gpui-realtime".into())
            .spawn(callback)
            .expect("failed to spawn GPUI realtime thread");
    }
}
