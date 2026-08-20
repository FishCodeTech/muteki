---
name: muteki-ctf-local-playbook
description: >
  Local black-box CTF playbook for forensics/crypto/misc file challenges on a
  full shell. Use when the challenge attaches a pcap, binary, disk image, or
  ciphertext blob — before burning long random exploration. Prefer host tools
  already on PATH (tshark, tcpdump, binwalk, strings, xxd, file, python3).
---

# Local CTF playbook (no answer spoilers)

Drive ONE direction to a recoverable artifact before switching.

## Pcap / network forensics

1. `file` + `capinfos` / `tshark -q -z io,phs` to confirm capture type.
2. List conversations: `tshark -r CAP -q -z conv,tcp`.
3. Follow the brief's port (or the busiest stream):  
   `tshark -r CAP -q -z follow,tcp,raw,N` and also `follow,tcp,ascii,N`.
4. Reassemble to a file — do not stop at hex dumps:  
   decode hex/raw stream to `shared/stream0.bin`, then `file`/`strings`/`binwalk`.
5. Custom/unknown protocols: parse length-prefixed frames in python3; carve
   embedded PE/ELF/ZIP/PNG by magic; write carved files under `shared/`.

FORBIDDEN for pcaps: XOR/LFSR "crypto" on the global header or filename hashes.

## Crypto / weird binaries

1. `file`, `strings`, `xxd`/`hexdump` headers, `readelf`/`objdump` if ELF.
2. Separate container format (magic/header) from ciphertext payload.
3. Prefer falsifiable tests: known-plaintext at a named offset, short repeating
   XOR, LFSR/Berlekamp on a cited keystream — expect printable/ELF/archive output.
4. Emit `VERIFIED_FACT=` for each confirmed structural finding.

## Flash / filesystem blobs

1. `binwalk -e` / signature scan; mount or unpack only player-facing layers.
2. Search carved trees for credentials, keys, and `flag{…}`-shaped strings.

## Always

- Read `.muteki_board.md` / blackboard before redo.
- Put long blobs in files; facts cite paths.
- Actually run tools; do not only plan.
