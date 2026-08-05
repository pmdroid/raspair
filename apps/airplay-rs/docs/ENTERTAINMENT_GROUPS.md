# AirPlay 2 Entertainment / Home-Theater Groups — Reverse-Engineering Notes

**Status:** key mechanism captured & decrypted (2026-06-28). Group session
SETUP **implemented** behind `airplay play --group` (the leader-targeted,
unicast-to-leader path). FairPlay M2 → M3 now handled by a Doubletake
subprocess oracle. Audio transport still being validated against a real receiver.

### How to try it

```sh
# 1. Build or obtain the Doubletake fp-m3 helper (see "Doubletake fp-m3
#    subprocess oracle" below). A pre-built binary may already be at bin/fp-m3.

# 2. Pair our controller with the Apple TV once. Run it; the TV shows a PIN;
#    re-run with that PIN:
airplay pair --device "Entertainment Room" --pin <PIN-shown-on-TV>

# 3. Stream into the group (sudo: PTP binds privileged ports 319/320).
#    Try the default first, then the single-variable matrix below:
sudo ./target/release/airplay play --device "Entertainment Room" \
      --file song.flac --group --debug \
      --fairplay-helper ./bin/fp-m3

# 4. If FairPlay clears (fp-setup2 → 200) but audio is silent, test the key
#    hypotheses without changing anything else:
AP2_GROUP_AUDIO_KEY=shk sudo ./target/release/airplay play \
      --device "Entertainment Room" --file song.flac --group --debug \
      --fairplay-helper ./bin/fp-m3

AP2_GROUP_EKEY=1 AP2_GROUP_AUDIO_KEY=shk sudo ./target/release/airplay play \
      --device "Entertainment Room" --file song.flac --group --debug \
      --fairplay-helper ./bin/fp-m3
```

The `--debug` log shows the `/fp-setup` handshake, the group SETUP request, and
the leader's response/status — that tells us whether FairPlay cleared (look for
`fp-setup2 → status=200`) and whether the leader admits us to the group.

This documents how Apple streams audio into an **entertainment group** (a.k.a.
home-theater group) — e.g. an Apple TV with a HomePod stereo pair set as its TV
speakers — and why no open-source AirPlay sender (pyatv, OwnTone,
shairport-as-sender, lmcgartland/airplay2-rs, **or this library**) can currently
do it.

## TL;DR

Apple distributes entertainment-group audio over **Source-Specific Multicast
(SSM) with a shared 32-byte group key**, *not* the per-speaker unicast fan-out
that every OSS sender (including ours) implements. The sender:

1. opens the encrypted control/session to the **group leader** (the Apple TV),
2. hands it an **SSM multicast group address + a `groupEncryptionKey`** in
   `SETUP` (`mc2ucDetectionSSMGroupInfo`),
3. streams the audio **once** to that multicast group, encrypted with the group
   key,
4. and every member (Apple TV + both HomePods) subscribes to the multicast group
   and decrypts with the same key, staying sample-locked via PTP.

Targeting the leader and emitting SSM multicast + group key is the missing
capability. Our `--no-stereo`/stereo fan-out path streams a separate unicast
session per speaker, which is *not* how a home-theater group works.

## How it was captured

`pyatv`'s `atvproxy` performs a credentialed MITM:

- It advertises a near-clone device (tweaked `deviceid`/`pk`) and the Mac
  re-pairs to it fresh (pair-setup with PIN), so the proxy holds the
  **sender-side** keys and can decrypt what the Mac sends.
- It separately pair-verifies to the **real** Apple TV using saved credentials
  (from `atvremote pair`), re-encrypts, and forwards.
- Two independently-terminated encrypted legs → full plaintext in the middle.
  This beats forward secrecy without extracting anyone's keys.

`pyatv` proxies only the **control channel** ("cannot play media"), so the
session tore down after `RECORD` — but that was enough to capture the SETUP that
describes the multicast group.

## Captured request order (decrypted)

```
GET /info                       → device + group capabilities
POST /pair-setup  (PIN)         → fresh pairing to the proxy
POST /pair-verify               → ECDH session keys
POST /fp-setup    (FPLY… blobs) → FairPlay SAP exchange (2 round trips)
SETUP   rtsp://…/<sid>          → ★ carries mc2ucDetectionSSMGroupInfo (multicast)
GET /info
RECORD  rtsp://…/<sid>          → 200 OK, Audio-Latency: 0
(event) POST /command updateInfo→ ATV pushes re-formed group state
TEARDOWN                        → proxy artifact (no media/multicast relay)
```

## The SETUP body (sender → leader) — the prize

Decoded binary plist (IDs are from the captured session):

```python
{
  'timingProtocol': 'PTP',
  'timingPeerInfo': { 'ID': '21D77A7B-…', 'ClockID': -87720157651140600,
                      'Addresses': ['192.168.8.141', 'fe80::…', 'fd30:…'],
                      'DeviceType': 0, 'SupportsClockPortMatchingOverride': True },
  'timingPeerList': [ …same… ],

  # ★ MULTICAST GROUP DESCRIPTOR ★
  'mc2ucDetectionSSMGroupInfo': {
     'groupIPv4Addr':     '232.109.226.176:8738',   # 232.x.x.x = IPv4 SSM
     'groupIPv6Addr':     '[ff33::c1c3:370d]:8738',  # ff3x:: = IPv6 SSM
     'sourceIPv4Addr':    '192.168.8.141',
     'sourceIPv6Addr':    'fd30:bb22:f547:4d81:485:b5d5:4aa8:2ae7',
     'groupEncryptionKey': b'\x89\xb6KJ}\xa5\xca\xa2\xed\xc6Ph\xe4f\x96\xaa'
                           b'9%\x1b\xa4\x90\x93O\x0e,\x96\x8dK\xef\xc8\x9d\xfb', # 32 bytes
  },

  'groupUUID': '2563374C-64B4-413E-A5BA-46A592E9522A',   # the sender session group
  'sessionUUID': 'BE8A65E5-68B4-4D4B-8347-17CA440C42DC',
  'sessionCorrelationUUID': '21D77A7B-5F9B-4A69-A7C6-697E53E6EB89',
  'hijackID': '1A238CE3-A81A-4087-84B0-ABE7ECEAB9A5',     # takes over the group
  'senderPerceivedClusterType': 2,
  'isMultiSelectAirPlay': True,
  'senderSupportsRelay': True,
  'groupContainsGroupLeader': False,
  'deviceID': '1E:11:6D:F1:DC:C1', 'macAddress': '26:86:DB:E2:B9:F1',
  'model': 'Mac17,2', 'osName': 'macOS', 'osVersion': '26.3.1',
  'name': "Pascal's MacBook Pro", 'sourceVersion': '935.7.1',
  'statsCollectionEnabled': False, 'updateSessionRequest': False,
}
```

`232.x.x.x` is the IANA **Source-Specific Multicast** block; `ff3x::` is IPv6
SSM. So this is genuinely multicast, and `mc2uc` = "multicast-to-unicast
detection" (receivers fall back to unicast if multicast routing fails on the
LAN).

## The SETUP response (leader → sender)

```python
{ 'eventPort': 64899,
  'timingPeerInfo': {
     'HTGroupUUID': '88D79821-4C02-5CD5-AEB2-425A90984BB3',   # Home-Theater group id
     'ClockPorts': {'21D77A7B-…': 32787},
     'ClockID': -8347797154270216184,
     'Addresses': ['192.168.8.245', 'fe80::…', …],
     'ID': 'BEC557CC-F403-4135-8DBB-2F8CA1AE5660',
     'DeviceType': 1, 'SupportsClockPortMatchingOverride': True } }
```

Control is established with the **leader** (`192.168.8.245`), which acknowledges
the **`HTGroupUUID`** (= the `psgid` from `/info`). Control = leader, audio =
multicast group.

## Dynamic group re-formation

`/info` before streaming advertised the ATV as leader of the home-theater group:
`igl=1 gcgl=1 gid=88D79821 psgsz=3`. During the session, the event-channel
`updateInfo` push showed it flip to:

```
igl=0  gcgl=0  gid=2563374C-…(sender session)  pgid=2563374C-…  pgcgl=0
psgid=88D79821-…(home-theater, now "parent speaker group")  psgsz=3  flags=0x38644
```

So the sender's `groupUUID` becomes the active group; the home-theater group
demotes to the parent speaker group; `hijackID` is what lets the sender take it
over.

## Group TXT / field reference

| Field | Meaning |
|---|---|
| `gid` | active group UUID |
| `igl` | is group leader (1/0) |
| `gcgl` | group contains (discoverable) leader |
| `pgid` / `pgcgl` | parent group id / parent contains leader |
| `psgid` / `psgsz` / `psgtp` | **p**arent **s**peaker **g**roup id / size / type |
| `gpn` | group (presentation) name, e.g. "Entertainment Room" |
| `HTGroupUUID` | Home-Theater group UUID (in SETUP response) |
| `tsid` | tight-sync id (stereo pair = one logical speaker) |

Captured group: **"Entertainment Room"**, `psgsz=3` (Apple TV + 2 HomePods),
home-theater group `88D79821-4C02-5CD5-AEB2-425A90984BB3`, leader Apple TV
`AppleTV14,1` at `192.168.8.245` (deviceID `26:44:D6:B8:21:EE`).

## Crypto layers (for context)

See `core/src/pairing/` and `core/src/stream/encrypt.rs`. Pairing = SRP-6a (PIN)
→ saved Ed25519 LTPKs → per-session ephemeral X25519 (forward secrecy). Control
& audio = ChaCha20-Poly1305. The entertainment-group audio is presumably the
same per-packet AEAD but keyed by `groupEncryptionKey` instead of a per-receiver
`shk` — **to be confirmed** from a multicast capture.

## Implementation plan (airplay-core)

1. **Detect the group / leader** from discovery (`igl`, `gcgl`, `psgid`,
   `gpn`). We already parse these in `protocol.rs`; surface a "target the
   leader" mode.
2. **SETUP to the leader** with a generated `mc2ucDetectionSSMGroupInfo`: pick
   an SSM group address (`232.x` / `ff3x::`) + a random 32-byte
   `groupEncryptionKey`, plus `timingPeerInfo` (reuse our PTP grandmaster) and a
   `hijackID`.
3. **RECORD**.
4. **Multicast audio socket**: open an IGMPv3/MLDv2 SSM sender socket and emit
   the audio frames to the group address, encrypted with `groupEncryptionKey`
   (start by assuming our existing buffered ChaCha20-Poly1305 framing, keyed by
   the group key).
5. **PTP** keeps all three members in sync — already implemented
   (`core/src/timing/ptp.rs`).

## Confirmed by packet capture (2026-06-28, on the sender)

A `tcpdump` on the AirPlaying Mac during a real (non-proxy) session captured the
audio leg directly:

```
192.168.8.141.55230 → 232.46.247.183:8738   UDP, 81-byte payload
dst MAC 01:00:5e:2e:f7:b7  (correct IPv4-multicast mapping → genuine L2 multicast)
```

- **Multicast is used (no `mc2uc` unicast fallback on this LAN).** Port `8738`
  matches the SETUP `groupIPv4Addr`; the group address (`232.46.247.183`) is
  **randomized per session** (≠ the earlier `232.109.226.176`).
- **Each audio packet is sent 3× (FEC).** Multicast has no retransmit, so the
  sender replicates each packet three times microseconds apart (identical
  bytes). Plan for this when implementing.
- **Payload is fully encrypted** with `groupEncryptionKey` — no cleartext RTP
  header (first byte `0xe1`, not `0x80`); trailing zero bytes = low nonce/packet
  counter at stream start (matches our `encrypt.rs` nonce-tail layout).

Implication: implementing the group sender = SETUP to the leader (captured) +
emit ChaCha20-Poly1305 audio (keyed by the group key, 3× replicated) to the SSM
multicast group, PTP-synced. We do **not** need per-member unicast on a LAN
where multicast works.

## Second capture: multicast PROBES, audio falls back to UNICAST TCP to the leader

A 13 s capture on the sender during sustained playback (961 packets) showed the
audio did **not** go over multicast on this Wi-Fi. Bytes per flow:

| Flow | Bytes / pkts | What |
|---|---|---|
| `Mac → AppleTV(.245):7000` TCP | **248 KB / 230** | **the audio** (~153 kbps ≈ AAC) |
| `AppleTV:320 ↔ Mac:320` UDP | 20 KB / 191 | PTP general |
| `:319 ↔ :319` UDP | 6 KB×2 / 84×2 | PTP event |
| `Mac.55230 → 232.46.247.183:8738` UDP | **0.98 KB / 9** | **multicast — probe only** |

The 9 multicast packets fire in a single **0.32 s burst at stream start**, then
silence for the rest — i.e. `mc2uc` ("multicast-to-unicast detection") probes
multicast, the LAN doesn't carry it, and the sender **falls back to unicast**:
buffered audio over TCP to the **leader** (Apple TV :7000), which distributes to
the group members.

**Implication (big):** on a typical home network we do **not** need SSM
multicast. The group audio transport is unicast buffered-over-TCP to the leader
— which `airplay-core` already implements (type-103 buffered path). The
home-theater group is, transport-wise, "a buffered receiver (the Apple TV) that
fans out to its HomePods."

### Revised implementation plan (unicast-to-leader)

1. **Target the leader** (`igl=1` Apple TV), not the HomePods.
2. **Add group fields to SETUP**: `mc2ucDetectionSSMGroupInfo` (group addr +
   32-byte `groupEncryptionKey`), `hijackID`, `timingPeerInfo`. Even if we never
   send multicast, the descriptor likely must be present for the leader to admit
   us as the group source.
3. **Stream buffered AAC unicast over TCP** to the leader (existing path),
   PTP-synced (existing grandmaster). Multicast leg = optional, only where the
   LAN supports it.
4. **Resolve FairPlay `/fp-setup`** (the `FPLY…` SAP exchange) — the main
   remaining gate to actual audio.

## FairPlay `/fp-setup` is the gate — and it's the real wall (confirmed 2026-06-28)

We implemented the two transport hypotheses and the FairPlay handshake; results,
in order, against the real "Entertainment Room" Apple TV leader:

| Attempt | Result |
|---|---|
| Buffered audio → side `dataPort`, keyed by `shk` | handshake accepted, **silent** |
| Buffered audio → side `dataPort`, keyed by `groupEncryptionKey` | accepted, **silent** |
| Buffered audio multiplexed over leader's main `:7000` (channel cipher) | **connection reset** (frame 440) — leader rejects audio on the control channel |
| `/fp-setup` replay of captured macOS blobs, then SETUP | **fp-setup1 → 200 (142 B reply); fp-setup2 → 403** |

**The blocker is FairPlay `/fp-setup`.** The Apple TV group leader:
- returns a **session-random** 142-byte reply to setup1 (the captured real reply
  `…82 02 03 70 23 b1 8e…` differs from the canned `reply_message[3]` tables in
  shairport/pyatv, and differs every run), and
- **validates** setup2 as a cryptographic derivation of *that* reply — replaying
  a setup2 from another session gets **403 Forbidden** (a real Apple device, unlike
  shairport/pyatv mocks which just echo the last 20 bytes, actually checks it).

That 403 (vs silently accepting) implies fp-setup is genuinely the rendering gate
for an Apple-TV-led entertainment group. HomePods don't require it (our buffered
path plays to them with no fp-setup), which is why no OSS sender needed it before.

## New: Doubletake `fp-m3` subprocess oracle (2026-06-28)

[`omarroth/doubletake`](https://github.com/omarroth/doubletake) contains a
sender-side FairPlay SAP implementation that computes the M3/setup2 response
from a fresh Apple TV M2. We verified the offline M3 test and built a tiny
subprocess helper so `airplay-core` can use it without importing GPL code.

Verified Doubletake commit:

```text
5f0c233f412d0b0e6c21d735d2afed95816a732b
```

### Build the helper

```bash
git clone https://github.com/omarroth/doubletake.git /tmp/doubletake
cd /tmp/doubletake
git checkout 5f0c233f412d0b0e6c21d735d2afed95816a732b

# Verify the offline M2 → M3 test passes:
go test ./internal/fpemu -run TestFPSAPExchangeM3 -v -count=1

# Create the helper (reads hex M2 from stdin, writes hex M3 to stdout):
cat > cmd/fp-m3/main.go <<'EOF'
package main

import (
    "encoding/hex"
    "fmt"
    "io"
    "os"
    "strings"

    "doubletake/internal/fpemu"
)

func main() {
    input, err := io.ReadAll(os.Stdin)
    if err != nil {
        fmt.Fprintln(os.Stderr, "read stdin:", err)
        os.Exit(1)
    }
    m2, err := hex.DecodeString(strings.TrimSpace(string(input)))
    if err != nil {
        fmt.Fprintln(os.Stderr, "decode M2 hex:", err)
        os.Exit(1)
    }
    m3, err := fpemu.FPSAPExchangeM3(m2)
    if err != nil {
        fmt.Fprintln(os.Stderr, "compute M3:", err)
        os.Exit(1)
    }
    fmt.Println(hex.EncodeToString(m3))
}
EOF

go build -o /path/to/airplay-rs/bin/fp-m3 ./cmd/fp-m3

# Optional: build the FairPlay audio-key helper for the AES-CBC path
# (only needed for AP2_GROUP_AUDIO_KEY=fairplay):
cat > cmd/fp-audio-key/main.go <<'EOF'
package main

import (
    "crypto/sha512"
    "encoding/hex"
    "fmt"
    "io"
    "os"
    "strings"

    "doubletake/internal/airplay"
)

func main() {
    input, err := io.ReadAll(os.Stdin)
    if err != nil {
        fmt.Fprintln(os.Stderr, "read stdin:", err)
        os.Exit(1)
    }
    parts := strings.Fields(string(input))
    if len(parts) != 3 {
        fmt.Fprintln(os.Stderr, "usage: echo '<m3-hex> <ekey-hex> <shared-secret-hex>' | fp-audio-key")
        os.Exit(1)
    }
    m3, _ := hex.DecodeString(parts[0])
    ekey, _ := hex.DecodeString(parts[1])
    secret, _ := hex.DecodeString(parts[2])
    fpAesKey := airplay.PlayfairDecryptExported(m3, ekey)
    h := sha512.New()
    h.Write(fpAesKey[:])
    h.Write(secret)
    fmt.Println(hex.EncodeToString(h.Sum(nil)[:16]))
}
EOF

go build -o /path/to/airplay-rs/bin/fp-audio-key ./cmd/fp-audio-key
```

The helpers are also available at `bin/fp-m3` and `bin/fp-audio-key` in this
workspace (built from the commit above on darwin/arm64). They are **not
committed** to the repo because they are GPL-3.0-derived binaries; build them
yourself before distributing anything.

### Use it from the CLI

```sh
airplay play --device "Entertainment Room" \
             --file song.flac --group --debug \
             --fairplay-helper ./bin/fp-m3
```

`airplay-core` will:

1. send `FP_SETUP1` to the Apple TV leader,
2. receive the fresh 142-byte M2 reply,
3. run `./bin/fp-m3` as a subprocess, feeding it the M2 hex on stdin,
4. validate that the returned M3 is 164 bytes and `FPLY`-framed,
5. send M3 as `/fp-setup` phase two,
6. proceed to the group SETUP / RECORD / buffered-audio path.

### Library API

```rust
use airplay_core::Client;

let mut client = Client::new();
client.set_fairplay_helper("./bin/fp-m3");
// ... connect, setup_group, play
```

### Fallback priority

`fairplay_setup` now tries, in order:

1. `--fairplay-helper` / `set_fairplay_helper` — Doubletake subprocess oracle.
2. `--oracle` / `set_oracle` — Python MITM oracle (`bin/fairplay_oracle.py`).
3. Captured `FP_SETUP2` replay — almost always returns `403` because the M2
   reply is session-random.

### Single-variable test knobs

```sh
# Key used for the leader-unicast buffered audio:
#   group    = groupEncryptionKey (default)
#   shk      = HKDF-derived shk from pair-verify shared secret
#   raw      = first 32 bytes of the pair-verify shared secret (Go/C reference)
#   fairplay = FairPlay-derived AES key (needs fp-audio-key helper + AES-CBC)
export AP2_GROUP_AUDIO_KEY=shk

# Include ekey/eiv/et=32 in the group SETUP (independent of the key above;
# forced on when key mode is fairplay):
export AP2_GROUP_EKEY=1
```

Try these combinations and record the result for each:

| Run | `AP2_GROUP_AUDIO_KEY` | `AP2_GROUP_EKEY` | Hypothesis |
|---|---|---|---|
| 1 | `group` | unset | Current default; group key for unicast fallback |
| 2 | `shk` | unset | Normal buffered shk for unicast fallback |
| 3 | `raw` | unset | Raw pair-verify shared secret (Go/C reference) |
| 4 | `group` | `1` | Group key + native-matching ekey/eiv present |
| 5 | `shk` | `1` | shk + native-matching ekey/eiv present |
| 6 | `fairplay` | n/a | FairPlay-derived AES key + AES-CBC framing |

Keep the same file, format, and volume for every run.

### Next validation step

The most important live result is now one log line:

```text
fp-setup2 → status=200 (32 bytes)
```

That is now working. The remaining problem is SETUP/media-key integration.

If **any** of the four runs above produces audio, we have identified the correct
key combination and can make it the default.

If all four are silent, the next required evidence is a fresh decrypted native
macOS group SETUP captured after FairPlay succeeds, plus the matching media
packets on the leader's negotiated `dataPort`. Compare these fields exactly:

```text
SETUP:  et, ekey, eiv, shk, streamConnectionID, streamConnections, type, ct,
        spf, sr, audioFormat, audioFormatIndex, controlPort, dataPort,
        latencyMin, latencyMax, redundantAudio, disableRetransmits
media:  first byte of each packet, payload size, cadence, key mode
```

If the native session uses AES-CBC on the unicast fallback (instead of our
ChaCha20-Poly1305 buffered framing), implementing that is the next gate.

## Open unknowns / next captures

- **Multicast audio framing**: exact packet layout on the SSM leg, and whether
  it's the type-103 buffered framing keyed by `groupEncryptionKey`.
  → `sudo tcpdump -i en0 -w mc.pcap host 232.109.226.176` during a **real**
  (non-proxy) session shows the multicast packets (payload encrypted, but
  size/cadence/format confirm framing).
- **Negotiated codec**: which `bufferStream` index the group selects
  (`supportedAudioFormatsExtended.bufferStream` lists the candidates).
- **FairPlay `/fp-setup`**: the `FPLY…` SAP exchange may gate full audio; need to
  determine whether the group path requires it or accepts our `shk`/group-key
  scheme.

## See also

- **`FAIRPLAY_RE_GUIDE.md`** — step-by-step guide to cracking/oracling the
  FairPlay `/fp-setup` sender attestation (the sole remaining gate).
- `protocol.rs` — TXT/group field parsing.
- Cross-session memory: `airplay2-entertainment-group-multicast`.
- pyatv `atvproxy` (control-channel MITM technique used to capture this).
