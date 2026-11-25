use super::{
    Archive, Item, Location, Partial, Result, Source, extension, verified_local,
};
use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    time::UNIX_EPOCH,
};
use uuid::Uuid;
use sha1::{Digest, Sha1};

const BLOCK: usize = 256 * 1024;

pub(super) fn confirm(
    source: &dyn Source, item: &Item, size: u64, digest: &str,
    mut progress: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    let mut reader = source.reader(item)?;
    let mut buffer = vec![0; BLOCK];
    let mut hash = blake3::Hasher::new();
    let mut measured = 0;
    progress(0)?;
    loop {
        let count = super::observed::read_block(&mut *reader, &mut buffer, measured, &hash)?;
        if count == 0 { break; }
        measured += count as u64;
        if measured > size { return Err(super::WrongObject.into()); }
        hash.update(&buffer[..count]);
        progress(measured)?;
    }
    if measured != size || hash.finalize().to_hex().as_str() != digest {
        return Err(super::observed::ReturnedData::measured(Some(size), measured, &hash));
    }
    source.verify_read(item)
}

struct Staged {
    // Close the file before the cleanup guard removes an uncommitted original.
    file: File,
    cleanup: Partial,
    location: Location,
}

impl Staged {
    fn from_prefix(
        archive: &Archive,
        item: &Item,
        reference: Option<&mut File>,
        length: u64,
        expected: &blake3::Hasher,
    ) -> Result<Self> {
        let location = Location {
            blob: Some(format!("{}.{}", Uuid::new_v4(), extension(&item.relative))),
            archived: None,
        };
        let path = location.path(&archive.root)?;
        let mut staged = Self {
            file: File::create_new(&path)?,
            cleanup: Partial(Some(path)),
            location,
        };
        if length == 0 {
            return Ok(staged);
        }
        // The prefix already matched the phone byte for byte. Recover it from disk
        // and check it again in case a different application edited the local file.
        let reference = reference.ok_or("缺少已核对的本地文件")?;
        reference.rewind()?;
        let mut prefix = reference.take(length);
        let mut buffer = vec![0; BLOCK];
        let mut hash = blake3::Hasher::new();
        let mut copied = 0;
        loop {
            let n = prefix.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            staged.file.write_all(&buffer[..n])?;
            hash.update(&buffer[..n]);
            copied += n as u64;
        }
        if copied != length || hash.finalize() != expected.finalize() {
            return Err("接收期间本地参照文件发生变化，将重新核对".into());
        }
        Ok(staged)
    }
}

pub(super) struct Payload {
    pub digest: String,
    pub size: u64,
    pub location: Location,
    staged: Option<Staged>,
}

impl Payload {
    pub fn existing(digest: String,size: u64,location: Location) -> Self {
        Self {digest,size,location,staged:None}
    }
    pub fn is_new(&self) -> bool {
        self.staged.is_some()
    }
    pub fn committed(&mut self) {
        if let Some(staged) = &mut self.staged {
            staged.cleanup.0 = None;
        }
    }
}

/// Compare against a known local copy while reading the phone exactly once.
/// Allocate a new file only after divergence. Identical versions reuse the
/// verified original, including a match with a different same-name candidate.
pub(super) fn read(
    archive: &Archive,
    source: &dyn Source,
    item: &Item,
    candidates: &[(String, Location, Option<String>)],
    source_checksum: Option<&[u8;20]>,
    progress: &mut impl FnMut(u64, bool) -> Result<()>,
) -> Result<Payload> {
    let mut reference = None;
    for (digest, location, _) in candidates {
        let path = location.path(&archive.root)?;
        if verified_local(archive, &path, digest)? {
            reference = Some(File::open(path)?);
            break;
        }
    }
    if reference.is_none() && let Some(expected) = item.size.filter(|size| *size > 0) {
        reference = super::fingerprints::comparison_file(archive, expected)?;
    }
    let mut reader = source.reader(item)?;
    let mut incoming = vec![0; BLOCK];
    let mut existing = vec![0; if reference.is_some() { BLOCK } else { 0 }];
    let mut staged: Option<Staged> = None;
    let mut hash = blake3::Hasher::new();
    let mut sha1 = source_checksum.map(|_| Sha1::new());
    let mut size = 0;
    loop {
        let count = super::observed::read_block(&mut *reader, &mut incoming, size, &hash)?;
        if count == 0 {
            break;
        }
        let bytes = &incoming[..count];
        if staged.is_none() {
            let matches = match reference.as_mut() {
                Some(file) => match file.read_exact(&mut existing[..count]) {
                    Ok(()) => existing[..count] == *bytes,
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => false,
                    Err(error) => return Err(error.into()),
                },
                None => false,
            };
            if !matches {
                staged = Some(Staged::from_prefix(
                    archive,
                    item,
                    reference.as_mut(),
                    size,
                    &hash,
                )?);
            }
        }
        if let Some(staged) = &mut staged {
            staged.file.write_all(bytes)?;
        }
        hash.update(bytes);
        if let Some(hasher) = &mut sha1 { hasher.update(bytes); }
        size += count as u64;
        progress(size, staged.is_none())?;
    }
    if size == 0 || item.size.is_some_and(|expected| size != expected) {
        return Err(super::observed::ReturnedData::measured(item.size, size, &hash));
    }
    source.verify_read(item)?;
    let digest = hash.finalize().to_hex().to_string();
    if let (Some(expected),Some(hasher)) = (source_checksum,sha1) {
        let measured: [u8;20] = hasher.finalize().into();
        if &measured != expected { return Err(super::WrongObject.into()); }
        super::fingerprints::index(archive)?.remember(&digest,size,expected)?;
    }
    for (candidate, location, _) in candidates {
        if *candidate == digest && verified_local(archive, &location.path(&archive.root)?, &digest)?
        {
            return Ok(Payload {
                digest,
                size,
                location: location.clone(),
                staged: None,
            });
        }
    }
    // WPD can give identical bytes a different name. Reuse an existing archive
    // or inbox file even when the phone could not supply a source-side checksum.
    if let Some(location) = super::fingerprints::by_digest(archive, size, &digest)? {
        return Ok(Payload::existing(digest, size, location));
    }
    // A provider with no size hint can return just the prefix of an old file.
    let staged = match staged {
        Some(staged) => staged,
        None => Staged::from_prefix(archive, item, reference.as_mut(), size, &hash)?,
    };
    staged
        .file
        .set_modified(item.modified.unwrap_or(UNIX_EPOCH))?;
    staged.file.sync_all()?;
    Ok(Payload {
        digest,
        size,
        location: staged.location.clone(),
        staged: Some(staged),
    })
}
