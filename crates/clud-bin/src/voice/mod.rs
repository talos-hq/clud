//! Voice mode (issue #13): F3 push-to-talk transcription.
//!
//! Flow:
//!   1. `F3Observer` in [`crate::session`] watches the byte stream coming
//!      from the user's terminal and reports F3 press/release events.
//!   2. [`VoiceMode::on_f3_press`] starts microphone capture and plays a
//!      start cue.
//!   3. Release (real kitty-protocol release, VAD-silence auto-stop, or the
//!      30-second hard cap — whichever fires first) calls
//!      [`VoiceMode::on_f3_release`], which plays a `dong` cue and hands
//!      the captured audio to a background [`VoiceWorker`] thread.
//!   4. The worker downsamples to 16 kHz mono and hands the audio to
//!      `transcribe_audio` in [`worker`], which sends the resulting text
//!      back as a [`WorkerEvent::Transcript`].
//!   5. The next tick of the PTY pump drains that event and writes the
//!      transcript bytes into the backend PTY via
//!      `NativePtyProcess::write_impl(..., is_paste=false)` — the user
//!      sees the text appear at their cursor without auto-submit.
//!
//! Speech-to-text transcription itself is currently unavailable: the
//! `whisper-rs` backend was removed after its vendored C++ CMake build
//! (`whisper-rs-sys`) repeatedly broke on Windows hosts. Mic capture, cues,
//! and the F3 hold-to-record state machine still work; `transcribe_audio`
//! always returns an "unavailable" error outside the
//! `CLUD_VOICE_TEST_TRANSCRIPT` test bypass. Model download/cache
//! resolution (`model`) is unaffected by this and still runs, in case a
//! future backend picks the model back up.

pub(crate) mod audio;
mod config;
mod cues;
mod mode;
mod model;
mod recording;
mod transcriber;
mod wav;
mod worker;

pub use mode::VoiceMode;
