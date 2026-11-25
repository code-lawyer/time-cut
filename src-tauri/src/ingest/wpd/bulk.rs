use super::*;
use windows_core::{GUID, Ref, implement};

enum Batch {
    Objects(Vec<(String, Object)>),
    End(HRESULT),
}

#[implement(IPortableDevicePropertiesBulkCallback)]
struct Callback {
    parent: PathBuf,
    sender: mpsc::Sender<Batch>,
}

impl IPortableDevicePropertiesBulkCallback_Impl for Callback_Impl {
    fn OnStart(&self, _context: *const GUID) -> windows_core::Result<()> {
        Ok(())
    }
    fn OnProgress(
        &self,
        _context: *const GUID,
        results: Ref<'_, IPortableDeviceValuesCollection>,
    ) -> windows_core::Result<()> {
        let values = results.ok()?;
        let mut count = 0;
        unsafe {
            values.GetCount(&raw mut count)?;
        }
        let mut rows = Vec::new();
        for index in 0..count {
            let item = unsafe { values.GetAt(index)? };
            if let Some(id) = text_value(&item, &WPD_OBJECT_ID)
                && let Ok(object) = Phone::describe(&item, &id, &self.parent, || None)
                && !matches!(&object,Object::Media(i) if i.size.is_none())
            {
                rows.push((id, object));
            }
        }
        let _ = self.sender.send(Batch::Objects(rows));
        Ok(())
    }
    fn OnEnd(&self, _context: *const GUID, status: HRESULT) -> windows_core::Result<()> {
        let _ = self.sender.send(Batch::End(status));
        Ok(())
    }
}

/// Only native strings and parsed metadata cross the callback boundary. Missing
/// properties fall back to the existing single-object path on the owning thread.
pub(super) fn objects(
    api: &IPortableDevicePropertiesBulk,
    keys: &IPortableDeviceKeyCollection,
    parent: &Path,
    ids: &[String],
) -> Result<HashMap<String, Object>> {
    unsafe {
        let list: IPortableDevicePropVariantCollection = CoCreateInstance(
            &PortableDevicePropVariantCollection,
            None,
            CLSCTX_INPROC_SERVER,
        )?;
        let scratch: IPortableDeviceValues =
            CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)?;
        for id in ids {
            scratch.SetStringValue(&WPD_OBJECT_ID, &HSTRING::from(id))?;
            let mut value = scratch.GetValue(&WPD_OBJECT_ID)?;
            let result = list.Add(&value);
            let _ = PropVariantClear(&mut value);
            result?;
        }
        let (sender, receiver) = mpsc::channel();
        let callback: IPortableDevicePropertiesBulkCallback = Callback {
            parent: parent.to_path_buf(),
            sender,
        }
        .into();
        let context = api.QueueGetValuesByObjectList(&list, keys, &callback)?;
        api.Start(&context)?;
        let mut rows = HashMap::new();
        loop {
            match receiver.recv_timeout(Duration::from_secs(40)) {
                Ok(Batch::Objects(objects)) => rows.extend(objects),
                Ok(Batch::End(status)) => {
                    status.ok()?;
                    return Ok(rows);
                }
                Err(_) => {
                    let _ = api.Cancel(&context);
                    return Err(windows_core::Error::from(E_WPD_DEVICE_IS_HUNG).into());
                }
            }
        }
    }
}
