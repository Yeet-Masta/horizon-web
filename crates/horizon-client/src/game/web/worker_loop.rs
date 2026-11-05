//! The game's loop in a Web Worker (web/README.md, "The game's loop in a worker"): opt-in with
//! `?env=HORIZON_WEB_WORKER_LOOP=1` on the threaded build.
//!
//! The page keeps winit's event loop, which has the canvas and its input, and forwards every
//! event to the game's worker. The worker runs the game's own handlers (the same
//! `ApplicationHandler`s as on the page) on its event loop, one step an animation frame, and
//! draws on the page's canvas through the OffscreenCanvas the page handed it. What the handlers
//! ask of the loop goes back to the page (`winit::platform::web::RemoteEventLoop`): a window is
//! made there and shared; the worker's own loop keeps the control flow and the exit.
//!
//! On the worker the game may wait on locks and channels, and nothing the page does (its layout,
//! its garbage collection, the network bridges) holds a frame back.
use horizon_client::Result;
use horizon_platform::time::Instant;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc,
};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use winit::{
    application::ApplicationHandler,
    error::OsError,
    event::{DeviceEvent, DeviceId, StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    platform::web::{EventLoopExtWebSys, RemoteEventLoop, WindowExtWebSys},
    window::{Window, WindowAttributes, WindowId},
};

/// What the page's loop forwards to the game's worker.
enum Forward {
    Resumed,
    Suspended,
    Window(WindowId, WindowEvent),
    Device(DeviceId, DeviceEvent),
    MemoryWarning,
}

/// What the game's worker asks of the page's loop.
enum Ask {
    Window(
        Box<WindowAttributes>,
        mpsc::Sender<std::result::Result<Window, OsError>>,
    ),
    Exit,
}

/// The worker's end of the page's loop, until the game's loop starts ([`spawn_app`]).
struct Link {
    proxy: EventLoopProxy<Ask>,
    events: mpsc::Receiver<Forward>,
}
thread_local! {
    static LINK: RefCell<Option<Link>> = const { RefCell::new(None) };
}

/// The page's canvas, which a worker can draw on once it is handed over.
fn page_canvas() -> Option<web_sys::HtmlCanvasElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id("horizon")?
        .dyn_into()
        .ok()
}

/// Whether the game's loop runs in a worker: asked for (`HORIZON_WEB_WORKER_LOOP=1`), on the
/// threaded build, and the browser hands a canvas to a worker.
pub fn wanted() -> bool {
    cfg!(target_feature = "atomics")
        && horizon_platform::env::var("HORIZON_WEB_WORKER_LOOP").is_ok_and(|v| v.trim() == "1")
        && page_canvas().is_some_and(|canvas| {
            js_sys::Reflect::has(&canvas, &"transferControlToOffscreen".into()).unwrap_or(false)
        })
}

/// On the page: starts the page's event loop and the game's worker, which runs `game` (the rest
/// of the start: the WebGPU device, then the game, whose loop [`spawn_app`] runs there).
#[cfg(target_feature = "atomics")]
pub fn start(game: impl FnOnce() + Send + 'static) -> Result<()> {
    let canvas = page_canvas()
        .ok_or("the page has no canvas")?
        .transfer_control_to_offscreen()
        .map_err(|e| format!("the canvas can't go to a worker: {e:?}"))?;
    let event_loop = EventLoop::<Ask>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let (forward, events) = mpsc::channel();
    horizon_client::window::pointer_lock::watch_page();
    horizon_platform::web::spawn_thread_with(
        "Render thread",
        canvas.into(),
        Box::new(move || {
            horizon_platform::web::run_event_loop_here();
            horizon_client::logging::name_this_thread("Render thread");
            LINK.set(Some(Link { proxy, events }));
            game();
        }),
    );
    log::info!("Horizon: the game's loop runs in a worker (HORIZON_WEB_WORKER_LOOP)");
    event_loop.spawn_app(PageLoop {
        forward,
        windows: Vec::new(),
    });
    Ok(())
}
#[cfg(not(target_feature = "atomics"))]
pub fn start(_game: impl FnOnce() + Send + 'static) -> Result<()> {
    Err("the game's loop runs in a worker only on the threaded build".into())
}

/// The page's side: winit's loop, forwarding its events and making the windows asked for.
struct PageLoop {
    forward: mpsc::Sender<Forward>,
    /// The windows made for the worker, which has handles to them: they stay open while it does.
    windows: Vec<Window>,
}
impl ApplicationHandler<Ask> for PageLoop {
    fn resumed(&mut self, _: &ActiveEventLoop) {
        let _ = self.forward.send(Forward::Resumed);
    }
    fn suspended(&mut self, _: &ActiveEventLoop) {
        let _ = self.forward.send(Forward::Suspended);
    }
    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let _ = self.forward.send(Forward::Window(id, event));
    }
    fn device_event(&mut self, _: &ActiveEventLoop, id: DeviceId, event: DeviceEvent) {
        let _ = self.forward.send(Forward::Device(id, event));
    }
    fn memory_warning(&mut self, _: &ActiveEventLoop) {
        let _ = self.forward.send(Forward::MemoryWarning);
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, ask: Ask) {
        match ask {
            Ask::Window(attributes, reply) => {
                // The page's canvas (the worker, which has no document, found none).
                let attributes = horizon_client::window::on_page(*attributes);
                let made = event_loop.create_window(attributes).map(|window| {
                    let shared = window.share();
                    self.windows.push(window);
                    shared
                });
                let _ = reply.send(made);
            }
            Ask::Exit => event_loop.exit(),
        }
    }
}

/// The worker's side of the loop the game's handlers are handed.
struct Remote {
    proxy: EventLoopProxy<Ask>,
    flow: Cell<ControlFlow>,
    exit: Cell<bool>,
}
impl RemoteEventLoop for Remote {
    fn create_window(&self, attributes: WindowAttributes) -> std::result::Result<Window, OsError> {
        let (reply, made) = mpsc::channel();
        let gone = "Horizon: the page's event loop is gone";
        if self
            .proxy
            .send_event(Ask::Window(Box::new(attributes), reply))
            .is_err()
        {
            panic!("{gone}");
        }
        made.recv().expect(gone)
    }
    fn set_control_flow(&self, control_flow: ControlFlow) {
        self.flow.set(control_flow);
    }
    fn control_flow(&self) -> ControlFlow {
        self.flow.get()
    }
    fn exit(&self) {
        self.exit.set(true);
    }
    fn exiting(&self) -> bool {
        self.exit.get()
    }
}

/// Runs `app`, the game's handlers: in the game's worker on its own event loop (when the page
/// started it, [`start`]), else on the page's, as winit does.
pub fn spawn_app<A: ApplicationHandler + 'static>(app: A) -> Result<()> {
    let Some(link) = LINK.take() else {
        EventLoop::new()?.spawn_app(app);
        return Ok(());
    };
    let remote = Rc::new(Remote {
        proxy: link.proxy,
        flow: Cell::new(ControlFlow::default()),
        exit: Cell::new(false),
    });
    let mut driver = Driver {
        app,
        event_loop: winit::platform::web::remote_event_loop(remote.clone()),
        remote,
        events: link.events,
        started: false,
    };
    // Where the browser has JSPI, each step is a promising call, in which a wait on the browser
    // (a WebGPU error scope) suspends the step (horizon_platform::web::jspi).
    #[cfg(target_feature = "atomics")]
    if horizon_platform::web::jspi::available() {
        log::info!("Horizon: the game's frames can wait on the browser (JSPI)");
        horizon_platform::web::jspi::run_frames(move || driver.step());
        return Ok(());
    }
    next_frame(Rc::new(RefCell::new(driver)));
    Ok(())
}

/// The game's loop on its worker.
struct Driver<A> {
    app: A,
    event_loop: ActiveEventLoop,
    remote: Rc<Remote>,
    events: mpsc::Receiver<Forward>,
    started: bool,
}
impl<A: ApplicationHandler> Driver<A> {
    /// One turn of the loop, as winit's: the events that came since, then `about_to_wait`. A
    /// loop that waits (`ControlFlow::Wait`) and got nothing does nothing. False once it ended.
    fn step(&mut self) -> bool {
        let forwarded: Vec<Forward> = self.events.try_iter().collect();
        let now = Instant::now();
        let flow = self.remote.flow.get();
        let cause = if !self.started {
            StartCause::Init
        } else if !forwarded.is_empty() {
            StartCause::WaitCancelled {
                start: now,
                requested_resume: match flow {
                    ControlFlow::WaitUntil(at) => Some(at),
                    _ => None,
                },
            }
        } else {
            match flow {
                ControlFlow::Poll => StartCause::Poll,
                ControlFlow::WaitUntil(at) if now >= at => StartCause::ResumeTimeReached {
                    start: now,
                    requested_resume: at,
                },
                _ => return true,
            }
        };
        self.started = true;
        let event_loop = &self.event_loop;
        self.app.new_events(event_loop, cause);
        for forward in forwarded {
            if self.remote.exit.get() {
                break;
            }
            match forward {
                Forward::Resumed => self.app.resumed(event_loop),
                Forward::Suspended => self.app.suspended(event_loop),
                Forward::Window(id, event) => self.app.window_event(event_loop, id, event),
                Forward::Device(id, event) => self.app.device_event(event_loop, id, event),
                Forward::MemoryWarning => self.app.memory_warning(event_loop),
            }
        }
        if !self.remote.exit.get() {
            self.app.about_to_wait(event_loop);
        }
        if self.remote.exit.get() {
            self.app.exiting(event_loop);
            let _ = self.remote.proxy.send_event(Ask::Exit);
            return false;
        }
        true
    }
}

/// Runs the loop's next step at the worker's next animation frame (a worker's
/// `requestAnimationFrame`, which its OffscreenCanvas presents after), or soon where the browser
/// has none in workers.
fn next_frame<A: ApplicationHandler + 'static>(driver: Rc<RefCell<Driver<A>>>) {
    let frame = Closure::once_assert_unwind_safe(move || {
        if driver.borrow_mut().step() {
            next_frame(driver);
        }
    })
    .into_js_value();
    let global = js_sys::global();
    let raf = js_sys::Reflect::get(&global, &"requestAnimationFrame".into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    let scheduled = match raf {
        Some(raf) => raf.call1(&global, &frame),
        None => js_sys::Reflect::get(&global, &"setTimeout".into())
            .and_then(|f| f.dyn_into::<js_sys::Function>().map_err(JsValue::from))
            .and_then(|timeout| timeout.call2(&global, &frame, &JsValue::from(4))),
    };
    if let Err(e) = scheduled {
        log::error!("Horizon: the game's worker can't schedule its frames: {e:?}");
    }
}
