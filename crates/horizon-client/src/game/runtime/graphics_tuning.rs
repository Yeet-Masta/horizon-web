//! Supervises graphics benchmark workers without giving them the player's game directory.
use horizon_bench::autotune::{
    Failure, Group, Identity, Mode, Outcome, Profile, SESSION_VERSION, Session,
};
use horizon_bench::{Gpu, Report};
use horizon_client::graphics::{GpuCullingMode, Graphics, GraphicsApi, PresentMode};
use horizon_client::graphics_ui::{
    BenchmarkAction, BenchmarkChange, BenchmarkOutcome, BenchmarkResult, BenchmarkView,
};
use horizon_client::settings::Settings;
use horizon_std::fs::{self, File, OpenOptions};
use horizon_std::sync::Mutex;
use horizon_std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock};

const STATE_DIR: &str = "graphics-benchmark";
const WORKER_ENV: &str = "HORIZON_GRAPHICS_BENCHMARK_WORKER";

/// A single writer per instance. The lock is released by the OS if the supervisor dies.
pub(super) struct Journal {
    dir: PathBuf,
    _lock: File,
    path: PathBuf,
}

impl Journal {
    fn lock(game_dir: &Path, identity: &Identity) -> io::Result<Self> {
        let dir = game_dir.join(STATE_DIR);
        fs::create_dir_all(&dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("session.lock"))?;
        lock.try_lock().map_err(io::Error::from)?;
        reclaim_abandoned_trials(&dir);
        let path = session_path(&dir, identity);
        Ok(Self {
            dir,
            path,
            _lock: lock,
        })
    }

    fn load(game_dir: &Path, identity: &Identity) -> io::Result<Option<Session>> {
        read_json(&session_path(&game_dir.join(STATE_DIR), identity))
    }

    fn save(&self, session: &Session) -> io::Result<()> {
        write_json(&self.path, session)
    }

    /// The web build starts no processes: no trial runs.
    #[cfg(target_family = "wasm")]
    fn run_trial(&self, _: &Path, _: Mode, _: &Profile) -> Result<Report, Failure> {
        Err(Failure::Unavailable)
    }
    #[cfg(not(target_family = "wasm"))]
    fn run_trial(
        &self,
        resources: &Path,
        mode: Mode,
        profile: &Profile,
    ) -> Result<Report, Failure> {
        let scratch = Scratch::new(&self.dir).map_err(|_| Failure::Unavailable)?;
        prepare_worker(&scratch.0, profile).map_err(|error| {
            log::warn!("Horizon graphics benchmark: couldn't prepare a trial: {error}");
            Failure::Unavailable
        })?;
        let game = horizon_std::env::current_exe().map_err(|_| Failure::Unavailable)?;
        let log = File::create(scratch.0.join("worker.log")).map_err(|_| Failure::Unavailable)?;
        let stderr = log.try_clone().map_err(|_| Failure::Unavailable)?;
        let child = Command::new(game)
            .current_dir(&scratch.0)
            .arg("--game-dir")
            .arg(&scratch.0)
            .arg("--resources")
            .arg(resources)
            .args(["--benchmark", mode.scenario(), "--no-pause"])
            .env(WORKER_ENV, "1")
            .env("HORIZON_HOME", scratch.0.join("home"))
            .env("HORIZON_LAUNCHER_HOME", scratch.0.join("launcher"))
            .env("XDG_DATA_HOME", scratch.0.join("data"))
            .env("XDG_CONFIG_HOME", scratch.0.join("config"))
            .env("HORIZON_LAUNCHER_TRASH", scratch.0.join("trash"))
            .env("HORIZON_LAUNCHER_CLIPBOARD", scratch.0.join("clipboard"))
            .env("HORIZON_LAUNCHER_DESKTOP", scratch.0.join("desktop"))
            .env("HORIZON_LAUNCHER_NO_SELF_UPDATE", "1")
            .env("HORIZON_NO_AUDIO", "1")
            .env_remove("HORIZON_GRAPHICS_API")
            .stdin(Stdio::piped())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| {
                log::warn!("Horizon graphics benchmark: couldn't start a trial: {error}");
                Failure::Unavailable
            })?;
        let deadline = match mode {
            Mode::Quick => Duration::from_secs(80),
            Mode::Thorough => Duration::from_secs(180),
        };
        let result =
            wait_for_report(Worker(child), deadline).and_then(|()| read_report(&scratch.0));
        if result.is_err() {
            // Keep both driver failures and benchmark validation errors visible after cleanup.
            if let Ok(output) = fs::read_to_string(scratch.0.join("worker.log")) {
                for line in output
                    .lines()
                    .rev()
                    .take(12)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    log::warn!("Horizon graphics benchmark worker: {line}");
                }
            }
        }
        result
    }
}

fn read_report(dir: &Path) -> Result<Report, Failure> {
    let mut reports = fs::read_dir(dir.join("benchmarks"))
        .map_err(|_| Failure::Unavailable)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"));
    let report = reports.next().ok_or(Failure::Unavailable)?;
    if reports.next().is_some() {
        return Err(Failure::Unavailable);
    }
    let contents = fs::read_to_string(report.path()).map_err(|_| Failure::Unavailable)?;
    Report::parse(&contents).map_err(|_| Failure::Unavailable)
}

fn short_hash(bytes: &[u8]) -> String {
    let key = Sha256::digest(bytes);
    key[..16]
        .iter()
        .fold(String::with_capacity(32), |mut name, byte| {
            use std::fmt::Write;
            write!(name, "{byte:02x}").expect("writing to string");
            name
        })
}

fn session_path(dir: &Path, identity: &Identity) -> PathBuf {
    let bytes = serde_json::to_vec(identity).expect("serializable benchmark identity");
    dir.join(format!("session-{}.json", short_hash(&bytes)))
}

fn undo_path(dir: &Path, options_path: &Path) -> PathBuf {
    dir.join(format!(
        "undo-{}.json",
        short_hash(options_path.to_string_lossy().as_bytes())
    ))
}

fn resolve_resources(dir: &Path, resources: PathBuf) -> PathBuf {
    let absolute = if resources.is_relative() {
        horizon_std::env::current_dir()
            .map_or_else(|_| dir.join(&resources), |cwd| cwd.join(&resources))
    } else {
        resources
    };
    fs::canonicalize(&absolute).unwrap_or(absolute)
}

fn reclaim_abandoned_trials(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_trial = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("trial-"));
        if is_trial
            && entry.path().is_dir()
            && let Err(error) = fs::remove_dir_all(entry.path())
        {
            log::warn!(
                "Horizon graphics benchmark: couldn't reclaim abandoned trial folder {}: {error}",
                entry.path().display()
            );
        }
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match File::open(path) {
        Ok(file) => serde_json::from_reader(file)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let temporary = path.with_extension("new");
    let mut file = File::create(&temporary)?;
    serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn prepare_worker(dir: &Path, profile: &Profile) -> Result<(), String> {
    use horizon_client::benchmark::{AUTOTUNE_PACK_ID, prepare_autotune_pack};
    let resources = serde_json::json!(["vanilla", AUTOTUNE_PACK_ID]);
    fs::write(
        dir.join("options.txt"),
        format!("enableVsync:false\npauseOnLostFocus:false\nresourcePacks:{resources}\n"),
    )
    .map_err(|e| e.to_string())?;
    // On macOS Auto is Metal; unlike the other desktop backends it isn't an option key.
    let api = if profile.graphics_api == "metal" {
        "auto"
    } else {
        &profile.graphics_api
    };
    fs::write(
        dir.join(horizon_client::graphics::FILE),
        format!(
            "graphicsApi:{api}\ngpuCulling:{}\nworkers:{}\nuploadBudgetKiB:{}\ntextureBudgetMiB:{}\nlodDistance:{}\npresentMode:{}\n",
            profile.gpu_culling, profile.workers, profile.upload_budget_kib,
            profile.texture_budget_mib, profile.lod_distance, profile.present_mode,
        ),
    )
    .map_err(|e| e.to_string())?;
    prepare_autotune_pack(dir)
}

/// Start before entering graphics initialization, which may hang in the driver. Keeping the
/// pipe open is the only work the parent must do to keep its specific child alive.
pub(crate) fn exit_if_supervisor_is_gone() {
    if horizon_std::env::var_os(WORKER_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return;
    }
    horizon_std::thread::spawn(|| {
        use std::io::Read;
        let mut input = io::stdin().lock();
        let mut buffer = [0_u8; 1];
        loop {
            match input.read(&mut buffer) {
                Ok(0) => std::process::exit(1),
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => std::process::exit(1),
            }
        }
    });
}

fn wait_for_report(mut worker: Worker, limit: Duration) -> Result<(), Failure> {
    let start = Instant::now();
    loop {
        match worker.0.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) | Err(_) => return Err(Failure::Crashed),
            Ok(None) if start.elapsed() >= limit => return Err(Failure::TimedOut),
            Ok(None) => horizon_std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(parent: &Path) -> io::Result<Self> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = parent.join(format!("trial-{}-{now}", std::process::id()));
        fs::create_dir(&dir)?;
        Ok(Self(dir))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            log::warn!("Horizon graphics benchmark: couldn't remove its trial folder: {error}");
        }
    }
}

static TUNING: OnceLock<Mutex<Tuning>> = OnceLock::new();

struct Tuning {
    dir: PathBuf,
    options_path: PathBuf,
    resources: PathBuf,
    identity: Identity,
    backend: String,
    current: Profile,
    vsync: bool,
    session: Option<Session>,
    undo: Option<Undo>,
    previous_hardware: bool,
    running: bool,
    next_title_frame: Instant,
    error: bool,
    view: Arc<BenchmarkView>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Undo {
    before: Profile,
    after: Profile,
}

impl Tuning {
    fn open(
        dir: PathBuf,
        options_path: PathBuf,
        resources: PathBuf,
        settings: &Settings,
    ) -> Option<Self> {
        let resources = resolve_resources(&dir, resources);
        let adapter = horizon_client::gfx::adapter()?;
        let build = option_env!("HORIZON_COMMIT").map_or_else(
            || {
                // Source builds may have no embedded revision. An executable's metadata changes
                // on rebuild, but not on restart.
                let stamp = horizon_std::env::current_exe()
                    .ok()
                    .and_then(|p| fs::metadata(p).ok())
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
                format!("{}:{stamp:?}", env!("CARGO_PKG_VERSION"))
            },
            str::to_owned,
        );
        let mut backend = adapter.backend.to_ascii_lowercase();
        if backend == "gl" {
            backend = "opengl".into();
        }
        let gpu = Gpu {
            name: adapter.name,
            vendor: adapter.vendor,
            driver: adapter.driver,
            backend: adapter.backend,
            kind: adapter.kind,
            vram_mb: None,
        };
        let identity = Identity::new(&gpu, build);
        let (session, error) = match Journal::load(&dir, &identity) {
            Ok(session) => (
                session.filter(|s| s.version() == SESSION_VERSION && s.identity() == &identity),
                false,
            ),
            Err(error) => {
                log::warn!("Horizon graphics benchmark: couldn't read saved trials: {error}");
                (None, true)
            }
        };
        let previous_hardware = session.is_none()
            && fs::read_dir(dir.join(STATE_DIR)).is_ok_and(|mut entries| {
                entries.any(|entry| {
                    entry.is_ok_and(|entry| {
                        entry.file_name().to_str().is_some_and(|name| {
                            name.starts_with("session-") && name.ends_with(".json")
                        })
                    })
                })
            });
        let undo = read_json(&undo_path(&dir.join(STATE_DIR), &options_path))
            .ok()
            .flatten();
        let mut tuning = Self {
            dir,
            options_path,
            resources,
            identity,
            backend,
            current: profile(&settings.horizon),
            vsync: settings.vsync,
            session,
            undo,
            previous_hardware,
            running: false,
            next_title_frame: Instant::now(),
            error,
            view: Arc::default(),
        };
        tuning.refresh();
        Some(tuning)
    }

    fn effective_recommendation(&self) -> Option<Profile> {
        let mut next = self
            .session
            .as_ref()
            .and_then(Session::recommendation)?
            .clone();
        // With VSync enabled the renderer forces FIFO, so skip recommending an overridden present mode.
        if self.vsync {
            next.present_mode = self.current.present_mode.clone();
        }
        Some(next)
    }

    fn refresh(&mut self) {
        let suggestion = self.effective_recommendation();
        let different = suggestion.as_ref().is_some_and(|next| {
            next != &self.current
                && !(next.graphics_api == "metal"
                    && profile_with_api(self.current.clone(), "metal") == *next)
        });
        let status = if self.error {
            "Could not access the local benchmark. Try again."
        } else if self.running {
            "Benchmark running. Close Horizon to pause it."
        } else if self.session.as_ref().is_some_and(|s| !s.is_complete()) {
            "Benchmark interrupted. It resumes at the next trial."
        } else if different {
            "Recommendation ready. Review before applying."
        } else if suggestion.is_some() {
            "Your benchmark settings are up to date."
        } else if self.session.is_some() {
            "No comparable result. See the failed trials below."
        } else if self.previous_hardware {
            "Your GPU, driver, or game build changed. Run a new benchmark."
        } else {
            "No benchmark has run on this computer."
        };
        let results = self
            .session
            .iter()
            .flat_map(Session::trials)
            .map(|trial| {
                let p = &trial.profile;
                let values = format!(
                    "{}, {}, {}, {} KiB, {} MiB, {}, {}",
                    p.graphics_api,
                    p.gpu_culling,
                    p.workers,
                    p.upload_budget_kib,
                    p.texture_budget_mib,
                    p.lod_distance,
                    p.present_mode
                );
                let candidate = match trial.group {
                    Group::GraphicsApi => p.graphics_api.clone(),
                    Group::GpuCulling => p.gpu_culling.clone(),
                    Group::Workers => p.workers.to_string(),
                    Group::Budgets => {
                        format!("{} KiB / {} MiB", p.upload_budget_kib, p.texture_budget_mib)
                    }
                    Group::FarTerrain => p.lod_distance.to_string(),
                    Group::PresentMode => p.present_mode.clone(),
                };
                BenchmarkResult {
                    group: match trial.group {
                        Group::GraphicsApi => "Graphics API",
                        Group::GpuCulling => "GPU Culling",
                        Group::Workers => "Worker Threads",
                        Group::Budgets => "Upload and Texture Budgets",
                        Group::FarTerrain => "Far Terrain",
                        Group::PresentMode => "Present Mode",
                    },
                    candidate,
                    profile: values,
                    outcome: match &trial.outcome {
                        None => BenchmarkOutcome::Pending,
                        Some(Outcome::Failed(reason)) => BenchmarkOutcome::Failed(match reason {
                            Failure::Crashed => "Crashed (skipped)",
                            Failure::TimedOut => "Timed out (skipped)",
                            Failure::Interrupted => "Interrupted (skipped)",
                            Failure::Unavailable => "Unavailable or incomplete",
                        }),
                        Some(Outcome::Measured(m)) => BenchmarkOutcome::Measured {
                            average: m.avg_fps,
                            low: m.low_1pct_fps,
                            load_ms: m.load_ms,
                            score: m.score,
                        },
                    },
                }
            })
            .collect();
        let changes = suggestion
            .as_ref()
            .filter(|_| different)
            .map_or_else(Vec::new, |next| changes(&self.current, next));
        self.view = Arc::new(BenchmarkView {
            status,
            can_start: !self.running,
            can_apply: !self.running && different,
            can_undo: !self.running
                && self
                    .undo
                    .as_ref()
                    .is_some_and(|undo| self.current == undo.after),
            results,
            changes,
        });
    }
}

fn profile(g: &Graphics) -> Profile {
    Profile {
        graphics_api: g.api.name().into(),
        gpu_culling: g.gpu_culling.name().into(),
        workers: g.workers,
        upload_budget_kib: g.upload_budget_kib,
        texture_budget_mib: g.texture_budget_mib,
        lod_distance: g.lod_distance,
        present_mode: g.present_mode.name().into(),
    }
}

fn profile_with_api(mut profile: Profile, api: &str) -> Profile {
    profile.graphics_api = api.into();
    profile
}

fn changes(before: &Profile, after: &Profile) -> Vec<BenchmarkChange> {
    let mut result = Vec::new();
    macro_rules! setting {
        ($key:expr, $field:ident) => {
            if before.$field != after.$field {
                result.push(BenchmarkChange {
                    setting: $key,
                    before: before.$field.to_string(),
                    after: after.$field.to_string(),
                });
            }
        };
    }
    if after.graphics_api != "metal" {
        setting!("Graphics API", graphics_api);
    }
    setting!("GPU Culling", gpu_culling);
    setting!("Worker Threads", workers);
    setting!("Upload Budget", upload_budget_kib);
    setting!("Texture Budget", texture_budget_mib);
    setting!("Far Terrain", lod_distance);
    setting!("Present Mode", present_mode);
    result
}

fn set_profile(g: &mut Graphics, profile: &Profile) -> bool {
    let api = if cfg!(target_os = "macos") && profile.graphics_api == "metal" {
        Some(GraphicsApi::Auto)
    } else {
        GraphicsApi::from_name(&profile.graphics_api)
    };
    let Some(api) = api.filter(|a| GraphicsApi::choices().contains(a)) else {
        return false;
    };
    let Some(culling) = GpuCullingMode::ALL
        .into_iter()
        .find(|m| m.name() == profile.gpu_culling)
    else {
        return false;
    };
    let Some(present) = PresentMode::ALL
        .into_iter()
        .find(|m| m.name() == profile.present_mode)
    else {
        return false;
    };
    g.api = api;
    g.gpu_culling = culling;
    g.present_mode = present;
    g.workers = profile.workers;
    g.lod_distance = profile.lod_distance;
    g.upload_budget_kib = profile.upload_budget_kib;
    g.texture_budget_mib = profile.texture_budget_mib;
    true
}

/// Called once a normal title-screen renderer has selected an adapter.
pub(super) fn ready_at_title(resources: &Path, settings: &Settings) {
    if TUNING.get().is_some()
        || !cfg!(feature = "horizon-extras")
        || worker()
        || horizon_std::env::var_os("HORIZON_GRAPHICS_API").is_some()
    {
        return;
    }
    let Some(tuning) = Tuning::open(
        super::Game::game_dir(),
        super::Game::options_path(),
        resources.to_owned(),
        settings,
    ) else {
        return;
    };
    if TUNING.set(Mutex::new(tuning)).is_ok() {
        let mut tuning = TUNING.get().unwrap().lock().unwrap();
        if tuning.session.as_ref().is_some_and(|s| !s.is_complete()) {
            start(&mut tuning, None);
        }
    }
}

pub(super) fn worker() -> bool {
    horizon_std::env::var_os(WORKER_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
}

pub(super) fn running() -> bool {
    TUNING
        .get()
        .is_some_and(|state| state.lock().unwrap().running)
}

/// Limit the idle title panorama while a separate process measures the GPU.
pub(super) fn title_redraw() -> Option<(bool, Instant)> {
    let mut tuning = TUNING.get()?.lock().unwrap();
    if !tuning.running {
        return None;
    }
    let now = Instant::now();
    let redraw = now >= tuning.next_title_frame;
    if redraw {
        tuning.next_title_frame = now + Duration::from_millis(125);
    }
    Some((redraw, tuning.next_title_frame))
}

pub(super) fn view(settings: &Settings) -> Option<Arc<BenchmarkView>> {
    let mut tuning = TUNING.get()?.lock().unwrap();
    let g = &settings.horizon;
    let p = &tuning.current;
    if g.api.name() != p.graphics_api
        || g.gpu_culling.name() != p.gpu_culling
        || g.workers != p.workers
        || g.upload_budget_kib != p.upload_budget_kib
        || g.texture_budget_mib != p.texture_budget_mib
        || g.lod_distance != p.lod_distance
        || g.present_mode.name() != p.present_mode
        || settings.vsync != tuning.vsync
    {
        tuning.current = profile(g);
        tuning.vsync = settings.vsync;
        tuning.refresh();
    }
    Some(Arc::clone(&tuning.view))
}

/// Only the title may start a benchmark: a loaded world would compete with the worker.
pub(super) fn action(action: BenchmarkAction, settings: &mut Settings) -> bool {
    let Some(state) = TUNING.get() else {
        return false;
    };
    let mut tuning = state.lock().unwrap();
    if tuning.running {
        return false;
    }
    match action {
        BenchmarkAction::Quick => start(&mut tuning, Some(Mode::Quick)),
        BenchmarkAction::Thorough => start(&mut tuning, Some(Mode::Thorough)),
        BenchmarkAction::Apply | BenchmarkAction::Undo => {
            tuning.vsync = settings.vsync;
            let before = profile(&settings.horizon);
            tuning.current = before.clone();
            let in_session = tuning.effective_recommendation();
            let Some(next) = (match action {
                BenchmarkAction::Apply => in_session,
                BenchmarkAction::Undo => tuning
                    .undo
                    .as_ref()
                    .filter(|u| u.after == before)
                    .map(|u| u.before.clone()),
                _ => unreachable!(),
            }) else {
                return false;
            };
            let mut g = settings.horizon.clone();
            if !set_profile(&mut g, &next) {
                return false;
            }
            let after = profile(&g);
            if before == after {
                return false;
            }
            let options_dir = tuning
                .options_path
                .parent()
                .unwrap_or(&tuning.dir)
                .to_path_buf();
            let result = Journal::lock(&tuning.dir, &tuning.identity).and_then(|journal| {
                let undo_file = undo_path(&journal.dir, &tuning.options_path);
                if action == BenchmarkAction::Apply {
                    let previous_undo = tuning.undo.clone();
                    let undo = Some(Undo { before, after });
                    write_json(&undo_file, &undo)?;
                    if let Err(error) = g.try_save(&options_dir) {
                        let _ = write_json(&undo_file, &previous_undo);
                        return Err(error);
                    }
                    tuning.undo = undo;
                } else {
                    // Keep the undo record durable until the restored graphics file is written.
                    g.try_save(&options_dir)?;
                    write_json(&undo_file, &None::<Undo>)?;
                    tuning.undo = None;
                }
                Ok(())
            });
            if let Err(error) = result {
                tuning.error = true;
                tuning.refresh();
                log::warn!("Horizon graphics benchmark: couldn't save the undo state: {error}");
                return false;
            }
            settings.horizon = g;
            tuning.current = profile(&settings.horizon);
            tuning.error = false;
            tuning.refresh();
            return true;
        }
    }
    false
}

fn start(tuning: &mut Tuning, mode: Option<Mode>) {
    let setup = (|| {
        let journal = Journal::lock(&tuning.dir, &tuning.identity)?;
        let current = match Journal::load(&tuning.dir, &tuning.identity) {
            Ok(session) => session,
            Err(error) if error.kind() == io::ErrorKind::InvalidData && mode.is_some() => {
                let stamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let backup = journal.path.with_extension(format!("corrupt-{stamp}.json"));
                let _ = fs::rename(&journal.path, &backup);
                log::warn!(
                    "Horizon graphics benchmark: preserved invalid journal at {}: {error}",
                    backup.display()
                );
                None
            }
            Err(error) => return Err(error),
        };
        let session = match current {
            Some(session) if session.version() == SESSION_VERSION && !session.is_complete() => {
                session
            }
            old => {
                let Some(mode) = mode else { return Ok(None) };
                let choices: Vec<&str> = std::iter::once(tuning.backend.as_str())
                    .chain(GraphicsApi::choices().iter().map(|api| api.name()))
                    .filter(|api| *api != "auto")
                    .collect();
                let workers =
                    horizon_std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
                let mut session = Session::new(
                    tuning.identity.clone(),
                    tuning.current.clone(),
                    mode,
                    &choices,
                    workers,
                );
                if let Some(old) = old {
                    session.reuse_exclusions(&old);
                }
                session
            }
        };
        Ok::<_, io::Error>(Some((journal, session)))
    })();
    match setup {
        Ok(Some((journal, session))) => {
            tuning.running = true;
            tuning.error = false;
            let resources = tuning.resources.clone();
            match horizon_std::thread::Builder::new()
                .name("Graphics benchmark".into())
                .spawn(move || {
                    if let Err(error) = run(journal, session, &resources) {
                        log::warn!("Horizon graphics benchmark: couldn't save a trial: {error}");
                        let mut tuning = TUNING.get().unwrap().lock().unwrap();
                        tuning.error = true;
                        tuning.running = false;
                        tuning.refresh();
                    }
                }) {
                Ok(_) => {}
                Err(error) => {
                    log::warn!("Horizon graphics benchmark: couldn't start supervisor: {error}");
                    tuning.error = true;
                    tuning.running = false;
                }
            }
        }
        Ok(None) => return,
        Err(error) => {
            log::warn!("Horizon graphics benchmark: couldn't open its journal: {error}");
            tuning.error = true;
        }
    }
    tuning.refresh();
}

fn run(journal: Journal, mut session: Session, resources: &Path) -> io::Result<()> {
    if session.fail_pending(Failure::Interrupted) {
        journal.save(&session)?;
    }
    while let Some(trial) = session.begin_trial() {
        let candidate = trial.profile.clone();
        // If checkpointing fails, never enter an unrecorded driver or renderer configuration.
        journal.save(&session)?;
        publish_session(&session, true);
        match journal.run_trial(resources, session.mode(), &candidate) {
            Ok(report) => {
                session.finish_report(&report);
            }
            Err(failure) => {
                session.fail_pending(failure);
            }
        }
        journal.save(&session)?;
    }
    journal.save(&session)?;
    drop(journal);
    publish_session(&session, false);
    Ok(())
}

fn publish_session(session: &Session, running: bool) {
    let mut tuning = TUNING.get().unwrap().lock().unwrap();
    tuning.session = Some(session.clone());
    tuning.running = running;
    tuning.refresh();
}
