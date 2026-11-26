//! A client mod starting, stopping and reading the recording of the world session's packets
//! (#19244): the platform's `/studio record`, asked for by a mod. The folder and the file name
//! are the platform's (`recordings_dir`, `generate_recording_filename`), the packets kept are
//! the recorder's privacy filter, and one recording runs at a time, whoever started it.
use super::Game;
use super::replay::StudioRecordError;
use horizon_client::file_id::{reparse_point, valid_id};
use horizon_client::studio::{RecorderStatus, generate_recording_filename};
use mod_api::{RecordingError, RecordingStatus};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A recording `recording_stop` ended, whose archive is being written on a thread of its own.
pub(super) struct Finalizing {
    id: String,
    done: Arc<AtomicBool>,
}

impl Finalizing {
    fn running(&self) -> bool {
        !self.done.load(Ordering::Acquire)
    }
}

/// Marks the archive finished when dropped: after it was written or failed, and also when the
/// thread that would write it never started, so a status never says `finalizing` for good.
struct DoneOnDrop(Arc<AtomicBool>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The id the recording at `path` goes by, as `replay_open` takes it: its file name without
/// `.mcpr`; empty when no id can hold that name.
pub(super) fn recording_id_of(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".mcpr"))
        .filter(|id| valid_id(id))
        .map(str::to_owned)
        .unwrap_or_default()
}

/// What a mod reads of the recorder `status` says (none: no recording) and of the archive being
/// finished in the background (`finalizing`: its id), which names the recording while no other
/// runs.
pub(super) fn status_of(
    status: Option<&RecorderStatus>,
    finalizing: Option<&str>,
) -> RecordingStatus {
    let Some(status) = status else {
        let id = finalizing.unwrap_or_default().to_owned();
        return mod_api::__private::built::recording_status(
            false,
            id,
            0,
            0,
            false,
            finalizing.is_some(),
        );
    };
    mod_api::__private::built::recording_status(
        status.active,
        recording_id_of(&status.path),
        u64::try_from(status.elapsed.as_millis()).unwrap_or(u64::MAX),
        status.bytes_written,
        status.error.is_some(),
        finalizing.is_some(),
    )
}

/// A `recordings/` folder that is itself a link (a symlink, or a junction on Windows) is
/// allowed, a player may keep recordings on another drive: the log names where they go.
fn log_linked_folder(dir: &Path) {
    let linked = horizon_std::fs::symlink_metadata(dir)
        .is_ok_and(|meta| meta.file_type().is_symlink() || reparse_point(&meta));
    if linked && let Ok(real) = horizon_std::fs::canonicalize(dir) {
        log::info!(
            "A mod's recording goes to {}, where {} links to",
            real.display(),
            dir.display()
        );
    }
}

impl Game<'_> {
    /// How the session's recording stands, whoever started it.
    pub(super) fn recording_status(&self) -> RecordingStatus {
        let status = self
            .remote
            .as_ref()
            .and_then(|remote| remote.session.recorder_status());
        let finalizing = self
            .replay
            .finalizing
            .as_ref()
            .filter(|finalizing| finalizing.running());
        status_of(
            status.as_ref(),
            finalizing.map(|finalizing| finalizing.id.as_str()),
        )
    }

    /// Starts recording the world session into the recordings folder, under a file name the
    /// platform makes from `name`; the recording's id.
    pub(super) fn recording_start(&mut self, name: Option<&str>) -> Result<String, RecordingError> {
        // A replay whose session has gone is no replay playing.
        self.drop_stale_replay();
        if self.replay.playing() {
            return Err(RecordingError::NotConnected);
        }
        // Another recording could be made beside the one being finished, but the status could
        // then not tell the two apart: a mod waits for the archive instead.
        if self
            .replay
            .finalizing
            .as_ref()
            .is_some_and(Finalizing::running)
        {
            return Err(RecordingError::Finalizing);
        }
        let file_name = generate_recording_filename(name);
        let dir = self.recordings_dir();
        match self.begin_studio_recording(&dir, &file_name) {
            Ok(path) => {
                log_linked_folder(&dir);
                Ok(recording_id_of(&path))
            }
            Err(StudioRecordError::NoSession) => Err(RecordingError::NotConnected),
            Err(StudioRecordError::Active) => Err(RecordingError::AlreadyRecording),
            Err(StudioRecordError::Disk(error)) => {
                log::warn!("A mod's recording could not be started: {error}");
                Err(RecordingError::DiskError)
            }
        }
    }

    /// Ends the recording, whoever started it, and answers its id at once: the archive is written
    /// on a thread of its own, while `recording_status` says `finalizing`.
    pub(super) fn recording_stop(&mut self) -> Result<String, RecordingError> {
        let Some(remote) = self.remote.as_ref() else {
            return Err(RecordingError::NotConnected);
        };
        let done = Arc::new(AtomicBool::new(false));
        let marker = DoneOnDrop(done.clone());
        let stopped = remote.session.stop_recording_in_background(move |result| {
            let _marker = marker;
            match result {
                Ok(path) => log::info!("A mod's recording was written to {}", path.display()),
                Err(error) => log::warn!("A mod's recording could not be finished: {error}"),
            }
        });
        match stopped {
            Some(status) => {
                let id = recording_id_of(&status.path);
                self.replay.finalizing = Some(Finalizing {
                    id: id.clone(),
                    done,
                });
                match status.error {
                    Some(error) => {
                        log::warn!("A mod's recording ended with an error: {error}");
                        Err(RecordingError::DiskError)
                    }
                    None => Ok(id),
                }
            }
            // A replay plays no recorder (checked after, so a running recorder stays stoppable).
            None if self.replay.playing() => Err(RecordingError::NotConnected),
            None => Err(RecordingError::NotRecording),
        }
    }
}
