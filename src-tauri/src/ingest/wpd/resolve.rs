use super::*;
use crate::ingest::archive::SavedFiles;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

// Both maps belong to one open WPD session. These handles must never go to disk.
#[derive(Default)]
pub(super) struct Folders {
    named: HashMap<String, HashMap<PathBuf, Option<(String, DirectoryLevel)>>>,
    members: HashMap<String, HashSet<String>>,
}

impl Phone {
    fn listed_child(&self, folder: &Path, file: &Path, id: &str) -> Result<bool> {
        let mut cache = self.folders.borrow_mut();
        let mut parent_id = String::from("DEVICE");
        let mut path = PathBuf::new();
        let mut level = DirectoryLevel::Device;
        for component in folder.components() {
            let std::path::Component::Normal(name) = component else {
                return Ok(false);
            };
            let folders = match cache.named.entry(parent_id.clone()) {
                std::collections::hash_map::Entry::Occupied(known) => known.into_mut(),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    let mut names = HashMap::new();
                    for id in self.children(&parent_id)? {
                        let values =
                            unsafe { self.props.GetValues(&HSTRING::from(&id), &self.fields)? };
                        if let Object::Directory { relative, storage } =
                            Self::describe(&values, &id, &path, false, || None)?
                            && let Some(child_level) = self.album_scope.child(level, &relative, storage)
                        {
                            // The displayed path must identify exactly one directory.
                            names
                                .entry(relative)
                                .and_modify(|old| *old = None)
                                .or_insert(Some((id, child_level)));
                        }
                    }
                    slot.insert(names)
                }
            };
            path.push(name);
            let Some(Some((id, child_level))) = folders.get(&path) else {
                return Ok(false);
            };
            parent_id.clone_from(id);
            level = *child_level;
        }
        if !self.album_scope.includes_file(level, file) {
            return Ok(false);
        }
        let children = match cache.members.entry(parent_id) {
            std::collections::hash_map::Entry::Occupied(known) => known.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let children = self.children(slot.key())?.into_iter().collect();
                slot.insert(children)
            }
        };
        Ok(children.contains(id))
    }

    fn current_id(&self, persistent: &str) -> Result<Option<String>> {
        unsafe {
            let request: IPortableDevicePropVariantCollection = CoCreateInstance(
                &PortableDevicePropVariantCollection,
                None,
                CLSCTX_INPROC_SERVER,
            )?;
            let values: IPortableDeviceValues =
                CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)?;
            values.SetStringValue(&WPD_OBJECT_PERSISTENT_UNIQUE_ID, &HSTRING::from(persistent))?;
            let mut value = values.GetValue(&WPD_OBJECT_PERSISTENT_UNIQUE_ID)?;
            let added = request.Add(&value);
            let _ = PropVariantClear(&mut value);
            added?;
            let mapped = self.content.GetObjectIDsFromPersistentUniqueIDs(&request)?;
            let mut count = 0;
            mapped.GetCount(&raw mut count)?;
            if count != 1 {
                return Ok(None);
            }
            let mut handle = PROPVARIANT::default();
            mapped.GetAt(0, &raw mut handle)?;
            let assigned = values.SetValue(&WPD_OBJECT_ID, &handle);
            let _ = PropVariantClear(&mut handle);
            assigned?;
            Ok(text_value(&values, &WPD_OBJECT_ID))
        }
    }
}

impl SavedFiles for Phone {
    fn resolve(&self, expected: &Item) -> Result<Option<Item>> {
        let Some((key, _)) = expected
            .version
            .as_deref()
            .and_then(|v| v.rsplit_once('/'))
            .and_then(|(prefix, _)| prefix.rsplit_once('/'))
        else {
            return Ok(None);
        };
        let Some(id) = self.current_id(key)? else {
            return Ok(None);
        };
        // On an iPhone the folder's reported parent can be an unreadable alias
        // for the storage. Anchor the file in the enumerated tree from DEVICE.
        let Some(parent) = expected.relative.parent() else {
            return Ok(None);
        };
        if !self.listed_child(parent, &expected.relative, &id)? {
            return Ok(None);
        }
        let Object::Media(item) = self.object(&id, parent)? else {
            return Ok(None);
        };
        Ok((item.relative == expected.relative
            && item.version == expected.version
            && item.size == expected.size
            && item.modified == expected.modified)
            .then_some(item))
    }
}
