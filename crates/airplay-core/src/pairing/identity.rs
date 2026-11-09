//! Ed25519 controller identity persisted as PKCS#8 PEM under
//! `$AIRPLAY_CONFIG_DIR` (or `~/.airplay`). The PEM label matches the Go
//! reference ("ED25519 PRIVATE KEY") so identities are interchangeable and an
//! Apple TV stays paired across implementations.

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use std::fs;
use std::path::PathBuf;

const IDENTITY_DIR: &str = ".airplay";
const PEM_LABEL: &str = "ED25519 PRIVATE KEY";

#[derive(Clone)]
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut OsRng),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.signing.sign(msg).to_bytes().to_vec()
    }
}

/// Verify an Ed25519 signature given a 32-byte public key.
pub fn verify(public_key: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    let pk: [u8; 32] = match public_key.try_into() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let vk = match VerifyingKey::from_bytes(&pk) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let signature = match Signature::from_slice(sig) {
        Ok(s) => s,
        Err(_) => return false,
    };
    vk.verify(msg, &signature).is_ok()
}

fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AIRPLAY_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(IDENTITY_DIR)
}

fn key_path(device_id: &str) -> PathBuf {
    config_dir().join(format!("{device_id}.pem"))
}

pub fn identity_exists(device_id: &str) -> bool {
    key_path(device_id).exists()
}

pub fn load(device_id: &str) -> Result<Identity> {
    let path = key_path(device_id);
    let data = fs::read(&path).with_context(|| format!("read identity {}", path.display()))?;
    let parsed = pem::parse(&data).context("decode identity PEM")?;
    let signing = SigningKey::from_pkcs8_der(parsed.contents())
        .map_err(|e| anyhow!("parse PKCS8 identity: {e}"))?;
    Ok(Identity { signing })
}

pub fn save(device_id: &str, ident: &Identity) -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let der = ident
        .signing
        .to_pkcs8_der()
        .map_err(|e| anyhow!("encode PKCS8 identity: {e}"))?;
    let pem_doc = pem::Pem::new(PEM_LABEL, der.as_bytes().to_vec());
    fs::write(key_path(device_id), pem::encode(&pem_doc)).context("write identity PEM")?;
    Ok(())
}
