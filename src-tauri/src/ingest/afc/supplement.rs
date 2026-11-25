//! Receive camera originals via the paired phone while its Windows export is unavailable.
//! The two source inventories remain independently identifiable in the same archive.
use super::{Audit, Phone, same_hardware, scan, schedule::Plan};
use crate::ingest::{
    Phase,
    archive::{Archive, Result, Source},
    inbox, verification,
};
use rusqlite::{OptionalExtension, params};
use std::{cell::Cell, path::Path};

pub(in crate::ingest) fn stalled(report: &crate::ingest::Report) -> bool {
    report.safe_at.is_none()
        && report.newly_verified == 0
        && report.device_key.is_some()
        && (report.export_shortfall.is_some() || matches!(
            report.phase,
            Phase::MediaUnavailable | Phase::SessionInterrupted | Phase::ContentMismatch | Phase::AppleNeedsSupplement
        ))
}

/// Enter from the normal desktop worker after the WPD session has released all handles.
pub(super) fn receive(
    view: &std::sync::Mutex<crate::ingest::View>,
    db: &rusqlite::Connection,
    dest: &crate::ingest::Target,
    windows: Option<&crate::ingest::Report>,
    phone: &Phone,
    key: &str,
) -> Result<bool> {
    use crate::ingest::{Report, catalog, recent, record_history};
    let root = Path::new(&dest.root);
    let mut report = match windows {
        Some(current) => current.clone(),
        None => recent(db)?.into_iter()
            .find(|r| r.device_key.as_deref() == Some(key) && r.safe_at.is_none())
            .unwrap_or_else(|| Report {
                source: "iPhone".into(), phone: true, capture: true,
                device_key: Some(key.to_owned()), ..Default::default()
            }),
    };
    // A saved report can describe an interrupted older native attempt.
    if report.native_originals.is_some() { report.issues.clear(); report.failed = 0; }
    report.native_originals = None;
    let active = || -> Result<()> {
        if view.lock().map_err(|e| e.to_string())?.target.as_ref() != Some(dest) {
            return Err("相册或自动导入设置已变化".into());
        }
        Ok(())
    };
    let guard = std::cell::RefCell::new(catalog::guard_destination(dest, catalog::album_path));
    let repeat_requested: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM phone_repeat WHERE library=? AND album=? AND device=?)",
        params![dest.library, dest.album, key], |row| row.get(0),
    )?;
    let result = collect(
        root,
        key,
        &phone.serial,
        phone,
        repeat_requested,
        |snapshot| {
            report.native_originals = Some(snapshot.clone());
            report.phase = "正在核对 Apple 相机原件".into();
            let mut live = view.lock().unwrap();
            live.busy = true;
            if !live.devices.contains(&report.source) { live.devices.push(report.source.clone()); }
            live.report = report.clone();
        },
        || (guard.borrow_mut())(),
        active,
    );
    let interrupted = result.is_err();
    match result {
        Ok(Some(summary)) => {
            report.native_originals = Some(summary);
            report.phase = "已核对 Apple 相机原件".into();
        }
        Ok(None) => return Ok(false),
        Err(e) => {
            if report.native_originals.is_none() {
                return Err(e);
            }
            report.phase = "Apple 相机原件核对中断".into();
            crate::ingest::issue(&mut report, e.to_string());
        }
    }
    let mut live = view.lock().unwrap();
    live.busy = false;
    live.report = report.clone();
    record_history(db, &report)?;
    live.recent = recent(db)?;
    Ok(interrupted)
}

pub(super) fn pending_sources(db: &rusqlite::Connection, expected: Option<&str>) -> Result<Vec<(String, String)>> {
    let has_runs: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='phone_runs')",
        [], |r| r.get(0),
    )?;
    if !has_runs {
        return Ok(Vec::new());
    }
    let mut query = db.prepare("SELECT DISTINCT source FROM phone_runs WHERE safe_at IS NULL AND source LIKE 'wpd:%' ORDER BY source")?;
    let mut pending = Vec::new();
    for row in query.query_map([], |r| r.get::<_, String>(0))? {
        let source = row?;
        let Some(id) = source.strip_prefix("wpd:").filter(|id| !id.is_empty()) else { continue };
        let key = verification::device_key(&source);
        if expected.is_none_or(|value| value == key) { pending.push((id.to_owned(), key)); }
    }
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlisted_windows_camera_can_resume_only_an_unfinished_device_in_the_selected_gallery() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        assert!(pending_sources(&db, None).unwrap().is_empty());
        db.execute_batch("CREATE TABLE phone_runs(source TEXT,safe_at INTEGER);
            INSERT INTO phone_runs VALUES('wpd:camera-a',NULL),('wpd:camera-a',NULL),('wpd:camera-b',NULL),
            ('wpd:completed',123),('afc:other',NULL),('wpd:',NULL)").unwrap();
        let a = verification::device_key("wpd:camera-a");
        let b = verification::device_key("wpd:camera-b");
        assert_eq!(pending_sources(&db, None).unwrap(), vec![("camera-a".into(),a.clone()),("camera-b".into(),b.clone())]);
        assert_eq!(pending_sources(&db, Some(&b)).unwrap(), vec![("camera-b".into(),b)]);
        assert!(pending_sources(&db, Some("another gallery device")).unwrap().is_empty());
    }

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
