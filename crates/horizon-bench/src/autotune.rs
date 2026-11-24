//! Local graphics trial planning. The caller persists the session before running each trial.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::{Frames, Gpu, Kind, Report};

pub const SESSION_VERSION: u32 = 1;
pub const WORKLOAD_REVISION: u32 = 1;
pub const SCENES: [&str; 7] = [
    "dense-terrain",
    "far-terrain",
    "entities",
    "block-entities",
    "particles-weather",
    "resource-pack",
    "ui",
];
pub const LOAD_METRIC: &str = "startup.world_ms";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub graphics_api: String,
    pub gpu_culling: String,
    pub workers: u32,
    pub upload_budget_kib: u32,
    pub texture_budget_mib: u32,
    pub lod_distance: u32,
    pub present_mode: String,
}

impl Profile {
    fn with_modes(&self, graphics_api: &str, gpu_culling: &str, present_mode: &str) -> Self {
        Self {
            graphics_api: graphics_api.into(),
            gpu_culling: gpu_culling.into(),
            workers: self.workers,
            upload_budget_kib: self.upload_budget_kib,
            texture_budget_mib: self.texture_budget_mib,
            lod_distance: self.lod_distance,
            present_mode: present_mode.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub gpu_vendor: String,
    pub gpu_name: String,
    #[serde(default)]
    pub gpu_backend: String,
    pub driver: String,
    pub game_build: String,
    pub workload_revision: u32,
}

impl Identity {
    pub fn new(gpu: &Gpu, game_build: impl Into<String>) -> Self {
        Self {
            gpu_vendor: gpu.vendor.clone(),
            gpu_name: gpu.name.clone(),
            gpu_backend: gpu.backend.to_ascii_lowercase(),
            driver: gpu.driver.clone(),
            game_build: game_build.into(),
            workload_revision: WORKLOAD_REVISION,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Quick,
    Thorough,
}

impl Mode {
    pub fn scenario(self) -> &'static str {
        match self {
            Self::Quick => "autotune-quick",
            Self::Thorough => "autotune-thorough",
        }
    }

    pub fn repeats(self) -> u32 {
        match self {
            Self::Quick => 1,
            Self::Thorough => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    GraphicsApi,
    GpuCulling,
    Workers,
    Budgets,
    FarTerrain,
    PresentMode,
}

impl Group {
    fn next(self) -> Option<Self> {
        Some(match self {
            Self::GraphicsApi => Self::GpuCulling,
            Self::GpuCulling => Self::Workers,
            Self::Workers => Self::Budgets,
            Self::Budgets => Self::FarTerrain,
            Self::FarTerrain => Self::PresentMode,
            Self::PresentMode => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Crashed,
    TimedOut,
    Unavailable,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "result", rename_all = "snake_case")]
pub enum Outcome {
    Measured(Measurement),
    Failed(Failure),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub avg_fps: f64,
    pub low_1pct_fps: f64,
    pub load_ms: f64,
    /// Higher is better. The first valid trial scores 1; ties keep the earlier trial.
    pub score: f64,
    // In SCENES order, retained so even a resumed run compares each scene to the same reference.
    scenes: [SceneMeasurement; SCENES.len()],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct SceneMeasurement {
    avg_fps: f64,
    low_1pct_fps: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trial {
    pub group: Group,
    pub profile: Profile,
    /// `None` marks the candidate in flight, not a candidate waiting to run.
    pub outcome: Option<Outcome>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    version: u32,
    identity: Identity,
    base: Profile,
    mode: Mode,
    available_workers: u32,
    group: Option<Group>,
    queued: VecDeque<Profile>,
    trials: Vec<Trial>,
    #[serde(default)]
    excluded: Vec<Profile>,
}

impl Session {
    /// Supply explicit API names in preference order. A supported API already selected in `base`
    /// goes first. `auto` is not a distinct backend and never enters the comparison.
    pub fn new(
        identity: Identity,
        base: Profile,
        mode: Mode,
        supported_apis: &[&str],
        available_workers: u32,
    ) -> Self {
        let mut queued: VecDeque<Profile> = VecDeque::new();
        let apis = std::iter::once(base.graphics_api.as_str())
            .filter(|api| supported_apis.contains(api))
            .chain(supported_apis.iter().copied());
        for api in apis {
            if backend(api).is_some() && !queued.iter().any(|p| p.graphics_api == api) {
                queued.push_back(base.with_modes(api, &base.gpu_culling, &base.present_mode));
            }
        }
        Self {
            version: SESSION_VERSION,
            identity,
            base,
            mode,
            available_workers,
            group: (!queued.is_empty()).then_some(Group::GraphicsApi),
            queued,
            trials: Vec::new(),
            excluded: Vec::new(),
        }
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn base(&self) -> &Profile {
        &self.base
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn trials(&self) -> &[Trial] {
        &self.trials
    }

    pub fn is_complete(&self) -> bool {
        self.group.is_none()
    }

    pub fn pending(&self) -> Option<&Trial> {
        self.trials.last().filter(|trial| trial.outcome.is_none())
    }

    /// A new run on the same GPU, driver and build does not revisit combinations that failed.
    pub fn reuse_exclusions(&mut self, previous: &Self) {
        if self.identity != previous.identity {
            return;
        }
        self.excluded.clone_from(&previous.excluded);
        for trial in &previous.trials {
            if matches!(
                trial.outcome,
                Some(Outcome::Failed(
                    Failure::Crashed | Failure::TimedOut | Failure::Interrupted
                ))
            ) && !self.excluded.contains(&trial.profile)
            {
                self.excluded.push(trial.profile.clone());
            }
        }
    }

    /// Marks the next candidate pending. Persist this session before launching the child.
    pub fn begin_trial(&mut self) -> Option<&Trial> {
        let mut group = self.group?;
        if self.pending().is_some() {
            return None;
        }
        while self
            .queued
            .front()
            .is_none_or(|profile| self.excluded.contains(profile))
        {
            if self.queued.pop_front().is_some() {
                continue;
            }
            self.advance();
            group = self.group?;
        }
        self.trials.push(Trial {
            group,
            profile: self.queued.pop_front()?,
            outcome: None,
        });
        self.pending()
    }

    /// Consumes a pending trial, recording `Unavailable` if the report is not comparable.
    /// Returns false only if nothing was pending.
    pub fn finish_report(&mut self, report: &Report) -> bool {
        let Some(trial) = self.pending() else {
            return false;
        };
        let reference = self.trials.iter().find_map(measured);
        let outcome =
            Measurement::from_report(report, self.mode, &trial.profile, reference, &self.identity)
                .map_or(Outcome::Failed(Failure::Unavailable), Outcome::Measured);
        self.finish(outcome)
    }

    /// On recovery, use `Interrupted` for a saved trial whose outcome isn't known.
    pub fn fail_pending(&mut self, reason: Failure) -> bool {
        self.finish(Outcome::Failed(reason))
    }

    pub fn recommendation(&self) -> Option<&Profile> {
        if !self.is_complete() {
            return None;
        }
        self.best().map(|trial| &trial.profile)
    }

    fn best(&self) -> Option<&Trial> {
        self.trials
            .iter()
            .filter_map(|trial| measured(trial).map(|m| (trial, m.score)))
            .reduce(|best, next| if next.1 > best.1 { next } else { best })
            .map(|(trial, _)| trial)
    }

    fn finish(&mut self, outcome: Outcome) -> bool {
        let Some(trial) = self.trials.last_mut().filter(|t| t.outcome.is_none()) else {
            return false;
        };
        trial.outcome = Some(outcome);
        if self.queued.is_empty() {
            self.advance();
        }
        true
    }

    fn advance(&mut self) {
        // Without a usable result yet, still try the next group on the first explicit API:
        // e.g. a crashing culling setting might have defeated the whole API group.
        let anchor = self.best().or_else(|| self.trials.first()).map_or_else(
            || {
                self.base.with_modes(
                    if self.base.graphics_api == "auto" {
                        self.excluded
                            .first()
                            .map_or("auto", |profile| profile.graphics_api.as_str())
                    } else {
                        &self.base.graphics_api
                    },
                    &self.base.gpu_culling,
                    &self.base.present_mode,
                )
            },
            |trial| trial.profile.clone(),
        );
        let mut next = self.group.and_then(Group::next);
        while let Some(group) = next {
            let queued = self.alternatives(group, &anchor);
            if !queued.is_empty() {
                self.queued = queued;
                break;
            }
            next = group.next();
        }
        self.group = next;
    }

    fn alternatives(&self, group: Group, base: &Profile) -> VecDeque<Profile> {
        let thorough = self.mode == Mode::Thorough;
        let mut profiles = VecDeque::new();
        match group {
            Group::GraphicsApi => {}
            Group::GpuCulling => {
                for gpu_culling in ["off", "on", "auto"] {
                    if gpu_culling != base.gpu_culling {
                        profiles.push_back(base.with_modes(
                            &base.graphics_api,
                            gpu_culling,
                            &base.present_mode,
                        ));
                    }
                }
            }
            Group::Workers => {
                // Graphics::parse caps explicit workers at 64. Auto still leaves room for
                // rendering; sample a smaller pool, and widen the search in Thorough.
                let max = self.available_workers.min(64);
                let counts = [0, max.div_ceil(2), max.saturating_sub(1), max];
                for (i, &workers) in counts[..if thorough { 4 } else { 2 }].iter().enumerate() {
                    if workers != base.workers && !counts[..i].contains(&workers) {
                        profiles.push_back(Profile {
                            workers,
                            ..base.clone()
                        });
                    }
                }
            }
            Group::Budgets => {
                let budgets = [(0, 0), (4096, 512), (2048, 128), (8192, 1024)];
                for &(upload_budget_kib, texture_budget_mib) in
                    &budgets[..if thorough { 4 } else { 2 }]
                {
                    if (upload_budget_kib, texture_budget_mib)
                        != (base.upload_budget_kib, base.texture_budget_mib)
                    {
                        profiles.push_back(Profile {
                            upload_budget_kib,
                            texture_budget_mib,
                            ..base.clone()
                        });
                    }
                }
            }
            Group::FarTerrain => {
                let distances = [0, 64, 32, 128];
                for &lod_distance in &distances[..if thorough { 4 } else { 2 }] {
                    if lod_distance != base.lod_distance {
                        profiles.push_back(Profile {
                            lod_distance,
                            ..base.clone()
                        });
                    }
                }
            }
            Group::PresentMode => {
                let modes = ["auto", "mailbox", "immediate", "fifo"];
                for &present_mode in &modes[..if thorough { 4 } else { 3 }] {
                    if present_mode != base.present_mode {
                        profiles.push_back(base.with_modes(
                            &base.graphics_api,
                            &base.gpu_culling,
                            present_mode,
                        ));
                    }
                }
            }
        }
        profiles
    }
}

fn measured(trial: &Trial) -> Option<&Measurement> {
    match trial.outcome.as_ref()? {
        Outcome::Measured(m) => Some(m),
        Outcome::Failed(_) => None,
    }
}

fn backend(api: &str) -> Option<&str> {
    match api {
        "vulkan" | "dx12" | "metal" => Some(api),
        "opengl" => Some("gl"),
        _ => None,
    }
}

fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

impl Measurement {
    fn from_report(
        report: &Report,
        mode: Mode,
        profile: &Profile,
        reference: Option<&Self>,
        identity: &Identity,
    ) -> Option<Self> {
        let repeats = mode.repeats() as usize;
        let gpu = report.hardware.gpu.as_ref()?;
        let same_backend = identity.gpu_backend.is_empty()
            || gpu.backend.eq_ignore_ascii_case(&identity.gpu_backend);
        if report.kind != Kind::Game
            || report.conditions.scenario.as_deref() != Some(mode.scenario())
            || report
                .conditions
                .repeats
                .is_some_and(|n| n != mode.repeats())
            || gpu.backend != backend(&profile.graphics_api)?
            || !gpu
                .name
                .trim()
                .eq_ignore_ascii_case(identity.gpu_name.trim())
            || (same_backend
                && !identity.driver.trim().is_empty()
                && !gpu
                    .driver
                    .trim()
                    .eq_ignore_ascii_case(identity.driver.trim()))
            || report.frames.len() != SCENES.len() * repeats
        {
            return None;
        }

        let load = report.metrics.get(LOAD_METRIC)?;
        if load.n != 1 || !positive(load.mean) {
            return None;
        }

        let mut samples = [[None::<&Frames>; 2]; SCENES.len()];
        for frame in &report.frames {
            let scene = SCENES
                .iter()
                .position(|&name| name == frame.phase.as_str())?;
            let repeat = frame.repeat as usize;
            if repeat >= repeats
                || samples[scene][repeat].is_some()
                || frame.frames == 0
                || !positive(frame.seconds)
                || !positive(frame.avg_fps)
                || !positive(frame.low_1pct_fps)
            {
                return None;
            }
            samples[scene][repeat] = Some(frame);
        }
        let mut scenes = [SceneMeasurement::default(); SCENES.len()];
        for (scene, frames) in scenes.iter_mut().zip(&samples) {
            for frame in &frames[..repeats] {
                let frame = frame.as_ref()?;
                scene.avg_fps += frame.avg_fps / repeats as f64;
                scene.low_1pct_fps += frame.low_1pct_fps / repeats as f64;
            }
        }

        let count = SCENES.len() as f64;
        let avg_fps = scenes.iter().map(|s| s.avg_fps / count).sum();
        let low_1pct_fps = scenes.iter().map(|s| s.low_1pct_fps / count).sum();
        let score = reference.map_or(1.0, |reference| {
            // Equal geometric weights for FPS, 1% low and load (inverted). Normalize each
            // scene before combining so a fast scene can't drown out a slow one.
            let frame_gain = scenes
                .iter()
                .zip(&reference.scenes)
                .map(|(s, r)| {
                    (s.avg_fps.ln() - r.avg_fps.ln()) + (s.low_1pct_fps.ln() - r.low_1pct_fps.ln())
                })
                .sum::<f64>()
                / count;
            ((frame_gain + reference.load_ms.ln() - load.mean.ln()) / 3.0).exp()
        });
        (positive(avg_fps) && positive(low_1pct_fps) && positive(score)).then_some(Self {
            avg_fps,
            low_1pct_fps,
            load_ms: load.mean,
            score,
            scenes,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{App, Conditions, Hardware, FORMAT, VERSION};

    use super::*;

    fn session(mode: Mode) -> Session {
        let base = Profile {
            graphics_api: "auto".into(),
            gpu_culling: "off".into(),
            workers: 0,
            upload_budget_kib: 0,
            texture_budget_mib: 0,
            lod_distance: 0,
            present_mode: "auto".into(),
        };
        Session::new(
            Identity::new(&Gpu::default(), "build"),
            base,
            mode,
            &["vulkan", "opengl", "auto", "vulkan"],
            8,
        )
    }

    fn report(mode: Mode, backend: &str, frame_ms: f32) -> Report {
        let mut report = Report {
            format: FORMAT.into(),
            version: VERSION,
            kind: Kind::Game,
            created: String::new(),
            app: App::default(),
            hardware: Hardware {
                gpu: Some(Gpu {
                    backend: backend.into(),
                    ..Gpu::default()
                }),
                ..Hardware::default()
            },
            conditions: Conditions {
                scenario: Some(mode.scenario().into()),
                repeats: Some(mode.repeats()),
                ..Conditions::default()
            },
            metrics: Default::default(),
            frames: Vec::new(),
            ab: None,
            label: None,
            local: Vec::new(),
        };
        report.metric(LOAD_METRIC, &[500.0]);
        for scene in SCENES {
            for repeat in 0..mode.repeats() {
                report
                    .frames
                    .push(Frames::of(scene, repeat, &[frame_ms; 8], &[], &[]).unwrap());
            }
        }
        report
    }

    #[test]
    fn resumes_an_interruption_and_recommends_only_complete_measurements() {
        let mut session = session(Mode::Quick);
        assert_eq!(
            session.begin_trial().unwrap().profile.graphics_api,
            "vulkan"
        );
        let saved = serde_json::to_string(&session).unwrap();
        let mut session: Session = serde_json::from_str(&saved).unwrap();
        assert!(session.begin_trial().is_none());
        assert!(session.fail_pending(Failure::Interrupted));
        assert_eq!(
            session.trials()[0].outcome,
            Some(Outcome::Failed(Failure::Interrupted))
        );

        let next = session.begin_trial().unwrap();
        assert_eq!(next.group, Group::GraphicsApi);
        assert_eq!(next.profile.graphics_api, "opengl");
        session.finish_report(&report(Mode::Quick, "gl", 20.0));
        let culling = session.begin_trial().unwrap();
        assert_eq!(culling.group, Group::GpuCulling);
        assert_eq!(culling.profile.gpu_culling, "on");
        session.finish_report(&report(Mode::Quick, "gl", 10.0));

        let culling = session.begin_trial().unwrap();
        assert_eq!(culling.profile.gpu_culling, "auto");
        let mut incomplete = report(Mode::Quick, "gl", 2.0);
        incomplete.frames.pop();
        session.finish_report(&incomplete);
        assert_eq!(
            session.trials()[3].outcome,
            Some(Outcome::Failed(Failure::Unavailable))
        );
        assert!(session.recommendation().is_none());

        let workers = session.begin_trial().unwrap();
        assert_eq!(workers.group, Group::Workers);
        assert_eq!(workers.profile.gpu_culling, "on");
        session.fail_pending(Failure::TimedOut);
        while session.begin_trial().is_some() {
            session.fail_pending(Failure::Unavailable);
        }
        let best = session.recommendation().unwrap();
        assert_eq!(best.graphics_api, "opengl");
        assert_eq!(best.gpu_culling, "on");
    }

    #[test]
    fn rejects_fallback_backends_and_noncomparable_repeats() {
        let mut session = session(Mode::Thorough);
        session.begin_trial().unwrap();
        session.finish_report(&report(Mode::Thorough, "gl", 10.0));
        assert_eq!(
            session.trials()[0].outcome,
            Some(Outcome::Failed(Failure::Unavailable))
        );

        session.begin_trial().unwrap();
        let mut duplicate = report(Mode::Thorough, "gl", 10.0);
        duplicate.frames[1].repeat = 0;
        session.finish_report(&duplicate);
        assert_eq!(
            session.trials()[1].outcome,
            Some(Outcome::Failed(Failure::Unavailable))
        );
        while session.begin_trial().is_some() {
            session.fail_pending(Failure::Crashed);
        }
        assert!(session.is_complete());
        assert!(session.recommendation().is_none());
    }
}
