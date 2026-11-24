//! The graph's scheduling on a plain Rust host: numbers as values,
//! closures as computations.

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    rc::Rc,
};

use super::*;

#[derive(Clone, Copy)]
struct Test;

type Func = Rc<dyn Fn(i64) -> Option<i64>>;

thread_local! {
    static GRAPH: UnsafeCell<Graph<Test>> = UnsafeCell::new(Graph::new(TestData::default()));
}

#[derive(Default)]
struct TestData {
    runaway: bool,
}

// SAFETY: one graph per thread; the module's borrows are short.
unsafe impl Host for Test {
    type Value = i64;
    type Func = Func;
    type Equals = fn(i64, i64) -> bool;
    type Cleanup = Box<dyn FnOnce()>;
    type Context = (u32, i64);
    type Data = TestData;

    unsafe fn graph<'a>(self) -> &'a mut Graph<Self> {
        // SAFETY: the thread's graph lives as long as the thread.
        GRAPH.with(|g| unsafe { &mut *g.get() })
    }
    fn empty() -> i64 {
        0
    }
    fn dup(self, v: &i64) -> i64 {
        *v
    }
    fn free(self, _: i64) {}
    fn dup_func(self, f: &Func) -> Func {
        f.clone()
    }
    fn free_func(self, _: Func) {}
    fn dup_equals(self, e: &Self::Equals) -> Self::Equals {
        *e
    }
    fn free_equals(self, _: Self::Equals) {}
    fn call(self, f: &Func, prev: i64) -> Option<i64> {
        f(prev)
    }
    fn same(self, a: &i64, b: &i64) -> bool {
        a == b
    }
    fn truthy(self, v: &i64) -> bool {
        *v != 0
    }
    fn custom_changed(self, e: &Self::Equals, prev: i64, next: &i64) -> bool {
        !e(prev, *next)
    }
    fn run_cleanup(self, f: Box<dyn FnOnce()>) {
        f();
    }
    fn free_cleanup(self, _: Box<dyn FnOnce()>) {}
    fn runaway(self) {
        // SAFETY: a short borrow.
        unsafe { self.graph() }.data.runaway = true;
    }
}

fn graph() -> &'static mut Graph<Test> {
    // SAFETY: tests take short borrows.
    unsafe { Test.graph() }
}

fn signal(v: i64) -> u32 {
    graph().signal(v, Equals::Default)
}

fn get(id: u32) -> i64 {
    read(Test, id)
}

fn set(id: u32, v: i64) {
    write(Test, id, v);
}

fn func(f: impl Fn(i64) -> i64 + 'static) -> Func {
    Rc::new(move |prev| Some(f(prev)))
}

fn computed(f: impl Fn() -> i64 + 'static) -> u32 {
    memo(Test, func(move |_| f()), 0, Equals::Default, false)
}

fn render_effect(f: impl Fn() + 'static) -> u32 {
    effect(
        Test,
        Kind::Effect,
        func(move |_| {
            f();
            0
        }),
        0,
    )
}

fn user_effect(f: impl Fn() + 'static) -> u32 {
    effect(
        Test,
        Kind::UserEffect,
        func(move |_| {
            f();
            0
        }),
        0,
    )
}

#[test]
fn a_diamond_runs_each_memo_once_per_write() {
    let a = signal(1);
    let runs = Rc::new(Cell::new(0));
    let b = computed(move || get(a) + 1);
    let c = computed(move || get(a) * 2);
    let d = {
        let runs = runs.clone();
        computed(move || {
            runs.set(runs.get() + 1);
            get(b) + get(c)
        })
    };
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = seen.clone();
    render_effect(move || log.borrow_mut().push(get(d)));
    set(a, 2);
    set(a, 3);
    assert_eq!(*seen.borrow(), [4, 7, 10]);
    assert_eq!(runs.get(), 3, "once at first, once per write");
}

#[test]
fn a_batch_runs_effects_once_at_its_end() {
    let a = signal(1);
    let b = signal(1);
    let runs = Rc::new(Cell::new(0));
    let r = runs.clone();
    render_effect(move || {
        get(a);
        get(b);
        r.set(r.get() + 1);
    });
    batch(Test, || {
        set(a, 2);
        set(b, 2);
    });
    assert_eq!(runs.get(), 2);
}

#[test]
fn equal_writes_and_equal_memos_stop_there() {
    let a = signal(1);
    let parity = computed(move || get(a) % 2);
    let runs = Rc::new(Cell::new(0));
    let r = runs.clone();
    render_effect(move || {
        get(parity);
        r.set(r.get() + 1);
    });
    set(a, 1);
    assert_eq!(runs.get(), 1, "an equal write notifies no one");
    set(a, 3);
    assert_eq!(runs.get(), 1, "a memo that stays equal stops the change");
    set(a, 4);
    assert_eq!(runs.get(), 2);
}

#[test]
fn user_effects_run_after_render_effects() {
    let a = signal(0);
    let order = Rc::new(RefCell::new(Vec::new()));
    let o = order.clone();
    user_effect(move || o.borrow_mut().push(("user", get(a))));
    let o = order.clone();
    render_effect(move || o.borrow_mut().push(("render", get(a))));
    order.borrow_mut().clear();
    set(a, 1);
    assert_eq!(*order.borrow(), [("render", 1), ("user", 1)]);
}

#[test]
fn cleanups_run_before_a_rerun_and_on_dispose() {
    let a = signal(0);
    let log = Rc::new(RefCell::new(Vec::new()));
    let l = log.clone();
    let (root, ()) = with_root(Test, || {
        render_effect(move || {
            let v = get(a);
            let l = l.clone();
            let _ = graph().on_cleanup(Box::new(move || l.borrow_mut().push(v)));
        });
    });
    set(a, 1);
    assert_eq!(*log.borrow(), [0]);
    dispose_owner(Test, root);
    assert_eq!(*log.borrow(), [0, 1]);
    set(a, 2);
    assert_eq!(*log.borrow(), [0, 1], "a disposed effect runs no more");
}

#[test]
fn an_owner_disposes_what_it_made_when_it_runs_again() {
    let show = signal(1);
    let inner = signal(0);
    let inner_runs = Rc::new(Cell::new(0));
    let r = inner_runs.clone();
    render_effect(move || {
        if get(show) != 0 {
            let r = r.clone();
            render_effect(move || {
                get(inner);
                r.set(r.get() + 1);
            });
        }
    });
    set(inner, 1);
    assert_eq!(inner_runs.get(), 2);
    set(show, 0);
    set(inner, 2);
    assert_eq!(inner_runs.get(), 2, "the inner effect went with its run");
}

#[test]
fn untracked_reads_make_no_edge() {
    let a = signal(0);
    let runs = Rc::new(Cell::new(0));
    let r = runs.clone();
    render_effect(move || {
        untracked(Test, || get(a));
        r.set(r.get() + 1);
    });
    set(a, 1);
    assert_eq!(runs.get(), 1);
}

#[test]
fn context_passes_to_what_is_made_under_its_owner() {
    let found = Rc::new(Cell::new(None));
    let f = found.clone();
    with_root(Test, || {
        let _ = graph().provide((7, 42));
        let (_, ()) = with_root(Test, || {
            let mut frame = graph().context();
            while let Some(fr) = frame {
                if fr.data.0 == 7 {
                    f.set(Some(fr.data.1));
                    break;
                }
                frame = fr.parent.clone();
            }
        });
    });
    assert_eq!(found.get(), Some(42));
}

#[test]
fn an_effect_that_keeps_writing_what_it_reads_runs_away() {
    let a = signal(0);
    render_effect(move || {
        let v = get(a);
        set(a, v + 1);
    });
    assert!(graph().data.runaway);
}

#[test]
fn rounds_do_not_add_up_to_a_runaway() {
    let a = signal(0);
    render_effect(move || {
        get(a);
    });
    // Each write is a round of its own: together they run the effect more
    // than `MAX_RUNS` times, none of them runs away.
    for v in 1..=MAX_RUNS as i64 + 10 {
        set(a, v);
    }
    assert!(!graph().data.runaway);
}

#[test]
fn handles_free_signals_and_reuse_slots_with_new_generations() {
    let a = signal(5);
    let generation = graph().generation(a);
    if let Some(held) = graph().drop_handle(a) {
        held.free(Test);
    }
    assert!(!graph().live(a, generation, u32::MAX));
    let b = signal(6);
    assert_eq!(b, a, "the slot is reused");
    assert!(graph().live(b, graph().generation(b), u32::MAX));
    assert_ne!(graph().generation(b), generation);
}
