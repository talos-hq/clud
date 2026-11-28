//! Unix-only tests of `crate::gh_broker`: non-UTF-8 argv and a real `/bin/sh`
//! fake `gh`.

#[cfg(test)]
mod classify_tests {
    use crate::gh_broker::classify::*;
    use std::ffi::OsString;

    #[test]
    fn non_utf8_args_pass_through() {
        use std::os::unix::ffi::OsStringExt;
        let args = vec![
            OsString::from("api"),
            OsString::from_vec(b"repos/\xff".to_vec()),
        ];
        assert_eq!(api_read(&args), None);
    }
}

#[cfg(test)]
mod service_tests {
    use crate::gh_broker::service::*;
    use crate::gh_broker::{ReadReply, ReadRequest};
    use base64::Engine as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// End to end through the real upstream transport: a fake `gh` that prints
    /// `gh api -i` output and answers `If-None-Match` with a 304.
    #[test]
    fn http_handler_runs_gh_api_include_and_revalidates_with_the_etag() {
        use std::os::unix::fs::PermissionsExt;
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let log = dir.path().join("calls");
        let gh = dir.path().join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n\
                 case \"$*\" in *If-None-Match*) printf 'HTTP/2.0 304 Not Modified\\nEtag: \"e1\"\\r\\n\\r\\n'; exit 1;; esac\n\
                 printf 'HTTP/2.0 200 OK\\nContent-Type: application/json\\r\\nEtag: \"e1\"\\r\\n\\r\\n{{\"id\":1}}\\n'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let now = Arc::new(AtomicU64::new(1_000_000));
        let clock_now = Arc::clone(&now);
        let broker = GhBroker::new(
            dir.path().join("b.redb"),
            Box::new(crate::gh_broker::upstream::GhCli),
            Box::new(move || clock_now.load(Ordering::SeqCst)),
        );
        let exe = std::env::current_exe().unwrap();
        let body = serde_json::to_vec(&ReadRequest {
            gh: gh.to_string_lossy().into_owned(),
            endpoint: "repos/o/r/actions/runs/5".into(),
            hostname: None,
            env: vec![],
            session_id: None,
            fresh: false,
            interactive: false,
        })
        .unwrap();
        let decode = |bytes: &[u8]| {
            let reply: ReadReply = serde_json::from_slice(bytes).unwrap();
            base64::engine::general_purpose::STANDARD
                .decode(reply.body_b64)
                .unwrap()
        };
        let (status, first) = broker.handle_http(&body, &exe, None);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&first));
        let (_, cached) = broker.handle_http(&body, &exe, None);
        now.fetch_add(RUNS_TTL_MS, Ordering::SeqCst);
        let (_, revalidated) = broker.handle_http(&body, &exe, None);
        assert_eq!(decode(&first), b"{\"id\":1}\n");
        assert_eq!(decode(&cached), decode(&first));
        assert_eq!(decode(&revalidated), decode(&first));
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                "api -i repos/o/r/actions/runs/5",
                "api -i repos/o/r/actions/runs/5 -H If-None-Match: \"e1\""
            ]
        );
        let outcomes: Vec<_> = broker
            .ledger()
            .unwrap()
            .into_iter()
            .map(|e| e.outcome)
            .collect();
        assert_eq!(outcomes, ["full", "cache", "304"]);
    }
}
