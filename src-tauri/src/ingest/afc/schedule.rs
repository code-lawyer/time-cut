use super::{AfcError, IdeviceError};
use crate::ingest::archive::{CopyOrder, Item, Result, ScanEvent, Source};
use rusqlite::{Connection, params};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    path::PathBuf,
};

#[derive(Default)]
pub(super) struct Plan {
    pub order: CopyOrder,
    saved: HashMap<PathBuf, HashSet<String>>,
    pending: Vec<Item>,
    pending_versions: HashMap<PathBuf, HashSet<String>>,
    pub saved_count: u64,
}

impl Plan {
    pub fn load(db: &Connection, run: i64, source: &str) -> Result<Self> {
        let mut plan = Self::default();
        let mut query = db.prepare(
            "SELECT n.entry,n.version,n.size,n.digest IS NOT NULL,n.conflict
             FROM phone_native_items n LEFT JOIN phone_native_pickups p USING(run_id,source,entry,version)
             WHERE n.run_id=? AND n.source=? ORDER BY COALESCE(p.attempted,0),n.entry,n.version",
        )?;
        let mut rows = query.query(params![run, source])?;
        while let Some(row) = rows.next()? {
            let path = PathBuf::from(row.get::<_, String>(0)?);
            let version: String = row.get(1)?;
            let saved: bool = row.get(3)?;
            let conflict: bool = row.get(4)?;
            let rank = if saved && !conflict { 3 } else { 2 };
            for location in path
                .parent()
                .into_iter()
                .chain(std::iter::once(path.as_path()))
            {
                let priority = plan
                    .order
                    .priorities
                    .entry(location.to_owned())
                    .or_insert(rank);
                *priority = (*priority).min(rank);
            }
            if saved && !conflict {
                if let Some(parent) = path.parent() {
                    *plan.order.folders.entry(parent.to_owned()).or_default() += 1;
                }
                plan.saved.entry(path).or_default().insert(version);
                plan.saved_count += 1;
            } else {
                plan.pending_versions
                    .entry(path.clone())
                    .or_default()
                    .insert(version.clone());
                if conflict {
                    continue;
                }
                plan.pending.push(Item {
                    id: super::remote_path(&path)?,
                    relative: path,
                    version: Some(version),
                    size: Some(row.get(2)?),
                    modified: None,
                });
            }
        }
        Ok(plan)
    }

    pub fn saved(&self, item: &Item) -> bool {
        self.saved
            .get(&item.relative)
            .zip(item.version.as_ref())
            .is_some_and(|(versions, version)| versions.contains(version))
    }

    fn pending(&self, item: &Item) -> bool {
        self.pending_versions
            .get(&item.relative)
            .zip(item.version.as_ref())
            .is_some_and(|(versions, version)| versions.contains(version))
    }
}

// Keep the remote error in the chain. A database error, destination change, bad
// checksum or disconnected socket must stop the attempt immediately.
pub(super) fn afc_status<'a>(mut error: &'a (dyn Error + 'static)) -> Option<&'a AfcError> {
    loop {
        if let Some(IdeviceError::Afc(e)) = error.downcast_ref::<IdeviceError>() {
            return Some(e);
        }
        error = error.source()?;
    }
}

pub(super) fn unavailable(error: &(dyn Error + 'static)) -> bool {
    matches!(
        afc_status(error),
        Some(
            AfcError::NoResources
                | AfcError::ObjectNotFound
                | AfcError::PermDenied
                | AfcError::ObjectBusy
                | AfcError::OpWouldBlock
                | AfcError::ReadError
        )
    )
}

/// Previously saved files are checked again after unseen files in *every*
/// directory have had their turn. They still participate in the full manifest.
pub(super) fn visit(
    source: &dyn Source,
    plan: &Plan,
    mut discover: impl FnMut(&Item) -> Result<()>,
    mut record_attempt: impl FnMut(&Item) -> Result<()>,
    mut emit: impl FnMut(ScanEvent) -> Result<()>,
) -> Result<()> {
    let mut seen = HashSet::new();
    let mut saved = Vec::new();
    let mut pending = Vec::new();
    let mut failures = Failures::default();
    let mut callback_failed = false;
    let scan = source.visit_for_copy(&plan.order, &mut |event| {
        let outcome = (|| {
            let item = match event {
                ScanEvent::Ready(item) => item,
                other => return emit(other),
            };
            if !seen.insert(item.id.clone()) {
                return Err("iPhone 原件清单包含重复路径".into());
            }
            if item.size.is_none_or(|n| n == 0) {
                return Err("iPhone 原件长度无效".into());
            }
            discover(&item)?;
            if plan.saved(&item) {
                saved.push(item);
            } else if plan.pending(&item) {
                pending.push(item);
            } else {
                failures.receive(item, &mut emit)?;
            }
            Ok(())
        })();
        callback_failed |= outcome.is_err();
        outcome
    });
    if let Err(error) = scan {
        if !callback_failed
            && unavailable(error.as_ref())
            && let Some(resolver) = source.saved_files()
        {
            // Direct reads only add individual receipts. The failed listing below
            // remains authoritative for both unplugging and independent verification.
            for expected in plan.pending.iter().take(8) {
                record_attempt(expected)?;
                match resolver.resolve(expected) {
                    Ok(Some(item))
                        if item.relative == expected.relative
                            && item.version == expected.version
                            && item.size == expected.size =>
                    {
                        failures.receive(item, &mut emit)?;
                    }
                    Ok(_) => continue,
                    Err(e) if unavailable(e.as_ref()) => failures.note(e)?,
                    Err(e) => return Err(e),
                }
            }
        }
        return Err(error);
    }
    for item in pending {
        failures.receive(item, &mut emit)?;
    }
    // Even one missing original prevents a successful full recheck. Spend
    // the remaining connection time on pickups and postpone verified files.
    if let Some(error) = failures.first.take() {
        return Err(error);
    }
    for item in saved {
        emit(ScanEvent::Ready(item))?;
    }
    Ok(())
}

#[derive(Default)]
struct Failures {
    consecutive: u8,
    first: Option<Box<dyn Error + Send + Sync>>,
}

impl Failures {
    fn receive(
        &mut self,
        item: Item,
        emit: &mut impl FnMut(ScanEvent) -> Result<()>,
    ) -> Result<()> {
        match emit(ScanEvent::Ready(item)) {
            Ok(()) => {
                self.consecutive = 0;
                Ok(())
            }
            Err(e) if unavailable(e.as_ref()) => self.note(e),
            Err(e) => Err(e),
        }
    }
    fn note(&mut self, error: Box<dyn Error + Send + Sync>) -> Result<()> {
        self.consecutive += 1;
        if self.consecutive >= 3 {
            return Err(error);
        }
        self.first.get_or_insert(error);
        Ok(())
    }
}
