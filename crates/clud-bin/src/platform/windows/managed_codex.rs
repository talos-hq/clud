//! Automatic login startup is currently provided by the Linux user service.

pub fn install() -> Result<&'static str, String> {
    Ok("automatic Codex gateway startup is unavailable on this host; run `clud serve --codex` manually before using clud-codex in Hermes; leave CLUD_SERVE_CLIENT_TOKEN unset to use the saved client token")
}
