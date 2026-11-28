use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::ValueEnum;
use icy_sixel::{BackgroundMode, EncodeOptions, QuantizeMethod, SixelImage};
use running_process::{
    CapabilityStatus, EvidenceStrength, GraphicsProtocol, TerminalCapabilities,
    TerminalGraphicsCapabilities,
};
use serde::{Deserialize, Serialize};

const DEFAULT_HEADER_ROWS: u16 = 6;
const MIN_TEXT_ROWS: u16 = 8;
const CELL_WIDTH_PX: u32 = 8;
const CELL_HEIGHT_PX: u32 = 12;
const MAX_HEADER_WIDTH_PX: u32 = 520;
const MAX_DEMO_WIDTH_PX: u32 = 960;
const MAX_DEMO_HEIGHT_PX: u32 = 540;
const DEMO_STATUS_LINE: &str = "\nclud Sixel demo: hero-clud\n";
const HERO_CLUD_JPG: &[u8] = include_bytes!("../assets/hero-clud.jpg");

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum GraphicsMode {
    #[default]
    Auto,
    Off,
    Sixel,
}

impl std::fmt::Display for GraphicsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::Sixel => "sixel",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphicsConfig {
    #[serde(default)]
    pub mode: GraphicsMode,
    #[serde(default)]
    pub image_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicsDecision {
    pub enabled: bool,
    pub forced: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderRender {
    pub bytes: Vec<u8>,
    pub restore_bytes: Vec<u8>,
    pub reserved_rows: u16,
    pub text_rows: u16,
}

pub fn reset_layout_bytes(terminal_rows: u16, clear_screen: bool) -> Vec<u8> {
    let row = terminal_rows.max(1);
    if clear_screen {
        format!("\x1b[?6l\x1b[r\x1b[H\x1b[J\x1b[{row};1H").into_bytes()
    } else {
        format!("\x1b[?6l\x1b[r\x1b[{row};1H").into_bytes()
    }
}

pub fn detect_current_terminal() -> TerminalCapabilities {
    running_process::current_terminal_capabilities_with_timeout(Duration::from_millis(150))
}

pub fn unknown_terminal() -> TerminalCapabilities {
    TerminalCapabilities {
        is_tty: false,
        term: None,
        terminal_program: None,
        graphics: TerminalGraphicsCapabilities::unknown(),
    }
}

pub fn decide_sixel(
    config: &GraphicsConfig,
    terminal: Option<&TerminalCapabilities>,
) -> GraphicsDecision {
    match config.mode {
        GraphicsMode::Off => GraphicsDecision {
            enabled: false,
            forced: false,
            reason: "graphics disabled by --graphics=off".to_string(),
        },
        GraphicsMode::Sixel => GraphicsDecision {
            enabled: true,
            forced: true,
            reason: "Sixel forced by --graphics=sixel".to_string(),
        },
        GraphicsMode::Auto => decide_auto_sixel(terminal),
    }
}

fn decide_auto_sixel(terminal: Option<&TerminalCapabilities>) -> GraphicsDecision {
    let Some(terminal) = terminal else {
        return GraphicsDecision {
            enabled: false,
            forced: false,
            reason: "graphics auto skipped: missing terminal capability metadata".to_string(),
        };
    };
    if !terminal.is_tty {
        return GraphicsDecision {
            enabled: false,
            forced: false,
            reason: "graphics auto skipped: attached client is not a TTY".to_string(),
        };
    }

    let Some(capability) = terminal
        .graphics
        .protocols
        .iter()
        .find(|capability| capability.protocol == GraphicsProtocol::Sixel)
    else {
        return GraphicsDecision {
            enabled: false,
            forced: false,
            reason: "graphics auto skipped: no Sixel capability record".to_string(),
        };
    };

    if capability.status == CapabilityStatus::Supported
        && capability.evidence == EvidenceStrength::Probe
    {
        GraphicsDecision {
            enabled: true,
            forced: false,
            reason: format!(
                "graphics auto enabled: Sixel supported by probe from {}",
                capability.source
            ),
        }
    } else {
        GraphicsDecision {
            enabled: false,
            forced: false,
            reason: format!(
                "graphics auto skipped: Sixel status={:?} evidence={:?} source={} risks={}",
                capability.status,
                capability.evidence,
                capability.source,
                capability.risks.join(",")
            ),
        }
    }
}

pub fn capability_summary(terminal: Option<&TerminalCapabilities>) -> String {
    let Some(terminal) = terminal else {
        return "terminal=missing".to_string();
    };
    let mut parts = vec![format!("is_tty={}", terminal.is_tty)];
    if let Some(term) = &terminal.term {
        parts.push(format!("TERM={term}"));
    }
    if let Some(program) = &terminal.terminal_program {
        parts.push(format!("TERM_PROGRAM={program}"));
    }
    for capability in &terminal.graphics.protocols {
        parts.push(format!(
            "{:?}:{:?}/{:?}/{}",
            capability.protocol, capability.status, capability.evidence, capability.source
        ));
    }
    parts.join(" ")
}

pub fn render_demo_sixel_bytes(terminal_cols: Option<u16>) -> io::Result<Vec<u8>> {
    let cols = terminal_cols.filter(|cols| *cols > 0).unwrap_or(80);
    let max_width = u32::from(cols)
        .saturating_mul(CELL_WIDTH_PX)
        .clamp(240, MAX_DEMO_WIDTH_PX);
    let max_height = max_width
        .saturating_mul(9)
        .checked_div(16)
        .unwrap_or(MAX_DEMO_HEIGHT_PX)
        .clamp(120, MAX_DEMO_HEIGHT_PX);
    let (rgba, width, height) = load_image_rgba_from_memory(HERO_CLUD_JPG, max_width, max_height)?;
    let sixel = encode_sixel_rgba(rgba, width, height)?;
    let mut bytes = Vec::with_capacity(sixel.len() + DEMO_STATUS_LINE.len());
    bytes.extend_from_slice(sixel.as_bytes());
    bytes.extend_from_slice(DEMO_STATUS_LINE.as_bytes());
    Ok(bytes)
}

pub fn render_header(
    config: &GraphicsConfig,
    terminal_rows: u16,
    terminal_cols: u16,
) -> io::Result<Option<HeaderRender>> {
    if terminal_rows <= MIN_TEXT_ROWS + 1 || terminal_cols < 30 {
        return Ok(None);
    }

    let max_reserved = terminal_rows.saturating_sub(MIN_TEXT_ROWS).max(1);
    let target_rows = DEFAULT_HEADER_ROWS.min(max_reserved);
    let max_width = u32::from(terminal_cols)
        .saturating_mul(CELL_WIDTH_PX)
        .clamp(160, MAX_HEADER_WIDTH_PX);
    let max_height = u32::from(target_rows).saturating_mul(CELL_HEIGHT_PX);

    let (rgba, width, height) = match &config.image_path {
        Some(path) => load_image_rgba(path, max_width, max_height)?,
        None => load_image_rgba_from_memory(HERO_CLUD_JPG, max_width, max_height)?,
    };
    let reserved_rows = rows_for_pixel_height(height).min(max_reserved).max(1);
    if terminal_rows.saturating_sub(reserved_rows) < MIN_TEXT_ROWS {
        return Ok(None);
    }

    let sixel = encode_sixel_rgba(rgba, width, height)?;

    let first_text_row = reserved_rows + 1;
    let text_rows = terminal_rows.saturating_sub(reserved_rows).max(1);
    let mut bytes = Vec::new();
    write!(
        bytes,
        "\x1b[?6l\x1b[r\x1b[H\x1b[J{}\x1b[{};1H\x1b[{};{}r\x1b[?6h\x1b[{};1H",
        sixel, first_text_row, first_text_row, terminal_rows, first_text_row
    )
    .expect("writing into Vec cannot fail");
    let restore_bytes = reset_layout_bytes(terminal_rows, false);

    Ok(Some(HeaderRender {
        bytes,
        restore_bytes,
        reserved_rows,
        text_rows,
    }))
}

fn load_image_rgba(
    path: &PathBuf,
    max_width: u32,
    max_height: u32,
) -> io::Result<(Vec<u8>, u32, u32)> {
    let reader = image::ImageReader::open(path)
        .map_err(|err| io::Error::other(format!("failed to open graphics image: {err}")))?;
    let image = reader
        .decode()
        .map_err(|err| io::Error::other(format!("failed to decode graphics image: {err}")))?;
    Ok(resize_image_rgba(image, max_width, max_height))
}

fn load_image_rgba_from_memory(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
) -> io::Result<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory(bytes).map_err(|err| {
        io::Error::other(format!("failed to decode bundled graphics image: {err}"))
    })?;
    Ok(resize_image_rgba(image, max_width, max_height))
}

fn resize_image_rgba(
    image: image::DynamicImage,
    max_width: u32,
    max_height: u32,
) -> (Vec<u8>, u32, u32) {
    let resized = image.resize(
        max_width.max(1),
        max_height.max(1),
        image::imageops::FilterType::Lanczos3,
    );
    let rgba = resized.to_rgba8();
    let (width, height) = rgba.dimensions();
    (rgba.into_raw(), width, height)
}

fn encode_sixel_rgba(rgba: Vec<u8>, width: u32, height: u32) -> io::Result<String> {
    let options = EncodeOptions {
        max_colors: 48,
        diffusion: 0.0,
        quantize_method: QuantizeMethod::Wu,
    };
    SixelImage::try_from_rgba(rgba, width as usize, height as usize)
        .map_err(|err| io::Error::other(err.to_string()))?
        .with_background_mode(BackgroundMode::Transparent)
        .encode_with(&options)
        .map_err(|err| io::Error::other(err.to_string()))
}

fn rows_for_pixel_height(height: u32) -> u16 {
    height
        .saturating_add(CELL_HEIGHT_PX - 1)
        .checked_div(CELL_HEIGHT_PX)
        .unwrap_or(1)
        .clamp(1, u32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::GraphicsCapability;

    fn terminal_with_sixel(
        status: CapabilityStatus,
        evidence: EvidenceStrength,
    ) -> TerminalCapabilities {
        TerminalCapabilities {
            is_tty: true,
            term: Some("xterm-256color".into()),
            terminal_program: Some("test".into()),
            graphics: TerminalGraphicsCapabilities {
                protocols: vec![GraphicsCapability {
                    protocol: GraphicsProtocol::Sixel,
                    status,
                    evidence,
                    source: "test".into(),
                    risks: Vec::new(),
                }],
                preferred: Some(GraphicsProtocol::Sixel),
            },
        }
    }

    #[test]
    fn auto_enables_only_probe_supported_sixel() {
        let config = GraphicsConfig::default();
        let terminal = terminal_with_sixel(CapabilityStatus::Supported, EvidenceStrength::Probe);
        let decision = decide_sixel(&config, Some(&terminal));
        assert!(decision.enabled);
        assert!(!decision.forced);
    }

    #[test]
    fn auto_rejects_strong_host_signal_for_v1() {
        let config = GraphicsConfig::default();
        let terminal = terminal_with_sixel(
            CapabilityStatus::Supported,
            EvidenceStrength::StrongHostSignal,
        );
        let decision = decide_sixel(&config, Some(&terminal));
        assert!(!decision.enabled);
    }

    #[test]
    fn auto_rejects_missing_and_non_tty_metadata() {
        let config = GraphicsConfig::default();
        assert!(!decide_sixel(&config, None).enabled);
        let mut terminal =
            terminal_with_sixel(CapabilityStatus::Supported, EvidenceStrength::Probe);
        terminal.is_tty = false;
        assert!(!decide_sixel(&config, Some(&terminal)).enabled);
    }

    #[test]
    fn explicit_modes_override_capabilities() {
        let terminal = terminal_with_sixel(
            CapabilityStatus::Blocked,
            EvidenceStrength::StrongHostSignal,
        );
        let off = GraphicsConfig {
            mode: GraphicsMode::Off,
            image_path: None,
        };
        assert!(!decide_sixel(&off, Some(&terminal)).enabled);
        let forced = GraphicsConfig {
            mode: GraphicsMode::Sixel,
            image_path: None,
        };
        let decision = decide_sixel(&forced, Some(&terminal));
        assert!(decision.enabled);
        assert!(decision.forced);
    }

    #[test]
    fn header_renderer_skips_tiny_terminals() {
        let config = GraphicsConfig::default();
        assert!(render_header(&config, 8, 80).unwrap().is_none());
        assert!(render_header(&config, 24, 80).unwrap().is_some());
    }

    #[test]
    fn header_bytes_manage_scroll_region_and_restore() {
        let config = GraphicsConfig::default();
        let header = render_header(&config, 24, 80).unwrap().unwrap();
        assert!(header.reserved_rows > 0);
        assert_eq!(header.text_rows, 24 - header.reserved_rows);
        let text = String::from_utf8_lossy(&header.bytes);
        assert!(text.contains("\x1b[?6h"));
        assert!(text.contains("\x1b["));
        let restore = String::from_utf8_lossy(&header.restore_bytes);
        assert!(restore.contains("\x1b[r"));
    }

    #[test]
    fn default_header_uses_bundled_hero_asset() {
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let hero_path = temp.path().join("hero.jpg");
        std::fs::write(&hero_path, HERO_CLUD_JPG).unwrap();

        let default = render_header(&GraphicsConfig::default(), 24, 80)
            .unwrap()
            .unwrap();
        let explicit_hero = render_header(
            &GraphicsConfig {
                mode: GraphicsMode::Auto,
                image_path: Some(hero_path),
            },
            24,
            80,
        )
        .unwrap()
        .unwrap();

        assert_eq!(default.bytes, explicit_hero.bytes);
        assert_eq!(default.reserved_rows, explicit_hero.reserved_rows);
    }

    #[test]
    fn explicit_header_image_override_takes_precedence() {
        let temp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let override_path = temp.path().join("override.png");
        let image = image::RgbaImage::from_pixel(16, 16, image::Rgba([255, 0, 0, 255]));
        image.save(&override_path).unwrap();

        let default = render_header(&GraphicsConfig::default(), 24, 80)
            .unwrap()
            .unwrap();
        let override_header = render_header(
            &GraphicsConfig {
                mode: GraphicsMode::Auto,
                image_path: Some(override_path),
            },
            24,
            80,
        )
        .unwrap()
        .unwrap();

        assert_ne!(default.bytes, override_header.bytes);
    }

    #[test]
    fn reset_layout_bytes_can_clear_for_resize_skip() {
        let clear = String::from_utf8_lossy(&reset_layout_bytes(24, true)).into_owned();
        assert!(clear.contains("\x1b[r"));
        assert!(clear.contains("\x1b[H\x1b[J"));
        assert!(clear.ends_with("\x1b[24;1H"));

        let restore = String::from_utf8_lossy(&reset_layout_bytes(24, false)).into_owned();
        assert!(restore.contains("\x1b[r"));
        assert!(!restore.contains("\x1b[H\x1b[J"));
        assert!(restore.ends_with("\x1b[24;1H"));
    }

    #[test]
    fn demo_sixel_bytes_use_bundled_hero_asset() {
        let bytes = render_demo_sixel_bytes(Some(80)).expect("render demo");
        assert!(
            bytes.starts_with(b"\x1bP"),
            "demo must start with Sixel DCS"
        );
        assert!(bytes.ends_with(DEMO_STATUS_LINE.as_bytes()));
        let payload = &bytes[..bytes.len() - DEMO_STATUS_LINE.len()];
        assert!(
            payload.ends_with(b"\x1b\\"),
            "demo payload must end with Sixel string terminator"
        );
        assert!(payload.contains(&b'q'), "Sixel raster introducer missing");
    }
}
