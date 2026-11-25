use std::path::Path;

use super::types::{LoopMarkers, LoopVerification};
use crate::loop_spec::{
    self, blocked_path_from_done, cache_path, classify, fetch_via_gh, render_cache,
    resolve_current_repo, GhKind, MarkerPaths, TaskSpec,
};

pub(super) fn marker_contract(paths: &MarkerPaths, minimum: Option<u32>) -> String {
    let mut contract = loop_spec::done_marker_contract(&paths.done, &paths.blocked);
    if let Some(minimum) = minimum {
        contract.push_str(&format!(
            "\nDo not signal DONE before iteration {minimum}. Check the current iteration in .clud/loop/info.json at the git root. Until then, record progress and return control for the next iteration.\n"
        ));
    }
    contract
}

pub(super) fn launch_markers(
    paths: MarkerPaths,
    min_iterations: Option<u32>,
    task: String,
    git_root: std::path::PathBuf,
    worker_prompt_index: usize,
) -> LoopMarkers {
    LoopMarkers {
        done_path: paths.done.to_string_lossy().into_owned(),
        blocked_path: paths.blocked.to_string_lossy().into_owned(),
        min_iterations,
        verification: Some(LoopVerification {
            task,
            git_root,
            worker_prompt_index,
            command: Vec::new(),
        }),
    }
}

pub(super) fn resolve_marker_paths(
    cwd: &Path,
    git_root: &Path,
    done_override: Option<&str>,
) -> MarkerPaths {
    match done_override {
        Some(raw) => {
            let done = cwd.join(raw);
            let blocked = blocked_path_from_done(&done);
            MarkerPaths { done, blocked }
        }
        None => loop_spec::default_marker_paths(git_root),
    }
}

/// Resolve the `clud loop` positional to an actual prompt body.
///
/// - GH issue/PR URL → fetch via `gh`, cache, return rendered body.
/// - Short-form `#42` → resolve owner/repo via `gh repo view`, then fetch.
/// - Local file path → read contents.
/// - Literal string → return as-is.
pub(super) fn resolve_loop_task(task: &str, git_root: &Path, refresh: bool) -> String {
    match classify(task) {
        TaskSpec::GhIssue {
            owner,
            repo,
            kind,
            number,
        } => fetch_and_cache_or_die(git_root, &owner, &repo, kind, number, refresh),
        TaskSpec::ShortForm(number) => {
            let (owner, repo) = resolve_current_repo().unwrap_or_else(|e| {
                eprintln!("error: `{task}` requires a GH remote; could not resolve via `gh`: {e}");
                std::process::exit(1);
            });
            fetch_and_cache_or_die(git_root, &owner, &repo, GhKind::Issue, number, refresh)
        }
        TaskSpec::File(path) => match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(e) => {
                eprintln!(
                    "error: failed to read task file '{}': {}",
                    path.display(),
                    e
                );
                std::process::exit(1);
            }
        },
        TaskSpec::Literal(s) => s,
    }
}

fn fetch_and_cache_or_die(
    git_root: &Path,
    owner: &str,
    repo: &str,
    kind: GhKind,
    number: u32,
    refresh: bool,
) -> String {
    let cache = cache_path(git_root, owner, repo, kind, number);
    if !refresh {
        if let Ok(existing) = std::fs::read_to_string(&cache) {
            eprintln!("[clud loop] using cached {}", cache.display());
            return strip_frontmatter(&existing);
        }
    }
    match fetch_via_gh(owner, repo, kind, number) {
        Ok(doc) => {
            let fetched_at = chrono_like_now();
            let rendered = render_cache(&doc, &fetched_at);
            if let Err(e) = loop_spec::ensure_loop_dir(git_root) {
                eprintln!(
                    "[clud loop] warning: could not create {}: {}",
                    loop_spec::LOOP_DIR,
                    e
                );
            }
            if let Err(e) = std::fs::write(&cache, &rendered) {
                eprintln!(
                    "[clud loop] warning: could not write cache {}: {}",
                    cache.display(),
                    e
                );
            } else {
                eprintln!("[clud loop] cached {}", cache.display());
            }
            strip_frontmatter(&rendered)
        }
        Err(e) => {
            eprintln!(
                "error: failed to fetch GH {} {}/{} #{}: {}",
                match kind {
                    GhKind::Issue => "issue",
                    GhKind::Pr => "pull request",
                },
                owner,
                repo,
                number,
                e
            );
            std::process::exit(1);
        }
    }
}

/// Strip a leading `---\n...\n---\n\n` frontmatter block.
fn strip_frontmatter(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            let after = &rest[end + "\n---\n".len()..];
            return after.trim_start_matches('\n').to_string();
        }
    }
    s.to_string()
}

/// ISO-8601 UTC timestamp via system time; avoids pulling chrono. The
/// calendar arithmetic lives in `crate::civil_time` (#1206); this function
/// owns only the format string, which is baked into the cached
/// `.clud/loop/` documents already on disk and must stay byte-for-byte the
/// same.
fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, se) = crate::civil_time::civil_from_unix_secs(secs as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z")
}
