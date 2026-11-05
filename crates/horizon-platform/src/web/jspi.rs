//! JavaScript Promise Integration for the game's worker (web/README.md, "JSPI"): a frame of the
//! game's loop in a worker runs as a `WebAssembly.promising` call (web/worker.js), and inside one
//! a wait on the browser (a WebGPU error scope, a buffer mapping) suspends the frame until the
//! promise settles, the worker's event loop turning meanwhile, instead of being given up on as
//! on the page ([`crate::block_on_or`]).
//!
//! wasm-bindgen's own JSPI refuses shared memory: a suspended call's stack frames stay in linear
//! memory, and other code run meanwhile must not overwrite them. Here only one frame is ever
//! suspended on a thread, entered from JavaScript with no WebAssembly beneath it, and nothing that
//! runs while it waits suspends ([`in_frame`] is false then): what runs meanwhile uses the stack
//! below the suspended frame's and gives it back before the frame resumes.
use std::cell::{Cell, RefCell};
use std::task::{Context, Poll, Wake, Waker};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// worker.js: settles with 0 when `promise` resolves, 1 when it rejects. Wrapped in
    /// `WebAssembly.Suspending` where the browser has JSPI, so the frame waits for it.
    #[wasm_bindgen(js_namespace = globalThis, js_name = horizonJspiSuspend)]
    fn horizon_jspi_suspend(promise: &js_sys::Promise) -> u32;
    /// worker.js: runs [`horizon_frame_step`] as a promising call each animation frame.
    #[wasm_bindgen(js_namespace = globalThis, js_name = horizonRunFrames)]
    fn horizon_run_frames();
}

type Step = Box<dyn FnMut() -> bool>;
thread_local! {
    /// The frame being run may suspend.
    static IN_FRAME: Cell<bool> = const { Cell::new(false) };
    /// What a frame runs, between frames.
    static STEP: RefCell<Option<Step>> = const { RefCell::new(None) };
    /// The resolver of the promise the frame waits on, for its waker.
    static RESUME: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    /// A frame has waited once (the console says so, once).
    static WAITED: Cell<bool> = const { Cell::new(false) };
}

/// Whether this worker's frames can suspend: its module's suspending import was wrapped
/// (worker.js, where the browser has JSPI).
pub fn available() -> bool {
    js_sys::Reflect::get(&js_sys::global(), &"horizonJspi".into())
        .ok()
        .and_then(|v| v.as_bool())
        == Some(true)
}

/// Whether a wait here can suspend: inside a promising frame and not inside a wait already.
pub fn in_frame() -> bool {
    IN_FRAME.get()
}

/// Runs `step` at each animation frame as a promising call, until it returns false.
pub fn run_frames(step: impl FnMut() -> bool + 'static) {
    STEP.set(Some(Box::new(step)));
    horizon_run_frames();
}

/// worker.js calls this, through `WebAssembly.promising`, at each animation frame: false when
/// the loop ended.
#[wasm_bindgen]
pub fn horizon_frame_step() -> bool {
    let Some(mut step) = STEP.take() else {
        return false;
    };
    IN_FRAME.set(true);
    let more = step();
    IN_FRAME.set(false);
    if more {
        STEP.set(Some(step));
    }
    more
}

/// Wakes a frame waiting in [`block_on`] when woken on its thread; from another, the frame's
/// wait times out soon and polls again.
struct Resume(std::thread::ThreadId);
impl Wake for Resume {
    fn wake(self: std::sync::Arc<Self>) {
        if std::thread::current().id() == self.0
            && let Some(resume) = RESUME.take()
        {
            let _ = resume.call0(&JsValue::NULL);
        }
    }
}

/// Suspends the frame until woken or `timeout_ms` passes.
fn suspend(timeout_ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let global = js_sys::global();
        if let Ok(timeout) = js_sys::Reflect::get(&global, &"setTimeout".into())
            .and_then(|f| f.dyn_into::<js_sys::Function>().map_err(JsValue::from))
        {
            let _ = timeout.call2(&global, &resolve, &JsValue::from(timeout_ms));
        }
        RESUME.set(Some(resolve));
    });
    // Nothing run while the frame waits may suspend: it is not the frame.
    IN_FRAME.set(false);
    horizon_jspi_suspend(&promise);
    IN_FRAME.set(true);
    RESUME.set(None);
}

/// Polls `future` to its end, suspending the frame between polls; `None` once `limit` went by.
pub fn block_on<F: std::future::Future>(
    future: F,
    limit: Option<std::time::Duration>,
) -> Option<F::Output> {
    let started = crate::time::Instant::now();
    let deadline = limit.map(|limit| started + limit);
    let waker = Waker::from(std::sync::Arc::new(Resume(std::thread::current().id())));
    let mut future = std::pin::pin!(future);
    let mut waited = false;
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut Context::from_waker(&waker)) {
            if waited && !WAITED.replace(true) {
                super::console(
                    3,
                    &format!(
                        "Horizon: a frame waited {} ms for the browser's answer (JSPI)",
                        started.elapsed().as_millis()
                    ),
                );
            }
            return Some(output);
        }
        waited = true;
        if deadline.is_some_and(|deadline| crate::time::Instant::now() >= deadline) {
            return None;
        }
        suspend(16);
    }
}
