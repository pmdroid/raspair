# Audio sender pairing and authentication matrix

Tracker: PAS-1405. Source baseline: `main` at 5521b57, checked 2026-11-10.

This matrix covers audio sender authentication only. Video, screen mirroring,
Apple accounts, Companion and MRP remote-control pairing are out of scope.
Entertainment and home-theater group audio is separate from authenticating the
group leader; see `apps/airplay-rs/docs/ENTERTAINMENT_GROUPS.md`.

Each row has three separate results: **pair** (the handshake completes),
**reconnect** (saved trust is verified on a new connection) and **playback**
(a receiver renders audio). A synthetic transcript can prove at most pair and
reconnect. Playback needs a dated hardware result with receiver model,
firmware, feature flags, method and effective format. A row without a fixture
or with an unexplained handshake stays open.

## Fixtures

Fixtures live in `src/pairing/fixtures.rs`. A synthetic receiver implements
`PairTransport` with fixed synthetic seeds, salt, PIN and identifiers. It
contains no user PIN, password, live private key, reusable session key or
personal device identifier. Controller keys come from `Identity::generate`, so
fixtures pin structure and validation, not bytes.

| Fixture | Proves |
| -- | -- |
| `homepod_transient_transcript_proves_pair_success_only` | HKP 4: `/pair-setup` M1 (method 0, flags 0x10), M3 (state, A, proof); shared secret equals the receiver SRP key; no identity saved; no playback request |
| `transient_pairing_rejects_wrong_receiver_pin` | M4 error 0x02 surfaces and no secret is kept |
| `persistent_pin_transcript_separates_pair_success_and_verified_reconnect` | HKP 3: `/pair-pin-start`, M1, M3, M5; receiver checks the controller M5 signature; receiver LTPK and saved identity match; then `/pair-verify` 1 and 3 succeed as a verified reconnect |
| `persistent_pairing_rejects_wrong_pin_before_identity_exchange` | Wrong PIN stops at M4 and no controller key reaches the receiver |
| `reconnect_rejects_impostor_when_receiver_ltpk_is_known` | Pair-verify M2 signed by another key fails when the receiver LTPK is known |
| `reconnect_without_receiver_ltpk_is_not_counted_as_verified` | With no stored receiver LTPK, an impostor completes pair-verify; this counts as unverified |
| `reconnect_fails_when_receiver_does_not_know_controller` | Receiver M4 error 0x02 surfaces for an unknown controller |

## Rows

| Row | Owner | Pair | Reconnect | Playback | Evidence |
| -- | -- | -- | -- | -- | -- |
| Unauthenticated receivers | PAS-1411 | open | n/a | open | gap |
| RTSP password / digest | PAS-1411 | open | n/a | open | gap |
| Legacy PIN / SRP (`/pair-setup-pin`) | PAS-1410 | open | open | open | gap |
| HAP persistent, on-screen PIN | PAS-1409 | fixture | fixture | open | fixture |
| HAP persistent, static PIN | PAS-1409 | open | open | open | gap |
| HAP pair-verify after persistent pairing | PAS-1407 | n/a | fixture, unverified in product | open | fixture + gap |
| HAP transient, default PIN 3939 | PAS-1408 | fixture | n/a | open | fixture |
| HAP transient, password | PAS-1408 | open | n/a | open | gap |
| Unified pair setup with receiver authentication | PAS-1412 | open | open | open | gap |
| MFi-SAP `/auth-setup` | PAS-1435 | open | n/a | open | gap |
| FairPlay `/fp-setup` | PAS-1413 | open | n/a | open | gap |
| FairPlay audio keys | PAS-1414 | open | n/a | open | gap |
| Legacy RSA/AES RAOP transport | PAS-1436 | n/a | n/a | open | gap |

## Requirements and gaps per row

**Unauthenticated receivers (PAS-1411).** No pairing request is sent. The
sender must still decide from TXT records and `/info` that no method is
required (PAS-1404). Gap: no transcript; RAOP OPTIONS/ANNOUNCE and AirPlay 2
`/info` ordering is not captured. References: [airplay-spec](https://openairplay.github.io/airplay-spec/),
[pyatv protocols](https://pyatv.dev/documentation/protocols/).

**RTSP password / digest (PAS-1411).** Receiver answers 401 with
`WWW-Authenticate: Digest realm=..., nonce=...`. Sender retries the same request
with `Authorization: Digest username, realm, nonce, uri, response`, where
`response = MD5(MD5(user:realm:password):nonce:MD5(method:uri))` per RFC 2617.
Gap: accepted username, hex case and which requests need the header must be
taken from a capture before implementation. Reference: [RFC 2617](https://www.rfc-editor.org/rfc/rfc2617).

**Legacy PIN / SRP (PAS-1410).** Order: `/pair-pin-start`, then two
`/pair-setup-pin` binary-plist requests (`method=pin`, `user` → `pk`, `salt`;
`pk`, `proof` → `proof`), then an AES-GCM `epk`/`authTag` exchange carrying the
Ed25519 key. SRP is 2048-bit with SHA-1, unlike the HAP 3072-bit SHA-512 group
in `srp.rs`. Reconnect is `/pair-verify` with a 4-byte header, X25519 and
Ed25519 keys and an AES-CTR encrypted signature. Gap: no fixture and no
published vector for the 2048/SHA-1 group; the credential fields to store are
controller seed, controller id and receiver Ed25519 key.

**HAP persistent, on-screen PIN (PAS-1409).** Covered by fixture. Credential
fields: controller Ed25519 identity, controller pairing id derived from its
LTPK, receiver pairing id and receiver LTPK. `PairingSession` saves only the
controller identity; the receiver id and LTPK are not persisted (PAS-1406).

**HAP persistent, static PIN (PAS-1409).** Same M1–M6 exchange. Gap: whether
`/pair-pin-start` is sent or skipped for receivers with a configured PIN is not
established.

**HAP pair-verify (PAS-1407).** Covered by fixture. `pair_verify` checks the
receiver signature only when `server_ltpk` is 32 bytes. `Client::connect`
never loads a receiver LTPK, so product reconnects are unverified today.

**HAP transient, default (PAS-1408).** Covered by fixture as the HomePod
regression baseline. Transient pairing ends at M4; the SRP session key is the
channel secret.

**HAP transient, password (PAS-1408).** Same as default transient with the
user password as SRP password. Gap: `pair_homepod` fixes the PIN to 3939, and
the receiver signal that selects password entry is not settled (PAS-1404).

**Unified pair setup with receiver authentication (PAS-1412).** Gap: the HKP
header value, M1 flags and the receiver authentication fields are not
established from a primary source or capture. Not ready for delegation.
Reference: [HKP pairing](https://openairplay.github.io/airplay-spec/pairing/hkp.html).

**MFi-SAP `/auth-setup` (PAS-1435).** Sender posts one byte `0x01` followed by
a 32-byte X25519 public key. The receiver returns its X25519 key and an
encrypted MFi certificate and signature. Gap: certificate and signature
validation rules have no public vectors, and no receiver requiring it has been
captured.

**FairPlay `/fp-setup` (PAS-1413).** `Client::fairplay_setup` sends the
16-byte `FPLY` setup1, expects a 142-byte reply, sends a 164-byte setup2 and
reads a 32-byte reply. Setup2 today comes from an external helper or oracle.
Gap: no in-process sender implementation with MIT-compatible licensing and
verified vectors has been identified. Not ready for delegation until a
licensing decision and source are settled; any port larger than one module is
split first.

**FairPlay audio keys (PAS-1414).** `fairplay_key_for_stream` derives the
audio key from setup2, `ekey`/`eiv` and the pair-verify secret through the
same external helper. Gap: same as PAS-1413.

**Legacy RSA/AES RAOP transport (PAS-1436).** ANNOUNCE SDP carries
`a=rsaaeskey:` (16-byte AES key, RSA-OAEP with the published AirPort Express
public key, base64) and `a=aesiv:`. Audio packets use AES-128-CBC with the IV
reset per packet and the trailing partial block left in clear. Gap: no code or
fixture yet.
