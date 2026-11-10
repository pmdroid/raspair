use super::crypto::{aes128_ctr_apply, aes128_gcm_decrypt, aes128_gcm_encrypt, generate_curve25519_keypair, shared_secret};
use super::identity::{self, Identity};
use super::session::{PairTransport, PairingError};
use super::srp::{padded_bytes, SrpClient, LEGACY_GROUP};
use super::store::{AuthMethod, PairingRecord};
use crate::session::SessionError;
use anyhow::{anyhow, Context, Result};
use num_bigint::BigUint;
use plist::{Dictionary, Value};
use sha2::{Digest, Sha512};

pub(crate) const SETUP_PATH: &str = "/pair-setup-pin";
const PLIST_TYPE: &str = "application/x-apple-binary-plist";

pub struct LegacyPairing {
    receiver_id: String,
    identity: Identity,
    salt: Vec<u8>,
    b_pub: BigUint,
}

pub fn controller_id(identity: &Identity) -> String {
    hex::encode_upper(&identity.public_key()[..8])
}

fn derive(label: &str, secret: &[u8]) -> Vec<u8> {
    Sha512::new().chain_update(label).chain_update(secret).finalize()[..16].to_vec()
}

fn bump(mut iv: Vec<u8>, by: u8) -> Vec<u8> {
    iv[15] = iv[15].wrapping_add(by);
    iv
}

fn rejected(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<SessionError>(), Some(SessionError::ReceiverRejected { status: 401 | 403 | 470 }))
}

fn post_plist(t: &mut dyn PairTransport, entries: Vec<(&str, Value)>) -> Result<Dictionary> {
    let dict: Dictionary = entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let mut body = Vec::new();
    Value::Dictionary(dict).to_writer_binary(&mut body)?;
    let headers = [("Content-Type".to_string(), PLIST_TYPE.to_string())];
    let resp = t.post(SETUP_PATH, &headers, &body)?;
    plist::from_bytes(&resp).context("decode binary plist")
}

fn data<'a>(dict: &'a Dictionary, key: &str) -> Result<&'a [u8]> {
    dict.get(key).and_then(Value::as_data).ok_or_else(|| anyhow!("legacy pairing reply missing {key}"))
}

impl LegacyPairing {
    pub fn begin(receiver_id: &str, identity: Identity, t: &mut dyn PairTransport) -> Result<Self> {
        t.post("/pair-pin-start", &[], &[]).context("pin-start")?;
        let user = controller_id(&identity);
        let reply = post_plist(t, vec![("method", Value::from("pin")), ("user", Value::from(user))])
            .context("pair-setup-pin user")?;
        Ok(Self {
            receiver_id: receiver_id.to_string(),
            salt: data(&reply, "salt")?.to_vec(),
            b_pub: BigUint::from_bytes_be(data(&reply, "pk")?),
            identity,
        })
    }

    pub fn finish(&self, t: &mut dyn PairTransport, pin: &str) -> Result<PairingRecord> {
        let user = controller_id(&self.identity);
        let mut srp = SrpClient::with_group(&LEGACY_GROUP, &user, pin);
        let a = srp.generate_a();
        srp.process_b(&self.salt, self.b_pub.clone()).context("process B")?;
        let proof = srp.compute_m1();
        let entries = vec![("pk", Value::Data(padded_bytes(&a, LEGACY_GROUP.bytes))), ("proof", Value::Data(proof))];
        let reply = match post_plist(t, entries) {
            Err(e) if rejected(&e) => {
                return Err(PairingError::Protocol { step: "pair-setup-pin proof", code: Some(0x02) }.into())
            }
            other => other.context("pair-setup-pin proof")?,
        };
        if !srp.verify_m2(data(&reply, "proof")?) {
            return Err(PairingError::ReceiverProofInvalid.into());
        }

        let key = derive("Pair-Setup-AES-Key", srp.session_key());
        let iv = derive("Pair-Setup-AES-IV", srp.session_key());
        let (epk, tag) = aes128_gcm_encrypt(&key, &bump(iv.clone(), 1), &self.identity.public_key())?;
        let reply = post_plist(t, vec![("epk", Value::Data(epk)), ("authTag", Value::Data(tag))])
            .context("pair-setup-pin key exchange")?;
        let receiver_key = aes128_gcm_decrypt(&key, &bump(iv, 2), data(&reply, "epk")?, data(&reply, "authTag")?)
            .ok()
            .and_then(|k| <[u8; 32]>::try_from(k).ok())
            .filter(|k| ed25519_dalek::VerifyingKey::from_bytes(k).is_ok())
            .ok_or(PairingError::ReceiverProofInvalid)?;
        Ok(PairingRecord {
            method: AuthMethod::LegacyPin,
            receiver_id: self.receiver_id.clone(),
            controller_id: user,
            accessory_id: self.receiver_id.clone(),
            controller_key: self.identity.clone(),
            accessory_public_key: Some(receiver_key),
            password: None,
        })
    }
}

pub fn verify(record: &PairingRecord, t: &mut dyn PairTransport) -> Result<Vec<u8>> {
    if record.method != AuthMethod::LegacyPin {
        return Err(PairingError::InvalidRecord.into());
    }
    let trusted = record
        .accessory_public_key
        .filter(|k| ed25519_dalek::VerifyingKey::from_bytes(k).is_ok())
        .ok_or(PairingError::MissingTrust)?;
    let (private, public) = generate_curve25519_keypair();
    let hello = [&[1, 0, 0, 0][..], &public, &record.controller_key.public_key()].concat();
    let reply = t.post("/pair-verify", &[], &hello).context("pair-verify legacy 1")?;
    if reply.len() != 96 {
        return Err(PairingError::Protocol { step: "pair-verify legacy 1", code: None }.into());
    }
    let (receiver_curve, sealed) = reply.split_at(32);
    let secret = shared_secret(&private, receiver_curve)?;
    let key = derive("Pair-Verify-AES-Key", &secret);
    let iv = derive("Pair-Verify-AES-IV", &secret);
    let mut signature = sealed.to_vec();
    aes128_ctr_apply(&key, &iv, 0, &mut signature)?;
    if !identity::verify(&trusted, &[receiver_curve, &public].concat(), &signature) {
        return Err(PairingError::SignatureInvalid.into());
    }
    let mut ours = record.controller_key.sign(&[&public[..], receiver_curve].concat());
    aes128_ctr_apply(&key, &iv, 64, &mut ours)?;
    match t.post("/pair-verify", &[], &[&[0u8; 4][..], &ours].concat()) {
        Err(e) if rejected(&e) => Err(PairingError::Protocol { step: "pair-verify legacy 2", code: Some(0x02) }.into()),
        other => other.context("pair-verify legacy 2").map(|_| secret.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::challenge::{PinPairingError, PinPairings, PinSource};
    use crate::pairing::srp::h1;
    use crate::pairing::store::{CredentialStore, FileStore, MemoryStore, Stored};
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::{Arc, Mutex};
    use x25519_dalek::{x25519, X25519_BASEPOINT_BYTES};

    const PIN: &str = "1234";
    const RECEIVER: &str = "legacy-fixture";
    const SALT: [u8; 16] = [0x11; 16];
    const SRP_SECRET: [u8; 32] = [0x77; 32];
    const CURVE_SECRET: [u8; 32] = [0x33; 32];
    const RECEIVER_SEED: [u8; 32] = [0xA5; 32];
    const IMPOSTOR_SEED: [u8; 32] = [0x5A; 32];

    type Handshake = ([u8; 32], Vec<u8>, Vec<u8>, Vec<u8>);

    #[derive(Default)]
    struct Fault {
        bad_m2: bool,
        bad_epk: bool,
    }

    struct Receiver {
        signing: SigningKey,
        transcript: Vec<(String, Vec<String>)>,
        user: String,
        b_pub: BigUint,
        srp_key: Vec<u8>,
        controller: Vec<u8>,
        verify: Option<Handshake>,
        secret: Vec<u8>,
        fault: Fault,
    }

    fn n() -> BigUint {
        BigUint::parse_bytes(LEGACY_GROUP.n_hex.as_bytes(), 16).unwrap()
    }

    fn g() -> BigUint {
        BigUint::from(LEGACY_GROUP.g)
    }

    fn verifier(user: &str) -> BigUint {
        g().modpow(&BigUint::from_bytes_be(&h1(&[&SALT, &h1(&[format!("{user}:{PIN}").as_bytes()])])), &n())
    }

    fn reject() -> anyhow::Error {
        SessionError::ReceiverRejected { status: 470 }.into()
    }

    impl Receiver {
        fn new(seed: [u8; 32]) -> Self {
            Self {
                signing: SigningKey::from_bytes(&seed),
                transcript: Vec::new(),
                user: String::new(),
                b_pub: BigUint::default(),
                srp_key: Vec::new(),
                controller: Vec::new(),
                verify: None,
                secret: Vec::new(),
                fault: Fault::default(),
            }
        }

        fn public(&self) -> [u8; 32] {
            self.signing.verifying_key().to_bytes()
        }

        fn paths(&self) -> Vec<(&str, Vec<&str>)> {
            self.transcript.iter().map(|(p, k)| (p.as_str(), k.iter().map(String::as_str).collect())).collect()
        }

        fn reply(entries: Vec<(&str, Value)>) -> Result<Vec<u8>> {
            let mut out = Vec::new();
            Value::Dictionary(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect()).to_writer_binary(&mut out)?;
            Ok(out)
        }

        fn setup(&mut self, body: &Dictionary) -> Result<Vec<u8>> {
            if let Some(user) = body.get("user").and_then(Value::as_string) {
                assert_eq!(body.get("method").and_then(Value::as_string), Some("pin"));
                self.user = user.to_string();
                let k = BigUint::from_bytes_be(&h1(&[&n().to_bytes_be(), &padded_bytes(&g(), 256)]));
                self.b_pub = (k * verifier(user) + g().modpow(&BigUint::from_bytes_be(&SRP_SECRET), &n())) % n();
                return Self::reply(vec![("pk", Value::Data(padded_bytes(&self.b_pub, 256))), ("salt", Value::Data(SALT.to_vec()))]);
            }
            if let Some(proof) = body.get("proof").and_then(Value::as_data) {
                let a = BigUint::from_bytes_be(data(body, "pk")?);
                let u = BigUint::from_bytes_be(&h1(&[&padded_bytes(&a, 256), &padded_bytes(&self.b_pub, 256)]));
                let s = (&a * verifier(&self.user).modpow(&u, &n())).modpow(&BigUint::from_bytes_be(&SRP_SECRET), &n());
                let s = s.to_bytes_be();
                let key = [h1(&[&s, &[0, 0, 0, 0]]), h1(&[&s, &[0, 0, 0, 1]])].concat();
                let xor: Vec<u8> = h1(&[&n().to_bytes_be()]).iter().zip(h1(&[&[2]])).map(|(x, y)| x ^ y).collect();
                let m1 = h1(&[&xor, &h1(&[self.user.as_bytes()]), &SALT, &a.to_bytes_be(), &self.b_pub.to_bytes_be(), &key]);
                if m1 != proof {
                    return Err(reject());
                }
                let mut m2 = h1(&[&a.to_bytes_be(), &m1, &key]);
                if self.fault.bad_m2 {
                    m2[0] ^= 1;
                }
                self.srp_key = key;
                return Self::reply(vec![("proof", Value::Data(m2))]);
            }
            let key = derive("Pair-Setup-AES-Key", &self.srp_key);
            let iv = derive("Pair-Setup-AES-IV", &self.srp_key);
            self.controller = aes128_gcm_decrypt(&key, &bump(iv.clone(), 1), data(body, "epk")?, data(body, "authTag")?)?;
            let (mut epk, tag) = aes128_gcm_encrypt(&key, &bump(iv, 2), &self.public())?;
            if self.fault.bad_epk {
                epk[0] ^= 1;
            }
            Self::reply(vec![("epk", Value::Data(epk)), ("authTag", Value::Data(tag))])
        }

        fn pair_verify(&mut self, body: &[u8]) -> Result<Vec<u8>> {
            let ours = x25519(CURVE_SECRET, X25519_BASEPOINT_BYTES);
            if body[..4] == [1, 0, 0, 0] {
                let theirs = body[4..36].to_vec();
                let secret = x25519(CURVE_SECRET, theirs.as_slice().try_into()?);
                let (key, iv) = (derive("Pair-Verify-AES-Key", &secret), derive("Pair-Verify-AES-IV", &secret));
                let mut sig = self.signing.sign(&[&ours[..], &theirs].concat()).to_bytes().to_vec();
                aes128_ctr_apply(&key, &iv, 0, &mut sig)?;
                self.verify = Some((secret, key, iv, theirs));
                return Ok([&ours[..], &sig].concat());
            }
            let (secret, key, iv, theirs) = self.verify.take().ok_or_else(reject)?;
            let mut sig = body[4..].to_vec();
            aes128_ctr_apply(&key, &iv, 64, &mut sig)?;
            if !identity::verify(&self.controller, &[&theirs[..], &ours].concat(), &sig) {
                return Err(reject());
            }
            self.secret = secret.to_vec();
            Ok(Vec::new())
        }
    }

    impl PairTransport for Receiver {
        fn post(&mut self, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<Vec<u8>> {
            match path {
                "/pair-pin-start" => {
                    self.transcript.push((path.into(), Vec::new()));
                    Ok(Vec::new())
                }
                SETUP_PATH => {
                    assert!(headers.iter().any(|(k, v)| k == "Content-Type" && v == PLIST_TYPE));
                    let dict: Dictionary = plist::from_bytes(body)?;
                    self.transcript.push((path.into(), dict.keys().cloned().collect()));
                    self.setup(&dict)
                }
                "/pair-verify" => {
                    self.transcript.push((path.into(), vec![hex::encode(&body[..4])]));
                    self.pair_verify(body)
                }
                _ => anyhow::bail!("legacy fixture rejects {path}"),
            }
        }
    }

    #[derive(Clone)]
    struct Shared(Arc<Mutex<Receiver>>);

    impl PairTransport for Shared {
        fn post(&mut self, path: &str, headers: &[(String, String)], body: &[u8]) -> Result<Vec<u8>> {
            self.0.lock().unwrap().post(path, headers, body)
        }
    }

    fn pair(receiver: Receiver, store: Arc<dyn CredentialStore>, pin: &str) -> (Receiver, Result<(), PinPairingError>) {
        let shared = Shared(Arc::new(Mutex::new(receiver)));
        let pairings = PinPairings::new(store);
        let challenge = pairings.begin_legacy(RECEIVER, PinSource::OnScreen, Box::new(shared.clone())).unwrap();
        let result = pairings.respond(&challenge.id, pin);
        drop(pairings);
        (Arc::try_unwrap(shared.0).ok().unwrap().into_inner().unwrap(), result)
    }

    fn saved(store: &dyn CredentialStore) -> Option<PairingRecord> {
        match store.load(RECEIVER).unwrap() {
            Stored::Paired(record) => Some(record),
            _ => None,
        }
    }

    fn paired() -> (Receiver, PairingRecord) {
        let store = Arc::new(MemoryStore::default());
        let (mut receiver, result) = pair(Receiver::new(RECEIVER_SEED), store.clone(), PIN);
        result.unwrap();
        receiver.transcript.clear();
        (receiver, saved(&*store).unwrap())
    }

    #[test]
    fn initial_pin_pairing_transcript_saves_verified_legacy_record() {
        let store = Arc::new(MemoryStore::default());
        let (receiver, result) = pair(Receiver::new(RECEIVER_SEED), store.clone(), PIN);
        result.unwrap();
        assert_eq!(
            receiver.paths(),
            vec![
                ("/pair-pin-start", vec![]),
                (SETUP_PATH, vec!["method", "user"]),
                (SETUP_PATH, vec!["pk", "proof"]),
                (SETUP_PATH, vec!["epk", "authTag"]),
            ]
        );
        let record = saved(&*store).unwrap();
        assert_eq!(record.method, AuthMethod::LegacyPin);
        assert_eq!(record.accessory_public_key, Some(receiver.public()));
        assert_eq!(receiver.controller, record.controller_key.public_key().to_vec());
        assert_eq!(receiver.user, record.controller_id);
        assert!(!receiver.transcript.iter().any(|(p, _)| p == "/pair-verify" || p == "/pair-setup"));
    }

    #[test]
    fn saved_credential_reconnect_verifies_receiver_after_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut receiver, result) = pair(Receiver::new(RECEIVER_SEED), Arc::new(FileStore::new(tmp.path())), PIN);
        result.unwrap();
        receiver.transcript.clear();
        let record = saved(&FileStore::new(tmp.path())).unwrap();
        assert_eq!(record.method, AuthMethod::LegacyPin);
        let secret = verify(&record, &mut receiver).unwrap();
        assert_eq!(secret, receiver.secret);
        assert_eq!(receiver.paths(), vec![("/pair-verify", vec!["01000000"]), ("/pair-verify", vec!["00000000"])]);
    }

    #[test]
    fn wrong_pin_stops_before_key_exchange_and_keeps_prior_record() {
        let (_, prior) = paired();
        let store = Arc::new(MemoryStore::default());
        store.save(&prior).unwrap();
        let (receiver, result) = pair(Receiver::new(RECEIVER_SEED), store.clone(), "9999");
        assert_eq!(result, Err(PinPairingError::WrongCode));
        assert_eq!(receiver.transcript.len(), 3);
        assert!(receiver.controller.is_empty());
        assert_eq!(saved(&*store).unwrap().controller_key.seed(), prior.controller_key.seed());
    }

    #[test]
    fn invalid_receiver_proofs_are_rejected_without_saving() {
        for fault in [Fault { bad_m2: true, ..Fault::default() }, Fault { bad_epk: true, ..Fault::default() }] {
            let bad_m2 = fault.bad_m2;
            let mut receiver = Receiver::new(RECEIVER_SEED);
            receiver.fault = fault;
            let store = Arc::new(MemoryStore::default());
            let (receiver, result) = pair(receiver, store.clone(), PIN);
            let Err(PinPairingError::Failed(message)) = result else { panic!("accepted invalid proof: {result:?}") };
            assert!(message.contains("pairing proof is invalid"), "{message}");
            assert_eq!(receiver.transcript.len(), if bad_m2 { 3 } else { 4 });
            assert!(saved(&*store).is_none());
        }
    }

    #[test]
    fn changed_receiver_identity_fails_reconnect_before_controller_signs() {
        let (genuine, record) = paired();
        let mut impostor = Receiver::new(IMPOSTOR_SEED);
        impostor.controller = genuine.controller.clone();
        let err = verify(&record, &mut impostor).unwrap_err();
        assert_eq!(err.downcast_ref::<PairingError>(), Some(&PairingError::SignatureInvalid));
        assert_eq!(impostor.transcript.len(), 1);

        let (mut receiver, mut record) = paired();
        record.accessory_public_key = Some(SigningKey::from_bytes(&IMPOSTOR_SEED).verifying_key().to_bytes());
        let err = verify(&record, &mut receiver).unwrap_err();
        assert_eq!(err.downcast_ref::<PairingError>(), Some(&PairingError::SignatureInvalid));
        assert_eq!(receiver.transcript.len(), 1);
    }

    #[test]
    fn receiver_that_forgot_controller_requires_re_pairing() {
        let (mut receiver, record) = paired();
        receiver.controller = Identity::generate().public_key().to_vec();
        let err = verify(&record, &mut receiver).unwrap_err();
        let typed = err.downcast_ref::<PairingError>().unwrap();
        assert!(typed.requires_re_pair());
        assert!(receiver.secret.is_empty());
    }

    #[test]
    fn hap_record_is_not_reused_for_legacy_verify() {
        let (mut receiver, mut record) = paired();
        record.method = AuthMethod::PairSetupPin;
        let err = verify(&record, &mut receiver).unwrap_err();
        assert_eq!(err.downcast_ref::<PairingError>(), Some(&PairingError::InvalidRecord));
        assert!(receiver.transcript.is_empty());
    }
}
