//! A native window with password widgets, including masked multiline paste.
//! Persistence is disabled in the dependency; secret buffers are zeroized.

use std::cell::RefCell;
use std::rc::Rc;

use eframe::egui;
use zeroize::Zeroizing;

use super::{Failure, Options, Secret};

pub(super) fn prompt(options: &Options) -> Result<Secret, Failure> {
    match native_prompt(options) {
        Err(Failure::Unavailable) if !options.multiline && !super::test_mode() => {
            external_fallback(options)
        }
        other => other,
    }
}

/// #2197: zenity/kdialog/ksshaskpass/pinentry when the native window can't load.
fn external_fallback(options: &Options) -> Result<Secret, Failure> {
    use crate::platform::secret_prompt::{external_prompt, ExternalFailure};
    external_prompt(&options.title, &options.label).map_err(|error| match error {
        ExternalFailure::Cancelled => Failure::Cancelled,
        ExternalFailure::Unavailable => Failure::Unavailable,
    })
}

fn native_prompt(options: &Options) -> Result<Secret, Failure> {
    if !crate::platform::secret_prompt::graphics_available() {
        return Err(Failure::Unavailable);
    }
    let outcome = Rc::new(RefCell::new(None));
    let returned = Rc::clone(&outcome);
    let options = options.clone();
    let title = options.title.clone();
    let fixture = if super::test_mode() && std::env::var_os("CLUD_TEST_SECRET_PROMPT_GUI").is_some()
    {
        std::env::var_os("CLUD_TEST_SECRET_PROMPT_FILE")
            .and_then(|path| std::fs::read_to_string(path).ok())
    } else {
        None
    };
    let automate = fixture.is_some();
    let mut secret = Zeroizing::new(fixture.unwrap_or_default());
    let mut first_frame = true;
    let mut painted = false;
    let mut native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, if options.multiline { 300.0 } else { 180.0 }])
            .with_resizable(false),
        ..Default::default()
    };
    crate::platform::secret_prompt::configure(&mut native);
    let result = eframe::run_ui_native(&title, native, move |ui, _frame| {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.label(&options.label);
            ui.label(destination_label(&options));
            let response = masked_editor(ui, &mut secret, options.multiline);
            if first_frame {
                response.request_focus();
                first_frame = false;
            }
            let auto_submit = automate && painted;
            if automate && !painted {
                painted = true;
                ui.ctx().request_repaint();
            }
            let enter = !options.multiline && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let mut accepted = (enter || auto_submit) && !secret.is_empty();
            let mut cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
            ui.horizontal(|ui| {
                accepted |= ui
                    .add_enabled(!secret.is_empty(), egui::Button::new("Store"))
                    .clicked();
                cancelled |= ui.button("Cancel").clicked();
            });
            if cancelled || accepted {
                if accepted && !cancelled {
                    *returned.borrow_mut() = Some(Zeroizing::new(std::mem::take(&mut *secret)));
                }
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    });
    if let Err(error) = result {
        if super::test_mode() && std::env::var_os("CLUD_TEST_SECRET_PROMPT_GUI").is_some() {
            eprintln!("native window test initialization failed: {error}");
        }
        return Err(Failure::Unavailable);
    }
    let value = outcome.borrow_mut().take();
    value.ok_or(Failure::Cancelled)
}

fn destination_label(options: &Options) -> String {
    if let Some(path) = &options.to_file {
        format!("Store in {}", path.display())
    } else if let Some(command) = options.to_command.first() {
        format!("Send to {command}")
    } else if options.vault.is_some() {
        "Store in the native credential vault".to_string()
    } else {
        "Return to the requesting authentication tool".to_string()
    }
}

fn masked_editor(ui: &mut egui::Ui, secret: &mut String, multiline: bool) -> egui::Response {
    let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, width: f32| {
        ui.fonts_mut(|fonts| {
            fonts.layout(
                masked_lines(text.as_str()),
                egui::TextStyle::Monospace.resolve(ui.style()),
                ui.visuals().text_color(),
                width,
            )
        })
    };
    let edit = if multiline {
        egui::TextEdit::multiline(secret).desired_rows(7)
    } else {
        egui::TextEdit::singleline(secret)
    };
    ui.add(
        edit.password(true)
            .layouter(&mut layouter)
            .char_limit(1024 * 1024)
            .desired_width(f32::INFINITY),
    )
}

fn masked_lines(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character == '\n' {
                '\n'
            } else {
                egui::epaint::text::PASSWORD_REPLACEMENT_CHAR
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_multiline_layout_preserves_only_line_boundaries() {
        let masked = masked_lines("private\nkey\n");
        assert_eq!(masked.lines().count(), 2);
        assert_eq!(
            masked
                .chars()
                .filter(|character| *character == '\n')
                .count(),
            2
        );
        assert!(!masked.contains("private"));
        assert!(!masked.contains("key"));
    }

    #[test]
    fn password_rendering_never_contains_the_single_or_multiline_secret() {
        for multiline in [false, true] {
            let context = egui::Context::default();
            let mut secret = "SENTINEL_CREDENTIAL\nPRIVATE_KEY".to_string();
            let mut output = context.run_ui(egui::RawInput::default(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    masked_editor(ui, &mut secret, multiline);
                });
            });
            let rendered = format!("{:?}", output.shapes);
            assert!(!rendered.contains("SENTINEL_CREDENTIAL"));
            assert!(!rendered.contains("PRIVATE_KEY"));
            // This headless rendering probe has no graphics backend to apply
            // texture updates; acknowledge them before dropping FullOutput.
            output.textures_delta.clear();
        }
    }
}
