use super::*;
use crate::ingest::archive::SavedFiles;
mod controlled;

struct Target {
    ordinal: i64,
    item: Item,
    reference: Option<String>,
}

const BATCH_SIZE: usize = 512;

// The optimizer can scan the entire run for each candidate. Pin the path index
// so that the ambiguity check only visits siblings with the same display path.
const SAVED_TARGETS: &str =
    "SELECT m.ordinal,m.entry,m.version,m.size,m.modified,m.digest,t.attempted IS NOT NULL FROM phone_list_items m
         LEFT JOIN phone_item_retries t ON t.run_id=m.run_id AND t.entry=m.entry AND t.version=m.version
         WHERE m.run_id=? AND m.digest IS NULL AND m.size>0 AND m.version IS NOT NULL AND m.modified IS NOT NULL
         AND NOT EXISTS(SELECT 1 FROM phone_list_items s INDEXED BY phone_list_signature WHERE s.run_id=m.run_id AND s.entry=m.entry
             AND s.ordinal<>m.ordinal AND (s.version=m.version OR (s.size=m.size AND s.modified=m.modified)))
         ORDER BY COALESCE(t.attempted,0),m.ordinal";

const RECENT_REFERENCES: &str =
    "SELECT m.ordinal,m.entry,m.version,m.size,m.modified,m.digest FROM phone_item_retries t
         JOIN phone_list_items m INDEXED BY phone_list_signature ON m.run_id=t.run_id AND m.entry=t.entry AND m.version=t.version
         WHERE t.run_id=? AND m.digest IS NOT NULL AND m.size BETWEEN 1 AND 262144 AND m.modified IS NOT NULL
         AND NOT EXISTS(SELECT 1 FROM phone_list_items s INDEXED BY phone_list_signature WHERE s.run_id=m.run_id AND s.entry=m.entry
             AND s.ordinal<>m.ordinal AND (s.version=m.version OR (s.size=m.size AND s.modified=m.modified)))
         ORDER BY t.attempted DESC LIMIT 16";

fn target_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Target> {
    let raw: String = row.get(4)?;
    let modified = inbox::read_time(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e)
    })?;
    Ok(Target {
        ordinal: row.get(0)?,
        item: Item {
            id: String::new(),
            relative: PathBuf::from(row.get::<_, String>(1)?),
            version: row.get(2)?,
            size: row.get(3)?,
            modified: Some(modified),
        },
        reference: row.get(5)?,
    })
}

fn rows(db: &Connection, run: i64, sql: &str) -> Result<Vec<Target>> {
    let mut q = db.prepare_cached(sql)?;
    Ok(q.query_map([run], target_from_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// A phone can change every object ID at once. Interleave untried files that
/// have no matching local path and size with the backlog of possible backups.
/// A failed file goes back into normal, least-recently-attempted retry order.
fn queue_batch(
    db: &Connection,
    run: i64,
    receipts: &inventory::Receipts,
    missing_first: bool,
) -> Result<Vec<Target>> {
    let mut select = db.prepare_cached(SAVED_TARGETS)?;
    let mut rows = select.query([run])?;
    let (mut missing, mut existing, mut exports) = (Vec::new(), Vec::new(), Vec::new());
    let mut untried_missing = 0;
    while let Some(row) = rows.next()? {
        let target = target_from_row(row)?;
        let priority = receipts.pending_priority(&target.item);
        if priority == inventory::POSSIBLE_USB_EXPORT {
            if exports.len() < BATCH_SIZE { exports.push(target); }
        } else if priority != 0 {
            if existing.len() < BATCH_SIZE {
                existing.push(target);
            }
        } else if missing.len() < BATCH_SIZE {
            untried_missing += usize::from(!row.get::<_, bool>(6)?);
            missing.push(target);
        }
        if missing.len() == BATCH_SIZE && existing.len() == BATCH_SIZE {
            break;
        }
    }
    // Resolving an object while the camera folder is empty also counts as an
    // attempt. Keep giving these files a turn after a long outage, while letting
    // the other group make uninterrupted progress if a bad export stops a read.
    let retries = if untried_missing > 0 {
        missing.split_off(untried_missing)
    } else {
        Vec::new()
    };
    let (mut first, mut second) = if missing_first {
        (missing.into_iter(), existing.into_iter())
    } else {
        (existing.into_iter(), missing.into_iter())
    };
    if untried_missing == 0 {
        return Ok(first.chain(second).chain(exports).take(BATCH_SIZE).collect());
    }
    let mut batch = Vec::new();
    while batch.len() < BATCH_SIZE {
        let Some(next) = first.next().or_else(|| second.next()) else {
            break;
        };
        batch.push(next);
        std::mem::swap(&mut first, &mut second);
    }
    batch.extend(retries.into_iter().take(BATCH_SIZE - batch.len()));
    batch.extend(exports.into_iter().take(BATCH_SIZE - batch.len()));
    Ok(batch)
}

/// Use two small, distinct files already verified on an earlier pass to check
/// which bytes the driver returns on this connection. First make progress on
/// pending files so that an unreadable control cannot starve the entire queue.
fn targets(
    db: &Connection,
    run: i64,
    receipts: &inventory::Receipts,
    missing_first: bool,
) -> Result<Vec<Target>> {
    let mut pending = queue_batch(db, run, receipts, missing_first)?;
    if pending.len() >= 16 {
        let mut seen = HashSet::new();
        let mut controls = rows(db, run, RECENT_REFERENCES)?;
        controls.retain(|target| seen.insert(target.reference.clone()));
        if controls.len() >= 2 {
            pending.splice(8..8, controls);
        }
    }
    Ok(pending)
}

/// A targeted pass can add receipts but can never issue the unplug notice.
/// A normal directory walk and independent recheck still own that decision.
pub(super) fn resume<Guard>(
    archive: &Archive,
    run: i64,
    source: &dyn Source,
    resolver: &dyn SavedFiles,
    receipts: &inventory::Receipts,
    queue: &mut PhoneQueue,
    report: &mut Report,
    active: &mut impl FnMut() -> Result<()>,
    destination: &mut impl FnMut() -> Result<Guard>,
    publish: &mut impl FnMut(&Report, bool),
) -> Result<bool> {
    let db = &archive.db;
    let start = Instant::now();
    let remaining = report
        .list_total
        .unwrap_or(0)
        .saturating_sub(report.list_covered);
    let witness_turn = report.receive_attempt % if remaining <= 256 { 2 } else { 8 } == 0;
    let isolated = if witness_turn {
        controlled::prepare(db, run)?
    } else {
        None
    };
    let reference_first = isolated.is_some();
    let batch = match isolated {
        Some(batch) => batch,
        // Alternate the first file so a bad export cannot end every session
        // before the other group has had a turn.
        None => targets(db, run, receipts, report.receive_attempt % 2 == 0)?,
    };
    let mut recovery = if reference_first {
        inbox::RecoveryCheck::for_controlled_target()
    } else {
        inbox::RecoveryCheck::for_saved_list()
    };
    let mut exports = ExportBatch::new(report, batch.iter().filter(|t| t.reference.is_none()
        && receipts.pending_priority(&t.item) == inventory::POSSIBLE_USB_EXPORT).filter_map(|t| t.item.size));
    let mut unavailable = 0;
    for target in batch {
        if start.elapsed() >= Duration::from_secs(90) || unavailable >= 4 {
            break;
        }
        active()?;
        if target.reference.is_none() && exports.defer(&archive.root, receipts.pending_priority(&target.item), report) {
            continue;
        }
        let _guard = destination()?;
        let expected = target.item;
        if !source.album_path_in_scope(&expected.relative) { continue; }
        if reference_first && target.reference.is_none() && !recovery.is_controlled_target() {
            break;
        }
        if let Some(hash) = &target.reference {
            if !recovery.wants_reference(hash, expected.size) {
                continue;
            }
        } else {
            db.prepare_cached("INSERT OR REPLACE INTO phone_item_retries VALUES(?,?,?,?)")?
                .execute(params![
                    run,
                    expected.relative.to_string_lossy(),
                    expected.version,
                    chrono::Utc::now().timestamp_micros()
                ])?;
        }
        let item = match resolver.resolve(&expected) {
            Ok(Some(item))
                if item.relative == expected.relative
                    && item.version == expected.version
                    && item.size == expected.size
                    && item.modified == expected.modified =>
            {
                item
            }
            #[cfg(windows)]
            Err(e) if super::super::wpd::interruption(e.as_ref()).is_some() => return Err(e),
            _ => {
                unavailable += 1;
                continue;
            }
        };
        unavailable = 0;
        let mut member = inventory::Member::new(item);
        receipts.match_members(std::slice::from_mut(&mut member));
        if target.reference.is_some() && (member.ambiguous || member.recorded != target.reference) {
            continue;
        }
        queue.discover(db, &member.item)?;
        report.scanned += 1;
        report.discovered += 1;
        let receipt = receive_one(
            archive,
            run,
            source,
            &member.item,
            !member.ambiguous,
            &mut recovery,
            queue,
            report,
            active,
            publish,
        );
        if reference_first && target.reference.is_some() {
            if receipt.as_ref().is_ok_and(Option::is_some)
                && !source.confirms_returned_object(&member.item)?
            {
                return Err(inbox::WrongObject.into());
            }
        }
        if target.reference.is_none()
            && let Ok(Some(r)) = &receipt
        {
            let added = db
                .prepare_cached(
                    "UPDATE phone_list_items SET digest=?
                WHERE run_id=? AND ordinal=? AND digest IS NULL",
                )?
                .execute(params![r.digest, run, target.ordinal])? as u64;
            report.list_covered += added;
            report.newly_verified += added;
        }
        save_progress(db, run, report)?;
        receipt?;
        publish(report, false);
    }
    if report.scanned == 0 {
        return Ok(false);
    }
    report.phase = "仍有素材待补".into();
    publish(report, true);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_a_batch_does_not_rescan_the_whole_manifest_for_every_file() {
        let db = Connection::open_in_memory().unwrap();
        inventory::initialize(&db).unwrap();
        db.execute_batch("WITH RECURSIVE files(n) AS (
            SELECT 1 UNION ALL SELECT n+1 FROM files WHERE n<2000)
            INSERT INTO phone_list_items SELECT 1,n,'photo-'||n||'.jpg','version-'||n,12,'1000',NULL FROM files").unwrap();
        let mut select = db.prepare(SAVED_TARGETS).unwrap();
        let rows = select
            .query_map([1], |r| r.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 2000);
        assert!(select.get_status(rusqlite::StatementStatus::VmStep) < 500_000);
        assert_eq!(
            targets(&db, 1, &inventory::Receipts::default(), true)
                .unwrap()
                .len(),
            BATCH_SIZE
        );
    }

    #[test]
    fn skips_ambiguous_queue_entries_and_moves_failed_versions_to_the_back() {
        let db = Connection::open_in_memory().unwrap();
        inventory::initialize(&db).unwrap();
        for (i, path, version, size) in [
            (0, "a.jpg", "v1", 4),
            (1, "a.jpg", "v2", 4),
            (2, "b.mov", "v3", 4),
            (3, "b.mov", "v3", 5),
            (4, "c.jpg", "v4", 6),
            (5, "d.jpg", "v5", 7),
        ] {
            db.execute(
                "INSERT INTO phone_list_items VALUES(1,?,?,?,?, '1000',NULL)",
                params![i, path, version, size],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO phone_item_retries VALUES(1,'c.jpg','v4',100)",
            [],
        )
        .unwrap();
        let order: Vec<_> = targets(&db, 1, &inventory::Receipts::default(), false)
            .unwrap()
            .into_iter()
            .map(|t| t.ordinal)
            .collect();
        assert_eq!(order, [5, 4]);
    }

    #[test]
    fn new_and_changed_originals_get_a_turn_before_a_large_backlog_of_changed_object_ids() {
        let db = Connection::open_in_memory().unwrap();
        inventory::initialize(&db).unwrap();
        let mut receipts = inventory::Receipts::default();
        let old = inbox::Received {
            digest: "old".into(),
            size: 12,
            bytes: 0,
            compared: 0,
            path: PathBuf::new(),
        };
        for n in 0..600 {
            let entry = format!("prior-{n}.jpg");
            db.execute(
                "INSERT INTO phone_list_items VALUES(1,?1,?2,'new-id',12,'1000',NULL)",
                params![n, entry],
            )
            .unwrap();
            receipts.received(
                &Item {
                    id: String::new(),
                    relative: entry.into(),
                    size: Some(12),
                    modified: None,
                    version: Some("old-id".into()),
                },
                &old,
            );
        }
        db.execute_batch(
            "INSERT INTO phone_list_items VALUES
            (1,600,'missing.mov','v1',20,'1000',NULL), (1,601,'another.jpg','v1',30,'1000',NULL);
            UPDATE phone_list_items SET size=16 WHERE ordinal=599;",
        )
        .unwrap();
        let order = |missing_first| {
            queue_batch(&db, 1, &receipts, missing_first)
                .unwrap()
                .into_iter()
                .map(|t| t.ordinal)
                .collect::<Vec<_>>()
        };
        let batch = order(true);
        assert_eq!(batch.len(), BATCH_SIZE);
        assert_eq!(&batch[..6], &[599, 0, 600, 1, 601, 2]);
        assert_eq!(batch.iter().collect::<HashSet<_>>().len(), batch.len());
        assert_eq!(&order(false)[..4], &[0, 599, 1, 600]);
        db.execute(
            "INSERT INTO phone_item_retries VALUES(1,'prior-599.jpg','new-id',1)",
            [],
        )
        .unwrap();
        assert_eq!(&order(true)[..4], &[600, 0, 601, 1]);
        assert!(!order(true).contains(&599));
        db.execute_batch("INSERT INTO phone_item_retries VALUES (1,'missing.mov','v1',3),(1,'another.jpg','v1',2)").unwrap();
        assert_eq!(&order(true)[..4], &[599, 601, 600, 0]);
        assert_eq!(&order(false)[..4], &[0, 1, 2, 3]);
    }

    #[test]
    fn a_saved_usb_list_reads_missing_originals_before_possible_jpeg_exports() {
        let db = Connection::open_in_memory().unwrap();
        inventory::initialize(&db).unwrap();
        let mut receipts = inventory::Receipts::default();
        let raw = Item {
            id: "old".into(), relative: "photos/image.HEIC".into(), size: Some(9),
            modified: None, version: Some("camera/asset/9/1000".into()),
        };
        let saved = inbox::Received {
            digest: "verified-original".into(), size: 9, bytes: 0, compared: 0, path: PathBuf::new(),
        };
        receipts.received(&raw, &saved);
        db.execute_batch("INSERT INTO phone_list_items VALUES
            (1,0,'photos/image.JPG','camera/asset/18/1000',18,'1000',NULL),
            (1,1,'photos/new.MOV','video/30/1001',30,'1001',NULL),
            (1,2,'photos/other.JPG','unrelated/11/1002',11,'1002',NULL)").unwrap();
        for missing_first in [true, false] {
            let targets = queue_batch(&db, 1, &receipts, missing_first).unwrap();
            assert_eq!(targets.iter().map(|t| t.ordinal).collect::<Vec<_>>(), [1, 2, 0]);
            assert!(targets.iter().all(|t| t.reference.is_none()));
        }
    }
}
