use super::*;
use std::cell::RefCell;
use std::path::Path;

/// Records every row it is asked to write; optionally fails.
#[derive(Default)]
pub(crate) struct FakeWriter {
    pub(crate) rows: RefCell<Vec<CreatedEntry>>,
    pub(crate) error: Option<String>,
}

impl LedgerWriter for FakeWriter {
    fn record(&self, entry: &CreatedEntry) -> Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.rows.borrow_mut().push(entry.clone());
        Ok(())
    }
}

pub(crate) fn base() -> (kernal_api::platform::fs::TemporaryDirectory, PathBuf) {
    let tmp = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let base = crate::path_norm::canonicalize_plain(tmp.path()).unwrap();
    (tmp, base)
}

pub(crate) fn request(cwd: &Path, session: Option<&str>) -> Request {
    Request {
        cwd: cwd.to_path_buf(),
        session_id: session.map(String::from),
        role: "agent".into(),
        now_unix: 42,
    }
}
