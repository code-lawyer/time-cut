//! Select the original camera source for the normal Windows import worker.
use super::{Phone, attached, runtime, same_hardware, supplement};
use crate::ingest::{ImportSource, Report, Target, TransferStop, View, archive::Result, job};
use rusqlite::{Connection, OpenFlags};
use std::{net::{Ipv4Addr, SocketAddr, TcpStream}, path::Path, sync::{Arc, Mutex}, time::Duration};
use tauri::AppHandle;

pub(in crate::ingest) const CAMERA_LABEL: &str = "iPhone (DCIM)";

#[derive(PartialEq, Eq)]
pub(in crate::ingest) struct FinishedCamera {
    udid: String,
    attachment: u32,
    target: Target,
}

impl FinishedCamera {
    pub(in crate::ingest) fn belongs_to(&self, selected: Option<&Target>) -> bool { selected == Some(&self.target) }
}

#[derive(Debug, PartialEq, Eq)]
enum Route<'a> { Standalone, Supplement(&'a str) }

fn route<'a>(pending: &'a [(String,String)], windows_key: Option<&str>, serial: &str, udid: &str) -> Option<Route<'a>> {
    if pending.is_empty() {
        return windows_key.is_none().then_some(Route::Standalone);
    }
    let mut candidates = pending.iter().filter(|(id,key)| {
        windows_key.is_none_or(|expected| expected == key) && same_hardware(id, serial, udid)
    });
    let (_,key) = candidates.next()?;
    candidates.next().is_none().then_some(Route::Supplement(key))
}

pub(in crate::ingest) fn receive(
    app: &AppHandle,
    view: &Arc<Mutex<View>>,
    db: &Connection,
    dest: &Target,
    windows: Option<&Report>,
    finished: &mut Option<FinishedCamera>,
) -> Result<bool> {
    if windows.is_some_and(|report| report.device_key.is_none()) { return Ok(false); }
    let endpoint = SocketAddr::from((Ipv4Addr::LOCALHOST,idevice::usbmuxd::UsbmuxdAddr::DEFAULT_PORT));
    if TcpStream::connect_timeout(&endpoint,Duration::from_millis(250)).is_err() {
        *finished = None;
        return Ok(false);
    }
    let rt = runtime()?;
    let devices = attached(&rt)?;
    let [device] = devices.as_slice() else {
        if devices.is_empty() { *finished = None; }
        return Ok(false);
    };
    let windows_key = windows.and_then(|report| report.device_key.as_deref());
    let file = Path::new(&dest.root).join(".time-cut/imports.sqlite3");
    let pending = if file.try_exists()? {
        let state = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
        supplement::pending_sources(&state, windows_key)?
    } else { Vec::new() };
    let current = FinishedCamera { udid: device.udid.clone(), attachment: device.device_id, target: dest.clone() };
    if finished.as_ref().is_some_and(|prior| prior != &current) { *finished = None; }
    if pending.is_empty() && (windows_key.is_some() || finished.as_ref() == Some(&current)) { return Ok(false); }
    let ids: Vec<_> = pending.iter().map(|(id,_)| id.as_str()).collect();
    let Some(phone) = Phone::open(rt,device,&ids)? else { return Ok(false) };
    match route(&pending,windows_key,&phone.serial,&device.udid) {
        Some(Route::Supplement(key)) => supplement::receive(view,db,dest,windows,&phone,key),
        Some(Route::Standalone) => {
            let label = CAMERA_LABEL;
            {
                let mut live = view.lock().unwrap();
                if !live.devices.iter().any(|name| name == label) { live.devices.push(label.into()); }
            }
            let source = ImportSource::Phone {
                label,
                open: Box::new(move || Ok(Box::new(phone))),
            };
            let report = job(app,view,dest,source,|| {
                match view.lock().unwrap().target.as_ref() {
                    Some(current) if current == dest => Ok(()),
                    Some(current) if current.auto => Err(TransferStop::DestinationChanged.into()),
                    _ => Err(TransferStop::AutoDisabled.into()),
                }
            })?;
            if report.safe_at.is_some() && !report.repeat_due { *finished = Some(current); }
            Ok(report.safe_at.is_none() && report.failed > 0)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standalone_apple_camera_can_start_a_fresh_gallery_but_cannot_bypass_an_unfinished_windows_inventory() {
        assert_eq!(route(&[],None,"serial","udid"),Some(Route::Standalone));
        assert_eq!(route(&[],Some("missing run"),"serial","udid"),None);
        let pending = vec![("Serial".into(),"first".into()),("another".into(),"second".into())];
        assert_eq!(route(&pending,None,"SERIAL","udid"),Some(Route::Supplement("first")));
        assert_eq!(route(&pending,Some("second"),"serial","udid"),None);
        assert_eq!(route(&pending,None,"foreign","unlisted"),None);
        let aliases = vec![("serial".into(),"key-a".into()),("UDID".into(),"key-b".into())];
        assert_eq!(route(&aliases,None,"serial","udid"),None);
    }
}
