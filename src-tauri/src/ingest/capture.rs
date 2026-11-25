use super::{
    Report,
    archive::{Archive, CopyOrder, Item, Result, ScanEvent, Source},
    inbox, inventory, issue,
    resume::PhoneQueue,
    verification,
};
mod known;
use rusqlite::{Connection, params};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn update_folder_coverage(root: &Path, folders: &HashMap<PathBuf, inventory::Coverage>, report: &mut Report) {
    let coverage = inventory::Coverage::total(folders);
    report.export_shortfall = inventory::Shortfall::check(root, coverage.possible_export_bytes);
    report.partial_coverage = Some(coverage);
}

struct ExportBatch { required: Option<u64> }

impl ExportBatch {
    fn new(report: &Report, sizes: impl Iterator<Item = u64>) -> Self {
        let saved = report.partial_coverage.map_or(0, |c| c.possible_export_bytes);
        Self { required: Some(saved.max(sizes.fold(0, u64::saturating_add))) }
    }

    fn defer(&mut self, root: &Path, priority: u8, report: &mut Report) -> bool {
        if priority != inventory::POSSIBLE_USB_EXPORT { return false; }
        if let Some(required) = self.required.take() {
            report.export_shortfall = inventory::Shortfall::check(root, required);
        }
        report.export_shortfall.is_some()
    }
}

fn flush_discoveries(db: &Connection, queue: &mut PhoneQueue, items: &mut Vec<Item>) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let tx = db.unchecked_transaction()?;
    for item in items.iter() {
        queue.discover(&tx, item)?;
    }
    tx.commit()?;
    items.clear();
    Ok(())
}

fn save_progress(
    db: &Connection,
    run: i64,
    report: &Report,
    counts: Option<&HashMap<PathBuf, u64>>,
) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    if let Some(counts) = counts {
        tx.execute(
            "INSERT OR REPLACE INTO phone_peak_folders SELECT id,?1 FROM phone_runs
            WHERE id=?2 AND peak<?3",
            params![serde_json::to_string(counts)?, run, report.discovered],
        )?;
    }
    tx.execute(
        "UPDATE phone_runs SET scanned=?,covered=?,peak=MAX(peak,?),progress_at=? WHERE id=?",
        params![
            report.scanned,
            report.added + report.skipped,
            report.discovered,
            chrono::Utc::now().timestamp_millis(),
            run
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn receive_one(
    archive: &Archive,
    run: i64,
    source: &dyn Source,
    item: &Item,
    trust_version: bool,
    recovery: &mut inbox::RecoveryCheck,
    queue: &mut PhoneQueue,
    report: &mut Report,
    active: &mut impl FnMut() -> Result<()>,
    publish: &mut impl FnMut(&Report, bool),
) -> Result<Option<inbox::Received>> {
    if !source.album_path_in_scope(&item.relative) {
        return Err("手机清单项目不属于当前相册范围".into());
    }
    let db = &archive.db;
    report.current = item.relative.to_string_lossy().into_owned();
    report.total_bytes = item.size;
    report.transferred_bytes = 0;
    report.phase = "接收手机原件".into();
    let result = inbox::receive(
        archive,
        run,
        source,
        item,
        trust_version,
        recovery,
        || queue.attempting(db, item, None),
        |count, phase| {
            let phase = match phase {
                inbox::ReadPhase::Copy => "接收手机原件",
                inbox::ReadPhase::Compare => "核对手机旧版本",
                inbox::ReadPhase::Confirm => "确认手机返回的原件",
            };
            if report.phase != phase {
                report.phase = phase.into();
            }
            report.transferred_bytes = count;
            publish(report, false);
            active()
        },
    );
    let receipt = match result {
        Ok(received) => {
            queue.verified(db, item, &received.digest)?;
            if received.bytes > 0 {
                report.added += 1;
            } else {
                report.skipped += 1;
            }
            report.received_bytes += received.bytes;
            report.compared_bytes += received.compared;
            report.total_received += u64::from(received.bytes > 0);
            report.total_received_bytes += received.bytes;
            Some(received)
        }
        Err(e) => {
            queue.attempting(db, item, Some(&e.to_string()))?;
            report.pending = queue.count();
            if let Some(returned) = e.downcast_ref::<inbox::observed::ReturnedData>()
                && source.confirms_returned_object(item).unwrap_or(false)
            {
                returned.record(db, run, report.receive_attempt, item, recovery.is_controlled_target())?;
            }
            #[cfg(windows)]
            if e.is::<super::TransferStop>() {
                return Err(e);
            }
            #[cfg(windows)]
            if let Some(stop) = super::wpd::interruption(e.as_ref()) {
                issue(report, format!("{}：{e}", item.relative.display()));
                return Err(stop.into());
            }
            active()?;
            issue(report, format!("{}：{e}", item.relative.display()));
            None
        }
    };
    report.pending = queue.count();
    report.current.clear();
    report.total_bytes = None;
    report.transferred_bytes = 0;
    publish(report, report.added + report.skipped == 1);
    Ok(receipt)
}

/// The source session and all its COM handles are dropped before local jobs are run.
pub(super) fn receive<Guard>(
    root: &Path,
    name: &str,
    minimum: impl FnOnce((&str, Option<&str>)) -> Result<u64>,
    make_source: impl FnOnce() -> Result<Box<dyn Source>>,
    mut update: impl FnMut(&Report),
    mut destination: impl FnMut() -> Result<Guard>,
    mut active: impl FnMut() -> Result<()>,
) -> Report {
    let mut report = Report {
        source: name.into(),
        phone: true,
        capture: true,
        phase: "正在读取".into(),
        ..Default::default()
    };
    update(&report);
    let result = (|| -> Result<()> {
        active()?;
        let source = make_source()?;
        #[cfg(windows)]
        let _awake = super::notice::ActiveTransfer::new();
        let identity = source.identity().to_owned();
        let key = identity.as_str();
        report.device_key = Some(verification::device_key(key));
        let scope = source.album_scope_version();
        report.album_scope = scope.map(str::to_owned);
        let archive = {
            let _guard = destination()?;
            Archive::open(root)?
        };
        inbox::initialize(&archive.db)?;
        let db = &archive.db;
        if let Some(serial) = source.paired_camera_serial() {
            let udid = key.strip_prefix("afc:").filter(|s| !s.is_empty()).ok_or("无法确定 Apple 原件来源")?;
            if serial.is_empty() { return Err("Apple 设备未提供硬件序列号".into()); }
            db.execute("INSERT OR IGNORE INTO phone_camera_roots(source,serial,udid) VALUES(?,?,?)",params![key,serial,udid])?;
            let consistent: bool = db.query_row("SELECT serial=? AND udid=? FROM phone_camera_roots WHERE source=?",
                params![serial,udid,key],|r|r.get(0))?;
            if !consistent { return Err("Apple 原件来源的硬件身份发生变化".into()); }
        } else if let Some(id) = key.strip_prefix("wpd:") {
            // A Windows interface can appear halfway through a camera-only capture.
            // Resume the original inventory before opening a competing Windows run.
            let camera_open: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM phone_camera_roots c
                JOIN phone_runs r ON r.source=c.source WHERE r.safe_at IS NULL AND (c.serial=?1 OR c.udid=?1))",
                [id],|r|r.get(0))?;
            if camera_open { report.phase = "等待 Apple 原件目录".into(); return Ok(()); }
        }
        inbox::recover_files(&archive)?;
        let previously_complete: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM phone_runs r LEFT JOIN phone_run_scopes s ON s.run_id=r.id
             WHERE r.source=? AND r.safe_at IS NOT NULL AND s.scope IS ?)",
            params![key,scope],
            |r| r.get(0),
        )?;
        let requested_minimum = if previously_complete {
            0
        } else {
            minimum((report.device_key.as_deref().unwrap(), scope))?
        };
        db.execute(
            "INSERT OR IGNORE INTO phone_runs(source,started) VALUES(?,?)",
            params![key, chrono::Utc::now().timestamp_millis()],
        )?;
        let run: i64 = db.query_row(
            "SELECT id FROM phone_runs WHERE source=? AND safe_at IS NULL", [key], |r| r.get(0),
        )?;
        if let Some(scope) = scope { inventory::apply_album_scope(db, run, source.as_ref(), scope)?; }
        let (started, peak, attempt, copies, bytes) = db.query_row(
            "SELECT started,peak,sessions+1,copies,bytes FROM phone_runs WHERE id=?",
            [run],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, u64>(1)?,
                    r.get::<_, u64>(2)?,
                    r.get::<_, u64>(3)?,
                    r.get::<_, u64>(4)?,
                ))
            },
        )?;
        let minimum = requested_minimum.max(peak);
        db.execute(
            "UPDATE phone_runs SET sessions=sessions+1 WHERE id=?",
            [run],
        )?;
        report.capture_started_at = Some(started);
        report.receive_attempt = attempt;
        report.total_received = copies;
        report.total_received_bytes = bytes;
        let mut queue = PhoneQueue::for_capture(db, key)?;
        report.pending = queue.count();
        let mut folders = db
            .prepare("SELECT path,completed FROM phone_directories WHERE source=?")?
            .query_map([key], |r| {
                Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        let mut outstanding_folders: HashSet<_> = folders
            .iter()
            .filter(|(_, time)| **time >= started)
            .map(|(path, _)| path.clone())
            .collect();
        // On migration prioritize unvisited folders over dates heavily represented in the old gallery.
        let mut historical = HashMap::<PathBuf, i64>::new();
        let mut entries = db.prepare("SELECT entry FROM sources WHERE source=?")?;
        for entry in entries.query_map([key], |r| r.get::<_, String>(0))? {
            let entry = PathBuf::from(entry?);
            if source.album_path_in_scope(&entry) && let Some(parent) = entry.parent() {
                *historical.entry(parent.to_path_buf()).or_default() += 1;
            }
        }
        for (path, count) in historical {
            folders.entry(path).or_insert(i64::MIN + count);
        }
        let interrupted: HashMap<PathBuf, i64> = db
            .prepare(
                "SELECT entry,MAX(attempted_at) FROM phone_pending
            WHERE source=? AND attempted_at>0 GROUP BY entry",
            )?
            .query_map([key], |r| {
                Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (entry, attempted_micros) in &interrupted {
            if let Some(parent) = entry.parent() {
                let rank = folders.entry(parent.to_path_buf()).or_insert(i64::MIN);
                *rank = (*rank).max(attempted_micros / 1000);
            }
        }
        let mut recovery = inbox::RecoveryCheck::new(!interrupted.is_empty());
        let mut receipts = inventory::Receipts::load(db, key)?;
        let mut folder_coverage = receipts.saved_folders(db, run)?;
        let mut priorities = folder_coverage
            .iter()
            .map(|(path, counts)| (path.clone(), if counts.missing > 0 { 0 } else { 2 }))
            .collect();
        let folder_pass = inventory::unfinished_folders(
            db,
            run,
            attempt,
            &mut outstanding_folders,
            &mut folders,
            &mut priorities,
        )?;
        let mut order = CopyOrder {
            folders,
            priorities,
            interrupted,
            preferred_folder: None,
        };
        // Preserve regular list refreshes and the sessions for current missing originals.
        let try_historical = attempt >= 4 && attempt % 4 == 2 && source.saved_files().is_some();
        let historical = inventory::restore(db, run, &mut report, try_historical)?;
        if let Some(item) = &historical {
            order.preferred_folder = item.path.parent().map(Path::to_path_buf);
            order.interrupted.remove(&item.path);
        }
        update_folder_coverage(root, &folder_coverage, &mut report);
        let mut minimum = minimum.max(report.list_total.unwrap_or(0));
        report.expected_minimum = (minimum > 0).then_some(minimum);
        report.phase = "正在盘点手机清单".into();
        let mut members: Vec<inventory::Member> = Vec::new();
        let mut discoveries = Vec::new();
        let mut first_attempted = false;
        let mut folder_start = 0;
        let mut recovering_folder = None;
        let mut listing_error = false;
        let mut counted_folder = PathBuf::new();
        let mut scope_counts = source.uses_album_markers().then(HashMap::new);
        let mut last = Instant::now();
        let mut last_checkpoint = Instant::now();
        let mut publish = |p: &Report, force: bool| {
            if force || last.elapsed() >= Duration::from_millis(200) {
                update(p);
                last = Instant::now();
            }
        };
        publish(&report, true);
        // Keep using a recently checked list while individual receipts advance. Older
        // lists get a full refresh turn so photo edits and newly taken items are seen.
        let recent_list = report.list_at.is_some_and(|at| (0..600_000).contains(&(chrono::Utc::now().timestamp_millis()-at)));
        if historical.is_none() && attempt >= 4 && (recent_list || attempt % 4 != 1) && report.list_total.is_some_and(|n| report.list_covered < n)
            && let Some(resolver) = source.saved_files()
            && known::resume(&archive, run, source.as_ref(), resolver, &receipts, &mut queue,
                &mut report, &mut active, &mut destination, &mut publish)? {
            return Ok(());
        }
        let scanned = source.visit_for_copy(&order, &mut |event| {
            active()?;
            match event {
                ScanEvent::DirectoryCheck { done, total } => {
                    report.phase = "核对手机目录".into();
                    report.checked_folders = done;
                    report.total_folders = total;
                }
                ScanEvent::Inspected { media } => {
                    report.scanned += 1;
                    report.discovered += u64::from(media);
                    if media && let Some(counts) = &mut scope_counts {
                        *counts.entry(counted_folder.clone()).or_insert(0) += 1;
                    }
                    if last_checkpoint.elapsed() >= Duration::from_secs(5) {
                        save_progress(db, run, &report, scope_counts.as_ref())?;
                        last_checkpoint = Instant::now();
                    }
                }
                ScanEvent::DirectoryFound(path) => {
                    outstanding_folders.insert(path.clone());
                    report.known_folders += db.execute("INSERT OR IGNORE INTO phone_list_folders(run_id,path) VALUES(?,?)",
                        params![run,path.to_string_lossy()])? as u64;
                }
                ScanEvent::DirectoryStart(path) => {
                    counted_folder.clone_from(&path);
                    folder_start=members.len();
                    recovering_folder = folder_pass.interrupted.contains(&path).then(|| path.clone());
                    db.execute("INSERT INTO phone_list_folders(run_id,path,attempted,probing) VALUES(?,?,?,1)
                        ON CONFLICT(run_id,path) DO UPDATE SET attempted=excluded.attempted,probing=1",
                        params![run,path.to_string_lossy(),chrono::Utc::now().timestamp_millis()])?;
                }
                ScanEvent::DirectoryDone{path,media} => {
                    let saved=inventory::save_folder(db,run,&path,&media)?;
                    if saved {
                        outstanding_folders.remove(&path);
                        folder_coverage.insert(path.clone(),receipts.folder_coverage(&media));
                        update_folder_coverage(root, &folder_coverage, &mut report);
                    } else {
                        outstanding_folders.insert(path.clone());
                        listing_error=true;
                        issue(&mut report,"此前有素材的目录暂时返回空清单，已保留上次盘点结果".into());
                    }
                    inventory::folder_progress(db,run,&mut report)?;
                    db.execute(
                        "INSERT OR REPLACE INTO phone_directories VALUES(?,?,?)",
                        params![
                            key,
                            path.to_string_lossy(),
                            chrono::Utc::now().timestamp_millis()
                        ],
                    )?;
                    let historical_folder = historical.as_ref().filter(|target| target.path.parent() == Some(path.as_path()));
                    if saved && (!folder_pass.list_first || historical_folder.is_some()) {
                        flush_discoveries(db, &mut queue, &mut discoveries)?;
                        let folder_members = &mut members[folder_start..];
                        receipts.match_members(folder_members);
                        let mut changed = false;
                        if let Some(item) = historical_folder.and_then(|target| target.unique_member(&media))
                            && let Some(member) = folder_members.iter_mut().find(|m| m.item.id == item.id && !m.attempted && !m.ambiguous)
                            && !(member.priority == inventory::POSSIBLE_USB_EXPORT && report.export_shortfall.is_some())
                        {
                            let _guard = destination()?;
                            member.attempted = true;
                            member.receipt = receive_one(&archive, run, source.as_ref(), &member.item, false,
                                &mut inbox::RecoveryCheck::for_returning_item(), &mut queue, &mut report,
                                &mut active, &mut publish)?;
                            if let Some(receipt) = &member.receipt {
                                receipts.received(&member.item, receipt);
                                report.list_unseen = report.list_unseen.saturating_sub(1);
                                changed = true;
                            }
                        }
                        let mut work: Vec<_> = folder_members.iter_mut().filter(|member| {
                            !folder_pass.list_first && !member.attempted && !order.interrupted.contains_key(&member.item.relative)
                                && !historical_folder.is_some_and(|target| target.path == member.item.relative)
                                && (member.priority == 0
                                    || (member.priority == 1 && member.recorded.as_deref().is_some_and(|hash|
                                        recovery.wants_reference(hash, member.item.size)))
                                    || (attempt > 1 && member.priority == 2
                                        && !member.ambiguous && member.item.version.is_some()))
                        }).collect();
                        // Save every new original first. A short batch of old versions can then
                        // make durable progress even if the next directory ends this session.
                        work.sort_by_key(|member| (match member.priority { 1 => 0, 0 => 1, _ => 2 },
                            if member.priority == 0 { 0 } else { member.item.size.unwrap_or(u64::MAX) }));
                        let mut compare_started = None;
                        let mut comparisons = 0;
                        let mut refreshed = Instant::now();
                        for member in work {
                            if member.priority == 1 && !member.recorded.as_deref().is_some_and(|hash|
                                recovery.wants_reference(hash, member.item.size)) { continue; }
                            if member.priority == 2 {
                                let start = compare_started.get_or_insert_with(Instant::now);
                                if comparisons >= 32 || start.elapsed() >= Duration::from_secs(2) {
                                    break;
                                }
                                comparisons += 1;
                            }
                            let _guard = destination()?;
                            member.attempted = true;
                            member.receipt = receive_one(
                                &archive, run, source.as_ref(), &member.item, member.priority == 1,
                                &mut recovery,
                                &mut queue, &mut report, &mut active, &mut publish,
                            )?;
                            if let Some(receipt) = &member.receipt {
                                receipts.received(&member.item, receipt);
                                changed = true;
                                if refreshed.elapsed() >= Duration::from_secs(2) {
                                    folder_coverage.insert(path.clone(), receipts.folder_coverage(&media));
                                    update_folder_coverage(root, &folder_coverage, &mut report);
                                    publish(&report, true);
                                    refreshed = Instant::now();
                                    changed = false;
                                }
                            }
                        }
                        if changed {
                            folder_coverage.insert(path, receipts.folder_coverage(&media));
                            update_folder_coverage(root, &folder_coverage, &mut report);
                        }
                        report.phase = "正在盘点手机清单".into();
                    }
                }
                ScanEvent::Unreadable(message) => {
                    listing_error = true;
                    issue(&mut report, message);
                }
                ScanEvent::CompanionFound(item) => discoveries.push(item),
                ScanEvent::Ready(item) => {
                    discoveries.push(item.clone());
                    let mut member = inventory::Member::new(item);
                    // A directory that previously stopped during metadata enumeration
                    // might never reach DirectoryDone. Keep new originals seen before the fault.
                    let retrying_list = recovering_folder.as_deref().is_some_and(|folder| {
                        member.item.relative.parent() == Some(folder)
                            && !order.interrupted.contains_key(&member.item.relative)
                    });
                    if !historical.as_ref().is_some_and(|target| target.path == member.item.relative)
                        && !folder_pass.list_first && ((attempt == 1 && !previously_complete && !first_attempted) || retrying_list)
                        && !receipts.has_candidate(&member.item)
                    {
                        flush_discoveries(db, &mut queue, &mut discoveries)?;
                        first_attempted = true;
                        member.attempted=true;
                        let _guard = destination()?;
                        member.receipt = receive_one(
                            &archive,
                            run,
                            source.as_ref(),
                            &member.item,
                            false,
                            &mut recovery,
                            &mut queue,
                            &mut report,
                            &mut active,
                            &mut publish,
                        )?;
                        if let Some(receipt)=&member.receipt {
                            receipts.received(&member.item,receipt);
                        }
                        report.phase = "正在盘点手机清单".into();
                    }
                    members.push(member);
                }
            }
            if discoveries.len() >= 128 {
                flush_discoveries(db, &mut queue, &mut discoveries)?;
            }
            report.pending = queue.count();
            publish(&report, false);
            Ok(())
        });
        flush_discoveries(db, &mut queue, &mut discoveries)?;
        report.pending = queue.count();
        save_progress(db, run, &report, scope_counts.as_ref())?;
        scanned?;
        active()?;
        let excluded = source.excluded_album_folders();
        if !listing_error && !excluded.is_empty() && let Some(counts) = &scope_counts {
            let _guard = destination()?;
            let retained = inventory::withhold_folders(
                db, run, key, &excluded, counts, &mut queue, &mut report,
            )?;
            let visible = |path: &Path| !excluded.iter().any(|dir| path.starts_with(dir));
            outstanding_folders.retain(|dir| visible(dir));
            folder_coverage.retain(|dir, _| visible(dir));
            update_folder_coverage(root, &folder_coverage, &mut report);
            minimum = requested_minimum.max(retained);
            report.expected_minimum = (minimum > 0).then_some(minimum);
            report.pending = queue.count();
        }
        let ids: HashSet<&str> = members.iter().map(|m| m.item.id.as_str()).collect();
        if ids.len() != members.len() {
            return Err("手机目录包含重复的设备标识，将重新盘点".into());
        }
        let complete_list = !listing_error
            && !members.is_empty()
            && members.len() as u64 == report.discovered
            && report.discovered >= minimum
            && outstanding_folders.is_empty();
        receipts.match_members(&mut members);
        if complete_list {
            let _guard = destination()?;
            inventory::publish(db, run, &members, &mut report)?;
            report.list_unseen = queue.unlisted_pending();
            publish(&report, true);
        }
        let mut work: Vec<usize> = (0..members.len())
            .filter(|&i| !members[i].attempted)
            .collect();
        work.sort_by_key(|&i| {
            let member = &members[i];
            (
                member.priority == inventory::POSSIBLE_USB_EXPORT,
                order.interrupted.contains_key(&member.item.relative),
                member.priority,
            )
        });
        // Some sources only report ready files, without per-folder checkpoints. Use the
        // enumerated group as well, and check available space after other originals have landed.
        let mut exports = ExportBatch::new(&report, work.iter().map(|&i| &members[i])
            .filter(|m| m.priority == inventory::POSSIBLE_USB_EXPORT).filter_map(|m| m.item.size));
        let mut changes = Vec::new();
        for i in work {
            let member = &mut members[i];
            if exports.defer(root, member.priority, &mut report) { continue; }
            let response = (|| {
                active()?;
                let _guard = destination()?;
                receive_one(
                    &archive,
                    run,
                    source.as_ref(),
                    &member.item,
                    !member.ambiguous,
                    &mut recovery,
                    &mut queue,
                    &mut report,
                    &mut active,
                    &mut publish,
                )
            })();
            if complete_list {
                let digest = response
                    .as_ref()
                    .ok()
                    .and_then(|r| r.as_ref())
                    .map(|r| r.digest.clone());
                if digest != member.recorded {
                    report.newly_verified += u64::from(digest.is_some() && member.recorded.is_none());
                    report.list_covered = report.list_covered
                        - u64::from(member.recorded.is_some())
                        + u64::from(digest.is_some());
                    changes.push((i, digest));
                }
                if changes.len() >= 64
                    || response.is_err()
                    || last_checkpoint.elapsed() >= Duration::from_secs(5)
                {
                    inventory::checkpoint(db, run, &mut changes)?;
                }
            }
            if response.is_err() || last_checkpoint.elapsed() >= Duration::from_secs(5) {
                save_progress(db, run, &report, scope_counts.as_ref())?;
                last_checkpoint = Instant::now();
            }
            member.receipt = response?;
        }
        inventory::checkpoint(db, run, &mut changes)?;
        save_progress(db, run, &report, scope_counts.as_ref())?;
        if report.failed > 0
            || report.discovered == 0
            || !complete_list
            || report.pending > 0
            || report.list_covered != report.discovered
            || report.discovered != report.added + report.skipped
        {
            report.phase = if report.failed > 0 {
                "部分未完成"
            } else if report.discovered == 0 {
                "手机媒体暂不可读"
            } else if report.pending > 0 {
                "仍有素材待补"
            } else {
                "等待完整媒体清单"
            }
            .into();
            return Ok(());
        }
        report.phase = "复核手机素材清单".into();
        inventory::confirm(source.as_ref(), &order, &members, |done, _| {
            active()?;
            report.rechecked = done;
            publish(&report, false);
            Ok(())
        })?;
        drop(source);
        active()?;
        let _guard = destination()?;
        report.phase = "核对本地收件".into();
        publish(&report, true);
        for member in &members {
            let receipt = member.receipt.as_ref().ok_or("清单有素材尚未确认")?;
            let stat = fs::symlink_metadata(&receipt.path)?;
            if !stat.is_file()
                || stat.len() != receipt.size
                || !inbox::verified_local(&archive, &receipt.path, &receipt.digest)?
            {
                return Err("接收期间本地原件已变化，将重新核对".into());
            }
        }
        let digest = queue.inventory_digest();
        if !inbox::verify_supplements(&archive, run)? {
            report.phase = "Apple 相机原件待补".into();
            return Ok(());
        }
        let tx = db.unchecked_transaction()?;
        tx.execute("DELETE FROM phone_manifest WHERE run_id=?", [run])?;
        {
            let mut insert = tx.prepare("INSERT INTO phone_manifest VALUES(?,?,?,?,?,?)")?;
            for (ordinal, m) in members.iter().enumerate() {
                let r = m.receipt.as_ref().ok_or("清单有素材尚未确认")?;
                insert.execute(params![
                    run,
                    ordinal as i64,
                    m.item.relative.to_string_lossy(),
                    m.item.version,
                    r.digest,
                    r.size
                ])?;
            }
        }
        let reconciled:u64 = tx.query_row(
            "SELECT COUNT(*) FROM phone_list_items m JOIN phone_manifest f ON m.run_id=f.run_id AND m.ordinal=f.ordinal
             WHERE m.run_id=? AND m.entry=f.entry AND m.version IS f.version AND m.digest=f.digest
             AND (m.size IS NULL OR m.size=f.size)",
            [run], |row|row.get(0))?;
        if reconciled != report.discovered {
            return Err("手机清单和本地校验凭据尚未逐项对齐".into());
        }
        let safe_at = chrono::Utc::now().timestamp_millis();
        tx.execute("DELETE FROM phone_item_retries WHERE run_id=?", [run])?;
        tx.execute("DELETE FROM phone_peak_folders WHERE run_id=?", [run])?;
        inbox::release(&tx, run, key, scope.is_some())?;
        tx.execute(
            "UPDATE phone_runs SET safe_at=?,discovered=?,inventory=? WHERE id=?",
            params![safe_at, report.discovered, digest, run],
        )?;
        report.local_pending = inbox::pending_local(&tx)?;
        inventory::compact_folder_checkpoints(&tx, key)?;
        tx.commit()?;
        report.safe_at = Some(safe_at);
        report.inventory = Some(digest);
        report.phase = "原件已收齐，可以拔线".into();
        Ok(())
    })();
    report.current.clear();
    report.total_bytes = None;
    report.transferred_bytes = 0;
    if let Err(e) = result {
        #[cfg(windows)]
        if let Some(stop) = e.downcast_ref::<super::TransferStop>() {
            report.phase = stop.phase().into();
            return report;
        }
        #[cfg(windows)]
        if let Some(stop) = super::wpd::interruption(e.as_ref()) {
            report.phase = stop.phase().into();
            issue(&mut report, e.to_string());
            return report;
        }
        report.phase = "部分未完成".into();
        issue(&mut report, e.to_string());
    }
    report
}
