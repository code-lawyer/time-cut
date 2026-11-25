//! Windows supplies the MTP and Apple camera drivers. All COM objects stay on
//! the worker thread that opened them; only strings cross back into the UI.
mod bulk;
mod resolve;
#[cfg(test)]
mod protocol_tests;

use super::archive::{
    CopyOrder, IncompleteTransfer, Item, Result, ScanEvent, Source, may_have_owner, media, ordered,
};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use windows::{
    Win32::{
        Devices::PortableDevices::*,
        Foundation::{ERROR_OPERATION_ABORTED, GENERIC_READ, PROPERTYKEY, S_FALSE},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree, CoUninitialize, IStream, STATFLAG_NONAME, STATSTG,
                StructuredStorage::{PropVariantClear, PropVariantToFileTime},
            },
            Variant::PSTF_UTC,
        },
    },
    core::{HRESULT, HSTRING, PWSTR},
};

// MTP response 0x2003, wrapped in a Windows HRESULT.
const MTP_SESSION_NOT_OPEN: HRESULT = HRESULT(0x80042003_u32 as i32);
const MTP_INCOMPLETE_TRANSFER: HRESULT = HRESULT(0x80042007_u32 as i32);

#[derive(Debug)]
struct DeviceReadError {
    operation: &'static str,
    path: PathBuf,
    cause: Box<dyn std::error::Error + Send + Sync>,
}

impl std::fmt::Display for DeviceReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.path.to_string_lossy();
        let location = if path.is_empty() { "设备根目录" } else { &path };
        write!(f, "{}「{}」失败，{}", self.operation, location, self.cause)
    }
}

impl std::error::Error for DeviceReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

fn read_error(
    operation: &'static str,
    path: &Path,
    cause: Box<dyn std::error::Error + Send + Sync>,
) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(DeviceReadError { operation, path: path.to_owned(), cause })
}

pub(super) fn interruption(error: &(dyn std::error::Error + 'static)) -> Option<super::TransferStop> {
    // A cancelled GetObject can leave the following requests one payload behind.
    // A later same-size object could otherwise receive different content undetected.
    if error.is::<IncompleteTransfer>() || error.is::<super::inbox::WrongObject>()
        || error.is::<super::inbox::observed::ReturnedData>() {
        return Some(super::TransferStop::ContentMismatch);
    }
    if let Some(error) = error.downcast_ref::<windows::core::Error>() {
        let interrupted = matches!(
            error.code(),
            E_WPD_DEVICE_IS_HUNG
                | E_WPD_DEVICE_NOT_OPEN
                | MTP_SESSION_NOT_OPEN
                | MTP_INCOMPLETE_TRANSFER
        ) || [
            ERROR_OPERATION_ABORTED,
            windows::Win32::Foundation::ERROR_READ_FAULT,
            windows::Win32::Foundation::ERROR_DEVICE_NOT_CONNECTED,
            windows::Win32::Foundation::ERROR_IO_DEVICE,
        ]
        .iter()
        .any(|code| error.code() == code.to_hresult());
        return interrupted.then_some(super::TransferStop::SessionInterrupted);
    }
    if let Some(inner) = error
        .downcast_ref::<io::Error>()
        .and_then(io::Error::get_ref)
    {
        return interruption(inner);
    }
    error.source().and_then(interruption)
}

pub struct Apartment;
impl Apartment {
    pub fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

use windows::Win32::Devices::DeviceAndDriverInstallation as pnp;

pub(super) struct Notifications {
    handle: pnp::HCMNOTIFICATION,
    _context: Box<NotificationContext>,
}

struct NotificationContext {
    signal: mpsc::Sender<super::Wake>,
    removed: Arc<AtomicBool>,
}
impl Notifications {
    pub(super) fn register(
        signal: mpsc::Sender<super::Wake>,
        removed: Arc<AtomicBool>,
    ) -> Option<Self> {
        unsafe extern "system" fn changed(
            _: pnp::HCMNOTIFICATION,
            context: *const std::ffi::c_void,
            action: pnp::CM_NOTIFY_ACTION,
            _: *const pnp::CM_NOTIFY_EVENT_DATA,
            _: u32,
        ) -> u32 {
            // CM_Unregister_Notification waits for running callbacks before the box drops.
            let context = unsafe { &*context.cast::<NotificationContext>() };
            if action == pnp::CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL {
                context.removed.store(true, Ordering::Release);
            }
            let _ = context.signal.send(super::Wake::Retry);
            0
        }
        let context = Box::new(NotificationContext { signal, removed });
        let filter = pnp::CM_NOTIFY_FILTER {
            cbSize: std::mem::size_of::<pnp::CM_NOTIFY_FILTER>() as u32,
            FilterType: pnp::CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
            u: pnp::CM_NOTIFY_FILTER_0 {
                DeviceInterface: pnp::CM_NOTIFY_FILTER_0_0 {
                    ClassGuid: GUID_DEVINTERFACE_WPD,
                },
            },
            ..Default::default()
        };
        let mut handle = pnp::HCMNOTIFICATION::default();
        let result = unsafe {
            pnp::CM_Register_Notification(
                &filter,
                Some(std::ptr::from_ref(&*context).cast()),
                Some(changed),
                &mut handle,
            )
        };
        (result == pnp::CR_SUCCESS).then(|| Self {
            handle,
            _context: context,
        })
    }
}
impl Drop for Notifications {
    fn drop(&mut self) {
        unsafe {
            let _ = pnp::CM_Unregister_Notification(self.handle);
        }
    }
}

// WPD allocates each returned string with CoTaskMemAlloc, including object IDs.
fn take_string(raw: PWSTR) -> String {
    if raw.is_null() {
        return String::new();
    }
    unsafe {
        let text = String::from_utf16_lossy(raw.as_wide());
        CoTaskMemFree(Some(raw.0.cast()));
        text
    }
}

#[derive(Clone, Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
}

impl Device {
    pub(super) fn is_apple(&self) -> bool {
        self.id.to_ascii_lowercase().contains("vid_05ac")
            || self.name.to_ascii_lowercase().contains("iphone")
    }
}

pub fn devices() -> Result<Vec<Device>> {
    unsafe {
        let manager: IPortableDeviceManager =
            CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)?;
        manager.RefreshDeviceList()?;
        let mut count = 0;
        manager.GetDevices(std::ptr::null_mut(), &mut count)?;
        if count == 0 {
            return Ok(vec![]);
        }
        let mut ids = vec![PWSTR::null(); count as usize];
        let status = manager.GetDevices(ids.as_mut_ptr(), &mut count);
        let ids: Vec<String> = ids
            .into_iter()
            .map(take_string)
            .filter(|s| !s.is_empty())
            .collect();
        status?;
        let mut found = vec![];
        for id in ids {
            let pnp = id.to_ascii_lowercase();
            // Fixed drives and SD readers can also register as WPD devices.
            if pnp.contains("usbstor") || pnp.contains("storage#volume") || pnp.contains("bthenum")
            {
                continue;
            }
            let device_id = HSTRING::from(&id);
            let mut buf = [0u16; 512];
            let mut length = buf.len() as u32;
            let name = if manager
                .GetDeviceFriendlyName(&device_id, PWSTR(buf.as_mut_ptr()), &mut length)
                .is_ok()
            {
                String::from_utf16_lossy(
                    &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())],
                )
            } else {
                "便携设备".to_string()
            };
            found.push(Device { id, name });
        }
        Ok(found)
    }
}

fn text_value(values: &IPortableDeviceValues, key: &PROPERTYKEY) -> Option<String> {
    unsafe {
        values
            .GetStringValue(key)
            .ok()
            .map(take_string)
            .filter(|s| !s.is_empty())
    }
}

fn date(values: &IPortableDeviceValues, key: &PROPERTYKEY) -> Option<SystemTime> {
    unsafe {
        let mut property = values.GetValue(key).ok()?;
        let converted = PropVariantToFileTime(&property, PSTF_UTC);
        let _ = PropVariantClear(&mut property);
        let ft = converted.ok()?;
        let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
        let unix_ticks = ticks.checked_sub(116444736000000000)?;
        UNIX_EPOCH.checked_add(Duration::new(
            unix_ticks / 10000000,
            ((unix_ticks % 10000000) * 100) as u32,
        ))
    }
}

fn keys(list: &[PROPERTYKEY]) -> Result<IPortableDeviceKeyCollection> {
    unsafe {
        let keys: IPortableDeviceKeyCollection =
            CoCreateInstance(&PortableDeviceKeyCollection, None, CLSCTX_INPROC_SERVER)?;
        for key in list {
            keys.Add(key)?;
        }
        Ok(keys)
    }
}

pub struct Phone {
    device: IPortableDevice,
    content: IPortableDeviceContent,
    props: IPortableDeviceProperties,
    transfer: IPortableDeviceResources,
    fields: IPortableDeviceKeyCollection,
    identity: String,
    album_scope: AlbumScope,
    storage_roots: HashSet<PathBuf>,
    hidden_folders: std::cell::RefCell<HashSet<PathBuf>>,
    folders: std::cell::RefCell<resolve::Folders>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AlbumScope {
    Apple,
    Shared,
    #[cfg(test)]
    Unrestricted,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirectoryLevel {
    Device,
    Storage,
    Album,
}

impl AlbumScope {
    fn is_album_root(self, name: &str) -> bool {
        name.eq_ignore_ascii_case("DCIM")
            || (self == Self::Shared
                && (name.eq_ignore_ascii_case("Pictures") || name.eq_ignore_ascii_case("Movies")))
    }

    fn child(self, parent: DirectoryLevel, path: &Path, storage: bool) -> Option<DirectoryLevel> {
        #[cfg(test)]
        if self == Self::Unrestricted {
            return Some(DirectoryLevel::Album);
        }
        let name = path.file_name()?.to_str()?;
        if name.starts_with('.') {
            return None;
        }
        match parent {
            DirectoryLevel::Device if storage => Some(DirectoryLevel::Storage),
            DirectoryLevel::Device | DirectoryLevel::Storage if self.is_album_root(name) => {
                Some(DirectoryLevel::Album)
            }
            DirectoryLevel::Album if !storage => Some(DirectoryLevel::Album),
            _ => None,
        }
    }

    fn includes_media(self, parent: DirectoryLevel) -> bool {
        #[cfg(test)]
        if self == Self::Unrestricted {
            return true;
        }
        parent == DirectoryLevel::Album
    }

    fn includes_file(self, parent: DirectoryLevel, path: &Path) -> bool {
        #[cfg(test)]
        if self == Self::Unrestricted {
            return true;
        }
        self.includes_media(parent)
            && path.file_name().and_then(|name| name.to_str()).is_some_and(|name| !name.starts_with('.'))
    }
}

impl Drop for Phone {
    fn drop(&mut self) {
        // Release the protocol session before the next automatic attempt opens one.
        let _ = unsafe { self.device.Close() };
    }
}

enum Object {
    Directory { relative: PathBuf, storage: bool },
    Media(Item),
    NoMedia,
    UnnamedFile,
    Other,
}

impl Phone {
    pub fn open(info: &Device) -> Result<Self> {
        unsafe {
            let device: IPortableDevice =
                CoCreateInstance(&PortableDeviceFTM, None, CLSCTX_INPROC_SERVER)?;
            let client: IPortableDeviceValues =
                CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)?;
            client.SetStringValue(&WPD_CLIENT_NAME, &HSTRING::from("Time-cut"))?;
            client.SetUnsignedIntegerValue(&WPD_CLIENT_MAJOR_VERSION, 1)?;
            client.SetUnsignedIntegerValue(&WPD_CLIENT_MINOR_VERSION, 0)?;
            client.SetUnsignedIntegerValue(&WPD_CLIENT_REVISION, 0)?;
            client.SetUnsignedIntegerValue(&WPD_CLIENT_DESIRED_ACCESS, GENERIC_READ.0)?;
            device.Open(&HSTRING::from(&info.id), &client)?;
            let content = device.Content()?;
            let props = content.Properties()?;
            let transfer = content.Transfer()?;
            let root = props.GetValues(
                WPD_DEVICE_OBJECT_ID,
                &keys(&[WPD_DEVICE_SERIAL_NUMBER, WPD_DEVICE_TRANSPORT])?,
            )?;
            if let Ok(transport) = root.GetUnsignedIntegerValue(&WPD_DEVICE_TRANSPORT) {
                if transport == WPD_DEVICE_TRANSPORT_IP.0 as u32
                    || transport == WPD_DEVICE_TRANSPORT_BLUETOOTH.0 as u32
                {
                    return Err("该设备使用无线连接".into());
                }
            }
            let identity = format!(
                "wpd:{}",
                text_value(&root, &WPD_DEVICE_SERIAL_NUMBER).unwrap_or_else(|| info.id.clone())
            );
            let fields = keys(&[
                WPD_OBJECT_ID,
                WPD_OBJECT_PARENT_ID,
                WPD_OBJECT_CONTENT_TYPE,
                WPD_FUNCTIONAL_OBJECT_CATEGORY,
                WPD_OBJECT_NAME,
                WPD_OBJECT_ORIGINAL_FILE_NAME,
                WPD_OBJECT_SIZE,
                WPD_OBJECT_PERSISTENT_UNIQUE_ID,
                WPD_OBJECT_DATE_MODIFIED,
                WPD_OBJECT_DATE_CREATED,
                WPD_OBJECT_DATE_AUTHORED,
            ])?;
            let mut phone = Self {
                device,
                content,
                props,
                transfer,
                fields,
                identity,
                album_scope: if info.is_apple() { AlbumScope::Apple } else { AlbumScope::Shared },
                storage_roots: HashSet::new(),
                hidden_folders: Default::default(),
                folders: Default::default(),
            };
            // Legacy checkpoints predate the scope. Identify actual storage
            // objects so a nested folder named "Pictures" can't impersonate one.
            for id in phone.children("DEVICE")? {
                if let Object::Directory { relative, storage: true } =
                    phone.object_with_media(&id, Path::new(""), false)?
                {
                    phone.storage_roots.insert(relative);
                }
            }
            Ok(phone)
        }
    }

    fn children(&self, parent: &str) -> Result<Vec<String>> {
        unsafe {
            let iter = self.content.EnumObjects(0, &HSTRING::from(parent), None)?;
            let mut children = Vec::new();
            loop {
                let mut raw = [PWSTR::null(); 32];
                let mut count = 0;
                let status = iter.Next(&mut raw, &mut count);
                let returned = raw.iter().filter(|p| !p.is_null()).count();
                children.extend(raw.into_iter().filter(|p| !p.is_null()).map(take_string));
                status.ok()?; // S_FALSE may still return a final, short batch.
                if count as usize != returned {
                    return Err("设备目录枚举返回的项目数不一致".into());
                }
                if status == S_FALSE || count == 0 {
                    break;
                }
            }
            Ok(children)
        }
    }

    fn object(&self, id: &str, parent: &Path) -> Result<Object> {
        self.object_with_media(id, parent, true)
    }

    fn object_with_media(&self, id: &str, parent: &Path, media_in_parent: bool) -> Result<Object> {
        let vals = unsafe { self.props.GetValues(&HSTRING::from(id), &self.fields)? };
        Self::describe(&vals, id, parent, media_in_parent, false, || unsafe {
            self.transfer
                .GetResourceAttributes(&HSTRING::from(id), &WPD_RESOURCE_DEFAULT)
                .ok()?
                .GetUnsignedLargeIntegerValue(&WPD_RESOURCE_ATTRIBUTE_TOTAL_SIZE)
                .ok()
                .filter(|n| *n > 0)
        })
    }

    fn describe(
        vals: &IPortableDeviceValues,
        id: &str,
        parent: &Path,
        media_in_parent: bool,
        check_marker: bool,
        fallback_size: impl FnOnce() -> Option<u64>,
    ) -> Result<Object> {
        let kind = unsafe { vals.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE)? };
        let name = text_value(vals, &WPD_OBJECT_ORIGINAL_FILE_NAME)
            .or_else(|| text_value(vals, &WPD_OBJECT_NAME));
        if media_in_parent && name.is_none()
            && (kind == WPD_CONTENT_TYPE_IMAGE || kind == WPD_CONTENT_TYPE_VIDEO)
        {
            return Err("照片或视频暂未返回文件名".into());
        }
        if check_marker && kind != WPD_CONTENT_TYPE_FOLDER && kind != WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT {
            if name.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(".nomedia")) {
                return Ok(Object::NoMedia);
            }
            if name.is_none() {
                return Ok(Object::UnnamedFile);
            }
        }
        let name = name.unwrap_or_else(|| id.to_owned());
        // A driver supplies one name, never a path.
        let name = name.replace(['/', '\\', ':'], "_");
        if name.is_empty() || name == "." || name == ".." {
            return Ok(Object::Other);
        }
        let relative = parent.join(name);
        if kind == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT
            && unsafe { vals.GetGuidValue(&WPD_FUNCTIONAL_OBJECT_CATEGORY) }
                .is_ok_and(|c| c != WPD_FUNCTIONAL_CATEGORY_STORAGE)
        {
            return Ok(Object::Other);
        }
        if kind == WPD_CONTENT_TYPE_FOLDER || kind == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT {
            return Ok(Object::Directory {
                relative,
                storage: kind == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT,
            });
        }
        if !media_in_parent || !media(&relative) {
            return Ok(Object::Other);
        }
        let size = unsafe {
            vals.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE)
                .ok()
                .filter(|n| *n > 0)
                .or_else(fallback_size)
        };
        let modified = date(&vals, &WPD_OBJECT_DATE_MODIFIED);
        let version = text_value(&vals, &WPD_OBJECT_PERSISTENT_UNIQUE_ID)
            .zip(modified)
            .zip(size)
            .and_then(|((id, time), len)| {
                time.duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| format!("{id}/{len}/{}", d.as_nanos()))
            });
        Ok(Object::Media(Item {
            id: id.to_owned(),
            relative,
            size,
            version,
            modified: modified
                .or_else(|| date(&vals, &WPD_OBJECT_DATE_AUTHORED))
                .or_else(|| date(&vals, &WPD_OBJECT_DATE_CREATED)),
        }))
    }
}

struct WpdReader {
    stream: IStream,
    chunk: usize,
    expected: Option<u64>,
    received: u64,
    eof: bool,
}
impl Read for WpdReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.eof {
            if let Some(expected) = self.expected && self.received < expected {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, IncompleteTransfer {
                    received: self.received, expected: Some(expected),
                }));
            }
            return Ok(0);
        }
        let mut read = 0;
        let size = buf.len().min(self.chunk) as u32;
        let status = unsafe {
            self.stream
                .Read(buf.as_mut_ptr().cast(), size, Some(&mut read))
        };
        status.ok().map_err(io::Error::other)?;
        self.received += u64::from(read);
        self.eof = status == S_FALSE || read == 0;
        if let Some(expected) = self.expected
            && (self.received > expected || (read == 0 && self.received < expected))
        {
            let kind = if self.received < expected {
                io::ErrorKind::UnexpectedEof
            } else {
                io::ErrorKind::InvalidData
            };
            return Err(io::Error::new(
                kind,
                IncompleteTransfer {
                    received: self.received,
                    expected: self.expected,
                },
            ));
        }
        Ok(read as usize)
    }
}

impl Phone {
    // A marker can be the last property returned. Decide visibility before opening
    // any payload in this folder or following any child directory.
    fn album_entries(
        &self,
        path: &Path,
        children: &[String],
        include_media: bool,
        bulk: &mut Option<IPortableDevicePropertiesBulk>,
    ) -> Result<Option<HashMap<String, Object>>> {
        let mut unknown = None;
        let mut entries = HashMap::new();
        for batch in children.chunks(128) {
            let mut values = HashMap::new();
            if batch.len() >= 8
                && let Some(api) = bulk.as_ref()
            {
                match bulk::objects(api, &self.fields, path, include_media, true, batch) {
                    Ok(objects) if objects.is_empty() => *bulk = None,
                    Ok(objects) => values = objects,
                    Err(e) if interruption(e.as_ref()).is_some() => return Err(e),
                    Err(_) => *bulk = None,
                }
            }
            for id in batch {
                let object = values.remove(id).map(Ok).unwrap_or_else(|| {
                    let properties =
                        unsafe { self.props.GetValues(&HSTRING::from(id), &self.fields)? };
                    Self::describe(&properties, id, path, include_media, true, || None)
                });
                match object {
                    Ok(Object::NoMedia) => return Ok(None),
                    Ok(Object::UnnamedFile) => {
                        unknown
                            .get_or_insert_with(|| "设备未返回文件名，无法核对相册可见性".into());
                    }
                    Ok(object) => {
                        if include_media {
                            entries.insert(id.clone(), object);
                        }
                    }
                    Err(e) if interruption(e.as_ref()).is_some() => return Err(e),
                    Err(e) => {
                        unknown.get_or_insert(e);
                    }
                }
            }
        }
        if let Some(e) = unknown {
            return Err(e);
        }
        Ok(Some(entries))
    }

    fn has_no_media(
        &self,
        path: &Path,
        children: &[String],
        bulk: &mut Option<IPortableDevicePropertiesBulk>,
    ) -> Result<bool> {
        Ok(self.album_entries(path, children, false, bulk)?.is_none())
    }

    fn walk(
        &self,
        completed: Option<&CopyOrder>,
        emit: &mut dyn FnMut(ScanEvent) -> Result<()>,
    ) -> Result<()> {
        let mut pending = vec![("DEVICE".to_owned(), PathBuf::new(), DirectoryLevel::Device)];
        let mut visited = HashSet::new();
        let mut album_paths = HashMap::new();
        let mut ambiguous_paths = HashSet::new();
        let mut directory_lists = Vec::new();
        let mut retry_last = Vec::new();
        let mut bulk = completed.and_then(|_| {
            windows_core::Interface::cast::<IPortableDevicePropertiesBulk>(&self.props).ok()
        });
        let mut any_storage = false;
        let mut unreadable = 0;
        let mut first_error = None;
        self.hidden_folders.borrow_mut().clear();
        while let Some((parent, path, level)) = pending.pop() {
            if ambiguous_paths.contains(&path) {
                continue;
            }
            if !visited.insert(parent.clone()) {
                continue;
            }
            if completed.is_some() { emit(ScanEvent::DirectoryStart(path.clone()))?; }
            let children = match self.children(&parent) {
                Ok(children) => children,
                Err(e) if path.as_os_str().is_empty() || interruption(e.as_ref()).is_some() => {
                    return Err(read_error("枚举目录", &path, e));
                }
                Err(e) => {
                    unreadable += 1;
                    first_error.get_or_insert_with(|| format!("{}：{e}", path.display()));
                    continue;
                }
            };
            let mut screened = None;
            let visibility = if self.album_scope == AlbumScope::Shared && level == DirectoryLevel::Album {
                match self.album_entries(&path, &children, true, &mut bulk) {
                    Ok(entries) => {
                        let hidden = entries.is_none();
                        screened = entries;
                        Some(hidden)
                    }
                    Err(e) if interruption(e.as_ref()).is_some() => {
                        return Err(read_error("核对相册可见性", &path, e));
                    }
                    Err(e) => {
                        unreadable += 1;
                        first_error.get_or_insert_with(|| format!("{}：{e}", path.display()));
                        continue;
                    }
                }
            } else {
                None
            };
            if completed.is_some() {
                directory_lists.push((
                    parent.clone(),
                    path.clone(),
                    directory_fingerprint(&children),
                    visibility,
                ));
            }
            if visibility == Some(true) {
                self.hidden_folders.borrow_mut().insert(path);
                continue;
            }
            let errors_before = unreadable;
            let mut companions = Vec::new();
            let mut listed = Vec::new();
            let media_in_parent = self.album_scope.includes_media(level);
            for batch in children.chunks(128) {
                let mut values = HashMap::new();
                if screened.is_none()
                    && batch.len() >= 8
                    && let Some(api) = &bulk
                {
                    match bulk::objects(api, &self.fields, &path, media_in_parent, false, batch) {
                        Ok(objects) if objects.is_empty() => bulk = None,
                        Ok(objects) => values = objects,
                        Err(e) if interruption(e.as_ref()).is_some() => return Err(read_error("批量读取目录内的文件信息", &path, e)),
                        Err(_) => bulk = None,
                    }
                }
                for id in batch {
                    let object = match screened.as_mut().and_then(|entries| entries.remove(id))
                        .or_else(|| values.remove(id))
                        .filter(|object| !matches!(object, Object::Media(item) if item.size.is_none()))
                        .map(Ok)
                        .unwrap_or_else(|| self.object_with_media(id, &path, media_in_parent))
                    {
                        Ok(object) => object,
                        Err(e) if interruption(e.as_ref()).is_some() => return Err(read_error("读取目录内的文件信息", &path, e)),
                        Err(e) => {
                            unreadable += 1;
                            first_error.get_or_insert_with(|| format!("{}：{e}", path.display()));
                            Object::Other
                        }
                    };
                    emit(ScanEvent::Inspected {
                        media: matches!(&object, Object::Media(item)
                            if self.album_scope.includes_file(level, &item.relative)),
                    })?;
                    match object {
                        Object::Directory { relative, storage } => {
                            let Some(child) = self.album_scope.child(level, &relative, storage) else {
                                continue;
                            };
                            if self.album_scope == AlbumScope::Shared
                                && album_paths.insert(relative.clone(), id.clone()).is_some_and(|old| old != *id)
                            {
                                ambiguous_paths.insert(relative.clone());
                                unreadable += 1;
                                first_error.get_or_insert_with(|| {
                                    format!("{}：设备返回了同名目录，无法核对可见性", relative.display())
                                });
                                continue;
                            }
                            any_storage = true;
                            if completed.is_some() { emit(ScanEvent::DirectoryFound(relative.clone()))?; }
                            pending.push((id.clone(), relative, child));
                        }
                        Object::Media(item) if self.album_scope.includes_file(level, &item.relative) => {
                            any_storage = true;
                            if completed.is_some() { listed.push(item.clone()); }
                            if let Some(last_attempt) =
                                completed.and_then(|order| order.interrupted.get(&item.relative))
                            {
                                emit(ScanEvent::CompanionFound(item.clone()))?;
                                retry_last.push((*last_attempt, item));
                            } else if completed.is_none() && may_have_owner(&item.relative) {
                                emit(ScanEvent::CompanionFound(item.clone()))?;
                                companions.push(item);
                            } else {
                                emit(ScanEvent::Ready(item))?;
                            }
                        }
                        _ => (),
                    }
                }
            }
            // Any later object in this directory can be the still or original that
            // determines a MOV/AAE/XMP companion's destination. Independent media
            // are transferred as soon as their properties are read.
            companions.sort_by_key(|f| ordered(&f.relative));
            for file in companions {
                emit(ScanEvent::Ready(file))?;
            }
            if let Some(ranks) = completed {
                if unreadable == errors_before {
                    emit(ScanEvent::DirectoryDone {path,media:listed})?;
                }
                // On reconnect, a saved list with missing originals can be used immediately.
                // Unvisited folders follow, then checked lists, then failing directories.
                pending.sort_by_key(|(_, p, _)| {
                    let preferred = ranks.preferred_folder.as_ref().is_some_and(|folder| folder.starts_with(p));
                    std::cmp::Reverse((!preferred, ranks.priorities.get(p).copied().unwrap_or(1), ranks.folders.get(p).copied().unwrap_or(i64::MIN)))
                });
            }
        }
        // A bad camera object must not prevent receipt of unrelated originals.
        retry_last.sort_by_key(|(time, _)| *time);
        for (_, item) in retry_last {
            emit(ScanEvent::Ready(item))?;
        }
        if completed.is_some() && unreadable == 0 && any_storage {
            let total = directory_lists.len() as u64;
            emit(ScanEvent::DirectoryCheck { done: 0, total })?;
            for (index, (id, path, expected, visibility)) in directory_lists.iter().enumerate() {
                let current = self.children(id).map_err(|e| read_error("复核目录", path, e))?;
                if directory_fingerprint(&current) != *expected
                    || (visibility.is_some() && Some(self.has_no_media(path, &current, &mut bulk)?) != *visibility)
                {
                    emit(ScanEvent::Unreadable(
                        "接收期间手机目录发生变化，将自动补查".into(),
                    ))?;
                    break;
                }
                emit(ScanEvent::DirectoryCheck {
                    done: index as u64 + 1,
                    total,
                })?;
            }
        }
        if let Some(first) = first_error {
            // Other objects in this session can still be copied from the deferred queue.
            emit(ScanEvent::Unreadable(format!(
                "{unreadable} 处目录或对象暂不可读。{first}"
            )))?;
            return Ok(());
        }
        if !any_storage {
            return Err("未看到手机存储。请解锁，选择文件传输或信任这台电脑。".into());
        }
        Ok(())
    }
}

fn directory_fingerprint(ids: &[String]) -> blake3::Hash {
    let mut ids: Vec<_> = ids.iter().map(String::as_bytes).collect();
    ids.sort_unstable();
    let mut hash = blake3::Hasher::new();
    for id in ids {
        hash.update(&(id.len() as u64).to_le_bytes());
        hash.update(id);
    }
    hash.finalize()
}

impl Source for Phone {
    fn saved_files(&self) -> Option<&dyn super::archive::SavedFiles> {
        // A saved Android path alone cannot attest visibility. Resume receipts from a
        // fresh folder walk so each set of siblings shares one live marker check.
        (self.album_scope != AlbumScope::Shared).then_some(self)
    }
    fn album_scope_version(&self) -> Option<&'static str> {
        match self.album_scope {
            AlbumScope::Apple => Some("apple-dcim-v1"),
            AlbumScope::Shared => Some("android-albums-v2"),
            #[cfg(test)]
            AlbumScope::Unrestricted => None,
        }
    }
    fn excluded_album_folders(&self) -> HashSet<PathBuf> {
        self.hidden_folders.borrow().clone()
    }
    fn uses_album_markers(&self) -> bool {
        self.album_scope == AlbumScope::Shared
    }

    fn album_path_in_scope(&self, relative: &Path) -> bool {
        #[cfg(test)]
        if self.album_scope == AlbumScope::Unrestricted {
            return true;
        }
        if relative.components().any(|part| !matches!(part, std::path::Component::Normal(_))) {
            return false;
        }
        let mut parts = relative.iter().peekable();
        if parts.peek().is_some_and(|root| self.storage_roots.contains(Path::new(root))) {
            parts.next();
        }
        let Some(root) = parts.next().and_then(|name| name.to_str()) else {
            return false;
        };
        self.album_scope.is_album_root(root)
            && parts.peek().is_some()
            && parts.all(|name| name.to_str().is_some_and(|name| !name.starts_with('.')))
    }
    fn is_phone(&self) -> bool {
        true
    }
    fn identity(&self) -> &str {
        &self.identity
    }
    fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> {
        self.walk(None, emit)
    }
    fn visit_for_copy(
        &self,
        ranks: &CopyOrder,
        emit: &mut dyn FnMut(ScanEvent) -> Result<()>,
    ) -> Result<()> {
        self.walk(Some(ranks), emit)
    }
    fn reader(&self, item: &Item) -> Result<Box<dyn Read>> {
        let mut hint = 0;
        let mut stream = None;
        unsafe {
            self.transfer.GetStream(
                &HSTRING::from(&item.id),
                &WPD_RESOURCE_DEFAULT,
                0,
                &mut hint,
                &mut stream,
            )?;
        }
        let stream = stream.ok_or("设备没有提供原件数据流")?;
        let mut stat = STATSTG::default();
        let expected = item.size.or_else(|| {
            (unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }.is_ok() && stat.cbSize > 0)
                .then_some(stat.cbSize)
        });
        Ok(Box::new(WpdReader {
            stream,
            expected,
            received: 0,
            eof: false,
            chunk: if hint == 0 { 65536 } else { hint as usize },
        }))
    }
    fn verify_read(&self, item: &Item) -> Result<()> {
        if item.size.is_none() {
            // Some camera drivers omit the size. Confirm that the object is still accessible after EOF.
            unsafe {
                self.props
                    .GetValues(&HSTRING::from(&item.id), &self.fields)?;
            }
        }
        Ok(())
    }
    fn confirms_returned_object(&self, item: &Item) -> Result<bool> {
        self.folders.replace(resolve::Folders::default());
        let current = super::archive::SavedFiles::resolve(self, item)?;
        Ok(current.is_some_and(|verified| verified.id == item.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::{
        Foundation::FILETIME, System::Com::StructuredStorage::InitPropVariantFromFileTime,
        UI::Shell::SHCreateMemStream,
    };

    #[test]
    fn windows_wpd_preserves_unicode_object_names_and_filetime() {
        let _apt = Apartment::new().unwrap();
        unsafe {
            let values: IPortableDeviceValues =
                CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER).unwrap();
            values
                .SetStringValue(
                    &WPD_OBJECT_ORIGINAL_FILE_NAME,
                    &HSTRING::from("海边原片.HEIC"),
                )
                .unwrap();
            assert_eq!(
                text_value(&values, &WPD_OBJECT_ORIGINAL_FILE_NAME).as_deref(),
                Some("海边原片.HEIC")
            );
            assert!(text_value(&values, &WPD_OBJECT_PERSISTENT_UNIQUE_ID).is_none());
            let ticks = 116444736000000000u64 + 1_700_000_000u64 * 10_000_000;
            let ft = FILETIME {
                dwLowDateTime: ticks as u32,
                dwHighDateTime: (ticks >> 32) as u32,
            };
            let mut value = InitPropVariantFromFileTime(&ft).unwrap();
            values.SetValue(&WPD_OBJECT_DATE_MODIFIED, &value).unwrap();
            PropVariantClear(&mut value).unwrap();
            assert_eq!(
                date(&values, &WPD_OBJECT_DATE_MODIFIED),
                Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            );
        }
    }

    #[test]
    fn windows_com_stream_rejects_short_reads_and_preserves_a_final_short_block() {
        let _apt = Apartment::new().unwrap();
        let bytes = b"1234567";
        let stream = || unsafe { SHCreateMemStream(Some(bytes)).unwrap() };
        let mut input = WpdReader {
            stream: stream(),
            chunk: 3,
            expected: Some(7),
            received: 0,
            eof: false,
        };
        let mut read = vec![];
        input.read_to_end(&mut read).unwrap();
        assert_eq!(read, bytes);
        let mut truncated = WpdReader {
            stream: stream(),
            chunk: 3,
            expected: Some(20),
            received: 0,
            eof: false,
        };
        let short = truncated.read_to_end(&mut vec![]).unwrap_err();
        assert_eq!(short.kind(), io::ErrorKind::UnexpectedEof);
        assert!(matches!(interruption(&short), Some(crate::ingest::TransferStop::ContentMismatch)));
        let mut oversized = WpdReader {
            stream: stream(),
            chunk: 3,
            expected: Some(4),
            received: 0,
            eof: false,
        };
        let long = oversized.read_to_end(&mut vec![]).unwrap_err();
        assert_eq!(long.kind(), io::ErrorKind::InvalidData);
        assert!(matches!(interruption(&long), Some(crate::ingest::TransferStop::ContentMismatch)));
    }
}
