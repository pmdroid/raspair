use super::identity::Identity;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const RECORD_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 16 * 1024;
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    PairSetupPin,
    Password,
    LegacyPin,
}

#[derive(Clone)]
pub struct PairingRecord {
    pub method: AuthMethod,
    pub receiver_id: String,
    pub controller_id: String,
    pub accessory_id: String,
    pub controller_key: Identity,
    pub accessory_public_key: [u8; 32],
    pub password: Option<String>,
}

impl fmt::Debug for PairingRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingRecord")
            .field("method", &self.method)
            .field("receiver_id", &self.receiver_id)
            .field("controller_id", &self.controller_id)
            .field("accessory_id", &self.accessory_id)
            .field("controller_key", &"<redacted>")
            .field("accessory_public_key", &hex::encode(self.accessory_public_key))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug)]
pub enum Stored {
    Paired(PairingRecord),
    Missing,
    RePairRequired { legacy_key: PathBuf },
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no state directory: pass one explicitly, set AIRPLAY_CONFIG_DIR or provide a home directory")]
    NoStateDir,
    #[error("refusing unsafe credential path {0}")]
    UnsafePath(PathBuf),
    #[error("credential record {0} exceeds the size limit")]
    TooLarge(PathBuf),
    #[error("malformed credential record {path} at line {line} column {column}")]
    Malformed { path: PathBuf, line: usize, column: usize },
    #[error("invalid {field} in credential record {path}")]
    InvalidField { path: PathBuf, field: &'static str },
    #[error("unsupported credential record version {version} in {path}")]
    UnsupportedVersion { path: PathBuf, version: u32 },
    #[error("credential store I/O on {path}")]
    Io { path: PathBuf, #[source] source: io::Error },
}

pub trait CredentialStore: Send + Sync {
    fn load(&self, receiver_id: &str) -> Result<Stored, StoreError>;
    fn save(&self, record: &PairingRecord) -> Result<(), StoreError>;
    fn forget(&self, receiver_id: &str) -> Result<(), StoreError>;
}

pub fn resolve_state_dir(explicit: Option<&Path>) -> Result<PathBuf, StoreError> {
    resolve_from(explicit, std::env::var_os("AIRPLAY_CONFIG_DIR"), dirs::home_dir())
}

fn resolve_from(explicit: Option<&Path>, env: Option<OsString>, home: Option<PathBuf>) -> Result<PathBuf, StoreError> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    if let Some(dir) = env.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    home.map(|h| h.join(".airplay")).ok_or(StoreError::NoStateDir)
}

#[derive(Default)]
pub struct MemoryStore {
    records: Mutex<HashMap<String, PairingRecord>>,
}

impl CredentialStore for MemoryStore {
    fn load(&self, receiver_id: &str) -> Result<Stored, StoreError> {
        Ok(match self.records.lock().unwrap().get(receiver_id) {
            Some(r) => Stored::Paired(r.clone()),
            None => Stored::Missing,
        })
    }
    fn save(&self, record: &PairingRecord) -> Result<(), StoreError> {
        self.records.lock().unwrap().insert(record.receiver_id.clone(), record.clone());
        Ok(())
    }
    fn forget(&self, receiver_id: &str) -> Result<(), StoreError> {
        self.records.lock().unwrap().remove(receiver_id);
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordFile {
    version: u32,
    method: AuthMethod,
    receiver_id: String,
    controller_id: String,
    accessory_id: String,
    controller_seed: String,
    accessory_public_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

#[derive(Deserialize)]
struct Header {
    version: u32,
}

pub struct FileStore {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl FileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), lock: Mutex::new(()) }
    }

    pub fn open_default() -> Result<Self, StoreError> {
        Ok(Self::new(resolve_state_dir(None)?))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn record_path(&self, receiver_id: &str) -> PathBuf {
        let digest = Sha256::digest(receiver_id.as_bytes());
        self.dir.join(format!("pairing-{}.json", hex::encode(&digest[..16])))
    }

    fn legacy_path(&self, receiver_id: &str) -> Option<PathBuf> {
        let safe = !receiver_id.is_empty()
            && receiver_id.len() <= 64
            && receiver_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_:".contains(&b));
        safe.then(|| self.dir.join(format!("{receiver_id}.pem")))
    }

    fn check_dir(&self) -> Result<bool, StoreError> {
        match fs::symlink_metadata(&self.dir) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => Err(StoreError::UnsafePath(self.dir.clone())),
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err(&self.dir, e)),
        }
    }

    fn ensure_dir(&self) -> Result<(), StoreError> {
        if !self.check_dir()? {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir).map_err(|e| io_err(&self.dir, e))?;
            self.check_dir()?;
        }
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700)).map_err(|e| io_err(&self.dir, e))
    }

    fn read_record(&self, path: &Path) -> Result<Option<PairingRecord>, StoreError> {
        let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(StoreError::UnsafePath(path.into())),
            Err(e) => return Err(io_err(path, e)),
        };
        if !file.metadata().map_err(|e| io_err(path, e))?.is_file() {
            return Err(StoreError::UnsafePath(path.into()));
        }
        let mut data = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut data).map_err(|e| io_err(path, e))?;
        if data.len() as u64 > MAX_RECORD_BYTES {
            return Err(StoreError::TooLarge(path.into()));
        }
        let malformed = |e: serde_json::Error| StoreError::Malformed { path: path.into(), line: e.line(), column: e.column() };
        let header: Header = serde_json::from_slice(&data).map_err(malformed)?;
        if header.version != RECORD_VERSION {
            return Err(StoreError::UnsupportedVersion { path: path.into(), version: header.version });
        }
        let raw: RecordFile = serde_json::from_slice(&data).map_err(malformed)?;
        let invalid = |field| StoreError::InvalidField { path: path.into(), field };
        let seed: [u8; 32] = decode32(&raw.controller_seed).ok_or_else(|| invalid("controller_seed"))?;
        let accessory_public_key = decode32(&raw.accessory_public_key)
            .filter(|k| ed25519_dalek::VerifyingKey::from_bytes(k).is_ok())
            .ok_or_else(|| invalid("accessory_public_key"))?;
        Ok(Some(PairingRecord {
            method: raw.method,
            receiver_id: raw.receiver_id,
            controller_id: raw.controller_id,
            accessory_id: raw.accessory_id,
            controller_key: Identity::from_seed(&seed),
            accessory_public_key,
            password: raw.password,
        }))
    }
}

impl CredentialStore for FileStore {
    fn load(&self, receiver_id: &str) -> Result<Stored, StoreError> {
        if !self.check_dir()? {
            return Ok(Stored::Missing);
        }
        let path = self.record_path(receiver_id);
        if let Some(record) = self.read_record(&path)? {
            if record.receiver_id != receiver_id {
                return Err(StoreError::InvalidField { path, field: "receiver_id" });
            }
            return Ok(Stored::Paired(record));
        }
        match self.legacy_path(receiver_id) {
            Some(p) if fs::symlink_metadata(&p).map(|m| m.is_file()).unwrap_or(false) => {
                Ok(Stored::RePairRequired { legacy_key: p })
            }
            _ => Ok(Stored::Missing),
        }
    }

    fn save(&self, record: &PairingRecord) -> Result<(), StoreError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure_dir()?;
        let body = serde_json::to_vec_pretty(&RecordFile {
            version: RECORD_VERSION,
            method: record.method,
            receiver_id: record.receiver_id.clone(),
            controller_id: record.controller_id.clone(),
            accessory_id: record.accessory_id.clone(),
            controller_seed: hex::encode(record.controller_key.seed()),
            accessory_public_key: hex::encode(record.accessory_public_key),
            password: record.password.clone(),
        })
        .map_err(|e| io_err(&self.dir, e.into()))?;
        let target = self.record_path(&record.receiver_id);
        let tmp = self.dir.join(format!(
            ".{}.{}.{}.{}.tmp",
            target.file_name().unwrap().to_string_lossy(),
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
            rand::random::<u32>()
        ));
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)?;
            file.write_all(&body)?;
            file.sync_all()?;
            fs::rename(&tmp, &target)?;
            File::open(&self.dir)?.sync_all()
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(io_err(&target, e));
        }
        Ok(())
    }

    fn forget(&self, receiver_id: &str) -> Result<(), StoreError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if !self.check_dir()? {
            return Ok(());
        }
        let path = self.record_path(receiver_id);
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_err(&path, e)),
            _ => Ok(()),
        }
    }
}

fn decode32(text: &str) -> Option<[u8; 32]> {
    hex::decode(text).ok()?.try_into().ok()
}

fn io_err(path: &Path, source: io::Error) -> StoreError {
    StoreError::Io { path: path.into(), source }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("airplay-store-{}-{}", std::process::id(), rand::random::<u64>()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record(receiver_id: &str) -> PairingRecord {
        let key = Identity::generate();
        PairingRecord {
            method: AuthMethod::Password,
            receiver_id: receiver_id.into(),
            controller_id: hex::encode(key.public_key()),
            accessory_id: format!("acc-{receiver_id}"),
            accessory_public_key: Identity::generate().public_key(),
            controller_key: key,
            password: Some("hunter2-secret".into()),
        }
    }

    fn paired(store: &dyn CredentialStore, id: &str) -> PairingRecord {
        match store.load(id).unwrap() {
            Stored::Paired(r) => r,
            other => panic!("expected record, got {other:?}"),
        }
    }

    fn files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn round_trip_survives_restart_with_restricted_permissions() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("state");
        let saved = record("AA:BB:CC:DD:EE:FF");
        FileStore::new(&dir).save(&saved).unwrap();
        let loaded = paired(&FileStore::new(&dir), "AA:BB:CC:DD:EE:FF");
        assert_eq!(loaded.controller_key.seed(), saved.controller_key.seed());
        assert_eq!(loaded.accessory_public_key, saved.accessory_public_key);
        assert_eq!(loaded.password, saved.password);
        assert_eq!((loaded.method, &loaded.accessory_id), (saved.method, &saved.accessory_id));
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let path = FileStore::new(&dir).record_path("AA:BB:CC:DD:EE:FF");
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        FileStore::new(&dir).forget("AA:BB:CC:DD:EE:FF").unwrap();
        assert!(matches!(FileStore::new(&dir).load("AA:BB:CC:DD:EE:FF").unwrap(), Stored::Missing));
    }

    #[test]
    fn malformed_oversize_and_foreign_versions_are_rejected_without_secrets() {
        let tmp = TempDir::new();
        let store = FileStore::new(&tmp.0);
        let path = store.record_path("dev");
        fs::write(&path, b"{\"version\":1,\"password\":\"hunter2-secret\"").unwrap();
        let err = store.load("dev").unwrap_err();
        assert!(matches!(err, StoreError::Malformed { .. }));
        assert!(!format!("{err} {err:?}").contains("hunter2"));
        fs::write(&path, br#"{"version":1,"method":"password","receiver_id":"dev","controller_id":"c","accessory_id":"a","controller_seed":"zz-hunter2","accessory_public_key":"00"}"#).unwrap();
        let err = store.load("dev").unwrap_err();
        assert!(matches!(err, StoreError::InvalidField { field: "controller_seed", .. }));
        assert!(!format!("{err} {err:?}").contains("hunter2"));
        fs::write(&path, br#"{"version":7}"#).unwrap();
        assert!(matches!(store.load("dev"), Err(StoreError::UnsupportedVersion { version: 7, .. })));
        fs::write(&path, vec![b' '; MAX_RECORD_BYTES as usize + 1]).unwrap();
        assert!(matches!(store.load("dev"), Err(StoreError::TooLarge(_))));
    }

    #[test]
    fn path_like_ids_stay_inside_the_state_dir_and_distinct() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("state");
        let store = FileStore::new(&dir);
        for id in ["../escape", "/etc/passwd", "a/b", "..", "Living Room", "living room"] {
            store.save(&record(id)).unwrap();
            assert_eq!(paired(&store, id).receiver_id, id);
        }
        assert_eq!(files(&tmp.0), vec!["state".to_string()]);
        assert_eq!(files(&dir).len(), 6);
        assert!(files(&dir).iter().all(|f| f.starts_with("pairing-") && f.ends_with(".json")));
    }

    #[test]
    fn same_display_name_receivers_stay_separate() {
        let store = FileStore::new(TempDir::new().0.join("s"));
        let (a, b) = (record("11:11:11:11:11:11"), record("22:22:22:22:22:22"));
        store.save(&a).unwrap();
        store.save(&b).unwrap();
        assert_eq!(paired(&store, &a.receiver_id).controller_key.seed(), a.controller_key.seed());
        assert_eq!(paired(&store, &b.receiver_id).controller_key.seed(), b.controller_key.seed());
        let mem = MemoryStore::default();
        mem.save(&a).unwrap();
        mem.save(&b).unwrap();
        assert_eq!(paired(&mem, &a.receiver_id).accessory_id, a.accessory_id);
        mem.forget(&a.receiver_id).unwrap();
        assert!(matches!(mem.load(&a.receiver_id).unwrap(), Stored::Missing));
    }

    #[test]
    fn symlinks_are_never_followed() {
        let tmp = TempDir::new();
        let outside = tmp.0.join("outside");
        fs::write(&outside, b"untouched").unwrap();
        let dir = tmp.0.join("state");
        let store = FileStore::new(&dir);
        store.ensure_dir().unwrap();
        let path = store.record_path("dev");
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(matches!(store.load("dev"), Err(StoreError::UnsafePath(_))));
        store.save(&record("dev")).unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        assert!(!fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        let link = tmp.0.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(matches!(FileStore::new(&link).save(&record("dev")), Err(StoreError::UnsafePath(_))));
    }

    #[test]
    fn interrupted_replacement_keeps_prior_record() {
        let tmp = TempDir::new();
        let store = FileStore::new(&tmp.0);
        let first = record("dev");
        store.save(&first).unwrap();
        let name = store.record_path("dev").file_name().unwrap().to_string_lossy().into_owned();
        fs::write(tmp.0.join(format!(".{name}.1.0.0.tmp")), b"{\"version\":1,\"meth").unwrap();
        assert_eq!(paired(&store, "dev").controller_key.seed(), first.controller_key.seed());
        let second = record("dev");
        store.save(&second).unwrap();
        assert_eq!(paired(&store, "dev").controller_key.seed(), second.controller_key.seed());
    }

    #[test]
    fn concurrent_writes_never_mix_credentials() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("state");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let store = FileStore::new(dir);
                    for _ in 0..20 {
                        store.save(&record("dev")).unwrap();
                        let r = paired(&store, "dev");
                        assert_eq!(r.controller_id, hex::encode(r.controller_key.public_key()));
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let r = paired(&FileStore::new(&dir), "dev");
        assert_eq!(r.controller_id, hex::encode(r.controller_key.public_key()));
        assert_eq!(files(&dir).len(), 1);
    }

    #[test]
    fn legacy_pem_requires_re_pairing_and_is_preserved() {
        let tmp = TempDir::new();
        let legacy = tmp.0.join("AA:BB:CC:DD:EE:FF.pem");
        fs::write(&legacy, b"-----BEGIN ED25519 PRIVATE KEY-----").unwrap();
        let store = FileStore::new(&tmp.0);
        assert!(matches!(store.load("AA:BB:CC:DD:EE:FF").unwrap(), Stored::RePairRequired { legacy_key } if legacy_key == legacy));
        assert!(matches!(store.load("../AA:BB:CC:DD:EE:FF").unwrap(), Stored::Missing));
        store.forget("AA:BB:CC:DD:EE:FF").unwrap();
        assert!(legacy.exists());
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let r = record("dev");
        let text = format!("{r:?} {:?}", Stored::Paired(r.clone()));
        assert!(!text.contains("hunter2"));
        assert!(!text.contains(&hex::encode(r.controller_key.seed())));
        assert!(text.contains("<redacted>"));
    }

    #[test]
    fn state_dir_precedence() {
        let home = Some(PathBuf::from("/home/u"));
        let env = Some(OsString::from("/env"));
        assert_eq!(resolve_from(Some(Path::new("/x")), env.clone(), home.clone()).unwrap(), PathBuf::from("/x"));
        assert_eq!(resolve_from(None, env, home.clone()).unwrap(), PathBuf::from("/env"));
        assert_eq!(resolve_from(None, Some(OsString::new()), home).unwrap(), PathBuf::from("/home/u/.airplay"));
        assert!(matches!(resolve_from(None, None, None), Err(StoreError::NoStateDir)));
    }
}
