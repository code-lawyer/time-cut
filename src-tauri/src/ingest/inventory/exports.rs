use std::{collections::HashMap, path::Path};

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(in crate::ingest) struct Shortfall {
    pub required: u64,
    pub available: u64,
}

impl Shortfall {
    pub fn check(root: &Path, required: u64) -> Option<Self> {
        if required == 0 { return None; }
        #[cfg(windows)]
        {
            use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::HSTRING};
            let mut available = 0;
            // The caller's free space accounts for quotas on the actual gallery volume.
            unsafe { GetDiskFreeSpaceExW(&HSTRING::from(root.as_os_str()), Some(&mut available), None, None) }.ok()?;
            (required > available).then_some(Self { required, available })
        }
        #[cfg(not(windows))]
        { let _ = root; None }
    }
}

type ObjectKey = (String, String, u128);
type ObjectVersions = HashMap<ObjectKey, Option<(u64, String)>>;

/// Windows can transcode photos and videos over USB. This only identifies work
/// that can wait. A changed export never acquires the earlier file's receipt.
#[derive(Default)]
pub(super) struct Originals {
    heif: ObjectVersions,
    movies: ObjectVersions,
    backed: HashMap<(String, u64, u128), Option<String>>,
}

fn identity(path: &Path, version: &str, size: u64) -> Option<ObjectKey> {
    let (prefix, time) = version.rsplit_once('/')?;
    let (object, reported_size) = prefix.rsplit_once('/')?;
    if object.is_empty() || size == 0 || reported_size.parse::<u64>().ok()? != size {
        return None;
    }
    let stem = path
        .with_extension("")
        .to_string_lossy()
        .to_ascii_lowercase();
    Some((stem, object.to_owned(), time.parse().ok()?))
}

fn remember(versions: &mut ObjectVersions, key: ObjectKey, size: u64, digest: &str) {
    versions.entry(key).and_modify(|known| {
        if !known.as_ref().is_some_and(|(length, hash)| *length == size && hash == digest) {
            *known = None;
        }
    }).or_insert_with(|| Some((size, digest.to_owned())));
}

impl Originals {
    fn versions(&mut self, path: &Path) -> Option<&mut ObjectVersions> {
        let ext = path.extension()?;
        if ext.eq_ignore_ascii_case("heic") || ext.eq_ignore_ascii_case("heif") { Some(&mut self.heif) }
        else if ext.eq_ignore_ascii_case("mov") { Some(&mut self.movies) } else { None }
    }

    pub fn record(&mut self, path: &Path, version: &str, size: u64, digest: &str) {
        let Some(versions) = self.versions(path) else { return };
        let Some(key) = identity(path, version, size) else { return };
        let signature = (path.to_string_lossy().to_ascii_lowercase(), size, key.2);
        remember(versions, key, size, digest);
        self.backed.entry(signature).and_modify(|known| {
            if known.as_deref() != Some(digest) { *known = None; }
        }).or_insert_with(|| Some(digest.to_owned()));
    }

    // A completed earlier listing can connect today's export ID to a specific raw
    // filename, even if Windows changed all IDs since its bytes were backed up.
    // This evidence is never added to the exact-version or content receipt index.
    pub fn observe(&mut self, path: &Path, version: &str, size: u64, modified: &str) {
        let Some(key) = identity(path, version, size) else { return };
        if modified.parse::<u128>().ok() != Some(key.2) { return; }
        let signature = (path.to_string_lossy().to_ascii_lowercase(), size, key.2);
        let Some(Some(digest)) = self.backed.get(&signature).cloned() else { return };
        if let Some(versions) = self.versions(path) { remember(versions, key, size, &digest); }
    }

    pub fn may_be_export(&self, item: &super::Item) -> bool {
        let (Some(ext), Some(version), Some(size)) = (
            item.relative.extension(),
            item.version.as_deref(),
            item.size,
        ) else {
            return false;
        };
        let Some(key) = identity(&item.relative, version, size) else { return false };
        if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg") {
            self.heif.get(&key).is_some_and(Option::is_some)
        } else if ext.eq_ignore_ascii_case("mov") {
            self.movies.get(&key).and_then(Option::as_ref).is_some_and(|(length, _)| *length != size)
        } else { false }
    }
}
