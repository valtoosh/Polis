"""
idls.py
=======

IDL loader for the agent. Reads the JSON IDLs emitted by `anchor build` (Anchor
0.30 format) and adapts them for `anchorpy` 0.20.1, whose IDL parser only
understands the older Anchor 0.28 schema.

The conversion is purely format-level (field rename `writable` -> `isMut`,
`signer` -> `isSigner`, `pubkey` -> `publicKey`, account-and-event types
inlined out of the new top-level `types` array). The on-chain wire formats —
instruction discriminators, borsh layouts, account structures — are identical
between 0.28 and 0.30, so the resulting `Program` instance round-trips bytes
correctly against the deployed 0.30 programs.

We do this in-process rather than committing a converted snapshot to disk so
the source of truth stays the single 0.30 IDL emitted by `anchor build`.

Usage:
    idls = load_idls()                           # dict: name -> anchorpy.Idl
    program = make_program(idls["credit"], pid, provider)
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Mapping, Optional

from anchorpy import Idl, Program, Provider
from solders.pubkey import Pubkey

# Names that we expect to find under target/idl/.
IDL_NAMES: tuple[str, ...] = (
    "identity",
    "wallet",
    "x402",
    "credit",
    "reputation",
)


def _default_idl_dir() -> Path:
    """Resolve the IDL directory.

    Order of precedence:
      1. `IDL_PATH` env var (absolute or relative to cwd).
      2. `../target/idl` relative to this file (matches the in-repo layout).
    """
    env = os.environ.get("IDL_PATH")
    if env:
        return Path(env).expanduser().resolve()
    here = Path(__file__).resolve().parent
    return (here.parent / "target" / "idl").resolve()


# --- Anchor 0.30 -> 0.28 IDL conversion ------------------------------------
#
# Why we need this: `anchorpy==0.20.1` is pinned in requirements.txt and only
# parses the 0.28 IDL schema. The instruction discriminator and account
# discriminator bytes are identical between the two versions, so once a Program
# is loaded with a converted IDL, all wire-level operations work against
# 0.30-emitted programs.

def _convert_field_type(t: Any) -> Any:
    """Recursively convert a type spec from 0.30 to 0.28 form."""
    if isinstance(t, str):
        if t == "pubkey":
            return "publicKey"
        return t
    if isinstance(t, dict):
        if "array" in t:
            inner, length = t["array"]
            return {"array": [_convert_field_type(inner), length]}
        if "option" in t:
            return {"option": _convert_field_type(t["option"])}
        if "vec" in t:
            return {"vec": _convert_field_type(t["vec"])}
        if "defined" in t:
            d = t["defined"]
            # 0.30 wraps defined types in {"name": "Foo"}; 0.28 wants the bare string.
            if isinstance(d, dict):
                return {"defined": d.get("name", str(d))}
            return {"defined": d}
        return t
    return t


def _convert_account_meta(item: Mapping[str, Any]) -> dict[str, Any]:
    out: dict[str, Any] = {
        "name": item["name"],
        "isMut": bool(item.get("writable", False)),
        "isSigner": bool(item.get("signer", False)),
    }
    if "docs" in item:
        out["docs"] = item["docs"]
    if item.get("optional", False):
        out["isOptional"] = True
    return out


def _convert_arg(arg: Mapping[str, Any]) -> dict[str, Any]:
    return {"name": arg["name"], "type": _convert_field_type(arg["type"])}


def _convert_struct_field(field: Mapping[str, Any]) -> dict[str, Any]:
    out: dict[str, Any] = {
        "name": field["name"],
        "type": _convert_field_type(field["type"]),
    }
    if "docs" in field:
        out["docs"] = field["docs"]
    return out


def _convert_type_def(td: Mapping[str, Any]) -> dict[str, Any]:
    """Convert a top-level type definition (struct or enum)."""
    out: dict[str, Any] = {"name": td["name"]}
    t = td["type"]
    if t.get("kind") == "struct":
        out["type"] = {
            "kind": "struct",
            "fields": [_convert_struct_field(f) for f in t.get("fields", [])],
        }
    elif t.get("kind") == "enum":
        out["type"] = {"kind": "enum", "variants": t.get("variants", [])}
    else:
        out["type"] = t
    return out


def _convert_instruction(ix: Mapping[str, Any]) -> dict[str, Any]:
    out: dict[str, Any] = {
        "name": ix["name"],
        "accounts": [_convert_account_meta(a) for a in ix.get("accounts", [])],
        "args": [_convert_arg(a) for a in ix.get("args", [])],
    }
    if "docs" in ix:
        out["docs"] = ix["docs"]
    if "returns" in ix:
        out["returns"] = _convert_field_type(ix["returns"])
    return out


def _convert_event(ev: Mapping[str, Any], types_lookup: Mapping[str, Any]) -> dict[str, Any]:
    """Inline the event's struct definition (in 0.30 it's a separate type)."""
    name = ev["name"]
    type_def = types_lookup.get(name)
    if not type_def:
        return {"name": name, "fields": []}
    t = type_def["type"]
    if t.get("kind") != "struct":
        return {"name": name, "fields": []}
    fields_out: list[dict[str, Any]] = []
    for f in t.get("fields", []):
        fields_out.append(
            {
                "name": f["name"],
                "type": _convert_field_type(f["type"]),
                "index": False,
            }
        )
    return {"name": name, "fields": fields_out}


def _convert_account(acct: Mapping[str, Any], types_lookup: Mapping[str, Any]) -> dict[str, Any]:
    """Inline the account's struct definition the same way."""
    name = acct["name"]
    type_def = types_lookup.get(name)
    if not type_def:
        return {"name": name, "type": {"kind": "struct", "fields": []}}
    return _convert_type_def(type_def)


def _convert_idl(raw: Mapping[str, Any]) -> dict[str, Any]:
    """Top-level conversion from 0.30 to 0.28 schema."""
    types_lookup: dict[str, Any] = {t["name"]: t for t in raw.get("types", [])}
    account_names = {a["name"] for a in raw.get("accounts", [])}
    event_names = {e["name"] for e in raw.get("events", [])}

    return {
        "version": raw["metadata"]["version"],
        "name": raw["metadata"]["name"],
        "instructions": [_convert_instruction(ix) for ix in raw.get("instructions", [])],
        "accounts": [_convert_account(a, types_lookup) for a in raw.get("accounts", [])],
        # `types` in 0.28 excludes anything that's been promoted to an account or event.
        "types": [
            _convert_type_def(t)
            for t in raw.get("types", [])
            if t["name"] not in account_names and t["name"] not in event_names
        ],
        "events": [_convert_event(e, types_lookup) for e in raw.get("events", [])],
        "errors": raw.get("errors", []),
    }


# --- public API ------------------------------------------------------------

def load_idl(name: str, idl_dir: Optional[Path] = None) -> Idl:
    """Load a single IDL by name. Caller passes the bare name (e.g. ``"credit"``)."""
    base = idl_dir or _default_idl_dir()
    path = base / f"{name}.json"
    if not path.exists():
        raise FileNotFoundError(
            f"IDL not found: {path}. Set $IDL_PATH or run `anchor build` first."
        )
    with path.open("r", encoding="utf-8") as f:
        raw = json.load(f)
    converted = _convert_idl(raw)
    return Idl.from_json(json.dumps(converted))


def load_idls(idl_dir: Optional[Path] = None) -> dict[str, Idl]:
    """Load every expected IDL into a dict keyed by program name."""
    return {name: load_idl(name, idl_dir=idl_dir) for name in IDL_NAMES}


def make_program(idl: Idl, program_id: Pubkey, provider: Optional[Provider]) -> Program:
    """Construct an anchorpy `Program` bound to a specific program ID + provider."""
    return Program(idl, program_id, provider=provider)
