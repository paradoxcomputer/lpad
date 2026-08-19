# Privacy & Anonymisation Properties

Covers both LPAD programs (RFP-015 bonding curve, RFP-016 LBP). Satisfies the
"privacy and anonymisation properties document" deliverable of both RFPs
(Supportability §5).

## 1. What is public (observable on-chain)

All curve/pool state is **public by design** — a deliberate architectural choice
that enables permissionless price verification, composability, and verifiable
analytics without cryptographic complexity in the program itself:

- **Bonding curve:** token pair, virtual reserves `Vt`/`Vc`, `k`, sale reserve,
  DEX-seed reserve, real collateral reserve, sale quantity `D`, current spot
  price, open/closed status, per-swap fee, cumulative collateral/fees/buy-count,
  and the bounded price-vs-supply observation ring.
- **LBP:** token pair, start/end weights and timestamps, current weight/price,
  reserves, total collateral raised, total tokens sold, buy count, the price
  observation ring; allowlist **configuration and whether the gate is enabled**
  (but never the eligible set); pause status.
- **Every PUBLIC buy/sell transaction:** collateral spent, tokens received, block.
  Under the `buy-private` saga (§3.1) the on-chain sender of that public leg is an
  **ephemeral intermediary account with no prior history**. Under `BuyDisposable`
  (§3.2) there is no public buy leg at all: the transaction publishes the sale/pool
  and its two vaults, and the buyer's amounts live in private slots.
- Sale close, creator withdrawal, and treasury-sweep transactions.

## 2. What is private (when using either private path)

**Two private modes ship** and are described in §3. Against an on-chain observer both
hide:

- **Which private account originated the collateral** for a buy.
- **Where the purchased tokens go.**
- **Any link between multiple buys by the same buyer** — the saga gives each buy a
  fresh ephemeral account with no shared on-chain history; `BuyDisposable` gives it a
  fresh single-use private NOTE, whose id is a hash of key material and is not
  something an observer can look up.
- **Whether a specific private account participated in the sale at all.**

They differ in one respect, and it is the reason both exist: the saga's ephemeral
account publicly holds first the collateral and then the tokens for the length of the
re-shield window, which is linkable; `BuyDisposable` has no such window because it has
no intermediate state.

The relevant anonymity set is **all private accounts in the zone**, not the
number of participants — the execution environment is shielded by construction.

### Allowlist + privacy (LBP)
The LBP allowlist is a **sorted-pair SHA-256 Merkle** set-membership gate: only the
Merkle **root** is committed on chain, so the eligibility set itself is never
published. The leaf is `SHA-256(buyer_collateral_holding_id)` - the account that
must authorize the buy - which is what stops a captured `(leaf, proof)` from being
replayed by a third party.

**It is not a ZK gate, and it does not compose with the private path.** An earlier
revision of this section said the two could both be enabled and that the effective
anonymity set was then unchanged relative to a non-gated sale. That was wrong twice
over, and it reached `proposal.md`:

* `BuyGated` is submitted as an ordinary **public** transaction
  (`LaunchpadClient::lbp_buy_gated` -> `submit_public`), with the buyer's collateral
  holding as a signer. The leaf and the entire inclusion path travel as **cleartext
  instruction data** - nothing about the membership proof is hidden.
* There is **no gated private path at all**: no `buy-gated-private` command exists,
  and neither the `deshield → buy → re-shield` saga nor `BuyDisposable` carries an
  allowlist proof. A buyer picks gated **or** private; the combination the old
  sentence described is not a configuration this build can produce.

So for a gated buy the anonymity set is **the allowlist, not the zone**. Against an
observer who holds or can guess the member list, a gated buy is a membership
confirmation, and two gated buys by the same account are linkable to each other by
their shared leaf. The gate constrains eligibility *and*, on this path, observability.

Verifying membership **inside** the privacy proof - the guest checking inclusion
against the root with neither leaf nor path published - is what would restore the
zone-wide anonymity set for gated sales. It is not implemented.

## 3. The two interaction flows

### 3.1 `buy-private` — the saga (SDK-enforced, the default)

1. The buyer initiates from their private account. The SDK deshields to a
   **fresh** public account A as one indivisible action — the collateral token
   and (once the chain has a fee model — see §8) the gas to fund A. A is never
   reused.
2. A executes the buy against the curve/pool.
3. A re-shields the purchased project tokens to the buyer's private account.

The SDK orchestrates this as a **3-tx saga**: tx1 deshields `U→A` (privacy proof),
tx2 is a normal **public** `Buy`/`Sell` by A (re-prices against live state, so a
competing trade never invalidates it), tx3 re-shields the proceeds `A→U` (privacy
proof). The SDK sequences the three with best-effort recovery (retry + rollback; **no
durable journal yet** — see §7). This is the mode that has been run against a real
sequencer.

### 3.2 `BuyDisposable` — one atomic transaction (on-chain instruction)

**Privacy on LEZ is a per-slot label**, positionally aligned 1:1 with the guest's
`pre_states`. So this mode declares the buyer's collateral holding and token holding
as PRIVATE slots of an otherwise ordinary buy: the debit and the credit are private
notes inside a single proof, authorised by the buyer's nullifier key rather than by a
signature on the message.

**There is no ephemeral account, no deshield leg and no re-shield leg.** Earlier
revisions of this document (and of the internal `SPEC.md`) described a 5-leg in-proof router
that built one; that design was evaluated and dropped, and it is not what shipped.
"Disposable" names the single-use NOTE, not an account. Five accounts:
`[sale|pool, token_vault, collateral_vault, buyer_collateral_holding(private),
buyer_token_holding(private)]`, and zero public signers — which is legal, and is the
point.

Either the whole buy lands or nothing happened, so no crash can leave a trade
publicly visible the way the saga can. What it gives up is drift-freeness (§7): the
sale or pool PDA is pinned for the length of the proof, so a competing trade that
lands first invalidates it and the proof has to be redone. That trade-off, not a
defect, is why both modes ship.

On the bonding curve there is one further consequence. The protocol fee cannot be
paid inside the buy — a privacy proof pins every public account it touches
byte-for-byte, and a treasury shared across sales changes constantly, so pinning one
would let any fee anywhere invalidate every in-flight proof. The fee is therefore
escrowed in the collateral vault (`SaleState::treasury_owed`) and settled afterwards
by the permissionless `SweepTreasury`.

## 4. Trust assumptions

These are the saga's assumptions. `BuyDisposable` carries none of them: it is one
transaction with no intermediate account, so there is no pattern for a client to get
wrong and nothing for the program to fail to enforce.

- The saga's privacy guarantees depend on the buyer using an SDK/client that correctly
  implements the full `deshield → buy → re-shield` pattern. **The program cannot
  enforce the re-shield step**: a buyer who calls the program directly, or funds
  account A from an external source (a CEX withdrawal, a known wallet), creates an
  on-chain link and breaks their own anonymity. The SDK makes the atomic deshield
  a single indivisible action precisely to prevent this; the program enforces
  correctness of the trade (pricing, slippage, solvency, supply/weight rules), not
  the privacy pattern.
- **Gas** for account A must come exclusively from the deshield in step 1. (LEZ
  v0.2.4 has no fee model yet — see §8; this assumption becomes load-bearing once
  fees land.)
- The SDK must validate that the re-shield target is a **private (shielded)**
  account before submitting, and reject otherwise.

## 5. What happens if a user bypasses the expected path

- **Calls the program directly with a public account:** fully public buy — no
  privacy, but economically correct and safe (this is the supported public path).
- **Funds account A externally:** links A to that funding source; the buyer's
  anonymity is broken even though the program executed correctly.
- **Skips the re-shield:** purchased tokens are stranded in the ephemeral public
  account A, publicly visible and linkable on the next move. The buyer accepts
  full responsibility for any resulting privacy loss.

None of the three applies to `BuyDisposable`, which has no ephemeral account to fund
or strand anything in.

## 6. LBP-specific: time, proofs, and price fairness

This section applies to `BuyDisposable` (§3.2). Under the saga (§3.1) the LBP's only
pool touch is a proofless **public** `Buy` threading the live `CLOCK_01`, so it prices
at live block time and none of the below arises.

**A privacy transaction cannot carry the clock account at all.** Every public account
in one is pinned byte-for-byte at proving time and re-verified against live state at
inclusion, and `CLOCK_01`'s data is rewritten every block — so a private buy that
declared it could never be included, however fast the prover. This is stronger than a
latency race: it is structural. The bonding curve sidesteps the consequence entirely
(its price is supply-driven and needs no clock); the LBP **needs** time to price.

So a private LBP buy prices at a caller-supplied `t_buy_ms` argument, and the **guest**
— not the caller — emits the transaction's timestamp validity window as
`[t_buy_ms, min(deadline, t_buy_ms + 1h, t_end_ms))`. The sequencer admits it only
inside that window, which is what binds the argument to real time; clamping to the
PINNED pool's `t_end_ms` is the only end-of-sale check available on this path, and
clamping to the caller's `deadline` keeps the usual expiry promise.

Because the LBP price only **falls** over time, at admission the live price ≤ the
priced one — so the buyer can never exploit a stale-favourable price, and is only
mildly disadvantaged. (`buy_tokens_out` being non-decreasing in time at fixed reserves
is the whole safety argument; a proptest pins it.)

A **minimum sale duration**, enforced at creation as `t_end - t_start >=
min_duration_ms`, is what bounds that disadvantage so private-path buyers are not
systematically penalised relative to public-path buyers. Attributing that mandate to
RFP-016 was an error in earlier revisions: the text is **RFP-015's**, it sits in that
RFP's *Soft Requirements*, and it is conditional on an end timestamp being configured.
Both programs implement the guard anyway, because the latency argument applies to both
and the LBP is the one where price actually moves with time.

## 7. Pool-state drift and the private-buy design

§6's clock problem is one instance of a general rule. When the sequencer
validates a privacy transaction, it does **not** trust the pre-state the prover
baked in: for every *public* account the proof names, it rebuilds the pre-state
from **live chain state at submission time** and verifies the recursive STARK
against that. So if any other transaction modifies that public account between
proof generation (minutes earlier) and submission, verification fails and the
transaction is rejected as stale — the user must re-prove.

A private buy must read+write the **public** sale/pool PDA (it has to, for price
discovery). That forces a hard trade-off: **no structure is both private-at-the-pool
and drift-free** — private-at-the-pool means the pool PDA is read inside a proof
(pinned → drifts); drift-free means the pool interaction is a proofless public
transaction (unavoidably public). The launchpad ships **both** sides of it, because
which one is right depends on the sale.

| Mode | Shape | Pool drift | Privacy footprint | Proofs |
|------|-------|-----------|-------------------|--------|
| **`BuyDisposable`** (`buy-disposable`) | one tx, one STARK: the buyer's two holdings are PRIVATE slots of an ordinary buy — no ephemeral account, no deshield/re-shield legs | **Exposed** — the sale/pool PDA is pinned for the whole proof; a competing trade invalidates it and it must be re-proved | Strong — no intermediate public state exists at any point | 1 |
| **the saga** (`buy-private`) | 3 txs: deshield (proof) → **public** buy → re-shield (proof) | **Drift-free** — the only pool touch is a proofless public buy, atomic vs live state with `min_out` | Weaker — the ephemeral account A publicly holds the collateral then the tokens across the ~re-shield window, linkable | 2 |

An earlier revision of this table described the first row as a removed 5-leg in-proof
router with an ephemeral account inside the proof, and claimed only the saga shipped.
Both halves of that are wrong: `BuyDisposable` exists on both programs, and it never
builds an ephemeral account — privacy is a per-slot label, so the buyer's own holdings
are simply private slots.

The saga is **best-effort SDK-sequenced** (3 on-chain
transactions cannot be on-chain-atomic). There is **no durable journal today** — the
SDK runs the legs sequentially in-memory with the recovery below. **No funds are lost**
in any case, but the privacy of a partially-completed trade can be:
- If the public buy (tx2) fails, the SDK rolls the deshield back to private (same
  process). A crash *between* tx1 and tx2 leaves the deshielded input in the public
  account A — recoverable from the wallet, but publicly held.
- If the re-shield (tx3) fails, the SDK retries it a few times; if it still fails (or
  the process dies after tx2), the proceeds remain in the wallet-owned **public**
  account A — recoverable by re-running the re-shield, but publicly held (the privacy
  of that one trade is lost) until then. The returned error names this explicitly.

A fully crash-safe **durable** journal (persist saga state, resume incomplete sagas on
startup) is planned hardening, not yet implemented. None of that applies to
`BuyDisposable`, which is on-chain atomic: there is no partial state to journal. Its
cost is the other one — on a contended sale, a competing buy forces a multi-minute
re-prove — which is why the saga remains the default and the mode proven against a
real sequencer.

## 8. Chain findings (LEZ `v0.2.4`) — notes for maintainers

Observations from building against the runtime, recorded so acceptance criteria
can be read against the chain as it actually is:

- **No fee/gas model is implemented.** The execution cap that a fee model would
  price against is still a constant, carrying a `Make this variable when fees are
  implemented` TODO (`lee/state_machine/src/program/mod.rs`), and every transaction
  executes free. Consequently the RFP's "atomic deshield of collateral **and gas**" has
  **no gas component on this build** — we satisfy it as a single-proof collateral
  deshield. The gas half is designed-for and activates once fees land (it will ride
  on the durable saga journal noted in §7, once that is implemented).
- **A privacy transaction admits exactly one private input.** The
  `authenticated_transfer` guest asserts a single sender note, so deshielding two
  notes (e.g. collateral + gas) in **one** proof is not possible without a new
  on-chain program. This is why a fee model would not simply fold gas into the
  existing deshield proof.
- **In-proof public state drifts** (§7) — a general property of the zone, not
  specific to the launchpad: any privacy-over-public-state flow (including a
  private AMM swap on a sibling DEX) inherits it. The drift-free recipe is the
  same everywhere: keep the contended public mutation in a proofless public
  transaction and confine the proofs to the user's own notes.
