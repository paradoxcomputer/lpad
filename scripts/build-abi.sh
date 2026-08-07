#!/usr/bin/env bash
# Generate one self-contained ABI per program:
#
#   programs/artifacts/bonding_curve-abi.json
#   programs/artifacts/lbp-abi.json
#   programs/artifacts/wlez-abi.json
#
# Everything is DERIVED FROM SOURCE, nothing hand-maintained:
#
#   * instructions, discriminants and arg types  <- the `Instruction` enum in
#     `<program>/core/src/lib.rs` (declaration order is the wire discriminant)
#   * account names and order                    <- the `let [...] = pre_states`
#     destructuring in `<program>/src/main.rs`, which IS the on-chain ABI
#   * program id                                 <- the committed guest artifact,
#     via `lpad program-id`
#
# This is deliberate. SPEL's idl-gen died with the LEZ v0.2.1 migration, so the
# old `*-idl.json` files became hand-maintained and would rot silently on the next
# instruction change. Deriving from the dispatcher and the enum means the ABI
# cannot disagree with the deployed program: if a guest changes, so does this.
#
# This replaces the old hand-maintained `*-idl.json` files, which are deleted.
# Account-DATA types (SaleState, PoolState, ...) come from the borsh-derived
# structs/enums in the core crate, so they track the real encoding too.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
LPAD="${LPAD_BIN:-$REPO/cli/target/release/lpad}"

[ -x "$LPAD" ] || { echo "✗ lpad CLI not built: $LPAD (cd cli && cargo build --release)" >&2; exit 1; }

cd "$REPO"
python3 - "$LPAD" <<'PY'
import json, re, subprocess, sys, datetime

LPAD = sys.argv[1]

ENCODING = {
    "instruction_data": "Vec<u32>",
    "codec": "risc0 word-oriented serde (risc0_zkvm::serde), NOT borsh",
    "notes": [
        "The instruction is encoded as its DECLARATION INDEX (see `discriminant`) as one u32 word, followed by its args in order.",
        "A u128 arg occupies 4 little-endian u32 words; u64 occupies 2; bool/u8/u32 occupy 1.",
        "AccountId and ProgramId are 32 bytes = 8 u32 words.",
        "Vec<T> is encoded as a u32 length followed by the elements.",
        "`accounts` gives NAME and ORDER only, taken from the guest dispatcher. The order IS the ABI: it must match message.account_ids exactly. Mutability/signer flags are deliberately absent rather than guessed - the SPEL-era IDLs never populated them, so every flag read `false` even for real signers like `creator`.",
        "Account DATA (SaleState, PoolState, TokenHolding, ...) is borsh, NOT risc0 serde. Do not use one codec for both.",
        "Account ids are [u32;8] little-endian words in getAccount/getProgramIds responses, and base58 with a Public/ or Private/ prefix in CLI and wallet output.",
    ],
}

def enum_variants(core):
    """[(Variant, [(field, type)])] in declaration order = wire discriminant order."""
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
    out = []
    for m in re.finditer(r'(\w+)\s*\{([^}]*)\}|^\s*(\w+)\s*,', body, re.M):
        if m.group(1):
            out.append((m.group(1),
                        [(f.group(1), f.group(2).strip())
                         for f in re.finditer(r'(\w+)\s*:\s*([^,]+?)\s*(?:,|$)', m.group(2))]))
        elif m.group(3):
            out.append((m.group(3), []))
    return out

def dispatcher_accounts(program):
    """Variant -> ordered account names, from `let [...] = pre_states`."""
    src = open(f"programs/{program}/src/main.rs").read()
    out = {}
    for name, _, accts in re.finditer(
            r'Instruction::(\w+)\s*(?:\{[^}]*\})?\s*=>\s*\{(.*?)let\s*\[([^\]]+)\]\s*=\s*pre_states',
            src, re.S) and re.findall(
            r'Instruction::(\w+)\s*(?:\{[^}]*\})?\s*=>\s*\{(.*?)let\s*\[([^\]]+)\]\s*=\s*pre_states',
            src, re.S):
        out[name] = [a.strip() for a in accts.replace('\n', ' ').split(',') if a.strip()]
    return out

def variant_docs(core, variant):
    """The /// block immediately above a variant, if any."""
    s = open(f"programs/{core}/core/src/lib.rs").read()
    e = s[s.index("pub enum Instruction"):]
    m = re.search(r'((?:^\s*///.*\n)+)\s*' + variant + r'\s*[{(,]', e, re.M)
    if not m:
        return []
    return [re.sub(r'^\s*///\s?', '', l).rstrip()
            for l in m.group(1).splitlines() if l.strip()]

def account_types(core):
    """Borsh-derived structs/enums from the core crate = the account-DATA types.

    These are what an indexer needs to decode `account.data`, which is borsh (as
    opposed to the risc0-serde instruction payload). Derived from source for the
    same reason as everything else: the legacy hand-maintained IDLs could rot.
    `Instruction` is excluded - it is the instruction payload, described above.
    """
    src = open(f"programs/{core}/core/src/lib.rs").read()
    out = []
    for m in re.finditer(r'((?:^\s*(?:///.*|#\[[^\n]*\])\n)+)\s*pub (struct|enum) (\w+)\s*\{',
                         src, re.M):
        attrs, kind, name = m.group(1), m.group(2), m.group(3)
        if name == "Instruction" or "Borsh" not in attrs:
            continue
        # body up to the matching brace
        start = src.index('{', m.end() - 1)
        depth = 0
        for i in range(start, len(src)):
            if src[i] == '{': depth += 1
            elif src[i] == '}':
                depth -= 1
                if depth == 0:
                    body = src[start + 1:i]
                    break
        body_nc = re.sub(r'///.*|//.*|#\[[^\n]*\]', '', body)
        docs = [re.sub(r'^\s*///\s?', '', l).rstrip()
                for l in attrs.splitlines() if l.strip().startswith('///')]
        if kind == "struct":
            fields = [{"name": f.group(1), "type": f.group(2).strip()}
                      for f in re.finditer(r'pub\s+(\w+)\s*:\s*([^,]+?)\s*(?:,|$)', body_nc, re.M)]
            out.append({"name": name, "kind": "struct", "docs": docs, "fields": fields})
        else:
            variants = [{"name": v, "discriminant": i} for i, v in enumerate(
                [x.strip().rstrip(',') for x in body_nc.splitlines()
                 if x.strip() and re.match(r'^[A-Z]\w*\s*,?$', x.strip())])]
            out.append({"name": name, "kind": "enum", "docs": docs, "variants": variants,
                        "note": "borsh encodes the variant index as a u8"})
    return out

def pid(which):
    h = json.loads(subprocess.run([LPAD, "program-id", which, "--json"],
                                  capture_output=True, text=True, check=True).stdout)["program_id"]
    b = bytes.fromhex(h)
    return h, [int.from_bytes(b[i:i + 4], "little") for i in range(0, 32, 4)]

snake = lambda s: re.sub(r'(?<!^)(?=[A-Z])', '_', s).lower()

PROGRAMS = [
    ("bonding_curve", "bc",   "RFP-015 constant-product bonding curve over virtual reserves."),
    ("lbp",           "lbp",  "RFP-016 Balancer weight-shifting liquidity bootstrapping pool."),
    ("wlez",          "wlez", "Wraps the native LEZ balance into a token, so it can be used as curve/pool collateral."),
]

stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
warnings = []

for core, cli_name, desc in PROGRAMS:
    variants = enum_variants(core)
    accounts = dispatcher_accounts(core)
    h, words = pid(cli_name)

    instructions = []
    for i, (variant, fields) in enumerate(variants):
        if variant not in accounts:
            warnings.append(f"{core}: {variant} has no dispatcher arm - not in the ABI")
            continue
        instructions.append({
            "name": snake(variant),
            "variant": variant,
            "discriminant": i,
            "docs": variant_docs(core, variant),
            "accounts": [{"index": n, "name": a} for n, a in enumerate(accounts[variant])],
            "args": [{"name": f, "type": t} for f, t in fields],
        })

    types = account_types(core)
    if core == "wlez":
        # wlez_core has no borsh struct: the vault's `data` is a raw 32-byte
        # little-endian ProgramId written by encode_program_id at Initialize and
        # read back by every Wrap to pin the native-transfer leg.
        types.append({
            "name": "VaultData",
            "kind": "raw",
            "docs": [
                "The WLEZ vault account's `data`: the canonical native (authenticated-transfer) program id.",
                "NOT borsh - 32 raw little-endian bytes, i.e. the 8 u32 words of a ProgramId.",
                "Written once at Initialize (wlez_core::encode_program_id) and read by every Wrap (decode_program_id).",
            ],
            "layout": [{"offset": 0, "size": 32, "name": "native_program_id", "type": "ProgramId"}],
        })

    abi = {
        "schema": "lpad-abi/1",
        "generated": stamp,
        "generated_by": "scripts/build-abi.sh (derived from source)",
        "lez_version": "v0.2.4",
        "name": core,
        "description": desc,
        "program_id": h,
        "program_id_words": words,
        "instruction_type": f"{core}_core::Instruction",
        "networks": {
            "testnet": "https://testnet.lez.logos.co",
            "paradox": "https://seq-testnet.paradox.computer",
        },
        "encoding": ENCODING,
        "instructions": instructions,
        "account_types": types,
    }
    out = f"programs/artifacts/{core}-abi.json"
    json.dump(abi, open(out, "w"), indent=2)
    print(f"✓ {out}")
    print(f"    id {h}")
    print(f"    {len(instructions)} instructions: {', '.join(i['name'] for i in instructions)}")

# The canonical ATA program is deployed by LEZ at genesis, not by lpad, but the
# launchpad's *_ata instructions chain into it, so an indexer needs its id.
h, words = pid("ata")
json.dump({
    "schema": "lpad-abi/1",
    "generated": stamp,
    "lez_version": "v0.2.4",
    "name": "associated_token_account",
    "description": "Canonical upstream ATA program, pre-deployed at LEZ genesis. lpad does not deploy it; the *_ata instructions chain into it.",
    "program_id": h,
    "program_id_words": words,
    "instruction_type": "associated_token_account_core::Instruction",
    "deployed_by": "LEZ genesis",
    "encoding": ENCODING,
    "note": "Instruction shapes belong to upstream LEZ; see associated_token_account_core. Every variant carries `ata_program_id` as its FIRST field as of v0.2.1.",
}, open("programs/artifacts/associated_token_account-abi.json", "w"), indent=2)
print(f"✓ programs/artifacts/associated_token_account-abi.json\n    id {h}  (upstream; ids only)")

if warnings:
    print("\n! warnings:", file=sys.stderr)
    for w in warnings:
        print("   " + w, file=sys.stderr)
PY
