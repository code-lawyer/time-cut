//! The source list and the content audit serve different purposes. A good list
//! survives a failed download. Only a verified content audit can release the phone.
mod historical;
mod exports;
pub(super) use exports::Shortfall;
use super::{
    Report,
    archive::{CopyOrder, Item, Result, ScanEvent, Source},
    inbox::{Received, stored_time},
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

pub(super) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS phone_lists(
        run_id INTEGER PRIMARY KEY, checked_at INTEGER NOT NULL, total INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS phone_list_items(
        run_id INTEGER NOT NULL, ordinal INTEGER NOT NULL, entry TEXT NOT NULL,
        version TEXT, size INTEGER, modified TEXT, digest TEXT,
        PRIMARY KEY(run_id,ordinal)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS phone_list_signature ON phone_list_items(run_id,entry,size,modified);
        CREATE TABLE IF NOT EXISTS phone_usb_original_listings(run_id INTEGER NOT NULL,entry TEXT NOT NULL,
        version TEXT NOT NULL,size INTEGER NOT NULL,modified TEXT NOT NULL,
        PRIMARY KEY(run_id,entry,version,size,modified)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_item_retries(run_id INTEGER NOT NULL,entry TEXT NOT NULL,
        version TEXT NOT NULL,attempted INTEGER NOT NULL,PRIMARY KEY(run_id,entry,version)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_returned_data(
        run_id INTEGER NOT NULL,entry TEXT NOT NULL,version TEXT NOT NULL,reported_size INTEGER NOT NULL,
        returned_size INTEGER NOT NULL,digest TEXT NOT NULL,first_session INTEGER NOT NULL,last_session INTEGER NOT NULL,
        sessions INTEGER NOT NULL,first_controlled INTEGER,last_controlled INTEGER,
        PRIMARY KEY(run_id,entry,version,returned_size,digest)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_list_folders(
        run_id INTEGER NOT NULL,path TEXT NOT NULL,attempted INTEGER NOT NULL DEFAULT 0,
        probing INTEGER NOT NULL DEFAULT 0,checked_at INTEGER,item_count INTEGER NOT NULL DEFAULT 0,fingerprint TEXT,
        PRIMARY KEY(run_id,path)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS phone_folder_items(
        run_id INTEGER NOT NULL,parent TEXT NOT NULL,ordinal INTEGER NOT NULL,entry TEXT NOT NULL,
        version TEXT,size INTEGER,modified TEXT,PRIMARY KEY(run_id,parent,ordinal)) WITHOUT ROWID;",
    )?;
    Ok(())
}

pub(super) struct Member {
    pub item: Item,
    pub receipt: Option<Received>,
    pub ambiguous: bool,
    pub priority: u8,
    pub recorded: Option<String>,
    pub attempted: bool,
}

impl Member {
    pub fn new(item: Item) -> Self {
        Self {
            item,
            receipt: None,
            ambiguous: false,
            priority: 0,
            recorded: None,
            attempted: false,
        }
    }
}

pub(super) fn folder_progress(db: &Connection, run: i64, report: &mut Report) -> Result<()> {
    (report.known_folders,report.listed_folders,report.listed_items)=db.query_row(
        "SELECT COUNT(*),COUNT(checked_at),COALESCE(SUM(item_count),0) FROM phone_list_folders WHERE run_id=?",
        [run],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    Ok(())
}

pub(super) struct FolderPass {
    pub interrupted: HashSet<PathBuf>,
    pub list_first: bool,
}

pub(super) fn unfinished_folders(
    db: &Connection,
    run: i64,
    attempt: u64,
    known: &mut HashSet<PathBuf>,
    ranks: &mut HashMap<PathBuf, i64>,
    priorities: &mut HashMap<PathBuf, u8>,
) -> Result<FolderPass> {
    let mut interrupted = HashSet::new();
    let mut unlisted = Vec::new();
    let mut statement =
        db.prepare("SELECT path,attempted,probing,checked_at IS NULL FROM phone_list_folders
            WHERE run_id=? ORDER BY attempted,path")?;
    let mut rows = statement.query([run])?;
    while let Some(row) = rows.next()? {
        let path = PathBuf::from(row.get::<_, String>(0)?);
        known.insert(path.clone());
        let last = row.get::<_, i64>(1)?;
        if row.get::<_, bool>(3)? {
            unlisted.push((path.clone(), last));
        }
        if row.get::<_, bool>(2)? {
            interrupted.insert(path.clone());
            priorities.insert(path.clone(), 3);
            ranks
                .entry(path)
                .and_modify(|t| *t = (*t).max(last))
                .or_insert(last);
        }
    }
    // A failed resource in a listed folder can end every session before the next
    // directory is even visited. Reserve alternate recovery passes for enumeration.
    // Fresh folders go first, then failed folders in least-recently-attempted order.
    let competing_work = unlisted.len() > 1 || priorities.values().any(|priority| *priority == 0);
    let listing_turn = attempt >= 3 && attempt % 2 == 1;
    let refresh_saved = listing_turn && unlisted.is_empty() && db.query_row(
        "SELECT EXISTS(SELECT 1 FROM phone_list_items WHERE run_id=? AND digest IS NULL)",
        [run], |r| r.get::<_,bool>(0),
    )?;
    let list_first = listing_turn && ((!unlisted.is_empty() && competing_work) || refresh_saved);
    if list_first {
        for rank in priorities.values_mut() { *rank=2; }
        for (path,last) in unlisted {
            priorities.insert(path.clone(),u8::from(last!=0));
            ranks.insert(path,last);
        }
    }
    Ok(FolderPass { interrupted, list_first })
}

/// Retain only complete directory listings. A bad first leaf on one USB session
/// must not discard yesterday's known members or keep other folders at the back.
pub(super) fn save_folder(db: &Connection, run: i64, path: &Path, media: &[Item]) -> Result<bool> {
    let mut parts = Vec::with_capacity(media.len());
    for m in media {
        parts.push(serde_json::to_vec(&(
            m.relative.to_string_lossy(),
            &m.version,
            m.size,
            m.modified.map(stored_time),
        ))?);
    }
    parts.sort_unstable();
    let mut hash = blake3::Hasher::new();
    for bytes in parts {
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    let fingerprint = hash.finalize().to_hex().to_string();
    let parent = path.to_string_lossy();
    let tx = db.unchecked_transaction()?;
    let previous: Option<(Option<String>, u64)> = tx
        .query_row(
            "SELECT fingerprint,item_count FROM phone_list_folders WHERE run_id=? AND path=?",
            params![run, parent],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if media.is_empty() && previous.as_ref().is_some_and(|(_, count)| *count > 0) {
        tx.execute(
            "UPDATE phone_list_folders SET probing=1 WHERE run_id=? AND path=?",
            params![run, parent],
        )?;
        tx.commit()?;
        return Ok(false);
    }
    if previous.as_ref().and_then(|(old, _)| old.as_ref()) != Some(&fingerprint) {
        tx.execute(
            "DELETE FROM phone_folder_items WHERE run_id=? AND parent=?",
            params![run, parent],
        )?;
        let mut insert=tx.prepare("INSERT INTO phone_folder_items(run_id,parent,ordinal,entry,version,size,modified) VALUES(?,?,?,?,?,?,?)")?;
        for (i, m) in media.iter().enumerate() {
            insert.execute(params![
                run,
                parent,
                i as i64,
                m.relative.to_string_lossy(),
                m.version,
                m.size,
                m.modified.map(stored_time)
            ])?;
        }
    }
    let now = chrono::Utc::now().timestamp_millis();
    tx.execute("INSERT INTO phone_list_folders(run_id,path,checked_at,item_count,fingerprint) VALUES(?,?,?,?,?)
        ON CONFLICT(run_id,path) DO UPDATE SET checked_at=excluded.checked_at,item_count=excluded.item_count,fingerprint=excluded.fingerprint,probing=0",
        params![run,parent,now,media.len() as u64,fingerprint])?;
    tx.commit()?;
    Ok(true)
}

pub(super) fn restore(db: &Connection, run: i64, report: &mut Report, try_historical: bool) -> Result<Option<historical::HistoricalItem>> {
    folder_progress(db, run, report)?;
    let summary = db
        .query_row(
            "SELECT total,checked_at,(SELECT COUNT(digest) FROM phone_list_items WHERE run_id=?1)
        FROM phone_lists WHERE run_id=?1",
            [run],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((total, checked, covered)) = summary {
        report.list_total = Some(total);
        report.list_at = Some(checked);
        report.list_covered = covered;
        report.source_mismatches = db.query_row(
            "SELECT COUNT(DISTINCT m.ordinal) FROM phone_returned_data o
             JOIN phone_list_items m INDEXED BY phone_list_signature ON m.run_id=o.run_id AND m.entry=o.entry
               AND m.version=o.version AND m.size=o.reported_size WHERE o.run_id=? AND m.digest IS NULL",
            [run], |r| r.get(0),
        )?;
        return historical::restore(db, run, report, try_historical);
    }
    Ok(None)
}

#[derive(Default)]
pub(super) struct Receipts {
    lengths: HashMap<String, HashSet<u64>>,
    versions: HashMap<(String, String, u64), Option<String>>,
    originals: exports::Originals,
}

pub(super) const POSSIBLE_USB_EXPORT: u8 = 3;

#[derive(Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Coverage {
    pub matched: u64,
    pub uncertain: u64,
    pub missing: u64,
    #[serde(default)]
    pub possible_jpeg_exports: u64,
    #[serde(default)]
    pub possible_mov_exports: u64,
    #[serde(default)]
    pub possible_export_bytes: u64,
}

impl Coverage {
    pub fn total(folders: &HashMap<PathBuf, Self>) -> Self {
        folders.values().fold(Self::default(), |mut sum, folder| {
            sum.matched += folder.matched;
            sum.uncertain += folder.uncertain;
            sum.missing += folder.missing;
            sum.possible_jpeg_exports += folder.possible_jpeg_exports;
            sum.possible_mov_exports += folder.possible_mov_exports;
            sum.possible_export_bytes = sum.possible_export_bytes.saturating_add(folder.possible_export_bytes);
            sum
        })
    }
}

impl Receipts {
    pub fn load(db: &Connection, source: &str) -> Result<Self> {
        let mut receipts = Self::default();
        let mut query = db.prepare("SELECT entry,version,size,digest FROM phone_inbox WHERE source=?1
        UNION ALL SELECT s.entry,s.version,f.size,s.digest FROM sources s JOIN files f ON s.path=f.path WHERE s.source=?1
        UNION ALL SELECT v.entry,v.version,f.size,v.digest FROM source_versions v JOIN files f ON v.path=f.path WHERE v.source=?1")?;
        let mut rows = query.query([source])?;
        while let Some(row) = rows.next()? {
            receipts.record(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
        }
        let mut originals = db.prepare("SELECT n.entry,n.version,n.size,n.modified FROM phone_usb_original_listings n
            JOIN phone_runs r ON r.id=n.run_id WHERE r.source=?1
            UNION ALL SELECT m.entry,m.version,m.size,m.modified FROM phone_list_items m
            JOIN phone_runs r ON m.run_id=r.id JOIN phone_lists l ON l.run_id=r.id WHERE r.source=?1
            AND m.version IS NOT NULL AND m.size>0 AND m.modified IS NOT NULL
            AND (lower(m.entry) LIKE '%.heic' OR lower(m.entry) LIKE '%.heif' OR lower(m.entry) LIKE '%.mov')")?;
        let mut listed = originals.query([source])?;
        while let Some(row) = listed.next()? {
            receipts.originals.observe(Path::new(row.get_ref(0)?.as_str()?), row.get_ref(1)?.as_str()?, row.get(2)?, row.get_ref(3)?.as_str()?);
        }
        Ok(receipts)
    }

    fn record(&mut self, path: String, version: Option<String>, size: u64, digest: String) {
        self.lengths.entry(path.clone()).or_default().insert(size);
        let Some(version) = version.filter(|v| !v.is_empty()) else {
            return;
        };
        self.originals.record(Path::new(&path), &version, size, &digest);
        self.versions
            .entry((path, version, size))
            .and_modify(|old| {
                if old.as_ref() != Some(&digest) {
                    *old = None;
                }
            })
            .or_insert(Some(digest));
    }
    pub fn received(&mut self, item: &Item, receipt: &Received) {
        self.record(
            item.relative.to_string_lossy().into_owned(),
            item.version.clone(),
            receipt.size,
            receipt.digest.clone(),
        );
    }
    pub fn has_candidate(&self, item: &Item) -> bool {
        self.pending_priority(item) != 0
    }
    pub fn pending_priority(&self, item: &Item) -> u8 {
        // A previous export can share this name and length while its bytes and
        // object ID differ. Only match_members can promote an exact receipt.
        if self.originals.may_be_export(item) {
            return POSSIBLE_USB_EXPORT;
        }
        let same_path = self.lengths
            .get(item.relative.to_string_lossy().as_ref())
            .is_some_and(|sizes| item.size.is_none_or(|s| sizes.contains(&s)));
        if same_path { 2 } else { 0 }
    }
    fn coverage(&self, mut members: Vec<Member>) -> Coverage {
        self.match_members(&mut members);
        let mut coverage = Coverage::default();
        for member in members {
            match member.priority {
                0 => coverage.missing += 1,
                1 => coverage.matched += 1,
                POSSIBLE_USB_EXPORT => {
                    coverage.uncertain += 1;
                    if member.item.relative.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("mov")) {
                        coverage.possible_mov_exports += 1;
                    } else { coverage.possible_jpeg_exports += 1; }
                    coverage.possible_export_bytes = coverage.possible_export_bytes.saturating_add(member.item.size.unwrap_or(0));
                }
                _ => coverage.uncertain += 1,
            }
        }
        coverage
    }

    pub fn folder_coverage(&self, items: &[Item]) -> Coverage {
        self.coverage(items.iter().cloned().map(Member::new).collect())
    }

    /// Recompute against today's receipts, including partial listings left by older builds.
    pub fn saved_folders(&self, db: &Connection, run: i64) -> Result<HashMap<PathBuf, Coverage>> {
        let mut by_folder = HashMap::new();
        let mut query=db.prepare("SELECT parent,entry,version,size FROM phone_folder_items WHERE run_id=? ORDER BY parent,ordinal")?;
        let mut rows = query.query([run])?;
        let mut previous = None;
        let mut members = Vec::new();
        while let Some(row) = rows.next()? {
            let parent: String = row.get(0)?;
            if previous.as_ref() != Some(&parent) {
                if let Some(old) = previous.replace(parent) {
                    by_folder.insert(
                        PathBuf::from(old),
                        self.coverage(std::mem::take(&mut members)),
                    );
                }
            }
            members.push(Member::new(Item {
                id: String::new(),
                relative: PathBuf::from(row.get::<_, String>(1)?),
                version: row.get(2)?,
                size: row.get(3)?,
                modified: None,
            }));
        }
        if let Some(parent) = previous {
            by_folder.insert(PathBuf::from(parent), self.coverage(members));
        }
        Ok(by_folder)
    }
    /// A reused display path or a conflicting strong version requires reading the content.
    pub fn match_members(&self, members: &mut [Member]) {
        let mut counts = HashMap::new();
        for member in members.iter() {
            if let Some(version) = member.item.version.as_deref() {
                *counts
                    .entry((member.item.relative.clone(), version.to_owned()))
                    .or_insert(0u32) += 1;
            }
        }
        for member in members {
            let item = &member.item;
            let entry = item.relative.to_string_lossy();
            member.recorded = None;
            member.ambiguous = false;
            if let (Some(version), Some(size)) = (&item.version, item.size) {
                let known = self
                    .versions
                    .get(&(entry.to_string(), version.clone(), size));
                member.ambiguous = counts
                    .get(&(item.relative.clone(), version.clone()))
                    .copied()
                    .unwrap_or(0)
                    > 1
                    || known.is_some_and(Option::is_none);
                if !member.ambiguous {
                    member.recorded = known.cloned().flatten();
                }
            }
            if let Some(receipt) = &member.receipt {
                member.recorded = Some(receipt.digest.clone());
            }
            // A different USB representation may explain a changed filename and size.
            // It can wait while original files are received; it cannot supply a digest.
            member.priority = if member.recorded.is_some() {
                1
            } else {
                self.pending_priority(item)
            };
        }
    }
}

pub(super) fn publish(
    db: &Connection,
    run: i64,
    members: &[Member],
    report: &mut Report,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let tx = db.unchecked_transaction()?;
    tx.execute("INSERT OR IGNORE INTO phone_usb_original_listings(run_id,entry,version,size,modified)
        SELECT m.run_id,m.entry,m.version,m.size,m.modified FROM phone_list_items m JOIN phone_lists l ON l.run_id=m.run_id
        WHERE m.run_id=? AND m.version IS NOT NULL AND m.size>0 AND m.modified IS NOT NULL
        AND (lower(m.entry) LIKE '%.heic' OR lower(m.entry) LIKE '%.heif' OR lower(m.entry) LIKE '%.mov')", [run])?;
    tx.execute("DELETE FROM phone_list_items WHERE run_id=?", [run])?;
    {
        let mut insert = tx.prepare("INSERT INTO phone_list_items(run_id,ordinal,entry,version,size,modified,digest) VALUES(?,?,?,?,?,?,?)")?;
        for (ordinal, member) in members.iter().enumerate() {
            let item = &member.item;
            insert.execute(params![
                run,
                ordinal as i64,
                item.relative.to_string_lossy(),
                item.version,
                item.size,
                item.modified.map(stored_time),
                member.recorded
            ])?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO phone_lists(run_id,checked_at,total) VALUES(?,?,?)",
        params![run, now, members.len() as u64],
    )?;
    tx.commit()?;
    report.list_at = Some(now);
    report.list_total = Some(members.len() as u64);
    report.list_covered = members.iter().filter(|m| m.recorded.is_some()).count() as u64;
    Ok(())
}

/// Folder checkpoints serve interrupted scans. Once a run is released, the
/// complete list and its content manifest are the permanent item-level evidence.
/// Remove only checkpoint rows also present in a verified final list. Retain
/// earlier differing observations, folder summaries, and every open run.
pub(super) fn compact_folder_checkpoints(db: &Connection, source: &str) -> Result<()> {
    db.execute("WITH unambiguous AS (
        SELECT run_id,entry,version,size,modified,MIN(parent) AS parent,MIN(ordinal) AS ordinal
        FROM phone_folder_items WHERE run_id IN
            (SELECT id FROM phone_runs WHERE source=? AND safe_at IS NOT NULL)
        GROUP BY run_id,entry,version,size,modified HAVING COUNT(*)=1)
        DELETE FROM phone_folder_items WHERE (run_id,parent,ordinal) IN (
            SELECT f.run_id,f.parent,f.ordinal FROM unambiguous f WHERE 1=(
                SELECT COUNT(*) FROM phone_list_items m INDEXED BY phone_list_signature CROSS JOIN phone_manifest p
                ON p.run_id=m.run_id AND p.ordinal=m.ordinal AND p.digest=m.digest
                    AND p.entry=m.entry AND p.version IS m.version AND (m.size IS NULL OR p.size=m.size)
                WHERE m.run_id=f.run_id AND m.entry=f.entry AND m.size IS f.size
                    AND m.modified IS f.modified AND m.version IS f.version))", [source])?;
    Ok(())
}

pub(super) fn checkpoint(
    db: &Connection,
    run: i64,
    changes: &mut Vec<(usize, Option<String>)>,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let tx = db.unchecked_transaction()?;
    {
        let mut update =
            tx.prepare("UPDATE phone_list_items SET digest=? WHERE run_id=? AND ordinal=?")?;
        for (ordinal, digest) in changes.iter() {
            if update.execute(params![digest, run, *ordinal as i64])? != 1 {
                return Err("原件清单已变化".into());
            }
        }
    }
    tx.commit()?;
    changes.clear();
    Ok(())
}

/// Re-list metadata on the still-open session after the long payload transfers.
/// This checks substitutions and edits even if the item count stayed identical.
pub(super) fn confirm(
    source: &dyn Source,
    order: &CopyOrder,
    members: &[Member],
    mut progress: impl FnMut(u64, u64) -> Result<()>,
) -> Result<()> {
    let mut expected: HashMap<&str, &Item> = members
        .iter()
        .map(|m| (m.item.id.as_str(), &m.item))
        .collect();
    if expected.len() != members.len() {
        return Err("手机清单有重复的设备标识，将重新盘点".into());
    }
    let total = members.len() as u64;
    progress(0, total)?;
    source.visit_for_copy(order, &mut |event| {
        match event {
            ScanEvent::Ready(item) => {
                let same = expected.remove(item.id.as_str()).is_some_and(|old| {
                    old.relative == item.relative
                        && old.version == item.version
                        && old.size == item.size
                        && old.modified == item.modified
                });
                if !same {
                    return Err("接收期间手机素材发生变化，将重新盘点".into());
                }
                progress(total - expected.len() as u64, total)?;
            }
            ScanEvent::Unreadable(message) => return Err(message.into()),
            ScanEvent::Inspected { .. }
            | ScanEvent::DirectoryFound(_)
            | ScanEvent::DirectoryStart(_)
            | ScanEvent::DirectoryDone { .. }
            | ScanEvent::DirectoryCheck { .. }
            | ScanEvent::CompanionFound(_) => progress(total - expected.len() as u64, total)?,
        }
        Ok(())
    })?;
    if !expected.is_empty() {
        return Err("手机复核清单少于接收清单，将重新盘点".into());
    }
    Ok(())
}

#[cfg(test)]
mod export_tests {
    use super::*;

    #[test]
    fn a_changed_usb_export_is_queued_after_missing_originals_without_granting_a_receipt() {
        let mut receipts = Receipts::default();
        receipts.record("DCIM/photo.HeIc".into(), Some("apple/object/8/1000".into()), 8, "raw".into());
        let jpeg = Item {
            id: "handle".into(), relative: "DCIM/photo.JPG".into(), size: Some(12),
            version: Some("apple/object/12/1000".into()), modified: None,
        };
        let mut independently_captured = jpeg.clone();
        independently_captured.version = Some("other-object/12/1000".into());
        let mut edited = jpeg.clone();
        edited.version = Some("apple/object/12/1001".into());
        let mut moved = jpeg.clone();
        moved.relative = "elsewhere/photo.JPG".into();
        let mut members: Vec<_> = [jpeg.clone(), independently_captured, edited, moved]
            .into_iter().map(Member::new).collect();
        receipts.match_members(&mut members);
        assert!(receipts.has_candidate(&jpeg), "a known HEIC should not re-enter the missing-original queue as JPG");
        assert!(members[0].priority > members[1].priority);
        assert!(members.iter().all(|m| m.recorded.is_none()));
        assert!(members[1..].iter().all(|m| m.priority == 0));
        receipts.record("DCIM/photo.JPG".into(), jpeg.version.clone(), 12, "jpeg-content".into());
        receipts.match_members(&mut members);
        assert_eq!(members[0].recorded.as_deref(), Some("jpeg-content"));
    }
}
