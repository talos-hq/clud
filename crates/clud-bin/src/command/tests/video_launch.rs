//! Unit tests for the `clud video` launch contract (#1851): the video-use
//! plugin is loaded with `--plugin-dir` for that one session and never for an
//! ordinary launch.

use super::*;
use crate::args::Command;

fn plugin_dir_arg(p: &LaunchPlan) -> Option<&str> {
    p.command
        .iter()
        .position(|arg| arg == "--plugin-dir")
        .and_then(|i| p.command.get(i + 1))
        .map(String::as_str)
}

#[test]
fn video_parses_as_its_own_subcommand_not_passthrough() {
    let args = parse(&["clud", "video", "clips", "--update"]);
    match args.command {
        Some(Command::Video(options)) => {
            let crate::args::VideoArgs { dir, update, path } = *options;
            assert!(!path);
            assert_eq!(dir.as_deref(), Some(std::path::Path::new("clips")));
            assert!(update);
        }
        other => panic!("expected Command::Video, got {other:?}"),
    }
    assert!(args.passthrough.is_empty());
    let bare = parse(&["clud", "video"]);
    assert!(matches!(
        bare.command,
        Some(Command::Video(options)) if options.dir.is_none() && !options.update && !options.path
    ));
    let path = parse(&["clud", "video", "--path"]);
    assert!(matches!(
        path.command,
        Some(Command::Video(options)) if options.path
    ));
    assert!(path.passthrough.is_empty());
}

#[test]
fn video_launch_loads_the_plugin_and_seeds_the_skill() {
    let p = plan(&["clud", "video"]);
    let dir = plugin_dir_arg(&p).expect("clud video passes --plugin-dir");
    assert!(
        std::path::Path::new(dir).ends_with(
            std::path::Path::new(".clud")
                .join("extern")
                .join("video-use-plugin")
        ),
        "{dir}"
    );
    assert!(last_arg(&p).starts_with("/video-use "), "{:?}", p.command);
}

#[test]
fn plain_launch_never_loads_the_video_plugin() {
    for raw in [&["clud"][..], &["clud", "-p", "hi"][..]] {
        let p = plan(raw);
        assert!(plugin_dir_arg(&p).is_none(), "{:?}", p.command);
        assert!(!p.command.iter().any(|arg| arg.contains("video-use")));
    }
}

#[test]
fn video_requires_the_claude_harness() {
    let args = parse(&["clud", "--harness", "deepseek", "video"]);
    assert!(video_launch_error(&args, deepseek_harness_target()).is_some());
    let plain = parse(&["clud", "--harness", "deepseek"]);
    assert!(video_launch_error(&plain, deepseek_harness_target()).is_none());
}
