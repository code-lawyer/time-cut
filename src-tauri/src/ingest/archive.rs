use chrono::{Local, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug)]
pub(super) struct IncompleteTransfer {
    pub(super) received: u64,
    pub(super) expected: Option<u64>,
}

impl std::fmt::Display for IncompleteTransfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "传输不完整，实际收到 {} 字节，设备报告 {:?}",
            self.received, self.expected
        )
    }
}

impl std::error::Error for IncompleteTransfer {}

const UNKNOWN_DATE_FOLDER: &str = "日期未知";

pub(crate) fn is_undated(path: &Path) -> bool {
    path.parent()
        .filter(|folder| {
            folder
                .file_name()
                .is_some_and(|name| name == UNKNOWN_DATE_FOLDER)
        })
        .and_then(Path::parent)
        .is_some_and(|root| root.join(".time-cut").join("imports.sqlite3").is_file())
}

#[derive(Clone)]
pub struct Item {
    pub id: String,
    pub relative: PathBuf,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    // Set only if the provider can identify this version across connections.
    pub version: Option<String>,
}

pub enum ScanEvent {
    Inspected { media: bool },
    DirectoryFound(PathBuf),
    DirectoryStart(PathBuf),
    DirectoryDone { path: PathBuf, media: Vec<Item> },
    DirectoryCheck { done: u64, total: u64 },
    // The phone has exposed this companion, but its primary image may be listed later.
    CompanionFound(Item),
    Unreadable(String),
    Ready(Item),
}

#[derive(Default)]
pub struct CopyOrder {
    pub folders: std::collections::HashMap<PathBuf, i64>,
    pub priorities: std::collections::HashMap<PathBuf, u8>,
    pub interrupted: std::collections::HashMap<PathBuf, i64>,
    pub preferred_folder: Option<PathBuf>,
}

pub trait SavedFiles {
    /// Resolve a previously listed version to a fresh handle on the current connection.
    fn resolve(&self, expected: &Item) -> Result<Option<Item>>;
}

pub trait Source {
    fn identity(&self) -> &str;
    /// Identifies the directory policy behind persisted phone inventories.
    fn album_scope_version(&self) -> Option<&'static str> { None }
    fn album_path_in_scope(&self, _relative: &Path) -> bool { true }
    /// Hardware serial read from the paired Apple session that owns this DCIM source.
    fn paired_camera_serial(&self) -> Option<&str> { None }
    fn is_phone(&self) -> bool {
        false
    }
    fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()>;
    fn visit_for_copy(
        &self,
        _order: &CopyOrder,
        emit: &mut dyn FnMut(ScanEvent) -> Result<()>,
    ) -> Result<()> {
        self.visit(emit)
    }
    fn reader(&self, item: &Item) -> Result<Box<dyn Read>>;
    fn saved_files(&self) -> Option<&dyn SavedFiles> { None }
    // Only a checksum computed by the source from the named file can avoid a USB read.
    fn content_sha1(&self, _item: &Item) -> Result<Option<[u8; 20]>> {
        Ok(None)
    }
    fn local_path(&self, _item: &Item) -> Option<PathBuf> {
        None
    }
    fn verify_read(&self, _item: &Item) -> Result<()> {
        Ok(())
    }
    /// A size mismatch is only attributable to this object when the same handle,
    /// complete metadata and directory membership still identify it after EOF.
    fn confirms_returned_object(&self, _item: &Item) -> Result<bool> {
        Ok(false)
    }
}

pub fn media(path: &Path) -> bool {
    crate::t_utils::get_file_type(&path.to_string_lossy()).is_some()
        || matches!(extension(path).as_str(), "aae" | "xmp")
}

pub(super) fn extension(path: &Path) -> String {
    path.extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
}

pub(super) fn may_have_owner(path: &Path) -> bool {
    matches!(extension(path).as_str(), "mov" | "aae" | "xmp")
}

fn is_still(path: &Path) -> bool {
    matches!(
        crate::t_utils::get_file_type(&path.to_string_lossy()),
        Some(1 | 3)
    )
}

pub fn ordered(path: &Path) -> (u8, String) {
    // The primary image fixes the destination stem for MOV, AAE and XMP companions.
    (
        if is_still(path) { 0 } else { 1 },
        path.to_string_lossy().to_lowercase(),
    )
}

pub(super) fn family(path: &Path) -> String {
    let first = path.with_extension("");
    let stem = if extension(path) == "xmp" && media(&first) {
        first.with_extension("")
    } else {
        first
    };
    stem.to_string_lossy().to_lowercase()
}

fn skip_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    if entry.file_name().eq_ignore_ascii_case(".time-cut") {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if entry.file_type().is_dir()
            && fs::symlink_metadata(entry.path()).is_ok_and(|m| m.file_attributes() & 0x400 != 0)
        {
            return true;
        }
    }
    entry.file_type().is_symlink()
}

pub struct Folder {
    root: PathBuf,
    identity: String,
}

/// A user can paste a path directly or reach it through a junction. Staged originals
/// must stay private even when a different library is selected as the destination.
pub(super) fn media_directory(path: &Path) -> Result<PathBuf> {
    let root = path.canonicalize()?;
    if root.ancestors().any(|dir| {
        dir.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(".time-cut"))
            && (dir.join("imports.sqlite3").is_file() || dir.join("import.lock").is_file())
    }) {
        return Err("请选择照片所在的目录，图库的 .time-cut 内部目录不能用作来源或目标".into());
    }
    Ok(root)
}

impl Folder {
    pub fn new(path: &Path, destination: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err("来源目录必须是绝对路径".into());
        }
        let root = media_directory(path)?;
        let dest = destination.canonicalize()?;
        if !root.is_dir() || root.starts_with(&dest) || dest.starts_with(&root) {
            return Err("来源和图库须为互不包含的两个文件夹".into());
        }
        Ok(Self {
            identity: format!("folder:{}", root.display()),
            root,
        })
    }
}

impl Source for Folder {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> {
        let mut unreadable = 0usize;
        let mut first_error = None;
        for entry in WalkDir::new(&self.root)
            .sort_by_key(|e| ordered(e.path()))
            .into_iter()
            .filter_entry(|e| !skip_entry(e))
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    unreadable += 1;
                    first_error.get_or_insert_with(|| e.to_string());
                    emit(ScanEvent::Inspected { media: false })?;
                    continue;
                }
            };
            let supported = entry.file_type().is_file() && media(entry.path());
            emit(ScanEvent::Inspected { media: supported })?;
            if !supported {
                continue;
            }
            let info = match entry.metadata() {
                Ok(info) => info,
                Err(e) => {
                    unreadable += 1;
                    first_error.get_or_insert_with(|| e.to_string());
                    continue;
                }
            };
            let relative = entry.path().strip_prefix(&self.root)?.to_owned();
            emit(ScanEvent::Ready(Item {
                id: relative.to_string_lossy().into_owned(),
                relative,
                size: Some(info.len()),
                modified: info.modified().ok(),
                version: None,
            }))?;
        }
        match first_error {
            Some(message) => Err(format!("{unreadable} 处来源暂不可读。{message}").into()),
            None => Ok(()),
        }
    }
    fn reader(&self, item: &Item) -> Result<Box<dyn Read>> {
        Ok(Box::new(open_stable_file(&self.root.join(&item.relative))?))
    }
    fn local_path(&self, item: &Item) -> Option<PathBuf> {
        Some(self.root.join(&item.relative))
    }
    fn verify_read(&self, item: &Item) -> Result<()> {
        let now = fs::metadata(self.root.join(&item.relative))?;
        if item.size != Some(now.len()) || item.modified != now.modified().ok() {
            return Err("文件在复制过程中发生变化，请重新导入".into());
        }
        Ok(())
    }
}

pub struct Archive {
    pub root: PathBuf,
    stage: PathBuf,
    pub(super) db: Connection,
    pub(super) content_index: std::cell::OnceCell<super::inbox::fingerprints::Index>,
    verified_sizes: HashSet<u64>,
    _lock: File,
}

fn stamp(time: Option<SystemTime>) -> String {
    time.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_default()
}

fn digest_reader(reader: impl Read) -> io::Result<String> {
    let mut hash = blake3::Hasher::new();
    hash.update_reader(reader)?;
    Ok(hash.finalize().to_hex().to_string())
}

fn digest_file(path: &Path) -> io::Result<String> {
    digest_reader(open_stable_file(path)?)
}

pub(super) fn open_stable_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
        // Hold a consistent source even when an editor restores the original mtime.
        options.share_mode(FILE_SHARE_READ.0);
    }
    options.open(path)
}

pub(super) fn contains_digest(path: &Path, digest: &str) -> io::Result<bool> {
    let comparison: io::Result<bool> = (|| {
        // A link or a directory can occupy an otherwise valid name, but cannot hold an original.
        if !fs::symlink_metadata(path)?.is_file() {
            return Ok(false);
        }
        Ok(digest_file(path)? == digest)
    })();
    match comparison {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        result => result,
    }
}

pub(super) fn real_dir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.is_symlink() {
        return Err(format!("目标不是普通目录：{}", path.display()).into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(format!("目标目录指向其他位置：{}", path.display()).into());
        }
    }
    Ok(())
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(windows)]
fn publish(temp: &Path, dest: &Path) -> io::Result<()> {
    use windows::{
        Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW},
        core::HSTRING,
    };
    // Same-volume atomic rename. Omit REPLACE_EXISTING to protect files added by other programs.
    unsafe {
        MoveFileExW(
            &HSTRING::from(temp.as_os_str()),
            &HSTRING::from(dest.as_os_str()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|e| io::Error::from_raw_os_error((e.code().0 & 0xffff) as i32))
}
#[cfg(not(windows))]
fn publish(temp: &Path, dest: &Path) -> io::Result<()> {
    fs::hard_link(temp, dest)
}

fn safe_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim_end_matches([' ', '.']);
    let head = clean
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    if clean.is_empty()
        || matches!(&*head, "CON" | "PRN" | "AUX" | "NUL")
        || (head.len() == 4
            && (head.starts_with("COM") || head.starts_with("LPT"))
            && head.as_bytes()[3].is_ascii_digit())
    {
        format!("_{clean}")
    } else {
        clean.to_string()
    }
}

fn collision_name(preferred: &Path, suffix: &str) -> String {
    let extension = preferred.extension().unwrap_or_default().to_string_lossy();
    let ending = format!("-{suffix}.{extension}");
    // A media collision can also have a full-name ".xmp" alongside it.
    let max_units: usize = if crate::t_utils::get_file_type(&preferred.to_string_lossy()).is_some()
    {
        251
    } else {
        255
    };
    let mut remaining = max_units.saturating_sub(ending.encode_utf16().count());
    let mut name = String::new();
    for ch in preferred
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
    {
        if ch.len_utf16() > remaining {
            break;
        }
        remaining -= ch.len_utf16();
        name.push(ch);
    }
    name.push_str(&ending);
    name
}

pub(super) fn archive_relative(path: &Path) -> bool {
    use std::path::Component;
    let mut parts = path.components();
    matches!(parts.next(), Some(Component::Normal(name)) if !name.eq_ignore_ascii_case(".time-cut"))
        && parts.all(|part| matches!(part, Component::Normal(_)))
}

fn sidecar_variant(owner: &Path, path: &Path, short: &str) -> bool {
    if !archive_relative(path) || owner.parent() != path.parent() {
        return false;
    }
    if owner == path || owner.with_file_name(collision_name(owner, short)) == path {
        return true;
    }
    let sequence = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplit_once('-'))
        .and_then(|(_, n)| n.parse::<u64>().ok());
    sequence.is_some_and(|n| {
        n > 0 && owner.with_file_name(collision_name(owner, &format!("{short}-{n}"))) == path
    })
}

impl Archive {
    pub fn open(root: &Path) -> Result<Self> {
        let root = media_directory(root)?;
        let state = root.join(".time-cut");
        real_dir(&state)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.join("import.lock"))?;
        lock.try_lock()
            .map_err(|_| "这个图库正在被另一个导入任务使用")?;
        let stage = state.join("staging");
        real_dir(&stage)?;
        for file in fs::read_dir(&stage)? {
            let file = file?;
            let path = file.path();
            if file.file_type()?.is_file()
                && path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.strip_prefix("transfer-"))
                    .is_some_and(|s| Uuid::parse_str(s).is_ok())
            {
                fs::remove_file(path)?;
            }
        }
        let db = Connection::open(state.join("imports.sqlite3"))?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, digest TEXT NOT NULL, size INTEGER NOT NULL, modified TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS file_digest ON files(digest);
            CREATE INDEX IF NOT EXISTS file_size ON files(size);
            CREATE TABLE IF NOT EXISTS sources(source TEXT NOT NULL, entry TEXT NOT NULL, version TEXT, digest TEXT NOT NULL,
                family TEXT NOT NULL, primary_image INTEGER NOT NULL, path TEXT NOT NULL, PRIMARY KEY(source,entry));
            CREATE INDEX IF NOT EXISTS source_family ON sources(source,family,primary_image);
            CREATE INDEX IF NOT EXISTS source_named ON sources(source,entry COLLATE NOCASE);
            CREATE TABLE IF NOT EXISTS source_versions(source TEXT NOT NULL, entry TEXT NOT NULL, version TEXT NOT NULL,
                digest TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(source,entry,version)) WITHOUT ROWID;
            CREATE TABLE IF NOT EXISTS sidecar_paths(owner TEXT NOT NULL, digest TEXT NOT NULL, path TEXT NOT NULL,
                PRIMARY KEY(owner,digest));")?;
        Ok(Self {
            root,
            stage,
            db,
            content_index: Default::default(),
            verified_sizes: HashSet::new(),
            _lock: lock,
        })
    }

    pub fn reconcile(&mut self) -> Result<()> {
        // This also repairs a crash between the atomic rename and the SQLite commit
        // and indexes pre-existing photos in a user-selected archive.
        self.verified_sizes.clear();
        let tx = self.db.transaction()?;
        let mut seen = HashSet::new();
        for entry in WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| !skip_entry(e))
        {
            let entry = match entry {
                Err(e)
                    if e.io_error()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                other => other?,
            };
            if !entry.file_type().is_file() || !media(entry.path()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)?
                .to_string_lossy()
                .into_owned();
            let info = match entry.metadata() {
                Err(e)
                    if e.io_error()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                other => other?,
            };
            let modified = stamp(info.modified().ok());
            // Empty digest means the contents must be checked on a matching size or
            // sidecar lookup. An unrelated file held by an editor need not block this job.
            tx.prepare_cached(
                "INSERT INTO files(path,digest,size,modified) VALUES(?,'',?,?)
                 ON CONFLICT(path) DO UPDATE SET digest='',size=excluded.size,modified=excluded.modified
                 WHERE files.size<>excluded.size OR files.modified<>excluded.modified",
            )?.execute(params![relative, info.len(), modified])?;
            seen.insert(relative);
        }
        let mut query = tx.prepare("SELECT path FROM files")?;
        let missing: Vec<String> = query
            .query_map([], |r| r.get::<_, String>(0))?
            .filter(|path| path.as_ref().map_or(true, |p| !seen.contains(p)))
            .collect::<rusqlite::Result<_>>()?;
        drop(query);
        for path in missing {
            tx.execute("DELETE FROM files WHERE path=?", [path])?;
        }
        tx.commit()?;
        Ok(())
    }

    fn contains_size(&self, size: u64) -> rusqlite::Result<bool> {
        self.db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM files WHERE size=?)")?
            .query_row([size], |row| row.get(0))
    }

    fn matching_sidecar(&self, owner: &Path, hash: &str) -> Result<Option<String>> {
        // Sidecars are editable documents attached to a particular archived original.
        // Reuse only this owner's filename or its collision variant.
        let saved: Option<String> = self
            .db
            .query_row(
                "SELECT path FROM sidecar_paths WHERE owner=? AND digest=?",
                params![owner.to_string_lossy(), hash],
                |r| r.get(0),
            )
            .optional()?;
        let mut candidates = vec![owner.to_path_buf()];
        if let Some(short) = hash
            .get(..10)
            .filter(|v| v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            if let Some(saved) = saved.map(PathBuf::from)
                && sidecar_variant(owner, &saved, short)
                && !candidates.contains(&saved)
            {
                candidates.push(saved);
            }
            // Also recover pre-journal and earlier-version collision outputs.
            let first_collision = owner.with_file_name(collision_name(owner, short));
            if !candidates.contains(&first_collision) {
                candidates.push(first_collision);
            }
        }
        for path in candidates {
            if contains_digest(&self.root.join(&path), hash)? {
                return Ok(Some(path.to_string_lossy().into_owned()));
            }
        }
        Ok(None)
    }

    fn matching(
        &mut self,
        hash: &str,
        size: Option<u64>,
        owner: Option<&Path>,
    ) -> Result<Option<String>> {
        if let Some(owner) = owner {
            return self.matching_sidecar(owner, hash);
        }
        {
            let mut q = self
                .db
                .prepare_cached("SELECT path FROM files WHERE digest=? ORDER BY path")?;
            for row in q.query_map([hash], |r| r.get::<_, String>(0))? {
                let path = row?;
                if archive_relative(Path::new(&path))
                    && contains_digest(&self.root.join(&path), hash)?
                {
                    return Ok(Some(path));
                }
            }
        }
        let Some(size) = size else { return Ok(None) };
        if self.verified_sizes.contains(&size) {
            return Ok(None);
        }
        // Editors and sync tools can replace content while preserving size and mtime.
        // On a cache miss verify this size group once per reconciliation. Files published
        // by this importer subsequently enter the digest index with their known content.
        let tx = self.db.transaction()?;
        let candidates = tx
            .prepare_cached("SELECT path,digest FROM files WHERE size=? ORDER BY path")?
            .query_map([size], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut found = None;
        for (relative, previous) in candidates {
            if !archive_relative(Path::new(&relative)) {
                continue;
            }
            let path = self.root.join(&relative);
            let info = match fs::symlink_metadata(&path) {
                Ok(info) if info.is_file() && info.len() == size => info,
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => continue,
            };
            let actual = match digest_file(&path) {
                Ok(digest) => digest,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if actual != previous {
                tx.execute(
                    "UPDATE files SET digest=?,modified=? WHERE path=?",
                    params![actual, stamp(info.modified().ok()), relative],
                )?;
            }
            if actual == hash && found.is_none() {
                found = Some(relative);
            }
        }
        tx.commit()?;
        self.verified_sizes.insert(size);
        Ok(found)
    }

    fn record(
        &mut self,
        source: &str,
        item: &Item,
        digest: &str,
        rel: &str,
        owner: Option<&Path>,
    ) -> Result<()> {
        let meta = fs::metadata(self.root.join(rel))?;
        let entry = item.relative.to_string_lossy();
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO files VALUES(?,?,?,?) ON CONFLICT(path) DO UPDATE SET
               digest=excluded.digest,size=excluded.size,modified=excluded.modified
             WHERE (files.digest,files.size,files.modified) IS NOT (excluded.digest,excluded.size,excluded.modified)",
            params![rel, digest, meta.len(), stamp(meta.modified().ok())],
        )?;
        // WPD may list distinct objects at the same display path. Keep the
        // previous verified version while updating the active companion lookup.
        tx.execute(
            "INSERT INTO source_versions(source,entry,version,digest,path)
             SELECT source,entry,version,digest,path FROM sources
             WHERE source=?1 AND entry=?2 AND version IS NOT NULL AND version IS NOT ?3
             ON CONFLICT(source,entry,version) DO UPDATE SET digest=excluded.digest,path=excluded.path
             WHERE (source_versions.digest,source_versions.path) IS NOT (excluded.digest,excluded.path)",
            params![source, entry, item.version],
        )?;
        tx.execute(
            "INSERT INTO sources VALUES(?,?,?,?,?,?,?) ON CONFLICT(source,entry) DO UPDATE SET
               version=excluded.version,digest=excluded.digest,family=excluded.family,
               primary_image=excluded.primary_image,path=excluded.path
             WHERE (sources.version,sources.digest,sources.family,sources.primary_image,sources.path)
               IS NOT (excluded.version,excluded.digest,excluded.family,excluded.primary_image,excluded.path)",
            params![
                source,
                entry,
                item.version,
                digest,
                family(&item.relative),
                is_still(&item.relative),
                rel
            ],
        )?;
        if let Some(owner) = owner {
            tx.execute(
                "INSERT INTO sidecar_paths VALUES(?,?,?) ON CONFLICT(owner,digest) DO UPDATE SET path=excluded.path
                 WHERE sidecar_paths.path<>excluded.path",
                params![owner.to_string_lossy(), digest, rel],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn companion_path(&self, source: &str, item: &Item, ext: &str) -> Result<Option<PathBuf>> {
        if may_have_owner(&item.relative) {
            let base = item.relative.with_extension("");
            let full_name = ext == "xmp" && media(&base);
            let (lookup, key) = if full_name {
                (
                    "SELECT s.path,s.digest,f.digest<>'' FROM sources s JOIN files f ON f.path=s.path
                     WHERE s.source=? AND s.entry=? COLLATE NOCASE AND (f.digest=s.digest OR f.digest='') ORDER BY s.path",
                    base.to_string_lossy().into_owned(),
                )
            } else {
                (
                    "SELECT s.path,s.digest,f.digest<>'' FROM sources s JOIN files f ON f.path=s.path
                     WHERE s.source=? AND s.family=? AND s.primary_image=1 AND (f.digest=s.digest OR f.digest='') ORDER BY s.path",
                    family(&item.relative),
                )
            };
            let mut query = self.db.prepare_cached(lookup)?;
            let rows = query.query_map(params![source, key], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })?;
            for row in rows {
                let (owner, digest, confirmed) = row?;
                if !archive_relative(Path::new(&owner))
                    || (!confirmed && !contains_digest(&self.root.join(&owner), &digest)?)
                {
                    continue;
                }
                let mut path = PathBuf::from(owner);
                if full_name {
                    path.as_mut_os_string().push(".xmp");
                } else {
                    path.set_extension(item.relative.extension().unwrap_or_default());
                }
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    fn dated_path(item: &Item, staged: &Path) -> Result<PathBuf> {
        let date = crate::t_utils::get_file_type(&staged.to_string_lossy())
            .and_then(|kind| {
                crate::t_sqlite::AFile::capture_timestamp_with_raw_info(
                    &staged.to_string_lossy(),
                    kind,
                    None,
                )
                .ok()
            })
            .or_else(|| crate::t_utils::systemtime_to_timestamp(item.modified))
            .filter(|t| *t != 0 || item.modified.is_some())
            .and_then(|t| Local.timestamp_opt(t, 0).single());
        let folder = date
            .map(|d| PathBuf::from(d.format("%Y").to_string()).join(d.format("%Y-%m").to_string()))
            .unwrap_or_else(|| PathBuf::from(UNKNOWN_DATE_FOLDER));
        let name = safe_name(
            &item
                .relative
                .file_name()
                .ok_or("来源文件没有名称")?
                .to_string_lossy(),
        );
        Ok(folder.join(name))
    }

    /// Returns true only when this transfer published a new original.
    #[cfg(test)]
    pub fn import(
        &mut self,
        source: &dyn Source,
        item: &Item,
        written: impl FnMut(u64) -> Result<()>,
    ) -> Result<bool> {
        self.import_tracked(source, item, |_| Ok(()), written)
            .map(|(added, _)| added)
    }

    pub(super) fn import_tracked(
        &mut self,
        source: &dyn Source,
        item: &Item,
        before_read: impl FnOnce(&Connection) -> Result<()>,
        mut written: impl FnMut(u64) -> Result<()>,
    ) -> Result<(bool, String)> {
        let ext = extension(&item.relative);
        let companion = self.companion_path(source.identity(), item, &ext)?;
        let owner = match ext.as_str() {
            "xmp" | "aae" => companion.as_deref(),
            _ => None,
        };
        if let Some(version) = &item.version {
            let known: Option<(String, String, bool)> = self
                .db
                .query_row(
                    "SELECT digest,path,1 AS active FROM sources WHERE source=?1 AND entry=?2 AND version=?3
                     UNION ALL
                     SELECT digest,path,0 FROM source_versions WHERE source=?1 AND entry=?2 AND version=?3
                     ORDER BY active DESC LIMIT 1",
                    params![source.identity(), item.relative.to_string_lossy(), version],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            if let Some((hash, previous, active)) = known
                && let Some(path) = self.matching(&hash, item.size, owner)?
            {
                if !active || path != previous || owner.is_some() {
                    self.record(source.identity(), item, &hash, &path, owner)?;
                }
                return Ok((false, hash));
            }
        }
        // Check plausible local duplicates before allocating or writing staging space.
        // Other sizes and non-seekable phone sources continue to transfer in a single pass.
        let mut reader: Box<dyn Read> = if let (Some(path), Some(size)) =
            (source.local_path(item), item.size)
            && size != 0
            && self.contains_size(size)?
        {
            let mut file = open_stable_file(&path)?;
            let digest = digest_reader(&mut file)?;
            source.verify_read(item)?;
            if let Some(path) = self.matching(&digest, item.size, owner)? {
                self.record(source.identity(), item, &digest, &path, owner)?;
                return Ok((false, digest));
            }
            file.rewind()?;
            Box::new(file)
        } else {
            before_read(&self.db)?;
            source.reader(item)?
        };
        let tmp = TempFile(
            self.stage
                .join(format!("transfer-{}.{}", Uuid::new_v4(), ext)),
        );
        let mut output = File::create_new(&tmp.0)?;
        let mut hash = blake3::Hasher::new();
        let mut size = 0u64;
        let mut buffer = vec![0u8; 256 * 1024];
        loop {
            let count = match reader.read(&mut buffer) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            hash.update(&buffer[..count]);
            size += count as u64;
            written(size)?;
        }
        if item.size.is_some_and(|n| size != n) || size == 0 {
            return Err(IncompleteTransfer {
                received: size,
                expected: item.size,
            }
            .into());
        }
        source.verify_read(item)?;
        output.set_modified(item.modified.unwrap_or(UNIX_EPOCH))?;
        output.sync_all()?;
        drop(output);
        let digest = hash.finalize().to_hex().to_string();
        if let Some(path) = self.matching(&digest, Some(size), owner)? {
            self.record(source.identity(), item, &digest, &path, owner)?;
            return Ok((false, digest));
        }
        self.file_staged(
            source.identity(),
            item,
            &digest,
            &tmp.0,
            companion.as_deref(),
            owner,
            false,
        )
    }

    /// The receipt remains durable until the file and its archive journal are committed.
    pub(super) fn file_receipt(
        &mut self,
        source: &str,
        item: &Item,
        digest: &str,
        data: &Path,
        size: u64,
    ) -> Result<bool> {
        let ext = extension(&item.relative);
        let companion = self.companion_path(source, item, &ext)?;
        let owner = match ext.as_str() {
            "aae" | "xmp" => companion.as_deref(),
            _ => None,
        };
        if let Some(path) = self.matching(digest, Some(size), owner)? {
            self.record(source, item, digest, &path, owner)?;
            return Ok(false);
        }
        self.file_staged(
            source,
            item,
            digest,
            data,
            companion.as_deref(),
            owner,
            true,
        )
        .map(|(added, _)| added)
    }

    fn file_staged(
        &mut self,
        source: &str,
        item: &Item,
        digest: &str,
        staged: &Path,
        companion: Option<&Path>,
        owner: Option<&Path>,
        keep: bool,
    ) -> Result<(bool, String)> {
        let preferred = match companion {
            Some(path) => path.to_path_buf(),
            None => Self::dated_path(item, staged)?,
        };
        let parent = preferred.parent().ok_or("目标缺少父目录")?;
        let mut dir = self.root.clone();
        for part in parent.components() {
            if let std::path::Component::Normal(name) = part {
                dir.push(name);
                real_dir(&dir)?;
            } else {
                return Err("无效的目标子目录".into());
            }
        }
        let mut candidate = preferred.clone();
        let link;
        let publish_from = if keep {
            link = TempFile(self.stage.join(format!(
                "transfer-{}.{}",
                Uuid::new_v4(),
                extension(&item.relative)
            )));
            if fs::hard_link(staged, &link.0).is_err() {
                fs::copy(staged, &link.0)?;
                File::options().write(true).open(&link.0)?.sync_all()?;
            }
            &link.0
        } else {
            staged
        };
        for collision in 0.. {
            let full = self.root.join(&candidate);
            match publish(publish_from, &full) {
                Ok(()) => {
                    self.record(source, item, digest, &candidate.to_string_lossy(), owner)?;
                    return Ok((true, digest.to_owned()));
                }
                Err(e)
                    if e.kind() == io::ErrorKind::AlreadyExists
                        || (e.kind() == io::ErrorKind::PermissionDenied
                            && fs::symlink_metadata(&full).is_ok()) =>
                {
                    if contains_digest(&full, digest)? {
                        self.record(source, item, digest, &candidate.to_string_lossy(), owner)?;
                        return Ok((false, digest.to_owned()));
                    }
                    let suffix = if collision == 0 {
                        digest[..10].to_string()
                    } else {
                        format!("{}-{collision}", &digest[..10])
                    };
                    candidate = parent.join(collision_name(&preferred, &suffix));
                }
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!()
    }
}
