# Release packaging

Tracking issue: PAS-1337. The supported deployment contract is in
`ACCEPTANCE.md`.

## Build

```sh
apps/pi4-host/scripts/build-release.sh
```

The script runs on an aarch64 Linux host, through Docker with the
digest-pinned `rust:1.98.1-trixie` builder, or as a host cross build when
`CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu` and a linker for that target
are set. It needs Bun 1.4.4 and Python 3 on the host. Inputs are locked by
`Cargo.lock` and `apps/web/bun.lock`.

`dist/` then contains:

| File | Content |
| -- | -- |
| `vinyl-bridge-pi` | The only application executable. The React build from `apps/web/dist` and the notices are embedded. |
| `vinyl-bridge-pi.manifest.json` | Every linked crate and bundled web package with version, license and source |
| `THIRD-PARTY-NOTICES.txt` | License texts, also printed by `vinyl-bridge-pi licenses` and served at `/licenses` |
| `vinyl-bridge-pi.elf.txt` | Interpreter, needed libraries and highest glibc symbol version |
| `raspair-<version>-source.tar.gz` | `git archive` of the built commit |
| `BUILDINFO`, `SHA256SUMS` | Commit, builder and build command; SHA-256 of every file above |

`RASPAIR_RELEASE=1` makes a missing web build or notices file a build error.
Other builds embed the web build if present and `LICENSES.dev.txt`.
`notices.py` rejects packages without a license field or license file and
any GPL or AGPL license.

## ELF allowlist

`scripts/check-elf.sh BINARY [ROOTFS]` fails unless the binary is AArch64,
uses `/lib/ld-linux-aarch64.so.1`, needs only libraries from this list, and
needs no glibc symbol newer than 2.41. With `ROOTFS` (or `PIOS_ROOTFS` in
`build-release.sh`), each needed library must also exist in that extracted
image.

| Library | Stock package on the image |
| -- | -- |
| `libc.so.6`, `libm.so.6`, `ld-linux-aarch64.so.1` | `libc6` 2.41-12+rpt1+deb13u4 |
| `libgcc_s.so.1` | `libgcc-s1` 14.2.0-19 |
| `libasound.so.2` | `libasound2t64` 1.2.14-1+rpt1+deb13u1 |
| `libssl.so.3`, `libcrypto.so.3` | `libssl3t64` 3.5.7-1~deb13u3+rpt1 |

The supported floor is Raspberry Pi OS Lite 64-bit Trixie. No older
release is claimed: the binary uses `GLIBC_2.39` symbols, which Bookworm
(glibc 2.36) lacks.

## Subprocess audit

| Helper | Daemon call path | Status |
| -- | -- | -- |
| `fpcalc` | `music_id::run_fpcalc`, only from manual or post-recording identification with music ID enabled and an AcoustID key | Optional. When absent the file is marked `untagged` with a note and can be tagged manually. In-process fingerprinting is later work. |
| `ffmpeg` | `airplay_core::audio::decode::decode_audio_file` | Not reachable from `vinyl-bridge-pi`; used by the airplay CLI file player |
| `flac`, `lame` | None | Recording writes WAV in process. FLAC and MP3 encoding are later work. |

No native codec library is linked. The only codec crates are pure-Rust
Symphonia 0.6.1 (MPL-2.0) through `airplay-core`. No mp3lame bindings are
included.

## Evidence, 2026-11-23

Image: `2026-10-06-raspios-trixie-arm64-lite.img.xz`, the current
`raspios_lite_arm64` release.

| Item | Value |
| -- | -- |
| `.img.xz` sha256 (matches the published `.sha256`) | `483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843` |
| `.img` sha256 | `b8a393dc1c3701450b5fffbfe74368a98fb6eab7415ce456cc0cead886ea8780` |
| `/etc/os-release` | Debian GNU/Linux 13 (trixie) |
| Kernel for Pi 4 | `6.18.50+rpt-rpi-v8` (from `/usr/lib/modules`; not booted) |
| Build | Host cross build, rustc 1.98.1, linked against the image's own glibc 2.41 |
| ELF needed | `libssl.so.3 libcrypto.so.3 libasound.so.2 libgcc_s.so.1 libm.so.6 libc.so.6`; highest symbol `GLIBC_2.39`; all present in the image root |

Container-only smoke test, not a clean-image boot: the root partition was
extracted from the image, only `vinyl-bridge-pi` was copied into it, and the
executable ran under `qemu-aarch64 -L <rootfs>` in a network namespace with
only loopback, with `PATH=/nonexistent` so no helper executable could run.

- `/` returned the React index; its `./assets/*.js` and `./assets/*.css`
  and the nested `assets/manrope-*.woff2` fonts were served with correct
  types. `/licenses`, `/ui` and `/api/status` returned 200.
- Recording start and stop wrote a WAV file. Identification of that file
  completed with status `untagged` and an `fpcalc unavailable` note.

Booting the image on a Pi 4, React navigation in a browser on the device,
and a release job that rebuilds from the source archive after FLAC,
fingerprint and MP3 work lands are not covered here.
