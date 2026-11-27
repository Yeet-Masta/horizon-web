// game-module-cfg: target_family = "wasm"
// game-module-visibility: pub
//! The web build's start (`web/README.md`): the page hands over the prepared
//! resources and the kept game folder, the WebGPU device opens (which waits
//! on the browser, as nothing else in the game may), then the game starts as
//! it does natively, on the page's event loop.
use horizon_client::Result;
use horizon_std::time::{Duration, Instant};

/// `main` on the web: starts the game once the page is ready, and returns.
pub fn start() {
    wasm_bindgen_futures::spawn_local(async {
        if let Err(error) = boot().await {
            stopped(Some(&*error));
        }
    });
}

async fn boot() -> Result<()> {
    horizon_std::web::boot().await?;
    // Logs, crash reports and saves all live in the game folder.
    if horizon_std::env::var_os("HORIZON_HOME").is_none() {
        horizon_std::env::set_var("HORIZON_HOME", horizon_std::env::HOME);
    }
    horizon_client::telemetry::mark_start();
    // Play with Friends' STUN servers, for the browsers' peer connections.
    #[cfg(feature = "friends")]
    horizon_client::net::set_web_stun_servers(
        horizon_friends::stun::DEFAULT_SERVERS
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    );
    horizon_client::gfx::open_web_device().await?;
    let Some(startup) = crate::entry::web_startup()? else {
        return Ok(());
    };
    // As natively, with no native mods: the web build has no process to load them into.
    crate::entry::start(
        Default::default(),
        |_, _| (Vec::new(), ()),
        crate::entry::CodeRuntime {
            initialize: |_| {},
            refusal: |_| None,
            startup_refused: |_| false,
        },
        startup,
    );
    Ok(())
}

/// A panic stops the page's game for good: say so over the page, after the crash report.
pub(crate) fn show_panics() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        horizon_std::web::status(&format!("Horizon crashed: {info}"), true);
    }));
}

/// The game stopped: its error, or that it quit, goes over the page.
pub fn stopped(error: Option<&dyn std::error::Error>) {
    match error {
        Some(error) => {
            log::error!("Horizon: {error}");
            horizon_std::web::status(&format!("Horizon stopped: {error}"), true);
        }
        None => horizon_std::web::status("Horizon quit. Reload the page to play again.", false),
    }
}

/// The page's frames, for Max Framerate (`skip_frame`): when the last came,
/// how far apart they come (the display's refresh), and when the next drawn
/// frame is due.
struct Pace {
    last: Option<Instant>,
    display: f64,
    due: Option<Instant>,
}
thread_local! {
    static PACE: std::cell::RefCell<Pace> = const {
        std::cell::RefCell::new(Pace { last: None, display: 1.0 / 60.0, due: None })
    };
}

/// Whether to skip this frame of the page's under Max Framerate `limit`
/// (260: unlimited). The page's frames come at the display's rate and the
/// web build can't sleep between them, so a frame draws when its slot has
/// come, to within half a display frame; slots follow each other by the
/// limit's interval, so 60 on a 144 Hz display draws 60 a second, not 48. A
/// browser draws no faster than its display: above that the limit does
/// nothing.
pub fn skip_frame(limit: i32) -> bool {
    let now = Instant::now();
    PACE.with_borrow_mut(|pace| {
        if let Some(last) = pace.last.replace(now) {
            let gap = now.duration_since(last).as_secs_f64();
            // A frame that took long says nothing of the display.
            if gap < 0.05 {
                pace.display = pace.display * 0.9 + gap * 0.1;
            }
        }
        if limit >= 260 {
            pace.due = None;
            return false;
        }
        let slot = Duration::from_secs_f64(1.0 / f64::from(limit.max(1)));
        let early = Duration::from_secs_f64(pace.display / 2.0);
        let due = *pace.due.get_or_insert(now);
        if now + early < due {
            return true;
        }
        // The next slot follows this one; after a slow frame, it follows now.
        let next = due + slot;
        pace.due = Some(if next + early < now { now + slot } else { next });
        false
    })
}

/// What the page asked of the frame statistics ([`horizon_frame_stats_ask`]): to read them, and
/// whether to start them over after.
static FRAME_STATS_ASK: horizon_std::sync::Mutex<Option<bool>> =
    horizon_std::sync::Mutex::new(None);
/// The frame statistics last asked for, as `horizon-ctl frame-stats` prints them.
static FRAME_STATS: horizon_std::sync::Mutex<Option<String>> = horizon_std::sync::Mutex::new(None);

/// The page asks for the game's frame statistics (`FrameStats::summary`): frame, CPU and GPU
/// times and where the slowest frames went. The game answers at its next frame
/// ([`horizon_frame_stats`]); with `reset` it starts counting again after (tools/web_profile.py).
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn horizon_frame_stats_ask(reset: bool) {
    *FRAME_STATS.lock().unwrap() = None;
    *FRAME_STATS_ASK.lock().unwrap() = Some(reset);
}

/// What the page asked of the GPU pass timers ([`horizon_pass_timing`]): 1 on, 2 off.
static PASS_TIMING_ASK: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// The page times Horizon's own passes on the GPU while `on`, as F3 does: the frame
/// statistics then carry each pass's median (`gpu_passes`, tools/web_profile.py `--passes`).
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn horizon_pass_timing(on: bool) {
    PASS_TIMING_ASK.store(if on { 1 } else { 2 }, std::sync::atomic::Ordering::Relaxed);
}

/// What the page last asked of the pass timers, once: the game's `gpu_passes` follows it.
pub fn pass_timing_ask() -> Option<bool> {
    match PASS_TIMING_ASK.swap(0, std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        ask => Some(ask == 1),
    }
}

/// The answer to [`horizon_frame_stats_ask`], as JSON, once the game gave it.
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn horizon_frame_stats() -> Option<String> {
    FRAME_STATS.lock().unwrap().clone()
}

/// Each frame: answers the page's ask, if there is one, with the chunks loaded and the sections
/// meshed so far beside the frame times: how fast terrain comes in, which a fast machine shows
/// where its frame times sit at the display's rate.
pub fn answer_frame_stats(
    stats: &mut horizon_client::control::FrameStats,
    chunks: usize,
    meshed_sections: u64,
    renderer: Option<&horizon_client_render::gfx::Renderer>,
) {
    let Some(reset) = FRAME_STATS_ASK.lock().unwrap().take() else {
        return;
    };
    let mut summary = stats.summary();
    if let Some(fields) = summary.as_object_mut() {
        fields.insert("chunks".into(), chunks.into());
        fields.insert("meshed_sections".into(), meshed_sections.into());
        let passes = renderer.map(|r| r.own_pass_times()).unwrap_or_default();
        if !passes.is_empty() {
            let passes: serde_json::Map<_, _> = passes
                .into_iter()
                .map(|(name, ms)| (name, f64::from(ms).into()))
                .collect();
            fields.insert("gpu_passes".into(), passes.into());
        }
    }
    *FRAME_STATS.lock().unwrap() = Some(summary.to_string());
    if reset {
        stats.reset();
    }
}
