#!/usr/bin/env bash
set -euo pipefail
BIN="${1:?usage: check-elf.sh BINARY [ROOTFS]}"
ROOTFS="${2:-}"
GLIBC_FLOOR="${GLIBC_FLOOR:-2.41}"
INTERP=/lib/ld-linux-aarch64.so.1
ALLOW=(ld-linux-aarch64.so.1 libc.so.6 libm.so.6 libgcc_s.so.1 libasound.so.2 libssl.so.3 libcrypto.so.3)
fail=0
err() { echo "error: $*" >&2; fail=1; }

readelf -h "$BIN" | grep -q 'Machine:.*AArch64' || err "$BIN is not an AArch64 ELF"
interp="$(readelf -l "$BIN" | sed -n 's/.*interpreter: \(.*\)\]/\1/p')"
[[ "$interp" == "$INTERP" ]] || err "interpreter $interp, expected $INTERP"
mapfile -t needed < <(readelf -d "$BIN" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
for lib in "${needed[@]}"; do
  [[ " ${ALLOW[*]} " == *" $lib "* ]] || err "$lib is not in the stock-OS allowlist"
  if [[ -n "$ROOTFS" && ! -e "$ROOTFS/usr/lib/aarch64-linux-gnu/$lib" ]]; then
    err "$lib missing from $ROOTFS"
  fi
done
glibc="$(readelf -V --wide "$BIN" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"
[[ "$(printf '%s\n%s\n' "$glibc" "$GLIBC_FLOOR" | sort -V | tail -1)" == "$GLIBC_FLOOR" ]] \
  || err "requires GLIBC_$glibc, floor is $GLIBC_FLOOR"

echo "interpreter: $interp"
echo "needed: ${needed[*]}"
echo "max glibc symbol: GLIBC_$glibc (floor $GLIBC_FLOOR)"
exit "$fail"
