#[cfg(windows)]
use super::transfer_guarded;
use super::{archive::*, transfer};
use std::{
    cell::RefCell,
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

pub(super) struct Sandbox(PathBuf);

#[test]
fn another_archives_staging_area_cannot_be_imported_or_used_as_a_destination() {
    let files = Sandbox::new();
    let originals = files.dir("library");
    let second = files.dir("second-library");
    drop(Archive::open(&originals).unwrap());
    let internal = originals.join(".time-cut/staging");
    let incomplete = internal.join("unfinished.jpg");
    fs::write(&incomplete, b"incomplete bytes").unwrap();

    assert!(Folder::new(&internal, &second).is_err());
    assert!(media_directory(&internal).is_err());
    assert!(Archive::open(&internal).is_err());
    assert!(!internal.join(".time-cut").exists());
    assert_eq!(fs::read(incomplete).unwrap(), b"incomplete bytes");
    assert!(Folder::new(&originals, &second).is_ok());
}

impl Sandbox {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("timecut-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub(super) fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn png(path: &Path, value: u8) -> Vec<u8> {
    image::RgbImage::from_pixel(4, 4, image::Rgb([value, 100, 150]))
        .save(path)
        .unwrap();
    fs::read(path).unwrap()
}
pub(super) fn originals(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && !e.path().starts_with(root.join(".time-cut")))
        .map(|e| e.into_path())
        .collect()
}
fn local(source: &Path, dest: &Path) -> super::Report {
    transfer(
        dest,
        "fixture",
        || Ok(Box::new(Folder::new(source, dest)?)),
        |_| {},
    )
}

#[test]
fn local_content_matches_across_names_folders_and_preexisting_files() {
    let t = Sandbox::new();
    let dest = t.dir("图库");
    let a = t.dir("手机");
    let b = t.dir("下载");
    let one = png(&dest.join("旧原件.png"), 10);
    fs::write(a.join("IMG0001.png"), one).unwrap();
    png(&b.join("IMG0001.png"), 50);
    assert_eq!((local(&a, &dest).skipped, local(&b, &dest).added), (1, 1));
    let copied = local(&b, &dest);
    assert_eq!((copied.added, copied.skipped, copied.failed), (0, 1, 0));
    assert_eq!(originals(&dest).len(), 2);
    assert!(Folder::new(&t.0, &dest).is_err());
    assert!(Folder::new(&dest, &dest).is_err());
}

#[test]
fn local_duplicates_need_no_staging_space_and_same_size_changes_are_preserved() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let source = t.dir("source");
    let input = source.join("IMG.jpg");
    fs::write(dest.join("old-name.jpg"), b"original").unwrap();
    fs::write(&input, b"original").unwrap();
    let original_time = fs::metadata(&input).unwrap().modified().unwrap();
    let mut archive = Archive::open(&dest).unwrap();
    archive.reconcile().unwrap();
    let folder = Folder::new(&source, &dest).unwrap();

    let stage = dest.join(".time-cut/staging");
    let parked = dest.join(".time-cut/parked");
    fs::rename(&stage, &parked).unwrap();
    let mut outcomes = vec![];
    folder
        .visit(&mut |event| {
            if let ScanEvent::Ready(item) = event {
                outcomes.push(archive.import(&folder, &item, |_| Ok(()))?);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(outcomes, [false]);
    fs::rename(&parked, &stage).unwrap();
    fs::write(&input, b"modified").unwrap();
    fs::File::options()
        .write(true)
        .open(&input)
        .unwrap()
        .set_modified(original_time)
        .unwrap();
    folder
        .visit(&mut |event| {
            if let ScanEvent::Ready(item) = event {
                outcomes.push(archive.import(&folder, &item, |_| Ok(()))?);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(outcomes, [false, true]);
    let mut all: Vec<_> = originals(&dest)
        .into_iter()
        .map(fs::read)
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    all.sort();
    assert_eq!(all, [b"modified", b"original"]);
}

#[test]
fn user_can_edit_an_archived_picture_and_restore_its_timestamp_before_reimport() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let source = t.dir("source");
    let existing = dest.join("original.jpg");
    fs::write(&existing, b"old bytes").unwrap();
    let previous = fs::metadata(&existing).unwrap().modified().unwrap();
    assert_eq!(local(&source, &dest).failed, 0);
    fs::write(&existing, b"new bytes").unwrap();
    fs::File::options()
        .write(true)
        .open(&existing)
        .unwrap()
        .set_modified(previous)
        .unwrap();
    fs::write(source.join("copy under another name.jpg"), b"new bytes").unwrap();
    let result = local(&source, &dest);
    assert_eq!((result.added, result.skipped, result.failed), (0, 1, 0));
    assert_eq!(originals(&dest), [existing]);
}

#[cfg(windows)]
#[test]
fn local_source_with_an_active_writer_is_retried_after_it_is_closed() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let source = t.dir("source");
    let photo = source.join("working.jpg");
    fs::write(&photo, b"original").unwrap();
    fs::write(dest.join("other name.jpg"), b"original").unwrap();
    let writer = fs::File::options().write(true).open(&photo).unwrap();
    let busy = local(&source, &dest);
    assert_eq!((busy.added, busy.skipped, busy.failed), (0, 0, 1));
    drop(writer);
    let finished = local(&source, &dest);
    assert_eq!(
        (finished.added, finished.skipped, finished.failed),
        (0, 1, 0)
    );
}

#[cfg(windows)]
#[test]
fn a_locked_unrelated_archived_file_does_not_block_import() {
    use std::os::windows::fs::OpenOptionsExt;
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let source = t.dir("source");
    let busy = dest.join("editing.jpg");
    fs::write(&busy, b"open elsewhere").unwrap();
    fs::write(source.join("photo.jpg"), b"new body").unwrap();
    let _lock = fs::File::options()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&busy)
        .unwrap();
    let report = local(&source, &dest);
    assert_eq!(
        (report.added, report.skipped, report.failed),
        (1, 0, 0),
        "{:?}",
        report.issues
    );
}

#[cfg(windows)]
#[test]
fn new_same_size_photos_do_not_repeat_the_archive_size_scan() {
    use std::os::windows::fs::OpenOptionsExt;
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let existing = dest.join("original.jpg");
    fs::write(&existing, b"AAA").unwrap();
    let mut archive = Archive::open(&dest).unwrap();
    archive.reconcile().unwrap();
    let phone = fixture(1, &[("B.jpg", b"BBB"), ("C.jpg", b"CCC")], None);
    assert!(
        archive
            .import(&phone, &phone.files[0].0, |_| Ok(()))
            .unwrap()
    );
    // The next miss has a different digest in the same size group. It should not
    // read any bytes from the unrelated original again.
    let _lock = fs::File::options()
        .read(true)
        .share_mode(0)
        .open(&existing)
        .unwrap();
    assert!(
        archive
            .import(&phone, &phone.files[1].0, |_| Ok(()))
            .unwrap()
    );
}

struct DeviceFixture {
    files: Vec<(Item, Vec<u8>)>,
    short: Option<usize>,
    #[cfg(windows)]
    session_error: Option<usize>,
    reads: RefCell<Vec<String>>,
}
impl Source for DeviceFixture {
    fn is_phone(&self) -> bool {
        true
    }

    fn identity(&self) -> &str {
        "fixture:serial-42"
    }
    fn confirms_returned_object(&self, item: &Item) -> Result<bool> {
        Ok(self.files.iter().any(|(current, _)| current.id == item.id
            && current.relative == item.relative && current.version == item.version
            && current.size == item.size && current.modified == item.modified))
    }
    fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> {
        for (item, _) in &self.files {
            emit(ScanEvent::Inspected { media: true })?;
            emit(ScanEvent::Ready(item.clone()))?;
        }
        Ok(())
    }
    fn reader(&self, item: &Item) -> Result<Box<dyn Read>> {
        let i: usize = item.id.split('-').next_back().unwrap().parse()?;
        self.reads.borrow_mut().push(item.id.clone());
        #[cfg(windows)]
        if self.session_error == Some(i) {
            struct Interrupted(bool);

            impl Read for Interrupted {
                fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                    if self.0 {
                        return Err(std::io::Error::other(windows::core::Error::from(
                            windows::Win32::Devices::PortableDevices::E_WPD_DEVICE_IS_HUNG,
                        )));
                    }
                    self.0 = true;
                    buffer[0] = 1;
                    Ok(1)
                }
            }

            return Ok(Box::new(Interrupted(false)));
        }
        let mut bytes = self.files[i].1.clone();
        if self.short == Some(i) {
            bytes.truncate(bytes.len() / 2);
        }
        Ok(Box::new(Cursor::new(bytes)))
    }
}
fn fixture(session: u32, entries: &[(&str, &[u8])], short: Option<usize>) -> DeviceFixture {
    let files = entries
        .iter()
        .enumerate()
        .map(|(i, (name, bytes))| {
            (
                Item {
                    id: format!("{session}-{i}"),
                    relative: PathBuf::from(name),
                    size: Some(bytes.len() as u64),
                    modified: Some(UNIX_EPOCH + Duration::from_secs(1700000000)),
                    version: Some(format!("persistent-{i}-v1")),
                },
                bytes.to_vec(),
            )
        })
        .collect();
    DeviceFixture {
        files,
        reads: RefCell::new(vec![]),
        short,
        #[cfg(windows)]
        session_error: None,
    }
}

fn capture_fixture(dest: &Path, phone: DeviceFixture, minimum: u64) -> super::Report {
    super::capture::receive(
        dest,
        "iPhone",
        |_| Ok(minimum),
        || Ok(Box::new(phone)),
        |_| {},
        || Ok(()),
        || Ok(()),
    )
}

#[cfg(windows)]
#[test]
fn an_unaffordable_group_of_changed_jpeg_exports_does_not_consume_space_needed_for_originals() {
    let temp = Sandbox::new();
    let root = temp.dir("archive");
    let mut first = fixture(1, &[("A.HEIC", b"raw"), ("B.HEIC", b"two"), ("C.MOV", b"source video"),
        ("A.JPG", b"prev"), ("exact.JPG", b"keep")], None);
    first.files[0].0.version = Some("camera/a/3/7900".into());
    first.files[1].0.version = Some("camera/b/3/7900".into());
    first.files[2].0.version = Some("camera/video/12/7900".into());
    // Windows can reuse the name and length of an earlier JPEG with a different object ID.
    first.files[3].0.version = Some("old-export/4/7900".into());
    first.files[4].0.version = Some("exact/4/7900".into());
    assert!(capture_fixture(&root, first, 5).safe_at.is_some());
    let declared: u64 = 1 << 55;
    let mut exports = fixture(2, &[("A.JPG", b"jpeg"), ("B.JPG", b"jpeg"), ("new.MOV", b"movie"),
        ("C.MOV", b"converted movie"), ("exact.JPG", b"keep")], None);
    exports.files[0].0.version = Some("camera/a/4/7900".into());
    exports.files[1].0.size = Some(declared);
    exports.files[1].0.version = Some(format!("camera/b/{declared}/7900"));
    exports.files[3].0.version = Some("camera/video/15/7900".into());
    exports.files[4].0.version = Some("exact/4/7900".into());
    let report = capture_fixture(&root, exports, 5);
    assert_eq!((report.added, report.failed), (1, 0), "{:?}", report.issues);
    assert!(report.safe_at.is_none());
    let archive = Archive::open(&root).unwrap();
    let jpgs:u64 = archive.db.query_row("SELECT COUNT(*) FROM phone_inbox WHERE lower(entry) LIKE '%.jpg'", [], |r| r.get(0)).unwrap();
    assert_eq!(jpgs, 2, "the old JPEG receipts stay intact without accepting another export");
    let pending:u64 = archive.db.query_row("SELECT COUNT(*) FROM phone_pending WHERE lower(entry) LIKE '%.jpg' AND attempted_at=0", [], |r| r.get(0)).unwrap();
    assert_eq!(pending, 2);
    assert_eq!(report.export_shortfall.unwrap().required, declared + 4 + 15);
    assert_eq!(report.list_covered,2);
}

#[test]
fn a_replaced_complete_heif_listing_can_schedule_changed_ids_but_cannot_supply_content_receipts() {
    use super::inventory::{self, Member, Receipts};
    let temp = Sandbox::new();
    let root = temp.dir("archive");
    let mut old = fixture(1, &[("A.HEIC", b"raw")], None);
    let timestamp = super::inbox::stored_time(old.files[0].0.modified.unwrap());
    old.files[0].0.version = Some(format!("old-device-id/3/{timestamp}"));
    let mut changed = old.files[0].0.clone();
    changed.version = Some(format!("new-device-id/3/{timestamp}"));
    let source = old.identity().to_owned();
    assert!(capture_fixture(&root, old, 1).safe_at.is_some());
    let archive = Archive::open(&root).unwrap();
    inventory::publish(&archive.db,1,&[Member::new(changed.clone())],&mut Default::default()).unwrap();
    let mut exported = changed.clone();
    exported.relative = "A.JPG".into();
    exported.size = Some(7);
    exported.version = Some(format!("new-device-id/7/{timestamp}"));
    inventory::publish(&archive.db,1,&[Member::new(exported.clone())],&mut Default::default()).unwrap();
    let receipts = Receipts::load(&archive.db, &source).unwrap();
    let mut unrelated = exported.clone();
    unrelated.version=Some(format!("independent/7/{timestamp}"));
    let mut members = [Member::new(exported.clone()), Member::new(changed), Member::new(unrelated)];
    receipts.match_members(&mut members);
    assert_eq!(members[0].priority, inventory::POSSIBLE_USB_EXPORT);
    assert_eq!(members[2].priority,0);
    assert!(members.iter().all(|m| m.recorded.is_none()));
}

#[cfg(windows)]
#[test]
fn a_short_jpeg_response_cannot_be_certified_by_a_different_native_representation() {
    let temp = Sandbox::new();
    let root = temp.dir("archive");
    let own_native = b"complete native one";
    let other_native = b"complete native two";
    let originals: &[(&str, &[u8])] = &[
        ("Camera/IMG_1.HEIC", own_native),
        ("Camera/IMG_2.HEIC", other_native),
    ];
    let prior = capture_fixture(&root, fixture(1, originals, None), 2);
    assert!(prior.safe_at.is_some());

    for (session, returned) in [(2, own_native), (3, other_native)] {
        let mut camera = fixture(session, &[("Camera/IMG_1.JPG", returned)], None);
        camera.files[0].0.size = Some(2 * own_native.len() as u64);
        let result = capture_fixture(&root, camera, 1);
        assert_eq!(result.phase, "手机返回内容与清单不符", "{:?}", result.issues);
        assert_eq!((result.added, result.failed), (0, 1));
        assert!(result.safe_at.is_none());
    }
    let archive = Archive::open(&root).unwrap();
    let matches: u64 = archive.db.query_row(
        "SELECT COUNT(*) FROM phone_inbox WHERE lower(entry) LIKE '%.jpg'",
        [], |r| r.get(0),
    ).unwrap();
    assert_eq!(matches, 0);
    let mut observations = archive.db.prepare(
        "SELECT reported_size,returned_size,digest,sessions,first_controlled,last_controlled
         FROM phone_returned_data ORDER BY first_session",
    ).unwrap();
    let observations: Vec<(u64, u64, String, u64, Option<u64>, Option<u64>)> = observations
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .unwrap().collect::<rusqlite::Result<_>>().unwrap();
    let expected: Vec<_> = [own_native, other_native].into_iter().map(|content| {
        (2 * own_native.len() as u64, content.len() as u64, blake3::hash(content).to_hex().to_string(), 1, None, None)
    }).collect();
    assert_eq!(observations, expected);
}

#[test]
fn phone_receipts_reuse_identical_content_across_paths_without_a_phone_checksum() {
    let temp = Sandbox::new();
    let dest = temp.dir("gallery");
    let local_source = temp.dir("local");
    fs::write(local_source.join("old.jpg"), b"abcd").unwrap();
    assert_eq!(local(&local_source, &dest).added, 1);

    let phone_items: &[(&str, &[u8])] = &[
        ("Camera/renamed.jpg", b"abcd"),
        ("Camera/new.jpg", b"wxyz"),
        ("Second folder/another name.jpg", b"wxyz"),
    ];
    let report = capture_fixture(&dest, fixture(1, phone_items, None), 3);
    assert!(report.safe_at.is_some(), "{:?}", report.issues);
    assert_eq!((report.added, report.skipped, report.received_bytes), (1, 2, 4));
    {
        let mut archive = Archive::open(&dest).unwrap();
        let stored: (u64, u64, u64) = archive.db.query_row(
            "SELECT COUNT(*),COUNT(blob),COUNT(DISTINCT blob) FROM phone_inbox",
            [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).unwrap();
        assert_eq!(stored, (3, 2, 1));
        let inbox_dir = dest.join(".time-cut/inbox");
        assert_eq!(fs::read_dir(&inbox_dir).unwrap().count(), 1);
        while super::inbox::file_next(&mut archive).unwrap().is_some() {}
        assert_eq!(fs::read_dir(inbox_dir).unwrap().count(), 0);
    }
    assert_eq!(originals(&dest).len(), 2);
    let repeat = capture_fixture(&dest, fixture(2, phone_items, None), 3);
    assert!(repeat.safe_at.is_some());
    assert_eq!((repeat.added, repeat.skipped, repeat.failed), (0, 3, 0));
}

#[test]
fn partial_scan_receipts_can_fill_only_unique_verified_entries_in_the_saved_inventory_offline() {
    use super::{inbox, inventory};
    use rusqlite::params;
    let temp = Sandbox::new();
    let root = temp.dir("incomplete album");
    let camera = fixture(1, &[("exact.jpg", b"one"), ("same.aae", b"two"), ("damaged.mov", b"old"),
        ("changed.jpg", b"four"), ("conflict.jpg", b"five"), ("unreliable.jpg", b"six")], None);
    let archive = Archive::open(&root).unwrap();
    inbox::recover(&archive).unwrap();
    archive.db.execute("INSERT INTO phone_runs(source,started) VALUES(?,1)", [camera.identity()]).unwrap();
    let mut receipts = Vec::new();
    let mut members = Vec::new();
    for (item, _) in &camera.files {
        let receipt = inbox::receive(&archive, 1, &camera, item, false, &mut inbox::RecoveryCheck::new(false),
            || Ok(()), |_, _| Ok(())).unwrap();
        receipts.push(receipt);
        members.push(inventory::Member::new(item.clone()));
    }
    let mut sibling = camera.files[1].0.clone();
    sibling.version = Some("separate object at the same size and time".into());
    members.push(inventory::Member::new(sibling));
    fs::write(&receipts[2].path, b"bad").unwrap();
    members[3].item.version = Some("new-no-receipt".into());
    archive.db.execute("INSERT INTO source_versions(source,entry,version,digest,path) VALUES(?,?,?,?,?)",
        params![camera.identity(), "conflict.jpg", camera.files[4].0.version, receipts[0].digest, "other.jpg"]).unwrap();
    archive.db.execute("INSERT INTO phone_returned_data(run_id,entry,version,reported_size,returned_size,digest,first_session,last_session,sessions)
        VALUES(1,?,?,3,3,?,1,1,1)",params!["unreliable.jpg",camera.files[5].0.version,receipts[0].digest]).unwrap();
    let mut report = super::Report::default();
    inventory::publish(&archive.db, 1, &members, &mut report).unwrap();
    let at = report.list_at.unwrap();
    let reads = camera.reads.borrow().len();
    assert!(inbox::saved_list::has_candidates(&archive.db,1).unwrap());
    assert_eq!(inbox::saved_list::reconcile(&archive,1,at-1).unwrap(),0);
    assert_eq!(inbox::saved_list::reconcile(&archive,1,at).unwrap(),1);
    assert_eq!(inbox::saved_list::reconcile(&archive,1,at).unwrap(),0);
    let only: (String,String) = archive.db.query_row("SELECT entry,digest FROM phone_list_items WHERE digest IS NOT NULL", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(only,("exact.jpg".into(),receipts[0].digest.clone()));
    assert_eq!(camera.reads.borrow().len(),reads);
    assert!(!archive.db.query_row("SELECT safe_at IS NOT NULL FROM phone_runs WHERE id=1",[],|r|r.get::<_,bool>(0)).unwrap());
}

struct ChecksumPhone {
    phone:DeviceFixture,
    stale_payload:Option<Vec<u8>>,
}
impl Source for ChecksumPhone {
    fn identity(&self)->&str { "afc:camera-42" }
    fn paired_camera_serial(&self)->Option<&str> { Some("HARDWARE-42") }
    fn is_phone(&self)->bool { true }
    fn visit(&self,emit:&mut dyn FnMut(ScanEvent)->Result<()>)->Result<()> {self.phone.visit(emit)}
    fn reader(&self,item:&Item)->Result<Box<dyn Read>> {
        if let Some(stale)=&self.stale_payload {
            self.phone.reads.borrow_mut().push(item.id.clone());
            return Ok(Box::new(Cursor::new(stale.clone())));
        }
        self.phone.reader(item)
    }
    fn content_sha1(&self,item:&Item)->Result<Option<[u8;20]>> {
        use sha1::{Digest,Sha1};
        let bytes=&self.phone.files.iter().find(|(i,_)|i.id==item.id).ok_or("unknown file")?.1;
        Ok(Some(Sha1::digest(bytes).into()))
    }
}

#[test]
fn iphone_filesystem_checksum_reuses_a_differently_named_wpd_receipt_without_usb_payload() {
    let temp=Sandbox::new();
    let root=temp.dir("archive");
    let old=capture_fixture(&root,fixture(1,&[("Internal Storage/202305__/IMG1.HEIC",b"camera original")],None),2);
    assert_eq!(old.added,1);
    let ready=super::inbox::fingerprints::prepare(&root, |_|{}).unwrap();
    assert_eq!((ready.verified,ready.unavailable),(1,0));
    let archive=Archive::open(&root).unwrap();
    let native=ChecksumPhone{phone:fixture(2,&[("DCIM/110APPLE/IMG1.HEIC",b"camera original")],None),stale_payload:None};
    let result=super::inbox::receive(&archive,1,&native,&native.phone.files[0].0,false,
        &mut super::inbox::RecoveryCheck::new(true),||Ok(()),|_,_|Ok(())).unwrap();
    assert_eq!((result.bytes,result.compared),(0,0));
    assert!(native.phone.reads.borrow().is_empty());
    assert_eq!(fs::read(&result.path).unwrap(),b"camera original");
    let (sources,blobs):(u64,u64)=archive.db.query_row("SELECT COUNT(DISTINCT source),COUNT(DISTINCT blob) FROM phone_inbox",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!((sources,blobs),(2,1));
}

#[cfg(windows)]
struct WindowsCamera(DeviceFixture);
#[cfg(windows)]
impl Source for WindowsCamera {
    fn identity(&self) -> &str { "wpd:HARDWARE-42" }
    fn is_phone(&self) -> bool { true }
    fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> { self.0.visit(emit) }
    fn reader(&self, item: &Item) -> Result<Box<dyn Read>> { self.0.reader(item) }
}

#[cfg(windows)]
#[test]
fn camera_only_capture_can_resume_file_and_repeat_when_the_windows_interface_arrives_midway() {
    let sandbox = Sandbox::new();
    let root = sandbox.dir("camera-first");
    let catalogue: &[(&str,&[u8])] = &[("DCIM/101APPLE/first.HEIC",b"original photo"),("DCIM/101APPLE/first.MOV",b"original movie")];
    let capture = |source: Box<dyn Source>| super::capture::receive(&root,"iPhone",|_|Ok(2),||Ok(source),|_|{},||Ok(()),||Ok(()));
    let phone = |session, n| Box::new(ChecksumPhone { phone:fixture(session,&catalogue[..n],None),stale_payload:None });
    let partial = capture(phone(1,1));
    assert_eq!(partial.added,1);
    assert!(partial.safe_at.is_none());
    let windows=capture(Box::new(WindowsCamera(fixture(2,catalogue,None))));
    assert!(windows.safe_at.is_none());
    assert_eq!(windows.phase,"等待 Apple 原件目录");
    let complete=capture(phone(3,2));
    assert!(complete.safe_at.is_some(),"{:?}",complete.issues);
    assert_eq!((complete.added,complete.skipped,complete.failed),(1,1,0));
    {
        let mut archive=Archive::open(&root).unwrap();
        let runs:u64=archive.db.query_row("SELECT COUNT(*) FROM phone_runs",[],|r|r.get(0)).unwrap();
        assert_eq!(runs,1);
        while super::inbox::file_next(&mut archive).unwrap().is_some() {}
    }
    assert_eq!(originals(&root).len(),2);
    let repeat=capture(phone(4,2));
    assert!(repeat.safe_at.is_some());
    assert_eq!((repeat.added,repeat.skipped,repeat.failed),(0,2,0));
    assert_eq!(repeat.inventory,complete.inventory);
}

#[cfg(windows)]
#[test]
fn paired_camera_can_supplement_an_incomplete_windows_run_without_releasing_it_early() {
    let sandbox = Sandbox::new();
    let root = sandbox.dir("archive");
    let windows = |items: &[(&str, &[u8])]| super::capture::receive(
        &root, "Test phone", |_| Ok(2), || Ok(Box::new(WindowsCamera(fixture(1,items,None)))),
        |_| {}, || Ok(()), || Ok(()),
    );
    let old = ("Camera/first.jpg", b"already saved".as_slice());
    let partial = windows(&[old]);
    assert!(partial.safe_at.is_none());
    assert_eq!(partial.added,1);
    let key=partial.device_key.unwrap();
    let data: &[(&str, &[u8])] = &[("DCIM/100APPLE/A.HEIC",old.1),("DCIM/100APPLE/B.MOV",b"good video")];
    let phone = |bad: bool| ChecksumPhone {
        phone:fixture(2,data,None),stale_payload:bad.then(|| b"wrong data".to_vec()),
    };
    let attempt = |source: &ChecksumPhone, serial: &str, receiver: &str| {
        super::afc::supplement::collect(&root,receiver,serial,source,false, |_|{},||Ok(()),||Ok(()))
    };
    let camera=phone(false);
    assert!(attempt(&camera,"different hardware",&key).unwrap().is_none());
    assert!(attempt(&camera,"hardware-42","unrelated album device").unwrap().is_none());
    assert!(camera.phone.reads.borrow().is_empty());
    // The cable can disappear between knowing that an Apple directory exists and
    // learning its first filename. A complete Windows pass cannot close that gap.
    let active_calls = std::cell::Cell::new(0);
    let early = super::afc::supplement::collect(&root,&key,"hardware-42",&camera,false,|_|{},||Ok(()),|| {
        active_calls.set(active_calls.get()+1);
        if active_calls.get()==2 { return Err("camera detached during listing".into()); }
        Ok(())
    });
    assert!(early.is_err());
    let next = ("Camera/added.jpg",b"another picture".as_slice());
    let interrupted_listing=windows(&[old,next]);
    assert!(interrupted_listing.safe_at.is_none(),"the Apple directory was not fully listed");
    assert!(attempt(&phone(true),"hardware-42",&key).is_err());
    let exported=windows(&[old,next]);
    assert!(exported.safe_at.is_none());
    assert_eq!(exported.phase,"Apple 相机原件待补");
    let stats=serde_json::to_value(attempt(&camera,"hardware-42",&key).unwrap().unwrap()).unwrap();
    assert_eq!(stats["listed"],2);
    assert_eq!(stats["copied"],1);
    assert_eq!(stats["copied_bytes"],10);
    assert_eq!(stats["without_local_proof"],0);
    assert_eq!(stats["stable_inventory"],true);
    assert_eq!(camera.phone.reads.borrow().len(),1);
    assert!(attempt(&camera,"hardware-42",&key).unwrap().is_none());
    let verify = || super::afc::supplement::collect(&root,&key,"hardware-42",&camera,true,|_|{},||Ok(()),||Ok(()));
    let repeated = serde_json::to_value(verify().unwrap().unwrap()).unwrap();
    assert_eq!(repeated["repeat_verified"],true);
    assert_eq!(repeated["copied"],0);
    assert_eq!(repeated["matched"],2);
    assert_eq!(camera.phone.reads.borrow().len(),1,"a repeat with matching checksums must not download the originals");
    assert!(verify().unwrap().is_none());
    {
        let archive=Archive::open(&root).unwrap();
        let (safe,released,native):(u64,u64,u64)=archive.db.query_row(
            "SELECT (SELECT COUNT(safe_at) FROM phone_runs),(SELECT COUNT(*) FROM phone_inbox WHERE released=1),
             (SELECT COUNT(*) FROM phone_inbox WHERE source='afc:camera-42' AND run_id IS NOT NULL)",
            [],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).unwrap();
        assert_eq!((safe,released,native),(0,0,2));
        let blob:String=archive.db.query_row("SELECT blob FROM phone_inbox WHERE source='afc:camera-42' AND entry LIKE '%B.MOV'",[],|r|r.get(0)).unwrap();
        fs::write(root.join(".time-cut/inbox").join(blob),b"wrong data").unwrap();
    }
    let damaged=windows(&[old,next]);
    assert!(damaged.safe_at.is_none());
    assert_eq!(damaged.phase,"Apple 相机原件待补");
    assert!(attempt(&camera,"hardware-42",&key).unwrap().is_some());
    let complete=windows(&[old,next]);
    assert!(complete.safe_at.is_some(),"{:?}",complete.issues);
    {
        let mut archive=Archive::open(&root).unwrap();
        assert_eq!(super::inbox::pending_local(&archive.db).unwrap(),4);
        while super::inbox::file_next(&mut archive).unwrap().is_some() {}
    }
    assert_eq!(originals(&root).len(),3);
}

#[test]
fn repeatedly_failing_payloads_cannot_starve_directory_inventory_on_all_retries() {
    use std::collections::{HashMap,HashSet};
    let db=rusqlite::Connection::open_in_memory().unwrap();
    super::inventory::initialize(&db).unwrap();
    db.execute_batch("INSERT INTO phone_list_folders(run_id,path,attempted,probing,checked_at)
        VALUES(1,'missing-bodies',10,0,12),(1,'old-fault',3,1,NULL),
        (1,'recent-fault',15,1,NULL),(1,'never-visited',0,0,NULL)").unwrap();
    let plan=|attempt| {
        let mut ranks=HashMap::new();
        let mut priorities=HashMap::from([(PathBuf::from("missing-bodies"),0u8)]);
        let p=super::inventory::unfinished_folders(&db,1,attempt,&mut HashSet::new(),&mut ranks,&mut priorities).unwrap();
        let mut names=vec!["missing-bodies","recent-fault","never-visited","old-fault"];
        names.sort_by_key(|p| (priorities.get(Path::new(p)).copied().unwrap_or(1),ranks.get(Path::new(p)).copied().unwrap_or(i64::MIN)));
        (p.list_first,names)
    };
    let (list_first,names)=plan(101);
    assert!(list_first);
    assert_eq!(names,["never-visited","old-fault","recent-fault","missing-bodies"]);
    let (list_first,names)=plan(102);
    assert!(!list_first);
    assert_eq!(names[0],"missing-bodies","normal retries still deliver discovered originals promptly");
}

#[test]
fn iphone_rejects_wrong_equal_length_contents_against_the_source_checksum_before_receipting() {
    let temp=Sandbox::new();
    let root=temp.dir("archive");
    let archive=Archive::open(&root).unwrap();
    super::inbox::recover(&archive).unwrap();
    archive.db.execute("INSERT INTO phone_runs(source,started) VALUES('afc:camera-42',0)",[]).unwrap();
    let mut native=ChecksumPhone{phone:fixture(2,&[("DCIM/A.JPG",b"real")],None),stale_payload:Some(b"prev".to_vec())};
    let receive=|n:&ChecksumPhone| super::inbox::receive(&archive,1,n,&n.phone.files[0].0,true,
        &mut super::inbox::RecoveryCheck::new(false),||Ok(()),|_,_|Ok(()));
    assert!(receive(&native).is_err());
    assert_eq!(fs::read_dir(root.join(".time-cut/inbox")).unwrap().count(),0);
    assert_eq!(archive.db.query_row("SELECT COUNT(*) FROM phone_inbox",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    native.stale_payload=None;
    assert_eq!(receive(&native).unwrap().bytes,4);
    native.phone.files[0].1=b"edit".to_vec();
    assert_eq!(receive(&native).unwrap().bytes,4,"the same path, length, timestamp and version can change content");
    assert_eq!(receive(&native).unwrap().bytes,0);
    assert_eq!(native.phone.reads.borrow().len(),3);
}

#[cfg(windows)]
#[test]
fn independent_iphone_audit_rechecks_disk_and_keeps_the_wpd_run_open() {
    let temp=Sandbox::new();
    let root=temp.dir("archive");
    let old=capture_fixture(&root,fixture(1,&[("Windows/A.JPG",b"abcd")],None),2);
    assert!(old.safe_at.is_none());
    let native=ChecksumPhone{phone:fixture(2,&[("DCIM/100APPLE/X.JPG",b"abcd"),("DCIM/100APPLE/X.MOV",b"absent")],None),stale_payload:None};
    let report=super::afc::audit(&root,&native,|_|{}).unwrap();
    assert_eq!(report.without_local_proof,1);
    assert!(native.phone.reads.borrow().is_empty());
    let blob=fs::read_dir(root.join(".time-cut/inbox")).unwrap().next().unwrap().unwrap().path();
    let time=fs::metadata(&blob).unwrap().modified().unwrap();
    fs::write(&blob,b"edit").unwrap();
    fs::File::options().write(true).open(&blob).unwrap().set_modified(time).unwrap();
    let changed=super::afc::audit(&root,&native,|_|{}).unwrap();
    assert_eq!(changed.without_local_proof,2);
    let archive=Archive::open(&root).unwrap();
    assert_eq!(archive.db.query_row("SELECT COUNT(*) FROM phone_runs WHERE safe_at IS NOT NULL",[],|r|r.get::<_,u64>(0)).unwrap(),0);
    assert!(super::afc::audit(&root,&ChecksumPhone{phone:fixture(3,&[],None),stale_payload:None},|_|{}).is_err());
    let evidence=rusqlite::Connection::open(root.join(".time-cut/iphone-originals-audit.sqlite3")).unwrap();
    assert_eq!(evidence.query_row("SELECT listed,missing FROM completed",[],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,u64>(1)?))).unwrap(),(2,2));
}

#[test]
fn complete_phone_list_is_saved_before_an_interrupted_payload_and_survives_an_empty_retry() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let entries: [(&str, &[u8]); 4] = [
        ("A.JPG", b"first"), ("B.MOV", b"cannot finish"),
        ("same.AAE", b"left"), ("same.AAE", b"both"),
    ];
    let first = capture_fixture(&dest, fixture(1, &entries, Some(1)), 4);
    assert!(first.safe_at.is_none());
    let count = || {
        let archive = Archive::open(&dest).unwrap();
        archive.db.query_row("SELECT COUNT(*),COUNT(digest) FROM phone_list_items WHERE run_id=1", [],
            |r| Ok((r.get::<_,u64>(0)?,r.get::<_,u64>(1)?))).expect("the enumerated phone list must already be durable")
    };
    let (listed, covered) = count();
    assert_eq!(listed,4,"a failed video must not prevent listing the two later attachments");
    assert!((1..4).contains(&covered));
    assert!(capture_fixture(&dest,fixture(2,&[],None),4).safe_at.is_none());
    assert_eq!(count(),(listed,covered),"an empty Windows session must preserve the last trustworthy list");
    let done=capture_fixture(&dest,fixture(3,&entries,None),4);
    assert!(done.safe_at.is_some(),"{:?}",done.issues);
    assert_eq!(count(),(4,4));
    assert_eq!(done.added + done.skipped, 4);
}

#[test]
fn a_transient_empty_directory_keeps_its_last_saved_inventory() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    super::inventory::initialize(&db).unwrap();
    let dir = Path::new("DCIM");
    let photo = Item {
        id: "1".into(), relative: dir.join("A.JPG"), size: Some(5),
        modified: None, version: Some("original".into()),
    };
    assert!(super::inventory::save_folder(&db, 1, dir, std::slice::from_ref(&photo)).unwrap());
    assert!(!super::inventory::save_folder(&db, 1, dir, &[]).unwrap());
    let saved: (u64,bool,u64) = db.query_row(
        "SELECT item_count,probing,(SELECT COUNT(*) FROM phone_folder_items WHERE run_id=1) FROM phone_list_folders WHERE run_id=1",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(saved,(1,true,1));
    assert!(super::inventory::save_folder(&db, 1, dir, &[photo]).unwrap());
}

#[test]
fn final_metadata_check_rejects_an_edit_with_unchanged_count_handle_and_size() {
    struct Edited { phone: DeviceFixture, visits: std::cell::Cell<u32> }
    impl Source for Edited {
        fn identity(&self) -> &str { self.phone.identity() }
        fn visit(&self, emit: &mut dyn FnMut(ScanEvent)->Result<()>) -> Result<()> {
            let pass=self.visits.get();
            self.visits.set(pass+1);
            self.phone.visit(&mut |mut event| {
                if pass>0 && let ScanEvent::Ready(item)=&mut event {
                    item.version=Some("edited-in-place".into());
                }
                emit(event)
            })
        }
        fn reader(&self, item:&Item) -> Result<Box<dyn Read>> { self.phone.reader(item) }
    }
    let t=Sandbox::new();
    let dest=t.dir("archive");
    let source=Edited { phone:fixture(1,&[("A.JPG", b"photo")],None), visits: std::cell::Cell::new(0) };
    let report=super::capture::receive(&dest,"iPhone",|_|Ok(1),||Ok(Box::new(source)),|_|{},||Ok(()),||Ok(()));
    assert_eq!((report.list_total,report.list_covered),(Some(1),1));
    assert!(report.safe_at.is_none());
    let archive=Archive::open(&dest).unwrap();
    assert_eq!(super::inbox::pending_local(&archive.db).unwrap(),0);
}

#[test]
fn phone_receipts_survive_interruption_then_file_offline_and_repeat_without_reading() {
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let entries: [(&str, &[u8]); 4] = [
        ("DCIM/A.MOV", b"paired movie"),
        ("DCIM/A.AAE", b"settings"),
        ("DCIM/A.JPG", b"original photo"),
        ("DCIM/B.JPG", b"original photo"),
    ];
    let interrupted = capture_fixture(&dest, fixture(1, &entries, Some(2)), 4);
    assert!(interrupted.safe_at.is_none());
    let archive = Archive::open(&dest).unwrap();
    assert_eq!(super::inbox::pending_local(&archive.db).unwrap(), 0);
    assert_eq!(
        archive
            .db
            .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        0
    );
    drop(archive);
    let release = capture_fixture(&dest, fixture(2, &entries, None), 4);
    assert!(release.safe_at.is_some());
    assert_eq!(release.capture_started_at, interrupted.capture_started_at);
    assert_eq!(release.failed, 0);
    assert_eq!(release.added + release.skipped, 4);
    assert!(release.skipped >= 2);
    let mut archive = Archive::open(&dest).unwrap();
    assert_eq!(super::inbox::pending_local(&archive.db).unwrap(), 4);
    assert!(originals(&dest).is_empty());
    archive.reconcile().unwrap();
    let mut added = 0;
    while let Some(new) = super::inbox::file_next(&mut archive).unwrap() {
        added += u32::from(new);
    }
    assert_eq!(added, 3);
    let paths = originals(&dest);
    assert_eq!(paths.len(), 3);
    let jpg = paths
        .iter()
        .find(|p| p.extension().unwrap().eq_ignore_ascii_case("jpg"))
        .unwrap();
    assert!(jpg.with_extension("MOV").is_file());
    assert!(jpg.with_extension("AAE").is_file());
    drop(archive);
    // A read of the first exact-version original would now fail the size check.
    let repeat = capture_fixture(&dest, fixture(3, &entries, Some(0)), 4);
    assert!(repeat.safe_at.is_some());
    assert_eq!(
        (
            repeat.added,
            repeat.skipped,
            repeat.received_bytes,
            repeat.compared_bytes
        ),
        (0, 4, 0, 0)
    );
    assert_eq!(repeat.inventory, release.inventory);
}

#[test]
fn a_resumed_phone_returning_the_previous_same_length_photo_cannot_commit_it() {
    struct Shifted {
        phone: DeviceFixture,
        first_wrong: std::cell::Cell<bool>,
    }
    impl Source for Shifted {
        fn identity(&self) -> &str { self.phone.identity() }
        fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> {
            self.phone.visit(emit)
        }
        fn reader(&self, item: &Item) -> Result<Box<dyn Read>> {
            if item.relative.ends_with("B.JPG") && self.first_wrong.replace(false) {
                return Ok(Box::new(Cursor::new(b"old!".to_vec())));
            }
            self.phone.reader(item)
        }
    }
    let temp = Sandbox::new();
    let root = temp.dir("archive");
    let files: [(&str, &[u8]); 2] = [("A.JPG", b"old!"), ("B.JPG", b"new!")];
    let first = capture_fixture(&root, fixture(1, &files, Some(1)), 2);
    assert!(first.safe_at.is_none());
    let retry = super::capture::receive(&root, "iPhone", |_| Ok(2),
        || Ok(Box::new(Shifted { phone: fixture(2, &files, None), first_wrong: std::cell::Cell::new(true) })),
        |_| {}, || Ok(()), || Ok(()));
    assert!(retry.safe_at.is_none(), "a stale same-length payload was accepted");
    assert!(retry.issues.iter().any(|s| s.contains("不一致")), "{:?}",retry.issues);
    {
        let archive = Archive::open(&root).unwrap();
        let saved: u64 = archive.db.query_row("SELECT COUNT(*) FROM phone_inbox WHERE entry='B.JPG'", [], |r| r.get(0)).unwrap();
        assert_eq!(saved, 0, "a rejected response must not create a receipt");
        assert_eq!(fs::read_dir(root.join(".time-cut/inbox")).unwrap().count(), 1);
    }
    let finished = capture_fixture(&root, fixture(3, &files, None), 2);
    assert!(finished.safe_at.is_some(), "{:?}",finished.issues);
    let archive = Archive::open(&root).unwrap();
    let digest: String = archive.db.query_row("SELECT digest FROM phone_manifest WHERE entry='B.JPG'", [], |r| r.get(0)).unwrap();
    assert_eq!(digest, blake3::hash(b"new!").to_hex().to_string());
}

#[test]
fn two_different_known_contents_restore_single_read_capture_after_a_fault() {
    let temp=Sandbox::new();
    let root=temp.dir("archive");
    let files: [(&str, &[u8]);3]=[("A.JPG",b"old A"),("B.JPG",b"old B"),("C.JPG",b"new C")];
    capture_fixture(&root,fixture(1,&files[..2],None),3);
    let archive=Archive::open(&root).unwrap();
    let phone=fixture(2,&files,None);
    let mut check=super::inbox::RecoveryCheck::new(true);
    for &(index,expected_check_bytes) in &[(0,5),(0,0),(1,5),(2,0)] {
        let result=super::inbox::receive(&archive,1,&phone,&phone.files[index].0,true,
            &mut check,||Ok(()),|_,_|Ok(())).unwrap();
        assert_eq!(result.compared,expected_check_bytes);
    }
    assert_eq!(&*phone.reads.borrow(), &["2-0", "2-1", "2-2"]);
}

#[test]
fn changed_original_with_a_long_identical_prefix_uses_one_phone_stream() {
    let temp = Sandbox::new();
    let root = temp.dir("archive");
    let previous = vec![7u8; 768 * 1024 + 91];
    let mut changed = previous.clone();
    changed[736 * 1024] = 19;
    let path = "DCIM/clip.MOV";
    assert!(capture_fixture(&root, fixture(1, &[(path, &previous)], None), 2).safe_at.is_none());
    let archive = Archive::open(&root).unwrap();
    let mut phone = fixture(2, &[(path, &changed)], None);
    phone.files[0].0.version = Some("remapped-camera-version".into());
    let saved = super::inbox::receive(&archive, 1, &phone, &phone.files[0].0, false,
        &mut super::inbox::RecoveryCheck::new(false), || Ok(()), |_,_| Ok(())).unwrap();
    assert_eq!(phone.reads.borrow().len(), 1, "a same-length edit must not be requested twice over USB");
    assert_eq!(fs::read(&saved.path).unwrap(), changed);
    assert_eq!(saved.digest, blake3::hash(&changed).to_hex().to_string());
    let original: String = archive.db.query_row("SELECT blob FROM phone_inbox WHERE digest=?", [blake3::hash(&previous).to_hex().to_string()], |r|r.get(0)).unwrap();
    assert_eq!(fs::read(root.join(".time-cut/inbox").join(original)).unwrap(), previous);
    assert_eq!(saved.compared, 0, "newly received bytes are counted once");
}

#[test]
fn unreported_length_can_be_a_shorter_prefix_of_an_old_original() {
    let temp = Sandbox::new();
    let root = temp.dir("archive");
    let previous = vec![17u8; 600 * 1024];
    let shorter = &previous[..511 * 1024];
    let path = "DCIM/clip.MOV";
    capture_fixture(&root, fixture(1, &[(path, &previous)], None), 2);
    let archive = Archive::open(&root).unwrap();
    let mut phone = fixture(2, &[(path, shorter)], None);
    phone.files[0].0.size=None;
    phone.files[0].0.version=None;
    let saved=super::inbox::receive(&archive,1,&phone,&phone.files[0].0,false,
        &mut super::inbox::RecoveryCheck::new(false),||Ok(()),|_,_|Ok(())).unwrap();
    assert_eq!(phone.reads.borrow().len(),1);
    assert_eq!(fs::read(saved.path).unwrap(),shorter);
}

#[test]
fn old_gallery_remapped_versions_compare_without_copy_and_changed_equal_size_is_preserved() {
    let temp = Sandbox::new();
    let dest = temp.dir("old");
    let entries: [(&str, &[u8]); 2] = [("DCIM/test.JPG", b"image"), ("DCIM/test.AAE", b"left")];
    let legacy = transfer(
        &dest,
        "old",
        || Ok(Box::new(fixture(1, &entries, None))),
        |_| {},
    );
    assert_eq!(legacy.added, 2);
    let unchanged = capture_fixture(&dest, fixture(2, &entries, Some(0)), 2);
    assert_eq!(
        (unchanged.added, unchanged.skipped, unchanged.compared_bytes),
        (0, 2, 0)
    );
    let changed: [(&str, &[u8]); 3] = [entries[0], entries[1], ("DCIM/test.AAE", b"next")];
    let mut remapped = fixture(3, &changed, None);
    for (index, (item, _)) in remapped.files.iter_mut().enumerate() {
        item.version = Some(format!("reassigned-{index}"));
    }
    let r = capture_fixture(&dest, remapped, 2);
    assert!(r.safe_at.is_some());
    assert_eq!(
        (r.added, r.skipped, r.received_bytes, r.compared_bytes),
        (1, 2, 4, 9)
    );
    let mut archive = Archive::open(&dest).unwrap();
    archive.reconcile().unwrap();
    while super::inbox::file_next(&mut archive).unwrap().is_some() {}
    assert_eq!(originals(&dest).len(), 3);
}

#[test]
fn partial_inventory_and_missing_receipt_never_release_the_cable_early() {
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let entries: [(&str, &[u8]); 2] = [("A.JPG", b"one"), ("B.MOV", b"unfinished")];
    let first = capture_fixture(&dest, fixture(1, &entries[..1], None), 2);
    assert!(first.safe_at.is_none());
    assert_eq!(first.phase, "等待完整媒体清单");
    let spool = std::fs::read_dir(dest.join(".time-cut/inbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(spool).unwrap();
    let again = capture_fixture(&dest, fixture(2, &entries, Some(0)), 2);
    assert!(again.safe_at.is_none());
    let last = capture_fixture(&dest, fixture(3, &entries, None), 2);
    assert!(last.safe_at.is_some());
    assert!(last.received_bytes >= 3);
}

#[test]
fn same_size_edit_with_restored_date_invalidates_a_local_receipt() {
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let entries: [(&str, &[u8]); 1] = [("DCIM/photo.jpg", b"original")];
    assert!(
        capture_fixture(&dest, fixture(1, &entries, None), 2)
            .safe_at
            .is_none()
    );
    let checked = capture_fixture(&dest, fixture(2, &entries, Some(0)), 2);
    assert_eq!((checked.added, checked.skipped), (0, 1));
    let spool = fs::read_dir(dest.join(".time-cut/inbox"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let old_time = fs::metadata(&spool).unwrap().modified().unwrap();
    fs::write(&spool, b"modified").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&spool)
        .unwrap()
        .set_modified(old_time)
        .unwrap();
    let repaired = capture_fixture(&dest, fixture(3, &entries, None), 1);
    assert!(repaired.safe_at.is_some(), "{:?}", repaired.issues);
    assert_eq!((repaired.added, repaired.received_bytes), (1, 8));
    let mut archive = Archive::open(&dest).unwrap();
    super::inbox::recover(&archive).unwrap();
    while super::inbox::file_next(&mut archive).unwrap().is_some() {}
    assert_eq!(fs::read(&originals(&dest)[0]).unwrap(), b"original");
}

#[test]
fn repeated_empty_sessions_retain_progress_and_the_full_inventory_floor() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    super::initialize_control(&db).unwrap();
    let dest = super::Target { library: "gallery".into(), album: 7, ..Default::default() };
    db.execute("INSERT INTO phone_verification(library,album,device,minimum) VALUES('gallery',7,'phone',4)", []).unwrap();
    let mut report = super::Report { source: "iPhone".into(), phone: true, capture: true,
        device_key: Some("phone".into()), capture_started_at: Some(10), discovered: 2,
        added: 2, expected_minimum: Some(9), ..Default::default() };
    super::record_history(&db, &report).unwrap();
    assert_eq!(super::verification::capture_minimum(&db, &dest, "phone").unwrap(), 9);
    report.discovered = 0;
    report.added = 0;
    report.phase = "手机媒体暂不可读".into();
    for attempt in 2..=52 {
        report.receive_attempt = attempt;
        super::record_history(&db, &report).unwrap();
    }
    let (count, latest): (u32,u32) = db.query_row("SELECT COUNT(*),MAX(json_extract(data,'$.receive_attempt')) FROM history", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!((count, latest), (2, 52));
    report.phase = "手机会话中断".into();
    super::record_history(&db, &report).unwrap();
    assert_eq!(db.query_row("SELECT COUNT(*) FROM history", [], |r| r.get::<_,u32>(0)).unwrap(),3);
    db.execute("DELETE FROM history", []).unwrap();
    assert_eq!(super::verification::capture_minimum(&db, &dest, "phone").unwrap(),9);
    assert_eq!(super::verification::capture_minimum(&db, &dest, "other").unwrap(),0);
}

#[test]
fn explicitly_requested_repeat_keeps_first_unplug_notice_and_stops_after_matching_scan() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    super::initialize_control(&db).unwrap();
    let dest = super::Target {
        library: "test".into(),
        album: 1,
        ..Default::default()
    };
    db.execute(
        "INSERT INTO phone_verification(library,album,device,minimum) VALUES('test',1,'device',2)",
        [],
    )
    .unwrap();
    let report = super::Report {
        device_key: Some("device".into()),
        discovered: 2,
        inventory: Some("sum".into()),
        phase: "原件已收齐，可以拔线".into(),
        safe_at: Some(100),
        ..Default::default()
    };
    let mut normal = report.clone();
    super::verification::observe_requested_repeat(&db, &dest, &mut normal).unwrap();
    assert!(!normal.repeat_due);
    db.execute("INSERT INTO phone_repeat VALUES('test',1,'device')", [])
        .unwrap();
    let mut first = report.clone();
    super::verification::observe_requested_repeat(&db, &dest, &mut first).unwrap();
    assert!(first.repeat_due);
    assert_eq!(first.phase, report.phase);
    let mut second = report;
    super::verification::observe_requested_repeat(&db, &dest, &mut second).unwrap();
    assert!(second.verified);
    assert!(!second.repeat_due);
    assert_eq!(
        db.query_row("SELECT count(*) FROM phone_repeat", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn automatic_local_index_retries_each_interrupted_file_once_per_unfinished_job() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    super::initialize_control(&db).unwrap();
    let retry =
        |album, path| super::retry_interrupted_index_file(&db, "photos", album, path).unwrap();
    assert!(!retry(1, "a.jpg"));
    super::request_index(&db, "photos", 1).unwrap();
    let revision = db
        .query_row("SELECT revision FROM indexing", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert!(retry(1, "a.jpg"));
    assert!(!retry(1, "a.jpg"));
    assert!(!retry(2, "a.jpg"));
    assert!(retry(1, "b.mov"));
    super::acknowledge_index(&db, "photos", 1, revision - 1).unwrap();
    assert!(!retry(1, "a.jpg"));
    super::acknowledge_index(&db, "photos", 1, revision).unwrap();
    assert!(!retry(1, "a.jpg"));
    super::request_index(&db, "photos", 1).unwrap();
    assert!(retry(1, "a.jpg"));
}

#[cfg(windows)]
#[test]
fn mismatched_phone_payload_closes_the_session_before_the_next_same_length_file() {
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let entries: [(&str, &[u8]); 3] = [
        ("first.jpg", b"first genuine picture"),
        ("second.jpg", b"abcd"),
        ("third.jpg", b"wxyz"),
    ];
    let mut shifted = fixture(1, &entries, None);
    shifted.files[1].1 = b"xx".to_vec();
    shifted.files[2].1 = entries[1].1.to_vec();
    let corrupt = transfer(&dest, "WPD", || Ok(Box::new(shifted)), |_| {});
    assert_eq!(
        (
            corrupt.phase.as_str(),
            corrupt.added,
            corrupt.scanned,
            corrupt.failed
        ),
        ("手机返回内容与清单不符", 1, 2, 1)
    );
    assert_eq!(originals(&dest).len(), 1);
    assert!(corrupt.inventory.is_none());
    let recovered = transfer(
        &dest,
        "WPD",
        || Ok(Box::new(fixture(2, &entries, None))),
        |_| {},
    );
    assert_eq!(
        (
            recovered.added,
            recovered.skipped,
            recovered.failed,
            recovered.pending
        ),
        (2, 1, 0, 0)
    );
    for path in originals(&dest) {
        let row = entries
            .iter()
            .find(|(name, _)| path.file_name().unwrap() == *name)
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), row.1);
    }
}

#[cfg(windows)]
#[test]
fn hung_wpd_session_stops_requests_and_keeps_completed_originals() {
    let t = Sandbox::new();
    let dest = t.dir("album");
    let entries: [(&str, &[u8]); 3] = [
        ("first.jpg", b"first complete image"),
        ("second.jpg", b"interrupted image"),
        ("third.jpg", b"third unread image"),
    ];
    let mut interrupted = fixture(1, &entries, None);
    interrupted.session_error = Some(1);
    let report = transfer(&dest, "WPD", || Ok(Box::new(interrupted)), |_| {});
    assert_eq!(
        (&*report.phase, report.added, report.scanned),
        ("手机会话中断", 1, 2)
    );
    assert_eq!(originals(&dest).len(), 1);
    assert_eq!(
        fs::read_dir(dest.join(".time-cut/staging"))
            .unwrap()
            .count(),
        0
    );

    let report = transfer(
        &dest,
        "WPD",
        || Ok(Box::new(fixture(2, &entries, None))),
        |_| {},
    );
    assert_eq!((report.added, report.skipped, report.failed), (2, 1, 0));
    for (_, bytes) in entries {
        assert!(
            originals(&dest)
                .iter()
                .any(|p| fs::read(p).unwrap() == bytes)
        );
    }
}

#[cfg(windows)]
#[test]
fn unattended_retry_after_restart_imports_new_files_before_revalidating_old_versions() {
    let sandbox = Sandbox::new();
    let dest = sandbox.dir("archive");
    let run = |session, entries: &[(&str, &[u8])], failed| {
        let mut phone = fixture(session, entries, None);
        for (item, _) in &mut phone.files {
            item.version = Some(format!("session-{session}-{}", item.id));
        }
        phone.session_error = failed;
        transfer(&dest, "WPD", || Ok(Box::new(phone)), |_| {})
    };
    let archived = [("DCIM/old.HEIC", b"old image".as_slice())];
    assert_eq!(run(1, &archived, None).added, 1);
    let entries = [archived[0], ("DCIM/new.jpg", b"new image")];
    let interrupted = run(2, &entries, Some(0));
    assert_eq!((interrupted.added, interrupted.failed), (1, 1));
    let new_only = [("DCIM/new.jpg", b"new image".as_slice())];
    let partial = run(3, &new_only, None);
    assert_ne!(
        partial.phase, "已完成",
        "the outstanding phone entry was absent"
    );
    let edited = [("DCIM/old.HEIC", b"new bytes".as_slice()), entries[1]];
    let recovered = run(4, &edited, None);
    assert_eq!(
        (recovered.added, recovered.skipped, recovered.failed),
        (1, 1, 0)
    );
    assert_eq!(recovered.phase, "已完成");
    let repeated = run(5, &edited, None);
    assert_eq!(
        (repeated.added, repeated.skipped, repeated.failed),
        (0, 2, 0)
    );
    assert_eq!(originals(&dest).len(), 3);
}

#[cfg(windows)]
#[test]
fn unattended_retry_remembers_a_failed_new_file_across_fresh_importers() {
    let sandbox = Sandbox::new();
    let dest = sandbox.dir("archive");
    let entries: [(&str, &[u8]); 3] = [
        ("DCIM/first.HEIC", b"source image"),
        ("DCIM/first.AAE", b"edit"),
        ("DCIM/later.jpg", b"later image"),
    ];
    let run = |session, failure| {
        let mut phone = fixture(session, &entries, None);
        phone.session_error = failure;
        transfer(&dest, "WPD", || Ok(Box::new(phone)), |_| {})
    };
    assert_eq!(run(1, Some(0)).added, 0);
    let second = run(2, Some(0));
    assert_eq!((second.added, second.failed), (1, 1));
    let finished = run(3, None);
    assert_eq!(
        (finished.added, finished.skipped, finished.failed),
        (2, 1, 0)
    );
    let photo = originals(&dest)
        .into_iter()
        .find(|p| p.extension().unwrap() == "HEIC")
        .unwrap();
    assert_eq!(fs::read(photo.with_extension("AAE")).unwrap(), b"edit");
}

#[cfg(windows)]
#[test]
fn unattended_process_exit_inside_a_phone_read_keeps_the_durable_queue() {
    const CHILD: &str = "TIMECUT_TEST_EXIT_DURING_PHONE_READ";
    let entries: [(&str, &[u8]); 2] = [("blocked.jpg", b"blocked"), ("later.jpg", b"later")];
    if let Some(dest) = std::env::var_os(CHILD) {
        struct Crash(DeviceFixture);
        impl Source for Crash {
            fn identity(&self) -> &str {
                self.0.identity()
            }
            fn is_phone(&self) -> bool {
                true
            }
            fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> {
                self.0.visit(emit)
            }
            fn reader(&self, _: &Item) -> Result<Box<dyn Read>> {
                std::process::exit(73)
            }
        }
        transfer(
            Path::new(&dest),
            "WPD",
            || Ok(Box::new(Crash(fixture(1, &entries, None)))),
            |_| {},
        );
        panic!("the test process did not reach the phone read");
    }
    let sandbox = Sandbox::new();
    let dest = sandbox.dir("archive");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ingest::tests::unattended_process_exit_inside_a_phone_read_keeps_the_durable_queue",
        ])
        .env(CHILD, &dest)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    let mut still_blocked = fixture(2, &entries, None);
    still_blocked.session_error = Some(0);
    let retry = transfer(&dest, "WPD", || Ok(Box::new(still_blocked)), |_| {});
    assert_eq!((retry.added, retry.pending, retry.failed), (1, 1, 1));
    let recovered = transfer(
        &dest,
        "WPD",
        || Ok(Box::new(fixture(3, &entries, None))),
        |_| {},
    );
    assert_eq!(
        (recovered.added, recovered.skipped, recovered.pending),
        (1, 1, 0)
    );
}

#[test]
fn unattended_verification_waits_for_two_identical_complete_inventories() {
    use super::{Target, initialize_control, verification};
    use rusqlite::{Connection, params};
    let sandbox = Sandbox::new();
    let dest = sandbox.dir("archive");
    let db_path = sandbox.0.join("control.sqlite3");
    let target = Target {
        library: "test".into(),
        album: 1,
        ..Default::default()
    };
    let db = Connection::open(&db_path).unwrap();
    initialize_control(&db).unwrap();
    let key = verification::device_key("fixture:serial-42");
    db.execute(
        "INSERT INTO phone_verification(library,album,device,minimum) VALUES('test',1,?,2)",
        [key],
    )
    .unwrap();
    drop(db);
    let run = |session, entries: &[(&str, &[u8])]| {
        let mut phone = fixture(session, entries, None);
        for (item, _) in &mut phone.files {
            item.version = Some(item.id.clone());
        }
        let mut report = transfer(&dest, "WPD", || Ok(Box::new(phone)), |_| {});
        let db = Connection::open(&db_path).unwrap();
        verification::observe(&db, &target, &mut report).unwrap();
        report
    };
    let a = ("a.jpg", b"one".as_slice());
    let b = ("b.jpg", b"two".as_slice());
    assert_eq!(run(1, &[a]).phase, "等待完整媒体清单");
    assert_eq!(run(2, &[a, b]).phase, "等待全量复扫");
    let alias = ("renamed.jpg", a.1);
    let changed = run(3, &[alias, b]);
    assert_eq!(
        (changed.phase.as_str(), changed.added, changed.verified),
        ("等待全量复扫", 0, false)
    );
    let confirmed = run(4, &[b, alias]);
    assert_eq!(
        (
            confirmed.phase.as_str(),
            confirmed.skipped,
            confirmed.verified
        ),
        ("已完成", 2, true)
    );
    let db = Connection::open(&db_path).unwrap();
    let verified: bool = db
        .query_row(
            "SELECT verified_at IS NOT NULL FROM phone_verification
        WHERE library=? AND album=?",
            params![target.library, target.album],
            |r| r.get(0),
        )
        .unwrap();
    assert!(verified);
}

#[cfg(windows)]
#[test]
fn unattended_retry_keeps_both_same_name_same_size_edit_files_accounted_for() {
    let sandbox = Sandbox::new();
    let dest = sandbox.dir("archive");
    let entries: [(&str, &[u8]); 3] = [
        ("photo.HEIC", b"primary"),
        ("photo.AAE", b"edit A"),
        ("photo.AAE", b"edit B"),
    ];
    let run = |session, count, failed| {
        let mut phone = fixture(session, &entries[..count], None);
        phone.session_error = failed;
        transfer(&dest, "WPD", || Ok(Box::new(phone)), |_| {})
    };
    let interrupted = run(1, 3, Some(2));
    assert_eq!((interrupted.added, interrupted.pending), (2, 1));
    let missing = run(2, 2, None);
    assert_eq!(
        (missing.phase.as_str(), missing.pending),
        ("仍有素材待补", 1)
    );
    let complete = run(3, 3, None);
    assert_eq!(
        (complete.added, complete.skipped, complete.pending),
        (1, 2, 0)
    );
    let repeat = run(4, 3, None);
    assert_eq!(
        (repeat.added, repeat.skipped, repeat.inventory),
        (0, 3, complete.inventory)
    );
}

#[cfg(windows)]
#[test]
fn unattended_retry_waits_longer_on_repeated_empty_sessions() {
    let mut report = super::Report {
        phase: "手机媒体暂不可读".into(),
        ..Default::default()
    };
    let mut previous = None;
    for seconds in [20, 40, 80, 160, 300, 300] {
        previous = super::next_phone_retry(&report, previous);
        assert_eq!(previous, Some(Duration::from_secs(seconds)));
    }
    report.added = 1;
    report.phase = "等待全量复扫".into();
    assert_eq!(
        super::next_phone_retry(&report, previous),
        Some(Duration::from_secs(20))
    );
    report.phase = "已完成".into();
    assert_eq!(super::next_phone_retry(&report, previous), None);
}

#[cfg(windows)]
#[test]
fn session_retry_imports_other_media_before_reopening_an_interrupted_family() {
    let t = Sandbox::new();
    let dest = t.dir("album");
    let entries: [(&str, &[u8]); 4] = [
        ("DCIM/first.jpg", b"first complete image"),
        ("DCIM/affected.HEIC", b"interrupted image"),
        ("DCIM/affected.AAE", b"companion edit"),
        ("DCIM/later.jpg", b"later readable image"),
    ];
    let run = |session, interrupted: bool| {
        let mut phone = fixture(session, &entries, None);
        phone.session_error = interrupted.then_some(1);
        transfer_guarded(
            &dest,
            "WPD",
            || Ok(Box::new(phone)),
            |_| {},
            || Ok(()),
            || Ok(()),
        )
    };
    let first = run(1, true);
    assert_eq!((first.added, first.scanned), (1, 2));
    let second = run(2, true);
    assert_eq!(&*second.phase, "手机会话中断");
    assert_eq!(
        (second.added, second.skipped, second.failed, second.scanned),
        (1, 1, 1, 4)
    );
    assert_eq!(originals(&dest).len(), 2);
    assert_eq!(
        fs::read_dir(dest.join(".time-cut/staging"))
            .unwrap()
            .count(),
        0
    );

    let recovered = run(3, false);
    assert_eq!(
        (recovered.added, recovered.skipped, recovered.failed),
        (2, 2, 0)
    );
    let files = originals(&dest);
    let photo = files
        .iter()
        .find(|file| fs::read(file).unwrap() == entries[1].1)
        .unwrap();
    assert_eq!(fs::read(photo.with_extension("AAE")).unwrap(), entries[2].1);
    let repeated = run(4, false);
    assert_eq!(
        (repeated.added, repeated.skipped, repeated.failed),
        (0, 4, 0)
    );
}

#[test]
fn disconnect_then_new_session_resumes_only_unfinished_original_with_companion() {
    let t = Sandbox::new();
    let dest = t.dir("图库");
    let entries: [(&str, &[u8]); 3] = [
        ("DCIM/IMG_0071.HEIC", b"image content for reader"),
        ("DCIM/IMG_0071.MOV", b"companion bytes"),
        ("Downloads/海边.jpg", b"third complete original"),
    ];
    let first = fixture(1, &entries, Some(2));
    {
        let mut a = Archive::open(&dest).unwrap();
        a.reconcile().unwrap();
        assert!(a.import(&first, &first.files[0].0, |_| Ok(())).unwrap());
        assert!(a.import(&first, &first.files[1].0, |_| Ok(())).unwrap());
        assert!(a.import(&first, &first.files[2].0, |_| Ok(())).is_err());
        assert!(Archive::open(&dest).is_err());
    }
    assert_eq!(originals(&dest).len(), 2);
    assert_eq!(
        fs::read_dir(dest.join(".time-cut/staging"))
            .unwrap()
            .count(),
        0
    );
    let second = fixture(77, &entries, None);
    let mut a = Archive::open(&dest).unwrap();
    a.reconcile().unwrap();
    for (i, (item, _)) in second.files.iter().enumerate() {
        assert_eq!(a.import(&second, item, |_| Ok(())).unwrap(), i == 2);
    }
    assert_eq!(&*second.reads.borrow(), &["77-2"]);
    let out = originals(&dest);
    assert_eq!(out.len(), 3);
    let photo = out
        .iter()
        .find(|p| p.extension().unwrap() == "HEIC")
        .unwrap();
    assert!(photo.with_extension("MOV").is_file());
    for (_, bytes) in entries {
        assert!(out.iter().any(|p| fs::read(p).unwrap() == bytes));
    }
}

#[test]
fn full_name_xmp_follows_its_explicit_original_in_a_mixed_format_family() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let items: [(&str, &[u8]); 4] = [
        ("IMG.HEIC", b"heic original"),
        ("IMG.JPG", b"JPEG original"),
        ("IMG.jpg.xmp", b"JPEG edits"),
        ("IMG.HEIC.xmp", b"HEIC edits"),
    ];
    let phone = fixture(1, &items, None);
    let mut archive = Archive::open(&dest).unwrap();
    for (item, _) in &phone.files {
        assert!(archive.import(&phone, item, |_| Ok(())).unwrap());
    }
    let all = originals(&dest);
    for (name, expected) in [
        ("IMG.JPG.xmp", &b"JPEG edits"[..]),
        ("IMG.HEIC.xmp", &b"HEIC edits"[..]),
    ] {
        let actual = all.iter().find(|p| p.file_name().unwrap() == name).unwrap();
        assert_eq!(fs::read(actual).unwrap(), expected);
    }
}

#[test]
fn same_named_phone_sidecars_keep_separate_receipts_across_sessions() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let entries: [(&str, &[u8]); 3] = [
        ("DCIM/IMG_0071.HEIC", b"original photo"),
        ("DCIM/IMG_0071.AAE", b"first adjustment"),
        ("DCIM/IMG_0071.AAE", b"second adjustment"),
    ];
    let initial = fixture(1, &entries[..2], None);
    {
        let mut archive = Archive::open(&dest).unwrap();
        for (item, _) in &initial.files {
            assert!(archive.import(&initial, item, |_| Ok(())).unwrap());
        }
    }
    // Start the next session from an archive made before version history existed.
    rusqlite::Connection::open(dest.join(".time-cut/imports.sqlite3"))
        .unwrap()
        .execute_batch("DROP TABLE IF EXISTS source_versions")
        .unwrap();

    let next = fixture(2, &entries, None);
    {
        let mut archive = Archive::open(&dest).unwrap();
        archive.reconcile().unwrap();
        for (index, (item, _)) in next.files.iter().enumerate() {
            assert_eq!(archive.import(&next, item, |_| Ok(())).unwrap(), index == 2);
        }
    }
    assert_eq!(&*next.reads.borrow(), &["2-2"]);
    let before = originals(&dest);
    assert_eq!(before.len(), 3);

    let repeat = fixture(3, &entries, None);
    let mut archive = Archive::open(&dest).unwrap();
    archive.reconcile().unwrap();
    for (item, _) in &repeat.files {
        assert!(!archive.import(&repeat, item, |_| Ok(())).unwrap());
    }
    assert!(
        repeat.reads.borrow().is_empty(),
        "reopened {:?}",
        repeat.reads.borrow()
    );
    let after = originals(&dest);
    assert_eq!(after.len(), before.len());
    for (_, bytes) in entries {
        assert!(after.iter().any(|p| fs::read(p).unwrap() == bytes));
    }
    let edited = after
        .iter()
        .find(|p| fs::read(p).unwrap() == entries[1].1)
        .unwrap();
    fs::write(edited, b"local adjustment").unwrap();
    archive.reconcile().unwrap();
    let repeat_after_edit = fixture(4, &entries, None);
    for (index, (item, _)) in repeat_after_edit.files.iter().enumerate() {
        assert_eq!(
            archive
                .import(&repeat_after_edit, item, |_| Ok(()))
                .unwrap(),
            index == 1
        );
    }
    assert_eq!(&*repeat_after_edit.reads.borrow(), &["4-1"]);
    assert_eq!(fs::read(edited).unwrap(), b"local adjustment");
    assert_eq!(originals(&dest).len(), 4);
}

#[test]
fn identical_xmp_and_aae_stay_editable_per_original_and_repeat_without_extra_copies() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let entries: [(&str, &[u8]); 8] = [
        ("A/IMG.JPG", b"first photo"),
        ("A/IMG.JPG.xmp", b"same preset"),
        ("A/IMG.AAE", b"same recipe"),
        ("A/clip.MP4", b"identical video"),
        ("B/IMG.JPG", b"second photo"),
        ("B/IMG.JPG.xmp", b"same preset"),
        ("B/IMG.AAE", b"same recipe"),
        ("B/clip.MP4", b"identical video"),
    ];
    let phone = fixture(3, &entries, None);
    let mut archive = Archive::open(&dest).unwrap();
    for (i, (item, _)) in phone.files.iter().enumerate() {
        assert_eq!(archive.import(&phone, item, |_| Ok(())).unwrap(), i != 7);
    }
    let first_files = originals(&dest);
    assert_eq!(first_files.len(), 7);
    let mut metadata = vec![];
    for photo in first_files
        .iter()
        .filter(|p| p.extension().is_some_and(|x| x == "JPG"))
    {
        let mut xmp = photo.as_os_str().to_owned();
        xmp.push(".xmp");
        let xmp = PathBuf::from(xmp);
        assert_eq!(fs::read(&xmp).unwrap(), b"same preset");
        assert_eq!(
            fs::read(photo.with_extension("AAE")).unwrap(),
            b"same recipe"
        );
        metadata.push(xmp);
    }
    assert_eq!(metadata.len(), 2);
    fs::write(&metadata[0], b"edited locally").unwrap();
    assert_eq!(fs::read(&metadata[1]).unwrap(), b"same preset");
    drop(archive);
    let rerun = fixture(8, &entries, None);
    let mut archive = Archive::open(&dest).unwrap();
    archive.reconcile().unwrap();
    let mut added = 0;
    for (item, _) in &rerun.files {
        added += usize::from(archive.import(&rerun, item, |_| Ok(())).unwrap());
    }
    assert_eq!(added, 1); // Retain local edits and remember the original sidecar's collision name.
    assert_eq!(fs::read(&metadata[0]).unwrap(), b"edited locally");
    let count = originals(&dest).len();
    assert_eq!(count, 8);
    rerun.reads.borrow_mut().clear();
    for (item, _) in &rerun.files {
        assert!(!archive.import(&rerun, item, |_| Ok(())).unwrap());
    }
    assert!(rerun.reads.borrow().is_empty());
    assert_eq!(originals(&dest).len(), count);
}

#[test]
fn a_long_renamed_photo_leaves_room_for_its_full_name_xmp() {
    let t = Sandbox::new();
    let dest = t.dir("archive").canonicalize().unwrap();
    let name = format!("{}x.JPG", "🌿".repeat(123));
    let first = format!("A/{name}");
    let second = format!("B/{name}");
    let first_xmp = format!("{first}.xmp");
    let second_xmp = format!("{second}.xmp");
    assert_eq!(format!("{name}.xmp").encode_utf16().count(), 255);
    let entries: [(&str, &[u8]); 4] = [
        (&first, b"one"),
        (&first_xmp, b"first metadata"),
        (&second, b"two"),
        (&second_xmp, b"second metadata"),
    ];
    let phone = fixture(1, &entries, None);
    let mut archive = Archive::open(&dest).unwrap();
    for (item, _) in &phone.files {
        assert!(archive.import(&phone, item, |_| Ok(())).unwrap());
    }
    let files = originals(&dest);
    assert_eq!(files.len(), 4);
    for photo in files
        .iter()
        .filter(|p| p.extension().is_some_and(|x| x == "JPG"))
    {
        let mut xmp = photo.as_os_str().to_owned();
        xmp.push(".xmp");
        let expected = if fs::read(photo).unwrap() == b"one" {
            &b"first metadata"[..]
        } else {
            &b"second metadata"[..]
        };
        assert_eq!(fs::read(PathBuf::from(xmp)).unwrap(), expected);
    }
}

#[test]
fn previous_cross_owner_sidecar_alias_is_repaired_on_the_next_phone_import() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let entries: [(&str, &[u8]); 4] = [
        ("one.JPG", b"first photo"),
        ("one.JPG.xmp", b"template"),
        ("two.JPG", b"second photo"),
        ("two.JPG.xmp", b"template"),
    ];
    let phone = fixture(1, &entries, None);
    let mut archive = Archive::open(&dest).unwrap();
    for (item, _) in &phone.files {
        assert!(archive.import(&phone, item, |_| Ok(())).unwrap());
    }
    drop(archive);
    let files = originals(&dest);
    let first = files.iter().find(|p| p.ends_with("one.JPG.xmp")).unwrap();
    let second = files.iter().find(|p| p.ends_with("two.JPG.xmp")).unwrap();
    fs::remove_file(second).unwrap();
    let db = rusqlite::Connection::open(dest.join(".time-cut/imports.sqlite3")).unwrap();
    db.execute_batch("DROP TABLE sidecar_paths").unwrap(); // Earlier archives had no owner index.
    db.execute(
        "UPDATE sources SET path=? WHERE entry='two.JPG.xmp'",
        [first.strip_prefix(&dest).unwrap().to_string_lossy()],
    )
    .unwrap();
    drop(db);

    let reconnect = fixture(99, &entries, None);
    let mut archive = Archive::open(&dest).unwrap();
    archive.reconcile().unwrap();
    for (i, (item, _)) in reconnect.files.iter().enumerate() {
        assert_eq!(
            archive.import(&reconnect, item, |_| Ok(())).unwrap(),
            i == 3
        );
    }
    assert_eq!(reconnect.reads.borrow().len(), 1);
    assert_eq!(fs::read(second).unwrap(), b"template");
    assert_eq!(originals(&dest).len(), 4);
}

#[test]
fn source_dates_before_1970_keep_their_year_on_files_without_embedded_dates() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let mut phone = fixture(1, &[("historical.xmp", b"sidecar")], None);
    phone.files[0].0.modified = Some(UNIX_EPOCH - Duration::from_secs(3 * 86_400));
    let mut archive = Archive::open(&dest).unwrap();
    assert!(
        archive
            .import(&phone, &phone.files[0].0, |_| Ok(()))
            .unwrap()
    );
    assert!(dest.join("1969/1969-12/historical.xmp").is_file());
}

#[test]
fn filename_collisions_preserve_long_unicode_names_and_existing_directories() {
    let t = Sandbox::new();
    let dest = t.dir("archive").canonicalize().unwrap();
    let long = format!("{}x.JPG", "🌿".repeat(125));
    assert_eq!(long.encode_utf16().count(), 255);
    let a = format!("a/{long}");
    let b = format!("b/{long}");
    let items: [(&str, &[u8]); 3] = [
        (&a, b"long photo one"),
        (&b, b"long photo two"),
        ("occupied.JPG", b"photo alongside a directory"),
    ];
    let phone = fixture(1, &items, None);
    let mut archive = Archive::open(&dest).unwrap();
    for (item, _) in &phone.files[..2] {
        assert!(archive.import(&phone, item, |_| Ok(())).unwrap());
    }
    let files = originals(&dest);
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|p| {
        p.file_name()
            .unwrap()
            .to_string_lossy()
            .encode_utf16()
            .count()
            <= 255
    }));
    let directory = files[0].parent().unwrap().join("occupied.JPG");
    fs::create_dir(&directory).unwrap();
    assert!(
        archive
            .import(&phone, &phone.files[2].0, |_| Ok(()))
            .unwrap()
    );
    assert!(directory.is_dir());
    assert_eq!(originals(&dest).len(), 3);
}

#[test]
fn name_collision_keeps_both_pairs_and_restart_recovers_unjournaled_original() {
    let t = Sandbox::new();
    let dest = t.dir("archive");
    let entries: [(&str, &[u8]); 4] = [
        ("A/IMG.JPG", b"first image"),
        ("A/IMG.MOV", b"first motion"),
        ("B/IMG.JPG", b"second image"),
        ("B/IMG.MOV", b"second motion"),
    ];
    let phone = fixture(5, &entries, None);
    let mut a = Archive::open(&dest).unwrap();
    a.reconcile().unwrap();
    for (item, _) in &phone.files {
        assert!(a.import(&phone, item, |_| Ok(())).unwrap());
    }
    drop(a);
    for p in originals(&dest)
        .into_iter()
        .filter(|p| p.extension().unwrap() == "JPG")
    {
        let expected = if fs::read(&p).unwrap() == b"first image" {
            &b"first motion"[..]
        } else {
            &b"second motion"[..]
        };
        assert_eq!(fs::read(p.with_extension("MOV")).unwrap(), expected);
    }
    // Crash after rename, before journal commit. Reconciliation finds the bytes.
    let db = rusqlite::Connection::open(dest.join(".time-cut/imports.sqlite3")).unwrap();
    db.execute_batch("DELETE FROM files; DELETE FROM sources;")
        .unwrap();
    drop(db);
    let abandoned = dest
        .join(".time-cut/staging")
        .join(format!("transfer-{}.jpg", uuid::Uuid::new_v4()));
    fs::write(&abandoned, b"incomplete").unwrap();
    let mut a = Archive::open(&dest).unwrap();
    a.reconcile().unwrap();
    for (item, _) in &phone.files {
        assert!(!a.import(&phone, item, |_| Ok(())).unwrap());
    }
    assert!(!abandoned.exists());
    assert_eq!(originals(&dest).len(), 4);
}
