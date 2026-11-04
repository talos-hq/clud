//! `clud install <product>` (#2609) — the steps behind the command.
//!
//! 1. resolve the product from [`super::products`];
//! 2. fetch the product's bootstrap at a pinned ref into clud's cache;
//! 3. run it with the caller's terminal, so the product's own wizard works;
//! 4. offer to migrate clud's provider credentials into the product;
//! 5. print what to run next.
//!
//! `--dry-run` does 1 and reports 2-5 without touching the network or the disk.
//! See docs/architecture/install-products.md.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use super::products::{self, Product};

/// Exit status for a refused or impossible request (usage-level failure).
pub const EXIT_USAGE: i32 = 2;
/// Largest bootstrap clud will accept from a product repository.
const MAX_BOOTSTRAP_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct Options {
    /// The word typed after `clud install`.
    pub target: String,
    pub dry_run: bool,
    pub yes: bool,
    /// Install directory (`--dir`), passed to the bootstrap through its own
    /// environment variable.
    pub dir: Option<PathBuf>,
    /// Existing install to configure (`--target`), for `--configure-only`.
    pub product_dir: Option<PathBuf>,
    /// Repository ref the bootstrap is fetched from.
    pub reference: Option<String>,
    /// Product version, passed straight to the product's own installer.
    pub version: Option<String>,
    /// Run the configure step (false with `--no-configure`).
    pub configure: bool,
    /// Skip the bootstrap and only configure (`--configure-only`).
    pub configure_only: bool,
    /// Migrate credentials without asking; `--yes` alone never does this.
    pub migrate_credentials: bool,
    /// Arguments after `--`, forwarded to the product's installer.
    pub passthrough: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            target: String::new(),
            dry_run: false,
            yes: false,
            dir: None,
            product_dir: None,
            reference: None,
            version: None,
            configure: true,
            configure_only: false,
            migrate_credentials: false,
            passthrough: Vec::new(),
        }
    }
}

/// Run against the real home, terminal and network.
pub fn run(options: &Options) -> i32 {
    let Some(home) = crate::home::user_home() else {
        eprintln!("clud install: cannot resolve the home directory");
        return 1;
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut stdout = std::io::stdout().lock();
    let code = run_at(&home, options, interactive, &mut stdout);
    let _ = stdout.flush();
    code
}

fn run_at(home: &Path, options: &Options, interactive: bool, out: &mut dyn Write) -> i32 {
    let Some(product) = products::find(&options.target) else {
        eprintln!(
            "clud install: unknown product {:?}; known products: {}",
            options.target,
            products::names()
        );
        return EXIT_USAGE;
    };
    if options.dir.is_some() && options.product_dir.is_some() {
        eprintln!("clud install: --dir and --target are mutually exclusive");
        return EXIT_USAGE;
    }
    let branch = options
        .reference
        .clone()
        .unwrap_or_else(|| product.branch.to_string());
    let dir = options
        .dir
        .clone()
        .or_else(|| options.product_dir.clone())
        .unwrap_or_else(|| home.join(product.default_dir));
    let _ = writeln!(
        out,
        "install: product {} ({})",
        product.target, product.display
    );
    let _ = writeln!(
        out,
        "install: bootstrap {}",
        product.bootstrap_label(&branch)
    );
    let _ = writeln!(
        out,
        "install: directory {} (via {})",
        dir.display(),
        product.dir_env
    );

    if options.dry_run {
        let _ = writeln!(
            out,
            "install: dry run; nothing fetched, run or written; reconfigure later with {}",
            product.setup_hint()
        );
        return 0;
    }

    if !options.configure_only {
        if !options.yes && !interactive {
            eprintln!("clud install: a terminal or --yes is required before installing");
            return EXIT_USAGE;
        }
        let script = match fetch_bootstrap(home, product, &branch) {
            Ok(script) => script,
            Err(error) => {
                eprintln!("clud install: {error}");
                return 1;
            }
        };
        let _ = writeln!(out, "install: bootstrap cached at {}", script.display());
        if let Err(failure) = run_bootstrap(&script, product, &dir, options) {
            eprintln!(
                "clud install: the {} installer failed ({})",
                product.display, failure.summary
            );
            return 1;
        }
        let _ = writeln!(
            out,
            "install: {} installed in {}",
            product.display,
            dir.display()
        );
    }

    if !options.configure {
        let _ = writeln!(
            out,
            "install: done; reconfigure later with {}",
            product.setup_hint()
        );
        return 0;
    }
    configure(home, product, options, out)
}

/// The credential-migration step (#2609). A secret goes from the vault straight
/// into the product's config; only the file and the providers are named.
fn configure(home: &Path, product: &Product, options: &Options, out: &mut dyn Write) -> i32 {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let product_home = std::env::var_os(product.home_env)
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(product.default_home));
    let code = super::configure::run(
        product,
        &product_home,
        options.migrate_credentials,
        options.yes,
        interactive,
        out,
    );
    if code == 0 {
        let _ = writeln!(
            out,
            "install: reconfigure later with {}",
            product.setup_hint()
        );
    }
    code
}

#[derive(Debug)]
struct Failure {
    summary: String,
}

/// Download the product's bootstrap into `~/.clud/cache/install/<target>/`.
fn fetch_bootstrap(home: &Path, product: &Product, branch: &str) -> Result<PathBuf, String> {
    let url = product.bootstrap_url(branch);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(30))
        .build();
    let response = agent
        .get(&url)
        .call()
        .map_err(|error| format!("cannot fetch {url}: {error}"))?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_BOOTSTRAP_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| format!("cannot read {url}: {error}"))?;
    if body.is_empty() {
        return Err(format!("{url} returned an empty script"));
    }
    if body.len() > MAX_BOOTSTRAP_BYTES {
        return Err(format!(
            "{url} is larger than the {MAX_BOOTSTRAP_BYTES}-byte bootstrap cap"
        ));
    }
    if !body.starts_with(b"#!") {
        return Err(format!("{url} is not a shell script (no shebang)"));
    }
    let directory = home
        .join(".clud")
        .join("cache")
        .join("install")
        .join(product.target);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let path = directory.join("bootstrap.sh");
    std::fs::write(&path, &body)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(path)
}

/// Run the bootstrap with the caller's terminal inherited, so the product's own
/// wizard works, and with its directory variable set.
///
/// `running-process` uses a cleared environment whenever `env` is `Some`, so the
/// parent's variables are carried over explicitly rather than silently dropped.
fn run_bootstrap(
    script: &Path,
    product: &Product,
    dir: &Path,
    options: &Options,
) -> Result<(), Failure> {
    use running_process::{CommandSpec, NativeProcess, ProcessConfig, StderrMode, StdinMode};

    // The host boundary decides how a script is executed: on Unix the script
    // runs through its own shebang (hermes-slim's is `#!/bin/sh`, talos' is
    // `#!/usr/bin/env bash`), on Windows it runs under bash.
    let mut argv = crate::platform::script_exec::script_argv(script).map_err(|error| Failure {
        summary: format!("cannot prepare {}: {error}", script.display()),
    })?;
    if let Some(version) = &options.version {
        argv.push("--version".to_string());
        argv.push(version.clone());
    }
    // Appended as-is, not behind a `--`: hermes-slim's bootstrap treats a bare
    // `--` as "hand the rest to hermes-slim-setup", while talos' rejects any
    // argument it does not know, so a separator is one product's syntax.
    argv.extend(options.passthrough.iter().cloned());

    let dir_value = dir.to_string_lossy().into_owned();
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| key != product.dir_env)
        .collect();
    env.push((product.dir_env.to_string(), dir_value));

    let process = NativeProcess::new(ProcessConfig {
        command: CommandSpec::Argv(argv),
        // No cwd: the bootstrap only reads its directory from the environment,
        // and the target may not exist yet.
        cwd: None,
        env: Some(env),
        capture: false,
        stderr_mode: StderrMode::Stdout,
        creationflags: None,
        create_process_group: false,
        stdin_mode: StdinMode::Inherit,
        nice: None,
        address_space_limit_bytes: None,
    });
    process.start().map_err(|error| Failure {
        summary: format!("cannot start {}: {error}", script.display()),
    })?;
    let code = process.wait(None).map_err(|error| Failure {
        summary: format!("cannot wait for {}: {error}", script.display()),
    })?;
    if code == 0 {
        Ok(())
    } else {
        Err(Failure {
            summary: format!("exit {code}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(target: &str) -> Options {
        Options {
            target: target.to_string(),
            ..Options::default()
        }
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "clud-install-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("tempdir");
        base
    }

    #[test]
    fn an_unknown_product_is_refused_with_the_known_ones() {
        let home = tempdir();
        let mut out = Vec::new();
        let code = run_at(&home, &options("nope"), false, &mut out);
        assert_eq!(code, EXIT_USAGE);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_dry_run_reports_the_plan_and_writes_nothing() {
        let home = tempdir();
        let mut out = Vec::new();
        let mut opts = options("hermes");
        opts.dry_run = true;
        let code = run_at(&home, &opts, false, &mut out);
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("talos-hq/hermes-slim@main/slim/install.sh"),
            "{text}"
        );
        assert!(
            text.contains(home.join(".hermes-slim").to_str().expect("utf8")),
            "{text}"
        );
        assert!(!home.join(".clud/cache/install/hermes").exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn installing_off_a_terminal_without_yes_is_refused_before_any_fetch() {
        let home = tempdir();
        let mut out = Vec::new();
        let code = run_at(&home, &options("talos"), false, &mut out);
        assert_eq!(code, EXIT_USAGE);
        assert!(!home.join(".clud/cache/install/talos").exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_bootstrap_runs_through_its_own_shebang_with_the_dir_and_extra_args() {
        let home = tempdir();
        let script = home.join("bootstrap.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s|%s' \"$HERMES_SLIM_DIR\" \"$1\" > \"$2\"\n",
        )
        .expect("seed");
        let product = products::find("hermes").expect("hermes");
        let dir = home.join("install-here");
        let seen = home.join("seen.txt");
        let options = Options {
            passthrough: vec!["--include-desktop".to_string(), seen.display().to_string()],
            ..Options::default()
        };
        run_bootstrap(&script, product, &dir, &options).expect("runs");
        assert_eq!(
            std::fs::read_to_string(&seen).expect("read"),
            format!("{}|--include-desktop", dir.display())
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_failing_bootstrap_reports_its_exit_status() {
        let home = tempdir();
        let script = home.join("bootstrap.sh");
        std::fs::write(&script, "#!/bin/sh\necho boom >&2\nexit 3\n").expect("seed");
        let product = products::find("talos").expect("talos");
        let options = Options::default();
        let failure =
            run_bootstrap(&script, product, &home.join("d"), &options).expect_err("fails");
        assert_eq!(failure.summary, "exit 3");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn dir_and_target_are_mutually_exclusive() {
        let home = tempdir();
        let mut out = Vec::new();
        let mut opts = options("hermes");
        opts.dir = Some(PathBuf::from("/tmp/a"));
        opts.product_dir = Some(PathBuf::from("/tmp/b"));
        let code = run_at(&home, &opts, true, &mut out);
        assert_eq!(code, EXIT_USAGE);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_configure_only_run_installs_nothing_and_names_the_reconfigure_hint() {
        let home = tempdir();
        let mut out = Vec::new();
        let mut opts = options("hermes");
        opts.configure_only = true;
        let code = run_at(&home, &opts, false, &mut out);
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("hermes setup clud"), "{text}");
        assert!(!home.join(".clud").exists());
        let _ = std::fs::remove_dir_all(home);
    }
}
