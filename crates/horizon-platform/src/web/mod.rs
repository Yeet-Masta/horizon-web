//! What the web build asks of its page (`web/horizon.js`): the arguments,
//! the prepared resources and the kept game folder before the game starts,
//! a place to keep what the game writes, the console and randomness.
#[cfg(target_feature = "atomics")]
mod alloc;
#[cfg(target_feature = "atomics")]
mod blobs;
pub mod http;
pub mod https;
#[cfg(target_feature = "atomics")]
pub mod jspi;
pub mod rtc;
pub mod socket;
pub mod vfs;

use crate::time::{Duration, SystemTime, UNIX_EPOCH};
use js_sys::{Array, Object, Reflect, Uint8Array};
use std::path::Path;
use wasm_bindgen::{JsCast, JsValue, prelude::*};

#[wasm_bindgen]
extern "C" {
    /// The page's start: resolves to `{ args, env, resources, files }`.
    #[wasm_bindgen(js_namespace = globalThis, js_name = horizonBoot)]
    fn horizon_boot() -> js_sys::Promise;
    /// Keeps `[path, kind, bytes, mtime]` records ("file", "dir" or "removed").
    #[wasm_bindgen(js_namespace = globalThis, js_name = horizonPersist)]
    fn horizon_persist(records: Array);
    /// Shows a message over the page (an error the game could not show itself).
    #[wasm_bindgen(js_namespace = globalThis, js_name = horizonStatus)]
    fn horizon_status(message: &str, error: bool);
    /// Hands the page a file for the player to keep: a download.
    #[wasm_bindgen(catch, js_namespace = globalThis, js_name = horizonSaveFile)]
    fn horizon_save_file(name: &str, bytes: Uint8Array) -> Result<(), JsValue>;
    /// Opens a web link in a new tab.
    #[wasm_bindgen(catch, js_namespace = globalThis, js_name = horizonOpenUrl)]
    fn horizon_open_url(url: &str) -> Result<(), JsValue>;
    /// Writes text to the system clipboard, where the browser lets the page.
    #[wasm_bindgen(catch, js_namespace = globalThis, js_name = horizonClipboardWrite)]
    fn horizon_clipboard_write(text: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(callback: &js_sys::Function, ms: i32) -> JsValue;
    #[wasm_bindgen(js_name = setInterval)]
    fn set_interval(callback: &Closure<dyn FnMut()>, ms: i32) -> i32;
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn console_warn(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = info)]
    fn console_info(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = debug)]
    fn console_debug(message: &str);
    #[wasm_bindgen(js_namespace = crypto, js_name = getRandomValues)]
    fn get_random_values(buf: &mut [u8]);
    /// Browsers refuse a view of shared memory, as the threaded build's is.
    #[wasm_bindgen(js_namespace = crypto, js_name = getRandomValues)]
    fn get_random_values_into(buf: &Uint8Array);
}

/// The browser console: 1 error, 2 warning, 3 info, else debug.
pub fn console(level: u8, message: &str) {
    match level {
        1 => console_error(message),
        2 => console_warn(message),
        3 => console_info(message),
        _ => console_debug(message),
    }
}
/// A message over the page, for what fails before the game can draw.
pub fn status(message: &str, error: bool) {
    horizon_status(message, error);
}

/// A file for the player to keep, such as a screenshot: the browser's
/// download, which the page makes ([`run_on_page`] from a worker).
pub fn save_file(name: &str, bytes: &[u8]) {
    let (name, bytes) = (name.to_owned(), bytes.to_vec());
    // A copy: a Blob can't hold a view of shared memory.
    run_on_page(Box::new(move || {
        let _ = horizon_save_file(&name, Uint8Array::from(bytes.as_slice()));
    }));
}
/// `Util.getPlatform().openUri`: a new tab on the link.
pub fn open_url(url: &str) {
    let url = url.to_owned();
    run_on_page(Box::new(move || {
        let _ = horizon_open_url(&url);
    }));
}

/// The system clipboard as far as the page reaches it: a page reads it only
/// in a paste event, so this is the text the last one gave
/// ([`horizon_clipboard_paste`]: web/horizon.js holds Ctrl+V back from the
/// game until then), or what the game copied since.
static CLIPBOARD: crate::sync::Mutex<Option<String>> = crate::sync::Mutex::new(None);
pub fn clipboard_text() -> Option<String> {
    CLIPBOARD.lock().unwrap().clone()
}
/// Copies `text`, to the system clipboard where the browser lets the page
/// write it (after the player's input, as Ctrl+C is).
pub fn set_clipboard_text(text: &str) {
    *CLIPBOARD.lock().unwrap() = Some(text.to_owned());
    let text = text.to_owned();
    run_on_page(Box::new(move || {
        let _ = horizon_clipboard_write(&text);
    }));
}
/// The text of a paste event, before the game sees the Ctrl+V that made it.
#[wasm_bindgen]
pub fn horizon_clipboard_paste(text: String) {
    *CLIPBOARD.lock().unwrap() = Some(text);
}

/// Resolves after `ms` milliseconds, the page's event loop turning meanwhile.
#[cfg(target_feature = "atomics")]
async fn nap(ms: i32) {
    let mut resolve = |resolve: js_sys::Function, _| {
        set_timeout(&resolve, ms);
    };
    let _ = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::new(&mut resolve)).await;
}

/// Bytes `start..end` of the page's Blob `id` (blobs.rs): the threaded
/// build's, which alone keeps files there.
pub(crate) fn read_blob(id: u32, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    #[cfg(target_feature = "atomics")]
    return blobs::read(id, start, end);
    #[cfg(not(target_feature = "atomics"))]
    {
        let _ = (id, start, end);
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

fn get(object: &JsValue, key: &str) -> JsValue {
    Reflect::get(object, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}
fn millis(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}
fn time(ms: &JsValue) -> SystemTime {
    match ms.as_f64() {
        Some(ms) if ms > 0.0 => UNIX_EPOCH + Duration::from_secs_f64(ms / 1000.0),
        _ => SystemTime::now(),
    }
}

/// Where the prepared resources are mounted: `resources/` beside the
/// executable, as in a release folder.
pub const RESOURCES: &str = "/app/resources";
/// What marks the prepared resources' folder in the player's zip.
const RESOURCES_MARKER: &str = "horizon-resources.json";

/// Takes what the page gives the game: the arguments and variables, the
/// prepared resources (a zip, mounted at [`RESOURCES`], or loose files) and
/// the game folder kept from earlier visits. Then keeps the game folder:
/// every two seconds, and when the page asks (`globalThis.horizonFlush`,
/// called as the page is hidden), what changed in it goes to the page.
pub async fn boot() -> Result<(), String> {
    let boot = wasm_bindgen_futures::JsFuture::from(horizon_boot())
        .await
        .map_err(|e| format!("the page did not start the game: {e:?}"))?;
    {
        let mut env = crate::env::ENV.lock().unwrap();
        if let Ok(args) = get(&boot, "args").dyn_into::<Array>() {
            env.args = args.iter().filter_map(|a| a.as_string()).collect();
        }
        if let Some(vars) = get(&boot, "env").dyn_ref::<Object>() {
            for entry in Object::entries(vars).iter() {
                let entry: Array = entry.unchecked_into();
                if let (Some(k), Some(v)) = (entry.get(0).as_string(), entry.get(1).as_string()) {
                    env.vars.insert(k, v);
                }
            }
        }
    }
    // With the game data in browser storage (the page's Options), the
    // threaded build reads the resources zip and the game folder's larger
    // files from the page's Blobs (blobs.rs), the page's thread through a
    // worker that must run first.
    #[cfg(target_feature = "atomics")]
    if get(&js_sys::global(), "horizonBlobs")
        .dyn_into::<Array>()
        .is_ok_and(|blobs| blobs.length() > 0)
    {
        blobs::start_reader().await;
    }
    if let Ok(zip) = get(&boot, "resources").dyn_into::<Uint8Array>() {
        vfs::mount_zip(Path::new(RESOURCES), zip.to_vec(), Some(RESOURCES_MARKER))
            .map_err(|e| format!("the resources zip could not be read: {e}"))?;
    }
    if let (Some(id), Some(len)) = (
        get(&boot, "resourcesBlob").as_f64(),
        get(&boot, "resourcesSize").as_f64(),
    ) {
        // Game data in memory (the page's Options): all of the zip but its sounds, which
        // are read as they are first played.
        let mount = if get(&boot, "resourcesEager").as_bool() == Some(true) {
            vfs::mount_zip_blob_eager
        } else {
            vfs::mount_zip_blob
        };
        mount(
            Path::new(RESOURCES),
            id as u32,
            len as u64,
            Some(RESOURCES_MARKER),
        )
        .map_err(|e| format!("the resources zip could not be read: {e}"))?;
    }
    // Loose files, `[path, bytes]`: a folder the player picked.
    if let Ok(files) = get(&boot, "resourceFiles").dyn_into::<Array>() {
        for file in files.iter() {
            let file: Array = file.unchecked_into();
            if let (Some(path), Ok(bytes)) = (
                file.get(0).as_string(),
                file.get(1).dyn_into::<Uint8Array>(),
            ) {
                vfs::restore(
                    &format!("{RESOURCES}/{}", path.trim_start_matches('/')),
                    vfs::Kept::File(bytes.to_vec(), SystemTime::now()),
                );
            }
        }
    }
    // The kept game folder, `[path, kind, bytes, mtime]`, or for a file the
    // page keeps as a Blob, `[path, "blob", id, mtime, size]`.
    if let Ok(files) = get(&boot, "files").dyn_into::<Array>() {
        for record in files.iter() {
            let record: Array = record.unchecked_into();
            let Some(path) = record.get(0).as_string() else {
                continue;
            };
            let kept = match record.get(1).as_string().as_deref() {
                Some("dir") => vfs::Kept::Dir(time(&record.get(3))),
                Some("file") => match record.get(2).dyn_into::<Uint8Array>() {
                    Ok(bytes) => vfs::Kept::File(bytes.to_vec(), time(&record.get(3))),
                    Err(_) => continue,
                },
                Some("blob") => match (record.get(2).as_f64(), record.get(4).as_f64()) {
                    (Some(id), Some(len)) => {
                        vfs::Kept::Stored(id as u32, len as u64, time(&record.get(3)))
                    }
                    _ => continue,
                },
                _ => continue,
            };
            vfs::restore(&path, kept);
        }
    }
    vfs::persist(Path::new(crate::env::HOME));
    let flush = Closure::<dyn FnMut()>::own_assert_unwind_safe(flush);
    set_interval(&flush, 2000);
    let _ = Reflect::set(
        &js_sys::global(),
        &JsValue::from_str("horizonFlush"),
        flush.as_ref(),
    );
    flush.forget();
    Ok(())
}

/// Hands the page what changed in the game folder.
pub fn flush() {
    let changes = vfs::drain();
    if changes.is_empty() {
        return;
    }
    let records = Array::new();
    for (path, kept) in changes {
        let record = Array::new();
        record.push(&JsValue::from_str(&path));
        match kept {
            vfs::Kept::Dir(time) => {
                record.push(&JsValue::from_str("dir"));
                record.push(&JsValue::NULL);
                record.push(&JsValue::from_f64(millis(time)));
            }
            vfs::Kept::File(bytes, time) => {
                record.push(&JsValue::from_str("file"));
                record.push(&Uint8Array::from(bytes.as_slice()));
                record.push(&JsValue::from_f64(millis(time)));
            }
            vfs::Kept::Removed => {
                record.push(&JsValue::from_str("removed"));
            }
            // Only ever restored: what changed is read into memory first.
            vfs::Kept::Stored(..) => continue,
        }
        records.push(&record);
    }
    horizon_persist(records);
}

fn fill(buf: &mut [u8]) {
    // getRandomValues fills at most 64 KiB per call.
    for chunk in buf.chunks_mut(65536) {
        if cfg!(target_feature = "atomics") {
            let copy = Uint8Array::new_with_length(chunk.len() as u32);
            get_random_values_into(&copy);
            copy.copy_to(chunk);
        } else {
            get_random_values(chunk);
        }
    }
}

/// The threaded build's threads (web/README.md): each is a Web Worker
/// (`web/worker.js`) that instantiates the page's module over its memory and
/// runs the work handed to it here.
#[cfg(target_feature = "atomics")]
mod threads {
    use crate::sync::Mutex;
    use wasm_bindgen::prelude::*;

    type Work = Box<dyn FnOnce() + Send>;
    /// Work handed out and not yet taken up, by number.
    static WORK: Mutex<Vec<Option<Work>>> = Mutex::new(Vec::new());

    thread_local! {
        /// This thread is one of `spawn_thread`'s, not the page's.
        static WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// This worker's event loop goes on after its work returns ([`keep_event_loop`]).
        static KEEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// This worker runs the game's frames on its event loop ([`run_event_loop_here`]).
        static EVENT_LOOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    /// Whether this is the page's thread, the one that may not wait.
    pub fn on_page_thread() -> bool {
        !WORKER.get()
    }
    /// Whether this thread's own event loop settles the browser's promises it waits on: the
    /// page's, and a worker that runs the game ([`run_event_loop_here`]). Waiting there for
    /// such a promise would wait forever ([`crate::block_on`] doesn't).
    pub fn owns_event_loop() -> bool {
        !WORKER.get() || EVENT_LOOP.get()
    }
    /// This worker's event loop goes on once its work returns (worker.js closes it otherwise):
    /// its tasks, timers and frames run there.
    pub fn keep_event_loop() {
        KEEP.set(true);
    }
    /// This worker runs the game, its frames called from its event loop, as the page's are
    /// (the game's loop in a worker, web/README.md): it keeps its event loop, and a future
    /// waiting on the browser is not waited on here ([`owns_event_loop`]).
    pub fn run_event_loop_here() {
        KEEP.set(true);
        EVENT_LOOP.set(true);
    }

    #[wasm_bindgen]
    extern "C" {
        /// Starts `web/worker.js` with the module and memory, to run `work`; `transfer` (an
        /// OffscreenCanvas, or undefined) goes to it as `globalThis.horizonCanvas`.
        #[wasm_bindgen(js_namespace = globalThis, js_name = horizonSpawnThread)]
        fn horizon_spawn_thread(
            name: &str,
            module: JsValue,
            memory: JsValue,
            work: u32,
            transfer: JsValue,
        );
    }

    pub fn spawn_thread(name: &str, work: Work) {
        spawn_thread_with(name, JsValue::UNDEFINED, work);
    }

    /// [`spawn_thread`], handing the worker `transfer` (the page's thread only): the canvas
    /// the game draws on from a worker.
    pub fn spawn_thread_with(name: &str, transfer: JsValue, work: Work) {
        let id = {
            let mut pending = WORK.lock().unwrap();
            match pending.iter().position(Option::is_none) {
                Some(free) => {
                    pending[free] = Some(work);
                    free
                }
                None => {
                    pending.push(Some(work));
                    pending.len() - 1
                }
            }
        };
        horizon_spawn_thread(
            name,
            wasm_bindgen::module(),
            wasm_bindgen::memory(),
            id as u32,
            transfer,
        );
    }

    /// `web/worker.js` runs this once its module is up: the thread's work. True when the
    /// worker's event loop goes on afterwards ([`keep_event_loop`]).
    #[wasm_bindgen]
    pub fn horizon_thread_entry(work: u32) -> bool {
        WORKER.set(true);
        super::alloc::worker_started();
        let work = WORK
            .lock()
            .unwrap()
            .get_mut(work as usize)
            .and_then(Option::take);
        if let Some(work) = work {
            work();
        }
        KEEP.get()
    }

    /// The canvas the page handed this worker (`spawn_thread_with`), once.
    pub fn worker_canvas() -> Option<JsValue> {
        let global = js_sys::global();
        let canvas = super::get(&global, "horizonCanvas");
        let _ = js_sys::Reflect::delete_property(&global, &"horizonCanvas".into());
        (!canvas.is_undefined() && !canvas.is_null()).then_some(canvas)
    }

    /// `navigator.hardwareConcurrency`: the cores the browser reports.
    pub fn hardware_concurrency() -> usize {
        let navigator = super::get(&js_sys::global(), "navigator");
        super::get(&navigator, "hardwareConcurrency")
            .as_f64()
            .map_or(1, |n| n as usize)
            .max(1)
    }
}
#[cfg(target_feature = "atomics")]
pub use threads::{
    hardware_concurrency, horizon_thread_entry, keep_event_loop, on_page_thread, owns_event_loop,
    run_event_loop_here, spawn_thread, spawn_thread_with, worker_canvas,
};
/// Whether this build's panics unwind (web/unwind.toml): one is raised and caught here, quietly.
/// The smoke test (tools/web_smoke.py) asks a build whose build.json says they do. A build whose
/// panics abort must not call it: the panic would stop the page.
#[wasm_bindgen]
pub fn horizon_panics_unwind() -> bool {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(|| panic!("the web build's unwind check")).is_err();
    std::panic::set_hook(hook);
    caught
}

/// Without threads every thread is the page's.
#[cfg(not(target_feature = "atomics"))]
pub fn on_page_thread() -> bool {
    true
}
#[cfg(not(target_feature = "atomics"))]
pub fn owns_event_loop() -> bool {
    true
}

/// Work handed to the page's thread ([`run_on_page`]), by number.
#[cfg(target_feature = "atomics")]
static PAGE_TASKS: crate::sync::Mutex<Vec<Option<Box<dyn FnOnce() + Send>>>> =
    crate::sync::Mutex::new(Vec::new());

#[cfg(target_feature = "atomics")]
#[wasm_bindgen]
extern "C" {
    /// worker.js: posts the task's number to the page, which runs it ([`horizon_page_task`]).
    #[wasm_bindgen(js_namespace = globalThis, js_name = horizonRunOnPage)]
    fn horizon_run_on_page(task: u32);
}

/// Runs `task` on the page's thread, whose event loop has the browser's `fetch`, sockets and
/// promises (a worker's never turns while it waits): at once there; from a worker, once the
/// page's event loop takes worker.js's message.
pub fn run_on_page(task: Box<dyn FnOnce() + Send>) {
    #[cfg(target_feature = "atomics")]
    if !on_page_thread() {
        let id = {
            let mut tasks = PAGE_TASKS.lock().unwrap();
            match tasks.iter().position(Option::is_none) {
                Some(free) => {
                    tasks[free] = Some(task);
                    free
                }
                None => {
                    tasks.push(Some(task));
                    tasks.len() - 1
                }
            }
        };
        horizon_run_on_page(id as u32);
        return;
    }
    task();
}

/// horizon.js runs this on the page's thread for a worker's [`run_on_page`].
#[cfg(target_feature = "atomics")]
#[wasm_bindgen]
pub fn horizon_page_task(task: u32) {
    let task = PAGE_TASKS
        .lock()
        .unwrap()
        .get_mut(task as usize)
        .and_then(Option::take);
    if let Some(task) = task {
        task();
    }
}

/// getrandom 0.3's and 0.4's source on wasm64 (`--cfg getrandom_backend="custom"`,
/// .cargo/config.toml); wasm32 takes their own (`wasm_js`).
///
/// # Safety
/// `dest` is valid for writes of `len` bytes, as getrandom promises.
#[cfg(target_arch = "wasm64")]
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // SAFETY: getrandom passes a buffer of `len` writable bytes.
    fill(unsafe { std::slice::from_raw_parts_mut(dest, len) });
    Ok(())
}

fn getrandom_02(buf: &mut [u8]) -> Result<(), getrandom02::Error> {
    fill(buf);
    Ok(())
}
getrandom02::register_custom_getrandom!(getrandom_02);
