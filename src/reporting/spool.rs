//! A single-owner spool. Only published `.json` records represent accepted work;
//! temporary files are never replayed. ACK is unlink + directory fsync, not a
//! second journal that could disagree with the pending records after a crash.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{Delivery, Entry, QueueLimits, State};

pub(super) struct Spool {
    path: PathBuf,
    directory: File,
    // Held for the lifetime of the queue, including replay.
    _lock: File,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    sequence: u64,
    payload: String,
    checksum: String,
}

fn filename(id: &str) -> String {
    format!("{}.json", hex::encode(Sha256::digest(id.as_bytes())))
}

fn checksum(sequence: u64, payload: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(1u8.to_be_bytes());
    digest.update(sequence.to_be_bytes());
    digest.update(payload);
    hex::encode(digest.finalize())
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn make_directory(path: &Path) -> Result<()> {
    if path.exists() {
        ensure!(
            fs::symlink_metadata(path)?.file_type().is_dir(),
            "outbox is not a directory"
        );
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    make_directory(parent)?;
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    match builder.create(path) {
        Ok(()) => File::open(parent)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(path)?.file_type().is_dir(),
                "outbox is not a directory"
            );
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "invalid outbox file type"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

impl Spool {
    pub(super) fn open(path: &Path, limits: QueueLimits) -> Result<(Self, State)> {
        ensure!(!path.as_os_str().is_empty(), "empty outbox path");
        make_directory(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        let lock_path = path.join(".lock");
        if fs::symlink_metadata(&lock_path).is_ok() {
            private_file(&lock_path)?;
        }
        let lock = private_options()
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock()
            .map_err(|_| anyhow::anyhow!("outbox is already locked or cannot be locked"))?;
        let spool = Self {
            path: path.to_owned(),
            directory: File::open(path)?,
            _lock: lock,
        };
        let mut records = Vec::new();
        let mut state = State::default();
        for item in fs::read_dir(path)? {
            let item = item?;
            let name = item.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid outbox filename"))?;
            if name == ".lock" {
                continue;
            }
            private_file(&item.path())?;
            // Incomplete atomic writes have never returned enqueue success.
            if name
                .strip_prefix(".tmp-")
                .is_some_and(|id| Uuid::parse_str(id).is_ok())
            {
                continue;
            }
            ensure!(
                name.len() == 69 && name.ends_with(".json"),
                "unexpected outbox file"
            );
            ensure!(
                records.len() < limits.max_items,
                "outbox exceeds item limit"
            );
            let cap = limits
                .max_payload_bytes
                .min(limits.max_bytes)
                .saturating_mul(6)
                .saturating_add(2048);
            let mut bytes = Vec::new();
            File::open(item.path())?
                .take(cap.saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= cap, "outbox record exceeds size limit");
            // Do not expose serde errors: malformed records may contain secrets.
            let record: Record = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid outbox record"))?;
            ensure!(record.version == 1, "unsupported outbox version");
            let payload = record.payload.into_bytes();
            ensure!(
                payload.len() <= limits.max_payload_bytes,
                "outbox payload exceeds limit"
            );
            ensure!(
                checksum(record.sequence, &payload) == record.checksum,
                "outbox checksum mismatch"
            );
            let value: serde_json::Value = serde_json::from_slice(&payload)
                .map_err(|_| anyhow::anyhow!("invalid outbox payload"))?;
            let id = value
                .as_object()
                .and_then(|v| v.get("execution_id"))
                .and_then(|v| v.as_str())
                .filter(|id| !id.trim().is_empty() && id.len() <= 256)
                .ok_or_else(|| anyhow::anyhow!("invalid outbox execution ID"))?;
            ensure!(filename(id) == name, "outbox filename mismatch");
            ensure!(
                payload.len() <= limits.max_bytes.saturating_sub(state.bytes),
                "outbox exceeds byte limit"
            );
            state.bytes += payload.len();
            state.next_sequence = state.next_sequence.max(
                record
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("outbox sequence exhausted"))?,
            );
            let entry = Entry {
                delivery: Delivery {
                    id: id.to_owned(),
                    payload: payload.clone().into(),
                },
                digest: Sha256::digest(&payload).into(),
            };
            records.push((record.sequence, entry));
        }
        records.sort_by_key(|(sequence, _)| *sequence);
        ensure!(
            !records.windows(2).any(|pair| pair[0].0 == pair[1].0),
            "duplicate outbox sequence"
        );
        state.pending = records.into_iter().map(|(_, entry)| entry).collect();
        spool.directory.sync_all()?;
        Ok((spool, state))
    }

    pub(super) fn store(&self, id: &str, payload: &[u8], sequence: u64) -> Result<()> {
        let record = Record {
            version: 1,
            sequence,
            payload: std::str::from_utf8(payload)?.to_owned(),
            checksum: checksum(sequence, payload),
        };
        let bytes = serde_json::to_vec(&record)?;
        let temporary = self.path.join(format!(".tmp-{}", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = private_options().create_new(true).open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, self.path.join(filename(id)))?;
            self.directory.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            // Only the unpublished temp is expendable. A rename with failed fsync
            // is ambiguous: retain the published file for restart/retry recovery.
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(super) fn acknowledge(&self, id: &str) -> Result<()> {
        ensure!(
            fs::symlink_metadata(&self.path)?.file_type().is_dir(),
            "outbox directory unavailable"
        );
        match fs::remove_file(self.path.join(filename(id))) {
            Ok(()) => {}
            // A previous unlink may have succeeded before directory fsync failed.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => bail!("cannot remove outbox record"),
        }
        self.directory.sync_all()?;
        Ok(())
    }
}
