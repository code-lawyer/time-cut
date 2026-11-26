//! Start a separate, verified DCIM capture before opening the Windows export.
use super::{Phone, attached, runtime, same_hardware, supplement};
use crate::ingest::{ImportSource, PhoneChannel, Report, Target, TransferStop, View, archive::Result, catalog, job};
use std::{net::{Ipv4Addr, SocketAddr, TcpStream}, path::Path, sync::{Arc, Mutex}, time::Duration};
use tauri::AppHandle;

pub(in crate::ingest) const CAMERA_LABEL: &str = "iPhone (DCIM)";

pub(in crate::ingest) struct FinishedCamera {
    udid: String,
    serial: String,
    attachment: u32,
    target: Target,
    pub report: Report,
}

impl FinishedCamera {
    pub(in crate::ingest) fn matches_export(&self, identity: &str) -> bool {
        identity.strip_prefix("wpd:").is_some_and(|id| same_hardware(id, &self.serial, &self.udid))
    }
}

pub(in crate::ingest) fn receive(
    app: &AppHandle,
    view: &Arc<Mutex<View>>,
    dest: &Target,
    label: &str,
    finished: &mut Option<FinishedCamera>,
) -> Result<bool> {
    let endpoint = SocketAddr::from((Ipv4Addr::LOCALHOST,idevice::usbmuxd::UsbmuxdAddr::DEFAULT_PORT));
    if TcpStream::connect_timeout(&endpoint,Duration::from_millis(250)).is_err() {
        *finished = None;
        return Ok(false);
    }
    let rt = runtime()?;
    let devices = attached(&rt)?;
    let [device] = devices.as_slice() else {
        *finished = None;
        return Ok(false);
    };
    if finished.as_ref().is_some_and(|prior| {
        prior.target == *dest && prior.udid == device.udid && prior.attachment == device.device_id
    }) {
        return Ok(false);
    }
    *finished = None;
    let phone = Phone::open(rt, device)?;
    let serial = phone.serial.clone();
    {
        let mut live = view.lock().unwrap();
        if !live.devices.iter().any(|name| name == label) { live.devices.push(label.into()); }
    }
    let source = ImportSource::Phone {
        label,
        channel: Some(PhoneChannel::AppleCamera),
        camera: None,
        open: Box::new(move || Ok(Box::new(phone))),
    };
    let report = job(app, view, dest, source, || match view.lock().unwrap().target.as_ref() {
        Some(current) if current == dest => Ok(()),
        Some(current) if current.auto => Err(TransferStop::DestinationChanged.into()),
        _ => Err(TransferStop::AutoDisabled.into()),
    })?;
    if report.safe_at.is_some() && !report.repeat_due {
        // The new manifest can also satisfy earlier Apple receipts collected as
        // supplements to a Windows run. Keep any historical gaps open.
        let _guard = catalog::guard_destination(dest, catalog::album_path)()?;
        supplement::reconcile(Path::new(&dest.root), &device.udid, &report)?;
        *finished = Some(FinishedCamera {
            udid: device.udid.clone(),
            serial,
            attachment: device.device_id,
            target: dest.clone(),
            report,
        });
        return Ok(false);
    }
    Ok(report.safe_at.is_none())
}
