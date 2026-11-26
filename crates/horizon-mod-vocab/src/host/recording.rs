//! Host method declarations; access defaults and game forwarders are generated.
#![allow(unused_variables)]
use crate::*;
/// The client host's methods of the `recording` area, implemented by the game and reached through [`crate::ClientHost`].
#[horizon_mod_api_macros::host_area]
pub trait RecordingClientHost {
    /// Side: client.
    /// How the session's recording stands now, whoever started it (a mod or `/studio record`):
    /// all fields empty or false while no recording runs; `finalizing` says a stopped recording's
    /// archive is still being written.
    #[access(ClientSetup)]
    fn recording_status(&self) -> RecordingStatus;
    /// Side: client.
    /// Starts recording the world session's packets, as `/studio record start` does: the
    /// platform picks the folder (the game folder's `recordings/`) and the file name, which
    /// begins with `name` when one is given (its letters, digits, `-` and `_` kept, whitespace
    /// made `_`, at most 80 characters) and ends with the time and a counter, so it is never a
    /// path and never names another recording. What the recording keeps is the platform's
    /// privacy filter, unchanged. Answers the recording's id, the one `recording_stop` gives and
    /// `replay_open` takes once the recording has ended.
    ///
    /// One recording runs at a time: `AlreadyRecording` while one does, a recorder that has
    /// failed (`RecordingStatus::failed`) excepted, which a new recording replaces. `Finalizing`
    /// while the archive of the recording `recording_stop` ended is still being written
    /// (`RecordingStatus::finalizing`; checked before `AlreadyRecording`): wait for the status to
    /// stop saying so. `NotConnected` with no world session, or while a replay plays.
    /// `DiskError` when the file cannot be created. Packaging recordings an earlier crash left
    /// behind runs in the background and never delays this call.
    #[access(WorldOnly)]
    fn recording_start(&mut self, name: Option<&str>) -> Result<String, RecordingError>;
    /// Side: client.
    /// Stops the recording, whoever started it, and answers at once with its id, as `replay_open`
    /// takes it (empty for a recording started by `/studio record start` under a name no id can
    /// hold). The `.mcpr` archive is written in the background: `RecordingStatus::finalizing` is
    /// true until it is in the recordings folder, and `replay_open` finds it only then. An archive
    /// that cannot be finished is logged by the game and leaves no file; the status then stops
    /// saying `finalizing`.
    /// `NotRecording` while no recording runs, `NotConnected` with no world session or, when no
    /// recording runs, while a replay plays (as `recording_start` answers during a replay),
    /// `DiskError` when the recorder had failed (the recording is over all the same).
    #[access(WorldOnly)]
    fn recording_stop(&mut self) -> Result<String, RecordingError>;
}
