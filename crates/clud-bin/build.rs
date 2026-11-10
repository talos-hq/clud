fn main() -> Result<(), Box<dyn std::error::Error>> {
    bundle_lokr()?;
    println!("cargo:rerun-if-changed=../../proto/clud_v1.proto");
    println!("cargo:rerun-if-changed=build.rs");

    // main.rs deliberately keeps `run()` (a large, monolithic function) on
    // the true process main thread rather than a spawned thread, because it
    // owns console/OLE APIs (drag-and-drop COM registration, console title)
    // that must run there. Only argument parsing is moved to a spawned
    // thread with a bigger stack. That leaves `run()` exposed to Windows'
    // small default main-thread stack (1 MiB): an unoptimized dev/test
    // build's stack frame for a function this size, with this many local
    // command-dispatch branches, has already overflowed it once (#869) and
    // did again after the DeepSeek command surface grew further (#878).
    // Raise the linked-in stack reservation instead of chasing individual
    // overflow sites; this is link-time (works under both link.exe and
    // lld-link for cross-compiled CI builds) and does not change which
    // thread `run()` executes on.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg=/STACK:8388608");
    }

    // Issue #1016 item 3: give ELF builds a GNU build-id.
    //
    // Debug info now ships *beside* the release as a per-triple `.dwp`, and a
    // sidecar has to be provably the one for this binary. Version plus triple
    // identifies a release, not a build: a rebuilt or patched binary at the
    // same version would pair with the wrong DWARF and mis-symbolicate, which
    // is worse than not symbolicating at all because the wrong line numbers
    // look authoritative.
    //
    // The format already has the field for exactly this, and clud was not
    // emitting it -- `readelf -n` on a built binary showed only
    // `.note.ABI-tag`. The linker default is no build-id unless a distro
    // patches it in, so it has to be asked for.
    //
    // Gated to ELF: this is a GNU-ld/lld flag. MSVC's link.exe would reject
    // it, and Mach-O carries LC_UUID instead, which the Apple linker emits
    // without being asked.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg=-Wl,--build-id=sha1");
    }
    // Issue #1016: bake the target triple into the binary.
    //
    // Cargo sets `TARGET` for build scripts but not for the crate itself, so
    // `env!("TARGET")` does not compile in a bin. Without this a running clud
    // cannot say which build it is, which is what picks the right sidecar:
    // debug info now lives *beside* the release rather than inside the binary,
    // and the `.dwp` / `.pdb` / `.dSYM` is per-triple. The wheel a user
    // installed does not otherwise record which one they got.
    //
    // This is the build-script `TARGET`, so a cross build stamps the triple it
    // produced rather than the host's -- which is the whole point, since the
    // cross-built artifacts are the ones shipped.
    println!(
        "cargo:rustc-env=CLUD_TARGET={}",
        std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string())
    );

    let file_descriptors = protox::compile(["../../proto/clud_v1.proto"], ["../../proto/"])?;
    let mut config = prost_build::Config::new();
    config.enum_attribute(
        ".clud.v1.WorkerServerEnvelope.message",
        "#[allow(clippy::large_enum_variant)]",
    );
    config.compile_fds(file_descriptors)?;
    Ok(())
}

/// Release preparation supplies verified native bytes; development builds stay network-free.
fn bundle_lokr() -> Result<(), Box<dyn std::error::Error>> {
    bundle_image("", "lokr.bin", true)?;
    bundle_image("FALLBACK_", "lokr-fallback.bin", false)
}

fn bundle_image(
    prefix: &str,
    filename: &str,
    required: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let binary_key = format!("CLUD_LOKR_{prefix}BINARY");
    let version_key = format!("CLUD_LOKR_{prefix}VERSION");
    let digest_key = format!("CLUD_LOKR_{prefix}SHA256");
    for key in [&binary_key, &version_key, &digest_key] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    let destination = std::path::PathBuf::from(std::env::var("OUT_DIR")?).join(filename);
    let (version, digest) = if let Some(path) = std::env::var_os(&binary_key) {
        let version = std::env::var(&version_key)?;
        let digest = std::env::var(&digest_key)?;
        if version.starts_with("0.0.")
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("functional, checksum-pinned lokr release is required".into());
        }
        println!(
            "cargo:rerun-if-changed={}",
            std::path::Path::new(&path).display()
        );
        std::fs::copy(path, &destination)?;
        (version, digest)
    } else {
        if required && std::env::var("PROFILE").as_deref() == Ok("release") {
            return Err("release builds require pinned lokr bytes; build through ci.xbuild".into());
        }
        std::fs::write(&destination, [])?;
        (String::new(), String::new())
    };
    println!(
        "cargo:rustc-env=CLUD_BUNDLED_LOKR_{prefix}PATH={}",
        destination.display()
    );
    println!("cargo:rustc-env=CLUD_BUNDLED_LOKR_{prefix}VERSION={version}");
    println!("cargo:rustc-env=CLUD_BUNDLED_LOKR_{prefix}SHA256={digest}");
    Ok(())
}
