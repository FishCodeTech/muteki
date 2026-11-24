//! GCD-backed GPUI task dispatcher.

use gpui::{PlatformDispatcher, Priority, RunnableVariant};
use std::{ffi::c_void, ptr::NonNull, time::Duration};

type DispatchQueue = *mut c_void;
type DispatchTime = u64;

const DISPATCH_TIME_NOW: DispatchTime = 0;
const DISPATCH_QUEUE_PRIORITY_HIGH: i64 = 2;
const DISPATCH_QUEUE_PRIORITY_DEFAULT: i64 = 0;
const DISPATCH_QUEUE_PRIORITY_LOW: i64 = -2;

unsafe extern "C" {
    static _dispatch_main_q: c_void;

    fn dispatch_async_f(
        queue: DispatchQueue,
        context: *mut c_void,
        work: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn dispatch_after_f(
        when: DispatchTime,
        queue: DispatchQueue,
        context: *mut c_void,
        work: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn dispatch_get_global_queue(identifier: i64, flags: usize) -> DispatchQueue;
    fn dispatch_time(when: DispatchTime, delta: i64) -> DispatchTime;
}

fn main_queue() -> DispatchQueue {
    std::ptr::addr_of!(_dispatch_main_q).cast_mut().cast()
}

fn gcd_priority(priority: Priority) -> i64 {
    match priority {
        Priority::RealtimeAudio => {
            panic!("RealtimeAudio work must use PlatformDispatcher::spawn_realtime")
        }
        Priority::High => DISPATCH_QUEUE_PRIORITY_HIGH,
        Priority::Medium => DISPATCH_QUEUE_PRIORITY_DEFAULT,
        Priority::Low => DISPATCH_QUEUE_PRIORITY_LOW,
    }
}

#[derive(Debug, Default)]
pub(crate) struct IosDispatcher;

impl PlatformDispatcher for IosDispatcher {
    fn is_main_thread(&self) -> bool {
        super::is_main_thread()
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        let context = runnable.into_raw().as_ptr().cast::<c_void>();
        unsafe {
            dispatch_async_f(
                dispatch_get_global_queue(gcd_priority(priority), 0),
                context,
                Some(run_runnable),
            );
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        let context = runnable.into_raw().as_ptr().cast::<c_void>();
        unsafe {
            dispatch_async_f(main_queue(), context, Some(run_runnable));
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let context = runnable.into_raw().as_ptr().cast::<c_void>();
        let nanoseconds = i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
        unsafe {
            let when = dispatch_time(DISPATCH_TIME_NOW, nanoseconds);
            let queue = dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0);
            dispatch_after_f(when, queue, context, Some(run_runnable));
        }
    }

    fn spawn_realtime(&self, task: Box<dyn FnOnce() + Send>) {
        if let Err(error) = std::thread::Builder::new()
            .name("gpui-ios-realtime".into())
            .spawn(task)
        {
            log::error!("failed to spawn iOS realtime worker: {error}");
        }
    }
}

unsafe extern "C" fn run_runnable(context: *mut c_void) {
    let Some(context) = NonNull::new(context.cast::<()>()) else {
        return;
    };
    let runnable = unsafe { RunnableVariant::from_raw(context) };
    runnable.run();
}
