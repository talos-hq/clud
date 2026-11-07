use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

use crate::session::{InteractiveHooks, PtyInputSink};

use super::config::VoiceConfig;
use super::cues::{play_cue, CueTone};
use super::model;
use super::recording::ActiveRecording;
use super::transcriber;
use super::worker::{VoiceWorker, WorkerEvent};

pub struct VoiceMode {
    config: VoiceConfig,
    recording: Option<ActiveRecording>,
    worker: Option<VoiceWorker>,
    transcribing: bool,
    missing_model_reported: bool,
    /// Auto-download completion signal. Set to true by the background
    /// download thread when the model is ready (or has failed and
    /// won't retry). Probed on each F3 press so we re-check the
    /// cache once the download lands without polling on every tick.
    download_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Cached model path once resolution succeeds. Avoids re-hashing
    /// a 466 MB file every time the user starts a recording.
    resolved_model: Option<PathBuf>,
}

impl VoiceMode {
    pub fn from_env() -> Self {
        let config = VoiceConfig::from_env();
        let download_done = model::fresh_completion_flag();

        // Eagerly resolve the model on startup. Three cases:
        //   1. Voice is disabled → skip everything (don't probe network).
        //   2. CLUD_VOICE_TEST_TRANSCRIPT is set → tests don't touch
        //      Whisper at all; skip model setup.
        //   3. A copy is already cached → resolve synchronously, no
        //      thread spawned.
        //   4. Model is missing → spawn the background download
        //      thread so the user can press F3 sooner.
        // An external transcriber needs none of this (DD-206).
        let mut resolved_model: Option<PathBuf> = None;
        if config.enabled
            && config.test_transcript.is_none()
            && transcriber::locate(&config).is_err()
        {
            resolved_model = model::resolve_if_available(config.model_path.as_deref());
            if resolved_model.is_none() {
                model::ensure_downloaded_in_background(Arc::clone(&download_done));
            } else {
                // Already on disk — short-circuit the "download done"
                // flag so the first F3 press doesn't print a "still
                // downloading" warning.
                download_done.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        Self {
            config,
            recording: None,
            worker: None,
            transcribing: false,
            missing_model_reported: false,
            download_done,
            resolved_model,
        }
    }

    /// Re-attempt to resolve a model path if we don't have one yet
    /// AND the background download has finished. Mutates
    /// `self.resolved_model` and (on success) `self.config.model_path`
    /// so downstream transcription can pick it up without further
    /// plumbing.
    fn refresh_resolved_model(&mut self) {
        if self.resolved_model.is_some() {
            return;
        }
        if !self.download_done.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if let Some(path) = model::resolve_if_available(self.config.model_path.as_deref()) {
            // Make the path visible to the worker via the existing
            // `model_path` field. The worker takes a `VoiceConfig`
            // snapshot when it spawns (see `VoiceWorker::spawn`), so
            // we also have to nuke and respawn the worker if it
            // captured a stale (None) path. Simpler than threading
            // shared state: drop self.worker so the next
            // `stop_recording` lazily spawns a fresh one.
            if self.config.model_path.is_none() {
                self.worker = None;
            }
            self.config.model_path = Some(path.clone());
            self.resolved_model = Some(path);
        }
    }

    fn start_recording(&mut self) {
        if !self.config.enabled {
            return;
        }

        // Probe for download completion in case a model just landed.
        self.refresh_resolved_model();

        // Refuse before recording: nobody should speak into a capture that
        // cannot be transcribed. Re-checked per press, so installing the CLI
        // mid-session works.
        if let Some(message) = recording_blocker(&self.config) {
            if !self.missing_model_reported {
                eprintln!("[clud] voice: {message}");
                self.missing_model_reported = true;
            }
            return;
        }

        if self.transcribing || self.recording.is_some() {
            return;
        }

        // `CLUD_VOICE_TEST_TRANSCRIPT` bypasses the mic entirely so the
        // integration test in `tests/integration/test_voice_mode.py`
        // runs on CI hosts with no audio device (Linux runners with no
        // ALSA card, macOS runners where the mic is too tightly
        // sandboxed for the timing window). The release path will
        // still queue a transcription; the worker short-circuits to
        // `config.test_transcript` and writes it back into the PTY.
        if self.config.test_transcript.is_some() {
            play_cue(CueTone::Start);
            self.recording = Some(ActiveRecording::synthetic());
            return;
        }

        match ActiveRecording::start() {
            Ok(recording) => {
                play_cue(CueTone::Start);
                self.recording = Some(recording);
            }
            Err(err) => eprintln!("[clud] voice: {err}"),
        }
    }

    fn stop_recording(&mut self) {
        let Some(recording) = self.recording.take() else {
            return;
        };

        play_cue(CueTone::Stop);
        match recording.finish() {
            Ok(audio) if audio.is_empty() => {}
            Ok(audio) => {
                let worker = self
                    .worker
                    .get_or_insert_with(|| VoiceWorker::spawn(self.config.clone()));
                if let Err(err) = worker.request_transcription(audio) {
                    eprintln!("[clud] voice: {err}");
                    self.transcribing = false;
                } else {
                    self.transcribing = true;
                }
            }
            Err(err) => eprintln!("[clud] voice: {err}"),
        }
    }

    fn drain_worker_events(&mut self, sink: &mut dyn PtyInputSink) -> io::Result<()> {
        let Some(worker) = self.worker.as_ref() else {
            return Ok(());
        };

        loop {
            match worker.try_recv() {
                Ok(WorkerEvent::Transcript(text)) => {
                    self.transcribing = false;
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    sink.write_input(trimmed.as_bytes(), false)?;
                }
                Ok(WorkerEvent::Error(err)) => {
                    self.transcribing = false;
                    eprintln!("[clud] voice: {err}");
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.transcribing = false;
                    break;
                }
            }
        }
        Ok(())
    }
}

fn recording_blocker(config: &VoiceConfig) -> Option<String> {
    if config.test_transcript.is_some() {
        return None;
    }
    transcriber::locate(config).err()
}

impl InteractiveHooks for VoiceMode {
    fn intercept_f3(&self) -> bool {
        self.config.enabled
    }

    /// F3 press starts recording if idle; otherwise it's a no-op.
    /// Toggle semantics (press-to-stop) used to live here but lose
    /// to the hold-to-record contract — a press while already
    /// recording either means autorepeat (terminal sent the SS3-R
    /// sequence twice) or the user re-pressed without an
    /// intervening release, neither of which should kill capture.
    /// Stops come from `on_f3_release` (kitty terminals) or the VAD
    /// auto-stop in `on_tick` (everyone else).
    fn on_f3_press(&mut self, _sink: &mut dyn PtyInputSink) -> io::Result<()> {
        if self.recording.is_none() {
            self.start_recording();
        }
        Ok(())
    }

    fn on_f3_release(&mut self, _sink: &mut dyn PtyInputSink) -> io::Result<()> {
        if self.recording.is_some() {
            self.stop_recording();
        }
        Ok(())
    }

    fn on_tick(&mut self, sink: &mut dyn PtyInputSink) -> io::Result<()> {
        // VAD-silence / hard-cap auto-stop. Fires on terminals that
        // don't emit kitty release events (Windows ConPTY in
        // particular); harmless on those that do, because
        // `on_f3_release` will have already drained `self.recording`.
        let auto_stop = self
            .recording
            .as_mut()
            .map(|rec| rec.should_auto_stop())
            .unwrap_or(false);
        if auto_stop {
            self.stop_recording();
        }
        self.drain_worker_events(sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::audio::TARGET_SAMPLE_RATE;
    use crate::voice::audio::{downmix_and_resample, has_speech_peak, is_effectively_silent};
    use running_process::pty::NativePtyProcess;
    use std::env;

    #[test]
    fn voice_config_can_enable_from_model_path_without_flag() {
        let _guard = EnvGuard::set("CLUD_WHISPER_MODEL", "C:\\models\\ggml-small.en.bin");
        let config = VoiceConfig::from_env();
        assert!(config.enabled);
        assert!(config.model_path.is_some());
    }

    #[test]
    fn downmix_and_resample_handles_stereo() {
        let audio = vec![1.0, -1.0, 0.5, -0.5];
        let output = downmix_and_resample(audio, 2, TARGET_SAMPLE_RATE);
        assert_eq!(output, vec![0.0, 0.0]);
    }

    #[test]
    fn silence_detection_works() {
        assert!(is_effectively_silent(&[0.0, 0.001, -0.002]));
        assert!(!is_effectively_silent(&[0.5]));
    }

    #[test]
    fn has_speech_peak_matches_silence_threshold() {
        // The VAD branch uses `has_speech_peak`; it must use the same
        // threshold as `is_effectively_silent`. If they drift, the
        // recording will either auto-stop while the user is still
        // speaking, or fail to auto-stop on long silences.
        assert!(!has_speech_peak(&[0.0, 0.001, -0.002]));
        assert!(has_speech_peak(&[0.0, 0.5, 0.0]));
        assert!(!has_speech_peak(&[]));
    }

    #[test]
    fn f3_is_gated_on_the_transcriber_not_on_the_whisper_model() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("fake-transcriber");
        std::fs::write(&exe, "#!/bin/sh\n").expect("write");
        crate::platform::shim_fs::set_mode(&exe, 0o755).expect("chmod");
        let config = |transcriber: PathBuf, test_transcript: Option<String>| VoiceConfig {
            enabled: true,
            model_path: None,
            language: None,
            test_transcript,
            transcriber: Some(transcriber),
        };
        if !crate::platform::host::WINDOWS {
            assert_eq!(recording_blocker(&config(exe, None)), None);
        }
        let missing = dir.path().join("missing");
        assert!(recording_blocker(&config(missing.clone(), None)).is_some());
        assert_eq!(
            recording_blocker(&config(missing, Some("text".to_string()))),
            None
        );
    }

    // ─── Model resolution ─────────────────────────────────────────
    // Issue #13: auto-downloader. Tests cover the no-network code
    // paths — actual downloads are exercised manually since CI has
    // no business pulling 466 MB per platform per job.

    #[test]
    fn model_resolver_uses_env_override_when_present() {
        // Write a tempfile and point CLUD_WHISPER_MODEL at it. The
        // resolver must return that exact path with no hash check.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_model = dir.path().join("user-model.bin");
        std::fs::write(&fake_model, b"not actually a model").expect("write");
        let resolved = model::resolve_if_available(Some(&fake_model));
        assert_eq!(resolved.as_deref(), Some(fake_model.as_path()));
    }

    #[test]
    fn model_resolver_returns_none_when_nothing_present() {
        // Env override points at a non-existent path AND no
        // (validated) cache copy. The resolver must report "not
        // available" so the caller can kick off the download.
        let dir = tempfile::tempdir().expect("tempdir");
        let bogus = dir.path().join("does-not-exist.bin");
        // `default_cache_path` may legitimately exist on a dev
        // machine; this test only asserts that an unreachable
        // override doesn't get returned.
        let resolved = model::resolve_if_available(Some(&bogus));
        assert_ne!(resolved.as_deref(), Some(bogus.as_path()));
    }

    // ─── Hold-to-record semantics ─────────────────────────────────
    // These exercise the F3 state machine WITHOUT touching the
    // audio device or Whisper. The mic/transcription paths are
    // mocked out via `CLUD_VOICE_TEST_TRANSCRIPT` which short-
    // circuits the worker.

    /// Helper: build a VoiceMode in a known good state for state-
    /// machine tests. The test-transcript bypass keeps Whisper and
    /// the network out of the test.
    fn voice_mode_for_state_test() -> (VoiceMode, EnvGuard, EnvGuard) {
        let model_guard = EnvGuard::set("CLUD_WHISPER_MODEL", "/dev/null");
        let test_guard = EnvGuard::set("CLUD_VOICE_TEST_TRANSCRIPT", "mock-text");
        let mode = VoiceMode::from_env();
        (mode, model_guard, test_guard)
    }

    #[test]
    fn voice_state_press_while_recording_is_ignored() {
        // Hold-to-record contract: a press WHILE already recording
        // must NOT toggle the recording off. This is the load-
        // bearing test against the old toggle behavior.
        //
        // The recording slot is filled via `ActiveRecording::synthetic`
        // — no real cpal stream, no audio device. The older helper
        // built a real WASAPI stream and dropped it without `.play()`,
        // which segfaulted `cargo test` on the Windows server runners
        // whose phantom default input device returned a stream that
        // crashed on drop (STATUS_ACCESS_VIOLATION).
        let (mut mode, _g1, _g2) = voice_mode_for_state_test();
        assert!(mode.recording.is_none(), "starts idle");

        mode.recording = Some(ActiveRecording::synthetic());
        let pty_handle = make_dummy_pty();
        let mut sink = crate::session::NativePtyProcessSink::new(&pty_handle);
        let result =
            <VoiceMode as crate::session::InteractiveHooks>::on_f3_press(&mut mode, &mut sink);
        assert!(result.is_ok());
        assert!(
            mode.recording.is_some(),
            "press while recording must NOT stop the capture (was the old toggle behavior)"
        );
    }

    #[test]
    fn voice_state_release_stops_recording() {
        let (mut mode, _g1, _g2) = voice_mode_for_state_test();
        mode.recording = Some(ActiveRecording::synthetic());
        let pty_handle = make_dummy_pty();
        let mut sink = crate::session::NativePtyProcessSink::new(&pty_handle);
        let _ =
            <VoiceMode as crate::session::InteractiveHooks>::on_f3_release(&mut mode, &mut sink);
        assert!(
            mode.recording.is_none(),
            "release must drain the recording slot so the next press can start fresh"
        );
    }

    #[test]
    fn voice_state_release_when_idle_is_noop() {
        // The pump fires `on_f3_release` whenever the byte stream
        // says so, even if we never saw the matching press. Must
        // tolerate that without panicking.
        let (mut mode, _g1, _g2) = voice_mode_for_state_test();
        assert!(mode.recording.is_none());
        let pty_handle = make_dummy_pty();
        let mut sink = crate::session::NativePtyProcessSink::new(&pty_handle);
        let result =
            <VoiceMode as crate::session::InteractiveHooks>::on_f3_release(&mut mode, &mut sink);
        assert!(result.is_ok());
    }

    fn make_dummy_pty() -> NativePtyProcess {
        // We never call any method on this handle in these tests —
        // the hooks only touch `process.write_impl` from the
        // transcript-injection path, which we deliberately don't
        // exercise here. `NativePtyProcess::new` is non-blocking
        // and accepts any argv; it doesn't spawn until
        // `start_impl` is called.
        let argv = vec!["true".to_string()];
        NativePtyProcess::new(argv, None, None, 24, 80, None).expect("pty handle")
    }

    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var(key).ok();
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { env::set_var(self.key, value) },
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }
}
