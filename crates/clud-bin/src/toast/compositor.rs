//! Toast compositor for the PTY pump's writer thread (#1189).
//!
//! Every byte the child writes passes through [`Compositor::on_child`] on its
//! way to the terminal. The compositor keeps a `vt100` shadow of the child's
//! intended screen and an [`EscapeTracker`], and inserts toast bytes only at
//! points where they cannot corrupt the child's output:
//!
//! - between escape sequences and outside a synchronized-update block (a
//!   block left open is waited out for [`SYNC_DEFER_LIMIT`]);
//! - never while the cursor sits in the last column, where a CUP would cancel
//!   the terminal's pending wrap.
//!
//! The cursor is always put back with an absolute CUP computed from the
//! shadow, never DECSC/DECRC: the child may be using that single save slot.
//! Origin mode is honoured by switching it off for the toast write and
//! restoring the cursor relative to the scroll region afterwards.
//!
//! Tier behaviour is documented in [`super::tier`]; the design and its limits
//! are in `docs/architecture/toasts.md`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vt100::{MouseProtocolEncoding, MouseProtocolMode};

use super::kitty;
use super::raster;
use super::statusline::{usage_title, StatusStateWriter, StatusUsage};
use super::text_tier::{self, CellRect};
use super::tier::ToastTier;
use super::tracker::EscapeTracker;
use super::{Toast, ToastEvent, ToastHub};
use crate::cpu_banner::CPU_TOAST_KEY;

/// How long a synchronized-update block may hold a toast back before it is
/// drawn anyway. A block that stays open this long is a child that stopped
/// mid-frame; drawing inside it is harmless once the stream is at ground.
pub const SYNC_DEFER_LIMIT: Duration = Duration::from_millis(250);

/// Writer-thread wake cadence while a toast is pending, visible, or may
/// expire.
pub const TICK: Duration = Duration::from_millis(100);

const CPU_FADE_AFTER: Duration = Duration::from_secs(2);
const CPU_INITIAL_OPACITY: u8 = 90;
const CPU_RESTING_OPACITY: u8 = 50;

/// What renders a toast when no in-grid tier applies.
#[derive(Clone, Default)]
pub enum Fallback {
    /// Claude: the injected `statusLine` reads this state file.
    StatusFile(Arc<StatusStateWriter>),
    /// Everything else: the terminal title (xterm title stack push/pop).
    Title,
    #[default]
    None,
}

impl std::fmt::Debug for Fallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StatusFile(_) => "StatusFile",
            Self::Title => "Title",
            Self::None => "None",
        })
    }
}

/// Everything the pump needs to run a compositor.
#[derive(Clone, Debug)]
pub struct ToastPumpOptions {
    pub hub: Arc<ToastHub>,
    pub tier: ToastTier,
    pub fallback: Fallback,
    /// Launch-wide exact bridge usage. This remains available in PTY mode for
    /// all harnesses; Claude's status line is merely an additional surface.
    pub usage: Option<Arc<StatusStateWriter>>,
    pub rows: u16,
    pub cols: u16,
    pub image_id: u32,
    pub process_gear: bool,
}

/// Shared with the stdin path: the close button to hit-test, present only
/// while clicking it can work (toast visible and the child has SGR mouse
/// reporting on).
#[derive(Debug, Default)]
pub struct ToastInput {
    state: Mutex<InputState>,
}

#[derive(Debug, Default)]
struct InputState {
    close: Option<CellRect>,
    gear: Option<CellRect>,
    cpu: Option<CellRect>,
    hover_armed: bool,
    hovering_cpu: bool,
}

impl ToastInput {
    pub fn gear_rect(&self) -> Option<CellRect> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).gear
    }

    fn set_gear(&self, rect: Option<CellRect>) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).gear = rect;
    }

    pub fn close_rect(&self) -> Option<CellRect> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).close
    }

    fn set(&self, rect: Option<CellRect>) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).close = rect;
    }

    pub fn cpu_rect(&self) -> Option<CellRect> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).cpu
    }

    pub fn cpu_hover_armed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hover_armed
    }

    pub fn set_cpu_hover(&self, hovering: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.hovering_cpu = state.cpu.is_some() && state.hover_armed && hovering;
    }

    fn cpu_hovered(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.hovering_cpu && state.hover_armed && state.cpu.is_some()
    }

    fn set_cpu(&self, rect: Option<CellRect>, hover_armed: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.cpu != rect {
            state.hovering_cpu = false;
        }
        state.cpu = rect;
        state.hover_armed = hover_armed;
        if rect.is_none() || !hover_armed {
            state.hovering_cpu = false;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Drawn {
    None,
    Kitty { cols: u16, rows: u16, col: u16 },
    Cells { rect: CellRect },
    Title,
    Status,
}

pub struct Compositor {
    hub: Arc<ToastHub>,
    tier: ToastTier,
    fallback: Fallback,
    usage_writer: Option<Arc<StatusStateWriter>>,
    image_id: u32,
    process_gear: bool,
    gear_drawn: bool,
    shadow: vt100::Parser,
    /// Rewrites the sequences `vt100` drops before they reach `shadow` (#2028).
    shadow_feed: super::shadow_input::ShadowFeed,
    tracker: EscapeTracker,
    input: Arc<ToastInput>,
    visible: Option<Toast>,
    drawn: Drawn,
    pending: bool,
    deferred_since: Option<Instant>,
    /// Content of the image currently uploaded to the terminal, if any.
    transmitted: Option<(String, super::Severity, u16, u16)>,
    /// The terminal's copy of the text-cell toast may be stale in unknown
    /// places (a burst began mid-sequence), so repaint the whole screen.
    repaint_screen: bool,
    title_pushed: bool,
    usage: Option<StatusUsage>,
    cpu: Option<Toast>,
    cpu_appeared_at: Option<Instant>,
    cpu_drawn: bool,
    cpu_transmitted: Option<(String, super::Severity, u16, u16, u8)>,
    /// The CPU toast last mirrored into Claude's status line (#2367).
    status_cpu: Option<(String, super::Severity, Instant, Option<Instant>)>,
}

impl Compositor {
    pub fn new(options: ToastPumpOptions) -> Self {
        let rows = options.rows.max(1);
        let cols = options.cols.max(1);
        Self {
            hub: options.hub,
            tier: options.tier,
            fallback: options.fallback,
            usage_writer: options.usage,
            image_id: options.image_id,
            process_gear: options.process_gear,
            gear_drawn: false,
            shadow: vt100::Parser::new(rows, cols, 0),
            shadow_feed: super::shadow_input::ShadowFeed::default(),
            tracker: EscapeTracker::new(),
            input: Arc::new(ToastInput::default()),
            visible: None,
            drawn: Drawn::None,
            pending: false,
            deferred_since: None,
            transmitted: None,
            repaint_screen: false,
            title_pushed: false,
            usage: None,
            cpu: None,
            cpu_appeared_at: None,
            cpu_drawn: false,
            cpu_transmitted: None,
            status_cpu: None,
        }
    }

    pub fn input(&self) -> Arc<ToastInput> {
        Arc::clone(&self.input)
    }

    /// The shadow of the child's intended screen (tests, diagnostics).
    pub fn shadow(&self) -> &vt100::Screen {
        self.shadow.screen()
    }

    /// Whether the writer should wake on [`TICK`] rather than block.
    pub fn wants_tick(&self) -> bool {
        // Producers publish from another thread while the child TUI may be
        // completely quiet. A bounded wake is needed even before the first
        // event, and later for the opacity transition and expiry.
        self.tier != ToastTier::Off
    }

    /// Forward one child burst, compositing the toast around it.
    pub fn on_child(&mut self, bytes: &[u8], now: Instant) -> Vec<u8> {
        if self.tier == ToastTier::Off {
            self.shadow_feed.feed(&mut self.shadow, bytes);
            return bytes.to_vec();
        }
        let mut out = Vec::with_capacity(bytes.len() + 64);
        let gear_before = self.gear_target();
        self.sync_hub(now);
        self.sync_usage();
        self.sync_cpu_opacity(now);

        // A text-cell toast is lifted before the child draws so scrolls and
        // partial rewrites move the child's real content, not ours. If the
        // stream is mid-sequence we cannot lift it; repaint everything later.
        if let Drawn::Cells { rect } = self.drawn {
            if self.tracker.is_safe() && !self.wrap_pending() {
                out.extend(self.lift_cells(rect));
            } else {
                self.repaint_screen = true;
                self.drawn = Drawn::None;
            }
            self.pending = true;
        }

        out.extend_from_slice(bytes);
        self.tracker.feed(bytes);
        self.shadow_feed.feed(&mut self.shadow, bytes);
        self.pending |= gear_before != self.gear_target();

        let flags = self.tracker.take_burst();
        if flags.any() {
            // Kitty drops images on clear/reset/alt switch; the alternate
            // screen takes any text-cell toast with it.
            self.transmitted = None;
            self.cpu_transmitted = None;
            self.cpu_drawn = false;
            self.gear_drawn = false;
            self.input.set_gear(None);
            if matches!(self.drawn, Drawn::Kitty { .. } | Drawn::Cells { .. }) {
                self.drawn = Drawn::None;
            }
            self.pending = true;
        }
        if matches!(self.drawn, Drawn::Kitty { .. }) {
            // Images scroll with text: pin the toast back after every burst.
            self.pending = true;
        }
        if self.cpu_drawn || self.gear_drawn {
            self.pending = true;
        }
        out.extend(self.render(now));
        out
    }

    pub fn on_resize(&mut self, rows: u16, cols: u16, now: Instant) -> Vec<u8> {
        self.shadow.screen_mut().set_size(rows.max(1), cols.max(1));
        if self.tier == ToastTier::Off {
            return Vec::new();
        }
        if matches!(self.drawn, Drawn::Cells { .. }) {
            self.repaint_screen = true;
        }
        // Keep Kitty placement ownership until render can either re-pin it or
        // delete it when the resized screen no longer has room.
        if matches!(self.drawn, Drawn::Cells { .. }) {
            self.drawn = Drawn::None;
        }
        self.sync_hub(now);
        self.sync_usage();
        self.sync_cpu_opacity(now);
        self.pending = true;
        self.render(now)
    }

    pub fn on_tick(&mut self, now: Instant) -> Vec<u8> {
        if self.tier == ToastTier::Off {
            return Vec::new();
        }
        self.sync_hub(now);
        self.sync_usage();
        self.sync_cpu_opacity(now);
        self.render(now)
    }

    /// Remove any toast and free terminal resources. Called when the pump
    /// exits; the stream is assumed quiescent, so this does not wait for a
    /// safe point.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = self.remove_drawn();
        out.extend(self.remove_cpu_drawn());
        out.extend(self.remove_gear());
        if self.transmitted.take().is_some() {
            out.extend(kitty::delete_image(self.image_id));
        }
        if self.cpu_transmitted.take().is_some() {
            out.extend(kitty::delete_image(self.cpu_image_id()));
        }
        self.input.set(None);
        out
    }

    fn sync_hub(&mut self, now: Instant) {
        let snapshot = self.hub.snapshot_with_key(now, CPU_TOAST_KEY);
        // With Claude's status line injected the CPU reading lives there: no
        // grid overlay, so nothing covers the header and nothing needs a
        // dismiss control (#2367).
        let cpu = if matches!(self.fallback, Fallback::StatusFile(_)) {
            self.mirror_cpu_to_status(snapshot.keyed.as_ref());
            None
        } else {
            snapshot
                .keyed
                .filter(|toast| self.cpu_cells(&toast.text).is_some())
        };
        let visible = if cpu.is_some() || matches!(self.fallback, Fallback::StatusFile(_)) {
            snapshot.other_visible
        } else {
            snapshot.visible
        };
        let changed = match (&self.visible, &visible) {
            (Some(a), Some(b)) => !a.same_content(b),
            (None, None) => false,
            _ => true,
        };
        let cpu_changed = match (&self.cpu, &cpu) {
            (Some(a), Some(b)) => !a.same_content(b),
            (None, None) => false,
            _ => true,
        };
        if self.cpu.is_none() && cpu.is_some() {
            self.cpu_appeared_at = Some(now);
        } else if cpu.is_none() {
            self.cpu_appeared_at = None;
        }
        self.visible = visible;
        self.cpu = cpu;
        if changed || cpu_changed {
            self.pending = true;
        }
    }

    fn mirror_cpu_to_status(&mut self, toast: Option<&Toast>) {
        let Fallback::StatusFile(writer) = &self.fallback else {
            return;
        };
        let next = toast.map(|t| (t.text.clone(), t.severity, t.shown_at, t.expires_at));
        if next == self.status_cpu {
            return;
        }
        match toast {
            Some(t) => writer.publish(ToastEvent::Show(t.clone())),
            None => writer.publish(ToastEvent::Close {
                key: CPU_TOAST_KEY.into(),
            }),
        }
        self.status_cpu = next;
    }

    fn sync_usage(&mut self) {
        let usage = self
            .usage_writer
            .as_ref()
            .and_then(|writer| writer.effective_usage_snapshot());
        if usage != self.usage {
            self.usage = usage;
            self.pending = true;
        }
    }

    fn sync_cpu_opacity(&mut self, now: Instant) {
        if self.cpu.is_some()
            && self.cpu_transmitted.as_ref().map(|content| content.4) != Some(self.cpu_opacity(now))
        {
            self.pending = true;
        }
    }

    fn cpu_opacity(&self, now: Instant) -> u8 {
        if self.input.cpu_hovered()
            || self
                .cpu_appeared_at
                .is_some_and(|at| now.saturating_duration_since(at) < CPU_FADE_AFTER)
        {
            CPU_INITIAL_OPACITY
        } else {
            CPU_RESTING_OPACITY
        }
    }

    fn cpu_cells(&self, text: &str) -> Option<(u16, u16)> {
        let (term_rows, term_cols) = self.shadow.screen().size();
        if self.tier != ToastTier::Kitty || term_rows < raster::TOAST_ROWS {
            return None;
        }
        raster::cpu_cells(text, term_cols)
    }

    fn wrap_pending(&self) -> bool {
        let (_, cols) = self.shadow.screen().size();
        let (_, col) = self.shadow.screen().cursor_position();
        col.saturating_add(1) >= cols
    }

    fn injection_allowed(&mut self, now: Instant) -> bool {
        if !self.tracker.at_ground() || self.wrap_pending() {
            self.deferred_since.get_or_insert(now);
            return false;
        }
        if !self.tracker.sync_active() {
            return true;
        }
        let since = *self.deferred_since.get_or_insert(now);
        now.duration_since(since) >= SYNC_DEFER_LIMIT
    }

    fn render(&mut self, now: Instant) -> Vec<u8> {
        if !self.pending {
            return Vec::new();
        }
        let needs_bytes = !matches!(self.target(), Target::Status | Target::Nothing)
            || matches!(
                self.drawn,
                Drawn::Kitty { .. } | Drawn::Cells { .. } | Drawn::Title
            )
            || self.repaint_screen
            || self.cpu_drawn
            || self.cpu_target()
            || self.gear_drawn
            || self.gear_target();
        if needs_bytes && !self.injection_allowed(now) {
            return Vec::new();
        }
        self.pending = false;
        self.deferred_since = None;
        let target = self.target();
        let mut out = Vec::new();

        // Leaving a surface: clean it up before drawing the new one.
        let keep = matches!(
            (&self.drawn, &target),
            (Drawn::Kitty { .. }, Target::Kitty)
                | (Drawn::Title, Target::Title)
                | (Drawn::Status, Target::Status)
        );
        if !keep {
            out.extend(self.remove_drawn());
        }
        if self.repaint_screen {
            self.repaint_screen = false;
            out.extend(self.with_cursor_parked(text_tier::restore_screen(self.shadow.screen())));
        }

        if let Some(toast) = self.visible.clone() {
            match target {
                Target::Kitty => out.extend(self.draw_kitty(&toast)),
                Target::Cells => out.extend(self.draw_cells(&toast)),
                Target::Title => out.extend(self.draw_title(&toast)),
                Target::Status => self.draw_status(&toast),
                Target::Nothing => self.input.set(None),
            }
        } else {
            if target == Target::Title {
                out.extend(self.draw_usage_title());
            } else {
                self.input.set(None);
            }
        }
        out.extend(self.render_cpu(now));
        out.extend(self.render_gear());
        out
    }

    fn target(&self) -> Target {
        if self.visible.is_none() {
            if self.usage.is_some()
                && !self.cpu_target()
                && matches!(self.fallback, Fallback::Title)
            {
                return Target::Title;
            }
            return Target::Nothing;
        }
        let (rows, cols) = self.shadow.screen().size();
        match self.tier {
            ToastTier::Kitty
                if raster::toast_cells("", cols).is_some()
                    && rows
                        >= raster::TOAST_ROWS
                            + if self.cpu_target() {
                                raster::TOAST_ROWS
                            } else {
                                0
                            } =>
            {
                return Target::Kitty;
            }
            ToastTier::TextCells
                if self.shadow.screen().alternate_screen()
                    && text_tier::toast_rect("", rows, cols).is_some() =>
            {
                return Target::Cells
            }
            ToastTier::Off => return Target::Nothing,
            _ => {}
        }
        match self.fallback {
            Fallback::StatusFile(_) => Target::Status,
            Fallback::Title => Target::Title,
            Fallback::None => Target::Nothing,
        }
    }

    fn remove_drawn(&mut self) -> Vec<u8> {
        let drawn = std::mem::replace(&mut self.drawn, Drawn::None);
        match drawn {
            Drawn::None => Vec::new(),
            Drawn::Kitty { .. } => {
                kitty::delete_placement(self.image_id, kitty::TOAST_PLACEMENT_ID)
            }
            Drawn::Cells { rect } => self.lift_cells(rect),
            Drawn::Title => {
                self.title_pushed = false;
                b"\x1b[23;0t".to_vec()
            }
            Drawn::Status => {
                if let Fallback::StatusFile(writer) = &self.fallback {
                    writer.publish(ToastEvent::Close {
                        key: STATUS_KEY.into(),
                    });
                }
                Vec::new()
            }
        }
    }

    fn gear_image_id(&self) -> u32 {
        self.image_id ^ 0x0080_0000
    }

    fn gear_target(&self) -> bool {
        self.process_gear
            && self.tier == ToastTier::Kitty
            && self.shadow.screen().size().1 >= 4
            && self.shadow.screen().mouse_protocol_mode() != MouseProtocolMode::None
            && self.shadow.screen().mouse_protocol_encoding() == MouseProtocolEncoding::Sgr
    }

    fn render_gear(&mut self) -> Vec<u8> {
        if !self.gear_target() {
            return self.remove_gear();
        }
        let image_id = self.gear_image_id();
        let mut out = Vec::new();
        if !self.gear_drawn {
            let Ok(png) = raster::render_gear_png() else {
                return out;
            };
            out.extend(kitty::transmit_png(image_id, &png));
            self.gear_drawn = true;
        }
        let col = self.shadow.screen().size().1 - 2;
        let mut place = text_tier::cup(0, col).into_bytes();
        place.extend(kitty::place(image_id, 3, 2, 1));
        out.extend(self.with_cursor_parked(place));
        self.input.set_gear(Some(CellRect {
            row: 0,
            col,
            width: 2,
            height: 1,
        }));
        out
    }

    fn remove_gear(&mut self) -> Vec<u8> {
        self.input.set_gear(None);
        if std::mem::take(&mut self.gear_drawn) {
            kitty::delete_image(self.gear_image_id())
        } else {
            Vec::new()
        }
    }

    fn cpu_image_id(&self) -> u32 {
        self.image_id ^ 0x0040_0000
    }

    fn cpu_target(&self) -> bool {
        self.cpu
            .as_ref()
            .is_some_and(|toast| self.cpu_cells(&toast.text).is_some())
    }

    fn render_cpu(&mut self, now: Instant) -> Vec<u8> {
        let Some(toast) = self.cpu.clone() else {
            return self.remove_cpu_drawn();
        };
        let Some((cols, rows)) = self.cpu_cells(&toast.text) else {
            return self.remove_cpu_drawn();
        };
        let (_, term_cols) = self.shadow.screen().size();
        let opacity = self.cpu_opacity(now);
        let content = (toast.text.clone(), toast.severity, cols, rows, opacity);
        let mut out = Vec::new();
        let image_id = self.cpu_image_id();
        if self.cpu_transmitted.as_ref() != Some(&content) {
            let Ok(png) = raster::render_cpu_png(&toast.text, toast.severity, cols, rows, opacity)
            else {
                return Vec::new();
            };
            if self.cpu_transmitted.is_some() {
                out.extend(kitty::delete_image(image_id));
            }
            out.extend(kitty::transmit_png(image_id, &png));
            self.cpu_transmitted = Some(content);
        }
        let col = term_cols.saturating_sub(cols + if self.gear_target() { 3 } else { 1 });
        let mut place = text_tier::cup(0, col).into_bytes();
        place.extend(kitty::place(image_id, kitty::CPU_PLACEMENT_ID, cols, rows));
        out.extend(self.with_cursor_parked(place));
        self.cpu_drawn = true;
        let hover_armed = self.shadow.screen().mouse_protocol_mode()
            == MouseProtocolMode::AnyMotion
            && self.shadow.screen().mouse_protocol_encoding() == MouseProtocolEncoding::Sgr;
        self.input.set_cpu(
            Some(CellRect {
                row: 0,
                col,
                width: cols,
                height: rows,
            }),
            hover_armed,
        );
        out
    }

    fn remove_cpu_drawn(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if std::mem::take(&mut self.cpu_drawn) {
            out.extend(kitty::delete_placement(
                self.cpu_image_id(),
                kitty::CPU_PLACEMENT_ID,
            ));
        }
        self.input.set_cpu(None, false);
        if self.cpu_transmitted.take().is_some() {
            out.extend(kitty::delete_image(self.cpu_image_id()));
        }
        out
    }

    /// Repaint a text toast's cells from the shadow and put the cursor back.
    fn lift_cells(&mut self, rect: CellRect) -> Vec<u8> {
        self.drawn = Drawn::None;
        self.input.set(None);
        self.with_cursor_parked(text_tier::restore_cells(self.shadow.screen(), rect))
    }

    fn draw_kitty(&mut self, toast: &Toast) -> Vec<u8> {
        let (_, term_cols) = self.shadow.screen().size();
        let Some((cols, rows)) = raster::toast_cells(&toast.text, term_cols) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let content = (toast.text.clone(), toast.severity, cols, rows);
        if self.transmitted.as_ref() != Some(&content) {
            let Ok(png) = raster::render_png(&toast.text, toast.severity, cols, rows) else {
                return Vec::new();
            };
            if self.transmitted.is_some() {
                out.extend(kitty::delete_image(self.image_id));
            }
            out.extend(kitty::transmit_png(self.image_id, &png));
            self.transmitted = Some(content);
        }
        let top_row = if self.cpu_target() {
            raster::TOAST_ROWS
        } else {
            0
        };
        let margin = if top_row == 0 && self.gear_target() {
            3
        } else {
            1
        };
        let col = term_cols.saturating_sub(cols + margin);
        let mut place = text_tier::cup(top_row, col).into_bytes();
        place.extend(kitty::place(
            self.image_id,
            kitty::TOAST_PLACEMENT_ID,
            cols,
            rows,
        ));
        out.extend(self.with_cursor_parked(place));
        self.drawn = Drawn::Kitty { cols, rows, col };
        self.arm_close(CellRect {
            row: top_row,
            col: col + cols.saturating_sub(raster::CLOSE_COLS),
            width: raster::CLOSE_COLS,
            height: rows,
        });
        out
    }

    fn draw_cells(&mut self, toast: &Toast) -> Vec<u8> {
        let (rows, cols) = self.shadow.screen().size();
        let Some(rect) = text_tier::toast_rect(&toast.text, rows, cols) else {
            return Vec::new();
        };
        let out = self.with_cursor_parked(text_tier::render_cells(toast, rect));
        self.drawn = Drawn::Cells { rect };
        self.arm_close(text_tier::close_rect(rect));
        out
    }

    fn draw_title(&mut self, toast: &Toast) -> Vec<u8> {
        self.draw_title_text(&toast.text)
    }

    fn draw_usage_title(&mut self) -> Vec<u8> {
        let usage = self.usage.as_ref().expect("usage title has usage");
        self.draw_title_text(&usage_title(usage))
    }

    fn draw_title_text(&mut self, value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.title_pushed {
            out.extend_from_slice(b"\x1b[22;0t");
            self.title_pushed = true;
        }
        let text: String = value.chars().filter(|c| !c.is_control()).collect();
        out.extend_from_slice(format!("\x1b]2;clud \u{b7} {text}\x07").as_bytes());
        self.drawn = Drawn::Title;
        self.input.set(None);
        out
    }

    fn draw_status(&mut self, toast: &Toast) {
        if let Fallback::StatusFile(writer) = &self.fallback {
            let mut status = toast.clone();
            status.key = STATUS_KEY.into();
            writer.publish(ToastEvent::Show(status));
        }
        self.drawn = Drawn::Status;
        self.input.set(None);
    }

    /// Arm click-to-dismiss only when the child already reports SGR mouse
    /// events; clud never enables mouse tracking itself.
    fn arm_close(&self, rect: CellRect) {
        let screen = self.shadow.screen();
        let armed = screen.mouse_protocol_mode() != MouseProtocolMode::None
            && screen.mouse_protocol_encoding() == MouseProtocolEncoding::Sgr;
        self.input.set(armed.then_some(rect));
    }

    /// Wrap toast bytes so the cursor, pen, and visibility the child expects
    /// are exactly as they were.
    fn with_cursor_parked(&self, body: Vec<u8>) -> Vec<u8> {
        let screen = self.shadow.screen();
        let origin = self.tracker.origin_mode();
        let mut out = Vec::with_capacity(body.len() + 32);
        let visible = !screen.hide_cursor();
        if visible {
            out.extend_from_slice(b"\x1b[?25l");
        }
        if origin {
            out.extend_from_slice(b"\x1b[?6l");
        }
        out.extend(body);
        out.extend(screen.attributes_formatted());
        if origin {
            out.extend_from_slice(b"\x1b[?6h");
        }
        let (row, col) = screen.cursor_position();
        let row_param = match (origin, self.tracker.scroll_region()) {
            (true, Some((top, _))) => (row + 1).saturating_sub(top - 1).max(1),
            _ => row + 1,
        };
        out.extend_from_slice(format!("\x1b[{};{}H", row_param, col + 1).as_bytes());
        if visible {
            out.extend_from_slice(b"\x1b[?25h");
        }
        out
    }
}

/// The key the compositor uses for its status-file mirror of the visible
/// toast, so it never collides with a producer's own key.
const STATUS_KEY: &str = "__compositor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Kitty,
    Cells,
    Title,
    Status,
    Nothing,
}

#[cfg(test)]
#[path = "compositor_tests.rs"]
mod tests;
