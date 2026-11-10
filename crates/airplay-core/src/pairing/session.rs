//! Pairing state machine: transient pair-setup (HomePod), persistent
//! pair-setup (Apple TV, saves an identity) and pair-verify (re-auth on the
//! RTSP connection that will carry the encrypted session). Mirrors the Go
//! `pairing.PairingSession`.

use super::crypto::{
    chacha20poly1305_decrypt, chacha20poly1305_encrypt, generate_curve25519_keypair, hkdf_expand,
    nonce_from_string, shared_secret,
};
use super::identity::{self, Identity};
use super::store::{AuthMethod, CredentialStore, PairingRecord, Stored};
use super::srp::{padded_bytes, SrpClient};
use super::tlv::{self, TlvItem};
use anyhow::{anyhow, bail, Context, Result};
use num_bigint::BigUint;

pub const HOMEPOD_PIN: &str = "3939";

const METHOD_PAIR_SETUP: u8 = 0x00;
const STATE_M1: u8 = 0x01;
const STATE_M3: u8 = 0x03;
const STATE_M4: u8 = 0x04;
const STATE_M5: u8 = 0x05;
const STATE_M6: u8 = 0x06;
const STEP1: u8 = 0x01;
const STEP3: u8 = 0x03;
const FLAG_TRANSIENT: u8 = 0x10;

fn err_code_name(code: u8) -> &'static str {
    match code {
        0x02 => "authentication (wrong PIN or SRP mismatch)",
        0x03 => "backoff (too many attempts, wait and retry)",
        0x04 => "max peers",
        0x05 => "max tries",
        0x06 => "unavailable",
        0x07 => "invalid request",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    #[error("not paired with this receiver; pair again")]
    NotPaired,
    #[error("saved key lacks receiver trust; pair again")]
    LegacyKeyOnly,
    #[error("saved pairing record is invalid; pair again")]
    InvalidRecord,
    #[error("no trusted receiver key; pair again")]
    MissingTrust,
    #[error("receiver identity does not match the saved pairing; pair again")]
    AccessoryIdMismatch,
    #[error("receiver signature does not match the saved pairing; pair again")]
    SignatureInvalid,
    #[error("{step} error: {}", code.map(|c| format!("code={c} {}", err_code_name(c))).unwrap_or_else(|| "empty error code".into()))]
    Protocol { step: &'static str, code: Option<u8> },
}

impl PairingError {
    pub fn requires_re_pair(&self) -> bool {
        match self {
            PairingError::Protocol { step, code } => step.starts_with("pair-verify") && *code == Some(0x02),
            _ => true,
        }
    }
}

fn check_error(items: &[TlvItem], step: &'static str) -> Result<(), PairingError> {
    match tlv::get(items, tlv::TLV_ERROR) {
        Some(e) => Err(PairingError::Protocol { step, code: e.first().copied() }),
        None => Ok(()),
    }
}

/// Posts one pairing message and returns the response body. The RTSP client
/// implements this so pair-verify runs on the same TCP connection that carries
/// the encrypted session afterwards.
pub trait PairTransport {
    fn post(&mut self, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<Vec<u8>>;
}

pub struct PairingSession {
    pub device_id: String,
    pub pin: String,
    pub is_transient: bool,
    pub identity: Option<Identity>,
    pub session_key: Vec<u8>,
    pub server_ltpk: Vec<u8>,
    pub accessory_id: String,
    pub shared_secret: Vec<u8>,
    srp: Option<SrpClient>,
}

impl PairingSession {
    pub fn new(device_id: &str, pin: &str) -> Self {
        Self {
            device_id: device_id.to_string(),
            pin: pin.to_string(),
            is_transient: false,
            identity: None,
            session_key: Vec::new(),
            server_ltpk: Vec::new(),
            accessory_id: String::new(),
            shared_secret: Vec::new(),
            srp: None,
        }
    }

    fn hkp_header(&self) -> &'static str {
        if self.is_transient {
            "4"
        } else {
            "3"
        }
    }

    fn post(&self, t: &mut dyn PairTransport, path: &str, body: &[u8]) -> Result<Vec<u8>> {
        let headers = vec![("X-Apple-HKP".to_string(), self.hkp_header().to_string())];
        t.post(path, &headers, body)
    }

    pub fn load_or_create_identity(&mut self, store: &dyn CredentialStore) -> Result<()> {
        self.identity = Some(match store.load(&self.device_id)? {
            Stored::Paired(record) => record.controller_key,
            Stored::Missing | Stored::RePairRequired { .. } => Identity::generate(),
        });
        Ok(())
    }

    pub fn use_record(&mut self, record: &PairingRecord) {
        self.identity = Some(record.controller_key.clone());
        self.server_ltpk = record.accessory_public_key.to_vec();
        self.accessory_id = record.accessory_id.clone();
    }

    pub fn pairing_record(&self) -> Option<PairingRecord> {
        if self.is_transient || self.accessory_id.is_empty() {
            return None;
        }
        Some(PairingRecord {
            method: AuthMethod::PairSetupPin,
            receiver_id: self.device_id.clone(),
            controller_id: String::from_utf8(self.controller_pairing_id()).ok()?,
            accessory_id: self.accessory_id.clone(),
            controller_key: self.identity.clone()?,
            accessory_public_key: self.server_ltpk.as_slice().try_into().ok()?,
            password: None,
        })
    }

    // A stable UUID-format identifier derived from our LTPK. pair-setup M5 and
    // pair-verify M3 must use the same value.
    fn controller_pairing_id(&self) -> Vec<u8> {
        let pk = self.identity.as_ref().expect("identity set").public_key();
        format!(
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            pk[0], pk[1], pk[2], pk[3], pk[4], pk[5], pk[6], pk[7],
            pk[8], pk[9], pk[10], pk[11], pk[12], pk[13], pk[14], pk[15]
        )
        .into_bytes()
    }

    fn pin_start(&self, t: &mut dyn PairTransport) -> Result<()> {
        self.post(t, "/pair-pin-start", &[]).map(|_| ())
    }

    fn pair_setup_enc_key(&self) -> Vec<u8> {
        let sk = self.srp.as_ref().unwrap().session_key();
        hkdf_expand(sk, b"Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info", 32)
    }

    fn pair_setup_phase1(&mut self, t: &mut dyn PairTransport) -> Result<(Vec<u8>, BigUint)> {
        if self.identity.is_none() {
            self.identity = Some(Identity::generate());
        }
        if !self.is_transient {
            self.pin_start(t).context("pin-start")?;
        }

        let mut tlvs = vec![
            TlvItem::new(tlv::TLV_METHOD, vec![METHOD_PAIR_SETUP]),
            TlvItem::new(tlv::TLV_STATE, vec![STATE_M1]),
        ];
        if self.is_transient {
            tlvs.push(TlvItem::new(tlv::TLV_FLAGS, vec![FLAG_TRANSIENT]));
        }
        let data = self.post(t, "/pair-setup", &tlv::encode(&tlvs)).context("M1")?;
        let items = tlv::decode(&data).context("decode M2")?;
        check_error(&items, "M2")?;
        let salt = tlv::get(&items, tlv::TLV_SALT)
            .ok_or_else(|| anyhow!("M2 missing salt"))?
            .to_vec();
        let pub_key = tlv::get(&items, tlv::TLV_PUBLIC_KEY).ok_or_else(|| anyhow!("M2 missing publicKey"))?;
        Ok((salt, BigUint::from_bytes_be(pub_key)))
    }

    fn pair_setup_phase2(&mut self, t: &mut dyn PairTransport, salt: &[u8], b: BigUint) -> Result<()> {
        let pin = self.pin.clone();
        let mut srp = SrpClient::new("Pair-Setup", &pin);
        let a = srp.generate_a();
        srp.process_b(salt, b).context("process B")?;
        let m1_proof = srp.compute_m1();
        self.srp = Some(srp);

        let m3 = tlv::encode(&[
            TlvItem::new(tlv::TLV_STATE, vec![STATE_M3]),
            TlvItem::new(tlv::TLV_PUBLIC_KEY, padded_bytes(&a, 384)),
            TlvItem::new(tlv::TLV_PROOF, m1_proof),
        ]);
        let data = self.post(t, "/pair-setup", &m3).context("M3")?;
        let items = tlv::decode(&data).context("decode M4")?;
        check_error(&items, "M4")?;
        match tlv::get(&items, tlv::TLV_STATE) {
            Some(s) if s.first() == Some(&STATE_M4) => {}
            _ => bail!("M4 unexpected state"),
        }
        let server_proof = tlv::get(&items, tlv::TLV_PROOF).ok_or_else(|| anyhow!("M4 missing proof"))?;
        if !self.srp.as_ref().unwrap().verify_m2(server_proof) {
            bail!("M2 proof verification failed");
        }

        let srp_session_key = self.srp.as_ref().unwrap().session_key().to_vec();

        if self.is_transient {
            // Transient pairing ends at M4; SRP session key is the shared secret.
            self.session_key = srp_session_key.clone();
            self.shared_secret = srp_session_key;
            return Ok(());
        }

        let enc_key = self.pair_setup_enc_key();
        let ident = self.identity.as_ref().unwrap();

        // M5: signature covers iOSDeviceX || iOSDevicePairingID || iOSDeviceLTPK.
        let device_x = hkdf_expand(
            &srp_session_key,
            b"Pair-Setup-Controller-Sign-Salt",
            "Pair-Setup-Controller-Sign-Info",
            32,
        );
        let pairing_id = self.controller_pairing_id();
        let mut sig_payload = Vec::new();
        sig_payload.extend_from_slice(&device_x);
        sig_payload.extend_from_slice(&pairing_id);
        sig_payload.extend_from_slice(&ident.public_key());
        let our_sig = ident.sign(&sig_payload);

        let m5_inner = tlv::encode(&[
            TlvItem::new(tlv::TLV_IDENTIFIER, pairing_id.clone()),
            TlvItem::new(tlv::TLV_PUBLIC_KEY, ident.public_key().to_vec()),
            TlvItem::new(tlv::TLV_SIGNATURE, our_sig),
        ]);
        let m5_enc = chacha20poly1305_encrypt(&enc_key, &nonce_from_string("PS-Msg05"), &m5_inner, &[])
            .context("encrypt M5")?;
        let m5 = tlv::encode(&[
            TlvItem::new(tlv::TLV_STATE, vec![STATE_M5]),
            TlvItem::new(tlv::TLV_ENCRYPTED_DATA, m5_enc),
        ]);
        let data = self.post(t, "/pair-setup", &m5).context("M5")?;

        // M6: accessory identity nested in EncryptedData.
        let items = tlv::decode(&data).context("decode M6")?;
        check_error(&items, "M6")?;
        let m6_enc = tlv::get(&items, tlv::TLV_ENCRYPTED_DATA).ok_or_else(|| anyhow!("M6 missing encrypted data"))?;
        match tlv::get(&items, tlv::TLV_STATE) {
            Some(s) if s.first() == Some(&STATE_M6) => {}
            _ => bail!("M6 unexpected state"),
        }
        let m6_inner = chacha20poly1305_decrypt(&enc_key, &nonce_from_string("PS-Msg06"), m6_enc, &[])
            .context("decrypt M6")?;
        let m6_items = tlv::decode(&m6_inner).context("decode M6 inner")?;
        let device_pub = tlv::get(&m6_items, tlv::TLV_PUBLIC_KEY).ok_or_else(|| anyhow!("M6 missing pub"))?;
        let device_sig = tlv::get(&m6_items, tlv::TLV_SIGNATURE).ok_or_else(|| anyhow!("M6 missing sig"))?;
        let device_id = tlv::get(&m6_items, tlv::TLV_IDENTIFIER).ok_or_else(|| anyhow!("M6 missing id"))?;

        // Verify accessory signature over AccessoryX || AccessoryPairingID || AccessoryLTPK.
        let accessory_x = hkdf_expand(
            &srp_session_key,
            b"Pair-Setup-Accessory-Sign-Salt",
            "Pair-Setup-Accessory-Sign-Info",
            32,
        );
        let mut check = Vec::new();
        check.extend_from_slice(&accessory_x);
        check.extend_from_slice(device_id);
        check.extend_from_slice(device_pub);
        if !identity::verify(device_pub, &check, device_sig) {
            bail!("device signature verification failed");
        }

        let accessory_id = String::from_utf8(device_id.to_vec()).context("M6 accessory id")?;
        if accessory_id.is_empty() {
            bail!("M6 empty accessory id");
        }
        self.server_ltpk = device_pub.to_vec();
        self.accessory_id = accessory_id;
        self.session_key = srp_session_key;
        Ok(())
    }

    /// Persistent pair-setup for an Apple TV (saves a controller identity).
    pub fn pair_apple_tv(&mut self, t: &mut dyn PairTransport) -> Result<()> {
        self.is_transient = false;
        let (salt, b) = self.pair_setup_phase1(t)?;
        self.pair_setup_phase2(t, &salt, b)
    }

    /// Transient pair-setup for a HomePod (fixed PIN, no saved identity).
    pub fn pair_homepod(&mut self, t: &mut dyn PairTransport) -> Result<()> {
        self.is_transient = true;
        self.pin = HOMEPOD_PIN.to_string();
        let (salt, b) = self.pair_setup_phase1(t)?;
        self.pair_setup_phase2(t, &salt, b)
    }

    /// pair-verify using a previously saved identity. Derives the channel
    /// shared secret via Curve25519 ECDH.
    pub fn pair_verify(&mut self, t: &mut dyn PairTransport) -> Result<()> {
        let ident = self
            .identity
            .as_ref()
            .ok_or_else(|| anyhow!("pair-verify requires an identity from pair-setup"))?
            .clone();

        let trusted_key = <[u8; 32]>::try_from(self.server_ltpk.as_slice())
            .ok()
            .filter(|k| ed25519_dalek::VerifyingKey::from_bytes(k).is_ok())
            .ok_or(PairingError::MissingTrust)?;
        if self.accessory_id.is_empty() {
            return Err(PairingError::MissingTrust.into());
        }

        let (priv_key, pub_key) = generate_curve25519_keypair();

        let m1 = tlv::encode(&[
            TlvItem::new(tlv::TLV_STATE, vec![STEP1]),
            TlvItem::new(tlv::TLV_PUBLIC_KEY, pub_key.to_vec()),
        ]);
        let data = self.post(t, "/pair-verify", &m1).context("pair-verify M1")?;
        let items = tlv::decode(&data).context("decode pair-verify M2")?;
        check_error(&items, "pair-verify M2")?;
        let accessory_pub = tlv::get(&items, tlv::TLV_PUBLIC_KEY)
            .ok_or_else(|| anyhow!("M2 missing publicKey"))?
            .to_vec();
        let encrypted = tlv::get(&items, tlv::TLV_ENCRYPTED_DATA).ok_or_else(|| anyhow!("M2 missing encryptedData"))?;

        let secret = shared_secret(&priv_key, &accessory_pub)?;
        let session_key = hkdf_expand(&secret, b"Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info", 32);

        let decrypted = chacha20poly1305_decrypt(&session_key, &nonce_from_string("PV-Msg02"), encrypted, &[])
            .context("decrypt M2 data")?;
        let inner = tlv::decode(&decrypted).context("decode M2 inner")?;
        let accessory_id = tlv::get(&inner, tlv::TLV_IDENTIFIER).ok_or_else(|| anyhow!("M2 inner missing id"))?;
        let accessory_sig = tlv::get(&inner, tlv::TLV_SIGNATURE).ok_or_else(|| anyhow!("M2 inner missing sig"))?;
        if accessory_id != self.accessory_id.as_bytes() {
            return Err(PairingError::AccessoryIdMismatch.into());
        }

        // Signature covers AccessoryCurvePub || AccessoryPairingID || iOSCurvePub.
        let mut payload = Vec::new();
        payload.extend_from_slice(&accessory_pub);
        payload.extend_from_slice(accessory_id);
        payload.extend_from_slice(&pub_key);
        if !identity::verify(&trusted_key, &payload, accessory_sig) {
            return Err(PairingError::SignatureInvalid.into());
        }

        // M3: signature covers iOSCurvePub || iOSPairingID || AccessoryCurvePub.
        let pairing_id = self.controller_pairing_id();
        let mut payload = Vec::new();
        payload.extend_from_slice(&pub_key);
        payload.extend_from_slice(&pairing_id);
        payload.extend_from_slice(&accessory_pub);
        let our_sig = ident.sign(&payload);

        let m3_inner = tlv::encode(&[
            TlvItem::new(tlv::TLV_IDENTIFIER, pairing_id),
            TlvItem::new(tlv::TLV_PUBLIC_KEY, ident.public_key().to_vec()),
            TlvItem::new(tlv::TLV_SIGNATURE, our_sig),
        ]);
        let enc_sig = chacha20poly1305_encrypt(&session_key, &nonce_from_string("PV-Msg03"), &m3_inner, &[])
            .context("encrypt M3 sig")?;
        let m3 = tlv::encode(&[
            TlvItem::new(tlv::TLV_STATE, vec![STEP3]),
            TlvItem::new(tlv::TLV_ENCRYPTED_DATA, enc_sig),
        ]);
        let data = self.post(t, "/pair-verify", &m3).context("pair-verify M3")?;
        let items = tlv::decode(&data).context("decode pair-verify M4")?;
        check_error(&items, "pair-verify M4")?;
        self.session_key = session_key;
        self.shared_secret = secret.to_vec();
        Ok(())
    }
}
