use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static LAUNCH_START: OnceLock<Instant> = OnceLock::new();
static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

const ENV_VERBOSE_LOG_DIR: &str = "CLUD_VERBOSE_LOG_DIR";

pub fn init_launch_clock() {
    let _ = LAUNCH_START.set(Instant::now());
}

pub fn enable_file_logging() -> io::Result<PathBuf> {
    let dir = default_log_dir()?;
    std::fs::create_dir_all(&dir)?;
    let (path, file) = create_launch_log_file(&dir)?;
    let file_slot = LOG_FILE.get_or_init(|| Mutex::new(None));
    *file_slot
        .lock()
        .map_err(|err| io::Error::other(err.to_string()))? = Some(file);
    Ok(path)
}

pub fn log(message: impl std::fmt::Display) {
    let start = LAUNCH_START.get_or_init(Instant::now);
    let line = format!("{:.2} {}", start.elapsed().as_secs_f64(), message);
    crate::loop_console::eprint_diagnostic(&line);
    if let Some(file_slot) = LOG_FILE.get() {
        if let Ok(mut guard) = file_slot.lock() {
            if let Some(file) = guard.as_mut() {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }
    }
}

pub fn display_path(path: &Path) -> String {
    if let Some(home) = crate::home::user_home() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rest) = path.strip_prefix(cwd) {
            return format!(".{}{}", std::path::MAIN_SEPARATOR, rest.display());
        }
    }
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn default_log_dir() -> io::Result<PathBuf> {
    if let Ok(path) = std::env::var(ENV_VERBOSE_LOG_DIR) {
        return Ok(PathBuf::from(path));
    }
    let home = crate::home::user_home()
        .ok_or_else(|| io::Error::other("no home directory; cannot resolve verbose log dir"))?;
    Ok(home.join(".clud").join("state").join("verbose"))
}

fn create_launch_log_file(dir: &Path) -> io::Result<(PathBuf, File)> {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    for suffix in 0..100 {
        let name = if suffix == 0 {
            format!("clud-{unix_ms}-{pid}.log")
        } else {
            format!("clud-{unix_ms}-{pid}-{suffix}.log")
        };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate unique verbose log file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_path_strips_home_prefix() {
        let Some(home) = crate::home::user_home() else {
            return;
        };
        let path = home
            .join(".clud")
            .join("state")
            .join("verbose")
            .join("x.log");
        let rendered = display_path(&path);
        assert!(rendered.starts_with('~'), "rendered={rendered}");
        assert!(rendered.ends_with("x.log"), "rendered={rendered}");
    }

    #[test]
    fn enabled_file_logging_writes_timestamped_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var(ENV_VERBOSE_LOG_DIR, tmp.path());
        let path = enable_file_logging().expect("enable file logging");

        log("[clud] test message");

        let text = std::fs::read_to_string(&path).expect("read log");
        assert!(text.contains("[clud] test message"), "text={text:?}");
        assert!(
            text.lines()
                .any(|line| line.split_once(' ').is_some_and(|(ts, _)| ts.contains('.'))),
            "text={text:?}"
        );
        std::env::remove_var(ENV_VERBOSE_LOG_DIR);
    }
}
