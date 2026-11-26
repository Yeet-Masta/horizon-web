//! A client mod starts, stops and reads the recording of the world session (#19244).
use super::native_recording::{recording_id_of, status_of};
use super::native_replay::recording_path;
use horizon_client::file_id::valid_id;
use horizon_client::studio::{RecorderStatus, generate_recording_filename};
use horizon_std::time::Duration;
use mod_api::RecordingError;
use std::path::{Path, PathBuf};

/// The platform makes the file name from the mod's `name`: only letters, digits, `-` and `_`
/// survive (whitespace becomes `_`, at most 80 characters), so a name is never a path, and the
/// file name's stem is always an id `replay_open` takes.
#[test]
fn a_recording_name_never_escapes_the_recordings_folder() {
    let long = "a".repeat(200);
    let cases = [
        (Some("my trip!"), "my_trip_"),
        (Some("../../etc/passwd"), "etcpasswd_"),
        (Some("C:\\temp\\x"), "Ctempx_"),
        (Some("CON"), "CON_"),
        (Some("!!!"), "recording_"),
        (Some("  "), "recording_"),
        (Some(""), "recording_"),
        (None, "recording_"),
    ];
    for (name, prefix) in cases {
        let file = generate_recording_filename(name);
        let id = file.strip_suffix(".mcpr").expect("an .mcpr file");
        assert!(id.starts_with(prefix), "{name:?} made {file}");
        assert!(valid_id(id), "{name:?} made {file}");
        assert_eq!(
            recording_id_of(&Path::new("recordings").join(&file)),
            id,
            "{name:?}"
        );
    }
    let file = generate_recording_filename(Some(long.as_str()));
    assert!(
        file.starts_with(&format!("{}_", "a".repeat(80))),
        "at most 80 characters of the name: {file}"
    );
}

/// Two recordings started in the same millisecond still get two files.
#[test]
fn two_recordings_of_one_name_get_two_files() {
    let first = generate_recording_filename(Some("trip"));
    let second = generate_recording_filename(Some("trip"));
    assert_ne!(first, second);
}

fn recorder_status(active: bool, path: &str, error: Option<&str>) -> RecorderStatus {
    RecorderStatus {
        active,
        path: PathBuf::from(path),
        elapsed: Duration::from_millis(2_500),
        packet_count: 7,
        bytes_written: 4_096,
        error: error.map(str::to_owned),
    }
}

/// The status a mod reads: the id, the time in ms and the bytes written; `failed` only for a
/// recorder that stopped on an error; empty when no recording runs.
#[test]
fn the_status_says_what_the_recorder_says() {
    let running = status_of(
        Some(&recorder_status(
            true,
            "recordings/trip_2026-11-22_10-00-00_123_4.mcpr",
            None,
        )),
        None,
    );
    assert!(running.active && !running.failed);
    assert_eq!(running.recording, "trip_2026-11-22_10-00-00_123_4");
    assert_eq!((running.elapsed_ms, running.bytes), (2_500, 4_096));

    let failed = status_of(
        Some(&recorder_status(
            false,
            "recordings/trip.mcpr",
            Some("recorder queue is full; capture stopped"),
        )),
        None,
    );
    assert!(!failed.active && failed.failed);
    assert_eq!(failed.recording, "trip");

    // A name no id can hold (`/studio record start` takes any word): no id, still a recording.
    let odd = status_of(
        Some(&recorder_status(true, "recordings/a:b_1.mcpr", None)),
        None,
    );
    assert!(odd.active);
    assert_eq!(odd.recording, "");

    let none = status_of(None, None);
    assert!(!none.active && !none.failed && !none.finalizing);
    assert_eq!(
        (none.recording.as_str(), none.elapsed_ms, none.bytes),
        ("", 0, 0)
    );
}

/// An archive being written after `recording_stop` shows as `finalizing`, under its id while no
/// other recording runs; beside a running one the status is that recording's.
#[test]
fn the_status_says_while_an_archive_is_being_finalized() {
    let finalizing = status_of(None, Some("trip_2026-11-22_10-00-00_123_4"));
    assert!(finalizing.finalizing && !finalizing.active && !finalizing.failed);
    assert_eq!(finalizing.recording, "trip_2026-11-22_10-00-00_123_4");
    assert_eq!((finalizing.elapsed_ms, finalizing.bytes), (0, 0));

    let beside = status_of(
        Some(&recorder_status(true, "recordings/next.mcpr", None)),
        Some("trip"),
    );
    assert!(beside.finalizing && beside.active);
    assert_eq!(beside.recording, "next");
}

/// A mod records the world session: nothing without one, one recording at a time, the status
/// naming the recording while it runs, and the archive in the recordings folder, under the id
/// `recording_stop` answers, once it ends.
#[test]
#[ignore = "daily"]
fn a_client_mod_records_the_session_into_the_recordings_folder() {
    let root = tempfile::tempdir().unwrap();
    crate::game::native_entity_refs::tests::with_client_game(|game| {
        game.replay.recordings_dir = Some(root.path().to_path_buf());
        assert_eq!(
            game.recording_start(Some("trip")),
            Err(RecordingError::NotConnected)
        );
        assert_eq!(game.recording_stop(), Err(RecordingError::NotConnected));
        assert!(!game.recording_status().active);

        game.init_remote_for_replay().unwrap();
        assert_eq!(game.recording_stop(), Err(RecordingError::NotRecording));
        let id = game.recording_start(Some("my trip")).unwrap();
        assert!(id.starts_with("my_trip_"), "{id}");
        let status = game.recording_status();
        assert!(status.active && !status.failed);
        assert_eq!(status.recording, id);
        assert_eq!(
            game.recording_start(None),
            Err(RecordingError::AlreadyRecording)
        );
        assert!(
            !root.path().join(format!("{id}.mcpr")).exists(),
            "the archive appears when the recording ends"
        );

        // Stop answers with the id at once; the archive follows in the background.
        assert_eq!(game.recording_stop(), Ok(id.clone()));
        let status = game.recording_status();
        assert!(!status.active);
        assert_eq!(game.recording_stop(), Err(RecordingError::NotRecording));
        if status.finalizing {
            assert_eq!(status.recording, id);
            assert_eq!(game.recording_start(None), Err(RecordingError::Finalizing));
        }
        let deadline = horizon_std::time::Instant::now() + Duration::from_secs(30);
        while game.recording_status().finalizing {
            assert!(
                horizon_std::time::Instant::now() < deadline,
                "the archive never finished"
            );
            horizon_std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(game.recording_status(), status_of(None, None));
        assert_eq!(
            recording_path(root.path(), &id),
            Ok(root.path().join(format!("{id}.mcpr")))
        );
        // Once the archive is in place a new recording can start.
        let next = game.recording_start(None).unwrap();
        assert_ne!(next, id);
        game.recording_stop().unwrap();
    });
}
