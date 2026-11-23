use anyhow::{bail, Context, Result};
use std::path::Path;

pub const SERVICE_USER: &str = "vinyl-bridge";
pub const BIN: &str = "/usr/local/bin/vinyl-bridge-pi";
pub const ETC: &str = "/etc/vinyl-bridge";
pub const CONFIG: &str = "/etc/vinyl-bridge/config.toml";
pub const TLS: &str = "/etc/vinyl-bridge/tls";
pub const CERT: &str = "/etc/vinyl-bridge/tls/cert.pem";
pub const KEY: &str = "/etc/vinyl-bridge/tls/key.pem";
pub const UNIT: &str = "/etc/systemd/system/vinyl-bridge.service";
pub const HELPER_UNIT: &str = "/etc/systemd/system/vinyl-bridge-helper.service";
pub const STATE: &str = "/var/lib/vinyl-bridge";
pub const REC: &str = "/var/lib/vinyl-bridge/rec";
pub const CREDENTIAL: &str = "/var/lib/vinyl-bridge/setup-credential";

const UNIT_TEXT: &str = include_str!("../deploy/systemd/vinyl-bridge.service");
const HELPER_UNIT_TEXT: &str = include_str!("../deploy/systemd/vinyl-bridge-helper.service");
const CONFIG_TEXT: &str = include_str!("../config.example.toml");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Root,
    RootServiceGroup,
    Service,
}

pub trait System {
    fn exists(&self, path: &Path) -> bool;
    fn read(&self, path: &Path) -> Option<Vec<u8>>;
    fn user_exists(&self, name: &str) -> bool;
    fn mkdir(&mut self, path: &Path, mode: u32, owner: Owner) -> Result<()>;
    fn write(&mut self, path: &Path, data: &[u8], mode: u32, owner: Owner) -> Result<()>;
    fn set_owner(&mut self, path: &Path, mode: u32, owner: Owner) -> Result<()>;
    fn run(&mut self, argv: &[&str]) -> Result<()>;
}

pub struct Outcome {
    pub url: String,
    pub credential: Option<String>,
}

pub fn install(sys: &mut dyn System, exe: &[u8]) -> Result<Outcome> {
    if !sys.user_exists(SERVICE_USER) {
        sys.run(&["useradd", "--system", "--user-group", "--home-dir", STATE, "--no-create-home", "--shell", "/usr/sbin/nologin", "--groups", "audio", SERVICE_USER])?;
    }
    if sys.read(Path::new(BIN)).as_deref() != Some(exe) {
        sys.write(Path::new(BIN), exe, 0o755, Owner::Root)?;
    }
    sys.set_owner(Path::new(BIN), 0o755, Owner::Root)?;
    root_dir(sys, ETC, 0o755, Owner::Root)?;
    if !sys.exists(Path::new(CONFIG)) {
        sys.write(Path::new(CONFIG), CONFIG_TEXT.as_bytes(), 0o644, Owner::Root)?;
    }
    sys.set_owner(Path::new(CONFIG), 0o644, Owner::Root)?;
    root_dir(sys, TLS, 0o750, Owner::RootServiceGroup)?;
    match (sys.exists(Path::new(CERT)), sys.exists(Path::new(KEY))) {
        (true, true) => {}
        (false, false) => sys.run(&["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-days", "3650", "-subj", "/CN=vinyl-bridge.local", "-addext", "subjectAltName=DNS:vinyl-bridge.local", "-keyout", KEY, "-out", CERT])?,
        _ => bail!("{TLS} holds only one of cert.pem and key.pem; restore or remove it and rerun install"),
    }
    sys.set_owner(Path::new(CERT), 0o644, Owner::Root)?;
    sys.set_owner(Path::new(KEY), 0o640, Owner::RootServiceGroup)?;
    sys.write(Path::new(UNIT), UNIT_TEXT.as_bytes(), 0o644, Owner::Root)?;
    sys.write(Path::new(HELPER_UNIT), HELPER_UNIT_TEXT.as_bytes(), 0o644, Owner::Root)?;
    for dir in [STATE, REC] {
        if !sys.exists(Path::new(dir)) {
            sys.mkdir(Path::new(dir), 0o750, Owner::Service)?;
        }
    }
    if !sys.exists(Path::new(CREDENTIAL)) {
        let token = uuid::Uuid::new_v4().simple().to_string();
        sys.write(Path::new(CREDENTIAL), token.as_bytes(), 0o600, Owner::Service)?;
    }
    let credential = sys.read(Path::new(CREDENTIAL)).map(|c| String::from_utf8_lossy(&c).trim().to_string());
    sys.run(&["systemctl", "daemon-reload"])?;
    sys.run(&["systemctl", "enable", "vinyl-bridge-helper.service", "vinyl-bridge.service"])?;
    sys.run(&["systemctl", "restart", "vinyl-bridge-helper.service"])?;
    sys.run(&["systemctl", "restart", "vinyl-bridge.service"])?;
    let cfg: crate::config::Config = sys.read(Path::new(CONFIG)).and_then(|t| toml::from_str(&String::from_utf8_lossy(&t)).ok()).unwrap_or_default();
    let port = cfg.http_bind.rsplit_once(':').map(|(_, p)| p).filter(|p| *p != "80").map(|p| format!(":{p}")).unwrap_or_default();
    Ok(Outcome { url: format!("http://{}.local{port}/", cfg.mdns_hostname), credential })
}

fn root_dir(sys: &mut dyn System, dir: &str, mode: u32, owner: Owner) -> Result<()> {
    if !sys.exists(Path::new(dir)) {
        sys.mkdir(Path::new(dir), mode, owner)?;
    }
    sys.set_owner(Path::new(dir), mode, owner)
}

pub fn service_ids() -> Result<(u32, u32)> {
    let name = std::ffi::CString::new(SERVICE_USER)?;
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        bail!("user {SERVICE_USER} not found");
    }
    Ok(unsafe { ((*pw).pw_uid, (*pw).pw_gid) })
}

pub struct HostSystem;

impl HostSystem {
    fn ids(owner: Owner) -> Result<(u32, u32)> {
        if owner == Owner::Root {
            return Ok((0, 0));
        }
        let (uid, gid) = service_ids()?;
        Ok(if owner == Owner::Service { (uid, gid) } else { (0, gid) })
    }
}

impl System for HostSystem {
    fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    fn user_exists(&self, name: &str) -> bool {
        let name = std::ffi::CString::new(name).unwrap_or_default();
        !unsafe { libc::getpwnam(name.as_ptr()) }.is_null()
    }

    fn mkdir(&mut self, path: &Path, mode: u32, owner: Owner) -> Result<()> {
        std::fs::create_dir(path).with_context(|| format!("mkdir {}", path.display()))?;
        self.set_owner(path, mode, owner)
    }

    fn write(&mut self, path: &Path, data: &[u8], mode: u32, owner: Owner) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{fchown, OpenOptionsExt, PermissionsExt};
        let tmp = path.with_extension("install-tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let (uid, gid) = Self::ids(owner)?;
        file.write_all(data)?;
        fchown(&file, Some(uid), Some(gid))?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
    }

    fn set_owner(&mut self, path: &Path, mode: u32, owner: Owner) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        if path.symlink_metadata()?.file_type().is_symlink() {
            bail!("{} is a symlink", path.display());
        }
        let (uid, gid) = Self::ids(owner)?;
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).with_context(|| format!("chmod {}", path.display()))
    }

    fn run(&mut self, argv: &[&str]) -> Result<()> {
        let status = std::process::Command::new(argv[0]).args(&argv[1..]).status().with_context(|| format!("run {}", argv[0]))?;
        if !status.success() {
            bail!("{} failed: {status}", argv.join(" "));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    #[derive(Clone, Debug, PartialEq)]
    struct Entry {
        data: Option<Vec<u8>>,
        mode: u32,
        owner: Owner,
    }

    #[derive(Default)]
    struct FakeSystem {
        files: BTreeMap<PathBuf, Entry>,
        users: BTreeSet<String>,
        commands: Vec<String>,
    }

    impl FakeSystem {
        fn put(&mut self, path: &str, data: Option<&str>, mode: u32, owner: Owner) {
            self.files.insert(path.into(), Entry { data: data.map(|d| d.as_bytes().to_vec()), mode, owner });
        }

        fn entry(&self, path: &str) -> &Entry {
            &self.files[Path::new(path)]
        }
    }

    impl System for FakeSystem {
        fn exists(&self, path: &Path) -> bool {
            self.files.contains_key(path)
        }

        fn read(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.get(path).and_then(|e| e.data.clone())
        }

        fn user_exists(&self, name: &str) -> bool {
            self.users.contains(name)
        }

        fn mkdir(&mut self, path: &Path, mode: u32, owner: Owner) -> Result<()> {
            assert!(self.files.insert(path.into(), Entry { data: None, mode, owner }).is_none());
            Ok(())
        }

        fn write(&mut self, path: &Path, data: &[u8], mode: u32, owner: Owner) -> Result<()> {
            self.files.insert(path.into(), Entry { data: Some(data.to_vec()), mode, owner });
            Ok(())
        }

        fn set_owner(&mut self, path: &Path, mode: u32, owner: Owner) -> Result<()> {
            let entry = self.files.get_mut(path).context("missing")?;
            entry.mode = mode;
            entry.owner = owner;
            Ok(())
        }

        fn run(&mut self, argv: &[&str]) -> Result<()> {
            match argv[0] {
                "useradd" => drop(self.users.insert(SERVICE_USER.into())),
                "openssl" => {
                    self.put(CERT, Some("cert"), 0o644, Owner::Root);
                    self.put(KEY, Some("key"), 0o600, Owner::Root);
                }
                _ => {}
            }
            self.commands.push(argv.join(" "));
            Ok(())
        }
    }

    #[test]
    fn fresh_install_plans_root_owned_binary_and_unprivileged_unit() {
        let mut sys = FakeSystem::default();
        let out = install(&mut sys, b"exe").unwrap();
        assert_eq!(out.url, "http://vinyl-bridge.local/");
        assert_eq!(out.credential.as_deref().map(str::len), Some(32));
        assert!(sys.commands[0].starts_with("useradd --system"));
        assert_eq!(sys.commands.iter().filter(|c| c.starts_with("openssl")).count(), 1);
        assert_eq!(sys.commands.last().unwrap(), "systemctl restart vinyl-bridge.service");
        let unit = String::from_utf8(sys.entry(UNIT).data.clone().unwrap()).unwrap();
        assert!(unit.contains("\nUser=vinyl-bridge\n") && unit.contains("\nStateDirectory=vinyl-bridge\n"));
        assert!(unit.contains(&format!("\nExecStart={BIN} --config {CONFIG}\n")));
        assert!(!unit.contains("User=root") && !unit.contains("ReadWritePaths"));
        let helper = String::from_utf8(sys.entry(HELPER_UNIT).data.clone().unwrap()).unwrap();
        assert!(helper.contains(&format!("\nExecStart={BIN} helper\n")) && helper.contains("\nRuntimeDirectory=vinyl-bridge-helper\n"));
        assert!(helper.contains("\nRuntimeDirectoryMode=0755\n") && !helper.contains("User="));
        assert!(sys.commands.contains(&"systemctl enable vinyl-bridge-helper.service vinyl-bridge.service".to_string()));
        assert!(sys.commands.contains(&"systemctl restart vinyl-bridge-helper.service".to_string()));
        for (path, mode, owner) in [(BIN, 0o755, Owner::Root), (ETC, 0o755, Owner::Root), (CONFIG, 0o644, Owner::Root), (TLS, 0o750, Owner::RootServiceGroup), (CERT, 0o644, Owner::Root), (KEY, 0o640, Owner::RootServiceGroup), (UNIT, 0o644, Owner::Root), (HELPER_UNIT, 0o644, Owner::Root), (STATE, 0o750, Owner::Service), (REC, 0o750, Owner::Service), (CREDENTIAL, 0o600, Owner::Service)] {
            let e = sys.entry(path);
            assert_eq!((e.mode, e.owner), (mode, owner), "{path}");
        }
    }

    #[test]
    fn service_account_cannot_write_privileged_paths() {
        let mut sys = FakeSystem::default();
        install(&mut sys, b"exe").unwrap();
        for (path, e) in &sys.files {
            let privileged = [BIN, ETC, UNIT, HELPER_UNIT].iter().any(|p| Path::new(p).starts_with(path) || path.starts_with(p));
            if privileged {
                assert_ne!(e.owner, Owner::Service, "{}", path.display());
                assert_eq!(e.mode & 0o022, 0, "{}", path.display());
            }
            if e.owner == Owner::Service {
                assert!(path.starts_with(STATE), "{}", path.display());
            }
        }
        assert!(!Path::new(BIN).starts_with(STATE) && !Path::new(ETC).starts_with(STATE));
    }

    #[test]
    fn reinstall_preserves_state_certificate_and_credential() {
        let mut sys = FakeSystem::default();
        sys.users.insert(SERVICE_USER.into());
        sys.put(STATE, None, 0o750, Owner::Service);
        sys.put(REC, None, 0o750, Owner::Service);
        sys.put("/var/lib/vinyl-bridge/settings.json", Some("{\"device_name\":\"Den\"}"), 0o644, Owner::Service);
        sys.put("/var/lib/vinyl-bridge/rec/side-a.flac", Some("fLaC"), 0o644, Owner::Service);
        sys.put("/var/lib/vinyl-bridge/paired-devices.json", Some("[\"phone\"]"), 0o600, Owner::Service);
        sys.put(CONFIG, Some("http_bind = \"0.0.0.0:8080\"\nmdns_hostname = \"den\"\n"), 0o644, Owner::Root);
        sys.put(CERT, Some("old-cert"), 0o644, Owner::Root);
        sys.put(KEY, Some("old-key"), 0o640, Owner::RootServiceGroup);
        let before: BTreeMap<_, _> = sys.files.iter().filter(|(p, _)| p.starts_with(STATE) || p.starts_with(ETC)).map(|(p, e)| (p.clone(), e.clone())).collect();
        let first = install(&mut sys, b"exe-v1").unwrap();
        let after_first = sys.files.clone();
        let second = install(&mut sys, b"exe-v2").unwrap();
        assert_eq!(first.url, "http://den.local:8080/");
        assert_eq!(first.credential, second.credential);
        for (path, entry) in &before {
            assert_eq!(&sys.files[path], entry, "{}", path.display());
        }
        sys.files.remove(Path::new(BIN));
        let mut expected = after_first;
        expected.remove(Path::new(BIN));
        assert_eq!(sys.files, expected);
        assert!(!sys.commands.iter().any(|c| c.starts_with("useradd") || c.starts_with("openssl")));
    }

    #[test]
    fn half_present_certificate_is_not_regenerated() {
        let mut sys = FakeSystem::default();
        sys.put(KEY, Some("key"), 0o640, Owner::RootServiceGroup);
        assert!(install(&mut sys, b"exe").is_err());
        assert_eq!(sys.entry(KEY).data.as_deref(), Some(&b"key"[..]));
    }
}
