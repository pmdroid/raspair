use super::Palette;
use crate::auth::{self, Backend};
use airplay_core::discovery::device::merge_device;
use airplay_core::discovery::Device;
use airplay_core::pairing::{CredentialStore, PairingError, PinChallenge, PinPairingError, PinSource, Stored};
use airplay_core::profile::preflight;
use airplay_core::protocol::AuthRequirement;
use airplay_core::{BufferedAudioFormat, FallbackPolicy, PlayerError, SessionError, SessionProfile, StereoPair};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Frame;
use std::collections::HashSet;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub enum Job {
    Refresh,
    Verify(Device, Option<String>),
    Begin(Device),
    Respond(Device, PinChallenge, String),
    Cancel(PinChallenge),
}

pub enum Outcome {
    Devices(anyhow::Result<Vec<Device>>),
    Verified(Device, bool, anyhow::Result<()>),
    Challenge(Device, anyhow::Result<PinChallenge>),
    Responded(Device, anyhow::Result<()>),
}

pub fn perform(backend: &mut dyn Backend, job: Job) -> Option<Outcome> {
    Some(match job {
        Job::Refresh => Outcome::Devices(backend.devices(Duration::ZERO)),
        Job::Verify(dev, password) => {
            let result = backend.verify(&dev, password.as_deref());
            Outcome::Verified(dev, password.is_some(), result)
        }
        Job::Begin(dev) => {
            let result = backend.begin(&dev);
            Outcome::Challenge(dev, result)
        }
        Job::Respond(dev, challenge, code) => {
            let result = backend.respond(&challenge, &code);
            Outcome::Responded(dev, result)
        }
        Job::Cancel(challenge) => {
            backend.cancel(&challenge);
            return None;
        }
    })
}

pub struct Worker {
    jobs: Sender<Job>,
    done: Receiver<Outcome>,
}

impl Worker {
    pub fn spawn(mut backend: impl Backend + Send + 'static) -> Self {
        let (jobs, inbox) = channel::<Job>();
        let (out, done) = channel();
        std::thread::spawn(move || {
            for job in inbox {
                if perform(&mut backend, job).is_some_and(|o| out.send(o).is_err()) {
                    break;
                }
            }
        });
        Self { jobs, done }
    }

    pub fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    pub fn poll(&self) -> Vec<Outcome> {
        self.done.try_iter().collect()
    }
}

enum Secret {
    Pin(PinChallenge),
    Password,
}

struct Dialog {
    device: Device,
    secret: Secret,
    input: String,
}

struct Row {
    key: String,
    members: Vec<Device>,
}

pub struct Picker {
    store: Arc<dyn CredentialStore>,
    devices: Vec<Device>,
    online: HashSet<String>,
    repair: HashSet<String>,
    focus: Option<String>,
    pub chosen: Option<String>,
    dialog: Option<Dialog>,
    confirm: bool,
    pub open: bool,
    pub status: String,
}

fn key_of(d: &Device) -> String {
    if d.record.device_id.is_empty() {
        format!("{}@{}", d.name, d.host)
    } else {
        d.record.device_id.clone()
    }
}

fn name(d: &Device) -> String {
    auth::clean(&d.name)
}

fn title(row: &Row) -> String {
    match row.members.as_slice() {
        [l, r] if name(l) != name(r) => format!("{} + {} (stereo)", name(l), name(r)),
        [l, _] => format!("{} (stereo)", name(l)),
        [d, ..] => format!("{} ({})", name(d), d.record.device_id),
        [] => String::new(),
    }
}

fn profiles(members: &[Device]) -> String {
    let all = [("PCM 24/48", BufferedAudioFormat::pcm_48000_24_stereo(true)), ("ALAC 24/48", BufferedAudioFormat::alac_48000_24_stereo())];
    let ok: Vec<&str> = all
        .into_iter()
        .filter(|(_, f)| members.iter().all(|d| preflight(&d.route(), &SessionProfile::Buffered(f.clone()), &FallbackPolicy::Forbid).is_ok()))
        .map(|(n, _)| n)
        .collect();
    if ok.is_empty() {
        "no buffered 24/48 profile".into()
    } else {
        ok.join(", ")
    }
}

pub fn recovery(e: &anyhow::Error) -> String {
    let retry = "fix the state directory permissions or restart with --state-dir DIR";
    for cause in e.chain() {
        if let Some(p) = cause.downcast_ref::<PinPairingError>() {
            return match p {
                PinPairingError::Expired => "The PIN challenge expired; press p to start a new one".into(),
                PinPairingError::WrongCode => "Wrong PIN; press p to start a new attempt".into(),
                PinPairingError::Backoff => "The receiver is backing off pairing attempts; wait a minute, then press p".into(),
                PinPairingError::MaxPeers => "The receiver cannot pair another controller; remove one in its settings, then press p".into(),
                PinPairingError::Failed(m) if m.starts_with("save pairing") => format!("Could not {m}; {retry}"),
                other => format!("{other}; press p to try again"),
            };
        }
        if let Some(SessionError::UnsupportedAuthentication(a)) = cause.downcast_ref::<SessionError>() {
            return format!("The receiver needs {} authentication, which airplay does not support yet; choose another receiver", auth::auth_label(a));
        }
        if cause.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied) {
            return format!("Permission denied: {e:#}; {retry}");
        }
    }
    format!("{e:#}")
}

impl Picker {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self { store, devices: Vec::new(), online: HashSet::new(), repair: HashSet::new(), focus: None, chosen: None, dialog: None, confirm: false, open: false, status: String::new() }
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = Vec::new();
        for d in &self.devices {
            let row = match StereoPair::resolve(&self.devices, d) {
                Some(pair) => Row { key: format!("stereo:{}", pair.group), members: vec![pair.left, pair.right] },
                None => Row { key: key_of(d), members: vec![d.clone()] },
            };
            if !rows.iter().any(|r| r.key == row.key) {
                rows.push(row);
            }
        }
        rows.sort_by_cached_key(|r| (name(&r.members[0]).to_lowercase(), r.key.clone()));
        rows
    }

    fn row(&self, key: Option<&String>) -> Option<Row> {
        self.rows().into_iter().find(|r| Some(&r.key) == key)
    }

    pub fn target(&self) -> Option<(Device, Vec<Device>)> {
        let row = self.row(self.chosen.as_ref())?;
        Some((row.members[0].clone(), self.devices.clone()))
    }

    pub fn chosen_label(&self) -> Option<String> {
        self.row(self.chosen.as_ref()).map(|r| title(&r))
    }

    pub fn describe(&self, error: &PlayerError) -> String {
        match error {
            PlayerError::Session(SessionError::MemberFailed { member, error }) => {
                let who = self.devices.iter().find(|d| &d.record.device_id == member).map(name).unwrap_or_default();
                format!("Stereo member {who} ({member}) failed: {error}; the pair stopped together. Press d to choose a receiver")
            }
            other => other.to_string(),
        }
    }

    fn saved(&self, d: &Device) -> bool {
        !d.record.device_id.is_empty() && matches!(self.store.load(&d.record.device_id), Ok(Stored::Paired(_))) && !self.repair.contains(&d.record.device_id)
    }

    fn pairing(&self, d: &Device) -> String {
        let auth = d.route().auth;
        if matches!(auth, AuthRequirement::None | AuthRequirement::HapTransient) {
            return "no pairing needed".into();
        }
        match self.store.load(&d.record.device_id) {
            _ if self.repair.contains(&d.record.device_id) => "re-pair required".into(),
            Ok(Stored::Paired(r)) => format!("paired ({})", auth::method_label(r.method)),
            Ok(Stored::RePairRequired { .. }) => "re-pair required".into(),
            Ok(Stored::Missing) => format!("needs {}", auth::auth_label(&auth)),
            Err(_) => "saved pairing unreadable".into(),
        }
    }

    fn password(&mut self, device: Device, status: String) {
        (self.status, self.open) = (status, true);
        self.dialog = Some(Dialog { device, secret: Secret::Password, input: String::new() });
    }

    fn pair(&mut self, members: &[Device]) -> Option<Job> {
        let needs = |d: &&Device| !matches!(d.route().auth, AuthRequirement::None | AuthRequirement::HapTransient) && !self.saved(d);
        let dev = members.iter().find(needs).unwrap_or(&members[0]).clone();
        let saved = self.saved(&dev);
        self.status = format!("Contacting {}…", name(&dev));
        match dev.route().auth {
            AuthRequirement::None | AuthRequirement::HapTransient => self.status = format!("{} pairs on every connection; nothing to save", name(&dev)),
            AuthRequirement::HapTransientPassword | AuthRequirement::HapPersistent if saved => return Some(Job::Verify(dev, None)),
            AuthRequirement::HapTransientPassword => self.password(dev.clone(), format!("Enter the password for {}", name(&dev))),
            AuthRequirement::HapPersistent => return Some(Job::Begin(dev)),
            other => self.status = recovery(&SessionError::UnsupportedAuthentication(other).into()),
        }
        None
    }

    fn forget(&mut self) {
        let Some(row) = self.row(self.focus.as_ref()) else { return };
        let mut removed = Vec::new();
        for d in &row.members {
            let id = &d.record.device_id;
            let had = !matches!(self.store.load(id), Ok(Stored::Missing));
            if let Err(e) = self.store.forget(id) {
                self.status = recovery(&e.into());
                return;
            }
            self.repair.remove(id);
            if had {
                removed.push(id.clone());
            }
        }
        self.status = match removed.is_empty() {
            true => format!("No local pairing record for {}", title(&row)),
            false => format!("Removed the local pairing for {}; the receiver was not contacted", removed.join(", ")),
        };
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Job> {
        if let Some(dialog) = &mut self.dialog {
            match key.code {
                KeyCode::Char(c) => dialog.input.push(c),
                KeyCode::Backspace => drop(dialog.input.pop()),
                KeyCode::Enter if !dialog.input.trim().is_empty() => {
                    let Dialog { device, secret, input } = self.dialog.take()?;
                    self.status = format!("Checking with {}…", name(&device));
                    return Some(match secret {
                        Secret::Pin(c) => Job::Respond(device, c, input.trim().to_string()),
                        Secret::Password => Job::Verify(device, Some(input)),
                    });
                }
                KeyCode::Esc => {
                    let dialog = self.dialog.take()?;
                    self.status = format!("Pairing with {} cancelled", name(&dialog.device));
                    if let Secret::Pin(c) = dialog.secret {
                        return Some(Job::Cancel(c));
                    }
                }
                _ => {}
            }
            return None;
        }
        if std::mem::take(&mut self.confirm) {
            match key.code {
                KeyCode::Char('y') => self.forget(),
                _ => self.status = "Forget cancelled; nothing was removed".into(),
            }
            return None;
        }
        let rows = self.rows();
        let at = rows.iter().position(|r| Some(&r.key) == self.focus.as_ref());
        let row = at.map(|i| &rows[i]);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.focus = rows.get(at.map_or(0, |i| i.saturating_sub(1))).map(|r| r.key.clone()),
            KeyCode::Down | KeyCode::Char('j') => self.focus = rows.get(at.map_or(0, |i| (i + 1).min(rows.len() - 1))).map(|r| r.key.clone()),
            KeyCode::Enter => {
                if let Some(row) = row {
                    (self.chosen, self.open) = (Some(row.key.clone()), false);
                    self.status = format!("Selected {}; press Space to play", title(row));
                }
            }
            KeyCode::Char('p') => return row.and_then(|r| self.pair(&r.members)),
            KeyCode::Char('f') => {
                if let Some(row) = row {
                    self.confirm = true;
                    self.status = format!("Forget the local pairing for {}? Press y to confirm", title(row));
                }
            }
            KeyCode::Char('r') => {
                self.status = "Searching for receivers…".into();
                return Some(Job::Refresh);
            }
            KeyCode::Esc | KeyCode::Char('d') => self.open = false,
            _ => {}
        }
        None
    }

    pub fn tick(&mut self, now: Instant) -> Option<Job> {
        if !matches!(&self.dialog, Some(Dialog { secret: Secret::Pin(c), .. }) if c.expires_at <= now) {
            return None;
        }
        let Some(Dialog { device, secret: Secret::Pin(c), .. }) = self.dialog.take() else { return None };
        self.status = format!("The PIN challenge for {} expired; press p to start a new one", name(&device));
        Some(Job::Cancel(c))
    }

    pub fn apply(&mut self, outcome: Outcome) -> Option<Job> {
        let wrong_password = |e: &anyhow::Error| matches!(auth::pairing_error(e), Some(PairingError::WrongPassword));
        match outcome {
            Outcome::Devices(Ok(found)) => {
                self.online = found.iter().map(key_of).collect();
                for d in found {
                    merge_device(&mut self.devices, d);
                }
                if self.row(self.focus.as_ref()).is_none() {
                    self.focus = self.rows().first().map(|r| r.key.clone());
                }
                self.status = format!("{} receivers available", self.online.len());
            }
            Outcome::Verified(dev, false, Err(e)) if wrong_password(&e) => self.password(dev, "The saved password was rejected; enter the current password".into()),
            Outcome::Verified(dev, true, Err(e)) if wrong_password(&e) => self.password(dev, "The receiver rejected the password; try again or press Esc".into()),
            Outcome::Verified(dev, _, Err(e)) if auth::pairing_error(&e).is_some_and(|p| p.requires_re_pair()) => {
                self.repair.insert(dev.record.device_id.clone());
                if dev.route().auth != AuthRequirement::HapPersistent {
                    self.password(dev, "The saved password is no longer valid; enter the current password".into());
                    return None;
                }
                self.status = format!("{} rejected the saved pairing; re-pair required, starting a new PIN challenge", name(&dev));
                return Some(Job::Begin(dev));
            }
            Outcome::Verified(dev, password, Ok(())) => {
                self.repair.remove(&dev.record.device_id);
                self.status = format!("{} for {}", if password { "Password accepted and saved" } else { "Saved pairing verified" }, name(&dev));
            }
            Outcome::Responded(dev, Ok(())) => {
                self.repair.remove(&dev.record.device_id);
                self.status = format!("Paired with {}; credentials saved", name(&dev));
            }
            Outcome::Challenge(device, Ok(challenge)) => {
                (self.status, self.open) = (format!("Enter the PIN for {}", name(&device)), true);
                self.dialog = Some(Dialog { device, secret: Secret::Pin(challenge), input: String::new() });
            }
            Outcome::Devices(Err(e)) | Outcome::Verified(_, _, Err(e)) | Outcome::Challenge(_, Err(e)) | Outcome::Responded(_, Err(e)) => self.status = recovery(&e),
        }
        None
    }

    pub fn render(&self, frame: &mut Frame, p: Palette) {
        let area = frame.area();
        let popup = Rect { x: 2.min(area.width), y: 1.min(area.height), width: area.width.saturating_sub(4), height: area.height.saturating_sub(2) };
        let rows = self.rows();
        let mut lines = Vec::new();
        if rows.is_empty() {
            lines.push(Line::styled("No receivers yet; press r to search", p.muted()));
        }
        for row in &rows {
            let focused = Some(&row.key) == self.focus.as_ref();
            let marks = format!("{}{} ", if focused { "▸" } else { " " }, if Some(&row.key) == self.chosen.as_ref() { "✓" } else { " " });
            let stereo = if row.members.len() == 2 { "stereo · " } else { "" };
            let style = if focused { p.focus().add_modifier(Modifier::BOLD) } else { Style::default() };
            let heads: Vec<String> = row.members.iter().map(|d| format!("{} · {} · {}", name(d), d.record.device_id, d.record.model)).collect();
            lines.push(Line::styled(format!("{marks}{stereo}{}", heads.join(" + ")), style));
            let states: Vec<String> = row
                .members
                .iter()
                .zip(if row.members.len() == 2 { ["L ", "R "] } else { ["", ""] })
                .map(|(d, side)| format!("{side}{} {}", if self.online.contains(&key_of(d)) { "online" } else { "unavailable" }, self.pairing(d)))
                .collect();
            lines.push(Line::styled(format!("    {} · {}", states.join(" · "), profiles(&row.members)), p.muted()));
        }
        lines.extend([Line::raw(""), Line::styled(self.status.clone(), p.warm())]);
        let block = Block::bordered().border_type(BorderType::Double).title(" Receivers ").title_bottom(" ↑↓ move  Enter select  p pair  f forget  r refresh  Esc close ");
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).style(p.base()).block(block), popup);
        let Some(dialog) = &self.dialog else { return };
        let (heading, prompt, expiry) = match &dialog.secret {
            Secret::Pin(c) => {
                let shown = if c.source == PinSource::OnScreen { "shown on" } else { "for" };
                let minutes = c.expires_at.saturating_duration_since(Instant::now()).as_secs().div_ceil(60);
                (" PIN ", format!("Enter the PIN {shown} {}", name(&dialog.device)), format!(" · expires in {minutes} min"))
            }
            Secret::Password => (" Password ", format!("Enter the password for {}", name(&dialog.device)), String::new()),
        };
        let body = vec![Line::raw(prompt), Line::raw(""), Line::styled(format!("> {}", "•".repeat(dialog.input.chars().count())), p.focus()), Line::raw(""), Line::styled(format!("Enter submit · Esc cancel{expiry}"), p.muted())];
        let width = 56.min(area.width);
        let rect = Rect { x: area.width.saturating_sub(width) / 2, y: area.height.saturating_sub(7) / 2, width, height: 7.min(area.height) };
        frame.render_widget(Clear, rect);
        frame.render_widget(Paragraph::new(body).style(p.base()).block(Block::bordered().border_type(BorderType::Thick).title(heading)), rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests::{apple_tv, device, record, Fake};
    use airplay_core::pairing::{AuthMethod, FileStore, MemoryStore, StoreError};
    use airplay_core::protocol::STATUS_PASSWORD_REQUIRED;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use std::sync::Mutex;

    struct Rig<'s> {
        picker: Picker,
        fake: Fake<'s>,
    }

    impl<'s> Rig<'s> {
        fn new(store: &'s Arc<dyn CredentialStore>, devices: Vec<Device>, verify_error: Option<PairingError>) -> Self {
            let fake = Fake { devices, store: &**store, log: Arc::new(Mutex::new(Vec::new())), verify_error };
            let mut rig = Self { picker: Picker::new(store.clone()), fake };
            rig.press("r");
            rig
        }

        fn run(&mut self, mut job: Option<Job>) {
            while let Some(next) = job.take() {
                job = perform(&mut self.fake, next).and_then(|o| self.picker.apply(o));
            }
        }

        fn key(&mut self, code: KeyCode) {
            let job = self.picker.key(KeyEvent::new(code, KeyModifiers::NONE));
            self.run(job);
        }

        fn press(&mut self, keys: &str) {
            keys.chars().for_each(|c| self.key(KeyCode::Char(c)));
        }

        fn log(&self) -> Vec<String> {
            std::mem::take(&mut *self.fake.log.lock().unwrap())
        }

        fn screen(&self) -> String {
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal.draw(|f| self.picker.render(f, Palette::Plain)).unwrap();
            format!("{}\n{}", terminal.backend(), self.picker.status)
        }
    }

    fn paired(store: &Arc<dyn CredentialStore>, id: &str) -> bool {
        matches!(store.load(id).unwrap(), Stored::Paired(_))
    }

    #[test]
    fn pin_is_masked_tied_to_the_challenge_and_reused_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(dir.path()));
        let mut rig = Rig::new(&store, vec![apple_tv("aa:01", "Den")], None);
        rig.press("p4821");
        let dialog = rig.screen();
        assert!(dialog.contains("> ••••") && dialog.contains("Enter the PIN shown on Den") && dialog.contains("expires in 2 min"), "{dialog}");
        assert!(!dialog.contains("4821"));
        insta::assert_snapshot!("pin_dialog_100x24", dialog);
        rig.key(KeyCode::Enter);
        assert_eq!(rig.log(), ["begin", "respond:4821"]);
        assert!(paired(&store, "aa:01") && !rig.screen().contains("4821"));
        assert!(rig.picker.status.contains("Paired with Den"));

        let reopened: Arc<dyn CredentialStore> = Arc::new(FileStore::new(dir.path()));
        let mut again = Rig::new(&reopened, vec![apple_tv("aa:01", "Den")], None);
        assert!(again.screen().contains("paired (pair_setup_pin)"));
        again.press("p");
        assert_eq!(again.log(), ["verify:false"]);
        assert!(again.picker.status.contains("Saved pairing verified"));
    }

    #[test]
    fn password_is_masked_saved_and_reprompted_when_rejected() {
        let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
        let mut rig = Rig::new(&store, vec![device("bb:01", "Kitchen", "AudioAccessory5,1", STATUS_PASSWORD_REQUIRED)], None);
        rig.press("phunter2");
        assert!(rig.screen().contains("> •••••••") && rig.log().is_empty());
        rig.key(KeyCode::Enter);
        assert_eq!(rig.log(), ["verify:true"]);
        assert!(paired(&store, "bb:01") && rig.picker.status.contains("Password accepted"));
        rig.fake.verify_error = Some(PairingError::WrongPassword);
        rig.press("p");
        assert_eq!(rig.log(), ["verify:false"]);
        assert!(rig.screen().contains("saved password was rejected") && rig.picker.dialog.is_some());
        rig.key(KeyCode::Esc);
        assert!(rig.picker.dialog.is_none() && rig.log().is_empty() && !rig.screen().contains("hunter2"));
    }

    #[test]
    fn expiry_and_cancel_close_the_library_challenge() {
        let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
        let mut rig = Rig::new(&store, vec![apple_tv("aa:01", "Den")], None);
        rig.press("p12");
        assert!(rig.picker.tick(Instant::now()).is_none());
        let job = rig.picker.tick(Instant::now() + Duration::from_secs(121));
        rig.run(job);
        assert_eq!(rig.log(), ["begin", "cancel"]);
        assert!(rig.picker.dialog.is_none() && rig.picker.status.contains("expired; press p"));
        rig.press("p");
        rig.key(KeyCode::Enter);
        assert!(rig.picker.dialog.is_some());
        rig.key(KeyCode::Esc);
        assert_eq!(rig.log(), ["begin", "cancel"]);
        assert!(rig.picker.status.contains("cancelled") && !paired(&store, "aa:01"));
    }

    #[test]
    fn revoked_credentials_require_a_new_pin_challenge() {
        let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
        store.save(&record("aa:01", AuthMethod::PairSetupPin, None)).unwrap();
        let mut rig = Rig::new(&store, vec![apple_tv("aa:01", "Den")], Some(PairingError::SignatureInvalid));
        rig.press("p");
        assert_eq!(rig.log(), ["verify:false", "begin"]);
        assert!(rig.screen().contains("re-pair required") && rig.picker.dialog.is_some());
        rig.press("9");
        rig.key(KeyCode::Enter);
        assert!(rig.screen().contains("paired (pair_setup_pin)"));
    }

    #[test]
    fn selection_survives_changed_address_and_name_and_duplicates_stay_distinct() {
        let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
        let mut rig = Rig::new(&store, vec![apple_tv("aa:01", "Living Room"), apple_tv("aa:02", "Living\\ Room")], None);
        let screen = rig.screen();
        assert!(screen.contains("Living Room · aa:01") && screen.contains("Living Room · aa:02"), "{screen}");
        rig.key(KeyCode::Down);
        rig.key(KeyCode::Enter);
        assert_eq!(rig.picker.chosen.as_deref(), Some("aa:02"));
        assert!(rig.picker.status.contains("press Space to play") && rig.log().is_empty());
        let mut moved = apple_tv("aa:02", "Den");
        moved.ipv4 = Some("192.0.2.99".parse().unwrap());
        rig.fake.devices = vec![moved];
        rig.press("r");
        let (target, _) = rig.picker.target().unwrap();
        assert_eq!((target.name.as_str(), target.ip().as_deref()), ("Den", Some("192.0.2.99")));
        assert_eq!(rig.picker.chosen_label().as_deref(), Some("Den (aa:02)"));
        assert!(rig.screen().contains("unavailable needs pin"));
    }

    #[test]
    fn stereo_pair_is_one_row_with_named_member_failures() {
        let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
        let member = |id: &str| {
            let mut d = device(id, "Kitchen", "AudioAccessory5,1", 0);
            d.record.tight_sync_id = "grp".into();
            d
        };
        let mut rig = Rig::new(&store, vec![member("cc:02"), member("cc:01")], None);
        assert_eq!(rig.picker.rows().len(), 1);
        assert!(rig.screen().contains("stereo · Kitchen · cc:01 · AudioAccessory5,1 + Kitchen · cc:02"));
        rig.key(KeyCode::Enter);
        let (left, devices) = rig.picker.target().unwrap();
        assert_eq!((left.record.device_id.as_str(), devices.len()), ("cc:01", 2));
        let error = PlayerError::Session(SessionError::MemberFailed { member: "cc:02".into(), error: Box::new(SessionError::Timeout) });
        let text = rig.picker.describe(&error);
        assert!(text.contains("Stereo member Kitchen (cc:02) failed") && text.contains("stopped together"), "{text}");
    }

    #[test]
    fn forget_needs_confirmation_and_reports_local_removal() {
        let store: Arc<dyn CredentialStore> = Arc::new(MemoryStore::default());
        store.save(&record("aa:01", AuthMethod::PairSetupPin, None)).unwrap();
        let mut rig = Rig::new(&store, vec![apple_tv("aa:01", "Den")], None);
        rig.press("fn");
        assert!(paired(&store, "aa:01") && rig.picker.status.contains("nothing was removed"));
        rig.press("fy");
        assert!(!paired(&store, "aa:01") && rig.picker.status.contains("Removed the local pairing for aa:01; the receiver was not contacted"));
        rig.press("fy");
        assert!(rig.picker.status.contains("No local pairing record") && rig.log().is_empty());
    }

    #[test]
    fn permission_and_unsupported_auth_errors_name_the_recovery() {
        let denied = StoreError::Io { path: "/state".into(), source: std::io::ErrorKind::PermissionDenied.into() };
        assert!(recovery(&denied.into()).contains("--state-dir DIR"));
        let unsupported = recovery(&SessionError::UnsupportedAuthentication(AuthRequirement::UnifiedAuth).into());
        assert!(unsupported.contains("unified authentication") && unsupported.contains("choose another receiver"));
        assert!(recovery(&PinPairingError::Backoff.into()).contains("wait a minute"));
    }
}
