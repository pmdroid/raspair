# Reverse-Engineering the AirPlay 2 FairPlay Sender Handshake

A working guide to getting audio **through** an Apple-TV-led entertainment group,
by solving the one remaining blocker: the FairPlay `/fp-setup` **sender**
attestation. This is interop RE against **your own** devices on **your own**
network.

Read `ENTERTAINMENT_GROUPS.md` first for how we got here. Short version: we built
the whole group path (SETUP, RECORD, PTP anchor — all accepted), eliminated
transport, and isolated FairPlay `/fp-setup` as the sole gate. Replaying captured
blobs fails because the Apple TV issues a **session-random** challenge and
**validates** the response (HTTP 403).

---

## 0. The exact problem

The handshake is two POSTs over the encrypted RTSP channel, in this order:

```
POST /fp-setup   body=setup1 (16 B)   →  reply   (142 B)   ← random, per session
POST /fp-setup   body=setup2 (164 B)  →  response (32 B)    ← fp_header + last 20 B of setup2
```

We need the function the Apple sender computes:

```
f(reply[142]) -> setup2[164]      such that the Apple TV returns 200, not 403
```

`f` lives inside Apple's client. Public projects (pyatv, RPiPlay, java-airplay-lib,
EstebanKubata libplayfair) only implement the **receiver** side, and they cheat:
they answer setup1 with one of **four fixed canned tables** and **echo** setup2
without validating it. So none of them contain `f` — they never need it.

There are two ways to obtain `f`:
- **Borrow it** at runtime from a real Apple device (Path A) — fastest to audio.
- **Reproduce it** by reverse-engineering (Paths B/C) — the "real" fix.

---

## 1. Reference data (captured from your Mac → "Entertainment Room" ATV via atvproxy)

One known-good triple from a real session (the Mac's `setup2` was accepted by the
ATV *in that session*, because it matched *that* session's reply):

```
setup1 (16B):  46504c590301010000000004020003bb
reply  (142B): 46504c59030102000000008202037023b18e03e9edeabed3267d35f51c19d7
               1334ff361e2ac6c5a7a44c523f144608ddf3ce08eedc2a711c1f44f781af0f
               c10500cad1384b363699c6968c07638e148f009ed3d1b2f80943e213e5712b
               3605e18a2a6149869e6ecf5f13e41375ae899892469166b651b6bc1ebd45a1
               cb85569cb3fd046f9df7b84aeb9b09c72936
setup2 (164B): 46504c590301030000000098038f1a9c9a53ee1b2b872454b4f542fdd7f9e5
               1244f27e8cf9383d1c326ebfd06eab0f58a023202c7074ca8e4d3bdea4a649
               1d35f37c66a9fc4f352360178e35dfe66c09523becd759d2533dffc83b78f9
               320a6860031218ee99a25d2581da6aaa3326f559197f42d0c035d32974099f
               292a773004c4ce15cb7a31e56dd95ad915ebd94a9347480bd299c62397b86e
               bf43fd033863827867
```

Header layout:
- `46 50 4c 59` = "FPLY"; byte[4]=`03` version; byte[5]=type; byte[6]=seq.
- setup1: byte[14]=`03` = **mode** (selects which of 4 reply families).
- reply: byte[13]=`03` echoes the mode; bytes[14..142] = **128-byte body** (the
  random challenge — this is what differs every run).
- setup2: bytes[12..164] = **152-byte body** (the response `f` produces).
- response(32B) = `46504c5903010400000000 14` + setup2[144..164] (just an echo).

These blobs are also in `core/src/rtsp/setup.rs` as `FP_SETUP1` / `FP_SETUP2`.

---

## 2. Tooling setup (macOS)

```sh
# capture / inspect
brew install wireshark            # tshark CLI + GUI

# dynamic instrumentation
pip install frida-tools           # frida, frida-trace
# (you already have pyatv in ~/.venv for atvproxy)

# static RE — pick one
brew install --cask ghidra        # free; loads dyld_shared_cache
brew install blacktop/tap/ipsw    # extract dylibs from the shared cache
# or Hopper / IDA if you have them

# analysis
python3 -m pip install pycryptodome numpy
```

mDNS/Bonjour note: the receiver-mock paths need your fake device to advertise on
the LAN; macOS firewall must allow it.

---

## Path A — Borrow Apple's FairPlay (most likely to actually play audio)

**Evidence it works:** when you AirPlayed Music.app → Entertainment Room *through*
`atvproxy`, the ATV accepted fp-setup and reached RECORD. A real Apple sender,
relayed, produces a valid `setup2`. So use the Mac as a **FairPlay co-processor**.

### A.1 Prove the relay end-to-end (do this first — half a day)

Goal: get the ATV to render audio while `atvproxy` relays a *real* Apple sender's
FairPlay. The blocker before was pyatv "can't play media" — but you don't need
pyatv to play; you need the Mac to play *through* the proxy.

1. Run atvproxy between your Mac and the ATV (as you did for the capture):
   ```sh
   source ~/.venv/bin/activate
   atvproxy <credentials> <apple_tv_ip>        # see pyatv atvproxy docs
   ```
2. From the Mac, AirPlay real audio (Music.app) to the **proxy's** advertised
   device, not the ATV directly.
3. Watch the proxy log: if RECORD succeeds and audio bytes flow, the proxy is
   relaying a valid FairPlay session. If you hear audio, **the relay model works**
   and A.2 is "just engineering."

If the proxy drops media after RECORD, patch atvproxy to also relay the audio /
event / timing TCP+UDP legs (it currently only proxies control). That patch alone
might give you working audio-through-ATV today.

### A.2 Split the FairPlay leg into our sender

Architecture: our Rust `--group` sender does everything *except* `f`, and asks a
local **FairPlay-oracle helper** to compute `setup2` from the ATV's live `reply`.

```
our sender ──setup1──▶ Apple TV
our sender ◀──reply─── Apple TV
our sender ──reply──▶ oracle helper ──(makes the Mac compute it)──▶ setup2
our sender ──setup2──▶ Apple TV  →  200 ✅
```

How the oracle helper makes the Mac compute `f(reply)`:
- It advertises a **fake AirPlay receiver** (Bonjour). When the Mac's AirPlay
  stack connects and sends its own `setup1`, the helper answers with **the ATV's
  reply** (matching mode), and captures the `setup2` the Mac returns. That setup2
  is `f(reply)`.
- Easiest substrate: fork pyatv's `protocols/airplay/server_auth.py`
  (`PlayFair`) into a standalone receiver; override `fairplay_setup` to return the
  injected reply and to log the inbound setup2 (see Path B.1 — same hook).
- Trigger: you (or a script) start an AirPlay session from the Mac to the fake
  device at stream time. The Mac must pick the same mode/format; pin it.

Caveats: needs an Apple device on the LAN at stream time; this is "interop via a
real client in the loop," not a standalone library. But it's the **only** path
with proof it yields an ATV-accepted setup2.

---

## Path B — Crack `f` by black-box differential analysis

Make a receiver you control, feed the Mac chosen replies, and watch how `setup2`
changes. This tells you the algorithm's *shape* fast.

### B.1 Build the probe (a controllable receiver)

Fork pyatv's receiver mock so you choose the reply and log the response. In
`~/.venv/lib/python3.12/site-packages/pyatv/protocols/airplay/server_auth.py`,
the `PlayFair` class is the hook:

```python
class PlayFair:
    INJECT_REPLY = None   # set to 142 bytes to override

    def fairplay_setup(self, request):
        if request[4] != 3:
            return -1
        msg_type = request[self.TYPE_POSITION]
        seq = request[self.SEQ_POSITION]
        if msg_type == self.SETUP_MESSAGE_TYPE:
            if seq == self.SETUP1_MESSAGE_SEQ:
                mode = request[self.MODE_POSITON]
                # ← inject a chosen reply instead of reply_message[mode]
                reply = self.INJECT_REPLY or self.reply_message[mode]
                return reply
            elif seq == self.SETUP2_MESSAGE_SEQ:
                # ← LOG the Mac's setup2 = f(reply) before echoing
                with open("/tmp/fp_pairs.log", "ab") as fh:
                    fh.write(b"SETUP2 " + request.hex().encode() + b"\n")
                response = self.fp_header + request[len(request)-20:]
                return response
        return None
```

You need this `PlayFair` to actually receive a session from the Mac. Run it via
`atvproxy` (which already stands up `server_auth` as the fake device) but have the
fp-setup handler inject `INJECT_REPLY` and log setup2. Pair the Mac to the proxy's
device once, then AirPlay to it repeatedly.

Also log the **reply you injected** alongside each setup2 so you have `(reply,
setup2)` pairs in `/tmp/fp_pairs.log`.

### B.2 Differential probing (the actual cryptanalysis)

Drive many sessions with crafted replies:

- `reply = 00*128` (after the 14-byte header) → record `setup2₀`.
- For each byte position `i` in the 128-byte body: `reply = 00.. with body[i]=0x01`
  → record `setup2ᵢ`. Diff against `setup2₀`.

Interpret the diffs:

| Observation | Implies |
|---|---|
| flipping reply byte *i* changes a **localized 16-byte** region of setup2 | **AES-ECB** (or a block permutation); 16-B block structure |
| change **avalanches** to all later bytes | **CBC** chaining |
| setup2 body is **identical** regardless of reply | reply isn't an input (unlikely, given the 403) |
| some setup2 bytes are constant across all replies | fixed framing / sender nonce / padding |

Script skeleton:

```python
import itertools
pairs = [l.split() for l in open("/tmp/fp_pairs.log")]  # adapt to your format
base = bytes.fromhex(pairs[0][1])
for tag, hexs in pairs[1:]:
    s2 = bytes.fromhex(hexs)
    diff = [j for j in range(len(s2)) if s2[j] != base[j]]
    # collapse to 16-byte blocks to see ECB vs CBC
    blocks = sorted({j // 16 for j in diff})
    print(tag, "changed blocks:", blocks)
```

### B.3 Test the "it's just AES with the public key" hypothesis

The receiver-side **libplayfair (EstebanKubata)** ships the *extracted hardcoded
FairPlay AES key* (it must, to decrypt the audio `ekey`). Find it in that repo's
`lib/playfair/` (look for the 16-byte `aeskey` / `default key` used by the
`Decrypt`/`hand` routines). Then, with the captured pair from §1:

```python
from Crypto.Cipher import AES
key   = bytes.fromhex("<libplayfair 16-byte key>")
reply = bytes.fromhex("...")[14:]    # 128-byte body
s2    = bytes.fromhex("...")[12:]    # 152-byte body
for mode, c in [("ECB", AES.new(key, AES.MODE_ECB))]:
    # try: does decrypting setup2 blocks reveal reply-derived plaintext?
    pt = c.decrypt(s2[:len(s2)//16*16])
    print(mode, pt.hex())
    # look for reply bytes, counters, or structure in pt
```

If reply-derived plaintext falls out → `f` is "AES(known key) over reply + a
sender nonce," and we reimplement it in Rust in an afternoon. If it's noise →
it's the obfuscated FairPlay VM; go to Path C.

---

## Path C — Static / dynamic RE of the macOS sender (ground truth)

Lift `f` straight out of Apple's code.

### C.1 Find the binary

The sender FairPlay is in a private framework / daemon, inside the **dyld shared
cache** (not standalone files anymore):

```sh
# list candidates
ls /System/Library/PrivateFrameworks | grep -iE "airplay|mediaremote|coremedia"
# extract dylibs from the shared cache
ipsw dyld extract /System/Library/dyld/dyld_shared_cache_arm64e AirPlaySupport
# good targets: AirPlaySupport.framework, airplayd, AirPlayXPCHelper,
#               libAirPlay*.dylib, "MediaToolbox"/SAP routines
```

### C.2 Locate `f` by signature

In Ghidra/Hopper, search bytes/strings:
- magic **`46 50 4c 59`** ("FPLY")
- lengths **`0x8e`=142**, **`0xa4`=164**, **`0x10`=16**
- strings `fp-setup`, `FairPlay`, `SAP`, `MFiSAP`, `fpHeader`
The function that reads a 142-byte buffer and writes a 164-byte buffer is `f`.

### C.3 Confirm/dump at runtime with Frida

Hook the candidate and watch real inputs/outputs while you AirPlay:

```js
// frida -n <airplay-process> -l fp.js
const f = Module.getExportByName(null, "<symbol or resolved addr>");
Interceptor.attach(f, {
  onEnter(args) { this.reply = args[0]; console.log("reply", hexdump(this.reply, {length:142})); },
  onLeave(ret)  { console.log("setup2", hexdump(this.out, {length:164})); }
});
```

Find the process first: AirPlay from Music.app, then `frida-ps | grep -iE
"airplay|coreaudio|mediaremote"`; attach and `frida-trace -i '*fairplay*' -i
'*fp_setup*'` to surface the function name. Once you have the function body
(de-obfuscated or as a Frida-callable), you can either reimplement it or call it
as an oracle (a cleaner version of Path A).

---

## Decision tree

```
Run Path B.2 differential probe
├─ setup2 changes in clean 16-B blocks, and B.3 AES test reveals structure
│     → reimplement f in Rust (core/src/pairing/fairplay.rs). DONE, portable.
├─ setup2 avalanches / B.3 is noise (FairPlay VM)
│     → Path C to lift the routine, OR fall back to Path A (oracle).
└─ want audio NOW, portability later
      → Path A.1 (prove relay) then A.2 (oracle bridge).
```

## Where to wire results into this repo

- A new `core/src/pairing/fairplay.rs` implementing `fn fp_setup2(reply: &[u8]) ->
  Vec<u8>` (cracked `f`) or an oracle client.
- Call it from `client.rs::fairplay_setup` between setup1 and setup2 (replace the
  current `FP_SETUP2` replay).
- Keep `FP_SETUP1`/`FP_SETUP2` as fixtures for tests.

## Reality check

The classic FairPlay used an obfuscated bytecode VM, so Path B/C may be a
weeks-to-months effort and could be defeated by future Apple changes. Path A
sidesteps the crypto entirely and is the realistic route to *hearing audio*
through the ATV soon. Start with **A.1** (proves the model in an afternoon) and
run **B.2/B.3** in parallel (tells you whether a clean reimplementation is even
possible).
