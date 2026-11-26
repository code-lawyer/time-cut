//! Carry forward receipts from archives that acquired AFC as a Windows supplement.
use crate::ingest::{Report, archive::{Archive, Result}};
use rusqlite::params;
use std::path::Path;

#[cfg(test)]
use super::{Audit, same_hardware, scan, schedule::Plan};
#[cfg(test)]
use crate::ingest::{archive::Source, inbox, verification};
#[cfg(test)]
use rusqlite::OptionalExtension;
#[cfg(test)]
use std::cell::Cell;

pub(super) fn reconcile(root: &Path, udid: &str, report: &Report) -> Result<()> {
    let source = format!("afc:{udid}");
    let archive = Archive::open(root)?;
    let tx = archive.db.unchecked_transaction()?;
    // Fill only exact missing receipts from the completed AFC manifest. Old
    // versions and conflicts remain visible on their original Windows run.
    let manifest = "SELECT m.entry,m.version,m.size,m.digest FROM phone_manifest m
        JOIN phone_runs r ON r.id=m.run_id WHERE r.source=?1 AND r.safe_at=?2";
    tx.execute(&format!("WITH verified AS ({manifest})
        UPDATE phone_native_items AS n SET digest=v.digest FROM verified v
        WHERE n.source=?1 AND n.entry=v.entry AND n.version=v.version AND n.size=v.size
          AND n.digest IS NULL AND n.conflict=0"), params![source, report.safe_at])?;
    tx.execute(&format!("WITH verified AS ({manifest})
        UPDATE phone_native_supplements AS s SET checked_at=?2,
          listed=(SELECT COUNT(*) FROM phone_native_items n WHERE n.run_id=s.run_id AND n.source=s.source)
        WHERE s.source=?1 AND s.checked_at IS NULL
          AND EXISTS(SELECT 1 FROM phone_native_items n WHERE n.run_id=s.run_id AND n.source=s.source)
          AND NOT EXISTS(SELECT 1 FROM phone_native_items n WHERE n.run_id=s.run_id AND n.source=s.source
            AND (n.conflict<>0 OR NOT EXISTS(SELECT 1 FROM verified v
              WHERE v.entry=n.entry AND v.version=n.version AND v.size=n.size AND v.digest=n.digest)))"),
        params![source, report.safe_at])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn after_a_resource_error_unknown_files_precede_failed_and_already_saved_files() {
        let db=rusqlite::Connection::open_in_memory().unwrap();
        inbox::initialize(&db).unwrap();
        db.execute_batch("INSERT INTO phone_native_items VALUES(1,'afc:one','DCIM/101/old.HEIC','v1',8,'hash',0),
            (1,'afc:one','DCIM/102/interrupted.AAE','v1',5,NULL,0),
            (1,'afc:two','DCIM/103/other.JPG','v1',9,'other',0)").unwrap();
        let plan=Plan::load(&db,1,"afc:one").unwrap();
        let priority=|p:&str|plan.order.priorities.get(Path::new(p)).copied().unwrap_or(1);
        assert_eq!(plan.saved_count,1);
        assert!(priority("DCIM/104")<priority("DCIM/102") && priority("DCIM/102")<priority("DCIM/101"));
        assert!(priority("DCIM/101/new.MOV")<priority("DCIM/101/old.HEIC"));
        assert_eq!(priority("DCIM/103"),1);
    }
}

#[cfg(test)]
pub(in crate::ingest) fn collect<Guard>(
    root: &Path,
    windows_key: &str,
    serial: &str,
    source: &dyn Source,
    repeat_requested: bool,
    progress: impl FnMut(&Audit),
    destination: impl Fn() -> Result<Guard>,
    active: impl Fn() -> Result<()>,
) -> Result<Option<Audit>> {
    active()?;
    let Some(udid) = source
        .identity()
        .strip_prefix("afc:")
        .filter(|id| !id.is_empty())
    else {
        return Err("无法确定 Apple 原件来源".into());
    };
    let archive = {
        let _guard = destination()?;
        let archive = Archive::open(root)?;
        inbox::initialize(&archive.db)?;
        archive
    };
    let mut q = archive.db.prepare(
        "SELECT id,source FROM phone_runs WHERE safe_at IS NULL AND source LIKE 'wpd:%'",
    )?;
    let mut rows = q.query([])?;
    let mut matched_run = None;
    while let Some(row) = rows.next()? {
        let windows: String = row.get(1)?;
        if verification::device_key(&windows) == windows_key
            && windows.strip_prefix("wpd:").is_some_and(|id| same_hardware(id, serial, udid))
        {
            matched_run = Some(row.get::<_, i64>(0)?);
            break;
        }
    }
    drop(rows);
    drop(q);
    let Some(run) = matched_run else {
        return Ok(None);
    };
    let native = source.identity();
    let (checked, previous_manifest, repeat_time) = archive.db.query_row(
        "SELECT s.checked_at,r.manifest,r.verified_at FROM phone_native_supplements s
         LEFT JOIN phone_native_rechecks r USING(run_id,source) WHERE s.run_id=? AND s.source=?",
        params![run, native], |r| Ok((r.get::<_,Option<i64>>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<i64>>(2)?)),
    ).optional()?.unwrap_or_default();
    if checked.is_some() && (!repeat_requested || repeat_time.is_some()) {
        return Ok(None);
    }
    {
        let _guard = destination()?;
        inbox::recover_files(&archive)?;
        let tx = archive.db.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO phone_native_supplements(run_id,source) VALUES(?,?)
             ON CONFLICT(run_id,source) DO UPDATE SET checked_at=NULL",
            params![run, native],
        )?;
        tx.execute("UPDATE phone_native_rechecks SET verified_at=NULL WHERE run_id=? AND source=?", params![run,native])?;
        tx.commit()?;
    }
    let mut recovery = inbox::RecoveryCheck::new(false);
    let plan = Plan::load(&archive.db,run,native)?;
    let saved_receipts = Cell::new(plan.saved_count);
    let direct_attempts = Cell::new(0);
    let direct_received = Cell::new(0);
    let mut progress = progress;
    let mut result = scan(
        root,
        source,
        &plan,
        |item| {
            active()?;
            let _guard = destination()?;
            let version = item
                .version
                .as_deref()
                .filter(|v| !v.is_empty())
                .ok_or("Apple 相机原件缺少版本")?;
            let size = item.size.ok_or("Apple 相机原件缺少长度")?;
            archive.db.prepare_cached("INSERT INTO phone_native_items(run_id,source,entry,version,size) VALUES(?,?,?,?,?)
                ON CONFLICT(run_id,source,entry,version) DO UPDATE SET conflict=1 WHERE phone_native_items.size<>excluded.size")?
                .execute(params![run,native,item.relative.to_string_lossy(),version,size])?;
            Ok(())
        },
        |item| {
            active()?;
            let _guard = destination()?;
            archive.db.prepare_cached("INSERT OR REPLACE INTO phone_native_pickups(run_id,source,entry,version,attempted) VALUES(?,?,?,?,?)")?
                .execute(params![run,native,item.relative.to_string_lossy(),item.version,chrono::Utc::now().timestamp_millis()])?;
            direct_attempts.set(direct_attempts.get()+1);
            Ok(())
        },
        |item, _, sha1, update| {
            active()?;
            let _guard = destination()?;
            let received = inbox::receive_checked(
                &archive,
                run,
                source,
                item,
                false,
                &mut recovery,
                Some(*sha1),
                || Ok(()),
                |n, _| update(n),
            )?;
            let conflict: bool = archive
                .db
                .prepare_cached(
                    "UPDATE phone_native_items SET
                conflict=conflict OR (digest IS NOT NULL AND digest<>?1),digest=COALESCE(digest,?1)
                WHERE run_id=?2 AND source=?3 AND entry=?4 AND version=?5 RETURNING conflict",
                )?
                .query_row(
                    params![
                        received.digest,
                        run,
                        native,
                        item.relative.to_string_lossy(),
                        item.version
                    ],
                    |r| r.get(0),
                )?;
            if conflict {
                return Err("同一版本的 Apple 相机原件返回了不同内容，已保留待核对记录".into());
            }
            if !plan.saved(item) {
                saved_receipts.set(saved_receipts.get()+1);
            }
            if direct_attempts.get()>0 { direct_received.set(direct_received.get()+1); }
            Ok((Some(received.digest), received.bytes))
        },
        |snapshot| {
            let mut snapshot=snapshot.clone();
            snapshot.saved_receipts=saved_receipts.get();
            snapshot.direct_attempts=direct_attempts.get();
            snapshot.direct_received=direct_received.get();
            progress(&snapshot);
        },
        &active,
    )?;
    active()?;
    result.saved_receipts=saved_receipts.get();
    let _guard = destination()?;
    if !result.stable_inventory || result.listed != result.matched || !inbox::verify_supplement_files(&archive, run)? {
        return Err("Apple 相机目录仍有原件缺少本地校验凭据".into());
    }
    result.repeat_verified = repeat_requested && result.copied == 0
        && previous_manifest.as_deref() == Some(result.manifest.as_str());
    let now = chrono::Utc::now().timestamp_millis();
    let tx = archive.db.unchecked_transaction()?;
    tx.execute(
        "UPDATE phone_native_supplements SET checked_at=?,listed=? WHERE run_id=? AND source=?",
        params![now, result.listed, run, native],
    )?;
    tx.execute("INSERT OR REPLACE INTO phone_native_rechecks(run_id,source,manifest,verified_at) VALUES(?,?,?,?)",
        params![run,native,result.manifest,result.repeat_verified.then_some(now)])?;
    tx.commit()?;
    Ok(Some(result))
}
