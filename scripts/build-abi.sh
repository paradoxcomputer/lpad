#!/usr/bin/env bash
# Build programs/artifacts/lpad-abi.json: one self-contained ABI bundle for
# indexers (zonescan) covering all three lpad programs plus the canonical ATA.
#
# Bundles, per program: the deployed program id (hex AND the [u32;8] words the RPC
# returns), and its instructions with account order, arg order and arg types.
#
# It also VALIDATES the hand-maintained per-program IDLs against the Rust
# `Instruction` enums before emitting, and fails if they have drifted. That guard
# matters because SPEL's idl-gen is gone, so nothing else keeps the IDLs honest.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
LPAD="${LPAD_BIN:-$REPO/cli/target/release/lpad}"
OUT="$REPO/programs/artifacts/lpad-abi.json"

[ -x "$LPAD" ] || { echo "✗ lpad CLI not built: $LPAD (cd cli && cargo build --release)" >&2; exit 1; }

cd "$REPO"
python3 - "$LPAD" "$OUT" <<'PY'
import json, re, subprocess, sys, datetime

LPAD, OUT = sys.argv[1], sys.argv[2]

def rust_enum(core):
    """Variant -> [(field, type)] parsed from the core crate's Instruction enum."""
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
    body = re.sub(r'///.*|//.*', '', e[e.index('{') + 1:end])
    out = {}
    for m in re.finditer(r'(\w+)\s*\{([^}]*)\}|^\s*(\w+)\s*,', body, re.M):
        if m.group(1):
            out[m.group(1)] = [(f.group(1), f.group(2).strip())
                               for f in re.finditer(r'(\w+)\s*:\s*([^,]+?)\s*(?:,|$)', m.group(2))]
        elif m.group(3):
            out[m.group(3)] = []
    return out

snake = lambda s: re.sub(r'(?<!^)(?=[A-Z])', '_', s).lower()

def pid(which):
    h = json.loads(subprocess.run([LPAD, "program-id", which, "--json"],
                                  capture_output=True, text=True, check=True).stdout)["program_id"]
    b = bytes.fromhex(h)
    return h, [int.from_bytes(b[i:i + 4], "little") for i in range(0, 32, 4)]

PROGRAMS = [
    ("bonding_curve", "bc",   "bonding_curve", "RFP-015 constant-product bonding curve over virtual reserves."),
    ("lbp",           "lbp",  "lbp",           "RFP-016 Balancer weight-shifting liquidity bootstrapping pool."),
    ("wlez",          "wlez", "wlez",          "Wraps the native LEZ balance into a token so it can be used as collateral."),
]

bundle = {
    "schema": "lpad-abi/1",
    "generated": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "lez_version": "v0.2.4",
    "networks": {
        "testnet": "https://testnet.lez.logos.co",
        "paradox": "https://seq-testnet.paradox.computer",
    },
    "encoding": {
        "instruction_data": "Vec<u32>",
        "codec": "risc0 word-oriented serde (risc0_zkvm::serde), NOT borsh",
        "notes": [
            "A u128 arg occupies 4 little-endian u32 words; u64 occupies 2; bool/u8/u32 occupy 1.",
            "An enum is encoded as its DECLARATION INDEX as one u32 word, followed by its fields. The `instructions` array below is in declaration order, so its index is the discriminant.",
            "AccountId and ProgramId are 32 bytes = 8 u32 words.",
            "`accounts` gives NAME and ORDER only. The order is the on-chain ABI - it must match message.account_ids exactly. Mutability/signer flags are deliberately omitted rather than guessed; consult the program source if you need them.",
            "Account DATA (SaleState, PoolState, TokenHolding, ...) is borsh, unlike instruction data. Do not use one codec for both.",
            "Account ids in getAccount/getProgramIds responses are [u32;8] little-endian words; account ids in CLI/wallet output are base58 with a Public/ or Private/ prefix.",
        ],
    },
    "programs": [],
}

problems = []
for core, cli_name, idl_name, desc in PROGRAMS:
    rs = rust_enum(core)
    idl = json.load(open(f"programs/artifacts/{idl_name}-idl.json"))
    by = {i["name"]: i for i in idl["instructions"]}

    instructions = []
    for discriminant, (variant, fields) in enumerate(rs.items()):
        name = snake(variant)
        if name not in by:
            problems.append(f"{core}: {name} missing from {idl_name}-idl.json")
            continue
        entry = by[name]
        idl_args = [a["name"] for a in entry.get("args", [])]
        if idl_args != [f for f, _ in fields]:
            problems.append(f"{core}: {name} arg order/names differ (rust={[f for f,_ in fields]} idl={idl_args})")
        instructions.append({
            "name": name,
            "variant": variant,
            "discriminant": discriminant,
            "docs": entry.get("docs", []),
            # Only name + order: the SPEL-era IDLs never populated
            # writable/signer/init (lpad used none of SPEL's #[account(...)]
            # constraints), so every flag was `false` regardless of reality -
            # e.g. `creator` really is a signer. Emitting them would be worse
            # than omitting them. Order and types are what a decoder needs.
            "accounts": [{"name": a["name"], "index": i}
                         for i, a in enumerate(entry.get("accounts", []))],
            "args": [{"name": f, "type": t} for f, t in fields],
        })

    h, words = pid(cli_name)
    bundle["programs"].append({
        "name": core,
        "description": desc,
        "program_id": h,
        "program_id_words": words,
        "instruction_type": idl.get("instruction_type"),
        "instructions": instructions,
        "account_types": idl.get("accounts", []) + idl.get("types", []),
    })

# The canonical ATA program is deployed by LEZ at genesis, not by lpad, but the
# launchpad's *_ata paths dispatch through it, so an indexer needs its id too.
h, words = pid("ata")
bundle["programs"].append({
    "name": "associated_token_account",
    "description": "Canonical upstream ATA program, pre-deployed at genesis. lpad's *_ata instructions chain into it.",
    "program_id": h,
    "program_id_words": words,
    "instruction_type": "associated_token_account_core::Instruction",
    "deployed_by": "LEZ genesis (not lpad)",
    "instructions": [],
    "account_types": [],
})

if problems:
    print("✗ ABI validation failed:", file=sys.stderr)
    for p in problems:
        print("   " + p, file=sys.stderr)
    sys.exit(1)

json.dump(bundle, open(OUT, "w"), indent=2)
print(f"✓ wrote {OUT}")
for p in bundle["programs"]:
    print(f"   {p['name']:26} {p['program_id']}  ({len(p['instructions'])} instructions)")
PY
