//! Read-only projections, without Store ownership, migrations, dispatch, or executor construction.
use super::*;
use crate::{
    store::goals::{GoalRow, goal_by_id},
    *,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct GoalClient {
    pub(super) root: PathBuf,
    caller: Caller,
    conn: Mutex<Connection>,
}
impl GoalClient {
    /// `caller` is a trusted host mapping for the local OS user, never a request JSON field.
    pub fn open(store_dir: impl AsRef<Path>, caller: Caller) -> Result<Self> {
        super::core::nonempty(&caller.principal, "/caller")?;
        let root = store_dir.as_ref().canonicalize()?;
        let metadata = std::fs::metadata(&root)?;
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 || metadata.uid() != uid
        {
            return Err(Error::new(
                "capability_denied",
                "/store",
                "GoalClient requires a private Store owned by the current OS user",
            ));
        }
        let db = root.join("state.sqlite");
        let metadata = std::fs::symlink_metadata(&db)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.uid() != uid {
            return Err(Error::new(
                "capability_denied",
                "/store",
                "state.sqlite must be a regular local-owner file",
            ));
        }
        let conn = Connection::open_with_flags(
            db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        crate::store::validate_readonly(&conn)?;
        Ok(Self {
            root,
            caller,
            conn: Mutex::new(conn),
        })
    }
    fn authorized(&self, conn: &Connection, id: &str) -> Result<GoalRow> {
        let row = goal_by_id(conn, id)?;
        if row.owner != self.caller.principal {
            return Err(Error::new("not_found", "/goal", "Goal not found"));
        }
        Ok(row)
    }
    pub fn get(&self, id: &str) -> Result<GoalSnapshot> {
        let mut conn = self.conn.lock().map_err(Error::io)?;
        let tx = conn.transaction()?;
        let snapshot = crate::store::goals::goal_snapshot_from(&tx, id, &self.caller.principal)?;
        tx.commit()?;
        Ok(snapshot)
    }
    pub fn plan(&self, id: &str) -> Result<LockedPlan> {
        let conn = self.conn.lock().map_err(Error::io)?;
        self.authorized(&conn, id)?
            .record
            .plan
            .ok_or_else(|| Error::new("not_ready", "/plan", "Goal plan is not locked"))
    }
    pub fn budgets(&self, id: &str) -> Result<GoalBudgetView> {
        let conn = self.conn.lock().map_err(Error::io)?;
        let record = self.authorized(&conn, id)?.record;
        Ok(GoalBudgetView {
            protocol_version: 1,
            goal_id: id.into(),
            available: record.budget.available(&record.prepared.effective_limits),
            budget: record.budget,
            effective_limits: record.prepared.effective_limits,
            sources: record.prepared.limit_sources,
        })
    }
    pub fn read_artifact(&self, id: &str) -> Result<ArtifactData> {
        let conn = self.conn.lock().map_err(Error::io)?;
        let raw: Option<String> = conn.query_row("SELECT a.metadata FROM goal_artifacts a JOIN goals g ON g.id=a.goal_id WHERE a.id=?1 AND g.owner=?2", params![id,self.caller.principal], |r| r.get(0)).optional()?;
        let artifact: GoalArtifact = serde_json::from_str(
            &raw.ok_or_else(|| Error::new("not_found", "/artifact", "artifact not found"))?,
        )?;
        Ok(ArtifactData {
            bytes: self.read_blob(&artifact.artifact.sha256)?.into(),
            reference: artifact.artifact,
        })
    }
    pub fn evaluations(&self, id: &str) -> Result<Vec<EvaluationReceipt>> {
        let conn = self.conn.lock().map_err(Error::io)?;
        self.authorized(&conn, id)?
            .record
            .checks
            .values()
            .filter_map(|c| c.receipt_sha.as_ref())
            .map(|sha| serde_json::from_slice(&self.read_blob(sha)?).map_err(Error::io))
            .collect()
    }
    pub fn trace(&self, id: &str, query: TraceQuery) -> Result<TracePage> {
        let conn = self.conn.lock().map_err(Error::io)?;
        self.authorized(&conn, id)?;
        crate::trace::readonly::read(&self.root, id, &query)
    }
    pub fn export_trace(&self, id: &str, query: TraceQuery) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&self.trace(id, query)?)?)
    }
    fn read_blob(&self, sha: &str) -> Result<Vec<u8>> {
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::new(
                "artifact_invalid",
                "/artifact",
                "invalid content digest",
            ));
        }
        let bytes = crate::definition::safe_read(
            &self.root.join("blobs"),
            sha,
            crate::store::MAX_BLOB_BYTES,
        )?;
        if crate::json::digest(&bytes) != sha {
            return Err(Error::io("immutable artifact corrupted"));
        }
        Ok(bytes)
    }
}
