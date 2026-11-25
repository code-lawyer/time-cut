//! Fixture writer for libraries created before phone receipts used the inbox.
use super::{
    Phase, Report,
    archive::{Archive, Result, ScanEvent, Source},
    issue,
    resume::PhoneQueue,
    verification,
};
use std::path::Path;

pub(super) fn transfer(
    root: &Path,
    name: &str,
    make_source: impl FnOnce() -> Result<Box<dyn Source>>,
) -> Report {
    let mut report = Report {
        source: name.into(),
        phone: true,
        ..Default::default()
    };
    let run = (|| -> Result<()> {
        let source = make_source()?;
        let mut archive = Archive::open(root)?;
        let mut queue = PhoneQueue::open(&archive.db, source.identity())?;
        report.device_key = Some(verification::device_key(source.identity()));
        report.pending = queue.count();
        let mut reconciled = false;
        let mut receive = |event, scanning, queue: &mut PhoneQueue| -> Result<()> {
            let item = match event {
                ScanEvent::Inspected { media } => {
                    report.scanned += 1;
                    report.discovered += u64::from(media);
                    return Ok(());
                }
                ScanEvent::CompanionFound(item) => {
                    queue.discover(&archive.db, &item)?;
                    report.pending = queue.count();
                    return Ok(());
                }
                ScanEvent::Unreadable(message) => {
                    issue(&mut report, message);
                    return Ok(());
                }
                ScanEvent::Ready(item) => item,
                _ => return Ok(()),
            };
            queue.discover(&archive.db, &item)?;
            report.pending = queue.count();
            if scanning && queue.defer(&item) {
                return Ok(());
            }
            if !reconciled {
                archive.reconcile()?;
                reconciled = true;
            }
            let result = archive.import_tracked(
                source.as_ref(),
                &item,
                |db| queue.attempting(db, &item, None),
                |_| Ok(()),
            );
            match &result {
                Ok((_, digest)) => queue.verified(&archive.db, &item, digest)?,
                Err(error) => queue.attempting(&archive.db, &item, Some(&error.to_string()))?,
            }
            report.pending = queue.count();
            match result {
                Ok((true, _)) => report.added += 1,
                Ok((false, _)) => report.skipped += 1,
                Err(error) => {
                    #[cfg(windows)]
                    if error.is::<super::TransferStop>() {
                        return Err(error);
                    }
                    issue(&mut report, format!("{}：{error}", item.relative.display()));
                    #[cfg(windows)]
                    if let Some(stop) = super::wpd::interruption(error.as_ref()) {
                        return Err(stop.into());
                    }
                }
            }
            Ok(())
        };
        source.visit(&mut |event| receive(event, true, &mut queue))?;
        for item in queue.take_postponed() {
            receive(ScanEvent::Ready(item), false, &mut queue)?;
        }
        if report.failed == 0 && report.pending == 0 && report.discovered > 0
            && report.added + report.skipped == report.discovered
        {
            report.inventory = Some(queue.inventory_digest());
        }
        Ok(())
    })();
    if let Err(error) = run {
        #[cfg(windows)]
        if let Some(stop) = error.downcast_ref::<super::TransferStop>() {
            report.phase = stop.phase();
            return report;
        }
        issue(&mut report, error.to_string());
        #[cfg(windows)]
        if let Some(stop) = super::wpd::interruption(error.as_ref()) {
            report.phase = stop.phase();
            return report;
        }
    }
    report.phase = if report.failed > 0 {
        "部分未完成".into()
    } else if report.discovered == 0 {
        Phase::MediaUnavailable
    } else if report.pending > 0 || report.discovered != report.added + report.skipped {
        Phase::Pending
    } else {
        Phase::Completed
    };
    report
}
