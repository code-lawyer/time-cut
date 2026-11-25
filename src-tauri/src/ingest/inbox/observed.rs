//! Record what an anomalous read actually returned. These observations are never import receipts.
use crate::ingest::archive::{IncompleteTransfer, Item, Result};
use rusqlite::{Connection, params};
use std::{
    error::Error,
    fmt,
    io::{self, Read},
};

#[derive(Debug)]
pub(in crate::ingest) struct ReturnedData {
    expected: u64,
    received: u64,
    digest: blake3::Hash,
}

impl fmt::Display for ReturnedData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.expected == self.received {
            f.write_str("手机对当前原件返回了不一致的内容，本次读取已丢弃")
        } else {
            write!(
                f,
                "传输不完整，实际收到 {} 字节，设备报告 {} 字节",
                self.received, self.expected
            )
        }
    }
}

impl Error for ReturnedData {}

impl ReturnedData {
    pub(super) fn measured(
        expected: Option<u64>,
        received: u64,
        hash: &blake3::Hasher,
    ) -> Box<dyn Error + Send + Sync> {
        match expected {
            Some(expected) if received > 0 => Box::new(Self {
                expected,
                received,
                digest: hash.finalize(),
            }),
            _ => Box::new(IncompleteTransfer { received, expected }),
        }
    }

    pub(in crate::ingest) fn record(
        &self,
        db: &Connection,
        run: i64,
        session: u64,
        item: &Item,
        calibrated: bool,
    ) -> Result<()> {
        let Some(version) = &item.version else {
            return Ok(());
        };
        if item.size != Some(self.expected) {
            return Ok(());
        }
        db.prepare_cached("INSERT INTO phone_returned_data(run_id,entry,version,reported_size,returned_size,digest,
                first_session,last_session,sessions,first_controlled,last_controlled)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?7,1,?8,?8)
            ON CONFLICT(run_id,entry,version,returned_size,digest) DO UPDATE SET
                sessions=sessions+(excluded.last_session>last_session),last_session=MAX(last_session,excluded.last_session),
                first_controlled=COALESCE(first_controlled,excluded.first_controlled),
                last_controlled=COALESCE(excluded.last_controlled,last_controlled)")?
            .execute(params![run,item.relative.to_string_lossy(),version,self.expected,self.received,
                self.digest.to_hex().as_str(),session,calibrated.then_some(session)])?;
        Ok(())
    }
}

/// A clean EOF may contradict the advertised length. A failing I/O call can
/// also interrupt a transfer and does not provide a complete stream to hash.
pub(super) fn read_block(
    reader: &mut dyn Read,
    buffer: &mut [u8],
    hashed: u64,
    hash: &blake3::Hasher,
) -> Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if let Some(end) = e
                    .get_ref()
                    .and_then(|e| e.downcast_ref::<IncompleteTransfer>())
                    && end.received == hashed
                    && end.expected.is_some_and(|n| hashed < n)
                {
                    return Err(ReturnedData::measured(end.expected, hashed, hash));
                }
                return Err(e.into());
            }
            Ok(n) => return Ok(n),
        }
    }
}
