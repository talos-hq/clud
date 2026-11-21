//! Early installer entry and automatic-offer eligibility.

use std::io::{self, IsTerminal, Read};
use std::time::Duration;

use crate::args::Args;
use crate::selector::{Style, Tone};

use super::activation::{self, ActivationPlan};
use super::catalog::{Arch, Catalog, GnuEligibility, Host, Os, ResolvedAsset, VersionChoice};
use super::picker::{self, ConfirmChoice, MenuChoice, ReleaseChoice};
use super::result::Outcome;
use super::summary::Facts;
use super::transaction::{self, InstallPlan};

const CANDIDATE_CATALOG_NAME: &str = "installer-candidate-manifest.json";
pub const INSTALL_PAGE: &str = "https://talos-hq.github.io/clud/install/index.html";
const MAX_CATALOG_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Menu,
    Current,
    Exact(String),
}

#[derive(Clone, Copy)]
struct InstallOptions {
    yes: bool,
    modify_path: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            yes: false,
            modify_path: true,
        }
    }
}

pub fn explicit_request(args: &Args) -> Result<Option<Request>, String> {
    if !args.installer {
        return Ok(None);
    }
    // An installer invocation has no backend argv. Keep unrelated launch
    // flags from changing behavior when the menu closes.
    let mut words = args.raw_argv.iter().skip(1);
    while let Some(word) = words.next() {
        match word.as_str() {
            "--installer"
            | "--installer-json"
            | "--installer-plan-json"
            | "--install-current"
            | "--no-modify-path"
            | "--yes"
            | "-y" => {}
            "--install-version" => {
                words.next().ok_or("--install-version requires a version")?;
            }
            _ if word.starts_with("--install-version=") => {}
            _ => return Err(format!("{word} cannot be combined with --installer")),
        }
    }
    if args.yes && !args.install_current && args.install_version.is_none() {
        return Err("--yes requires --install-current or --install-version".into());
    }
    if args.installer_json && !args.install_current && args.install_version.is_none() {
        return Err("--installer-json requires --install-current or --install-version".into());
    }
    if args.installer_plan_json && !args.install_current && args.install_version.is_none() {
        return Err("--installer-plan-json requires --install-current or --install-version".into());
    }
    if args.install_current {
        Ok(Some(Request::Current))
    } else if let Some(version) = &args.install_version {
        crate::self_install::catalog::validate_version(version)?;
        Ok(Some(Request::Exact(version.clone())))
    } else {
        Ok(Some(Request::Menu))
    }
}

pub fn should_offer(args: &Args, stdin_tty: bool, stderr_tty: bool, path_has_clud: bool) -> bool {
    stdin_tty
        && stderr_tty
        && !path_has_clud
        && !args.installer
        && args.command.is_none()
        && !args.dry_run
        && args.prompt.is_none()
        && args.message.is_none()
        && args.passthrough.is_empty()
        && args.raw_argv.len() == 1
}

/// Run an explicit installer request before launch setup has side effects.
pub fn run_explicit(args: &Args) -> Option<i32> {
    if args.installer_plan_json {
        return match explicit_request(args) {
            Ok(None) => None,
            Ok(Some(request)) => Some(print_plan(request, !args.no_modify_path)),
            Err(error) => {
                eprintln!("clud installer: {error}");
                Some(2)
            }
        };
    }
    let outcome = match explicit_request(args) {
        Ok(None) => return None,
        Ok(Some(request)) => run_request(
            request,
            InstallOptions {
                yes: args.yes,
                modify_path: !args.no_modify_path,
            },
            false,
        ),
        Err(error) => {
            eprintln!("clud installer: {error}");
            Outcome::failed(2, "invalid_request")
        }
    };
    if !args.installer_json {
        return Some(finish_with_provider_setup(args, &outcome));
    }
    match outcome.write_json(io::stdout().lock()) {
        Ok(()) => Some(outcome.exit_code()),
        Err(error) => {
            eprintln!("clud installer: cannot write structured outcome: {error}");
            Some(1)
        }
    }
}

/// Offer installation only for a bare interactive first run with no `clud`
/// executable resolvable by name on the effective PATH.
pub fn run_auto_offer(args: &Args) -> Option<i32> {
    let missing = matches!(
        which::which("clud"),
        Err(which::Error::CannotFindBinaryPath)
    );
    if !should_offer(
        args,
        io::stdin().is_terminal(),
        io::stderr().is_terminal(),
        !missing,
    ) {
        return None;
    }
    let plan = current_plan(InstallOptions::default());
    match picker::prompt_menu(plan.as_ref().ok().map(Facts::of).as_ref(), true) {
        Ok(MenuChoice::NotNow) => None,
        Ok(choice) => {
            let outcome = run_menu_choice(choice, plan, InstallOptions::default());
            Some(finish_with_provider_setup(args, &outcome))
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Some(130),
        Err(error) => {
            eprintln!("clud installer: {error}");
            Some(1)
        }
    }
}

/// `clud setup --write-profile`: the installer's own current-version install
/// with shell activation, so profile/PATH edits have exactly one owner.
pub fn install_current_with_profile(yes: bool) -> i32 {
    run_request(
        Request::Current,
        InstallOptions {
            yes,
            modify_path: true,
        },
        false,
    )
    .exit_code()
}

/// After a complete interactive install, continue into the provider step
/// `clud setup` runs (#2219): one implementation, two entry points.
/// Scripted (`--yes`), structured and non-terminal installs skip it.
fn finish_with_provider_setup(args: &Args, outcome: &Outcome) -> i32 {
    if !outcome.is_complete() || args.yes || !terminal_available() {
        return outcome.exit_code();
    }
    crate::setup_command::run_provider_step_interactive()
}

fn run_request(request: Request, options: InstallOptions, automatic: bool) -> Outcome {
    match request {
        Request::Menu => {
            if !terminal_available() {
                eprintln!("clud installer: a terminal is required without an explicit install choice and --yes");
                return Outcome::refused(2, "terminal_required");
            }
            let plan = current_plan(options);
            match picker::prompt_menu(plan.as_ref().ok().map(Facts::of).as_ref(), automatic) {
                Ok(choice) => run_menu_choice(choice, plan, options),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    Outcome::refused(130, "cancelled")
                }
                Err(error) => {
                    eprintln!("clud installer: {error}");
                    Outcome::failed(1, "menu_failed")
                }
            }
        }
        Request::Current => run_selected(InstallIntent::CurrentExecutable, options),
        Request::Exact(version) => {
            if !options.yes && !terminal_available() {
                eprintln!("clud installer: a terminal or --yes is required before installation");
                return Outcome::refused(2, "consent_required");
            }
            match published_intent(version) {
                Ok(intent) => run_selected(intent, options),
                Err((error, reason)) => {
                    eprintln!("clud installer: {error}");
                    Outcome::failed(1, reason)
                }
            }
        }
    }
}

/// Resolve an exact published version for this host from the catalog.
fn published_intent(version: String) -> Result<InstallIntent, (String, &'static str)> {
    let catalog = fetch_catalog().map_err(|error| (error, "catalog_unavailable"))?;
    let host = host().map_err(|error| (error, "unsupported_host"))?;
    catalog
        .resolve(VersionChoice::Exact(version), host)
        .map(InstallIntent::Published)
        .map_err(|error| (error, "version_unavailable"))
}

/// `--installer-plan-json`: print the plan an explicit request would show
/// for consent, as one JSON object on stdout. Planning never writes.
fn print_plan(request: Request, modify_path: bool) -> i32 {
    let intent = match request {
        Request::Current => Ok(InstallIntent::CurrentExecutable),
        Request::Exact(version) => published_intent(version).map_err(|(error, _)| error),
        Request::Menu => Err("--installer-plan-json requires an explicit install choice".into()),
    };
    match intent.and_then(|intent| transaction::plan(intent, modify_path)) {
        Ok(plan) => {
            println!("{}", Facts::of(&plan).report());
            0
        }
        Err(error) => {
            eprintln!("clud installer: {error}");
            1
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallIntent {
    CurrentExecutable,
    Published(ResolvedAsset),
}

fn run_selected(intent: InstallIntent, options: InstallOptions) -> Outcome {
    if !options.yes && !terminal_available() {
        eprintln!("clud installer: a terminal or --yes is required before installation");
        return Outcome::refused(2, "consent_required");
    }
    let plan = match transaction::plan(intent, options.modify_path) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("clud installer: {error}");
            return Outcome::failed(1, "planning_failed");
        }
    };
    if options.yes {
        // Unattended: no screen, so the log carries the exact plan.
        eprintln!("{}", plan.description());
    } else if let Some(refusal) = confirm_installation(&plan) {
        return refusal;
    }
    install_planned(plan, options)
}

/// The plan for installing the running executable, read before the first
/// screen so that screen can show it. A planning error is reported only when
/// the user chooses to install.
fn current_plan(options: InstallOptions) -> Result<InstallPlan, String> {
    transaction::plan(InstallIntent::CurrentExecutable, options.modify_path)
}

/// The running executable's plan with default options (PATH activation on),
/// for `Install clud.app`'s consent dialog.
pub(super) fn current_install_plan() -> Result<InstallPlan, String> {
    current_plan(InstallOptions::default())
}

/// Commit a plan consented to outside the terminal (the app bundle's
/// dialog): unattended from here on, so no further prompt can block.
pub(super) fn install_consented(plan: InstallPlan) -> Outcome {
    install_planned(
        plan,
        InstallOptions {
            yes: true,
            modify_path: true,
        },
    )
}

/// Commit a plan the user has consented to, then activate it.
fn install_planned(plan: InstallPlan, options: InstallOptions) -> Outcome {
    let interactive = !options.yes && terminal_available();
    let activation = plan.activation.clone();
    let version = plan.version.clone();
    let destination = plan.destination.clone();
    match transaction::execute(plan) {
        Ok(()) => {
            let outcome = finish_activation(activation, &version, interactive);
            // A verified interactive install has already said how to reach
            // clud; the per-shell commands are for everyone else.
            if !(interactive && outcome.is_complete()) {
                print_terminal_commands(&destination);
            }
            outcome.committed(&version, &destination)
        }
        Err(error) => {
            eprintln!("clud installer: {error}");
            Outcome::failed(1, "installation_failed").selected(&version, &destination)
        }
    }
}

fn confirm_installation(plan: &InstallPlan) -> Option<Outcome> {
    let outcome = match picker::prompt_confirm(&Facts::of(plan)) {
        Ok(ConfirmChoice::Proceed) => return None,
        Ok(ConfirmChoice::NotNow) => Outcome::refused(0, "declined"),
        Ok(ConfirmChoice::Cancelled) => Outcome::refused(130, "cancelled"),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            Outcome::refused(130, "cancelled")
        }
        Err(error) => {
            eprintln!("clud installer: {error}");
            Outcome::failed(1, "confirmation_failed")
        }
    };
    Some(outcome.selected(&plan.version, &plan.destination))
}

fn print_terminal_commands(destination: &std::path::Path) {
    match activation::terminal_commands(destination) {
        Ok(commands) => {
            eprintln!("Activate a terminal by running the command for your shell:");
            for (shell, command) in commands {
                eprintln!("{shell}: {command}");
            }
        }
        Err(error) => eprintln!("clud installer: manual activation guidance unavailable: {error}"),
    }
}

/// `interactive` offers a repair after a failed activation and reports
/// success for a person rather than a log.
fn finish_activation(
    activation: Option<ActivationPlan>,
    version: &str,
    interactive: bool,
) -> Outcome {
    let Some(activation) = activation else {
        eprintln!(
            "clud installer: installed clud {version}; activation required (--no-modify-path)"
        );
        return Outcome::activation_required();
    };
    if let Err(error) = activation.apply_and_verify(version) {
        eprintln!("clud installer: binary committed, but activation failed: {error}");
        return if interactive {
            offer_activation_repair(&activation, version)
        } else {
            Outcome::failed(1, "activation_failed")
        };
    }
    let outcome = activation_result(Ok(()), version, interactive);
    if interactive {
        eprintln!("    {}", activation.reload_hint());
    } else {
        eprintln!("{}", activation.reload_hint());
    }
    outcome
}

fn offer_activation_repair(previous: &ActivationPlan, version: &str) -> Outcome {
    let repair = match activation::plan(&previous.destination) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("clud installer: cannot safely plan activation repair: {error}");
            return Outcome::failed(1, "repair_planning_failed");
        }
    };
    eprintln!(
        "Additional activation repair plan:\n{}",
        repair.description()
    );
    execute_approved_repair(
        version,
        || picker::prompt_repair("Approve this additional activation repair?".into()),
        || repair.apply_and_verify(version),
    )
}

fn execute_approved_repair(
    version: &str,
    prompt: impl FnOnce() -> io::Result<ConfirmChoice>,
    apply: impl FnOnce() -> Result<(), String>,
) -> Outcome {
    match prompt() {
        Ok(ConfirmChoice::Proceed) => activation_result(apply(), version, true),
        Ok(ConfirmChoice::NotNow) => Outcome::refused(1, "repair_declined"),
        Ok(ConfirmChoice::Cancelled) => Outcome::refused(130, "cancelled"),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            Outcome::refused(130, "cancelled")
        }
        Err(error) => {
            eprintln!("clud installer: repair confirmation failed: {error}");
            Outcome::failed(1, "confirmation_failed")
        }
    }
}

fn activation_result(result: Result<(), String>, version: &str, interactive: bool) -> Outcome {
    match result {
        Ok(()) if interactive => {
            let style = Style::detect();
            eprintln!(
                "\n  {} clud {version} is installed and on your PATH",
                style.paint(style.check(), Tone::Good)
            );
            Outcome::complete()
        }
        Ok(()) => {
            eprintln!(
                "clud installer: installed clud {version} and verified fresh name-based lookup"
            );
            Outcome::complete()
        }
        Err(error) => {
            eprintln!("clud installer: binary committed, but activation repair failed: {error}");
            Outcome::failed(1, "activation_failed")
        }
    }
}

/// Act on the first screen's choice. That screen showed `plan`, so choosing
/// to install commits it without a second confirmation.
fn run_menu_choice(
    choice: MenuChoice,
    plan: Result<InstallPlan, String>,
    options: InstallOptions,
) -> Outcome {
    match choice {
        MenuChoice::Current => match plan {
            Ok(plan) => install_planned(plan, options),
            Err(error) => {
                eprintln!("clud installer: {error}");
                Outcome::failed(1, "planning_failed")
            }
        },
        MenuChoice::Releases => run_releases(options),
        MenuChoice::Browser => {
            let code = open_download_page();
            if code == 0 {
                Outcome::browser_opened()
            } else {
                Outcome::failed(code, "browser_open_failed")
            }
        }
        MenuChoice::NotNow => Outcome::refused(0, "declined"),
        MenuChoice::Cancelled => Outcome::refused(130, "cancelled"),
    }
}

fn run_releases(options: InstallOptions) -> Outcome {
    let catalog = match fetch_catalog() {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("clud installer: {error}");
            return Outcome::failed(1, "catalog_unavailable");
        }
    };
    let host = match host() {
        Ok(host) => host,
        Err(error) => {
            eprintln!("clud installer: {error}");
            return Outcome::failed(1, "unsupported_host");
        }
    };
    let releases = catalog.compatible_releases(host);
    if releases.is_empty() {
        eprintln!("clud installer: no complete release is compatible with this host");
        return Outcome::failed(1, "no_compatible_release");
    }
    match picker::prompt_releases(releases, catalog.latest_stable_version()) {
        Ok(ReleaseChoice::Selected(selected)) => {
            run_selected(InstallIntent::Published(selected.asset), options)
        }
        Ok(ReleaseChoice::Cancelled) => Outcome::refused(130, "cancelled"),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {
            Outcome::refused(130, "cancelled")
        }
        Err(error) => {
            eprintln!("clud installer: {error}");
            Outcome::failed(1, "release_picker_failed")
        }
    }
}

// Candidate-only injection exercises the real menu and URL/failure reporting
// without opening a browser on the acceptance runner. Release binaries do not
// recognize the variable or replace their OS browser opener.
fn open_download_page() -> i32 {
    #[cfg(feature = "installer-ci-fixture")]
    if let Some(log) = std::env::var_os("CLUD_INSTALLER_CI_BROWSER_LOG") {
        return open_page_with(|url| std::fs::write(log, url).map_err(|error| error.to_string()));
    }
    open_page_with(|url| open::that_detached(url).map_err(|error| error.to_string()))
}

fn open_page_with(open: impl FnOnce(&str) -> Result<(), String>) -> i32 {
    println!("{INSTALL_PAGE}");
    match open(INSTALL_PAGE) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("clud installer: could not open browser: {error}; use {INSTALL_PAGE}");
            1
        }
    }
}

fn terminal_available() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn fetch_catalog() -> Result<Catalog, String> {
    #[cfg(feature = "installer-ci-fixture")]
    if let Some(directory) = std::env::var_os("CLUD_INSTALLER_CI_FIXTURE_DIR") {
        let path = std::path::PathBuf::from(directory).join("catalog.json");
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("candidate catalog {}: {error}", path.display()))?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Err("candidate catalog exceeds size limit".into());
        }
        return Catalog::parse(&bytes);
    }
    if let Some(tag) = std::env::var_os("CLUD_INSTALLER_CANDIDATE_TAG") {
        let tag = tag
            .into_string()
            .map_err(|_| "candidate tag is not valid UTF-8")?;
        let (url, version) = candidate_catalog_url(&tag)?;
        let response = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .redirects(3)
            .build()
            .get(&url)
            .call()
            .map_err(|error| format!("candidate catalog fetch failed: {error}"))?;
        if !response.get_url().starts_with("https://") {
            return Err("candidate catalog response is not HTTPS".into());
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(MAX_CATALOG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("candidate catalog read failed: {error}"))?;
        return Catalog::parse_candidate(&bytes, version);
    }
    fetch_published_catalog(crate::published_urls::INSTALL_CATALOG)
}

/// The first catalog in `urls` that fetches and parses (#2449). The canonical
/// domain must answer from its own origin; a GitHub release fallback may
/// redirect to GitHub's asset host, and names its own release as `candidate`.
fn fetch_published_catalog(urls: &[&str]) -> Result<Catalog, String> {
    let mut errors = Vec::new();
    for (index, url) in urls.iter().enumerate() {
        match fetch_catalog_from(url) {
            Ok(catalog) => {
                if index > 0 {
                    eprintln!("clud installer: using fallback catalog {url}");
                }
                return Ok(catalog);
            }
            Err(error) => errors.push(format!("{url}: {error}")),
        }
    }
    Err(format!("catalog fetch failed: {}", errors.join("; ")))
}

fn fetch_catalog_from(url: &str) -> Result<Catalog, String> {
    let github = crate::published_urls::is_github_owned(url);
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(12))
        .redirects(if github { 3 } else { 0 })
        .build()
        .get(url)
        .call()
        .map_err(|error| error.to_string())?;
    let final_url = response.get_url().to_string();
    if github && !final_url.starts_with("https://") {
        return Err("response is not HTTPS".into());
    }
    if !github && final_url != url {
        return Err("response changed origin".into());
    }
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read failed: {error}"))?;
    if !github {
        return Catalog::parse(&bytes);
    }
    match release_candidate(&bytes) {
        // `releases/latest` only resolves to a published release, so its
        // candidate is a real release, not a pending one.
        Some(candidate) => Catalog::parse_candidate(&bytes, &candidate),
        None => Catalog::parse(&bytes),
    }
}

fn release_candidate(bytes: &[u8]) -> Option<String> {
    let document: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    document["channels"]["candidate"]
        .as_str()
        .map(str::to_owned)
}

fn candidate_catalog_url(tag: &str) -> Result<(String, &str), String> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err("candidate tag must be a numeric release version".into());
    }
    crate::self_install::catalog::validate_version(version)?;
    Ok((
        format!("https://github.com/zackees/clud/releases/download/{tag}/{CANDIDATE_CATALOG_NAME}"),
        version,
    ))
}

fn host() -> Result<Host, String> {
    let os = match std::env::consts::OS {
        "windows" => Os::Windows,
        "macos" => Os::Darwin,
        "linux" => Os::Linux,
        value => return Err(format!("unsupported host OS: {value}")),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::X86_64,
        "aarch64" => Arch::Aarch64,
        value => return Err(format!("unsupported host architecture: {value}")),
    };
    Ok(Host {
        os,
        arch,
        gnu: gnu_eligibility(arch),
    })
}

fn gnu_eligibility(arch: Arch) -> GnuEligibility {
    crate::platform::self_install::gnu_eligibility(arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(parts: &[&str]) -> Args {
        Args::parse_from_raw(parts.iter().map(|part| (*part).to_owned()).collect())
    }

    #[test]
    fn candidate_tag_can_only_select_a_fixed_release_asset() {
        let (url, version) = candidate_catalog_url("v2.10.0").unwrap();
        assert_eq!(version, "2.10.0");
        assert_eq!(
            url,
            "https://github.com/zackees/clud/releases/download/v2.10.0/installer-candidate-manifest.json"
        );
        for invalid in ["2.10.0/other", "2.10.0?x=1", "2.10.0-rc.1", "2.10", ""] {
            assert!(candidate_catalog_url(invalid).is_err());
        }
    }

    #[test]
    fn explicit_flags_are_claimed_and_scoped() {
        let menu = parse(&["clud", "--installer"]);
        assert!(menu.passthrough.is_empty());
        assert_eq!(explicit_request(&menu).unwrap(), Some(Request::Menu));

        let current = parse(&["clud", "--installer", "--install-current", "--yes"]);
        assert!(current.passthrough.is_empty());
        assert_eq!(explicit_request(&current).unwrap(), Some(Request::Current));

        let exact = parse(&[
            "clud",
            "--installer",
            "--install-version",
            "2.8.14",
            "--yes",
        ]);
        assert!(exact.passthrough.is_empty());
        assert_eq!(
            explicit_request(&exact).unwrap(),
            Some(Request::Exact("2.8.14".into()))
        );

        assert!(explicit_request(&parse(&["clud", "--installer", "--yes"])).is_err());
        assert!(explicit_request(&parse(&["clud", "--installer", "--", "--codex"])).is_err());
        assert!(
            explicit_request(&parse(&["clud", "--yes", "--clean-worktrees"]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn install_only_is_claimed_for_current_exact_and_menu_requests() {
        for (flags, expected) in [
            (vec!["--installer", "--no-modify-path"], Request::Menu),
            (
                vec![
                    "--installer",
                    "--install-current",
                    "--no-modify-path",
                    "--yes",
                ],
                Request::Current,
            ),
            (
                vec![
                    "--installer",
                    "--install-version",
                    "2.8.14",
                    "--no-modify-path",
                ],
                Request::Exact("2.8.14".into()),
            ),
        ] {
            let mut argv = vec!["clud"];
            argv.extend(flags);
            let parsed = Args::try_parse_from(argv).expect("install-only flags must parse");
            assert!(parsed.no_modify_path);
            assert_eq!(explicit_request(&parsed).unwrap(), Some(expected));
        }
        let split = parse(&[
            "clud",
            "--installer",
            "--install-current",
            "--no-modify-path",
        ]);
        assert!(split.no_modify_path);
        assert!(split.passthrough.is_empty());
        assert_eq!(explicit_request(&split).unwrap(), Some(Request::Current));
        assert!(Args::try_parse_from(["clud", "--no-modify-path"]).is_err());
    }

    #[test]
    fn repair_refusal_and_cancellation_never_apply_changes() {
        for (choice, status) in [(ConfirmChoice::NotNow, 1), (ConfirmChoice::Cancelled, 130)] {
            assert_eq!(
                execute_approved_repair("2.8.14", || Ok(choice), || panic!("unapproved write"))
                    .exit_code(),
                status
            );
        }
    }

    #[test]
    fn approved_repair_has_exactly_one_attempt_even_when_it_fails() {
        let attempts = std::cell::Cell::new(0);
        let status = execute_approved_repair(
            "2.8.14",
            || Ok(ConfirmChoice::Proceed),
            || {
                attempts.set(attempts.get() + 1);
                Err("profile changed again".into())
            },
        );
        assert_eq!(status.exit_code(), 1);
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn failed_repair_confirmation_never_applies_changes() {
        assert_eq!(
            execute_approved_repair(
                "2.8.14",
                || Err(io::Error::from(io::ErrorKind::Interrupted)),
                || panic!("unapproved write")
            )
            .exit_code(),
            130
        );
    }

    #[test]
    fn invalid_installer_combinations_fail_in_clap() {
        assert!(Args::try_parse_from(["clud", "--install-current"]).is_err());
        assert!(Args::try_parse_from(["clud", "--install-version", "2.8.14"]).is_err());
        assert!(Args::try_parse_from([
            "clud",
            "--installer",
            "--install-current",
            "--install-version",
            "2.8.14"
        ])
        .is_err());
    }

    #[test]
    fn structured_output_is_scoped_and_never_leaks_to_the_backend() {
        let current = parse(&[
            "clud",
            "--installer",
            "--install-current",
            "--installer-json",
        ]);
        assert!(current.installer_json);
        assert!(current.passthrough.is_empty());
        assert_eq!(explicit_request(&current).unwrap(), Some(Request::Current));
        assert!(Args::try_parse_from(["clud", "--installer-json"]).is_err());
        assert!(explicit_request(&parse(&["clud", "--installer", "--installer-json"])).is_err());
    }

    #[test]
    fn plan_json_is_scoped_read_only_and_needs_an_explicit_choice() {
        for (flags, expected) in [
            (vec!["--install-current"], Request::Current),
            (
                vec!["--install-version", "2.8.14"],
                Request::Exact("2.8.14".into()),
            ),
            (
                vec!["--install-current", "--no-modify-path"],
                Request::Current,
            ),
        ] {
            let mut argv = vec!["clud", "--installer", "--installer-plan-json"];
            argv.extend(flags);
            let parsed = parse(&argv);
            assert!(parsed.installer_plan_json);
            assert!(parsed.passthrough.is_empty());
            assert_eq!(explicit_request(&parsed).unwrap(), Some(expected));
        }
        assert!(
            explicit_request(&parse(&["clud", "--installer", "--installer-plan-json"])).is_err()
        );
        assert!(Args::try_parse_from(["clud", "--installer-plan-json"]).is_err());
        // A plan is never consent: it cannot ride along with an install.
        for extra in ["--yes", "--installer-json"] {
            assert!(Args::try_parse_from([
                "clud",
                "--installer",
                "--install-current",
                "--installer-plan-json",
                extra
            ])
            .is_err());
        }
    }

    #[test]
    fn automatic_offer_requires_plain_interactive_launch_and_missing_path_name() {
        let bare = parse(&["clud"]);
        assert!(should_offer(&bare, true, true, false));
        assert!(!should_offer(&bare, true, true, true));
        assert!(!should_offer(&bare, false, true, false));
        assert!(!should_offer(&bare, true, false, false));
        assert!(!should_offer(
            &parse(&["clud", "--dry-run"]),
            true,
            true,
            false
        ));
        assert!(!should_offer(
            &parse(&["clud", "--prompt", "hi"]),
            true,
            true,
            false
        ));
        assert!(!should_offer(
            &parse(&["clud", "models", "cheapest"]),
            true,
            true,
            false
        ));
    }

    #[test]
    fn no_tty_without_automation_exits_before_fetch_or_install() {
        assert_eq!(run_explicit(&parse(&["clud", "--installer"])), Some(2));
        assert_eq!(
            run_explicit(&parse(&["clud", "--installer", "--install-current"])),
            Some(2)
        );
        assert_eq!(
            run_explicit(&parse(&[
                "clud",
                "--installer",
                "--install-version",
                "2.8.14"
            ])),
            Some(2)
        );
    }

    #[test]
    fn browser_failure_reports_the_canonical_page() {
        let mut called = false;
        assert_eq!(
            open_page_with(|url| {
                called = true;
                assert_eq!(url, INSTALL_PAGE);
                Err("opener unavailable".into())
            }),
            1
        );
        assert!(called);
    }
}
