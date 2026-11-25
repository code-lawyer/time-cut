mod transfer;
pub(super) mod observed;
pub(super) mod fingerprints;
pub(super) mod saved_list;

use super::archive::{
    Archive, Item, Result, Source, archive_relative, contains_digest,
    extension, ordered, real_dir,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub(super) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS phone_runs(
        id INTEGER PRIMARY KEY,source TEXT NOT NULL,started INTEGER NOT NULL,first_copy INTEGER,
        sessions INTEGER NOT NULL DEFAULT 0,copies INTEGER NOT NULL DEFAULT 0,
        bytes INTEGER NOT NULL DEFAULT 0,compared INTEGER NOT NULL DEFAULT 0,
        safe_at INTEGER,discovered INTEGER,inventory TEXT,filed INTEGER NOT NULL DEFAULT 0,
        duplicates INTEGER NOT NULL DEFAULT 0, peak INTEGER NOT NULL DEFAULT 0,
        scanned INTEGER NOT NULL DEFAULT 0,covered INTEGER NOT NULL DEFAULT 0,progress_at INTEGER);
        CREATE TABLE IF NOT EXISTS phone_local_proofs(path TEXT PRIMARY KEY,digest TEXT NOT NULL,
        changed INTEGER NOT NULL,volume BLOB NOT NULL,file_id BLOB NOT NULL) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_manifest(run_id INTEGER NOT NULL,ordinal INTEGER NOT NULL,
        entry TEXT NOT NULL,version TEXT,digest TEXT NOT NULL,size INTEGER NOT NULL,PRIMARY KEY(run_id,ordinal)) WITHOUT ROWID;
        CREATE UNIQUE INDEX IF NOT EXISTS phone_run_open ON phone_runs(source) WHERE safe_at IS NULL;
        CREATE TABLE IF NOT EXISTS phone_inbox(
        id INTEGER PRIMARY KEY,source TEXT NOT NULL,entry TEXT NOT NULL,version TEXT NOT NULL,
        digest TEXT NOT NULL,size INTEGER NOT NULL,modified TEXT,priority INTEGER NOT NULL,
        blob TEXT,archive_path TEXT,released INTEGER NOT NULL DEFAULT 0,run_id INTEGER,
        CHECK((blob IS NULL) <> (archive_path IS NULL)),UNIQUE(source,entry,version,digest));
        CREATE INDEX IF NOT EXISTS phone_inbox_ready ON phone_inbox(released,priority,id);
        CREATE INDEX IF NOT EXISTS phone_inbox_blob ON phone_inbox(blob);
        CREATE INDEX IF NOT EXISTS phone_inbox_size ON phone_inbox(size,digest);
        CREATE TABLE IF NOT EXISTS phone_local_retries(
        inbox_id INTEGER PRIMARY KEY,source TEXT NOT NULL,family TEXT NOT NULL,priority INTEGER NOT NULL,
        retry_at INTEGER NOT NULL,error TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS phone_local_retry_family ON phone_local_retries(source,family,priority);
        CREATE TABLE IF NOT EXISTS phone_preserved_blobs(blob TEXT PRIMARY KEY) WITHOUT ROWID;
        CREATE TRIGGER IF NOT EXISTS phone_inbox_repaired AFTER UPDATE OF blob,archive_path ON phone_inbox
        WHEN EXISTS(SELECT 1 FROM phone_local_retries WHERE inbox_id=OLD.id) BEGIN
            INSERT OR IGNORE INTO phone_preserved_blobs SELECT OLD.blob
                WHERE OLD.blob IS NOT NULL AND OLD.blob IS NOT NEW.blob;
            DELETE FROM phone_local_retries WHERE inbox_id=OLD.id;
        END;
        CREATE TRIGGER IF NOT EXISTS phone_inbox_filed AFTER DELETE ON phone_inbox BEGIN
            DELETE FROM phone_local_retries WHERE inbox_id=OLD.id;
        END;
        CREATE TABLE IF NOT EXISTS phone_native_supplements(run_id INTEGER NOT NULL,source TEXT NOT NULL,
        checked_at INTEGER,listed INTEGER,PRIMARY KEY(run_id,source)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_native_rechecks(run_id INTEGER NOT NULL,source TEXT NOT NULL,
        manifest TEXT NOT NULL,verified_at INTEGER,PRIMARY KEY(run_id,source)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_native_pickups(run_id INTEGER NOT NULL,source TEXT NOT NULL,
            entry TEXT NOT NULL,version TEXT NOT NULL,attempted INTEGER NOT NULL,
            PRIMARY KEY(run_id,source,entry,version)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_camera_roots(source TEXT PRIMARY KEY,serial TEXT NOT NULL COLLATE NOCASE,
        udid TEXT NOT NULL COLLATE NOCASE) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_native_items(run_id INTEGER NOT NULL,source TEXT NOT NULL,entry TEXT NOT NULL,
        version TEXT NOT NULL,size INTEGER NOT NULL,digest TEXT,conflict INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(run_id,source,entry,version)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_directories(source TEXT NOT NULL,path TEXT NOT NULL,completed INTEGER NOT NULL,
        PRIMARY KEY(source,path)) WITHOUT ROWID;")?;
    super::inventory::initialize(db)
}

#[derive(Clone)]
struct Location {
    blob: Option<String>,
    archived: Option<String>,
}

impl Location {
    fn path(&self, root: &Path) -> Result<PathBuf> {
        if let Some(name) = &self.blob {
            let name = Path::new(name);
            if name.components().count() != 1
                || name
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .is_none()
            {
                return Err("无效的收件路径".into());
            }
            return Ok(root.join(".time-cut").join("inbox").join(name));
        }
        let relative = Path::new(self.archived.as_deref().ok_or("缺少已备份文件路径")?);
        if !archive_relative(relative) {
            return Err("无效的备份路径".into());
        }
        Ok(root.join(relative))
    }
}

// The inbox is private to the archive. Uncommitted bytes are never counted or indexed.
// A crash before the INSERT can leave at most the current unacknowledged spool file.
pub(super) fn recover(archive: &Archive) -> Result<()> {
    initialize(&archive.db)?;
    recover_files(archive)
}

pub(super) fn recover_files(archive: &Archive) -> Result<()> {
    // An explicit movie-name XMP has to follow its movie, including receipts from older versions.
    archive.db.execute("UPDATE phone_inbox SET priority=2 WHERE released=1 AND priority<2 AND lower(entry) LIKE '%.xmp'",[])?;
    let dir = archive.root.join(".time-cut").join("inbox");
    real_dir(&dir)?;
    let mut used = archive
        .db
        .prepare("SELECT blob FROM phone_inbox WHERE blob IS NOT NULL UNION SELECT blob FROM phone_preserved_blobs")?;
    let known = used
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| Uuid::parse_str(s).is_ok())
            && !known.contains(entry.file_name().to_string_lossy().as_ref())
        {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

pub(super) fn stored_time(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos().to_string(),
        Err(e) => format!("-{}", e.duration().as_nanos()),
    }
}

pub(super) fn read_time(raw: &str) -> Result<SystemTime> {
    let nanos: i128 = raw.parse()?;
    let n = nanos.unsigned_abs();
    let duration = Duration::new((n / 1_000_000_000).try_into()?, (n % 1_000_000_000) as u32);
    (if nanos < 0 {
        UNIX_EPOCH.checked_sub(duration)
    } else {
        UNIX_EPOCH.checked_add(duration)
    })
    .ok_or_else(|| "无效的原件日期".into())
}

struct Partial(Option<PathBuf>);
impl Drop for Partial {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

pub(super) struct Received {
    pub digest: String,
    pub bytes: u64,
    pub compared: u64,
    pub path: PathBuf,
    pub size: u64,
}

/// A WPD provider can return the previous request's bytes even after the handle
/// has been reopened. Two different, previously known objects check stream order.
pub(super) struct RecoveryCheck {
    required: bool,
    first_digest: Option<String>,
    confirm_unrecognized: bool,
    reference_first: bool,
    recheck_object: bool,
}

impl RecoveryCheck {
    pub fn new(required: bool) -> Self {
        Self {
            required,
            first_digest: None,
            confirm_unrecognized: false,
            reference_first: false,
            recheck_object: false,
        }
    }

    pub fn for_saved_list() -> Self {
        Self {
            confirm_unrecognized: true,
            ..Self::new(true)
        }
    }

    pub fn wants_reference(&self, digest: &str, size: Option<u64>) -> bool {
        self.required && size.is_some_and(|n| n > 0 && n <= 2 * 1024 * 1024)
            && self.first_digest.as_deref() != Some(digest)
    }

    pub fn for_controlled_target() -> Self {
        Self { reference_first: true, ..Self::for_saved_list() }
    }

    pub fn for_returning_item() -> Self {
        Self { recheck_object: true, ..Self::for_saved_list() }
    }

    pub fn is_controlled_target(&self) -> bool {
        self.reference_first && !self.required && self.first_digest.is_some()
    }

    fn matched(&mut self, digest: &str) {
        if self.first_digest.is_some() {
            self.required = false;
        } else {
            self.first_digest = Some(digest.to_owned());
        }
    }
}

pub(super) enum ReadPhase { Copy, Compare, Confirm }

#[derive(Debug)]
pub(super) struct WrongObject;

impl std::fmt::Display for WrongObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("手机对当前原件返回了不一致的内容，本次读取已丢弃")
    }
}

impl std::error::Error for WrongObject {}

// The NTFS change time also changes when an editor restores an old modification
// time. Filesystems without a comparable change token get a fresh local hash.
pub(super) fn change_token(path: &Path) -> Option<(i64, u64, [u8; 16])> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                FILE_BASIC_INFO, FILE_ID_INFO, FileBasicInfo, FileIdInfo,
                GetFileInformationByHandleEx,
            },
        };
        let file = fs::File::open(path).ok()?;
        let handle = HANDLE(file.as_raw_handle());
        let mut basic = FILE_BASIC_INFO::default();
        let mut id = FILE_ID_INFO::default();
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                (&raw mut basic).cast(),
                std::mem::size_of_val(&basic) as u32,
            )
            .ok()?;
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&raw mut id).cast(),
                std::mem::size_of_val(&id) as u32,
            )
            .ok()?;
        }
        if basic.ChangeTime == 0 {
            return None;
        }
        Some((
            basic.ChangeTime,
            id.VolumeSerialNumber,
            id.FileId.Identifier,
        ))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

pub(super) fn verified_local(archive: &Archive, path: &Path, expected: &str) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    let relative = path.strip_prefix(&archive.root)?.to_string_lossy();
    let Some(token @ (changed, volume, id)) = change_token(path) else {
        return Ok(contains_digest(path, expected)?);
    };
    let volume = volume.to_be_bytes();
    let already_verified: bool = archive
        .db
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM phone_local_proofs
        WHERE path=? AND digest=? AND changed=? AND volume=? AND file_id=?)",
        )?
        .query_row(
            params![
                relative,
                expected,
                changed,
                volume.as_slice(),
                id.as_slice()
            ],
            |r| r.get(0),
        )?;
    if already_verified {
        return Ok(true);
    }
    if !contains_digest(path, expected)? {
        return Ok(false);
    }
    if change_token(path) != Some(token) {
        return Ok(false);
    }
    archive
            .db
            .prepare_cached("INSERT OR REPLACE INTO phone_local_proofs VALUES(?,?,?,?,?)")?
            .execute(params![
                relative,
                expected,
                changed,
                volume.as_slice(),
                id.as_slice()
            ])?;
    Ok(true)
}

pub(super) fn receive(
    archive: &Archive,
    run: i64,
    source: &dyn Source,
    item: &Item,
    trust_version: bool,
    recovery: &mut RecoveryCheck,
    before_read: impl FnOnce() -> Result<()>,
    progress: impl FnMut(u64, ReadPhase) -> Result<()>,
) -> Result<Received> {
    let source_checksum = source.content_sha1(item)?;
    receive_checked(archive, run, source, item, trust_version, recovery, source_checksum,
        before_read, progress)
}

pub(super) fn receive_checked(
    archive: &Archive,
    run: i64,
    source: &dyn Source,
    item: &Item,
    trust_version: bool,
    recovery: &mut RecoveryCheck,
    source_checksum: Option<[u8; 20]>,
    before_read: impl FnOnce() -> Result<()>,
    mut progress: impl FnMut(u64, ReadPhase) -> Result<()>,
) -> Result<Received> {
    let db = &archive.db;
    let entry = item.relative.to_string_lossy();
    let version = item.version.as_deref().unwrap_or("");
    // A WPD display path can identify several different originals, even at identical sizes and dates.
    let candidates = db.prepare_cached("SELECT digest,blob,archive_path,version FROM phone_inbox
            WHERE source=?1 AND entry=?2 AND (?3 IS NULL OR size=?3)
        UNION ALL SELECT s.digest,NULL,s.path,s.version FROM sources s JOIN files f ON f.path=s.path
            WHERE s.source=?1 AND s.entry=?2 AND (?3 IS NULL OR f.size=?3)
        UNION ALL SELECT v.digest,NULL,v.path,v.version FROM source_versions v JOIN files f ON f.path=v.path
            WHERE v.source=?1 AND v.entry=?2 AND (?3 IS NULL OR f.size=?3)")?
        .query_map(params![source.identity(), entry, item.size], |r| Ok((r.get::<_, String>(0)?,
            Location {blob:r.get(1)?,archived:r.get(2)?},r.get::<_,Option<String>>(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (hash, location, known) in &candidates {
        if recovery.confirm_unrecognized && source_checksum.is_none() && trust_version && !version.is_empty()
            && recovery.wants_reference(hash, item.size)
            && known.as_deref() == Some(version)
            && verified_local(archive, &location.path(&archive.root)?, hash)?
        {
            let path = location.path(&archive.root)?;
            let size = fs::symlink_metadata(&path)?.len();
            before_read()?;
            transfer::confirm(source, item, size, hash, |n| progress(n, ReadPhase::Confirm))?;
            recovery.matched(hash);
            db.execute("UPDATE phone_runs SET compared=compared+? WHERE id=?", params![size, run])?;
            return Ok(Received {
                digest: hash.clone(),
                bytes: 0,
                compared: size,
                size,
                path,
            });
        }
    }
    before_read()?;
    let local_match = match (source_checksum.as_ref(),item.size) {
        (Some(checksum),Some(size)) if size>0 => fingerprints::matching(archive,size,checksum)?,
        _ => None,
    };
    let via_checksum = local_match.is_some();
    let mut payload = if let Some((digest,location)) = local_match {
        source.verify_read(item)?;
        transfer::Payload::existing(digest,item.size.unwrap(),location)
    } else {
        transfer::read(archive, source, item, &candidates, source_checksum.as_ref(), &mut |n, comparing| {
            progress(n, if comparing { ReadPhase::Compare } else { ReadPhase::Copy })
        })?
    };
    let size = payload.size;
    let (bytes, mut compared) = if payload.is_new() {(size, 0)} else if via_checksum {(0,0)} else {(0, size)};
    // On a resumed manifest, a single phone read is sufficient only after the
    // reference controls pass and all same-path, same-size receipts agree with
    // those bytes. New content and cross-name matches still get two full reads.
    let recognized = !candidates.is_empty()
        && candidates.iter().all(|(hash, _, _)| hash == &payload.digest);
    // WPD object metadata is a locator, not a content fingerprint. Only reuse a
    // prior receipt after comparing this session's actual bytes with that file.
    // A different payload under the same version also needs an independent read.
    let exact_version = |known: &Option<String>| !version.is_empty()
        && known.as_deref() == Some(version);
    let same_version_changed = candidates.iter().any(|(hash, _, known)|
        exact_version(known) && hash != &payload.digest);
    if trust_version && source_checksum.is_none() && !payload.is_new() && !same_version_changed
        && candidates.iter().any(|(hash, location, known)| exact_version(known)
            && hash == &payload.digest && location.blob == payload.location.blob
            && location.archived == payload.location.archived)
    {
        if recovery.recheck_object && !source.confirms_returned_object(item)? {
            return Err(WrongObject.into());
        }
        if recovery.wants_reference(&payload.digest, item.size) {
            recovery.matched(&payload.digest);
        }
        db.execute("UPDATE phone_runs SET compared=compared+? WHERE id=?", params![size, run])?;
        return Ok(Received { digest: payload.digest, bytes: 0, compared: size,
            path: payload.location.path(&archive.root)?, size });
    }
    if source_checksum.is_none()
        && (same_version_changed || recovery.required || (recovery.confirm_unrecognized && !recognized))
    {
        transfer::confirm(source, item, size, &payload.digest, |n| progress(n, ReadPhase::Confirm))?;
        compared += size;
    }
    if recovery.recheck_object && !source.confirms_returned_object(item)? {
        return Err(WrongObject.into());
    }
    let location = &payload.location;
    let path = location.path(&archive.root)?;
    let tx = db.unchecked_transaction()?;
    // Replace a matching but missing/damaged backing file on repair. Other receipts
    // sharing that file are safely revalidated when visited or before local filing.
    tx.execute("INSERT INTO phone_inbox(source,entry,version,digest,size,modified,priority,blob,archive_path,run_id)
        VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(source,entry,version,digest) DO UPDATE SET
        blob=excluded.blob,archive_path=excluded.archive_path,size=excluded.size,
        run_id=CASE WHEN phone_inbox.released=0 THEN excluded.run_id ELSE phone_inbox.run_id END",
        params![source.identity(),entry,version,payload.digest,size,item.modified.map(stored_time),
            if extension(&item.relative) == "xmp" {2} else {ordered(&item.relative).0},location.blob,location.archived,run])?;
    tx.execute(
        "UPDATE phone_runs SET copies=copies+?,bytes=bytes+?,compared=compared+?,
        first_copy=CASE WHEN first_copy IS NULL AND ?>0 THEN ? ELSE first_copy END WHERE id=?",
        params![
            u64::from(bytes > 0),
            bytes,
            compared,
            bytes,
            chrono::Utc::now().timestamp_millis(),
            run
        ],
    )?;
    tx.commit()?;
    payload.committed();
    Ok(Received {
        digest: payload.digest,
        bytes,
        compared,
        path,
        size,
    })
}

pub(super) fn release(db: &Connection, run: i64, key: &str, scoped: bool) -> Result<()> {
    if scoped {
        // The spool can also contain prior edits and receipts from outside today's albums.
        db.execute("UPDATE phone_inbox SET released=1,run_id=?1 WHERE source=?2 AND released=0
            AND (entry,version,digest,size) IN (
                SELECT entry,COALESCE(version,''),digest,size FROM phone_manifest WHERE run_id=?1)",
            params![run,key])?;
        db.execute("UPDATE phone_inbox SET released=1 WHERE run_id=?1 AND released=0
            AND (source,entry,version,digest,size) IN (
                SELECT n.source,n.entry,n.version,n.digest,n.size FROM phone_native_items n
                JOIN phone_native_supplements s ON s.run_id=n.run_id AND s.source=n.source
                WHERE n.run_id=?1 AND n.conflict=0 AND s.checked_at IS NOT NULL)", [run])?;
    } else {
        db.execute("UPDATE phone_inbox SET released=1,run_id=?1 WHERE released=0 AND
            (source=?2 OR (run_id=?1 AND source IN (SELECT source FROM phone_native_supplements WHERE run_id=?1)))",
            params![run,key])?;
    }
    Ok(())
}

pub(super) fn pending_local(db: &Connection) -> Result<u64> {
    Ok(db.query_row(
        "SELECT COUNT(*) FROM phone_inbox WHERE released=1",
        [],
        |r| r.get(0),
    )?)
}

pub(super) fn verify_supplements(archive: &Archive, run: i64) -> Result<bool> {
    let unfinished: bool = archive.db.query_row(
        "SELECT EXISTS(SELECT 1 FROM phone_native_supplements WHERE run_id=? AND (checked_at IS NULL OR COALESCE(listed,0)=0))",
        [run], |row| row.get(0),
    )?;
    if unfinished { return Ok(false); }
    verify_supplement_files(archive, run)
}

pub(super) fn verify_supplement_files(archive: &Archive, run: i64) -> Result<bool> {
    let mut q = archive.db.prepare(
        "SELECT i.blob,i.archive_path,i.digest,n.size,n.source FROM phone_native_items n
         LEFT JOIN phone_inbox i ON i.source=n.source AND i.run_id=n.run_id AND i.entry=n.entry
           AND i.version=n.version AND i.digest=n.digest AND i.size=n.size AND i.released=0 AND n.conflict=0
         WHERE n.run_id=?",
    )?;
    let mut rows = q.query([run])?;
    while let Some(row) = rows.next()? {
        let size: u64 = row.get(3)?;
        if let Some(digest) = row.get::<_,Option<String>>(2)? {
            let path = Location { blob: row.get(0)?, archived: row.get(1)? }.path(&archive.root)?;
            if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == size)
                && verified_local(archive, &path, &digest)? { continue; }
        }
        let source: String = row.get(4)?;
        let tx = archive.db.unchecked_transaction()?;
        tx.execute("UPDATE phone_native_supplements SET checked_at=NULL WHERE run_id=? AND source=?",
            params![run,source])?;
        tx.execute("UPDATE phone_native_rechecks SET verified_at=NULL WHERE run_id=? AND source=?",
            params![run,source])?;
        tx.commit()?;
        return Ok(false);
    }
    Ok(true)
}

pub(super) fn has_due_filing(db: &Connection) -> Result<bool> {
    // An existing preview archive receives this table on its next writable open.
    let initialized: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='phone_local_retries')", [], |r| r.get(0))?;
    if !initialized { return Ok(true); }
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM phone_inbox i WHERE i.released=1
        AND NOT EXISTS(SELECT 1 FROM phone_local_retries t WHERE t.inbox_id=i.id AND t.retry_at>unixepoch()))",
        [], |r| r.get(0))?)
}

pub(super) fn local_errors(db: &Connection) -> Result<(u64, Option<String>)> {
    Ok(db.query_row("SELECT COUNT(*),MIN(t.error) FROM phone_local_retries t
        JOIN phone_inbox i ON i.id=t.inbox_id WHERE i.released=1", [],
        |r| Ok((r.get(0)?,r.get(1)?)))?)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Filed {
    Original(bool),
    Deferred,
}

#[cfg(test)]
pub(super) fn file_next(archive: &mut Archive) -> Result<Option<Filed>> {
    file_next_at(archive, chrono::Utc::now().timestamp())
}

pub(super) fn file_next_at(archive: &mut Archive, pass_started: i64) -> Result<Option<Filed>> {
    let row = archive
        .db
        .query_row(
            "SELECT id,source,entry,version,digest,size,modified,blob,archive_path
        FROM phone_inbox i WHERE released=1 AND NOT EXISTS(
            SELECT 1 FROM phone_local_retries t WHERE t.inbox_id=i.id AND t.retry_at>?)
        ORDER BY priority,id LIMIT 1",
            [pass_started],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, u64>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    Location {
                        blob: r.get(7)?,
                        archived: r.get(8)?,
                    },
                ))
            },
        )
        .optional()?;
    let Some((id, source, entry, version, digest, size, modified, location)) = row else {
        return Ok(None);
    };
    let relative = Path::new(&entry);
    let family = super::archive::family(relative);
    let priority = if extension(relative) == "xmp" { 2 } else { ordered(relative).0 };
    let filed = (|| -> Result<_> {
        // Never file a Live Photo movie or edits under an unrelated fallback
        // stem while a previously seen primary from this family needs repair.
        if super::archive::may_have_owner(relative) && archive.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM phone_local_retries WHERE source=? AND family=? AND priority<?)",
            params![source, family, priority], |r| r.get::<_,bool>(0))? {
            return Err("正在等待同组原件完成归档".into());
        }
        let data = location.path(&archive.root)?;
        if !contains_digest(&data, &digest)? {
            return Err("本地收件内容与已存凭据不符".into());
        }
        let item = Item {
            id: String::new(),
            relative: PathBuf::from(&entry),
            version: (!version.is_empty()).then_some(version),
            modified: modified.as_deref().map(read_time).transpose()?,
            size: Some(size),
        };
        let added = archive.file_receipt(&source, &item, &digest, &data, size)?;
        Ok((data, added))
    })();
    let (data, added) = match filed {
        Ok(result) => result,
        Err(error) => {
            archive.db.execute("INSERT OR REPLACE INTO phone_local_retries(inbox_id,source,family,priority,retry_at,error)
                VALUES(?,?,?,?,unixepoch()+60,?)", params![id,source,family,priority,format!("{entry}：{error}")])?;
            return Ok(Some(Filed::Deferred));
        }
    };
    let tx = archive.db.unchecked_transaction()?;
    tx.execute(
        "UPDATE phone_runs SET filed=filed+?,duplicates=duplicates+?
        WHERE id=(SELECT run_id FROM phone_inbox WHERE id=?)",
        params![i64::from(added), i64::from(!added), id],
    )?;
    tx.execute("DELETE FROM phone_inbox WHERE id=?", [id])?;
    tx.commit()?;
    if let Some(blob) = &location.blob
        && !archive.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM phone_inbox WHERE blob=?1 UNION ALL SELECT 1 FROM phone_preserved_blobs WHERE blob=?1)",
            [blob],
            |r| r.get::<_, bool>(0),
        )?
    {
        fs::remove_file(&data)?;
        archive.db.execute("DELETE FROM phone_local_proofs WHERE path=?", [data.strip_prefix(&archive.root)?.to_string_lossy()])?;
    }
    Ok(Some(Filed::Original(added)))
}
