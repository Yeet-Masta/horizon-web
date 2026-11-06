//! Threads, deterministically on one host thread (docs/java-interpreter.md,
//! "Threads"): a started thread runs to its end at the next point where Java
//! would let it run — `join`, `sleep`, `yield`, `wait`, a blocking take, a
//! future's `get`, a latch's `await`, or the end of `main` — nested on the
//! Rust stack of the thread that yielded. A thread that would have to block
//! on one suspended below it ends the run as unsupported instead of hanging.
use super::{A, FINAL, INTERFACE, M, NativeClass, NativeMethod, PUBLIC, S, State, arg, class, ret_bool, ret_ref, ret_str, state, state_mut, this, void};
use crate::natives;
use crate::vm::interp::Vm;
use crate::vm::{Data, Exc, ObjId, Res, Value};
use std::collections::{HashMap, VecDeque};

pub fn classes() -> &'static [NativeClass] {
    CLASSES
}

/// The id HotSpot 25 gives `main`, and the first id a program's own thread gets.
const MAIN_ID: i64 = 3;
const FIRST_ID: i64 = 26;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Phase {
    #[default]
    New,
    Queued,
    Running,
    Terminated,
}

#[derive(Debug, Clone, Default)]
pub struct ThreadRec {
    /// The VM's thread index (monitor owner); 0 is `main`.
    pub index: usize,
    pub id: i64,
    pub name: ObjId,
    pub target: ObjId,
    pub daemon: bool,
    pub priority: i32,
    pub phase: Phase,
    pub interrupted: bool,
    pub handler: ObjId,
    pub virtual_thread: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Pool {
    pub number: i32,
    /// Worker threads made so far, and the most a fixed pool makes (0: no limit).
    pub workers: Vec<ObjId>,
    pub max: usize,
    pub submitted: usize,
    pub shutdown: bool,
    pub virtual_threads: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FutureRec {
    pub task: ObjId,
    /// `Callable.call` (true) or `Runnable.run`.
    pub callable: bool,
    pub phase: Phase,
    pub result: Value,
    pub failure: ObjId,
    pub cancelled: bool,
    pub worker: ObjId,
}

#[derive(Debug, Clone)]
pub enum JThread {
    Thread(ThreadRec),
    Pool(Pool),
    Future(FutureRec),
    /// A `CountDownLatch` or a `Semaphore`: its count.
    Count(i64),
}

impl JThread {
    pub fn refs(&self, f: &mut dyn FnMut(Value)) {
        match self {
            JThread::Thread(t) => {
                f(Value::Ref(t.name));
                f(Value::Ref(t.target));
                f(Value::Ref(t.handler));
            }
            JThread::Pool(p) => p.workers.iter().for_each(|w| f(Value::Ref(*w))),
            JThread::Future(fu) => {
                f(Value::Ref(fu.task));
                f(fu.result);
                f(Value::Ref(fu.failure));
                f(Value::Ref(fu.worker));
            }
            JThread::Count(_) => {}
        }
    }
}

/// The VM's threads.
#[derive(Debug, Default)]
pub struct Threads {
    /// Started threads and submitted tasks not yet run, in order.
    pub queue: VecDeque<ObjId>,
    /// The current thread's `Thread` object (0: `main`, made on first use).
    pub current: ObjId,
    pub main: ObjId,
    pub next_index: usize,
    pub next_id: i64,
    pub next_number: i32,
    pub next_pool: i32,
    pub default_handler: ObjId,
    /// `ThreadLocal` values by thread index and `ThreadLocal`.
    pub locals: HashMap<(usize, ObjId), Value>,
    /// The `Random` of `Math.random()`.
    pub math_random: ObjId,
    /// `Runtime.getRuntime()`, and the hooks `addShutdownHook` registered.
    pub runtime: ObjId,
    pub shutdown_hooks: Vec<ObjId>,
}

impl Threads {
    pub fn refs(&self, out: &mut Vec<ObjId>) {
        out.extend(self.queue.iter().copied());
        out.extend([self.current, self.main, self.default_handler, self.math_random, self.runtime]);
        out.extend(self.shutdown_hooks.iter().copied());
        for ((_, k), v) in &self.locals {
            out.push(*k);
            if let Value::Ref(r) = v {
                out.push(*r);
            }
        }
    }
}

fn rec(vm: &Vm, t: ObjId) -> Option<&ThreadRec> {
    match state(vm, t) {
        Some(State::Thread(j)) => match &**j {
            JThread::Thread(r) => Some(r),
            _ => None,
        },
        _ => None,
    }
}

fn rec_mut(vm: &mut Vm, t: ObjId) -> Option<&mut ThreadRec> {
    match state_mut(vm, t) {
        Some(State::Thread(j)) => match &mut **j {
            JThread::Thread(r) => Some(r),
            _ => None,
        },
        _ => None,
    }
}

fn future_mut(vm: &mut Vm, f: ObjId) -> Option<&mut FutureRec> {
    match state_mut(vm, f) {
        Some(State::Thread(j)) => match &mut **j {
            JThread::Future(r) => Some(r),
            _ => None,
        },
        _ => None,
    }
}

fn future(vm: &Vm, f: ObjId) -> Option<&FutureRec> {
    match state(vm, f) {
        Some(State::Thread(j)) => match &**j {
            JThread::Future(r) => Some(r),
            _ => None,
        },
        _ => None,
    }
}

fn new_thread_object(vm: &mut Vm, class: &str, r: ThreadRec) -> Res<ObjId> {
    let c = vm.load(class)?;
    let t = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Thread(Box::new(JThread::Thread(r)))));
    }
    Ok(t)
}

/// `Thread.currentThread()`.
pub fn current(vm: &mut Vm) -> Res<ObjId> {
    if vm.jdk.threads.current == 0 {
        if vm.jdk.threads.main == 0 {
            let name = vm.intern("main")?;
            let t = new_thread_object(
                vm,
                "java/lang/Thread",
                ThreadRec { index: 0, id: MAIN_ID, name, priority: 5, phase: Phase::Running, ..Default::default() },
            )?;
            vm.pinned.push(t);
            vm.jdk.threads.main = t;
        }
        vm.jdk.threads.current = vm.jdk.threads.main;
    }
    Ok(vm.jdk.threads.current)
}

fn next_id(vm: &mut Vm) -> i64 {
    let t = &mut vm.jdk.threads;
    if t.next_id < FIRST_ID {
        t.next_id = FIRST_ID;
    }
    t.next_id += 1;
    t.next_id - 1
}

fn next_index(vm: &mut Vm) -> usize {
    vm.jdk.threads.next_index += 1;
    vm.jdk.threads.next_index
}

/// Runs a started thread or a submitted task to its end, as its own thread.
fn run_entry(vm: &mut Vm, entry: ObjId) -> Res<()> {
    if future(vm, entry).is_some() {
        return run_task(vm, entry);
    }
    let Some(r) = rec(vm, entry).cloned() else { return Ok(()) };
    if r.phase != Phase::Queued {
        return Ok(());
    }
    if let Some(t) = rec_mut(vm, entry) {
        t.phase = Phase::Running;
    }
    let saved = (vm.current_thread, vm.jdk.threads.current);
    vm.current_thread = r.index;
    vm.jdk.threads.current = entry;
    let result = vm.invoke_virtual(entry, "run", "()V", &[]);
    let result = match result {
        Err(Exc::Throw(ex)) => uncaught(vm, entry, ex),
        other => other.map(|_| ()),
    };
    vm.current_thread = saved.0;
    vm.jdk.threads.current = saved.1;
    if let Some(t) = rec_mut(vm, entry) {
        t.phase = Phase::Terminated;
    }
    // Monitors a dying thread still held are released, as the JVM would not;
    // nothing can wait for them here.
    let index = r.index;
    vm.jdk.monitors.retain(|_, (owner, _)| *owner != index);
    result
}

/// An exception that left a thread's `run`: its handler, the default one, or
/// the trace on stderr.
fn uncaught(vm: &mut Vm, thread: ObjId, ex: ObjId) -> Res<()> {
    let handler = rec(vm, thread).map_or(0, |r| r.handler);
    let handler = if handler != 0 { handler } else { vm.jdk.threads.default_handler };
    if handler != 0 {
        return match vm.invoke_virtual(handler, "uncaughtException", "(Ljava/lang/Thread;Ljava/lang/Throwable;)V", &[Value::Ref(thread), Value::Ref(ex)]) {
            Ok(_) | Err(Exc::Throw(_)) => Ok(()),
            Err(e) => Err(e),
        };
    }
    let name = rec(vm, thread).and_then(|r| vm.string(r.name)).unwrap_or_default();
    let text = vm.throwable_text(ex);
    let trace = super::lang::trace_lines(vm, ex);
    let msg = format!("Exception in thread \"{name}\" {text}\n{trace}");
    vm.stderr.extend_from_slice(msg.as_bytes());
    Ok(())
}

/// Runs every thread and task queued so far (a point where others may run).
pub fn run_pending(vm: &mut Vm) -> Res<()> {
    while let Some(entry) = vm.jdk.threads.queue.pop_front() {
        run_entry(vm, entry)?;
    }
    Ok(())
}

/// The end of `main`: every non-daemon thread still to run (daemon threads
/// end with the VM, as the JVM's do).
pub fn run_remaining(vm: &mut Vm) -> Res<()> {
    while let Some(entry) = vm.jdk.threads.queue.pop_front() {
        if rec(vm, entry).is_some_and(|r| r.daemon) {
            continue;
        }
        run_entry(vm, entry)?;
    }
    Ok(())
}

/// Runs one queued entry now (a `join` or a `get` waiting on it).
fn run_now(vm: &mut Vm, entry: ObjId) -> Res<()> {
    if let Some(i) = vm.jdk.threads.queue.iter().position(|&e| e == entry) {
        vm.jdk.threads.queue.remove(i);
        // Those queued before it start first, as they would on a real JVM.
        let before: Vec<ObjId> = vm.jdk.threads.queue.drain(..i).collect();
        for e in before {
            run_entry(vm, e)?;
        }
        run_entry(vm, entry)?;
    }
    Ok(())
}

fn deadlock(what: &str) -> Exc {
    Exc::Unsupported(format!("{what}: the thread it waits for is suspended below it (threads run to completion; docs/java-interpreter.md)"))
}

/// `monitorenter` on a monitor another thread holds: let the others run; the
/// holder can only be below this thread on the stack, so that cannot free it.
pub fn block_on_monitor(vm: &mut Vm, r: ObjId) -> Res<()> {
    run_pending(vm)?;
    let thread = vm.current_thread;
    let entry = vm.jdk.monitors.entry(r).or_insert((thread, 0));
    if entry.1 == 0 || entry.0 == thread {
        entry.0 = thread;
        entry.1 += 1;
        return Ok(());
    }
    Err(deadlock("monitorenter"))
}

fn owns(vm: &Vm, r: ObjId) -> bool {
    vm.jdk.monitors.get(&r).is_some_and(|(owner, n)| *owner == vm.current_thread && *n > 0)
}

fn not_owner(vm: &mut Vm) -> Exc {
    vm.throw_new("java/lang/IllegalMonitorStateException", Some("current thread is not owner"))
}

/// `Object.wait`: releases the monitor, lets the other threads run (one of
/// them may notify), takes the monitor back. A wait nothing can end returns
/// as a spurious wakeup would.
pub fn object_wait(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    if !owns(vm, t) {
        return Err(not_owner(vm));
    }
    let millis = match a.get(1) {
        Some(Value::Long(ms)) => *ms,
        _ => 0,
    };
    if millis < 0 {
        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("timeout value is negative")));
    }
    take_interrupt(vm)?;
    let held = vm.jdk.monitors.remove(&t).unwrap_or((vm.current_thread, 1));
    let result = run_pending(vm);
    if millis > 0 {
        vm.jdk.nanos += millis as u64 * 1_000_000;
    }
    vm.jdk.monitors.insert(t, held);
    result?;
    void()
}

pub fn object_notify(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    if !owns(vm, t) {
        return Err(not_owner(vm));
    }
    void()
}

pub fn object_notify_all(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    object_notify(vm, a)
}

/// Throws `InterruptedException` (and clears the flag) when the current thread
/// was interrupted.
fn take_interrupt(vm: &mut Vm) -> Res<()> {
    let t = current(vm)?;
    if rec(vm, t).is_some_and(|r| r.interrupted) {
        if let Some(r) = rec_mut(vm, t) {
            r.interrupted = false;
        }
        return Err(vm.throw_new("java/lang/InterruptedException", None));
    }
    Ok(())
}

fn sleep(vm: &mut Vm, nanos: i64) -> Res<Value> {
    if nanos < 0 {
        return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("timeout value is negative")));
    }
    let t = current(vm)?;
    if rec(vm, t).is_some_and(|r| r.interrupted) {
        if let Some(r) = rec_mut(vm, t) {
            r.interrupted = false;
        }
        return Err(vm.throw_new("java/lang/InterruptedException", Some("sleep interrupted")));
    }
    run_pending(vm)?;
    vm.jdk.nanos += nanos as u64;
    void()
}

// ------------------------------------------------------------------ Thread

fn thread_init(vm: &mut Vm, a: &[Value]) -> Res<Value> {
    let t = this(vm, a)?;
    let string = vm.known.string;
    let mut target = 0;
    let mut name = 0;
    for v in &a[1..] {
        if let Value::Ref(r) = v
            && *r != 0
        {
            if vm.instance_of(*r, string) {
                name = *r;
            } else if vm.class_of(*r).is_some_and(|c| vm.class_name(c) != "java/lang/ThreadGroup") {
                target = *r;
            }
        }
    }
    if name == 0 {
        let n = vm.jdk.threads.next_number;
        vm.jdk.threads.next_number += 1;
        name = vm.new_string(&format!("Thread-{n}"))?;
    }
    let parent = current(vm)?;
    let (daemon, priority) = rec(vm, parent).map_or((false, 5), |p| (p.daemon, p.priority));
    let id = next_id(vm);
    let index = next_index(vm);
    if let Some(o) = vm.obj_mut(t) {
        o.data = Data::Jdk(Box::new(State::Thread(Box::new(JThread::Thread(ThreadRec {
            index,
            id,
            name,
            target,
            daemon,
            priority,
            phase: Phase::New,
            ..Default::default()
        })))));
    }
    void()
}

fn thread_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let Some(r) = rec(vm, t).cloned() else { return Err(Exc::Internal("not a thread".into())) };
    match op {
        "start" => {
            if r.phase != Phase::New {
                return Err(vm.throw_new("java/lang/IllegalThreadStateException", None));
            }
            if let Some(x) = rec_mut(vm, t) {
                x.phase = Phase::Queued;
            }
            vm.jdk.threads.queue.push_back(t);
            void()
        }
        "run" => {
            if r.target != 0 {
                vm.invoke_virtual(r.target, "run", "()V", &[])?;
            }
            void()
        }
        "join" => {
            match r.phase {
                Phase::Queued => run_now(vm, t)?,
                Phase::Running if t != current(vm)? => {
                    if a.len() < 2 {
                        return Err(deadlock("Thread.join"));
                    }
                }
                _ => {}
            }
            if let Some(Value::Long(ms)) = a.get(1) {
                vm.jdk.nanos += (*ms).max(0) as u64 * 1_000_000;
            }
            void()
        }
        "joinDuration" => {
            if r.phase == Phase::Queued {
                run_now(vm, t)?;
            }
            ret_bool(rec(vm, t).is_some_and(|x| x.phase == Phase::Terminated))
        }
        "getName" => ret_ref(r.name),
        "setName" => {
            let n = arg(a, 1).r();
            if n == 0 {
                return Err(vm.npe(Some("'name' is null")));
            }
            if let Some(x) = rec_mut(vm, t) {
                x.name = n;
            }
            void()
        }
        "getId" | "threadId" => Ok(Value::Long(r.id)),
        "isAlive" => ret_bool(matches!(r.phase, Phase::Queued | Phase::Running)),
        "isDaemon" => ret_bool(r.daemon),
        "setDaemon" => {
            if r.phase != Phase::New {
                return Err(vm.throw_new("java/lang/IllegalThreadStateException", None));
            }
            if let Some(x) = rec_mut(vm, t) {
                x.daemon = arg(a, 1).z();
            }
            void()
        }
        "getPriority" => Ok(Value::Int(r.priority)),
        "setPriority" => {
            let p = arg(a, 1).i();
            if !(1..=10).contains(&p) {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", None));
            }
            if let Some(x) = rec_mut(vm, t) {
                x.priority = p;
            }
            void()
        }
        "interrupt" => {
            if let Some(x) = rec_mut(vm, t) {
                x.interrupted = true;
            }
            void()
        }
        "isInterrupted" => ret_bool(r.interrupted),
        "isVirtual" => ret_bool(r.virtual_thread),
        "setUncaughtExceptionHandler" => {
            if let Some(x) = rec_mut(vm, t) {
                x.handler = arg(a, 1).r();
            }
            void()
        }
        "getUncaughtExceptionHandler" => ret_ref(r.handler),
        "getState" => {
            let name = match r.phase {
                Phase::New => "NEW",
                Phase::Queued | Phase::Running => "RUNNABLE",
                Phase::Terminated => "TERMINATED",
            };
            vm.get_static("java/lang/Thread$State", name, "Ljava/lang/Thread$State;")
        }
        "toString" => {
            let name = vm.string(r.name).unwrap_or_default();
            if r.virtual_thread {
                return ret_str(vm, &format!("VirtualThread[#{}{}]/runnable", r.id, if name.is_empty() { String::new() } else { format!(",{name}") }));
            }
            let group = if r.phase == Phase::Terminated { "" } else { "main" };
            ret_str(vm, &format!("Thread[#{},{name},{},{group}]", r.id, r.priority))
        }
        "getStackTrace" => {
            let c = vm.load("java/lang/StackTraceElement")?;
            let arr_class = vm.array_class_of(c)?;
            ret_ref(vm.new_array(arr_class, 0)?)
        }
        "checkAccess" => void(),
        _ => Err(Exc::Unsupported(format!("Thread.{op}"))),
    }
}

fn thread_static(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "currentThread" => ret_ref(current(vm)?),
        "sleep" => sleep(vm, arg(a, 0).j().saturating_mul(1_000_000)),
        "sleepNanos" => sleep(vm, arg(a, 0).j().saturating_mul(1_000_000) + arg(a, 1).i() as i64),
        "sleepDuration" => {
            let nanos = vm.invoke_virtual(arg(a, 0).r(), "toNanos", "()J", &[])?.j();
            sleep(vm, nanos)
        }
        "yield" | "onSpinWait" => {
            run_pending(vm)?;
            void()
        }
        "interrupted" => {
            let t = current(vm)?;
            let was = rec(vm, t).is_some_and(|r| r.interrupted);
            if let Some(r) = rec_mut(vm, t) {
                r.interrupted = false;
            }
            ret_bool(was)
        }
        "setDefaultUncaughtExceptionHandler" => {
            vm.jdk.threads.default_handler = arg(a, 0).r();
            void()
        }
        "getDefaultUncaughtExceptionHandler" => ret_ref(vm.jdk.threads.default_handler),
        "activeCount" => Ok(Value::Int(1 + vm.jdk.threads.queue.len() as i32)),
        "holdsLock" => ret_bool(owns(vm, arg(a, 0).r())),
        "startVirtualThread" => {
            let t = new_virtual(vm, arg(a, 0).r())?;
            thread_op(vm, &[Value::Ref(t)], "start")?;
            ret_ref(t)
        }
        _ => Err(Exc::Unsupported(format!("Thread.{op}"))),
    }
}

fn new_virtual(vm: &mut Vm, task: ObjId) -> Res<ObjId> {
    let name = vm.intern("")?;
    let id = next_id(vm);
    let index = next_index(vm);
    new_thread_object(vm, "java/lang/Thread", ThreadRec { index, id, name, target: task, daemon: true, priority: 5, virtual_thread: true, ..Default::default() })
}

// ------------------------------------------------------------------ ThreadLocal

fn thread_local_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let key = (vm.current_thread, t);
    match op {
        "get" => {
            if let Some(v) = vm.jdk.threads.locals.get(&key) {
                return Ok(*v);
            }
            let v = vm.invoke_virtual(t, "initialValue", "()Ljava/lang/Object;", &[])?;
            vm.jdk.threads.locals.insert(key, v);
            Ok(v)
        }
        "set" => {
            vm.jdk.threads.locals.insert(key, arg(a, 1));
            void()
        }
        "remove" => {
            vm.jdk.threads.locals.remove(&key);
            void()
        }
        "initialValue" => {
            // ThreadLocal.withInitial keeps its supplier as the native state.
            match state(vm, t) {
                Some(State::Generic(v)) if !v.is_empty() && v[0].r() != 0 => vm.invoke_virtual(v[0].r(), "get", "()Ljava/lang/Object;", &[]),
                _ => ret_ref(0),
            }
        }
        _ => Err(Exc::Unsupported(format!("ThreadLocal.{op}"))),
    }
}

// ------------------------------------------------------------------ executors

fn new_pool(vm: &mut Vm, max: usize, virtual_threads: bool) -> Res<Value> {
    vm.jdk.threads.next_pool += 1;
    let number = vm.jdk.threads.next_pool;
    let c = vm.load("java/util/concurrent/ThreadPoolExecutor")?;
    let r = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Thread(Box::new(JThread::Pool(Pool { number, max, virtual_threads, ..Default::default() })))));
    }
    ret_ref(r)
}

fn pool(vm: &Vm, p: ObjId) -> Option<Pool> {
    match state(vm, p) {
        Some(State::Thread(j)) => match &**j {
            JThread::Pool(p) => Some(p.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn set_pool(vm: &mut Vm, p: ObjId, value: Pool) {
    if let Some(State::Thread(j)) = state_mut(vm, p) {
        **j = JThread::Pool(value);
    }
}

/// Queues a task on a pool's next worker and answers its `Future`.
fn submit(vm: &mut Vm, p: ObjId, task: ObjId, callable: bool) -> Res<ObjId> {
    if task == 0 {
        return Err(vm.npe(None));
    }
    let Some(mut pl) = pool(vm, p) else { return Err(Exc::Internal("not a pool".into())) };
    if pl.shutdown {
        let msg = format!("Task rejected from {}", pool_text(vm, &pl));
        return Err(vm.throw_new("java/util/concurrent/RejectedExecutionException", Some(&msg)));
    }
    let k = pl.submitted;
    pl.submitted += 1;
    let worker = if pl.virtual_threads {
        new_virtual(vm, 0)?
    } else if pl.max == 0 || pl.workers.len() < pl.max {
        let name = vm.new_string(&format!("pool-{}-thread-{}", pl.number, pl.workers.len() + 1))?;
        let id = next_id(vm);
        let index = next_index(vm);
        let w = new_thread_object(vm, "java/lang/Thread", ThreadRec { index, id, name, priority: 5, phase: Phase::Running, ..Default::default() })?;
        pl.workers.push(w);
        w
    } else {
        pl.workers[k % pl.max]
    };
    set_pool(vm, p, pl);
    let c = vm.load("java/util/concurrent/FutureTask")?;
    let f = vm.new_instance(c)?;
    if let Some(o) = vm.obj_mut(f) {
        o.data = Data::Jdk(Box::new(State::Thread(Box::new(JThread::Future(FutureRec { task, callable, phase: Phase::Queued, worker, ..Default::default() })))));
    }
    vm.jdk.threads.queue.push_back(f);
    Ok(f)
}

fn pool_text(vm: &Vm, p: &Pool) -> String {
    let _ = vm;
    let state = if p.shutdown { "Terminated" } else { "Running" };
    format!("java.util.concurrent.ThreadPoolExecutor[{state}, pool size = {}, active threads = 0, queued tasks = 0, completed tasks = {}]", p.workers.len(), p.submitted)
}

fn run_task(vm: &mut Vm, f: ObjId) -> Res<()> {
    let Some(fu) = future(vm, f).cloned() else { return Ok(()) };
    if fu.phase != Phase::Queued || fu.cancelled {
        return Ok(());
    }
    if let Some(x) = future_mut(vm, f) {
        x.phase = Phase::Running;
    }
    let saved = (vm.current_thread, vm.jdk.threads.current);
    if fu.worker != 0 {
        vm.current_thread = rec(vm, fu.worker).map_or(saved.0, |r| r.index);
        vm.jdk.threads.current = fu.worker;
    }
    let result = if fu.callable {
        vm.invoke_virtual(fu.task, "call", "()Ljava/lang/Object;", &[])
    } else {
        vm.invoke_virtual(fu.task, "run", "()V", &[]).map(|_| Value::NULL)
    };
    vm.current_thread = saved.0;
    vm.jdk.threads.current = saved.1;
    let (value, failure) = match result {
        Ok(v) => (v, 0),
        Err(Exc::Throw(ex)) => (Value::NULL, ex),
        Err(e) => return Err(e),
    };
    if let Some(x) = future_mut(vm, f) {
        x.phase = Phase::Terminated;
        x.result = value;
        x.failure = failure;
    }
    Ok(())
}

fn future_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let f = this(vm, a)?;
    let Some(fu) = future(vm, f).cloned() else { return Err(Exc::Internal("not a future".into())) };
    match op {
        "get" | "join" | "resultNow" => {
            if fu.cancelled {
                return Err(vm.throw_new("java/util/concurrent/CancellationException", None));
            }
            match fu.phase {
                Phase::Queued => run_now(vm, f)?,
                Phase::Running => return Err(deadlock("Future.get")),
                _ => {}
            }
            let Some(done) = future(vm, f).cloned() else { return ret_ref(0) };
            if done.failure != 0 {
                let class = if op == "join" { "java/util/concurrent/CompletionException" } else { "java/util/concurrent/ExecutionException" };
                let text = vm.throwable_text(done.failure);
                let ex = vm.make_throwable(class, Some(&text))?;
                if let Some(Data::Throwable(t)) = vm.obj_mut(ex).map(|o| &mut o.data) {
                    t.cause = Some(done.failure);
                }
                return Err(Exc::Throw(ex));
            }
            Ok(done.result)
        }
        "isDone" => ret_bool(fu.phase == Phase::Terminated || fu.cancelled),
        "isCancelled" => ret_bool(fu.cancelled),
        "cancel" => {
            let can = fu.phase == Phase::Queued && !fu.cancelled;
            if can && let Some(x) = future_mut(vm, f) {
                x.cancelled = true;
            }
            ret_bool(can)
        }
        "run" => {
            run_now(vm, f)?;
            void()
        }
        _ => Err(Exc::Unsupported(format!("Future.{op}"))),
    }
}

fn pool_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let p = this(vm, a)?;
    let Some(pl) = pool(vm, p) else { return Err(Exc::Internal("not a pool".into())) };
    match op {
        "submitRunnable" => ret_ref(submit(vm, p, arg(a, 1).r(), false)?),
        "submitCallable" => ret_ref(submit(vm, p, arg(a, 1).r(), true)?),
        "execute" => {
            submit(vm, p, arg(a, 1).r(), false)?;
            void()
        }
        "shutdown" => {
            set_pool(vm, p, Pool { shutdown: true, ..pl });
            void()
        }
        "shutdownNow" => {
            set_pool(vm, p, Pool { shutdown: true, ..pl });
            ret_ref(super::util::new_list(vm, Vec::new())?)
        }
        "awaitTermination" | "close" => {
            if op == "close" {
                set_pool(vm, p, Pool { shutdown: true, ..pl });
            }
            run_pending(vm)?;
            if op == "close" { void() } else { ret_bool(true) }
        }
        "isShutdown" | "isTerminated" => ret_bool(pl.shutdown),
        "invokeAll" => {
            let tasks = super::util::iterate(vm, arg(a, 1).r())?;
            let mut futures = Vec::new();
            for t in tasks {
                futures.push(Value::Ref(submit(vm, p, t.r(), true)?));
            }
            for f in &futures {
                run_now(vm, f.r())?;
            }
            ret_ref(super::util::new_list(vm, futures)?)
        }
        "invokeAny" => {
            let tasks = super::util::iterate(vm, arg(a, 1).r())?;
            let Some(first) = tasks.first() else { return Err(vm.throw_new("java/lang/IllegalArgumentException", None)) };
            let f = submit(vm, p, first.r(), true)?;
            future_op(vm, &[Value::Ref(f)], "get")
        }
        "toString" => ret_str(vm, &pool_text(vm, &pl)),
        _ => Err(Exc::Unsupported(format!("ExecutorService.{op}"))),
    }
}

// ------------------------------------------------------------------ latches, locks

fn count_of(vm: &Vm, r: ObjId) -> i64 {
    match state(vm, r) {
        Some(State::Thread(j)) => match &**j {
            JThread::Count(n) => *n,
            _ => 0,
        },
        _ => 0,
    }
}

fn set_count(vm: &mut Vm, r: ObjId, n: i64) {
    if let Some(o) = vm.obj_mut(r) {
        o.data = Data::Jdk(Box::new(State::Thread(Box::new(JThread::Count(n)))));
    }
}

fn latch_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "init" => {
            let n = arg(a, 1).i();
            if n < 0 {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", Some("count < 0")));
            }
            set_count(vm, t, n as i64);
            void()
        }
        "countDown" => {
            let n = count_of(vm, t);
            if n > 0 {
                set_count(vm, t, n - 1);
            }
            void()
        }
        "getCount" => Ok(Value::Long(count_of(vm, t))),
        "await" | "awaitTimed" => {
            take_interrupt(vm)?;
            if count_of(vm, t) > 0 {
                run_pending(vm)?;
            }
            let done = count_of(vm, t) == 0;
            if op == "awaitTimed" {
                return ret_bool(done);
            }
            if !done {
                return Err(deadlock("CountDownLatch.await"));
            }
            void()
        }
        "toString" => {
            let n = count_of(vm, t);
            let base = super::lang::default_to_string(vm, t)?;
            ret_str(vm, &format!("{base}[Count = {n}]"))
        }
        _ => Err(Exc::Unsupported(format!("CountDownLatch.{op}"))),
    }
}

fn semaphore_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let n = if a.len() > 1 && op != "init" { arg(a, 1).i().max(1) as i64 } else { 1 };
    match op {
        "init" => {
            set_count(vm, t, arg(a, 1).i() as i64);
            void()
        }
        "acquire" | "tryAcquire" => {
            if count_of(vm, t) < n && op == "acquire" {
                run_pending(vm)?;
            }
            let have = count_of(vm, t);
            if have >= n {
                set_count(vm, t, have - n);
                return if op == "acquire" { void() } else { ret_bool(true) };
            }
            if op == "tryAcquire" { ret_bool(false) } else { Err(deadlock("Semaphore.acquire")) }
        }
        "release" => {
            let have = count_of(vm, t);
            set_count(vm, t, have + n);
            void()
        }
        "availablePermits" => Ok(Value::Int(count_of(vm, t) as i32)),
        _ => Err(Exc::Unsupported(format!("Semaphore.{op}"))),
    }
}

/// `ReentrantLock`: the lock object's own monitor.
fn lock_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "init" => void(),
        "lock" | "lockInterruptibly" => {
            vm.monitor_enter(t)?;
            void()
        }
        "tryLock" => {
            let thread = vm.current_thread;
            let free = vm.jdk.monitors.get(&t).is_none_or(|(owner, n)| *n == 0 || *owner == thread);
            if free {
                vm.monitor_enter(t)?;
            }
            ret_bool(free)
        }
        "unlock" => {
            if !owns(vm, t) {
                return Err(vm.throw_new("java/lang/IllegalMonitorStateException", None));
            }
            vm.monitor_exit(t)?;
            void()
        }
        "isLocked" => ret_bool(vm.jdk.monitors.get(&t).is_some_and(|(_, n)| *n > 0)),
        "isHeldByCurrentThread" => ret_bool(owns(vm, t)),
        "getHoldCount" => Ok(Value::Int(if owns(vm, t) { vm.jdk.monitors[&t].1 as i32 } else { 0 })),
        "newCondition" => {
            let c = vm.load("java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject")?;
            let r = vm.new_instance(c)?;
            if let Some(o) = vm.obj_mut(r) {
                o.data = Data::Jdk(Box::new(State::Generic(vec![Value::Ref(t)])));
            }
            ret_ref(r)
        }
        _ => Err(Exc::Unsupported(format!("ReentrantLock.{op}"))),
    }
}

fn condition_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let lock = match state(vm, t) {
        Some(State::Generic(v)) => v[0].r(),
        _ => 0,
    };
    match op {
        "await" => object_wait(vm, &[Value::Ref(lock)]),
        "awaitTimed" => {
            object_wait(vm, &[Value::Ref(lock)])?;
            ret_bool(true)
        }
        _ => object_notify(vm, &[Value::Ref(lock)]),
    }
}

// ------------------------------------------------------------------ blocking queues

/// `take`/`put` of a blocking queue: an empty queue lets the producers run first.
fn blocking_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    match op {
        "put" => {
            vm.invoke_virtual(t, "add", "(Ljava/lang/Object;)Z", &[arg(a, 1)])?;
            void()
        }
        "take" | "pollTimed" => {
            let empty = |vm: &mut Vm| -> Res<bool> { Ok(vm.invoke_virtual(t, "isEmpty", "()Z", &[])?.z()) };
            if empty(vm)? {
                take_interrupt(vm)?;
                run_pending(vm)?;
            }
            if empty(vm)? {
                if op == "pollTimed" {
                    return ret_ref(0);
                }
                return Err(deadlock("BlockingQueue.take"));
            }
            vm.invoke_virtual(t, "poll", "()Ljava/lang/Object;", &[])
        }
        "offerTimed" => vm.invoke_virtual(t, "offer", "(Ljava/lang/Object;)Z", &[arg(a, 1)]),
        "remainingCapacity" => Ok(Value::Int(i32::MAX)),
        "drainTo" => {
            let mut n = 0;
            while !vm.invoke_virtual(t, "isEmpty", "()Z", &[])?.z() {
                let v = vm.invoke_virtual(t, "poll", "()Ljava/lang/Object;", &[])?;
                vm.invoke_virtual(arg(a, 1).r(), "add", "(Ljava/lang/Object;)Z", &[v])?;
                n += 1;
            }
            Ok(Value::Int(n))
        }
        _ => Err(Exc::Unsupported(format!("BlockingQueue.{op}"))),
    }
}

// ------------------------------------------------------------------ TimeUnit

const UNIT_NAMES: &[&str] = &["NANOSECONDS", "MICROSECONDS", "MILLISECONDS", "SECONDS", "MINUTES", "HOURS", "DAYS"];
const UNIT_NANOS: [i64; 7] = [1, 1_000, 1_000_000, 1_000_000_000, 60_000_000_000, 3_600_000_000_000, 86_400_000_000_000];

fn unit_scale(vm: &Vm, unit: ObjId) -> i64 {
    UNIT_NANOS[super::lang::enum_parts(vm, unit).1.clamp(0, 6) as usize]
}

/// `d` of one unit in another, saturating as `TimeUnit.convert` does.
fn convert(d: i64, from: i64, to: i64) -> i64 {
    if from == to {
        d
    } else if from > to {
        d.saturating_mul(from / to)
    } else {
        d / (to / from)
    }
}

fn unit_op(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    let t = this(vm, a)?;
    let scale = unit_scale(vm, t);
    let d = arg(a, 1).j();
    let to = match op {
        "toNanos" => 1,
        "toMicros" => 1_000,
        "toMillis" => 1_000_000,
        "toSeconds" => 1_000_000_000,
        "toMinutes" => 60_000_000_000,
        "toHours" => 3_600_000_000_000,
        "toDays" => 86_400_000_000_000,
        "sleep" => return sleep(vm, convert(d, scale, 1)),
        "convert" => {
            let from = unit_scale(vm, arg(a, 2).r());
            return Ok(Value::Long(convert(d, from, scale)));
        }
        _ => return Err(Exc::Unsupported(format!("TimeUnit.{op}"))),
    };
    Ok(Value::Long(convert(d, scale, to)))
}

// ------------------------------------------------------------------ classes

static THREAD_METHODS: &[NativeMethod] = natives![
    "<init>" "()V" M => Some(thread_init),
    "<init>" "(Ljava/lang/Runnable;)V" M => Some(thread_init),
    "<init>" "(Ljava/lang/String;)V" M => Some(thread_init),
    "<init>" "(Ljava/lang/Runnable;Ljava/lang/String;)V" M => Some(thread_init),
    "<init>" "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;)V" M => Some(thread_init),
    "<init>" "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V" M => Some(thread_init),
    "<init>" "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V" M => Some(thread_init),
    "<init>" "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;J)V" M => Some(thread_init),
    "start" "()V" M => Some(|vm, a| thread_op(vm, a, "start")),
    "run" "()V" M => Some(|vm, a| thread_op(vm, a, "run")),
    "join" "()V" M | FINAL => Some(|vm, a| thread_op(vm, a, "join")),
    "join" "(J)V" M | FINAL => Some(|vm, a| thread_op(vm, a, "join")),
    "join" "(Ljava/time/Duration;)Z" M | FINAL => Some(|vm, a| thread_op(vm, a, "joinDuration")),
    "getName" "()Ljava/lang/String;" M | FINAL => Some(|vm, a| thread_op(vm, a, "getName")),
    "setName" "(Ljava/lang/String;)V" M | FINAL => Some(|vm, a| thread_op(vm, a, "setName")),
    "getId" "()J" M => Some(|vm, a| thread_op(vm, a, "getId")),
    "threadId" "()J" M | FINAL => Some(|vm, a| thread_op(vm, a, "threadId")),
    "isAlive" "()Z" M | FINAL => Some(|vm, a| thread_op(vm, a, "isAlive")),
    "isDaemon" "()Z" M | FINAL => Some(|vm, a| thread_op(vm, a, "isDaemon")),
    "setDaemon" "(Z)V" M | FINAL => Some(|vm, a| thread_op(vm, a, "setDaemon")),
    "getPriority" "()I" M | FINAL => Some(|vm, a| thread_op(vm, a, "getPriority")),
    "setPriority" "(I)V" M | FINAL => Some(|vm, a| thread_op(vm, a, "setPriority")),
    "interrupt" "()V" M => Some(|vm, a| thread_op(vm, a, "interrupt")),
    "isInterrupted" "()Z" M => Some(|vm, a| thread_op(vm, a, "isInterrupted")),
    "isVirtual" "()Z" M | FINAL => Some(|vm, a| thread_op(vm, a, "isVirtual")),
    "setUncaughtExceptionHandler" "(Ljava/lang/Thread$UncaughtExceptionHandler;)V" M => Some(|vm, a| thread_op(vm, a, "setUncaughtExceptionHandler")),
    "getUncaughtExceptionHandler" "()Ljava/lang/Thread$UncaughtExceptionHandler;" M => Some(|vm, a| thread_op(vm, a, "getUncaughtExceptionHandler")),
    "getState" "()Ljava/lang/Thread$State;" M => Some(|vm, a| thread_op(vm, a, "getState")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| thread_op(vm, a, "toString")),
    "getStackTrace" "()[Ljava/lang/StackTraceElement;" M => Some(|vm, a| thread_op(vm, a, "getStackTrace")),
    "checkAccess" "()V" M | FINAL => Some(|vm, a| thread_op(vm, a, "checkAccess")),
    "currentThread" "()Ljava/lang/Thread;" S => Some(|vm, a| thread_static(vm, a, "currentThread")),
    "sleep" "(J)V" S => Some(|vm, a| thread_static(vm, a, "sleep")),
    "sleep" "(JI)V" S => Some(|vm, a| thread_static(vm, a, "sleepNanos")),
    "sleep" "(Ljava/time/Duration;)V" S => Some(|vm, a| thread_static(vm, a, "sleepDuration")),
    "yield" "()V" S => Some(|vm, a| thread_static(vm, a, "yield")),
    "onSpinWait" "()V" S => Some(|vm, a| thread_static(vm, a, "onSpinWait")),
    "interrupted" "()Z" S => Some(|vm, a| thread_static(vm, a, "interrupted")),
    "setDefaultUncaughtExceptionHandler" "(Ljava/lang/Thread$UncaughtExceptionHandler;)V" S => Some(|vm, a| thread_static(vm, a, "setDefaultUncaughtExceptionHandler")),
    "getDefaultUncaughtExceptionHandler" "()Ljava/lang/Thread$UncaughtExceptionHandler;" S => Some(|vm, a| thread_static(vm, a, "getDefaultUncaughtExceptionHandler")),
    "activeCount" "()I" S => Some(|vm, a| thread_static(vm, a, "activeCount")),
    "holdsLock" "(Ljava/lang/Object;)Z" S => Some(|vm, a| thread_static(vm, a, "holdsLock")),
    "startVirtualThread" "(Ljava/lang/Runnable;)Ljava/lang/Thread;" S => Some(|vm, a| thread_static(vm, a, "startVirtualThread")),
];

static POOL_METHODS: &[NativeMethod] = natives![
    "submit" "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;" M => Some(|vm, a| pool_op(vm, a, "submitRunnable")),
    "submit" "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;" M => Some(|vm, a| pool_op(vm, a, "submitCallable")),
    "execute" "(Ljava/lang/Runnable;)V" M => Some(|vm, a| pool_op(vm, a, "execute")),
    "shutdown" "()V" M => Some(|vm, a| pool_op(vm, a, "shutdown")),
    "shutdownNow" "()Ljava/util/List;" M => Some(|vm, a| pool_op(vm, a, "shutdownNow")),
    "awaitTermination" "(JLjava/util/concurrent/TimeUnit;)Z" M => Some(|vm, a| pool_op(vm, a, "awaitTermination")),
    "close" "()V" M => Some(|vm, a| pool_op(vm, a, "close")),
    "isShutdown" "()Z" M => Some(|vm, a| pool_op(vm, a, "isShutdown")),
    "isTerminated" "()Z" M => Some(|vm, a| pool_op(vm, a, "isTerminated")),
    "invokeAll" "(Ljava/util/Collection;)Ljava/util/List;" M => Some(|vm, a| pool_op(vm, a, "invokeAll")),
    "invokeAny" "(Ljava/util/Collection;)Ljava/lang/Object;" M => Some(|vm, a| pool_op(vm, a, "invokeAny")),
    "toString" "()Ljava/lang/String;" M => Some(|vm, a| pool_op(vm, a, "toString")),
];

static FUTURE_METHODS: &[NativeMethod] = natives![
    "get" "()Ljava/lang/Object;" M => Some(|vm, a| future_op(vm, a, "get")),
    "get" "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;" M => Some(|vm, a| future_op(vm, a, "get")),
    "resultNow" "()Ljava/lang/Object;" M => Some(|vm, a| future_op(vm, a, "resultNow")),
    "isDone" "()Z" M => Some(|vm, a| future_op(vm, a, "isDone")),
    "isCancelled" "()Z" M => Some(|vm, a| future_op(vm, a, "isCancelled")),
    "cancel" "(Z)Z" M => Some(|vm, a| future_op(vm, a, "cancel")),
    "run" "()V" M => Some(|vm, a| future_op(vm, a, "run")),
];

fn executors(vm: &mut Vm, a: &[Value], op: &str) -> Res<Value> {
    match op {
        "fixed" => {
            let n = arg(a, 0).i();
            if n <= 0 {
                return Err(vm.throw_new("java/lang/IllegalArgumentException", None));
            }
            new_pool(vm, n as usize, false)
        }
        "single" => new_pool(vm, 1, false),
        "cached" => new_pool(vm, 0, false),
        "virtual" => {
            let p = new_pool(vm, 0, true)?;
            // Virtual-thread executors number no pool.
            vm.jdk.threads.next_pool -= 1;
            Ok(p)
        }
        _ => Err(Exc::Unsupported(format!("Executors.{op}"))),
    }
}

static CLASSES: &[NativeClass] = &[
    NativeClass {
        name: "java/lang/Thread",
        super_name: Some("java/lang/Object"),
        interfaces: &["java/lang/Runnable"],
        access: PUBLIC,
        methods: THREAD_METHODS,
        statics: &[("MIN_PRIORITY", "I"), ("NORM_PRIORITY", "I"), ("MAX_PRIORITY", "I")],
        clinit: Some(|vm, _| {
            let c = vm.load("java/lang/Thread")?;
            vm.set_static(c, "MIN_PRIORITY", Value::Int(1));
            vm.set_static(c, "NORM_PRIORITY", Value::Int(5));
            vm.set_static(c, "MAX_PRIORITY", Value::Int(10));
            void()
        }),
        data: Some(|| Data::Jdk(Box::new(State::Thread(Box::new(JThread::Thread(ThreadRec::default())))))),
    },
    NativeClass {
        name: "java/lang/Thread$State",
        super_name: Some("java/lang/Enum"),
        interfaces: &[],
        access: PUBLIC | FINAL | 0x4000,
        methods: natives!["values" "()[Ljava/lang/Thread$State;" S => Some(|vm, _| super::math::enum_values(vm, "java/lang/Thread$State"))],
        statics: &[
            ("NEW", "Ljava/lang/Thread$State;"),
            ("RUNNABLE", "Ljava/lang/Thread$State;"),
            ("BLOCKED", "Ljava/lang/Thread$State;"),
            ("WAITING", "Ljava/lang/Thread$State;"),
            ("TIMED_WAITING", "Ljava/lang/Thread$State;"),
            ("TERMINATED", "Ljava/lang/Thread$State;"),
            ("$VALUES", "[Ljava/lang/Thread$State;"),
        ],
        clinit: Some(|vm, _| super::math::make_enum_constants(vm, "java/lang/Thread$State", &["NEW", "RUNNABLE", "BLOCKED", "WAITING", "TIMED_WAITING", "TERMINATED"])),
        data: None,
    },
    class("java/lang/Thread$UncaughtExceptionHandler", None, &[], INTERFACE, natives!["uncaughtException" "(Ljava/lang/Thread;Ljava/lang/Throwable;)V" A => None]),
    class("java/lang/ThreadGroup", Some("java/lang/Object"), &[], PUBLIC, natives![]),
    NativeClass {
        name: "java/lang/ThreadLocal",
        super_name: Some("java/lang/Object"),
        interfaces: &[],
        access: PUBLIC,
        methods: natives![
            "<init>" "()V" M => Some(|_, _| void()),
            "get" "()Ljava/lang/Object;" M => Some(|vm, a| thread_local_op(vm, a, "get")),
            "set" "(Ljava/lang/Object;)V" M => Some(|vm, a| thread_local_op(vm, a, "set")),
            "remove" "()V" M => Some(|vm, a| thread_local_op(vm, a, "remove")),
            "initialValue" "()Ljava/lang/Object;" 0x0004 => Some(|vm, a| thread_local_op(vm, a, "initialValue")),
            "withInitial" "(Ljava/util/function/Supplier;)Ljava/lang/ThreadLocal;" S => Some(|vm, a| { let c = vm.load("java/lang/ThreadLocal")?; let r = vm.new_instance(c)?; if let Some(o) = vm.obj_mut(r) { o.data = Data::Jdk(Box::new(State::Generic(vec![arg(a, 0)]))); } ret_ref(r) }),
        ],
        statics: &[],
        clinit: None,
        data: Some(|| Data::Jdk(Box::new(State::Generic(vec![Value::NULL])))),
    },
    class("java/lang/InheritableThreadLocal", Some("java/lang/ThreadLocal"), &[], PUBLIC, natives!["<init>" "()V" M => Some(|_, _| void())]),
    class("java/util/concurrent/Executor", None, &[], INTERFACE, natives!["execute" "(Ljava/lang/Runnable;)V" A => None]),
    class("java/util/concurrent/ExecutorService", None, &["java/util/concurrent/Executor", "java/lang/AutoCloseable"], INTERFACE, &[]),
    class("java/util/concurrent/Callable", None, &[], INTERFACE, natives!["call" "()Ljava/lang/Object;" A => None]),
    class("java/util/concurrent/Future", None, &[], INTERFACE, natives!["get" "()Ljava/lang/Object;" A => None]),
    class("java/util/concurrent/RunnableFuture", None, &["java/lang/Runnable", "java/util/concurrent/Future"], INTERFACE, &[]),
    NativeClass { name: "java/util/concurrent/ThreadPoolExecutor", super_name: Some("java/lang/Object"), interfaces: &["java/util/concurrent/ExecutorService"], access: PUBLIC, methods: POOL_METHODS, statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/concurrent/FutureTask", super_name: Some("java/lang/Object"), interfaces: &["java/util/concurrent/RunnableFuture"], access: PUBLIC, methods: FUTURE_METHODS, statics: &[], clinit: None, data: None },
    class("java/util/concurrent/Executors", Some("java/lang/Object"), &[], PUBLIC, natives![
        "newFixedThreadPool" "(I)Ljava/util/concurrent/ExecutorService;" S => Some(|vm, a| executors(vm, a, "fixed")),
        "newSingleThreadExecutor" "()Ljava/util/concurrent/ExecutorService;" S => Some(|vm, a| executors(vm, a, "single")),
        "newCachedThreadPool" "()Ljava/util/concurrent/ExecutorService;" S => Some(|vm, a| executors(vm, a, "cached")),
        "newWorkStealingPool" "()Ljava/util/concurrent/ExecutorService;" S => Some(|vm, a| executors(vm, a, "cached")),
        "newVirtualThreadPerTaskExecutor" "()Ljava/util/concurrent/ExecutorService;" S => Some(|vm, a| executors(vm, a, "virtual")),
        "newScheduledThreadPool" "(I)Ljava/util/concurrent/ScheduledExecutorService;" S => Some(|_, _| Err(Exc::Unsupported("Executors.newScheduledThreadPool".into()))),
    ]),
    NativeClass { name: "java/util/concurrent/CountDownLatch", super_name: Some("java/lang/Object"), interfaces: &[], access: PUBLIC, methods: natives![
        "<init>" "(I)V" M => Some(|vm, a| latch_op(vm, a, "init")),
        "countDown" "()V" M => Some(|vm, a| latch_op(vm, a, "countDown")),
        "getCount" "()J" M => Some(|vm, a| latch_op(vm, a, "getCount")),
        "await" "()V" M => Some(|vm, a| latch_op(vm, a, "await")),
        "await" "(JLjava/util/concurrent/TimeUnit;)Z" M => Some(|vm, a| latch_op(vm, a, "awaitTimed")),
        "toString" "()Ljava/lang/String;" M => Some(|vm, a| latch_op(vm, a, "toString")),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/concurrent/Semaphore", super_name: Some("java/lang/Object"), interfaces: &["java/io/Serializable"], access: PUBLIC, methods: natives![
        "<init>" "(I)V" M => Some(|vm, a| semaphore_op(vm, a, "init")),
        "<init>" "(IZ)V" M => Some(|vm, a| semaphore_op(vm, a, "init")),
        "acquire" "()V" M => Some(|vm, a| semaphore_op(vm, a, "acquire")),
        "acquire" "(I)V" M => Some(|vm, a| semaphore_op(vm, a, "acquire")),
        "tryAcquire" "()Z" M => Some(|vm, a| semaphore_op(vm, a, "tryAcquire")),
        "release" "()V" M => Some(|vm, a| semaphore_op(vm, a, "release")),
        "release" "(I)V" M => Some(|vm, a| semaphore_op(vm, a, "release")),
        "availablePermits" "()I" M => Some(|vm, a| semaphore_op(vm, a, "availablePermits")),
    ], statics: &[], clinit: None, data: None },
    class("java/util/concurrent/locks/Lock", None, &[], INTERFACE, natives!["lock" "()V" A => None, "unlock" "()V" A => None]),
    class("java/util/concurrent/locks/Condition", None, &[], INTERFACE, natives!["await" "()V" A => None]),
    NativeClass { name: "java/util/concurrent/locks/ReentrantLock", super_name: Some("java/lang/Object"), interfaces: &["java/util/concurrent/locks/Lock", "java/io/Serializable"], access: PUBLIC, methods: natives![
        "<init>" "()V" M => Some(|vm, a| lock_op(vm, a, "init")),
        "<init>" "(Z)V" M => Some(|vm, a| lock_op(vm, a, "init")),
        "lock" "()V" M => Some(|vm, a| lock_op(vm, a, "lock")),
        "lockInterruptibly" "()V" M => Some(|vm, a| lock_op(vm, a, "lockInterruptibly")),
        "tryLock" "()Z" M => Some(|vm, a| lock_op(vm, a, "tryLock")),
        "tryLock" "(JLjava/util/concurrent/TimeUnit;)Z" M => Some(|vm, a| lock_op(vm, a, "tryLock")),
        "unlock" "()V" M => Some(|vm, a| lock_op(vm, a, "unlock")),
        "isLocked" "()Z" M => Some(|vm, a| lock_op(vm, a, "isLocked")),
        "isHeldByCurrentThread" "()Z" M => Some(|vm, a| lock_op(vm, a, "isHeldByCurrentThread")),
        "getHoldCount" "()I" M => Some(|vm, a| lock_op(vm, a, "getHoldCount")),
        "newCondition" "()Ljava/util/concurrent/locks/Condition;" M => Some(|vm, a| lock_op(vm, a, "newCondition")),
    ], statics: &[], clinit: None, data: None },
    NativeClass { name: "java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionObject", super_name: Some("java/lang/Object"), interfaces: &["java/util/concurrent/locks/Condition"], access: PUBLIC, methods: natives![
        "await" "()V" M => Some(|vm, a| condition_op(vm, a, "await")),
        "await" "(JLjava/util/concurrent/TimeUnit;)Z" M => Some(|vm, a| condition_op(vm, a, "awaitTimed")),
        "signal" "()V" M => Some(|vm, a| condition_op(vm, a, "signal")),
        "signalAll" "()V" M => Some(|vm, a| condition_op(vm, a, "signalAll")),
    ], statics: &[], clinit: None, data: None },
    class("java/util/concurrent/BlockingQueue", None, &["java/util/Queue"], INTERFACE, &[]),
    class("java/util/concurrent/BlockingDeque", None, &["java/util/concurrent/BlockingQueue", "java/util/Deque"], INTERFACE, &[]),
    class("java/util/concurrent/LinkedBlockingQueue$Extra", None, &[], 0, BLOCKING_METHODS),
    class("java/util/concurrent/ArrayBlockingQueue$Extra", None, &[], 0, BLOCKING_METHODS),
    class("java/util/concurrent/LinkedBlockingDeque$Extra", None, &[], 0, BLOCKING_METHODS),
    class("java/util/concurrent/PriorityBlockingQueue$Extra", None, &[], 0, BLOCKING_METHODS),
    NativeClass {
        name: "java/util/concurrent/TimeUnit",
        super_name: Some("java/lang/Enum"),
        interfaces: &[],
        access: PUBLIC | 0x4000,
        methods: natives![
            "values" "()[Ljava/util/concurrent/TimeUnit;" S => Some(|vm, _| super::math::enum_values(vm, "java/util/concurrent/TimeUnit")),
            "toNanos" "(J)J" M => Some(|vm, a| unit_op(vm, a, "toNanos")),
            "toMicros" "(J)J" M => Some(|vm, a| unit_op(vm, a, "toMicros")),
            "toMillis" "(J)J" M => Some(|vm, a| unit_op(vm, a, "toMillis")),
            "toSeconds" "(J)J" M => Some(|vm, a| unit_op(vm, a, "toSeconds")),
            "toMinutes" "(J)J" M => Some(|vm, a| unit_op(vm, a, "toMinutes")),
            "toHours" "(J)J" M => Some(|vm, a| unit_op(vm, a, "toHours")),
            "toDays" "(J)J" M => Some(|vm, a| unit_op(vm, a, "toDays")),
            "sleep" "(J)V" M => Some(|vm, a| unit_op(vm, a, "sleep")),
            "convert" "(JLjava/util/concurrent/TimeUnit;)J" M => Some(|vm, a| unit_op(vm, a, "convert")),
        ],
        statics: &[
            ("NANOSECONDS", "Ljava/util/concurrent/TimeUnit;"),
            ("MICROSECONDS", "Ljava/util/concurrent/TimeUnit;"),
            ("MILLISECONDS", "Ljava/util/concurrent/TimeUnit;"),
            ("SECONDS", "Ljava/util/concurrent/TimeUnit;"),
            ("MINUTES", "Ljava/util/concurrent/TimeUnit;"),
            ("HOURS", "Ljava/util/concurrent/TimeUnit;"),
            ("DAYS", "Ljava/util/concurrent/TimeUnit;"),
            ("$VALUES", "[Ljava/util/concurrent/TimeUnit;"),
        ],
        clinit: Some(|vm, _| super::math::make_enum_constants(vm, "java/util/concurrent/TimeUnit", UNIT_NAMES)),
        data: None,
    },
];

static BLOCKING_METHODS: &[NativeMethod] = natives![
    "put" "(Ljava/lang/Object;)V" M => Some(|vm, a| blocking_op(vm, a, "put")),
    "take" "()Ljava/lang/Object;" M => Some(|vm, a| blocking_op(vm, a, "take")),
    "poll" "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;" M => Some(|vm, a| blocking_op(vm, a, "pollTimed")),
    "offer" "(Ljava/lang/Object;JLjava/util/concurrent/TimeUnit;)Z" M => Some(|vm, a| blocking_op(vm, a, "offerTimed")),
    "remainingCapacity" "()I" M => Some(|vm, a| blocking_op(vm, a, "remainingCapacity")),
    "drainTo" "(Ljava/util/Collection;)I" M => Some(|vm, a| blocking_op(vm, a, "drainTo")),
];
