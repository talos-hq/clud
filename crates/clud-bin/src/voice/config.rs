use std::env;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct VoiceConfig {
    pub(super) enabled: bool,
    pub(super) model_path: Option<PathBuf>,
    pub(super) language: Option<String>,
    pub(super) test_transcript: Option<String>,
    pub(super) transcriber: Option<PathBuf>,
}

impl VoiceConfig {
    pub(super) fn from_env() -> Self {
        let model_path = env::var_os("CLUD_WHISPER_MODEL").map(PathBuf::from);
        let transcriber = env::var_os("CLUD_VOICE_TRANSCRIBER")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let test_transcript = env::var("CLUD_VOICE_TEST_TRANSCRIPT")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let language = env::var("CLUD_VOICE_LANGUAGE")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let enabled_flag = matches!(
            env::var("CLUD_VOICE")
                .ok()
                .as_deref()
                .map(|value| value.to_ascii_lowercase()),
            Some(ref value) if matches!(value.as_str(), "1" | "true" | "yes" | "on")
        );

        Self {
            enabled: enabled_flag
                || model_path.is_some()
                || test_transcript.is_some()
                || transcriber.is_some(),
            model_path,
            language,
            test_transcript,
            transcriber,
        }
    }
}
