//! In-process WPD provider for the interfaces consumed by Phone::visit and Phone::reader.
//! Nothing is registered with the system. Enumeration, values, and streams cross the COM ABI.
use super::*;
use crate::ingest::{
    archive::Folder,
    tests::{Sandbox, originals},
    transfer,
};
use std::{
    cell::Cell,
    fs,
    path::Path,
    sync::{Arc, Mutex},
};
use windows::Win32::{
    Foundation::{
        E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, ERROR_DEVICE_NOT_CONNECTED, FILETIME,
        S_FALSE, S_OK,
    },
    System::Com::{CoTaskMemAlloc, StructuredStorage::InitPropVariantFromFileTime},
    UI::Shell::SHCreateMemStream,
};
use windows_core::{
    Error, HRESULT, Interface, OutRef, PCWSTR, Ref, Result as ComResult, implement,
};

// All mutating WPD calls and operations outside the reader's contract are unsupported.
macro_rules! unsupported {
    ($(fn $method:ident($($arg:ty),*) -> $out:ty;)+) => {
        $(fn $method(&self, $(_: $arg),*) -> ComResult<$out> { Err(E_NOTIMPL.into()) })+
    };
}

#[implement(IEnumPortableDeviceObjectIDs)]
struct ObjectIds {
    ids: Vec<String>,
    offset: Cell<usize>,
}

impl IEnumPortableDeviceObjectIDs_Impl for ObjectIds_Impl {
    fn Next(&self, requested: u32, out: *mut PWSTR, fetched: *mut u32) -> HRESULT {
        if out.is_null() || fetched.is_null() {
            return E_POINTER;
        }
        unsafe { fetched.write(0) };
        for id in self.ids[self.offset.get()..]
            .iter()
            .take(requested as usize)
        {
            let wide: Vec<_> = id.encode_utf16().chain([0]).collect();
            unsafe {
                let ptr = CoTaskMemAlloc(std::mem::size_of_val(&wide[..])).cast::<u16>();
                if ptr.is_null() {
                    return E_OUTOFMEMORY;
                }
                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                out.add(*fetched as usize).write(PWSTR(ptr));
                *fetched += 1;
            }
            self.offset.set(self.offset.get() + 1);
        }
        if unsafe { *fetched } == requested {
            S_OK
        } else {
            S_FALSE
        }
    }
    fn Skip(&self, _: u32) -> HRESULT {
        E_NOTIMPL
    }
    unsupported! {
        fn Reset() -> ();
        fn Clone() -> IEnumPortableDeviceObjectIDs;
        fn Cancel() -> ();
    }
}

#[derive(Clone, Copy)]
enum SizeHint {
    Object,
    Resource,
    Stream,
}
#[derive(Clone, Copy)]
enum Kind {
    Storage,
    Folder,
    FolderWithStorageAlias,
    ReadFaultFolder,
    ReadFaultMetadata,
    Service,
    MissingType,
    File(&'static [u8], SizeHint),
}
type Row = (String, Kind);

#[implement(
    IPortableDeviceContent,
    IPortableDeviceProperties,
    IPortableDeviceResources
)]
struct CameraSession {
    number: u32,
    rows: Vec<Row>,
    short_file: Option<&'static str>,
    opened_files: Arc<Mutex<Vec<String>>>,
    property_trace: Option<Arc<Mutex<Vec<(String, usize)>>>>,
}

impl CameraSession {
    fn row(&self, raw: &PCWSTR) -> ComResult<&Row> {
        let id = unsafe { raw.to_string()? };
        let primary = format!("{}-", self.number);
        let index = id
            .strip_prefix(&primary)
            .and_then(|s| s.parse::<usize>().ok());
        index
            .and_then(|i| self.rows.get(i))
            .ok_or(E_INVALIDARG.into())
    }
}

impl IPortableDeviceContent_Impl for CameraSession_Impl {
    fn GetObjectIDsFromPersistentUniqueIDs(&self, requested: Ref<'_, IPortableDevicePropVariantCollection>) -> ComResult<IPortableDevicePropVariantCollection> {
        unsafe {
            let ids: IPortableDevicePropVariantCollection = CoCreateInstance(&PortableDevicePropVariantCollection,None,CLSCTX_INPROC_SERVER)?;
            let values = values()?;
            let requested = requested.ok()?;
            let mut count = 0;
            requested.GetCount(&raw mut count)?;
            for index in 0..count {
                let mut value = windows::Win32::System::Com::StructuredStorage::PROPVARIANT::default();
                requested.GetAt(index,&raw mut value)?;
                let stored=values.SetValue(&WPD_OBJECT_PERSISTENT_UNIQUE_ID,&value);
                let _=PropVariantClear(&mut value);
                stored?;
                let persistent=text_value(&values,&WPD_OBJECT_PERSISTENT_UNIQUE_ID).ok_or(E_INVALIDARG)?;
                let handle=self.rows.iter().position(|(path,_)| path==&persistent)
                    .map(|n|format!("{}-{n}",self.number)).unwrap_or_default();
                values.SetStringValue(&WPD_OBJECT_ID,&HSTRING::from(handle))?;
                let mut value=values.GetValue(&WPD_OBJECT_ID)?;
                let added=ids.Add(&value);
                let _=PropVariantClear(&mut value);
                added?;
            }
            Ok(ids)
        }
    }
    fn EnumObjects(
        &self,
        _: u32,
        parent: &PCWSTR,
        _: Ref<'_, IPortableDeviceValues>,
    ) -> ComResult<IEnumPortableDeviceObjectIDs> {
        let path = if unsafe { parent.to_string()? } == "DEVICE" {
            ""
        } else {
            let (path,kind)=self.row(parent)?;
            if matches!(kind,Kind::ReadFaultFolder) {
                return Err(windows::Win32::Foundation::ERROR_READ_FAULT.to_hresult().into());
            }
            path.as_str()
        };
        let ids = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, (p, _))| p.rsplit_once('/').map_or("", |(dir, _)| dir) == path)
            .map(|(i, _)| format!("{}-{i}", self.number))
            .collect();
        Ok(ObjectIds {
            ids,
            offset: Cell::new(0),
        }
        .into())
    }
    unsupported! {
        fn Properties() -> IPortableDeviceProperties;
        fn Transfer() -> IPortableDeviceResources;
        fn CreateObjectWithPropertiesOnly(Ref<'_, IPortableDeviceValues>, *mut PWSTR) -> ();
        fn CreateObjectWithPropertiesAndData(Ref<'_, IPortableDeviceValues>, OutRef<'_, IStream>, *mut u32, *mut PWSTR) -> ();
        fn Delete(u32, Ref<'_, IPortableDevicePropVariantCollection>, OutRef<'_, IPortableDevicePropVariantCollection>) -> ();
        fn Cancel() -> ();
        fn Move(Ref<'_, IPortableDevicePropVariantCollection>, &PCWSTR, OutRef<'_, IPortableDevicePropVariantCollection>) -> ();
        fn Copy(Ref<'_, IPortableDevicePropVariantCollection>, &PCWSTR, OutRef<'_, IPortableDevicePropVariantCollection>) -> ();
    }
}

fn values() -> ComResult<IPortableDeviceValues> {
    unsafe { CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER) }
}

#[test]
fn wpd_resumes_only_the_listed_version_on_its_current_handle_and_parent() {
    use crate::ingest::{archive::Archive, capture, inbox, inventory};
    let _apartment = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let dest = temp.dir("gallery");
    let opened = Arc::new(Mutex::new(Vec::new()));
    let rows = || vec![row("Internal", Kind::Storage), row("Internal/Camera", Kind::FolderWithStorageAlias),
        row("Internal/Other", Kind::Folder),
        row("Internal/Camera/photo.jpg", Kind::File(b"abcd", SizeHint::Object)),
        row("Internal/Camera/clip.mov", Kind::File(b"abcde", SizeHint::Object))];
    let old = phone(1,rows(),None,opened.clone());
    let mut members = Vec::new();
    old.visit(&mut |event| {
        if let ScanEvent::Ready(item) = event { members.push(inventory::Member::new(item)); }
        Ok(())
    }).unwrap();
    let next=phone(2,rows(),None,opened.clone());
    let saved=&members[0].item;
    let current=next.saved_files().unwrap().resolve(saved).unwrap().unwrap();
    assert_ne!(current.id,saved.id);
    let mut wrong=saved.clone();
    wrong.relative=Path::new("Internal/Other").join(saved.relative.file_name().unwrap());
    assert!(next.saved_files().unwrap().resolve(&wrong).unwrap().is_none());
    wrong=saved.clone(); wrong.size=Some(102);
    assert!(next.saved_files().unwrap().resolve(&wrong).unwrap().is_none());
    {
        let archive=Archive::open(&dest).unwrap();
        inbox::recover(&archive).unwrap();
        archive.db.execute("INSERT INTO phone_runs(source,started,sessions) VALUES(?,1000,3)",[old.identity()]).unwrap();
        inventory::publish(&archive.db,1,&members,&mut Default::default()).unwrap();
    }
    let receive=|number|capture::receive(&dest,"iPhone",|_|Ok(2),
        ||Ok(Box::new(phone(number,rows(),None,opened.clone()))),|_|{},||Ok(()),||Ok(()));
    let targeted=receive(3);
    assert!(targeted.safe_at.is_none(),"direct transfers cannot release the cable");
    assert_eq!((targeted.added,targeted.newly_verified,targeted.list_covered),(2,2,2),"{:?}",targeted.issues);
    assert_eq!(opened.lock().unwrap().len(),4,"untrusted resumed streams are checked twice");
    let complete=receive(4);
    assert!(complete.safe_at.is_some(),"{:?}",complete.issues);
    assert_eq!((complete.added,complete.skipped),(0,2));
}

#[test]
fn direct_receipt_comparisons_use_positive_controls_and_double_read_new_contents() {
    use crate::ingest::{archive::Archive, capture, inbox, inventory};
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let dest = temp.dir("gallery");
    let opened = Arc::new(Mutex::new(Vec::new()));
    let rows = |current: bool, bad_reference: bool| {
        let mut files = vec![
            row("Storage", Kind::Storage),
            row("Storage/Camera", Kind::FolderWithStorageAlias),
            row(
                "Storage/Camera/control-a.jpg",
                Kind::File(b"right A", SizeHint::Object),
            ),
            row(
                "Storage/Camera/control-b.jpg",
                Kind::File(
                    if bad_reference {
                        b"wrong B"
                    } else {
                        b"right B"
                    },
                    SizeHint::Object,
                ),
            ),
        ];
        for i in 0..26 {
            let bytes = if current && i == 25 {
                b"new photo!"
            } else {
                b"old photo!"
            };
            let entry = if current && i == 24 {
                "Storage/Camera/fresh-rename.jpg".to_string()
            } else {
                format!("Storage/Camera/p{i}.jpg")
            };
            files.push(row(&entry, Kind::File(bytes, SizeHint::Object)));
        }
        if current {
            files.push(row(
                "Storage/Camera/new-name.jpg",
                Kind::File(b"old photo!", SizeHint::Object),
            ));
        }
        files
    };
    assert_eq!(
        run(&dest, phone(1, rows(false, false), None, opened.clone())).failed,
        0
    );
    let camera = phone(2, rows(true, false), None, opened.clone());
    {
        let archive = Archive::open(&dest).unwrap();
        inbox::recover(&archive).unwrap();
        for table in ["sources", "source_versions"] {
            archive.db.execute(&format!("UPDATE {table} SET version='old-'||version WHERE entry NOT LIKE '%control-%'"), []).unwrap();
        }
        archive
            .db
            .execute(
                "INSERT INTO phone_runs(source,started,sessions) VALUES(?,1000,3)",
                [camera.identity()],
            )
            .unwrap();
        let mut members = vec![];
        camera
            .visit(&mut |event| {
                if let ScanEvent::Ready(item) = event {
                    members.push(inventory::Member::new(item));
                }
                Ok(())
            })
            .unwrap();
        inventory::Receipts::load(&archive.db, camera.identity())
            .unwrap()
            .match_members(&mut members);
        inventory::publish(&archive.db, 1, &members, &mut Default::default()).unwrap();
        let n=archive.db.execute("INSERT INTO phone_item_retries SELECT run_id,entry,version,ordinal FROM phone_list_items WHERE digest IS NOT NULL",[]).unwrap();
        assert_eq!(n, 2);
        // One rename is fresh and the other had an unsuccessful lookup. This
        // exercises independent content confirmation on both sides of calibration.
        let tried = archive.db.execute("INSERT INTO phone_item_retries SELECT run_id,entry,version,1 FROM phone_list_items WHERE entry LIKE '%new-name.jpg'", []).unwrap();
        assert_eq!(tried, 1);
    }
    let receive = |session, bad| {
        capture::receive(
            &dest,
            "iPhone",
            |_| Ok(29),
            || {
                Ok(Box::new(phone(
                    session,
                    rows(true, bad),
                    None,
                    opened.clone(),
                )))
            },
            |_| {},
            || Ok(()),
            || Ok(()),
        )
    };
    opened.lock().unwrap().clear();
    let interrupted = receive(3, true);
    assert_eq!(interrupted.newly_verified, 8);
    assert!(interrupted.safe_at.is_none() && interrupted.failed > 0);
    assert_eq!(interrupted.phase, "手机返回内容与清单不符");
    assert_eq!(opened.lock().unwrap().iter().filter(|p| p.ends_with("/fresh-rename.jpg")).count(), 2);
    assert!(
        interrupted
            .issues
            .iter()
            .any(|message| message.contains("不一致"))
    );
    assert!(
        !opened
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.ends_with("/p20.jpg"))
    );
    opened.lock().unwrap().clear();
    let checked = receive(4, false);
    assert_eq!(
        (checked.newly_verified, checked.list_covered, checked.failed),
        (19, 29, 0),
        "{:?}",
        checked.issues
    );
    let requests = opened.lock().unwrap();
    let count = |file: &str| requests.iter().filter(|p| p.ends_with(file)).count();
    assert_eq!(
        count("/p8.jpg"),
        2,
        "before the controls both phone reads are needed"
    );
    assert_eq!(
        count("/p20.jpg"),
        1,
        "verified historical bytes take one USB read"
    );
    assert_eq!(
        count("/p25.jpg"),
        2,
        "a same-size edit still requires independent confirmation"
    );
    assert_eq!(
        count("/new-name.jpg"),
        2,
        "a cross-name match cannot calibrate its own identity"
    );
}

#[test]
fn two_checked_sessions_preserve_a_short_response_without_certifying_the_listed_jpeg() {
    use crate::ingest::{archive::Archive, capture, inbox, inventory};
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let dest = temp.dir("gallery");
    let opened = Arc::new(Mutex::new(Vec::new()));
    const A: &[u8] = &[65; 4096];
    const B: &[u8] = &[66; 4100];
    const TARGET: &str = "Storage/Camera/target.JPG";
    let objects = |with_target| {
        let mut rows = vec![
            row("Storage", Kind::Storage), row("Storage/Camera", Kind::FolderWithStorageAlias),
            row("Storage/Camera/a.jpg", Kind::File(A,SizeHint::Object)),
            row("Storage/Camera/b.jpg", Kind::File(B,SizeHint::Object)),
        ];
        if with_target { rows.push(row(TARGET, Kind::File(b"12345678", SizeHint::Object))); }
        rows
    };
    assert_eq!(run(&dest,phone(1, objects(false),None,opened.clone())).failed,0);
    let camera = phone(2, objects(true),None,opened.clone());
    {
        let archive=Archive::open(&dest).unwrap();
        inbox::recover(&archive).unwrap();
        archive.db.execute("INSERT INTO phone_runs(source,started,sessions) VALUES(?,1000,3)",[camera.identity()]).unwrap();
        let mut members=vec![];
        camera.visit(&mut |event| {
            if let ScanEvent::Ready(item)=event { members.push(inventory::Member::new(item)); }
            Ok(())
        }).unwrap();
        inventory::Receipts::load(&archive.db,camera.identity()).unwrap().match_members(&mut members);
        inventory::publish(&archive.db,1,&members,&mut Default::default()).unwrap();
    }
    let mut rounds = vec![];
    for session in 4..=8 {
        opened.lock().unwrap().clear();
        let r=capture::receive(&dest,"iPhone", |_| Ok(3),
            ||Ok(Box::new(phone(session,objects(true),Some(TARGET),opened.clone()))), |_|{}, ||Ok(()), ||Ok(()));
        assert_eq!(r.list_covered,2,"{session}: {:?}",r.issues);
        assert!(r.safe_at.is_none());
        let calls=opened.lock().unwrap().clone();
        assert!(calls.last().is_some_and(|s| s==TARGET));
        if calls.len()==3 { rounds.push(calls); } else { assert_eq!(calls.len(),1); }
    }
    assert_eq!(rounds.len(),2);
    assert_eq!(rounds[0][0],rounds[1][1]);
    assert_eq!(rounds[0][1],rounds[1][0]);
    assert_ne!(rounds[0][0],rounds[0][1]);
    let archive=Archive::open(&dest).unwrap();
    let evidence:(u64,u64,u64,String)=archive.db.query_row(
        "SELECT sessions,first_controlled,last_controlled,digest FROM phone_returned_data",
        [], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).unwrap();
    assert_eq!(evidence,(5,6,8,blake3::hash(b"1234").to_hex().to_string()));
    let credits:u64=archive.db.query_row("SELECT COUNT(digest) FROM phone_list_items WHERE lower(entry) LIKE '%target.jpg'",[],|r|r.get(0)).unwrap();
    assert_eq!(credits,0);
}

impl IPortableDeviceProperties_Impl for CameraSession_Impl {
    fn GetValues(
        &self,
        object: &PCWSTR,
        _: Ref<'_, IPortableDeviceKeyCollection>,
    ) -> ComResult<IPortableDeviceValues> {
        let (path, kind) = self.row(object)?;
        if let Some(trace) = &self.property_trace {
            trace
                .lock()
                .unwrap()
                .push((path.clone(), self.opened_files.lock().unwrap().len()));
        }
        // A disconnected driver can return a clean EOF and stop serving the object's properties.
        if self.short_file == Some(path.as_str())
            && matches!(kind, Kind::File(_, SizeHint::Stream))
            && self.opened_files.lock().unwrap().contains(path)
        {
            return Err(ERROR_DEVICE_NOT_CONNECTED.to_hresult().into());
        }
        let vals = values()?;
        let name = HSTRING::from(path.rsplit('/').next().unwrap());
        let content = match kind {
            Kind::ReadFaultMetadata => return Err(windows::Win32::Foundation::ERROR_READ_FAULT.to_hresult().into()),
            Kind::MissingType => return Ok(vals),
            Kind::Storage | Kind::Service => WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT,
            Kind::Folder | Kind::FolderWithStorageAlias | Kind::ReadFaultFolder => WPD_CONTENT_TYPE_FOLDER,
            Kind::File(..) => WPD_CONTENT_TYPE_GENERIC_FILE,
        };
        unsafe {
            vals.SetGuidValue(&WPD_OBJECT_CONTENT_TYPE, &content)?;
            vals.SetStringValue(&WPD_OBJECT_NAME, &name)?;
            let parent = path.rsplit_once('/').and_then(|(dir,_)| self.rows.iter().position(|(name,_)|name==dir))
                .map(|index|format!("{}-{index}",self.number)).unwrap_or_else(|| "DEVICE".into());
            // The actual device exposes the storage by enumeration, but its
            // month folders report a parent ID which GetValues cannot open.
            let parent = if matches!(kind, Kind::FolderWithStorageAlias) { "unreadable-storage-alias".into() } else { parent };
            vals.SetStringValue(&WPD_OBJECT_PARENT_ID,&HSTRING::from(parent))?;
            match kind {
                Kind::Storage => vals.SetGuidValue(
                    &WPD_FUNCTIONAL_OBJECT_CATEGORY,
                    &WPD_FUNCTIONAL_CATEGORY_STORAGE,
                )?,
                Kind::Service => vals.SetGuidValue(
                    &WPD_FUNCTIONAL_OBJECT_CATEGORY,
                    &WPD_FUNCTIONAL_CATEGORY_RENDERING_INFORMATION,
                )?,
                Kind::File(bytes, hint) => {
                    vals.SetStringValue(
                        &WPD_OBJECT_PERSISTENT_UNIQUE_ID,
                        &HSTRING::from(path.as_str()),
                    )?;
                    vals.SetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME, &name)?;
                    if matches!(hint, SizeHint::Object) {
                        vals.SetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE, bytes.len() as u64)?;
                    }
                    let ticks = 116444736000000000u64 + 1_700_000_000u64 * 10_000_000;
                    let mut value = InitPropVariantFromFileTime(&FILETIME {
                        dwLowDateTime: ticks as u32,
                        dwHighDateTime: (ticks >> 32) as u32,
                    })?;
                    let set = vals.SetValue(&WPD_OBJECT_DATE_MODIFIED, &value);
                    PropVariantClear(&mut value)?;
                    set?;
                }
                Kind::Folder | Kind::FolderWithStorageAlias | Kind::ReadFaultFolder | Kind::ReadFaultMetadata | Kind::MissingType => (),
            }
        }
        Ok(vals)
    }
    unsupported! {
        fn GetSupportedProperties(&PCWSTR) -> IPortableDeviceKeyCollection;
        fn GetPropertyAttributes(&PCWSTR, *const PROPERTYKEY) -> IPortableDeviceValues;
        fn SetValues(&PCWSTR, Ref<'_, IPortableDeviceValues>) -> IPortableDeviceValues;
        fn Delete(&PCWSTR, Ref<'_, IPortableDeviceKeyCollection>) -> ();
        fn Cancel() -> ();
    }
}

impl IPortableDeviceResources_Impl for CameraSession_Impl {
    fn GetResourceAttributes(
        &self,
        object: &PCWSTR,
        key: *const PROPERTYKEY,
    ) -> ComResult<IPortableDeviceValues> {
        if unsafe { key.as_ref() } != Some(&WPD_RESOURCE_DEFAULT) {
            return Err(E_INVALIDARG.into());
        }
        let vals = values()?;
        if let Kind::File(bytes, SizeHint::Resource) = self.row(object)?.1 {
            unsafe {
                vals.SetUnsignedLargeIntegerValue(
                    &WPD_RESOURCE_ATTRIBUTE_TOTAL_SIZE,
                    bytes.len() as u64,
                )?
            };
        }
        Ok(vals)
    }
    fn GetStream(
        &self,
        object: &PCWSTR,
        key: *const PROPERTYKEY,
        mode: u32,
        hint: *mut u32,
        out: OutRef<'_, IStream>,
    ) -> ComResult<()> {
        if unsafe { key.as_ref() } != Some(&WPD_RESOURCE_DEFAULT) || mode != 0 || hint.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let (path, kind) = self.row(object)?;
        let Kind::File(bytes, _) = kind else {
            return Err(E_INVALIDARG.into());
        };
        self.opened_files.lock().unwrap().push(path.clone());
        let bytes = if Some(path.as_str()) == self.short_file {
            &bytes[..bytes.len() / 2]
        } else {
            bytes
        };
        let stream = unsafe { SHCreateMemStream(Some(bytes)) }.ok_or(Error::from(E_OUTOFMEMORY))?;
        unsafe { hint.write(3) };
        out.write(Some(stream))
    }
    unsupported! {
        fn GetSupportedResources(&PCWSTR) -> IPortableDeviceKeyCollection;
        fn Delete(&PCWSTR, Ref<'_, IPortableDeviceKeyCollection>) -> ();
        fn Cancel() -> ();
        fn CreateResource(Ref<'_, IPortableDeviceValues>, OutRef<'_, IStream>, *mut u32, *mut PWSTR) -> ();
    }
}

fn phone(
    number: u32,
    rows: Vec<Row>,
    short_file: Option<&'static str>,
    opened_files: Arc<Mutex<Vec<String>>>,
) -> Phone {
    traced_phone(number, rows, short_file, opened_files, None)
}

fn traced_phone(
    number: u32,
    rows: Vec<Row>,
    short_file: Option<&'static str>,
    opened_files: Arc<Mutex<Vec<String>>>,
    property_trace: Option<Arc<Mutex<Vec<(String, usize)>>>>,
) -> Phone {
    let content: IPortableDeviceContent = CameraSession {
        number,
        rows,
        short_file,
        opened_files,
        property_trace,
    }
    .into();
    Phone {
        device: unsafe {
            CoCreateInstance(&PortableDeviceFTM, None, CLSCTX_INPROC_SERVER).unwrap()
        },
        props: content.cast().unwrap(),
        transfer: content.cast().unwrap(),
        content,
        fields: keys(&[]).unwrap(), // The fixture returns all supported properties for each object.
        identity: "wpd:test-serial".into(),
        folders: Default::default(),
    }
}

fn row(path: &str, kind: Kind) -> Row {
    (path.to_owned(), kind)
}

#[test]
fn a_returning_historical_file_is_received_before_an_unrelated_folder_fails() {
    use crate::ingest::{archive::Archive, capture, inbox, inventory, resume::PhoneQueue};
    let _apartment = Apartment::new().unwrap();
    const LOST: &str = "Phone/Recovered/old.jpg";
    const STABLE: &str = "Phone/Current/stable.mov";
    for (legacy, bytes, short, expected_reads, expected) in [
        (true, b"returned" as &[u8], false, 2, true),
        (false, b"returned", false, 2, true),
        (true, b"changed size", false, 0, false),
        (true, b"returned", true, 1, false),
    ] {
        let temp = Sandbox::new();
        let dest = temp.dir("library");
        let opened = Arc::new(Mutex::new(Vec::new()));
        let rows = |content: &'static [u8], fault: bool| {
            vec![
                row("Phone", Kind::Storage),
                row("Phone/Recovered", Kind::FolderWithStorageAlias),
                row(LOST, Kind::File(content, SizeHint::Object)),
                row("Phone/Current", Kind::Folder),
                row(STABLE, Kind::File(b"saved", SizeHint::Object)),
                row("Phone/Unavailable", if fault { Kind::ReadFaultFolder } else { Kind::Folder }),
            ]
        };
        let prior = phone(1, rows(b"returned", false), None, opened.clone());
        {
            let mut archive = Archive::open(&dest).unwrap();
            inbox::recover(&archive).unwrap();
            archive.db.execute("INSERT INTO phone_runs(source,started,sessions) VALUES(?,1000,5)",[prior.identity()]).unwrap();
            let mut queue = PhoneQueue::for_capture(&archive.db, prior.identity()).unwrap();
            let mut current = Vec::new();
            prior.visit(&mut |event| {
                if let ScanEvent::Ready(mut item) = event {
                    if item.relative.ends_with("old.jpg") {
                        if legacy { item.version = None; }
                        queue.discover(&archive.db, &item)?;
                        queue.attempting(&archive.db, &item, None)?;
                    } else {
                        archive.import(&prior, &item, |_| Ok(()))?;
                        current.push(inventory::Member::new(item));
                    }
                }
                Ok(())
            }).unwrap();
            // The last complete inventory saw the movie but no longer offered the old photo.
            inventory::Receipts::load(&archive.db, prior.identity()).unwrap().match_members(&mut current);
            inventory::publish(&archive.db, 1, &current, &mut Default::default()).unwrap();
        }
        opened.lock().unwrap().clear();
        let receive = |session, fault| capture::receive(&dest,"iPhone", |_| Ok(1),
            || Ok(Box::new(phone(session, rows(bytes, fault), short.then_some(LOST), opened.clone()))),
            |_| {}, || Ok(()), || Ok(()));
        let partial = receive(6, true);
        assert!(partial.safe_at.is_none());
        let reads = opened.lock().unwrap().iter().filter(|p| p.as_str() == LOST).count();
        assert_eq!(reads, expected_reads, "legacy={legacy}, short={short}, issues={:?}", partial.issues);
        let archive = Archive::open(&dest).unwrap();
        let hash = blake3::hash(b"returned").to_hex().to_string();
        let received = archive.db.query_row("SELECT COUNT(*) FROM phone_inbox WHERE digest=?", [&hash], |r| r.get::<_,u64>(0)).unwrap();
        assert_eq!(received, u64::from(expected));
        let pending = archive.db.query_row("SELECT COUNT(*) FROM phone_pending WHERE signature LIKE '8/%'", [], |r| r.get::<_,u64>(0)).unwrap();
        assert_eq!(pending, u64::from(!expected));
        drop(archive);
        if expected {
            let complete = receive(7, false);
            assert!(complete.safe_at.is_some(), "{:?}", complete.issues);
            assert_eq!(complete.added, 0);
        }
    }
}

#[test]
fn a_metadata_failure_in_another_folder_does_not_starve_old_versions_with_changed_ids() {
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let opened = Arc::new(Mutex::new(Vec::new()));
    let same = "Phone/Healthy/Photo.JPG";
    let changed = "Phone/Healthy/Edit.JPG";
    let fresh = "Phone/Healthy/New.MOV";
    let initial = phone(
        1,
        vec![
            row("Phone", Kind::Storage),
            row("Phone/Healthy", Kind::Folder),
            row(same, Kind::File(b"original", SizeHint::Object)),
            row(changed, Kind::File(b"old edit", SizeHint::Object)),
        ],
        None,
        opened.clone(),
    );
    {
        let mut archive = crate::ingest::archive::Archive::open(&dest).unwrap();
        initial.visit(&mut |event| {
            if let ScanEvent::Ready(mut item) = event {
                item.version = Some(format!("previous/{}", item.id));
                archive.import(&initial, &item, |_| Ok(()))?;
            }
            Ok(())
        }).unwrap();
    }
    let attempt = |number, rows| crate::ingest::capture::receive(
        &dest, "iPhone", |_| Ok(4),
        || Ok(Box::new(phone(number, rows, None, opened.clone()))),
        |_| {}, || Ok(()), || Ok(()),
    );
    let fault = || row("Phone/Unavailable", Kind::ReadFaultFolder);
    let rows = || vec![
        row("Phone", Kind::Storage), fault(), row("Phone/Healthy", Kind::Folder),
        row(same, Kind::File(b"original", SizeHint::Object)),
        row(changed, Kind::File(b"new edit", SizeHint::Object)),
        row(fresh, Kind::File(b"new video", SizeHint::Object)),
    ];
    assert!(attempt(2, vec![row("Phone", Kind::Storage), fault()]).safe_at.is_none());
    opened.lock().unwrap().clear();
    let progress = attempt(3, rows());
    assert!(progress.safe_at.is_none());
    assert_eq!((progress.added, progress.skipped, progress.compared_bytes), (2, 1, 8));
    let seen = opened.lock().unwrap().clone();
    assert_eq!(seen.first().map(String::as_str), Some(fresh));
    assert_eq!(seen.len(), 3);
    opened.lock().unwrap().clear();
    assert!(attempt(4, rows()).safe_at.is_none());
    assert!(opened.lock().unwrap().is_empty(), "the saved new versions must survive the failed listing");
}

#[test]
fn resuming_an_unlistable_folder_keeps_new_originals_seen_before_the_bad_object() {
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let opened = Arc::new(Mutex::new(Vec::new()));
    let wanted = "Phone/Month/new.MOV";
    let rows = |fault| vec![
        row("Phone", Kind::Storage),
        row("Phone/Month", Kind::Folder),
        row(wanted, Kind::File(b"video", SizeHint::Object)),
        row("Phone/Month/last.JPG", if fault { Kind::ReadFaultMetadata }
            else { Kind::File(b"photo", SizeHint::Object) }),
    ];
    let run = |n, rows| crate::ingest::capture::receive(
        &dest, "iPhone", |_| Ok(2), || Ok(Box::new(phone(n, rows, None, opened.clone()))),
        |_| {}, || Ok(()), || Ok(()),
    );
    assert!(run(1, vec![row("Phone", Kind::Storage)]).safe_at.is_none());
    assert!(run(2, rows(true)).safe_at.is_none());
    let partial = run(3, rows(true));
    assert!(partial.safe_at.is_none());
    assert_eq!(partial.added, 1);
    assert_eq!(partial.listed_items, 0, "a partial directory must not count as fully listed");
    assert_eq!(&*opened.lock().unwrap(), &[wanted]);
    let complete = run(4, rows(false));
    assert!(complete.safe_at.is_some(), "{:?}", complete.issues);
    assert_eq!((complete.added, complete.skipped), (1, 1));
    assert_eq!(opened.lock().unwrap().len(), 2);
}

#[test]
fn unreadable_folder_reports_the_failed_operation_and_preserves_the_driver_error() {
    let _apt = Apartment::new().unwrap();
    let device = phone(
        1,
        vec![row("Phone", Kind::Storage), row("Phone/Unavailable", Kind::ReadFaultFolder)],
        None,
        Arc::new(Mutex::new(Vec::new())),
    );
    let error = device.visit(&mut |_| Ok(())).expect_err("folder enumeration failed");
    assert!(matches!(interruption(error.as_ref()), Some(crate::ingest::TransferStop::SessionInterrupted)));
    assert!(error.to_string().contains("枚举目录"), "{error}");
    assert!(error.to_string().contains("Unavailable"), "{error}");
}

#[test]
fn visible_new_original_is_received_before_an_unrelated_directory_read_fails() {
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let archive = temp.dir("archive");
    let opened = Arc::new(Mutex::new(Vec::new()));
    let run = |n,rows| crate::ingest::capture::receive(&archive,"iPhone",|_| Ok(2),
        || Ok(Box::new(phone(n,rows,None,opened.clone()))), |_|{}, ||Ok(()), ||Ok(()));
    let empty = run(1,vec![row("Phone",Kind::Storage)]);
    assert!(empty.safe_at.is_none());
    let new = "Phone/Healthy/Trip.MOV";
    let retry = run(2,vec![row("Phone",Kind::Storage),row("Phone/Bad",Kind::ReadFaultFolder),
        row("Phone/Healthy",Kind::Folder),row(new,Kind::File(b"new media",SizeHint::Object))]);
    assert!(retry.safe_at.is_none());
    assert_eq!(retry.listed_items,1);
    assert_eq!(retry.added,1,"saving a healthy folder list should also save its new media");
    assert_eq!(&*opened.lock().unwrap(), &[new]);
}

#[test]
fn a_failing_directory_rotates_behind_healthy_folders_and_preserves_their_inventory_and_files() {
    let _apt=Apartment::new().unwrap();
    let t=Sandbox::new();
    let archive=t.dir("archive");
    let opened=Arc::new(Mutex::new(Vec::new()));
    let good="Phone/Good/new.JPG";
    let recovered="Phone/Bad/b.MOV";
    let attempt=|n,broken|{
        let rows=vec![
            row("Phone",Kind::Storage),row("Phone/Good",Kind::Folder),
            row("Phone/Bad",if broken {Kind::ReadFaultFolder}else{Kind::Folder}),
            row(good,Kind::File(b"photo",SizeHint::Object)),
            row(recovered,Kind::File(b"video",SizeHint::Object)),
        ];
        crate::ingest::capture::receive(&archive,"iPhone",|_|Ok(2),
            ||Ok(Box::new(phone(n,rows,None,opened.clone()))),|_|{},||Ok(()),||Ok(()))
    };
    let first=attempt(1,true);
    assert!(first.safe_at.is_none());
    assert!(opened.lock().unwrap().is_empty());
    let retry=attempt(2,true);
    assert!(retry.safe_at.is_none());
    assert_eq!(retry.list_total,None);
    assert_eq!((retry.listed_items,retry.added),(1,1));
    assert_eq!(&*opened.lock().unwrap(),&[good]);
    let last=attempt(3,false);
    assert!(last.safe_at.is_some(),"{:?}",last.issues);
    assert_eq!((last.list_total,last.list_covered,last.added),(Some(2),2,1));
    assert_eq!(&*opened.lock().unwrap(),&[good,recovered]);
}

#[test]
fn capture_delays_a_failed_video_and_finishes_from_verified_receipts() {
    let _apartment = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let archive = temp.dir("originals");
    let opened = Arc::new(Mutex::new(vec![]));
    let motion = "Internal Storage/A.MOV";
    let still = "Internal Storage/B.JPG";
    let rows = vec![
        row("Internal Storage", Kind::Storage),
        row(motion, Kind::File(b"video original", SizeHint::Object)),
        row(still, Kind::File(b"new photo", SizeHint::Object)),
    ];
    let run = |n, short| {
        crate::ingest::capture::receive(
            &archive,
            "iPhone",
            |_| Ok(2),
            || Ok(Box::new(phone(n, rows.clone(), short, opened.clone()))),
            |_| {},
            || Ok(()),
            || Ok(()),
        )
    };
    let failed = run(1, Some(motion));
    assert!(failed.safe_at.is_none());
    opened.lock().unwrap().clear();
    let retry = run(2, Some(motion));
    assert!(retry.safe_at.is_none());
    // There are no two known originals yet to check that this recovered session is aligned.
    assert_eq!(&*opened.lock().unwrap(), &[still, still, motion]);
    assert!(originals(&archive).is_empty());
    opened.lock().unwrap().clear();
    let complete = run(3, None);
    assert!(complete.safe_at.is_some(), "{:?}", complete.issues);
    assert_eq!(complete.added, 1);
    assert_eq!(complete.skipped, 1);
    assert_eq!(complete.failed, 0);
    for error in [
        MTP_INCOMPLETE_TRANSFER,
        windows::Win32::Foundation::ERROR_READ_FAULT.to_hresult(),
    ] {
        let stream = io::Error::other(windows_core::Error::from(error));
        assert!(matches!(interruption(&stream), Some(crate::ingest::TransferStop::SessionInterrupted)));
    }
}

fn run(dest: &Path, endpoint: Phone) -> crate::ingest::Report {
    transfer(dest, "WPD", || Ok(Box::new(endpoint)), |_| {})
}

#[test]
fn capture_recovers_other_directories_while_one_folder_keeps_failing() {
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let opened = Arc::new(Mutex::new(Vec::new()));
    let good = "Phone/Healthy/Trip.JPG";
    let first = "Phone/Stalled/First.JPG";
    let second = "Phone/Stalled/Second.JPG";
    let rows = vec![
        row("Phone", Kind::Storage),
        row("Phone/Healthy", Kind::Folder),
        row("Phone/Stalled", Kind::Folder),
        row(first, Kind::File(b"first original", SizeHint::Object)),
        row(second, Kind::File(b"second original", SizeHint::Object)),
        row(good, Kind::File(b"healthy original", SizeHint::Object)),
    ];
    let run = |n, short| {
        crate::ingest::capture::receive(
            &dest,
            "iPhone",
            |_| Ok(3),
            || Ok(Box::new(phone(n, rows.clone(), short, opened.clone()))),
            |_| {},
            || Ok(()),
            || Ok(()),
        )
    };
    assert!(run(1, Some(first)).safe_at.is_none());
    assert_eq!(&*opened.lock().unwrap(), &[first]);
    opened.lock().unwrap().clear();
    let next = run(2, Some(second));
    assert_eq!(&*opened.lock().unwrap(), &[good, good, second]);
    assert_eq!(next.added, 1);
    assert!(next.safe_at.is_none());
    let final_pass = run(3, None);
    assert!(final_pass.safe_at.is_some(), "{:?}", final_pass.issues);
    assert_eq!((final_pass.added, final_pass.skipped), (2, 1));
}

#[test]
fn wpd_large_directory_imports_photos_and_standalone_videos_before_later_metadata() {
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let still = "Phone/DCIM/IMG_0001.HEIC";
    let motion = "Phone/DCIM/IMG_0001.MOV";
    let video = "Phone/DCIM/VIDEO.MP4";
    let sentinel = "Phone/DCIM/last.txt";
    let mut rows = vec![
        row("Phone", Kind::Storage),
        row("Phone/DCIM", Kind::Folder),
        row(motion, Kind::File(b"live payload", SizeHint::Object)),
        row(still, Kind::File(b"the original still", SizeHint::Object)),
        row(video, Kind::File(b"standalone video", SizeHint::Object)),
    ];
    for n in 0..64 {
        rows.push(row(
            &format!("Phone/DCIM/{n}.txt"),
            Kind::File(b"notes", SizeHint::Object),
        ));
    }
    rows.push(row(sentinel, Kind::File(b"last object", SizeHint::Object)));
    let opened = Arc::new(Mutex::new(Vec::new()));
    let trace = Arc::new(Mutex::new(Vec::new()));
    let report = run(
        &dest,
        traced_phone(1, rows, None, opened.clone(), Some(trace.clone())),
    );
    assert_eq!((report.added, report.failed), (3, 0), "{:?}", report.issues);
    assert_eq!((report.scanned, report.discovered), (70, 3));
    let opened_at_last_metadata = trace
        .lock()
        .unwrap()
        .iter()
        .find(|(path, _)| path == sentinel)
        .unwrap()
        .1;
    assert_eq!(
        opened_at_last_metadata, 2,
        "the still and standalone video should already be transferred"
    );
    assert_eq!(&*opened.lock().unwrap(), &[still, video, motion]);
}

#[test]
fn android_wpd_reads_paginated_dcim_downloads_and_pictures_and_deduplicates_local_copy() {
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let (local, dest) = (temp.dir("local"), temp.dir("archive"));
    let camera_copy = b"camera photo from the second enumeration page";
    fs::write(local.join("renamed.jpg"), camera_copy).unwrap();
    let preload = transfer(
        &dest,
        "local",
        || Ok(Box::new(Folder::new(&local, &dest)?)),
        |_| {},
    );
    assert_eq!((preload.added, preload.failed), (1, 0));

    let mut rows = vec![
        row("内部存储", Kind::Storage),
        row("内部存储/DCIM", Kind::Folder),
        row("内部存储/DCIM/Camera", Kind::Folder),
        row("内部存储/Pictures", Kind::Folder),
        row("内部存储/Download", Kind::Folder),
        row("Device services", Kind::Service),
        row(
            "Device services/ignored.jpg",
            Kind::File(b"service image", SizeHint::Object),
        ),
        row(
            "内部存储/Pictures/海边.png",
            Kind::File(b"saved photo", SizeHint::Stream),
        ),
        row(
            "内部存储/Download/web.webp",
            Kind::File(b"other photo", SizeHint::Resource),
        ),
    ];
    for i in 0..32 {
        rows.push(row(
            &format!("内部存储/DCIM/Camera/{i}.txt"),
            Kind::File(b"text", SizeHint::Object),
        ));
    }
    let last_photo = "内部存储/DCIM/Camera/IMG.jpg";
    rows.push(row(last_photo, Kind::File(camera_copy, SizeHint::Object)));
    let opened = Arc::new(Mutex::new(Vec::new()));
    let device = phone(1, rows, None, opened.clone());
    let mut listed = vec![];
    device
        .visit(&mut |event| {
            if let ScanEvent::Ready(item) = event {
                listed.push(item);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(listed.len(), 3);
    let by_name = |name: &str| {
        listed
            .iter()
            .find(|item| item.relative.ends_with(name))
            .unwrap()
    };
    assert_eq!(by_name("web.webp").size, Some(11));
    assert_eq!(by_name("海边.png").size, None);
    assert_eq!(by_name("IMG.jpg").size, Some(camera_copy.len() as u64));

    let result = run(&dest, device);
    assert_eq!(
        (result.added, result.skipped, result.failed),
        (2, 1, 0),
        "{:?}",
        result.issues
    );
    let requests = opened.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().any(|p| p == last_photo));
    let mut bytes: Vec<_> = originals(&dest)
        .into_iter()
        .map(|p| fs::read(p).unwrap())
        .collect();
    bytes.sort();
    assert_eq!(
        bytes,
        [camera_copy.as_slice(), b"other photo", b"saved photo"]
    );
}

#[test]
fn iphone_wpd_recovers_a_short_motion_stream_after_object_ids_change() {
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let opened = Arc::new(Mutex::new(Vec::new()));
    let locked = run(&dest, phone(1, vec![], None, opened.clone()));
    assert_eq!(locked.failed, 1);
    assert!(locked.issues[0].contains("解锁"));

    let still = "Internal Storage/DCIM/100APPLE/IMG_0042.HEIC";
    let motion = "Internal Storage/DCIM/100APPLE/IMG_0042.MOV";
    let rows = vec![
        row("Internal Storage", Kind::Storage),
        row("Internal Storage/DCIM", Kind::Folder),
        row("Internal Storage/DCIM/100APPLE", Kind::Folder),
        row(still, Kind::File(b"paired still", SizeHint::Object)),
        row(
            motion,
            Kind::File(b"complete motion payload", SizeHint::Resource),
        ),
    ];
    let first = run(&dest, phone(2, rows.clone(), Some(motion), opened.clone()));
    assert_eq!(
        (first.added, first.skipped, first.failed),
        (1, 0, 1),
        "{:?}",
        first.issues
    );
    assert_eq!(originals(&dest).len(), 1);
    assert_eq!(
        fs::read_dir(dest.join(".time-cut/staging"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(&*opened.lock().unwrap(), &[still, motion]);
    opened.lock().unwrap().clear();
    let second = run(&dest, phone(48, rows.clone(), None, opened.clone()));
    assert_eq!(
        (second.added, second.skipped, second.failed),
        (1, 1, 0),
        "{:?}",
        second.issues
    );
    assert_eq!(&*opened.lock().unwrap(), &[motion]);
    let files = originals(&dest);
    assert_eq!(files.len(), 2);
    let photo = files
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "HEIC"))
        .unwrap();
    assert_eq!(fs::read(photo).unwrap(), b"paired still");
    assert_eq!(
        fs::read(photo.with_extension("MOV")).unwrap(),
        b"complete motion payload"
    );
    opened.lock().unwrap().clear();
    let repeat = run(&dest, phone(99, rows, None, opened.clone()));
    assert_eq!((repeat.added, repeat.skipped, repeat.failed), (0, 2, 0));
    assert!(opened.lock().unwrap().is_empty());
}

#[test]
fn incomplete_device_properties_cannot_produce_a_complete_inventory() {
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let rows = vec![
        row("Phone", Kind::Storage),
        row("Phone/unreadable", Kind::MissingType),
        row("Phone/valid.jpg", Kind::File(b"readable", SizeHint::Object)),
    ];
    let result = run(
        &temp.dir("archive"),
        phone(1, rows, None, Default::default()),
    );
    assert_eq!((result.added, result.failed), (1, 1));
    assert!(result.inventory.is_none());
}

#[test]
fn wpd_discards_a_short_unknown_length_file_when_the_object_disappears_at_eof() {
    let _apt = Apartment::new().unwrap();
    let temp = Sandbox::new();
    let dest = temp.dir("archive");
    let name = "Phone/IMG.jpg";
    let contents = b"original photos must survive reconnection";
    let rows = vec![
        row("Phone", Kind::Storage),
        row(name, Kind::File(contents, SizeHint::Stream)),
    ];
    let opened = Arc::new(Mutex::new(Vec::new()));
    let interrupted = run(&dest, phone(1, rows.clone(), Some(name), opened.clone()));
    assert_eq!(
        (interrupted.added, interrupted.skipped, interrupted.failed),
        (0, 0, 1)
    );
    assert!(originals(&dest).is_empty());
    assert_eq!(
        fs::read_dir(dest.join(".time-cut/staging"))
            .unwrap()
            .count(),
        0
    );
    let recovered = run(&dest, phone(2, rows, None, opened));
    assert_eq!(
        (recovered.added, recovered.skipped, recovered.failed),
        (1, 0, 0)
    );
    let files = originals(&dest);
    assert_eq!(files.len(), 1);
    assert_eq!(fs::read(&files[0]).unwrap(), contents);
}

#[test]
fn windows_registers_and_unregisters_the_wpd_arrival_listener() {
    let (sender, _receiver) = std::sync::mpsc::channel();
    for _ in 0..2 {
        drop(
            Notifications::register(sender.clone(), Arc::new(AtomicBool::new(false)))
                .expect("register WPD notification"),
        );
    }
}
