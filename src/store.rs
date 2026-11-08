use crate::{Error, Result, json::digest};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub(crate) mod goals;
mod leases;
mod migration;

pub(crate) const MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) struct Store {
    conn: Mutex<Connection>,
    root: PathBuf,
    _lock: File,
    global_slots: std::sync::atomic::AtomicUsize,
}
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub id: String,
    pub owner: String,
    pub revision: u64,
    pub queue_order: u64,
    pub body: Value,
}
#[derive(Debug, Clone)]
pub(crate) struct CommandReceipt {
    pub kind: String,
    pub key: String,
    pub fingerprint: String,
    pub response: Value,
    pub not_after_ms: Option<u64>,
}
pub(crate) enum Commit {
    Saved(Row),
    Replayed(Value),
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_mode(root, false, 1)
    }
    pub(crate) fn open_mode(root: &Path, orchestrated: bool, global_slots: usize) -> Result<Self> {
        if global_slots == 0 || global_slots > i64::MAX as usize {
            return Err(Error::new(
                "input_invalid",
                "/max_concurrent_runs",
                "invalid global slot capacity",
            ));
        }
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        let root = root.canonicalize()?;
        if std::fs::metadata(&root)?.permissions().mode() & 0o077 != 0 {
            return Err(Error::new(
                "capability_denied",
                "/store",
                "store directory must be private to its owner (0700)",
            ));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("owner.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|_| {
            Error::new(
                "already_running",
                "/store",
                "another TaskLoop host owns this store",
            )
        })?;
        std::fs::create_dir_all(root.join("blobs"))?;
        std::fs::create_dir_all(root.join("runs"))?;
        let mut conn = Connection::open(root.join("state.sqlite"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migration::initialize(&mut conn, orchestrated)?;
        Ok(Self {
            conn: Mutex::new(conn),
            root,
            _lock: lock,
            global_slots: std::sync::atomic::AtomicUsize::new(global_slots),
        })
    }
    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| Error::io("store mutex poisoned"))
    }
    fn next_queue(conn: &Connection) -> Result<u64> {
        conn.execute("UPDATE meta SET value=value+1 WHERE key='queue'", [])?;
        let n: i64 =
            conn.query_row("SELECT value FROM meta WHERE key='queue'", [], |r| r.get(0))?;
        u64::try_from(n).map_err(|_| Error::io("negative queue counter"))
    }
    fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, i64, i64, String)> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    }
    fn decode(
        (id, owner, revision, queue_order, body): (String, String, i64, i64, String),
    ) -> Result<Row> {
        Ok(Row {
            id,
            owner,
            revision: u64::try_from(revision).map_err(|_| Error::io("negative revision"))?,
            queue_order: u64::try_from(queue_order)
                .map_err(|_| Error::io("negative queue position"))?,
            body: serde_json::from_str(&body)?,
        })
    }
    pub fn load(&self, id: &str) -> Result<Row> {
        let c = self.connection()?;
        let r = c
            .query_row(
                "SELECT id,owner,revision,queue_order,body FROM executions WHERE id=?1",
                [id],
                Self::row,
            )
            .optional()?;
        Self::decode(r.ok_or_else(|| Error::new("not_found", "/execution", "execution not found"))?)
    }
    pub fn all(&self) -> Result<Vec<Row>> {
        let c = self.connection()?;
        let mut st = c.prepare(
            "SELECT id,owner,revision,queue_order,body FROM executions WHERE json_extract(body,'$.status') NOT IN ('completed','failed','cancelled') ORDER BY queue_order,id",
        )?;
        let rows = st
            .query_map([], Self::row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter().map(Self::decode).collect()
    }
    pub fn create(
        &self,
        id: &str,
        owner: &str,
        key: &str,
        fingerprint: &str,
        body: &Value,
        capacity: usize,
    ) -> Result<(Row, bool)> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT id,fingerprint FROM executions WHERE owner=?1 AND request_key=?2",
                params![owner, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((existing, fp)) = prior {
            if fp != fingerprint {
                return Err(Error::new(
                    "request_conflict",
                    "/request_key",
                    "same key has different request",
                ));
            }
            let row = Self::decode(tx.query_row(
                "SELECT id,owner,revision,queue_order,body FROM executions WHERE id=?1",
                [existing],
                Self::row,
            )?)?;
            tx.commit()?;
            return Ok((row, false));
        }
        let count:i64=tx.query_row("SELECT COUNT(*) FROM executions WHERE json_extract(body,'$.status') NOT IN ('completed','failed','cancelled')",[],|r|r.get(0))?;
        if count >= capacity as i64 {
            return Err(Error::new(
                "admission_full",
                "/execution",
                "admission limit reached",
            ));
        }
        let queue = Self::next_queue(&tx)?;
        tx.execute(
            "INSERT INTO executions VALUES(?1,?2,?3,?4,0,?5,?6)",
            params![
                id,
                owner,
                key,
                fingerprint,
                queue as i64,
                serde_json::to_string(body)?
            ],
        )?;
        leases::sync_flow(&tx, id, body)?;
        tx.commit()?;
        Ok((
            Row {
                id: id.into(),
                owner: owner.into(),
                revision: 0,
                queue_order: queue,
                body: body.clone(),
            },
            true,
        ))
    }
    pub fn command(&self, id: &str, kind: &str, key: &str, fp: &str) -> Result<Option<Value>> {
        let c = self.connection()?;
        let prior:Option<(String,String)>=c.query_row("SELECT fingerprint,response FROM commands WHERE execution_id=?1 AND kind=?2 AND request_key=?3",params![id,kind,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        match prior {
            Some((old, response)) => {
                if old != fp {
                    return Err(Error::new(
                        "request_conflict",
                        "/request_key",
                        "same command key has different content",
                    ));
                }
                Ok(Some(serde_json::from_str(&response)?))
            }
            None => Ok(None),
        }
    }
    pub fn commit(
        &self,
        row: &Row,
        body: &Value,
        enqueue: bool,
        command: Option<CommandReceipt>,
        artifacts: &[crate::ArtifactRef],
    ) -> Result<Commit> {
        let mut c = self.connection()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(ref cmd) = command {
            let prior:Option<(String,String)>=tx.query_row("SELECT fingerprint,response FROM commands WHERE execution_id=?1 AND kind=?2 AND request_key=?3",params![row.id,cmd.kind,cmd.key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((fp, response)) = prior {
                if fp != cmd.fingerprint {
                    return Err(Error::new(
                        "request_conflict",
                        "/request_key",
                        "same command key has different content",
                    ));
                }
                let v = serde_json::from_str(&response)?;
                tx.commit()?;
                return Ok(Commit::Replayed(v));
            }
        }
        if command
            .as_ref()
            .and_then(|c| c.not_after_ms)
            .is_some_and(|deadline| crate::runtime::now_ms() >= deadline)
        {
            return Err(Error::new(
                "signal_expired",
                "/signal",
                "waiting deadline passed before transaction commit",
            ));
        }
        let queue = if enqueue {
            Self::next_queue(&tx)?
        } else {
            row.queue_order
        };
        let revision = row.revision + 1;
        let changed = tx.execute(
            "UPDATE executions SET revision=?1,queue_order=?2,body=?3 WHERE id=?4 AND revision=?5",
            params![
                revision as i64,
                queue as i64,
                serde_json::to_string(body)?,
                row.id,
                row.revision as i64
            ],
        )?;
        if changed != 1 {
            return Err(Error::new(
                "revision_conflict",
                "/revision",
                "instance changed; refresh snapshot",
            ));
        }
        leases::sync_flow(&tx, &row.id, body)?;
        for a in artifacts {
            tx.execute(
                "INSERT INTO artifacts VALUES(?1,?2,?3,?4)",
                params![a.id, row.id, a.sha256, serde_json::to_string(a)?],
            )?;
        }
        if let Some(cmd) = command {
            tx.execute(
                "INSERT INTO commands VALUES(?1,?2,?3,?4,?5)",
                params![
                    row.id,
                    cmd.kind,
                    cmd.key,
                    cmd.fingerprint,
                    serde_json::to_string(&cmd.response)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(Commit::Saved(Row {
            id: row.id.clone(),
            owner: row.owner.clone(),
            revision,
            queue_order: queue,
            body: body.clone(),
        }))
    }
    pub fn write_blob(&self, raw: &[u8]) -> Result<String> {
        if raw.len() as u64 > MAX_BLOB_BYTES {
            return Err(Error::new(
                "artifact_invalid",
                "/artifact",
                "artifact exceeds storage limit",
            ));
        }
        let hash = digest(raw);
        let path = self.root.join("blobs").join(&hash);
        // The final digest path must never name a partially written file.
        let pending = self
            .root
            .join("blobs")
            .join(format!(".pending-{}", uuid::Uuid::new_v4()));
        let publish = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&pending)?;
            file.write_all(raw)?;
            file.sync_all()?;
            match std::fs::hard_link(&pending, &path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if self.read_blob(&hash)? != raw {
                        return Err(Error::io("content addressed blob mismatch"));
                    }
                }
                Err(e) => return Err(e.into()),
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(&pending);
        publish?;
        File::open(self.root.join("blobs"))?.sync_all()?;
        Ok(hash)
    }
    pub fn read_blob(&self, hash: &str) -> Result<Vec<u8>> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::new(
                "artifact_invalid",
                "/artifact",
                "invalid content digest",
            ));
        }
        let raw = crate::definition::safe_read(&self.root.join("blobs"), hash, MAX_BLOB_BYTES)
            .map_err(|_| Error::io("locked artifact bytes unavailable"))?;
        if digest(&raw) != hash {
            return Err(Error::io("immutable artifact corrupted"));
        }
        Ok(raw)
    }
    pub fn artifact(&self, id: &str) -> Result<(String, crate::ArtifactRef)> {
        let c = self.connection()?;
        let pair: Option<(String, String)> = c
            .query_row(
                "SELECT execution_id,metadata FROM artifacts WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (execution, metadata) = pair
            .ok_or_else(|| Error::new("not_found", "/artifact", "accepted artifact not found"))?;
        Ok((execution, serde_json::from_str(&metadata)?))
    }
    pub fn save_definition(&self, definition: &crate::Definition) -> Result<()> {
        let c = self.connection()?;
        c.execute(
            "INSERT OR IGNORE INTO definitions VALUES(?1,?2)",
            params![definition.digest, serde_json::to_string(definition)?],
        )?;
        Ok(())
    }
    pub fn definition(&self, digest: &str) -> Result<crate::Definition> {
        let c = self.connection()?;
        let body: Option<String> = c
            .query_row(
                "SELECT body FROM definitions WHERE digest=?1",
                [digest],
                |r| r.get(0),
            )
            .optional()?;
        let definition: crate::Definition = serde_json::from_str(&body.ok_or_else(|| {
            Error::new(
                "definition_invalid",
                "/definition",
                "locked definition unavailable",
            )
        })?)
        .map_err(Error::io)?;
        definition.verify_integrity()?;
        if definition.digest != digest {
            return Err(Error::io("definition identity mismatch"));
        }
        Ok(definition)
    }
    pub fn run_dir(&self, nonce: &str) -> Result<PathBuf> {
        if !nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(Error::new("artifact_invalid", "/run", "invalid run nonce"));
        }
        let p = self.root.join("runs").join(nonce);
        std::fs::create_dir_all(&p)?;
        Ok(p)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn blob_read_and_write_share_the_exact_size_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("store")).unwrap();
        let mut bytes = vec![42; MAX_BLOB_BYTES as usize];
        let hash = store.write_blob(&bytes).unwrap();
        assert_eq!(store.read_blob(&hash).unwrap(), bytes);
        // Re-publication is the path used when accepted candidates already exist.
        assert_eq!(store.write_blob(&bytes).unwrap(), hash);
        bytes.push(42);
        assert_eq!(
            store.write_blob(&bytes).unwrap_err().code,
            "artifact_invalid"
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("store/blobs"))
                .unwrap()
                .count(),
            1
        );
    }
    #[test]
    fn readers_never_observe_partial_content_addressed_files() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("store");
        let s = Store::open(&root).unwrap();
        let bytes = vec![42u8; 32 * 1024 * 1024];
        let hash = digest(&bytes);
        let path = root.join("blobs").join(&hash);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let reader_barrier = barrier.clone();
        let reader = std::thread::spawn(move || {
            reader_barrier.wait();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Ok(meta) = std::fs::metadata(&path) {
                    break meta.len();
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "blob was never published"
                );
                std::thread::yield_now();
            }
        });
        barrier.wait();
        assert_eq!(s.write_blob(&bytes).unwrap(), hash);
        assert_eq!(
            reader.join().unwrap(),
            bytes.len() as u64,
            "final digest path became visible before complete publication"
        );
    }
    #[test]
    fn refuses_public_store_without_changing_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("public");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(Store::open(&root).err().unwrap().code, "capability_denied");
        assert_eq!(
            std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    #[test]
    fn expired_signal_cannot_commit_and_replay_survives_deadline() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(&t.path().join("store")).unwrap();
        let (row, _) = s
            .create("a", "u", "start", "fp", &json!({"status":"waiting"}), 8)
            .unwrap();
        let expired = CommandReceipt {
            kind: "signal".into(),
            key: "late".into(),
            fingerprint: "one".into(),
            response: json!({"ok":true}),
            not_after_ms: Some(1),
        };
        assert_eq!(
            s.commit(
                &row,
                &json!({"status":"completed"}),
                false,
                Some(expired),
                &[]
            )
            .err()
            .unwrap()
            .code,
            "signal_expired"
        );
        assert_eq!(s.load("a").unwrap().revision, row.revision);
        let receipt = CommandReceipt {
            kind: "signal".into(),
            key: "accepted".into(),
            fingerprint: "two".into(),
            response: json!({"ok":true}),
            not_after_ms: None,
        };
        s.commit(
            &row,
            &json!({"status":"completed"}),
            false,
            Some(receipt.clone()),
            &[],
        )
        .unwrap();
        let late_replay = CommandReceipt {
            not_after_ms: Some(1),
            ..receipt
        };
        assert!(matches!(
            s.commit(
                &row,
                &json!({"status":"failed"}),
                false,
                Some(late_replay),
                &[]
            )
            .unwrap(),
            Commit::Replayed(_)
        ));
        assert_eq!(s.load("a").unwrap().body["status"], "completed");
    }
    #[test]
    fn rejects_second_host() {
        let t = tempfile::tempdir().unwrap();
        let _a = Store::open(&t.path().join("store")).unwrap();
        assert!(Store::open(&t.path().join("store")).is_err());
    }
    #[test]
    fn idempotency_precedes_capacity_and_cas_rejects_stale() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(&t.path().join("store")).unwrap();
        let (a, new) = s
            .create("a", "u", "one", "fp", &json!({"status":"running"}), 1)
            .unwrap();
        assert!(new);
        assert!(
            !s.create("ignored", "u", "one", "fp", &json!({}), 1)
                .unwrap()
                .1
        );
        assert!(
            s.create("b", "u", "two", "fp", &json!({"status":"running"}), 1)
                .is_err()
        );
        s.commit(&a, &json!({"status":"waiting"}), false, None, &[])
            .unwrap();
        assert!(
            s.commit(&a, &json!({"status":"completed"}), false, None, &[])
                .is_err()
        );
        assert_eq!(s.load("a").unwrap().body["status"], "waiting");
    }
    #[test]
    fn committed_facts_and_blobs_survive_restart() {
        let t = tempfile::tempdir().unwrap();
        let hash;
        {
            let s = Store::open(&t.path().join("store")).unwrap();
            hash = s.write_blob(b"saved content").unwrap();
            s.create(
                "a",
                "u",
                "one",
                "fp",
                &json!({"status":"waiting","artifact":hash}),
                1,
            )
            .unwrap();
        }
        let s = Store::open(&t.path().join("store")).unwrap();
        assert_eq!(s.read_blob(&hash).unwrap(), b"saved content");
        assert_eq!(s.load("a").unwrap().body["artifact"], hash);
    }
}
