//! The broker's durable store: cached responses, the invalidation stamp and
//! the ledger, in one redb file under the daemon state dir (#1743).
//!
//! redb rather than SQLite: clud dropped its bundled SQLite for redb
//! (Cargo.toml, #73/#110), and only the daemon opens this file, which is
//! redb's single-writer model. See DD-150.

use std::path::Path;

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use super::collection::CollectionState;

/// cache key -> JSON [`ObjectMeta`].
const OBJECT_META: TableDefinition<&str, &[u8]> = TableDefinition::new("object_meta");
/// cache key -> raw response body.
const OBJECT_BODY: TableDefinition<&str, &[u8]> = TableDefinition::new("object_body");
/// `invalidated_at_ms` -> stamp of the last possible write.
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// monotonic sequence -> JSON [`LedgerEntry`].
const LEDGER: TableDefinition<u64, &[u8]> = TableDefinition::new("ledger");
/// collection key -> JSON [`CollectionState`] (phase 2 merged reads).
const COLLECTIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("collections");
/// invalidation tag -> stamp of the last write that named it.
const SCOPES: TableDefinition<&str, u64> = TableDefinition::new("scopes");

const INVALIDATED_AT: &str = "invalidated_at_ms";
/// Cached objects kept before the oldest are pruned.
const MAX_OBJECTS: u64 = 4096;
const PRUNE_OBJECTS: usize = 512;
/// Ledger rows kept before the oldest are pruned.
const MAX_LEDGER: u64 = 20_000;
const PRUNE_LEDGER: usize = 2_000;
/// Merged collections kept before the oldest are pruned.
const MAX_COLLECTIONS: u64 = 1024;
const PRUNE_COLLECTIONS: usize = 128;
/// Invalidation tags kept. Pruned tags raise the global stamp to the newest
/// pruned one, so dropping a tag can only make reads more conservative.
const MAX_SCOPES: u64 = 4096;
const PRUNE_SCOPES: usize = 512;

/// Everything about a cached response except its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectMeta {
    /// `<host or "">/<endpoint>`, for humans reading the store.
    pub label: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// When the upstream request that produced or last revalidated this
    /// body *started*, Unix ms. A fetch that began before an invalidation
    /// therefore never counts as fresh after it.
    pub fetched_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub ts_ms: u64,
    pub session_id: Option<String>,
    pub key: String,
    /// `cache`, `304`, `incremental`, `full`, `passthrough` or `error`.
    pub outcome: String,
    pub upstream_requests: u32,
    pub rate_remaining: Option<u64>,
    /// Merged reads: objects the upstream fetch added or changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<u32>,
    /// Merged reads: objects that dropped out of the collection (deleted
    /// upstream) on this refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<u32>,
}

pub struct Store {
    db: Database,
}

fn err(error: impl std::fmt::Display) -> String {
    format!("gh broker store: {error}")
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        let db = Database::create(path).map_err(err)?;
        let txn = db.begin_write().map_err(err)?;
        {
            txn.open_table(OBJECT_META).map_err(err)?;
            txn.open_table(OBJECT_BODY).map_err(err)?;
            txn.open_table(META).map_err(err)?;
            txn.open_table(LEDGER).map_err(err)?;
            txn.open_table(COLLECTIONS).map_err(err)?;
            txn.open_table(SCOPES).map_err(err)?;
        }
        txn.commit().map_err(err)?;
        Ok(Self { db })
    }

    pub fn get(&self, key: &str) -> Result<Option<(ObjectMeta, Vec<u8>)>, String> {
        let txn = self.db.begin_read().map_err(err)?;
        let metas = txn.open_table(OBJECT_META).map_err(err)?;
        let bodies = txn.open_table(OBJECT_BODY).map_err(err)?;
        let Some(meta) = metas.get(key).map_err(err)? else {
            return Ok(None);
        };
        let Some(body) = bodies.get(key).map_err(err)? else {
            return Ok(None);
        };
        let meta: ObjectMeta = serde_json::from_slice(meta.value()).map_err(err)?;
        Ok(Some((meta, body.value().to_vec())))
    }

    /// Insert or replace one object. `body` is `None` to keep the stored
    /// body (a `304` revalidation).
    pub fn put(&self, key: &str, meta: &ObjectMeta, body: Option<&[u8]>) -> Result<(), String> {
        let bytes = serde_json::to_vec(meta).map_err(err)?;
        let txn = self.db.begin_write().map_err(err)?;
        {
            let mut metas = txn.open_table(OBJECT_META).map_err(err)?;
            let mut bodies = txn.open_table(OBJECT_BODY).map_err(err)?;
            metas.insert(key, bytes.as_slice()).map_err(err)?;
            if let Some(body) = body {
                bodies.insert(key, body).map_err(err)?;
            }
            if metas.len().map_err(err)? > MAX_OBJECTS {
                let mut ages = Vec::new();
                for row in metas.iter().map_err(err)? {
                    let (k, v) = row.map_err(err)?;
                    let at = serde_json::from_slice::<ObjectMeta>(v.value())
                        .map(|m| m.fetched_at_ms)
                        .unwrap_or(0);
                    ages.push((at, k.value().to_string()));
                }
                ages.sort();
                for (_, old) in ages.into_iter().take(PRUNE_OBJECTS) {
                    if old != key {
                        metas.remove(old.as_str()).map_err(err)?;
                        bodies.remove(old.as_str()).map_err(err)?;
                    }
                }
            }
        }
        txn.commit().map_err(err)
    }

    pub fn invalidated_at(&self) -> Result<u64, String> {
        let txn = self.db.begin_read().map_err(err)?;
        let meta = txn.open_table(META).map_err(err)?;
        Ok(meta
            .get(INVALIDATED_AT)
            .map_err(err)?
            .map(|v| v.value())
            .unwrap_or(0))
    }

    pub fn invalidate(&self, now_ms: u64) -> Result<(), String> {
        let txn = self.db.begin_write().map_err(err)?;
        {
            let mut meta = txn.open_table(META).map_err(err)?;
            meta.insert(INVALIDATED_AT, now_ms).map_err(err)?;
        }
        txn.commit().map_err(err)
    }

    /// The newest stamp that makes a read with `tags` stale: the global
    /// invalidation or any write that named one of its tags.
    pub fn stale_after(&self, tags: &[String]) -> Result<u64, String> {
        let txn = self.db.begin_read().map_err(err)?;
        let meta = txn.open_table(META).map_err(err)?;
        let scopes = txn.open_table(SCOPES).map_err(err)?;
        let mut newest = meta
            .get(INVALIDATED_AT)
            .map_err(err)?
            .map(|v| v.value())
            .unwrap_or(0);
        for tag in tags {
            if let Some(stamp) = scopes.get(tag.as_str()).map_err(err)? {
                newest = newest.max(stamp.value());
            }
        }
        Ok(newest)
    }

    /// Stamp `tags` with `now_ms`: reads carrying any of them are stale.
    pub fn invalidate_tags(&self, tags: &[String], now_ms: u64) -> Result<(), String> {
        let txn = self.db.begin_write().map_err(err)?;
        {
            let mut scopes = txn.open_table(SCOPES).map_err(err)?;
            for tag in tags {
                scopes.insert(tag.as_str(), now_ms).map_err(err)?;
            }
            if scopes.len().map_err(err)? > MAX_SCOPES {
                let mut ages = Vec::new();
                for row in scopes.iter().map_err(err)? {
                    let (k, v) = row.map_err(err)?;
                    ages.push((v.value(), k.value().to_string()));
                }
                ages.sort();
                let pruned: Vec<_> = ages.into_iter().take(PRUNE_SCOPES).collect();
                let newest_pruned = pruned.iter().map(|(at, _)| *at).max().unwrap_or(0);
                for (_, tag) in &pruned {
                    scopes.remove(tag.as_str()).map_err(err)?;
                }
                let mut meta = txn.open_table(META).map_err(err)?;
                let global = meta
                    .get(INVALIDATED_AT)
                    .map_err(err)?
                    .map(|v| v.value())
                    .unwrap_or(0);
                meta.insert(INVALIDATED_AT, global.max(newest_pruned))
                    .map_err(err)?;
            }
        }
        txn.commit().map_err(err)
    }

    pub fn collection(&self, key: &str) -> Result<Option<CollectionState>, String> {
        let txn = self.db.begin_read().map_err(err)?;
        let table = txn.open_table(COLLECTIONS).map_err(err)?;
        let Some(row) = table.get(key).map_err(err)? else {
            return Ok(None);
        };
        serde_json::from_slice(row.value()).map(Some).map_err(err)
    }

    pub fn put_collection(&self, key: &str, state: &CollectionState) -> Result<(), String> {
        let bytes = serde_json::to_vec(state).map_err(err)?;
        let txn = self.db.begin_write().map_err(err)?;
        {
            let mut table = txn.open_table(COLLECTIONS).map_err(err)?;
            table.insert(key, bytes.as_slice()).map_err(err)?;
            if table.len().map_err(err)? > MAX_COLLECTIONS {
                let mut ages = Vec::new();
                for row in table.iter().map_err(err)? {
                    let (k, v) = row.map_err(err)?;
                    let at = serde_json::from_slice::<CollectionState>(v.value())
                        .map(|s| s.fetched_at_ms)
                        .unwrap_or(0);
                    ages.push((at, k.value().to_string()));
                }
                ages.sort();
                for (_, old) in ages.into_iter().take(PRUNE_COLLECTIONS) {
                    if old != key {
                        table.remove(old.as_str()).map_err(err)?;
                    }
                }
            }
        }
        txn.commit().map_err(err)
    }

    pub fn append_ledger(&self, entry: &LedgerEntry) -> Result<(), String> {
        let bytes = serde_json::to_vec(entry).map_err(err)?;
        let mut txn = self.db.begin_write().map_err(err)?;
        // The ledger is diagnostics: no fsync on the read path.
        txn.set_durability(redb::Durability::Eventual);
        {
            let mut ledger = txn.open_table(LEDGER).map_err(err)?;
            let next = ledger
                .last()
                .map_err(err)?
                .map(|(k, _)| k.value() + 1)
                .unwrap_or(0);
            ledger.insert(next, bytes.as_slice()).map_err(err)?;
            if ledger.len().map_err(err)? > MAX_LEDGER {
                let mut old = Vec::new();
                for row in ledger.iter().map_err(err)?.take(PRUNE_LEDGER) {
                    old.push(row.map_err(err)?.0.value());
                }
                for seq in old {
                    ledger.remove(seq).map_err(err)?;
                }
            }
        }
        txn.commit().map_err(err)
    }

    pub fn ledger(&self) -> Result<Vec<LedgerEntry>, String> {
        let txn = self.db.begin_read().map_err(err)?;
        let ledger = txn.open_table(LEDGER).map_err(err)?;
        let mut out = Vec::new();
        for row in ledger.iter().map_err(err)? {
            let (_, v) = row.map_err(err)?;
            out.push(serde_json::from_slice(v.value()).map_err(err)?);
        }
        Ok(out)
    }

    /// The newest `limit` ledger rows, oldest first. A row that does not
    /// parse is skipped, so one bad row never hides the rest.
    pub fn ledger_tail(&self, limit: usize) -> Result<Vec<LedgerEntry>, String> {
        let txn = self.db.begin_read().map_err(err)?;
        let ledger = txn.open_table(LEDGER).map_err(err)?;
        let mut out = Vec::new();
        for row in ledger.iter().map_err(err)?.rev().take(limit) {
            let (_, v) = row.map_err(err)?;
            if let Ok(entry) = serde_json::from_slice(v.value()) {
                out.push(entry);
            }
        }
        out.reverse();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(at: u64) -> ObjectMeta {
        ObjectMeta {
            label: "/repos/o/r".into(),
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            etag: Some("\"e1\"".into()),
            last_modified: None,
            fetched_at_ms: at,
        }
    }

    #[test]
    fn objects_survive_reopen_and_a_304_keeps_the_body() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("b.redb");
        {
            let store = Store::open(&path).unwrap();
            store.put("k", &meta(1), Some(b"{\"a\":1}")).unwrap();
            store.put("k", &meta(2), None).unwrap();
            store.invalidate(7).unwrap();
        }
        let store = Store::open(&path).unwrap();
        let (m, body) = store.get("k").unwrap().unwrap();
        assert_eq!(m.fetched_at_ms, 2);
        assert_eq!(body, b"{\"a\":1}");
        assert_eq!(store.invalidated_at().unwrap(), 7);
        assert!(store.get("missing").unwrap().is_none());
    }

    #[test]
    fn ledger_appends_in_order() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let store = Store::open(&dir.path().join("b.redb")).unwrap();
        for outcome in ["full", "cache", "304"] {
            store
                .append_ledger(&LedgerEntry {
                    ts_ms: 1,
                    session_id: None,
                    key: "k".into(),
                    outcome: outcome.into(),
                    upstream_requests: 0,
                    rate_remaining: None,
                    changed: None,
                    removed: None,
                })
                .unwrap();
        }
        let outcomes: Vec<_> = store
            .ledger()
            .unwrap()
            .into_iter()
            .map(|e| e.outcome)
            .collect();
        assert_eq!(outcomes, ["full", "cache", "304"]);
    }

    #[test]
    fn tag_stamps_raise_staleness_and_collections_round_trip() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let store = Store::open(&dir.path().join("b.redb")).unwrap();
        let tags = vec!["o/r#num:5".to_string(), "*#num:5".to_string()];
        assert_eq!(store.stale_after(&tags).unwrap(), 0);
        store.invalidate(10).unwrap();
        store.invalidate_tags(&["*#num:5".to_string()], 20).unwrap();
        assert_eq!(store.stale_after(&tags).unwrap(), 20);
        assert_eq!(store.stale_after(&["run:7".to_string()]).unwrap(), 10);
        let state = CollectionState {
            width: 7,
            fetched_at_ms: 3,
            ..CollectionState::default()
        };
        store.put_collection("c", &state).unwrap();
        assert_eq!(store.collection("c").unwrap(), Some(state));
        assert_eq!(store.collection("missing").unwrap(), None);
    }

    #[test]
    fn pruned_tags_fold_into_the_global_stamp() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let store = Store::open(&dir.path().join("b.redb")).unwrap();
        let old: Vec<String> = (0..MAX_SCOPES).map(|i| format!("run:{i}")).collect();
        store.invalidate_tags(&old, 100).unwrap();
        assert_eq!(store.stale_after(&[]).unwrap(), 0);
        store
            .invalidate_tags(&["run:new".to_string()], 200)
            .unwrap();
        // Pruned tags are as stale as the newest pruned stamp: a read never
        // turns fresher because its tag was dropped.
        assert_eq!(store.stale_after(&[]).unwrap(), 100);
        assert_eq!(store.stale_after(&["run:0".to_string()]).unwrap(), 100);
        assert_eq!(store.stale_after(&["run:new".to_string()]).unwrap(), 200);
    }
}
