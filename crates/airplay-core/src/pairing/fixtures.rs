use super::crypto::{chacha20poly1305_decrypt, chacha20poly1305_encrypt, hkdf_expand, nonce_from_string};
use super::identity::{self, Identity};
use super::store::{CredentialStore, FileStore, MemoryStore, PairingRecord, Stored};
use super::session::{PairTransport, PairingError, PairingSession, HOMEPOD_PIN};
use super::srp::{compute_k, generator, h, modulus, padded_bytes, GROUP_BYTES};
use super::tlv::{self, TlvItem};
use anyhow::{anyhow, bail, Result};
use ed25519_dalek::{Signer, SigningKey};
use num_bigint::BigUint;
use sha2::{Digest, Sha512};
use x25519_dalek::{x25519, X25519_BASEPOINT_BYTES};

const SYNTHETIC_PIN: &str = "0000";
const SYNTHETIC_RECEIVER_ID: &str = "fixture-receiver";
const SYNTHETIC_ACCESSORY_ID: &[u8] = b"00000000-FIXT-URE0-ACCE-SS0RY000000";
const SYNTHETIC_ACCESSORY_SEED: [u8; 32] = [0xA5; 32];
const SYNTHETIC_IMPOSTOR_SEED: [u8; 32] = [0x5A; 32];
const SYNTHETIC_CURVE_SECRET: [u8; 32] = [0x33; 32];
const SYNTHETIC_SRP_SECRET: [u8; 32] = [0x77; 32];
const SYNTHETIC_SALT: [u8; 16] = [0x11; 16];

#[derive(Debug, Clone, PartialEq)]
struct Exchange {
    path: String,
    hkp: String,
    state: Option<u8>,
    method: Option<u8>,
    flags: Option<u8>,
    fields: Vec<u8>,
}

#[derive(Debug, PartialEq)]
enum Proven {
    PairSuccess,
    VerifiedReconnect,
}

struct SyntheticReceiver {
    pin: String,
    signing: SigningKey,
    transcript: Vec<Exchange>,
    verifier: BigUint,
    b: BigUint,
    b_pub: BigUint,
    srp_key: Vec<u8>,
    controller_ltpk: Vec<u8>,
    controller_curve: Vec<u8>,
    verify_key: Vec<u8>,
    tamper_verify_sig: bool,
}

impl SyntheticReceiver {
    fn new(pin: &str) -> Self {
        Self::with_signing_seed(pin, SYNTHETIC_ACCESSORY_SEED)
    }

    fn with_signing_seed(pin: &str, seed: [u8; 32]) -> Self {
        let n = modulus();
        let g = generator();
        let x = BigUint::from_bytes_be(&h(&[&SYNTHETIC_SALT, &h(&[format!("Pair-Setup:{pin}").as_bytes()])]));
        let verifier = g.modpow(&x, &n);
        let b = BigUint::from_bytes_be(&SYNTHETIC_SRP_SECRET);
        let b_pub = (compute_k(&n, &g) * &verifier + g.modpow(&b, &n)) % &n;
        Self {
            pin: pin.to_string(),
            signing: SigningKey::from_bytes(&seed),
            transcript: Vec::new(),
            verifier,
            b,
            b_pub,
            srp_key: Vec::new(),
            controller_ltpk: Vec::new(),
            controller_curve: Vec::new(),
            verify_key: Vec::new(),
            tamper_verify_sig: false,
        }
    }

    fn accessory_ltpk(&self) -> Vec<u8> {
        self.signing.verifying_key().to_bytes().to_vec()
    }

    fn paths(&self) -> Vec<(&str, Option<u8>)> {
        self.transcript.iter().map(|e| (e.path.as_str(), e.state)).collect()
    }

    fn saw_playback(&self) -> bool {
        self.transcript.iter().any(|e| !e.path.starts_with("/pair-"))
    }

    fn setup_m2(&self) -> Vec<u8> {
        tlv::encode(&[
            TlvItem::new(tlv::TLV_STATE, vec![0x02]),
            TlvItem::new(tlv::TLV_SALT, SYNTHETIC_SALT.to_vec()),
            TlvItem::new(tlv::TLV_PUBLIC_KEY, padded_bytes(&self.b_pub, GROUP_BYTES)),
        ])
    }

    fn setup_m4(&mut self, items: &[TlvItem]) -> Result<Vec<u8>> {
        let n = modulus();
        let a_bytes = tlv::get(items, tlv::TLV_PUBLIC_KEY).ok_or_else(|| anyhow!("M3 missing A"))?;
        let proof = tlv::get(items, tlv::TLV_PROOF).ok_or_else(|| anyhow!("M3 missing proof"))?;
        let a_pub = BigUint::from_bytes_be(a_bytes);
        let pad_a = padded_bytes(&a_pub, GROUP_BYTES);
        let pad_b = padded_bytes(&self.b_pub, GROUP_BYTES);
        let u = BigUint::from_bytes_be(&h(&[&pad_a, &pad_b]));
        let s = (a_pub * self.verifier.modpow(&u, &n)).modpow(&self.b, &n);
        let key = h(&[&padded_bytes(&s, GROUP_BYTES)]);
        let h_n = Sha512::digest(n.to_bytes_be());
        let h_g = Sha512::digest(generator().to_bytes_be());
        let hn_xor_hg: Vec<u8> = h_n.iter().zip(h_g.iter()).map(|(a, b)| a ^ b).collect();
        let expected = h(&[&hn_xor_hg, &h(&[b"Pair-Setup"]), &SYNTHETIC_SALT, &pad_a, &pad_b, &key]);
        if expected != proof {
            return Ok(tlv::encode(&[
                TlvItem::new(tlv::TLV_STATE, vec![0x04]),
                TlvItem::new(tlv::TLV_ERROR, vec![0x02]),
            ]));
        }
        let server_proof = h(&[&pad_a, &expected, &key]);
        self.srp_key = key;
        Ok(tlv::encode(&[
            TlvItem::new(tlv::TLV_STATE, vec![0x04]),
            TlvItem::new(tlv::TLV_PROOF, server_proof),
        ]))
    }

    fn setup_m6(&mut self, items: &[TlvItem]) -> Result<Vec<u8>> {
        let enc_key = hkdf_expand(&self.srp_key, b"Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info", 32);
        let sealed = tlv::get(items, tlv::TLV_ENCRYPTED_DATA).ok_or_else(|| anyhow!("M5 missing data"))?;
        let inner = tlv::decode(&chacha20poly1305_decrypt(&enc_key, &nonce_from_string("PS-Msg05"), sealed, &[])?)?;
        let id = tlv::get(&inner, tlv::TLV_IDENTIFIER).ok_or_else(|| anyhow!("M5 missing id"))?;
        let ltpk = tlv::get(&inner, tlv::TLV_PUBLIC_KEY).ok_or_else(|| anyhow!("M5 missing ltpk"))?;
        let sig = tlv::get(&inner, tlv::TLV_SIGNATURE).ok_or_else(|| anyhow!("M5 missing sig"))?;
        let controller_x = hkdf_expand(&self.srp_key, b"Pair-Setup-Controller-Sign-Salt", "Pair-Setup-Controller-Sign-Info", 32);
        if !identity::verify(ltpk, &[controller_x.as_slice(), id, ltpk].concat(), sig) {
            bail!("controller M5 signature rejected");
        }
        self.controller_ltpk = ltpk.to_vec();
        let accessory_x = hkdf_expand(&self.srp_key, b"Pair-Setup-Accessory-Sign-Salt", "Pair-Setup-Accessory-Sign-Info", 32);
        let ltpk = self.accessory_ltpk();
        let sig = self.signing.sign(&[accessory_x.as_slice(), SYNTHETIC_ACCESSORY_ID, &ltpk].concat());
        let inner = tlv::encode(&[
            TlvItem::new(tlv::TLV_IDENTIFIER, SYNTHETIC_ACCESSORY_ID.to_vec()),
            TlvItem::new(tlv::TLV_PUBLIC_KEY, ltpk),
            TlvItem::new(tlv::TLV_SIGNATURE, sig.to_bytes().to_vec()),
        ]);
        Ok(tlv::encode(&[
            TlvItem::new(tlv::TLV_STATE, vec![0x06]),
            TlvItem::new(tlv::TLV_ENCRYPTED_DATA, chacha20poly1305_encrypt(&enc_key, &nonce_from_string("PS-Msg06"), &inner, &[])?),
        ]))
    }

    fn verify_m2(&mut self, items: &[TlvItem]) -> Result<Vec<u8>> {
        let client_curve = tlv::get(items, tlv::TLV_PUBLIC_KEY).ok_or_else(|| anyhow!("verify M1 missing key"))?;
        let client: [u8; 32] = client_curve.try_into()?;
        let server_curve = x25519(SYNTHETIC_CURVE_SECRET, X25519_BASEPOINT_BYTES);
        let secret = x25519(SYNTHETIC_CURVE_SECRET, client);
        self.verify_key = hkdf_expand(&secret, b"Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info", 32);
        self.controller_curve = client_curve.to_vec();
        let mut sig = self.signing.sign(&[server_curve.as_slice(), SYNTHETIC_ACCESSORY_ID, client_curve].concat()).to_bytes();
        if self.tamper_verify_sig {
            sig[0] ^= 0x01;
        }
        let inner = tlv::encode(&[
            TlvItem::new(tlv::TLV_IDENTIFIER, SYNTHETIC_ACCESSORY_ID.to_vec()),
            TlvItem::new(tlv::TLV_SIGNATURE, sig.to_vec()),
        ]);
        Ok(tlv::encode(&[
            TlvItem::new(tlv::TLV_STATE, vec![0x02]),
            TlvItem::new(tlv::TLV_PUBLIC_KEY, server_curve.to_vec()),
            TlvItem::new(tlv::TLV_ENCRYPTED_DATA, chacha20poly1305_encrypt(&self.verify_key, &nonce_from_string("PV-Msg02"), &inner, &[])?),
        ]))
    }

    fn verify_m4(&mut self, items: &[TlvItem]) -> Result<Vec<u8>> {
        let sealed = tlv::get(items, tlv::TLV_ENCRYPTED_DATA).ok_or_else(|| anyhow!("verify M3 missing data"))?;
        let inner = tlv::decode(&chacha20poly1305_decrypt(&self.verify_key, &nonce_from_string("PV-Msg03"), sealed, &[])?)?;
        let id = tlv::get(&inner, tlv::TLV_IDENTIFIER).ok_or_else(|| anyhow!("verify M3 missing id"))?;
        let sig = tlv::get(&inner, tlv::TLV_SIGNATURE).ok_or_else(|| anyhow!("verify M3 missing sig"))?;
        let server_curve = x25519(SYNTHETIC_CURVE_SECRET, X25519_BASEPOINT_BYTES);
        let payload = [self.controller_curve.as_slice(), id, &server_curve].concat();
        if !identity::verify(&self.controller_ltpk, &payload, sig) {
            return Ok(tlv::encode(&[
                TlvItem::new(tlv::TLV_STATE, vec![0x04]),
                TlvItem::new(tlv::TLV_ERROR, vec![0x02]),
            ]));
        }
        Ok(tlv::encode(&[TlvItem::new(tlv::TLV_STATE, vec![0x04])]))
    }
}

impl PairTransport for SyntheticReceiver {
    fn post(&mut self, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<Vec<u8>> {
        let items = tlv::decode(body)?;
        let first = |typ| tlv::get(&items, typ).and_then(|v| v.first().copied());
        self.transcript.push(Exchange {
            path: path.to_string(),
            hkp: headers.iter().find(|(k, _)| k == "X-Apple-HKP").map(|(_, v)| v.clone()).unwrap_or_default(),
            state: first(tlv::TLV_STATE),
            method: first(tlv::TLV_METHOD),
            flags: first(tlv::TLV_FLAGS),
            fields: items.iter().map(|i| i.typ).collect(),
        });
        match (path, first(tlv::TLV_STATE)) {
            ("/pair-pin-start", None) => Ok(Vec::new()),
            ("/pair-setup", Some(0x01)) => Ok(self.setup_m2()),
            ("/pair-setup", Some(0x03)) => self.setup_m4(&items),
            ("/pair-setup", Some(0x05)) => self.setup_m6(&items),
            ("/pair-verify", Some(0x01)) => self.verify_m2(&items),
            ("/pair-verify", Some(0x03)) => self.verify_m4(&items),
            _ => bail!("synthetic receiver rejects {path}"),
        }
    }
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("airplay-fixture-{}-{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct EmptyError;

impl PairTransport for EmptyError {
    fn post(&mut self, _: &str, _: &[(String, String)], _: &[u8]) -> Result<Vec<u8>> {
        Ok(vec![tlv::TLV_STATE, 1, 0x02, tlv::TLV_ERROR, 0])
    }
}

fn reconnect(receiver: &mut SyntheticReceiver, record: &PairingRecord) -> Result<Proven> {
    let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, SYNTHETIC_PIN);
    session.use_record(record);
    session.pair_verify(receiver)?;
    assert_eq!(session.shared_secret.len(), 32);
    Ok(Proven::VerifiedReconnect)
}

fn paired_record() -> (SyntheticReceiver, PairingRecord) {
    let mut receiver = SyntheticReceiver::new(SYNTHETIC_PIN);
    let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, SYNTHETIC_PIN);
    session.pair_apple_tv(&mut receiver).unwrap();
    receiver.transcript.clear();
    (receiver, session.pairing_record().unwrap())
}

fn rejected(receiver: &mut SyntheticReceiver, record: &PairingRecord) -> PairingError {
    let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, SYNTHETIC_PIN);
    session.use_record(record);
    let err = session.pair_verify(receiver).unwrap_err();
    assert!(session.shared_secret.is_empty());
    assert!(session.session_key.is_empty());
    assert!(receiver.transcript.iter().all(|e| e.state == Some(0x01)));
    let typed = err.downcast_ref::<PairingError>().cloned().expect("typed pairing error");
    assert!(typed.requires_re_pair());
    typed
}

#[test]
fn homepod_transient_transcript_proves_pair_success_only() {
    let mut receiver = SyntheticReceiver::new(HOMEPOD_PIN);
    let mut session = PairingSession::new("fixture-transient-receiver", "");
    let proven = session.pair_homepod(&mut receiver).map(|_| Proven::PairSuccess).unwrap();
    assert_eq!(proven, Proven::PairSuccess);

    assert_eq!(receiver.paths(), vec![("/pair-setup", Some(0x01)), ("/pair-setup", Some(0x03))]);
    assert!(receiver.transcript.iter().all(|e| e.hkp == "4"));
    assert_eq!(receiver.transcript[0].method, Some(0x00));
    assert_eq!(receiver.transcript[0].flags, Some(0x10));
    assert_eq!(receiver.transcript[1].fields, vec![tlv::TLV_STATE, tlv::TLV_PUBLIC_KEY, tlv::TLV_PROOF]);
    assert_eq!(session.shared_secret, receiver.srp_key);
    assert_eq!(session.shared_secret.len(), 64);
    assert!(session.server_ltpk.is_empty());
    assert!(receiver.controller_ltpk.is_empty());
    assert!(session.pairing_record().is_none());
    assert!(!receiver.saw_playback());
}

#[test]
fn transient_pairing_rejects_wrong_receiver_pin() {
    let mut receiver = SyntheticReceiver::new("1111");
    let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, "");
    let err = session.pair_homepod(&mut receiver).unwrap_err();
    assert!(format!("{err:#}").contains("code=2"));
    assert!(session.shared_secret.is_empty());
}

#[test]
fn persistent_pin_transcript_separates_pair_success_and_verified_reconnect() {
    let mut receiver = SyntheticReceiver::new(SYNTHETIC_PIN);
    let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, &receiver.pin.clone());
    let proven = session.pair_apple_tv(&mut receiver).map(|_| Proven::PairSuccess).unwrap();
    assert_eq!(proven, Proven::PairSuccess);

    assert_eq!(
        receiver.paths(),
        vec![
            ("/pair-pin-start", None),
            ("/pair-setup", Some(0x01)),
            ("/pair-setup", Some(0x03)),
            ("/pair-setup", Some(0x05)),
        ]
    );
    assert!(receiver.transcript.iter().all(|e| e.hkp == "3"));
    assert_eq!(receiver.transcript[1].flags, None);
    assert_eq!(receiver.transcript[3].fields, vec![tlv::TLV_STATE, tlv::TLV_ENCRYPTED_DATA]);
    assert_eq!(session.server_ltpk, receiver.accessory_ltpk());
    let controller = session.identity.clone().unwrap();
    assert_eq!(receiver.controller_ltpk, controller.public_key().to_vec());
    let store = MemoryStore::default();
    store.save(&session.pairing_record().unwrap()).unwrap();
    let Stored::Paired(record) = store.load(SYNTHETIC_RECEIVER_ID).unwrap() else { panic!("record not saved") };
    assert_eq!(record.accessory_public_key.to_vec(), receiver.accessory_ltpk());
    assert_eq!(record.controller_key.public_key(), controller.public_key());

    receiver.transcript.clear();
    let proven = reconnect(&mut receiver, &record).unwrap();
    assert_eq!(proven, Proven::VerifiedReconnect);
    assert_eq!(receiver.paths(), vec![("/pair-verify", Some(0x01)), ("/pair-verify", Some(0x03))]);
    assert!(receiver.transcript.iter().all(|e| e.hkp == "3"));
    assert!(!receiver.saw_playback());
}

#[test]
fn reloaded_record_verifies_receiver_after_process_state_is_dropped() {
    let tmp = TempDir::new();
    let mut receiver = SyntheticReceiver::new(SYNTHETIC_PIN);
    {
        let store = FileStore::new(&tmp.0);
        let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, SYNTHETIC_PIN);
        session.load_or_create_identity(&store).unwrap();
        session.pair_apple_tv(&mut receiver).unwrap();
        store.save(&session.pairing_record().unwrap()).unwrap();
    }
    receiver.transcript.clear();
    let Stored::Paired(record) = FileStore::new(&tmp.0).load(SYNTHETIC_RECEIVER_ID).unwrap() else { panic!("not reloaded") };
    assert_eq!(record.accessory_id.as_bytes(), SYNTHETIC_ACCESSORY_ID);
    assert_eq!(reconnect(&mut receiver, &record).unwrap(), Proven::VerifiedReconnect);
    assert_eq!(receiver.paths(), vec![("/pair-verify", Some(0x01)), ("/pair-verify", Some(0x03))]);
}

#[test]
fn legacy_pem_state_re_pairs_explicitly_and_stays_intact() {
    let tmp = TempDir::new();
    let legacy = tmp.0.join(format!("{SYNTHETIC_RECEIVER_ID}.pem"));
    std::fs::write(&legacy, b"-----BEGIN ED25519 PRIVATE KEY-----\n").unwrap();
    let store = FileStore::new(&tmp.0);
    assert!(matches!(store.load(SYNTHETIC_RECEIVER_ID).unwrap(), Stored::RePairRequired { .. }));
    let mut receiver = SyntheticReceiver::new(SYNTHETIC_PIN);
    let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, SYNTHETIC_PIN);
    session.load_or_create_identity(&store).unwrap();
    session.pair_apple_tv(&mut receiver).unwrap();
    store.save(&session.pairing_record().unwrap()).unwrap();
    assert_eq!(std::fs::read(&legacy).unwrap(), b"-----BEGIN ED25519 PRIVATE KEY-----\n");
    assert!(matches!(store.load(SYNTHETIC_RECEIVER_ID).unwrap(), Stored::Paired(_)));
}

#[test]
fn persistent_pairing_rejects_wrong_pin_before_identity_exchange() {
    let mut receiver = SyntheticReceiver::new(SYNTHETIC_PIN);
    let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, "9999");
    let err = session.pair_apple_tv(&mut receiver).unwrap_err();
    assert!(format!("{err:#}").contains("code=2"));
    assert_eq!(receiver.transcript.len(), 3);
    assert!(receiver.controller_ltpk.is_empty());
}

#[test]
fn reconnect_rejects_receiver_signing_with_a_different_key() {
    let (genuine, record) = paired_record();
    let mut impostor = SyntheticReceiver::with_signing_seed(SYNTHETIC_PIN, SYNTHETIC_IMPOSTOR_SEED);
    impostor.controller_ltpk = genuine.controller_ltpk.clone();
    assert_eq!(rejected(&mut impostor, &record), PairingError::SignatureInvalid);
    assert_eq!(impostor.transcript.len(), 1);
}

#[test]
fn reconnect_rejects_wrong_trusted_key_in_record() {
    let (mut receiver, mut record) = paired_record();
    record.accessory_public_key = SigningKey::from_bytes(&SYNTHETIC_IMPOSTOR_SEED).verifying_key().to_bytes();
    assert_eq!(rejected(&mut receiver, &record), PairingError::SignatureInvalid);
    assert_eq!(receiver.transcript.len(), 1);
}

#[test]
fn reconnect_rejects_changed_accessory_id() {
    let (mut receiver, mut record) = paired_record();
    record.accessory_id = "00000000-OTHE-RACC-ESSO-RY0000000000".into();
    assert_eq!(rejected(&mut receiver, &record), PairingError::AccessoryIdMismatch);
    assert_eq!(receiver.transcript.len(), 1);
}

#[test]
fn reconnect_rejects_tampered_signature() {
    let (mut receiver, record) = paired_record();
    receiver.tamper_verify_sig = true;
    assert_eq!(rejected(&mut receiver, &record), PairingError::SignatureInvalid);
    assert_eq!(receiver.transcript.len(), 1);
}

#[test]
fn reconnect_without_trusted_key_fails_before_contacting_receiver() {
    let (mut receiver, record) = paired_record();
    for (ltpk, id) in [(Vec::new(), record.accessory_id.clone()), (vec![0u8; 31], record.accessory_id.clone()), (record.accessory_public_key.to_vec(), String::new())] {
        let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, SYNTHETIC_PIN);
        session.identity = Some(record.controller_key.clone());
        session.server_ltpk = ltpk;
        session.accessory_id = id;
        let err = session.pair_verify(&mut receiver).unwrap_err();
        assert_eq!(err.downcast_ref::<PairingError>(), Some(&PairingError::MissingTrust));
        assert!(session.shared_secret.is_empty());
    }
    assert!(receiver.transcript.is_empty());
}

#[test]
fn empty_error_tlvs_return_typed_protocol_errors() {
    let (_, record) = paired_record();
    let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, SYNTHETIC_PIN);
    session.use_record(&record);
    let err = session.pair_verify(&mut EmptyError).unwrap_err();
    assert_eq!(err.downcast_ref::<PairingError>(), Some(&PairingError::Protocol { step: "pair-verify M2", code: None }));
    let err = PairingSession::new(SYNTHETIC_RECEIVER_ID, SYNTHETIC_PIN).pair_homepod(&mut EmptyError).unwrap_err();
    assert_eq!(err.downcast_ref::<PairingError>(), Some(&PairingError::Protocol { step: "M2", code: None }));
    assert!(format!("{err:#}").contains("empty error code"));
}

#[test]
fn truncated_error_tlv_is_rejected_without_panic() {
    struct Truncated;
    impl PairTransport for Truncated {
        fn post(&mut self, _: &str, _: &[(String, String)], _: &[u8]) -> Result<Vec<u8>> {
            Ok(vec![tlv::TLV_ERROR, 4, 0x02])
        }
    }
    let (_, record) = paired_record();
    let mut session = PairingSession::new(SYNTHETIC_RECEIVER_ID, SYNTHETIC_PIN);
    session.use_record(&record);
    assert!(format!("{:#}", session.pair_verify(&mut Truncated).unwrap_err()).contains("truncated TLV"));
}

#[test]
fn reconnect_fails_when_receiver_does_not_know_controller() {
    let mut receiver = SyntheticReceiver::new(SYNTHETIC_PIN);
    receiver.controller_ltpk = Identity::generate().public_key().to_vec();
    let (_, mut record) = paired_record();
    record.controller_key = Identity::generate();
    let err = reconnect(&mut receiver, &record).unwrap_err();
    assert!(format!("{err:#}").contains("pair-verify M4 error: code=2"));
    assert!(err.downcast_ref::<PairingError>().unwrap().requires_re_pair());
}
