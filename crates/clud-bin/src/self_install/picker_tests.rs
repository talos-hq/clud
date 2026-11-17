use std::path::PathBuf;

use unicode_width::UnicodeWidthStr;

use super::*;
use crate::self_install::activation::PathChange;
use crate::self_install::catalog::{Arch, Flavor, MediaType, Os, ResolvedAsset};

const COLOR: Style = Style {
    color: true,
    unicode: true,
};

fn row(version: &str) -> ResolvedRelease {
    ResolvedRelease {
        version: version.into(),
        asset: ResolvedAsset {
            version: version.into(),
            filename: format!("clud-{version}-x86_64-unknown-linux-musl"),
            media_type: MediaType::Direct,
            size_bytes: 1,
            sha256: "0".repeat(64),
            url: String::new(),
            flavor: Flavor::StaticMusl,
            os: Os::Linux,
            arch: Arch::X86_64,
        },
    }
}

/// The plan from the Windows 10 report that prompted the redesign.
fn windows_facts() -> Facts {
    let bin = r"C:\Users\vpkmc\AppData\Local\Programs\clud\bin";
    Facts {
        version: "2.8.65".into(),
        destination: PathBuf::from(format!(r"{bin}\clud.exe")),
        existing_version: None,
        replaces: false,
        path: Some(PathChange::Add {
            directory: bin.into(),
            files: Vec::new(),
        }),
        source: r"C:\Users\vpkmc\Downloads\clud-setup.exe".into(),
        sha256: "b8d6b6b8d163648b1928548c2b719174001f7910e0ebbf699e7add3cda45842a".into(),
        path_edit: format!(
            r#"write HKCU\Environment\Path (REG_EXPAND_SZ) from "C:\Users\vpkmc\AppData\Local\hermes\bin;%USERPROFILE%\AppData\Local\Microsoft\WindowsApps" to "{bin};C:\Users\vpkmc\AppData\Local\hermes\bin;%USERPROFILE%\AppData\Local\Microsoft\WindowsApps"; prepend {bin}; broadcast Environment change; verify OS-built User environment"#
        ),
        prompt_engine: r"pinned lokr 0.1.0 at C:\Users\vpkmc\.clud\libexec\lokr\0.1.0-edcf91f8be4151d782e7fcb3899c4843630b052f37c631b21408eada9ef0077f\lokr.exe".into(),
    }
}

fn unix_facts() -> Facts {
    Facts {
        version: "2.8.65".into(),
        destination: "/home/me/.local/bin/clud".into(),
        existing_version: Some("2.8.40".into()),
        replaces: true,
        path: Some(PathChange::Add {
            directory: "/home/me/.local/bin".into(),
            files: vec!["/home/me/.bashrc".into(), "/home/me/.bash_profile".into()],
        }),
        source: "https://github.com/talos-hq/clud/releases/download/v2.8.65/clud-2.8.65-x86_64-unknown-linux-musl".into(),
        sha256: "b8d6b6b8d163648b1928548c2b719174001f7910e0ebbf699e7add3cda45842a".into(),
        path_edit: "shell: /bin/bash; update /home/me/.bashrc; update /home/me/.bash_profile\nmanaged startup text:\n# >>> clud installer PATH >>>\nexport PATH=\"/home/me/.local/bin:$PATH\"\n# <<< clud installer PATH <<<".into(),
        prompt_engine: "pinned lokr 0.1.0 at /home/me/.clud/libexec/lokr/0.1.0-edcf91f8/lokr".into(),
    }
}

fn unix_paths() -> PathStyle {
    PathStyle {
        home: Some("/home/me".into()),
    }
}

fn render(selector: &impl Selector, columns: usize) -> String {
    selector::render(&selector.view(Duration::ZERO), columns).text()
}

/// Draws one screen at a terminal width.
type Draw = Box<dyn Fn(usize) -> String>;

/// Every screen at the widths the redesign targets, in every style.
fn gallery() -> Vec<(String, String)> {
    let mut renders = Vec::new();
    // The legacy Windows console: colour through VT, but ASCII glyphs.
    let conhost = Style {
        color: true,
        unicode: false,
    };
    for (name, style) in [
        ("plain", Style::PLAIN),
        ("color", COLOR),
        ("conhost", conhost),
    ] {
        // What the terminal holds after Show details: the plan in
        // scrollback, then the screen reopened below it.
        let details = move |columns| {
            let mut screen =
                Screen::menu(Some(&windows_facts()), &PathStyle::default(), false, style);
            screen.selected = 4;
            assert_eq!(screen.on_key(Key::Enter), Step::Done(None));
            assert_eq!(screen.on_exit(), OnExit::Erase);
            let printed = screen.details_text(columns);
            screen.details_shown();
            format!("\n{printed}{}", render(&screen, columns))
        };
        let screens: Vec<(&str, Draw)> = vec![
            (
                "windows-menu",
                Box::new(move |columns| {
                    render(
                        &Screen::menu(Some(&windows_facts()), &PathStyle::default(), false, style),
                        columns,
                    )
                }),
            ),
            ("windows-details", Box::new(details)),
            (
                "first-run-offer",
                Box::new(move |columns| {
                    render(
                        &Screen::menu(Some(&unix_facts()), &unix_paths(), true, style),
                        columns,
                    )
                }),
            ),
            (
                "unix-confirm",
                Box::new(move |columns| {
                    render(
                        &Screen::confirm(&unix_facts(), &unix_paths(), style),
                        columns,
                    )
                }),
            ),
            (
                "releases",
                Box::new(move |columns| {
                    render(
                        &Releases::new(
                            vec![row("2.9.0"), row(env!("CARGO_PKG_VERSION")), row("2.8.14")],
                            "2.9.0",
                            style,
                        ),
                        columns,
                    )
                }),
            ),
            (
                "repair",
                Box::new(move |columns| {
                    render(
                        &Screen::repair("Approve this additional activation repair?".into(), style),
                        columns,
                    )
                }),
            ),
        ];
        for (screen, draw) in &screens {
            for columns in [60, 80, 120] {
                renders.push((format!("{screen}-{columns}-{name}"), draw(columns)));
            }
        }
    }
    renders
}

/// Set `CLUD_INSTALLER_GALLERY=<dir>` to write every render for review.
#[test]
fn installer_screens_fit_their_width_and_degrade_without_colour() {
    let gallery = gallery();
    if let Some(dir) = std::env::var_os("CLUD_INSTALLER_GALLERY") {
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in &gallery {
            std::fs::write(dir.join(format!("{name}.txt")), text.replace("\r\n", "\n")).unwrap();
        }
    }
    for (name, text) in &gallery {
        let columns: usize = name.split('-').rev().nth(1).unwrap().parse().unwrap();
        if !name.starts_with("windows-details") {
            crate::selector::testing::assert_crlf_only(text);
        }
        for line in text.replace("\r\n", "\n").split('\n') {
            let visible = strip_ansi(line);
            assert!(visible.width() <= columns, "{name}: {visible:?}");
            assert_eq!(visible, visible.trim_end(), "{name}: trailing space");
        }
        if name.ends_with("plain") {
            assert!(!text.contains('\x1b'), "{name}");
            assert!(text.contains("  > "), "{name}: no pointer\n{text}");
        }
        assert!(!text.contains("[x]"), "{name}");
    }
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn summary_hides_internals_until_details_are_shown() {
    let facts = windows_facts();
    let mut screen = Screen::menu(Some(&facts), &PathStyle::default(), false, Style::PLAIN);
    let text = render(&screen, 80);
    assert!(text.contains("Install clud 2.8.65"), "{text}");
    assert!(
        text.contains(r"Adds to PATH  C:\Users\vpkmc\AppData\Local\Programs\clud\bin"),
        "{text}"
    );
    for internal in [
        facts.sha256.as_str(),
        "lokr",
        "REG_EXPAND_SZ",
        "hermes",
        "broadcast",
    ] {
        assert!(!text.contains(internal), "{internal} leaked:\n{text}");
    }
    for _ in 0..4 {
        let _ = screen.on_key(Key::Down);
    }
    assert_eq!(screen.on_key(Key::Enter), Step::Done(None));
    let text = screen.details_text(200);
    for internal in [facts.sha256.as_str(), "lokr", "REG_EXPAND_SZ"] {
        assert!(text.contains(internal), "{internal} missing:\n{text}");
    }
    // Reopened below the printed details, without the row that printed them.
    screen.details_shown();
    assert_eq!(screen.on_exit(), OnExit::Keep);
    assert_eq!(screen.selected(), 3);
    assert!(!render(&screen, 80).contains("Show details"));
}

#[test]
fn automatic_defaults_to_not_now_and_explicit_to_install() {
    let facts = unix_facts();
    let paths = unix_paths();
    let mut auto = Screen::menu(Some(&facts), &paths, true, Style::PLAIN);
    assert_eq!(auto.selected(), 3);
    assert!(render(&auto, 80).contains("  > Not now\r\n"));
    assert_eq!(
        auto.on_key(Key::Enter),
        Step::Done(Some(MenuChoice::NotNow))
    );
    assert_eq!(
        Screen::menu(Some(&facts), &paths, false, Style::PLAIN).on_key(Key::Enter),
        Step::Done(Some(MenuChoice::Current))
    );
    assert_eq!(
        auto.on_key(Key::Escape),
        Step::Done(Some(MenuChoice::Cancelled))
    );
    // Down from the explicit default reaches the release list, and twice the
    // download page: the order automation drives.
    let mut menu = Screen::menu(None, &paths, false, Style::PLAIN);
    let _ = menu.on_key(Key::Down);
    assert_eq!(
        menu.on_key(Key::Enter),
        Step::Done(Some(MenuChoice::Releases))
    );
    let _ = menu.on_key(Key::Down);
    assert_eq!(
        menu.on_key(Key::Enter),
        Step::Done(Some(MenuChoice::Browser))
    );
    // Without a plan there is nothing to detail.
    let _ = menu.on_key(Key::Down);
    let _ = menu.on_key(Key::Down);
    assert_eq!(
        menu.on_key(Key::Enter),
        Step::Done(Some(MenuChoice::NotNow))
    );
}

#[test]
fn confirm_defaults_to_not_now_and_up_installs() {
    let mut confirm = Screen::confirm(&unix_facts(), &unix_paths(), Style::PLAIN);
    assert_eq!(
        ConfirmChoice::from(match confirm.on_key(Key::Enter) {
            Step::Done(Some(choice)) => choice,
            other => panic!("{other:?}"),
        }),
        ConfirmChoice::NotNow
    );
    assert_eq!(confirm.on_key(Key::Up), Step::Redraw);
    assert_eq!(
        confirm.on_key(Key::Enter),
        Step::Done(Some(MenuChoice::Current))
    );
    let mut repair = Screen::repair("Repair?".into(), Style::PLAIN);
    assert_eq!(
        repair.on_key(Key::Enter),
        Step::Done(Some(MenuChoice::NotNow))
    );
    let _ = repair.on_key(Key::Up);
    assert_eq!(
        repair.on_key(Key::Enter),
        Step::Done(Some(MenuChoice::Current))
    );
}

#[test]
fn release_tags_are_words_and_escapes_only_with_colour() {
    let current = env!("CARGO_PKG_VERSION");
    for latest in [current, "99.0.0"] {
        let picker = Releases::new(vec![row(current), row("99.0.0")], latest, COLOR);
        let view = picker.view(Duration::ZERO);
        assert!(view.hints[0].contains(current));
        assert!(view.hints[0].contains(latest));
        let rendered = selector::render(&view, 200).text();
        // Tags are words in the default colour: no hue to lose on a theme.
        assert!(rendered.contains("  (this copy)"), "{rendered:?}");
        assert!(rendered.contains("  (latest)"), "{rendered:?}");
        assert!(!rendered.contains("\x1b[3"), "{rendered:?}");
        assert_eq!(rendered.matches("(this copy)").count(), 1);
        assert_eq!(rendered.matches("(latest)").count(), 1);
        let plain = Releases::new(vec![row(current), row("99.0.0")], latest, Style::PLAIN);
        let plain = selector::render(&plain.view(Duration::ZERO), 200).text();
        assert!(!plain.contains('\x1b'));
        assert_eq!(plain.matches("(this copy)").count(), 1);
    }
    let picker = Releases::new(vec![row("99.0.0")], "99.0.0", Style::PLAIN);
    assert!(!render(&picker, 200).contains("(this copy)"));
}

#[test]
fn release_picker_defaults_to_stable_pointer_and_keeps_exact_choice() {
    let mut picker = Releases::new(
        vec![row("3.0.0-rc.1"), row("2.9.0"), row("2.8.14")],
        "2.9.0",
        Style::PLAIN,
    );
    assert_eq!(picker.selected().unwrap().version, "2.9.0");
    assert!(matches!(picker.on_key(Key::Down), Step::Redraw));
    assert!(
        matches!(picker.on_key(Key::Enter), Step::Done(ReleaseChoice::Selected(ref selected)) if selected.version == "2.8.14")
    );
    let long = Releases::new(
        (0..10).map(|minor| row(&format!("2.{minor}.0"))).collect(),
        "2.5.0",
        Style::PLAIN,
    );
    let frame = selector::render_within(&long.view(Duration::ZERO), 40, 14);
    assert!(frame.text().contains("more above"), "{}", frame.text());
    assert!(frame.text().contains("more below"), "{}", frame.text());
    assert!(frame.rows() <= 13, "{}", frame.text());
    assert!(frame
        .bytes()
        .iter()
        .enumerate()
        .all(|(i, byte)| *byte != b'\n' || (i > 0 && frame.bytes()[i - 1] == b'\r')));
}
