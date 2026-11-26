# Recording a session

A client mod starts and stops the recording `/studio record` makes of the world session (a `.mcpr` file in the game
folder's `recordings/`) and reads how it goes, as a replay editor's record button does. Native and wasm mods use
the same calls; a wasm mod needs no file access, because the platform picks the folder and the file name, and a
recording is named by its **id**: its file name without `.mcpr`, the id [`replay_open`](replay-playback.md) takes.

| Call | Access | What it does |
|---|---|---|
| `recording_status()` | client setup | How the recording stands, whoever started it: `active`, `recording` (its id), `elapsed_ms`, `bytes` written, `failed` and `finalizing`. All empty, zero or false while no recording runs. |
| `recording_start(name)` | world | Starts recording the world session's packets and answers the recording's id. `Finalizing` while the previous archive is still being written. |
| `recording_stop()` | world | Stops the recording, whoever started it, and answers the id at once; the `.mcpr` archive is written in the background. |

The rules are those of `/studio record`:

- **One recording at a time.** `recording_start` is `AlreadyRecording` while one runs, whether a mod or
  `/studio record start` began it, and `recording_stop` ends whichever it is. A recorder that has failed (below)
  does not count: a new recording replaces it.
- **The platform names the file.** `name` is optional and only gives the file name its start: its letters, digits,
  `-` and `_` are kept (whitespace becomes `_`, at most 80 characters), and the time and a counter follow, so
  `my trip!` records `my_trip_2026-11-22_10-00-00_123_0.mcpr`. A name is never a path, and two recordings never
  share a file. The id is that file name without `.mcpr`.
- **The same privacy filter.** What the recording keeps is the platform's, unchanged: chat, the tab list, player
  accounts and similar data are left out, and an unknown packet is not recorded.
- **A world session is needed.** `recording_start` is `NotConnected` on the title screen or any menu with no world
  behind it, and while a replay plays. `recording_stop` is `NotConnected` with no world session too, and during a
  replay only when no recorder runs: a recorder `/studio record` started during a replay stays stoppable (with
  none, it answers `NotRecording` as anywhere else). A single-player world, a multiplayer server and a LAN guest all record what the client receives.
- **The file appears when the archive is finished.** While the recording runs the packets go to a temporary file
  beside it, so `replay_open(id)` is `NotFound` until `recording_status` no longer says `finalizing` after
  `recording_stop`. The recorder belongs to the session: leaving the world ends the recording with it, and
  `recording_status` reads as no recording in the next one. Stop it first, and wait for `finalizing` to clear, to be
  sure its archive is written.

When the recorder fails (the disk refuses a write, or falls too far behind) it stops taking packets by itself:
`recording_status` then reads `active` false and `failed` true, with the id and the bytes it got to; `elapsed_ms` keeps counting.
`recording_stop` ends the recording as usual and answers `DiskError`; the file may be incomplete or missing.

## Finalizing

`recording_stop` never waits for the archive: it answers the id at once, and compressing and syncing the
`.mcpr` file runs on a thread of its own, so stopping a long recording does not stall a frame. Until the archive is in the
recordings folder `recording_status` reads `finalizing` true (with `active` false, and `recording` naming the
recording being finished while no other one runs), then goes back to all empty. Poll it before you
`replay_open` the id.

- `recording_start` answers `Finalizing` while the previous archive is being written, and changes nothing: wait for
  the status to stop saying so and ask again. (A recording could be made beside it, but one status could not tell the
  two apart.) It is checked before `AlreadyRecording`.
- If the archive cannot be finished (a full disk, say) the game logs it, `finalizing` goes back to false and the id
  has no file: `replay_open` answers `NotFound`.
- Packaging recordings an earlier crash left behind runs in the background too, when a recording starts; it never
  delays `recording_start`, and a recovered recording shows up in `recordings/` a moment later.

`recording_status` is cheap enough to read every frame.

`RecordingStatus` and `RecordingError` are `#[non_exhaustive]` for Rust callers only. On the wire every field and
variant is encoded and its layout hashed ([wire types](../../concepts/wire-types.md)), so they stay as they are:
adding one would break wasm mods built against this version.
