//! SRP-6a client (3072-bit group, SHA-512) as used by AirPlay pair-setup.
//! Ported byte-for-byte from the Go reference; the padding rules around `g`
//! in `k`/`u` versus the raw generator byte in M1 are load-bearing — getting
//! them wrong yields an M4 authentication error (0x02).

use num_bigint::BigUint;
use num_traits::Zero;
use sha1::Sha1;
use sha2::{Digest, Sha512};

const N_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";

pub(super) const GROUP_BYTES: usize = 384; // 3072 bits

const N_2048_HEX: &str = "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050A37329CBB4A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50E8083969EDB767B0CF6095179A163AB3661A05FBD5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF747359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A436C6481F1D2B9078717461A5B9D32E688F87748544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB3786160279004E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DBFBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73";

pub struct SrpGroup {
    pub(super) n_hex: &'static str,
    pub(super) g: u32,
    pub(super) bytes: usize,
    pub(super) hash: fn(&[&[u8]]) -> Vec<u8>,
    pub(super) apple_variant: bool,
}

pub const HAP_GROUP: SrpGroup = SrpGroup { n_hex: N_HEX, g: 5, bytes: GROUP_BYTES, hash: h, apple_variant: false };
pub const LEGACY_GROUP: SrpGroup = SrpGroup { n_hex: N_2048_HEX, g: 2, bytes: 256, hash: h1, apple_variant: true };

#[cfg(test)]
pub(super) fn modulus() -> BigUint {
    BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("valid N hex")
}

#[cfg(test)]
pub(super) fn generator() -> BigUint {
    BigUint::from(5u32)
}

pub(super) fn h(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha512::new();
    for p in parts {
        hasher.update(p);
    }
    hasher.finalize().to_vec()
}

pub(super) fn h1(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha1::new();
    for p in parts {
        hasher.update(p);
    }
    hasher.finalize().to_vec()
}

/// Big-endian bytes of `n`, left-padded with zeros to `size`.
pub fn padded_bytes(n: &BigUint, size: usize) -> Vec<u8> {
    let b = n.to_bytes_be();
    if b.len() >= size {
        return b;
    }
    let mut out = vec![0u8; size];
    out[size - b.len()..].copy_from_slice(&b);
    out
}

fn mod_sub(a: &BigUint, b: &BigUint, n: &BigUint) -> BigUint {
    let am = a % n;
    let bm = b % n;
    if am >= bm {
        (am - bm) % n
    } else {
        (am + n - bm) % n
    }
}

// k = H(N | PAD(g)) per RFC 5054; g padded to the modulus length.
#[cfg(test)]
pub(super) fn compute_k(n: &BigUint, g: &BigUint) -> BigUint {
    let digest = h(&[&n.to_bytes_be(), &padded_bytes(g, GROUP_BYTES)]);
    BigUint::from_bytes_be(&digest)
}

pub struct SrpClient {
    group: &'static SrpGroup,
    username: String,
    password: String,
    n: BigUint,
    g: BigUint,
    k: BigUint,
    a: BigUint,
    a_pub: BigUint,
    salt: Vec<u8>,
    b_pub: BigUint,
    session_key: Vec<u8>,
    m1: Vec<u8>,
}

impl SrpClient {
    pub fn new(username: &str, password: &str) -> Self {
        Self::with_group(&HAP_GROUP, username, password)
    }

    pub fn with_group(group: &'static SrpGroup, username: &str, password: &str) -> Self {
        let n = BigUint::parse_bytes(group.n_hex.as_bytes(), 16).expect("valid N hex");
        let g = BigUint::from(group.g);
        let k = BigUint::from_bytes_be(&(group.hash)(&[&n.to_bytes_be(), &padded_bytes(&g, group.bytes)]));
        Self {
            group,
            username: username.to_string(),
            password: password.to_string(),
            n,
            g,
            k,
            a: BigUint::zero(),
            a_pub: BigUint::zero(),
            salt: Vec::new(),
            b_pub: BigUint::zero(),
            session_key: Vec::new(),
            m1: Vec::new(),
        }
    }

    /// Generate the client ephemeral A = g^a mod N (a is 64 random bytes).
    pub fn generate_a(&mut self) -> BigUint {
        let mut priv_bytes = [0u8; 64];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut priv_bytes);
        self.a = BigUint::from_bytes_be(&priv_bytes);
        self.a_pub = self.g.modpow(&self.a, &self.n);
        self.a_pub.clone()
    }

    pub fn process_b(&mut self, salt: &[u8], b: BigUint) -> anyhow::Result<()> {
        self.salt = salt.to_vec();
        self.b_pub = b;

        let b_mod = &self.b_pub % &self.n;
        if b_mod.is_zero() {
            anyhow::bail!("B mod N == 0");
        }

        let x = self.compute_x();
        let u = self.compute_u();

        // S = (B - k*g^x)^(a + u*x) mod N
        let gx = self.g.modpow(&x, &self.n);
        let kgx = &self.k * &gx;
        let base = mod_sub(&b_mod, &kgx, &self.n);
        let aux = &self.a + &u * &x;
        let s = base.modpow(&aux, &self.n);

        self.session_key = if self.group.apple_variant {
            let s = s.to_bytes_be();
            [(self.group.hash)(&[&s, &[0, 0, 0, 0]]), (self.group.hash)(&[&s, &[0, 0, 0, 1]])].concat()
        } else {
            h(&[&padded_bytes(&s, GROUP_BYTES)])
        };
        Ok(())
    }

    pub fn compute_m1(&mut self) -> Vec<u8> {
        let hash = self.group.hash;
        let h_n = hash(&[&self.n.to_bytes_be()]);
        // H(g) over the raw generator byte (0x05), NOT PAD(g) — unlike k and u.
        let h_g = hash(&[&self.g.to_bytes_be()]);
        let hn_xor_hg: Vec<u8> = h_n.iter().zip(&h_g).map(|(a, b)| a ^ b).collect();
        let hi = hash(&[self.username.as_bytes()]);

        self.m1 = hash(&[
            &hn_xor_hg,
            &hi,
            self.salt_term(),
            &padded_bytes(&self.a_pub, self.proof_width()),
            &padded_bytes(&self.b_pub, self.proof_width()),
            &self.session_key,
        ]);
        self.m1.clone()
    }

    pub fn verify_m2(&self, server_proof: &[u8]) -> bool {
        let expected = (self.group.hash)(&[
            &padded_bytes(&self.a_pub, self.proof_width()),
            &self.m1,
            &self.session_key,
        ]);
        expected == server_proof
    }

    pub fn session_key(&self) -> &[u8] {
        &self.session_key
    }

    fn proof_width(&self) -> usize {
        if self.group.apple_variant {
            0
        } else {
            self.group.bytes
        }
    }

    fn salt_term(&self) -> &[u8] {
        match self.group.apple_variant {
            true => &self.salt[self.salt.iter().position(|b| *b != 0).unwrap_or(self.salt.len())..],
            false => &self.salt,
        }
    }

    fn compute_x(&self) -> BigUint {
        let hash = self.group.hash;
        let ip = hash(&[format!("{}:{}", self.username, self.password).as_bytes()]);
        let x = hash(&[self.salt_term(), &ip]);
        BigUint::from_bytes_be(&x)
    }

    // u = H(PAD(A) | PAD(B)) per RFC 5054.
    fn compute_u(&self) -> BigUint {
        let u = (self.group.hash)(&[
            &padded_bytes(&self.a_pub, self.group.bytes),
            &padded_bytes(&self.b_pub, self.group.bytes),
        ]);
        BigUint::from_bytes_be(&u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        hex::decode(cleaned).unwrap()
    }

    // HomeKit Accessory Protocol SRP-6a/SHA-512/3072-bit vector:
    // username "alice", password "password123".
    #[test]
    fn hap_test_vector() {
        const A: &str = "60975527035CF2AD1989806F0407210BC81EDC04E2762A56AFD529DDDA2D4393";
        const A_PUB: &str = "FAB6F5D2615D1E323512E7991CC37443F487DA604CA8C9230FCB04E541DCE628\
            0B27CA4680B0374F179DC3BDC7553FE62459798C701AD864A91390A28C93B644\
            ADBF9C00745B942B79F9012A21B9B78782319D83A1F8362866FBD6F46BFC0DDB\
            2E1AB6E4B45A9906B82E37F05D6F97F6A3EB6E182079759C4F6847837B62321A\
            C1B4FA68641FCB4BB98DD697A0C73641385F4BAB25B793584CC39FC8D48D4BD8\
            67A9A3C10F8EA12170268E34FE3BBE6FF89998D60DA2F3E4283CBEC1393D52AF\
            724A57230C604E9FBCE583D7613E6BFFD67596AD121A8707EEC4694495703368\
            6A155F644D5C5863B48F61BDBF19A53EAB6DAD0A186B8C152E5F5D8CAD4B0EF8\
            AA4EA5008834C3CD342E5E0F167AD04592CD8BD279639398EF9E114DFAAAB919\
            E14E850989224DDD98576D79385D2210902E9F9B1F2D86CFA47EE244635465F7\
            1058421A0184BE51DD10CC9D079E6F1604E7AA9B7CF7883C7D4CE12B06EBE160\
            81E23F27A231D18432D7D1BB55C28AE21FFCF005F57528D15A88881BB3BBB7FE";
        const B_PUB: &str = "40F57088A482D4C7733384FE0D301FDDCA9080AD7D4F6FDF09A01006C3CB6D56\
            2E41639AE8FA21DE3B5DBA7585B275589BDB279863C562807B2B99083CD1429C\
            DBE89E25BFBD7E3CAD3173B2E3C5A0B174DA6D5391E6A06E465F037A40062548\
            39A56BF76DA84B1C94E0AE208576156FE5C140A4BA4FFC9E38C3B07B88845FC6\
            F7DDDA93381FE0CA6084C4CD2D336E5451C464CCB6EC65E7D16E548A273E8262\
            84AF2559B6264274215960FFF47BDD63D3AFF064D6137AF769661C9D4FEE4738\
            2603C88EAA0980581D07758461B777E4356DDA5835198B51FEEA308D70F75450\
            B71675C08C7D8302FD7539DD1FF2A11CB4258AA70D234436AA42B6A0615F3F91\
            5D55CC3B966B2716B36E4D1A06CE5E5D2EA3BEE5A1270E8751DA45B60B997B0F\
            FDB0F9962FEE4F03BEE780BA0A845B1D9271421783AE6601A61EA2E342E4F2E8\
            BC935A409EAD19F221BD1B74E2964DD19FC845F60EFC09338B60B6B256D8CAC8\
            89CCA306CC370A0B18C8B886E95DA0AF5235FEF4393020D2B7F3056904759042";
        const SALT: &str = "BEB25379D1A8581EB5A727673A2441EE";
        const U: &str = "03AE5F3C3FA9EFF1A50D7DBB8D2F60A1EA66EA712D50AE976EE34641A1CD0E51\
            C4683DA383E8595D6CB56A15D5FBC7543E07FBDDD316217E01A391A18EF06DFF";
        const K: &str = "5CBC219DB052138EE1148C71CD4498963D682549CE91CA24F098468F06015BEB\
            6AF245C2093F98C3651BCA83AB8CAB2B580BBF02184FEFDF26142F73DF95AC50";

        let mut c = SrpClient::new("alice", "password123");
        c.a = BigUint::from_bytes_be(&unhex(A));
        c.a_pub = c.g.modpow(&c.a, &c.n);
        assert_eq!(padded_bytes(&c.a_pub, GROUP_BYTES), unhex(A_PUB), "A mismatch");

        let b = BigUint::from_bytes_be(&unhex(B_PUB));
        c.process_b(&unhex(SALT), b).unwrap();

        assert_eq!(c.compute_u().to_bytes_be(), unhex(U), "u mismatch");
        assert_eq!(c.session_key(), unhex(K).as_slice(), "session key K mismatch");
    }

    #[test]
    fn rfc5054_sha1_vector_with_apple_session_key() {
        const GROUP: SrpGroup = SrpGroup {
            n_hex: "EEAF0AB9ADB38DD69C33F80AFA8FC5E86072618775FF3C0B9EA2314C9C256576D674DF7496EA81D3383B4813D692C6E0E0D5D8E250B98BE48E495C1D6089DAD15DC7D7B46154D6B6CE8EF4AD69B15D4982559B297BCF1885C529F566660E57EC68EDBC3C05726CC02FD4CBF4976EAA9AFD5138FE8376435B9FC61D2FC0EB06E3",
            g: 2,
            bytes: 128,
            hash: h1,
            apple_variant: true,
        };
        const A_PUB: &str = "61D5E490F6F1B79547B0704C436F523DD0E560F0C64115BB72557EC44352E8903211C04692272D8B2D1A5358A2CF1B6E\
            0BFCF99F921530EC8E39356179EAE45E42BA92AEACED825171E1E8B9AF6D9C03E1327F44BE087EF06530E69F66615261EEF54073\
            CA11CF5858F0EDFDFE15EFEAB349EF5D76988A3672FAC47B0769447B";
        const B_PUB: &str = "BD0C61512C692C0CB6D041FA01BB152D4916A1E77AF46AE105393011BAF38964DC46A0670DD125B95A981652236F99D9\
            B681CBF87837EC996C6DA04453728610D0C6DDB58B318885D7D82C7F8DEB75CE7BD4FBAA37089E6F9C6059F388838E7A00030B33\
            1EB76840910440B1B27AAEAEEB4012B7D7665238A8E3FB004B117B58";
        const S: &str = "B0DC82BABCF30674AE450C0287745E7990A3381F63B387AAF271A10D233861E359B48220F7C4693C9AE12B0A6F67809F\
            0876E2D013800D6C41BB59B6D5979B5C00A172B4A2A5903A0BDCAF8A709585EB2AFAFA8F3499B200210DCC1F10EB33943CD67FC8\
            8A2F39A4BE5BEC4EC0A3212DC346D7E474B29EDE8A469FFECA686E5A";

        let mut c = SrpClient::with_group(&GROUP, "alice", "password123");
        assert_eq!(c.k.to_bytes_be(), unhex("7556AA045AEF2CDD07ABAF0F665C3E818913186F"), "k mismatch");
        c.a = BigUint::from_bytes_be(&unhex("60975527035CF2AD1989806F0407210BC81EDC04E2762A56AFD529DDDA2D4393"));
        c.a_pub = c.g.modpow(&c.a, &c.n);
        assert_eq!(c.a_pub.to_bytes_be(), unhex(A_PUB), "A mismatch");
        c.process_b(&unhex("BEB25379D1A8581EB5A727673A2441EE"), BigUint::from_bytes_be(&unhex(B_PUB))).unwrap();
        assert_eq!(c.compute_x().to_bytes_be(), unhex("94B7555AABE9127CC58CCF4993DB6CF84D16C124"), "x mismatch");
        assert_eq!(c.compute_u().to_bytes_be(), unhex("CE38B9593487DA98554ED47D70A7AE5F462EF019"), "u mismatch");
        let s = unhex(S);
        assert_eq!(c.session_key(), [h1(&[&s, &[0, 0, 0, 0]]), h1(&[&s, &[0, 0, 0, 1]])].concat().as_slice());
    }
}
