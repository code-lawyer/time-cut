//! Bridge the iPhone's filesystem checksum to the archive's BLAKE3 receipts.
//! The cache describes bytes, never a filename. A hit still needs a live local original.
use super::{Archive, Location, Result, verified_local};
use crate::ingest::archive::{media_directory, open_stable_file, real_dir};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

pub(in crate::ingest) struct Index {
    db: Connection,
    indexed_sizes: RefCell<HashSet<u64>>,
}

struct CheckedHash {
    sha1: [u8; 20],
    read_from_disk: bool,
}

impl Index {
    fn open(root: &Path) -> Result<Self> {
        let dir = root.join(".time-cut");
        real_dir(&dir)?;
        let db = Connection::open_with_flags(
            dir.join("iphone-checksums.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS fingerprints(digest TEXT PRIMARY KEY, size INTEGER NOT NULL,
            sha1 BLOB NOT NULL CHECK(length(sha1)=20)) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS iphone_hash ON fingerprints(size,sha1);
            CREATE TABLE IF NOT EXISTS backing_files(path TEXT PRIMARY KEY,digest TEXT NOT NULL,
            changed INTEGER NOT NULL,volume BLOB NOT NULL,file_id BLOB NOT NULL) WITHOUT ROWID;",
        )?;
        Ok(Self {
            db,
            indexed_sizes: Default::default(),
        })
    }

    fn known(&self, digest: &str, size: u64) -> Result<bool> {
        Ok(self
            .db
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM fingerprints WHERE digest=? AND size=?)")?
            .query_row(params![digest, size], |r| r.get(0))?)
    }

    pub(super) fn remember(&self, digest: &str, size: u64, remote: &[u8; 20]) -> Result<()> {
        self.db
            .prepare_cached("INSERT OR IGNORE INTO fingerprints VALUES(?,?,?)")?
            .execute(params![digest, size, remote.as_slice()])?;
        Ok(())
    }

    fn indexed(&self, size: u64, sha: &[u8; 20]) -> Result<Vec<String>> {
        Ok(self
            .db
            .prepare_cached("SELECT digest FROM fingerprints WHERE size=? AND sha1=? LIMIT 2")?
            .query_map(params![size, sha.as_slice()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn local_checksum(
        &self,
        root: &Path,
        location: &Location,
        digest: &str,
        size: u64,
    ) -> Result<Option<CheckedHash>> {
        let path = location.path(root)?;
        let token = super::change_token(&path);
        let relative = path.strip_prefix(root)?.to_string_lossy();
        if let Some((changed, volume, id)) = token {
            let saved=self.db.prepare_cached("SELECT f.sha1 FROM backing_files b JOIN fingerprints f ON f.digest=b.digest
                WHERE b.path=? AND b.digest=? AND f.size=? AND b.changed=? AND b.volume=? AND b.file_id=?")?
                .query_row(params![relative,digest,size,changed,volume.to_be_bytes().as_slice(),id.as_slice()],|r|r.get::<_,[u8;20]>(0)).optional()?;
            if saved.is_some()
                && fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == size)
                && path.canonicalize().is_ok_and(|p| p.starts_with(root))
            {
                return Ok(saved.map(|sha1| CheckedHash {
                    sha1,
                    read_from_disk: false,
                }));
            }
        }
        let Some(sha) = measure(root, location, digest, size)? else {
            return Ok(None);
        };
        if let Some((changed, volume, id)) = token {
            if super::change_token(&path) != token {
                return Ok(None);
            }
            self.db
                .prepare_cached("INSERT OR REPLACE INTO backing_files VALUES(?,?,?,?,?)")?
                .execute(params![
                    relative,
                    digest,
                    changed,
                    volume.to_be_bytes().as_slice(),
                    id.as_slice()
                ])?;
        }
        self.remember(digest, size, &sha)?;
        Ok(Some(CheckedHash {
            sha1: sha,
            read_from_disk: true,
        }))
    }
}

pub(super) fn index(archive: &Archive) -> Result<&Index> {
    if archive.content_index.get().is_none() {
        let fresh = Index::open(&archive.root)?;
        let _ = archive.content_index.set(fresh);
    }
    Ok(archive.content_index.get().expect("index was initialized"))
}

// Read one stable file only once to calculate both algorithms. A catalogue row alone
// cannot create a fingerprint, and an edited file cannot be reused for a different one.
fn measure(
    root: &Path,
    location: &Location,
    expected: &str,
    size: u64,
) -> Result<Option<[u8; 20]>> {
    let path = location.path(root)?;
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.len() != size || !path.canonicalize()?.starts_with(root) {
        return Ok(None);
    }
    let mut reader = open_stable_file(&path)?;
    let mut b3 = blake3::Hasher::new();
    let mut sha = Sha1::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut read = 0u64;
    loop {
        let n = match reader.read(&mut buffer) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            res => res?,
        };
        if n == 0 {
            break;
        }
        b3.update(&buffer[..n]);
        sha.update(&buffer[..n]);
        read += n as u64;
    }
    Ok(
        (read == size && b3.finalize().to_hex().as_str() == expected)
            .then(|| sha.finalize().into()),
    )
}

fn reference(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, u64, Location)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        Location {
            blob: row.get(2)?,
            archived: row.get(3)?,
        },
    ))
}

/// A same-length file is only a comparison buffer. The complete phone payload and
/// a verified local digest are still required before a receipt can reuse it.
pub(super) fn comparison_file(archive: &Archive, size: u64) -> Result<Option<fs::File>> {
    let hint = archive.db.prepare_cached(
        "SELECT NULL,path FROM files WHERE size=?1 UNION ALL
         SELECT blob,archive_path FROM phone_inbox WHERE size=?1 LIMIT 1",
    )?.query_row([size], |r| Ok(Location { blob: r.get(0)?, archived: r.get(1)? })).optional()?;
    let Some(location) = hint else { return Ok(None) };
    let path = location.path(&archive.root)?;
    if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == size)
        || !path.canonicalize().is_ok_and(|p| p.starts_with(&archive.root)) {
        return Ok(None);
    }
    Ok(open_stable_file(&path).ok())
}

pub(super) fn by_digest(archive: &Archive, size: u64, digest: &str) -> Result<Option<Location>> {
    let mut refs = archive.db.prepare_cached(
        "SELECT NULL,path FROM files WHERE digest=?1 AND size=?2
        UNION ALL SELECT blob,archive_path FROM phone_inbox WHERE digest=?1 AND size=?2",
    )?;
    let locations = refs.query_map(params![digest, size], |r| {
        Ok(Location {
            blob: r.get(0)?,
            archived: r.get(1)?,
        })
    })?;
    for location in locations {
        let location = location?;
        match verified_local(archive, &location.path(&archive.root)?, digest) {
            Ok(true) => return Ok(Some(location)),
            Ok(false) => (),
            Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|io| {
                io.kind() == std::io::ErrorKind::PermissionDenied || matches!(io.raw_os_error(), Some(32 | 33))
            }) => (), // Another copy may be readable while this one is open in an editor.
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

pub(super) fn matching(
    archive: &Archive,
    size: u64,
    sha: &[u8; 20],
) -> Result<Option<(String, Location)>> {
    let cache = index(archive)?;
    if !cache.indexed_sizes.borrow().contains(&size) {
        let mut refs = archive.db.prepare_cached(
            "SELECT digest,size,NULL,path FROM files WHERE size=?1 AND length(digest)=64
            UNION SELECT digest,size,blob,archive_path FROM phone_inbox WHERE size=?1",
        )?;
        for row in refs.query_map([size], reference)? {
            let (digest, _, location) = row?;
            if !cache.known(&digest, size)? {
                cache.local_checksum(&archive.root, &location, &digest, size)?;
            }
        }
        cache.indexed_sizes.borrow_mut().insert(size);
    }
    let digests = cache.indexed(size, sha)?;
    // Known SHA-1 collisions require reading and identifying the actual bytes with BLAKE3.
    if let [digest] = digests.as_slice() {
        if let Some(location) = by_digest(archive, size, digest)? {
            return Ok(Some((digest.clone(), location)));
        }
    }
    Ok(None)
}

#[derive(Default, Serialize)]
pub(in crate::ingest) struct IndexProgress {
    pub total: usize,
    pub cached: usize,
    pub verified: usize,
    pub unavailable: usize,
    pub read_bytes: u64,
}

type Locations = BTreeMap<(String, u64), Vec<Location>>;

fn locations(root: &Path) -> Result<Locations> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let db = Connection::open_with_flags(root.join(".time-cut/imports.sqlite3"), flags)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let inbox_exists = db
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type='table' AND name='phone_inbox'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let sql = if inbox_exists {
        "SELECT digest,size,NULL,path FROM files WHERE length(digest)=64
        UNION SELECT digest,size,blob,archive_path FROM phone_inbox WHERE length(digest)=64"
    } else {
        "SELECT digest,size,NULL,path FROM files WHERE length(digest)=64"
    };
    let mut content = BTreeMap::<(String, u64), Vec<Location>>::new();
    for row in db.prepare(sql)?.query_map([], reference)? {
        let (digest, size, location) = row?;
        content.entry((digest, size)).or_default().push(location);
    }
    drop(db);
    Ok(content)
}

/// Populate an existing gallery offline without taking the active importer's archive lock.
pub(in crate::ingest) fn prepare(
    root: &Path,
    mut progress: impl FnMut(&IndexProgress),
) -> Result<IndexProgress> {
    let root = media_directory(root)?;
    let content = locations(&root)?;
    let cache = Index::open(&root)?;
    let mut done = IndexProgress {
        total: content.len(),
        ..Default::default()
    };
    progress(&done);
    for ((digest, size), locations) in content {
        let mut available = false;
        for loc in locations {
            if let Some(checked) = cache.local_checksum(&root, &loc, &digest, size)? {
                if checked.read_from_disk {
                    done.verified += 1;
                    done.read_bytes += size;
                } else {
                    done.cached += 1;
                }
                available = true;
                break;
            }
        }
        done.unavailable += usize::from(!available);
        if (done.verified + done.cached + done.unavailable) % 128 == 0 {
            progress(&done);
        }
    }
    progress(&done);
    Ok(done)
}

/// A read-only snapshot of catalogue references. It can run alongside WPD reception.
/// Cache hits still require a verified, present backing file before counting a match.
pub(in crate::ingest) struct Catalog {
    root: PathBuf,
    index: Index,
    locations: Locations,
    by_size: HashMap<u64, Vec<String>>,
}

impl Catalog {
    pub(in crate::ingest) fn open(root: &Path) -> Result<Self> {
        let root = media_directory(root)?;
        let locations = locations(&root)?;
        let mut by_size = HashMap::<u64, Vec<String>>::new();
        for (digest, size) in locations.keys() {
            by_size.entry(*size).or_default().push(digest.clone());
        }
        let index = Index::open(&root)?;
        Ok(Self {
            root,
            index,
            locations,
            by_size,
        })
    }

    pub(in crate::ingest) fn verified(
        &mut self,
        size: u64,
        sha: &[u8; 20],
    ) -> Result<Option<String>> {
        if let Some(digests) = self.by_size.remove(&size) {
            for digest in digests {
                if self.index.known(&digest, size)? {
                    continue;
                }
                for loc in &self.locations[&(digest.clone(), size)] {
                    if self
                        .index
                        .local_checksum(&self.root, loc, &digest, size)?
                        .is_some()
                    {
                        break;
                    }
                }
            }
        }
        let candidates = self.index.indexed(size, sha)?;
        let [digest] = candidates.as_slice() else {
            return Ok(None);
        };
        if let Some(locations) = self.locations.get(&(digest.clone(), size)) {
            for location in locations {
                if self
                    .index
                    .local_checksum(&self.root, location, digest, size)?
                    .is_some_and(|checked| &checked.sha1 == sha)
                {
                    return Ok(Some(digest.clone()));
                }
            }
        }
        Ok(None)
    }
}
