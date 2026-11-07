//! Whisper model resolution + auto-download (issue #13).
//!
//! Resolution order:
//!   1. `CLUD_WHISPER_MODEL` env var → trusted as-is (no hash check).
//!   2. `<cache-dir>/clud/whisper/ggml-small.en.bin` if file exists
//!      AND its SHA-256 matches `MODEL_SHA256`.
//!   3. Download from Hugging Face into (2)'s path, atomic-rename
//!      from `<name>.partial`, verify hash, retry once on hash
//!      mismatch.
//!
//! On any failure the transcriber-not-installed message is surfaced
//! to the user with a hint about where they can drop the model
//! manually.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

/// Default model filename. Whisper.cpp uses this name on Hugging
/// Face and most user-facing docs reference it.
pub(super) const MODEL_FILENAME: &str = "ggml-small.en.bin";
/// Upstream model URL. Hugging Face serves it un-gated.
const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin";
/// SHA-256 of the small.en model as published by ggerganov. Pinned
/// so a corrupt download or upstream swap can't silently break
/// transcription quality. If the upstream model rev changes,
/// update this constant in the same PR.
const MODEL_SHA256: &str = "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b";

/// Compute the default per-OS cache path for the model.
///
/// Falls back to `./.clud-cache/whisper/` (relative to cwd) if
/// `dirs::cache_dir()` returns `None`, which happens on stripped
/// environments where neither `XDG_CACHE_HOME` nor the home dir
/// is resolvable.
pub(super) fn default_cache_path() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".clud-cache"));
    base.join("clud").join("whisper").join(MODEL_FILENAME)
}

/// Resolve a usable model path WITHOUT triggering a download.
///
/// Returns `Some(path)` if the env override is set OR the cached
/// copy is present and intact. Returns `None` if the model needs
/// to be downloaded — `ensure_downloaded_in_background` is the
/// next step in that case.
pub(super) fn resolve_if_available(env_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = env_override {
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }
    let cache = default_cache_path();
    if cache.is_file() && verify_sha256(&cache).unwrap_or(false) {
        return Some(cache);
    }
    None
}

/// Kick off a detached download thread for the cache path. Sets
/// `*flag` to true when the download has finished (success OR
/// failure) so the caller can probe completion without joining.
///
/// No-op if the cached file is already present and hash-valid —
/// the caller should call `resolve_if_available` first.
pub(super) fn ensure_downloaded_in_background(done_flag: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let result = download_to_cache();
        if let Err(err) = &result {
            eprintln!("[clud] voice: model auto-download failed: {err}");
            eprintln!(
                "[clud] voice: drop ggml-small.en.bin at {:?} or set CLUD_WHISPER_MODEL",
                default_cache_path()
            );
        }
        done_flag.store(true, Ordering::SeqCst);
    });
}

/// Download the model to the cache path. Streams to a `.partial`
/// temp file alongside, verifies SHA-256, then atomic-renames.
/// Idempotent: succeeds without downloading if a valid copy
/// already exists.
fn download_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(60))
        .build()
}

fn download_to_cache() -> Result<PathBuf, String> {
    let final_path = default_cache_path();
    if final_path.is_file() && verify_sha256(&final_path).unwrap_or(false) {
        return Ok(final_path);
    }

    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("could not create cache dir {parent:?}: {err}"))?;
    }
    let partial_path = final_path.with_extension("partial");
    // Best-effort clean of a stale partial from a previous run.
    let _ = fs::remove_file(&partial_path);

    eprintln!(
        "[clud] voice: downloading Whisper model (~466 MB) to {:?} — first F3 use will block until this finishes",
        final_path
    );

    // Connect and per-read stall timeouts, not an overall deadline: ureq's
    // `timeout` also covers the body, so a ~466 MB download on a link slower
    // than ~12 Mbit/s always failed partway (#1917).
    let response = download_agent()
        .get(MODEL_URL)
        .call()
        .map_err(|err| format!("HTTP error fetching {MODEL_URL}: {err}"))?;
    let total_bytes: Option<u64> = response
        .header("Content-Length")
        .and_then(|s| s.parse().ok());

    let mut reader = response.into_reader();
    let mut file = File::create(&partial_path)
        .map_err(|err| format!("could not create {partial_path:?}: {err}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    let mut next_progress_pct: u64 = 5;
    loop {
        let n = reader
            .read(&mut buffer)
            .map_err(|err| format!("download read error: {err}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])
            .map_err(|err| format!("could not write to {partial_path:?}: {err}"))?;
        hasher.update(&buffer[..n]);
        downloaded += n as u64;
        if let Some(total) = total_bytes {
            let pct = downloaded * 100 / total.max(1);
            if pct >= next_progress_pct {
                eprintln!(
                    "[clud] voice: download {pct}% ({downloaded}/{total} bytes)",
                    pct = pct,
                );
                // Step in 5% increments but jump forward if we
                // already skipped past the next mark.
                next_progress_pct = pct + 5;
            }
        }
    }
    file.flush().map_err(|err| format!("flush failed: {err}"))?;
    drop(file);

    let digest = format!("{:x}", hasher.finalize());
    if digest != MODEL_SHA256 {
        let _ = fs::remove_file(&partial_path);
        return Err(format!(
            "SHA-256 mismatch: expected {MODEL_SHA256}, got {digest}; refusing to use a corrupt model"
        ));
    }

    fs::rename(&partial_path, &final_path)
        .map_err(|err| format!("could not rename {partial_path:?} -> {final_path:?}: {err}"))?;
    eprintln!("[clud] voice: model ready at {final_path:?}");
    Ok(final_path)
}

/// Compute SHA-256 of the file at `path` and compare to
/// [`MODEL_SHA256`]. `Ok(false)` means the file exists but the
/// hash doesn't match (corrupt or stale download). `Err` means
/// the file couldn't be opened or read.
pub(super) fn verify_sha256(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()) == MODEL_SHA256)
}

/// `AtomicBool::clone`-free way to signal a probe-completion
/// flag across the worker thread boundary. Returned from
/// `VoiceMode::from_env` so the input loop can check whether
/// the auto-download has finished without joining.
pub(super) fn fresh_completion_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[cfg(test)]
mod tests {
    #[test]
    fn model_download_has_no_overall_deadline() {
        // An overall request timeout also bounds the body read, which a large
        // model on a slow link cannot meet (#1917). Only connect/read stall
        // timeouts belong on this download.
        let source = include_str!("model.rs");
        let overall = concat!(".timeout", "(Duration");
        assert!(!source.contains(overall));
        assert!(source.contains(".timeout_read("));
    }
}
