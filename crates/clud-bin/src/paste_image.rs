//! Clipboard paste helpers for PTY-mode Ctrl+V interception (#328).

use std::borrow::Cow;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Matches the 50 MiB upper bound checked by the WezTerm paste action.
const MAX_KITTY_PASTE_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct KittyPastePayload {
    pub kind: &'static str,
    pub value: String,
    pub bytes: usize,
}

/// Take one clipboard snapshot for the native WezTerm paste helper. Images
/// have priority over text when the clipboard advertises both formats.
pub fn kitty_clipboard_payload() -> io::Result<KittyPastePayload> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|err| io::Error::other(format!("open clipboard: {err}")))?;
    if let Ok(image) = clipboard.get_image() {
        let dir = kitty_pictures_dir()?;
        return kitty_image_payload_in(&dir, image.width, image.height, image.bytes.as_ref());
    }
    let text = clipboard
        .get_text()
        .map_err(|err| io::Error::other(format!("read clipboard text or image: {err}")))?;
    kitty_text_payload(text)
}

/// Print one clipboard snapshot as a single JSON line on `stdout` for the
/// `clud-kittyterm-paste` helper; failures go to `stderr` only. Returns the
/// process exit code.
pub fn write_kitty_paste(
    read: io::Result<KittyPastePayload>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    match read {
        Ok(payload) => {
            let json = match serde_json::to_string(&payload) {
                Ok(json) => json,
                Err(error) => {
                    let _ = writeln!(stderr, "clipboard JSON failed: {error}");
                    return 1;
                }
            };
            if writeln!(stdout, "{json}").is_err() {
                let _ = writeln!(stderr, "clipboard output failed");
                return 1;
            }
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "clipboard snapshot failed: {error}");
            1
        }
    }
}

fn kitty_text_payload(text: String) -> io::Result<KittyPastePayload> {
    let bytes = text.len();
    if bytes == 0 || bytes > MAX_KITTY_PASTE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "clipboard text is empty or exceeds 50 MiB",
        ));
    }
    Ok(KittyPastePayload {
        kind: "text",
        value: text,
        bytes,
    })
}

fn kitty_pictures_dir() -> io::Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = dirs::picture_dir() {
        candidates.push(path.join("clud-kitty-pastes"));
    }
    if let Some(path) = crate::home::user_home() {
        let fallback = path.join("Pictures").join("clud-kitty-pastes");
        if !candidates.contains(&fallback) {
            candidates.push(fallback);
        }
    }
    for path in candidates {
        if std::fs::create_dir_all(&path).is_ok() && path.is_dir() {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no writable user Pictures directory for Kitty clipboard image",
    ))
}

fn kitty_image_payload_in(
    dir: &Path,
    width: usize,
    height: usize,
    rgba: &[u8],
) -> io::Result<KittyPastePayload> {
    let byte_count = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "clipboard dimensions overflow")
        })?;
    if byte_count != rgba.len() || byte_count > MAX_KITTY_PASTE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard image byte count is invalid or exceeds 50 MiB",
        ));
    }
    let width = u32::try_from(width)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "clipboard image too wide"))?;
    let height = u32::try_from(height)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "clipboard image too tall"))?;
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard image byte count is invalid",
        )
    })?;

    // The temporary file is private until complete. `persist_noclobber` then
    // publishes the PNG at a random create-new path without overwriting one.
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    image::DynamicImage::ImageRgba8(image)
        .write_to(temporary.as_file_mut(), image::ImageFormat::Png)
        .map_err(|err| io::Error::other(format!("encode clipboard png: {err}")))?;
    temporary.as_file_mut().sync_all()?;
    let bytes = usize::try_from(temporary.as_file().metadata()?.len())
        .map_err(|_| io::Error::other("clipboard PNG size overflow"))?;
    if bytes == 0 || bytes > MAX_KITTY_PASTE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "clipboard PNG is empty or exceeds 50 MiB",
        ));
    }
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|err| io::Error::other(format!("name clipboard PNG: {err}")))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = dir.join(format!("paste-{suffix}.png"));
    let value = path
        .to_str()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "clipboard PNG path is not UTF-8",
            )
        })?
        .to_string();
    temporary
        .persist_noclobber(&path)
        .map_err(|err| io::Error::other(format!("publish clipboard PNG: {err}")))?;
    Ok(KittyPastePayload {
        kind: "image",
        value,
        bytes,
    })
}

pub fn handle_clipboard() -> io::Result<Option<Vec<u8>>> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|err| io::Error::other(format!("open clipboard: {err}")))?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    let path = write_clipboard_png(image.width, image.height, image.bytes.as_ref())?;
    Ok(Some(path_to_pty_bytes(&path)))
}

pub fn expand_ctrl_v_bytes<F>(chunk: &[u8], mut handle_clipboard: F) -> Cow<'_, [u8]>
where
    F: FnMut() -> Option<Vec<u8>>,
{
    if !chunk.contains(&CTRL_V) {
        return Cow::Borrowed(chunk);
    }
    let mut out = Vec::with_capacity(chunk.len());
    for &byte in chunk {
        if byte == CTRL_V {
            if let Some(bytes) = handle_clipboard() {
                out.extend_from_slice(&bytes);
            } else {
                out.push(CTRL_V);
            }
        } else {
            out.push(byte);
        }
    }
    Cow::Owned(out)
}

pub fn write_clipboard_png(width: usize, height: usize, rgba: &[u8]) -> io::Result<PathBuf> {
    let width_u32 = u32::try_from(width)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "clipboard image too wide"))?;
    let height_u32 = u32::try_from(height)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "clipboard image too tall"))?;
    let image =
        image::RgbaImage::from_raw(width_u32, height_u32, rgba.to_vec()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "clipboard image byte count does not match dimensions",
            )
        })?;
    let dir = std::env::temp_dir().join("clud-clipboard");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(unique_png_name());
    image
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(|err| io::Error::other(format!("write clipboard png: {err}")))?;
    Ok(path)
}

fn path_to_pty_bytes(path: &Path) -> Vec<u8> {
    let mut bytes = path.to_string_lossy().into_owned().into_bytes();
    bytes.push(b'\n');
    bytes
}

fn unique_png_name() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("paste-{}-{nanos}.png", std::process::id())
}

const CTRL_V: u8 = 0x16;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_ctrl_v_replaces_marker_when_clipboard_has_bytes() {
        let expanded = expand_ctrl_v_bytes(b"a\x16b", || Some(b"image.png\n".to_vec()));
        assert_eq!(expanded.as_ref(), b"aimage.png\nb");
    }

    #[test]
    fn expand_ctrl_v_falls_through_when_clipboard_unavailable() {
        let expanded = expand_ctrl_v_bytes(b"a\x16b", || None);
        assert_eq!(expanded.as_ref(), b"a\x16b");
    }

    #[test]
    fn expand_ctrl_v_borrows_chunks_without_marker() {
        let expanded = expand_ctrl_v_bytes(b"abc", || Some(b"unused".to_vec()));
        assert!(matches!(expanded, Cow::Borrowed(_)));
        assert_eq!(expanded.as_ref(), b"abc");
    }

    #[test]
    fn write_clipboard_png_rejects_bad_rgba_len() {
        let err = write_clipboard_png(2, 2, &[0, 0, 0]).expect_err("bad len");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn kitty_text_payload_counts_utf8_bytes_and_rejects_empty_or_oversized_text() {
        let payload = kitty_text_payload("hello 🦀".to_string()).unwrap();
        assert_eq!(payload.kind, "text");
        assert_eq!(payload.value, "hello 🦀");
        assert_eq!(payload.bytes, "hello 🦀".len());
        assert!(kitty_text_payload(String::new()).is_err());
        assert!(kitty_text_payload("x".repeat(MAX_KITTY_PASTE_BYTES + 1)).is_err());
    }

    #[test]
    fn kitty_image_payload_writes_distinct_complete_png_files() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let rgba = [255, 0, 0, 255];
        let first = kitty_image_payload_in(dir.path(), 1, 1, &rgba).unwrap();
        let second = kitty_image_payload_in(dir.path(), 1, 1, &rgba).unwrap();
        assert_eq!(first.kind, "image");
        assert_ne!(first.value, second.value);
        assert_eq!(
            first.bytes as u64,
            std::fs::metadata(&first.value).unwrap().len()
        );
        assert!(image::open(&first.value).is_ok());
        assert!(image::open(&second.value).is_ok());
    }

    #[test]
    fn kitty_image_payload_rejects_bad_dimensions_without_leaving_files() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let error = kitty_image_payload_in(dir.path(), usize::MAX, 2, &[]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn emits_one_json_object_for_text_without_logs_on_stdout() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = write_kitty_paste(
            Ok(KittyPastePayload {
                kind: "text",
                value: "hello 🦀".into(),
                bytes: "hello 🦀".len(),
            }),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        let payload: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(payload["kind"], "text");
        assert_eq!(payload["value"], "hello 🦀");
        assert_eq!(payload["bytes"], "hello 🦀".len());
        assert_eq!(stdout.iter().filter(|&&byte| byte == b'\n').count(), 1);
    }

    #[test]
    fn emits_image_path_and_file_size_in_json() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = write_kitty_paste(
            Ok(KittyPastePayload {
                kind: "image",
                value: "C:/Users/test/Pictures/clud-kitty-pastes/paste-1.png".into(),
                bytes: 1234,
            }),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        let payload: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(payload["kind"], "image");
        assert_eq!(payload["bytes"], 1234);
        assert_eq!(
            payload["value"],
            "C:/Users/test/Pictures/clud-kitty-pastes/paste-1.png"
        );
    }

    #[test]
    fn failure_returns_nonzero_without_stdout_json() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = write_kitty_paste(
            Err(io::Error::new(io::ErrorKind::NotFound, "empty clipboard")),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 1);
        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains("empty clipboard"));
    }
}
