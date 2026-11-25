use super::*;
use idevice::{Idevice, afc::opcode::AfcOpcode};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn service(rt: &Runtime) -> AfcClient {
    let (desktop, mut camera) = tokio::io::duplex(8192);
    rt.spawn(async move {
        let mut position = 0usize;
        loop {
            let mut header = [0u8; 40];
            if camera.read_exact(&mut header).await.is_err() {
                return;
            }
            let number = |start| u64::from_le_bytes(header[start..start + 8].try_into().unwrap());
            assert_eq!(number(0), idevice::afc::MAGIC);
            let mut request = vec![0; (number(8) - 40) as usize];
            camera.read_exact(&mut request).await.unwrap();
            let op = AfcOpcode::try_from(number(32)).unwrap();
            let path = String::from_utf8_lossy(&request);
            let path = if matches!(
                op,
                AfcOpcode::ReadDir | AfcOpcode::GetFileInfo | AfcOpcode::GetFileHash
            ) {
                path.strip_suffix('\0').expect("AFC path terminator")
            } else {
                path.as_ref()
            };
            let mut fields = Vec::new();
            let mut data = Vec::new();
            let response = match op {
                AfcOpcode::ReadDir => {
                    data = match path {
                        "/DCIM" => b".\0..\x00100APPLE\0".to_vec(),
                        "/DCIM/100APPLE" => b"IMG.JPG\0".to_vec(),
                        p => panic!("unexpected directory {p}"),
                    };
                    AfcOpcode::Data
                }
                AfcOpcode::GetFileInfo if path == "/DCIM/100APPLE/MISSING.JPG" => {
                    fields = (AfcError::ObjectNotFound as u64).to_le_bytes().to_vec();
                    AfcOpcode::Status
                }
                AfcOpcode::GetFileInfo => {
                    data = match path {
                        "/DCIM" | "/DCIM/100APPLE" => b"st_ifmt\0S_IFDIR\0".to_vec(),
                        "/DCIM/100APPLE/IMG.JPG" => {
                            b"st_ifmt\0S_IFREG\0st_size\x004\0st_mtime\x001700000000000000000\0"
                                .to_vec()
                        }
                        "/DCIM/ALIAS" => b"st_ifmt\0S_IFDIR\0LinkTarget\0/other\0".to_vec(),
                        "/DCIM/100APPLE/REDIRECT.JPG" => {
                            b"st_ifmt\0S_IFREG\0st_link_target\0/other\0".to_vec()
                        }
                        p => panic!("unexpected file {p}"),
                    };
                    AfcOpcode::Data
                }
                AfcOpcode::GetFileHash => {
                    assert_eq!(path, "/DCIM/100APPLE/IMG.JPG");
                    data = Sha1::digest(b"abcd").to_vec();
                    AfcOpcode::Data
                }
                AfcOpcode::FileOpen => {
                    assert_eq!(&request[..8], &(AfcFopenMode::RdOnly as u64).to_le_bytes());
                    assert_eq!(&request[8..], b"/DCIM/100APPLE/IMG.JPG\0");
                    position = 0;
                    fields = 1u64.to_le_bytes().to_vec();
                    AfcOpcode::FileOpenRes
                }
                AfcOpcode::Read => {
                    let n = u64::from_le_bytes(request[8..16].try_into().unwrap()) as usize;
                    let end = (position + n).min(4);
                    data = b"abcd"[position..end].to_vec();
                    position = end;
                    AfcOpcode::Data
                }
                AfcOpcode::FileClose => {
                    fields = 0u64.to_le_bytes().to_vec();
                    AfcOpcode::Status
                }
                _ => panic!("unexpected operation {op:?}"),
            };
            let full = 40 + fields.len() + data.len();
            let mut packet = Vec::new();
            for value in [
                idevice::afc::MAGIC,
                full as u64,
                40 + fields.len() as u64,
                number(24),
                response as u64,
            ] {
                packet.extend_from_slice(&value.to_le_bytes());
            }
            packet.extend(fields);
            packet.extend(data);
            camera.write_all(&packet).await.unwrap();
        }
    });
    AfcClient::new(Idevice::new(Box::new(desktop), "camera fixture"))
}

#[test]
fn camera_protocol_lists_dcim_checks_remote_hash_and_only_opens_read_only_streams() {
    let rt = runtime().unwrap();
    let device = UsbmuxdDevice {
        udid: "fixture".into(),
        connection_type: Transport::Usb,
        device_id: 1,
    };
    let phone = Phone {
        provider: device.to_provider(address(), "Time-cut test"),
        control: RefCell::new(service(&rt)),
        data: Rc::new(RefCell::new(Some(service(&rt)))),
        rt,
        identity: "afc:fixture".into(),
        serial: "fixture".into(),
    };
    let mut found = Vec::new();
    phone
        .visit(&mut |event| {
            if let ScanEvent::Ready(item) = event {
                assert_eq!(item.id, "/DCIM/100APPLE/IMG.JPG");
                assert_eq!(item.size, Some(4));
                assert_eq!(phone.resolve(&item).unwrap().unwrap().id, item.id);
                let mut different = item.clone();
                different.size = Some(5);
                assert!(phone.resolve(&different).unwrap().is_none());
                different = item.clone();
                different.version = Some("prior version".into());
                assert!(phone.resolve(&different).unwrap().is_none());
                for path in [
                    "DCIM/100APPLE/MISSING.JPG",
                    "DCIM/ALIAS/IMG.JPG",
                    "DCIM/100APPLE/REDIRECT.JPG",
                ] {
                    let mut expected = item.clone();
                    expected.relative = path.into();
                    assert!(phone.resolve(&expected).unwrap().is_none());
                }
                assert_eq!(
                    phone.content_sha1(&item).unwrap().unwrap().as_slice(),
                    Sha1::digest(b"abcd").as_slice()
                );
                let mut bytes = Vec::new();
                let mut file = phone.reader(&item).unwrap();
                file.read_to_end(&mut bytes).unwrap();
                drop(file);
                phone.verify_read(&item).unwrap();
                found.push(bytes);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(found, [b"abcd".to_vec()]);
    assert!(
        phone.data.borrow().is_some(),
        "a clean EOF returns the file session for reuse"
    );
}

#[test]
fn ambiguous_names_cannot_escape_or_collapse_camera_paths() {
    for value in ["../elsewhere", "C:drive", "a/b", "a\\b", "bad\u{fffd}"] {
        assert!(children(vec![value.to_owned()]).is_err());
    }
    assert!(children(vec!["same".into(), "same".into()]).is_err());
    assert!(remote_path(Path::new("Media/other.jpg")).is_err());
    assert!(remote_path(Path::new("DCIM/../other.jpg")).is_err());
}

struct ShortConnection {
    names: Vec<&'static str>,
    unreadable: Option<&'static str>,
    changed: Option<&'static str>,
    root_unavailable: bool,
    interrupted_after_listing: bool,
    hashes: RefCell<Vec<PathBuf>>,
}

impl ShortConnection {
    fn new(names: &[&'static str]) -> Self {
        Self {
            names: names.to_vec(),
            unreadable: None,
            changed: None,
            root_unavailable: false,
            interrupted_after_listing: false,
            hashes: RefCell::new(Vec::new()),
        }
    }
    fn item(name: &str) -> Item {
        Item {
            relative: PathBuf::from(name),
            id: format!("/{name}"),
            version: Some("native-v1".into()),
            size: Some(name.len() as u64),
            modified: Some(UNIX_EPOCH),
        }
    }
    fn scarce() -> Box<dyn std::error::Error + Send + Sync> {
        IdeviceError::Afc(AfcError::NoResources).into()
    }
    fn bytes(&self, item: &Item) -> Vec<u8> {
        let mut bytes = item.id.as_bytes()[1..].to_vec();
        if self
            .changed
            .is_some_and(|name| item.relative == Path::new(name))
        {
            bytes[0] ^= 1;
        }
        bytes
    }
    fn receive(&self, root: &Path) -> Result<Option<Audit>> {
        let key = crate::ingest::verification::device_key("wpd:CAMERA");
        supplement::collect(
            root,
            &key,
            "CAMERA",
            self,
            true,
            |_| {},
            || Ok(()),
            || Ok(()),
        )
    }
}
impl crate::ingest::archive::SavedFiles for ShortConnection {
    fn resolve(&self, expected: &Item) -> Result<Option<Item>> {
        Ok(self
            .names
            .iter()
            .map(|n| Self::item(n))
            .find(|i| i.relative == expected.relative))
    }
}
impl Source for ShortConnection {
    fn identity(&self) -> &str {
        "afc:CAMERA"
    }
    fn saved_files(&self) -> Option<&dyn crate::ingest::archive::SavedFiles> {
        Some(self)
    }
    fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> {
        if self.root_unavailable {
            return Err(Self::scarce());
        }
        for name in &self.names {
            emit(ScanEvent::Ready(Self::item(name)))?;
        }
        if self.interrupted_after_listing {
            return Err("session ended".into());
        }
        Ok(())
    }
    fn content_sha1(&self, item: &Item) -> Result<Option<[u8; 20]>> {
        self.hashes.borrow_mut().push(item.relative.clone());
        if self
            .unreadable
            .is_some_and(|p| item.relative == Path::new(p))
        {
            return Err(Self::scarce());
        }
        Ok(Some(Sha1::digest(self.bytes(item)).into()))
    }
    fn reader(&self, item: &Item) -> Result<Box<dyn Read>> {
        Ok(Box::new(io::Cursor::new(self.bytes(item))))
    }
}

#[test]
fn interrupted_camera_prioritizes_new_files_across_folders_and_picks_up_by_path_without_claiming_completion()
 {
    use crate::ingest::{archive::Archive, inbox, tests::Sandbox};
    let sandbox = Sandbox::new();
    let root = sandbox.dir("originals");
    {
        let archive = Archive::open(&root).unwrap();
        inbox::initialize(&archive.db).unwrap();
        archive
            .db
            .execute(
                "INSERT INTO phone_runs(source,started) VALUES('wpd:CAMERA',1)",
                [],
            )
            .unwrap();
    }
    let db = rusqlite::Connection::open(root.join(".time-cut/imports.sqlite3")).unwrap();
    let old = "DCIM/100/OLD.HEIC";
    let broken = "DCIM/101/EDIT.AAE";
    let movie = "DCIM/102/NEW.MOV";
    let mut first = ShortConnection::new(&[old]);
    first.interrupted_after_listing = true;
    assert!(first.receive(&root).is_err());
    let receipts = || {
        db.query_row("SELECT COUNT(digest) FROM phone_native_items", [], |r| {
            r.get::<_, u64>(0)
        })
        .unwrap()
    };
    assert_eq!(
        receipts(),
        1,
        "the first camera connection saved the old original"
    );
    let mut second = ShortConnection::new(&[old, broken, movie]);
    second.unreadable = Some(broken);
    assert!(second.receive(&root).is_err());
    assert_eq!(
        receipts(),
        2,
        "a single unreadable edit must not block a new video from another folder"
    );
    let hashes = second.hashes.borrow();
    assert_eq!(
        hashes.first().map(PathBuf::as_path),
        Some(Path::new(broken)),
        "old files must yield the readable window to new ones"
    );
    assert!(hashes.iter().any(|p| p == Path::new(movie)));
    assert!(
        !hashes.iter().any(|p| p == Path::new(old)),
        "an incomplete pass need not rehash verified originals"
    );
    let mut third = ShortConnection::new(&[old, broken, movie]);
    third.root_unavailable = true;
    assert!(third.receive(&root).is_err());
    assert_eq!(
        receipts(),
        3,
        "a previously observed original can be received by its exact path during a directory outage"
    );
    assert_eq!(third.hashes.borrow().as_slice(), [PathBuf::from(broken)]);
    assert!(!inbox::verify_supplements(&Archive::open(&root).unwrap(), 1).unwrap());
    let complete = ShortConnection::new(&[old, broken, movie]);
    let status = complete.receive(&root).unwrap().unwrap();
    assert_eq!((status.listed, status.matched, status.copied), (3, 3, 0));
    assert!(!status.repeat_verified);
    let next = complete.receive(&root).unwrap().unwrap();
    assert!(next.repeat_verified);
    assert_eq!(next.copied, 0);
}

#[test]
fn a_same_size_edit_during_the_required_second_pass_preserves_both_bodies_and_blocks_completion() {
    use crate::ingest::{archive::Archive, inbox, tests::Sandbox};
    let sandbox = Sandbox::new();
    let root = sandbox.dir("originals");
    {
        let archive = Archive::open(&root).unwrap();
        inbox::initialize(&archive.db).unwrap();
        archive
            .db
            .execute(
                "INSERT INTO phone_runs(source,started) VALUES('wpd:CAMERA',1)",
                [],
            )
            .unwrap();
    }
    let old = "DCIM/100/OLD.HEIC";
    let original = ShortConnection::new(&[old]);
    let initial = original.receive(&root).unwrap().unwrap();
    assert_eq!(initial.matched, 1);
    assert!(!initial.repeat_verified);
    let mut edited = ShortConnection::new(&[old]);
    edited.changed = Some(old);
    assert!(edited.receive(&root).is_err());
    let db = rusqlite::Connection::open(root.join(".time-cut/imports.sqlite3")).unwrap();
    let versions: u64 = db
        .query_row(
            "SELECT count(DISTINCT digest) FROM phone_inbox WHERE source=? AND entry=?",
            [edited.identity(), old],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        versions, 2,
        "both contents returned under the same camera metadata survive"
    );
    let conflict: bool = db
        .query_row(
            "SELECT conflict FROM phone_native_items WHERE entry=?",
            [old],
            |r| r.get(0),
        )
        .unwrap();
    assert!(conflict);
    assert!(!inbox::verify_supplements(&Archive::open(&root).unwrap(), 1).unwrap());
}
