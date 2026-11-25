//! Opt-in Windows hardware test of a receipt-owned directory, never a whole-phone verdict.
//! Set TIMECUT_ANDROID_FIXTURE_OPT_IN=read-only-wpd and TIMECUT_ANDROID_FIXTURE_RECEIPT
//! to the absolute test_android_mtp_fixture.ps1 receipt. Run once while seeded, again
//! after mutation. Each invocation opens two independent read-only WPD sessions.
use super::super::*;
use crate::ingest::{
    archive::{self, Archive, SavedFiles},
    audit_archive, capture, inbox,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::Write};

const LEAVES: [&str; 5] = [
    "sample.png",
    "alias.png",
    "nested/sample.png",
    "sample.xmp",
    "clip.mp4",
];

fn ensure(condition: bool, message: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn native_key(value: &str) -> String {
    hash(
        &value
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}
fn hex_key(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
#[derive(Deserialize)]
struct Expected {
    path: String,
    bytes: u64,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    format: String,
    folder: String,
    owner: String,
    device_key: String,
    storage_key: String,
    ownership_leaf: Option<String>,
    provider_marker_sha256: Option<String>,
    phase: String,
    seed: Vec<Expected>,
    mutation: Vec<Expected>,
}
impl Receipt {
    fn validate(&self) -> Result<()> {
        if let Some(expected) = &self.provider_marker_sha256 {
            ensure(
                self.marker() == "TimecutQaOwner.txt"
                    && *expected == hash(format!("/storage/emulated/0/{}", self.folder).as_bytes()),
                "Provider marker must name this exact directory and have a separate owner",
            )?;
        }
        ensure(
            self.format == "timecut-android-fixture-v1"
                && self
                    .folder
                    .strip_prefix("TimecutQa_")
                    .is_some_and(|id| hex_key(id, 32))
                && hex_key(&self.owner, 64)
                && hex_key(&self.device_key, 64)
                && hex_key(&self.storage_key, 64)
                && matches!(self.marker(), ".nomedia" | "TimecutQaOwner.txt")
                && matches!(self.phase.as_str(), "seeded" | "mutated"),
            "Invalid fixture receipt, phase or ownership marker",
        )?;
        for files in [&self.seed, &self.mutation] {
            ensure(
                files.len() == 5
                    && LEAVES.iter().all(|name| {
                        files.iter().any(|f| {
                            f.path == *name
                                && f.bytes > 0
                                && f.bytes <= 1024 * 1024
                                && hex_key(&f.sha256, 64)
                        })
                    }),
                "Invalid five-file expectation",
            )?;
        }
        let file = |phase: &[Expected], name: &str| -> (u64, String) {
            let f = phase.iter().find(|f| f.path == name).unwrap();
            (f.bytes, f.sha256.clone())
        };
        let old = file(&self.seed, "sample.png");
        let new = file(&self.mutation, "sample.png");
        ensure(
            old.0 == new.0
                && old.1 != new.1
                && old == file(&self.seed, "alias.png")
                && self
                    .seed
                    .iter()
                    .map(|f| &f.sha256)
                    .collect::<HashSet<_>>()
                    .len()
                    == 4
                && self
                    .mutation
                    .iter()
                    .map(|f| &f.sha256)
                    .collect::<HashSet<_>>()
                    .len()
                    == 5
                && LEAVES[1..]
                    .iter()
                    .all(|n| file(&self.seed, n) == file(&self.mutation, n)),
            "Invalid same-length mutation or duplicate test",
        )
    }
    fn marker(&self) -> &str {
        self.ownership_leaf.as_deref().unwrap_or(".nomedia")
    }
    fn expected(&self) -> &[Expected] {
        if self.phase == "seeded" {
            &self.seed
        } else {
            &self.mutation
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Binding {
    format: String,
    folder: String,
    owner: String,
    device_key: String,
    storage_key: String,
    native: [String; 5],
}

fn values(phone: &Phone, id: &str) -> Result<IPortableDeviceValues> {
    Ok(unsafe { phone.props.GetValues(&HSTRING::from(id), &phone.fields)? })
}
fn name(v: &IPortableDeviceValues) -> Result<String> {
    text_value(v, &WPD_OBJECT_ORIGINAL_FILE_NAME)
        .or_else(|| text_value(v, &WPD_OBJECT_NAME))
        .ok_or_else(|| "Fixture object has no name".into())
}
fn persistent(v: &IPortableDeviceValues) -> Result<String> {
    Ok(native_key(
        &text_value(v, &WPD_OBJECT_PERSISTENT_UNIQUE_ID)
            .ok_or("Missing persistent WPD identity")?,
    ))
}
fn parent_is(v: &IPortableDeviceValues, id: &str) -> Result<()> {
    ensure(
        text_value(v, &WPD_OBJECT_PARENT_ID).as_deref() == Some(id),
        "WPD object parent differs from enumeration",
    )
}
fn unique_child(
    phone: &Phone,
    parent: &str,
    mut select: impl FnMut(&IPortableDeviceValues) -> Result<bool>,
) -> Result<(String, IPortableDeviceValues)> {
    let mut found = None;
    for id in phone.children(parent)? {
        let v = values(phone, &id)?;
        if select(&v)? {
            parent_is(&v, parent)?;
            ensure(found.is_none(), "Ambiguous device, storage or directory")?;
            found = Some((id, v));
        }
    }
    found.ok_or_else(|| "Expected fixture object is unavailable".into())
}
struct Scoped {
    phone: Phone,
    id: String,
    physical: [String; 5],
    storage: String,
    root: String,
    sub: String,
    path: PathBuf,
    marker: String,
    owner: String,
    provider_marker_sha256: Option<String>,
}
impl Scoped {
    fn open(receipt: &Receipt, expected: Option<&[String; 5]>) -> Result<Self> {
        let devices = devices()?;
        ensure(
            devices.len() == 1 && devices[0].id.to_ascii_lowercase().contains("vid_04e8"),
            "Connect exactly one Samsung WPD device",
        )?;
        let interface = native_key(&devices[0].id.to_ascii_lowercase());
        ensure(
            expected.is_none_or(|k| k[0] == interface),
            "A different USB interface is attached",
        )?;
        let phone = Phone::open(&devices[0])?; // The production client requests GENERIC_READ.
        let serial = phone
            .identity()
            .strip_prefix("wpd:")
            .filter(|s| !s.is_empty() && *s != devices[0].id)
            .ok_or("WPD did not supply the hardware serial")?;
        let physical = native_key(serial);
        ensure(
            expected.is_none_or(|k| k[1] == physical),
            "Different physical device",
        )?;
        // Only enumerate immediate device/storage children to locate the exact GUID. No
        // sibling folder is descended into and no unrelated content stream is requested.
        let (storage, sv) = unique_child(&phone, "DEVICE", |v| {
            let group = unsafe { v.GetGuidValue(&WPD_FUNCTIONAL_OBJECT_CATEGORY) }.ok();
            Ok(group == Some(WPD_FUNCTIONAL_CATEGORY_STORAGE)
                && (expected.is_none() || expected.unwrap()[2] == persistent(v)?))
        })?;
        let is_folder = |v: &IPortableDeviceValues| {
            unsafe { v.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE) }.ok()
                == Some(WPD_CONTENT_TYPE_FOLDER)
        };
        let (root, rv) = unique_child(&phone, &storage, |v| {
            Ok(is_folder(v) && name(v)? == receipt.folder)
        })?;
        let (sub, nv) = unique_child(&phone, &root, |v| Ok(is_folder(v) && name(v)? == "nested"))?;
        let identity = [
            interface,
            physical,
            persistent(&sv)?,
            persistent(&rv)?,
            persistent(&nv)?,
        ];
        ensure(
            expected.is_none_or(|keys| keys == &identity),
            "Bound directory or storage was replaced",
        )?;
        let storage_name = name(&sv)?.replace(['/', '\\', ':'], "_");
        ensure(
            !matches!(storage_name.as_str(), "" | "." | ".."),
            "Invalid storage name",
        )?;
        let path = PathBuf::from(storage_name).join(&receipt.folder);
        let scope = Self {
            phone,
            id: format!("wpd:qa-{}-{}", identity[1], receipt.folder),
            physical: identity,
            storage,
            root,
            sub,
            path,
            marker: receipt.marker().into(),
            owner: receipt.owner.clone(),
            provider_marker_sha256: receipt.provider_marker_sha256.clone(),
        };
        scope.check_marker()?;
        Ok(scope)
    }
    fn check_marker(&self) -> Result<()> {
        self.check_named_marker(&self.marker, &self.owner)?;
        if let Some(expected) = &self.provider_marker_sha256 {
            self.check_named_marker(".nomedia", expected)?;
        }
        Ok(())
    }
    fn check_named_marker(&self, leaf: &str, expected: &str) -> Result<()> {
        let (id, v) = unique_child(&self.phone, &self.root, |v| Ok(name(v)? == leaf))?;
        ensure(
            unsafe { v.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE) }? != WPD_CONTENT_TYPE_FOLDER,
            "Marker became a directory",
        )?;
        let item = Item {
            id,
            relative: self.path.join(leaf),
            size: None,
            modified: None,
            version: None,
        };
        let mut bytes = Vec::new();
        self.phone
            .reader(&item)?
            .take(257)
            .read_to_end(&mut bytes)?;
        ensure(
            bytes.len() <= 256 && hash(&bytes) == expected,
            "Fixture ownership marker mismatch",
        )
    }
    fn permitted(&self, item: &Item) -> bool {
        item.relative
            .strip_prefix(&self.path)
            .ok()
            .is_some_and(|p| LEAVES.iter().any(|n| p == Path::new(n)))
    }
    fn scan(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> {
        self.check_marker()?;
        parent_is(&values(&self.phone, &self.root)?, &self.storage)?;
        let nested = values(&self.phone, &self.sub)?;
        parent_is(&nested, &self.root)?;
        ensure(name(&nested)? == "nested", "Owned subfolder was renamed")?;
        let mut folders = Vec::new();
        let mut leaves = HashSet::new();
        for (id, path) in [
            (&self.root, self.path.clone()),
            (&self.sub, self.path.join("nested")),
        ] {
            emit(ScanEvent::DirectoryFound(path.clone()))?;
            emit(ScanEvent::DirectoryStart(path.clone()))?;
            let children = self.phone.children(id)?;
            if id == &self.root {
                ensure(
                    children.contains(&self.sub),
                    "Owned subfolder was moved or replaced",
                )?;
            }
            folders.push((id, directory_fingerprint(&children)));
            let mut files = Vec::new();
            for child in children {
                let v = values(&self.phone, &child)?;
                parent_is(&v, id)?;
                let leaf = name(&v)?;
                if id == &self.root && child == self.sub && leaf == "nested" {
                    continue;
                }
                if id == &self.root
                    && (leaf == self.marker
                        || (self.marker == "TimecutQaOwner.txt" && leaf == ".nomedia"))
                {
                    continue;
                }
                ensure(
                    LEAVES.iter().any(|n| self.path.join(n) == path.join(&leaf)),
                    "Unexpected entry in the owned fixture; refusing its content",
                )?;
                let Object::Media(item) = self.phone.object(&child, &path)? else {
                    return Err("Fixture leaf is not WPD media".into());
                };
                ensure(
                    self.permitted(&item) && leaves.insert(item.relative.clone()),
                    "Duplicate or out-of-scope WPD item",
                )?;
                emit(ScanEvent::Inspected { media: true })?;
                files.push(item);
            }
            files.sort_by_key(|i| ordered(&i.relative));
            for file in &files {
                emit(ScanEvent::Ready(file.clone()))?;
            }
            emit(ScanEvent::DirectoryDone { path, media: files })?;
        }
        ensure(
            leaves.len() == LEAVES.len(),
            "Expected exactly five test media entries",
        )?;
        for (index, (id, before)) in folders.iter().enumerate() {
            ensure(
                directory_fingerprint(&self.phone.children(id)?) == *before,
                "Scoped directory changed during capture",
            )?;
            emit(ScanEvent::DirectoryCheck {
                done: index as u64 + 1,
                total: folders.len() as u64,
            })?;
        }
        Ok(())
    }
}
impl Source for Scoped {
    fn identity(&self) -> &str {
        &self.id
    }
    fn is_phone(&self) -> bool {
        true
    }
    fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> {
        self.scan(emit)
    }
    fn reader(&self, item: &Item) -> Result<Box<dyn Read>> {
        ensure(self.permitted(item), "Refusing an out-of-scope stream")?;
        let parent = if item.relative.parent() == Some(self.path.as_path()) {
            &self.root
        } else {
            &self.sub
        };
        ensure(
            self.phone.children(parent)?.contains(&item.id),
            "Media handle is no longer in the owned directory",
        )?;
        self.phone.reader(item)
    }
    fn verify_read(&self, item: &Item) -> Result<()> {
        ensure(self.permitted(item), "Out-of-scope item")?;
        self.phone.verify_read(item)
    }
    fn confirms_returned_object(&self, item: &Item) -> Result<bool> {
        Ok(self.permitted(item) && self.phone.confirms_returned_object(item)?)
    }
    fn saved_files(&self) -> Option<&dyn SavedFiles> {
        Some(self)
    }
}
impl SavedFiles for Scoped {
    fn resolve(&self, item: &Item) -> Result<Option<Item>> {
        ensure(self.permitted(item), "Refusing an out-of-scope saved item")?;
        SavedFiles::resolve(&self.phone, item)
    }
}

#[derive(serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Evidence {
    scope: String,
    folder: String,
    owner: String,
    physical: [String; 5],
    archive: String,
    runs: Vec<Round>,
}
#[derive(serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Round {
    id: i64,
    safe_at: i64,
    entries: u64,
    added: u64,
    skipped: u64,
    rechecked: u64,
    archive_files_verified: u64,
}

fn evidence_file(path: &Path, value: &Evidence) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    serde_json::to_writer_pretty(&file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)?;
    Ok(())
}

fn finish_round(
    root: &Path,
    prefix: &Path,
    expected: &[Expected],
    seed: &[Expected],
    added: u64,
    source: Box<dyn Source>,
) -> Result<Round> {
    // receive owns and drops the live Source. The filing and audits below have only
    // a local path. They cannot make a new request for media from the device.
    let report = capture::receive(
        root,
        "Controlled Android fixture",
        |_| Ok(5),
        || Ok(source),
        |_| {},
        || Ok(()),
        || Ok(()),
    );
    let safe = report.safe_at.ok_or_else(|| {
        format!(
            "Scoped capture did not reach safe_at: {} {:?}",
            report.phase, report.issues
        )
    })?;
    ensure(
        report.rechecked == 5
            && report.discovered == 5
            && report.pending == 0
            && report.failed == 0
            && report.added == added
            && report.skipped == 5 - added,
        "Unexpected scoped capture counts",
    )?;
    let mut archive = Archive::open(root)?;
    while inbox::file_next(&mut archive)?.is_some() {}
    let db = &archive.db;
    let run: i64 = db.query_row(
        "SELECT id FROM phone_runs WHERE safe_at=? ORDER BY id DESC LIMIT 1",
        [safe],
        |r| r.get(0),
    )?;
    let rows:i64=db.query_row("SELECT COUNT(*) FROM phone_list_items l JOIN phone_manifest m ON m.run_id=l.run_id AND m.ordinal=l.ordinal
        AND m.entry=l.entry AND m.version IS l.version AND m.digest=l.digest AND m.size=l.size WHERE l.run_id=?",[run],|r|r.get(0))?;
    ensure(rows == 5, "Manifest does not match the scoped list")?;
    let pending: i64 = db.query_row(
        "SELECT (SELECT COUNT(*) FROM phone_inbox)+(SELECT COUNT(*) FROM phone_local_retries)",
        [],
        |r| r.get(0),
    )?;
    ensure(pending == 0, "Offline filing has unresolved receipts")?;
    let mut local = BTreeMap::new();
    {
        let mut q = db.prepare("SELECT path,digest FROM files")?;
        for row in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (path, key) = row?;
            let path = PathBuf::from(path);
            ensure(archive::archive_relative(&path), "Unsafe archived path")?;
            let data = fs::read(root.join(path))?;
            local
                .entry(key)
                .or_insert_with(Vec::new)
                .push((hash(&data), data.len() as u64));
        }
    }
    for file in expected {
        let entry = file
            .path
            .split('/')
            .fold(prefix.to_owned(), |path, name| path.join(name))
            .to_string_lossy()
            .into_owned();
        let digest: String = db
            .query_row(
                "SELECT digest FROM phone_list_items WHERE run_id=? AND entry=?",
                rusqlite::params![run, entry],
                |r| r.get(0),
            )
            .map_err(|e| format!("Looking up scoped media {} in run {run}: {e}", file.path))?;
        ensure(
            local.get(&digest).is_some_and(|paths| {
                paths
                    .iter()
                    .all(|(hash, bytes)| (hash, *bytes) == (&file.sha256, file.bytes))
            }),
            "Filed test original differs from the fixture SHA256",
        )?;
        if file.path.ends_with(".png") {
            ensure(
                local[&digest].len() == 1,
                "Duplicate physical PNG copies in test archive",
            )?;
        }
    }
    for file in seed {
        ensure(
            local
                .values()
                .flatten()
                .any(|(hash, n)| hash == &file.sha256 && *n == file.bytes),
            "A previously filed original disappeared",
        )?;
    }
    drop(archive);
    let audit = audit_archive::verify(root)?;
    ensure(
        audit.failed == 0 && audit.files_verified >= 4,
        "Offline archive verification failed",
    )?;
    Ok(Round {
        id: run,
        safe_at: safe,
        entries: rows as u64,
        added: report.added,
        skipped: report.skipped,
        rechecked: report.rechecked,
        archive_files_verified: audit.files_verified,
    })
}

#[test]
#[ignore = "requires explicit read-only Samsung USB fixture opt-in; test-build directory scope only"]
fn owned_android_fixture_capture_and_repeat() -> Result<()> {
    ensure(
        std::env::var("TIMECUT_ANDROID_FIXTURE_OPT_IN").as_deref() == Ok("read-only-wpd"),
        "Expected explicit read-only WPD opt-in",
    )?;
    let receipt = PathBuf::from(
        std::env::var_os("TIMECUT_ANDROID_FIXTURE_RECEIPT")
            .ok_or("Missing absolute receipt path")?,
    );
    ensure(receipt.is_absolute(), "Receipt must be absolute")?;
    let path = receipt.canonicalize()?;
    let state: Receipt = serde_json::from_reader(fs::File::open(&path)?)?;
    state.validate()?;
    let sibling = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    let data = sibling(".data").canonicalize()?;
    let marker = fs::read(data.join("seed/.nomedia"))?;
    let payload = std::str::from_utf8(&marker)?;
    ensure(
        hash(&marker) == state.owner
            && payload
                .strip_prefix(&format!("{}/", state.folder))
                .is_some_and(|token| hex_key(token, 32)),
        "Local ownership nonce differs",
    )?;
    let bound_path = sibling(".wpd.json");
    let bound = match fs::File::open(bound_path) {
        Ok(file) => {
            let b: Binding = serde_json::from_reader(file)?;
            ensure(
                b.format == "timecut-wpd-empty-dirs-v1"
                    && b.folder == state.folder
                    && b.owner == state.owner
                    && b.device_key == state.device_key
                    && b.storage_key == state.storage_key
                    && b.native.iter().all(|k| hex_key(k, 64)),
                "Native binding differs from fixture receipt",
            )?;
            Some(b.native)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let seed_path = data.join("scoped-wpd-seed.json");
    let prior: Option<Evidence> = if state.phase == "mutated" {
        Some(serde_json::from_reader(fs::File::open(&seed_path)?)?)
    } else {
        None
    };
    if let Some(p) = &prior {
        ensure(
            p.scope == "test-build-owned-directory"
                && p.owner == state.owner
                && p.folder == state.folder
                && p.runs.len() == 2
                && p.runs[0].added == 4
                && p.runs[1].added == 0
                && bound.as_ref().is_none_or(|keys| keys == &p.physical)
                && p.archive
                    .strip_prefix("scoped-archive-")
                    .is_some_and(|id| hex_key(id, 32)),
            "Seed proof for this physical fixture is missing",
        )?;
    }
    let _apt = Apartment::new()?;
    let live = Scoped::open(
        &state,
        prior.as_ref().map(|p| &p.physical).or(bound.as_ref()),
    )?;
    let name = prior
        .as_ref()
        .map(|p| p.archive.clone())
        .unwrap_or_else(|| format!("scoped-archive-{}", uuid::Uuid::new_v4().simple()));
    let root = data.join(&name);
    if prior.is_none() {
        fs::create_dir(&root)?;
    }
    ensure(
        root.canonicalize()?.parent() == Some(data.as_path()),
        "Archive outside controlled fixture data",
    )?;
    let prefix = live.path.clone();
    let keys = live.physical.clone();
    let expected = state.expected();
    let initial = finish_round(
        &root,
        &prefix,
        expected,
        &state.seed,
        if state.phase == "seeded" { 4 } else { 1 },
        Box::new(live),
    )?;
    if let Some(p) = &prior {
        ensure(
            p.runs[1].id < initial.id,
            "Expected a fresh mutation capture",
        )?;
    }
    let again = Scoped::open(&state, Some(&keys))?;
    ensure(
        again.path == prefix,
        "Storage path changed between fixture runs",
    )?;
    let repeat = finish_round(&root, &prefix, expected, &state.seed, 0, Box::new(again))?;
    ensure(
        repeat.id > initial.id,
        "Repeat must independently re-list and verify the fixture",
    )?;
    let evidence = Evidence {
        scope: "test-build-owned-directory".into(),
        folder: state.folder.clone(),
        owner: state.owner.clone(),
        physical: keys,
        archive: name,
        runs: vec![initial, repeat],
    };
    let result = if prior.is_none() {
        seed_path
    } else {
        data.join("scoped-wpd-mutation.json")
    };
    evidence_file(&result, &evidence)?;
    println!("Scoped test-build evidence: {}", result.display());
    Ok(())
}

// Exercise the same scoped adapter, WPD property/stream handling and local archive assertions
// without opening a hardware device. The extra inaccessible folder must never be traversed.
#[test]
fn scoped_rounds_use_only_owned_wpd_content() -> Result<()> {
    use super::{Kind, SizeHint, phone, row};
    use crate::ingest::tests::Sandbox;
    use std::sync::Mutex;

    let _apt = Apartment::new()?;
    let temp = Sandbox::new();
    let destination = temp.dir("fixture-only");
    let parent = Path::new("Internal/TimecutQa_12345678123456781234567812345678");
    let marker = b"TimecutQa_12345678123456781234567812345678/0123456789abcdff0123456789abcdff";
    let files: [(&str, &'static [u8]); 5] = [
        ("sample.png", b"one"),
        ("alias.png", b"one"),
        ("nested/sample.png", b"two"),
        ("sample.xmp", b"sidecar"),
        ("clip.mp4", b"video"),
    ];
    let expectations = |files: &[(&str, &[u8])]| {
        files
            .iter()
            .map(|(path, body)| Expected {
                path: path.to_string(),
                bytes: body.len() as u64,
                sha256: hash(body),
            })
            .collect::<Vec<_>>()
    };
    let seed = expectations(&files);
    let mut changed = files;
    changed[0].1 = b"new";
    let mutation = expectations(&changed);
    for (i, (entries, expected, added)) in [
        (&files, &seed, 4),
        (&files, &seed, 0),
        (&changed, &mutation, 1),
        (&changed, &mutation, 0),
    ]
    .into_iter()
    .enumerate()
    {
        let number = i as u32 + 1;
        let root = parent.to_string_lossy().replace('\\', "/");
        let mut rows = vec![
            row("Internal", Kind::Storage),
            row(&root, Kind::Folder),
            row(&format!("{root}/nested"), Kind::Folder),
            row(
                &format!("{root}/TimecutQaOwner.txt"),
                Kind::File(marker, SizeHint::Object),
            ),
            row(
                &format!("{root}/.nomedia"),
                Kind::File(
                    b"/storage/emulated/0/TimecutQa_12345678123456781234567812345678",
                    SizeHint::Object,
                ),
            ),
            row("Internal/unrelated", Kind::ReadFaultFolder),
        ];
        for (leaf, content) in entries {
            rows.push(row(
                &format!("{root}/{leaf}"),
                Kind::File(content, SizeHint::Object),
            ));
        }
        let opened = Arc::new(Mutex::new(Vec::new()));
        let source = Scoped {
            phone: phone(number, rows, None, opened.clone()),
            id: "wpd:qa-simulated".into(),
            physical: Default::default(),
            storage: format!("{number}-0"),
            root: format!("{number}-1"),
            sub: format!("{number}-2"),
            path: parent.into(),
            marker: "TimecutQaOwner.txt".into(),
            owner: hash(marker),
            provider_marker_sha256: Some(hash(
                b"/storage/emulated/0/TimecutQa_12345678123456781234567812345678",
            )),
        };
        finish_round(
            &destination,
            parent,
            expected,
            &seed,
            added,
            Box::new(source),
        )?;
        ensure(
            opened
                .lock()
                .unwrap()
                .iter()
                .all(|p| p.starts_with(&format!("{root}/"))),
            "Read outside the test directory",
        )?;
    }
    Ok(())
}
