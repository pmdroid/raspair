use airplay_core::client::Client;
use airplay_core::discovery::{self, Device};
use airplay_core::pairing::{
    AuthMethod, CredentialStore, PairingError, PinChallenge, PinPairings, PinSource, Stored,
};
use airplay_core::protocol::{AuthRequirement, HOMEPOD_PIN};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

pub const CACHE_AGE: Duration = Duration::from_secs(1800);

pub trait Backend {
    fn devices(&mut self, max_age: Duration) -> Result<Vec<Device>>;
    fn verify(&mut self, dev: &Device, password: Option<&str>) -> Result<()>;
    fn begin(&mut self, dev: &Device) -> Result<PinChallenge>;
    fn respond(&mut self, challenge: &PinChallenge, code: &str) -> Result<()>;
    fn cancel(&mut self, challenge: &PinChallenge);
}

pub trait Prompt {
    fn interactive(&self) -> bool;
    fn secret(&mut self, label: &str) -> Result<String>;
}

pub struct Live {
    store: Arc<dyn CredentialStore>,
    pairings: PinPairings,
}

impl Live {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self { pairings: PinPairings::new(store.clone()), store }
    }
}

impl Backend for Live {
    fn devices(&mut self, max_age: Duration) -> Result<Vec<Device>> {
        discovery::load_or_discover("", Duration::from_secs(5), max_age)
    }

    fn verify(&mut self, dev: &Device, password: Option<&str>) -> Result<()> {
        let mut client = Client::new();
        client.set_credential_store(self.store.clone());
        if let Some(p) = password {
            client.set_password(p);
        }
        client.connect(dev, HOMEPOD_PIN)?;
        client.stop().ok();
        Ok(())
    }

    fn begin(&mut self, dev: &Device) -> Result<PinChallenge> {
        Client::begin_pairing(dev, &self.pairings)
    }

    fn respond(&mut self, challenge: &PinChallenge, code: &str) -> Result<()> {
        Ok(self.pairings.respond(&challenge.id, code)?)
    }

    fn cancel(&mut self, challenge: &PinChallenge) {
        let _ = self.pairings.cancel(&challenge.id);
    }
}

pub struct Tty;

impl Prompt for Tty {
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    fn secret(&mut self, label: &str) -> Result<String> {
        eprint!("{label}");
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
            bail!("cannot disable terminal echo for the secret prompt");
        }
        let original = term;
        term.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) };
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &original) };
        eprintln!();
        read?;
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }
}

pub struct Cli<'a> {
    pub backend: &'a mut dyn Backend,
    pub store: &'a dyn CredentialStore,
    pub prompt: &'a mut dyn Prompt,
    pub out: &'a mut dyn Write,
    pub json: bool,
}

pub fn clean(name: &str) -> String {
    name.replace("\\ ", " ").replace("\\(", "(").replace("\\)", ")")
}

pub fn select<'d>(devices: &'d [Device], query: &str) -> Result<&'d Device> {
    let by_id = |d: &&Device| !d.record.device_id.is_empty() && d.record.device_id.eq_ignore_ascii_case(query);
    if let Some(d) = devices.iter().find(by_id) {
        return Ok(d);
    }
    let named: Vec<&Device> = devices.iter().filter(|d| clean(&d.name).eq_ignore_ascii_case(query)).collect();
    match named.as_slice() {
        [] => bail!("device {query:?} not found; run `airplay discover` and pass --device ID"),
        [one] => Ok(one),
        many => {
            let ids: Vec<&str> = many.iter().map(|d| d.record.device_id.as_str()).collect();
            bail!("device name {query:?} matches {} receivers; pass --device with one of: {}", many.len(), ids.join(", "))
        }
    }
}

pub fn auth_label(auth: &AuthRequirement) -> &'static str {
    match auth {
        AuthRequirement::None => "none",
        AuthRequirement::Password => "password",
        AuthRequirement::LegacyPin => "legacy_pin",
        AuthRequirement::HapTransient => "transient",
        AuthRequirement::HapTransientPassword => "transient_password",
        AuthRequirement::HapPersistent => "pin",
        AuthRequirement::UnifiedAuth => "unified",
        AuthRequirement::Unknown(_) => "unknown",
    }
}

pub fn method_label(method: AuthMethod) -> String {
    serde_json::to_value(method).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

pub fn pairing_error(e: &anyhow::Error) -> Option<&PairingError> {
    e.chain().find_map(|c| c.downcast_ref::<PairingError>())
}

impl Cli<'_> {
    fn saved(&self, id: &str) -> Option<AuthMethod> {
        match self.store.load(id) {
            Ok(Stored::Paired(r)) if !id.is_empty() => Some(r.method),
            _ => None,
        }
    }

    pub fn discover(&mut self) -> Result<i32> {
        let devices = self.backend.devices(Duration::ZERO)?;
        if self.json {
            let list: Vec<Value> = devices
                .iter()
                .map(|d| {
                    json!({
                        "id": d.record.device_id,
                        "name": clean(&d.name),
                        "model": d.record.model,
                        "address": d.ip(),
                        "port": d.port,
                        "auth": auth_label(&d.route().auth),
                        "paired": self.saved(&d.record.device_id).map(method_label),
                        "buffered": d.supports_buffered_audio(),
                        "stereo_pair": (!d.record.tight_sync_id.is_empty()).then_some(&d.record.tight_sync_id),
                    })
                })
                .collect();
            writeln!(self.out, "{}", json!({ "devices": list }))?;
            return Ok(0);
        }
        if devices.is_empty() {
            writeln!(self.out, "no AirPlay devices found")?;
        }
        for d in &devices {
            let paired = if self.saved(&d.record.device_id).is_some() { " paired" } else { "" };
            let buf = if d.supports_buffered_audio() { " buffered" } else { "" };
            let stereo = if d.record.tight_sync_id.is_empty() {
                String::new()
            } else {
                format!(" [pair:{:.8}]", d.record.tight_sync_id)
            };
            writeln!(
                self.out,
                "{:<17} {:<30} {:<18} {:<15} auth:{}{paired}{buf}{stereo}",
                d.record.device_id,
                clean(&d.name),
                d.record.model,
                d.ip().unwrap_or_default(),
                auth_label(&d.route().auth)
            )?;
        }
        Ok(0)
    }

    pub fn pair(&mut self, query: &str) -> Result<i32> {
        let devices = self.backend.devices(CACHE_AGE)?;
        let dev = select(&devices, query)?.clone();
        let saved = self.saved(&dev.record.device_id);
        match dev.route().auth {
            AuthRequirement::HapTransient => self.report(&dev, "transient", "pairs transiently on every connection; nothing to save"),
            AuthRequirement::HapTransientPassword => {
                if saved == Some(AuthMethod::Password) {
                    match self.backend.verify(&dev, None) {
                        Ok(()) => return self.report(&dev, "reused", "saved password accepted"),
                        Err(e) if !matches!(pairing_error(&e), Some(PairingError::WrongPassword)) => return Err(e),
                        Err(_) => {}
                    }
                }
                if !self.prompt.interactive() {
                    return self.require(&dev, "password", None);
                }
                let password = self.prompt.secret(&format!("Password for {}: ", clean(&dev.name)))?;
                if password.is_empty() {
                    bail!("no password entered");
                }
                self.backend.verify(&dev, Some(&password))?;
                self.report(&dev, "paired", "password accepted and saved")
            }
            AuthRequirement::HapPersistent => {
                if saved == Some(AuthMethod::PairSetupPin) {
                    match self.backend.verify(&dev, None) {
                        Ok(()) => return self.report(&dev, "reused", "saved pairing verified"),
                        Err(e) if !pairing_error(&e).is_some_and(|p| p.requires_re_pair()) => return Err(e),
                        Err(_) => {}
                    }
                }
                let source = if dev.record.displays_pin() { PinSource::OnScreen } else { PinSource::Static };
                if !self.prompt.interactive() {
                    return self.require(&dev, "pin", Some(source));
                }
                let challenge = self.backend.begin(&dev)?;
                let label = match challenge.source {
                    PinSource::OnScreen => format!("PIN shown on {}: ", clean(&dev.name)),
                    PinSource::Static => format!("PIN for {}: ", clean(&dev.name)),
                };
                let code = match self.prompt.secret(&label) {
                    Ok(code) if !code.trim().is_empty() => code,
                    other => {
                        self.backend.cancel(&challenge);
                        other?;
                        bail!("no PIN entered; pairing cancelled");
                    }
                };
                self.backend.respond(&challenge, code.trim())?;
                self.report(&dev, "paired", "pairing saved")
            }
            other => bail!("{} requires {} authentication, which the library does not support yet", clean(&dev.name), auth_label(&other)),
        }
    }

    pub fn paired(&mut self) -> Result<i32> {
        let devices = self.backend.devices(CACHE_AGE)?;
        let rows: Vec<(&Device, String)> = devices
            .iter()
            .filter_map(|d| self.saved(&d.record.device_id).map(|m| (d, method_label(m))))
            .collect();
        if self.json {
            let list: Vec<Value> = rows
                .iter()
                .map(|(d, m)| json!({ "id": d.record.device_id, "name": clean(&d.name), "method": m }))
                .collect();
            writeln!(self.out, "{}", json!({ "paired": list }))?;
            return Ok(0);
        }
        if rows.is_empty() {
            writeln!(self.out, "no saved pairings for known receivers")?;
        }
        for (d, m) in rows {
            writeln!(self.out, "{:<17} {:<30} {m}", d.record.device_id, clean(&d.name))?;
        }
        Ok(0)
    }

    pub fn forget(&mut self, query: &str) -> Result<i32> {
        let devices = self.backend.devices(CACHE_AGE)?;
        let (id, name) = match select(&devices, query) {
            Ok(d) => (d.record.device_id.clone(), clean(&d.name)),
            Err(_) if !matches!(self.store.load(query), Ok(Stored::Missing)) => (query.to_string(), query.to_string()),
            Err(e) => return Err(e),
        };
        let removed = !matches!(self.store.load(&id), Ok(Stored::Missing));
        self.store.forget(&id)?;
        if self.json {
            let status = if removed { "forgotten" } else { "not_paired" };
            writeln!(
                self.out,
                "{}",
                json!({ "status": status, "id": id, "name": name, "local_record_removed": removed, "remote_revocation": "unsupported" })
            )?;
        } else if removed {
            writeln!(self.out, "Removed the local pairing record for {name} ({id}).")?;
            writeln!(self.out, "The receiver was not contacted and may still list this controller; remove it in the receiver's settings to revoke it there.")?;
        } else {
            writeln!(self.out, "No local pairing record for {name} ({id}).")?;
        }
        Ok(0)
    }

    fn report(&mut self, dev: &Device, status: &str, detail: &str) -> Result<i32> {
        if self.json {
            writeln!(self.out, "{}", json!({ "status": status, "id": dev.record.device_id, "name": clean(&dev.name) }))?;
        } else {
            writeln!(self.out, "{} ({}): {detail}", clean(&dev.name), dev.record.device_id)?;
        }
        Ok(0)
    }

    fn require(&mut self, dev: &Device, challenge: &str, source: Option<PinSource>) -> Result<i32> {
        let source = source.map(|s| if s == PinSource::OnScreen { "on_screen" } else { "static" });
        if self.json {
            writeln!(
                self.out,
                "{}",
                json!({ "status": "challenge_required", "challenge": challenge, "source": source, "id": dev.record.device_id, "name": clean(&dev.name) })
            )?;
        } else {
            writeln!(
                self.out,
                "{} ({}) requires a {challenge}; run `airplay pair --device {}` in an interactive terminal",
                clean(&dev.name),
                dev.record.device_id,
                dev.record.device_id
            )?;
        }
        Ok(2)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use airplay_core::pairing::{FileStore, Identity, PairingRecord};
    use airplay_core::protocol::{FeatureFlags, TxtRecord, STATUS_PASSWORD_REQUIRED, STATUS_PIN_REQUIRED};
    use std::time::Instant;

    pub(crate) fn device(id: &str, name: &str, model: &str, flags: u64) -> Device {
        Device {
            name: name.into(),
            host: format!("{id}.local"),
            port: 7000,
            ipv4: Some("192.0.2.10".parse().unwrap()),
            ipv6: None,
            record: TxtRecord {
                device_id: id.into(),
                model: model.into(),
                features: FeatureFlags(1 << 9 | 1 << 40 | 1 << 46 | 1 << 48),
                flags,
                has_flags: true,
                ..Default::default()
            },
            last_seen: 0,
        }
    }

    pub(crate) fn apple_tv(id: &str, name: &str) -> Device {
        device(id, name, "AppleTV14,1", STATUS_PIN_REQUIRED)
    }

    pub(crate) struct Fake<'s> {
        pub(crate) devices: Vec<Device>,
        pub(crate) store: &'s dyn CredentialStore,
        pub(crate) log: Arc<std::sync::Mutex<Vec<String>>>,
        pub(crate) verify_error: Option<PairingError>,
    }

    impl Backend for Fake<'_> {
        fn devices(&mut self, _: Duration) -> Result<Vec<Device>> {
            Ok(self.devices.clone())
        }
        fn verify(&mut self, dev: &Device, password: Option<&str>) -> Result<()> {
            self.log.lock().unwrap().push(format!("verify:{}", password.is_some()));
            if let Some(e) = self.verify_error.take() {
                return Err(anyhow::Error::from(e).context("pair-verify"));
            }
            if let Some(p) = password {
                self.store.save(&record(&dev.record.device_id, AuthMethod::Password, Some(p)))?;
            }
            Ok(())
        }
        fn begin(&mut self, dev: &Device) -> Result<PinChallenge> {
            self.log.lock().unwrap().push("begin".into());
            Ok(PinChallenge {
                id: "c1".into(),
                receiver_id: dev.record.device_id.clone(),
                source: PinSource::OnScreen,
                expires_at: Instant::now() + Duration::from_secs(120),
            })
        }
        fn respond(&mut self, c: &PinChallenge, code: &str) -> Result<()> {
            self.log.lock().unwrap().push(format!("respond:{code}"));
            self.store.save(&record(&c.receiver_id, AuthMethod::PairSetupPin, None))?;
            Ok(())
        }
        fn cancel(&mut self, _: &PinChallenge) {
            self.log.lock().unwrap().push("cancel".into());
        }
    }

    pub(crate) fn record(id: &str, method: AuthMethod, password: Option<&str>) -> PairingRecord {
        PairingRecord {
            method,
            receiver_id: id.into(),
            controller_id: "controller".into(),
            accessory_id: id.into(),
            controller_key: Identity::generate(),
            accessory_public_key: (method != AuthMethod::Password).then(|| Identity::generate().public_key()),
            password: password.map(str::to_string),
        }
    }

    struct Answers {
        interactive: bool,
        answers: Vec<&'static str>,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Prompt for Answers {
        fn interactive(&self) -> bool {
            self.interactive
        }
        fn secret(&mut self, _: &str) -> Result<String> {
            self.log.lock().unwrap().push("prompt".into());
            Ok(self.answers.remove(0).to_string())
        }
    }

    struct Run {
        code: i32,
        out: String,
        log: Vec<String>,
    }

    fn run(
        store: &dyn CredentialStore,
        devices: Vec<Device>,
        answers: Option<Vec<&'static str>>,
        verify_error: Option<PairingError>,
        json: bool,
        cmd: impl FnOnce(&mut Cli) -> Result<i32>,
    ) -> Result<Run> {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut backend = Fake { devices, store, log: log.clone(), verify_error };
        let mut prompt = Answers { interactive: answers.is_some(), answers: answers.unwrap_or_default(), log: log.clone() };
        let mut out = Vec::new();
        let mut cli = Cli { backend: &mut backend, store, prompt: &mut prompt, out: &mut out, json };
        let code = cmd(&mut cli)?;
        let log = log.lock().unwrap().clone();
        Ok(Run { code, out: String::from_utf8(out).unwrap(), log })
    }

    #[test]
    fn discover_lists_duplicate_names_by_stable_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        store.save(&record("aa:01", AuthMethod::PairSetupPin, None)).unwrap();
        let devices = vec![apple_tv("aa:01", "Living\\ Room"), apple_tv("aa:02", "Living Room")];
        let r = run(&store, devices, None, None, true, |c| c.discover()).unwrap();
        let v: Value = serde_json::from_str(&r.out).unwrap();
        assert_eq!(v["devices"][0]["id"], "aa:01");
        assert_eq!(v["devices"][0]["name"], "Living Room");
        assert_eq!(v["devices"][0]["paired"], "pair_setup_pin");
        assert_eq!(v["devices"][0]["auth"], "pin");
        assert_eq!(v["devices"][1]["id"], "aa:02");
        assert_eq!(v["devices"][1]["paired"], Value::Null);
    }

    #[test]
    fn duplicate_names_are_rejected_and_ids_select() {
        let devices = vec![apple_tv("aa:01", "Living Room"), apple_tv("aa:02", "living room"), apple_tv("aa:03", "Kitchen")];
        let err = select(&devices, "Living Room").unwrap_err().to_string();
        assert!(err.contains("aa:01") && err.contains("aa:02"), "{err}");
        assert_eq!(select(&devices, "AA:02").unwrap().record.device_id, "aa:02");
        assert_eq!(select(&devices, "kitchen").unwrap().record.device_id, "aa:03");
        assert!(select(&devices, "Bedroom").is_err());
    }

    #[test]
    fn pin_challenge_begins_before_prompt_and_second_run_reuses_record() {
        let dir = tempfile::tempdir().unwrap();
        let devices = vec![apple_tv("aa:01", "Den")];
        let store = FileStore::new(dir.path());
        let first = run(&store, devices.clone(), Some(vec!["4821\n"]), None, false, |c| c.pair("Den")).unwrap();
        assert_eq!(first.code, 0);
        assert_eq!(first.log, ["begin", "prompt", "respond:4821"]);
        assert!(!first.out.contains("4821"));

        let reopened = FileStore::new(dir.path());
        let second = run(&reopened, devices, None, None, true, |c| c.pair("aa:01")).unwrap();
        assert_eq!(second.code, 0);
        assert_eq!(second.log, ["verify:false"]);
        assert_eq!(serde_json::from_str::<Value>(&second.out).unwrap()["status"], "reused");
    }

    #[test]
    fn untrusted_saved_record_starts_a_new_challenge() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        store.save(&record("aa:01", AuthMethod::PairSetupPin, None)).unwrap();
        let r = run(&store, vec![apple_tv("aa:01", "Den")], Some(vec!["1111"]), Some(PairingError::SignatureInvalid), false, |c| {
            c.pair("Den")
        })
        .unwrap();
        assert_eq!(r.log, ["verify:false", "begin", "prompt", "respond:1111"]);
    }

    #[test]
    fn empty_pin_cancels_the_challenge() {
        let store = airplay_core::pairing::MemoryStore::default();
        let err = run(&store, vec![apple_tv("aa:01", "Den")], Some(vec![""]), None, false, |c| c.pair("Den"));
        assert!(err.is_err());
        assert!(matches!(store.load("aa:01").unwrap(), Stored::Missing));
    }

    #[test]
    fn noninteractive_pairing_returns_a_requirement_without_starting() {
        let store = airplay_core::pairing::MemoryStore::default();
        let r = run(&store, vec![apple_tv("aa:01", "Den")], None, None, true, |c| c.pair("Den")).unwrap();
        assert_eq!(r.code, 2);
        assert!(r.log.is_empty());
        let v: Value = serde_json::from_str(&r.out).unwrap();
        assert_eq!(v["status"], "challenge_required");
        assert_eq!(v["challenge"], "pin");
        assert_eq!(v["source"], "on_screen");
    }

    #[test]
    fn password_is_prompted_saved_and_never_printed() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        let devices = vec![device("bb:01", "Kitchen", "AudioAccessory5,1", STATUS_PASSWORD_REQUIRED)];
        let first = run(&store, devices.clone(), Some(vec!["hunter2"]), None, true, |c| c.pair("Kitchen")).unwrap();
        assert_eq!(first.log, ["prompt", "verify:true"]);
        assert!(!first.out.contains("hunter2"));
        let second = run(&store, devices, None, None, false, |c| c.pair("Kitchen")).unwrap();
        assert_eq!(second.log, ["verify:false"]);
        let listed = run(&store, vec![device("bb:01", "Kitchen", "AudioAccessory5,1", 0)], None, None, true, |c| c.paired()).unwrap();
        assert!(!listed.out.contains("hunter2"));
        assert_eq!(serde_json::from_str::<Value>(&listed.out).unwrap()["paired"][0]["method"], "password");
    }

    #[test]
    fn forget_removes_only_the_local_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path());
        store.save(&record("aa:01", AuthMethod::PairSetupPin, None)).unwrap();
        store.save(&record("aa:02", AuthMethod::PairSetupPin, None)).unwrap();
        let r = run(&store, vec![apple_tv("aa:02", "Den")], None, None, true, |c| c.forget("aa:01")).unwrap();
        let v: Value = serde_json::from_str(&r.out).unwrap();
        assert_eq!(v["status"], "forgotten");
        assert_eq!(v["remote_revocation"], "unsupported");
        assert!(r.log.is_empty());
        assert!(matches!(store.load("aa:01").unwrap(), Stored::Missing));
        assert!(matches!(store.load("aa:02").unwrap(), Stored::Paired(_)));
        let human = run(&store, vec![apple_tv("aa:02", "Den")], None, None, false, |c| c.forget("Den")).unwrap();
        assert!(human.out.contains("was not contacted"));
        assert!(matches!(store.load("aa:02").unwrap(), Stored::Missing));
    }
}
