//! Portal restore tokens are local credentials, never RPC/status metadata.
use crate::model::{CaptureResult, SourceType};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::PathBuf,
};

const MAX_BYTES: u64 = 8192;
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Authorization {
    owner: String,
    source: SourceType,
    token: String,
}

pub(super) struct TokenStore {
    directory: PathBuf,
    uid: u32,
}
impl TokenStore {
    pub fn for_user() -> CaptureResult<Self> {
        let directory = match std::env::var_os("XDG_STATE_HOME") {
            Some(path) if PathBuf::from(&path).is_absolute() => PathBuf::from(path),
            _ => PathBuf::from(std::env::var_os("HOME").ok_or("restore_storage_unavailable")?)
                .join(".local/state"),
        }
        .join("amaterasu/portal");
        Self::new(directory)
    }
    fn new(directory: PathBuf) -> CaptureResult<Self> {
        if !directory.is_absolute() {
            return Err("restore_storage_unavailable");
        }
        let uid = fs::metadata("/proc/self")
            .map_err(|_| "restore_storage_unavailable")?
            .uid();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(|_| "restore_storage_unavailable")?;
        let metadata =
            fs::symlink_metadata(&directory).map_err(|_| "restore_storage_unavailable")?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err("restore_storage_not_private");
        }
        let parent = fs::symlink_metadata(directory.parent().ok_or("restore_storage_not_private")?)
            .map_err(|_| "restore_storage_not_private")?;
        if !parent.is_dir() || parent.uid() != uid || parent.mode() & 0o022 != 0 {
            return Err("restore_storage_not_private");
        }
        Ok(Self { directory, uid })
    }
    fn path(&self, _source: SourceType) -> PathBuf {
        // Retain only the most recently approved source, never silently switch
        // back to an older monitor when the user last selected a window.
        self.directory.join("authorization.json")
    }
    fn load(&self, owner: &str, source: SourceType) -> Option<Authorization> {
        let path = self.path(source);
        let metadata = fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file()
            || metadata.uid() != self.uid
            || metadata.mode() & 0o077 != 0
            || metadata.len() > MAX_BYTES
        {
            return None;
        }
        let file = fs::File::open(&path).ok()?;
        let opened = file.metadata().ok()?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > MAX_BYTES {
            return None;
        }
        let record: Authorization = serde_json::from_slice(&bytes).ok()?;
        (record.owner == owner && record.source == source && valid_token(&record.token))
            .then_some(record)
    }
    pub fn available(&self, owner: &str, source: SourceType) -> bool {
        self.load(owner, source).is_some()
    }
    pub fn take(&self, owner: &str, source: SourceType) -> Option<String> {
        let record = self.load(owner, source)?;
        // Tokens are single-use. Consume locally before attempting the portal,
        // including cancellation and failed starts, rather than replaying one.
        fs::remove_file(self.path(source)).ok()?;
        Some(record.token)
    }
    pub fn put(&self, owner: &str, source: SourceType, token: &str) -> CaptureResult<()> {
        if !valid_token(token) {
            return Err("invalid_restore_token");
        }
        let bytes = serde_json::to_vec(&Authorization {
            owner: owner.into(),
            source,
            token: token.into(),
        })
        .map_err(|_| "restore_storage_unavailable")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("invalid_restore_token");
        }
        let mut random = [0; 8];
        getrandom::fill(&mut random).map_err(|_| "restore_storage_unavailable")?;
        let temporary = self
            .directory
            .join(format!(".token-{:016x}", u64::from_ne_bytes(random)));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, self.path(source))?;
            fs::File::open(&self.directory)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|_| "restore_storage_unavailable")
    }
}
fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 4096 && !token.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_are_private_owner_bound_single_use_and_rotated() {
        let path = std::env::temp_dir().join(format!(
            "amaterasu-token-{}-{}",
            std::process::id(),
            crate::model::new_id().unwrap()
        ));
        let store = TokenStore::new(path.join("portal")).unwrap();
        store.put("video", SourceType::Monitor, "first").unwrap();
        assert!(!store.available("other", SourceType::Monitor));
        assert!(!store.available("video", SourceType::Window));
        assert_eq!(
            store.take("video", SourceType::Monitor).as_deref(),
            Some("first")
        );
        assert_eq!(store.take("video", SourceType::Monitor), None);
        store.put("video", SourceType::Monitor, "second").unwrap();
        assert_eq!(
            fs::metadata(store.path(SourceType::Monitor))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            store.take("video", SourceType::Monitor).as_deref(),
            Some("second")
        );
        assert!(
            store
                .put("video", SourceType::Monitor, "bad\nvalue")
                .is_err()
        );
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn token_symlinks_and_public_credentials_are_never_consumed() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let path = std::env::temp_dir().join(format!(
            "amaterasu-token-{}-{}",
            std::process::id(),
            crate::model::new_id().unwrap()
        ));
        let store = TokenStore::new(path.join("portal")).unwrap();
        store.put("video", SourceType::Window, "private").unwrap();
        let target = path.join("target.json");
        fs::rename(store.path(SourceType::Window), &target).unwrap();
        symlink(&target, store.path(SourceType::Monitor)).unwrap();
        assert!(store.take("video", SourceType::Monitor).is_none());
        assert!(target.exists());
        fs::remove_file(store.path(SourceType::Monitor)).unwrap();
        fs::rename(target, store.path(SourceType::Window)).unwrap();
        fs::set_permissions(
            store.path(SourceType::Window),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(store.take("video", SourceType::Window).is_none());
        fs::remove_dir_all(path).unwrap();
    }
}
