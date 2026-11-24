//! Recording the session as a client mod starts, stops and reads it (`recording_start`...).

/// How the session's recording stands, as `recording_status` reads it this frame.
/// Mods receive this snapshot from the game; they do not construct it.
/// `#[non_exhaustive]` is for Rust callers only: on the wire every field is encoded and its
/// layout hashed, so adding one later breaks wasm mods built against this one
/// (docs/modding/src/concepts/wire-types.md).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
#[derive(horizon_mod_api_macros::Wire)]
pub struct RecordingStatus {
    /// The recorder is taking packets. False with no recording, and once the recorder has
    /// failed (`failed`).
    pub active: bool,
    /// The recording being written, as the id `replay_open` takes: its file name in the
    /// recordings folder without `.mcpr`. Empty while there is no recording, and for one no
    /// id can name (started by `/studio record start` with a name holding a character an id
    /// refuses). The file itself appears when the recording ends. While `finalizing` and no
    /// other recording runs, it names the recording being finished.
    pub recording: String,
    /// How long the recording has run, in ms, counting on after a failure. 0 with no recording.
    pub elapsed_ms: u64,
    /// The bytes written so far (they stop growing once the recorder fails). 0 with no recording.
    pub bytes: u64,
    /// The recorder stopped by itself on an error (the disk refused a write, or fell too far
    /// behind) and the recording has not been stopped since: `recording_stop` ends it and
    /// answers `DiskError`.
    pub failed: bool,
    /// A recording `recording_stop` ended is still having its archive written in the
    /// background: its file is not in the recordings folder yet, and `recording_start` answers
    /// `Finalizing`. False once the archive is in place, or its writing failed (the game logs
    /// that; the recording then has no file).
    pub finalizing: bool,
}

/// Why a recording call changed nothing.
/// `#[non_exhaustive]` is for Rust callers only: on the wire every variant is encoded and its
/// layout hashed, so adding one later breaks wasm mods built against this one
/// (docs/modding/src/concepts/wire-types.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
#[derive(horizon_mod_api_macros::Wire)]
pub enum RecordingError {
    /// A recording is already being written, by this mod, another mod or `/studio record`.
    AlreadyRecording,
    /// There is no world session to record: the title screen, a menu with no world behind it,
    /// or a replay playing.
    NotConnected,
    /// No recording is running to stop.
    NotRecording,
    /// The recording's file could not be created, or the recorder had failed by the time
    /// `recording_stop` ended it. The recording is over, and its file may be incomplete or
    /// missing.
    DiskError,
    /// The archive of the recording `recording_stop` ended is still being written
    /// (`RecordingStatus::finalizing`): `recording_start` changed nothing, try again once the
    /// status no longer says so.
    Finalizing,
}
