//! Published Codex model choices, checked against the local Codex account.

pub const FALLBACK_SOL: &str = "gpt-6-sol";
pub const FALLBACK_LUNA: &str = "gpt-6-luna";
pub const FALLBACK_TERRA: &str = "gpt-5.6-terra";
const BUNDLED_MODELS: &str = include_str!("../assets/codex-models.json");
const CACHE_PATH: &str = ".clud/cache/models/manifest.json";
const LAST_GOOD_PATH: &str = ".clud/cache/models/last-good.json";
const CACHE_INTERVAL: Duration = Duration::from_secs(15 * 60);
const MAX_MODEL_PAGES: usize = 8;
const MAX_MODEL_BYTES: usize = 512 * 1024;
const MAX_MODEL_FRAME_BYTES: usize = 128 * 1024;

use fs4::fs_std::FileExt;
use running_process::{
    CommandSpec, NativeProcess, ProcessConfig, ReadStatus, StderrMode, StdinMode, StreamKind,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn cache_path() -> Option<PathBuf> {
    crate::home::user_home().map(|home| home.join(CACHE_PATH))
}

fn last_good_path() -> Option<PathBuf> {
    crate::home::user_home().map(|home| home.join(LAST_GOOD_PATH))
}

fn read_cached(path: &Path) -> Option<Manifest> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    Manifest::parse(&bytes).ok()
}

fn cache_fresh(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < CACHE_INTERVAL)
}

fn attempt_path(path: &Path) -> PathBuf {
    path.with_extension("json.last-attempt")
}

fn record_attempt(path: &Path) {
    let stamp = attempt_path(path);
    if let Some(parent) = stamp.parent() {
        if std::fs::create_dir_all(parent).is_ok() {
            let _ = std::fs::write(stamp, b"");
        }
    }
}

fn fetch_manifest(url: &str) -> Result<Manifest, String> {
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(900))
        .build()
        .get(url)
        .call()
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Manifest::parse(&bytes)
}

/// The first manifest in `urls` that fetches and parses: the canonical domain,
/// then the GitHub release fallback (#2449).
fn fetch_first_manifest(urls: &[&str]) -> Result<Manifest, String> {
    let mut errors = Vec::new();
    for url in urls {
        match fetch_manifest(url) {
            Ok(manifest) => return Ok(manifest),
            Err(error) => errors.push(format!("{url}: {error}")),
        }
    }
    Err(errors.join("; "))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path.parent().ok_or("model cache has no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut temporary, value).map_err(|error| error.to_string())?;
    temporary.persist(path).map_err(|error| error.to_string())?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct LastGood {
    schema_version: u64,
    sol: Option<String>,
    luna: Option<String>,
    terra: Option<String>,
}

impl LastGood {
    fn from_manifest(manifest: &Manifest) -> Self {
        Self {
            schema_version: 1,
            sol: Some(manifest.families.sol.clone()),
            luna: Some(manifest.families.luna.clone()),
            terra: manifest.families.terra.clone(),
        }
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let value = crate::server_settings::parse_strict_json(bytes, 64 * 1024)?;
        let state: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        if state.schema_version != 1
            || state.sol.as_deref().is_some_and(|id| !stable_id(id, "sol"))
            || state
                .luna
                .as_deref()
                .is_some_and(|id| !stable_id(id, "luna"))
            || state
                .terra
                .as_deref()
                .is_some_and(|id| !stable_id(id, "terra"))
        {
            return Err("invalid last-good Codex models".to_string());
        }
        Ok(state)
    }
}

fn read_last_good(path: &Path) -> Option<LastGood> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    LastGood::parse(&bytes).ok()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Families {
    pub sol: String,
    pub luna: String,
    pub terra: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CodexDefault {
    model: String,
    effort: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Defaults {
    codex: CodexDefault,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    schema_version: u64,
    source: String,
    checked_at: String,
    trigger: String,
    families: Families,
    defaults: Defaults,
}

fn stable_id(id: &str, family: &str) -> bool {
    let Some(version) = id
        .strip_prefix("gpt-")
        .and_then(|rest| rest.strip_suffix(&format!("-{family}")))
    else {
        return false;
    };
    let mut parts = version.split('.');
    let major = parts.next().unwrap_or_default();
    let minor = parts.next();
    !major.is_empty()
        && !major.starts_with('0')
        && major.bytes().all(|byte| byte.is_ascii_digit())
        && minor
            .is_none_or(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && parts.next().is_none()
}

fn valid_checked_at(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return false;
    }
    let number = |start: usize, end: usize| -> Option<u32> {
        let part = bytes.get(start..end)?;
        part.iter().all(u8::is_ascii_digit).then(|| {
            part.iter()
                .fold(0_u32, |value, digit| value * 10 + u32::from(digit - b'0'))
        })
    };
    let (Some(year), Some(month), Some(day)) = (number(0, 4), number(5, 7), number(8, 10)) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    year > 0
        && (1..=days).contains(&day)
        && matches!(number(11, 13), Some(0..=23))
        && matches!(number(14, 16), Some(0..=59))
        && matches!(number(17, 19), Some(0..=59))
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let value = crate::server_settings::parse_strict_json(bytes, 64 * 1024)?;
        let document: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        if document.schema_version != 1 {
            return Err("unsupported model manifest schema".to_string());
        }
        if document.source != "openrouter+models.dev-merge"
            && document.source != "openai-models+openrouter-cross-check"
        {
            return Err("untrusted model manifest source".to_string());
        }
        if !matches!(document.trigger.as_str(), "manual" | "nightly") {
            return Err("invalid model manifest trigger".to_string());
        }
        if !valid_checked_at(&document.checked_at) {
            return Err("invalid model manifest timestamp".to_string());
        }
        if !stable_id(&document.families.sol, "sol")
            || !stable_id(&document.families.luna, "luna")
            || document
                .families
                .terra
                .as_deref()
                .is_some_and(|id| !stable_id(id, "terra"))
        {
            return Err("invalid stable Sol or Luna model ID".to_string());
        }
        if document.defaults.codex.model != document.families.sol
            || document.defaults.codex.effort != "low"
        {
            return Err("manifest Codex default must be Sol/low".to_string());
        }
        Ok(document)
    }

    pub fn families(&self) -> &Families {
        &self.families
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ModelList {
    data: Vec<Value>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}

pub fn usable_low_effort_ids(result: &serde_json::Value) -> Result<Vec<String>, String> {
    let list: ModelList = serde_json::from_value(result.clone()).map_err(|e| e.to_string())?;
    Ok(list
        .data
        .into_iter()
        .filter(|model| {
            model.get("hidden").is_none_or(|hidden| hidden == false)
                && model
                    .get("supportedReasoningEfforts")
                    .and_then(Value::as_array)
                    .is_some_and(|efforts| {
                        efforts.iter().any(|effort| {
                            effort.get("reasoningEffort").and_then(Value::as_str) == Some("low")
                        })
                    })
        })
        .filter_map(|model| model.get("id")?.as_str().map(str::to_string))
        .collect())
}

type PageResult = Result<(Value, usize), String>;
type ModelPageFetch<'a> = dyn FnMut(usize, Option<&str>, Instant) -> PageResult + 'a;

fn collect_model_pages(
    deadline: Instant,
    fetch_page: &mut ModelPageFetch<'_>,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut total_bytes = 0_usize;
    for page in 0..MAX_MODEL_PAGES {
        if Instant::now() >= deadline {
            return Err("Codex model list timed out".to_string());
        }
        let (result, bytes) = fetch_page(page + 1, cursor.as_deref(), deadline)?;
        if Instant::now() >= deadline {
            return Err("Codex model list timed out".to_string());
        }
        total_bytes = total_bytes.saturating_add(bytes);
        if total_bytes > MAX_MODEL_BYTES {
            return Err("Codex model list pages are too large".to_string());
        }
        let list: ModelList = serde_json::from_value(result.clone()).map_err(|e| e.to_string())?;
        ids.extend(usable_low_effort_ids(&result)?);
        match list.next_cursor {
            None => return Ok(ids),
            Some(next) if !next.is_empty() && next.len() <= 1024 && seen.insert(next.clone()) => {
                cursor = Some(next);
            }
            Some(_) => return Err("Codex model list returned an invalid cursor".to_string()),
        }
    }
    Err("Codex model list exceeded the page limit".to_string())
}

/// Ask the installed Codex App Server which models this account can use at low effort.
pub fn local_low_effort_models() -> Result<Vec<String>, String> {
    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(vec!["codex".into(), "app-server".into()]),
        cwd: None,
        env: None,
        capture: true,
        stderr_mode: StderrMode::Pipe,
        creationflags: crate::win_creation_flags::invisible_helper_creationflags(),
        create_process_group: false,
        stdin_mode: StdinMode::Piped,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().map_err(|error| error.to_string())?;
    let messages = concat!(
        "{\"method\":\"initialize\",\"id\":0,\"params\":{\"clientInfo\":{\"name\":\"clud\",\"title\":\"clud\",\"version\":\"1\"}}}\n",
        "{\"method\":\"initialized\",\"params\":{}}\n",
        "{\"method\":\"model/list\",\"id\":1,\"params\":{\"limit\":100,\"includeHidden\":true}}\n",
    );
    let result = (|| {
        process
            .write_stdin_streaming(messages.as_bytes())
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(3);
        collect_model_pages(deadline, &mut |request_id, cursor, deadline| {
            if let Some(cursor) = cursor {
                let request = serde_json::json!({
                    "method": "model/list",
                    "id": request_id,
                    "params": {"limit": 100, "includeHidden": true, "cursor": cursor},
                });
                process
                    .write_stdin_streaming(format!("{request}\n").as_bytes())
                    .map_err(|error| error.to_string())?;
            }
            loop {
                if Instant::now() >= deadline {
                    return Err("Codex model/list timed out".to_string());
                }
                match process.read_stream(StreamKind::Stdout, Some(Duration::from_millis(50))) {
                    ReadStatus::Line(bytes) => {
                        if bytes.len() > MAX_MODEL_FRAME_BYTES {
                            return Err("Codex model/list response is too large".to_string());
                        }
                        let Ok(frame) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                            continue;
                        };
                        if frame.get("id").and_then(serde_json::Value::as_u64)
                            == Some(request_id as u64)
                        {
                            return match frame.get("result") {
                                Some(value) => Ok((value.clone(), bytes.len())),
                                None => Err(format!("Codex model/list failed: {}", frame["error"])),
                            };
                        }
                    }
                    ReadStatus::Eof => {
                        return Err("Codex app-server closed before model/list".to_string())
                    }
                    ReadStatus::Timeout => {}
                }
            }
        })
    })();
    let _ = process.kill();
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Published,
    LastGood,
    BuiltIn,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::LastGood => "last_good",
            Self::BuiltIn => "built_in",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub sol: String,
    pub luna: String,
    pub terra: String,
    pub sol_source: Source,
    pub luna_source: Source,
    pub terra_source: Source,
}

impl Choice {
    pub fn source_for(&self, cli_id: &str) -> Option<Source> {
        match cli_id {
            "codex-sol" => Some(self.sol_source),
            "codex-luna" => Some(self.luna_source),
            "codex-terra" => Some(self.terra_source),
            _ => None,
        }
    }
}

fn choose_family(
    published: Option<&str>,
    last_good: Option<&str>,
    available: &[String],
    built_in: &str,
) -> (String, Source) {
    if let Some(id) = published.filter(|id| available.iter().any(|item| item == id)) {
        return (id.to_string(), Source::Published);
    }
    if let Some(id) = last_good.filter(|id| available.iter().any(|item| item == id)) {
        return (id.to_string(), Source::LastGood);
    }
    (built_in.to_string(), Source::BuiltIn)
}

fn stable_version(id: &str, family: &str) -> Option<(u64, u64)> {
    if !stable_id(id, family) {
        return None;
    }
    let version = id
        .strip_prefix("gpt-")?
        .strip_suffix(&format!("-{family}"))?;
    let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn bundled_model(
    family: &str,
    published: Option<&Manifest>,
    available: &[String],
) -> Option<String> {
    let document: Value = serde_json::from_str(BUNDLED_MODELS).ok()?;
    let checked_at = document.get("checked_at")?.as_str()?;
    if !valid_checked_at(checked_at)
        || published.is_some_and(|manifest| manifest.checked_at.as_str() >= checked_at)
    {
        return None;
    }
    let model = document.get("families")?.get(family)?.as_str()?;
    (stable_id(model, family) && available.iter().any(|item| item == model))
        .then(|| model.to_string())
}

fn choose(
    published: Option<&Manifest>,
    last_good: Option<&LastGood>,
    available: &[String],
) -> Choice {
    let (sol, sol_source) = choose_family(
        published.map(|value| value.families.sol.as_str()),
        last_good.and_then(|value| value.sol.as_deref()),
        available,
        FALLBACK_SOL,
    );
    let (luna, luna_source) = choose_family(
        published.map(|value| value.families.luna.as_str()),
        last_good.and_then(|value| value.luna.as_deref()),
        available,
        FALLBACK_LUNA,
    );
    let (terra, terra_source) = choose_family(
        published.and_then(|value| value.families.terra.as_deref()),
        last_good.and_then(|value| value.terra.as_deref()),
        available,
        FALLBACK_TERRA,
    );
    let (sol, sol_source) = bundled_model("sol", published, available)
        .filter(|model| stable_version(model, "sol") > stable_version(&sol, "sol"))
        .map_or((sol, sol_source), |model| (model, Source::BuiltIn));
    let (luna, luna_source) = bundled_model("luna", published, available)
        .filter(|model| stable_version(model, "luna") > stable_version(&luna, "luna"))
        .map_or((luna, luna_source), |model| (model, Source::BuiltIn));
    let (terra, terra_source) = bundled_model("terra", published, available)
        .filter(|model| stable_version(model, "terra") > stable_version(&terra, "terra"))
        .map_or((terra, terra_source), |model| (model, Source::BuiltIn));
    Choice {
        sol,
        luna,
        terra,
        sol_source,
        luna_source,
        terra_source,
    }
}

fn updated_last_good(previous: Option<&LastGood>, choice: &Choice) -> LastGood {
    let mut state = previous.cloned().unwrap_or(LastGood {
        schema_version: 1,
        sol: None,
        luna: None,
        terra: None,
    });
    if choice.sol_source == Source::Published {
        state.sol = Some(choice.sol.clone());
    } else if choice.sol_source == Source::LastGood {
        state.sol.get_or_insert_with(|| choice.sol.clone());
    }
    if choice.luna_source == Source::Published {
        state.luna = Some(choice.luna.clone());
    } else if choice.luna_source == Source::LastGood {
        state.luna.get_or_insert_with(|| choice.luna.clone());
    }
    if choice.terra_source == Source::Published {
        state.terra = Some(choice.terra.clone());
    } else if choice.terra_source == Source::LastGood {
        state.terra.get_or_insert_with(|| choice.terra.clone());
    }
    state
}

/// How long a writer waits for another's model-cache update to finish. The
/// holder does an atomic write (temp file, fsync, rename), which took longer
/// than the previous 250 ms on a loaded Windows runner and made a concurrent
/// launch lose its update (#1729). Bounded, so a launch never hangs on it.
const MODEL_CACHE_LOCK_WAIT: Duration = Duration::from_secs(5);

fn persist_last_good(
    path: &Path,
    fallback: Option<&LastGood>,
    choice: &Choice,
) -> Result<(), String> {
    let parent = path.parent().ok_or("model cache has no parent directory")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("json.lock"))
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + MODEL_CACHE_LOCK_WAIT;
    loop {
        match FileExt::try_lock_exclusive(&lock) {
            Ok(true) => break,
            Ok(false) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(false) => return Err("timed out waiting for model cache lock".to_string()),
            Err(error) => return Err(error.to_string()),
        }
    }
    let stored = read_last_good(path);
    let updated = updated_last_good(stored.as_ref().or(fallback), choice);
    if stored.as_ref() != Some(&updated) {
        write_json_atomic(path, &updated)?;
    }
    Ok(())
}

/// The per-process effective dynamic Codex choice. Explicit pins never use it.
pub fn active_choice() -> &'static Choice {
    static CHOICE: OnceLock<Choice> = OnceLock::new();
    CHOICE.get_or_init(load_choice)
}

/// Resolve a stable catalog row to its current account-checked wire ID.
pub fn wire_id(cli_id: &str, built_in: &str) -> String {
    if matches!(cli_id, "codex-sol" | "codex-luna" | "codex-terra") {
        wire_id_for_choice(cli_id, built_in, active_choice())
    } else {
        built_in.to_string()
    }
}

fn wire_id_for_choice(cli_id: &str, built_in: &str, choice: &Choice) -> String {
    match cli_id {
        "codex-sol" => choice.sol.clone(),
        "codex-luna" => choice.luna.clone(),
        "codex-terra" => choice.terra.clone(),
        _ => built_in.to_string(),
    }
}

fn manifest_or_cached(
    result: Result<Manifest, String>,
    cached: Option<&Manifest>,
) -> Option<Manifest> {
    match result {
        Ok(manifest) => Some(manifest),
        Err(error) => {
            eprintln!("[clud] model manifest unavailable ({error}); checking cached models");
            cached.cloned()
        }
    }
}

fn load_choice() -> Choice {
    if cfg!(test) {
        return choose(None, None, &[]);
    }
    let url_override = std::env::var("CLUD_MODEL_MANIFEST_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let path = url_override.is_none().then(cache_path).flatten();
    let cached = path.as_deref().and_then(read_cached);
    let good_path = url_override.is_none().then(last_good_path).flatten();
    let last_good = good_path
        .as_deref()
        .and_then(read_last_good)
        .or_else(|| cached.as_ref().map(LastGood::from_manifest));
    let fresh = cached.is_some() && path.as_deref().is_some_and(cache_fresh);
    let recently_attempted = match path.as_deref() {
        Some(candidate) => cache_fresh(&attempt_path(candidate)),
        None => false,
    };
    let served = if fresh || recently_attempted {
        cached.clone()
    } else {
        if let Some(path) = path.as_deref() {
            record_attempt(path);
        }
        let result = match url_override.as_deref() {
            Some(url) => fetch_manifest(url),
            None => fetch_first_manifest(crate::published_urls::MODEL_MANIFEST),
        };
        manifest_or_cached(result, cached.as_ref())
    };
    let available = match local_low_effort_models() {
        Ok(models) => models,
        Err(error) => {
            eprintln!("[clud] Codex model/list unavailable ({error}); using built-in Sol/low");
            Vec::new()
        }
    };
    let choice = choose(served.as_ref(), last_good.as_ref(), &available);
    if !fresh && !recently_attempted {
        if let (Some(path), Some(manifest)) = (path.as_deref(), served.as_ref()) {
            if let Err(error) = write_json_atomic(path, manifest) {
                eprintln!("[clud] could not cache model manifest: {error}");
            }
        }
    }
    if let Some(path) = good_path.as_deref() {
        if let Err(error) = persist_last_good(path, last_good.as_ref(), &choice) {
            eprintln!("[clud] could not cache last-good Codex models: {error}");
        }
    }
    if choice.sol_source != Source::Published || choice.luna_source != Source::Published {
        eprintln!(
            "[clud] Codex model sources: Sol {} ({:?}), Luna {} ({:?}); effort low",
            choice.sol, choice.sol_source, choice.luna, choice.luna_source
        );
    }
    choice
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(sol: &str, luna: &str) -> Manifest {
        let document = json!({
            "schema_version": 1,
            "source": "openrouter+models.dev-merge",
            "checked_at": "2026-09-26T21:00:00Z",
            "trigger": "manual",
            "families": {"sol": sol, "luna": luna, "terra": "gpt-5.6-terra"},
            "defaults": {"codex": {"model": sol, "effort": "low"}}
        });
        Manifest::parse(document.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn malformed_preview_id_is_rejected() {
        let mut value = serde_json::to_value(manifest("gpt-6-sol", "gpt-6-luna")).unwrap();
        value["families"]["sol"] = json!("gpt-7-sol-preview");
        assert!(Manifest::parse(value.to_string().as_bytes()).is_err());
    }

    #[test]
    fn malformed_timestamp_is_rejected() {
        let mut value = serde_json::to_value(manifest("gpt-6-sol", "gpt-6-luna")).unwrap();
        value["checked_at"] = json!("not-a-real-timestampZ");
        assert!(Manifest::parse(value.to_string().as_bytes()).is_err());
        value["checked_at"] = json!("2026-19-26T21:00:00Z");
        assert!(Manifest::parse(value.to_string().as_bytes()).is_err());
    }

    #[test]
    fn legacy_published_source_remains_usable_during_migration() {
        let mut value = serde_json::to_value(manifest("gpt-6-sol", "gpt-6-luna")).unwrap();
        value["source"] = json!("openai-models+openrouter-cross-check");
        assert!(Manifest::parse(value.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn future_additive_fields_do_not_break_a_valid_document() {
        let mut value = serde_json::to_value(manifest("gpt-7-sol", "gpt-7-luna")).unwrap();
        value["future"] = json!({"publisher": "example"});
        value["families"]["future"] = json!("gpt-7-terra");
        value["defaults"]["codex"]["future"] = json!(true);
        assert!(Manifest::parse(value.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn fake_model_list_filters_hidden_and_unsupported_efforts() {
        let result = json!({"data": [
            {"id": "gpt-6-sol", "supportedReasoningEfforts": [{"reasoningEffort": "low"}]},
            {"id": "gpt-7-sol", "supportedReasoningEfforts": [{"reasoningEffort": "high"}]},
            {"id": "gpt-6-luna", "hidden": true, "supportedReasoningEfforts": [{"reasoningEffort": "low"}]}
        ]});
        assert_eq!(usable_low_effort_ids(&result).unwrap(), ["gpt-6-sol"]);
    }

    #[test]
    fn published_then_last_good_then_builtin() {
        let latest = manifest("gpt-7-sol", "gpt-7-luna");
        let previous = manifest("gpt-6-sol", "gpt-6-luna");
        let previous = LastGood::from_manifest(&previous);
        let all = ["gpt-7-sol", "gpt-7-luna", "gpt-6-sol", "gpt-6-luna"].map(str::to_string);
        assert_eq!(
            choose(Some(&latest), Some(&previous), &all).sol_source,
            Source::Published
        );
        assert_eq!(
            choose(Some(&latest), Some(&previous), &all[2..]).sol_source,
            Source::LastGood
        );
        let fallback = choose(Some(&latest), Some(&previous), &[]);
        assert_eq!(fallback.sol_source, Source::BuiltIn);
        assert_eq!(fallback.sol, "gpt-6-sol");
    }

    #[test]
    fn bundled_sol_and_terra_are_selected_when_locally_available() {
        let available = ["gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-luna"].map(str::to_string);
        let choice = choose(None, None, &available);
        assert_eq!(choice.sol, "gpt-6.1-sol");
        assert_eq!(choice.terra, "gpt-6.1-terra");
        assert_eq!(choice.sol_source, Source::BuiltIn);
        assert_eq!(choice.terra_source, Source::BuiltIn);
        assert_eq!(
            wire_id_for_choice("codex-terra", FALLBACK_TERRA, &choice),
            "gpt-6.1-terra"
        );
    }

    #[test]
    fn newer_bundled_models_supersede_stale_publication_but_not_newer_rollback() {
        let available = ["gpt-6-sol", "gpt-6.1-sol", "gpt-6.1-terra"].map(str::to_string);
        let mut published = manifest("gpt-6-sol", "gpt-6-luna");
        let choice = choose(Some(&published), None, &available);
        assert_eq!(choice.sol, "gpt-6.1-sol");
        assert_eq!(choice.terra, "gpt-6.1-terra");
        published.checked_at = "2099-01-01T00:00:00Z".to_string();
        let rollback = choose(Some(&published), None, &available);
        assert_eq!(rollback.sol, "gpt-6-sol");
        assert_eq!(rollback.sol_source, Source::Published);
        published.checked_at = "2026-09-26T21:00:00Z".to_string();
        published.families.sol = "gpt-7-sol".to_string();
        let mut newer_available = available.to_vec();
        newer_available.push("gpt-7-sol".to_string());
        let newer = choose(Some(&published), None, &newer_available);
        assert_eq!(newer.sol, "gpt-7-sol");
    }

    #[test]
    fn failed_refresh_retains_newer_cached_rollback() {
        let mut rollback = manifest("gpt-6-sol", "gpt-6-luna");
        rollback.checked_at = "2099-01-01T00:00:00Z".to_string();
        let served = manifest_or_cached(Err("offline".to_string()), Some(&rollback));
        let available = ["gpt-6-sol", "gpt-6.1-sol"].map(str::to_string);
        let choice = choose(served.as_ref(), None, &available);
        assert_eq!(choice.sol, "gpt-6-sol");
        assert_eq!(choice.sol_source, Source::Published);
    }

    #[test]
    fn bundled_luna_advances_an_older_available_publication() {
        let published = manifest("gpt-6-sol", "gpt-5.6-luna");
        let available = ["gpt-5.6-luna", "gpt-6-luna"].map(str::to_string);
        let choice = choose(Some(&published), None, &available);
        assert_eq!(choice.luna, "gpt-6-luna");
        assert_eq!(choice.luna_source, Source::BuiltIn);
    }

    #[test]
    fn terra_falls_back_when_not_locally_available() {
        let available = ["gpt-6.1-sol", "gpt-6-luna"].map(str::to_string);
        let choice = choose(None, None, &available);
        assert_eq!(choice.terra, FALLBACK_TERRA);
    }

    #[test]
    fn issue_1480_sol_and_luna_rollouts_are_independent() {
        let latest = manifest("gpt-7-sol", "gpt-7-luna");
        let previous = manifest("gpt-6-sol", "gpt-6-luna");
        let previous = LastGood::from_manifest(&previous);
        let sol_only = ["gpt-7-sol", "gpt-6-luna"].map(str::to_string);
        let choice = choose(Some(&latest), Some(&previous), &sol_only);
        assert_eq!(choice.sol, "gpt-7-sol");
        assert_eq!(choice.luna, "gpt-6-luna");
        assert_eq!(choice.sol_source, Source::Published);
        assert_eq!(choice.luna_source, Source::LastGood);
        assert_eq!(choice.source_for("codex-sol"), Some(Source::Published));
        assert_eq!(choice.source_for("codex-luna"), Some(Source::LastGood));

        let no_luna = ["gpt-7-sol".to_string()];
        let choice = choose(Some(&latest), Some(&previous), &no_luna);
        assert_eq!(choice.sol, "gpt-7-sol");
        assert_eq!(choice.luna, FALLBACK_LUNA);
        assert_eq!(choice.luna_source, Source::BuiltIn);

        let luna_only = ["gpt-6-sol", "gpt-7-luna"].map(str::to_string);
        let choice = choose(Some(&latest), Some(&previous), &luna_only);
        assert_eq!(choice.sol, "gpt-6-sol");
        assert_eq!(choice.luna, "gpt-7-luna");
        assert_eq!(choice.sol_source, Source::LastGood);
        assert_eq!(choice.luna_source, Source::Published);
    }

    #[test]
    fn issue_1480_unrelated_missing_efforts_does_not_discard_valid_rows() {
        let result = json!({"data": [
            {"id": "unrelated"},
            {"id": "gpt-7-sol", "supportedReasoningEfforts": [{"reasoningEffort": "low"}]},
            {"id": "gpt-7-luna", "supportedReasoningEfforts": "malformed"}
        ]});
        assert_eq!(usable_low_effort_ids(&result).unwrap(), ["gpt-7-sol"]);
    }

    #[test]
    fn issue_1480_last_good_survives_asymmetric_refresh_and_offline_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("last-good.json");
        let old = LastGood::from_manifest(&manifest("gpt-6-sol", "gpt-6-luna"));
        let published = manifest("gpt-7-sol", "gpt-7-luna");
        let available = ["gpt-7-sol", "gpt-6-luna"].map(str::to_string);
        let choice = choose(Some(&published), Some(&old), &available);
        persist_last_good(&path, Some(&old), &choice).unwrap();
        let saved = read_last_good(&path).unwrap();
        assert_eq!(saved.sol.as_deref(), Some("gpt-7-sol"));
        assert_eq!(saved.luna.as_deref(), Some("gpt-6-luna"));

        let offline = choose(None, Some(&saved), &available);
        assert_eq!(offline.sol, "gpt-7-sol");
        assert_eq!(offline.luna, "gpt-6-luna");
        assert_eq!(offline.sol_source, Source::LastGood);
        assert_eq!(offline.luna_source, Source::LastGood);
    }

    #[test]
    fn issue_1480_concurrent_family_updates_merge_without_corrupting_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("last-good.json");
        let old = LastGood::from_manifest(&manifest("gpt-6-sol", "gpt-6-luna"));
        write_json_atomic(&path, &old).unwrap();
        let published = manifest("gpt-7-sol", "gpt-7-luna");
        let sol = choose(
            Some(&published),
            Some(&old),
            &["gpt-7-sol", "gpt-6-luna"].map(str::to_string),
        );
        let luna = choose(
            Some(&published),
            Some(&old),
            &["gpt-6-sol", "gpt-7-luna"].map(str::to_string),
        );
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            for choice in [&sol, &luna] {
                let barrier = &barrier;
                let path = &path;
                let old = &old;
                scope.spawn(move || {
                    barrier.wait();
                    persist_last_good(path, Some(old), choice).unwrap();
                });
            }
            barrier.wait();
        });
        let saved = read_last_good(&path).unwrap();
        assert_eq!(saved.sol.as_deref(), Some("gpt-7-sol"));
        assert_eq!(saved.luna.as_deref(), Some("gpt-7-luna"));
    }

    /// #1729: a writer that holds the cache lock longer than the old 250 ms
    /// deadline (a slow atomic write on a loaded host) must not make a
    /// concurrent update fail.
    #[test]
    fn a_slow_lock_holder_does_not_make_a_concurrent_update_fail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("last-good.json");
        let old = LastGood::from_manifest(&manifest("gpt-6-sol", "gpt-6-luna"));
        let choice = choose(
            Some(&manifest("gpt-7-sol", "gpt-7-luna")),
            Some(&old),
            &["gpt-7-sol", "gpt-7-luna"].map(str::to_string),
        );
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("json.lock"))
            .unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            drop(lock);
        });
        persist_last_good(&path, Some(&old), &choice).expect("waits out the holder");
        holder.join().unwrap();
        assert_eq!(
            read_last_good(&path).unwrap().sol.as_deref(),
            Some("gpt-7-sol")
        );
    }

    #[test]
    fn issue_1480_page_two_is_discovered_without_accepting_hidden_or_missing_low() {
        let rows = [
            json!({"data": [
                {"id": "gpt-8-sol", "hidden": true, "supportedReasoningEfforts": [{"reasoningEffort": "low"}]},
                {"id": "gpt-8-luna"}
            ], "nextCursor": "next"}),
            json!({"data": [
                {"id": "gpt-7-sol", "supportedReasoningEfforts": [{"reasoningEffort": "low"}]},
                {"id": "gpt-7-luna", "supportedReasoningEfforts": [{"reasoningEffort": "high"}]}
            ], "nextCursor": null}),
        ];
        let found = collect_model_pages(
            Instant::now() + Duration::from_secs(1),
            &mut |page, cursor, _| {
                assert_eq!(cursor, if page == 1 { None } else { Some("next") });
                Ok((rows[page - 1].clone(), rows[page - 1].to_string().len()))
            },
        )
        .unwrap();
        assert_eq!(found, ["gpt-7-sol"]);
    }

    #[test]
    fn issue_1480_incomplete_or_malformed_listing_is_not_treated_as_absence() {
        let deadline = || Instant::now() + Duration::from_secs(1);
        let invalid = collect_model_pages(deadline(), &mut |_, _, _| Ok((json!({"data": {}}), 12)));
        assert!(invalid.is_err());

        let repeated = collect_model_pages(deadline(), &mut |_, _, _| {
            Ok((json!({"data": [], "nextCursor": "again"}), 32))
        });
        assert!(repeated.unwrap_err().contains("cursor"));

        let too_many = collect_model_pages(deadline(), &mut |page, _, _| {
            Ok((
                json!({"data": [], "nextCursor": format!("page-{page}")}),
                32,
            ))
        });
        assert!(too_many.unwrap_err().contains("page limit"));

        let too_large = collect_model_pages(deadline(), &mut |_, _, _| {
            Ok((json!({"data": [], "nextCursor": null}), MAX_MODEL_BYTES + 1))
        });
        assert!(too_large.unwrap_err().contains("too large"));

        let expired =
            collect_model_pages(Instant::now() - Duration::from_millis(1), &mut |_, _, _| {
                panic!("expired lookup must not request a page")
            });
        assert!(expired.unwrap_err().contains("timed out"));
    }

    #[test]
    fn stable_discovery_rows_map_to_future_wire_ids() {
        let future = manifest("gpt-7-sol", "gpt-7-luna");
        let available = ["gpt-7-sol".to_string(), "gpt-7-luna".to_string()];
        let choice = choose(Some(&future), None, &available);
        assert_eq!(
            wire_id_for_choice("codex-sol", FALLBACK_SOL, &choice),
            "gpt-7-sol"
        );
        assert_eq!(
            wire_id_for_choice("codex-luna", FALLBACK_LUNA, &choice),
            "gpt-7-luna"
        );
        assert_eq!(
            wire_id_for_choice("codex-terra", "gpt-5.6-terra", &choice),
            "gpt-5.6-terra"
        );
    }
}
