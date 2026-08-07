#!/usr/bin/env bash
# Generate one zonescan-format ABI per program:
#
#   programs/artifacts/bonding_curve-abi.json
#   programs/artifacts/lbp-abi.json
#   programs/artifacts/wlez-abi.json
#
# Each file IS the `instruction` type descriptor zonescan expects for a custom
# program (`ProgSchema { id, instruction }`): a bare
# `{"enum":[{"name":..,"fields":[{"name":..,"type":..}]}]}`.
#
# Type vocabulary, per zone-scan/src/serve.rs:
#   "u8"/"u16"/"u32" (1 risc0 word) - "u64" (2) - "u128" (4) - "bool" (1)
#   "string" (len + packed) - "bytes" (len + 1 word/byte)
#   {"vec":T} - {"array":[T,N]} - {"struct":[{name,type}]} - {"enum":[...]}
# An enum is a 1-word variant index then that variant's fields, so DECLARATION
# ORDER is the wire discriminant. Unit variants omit "fields".
#
# Rust types are resolved to that vocabulary rather than emitted as alias names -
# a decoder cannot know what `AccountId` means. The two that matter:
#
#   AccountId  -> "string"             it derives SerializeDisplay, so serde
#                                      emits base58 text, NOT 32 bytes
#   ProgramId  -> {"array":["u32",8]}  a plain `[u32; 8]` alias
#
# Derived from the core crate's `Instruction` enum, so the ABI cannot drift from
# the deployed program. Replaces SPEL's idl-gen, which died with the LEZ v0.2.1
# migration. An unmapped Rust type is a hard error, never a guess.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
LPAD="${LPAD_BIN:-$REPO/cli/target/release/lpad}"

[ -x "$LPAD" ] || { echo "✗ lpad CLI not built: $LPAD (cd cli && cargo build --release)" >&2; exit 1; }

cd "$REPO"
python3 - "$LPAD" <<'PY'
import json, re, subprocess, sys

LPAD = sys.argv[1]

def resolve(ty):
    """Rust type -> zonescan type descriptor."""
    t = ty.strip()
    m = re.fullmatch(r'Vec\s*<\s*(.+)\s*>', t)
    if m:
        return {"vec": resolve(m.group(1))}
    m = re.fullmatch(r'\[\s*(.+?)\s*;\s*(\d+)\s*\]', t)
    if m:
        return {"array": [resolve(m.group(1)), int(m.group(2))]}
    m = re.fullmatch(r'Option\s*<\s*(.+)\s*>', t)
    if m:
        return {"enum": [{"name": "None"},
                         {"name": "Some",
                          "fields": [{"name": "0", "type": resolve(m.group(1))}]}]}
    prim = {
        "u8": "u8", "u16": "u16", "u32": "u32", "u64": "u64", "u128": "u128",
        "i64": "u64",
        "bool": "bool",
        "String": "string", "&str": "string",
        "AccountId": "string",
        "ProgramId": {"array": ["u32", 8]},
    }
    if t in prim:
        return prim[t]
    raise SystemExit(
        f"✗ unmapped Rust type in an Instruction variant: {t!r}\n"
        f"  Add it to resolve() in scripts/build-abi.sh - guessing would produce\n"
        f"  an ABI that silently mis-decodes.")

def instruction_enum(core):
    s = open(f"programs/{core}/core/src/lib.rs").read()
    e = s[s.index("pub enum Instruction"):]
    depth = 0
    for i, ch in enumerate(e):
        if ch == '{': depth += 1
        elif ch == '}':
            depth -= 1
            if depth == 0:
                end = i
                break
    body = re.sub(r'///.*|//.*|#\[[^\n]*\]', '', e[e.index('{') + 1:end])
    out = []
    for m in re.finditer(r'(\w+)\s*\{([^}]*)\}|^\s*(\w+)\s*,', body, re.M):
        if m.group(1):
            out.append((m.group(1),
                        [(f.group(1), f.group(2).strip())
                         for f in re.finditer(r'(\w+)\s*:\s*([^,]+?)\s*(?:,|$)', m.group(2))]))
        elif m.group(3):
            out.append((m.group(3), []))
    return out

def pid(which):
    return json.loads(subprocess.run([LPAD, "program-id", which, "--json"],
                                     capture_output=True, text=True,
                                     check=True).stdout)["program_id"]

PROGRAMS = [("bonding_curve", "bc"), ("lbp", "lbp"), ("wlez", "wlez")]

for core, cli_name in PROGRAMS:
    variants = []
    for variant, fields in instruction_enum(core):
        v = {"name": variant}
        if fields:
            v["fields"] = [{"name": f, "type": resolve(t)} for f, t in fields]
        variants.append(v)
    out = f"programs/artifacts/{core}-abi.json"
    with open(out, "w") as fh:
        json.dump({"enum": variants}, fh, indent=2)
        fh.write("\n")
    print(f"✓ {out}")
    print(f"    program id {pid(cli_name)}")
    print("    " + ", ".join(f"{i}:{v['name']}" for i, v in enumerate(variants)))

# Ready-to-paste zonescan config fragment, so ids and descriptors cannot be
# mismatched by hand.
frag = [{"id": pid(cli),
         "instruction": json.load(open(f"programs/artifacts/{core}-abi.json"))}
        for core, cli in PROGRAMS]
with open("programs/artifacts/zonescan-program-schemas.json", "w") as fh:
    json.dump({"program_schemas": frag}, fh, indent=2)
    fh.write("\n")
print("✓ programs/artifacts/zonescan-program-schemas.json  (paste into zonescan config)")
PY
