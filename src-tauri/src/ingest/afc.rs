//! The paired iPhone's original camera files, accessed through Apple's local USB service.
#[cfg(test)]
mod tests;
mod schedule;
pub(super) mod supplement;
pub(super) mod desktop;
use super::archive::{CopyOrder, Item, Result, SavedFiles, ScanEvent, Source, media};
use idevice::{
    IdeviceError, IdeviceService,
    afc::{AfcClient, errors::AfcError, file::OwnedFileDescriptor, opcode::AfcFopenMode},
    lockdown::LockdownClient,
    provider::{IdeviceProvider, UsbmuxdProvider},
    usbmuxd::{Connection as Transport, UsbmuxdAddr, UsbmuxdDevice},
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    future::Future,
    io::{self, Read},
    net::{Ipv4Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    rc::Rc,
    time::{Duration, UNIX_EPOCH},
};
use tokio::runtime::Runtime;

const CAMERA: &str = "DCIM";
pub(super) const CAMERA_SCOPE: &str = "apple-afc-dcim-v1";
const REQUEST: Duration = Duration::from_secs(20);

fn runtime() -> Result<Rc<Runtime>> {
    Ok(Rc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
    ))
}

fn wait<T>(
    rt: &Runtime,
    limit: Duration,
    op: impl Future<Output = std::result::Result<T, IdeviceError>>,
) -> Result<T> {
    let result = rt
        .block_on(async { tokio::time::timeout(limit, op).await })
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "苹果设备服务读取超时"))?;
    Ok(result?)
}

fn address() -> UsbmuxdAddr {
    UsbmuxdAddr::TcpSocket(SocketAddr::from((
        Ipv4Addr::LOCALHOST,
        UsbmuxdAddr::DEFAULT_PORT,
    )))
}

fn attached(rt: &Runtime) -> Result<Vec<UsbmuxdDevice>> {
    let devices = wait(rt, Duration::from_secs(4), async {
        address().connect(1).await?.get_devices().await
    })
    .map_err(|e| format!("无法通过本机 Apple 设备服务列出 USB 设备。{e}"))?;
    Ok(devices
        .into_iter()
        .filter(|d| d.connection_type == Transport::Usb)
        .collect())
}

fn children(names: Vec<String>) -> Result<Vec<String>> {
    let mut names = names
        .into_iter()
        .filter(|n| n != "." && n != "..")
        .collect::<Vec<_>>();
    for name in &names {
        if name.is_empty()
            || name.contains(['/', '\\', ':', '\0', '\u{fffd}'])
            || name.chars().any(char::is_control)
        {
            return Err("iPhone 返回了无法唯一表示的原件路径".into());
        }
    }
    names.sort_unstable();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("iPhone 在同一目录返回了重复名称".into());
    }
    Ok(names)
}

fn remote_path(relative: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err("无效的 iPhone 原件路径".into());
        };
        let name = name.to_str().ok_or("无效的 iPhone 文件名")?;
        if name.contains(['/', '\\', ':', '\0']) {
            return Err("无效的 iPhone 文件名".into());
        }
        parts.push(name);
    }
    if parts.first() != Some(&CAMERA) {
        return Err("请选择 iPhone 相机原件目录".into());
    }
    Ok(format!("/{}", parts.join("/")))
}

// AFC path fields are C strings. Keep the terminator out of stored names and identities.
fn wire_path(path: &str) -> String { format!("{path}\0") }

fn link(fields: &HashMap<String, String>) -> bool {
    fields.contains_key("LinkTarget") || fields.contains_key("st_link_target")
}

#[derive(Debug)]
struct CameraOperation {
    operation: &'static str,
    path: String,
    source: Box<dyn std::error::Error + Send + Sync>,
}
impl std::fmt::Display for CameraOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f,"{} {} 失败。{}",self.operation,self.path,self.source)
    }
}
impl std::error::Error for CameraOperation {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { Some(self.source.as_ref()) }
}

fn context(operation: &'static str, path: &str, source: Box<dyn std::error::Error + Send + Sync>) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(CameraOperation { operation, path: path.into(), source })
}

fn regular_file(relative: PathBuf, fields: &HashMap<String, String>) -> Result<Item> {
    if fields.get("st_ifmt").map(String::as_str) != Some("S_IFREG")
        || link(fields)
    {
        return Err("原件路径已变化".into());
    }
    let size = fields
        .get("st_size")
        .ok_or("原件缺少大小")?
        .parse::<u64>()?;
    let modified = fields
        .get("st_mtime")
        .ok_or("原件缺少修改时间")?
        .parse::<u64>()?;
    let version = format!(
        "afc:{}:{}:{modified}:{size}",
        fields.get("st_ino").map_or("", String::as_str),
        fields.get("st_birthtime").map_or("", String::as_str)
    );
    Ok(Item {
        id: remote_path(&relative)?,
        relative,
        size: Some(size),
        modified: UNIX_EPOCH.checked_add(Duration::from_nanos(modified)),
        version: Some(version),
    })
}

pub(super) struct Phone {
    rt: Rc<Runtime>,
    provider: UsbmuxdProvider,
    control: RefCell<AfcClient>,
    data: Rc<RefCell<Option<AfcClient>>>,
    identity: String,
    pub(super) serial: String,
}

fn same_hardware(id: &str, serial: &str, udid: &str) -> bool {
    !id.is_empty() && (id.eq_ignore_ascii_case(serial) || id.eq_ignore_ascii_case(udid))
}

impl Phone {
    fn open(rt: Rc<Runtime>, device: &UsbmuxdDevice) -> Result<Self> {
        let provider = device.to_provider(address(), "Time-cut");
        let serial = wait(&rt, REQUEST, async {
            let mut lockdown = LockdownClient::connect(&provider).await?;
            lockdown
                .start_session(&provider.get_pairing_file().await?)
                .await?;
            lockdown.get_value(Some("SerialNumber"), None).await
        })?
        .as_string()
        .filter(|s| !s.is_empty())
        .ok_or("Apple 设备未提供硬件序列号")?
        .to_owned();
        let client = wait(&rt, REQUEST, AfcClient::connect(&provider))?;
        let phone = Self {
            rt,
            provider,
            control: RefCell::new(client),
            data: Default::default(),
            identity: format!("afc:{}", device.udid),
            serial,
        };
        let info = phone.info("/DCIM")?;
        if info.get("st_ifmt").map(String::as_str) != Some("S_IFDIR")
            || link(&info)
        {
            return Err("iPhone 尚未提供原件目录 DCIM".into());
        }
        Ok(phone)
    }

    // The explicit diagnostic must never pick an arbitrary phone on a multi-device computer.
    pub(super) fn only() -> Result<Self> {
        let rt = runtime()?;
        let devices = attached(&rt)?;
        let [device] = devices.as_slice() else {
            return Err(format!(
                "Apple 设备服务报告 {} 台 USB 设备，无法唯一确定待核对的 iPhone",
                devices.len()
            )
            .into());
        };
        Self::open(rt, device)
    }

    fn info(&self, path: &str) -> Result<HashMap<String, String>> {
        wait(
            &self.rt,
            REQUEST,
            self.control.borrow_mut().get_file_info_raw(wire_path(path)),
        ).map_err(|error| context("读取 Apple 文件属性",path,error))
    }

    fn info_if_present(&self, path: &str) -> Result<Option<HashMap<String, String>>> {
        match self.info(path) {
            Ok(fields) => Ok(Some(fields)),
            Err(error) if matches!(schedule::afc_status(error.as_ref()), Some(AfcError::ObjectNotFound)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn list(&self, path: &Path) -> Result<Vec<String>> {
        children(wait(
            &self.rt,
            REQUEST,
            self.control.borrow_mut().list_dir(wire_path(&remote_path(path)?)),
        ).map_err(|error| context("列出 Apple 目录",&path.to_string_lossy(),error))?)
    }

    pub(super) fn camera_entry_count(&self) -> Result<usize> {
        Ok(self.list(Path::new(CAMERA))?.len())
    }

    fn walk(
        &self,
        order: Option<&CopyOrder>,
        emit: &mut dyn FnMut(ScanEvent) -> Result<()>,
    ) -> Result<()> {
        let root = PathBuf::from(CAMERA);
        let mut pending = vec![root.clone()];
        let mut snapshots = Vec::new();
        emit(ScanEvent::DirectoryFound(root))?;
        while let Some(parent) = pending.pop() {
            emit(ScanEvent::DirectoryStart(parent.clone()))?;
            let names = self.list(&parent)?;
            let mut work: Vec<_> = names.iter().collect();
            if let Some(order) = order {
                work.sort_by_key(|name|order.priorities.get(&parent.join(name)).copied().unwrap_or(1));
            }
            let mut listed = Vec::new();
            let mut complete = true;
            for name in work {
                let relative = parent.join(name);
                let path = remote_path(&relative)?;
                let fields = self.info(&path)?;
                let kind = fields
                    .get("st_ifmt")
                    .ok_or("iPhone 未提供对象类型")?
                    .as_str();
                if kind == "S_IFDIR" && !link(&fields) {
                    emit(ScanEvent::Inspected { media: false })?;
                    emit(ScanEvent::DirectoryFound(relative.clone()))?;
                    pending.push(relative);
                } else if kind == "S_IFREG" && media(&relative) {
                    let original = regular_file(relative, &fields)?;
                    emit(ScanEvent::Inspected { media: true })?;
                    emit(ScanEvent::Ready(original.clone()))?;
                    listed.push(original);
                } else {
                    emit(ScanEvent::Inspected { media: false })?;
                    if media(&relative) || (kind != "S_IFREG" && kind != "S_IFLNK") {
                        complete = false;
                        emit(ScanEvent::Unreadable(format!(
                            "原件目录包含未能读取的对象 {}",
                            relative.display()
                        )))?;
                    }
                }
            }
            if complete {
                emit(ScanEvent::DirectoryDone {
                    path: parent.clone(),
                    media: listed,
                })?;
            }
            snapshots.push((parent, names));
            if let Some(order) = order {
                pending.sort_by_key(|p| {
                    std::cmp::Reverse((
                        order.priorities.get(p).copied().unwrap_or(1),
                        order.folders.get(p).copied().unwrap_or(i64::MIN),
                    ))
                });
            }
        }
        let total = snapshots.len() as u64;
        for (n, (path, names)) in snapshots.into_iter().enumerate() {
            if self.list(&path)? != names {
                return Err("接收期间 iPhone 原件目录有变化，将重新盘点".into());
            }
            emit(ScanEvent::DirectoryCheck {
                done: n as u64 + 1,
                total,
            })?;
        }
        Ok(())
    }
}

impl Source for Phone {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn paired_camera_serial(&self) -> Option<&str> { Some(&self.serial) }
    fn album_scope_version(&self) -> Option<&'static str> { Some(CAMERA_SCOPE) }
    fn album_path_in_scope(&self, path: &Path) -> bool {
        path.starts_with(CAMERA) && path.parent().is_some_and(|parent| parent.starts_with(CAMERA))
            && remote_path(path).is_ok()
    }
    fn saved_files(&self) -> Option<&dyn SavedFiles> { Some(self) }
    fn visit(&self, emit: &mut dyn FnMut(ScanEvent) -> Result<()>) -> Result<()> {
        self.walk(None, emit)
    }
    fn visit_for_copy(
        &self,
        order: &CopyOrder,
        emit: &mut dyn FnMut(ScanEvent) -> Result<()>,
    ) -> Result<()> {
        self.walk(Some(order), emit)
    }
    fn reader(&self, item: &Item) -> Result<Box<dyn Read>> {
        let path = remote_path(&item.relative)?;
        let existing = self.data.borrow_mut().take();
        let session = match existing {
            Some(client) => client,
            None => wait(&self.rt, REQUEST, AfcClient::connect(&self.provider))?,
        };
        let file = wait(
            &self.rt,
            REQUEST,
            session.open_owned(wire_path(&path), AfcFopenMode::RdOnly),
        ).map_err(|error| context("只读打开 Apple 原件",&path,error))?;
        Ok(Box::new(Reader {
            rt: self.rt.clone(),
            pool: self.data.clone(),
            file: Some(file),
            eof: false,
        }))
    }
    fn content_sha1(&self, item: &Item) -> Result<Option<[u8; 20]>> {
        let path = remote_path(&item.relative)?;
        let limit = Duration::from_secs(20 + (item.size.unwrap_or(0) / (1024 * 1024)).min(3600));
        let reply = wait(
            &self.rt,
            limit,
            self.control.borrow_mut().get_file_hash(wire_path(&path)),
        );
        let digest = match reply {
            Ok(bytes) => bytes
                .try_into()
                .map_err(|_| "iPhone 返回了无效的文件摘要")?,
            Err(e)
                if e.downcast_ref::<IdeviceError>().is_some_and(|e| {
                    matches!(
                        e,
                        IdeviceError::Afc(AfcError::OpNotSupported | AfcError::UnknownOpcode)
                    )
                }) =>
            {
                use sha1::{Digest, Sha1};
                let mut reader = self.reader(item)?;
                let mut hash = Sha1::new();
                let size = io::copy(&mut reader, &mut hash)?;
                if item.size != Some(size) {
                    return Err("读取 iPhone 原件摘要时长度变化".into());
                }
                hash.finalize().into()
            }
            Err(e) => return Err(context("读取 Apple 原件摘要",&path,e)),
        };
        Ok(Some(digest))
    }
    fn verify_read(&self, item: &Item) -> Result<()> {
        let current = regular_file(
            item.relative.clone(),
            &self.info(&remote_path(&item.relative)?)?,
        )?;
        if current.version != item.version || current.size != item.size {
            return Err("核对期间 iPhone 原件已改变".into());
        }
        Ok(())
    }
}

impl SavedFiles for Phone {
    fn resolve(&self, expected: &Item) -> Result<Option<Item>> {
        if expected.version.as_deref().is_none_or(str::is_empty) || expected.size.is_none_or(|n| n == 0)
            || !media(&expected.relative) { return Ok(None); }
        let path = remote_path(&expected.relative)?;
        // A stored path never authorizes crossing a link outside the camera folder.
        let mut parent = PathBuf::new();
        for part in expected.relative.parent().into_iter().flat_map(Path::components) {
            parent.push(part);
            let Some(attrs) = self.info_if_present(&remote_path(&parent)?)? else { return Ok(None); };
            if attrs.get("st_ifmt").map(String::as_str) != Some("S_IFDIR") || link(&attrs) {
                return Ok(None);
            }
        }
        let Some(attrs) = self.info_if_present(&path)? else { return Ok(None); };
        if attrs.get("st_ifmt").map(String::as_str) != Some("S_IFREG") || link(&attrs) {
            return Ok(None);
        }
        let item = regular_file(expected.relative.clone(), &attrs)?;
        Ok((item.version == expected.version && item.size == expected.size
            && expected.modified.is_none_or(|t| item.modified == Some(t))).then_some(item))
    }
}

struct Reader {
    rt: Rc<Runtime>,
    pool: Rc<RefCell<Option<AfcClient>>>,
    file: Option<OwnedFileDescriptor>,
    eof: bool,
}
impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.eof {
            return Ok(0);
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("iPhone 数据流已关闭"))?;
        match wait(&self.rt, REQUEST, file.read_n(buf.len().min(256 * 1024))) {
            Ok(bytes) if bytes.len() <= buf.len() => {
                let n = bytes.len();
                buf[..n].copy_from_slice(&bytes);
                self.eof = n == 0;
                Ok(n)
            }
            Err(e)
                if matches!(
                    e.downcast_ref::<IdeviceError>(),
                    Some(IdeviceError::Afc(AfcError::EndOfData))
                ) =>
            {
                self.eof = true;
                Ok(0)
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "iPhone 返回的数据超出请求长度",
            )),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}
impl Drop for Reader {
    fn drop(&mut self) {
        // An interrupted data connection is discarded. Only a clean EOF is reusable.
        if let Some(file) = self.file.take() {
            let deadline = if self.eof { REQUEST } else { Duration::from_secs(2) };
            if let Ok(session) = wait(&self.rt, deadline, file.close()) && self.eof {
                *self.pool.borrow_mut() = Some(session);
            }
        }
    }
}

pub(super) fn verify_gallery(root: &Path, phone: &Phone) -> Result<()> {
    use rusqlite::{Connection, OpenFlags};
    let db = Connection::open_with_flags(
        root.join(".time-cut/imports.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut sources = HashSet::new();
    for table in ["sources", "phone_runs", "phone_inbox"] {
        if db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=? AND type='table')",
            [table],
            |r| r.get::<_, bool>(0),
        )? {
            for row in db.prepare(&format!("SELECT DISTINCT source FROM {table} WHERE source LIKE 'wpd:%' OR source LIKE 'afc:%'"))?
                .query_map([],|r|r.get::<_,String>(0))? { sources.insert(row?); }
        }
    }
    if !sources.is_empty()
        && !sources.iter().any(|s| {
            s == phone.identity()
                || s.strip_prefix("wpd:")
                    .is_some_and(|id| same_hardware(id, &phone.serial, &phone.provider.udid))
        })
    {
        return Err("已连接 iPhone 的硬件身份与此图库的历史设备不符".into());
    }
    Ok(())
}

#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct Audit {
    discovered: u64,
    listed: u64,
    matched: u64,
    pub(super) without_local_proof: u64,
    matched_bytes: u64,
    rechecked: u64,
    stable_inventory: bool,
    same_as_previous: bool,
    manifest: String,
    repeat_verified: bool,
    copied: u64,
    copied_bytes: u64,
    current_bytes: u64,
    current_size: Option<u64>,
    saved_receipts: u64,
    direct_attempts: u64,
    direct_received: u64,
}

/// Independent evidence for diagnosing a WPD import. This connection never changes
/// the ingest journal, the unplug signal, the phone, or any archived original.
pub(super) fn audit(
    root: &Path,
    source: &dyn Source,
    progress: impl FnMut(&Audit),
) -> Result<Audit> {
    use super::inbox::fingerprints::Catalog;
    let mut catalog = Catalog::open(root)?;
    scan(root, source, &schedule::Plan::default(), |_| Ok(()), |_| Ok(()), |_, size, checksum, _| Ok((catalog.verified(size, checksum)?, 0)), progress, || Ok(()))
}

fn scan(
    root: &Path,
    source: &dyn Source,
    plan: &schedule::Plan,
    mut discovered: impl FnMut(&Item) -> Result<()>,
    record_attempt: impl FnMut(&Item) -> Result<()>,
    mut local_copy: impl FnMut(&Item, u64, &[u8; 20], &mut dyn FnMut(u64) -> Result<()>) -> Result<(Option<String>, u64)>,
    mut progress: impl FnMut(&Audit),
    mut active: impl FnMut() -> Result<()>,
) -> Result<Audit> {
    use super::inventory;
    use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
    let db = Connection::open_with_flags(
        root.join(".time-cut/iphone-originals-audit.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch("PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS evidence(device TEXT NOT NULL,path TEXT NOT NULL,version TEXT,size INTEGER NOT NULL,
            sha1 BLOB NOT NULL,local_digest TEXT,attempt TEXT NOT NULL,PRIMARY KEY(device,path)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS completed(device TEXT PRIMARY KEY,checked INTEGER NOT NULL,
            manifest TEXT NOT NULL,listed INTEGER NOT NULL,missing INTEGER NOT NULL) WITHOUT ROWID;")?;
    let attempt = uuid::Uuid::new_v4().to_string();
    let device = super::verification::device_key(source.identity());
    let mut summary = Audit::default();
    let mut members = Vec::new();
    let mut last = std::time::Instant::now();
    let found = std::cell::Cell::new(0);
    progress(&summary);
    let visited = schedule::visit(source, plan, |item| {
        discovered(item)?;
        found.set(found.get()+1);
        Ok(())
    }, record_attempt, |event| {
        active()?;
        summary.discovered = found.get();
        match event {
            ScanEvent::Ready(item) => {
                let size = item
                    .size
                    .filter(|size| *size > 0)
                    .ok_or("iPhone 原件长度无效")?;
                let checksum = source.content_sha1(&item)?.ok_or("来源未提供原件摘要")?;
                summary.current_bytes = 0;
                summary.current_size = Some(size);
                let (local, copied) = local_copy(&item, size, &checksum, &mut |n| {
                    active()?;
                    summary.current_bytes = n;
                    if last.elapsed() >= Duration::from_millis(250) {
                        progress(&summary);
                        last = std::time::Instant::now();
                    }
                    Ok(())
                })?;
                source.verify_read(&item)?;
                summary.current_size = None;
                summary.current_bytes = 0;
                summary.copied += u64::from(copied > 0);
                summary.copied_bytes += copied;
                db.prepare_cached("INSERT OR REPLACE INTO evidence VALUES(?,?,?,?,?,?,?)")?
                    .execute(params![
                        device,
                        item.id,
                        item.version,
                        size,
                        checksum.as_slice(),
                        local,
                        attempt
                    ])?;
                summary.listed += 1;
                if local.is_some() {
                    summary.matched += 1;
                    summary.matched_bytes += size;
                } else {
                    summary.without_local_proof += 1;
                }
                members.push(inventory::Member::new(item));
            }
            ScanEvent::Unreadable(message) => return Err(message.into()),
            _ => (),
        }
        if last.elapsed() >= Duration::from_secs(2) {
            progress(&summary);
            last = std::time::Instant::now();
        }
        Ok(())
    });
    summary.discovered = found.get();
    summary.current_size = None;
    summary.current_bytes = 0;
    progress(&summary);
    visited?;
    if summary.listed == 0 {
        return Err("这次连接没有列出可核对的 iPhone 原件".into());
    }
    let order = CopyOrder {
        folders: Default::default(),
        priorities: Default::default(),
        interrupted: Default::default(),
        preferred_folder: None,
    };
    inventory::confirm(source, &order, &members, |done, _| {
        active()?;
        summary.rechecked = done;
        if last.elapsed() >= Duration::from_secs(2) {
            progress(&summary);
            last = std::time::Instant::now();
        }
        Ok(())
    })?;
    let tx = db.unchecked_transaction()?;
    let mut hash = blake3::Hasher::new();
    let mut count = 0;
    {
        let mut q=tx.prepare("SELECT path,version,size,sha1 FROM evidence WHERE device=? AND attempt=? ORDER BY path")?;
        let mut rows = q.query(params![device, attempt])?;
        while let Some(r) = rows.next()? {
            for value in [
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ] {
                hash.update(&(value.len() as u64).to_be_bytes());
                hash.update(value.as_bytes());
            }
            hash.update(&r.get::<_, u64>(2)?.to_be_bytes());
            hash.update(&r.get::<_, [u8; 20]>(3)?);
            count += 1;
        }
    }
    if count != summary.listed || summary.rechecked != count {
        return Err("独立核对清单尚未对齐".into());
    }
    let fingerprint = hash.finalize().to_hex().to_string();
    let previous = tx
        .query_row(
            "SELECT manifest FROM completed WHERE device=?",
            [&device],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    summary.same_as_previous = previous.as_ref() == Some(&fingerprint);
    tx.execute(
        "INSERT OR REPLACE INTO completed VALUES(?,?,?,?,?)",
        params![
            device,
            chrono::Utc::now().timestamp_millis(),
            fingerprint,
            count,
            summary.without_local_proof
        ],
    )?;
    tx.commit()?;
    summary.manifest = fingerprint;
    summary.stable_inventory = true;
    progress(&summary);
    Ok(summary)
}
