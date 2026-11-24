#!/usr/bin/env python3
"""Make a rustc/lld Windows executable and its PDB reproducible.

rustc links a temporary `symbols.o` that lives in a per-invocation directory
(`/tmp/rustcXXXXXX`). lld records that path in the PDB, hashes the PDB to make
the CodeView GUID, and stamps the GUID into both the PDB header and the
executable's `RSDS` record -- so two builds of identical code differ in the PDB
and in 8 bytes of the executable.

This rewrites the random directory to a fixed name (`rustcs1ph0n`, same width as
`rustc` + six characters, so no offsets move), recomputes the digest the
way lld does (over the file with the signature and GUID zeroed) and writes it
back into both files, so the pair stays matched for a debugger.

The PDB is named after the crate (`siphon-daemon.exe` -> `siphon_daemon.pdb`),
so it is taken from the path the linker recorded rather than guessed. The path
of the normalized PDB is printed on stdout.

Usage: normalize-pe-debug.py <executable>
"""
import re
import struct
import sys
from pathlib import Path

import xxhash

TEMP_DIR = re.compile(rb"rustc[0-9A-Za-z]{6}")
FIXED_DIR = b"rustcs1ph0n"
MARKER = b"LLD PDB."

# `RSDS`, a 16-byte GUID, a u32 age, then a NUL-terminated path to the PDB.
CODEVIEW = re.compile(rb"RSDS")
CODEVIEW_PATH = 4 + 16 + 4


def recorded_pdb(exe: bytes, exe_path: str) -> tuple[int, Path]:
    """The CodeView GUID offset and the PDB path the linker wrote there."""
    records = [m.start() for m in CODEVIEW.finditer(exe)]
    if len(records) != 1:
        raise SystemExit(f"{exe_path}: expected one CodeView record, found {len(records)}")
    start = records[0] + CODEVIEW_PATH
    end = exe.find(b"\0", start)
    if end < 0:
        raise SystemExit(f"{exe_path}: unterminated PDB path in the CodeView record")
    # The recorded path is absolute and belongs to the build directory; the PDB
    # is next to the executable either way.
    name = Path(exe[start:end].decode("utf-8", "surrogateescape")).name
    return records[0], Path(exe_path).parent / name


def freeze_temp_dir(raw: bytes, pdb_path: Path) -> bytearray:
    """Rewrite the per-link temp directory to a fixed name of the same width."""
    found = False
    for match in TEMP_DIR.finditer(raw):
        found = True
        # The pattern is `rustc` plus six characters, so the replacement is the
        # same width and no offset moves. A longer name would leave a tail
        # behind, so fail rather than ship a PDB whose GUID still moves.
        if raw[match.end() : match.end() + 1].isalnum():
            raise SystemExit(f"{pdb_path}: {match.group()!r} is not a rustc temp dir")
    if not found:
        raise SystemExit(f"{pdb_path}: no rustc temp directory recorded")
    return bytearray(TEMP_DIR.sub(FIXED_DIR, raw))


def normalize(exe_path: str) -> Path:
    exe = bytearray(Path(exe_path).read_bytes())
    record, pdb_path = recorded_pdb(exe, exe_path)
    pdb = freeze_temp_dir(pdb_path.read_bytes(), pdb_path)

    markers = [m.start() for m in re.finditer(re.escape(MARKER), bytes(pdb))]
    if len(markers) != 1:
        raise SystemExit(f"{pdb_path}: expected one PDB stream marker, found {len(markers)}")
    # PDB stream: u32 signature, u32 age, 16-byte GUID starting with the marker.
    guid_off = markers[0] - 8
    sig_off = guid_off - 8

    # lld hashes the buffer before it writes the signature, age and GUID.
    pdb[sig_off : sig_off + 4] = b"\0" * 4
    pdb[guid_off : guid_off + 16] = b"\0" * 16
    digest = xxhash.xxh3_64(bytes(pdb)).intdigest()

    pdb[sig_off : sig_off + 4] = struct.pack("<I", digest & 0xFFFF_FFFF)
    pdb[guid_off : guid_off + 8] = struct.pack("<Q", digest)
    pdb[guid_off + 8 : guid_off + 16] = MARKER

    exe[record + 4 : record + 12] = struct.pack("<Q", digest)
    exe[record + 12 : record + 20] = MARKER

    pdb_path.write_bytes(pdb)
    Path(exe_path).write_bytes(exe)
    return pdb_path


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} <executable>")
    print(normalize(sys.argv[1]))
