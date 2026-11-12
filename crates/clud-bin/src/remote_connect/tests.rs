//! The command advertised by clud.bot must be consumed by clud itself.
use crate::args::Args;

#[test]
fn advertised_remote_connect_is_not_backend_passthrough() {
    let args = Args::try_parse_from(["clud", "remote", "connect", "clud.bot"]);
    assert!(args.is_ok(), "advertised command must parse: {args:?}");
    let args = Args::parse_from_raw(
        ["clud", "remote", "connect", "clud.bot"]
            .map(str::to_owned)
            .to_vec(),
    );
    assert!(args.command.is_some());
    assert!(args.passthrough.is_empty());
}

#[test]
fn sanitize_open_url_accepts_https_and_loopback_only() {
    use super::sanitize_open_url;
    // Accepted: https with a host, and loopback http for local development.
    assert_eq!(
        sanitize_open_url("https://clud.bot/connect"),
        Ok("https://clud.bot/connect".to_owned())
    );
    assert_eq!(
        sanitize_open_url("  https://docs.example.com  "),
        Ok("https://docs.example.com".to_owned())
    );
    assert!(sanitize_open_url("http://localhost:8080/x").is_ok());
    assert!(sanitize_open_url("http://127.0.0.1:3000").is_ok());
    assert!(sanitize_open_url("http://[::1]:5000/ui").is_ok());
    // Refused: local files, scripts, unencrypted remote origins, bare paths.
    assert!(sanitize_open_url("file:///etc/passwd").is_err());
    assert!(sanitize_open_url("javascript:alert(1)").is_err());
    assert!(sanitize_open_url("data:text/html,<script>").is_err());
    assert!(sanitize_open_url("http://evil.example.com").is_err());
    assert!(sanitize_open_url("/etc/passwd").is_err());
    assert!(sanitize_open_url("https://").is_err());
    assert!(sanitize_open_url("https://user:pw@").is_err());
}
