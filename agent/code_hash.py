# ---------------------------------------------------------------------------
# IMPORTANT: This module computes the agent's on-chain identity.
#
# The hash is computed over EVERY *.py file in the agent/ directory, sorted by
# filename, byte-for-byte. Any modification to source — including adding a log
# line, a comment, or reformatting whitespace — invalidates the code-hash and
# therefore invalidates the agent's registered identity on-chain.
#
# DO NOT edit agent source files after registration. The agent's identity is
# bound to the exact bytes of its source. To upgrade the agent, register a new
# identity under a new code-hash.
#
# Hash algorithm: Blake3 over the concatenation of (filename || NUL || bytes)
# for each .py file in agent/, sorted lexicographically. Encoding is UTF-8 on
# the filename; raw bytes on file contents.
# ---------------------------------------------------------------------------
from __future__ import annotations

import os
from pathlib import Path
from typing import Iterable

import blake3

AGENT_DIR = Path(__file__).resolve().parent


def _iter_source_files(directory: Path) -> Iterable[Path]:
    """Yield every *.py file in `directory`, sorted by filename.

    Only top-level files are considered (no recursion into subpackages). The
    reference agent is intentionally flat for hashing simplicity.
    """
    entries = sorted(p for p in directory.iterdir() if p.is_file() and p.suffix == ".py")
    return entries


def compute_code_hash(directory: Path | None = None) -> bytes:
    """Compute the Blake3 hash of the agent's source files.

    Returns a 32-byte digest matching the on-chain `code_hash: [u8; 32]` field.

    Deterministic given the same source bytes on disk. Filesystem metadata
    (mtime, permissions) is intentionally not included.
    """
    target = directory or AGENT_DIR
    h = blake3.blake3()
    for path in _iter_source_files(target):
        # Domain separation: filename, NUL byte, then file content. This
        # prevents collisions between (file_a, file_b) and (file_a+file_b, "").
        h.update(path.name.encode("utf-8"))
        h.update(b"\x00")
        h.update(path.read_bytes())
    digest = h.digest(length=32)
    assert len(digest) == 32
    return digest


def code_hash_hex(directory: Path | None = None) -> str:
    """Hex-encoded convenience form for logs and `/code-hash` endpoint."""
    return compute_code_hash(directory).hex()


if __name__ == "__main__":
    # CLI: print the agent's current code-hash. Used by the bootstrap script
    # to register the identity on-chain.
    print(code_hash_hex())
