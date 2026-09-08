//! Transactional persistent cache for physical workspace inputs.
//!
//! Roots are deliberately absent from the stored identity: they define the
//! current discovery order and base/workspace role, while the expensive scan
//! result belongs to a canonical physical input. One SQLite database provides
//! batched page I/O, atomic updates, and safe sharing between editor windows.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, TransactionBehavior};
use zerosyntax_analysis::Analyzer;

const STORE_FILE: &str = "index-v1.sqlite3";
const STORE_VERSION: i64 = 1;
/// Bump when `CachedEntry` serialization or any extractor feeding it changes.
/// SQL layout changes instead bump `STORE_VERSION` and the store filename.
const PRODUCER_ABI: &[u8] = b"zerosyntax-physical-input-v3";
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const TOUCH_INTERVAL_SECS: i64 = 24 * 60 * 60;
const MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;
const MAX_PAYLOAD_BYTES: i64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub(crate) len: u64,
    pub(crate) modified_secs: u64,
    pub(crate) modified_nanos: u32,
}

#[derive(Clone)]
struct PendingRecord {
    path: String,
    fingerprint: Fingerprint,
    payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommitOutcome {
    /// At least one analyzed payload was inserted, replaced, or invalidated.
    pub(crate) updated: bool,
    pub(crate) pruned: usize,
}

/// A scan-scoped view of the persistent store.
///
/// The captured epoch prevents a scan that started before `clear()` from
/// repopulating the store after the clear transaction commits.
pub(crate) struct InputCache {
    connection: Connection,
    cache_dir: PathBuf,
    producer: [u8; 32],
    epoch: i64,
    now: i64,
    pending: Vec<PendingRecord>,
    touches: HashSet<String>,
    invalid: HashSet<String>,
}

pub(crate) fn cache_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(STORE_FILE)
}

pub(crate) fn producer_id(analyzer: &Analyzer) -> Result<[u8; 32]> {
    let schema = serde_json::to_vec(analyzer.schema()).context("failed to serialize schema")?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(PRODUCER_ABI);
    hasher.update(&(schema.len() as u64).to_le_bytes());
    hasher.update(&schema);
    Ok(*hasher.finalize().as_bytes())
}

impl InputCache {
    pub(crate) fn open(cache_dir: &Path, producer: [u8; 32]) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("failed to create cache directory {}", cache_dir.display()))?;
        let connection = open_recovering(cache_dir)?;
        let epoch = connection.query_row(
            "SELECT epoch FROM cache_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(Self {
            connection,
            cache_dir: cache_dir.to_path_buf(),
            producer,
            epoch,
            now: unix_now(),
            pending: Vec::new(),
            touches: HashSet::new(),
            invalid: HashSet::new(),
        })
    }

    pub(crate) fn lookup(
        &mut self,
        path: &str,
        fingerprint: &Fingerprint,
    ) -> Result<Option<Vec<u8>>> {
        let row = self
            .connection
            .prepare_cached(
                "SELECT payload, last_used
                   FROM input_cache
                  WHERE producer = ?1 AND path = ?2
                    AND file_len = ?3 AND modified_secs = ?4 AND modified_nanos = ?5",
            )?
            .query_row(
                params![
                    self.producer.as_slice(),
                    path,
                    as_sql_int(fingerprint.len)?,
                    as_sql_int(fingerprint.modified_secs)?,
                    i64::from(fingerprint.modified_nanos),
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((payload, last_used)) = row else {
            return Ok(None);
        };
        if last_used < self.now - TOUCH_INTERVAL_SECS {
            self.touches.insert(path.to_owned());
        }
        Ok(Some(payload))
    }

    pub(crate) fn invalidate(&mut self, path: &str) {
        self.invalid.insert(path.to_owned());
    }

    pub(crate) fn queue_store(&mut self, path: String, fingerprint: Fingerprint, payload: Vec<u8>) {
        self.pending.push(PendingRecord {
            path,
            fingerprint,
            payload,
        });
    }

    pub(crate) fn has_pending_writes(&self) -> bool {
        !self.pending.is_empty() || !self.invalid.is_empty()
    }

    pub(crate) fn commit(&mut self) -> Result<CommitOutcome> {
        let payload_updated = self.has_pending_writes();
        if !payload_updated && self.touches.is_empty() {
            return Ok(CommitOutcome::default());
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_epoch: i64 = transaction.query_row(
            "SELECT epoch FROM cache_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if current_epoch != self.epoch {
            transaction.commit()?;
            self.pending.clear();
            self.touches.clear();
            self.invalid.clear();
            return Ok(CommitOutcome::default());
        }

        {
            let mut invalidate = transaction
                .prepare_cached("DELETE FROM input_cache WHERE producer = ?1 AND path = ?2")?;
            for path in &self.invalid {
                invalidate.execute(params![self.producer.as_slice(), path])?;
            }
        }
        {
            let mut touch = transaction.prepare_cached(
                "UPDATE input_cache SET last_used = ?1
                  WHERE producer = ?2 AND path = ?3",
            )?;
            for path in &self.touches {
                touch.execute(params![self.now, self.producer.as_slice(), path])?;
            }
        }
        {
            let mut upsert = transaction.prepare_cached(
                "INSERT INTO input_cache (
                     producer, path, file_len, modified_secs, modified_nanos,
                     payload, payload_bytes, last_used
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(producer, path) DO UPDATE SET
                     file_len = excluded.file_len,
                     modified_secs = excluded.modified_secs,
                     modified_nanos = excluded.modified_nanos,
                     payload = excluded.payload,
                     payload_bytes = excluded.payload_bytes,
                     last_used = excluded.last_used",
            )?;
            for record in &self.pending {
                upsert.execute(params![
                    self.producer.as_slice(),
                    record.path,
                    as_sql_int(record.fingerprint.len)?,
                    as_sql_int(record.fingerprint.modified_secs)?,
                    i64::from(record.fingerprint.modified_nanos),
                    record.payload,
                    as_sql_int(record.payload.len() as u64)?,
                    self.now,
                ])?;
            }
        }

        let pruned = prune(&transaction, self.now, MAX_PAYLOAD_BYTES)?;
        transaction.commit()?;
        self.pending.clear();
        self.touches.clear();
        self.invalid.clear();
        remove_legacy_caches(&self.cache_dir);
        Ok(CommitOutcome {
            updated: payload_updated,
            pruned,
        })
    }
}

pub(crate) fn clear(cache_dir: &Path) -> Result<bool> {
    let legacy_existed = legacy_cache_paths(cache_dir).next().is_some();
    std::fs::create_dir_all(cache_dir)?;
    let mut connection = open_recovering(cache_dir)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let cached: i64 =
        transaction.query_row("SELECT COUNT(*) FROM input_cache", [], |row| row.get(0))?;
    transaction.execute(
        "UPDATE cache_meta SET epoch = epoch + 1 WHERE singleton = 1",
        [],
    )?;
    transaction.execute("DELETE FROM input_cache", [])?;
    transaction.commit()?;
    remove_legacy_caches(cache_dir);
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(legacy_existed || cached > 0)
}

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let mut connection = Connection::open(path)?;
    configure(&mut connection)?;
    Ok(connection)
}

fn open_recovering(cache_dir: &Path) -> Result<Connection> {
    let path = cache_path(cache_dir);
    match open_connection(&path) {
        Ok(connection) => Ok(connection),
        Err(error) if is_corrupt(&error) => {
            remove_store_files(&path).with_context(|| {
                format!("failed to replace corrupt cache at {}", path.display())
            })?;
            open_connection(&path)
                .with_context(|| format!("failed to recreate corrupt cache at {}", path.display()))
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to open cache in {}", cache_dir.display()))
        }
    }
}

fn configure(connection: &mut Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA wal_autocheckpoint = 1000;",
    )?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != STORE_VERSION {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "DROP TABLE IF EXISTS input_cache;
             DROP TABLE IF EXISTS cache_meta;",
        )?;
        create_schema(&transaction)?;
        transaction.pragma_update(None, "user_version", STORE_VERSION)?;
        transaction.commit()?;
    } else {
        create_schema(connection)?;
    }
    Ok(())
}

fn create_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS cache_meta (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             epoch INTEGER NOT NULL
         );
         INSERT OR IGNORE INTO cache_meta(singleton, epoch) VALUES (1, 0);
         CREATE TABLE IF NOT EXISTS input_cache (
             producer BLOB NOT NULL,
             path TEXT NOT NULL,
             file_len INTEGER NOT NULL,
             modified_secs INTEGER NOT NULL,
             modified_nanos INTEGER NOT NULL,
             payload BLOB NOT NULL,
             payload_bytes INTEGER NOT NULL,
             last_used INTEGER NOT NULL,
             PRIMARY KEY (producer, path)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS input_cache_last_used
             ON input_cache(last_used);",
    )?;
    Ok(())
}

fn prune(connection: &Connection, now: i64, max_payload_bytes: i64) -> Result<usize> {
    let mut pruned = connection.execute(
        "DELETE FROM input_cache WHERE last_used < ?1",
        params![now - MAX_AGE_SECS],
    )?;
    let total: i64 = connection.query_row(
        "SELECT COALESCE(SUM(payload_bytes), 0) FROM input_cache",
        [],
        |row| row.get(0),
    )?;
    if total <= max_payload_bytes {
        return Ok(pruned);
    }

    let mut reclaimed = 0i64;
    let mut victims = Vec::new();
    {
        let mut statement = connection.prepare(
            "SELECT producer, path, payload_bytes
               FROM input_cache
              ORDER BY last_used ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (producer, path, bytes) = row?;
            reclaimed += bytes;
            victims.push((producer, path));
            if total - reclaimed <= max_payload_bytes {
                break;
            }
        }
    }
    let mut delete =
        connection.prepare_cached("DELETE FROM input_cache WHERE producer = ?1 AND path = ?2")?;
    for (producer, path) in victims {
        pruned += delete.execute(params![producer, path])?;
    }
    Ok(pruned)
}

fn as_sql_int(value: u64) -> Result<i64> {
    i64::try_from(value).context("input metadata exceeds SQLite integer range")
}

fn is_corrupt(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(code.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

fn remove_store_files(path: &Path) -> std::io::Result<()> {
    let sidecar = |suffix: &str| {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    };
    for candidate in [path.to_path_buf(), sidecar("-wal"), sidecar("-shm")] {
        match std::fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn legacy_cache_paths(cache_dir: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(cache_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            is_legacy_cache_name(name).then_some(path)
        })
}

fn remove_legacy_caches(cache_dir: &Path) {
    for path in legacy_cache_paths(cache_dir) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::debug!(path = %path.display(), %error, "legacy index cache could not be removed");
        }
    }
}

fn is_legacy_cache_name(name: &str) -> bool {
    let Some((version, hash)) = name
        .strip_prefix("index-v")
        .and_then(|name| name.strip_suffix(".json"))
        .and_then(|name| name.split_once('-'))
    else {
        return false;
    };
    version.parse::<u32>().is_ok()
        && hash.len() == 16
        && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zerosyntax-input-cache-{name}-{}-{}",
            std::process::id(),
            UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ))
    }

    fn fingerprint(value: u64) -> Fingerprint {
        Fingerprint {
            len: value,
            modified_secs: value + 1,
            modified_nanos: value as u32 + 2,
        }
    }

    #[test]
    fn records_are_reused_by_input_not_root_set() {
        let dir = temp_dir("reuse");
        let producer = [7; 32];
        let mut first = InputCache::open(&dir, producer).unwrap();
        first.queue_store("base/Weapon.ini".into(), fingerprint(10), vec![1, 2, 3]);
        assert!(first.commit().unwrap().updated);
        drop(first);

        let mut second = InputCache::open(&dir, producer).unwrap();
        assert_eq!(
            second.lookup("base/Weapon.ini", &fingerprint(10)).unwrap(),
            Some(vec![1, 2, 3])
        );
        assert!(second
            .lookup("base/Weapon.ini", &fingerprint(11))
            .unwrap()
            .is_none());
        drop(second);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn producer_identity_isolated_records() {
        let dir = temp_dir("producer");
        let mut first = InputCache::open(&dir, [1; 32]).unwrap();
        first.queue_store("Weapon.ini".into(), fingerprint(10), vec![1]);
        first.commit().unwrap();
        drop(first);

        let mut other = InputCache::open(&dir, [2; 32]).unwrap();
        assert!(other
            .lookup("Weapon.ini", &fingerprint(10))
            .unwrap()
            .is_none());
        drop(other);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clear_epoch_rejects_late_writes() {
        let dir = temp_dir("epoch");
        let producer = [3; 32];
        let mut stale_scan = InputCache::open(&dir, producer).unwrap();
        stale_scan.queue_store("Weapon.ini".into(), fingerprint(10), vec![1]);

        clear(&dir).unwrap();
        assert!(!stale_scan.commit().unwrap().updated);
        drop(stale_scan);

        let mut after = InputCache::open(&dir, producer).unwrap();
        assert!(after
            .lookup("Weapon.ini", &fingerprint(10))
            .unwrap()
            .is_none());
        drop(after);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clear_removes_legacy_caches_but_preserves_unrelated_files() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("index-v5-0123456789abcdef.json");
        let unrelated = dir.join("notes.txt");
        std::fs::write(&legacy, b"legacy").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();

        assert!(clear(&dir).unwrap());
        assert!(!legacy.exists());
        assert!(unrelated.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_database_is_replaced_and_reused() {
        let dir = temp_dir("corrupt-database");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(cache_path(&dir), b"not sqlite").unwrap();

        let mut cache = InputCache::open(&dir, [9; 32]).unwrap();
        cache.queue_store("Weapon.ini".into(), fingerprint(10), vec![4, 5]);
        cache.commit().unwrap();
        drop(cache);

        let mut warm = InputCache::open(&dir, [9; 32]).unwrap();
        assert_eq!(
            warm.lookup("Weapon.ini", &fingerprint(10)).unwrap(),
            Some(vec![4, 5])
        );
        drop(warm);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_scan_sessions_commit_a_valid_union() {
        let dir = temp_dir("concurrent-union");
        let producer = [5; 32];
        let mut first = InputCache::open(&dir, producer).unwrap();
        let mut second = InputCache::open(&dir, producer).unwrap();
        first.queue_store("Weapon.ini".into(), fingerprint(10), vec![1]);
        second.queue_store("Object.ini".into(), fingerprint(20), vec![2]);
        first.commit().unwrap();
        second.commit().unwrap();
        drop(first);
        drop(second);

        let mut combined = InputCache::open(&dir, producer).unwrap();
        assert_eq!(
            combined.lookup("Weapon.ini", &fingerprint(10)).unwrap(),
            Some(vec![1])
        );
        assert_eq!(
            combined.lookup("Object.ini", &fingerprint(20)).unwrap(),
            Some(vec![2])
        );
        drop(combined);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expired_records_are_pruned_individually() {
        let dir = temp_dir("expired");
        let producer = [6; 32];
        let mut cache = InputCache::open(&dir, producer).unwrap();
        cache.queue_store("Old.ini".into(), fingerprint(10), vec![1]);
        cache.commit().unwrap();
        cache
            .connection
            .execute("UPDATE input_cache SET last_used = 0", [])
            .unwrap();
        cache.queue_store("Fresh.ini".into(), fingerprint(20), vec![2]);
        assert_eq!(cache.commit().unwrap().pruned, 1);
        assert!(cache.lookup("Old.ini", &fingerprint(10)).unwrap().is_none());
        assert_eq!(
            cache.lookup("Fresh.ini", &fingerprint(20)).unwrap(),
            Some(vec![2])
        );
        drop(cache);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clearing_an_empty_store_reports_no_removed_data() {
        let dir = temp_dir("empty-clear");
        assert!(!clear(&dir).unwrap());
        assert!(!clear(&dir).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn payload_budget_prunes_least_recent_records() {
        let dir = temp_dir("payload-budget");
        let producer = [8; 32];
        let mut cache = InputCache::open(&dir, producer).unwrap();
        cache.queue_store("Old.ini".into(), fingerprint(10), vec![1; 16]);
        cache.commit().unwrap();
        cache
            .connection
            .execute(
                "UPDATE input_cache SET last_used = ?1",
                params![unix_now() - 1],
            )
            .unwrap();
        cache.queue_store("New.ini".into(), fingerprint(20), vec![2; 16]);
        cache.commit().unwrap();

        let transaction = cache.connection.transaction().unwrap();
        assert_eq!(prune(&transaction, unix_now(), 16).unwrap(), 1);
        transaction.commit().unwrap();
        assert!(cache.lookup("Old.ini", &fingerprint(10)).unwrap().is_none());
        assert_eq!(
            cache.lookup("New.ini", &fingerprint(20)).unwrap(),
            Some(vec![2; 16])
        );
        drop(cache);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
