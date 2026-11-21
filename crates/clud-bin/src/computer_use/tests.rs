use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use super::pin::{self, ArchiveKind, Artifact};
use super::provision::{self, HermesInstall, Inputs, Resolution, Source};
use super::session_env::{self, DisplayKind, PermissionMode, Probes};
use super::*;

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

/// A gzip'd tar holding `entries` (path, bytes) and its pinned identity.
fn fake_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (path, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, *bytes).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn artifact_for(bytes: &[u8], kind: ArchiveKind, binary: &'static str) -> Artifact {
    Artifact {
        target: "linux-x64",
        asset: "fake.tar.gz",
        sha256: leak(format!("{:x}", Sha256::digest(bytes))),
        size: bytes.len() as u64,
        kind,
        binary,
    }
}

fn inputs<'a>(home: &'a Path, artifact: &'a Artifact) -> Inputs<'a> {
    Inputs {
        home,
        override_path: None,
        hermes_home: None,
        artifact,
    }
}

// ---- pin ----------------------------------------------------------------

#[test]
fn every_pinned_artifact_is_a_well_formed_release_asset() {
    for artifact in pin::ARTIFACTS {
        assert_eq!(artifact.sha256.len(), 64, "{}", artifact.target);
        assert!(artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(artifact.asset.contains(pin::VERSION), "{}", artifact.asset);
        assert!(artifact.size > 1_000_000, "{}", artifact.target);
        provision::validate_location(&artifact.url(), true).unwrap();
    }
}

#[test]
fn target_labels_cover_every_published_host() {
    let label = pin::target_label;
    assert_eq!(label(false, false, true, false), Some("linux-x64"));
    assert_eq!(label(false, false, true, true), Some("linux-arm64"));
    assert_eq!(label(false, true, false, true), Some("darwin-arm64"));
    assert_eq!(label(true, false, false, false), Some("win32-x64"));
    assert_eq!(label(false, false, false, false), None);
    // Every CI host is a published target.
    assert!(pin::for_host().is_some());
}

#[test]
fn macos_uses_the_signed_app_bundle_for_tcc() {
    for target in ["darwin-x64", "darwin-arm64"] {
        let artifact = pin::ARTIFACTS.iter().find(|a| a.target == target).unwrap();
        assert!(artifact.binary.starts_with("CuaDriver.app/"), "{target}");
        assert!(!artifact.asset.contains("-binary"), "{target}");
    }
}

#[test]
fn downloads_stay_on_the_pinned_release_and_its_cdn() {
    let ok = format!(
        "https://github.com/trycua/cua/releases/download/cua-driver-rs-v{}/x.tar.gz",
        pin::VERSION
    );
    provision::validate_location(&ok, true).unwrap();
    for bad in [
        ok.replace("https://", "http://"),
        ok.replace("trycua/cua", "evil/cua"),
        ok.replace(pin::VERSION, "0.0.1"),
        "https://example.invalid/x.tar.gz".to_string(),
        format!("{ok}?token=1"),
    ] {
        assert!(provision::validate_location(&bad, true).is_err(), "{bad}");
    }
    let cdn = "https://release-assets.githubusercontent.com/asset?sig=1";
    assert!(provision::validate_location(cdn, true).is_err());
    provision::validate_location(cdn, false).unwrap();
}

// ---- provisioning decision -----------------------------------------------

#[test]
fn missing_driver_installs_verified_and_is_reused_offline() {
    let home = tempfile::tempdir().unwrap();
    let archive = fake_tar(&[("cua-driver", b"#!/bin/sh\n")]);
    let artifact = artifact_for(&archive, ArchiveKind::TarGz, "cua-driver");
    let inputs = inputs(home.path(), &artifact);
    assert!(matches!(
        provision::resolve(&inputs),
        Resolution::Missing {
            hermes: HermesInstall::Absent
        }
    ));
    let driver = provision::ensure_with_fetch(&inputs, |_| Ok(archive.clone())).unwrap();
    assert_eq!(driver.source, Source::Managed);
    assert!(driver.path.starts_with(inputs.managed_dir()));
    assert_eq!(std::fs::read(&driver.path).unwrap(), b"#!/bin/sh\n");
    let again = provision::ensure_with_fetch(&inputs, |_| panic!("installed driver stays offline"));
    assert_eq!(again.unwrap(), driver);
}

#[test]
fn a_tampered_managed_driver_is_not_trusted_and_is_replaced() {
    let home = tempfile::tempdir().unwrap();
    let archive = fake_tar(&[("cua-driver", b"good")]);
    let artifact = artifact_for(&archive, ArchiveKind::TarGz, "cua-driver");
    let inputs = inputs(home.path(), &artifact);
    let driver = provision::ensure_with_fetch(&inputs, |_| Ok(archive.clone())).unwrap();
    std::fs::write(&driver.path, b"evil").unwrap();
    assert!(matches!(
        provision::resolve(&inputs),
        Resolution::Missing { .. }
    ));
    let fixed = provision::ensure_with_fetch(&inputs, |_| Ok(archive.clone())).unwrap();
    assert_eq!(std::fs::read(fixed.path).unwrap(), b"good");
}

#[test]
fn wrong_size_or_digest_never_unpacks() {
    let home = tempfile::tempdir().unwrap();
    let archive = fake_tar(&[("cua-driver", b"good")]);
    let artifact = artifact_for(&archive, ArchiveKind::TarGz, "cua-driver");
    let inputs = inputs(home.path(), &artifact);
    let mut flipped = archive.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 0xff;
    for payload in [vec![0; archive.len() + 1], flipped] {
        assert!(provision::ensure_with_fetch(&inputs, |_| Ok(payload)).is_err());
        assert!(!inputs.managed_dir().exists());
    }
}

#[test]
fn a_wrapped_archive_is_flattened_to_its_single_top_directory() {
    let home = tempfile::tempdir().unwrap();
    let archive = fake_tar(&[
        ("pkg/CuaDriver.app/Contents/MacOS/cua-driver", b"app"),
        ("pkg/LICENSE", b"mit"),
    ]);
    let artifact = artifact_for(
        &archive,
        ArchiveKind::TarGz,
        "CuaDriver.app/Contents/MacOS/cua-driver",
    );
    let inputs = inputs(home.path(), &artifact);
    let driver = provision::ensure_with_fetch(&inputs, |_| Ok(archive.clone())).unwrap();
    assert_eq!(
        driver.path,
        inputs
            .managed_dir()
            .join("CuaDriver.app/Contents/MacOS/cua-driver")
    );
    assert!(inputs.managed_dir().join("LICENSE").is_file());
}

#[test]
fn zip_archives_unpack_like_the_windows_release() {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("cua-driver.exe", options).unwrap();
        std::io::Write::write_all(&mut writer, b"MZ").unwrap();
        writer.finish().unwrap();
    }
    let archive = cursor.into_inner();
    let home = tempfile::tempdir().unwrap();
    let artifact = artifact_for(&archive, ArchiveKind::Zip, "cua-driver.exe");
    let inputs = inputs(home.path(), &artifact);
    let driver = provision::ensure_with_fetch(&inputs, |_| Ok(archive.clone())).unwrap();
    assert_eq!(std::fs::read(driver.path).unwrap(), b"MZ");
}

#[test]
fn an_override_wins_and_a_broken_one_is_reported_not_replaced() {
    let home = tempfile::tempdir().unwrap();
    let archive = fake_tar(&[("cua-driver", b"x")]);
    let artifact = artifact_for(&archive, ArchiveKind::TarGz, "cua-driver");
    let custom = home.path().join("my-driver");
    std::fs::write(&custom, b"custom").unwrap();
    let mut inputs = inputs(home.path(), &artifact);
    inputs.override_path = Some(custom.clone());
    let driver = provision::ensure_with_fetch(&inputs, |_| panic!("no download")).unwrap();
    assert_eq!((driver.path, driver.source), (custom, Source::Override));

    inputs.override_path = Some(home.path().join("absent"));
    assert!(matches!(
        provision::resolve(&inputs),
        Resolution::BadOverride(_)
    ));
    let error = provision::ensure_with_fetch(&inputs, |_| panic!("no download")).unwrap_err();
    assert!(error.contains(provision::OVERRIDE_ENV), "{error}");
}

fn write_hermes(root: &Path, version: &str, digest: &str, binary: &str) -> PathBuf {
    let entry = format!("cua-driver-{version}-linux-x64");
    let tools = root.join("tools");
    let path = tools.join(&entry).join(binary);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"hermes").unwrap();
    let facts = serde_json::json!({"packages": {"cua-driver": {
        "artifacts": [digest], "entry": entry, "target": "linux-x64", "version": version,
    }}});
    std::fs::write(tools.join("facts.json"), facts.to_string()).unwrap();
    path
}

#[test]
fn a_hermes_install_of_the_exact_pin_is_reused() {
    let home = tempfile::tempdir().unwrap();
    let hermes = tempfile::tempdir().unwrap();
    let artifact = *pin::ARTIFACTS.first().unwrap();
    let path = write_hermes(
        hermes.path(),
        pin::VERSION,
        artifact.sha256,
        artifact.binary,
    );
    let mut inputs = inputs(home.path(), &artifact);
    inputs.hermes_home = Some(hermes.path().to_path_buf());
    let driver = provision::ensure_with_fetch(&inputs, |_| panic!("reuse, no download")).unwrap();
    assert_eq!((driver.path, driver.source), (path, Source::Hermes));
}

#[test]
fn a_hermes_install_of_another_version_or_archive_is_not_reused() {
    let home = tempfile::tempdir().unwrap();
    let artifact = *pin::ARTIFACTS.first().unwrap();
    let zeros = "0".repeat(64);
    for (version, digest) in [("0.21.0", artifact.sha256), (pin::VERSION, zeros.as_str())] {
        let hermes = tempfile::tempdir().unwrap();
        write_hermes(hermes.path(), version, digest, artifact.binary);
        let mut inputs = inputs(home.path(), &artifact);
        inputs.hermes_home = Some(hermes.path().to_path_buf());
        assert_eq!(
            provision::resolve(&inputs),
            Resolution::Missing {
                hermes: HermesInstall::Mismatch {
                    version: version.to_string()
                }
            }
        );
    }
}

// ---- environment policy ----------------------------------------------------

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, v)| v.as_str())
}

fn never_exists(_: &Path) -> bool {
    false
}

fn empty_dir(_: &Path) -> Vec<(PathBuf, SystemTime)> {
    Vec::new()
}

fn no_files() -> Probes<'static> {
    Probes {
        exists: &never_exists,
        list_runtime_dir: &empty_dir,
    }
}

#[test]
fn yolo_runs_the_driver_unrestricted_and_safe_keeps_its_standard_profile() {
    assert_eq!(
        PermissionMode::for_launch(false),
        PermissionMode::Unrestricted
    );
    assert_eq!(PermissionMode::for_launch(true), PermissionMode::Standard);
    let yolo = session_env::child_env(Vec::new(), PermissionMode::Unrestricted, false, &no_files());
    assert_eq!(
        value(&yolo, "CUA_DRIVER_PERMISSION_MODE"),
        Some("unrestricted")
    );
    assert_eq!(
        value(&yolo, "CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS"),
        Some("1")
    );
    // An inherited unrestricted setting cannot widen a --safe launch.
    let inherited = env(&[
        ("CUA_DRIVER_PERMISSION_MODE", "unrestricted"),
        ("CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS", "1"),
    ]);
    let safe = session_env::child_env(inherited, PermissionMode::Standard, false, &no_files());
    assert_eq!(value(&safe, "CUA_DRIVER_PERMISSION_MODE"), None);
    assert_eq!(
        value(&safe, "CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS"),
        None
    );
}

#[test]
fn a_garbled_mode_only_narrows() {
    for raw in [None, Some(""), Some("Unrestricted"), Some("bounded")] {
        assert_eq!(
            PermissionMode::from_env_value(raw),
            PermissionMode::Standard
        );
    }
    assert_eq!(
        PermissionMode::from_env_value(Some("unrestricted")),
        PermissionMode::Unrestricted
    );
}

#[test]
fn the_driver_never_inherits_credentials_and_stays_quiet() {
    let parent = env(&[
        ("ANTHROPIC_API_KEY", "k"),
        ("GH_TOKEN", "t"),
        ("AWS_SECRET_ACCESS_KEY", "s"),
        ("PATH", "/bin"),
        ("CUA_DRIVER_RS_UPDATE_CHECK", "1"),
    ]);
    let child = session_env::child_env(parent, PermissionMode::Standard, false, &no_files());
    for name in ["ANTHROPIC_API_KEY", "GH_TOKEN", "AWS_SECRET_ACCESS_KEY"] {
        assert_eq!(value(&child, name), None, "{name}");
    }
    assert_eq!(value(&child, "PATH"), Some("/bin"));
    assert_eq!(value(&child, "CUA_DRIVER_RS_TELEMETRY_ENABLED"), Some("0"));
    assert_eq!(value(&child, "CUA_DRIVER_RS_UPDATE_CHECK"), Some("0"));
    let opted_in = session_env::child_env(
        env(&[("CUA_DRIVER_RS_TELEMETRY_ENABLED", "1")]),
        PermissionMode::Standard,
        false,
        &no_files(),
    );
    assert_eq!(
        value(&opted_in, "CUA_DRIVER_RS_TELEMETRY_ENABLED"),
        Some("1")
    );
}

#[test]
fn display_kinds_are_detected_from_the_session() {
    let kind = |pairs: &[(&str, &str)]| session_env::display_kind(&env(pairs), true);
    assert_eq!(kind(&[("DISPLAY", ":0")]), DisplayKind::X11);
    assert_eq!(
        kind(&[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]),
        DisplayKind::Wayland { xwayland: true }
    );
    assert_eq!(
        kind(&[("XDG_SESSION_TYPE", "wayland")]),
        DisplayKind::Wayland { xwayland: false }
    );
    assert_eq!(kind(&[]), DisplayKind::Headless);
    assert_eq!(
        session_env::display_kind(&env(&[("DISPLAY", ":0")]), false),
        DisplayKind::Native
    );
}

#[test]
fn wayland_turns_on_the_native_backend_unless_opted_out() {
    let wayland = [("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")];
    let on = session_env::child_env(env(&wayland), PermissionMode::Standard, true, &no_files());
    assert_eq!(value(&on, "CUA_DRIVER_RS_ENABLE_WAYLAND"), Some("1"));
    let mut opted_out = env(&wayland);
    opted_out.push((session_env::NATIVE_WAYLAND_ENV.into(), "0".into()));
    let off = session_env::child_env(opted_out, PermissionMode::Standard, true, &no_files());
    assert_eq!(value(&off, "CUA_DRIVER_RS_ENABLE_WAYLAND"), None);
    let x11 = session_env::child_env(
        env(&[("DISPLAY", ":0")]),
        PermissionMode::Standard,
        true,
        &no_files(),
    );
    assert_eq!(value(&x11, "CUA_DRIVER_RS_ENABLE_WAYLAND"), None);
    // Off Linux the display policy never applies.
    let mac = session_env::child_env(env(&wayland), PermissionMode::Standard, false, &no_files());
    assert_eq!(value(&mac, "CUA_DRIVER_RS_ENABLE_WAYLAND"), None);
}

#[test]
fn a_stale_xauthority_is_replaced_by_the_newest_live_cookie() {
    // The host this was found on: a terminal kept the previous login's
    // XAUTHORITY, so every X11 call failed and list_windows returned [].
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    let new = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
    let listing = move |_: &Path| {
        vec![
            (PathBuf::from("/run/user/1000/xauth_OLDER"), old),
            (PathBuf::from("/run/user/1000/xauth_NEWER"), new),
            (PathBuf::from("/run/user/1000/bus"), new),
        ]
    };
    let probes = Probes {
        exists: &|path: &Path| path.to_string_lossy().contains("xauth_N"),
        list_runtime_dir: &listing,
    };
    let stale = env(&[
        ("DISPLAY", ":0"),
        ("XAUTHORITY", "/run/user/1000/xauth_GONE"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ]);
    let child = session_env::child_env(stale, PermissionMode::Standard, true, &probes);
    assert_eq!(
        value(&child, "XAUTHORITY"),
        Some("/run/user/1000/xauth_NEWER")
    );

    let live = env(&[
        ("DISPLAY", ":0"),
        ("XAUTHORITY", "/run/user/1000/xauth_NEWER"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
    ]);
    assert_eq!(
        session_env::repaired_xauthority(&live, probes.exists, probes.list_runtime_dir),
        None
    );
}

// ---- registration ----------------------------------------------------------

#[test]
fn the_claude_server_is_one_token_deferred_and_carries_the_mode() {
    let arg = claude_mcp_config_arg(Path::new("/opt/clud"), PermissionMode::Unrestricted);
    let json = arg.strip_prefix("--mcp-config=").unwrap();
    let config: serde_json::Value = serde_json::from_str(json).unwrap();
    let server = &config["mcpServers"][SERVER_NAME];
    assert_eq!(server["command"], "/opt/clud");
    assert_eq!(server["args"], serde_json::json!(["computer-use", "mcp"]));
    assert_eq!(server["env"][session_env::MODE_ENV], "unrestricted");
    assert_eq!(server["alwaysLoad"], false);
    assert_eq!(claude_allowed_tools_arg(), "--allowedTools=mcp__cua");
}

#[test]
fn codex_overrides_are_valid_toml_and_preapprove_tools() {
    let overrides =
        codex_config_overrides(Path::new("C:\\clud\\clud.exe"), PermissionMode::Standard);
    let mut doc = String::new();
    for line in &overrides {
        let (key, value) = line.split_once('=').unwrap();
        let key = key.trim_start_matches("mcp_servers.cua.");
        doc.push_str(&format!("{key} = {value}\n"));
    }
    let parsed: toml::Value = toml::from_str(&doc).unwrap();
    assert_eq!(parsed["command"].as_str(), Some("C:\\clud\\clud.exe"));
    assert_eq!(
        parsed["env"][session_env::MODE_ENV].as_str(),
        Some("standard")
    );
    assert_eq!(
        parsed["default_tools_approval_mode"].as_str(),
        Some("approve")
    );
}

#[test]
fn first_party_claude_gets_it_by_default_others_on_request() {
    assert!(registered_for(true, false, false));
    assert!(!registered_for(false, false, false));
    assert!(registered_for(false, true, false));
    assert!(!registered_for(true, true, true));
}
