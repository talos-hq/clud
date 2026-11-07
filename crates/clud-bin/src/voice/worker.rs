use std::sync::mpsc;

use super::config::VoiceConfig;

#[derive(Debug)]
pub(super) enum WorkerCommand {
    Transcribe { audio: Vec<f32> },
}

#[derive(Debug)]
pub(super) enum WorkerEvent {
    Transcript(String),
    Error(String),
}

#[derive(Debug)]
pub(super) struct VoiceWorker {
    commands: mpsc::Sender<WorkerCommand>,
    events: mpsc::Receiver<WorkerEvent>,
}

impl VoiceWorker {
    pub(super) fn spawn(config: VoiceConfig) -> Self {
        let (command_tx, command_rx) = mpsc::channel::<WorkerCommand>();
        let (event_tx, event_rx) = mpsc::channel::<WorkerEvent>();

        std::thread::spawn(move || {
            while let Ok(command) = command_rx.recv() {
                match command {
                    WorkerCommand::Transcribe { audio } => {
                        let result = transcribe_audio(&config, &audio);
                        let event = match result {
                            Ok(text) => WorkerEvent::Transcript(text),
                            Err(err) => WorkerEvent::Error(err),
                        };
                        let _ = event_tx.send(event);
                    }
                }
            }
        });

        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub(super) fn request_transcription(&self, audio: Vec<f32>) -> Result<(), String> {
        self.commands
            .send(WorkerCommand::Transcribe { audio })
            .map_err(|_| "voice worker is unavailable".to_string())
    }

    pub(super) fn try_recv(&self) -> Result<WorkerEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }
}

/// `CLUD_VOICE_TEST_TRANSCRIPT` comes first so state-machine tests need no
/// real backend; everything else goes to the external CLI (`transcriber`).
fn transcribe_audio(config: &VoiceConfig, audio: &[f32]) -> Result<String, String> {
    if let Some(test_transcript) = &config.test_transcript {
        return Ok(test_transcript.clone());
    }
    super::transcriber::transcribe(config, audio)
}
