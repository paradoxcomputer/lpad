//! Host-logic unit tests for the bonding-curve state machine (pure, no accounts).

use bonding_curve_core::{
    private_window_hi, sell_collateral_out, SaleState, SaleStatus, MAX_PRIVATE_WINDOW_MS,
};
use lee_core::program::TimestampValidityWindow;

use crate::buy::apply_buy;
use crate::sell::apply_sell;

fn sample_sale() -> SaleState {
    SaleState {
        fee_bps: 100, // 1%
        virt_token: 1_100_000_000,
        virt_collateral: 30_000_000,
        k: 1_100_000_000u128 * 30_000_000u128,
        sale_reserve_initial: 1_000_000_000,
        sale_reserve: 1_000_000_000,
        status: SaleStatus::Open,
        ..Default::default()
    }
}

/// RFP-015 §Supportability names "invariant preservation across multiple buys"
/// as required coverage, and Functionality #1 says the pricing invariant "must
/// never change after creation". Both were only half-covered.
///
/// `invariant_never_weakens_across_buys` in `bonding_curve_core` does loop 200
/// times, but it drives the pure `buy_tokens_out` and then applies the state
/// transition BY HAND (`vt -= out; vc += c_eff;`). That tests a re-implementation
/// of `apply_buy` living in the test body - if `apply_buy` ever updated the
/// reserves differently, the test would keep passing. And `SaleState::
/// invariant_holds()` is asserted exactly once in the whole repo, on a hand-built
/// state with ZERO buys applied.
///
/// This drives the real `apply_buy` over a real `SaleState` and asserts the
/// invariant *and* the immutability of `k` after every single buy.
#[test]
fn the_invariant_holds_and_k_never_moves_across_many_buys() {
    let mut s = sample_sale();
    let k_at_creation = s.k;
    assert!(s.invariant_holds(), "the fixture must start valid");

    let mut buys = 0u32;
    let mut last_price = 0u128;
    for _ in 0..200 {
        if !matches!(s.status, SaleStatus::Open) {
            break;
        }
        let out = apply_buy(s.clone(), 1_000_000, 0, Some(1_000));
        if out.tokens_out == 0 {
            break;
        }
        s = out.state;
        buys += 1;

        assert!(
            s.invariant_holds(),
            "Vt*Vc fell below k after buy {buys}: vt={} vc={} k={}",
            s.virt_token,
            s.virt_collateral,
            s.k
        );
        assert_eq!(
            s.k, k_at_creation,
            "k is fixed at creation and must never be rewritten (buy {buys})"
        );
        // Supply-driven pricing: every buy must make the next token dearer.
        let price = (s.virt_collateral << 64) / s.virt_token;
        assert!(price > last_price, "price must rise with every buy (buy {buys})");
        last_price = price;
    }
    assert!(buys >= 2, "\"across multiple buys\" needs at least two; got {buys}");
    assert!(s.buy_count as u32 == buys, "every applied buy must be counted");
}

#[test]
fn buy_updates_reserves_and_accounting() {
    let s = sample_sale();
    let out = apply_buy(s.clone(), 1_000_000, 0, Some(1_000));
    assert!(out.tokens_out > 0);
    assert_eq!(out.fee, (1_000_000u128 * 100).div_ceil(10_000));
    assert_eq!(out.state.sale_reserve, s.sale_reserve - out.tokens_out);
    assert_eq!(out.state.real_collateral, out.c_eff);
    assert_eq!(out.state.virt_token, s.virt_token - out.tokens_out);
    assert_eq!(out.state.virt_collateral, s.virt_collateral + out.c_eff);
    assert_eq!(out.state.cum_collateral_in, 1_000_000);
    assert_eq!(out.state.cum_fees, out.fee);
    assert_eq!(out.state.buy_count, 1);
    assert_eq!(out.state.obs.len(), 1);
}

#[test]
#[should_panic(expected = "slippage")]
fn buy_reverts_on_slippage() {
    let s = sample_sale();
    let _ = apply_buy(s, 1_000_000, u128::MAX, Some(1_000));
}

#[test]
#[should_panic(expected = "remaining sale reserve")]
fn buy_reverts_when_exceeding_reserve() {
    let mut s = sample_sale();
    s.sale_reserve = 10;
    let _ = apply_buy(s, 1_000_000, 0, Some(1_000));
}

#[test]
fn buy_auto_closes_on_supply_target() {
    // Drive the sale until the reserve is exhausted; the final buy must flip to Closed.
    let mut s = sample_sale();
    s.sale_reserve = s.sale_reserve.min(s.virt_token - 1);
    let mut closed = false;
    for _ in 0..100_000 {
        if s.sale_reserve == 0 {
            break;
        }
        let want = s.sale_reserve;
        let cost = bonding_curve_core::buy_cost_for_tokens(
            s.virt_token,
            s.virt_collateral,
            s.fee_bps,
            want,
        );
        let out = apply_buy(s.clone(), cost, 0, Some(1_000));
        s = out.state;
        if matches!(s.status, SaleStatus::Closed) {
            closed = true;
            assert_eq!(s.sale_reserve, 0);
            break;
        }
    }
    assert!(closed, "sale must auto-close when the supply target is reached");
}

#[test]
#[should_panic(expected = "not open")]
fn buy_reverts_when_closed() {
    let mut s = sample_sale();
    s.status = SaleStatus::Closed;
    let _ = apply_buy(s, 1_000_000, 0, Some(1_000));
}

#[test]
#[should_panic(expected = "ended")]
fn buy_reverts_after_end_timestamp() {
    let mut s = sample_sale();
    s.end_timestamp_ms = 5_000;
    let _ = apply_buy(s, 1_000_000, 0, Some(6_000));
}

#[test]
fn private_buy_ignores_end_timestamp_in_logic() {
    // The disposable path passes None for the clock (it cannot carry one);
    // end-timestamp is enforced by the emitted validity window, not in-circuit.
    // apply_buy must not panic on time here.
    let mut s = sample_sale();
    s.end_timestamp_ms = 5_000;
    let out = apply_buy(s, 1_000_000, 0, None);
    assert!(out.tokens_out > 0);
    assert!(out.state.obs.is_empty(), "private path records no timestamped observation");
}

// ---- disposable-buy validity window ----------------------------------------
//
// A disposable buy carries no clock, so the window the guest emits around it is
// the whole of its time enforcement. `private_window_hi` returns the tightest of
// three upper bounds; one test per bound, each making that bound the winner.

#[test]
fn private_window_hi_takes_the_submitter_deadline() {
    let lo = 1_000_000;
    // Comfortably inside the free-option cap, and the sale has no end.
    assert_eq!(private_window_hi(lo + 60_000, lo, u64::MAX), lo + 60_000);
}

#[test]
fn private_window_hi_takes_the_free_option_cap() {
    // An open-ended deadline is clamped to MAX_PRIVATE_WINDOW_MS past the lower
    // bound, so a buyer cannot sit on a priced proof indefinitely.
    let lo = 1_000_000;
    assert_eq!(private_window_hi(u64::MAX, lo, u64::MAX), lo + MAX_PRIVATE_WINDOW_MS);
    // The cap saturates instead of wrapping at the end of representable time.
    assert_eq!(private_window_hi(u64::MAX, u64::MAX - 1, u64::MAX), u64::MAX);
}

#[test]
fn private_window_hi_takes_the_sale_end() {
    // This bound is the only way a private buy honours `end_timestamp_ms` at all.
    let lo = 1_000_000;
    assert_eq!(private_window_hi(u64::MAX, lo, lo + 5_000), lo + 5_000);
}

/// Each way the window can collapse must leave `hi <= not_before_ms`, which is
/// exactly the condition the guest asserts on before building the window. (The
/// assert itself lives in `main.rs`, a binary crate, so it is not reachable from
/// a unit test - the condition is.)
#[test]
fn private_window_collapses_when_an_upper_bound_precedes_the_lower_one() {
    let lo = 1_000_000;
    assert!(
        private_window_hi(lo - 1, lo, u64::MAX) <= lo,
        "a deadline before not_before_ms must collapse the window"
    );
    assert!(
        private_window_hi(u64::MAX, lo, lo) <= lo,
        "a sale ending exactly at not_before_ms must collapse the window (the bound is exclusive)"
    );
    assert!(
        private_window_hi(u64::MAX, lo, lo - 1) <= lo,
        "an already-ended sale must collapse the window"
    );
}

/// Second line of defence behind that assert: even if the guest let a collapsed
/// window through, `lee_core` refuses to build one, so the transaction cannot be
/// emitted with a window that admits nothing.
#[test]
#[should_panic(expected = "empty timestamp validity window")]
fn collapsed_private_window_is_rejected_by_lee_core() {
    let lo = 1_000_000;
    let hi = private_window_hi(lo - 1, lo, u64::MAX);
    let _ = TimestampValidityWindow::try_from(lo..hi)
        .expect("empty timestamp validity window: window_lo is at or past window_hi");
}

/// A sample sale with the real collateral reserve pre-funded, so a sell can pay
/// out (the two-way curve refunds real collateral from the vault).
fn sample_sale_with_reserve(real_collateral: u128) -> SaleState {
    SaleState { real_collateral, ..sample_sale() }
}

#[test]
fn sell_updates_reserves_and_accounting() {
    let s = sample_sale_with_reserve(1_000_000);
    let (to_seller, fee, c_out_raw) =
        sell_collateral_out(s.virt_token, s.virt_collateral, s.fee_bps, 1_000_000);
    let out = apply_sell(s.clone(), 1_000_000, 0, 1_000);
    assert_eq!(out.to_seller, to_seller);
    assert_eq!(out.fee, fee);
    assert_eq!(out.c_out_raw, c_out_raw);
    assert!(out.to_seller > 0 && out.fee > 0, "test must exercise a non-zero fee");
    assert_eq!(out.state.virt_token, s.virt_token + 1_000_000);
    assert_eq!(out.state.virt_collateral, s.virt_collateral - c_out_raw);
    assert_eq!(out.state.sale_reserve, s.sale_reserve + 1_000_000);
    assert_eq!(out.state.real_collateral, s.real_collateral - c_out_raw);
    assert_eq!(out.state.cum_fees, fee);
    assert_eq!(out.state.sell_count, 1);
    assert_eq!(out.state.obs.len(), 1);
}

#[test]
#[should_panic(expected = "one-directional")]
fn sell_reverts_when_one_directional() {
    let mut s = sample_sale_with_reserve(1_000_000);
    s.one_directional = true;
    let _ = apply_sell(s, 1_000_000, 0, 1_000);
}

#[test]
#[should_panic(expected = "more collateral than the real reserve")]
fn sell_reverts_over_real_reserve() {
    // Real reserve cannot cover the computed collateral-out.
    let s = sample_sale_with_reserve(1);
    let _ = apply_sell(s, 1_000_000, 0, 1_000);
}

#[test]
#[should_panic(expected = "slippage")]
fn sell_reverts_on_slippage() {
    let s = sample_sale_with_reserve(1_000_000);
    let _ = apply_sell(s, 1_000_000, u128::MAX, 1_000);
}

#[test]
#[should_panic(expected = "not open")]
fn sell_reverts_when_closed() {
    let mut s = sample_sale_with_reserve(1_000_000);
    s.status = SaleStatus::Closed;
    let _ = apply_sell(s, 1_000_000, 0, 1_000);
}

/// Account fixtures shared by the account-level test modules below (the pure
/// pricing tests above need none of them).
mod fixtures {
    use bonding_curve_core::SaleState;
    use lee_core::{
        account::{Account, AccountId, AccountWithMetadata, Data, Nonce},
        program::{ChainedCall, ProgramId},
    };
    use token_core::TokenHolding;

    pub const BC_PROGRAM_ID: ProgramId = [7u32; 8];
    pub const TOKEN_PROGRAM_ID: ProgramId = [5u32; 8];
    /// A second, equally legitimate token program. The project and collateral
    /// tokens need not live under the same one (see `lifecycle::withdraw`'s two
    /// legs), and only distinct ids can tell a vault-anchored dispatch apart from
    /// one read off a submitter-supplied account.
    pub const COLLATERAL_PROGRAM_ID: ProgramId = [15u32; 8];
    /// A program the attacker deployed. Deployment on LEZ is permissionless, so
    /// every account a submitter names can be owned by one of these.
    pub const EVIL_PROGRAM_ID: ProgramId = [66u32; 8];

    pub const CREATOR_ID: AccountId = AccountId::new([1u8; 32]);
    pub const SALE_ID: AccountId = AccountId::new([2u8; 32]);
    pub const TOKEN_VAULT_ID: AccountId = AccountId::new([3u8; 32]);
    pub const COLLATERAL_VAULT_ID: AccountId = AccountId::new([4u8; 32]);
    pub const TREASURY_ID: AccountId = AccountId::new([6u8; 32]);
    pub const TOKEN_DEF_ID: AccountId = AccountId::new([8u8; 32]);
    pub const COLLATERAL_DEF_ID: AccountId = AccountId::new([9u8; 32]);
    pub const CREATOR_COLL_HOLDING_ID: AccountId = AccountId::new([10u8; 32]);
    pub const CREATOR_TOKEN_HOLDING_ID: AccountId = AccountId::new([11u8; 32]);
    pub const BUYER_COLL_HOLDING_ID: AccountId = AccountId::new([12u8; 32]);
    pub const BUYER_TOKEN_HOLDING_ID: AccountId = AccountId::new([13u8; 32]);

    pub fn sale_account(state: &SaleState) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account {
                program_owner: BC_PROGRAM_ID,
                balance: 0u128,
                data: Data::from(state),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id: SALE_ID,
        }
    }

    pub fn creator_account(authorized: bool, id: AccountId) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account {
                program_owner: [0u32; 8],
                balance: 0u128,
                data: Data::default(),
                nonce: Nonce(0),
            },
            is_authorized: authorized,
            account_id: id,
        }
    }

    pub fn holding(id: AccountId, definition_id: AccountId, balance: u128) -> AccountWithMetadata {
        holding_owned_by(id, definition_id, balance, TOKEN_PROGRAM_ID)
    }

    /// A Fungible holding under an arbitrary token program. The `definition_id`
    /// in the data and the owning program are independent: the data is whatever
    /// the owning program wrote, so a holding under a program the submitter
    /// deployed can name any definition it likes.
    pub fn holding_owned_by(
        id: AccountId,
        definition_id: AccountId,
        balance: u128,
        program_owner: ProgramId,
    ) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account {
                program_owner,
                balance: 0u128,
                data: Data::from(&TokenHolding::Fungible { definition_id, balance }),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id: id,
        }
    }

    /// An account that has never been touched - the shape of an uninitialized
    /// PDA, and of a treasury id nothing has ever paid into.
    pub fn uninitialized(account_id: AccountId) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account::default(),
            is_authorized: false,
            account_id,
        }
    }

    pub fn collateral_vault(balance: u128) -> AccountWithMetadata {
        holding(COLLATERAL_VAULT_ID, COLLATERAL_DEF_ID, balance)
    }

    pub fn token_vault(balance: u128) -> AccountWithMetadata {
        holding(TOKEN_VAULT_ID, TOKEN_DEF_ID, balance)
    }

    /// The shared fee sink. It holds the collateral token, since that is what
    /// both the per-swap sweeps and the escrowed disposable-buy fees pay in.
    pub fn treasury() -> AccountWithMetadata {
        holding(TREASURY_ID, COLLATERAL_DEF_ID, 0)
    }

    /// Expected `instruction_data` for a `token::Transfer`, encoded exactly as the
    /// production code does via `ChainedCall::new`, so the tests assert the real
    /// transfer amount rather than just that a call exists.
    pub fn expected_transfer_data(amount: u128) -> Vec<u32> {
        ChainedCall::new(
            TOKEN_PROGRAM_ID,
            vec![],
            &token_core::Instruction::Transfer { amount_to_transfer: amount },
        )
        .instruction_data
    }

    /// Fungible balance of a chained call's pre-state - used to assert that a leg
    /// was built against the running state diff, not the proof-start snapshot.
    pub fn balance_of(awm: &AccountWithMetadata) -> u128 {
        match TokenHolding::try_from(&awm.account.data) {
            Ok(TokenHolding::Fungible { balance, .. }) => balance,
            _ => panic!("expected a Fungible token holding"),
        }
    }
}

/// Buy-path dispatch tests - the guard that stops a buy from moving the curve
/// without moving funds.
///
/// It has two halves and BOTH are load-bearing, so both are pinned here:
///   1. every chained transfer is dispatched on a VAULT's `program_owner`, never
///      on one read off the submitter-supplied buyer holding, and
///   2. the buyer's collateral holding must be owned by the same token program as
///      the collateral vault it pays into.
///
/// Remove (2) and a buyer can hand over a holding whose data claims the sale's
/// collateral definition while it is owned by a no-op program they deployed;
/// remove (1) and that program is handed the transfer legs, which then echo their
/// pre-states while `apply_buy` has already consumed the reserve and credited the
/// curve. A free sale-kill, and a drain on a two-way sale, since the inflated
/// `real_collateral` bounds a later `Sell` that pays out of the real vault.
///
/// Neither half is visible to the pure pricing tests above, so these run over
/// account fixtures and inspect the emitted `ChainedCall`s' program ids - the
/// technique `sweep_dispatches_on_the_vault_program_not_the_treasury` uses.
mod buy_dispatch_tests {
    use bonding_curve_core::{SaleState, SaleStatus};
    use lee_core::{
        account::AccountWithMetadata,
        program::{AccountPostState, ChainedCall},
    };

    use super::fixtures::*;
    use crate::buy::{buy, buy_disposable};
    use crate::util::authorized;

    const COLLATERAL_IN: u128 = 1_000_000;
    const RAISED: u128 = 4_000_000;

    /// A sale whose project and collateral tokens live under DIFFERENT token
    /// programs. That is legal, and it is what makes half (1) observable: with a
    /// single program every dispatch id is the same value, so a leg re-pointed at
    /// the payer would look identical to a correctly anchored one.
    fn open_sale() -> SaleState {
        SaleState {
            creator: CREATOR_ID,
            token_definition_id: TOKEN_DEF_ID,
            collateral_definition_id: COLLATERAL_DEF_ID,
            token_vault_id: TOKEN_VAULT_ID,
            collateral_vault_id: COLLATERAL_VAULT_ID,
            treasury_id: TREASURY_ID,
            fee_bps: 100, // 1%, so the public path emits its fee leg
            virt_token: 1_100_000_000,
            virt_collateral: 30_000_000,
            k: 1_100_000_000u128 * 30_000_000u128,
            sale_reserve_initial: 1_000_000_000,
            sale_reserve: 1_000_000_000,
            real_collateral: RAISED,
            status: SaleStatus::Open,
            ..Default::default()
        }
    }

    fn token_vault_acct() -> AccountWithMetadata {
        holding_owned_by(TOKEN_VAULT_ID, TOKEN_DEF_ID, 1_000_000_000, TOKEN_PROGRAM_ID)
    }

    fn collateral_vault_acct() -> AccountWithMetadata {
        holding_owned_by(COLLATERAL_VAULT_ID, COLLATERAL_DEF_ID, RAISED, COLLATERAL_PROGRAM_ID)
    }

    /// The buyer's collateral holding as an honest one looks: under the same token
    /// program as the collateral vault, and authorized (the buyer signs).
    fn buyer_collateral() -> AccountWithMetadata {
        authorized(&holding_owned_by(
            BUYER_COLL_HOLDING_ID,
            COLLATERAL_DEF_ID,
            COLLATERAL_IN,
            COLLATERAL_PROGRAM_ID,
        ))
    }

    /// The buyer's collateral holding as an attacker builds it: the data names the
    /// sale's collateral definition (data is written by whoever owns the account),
    /// but the account belongs to a program of their own.
    fn hostile_buyer_collateral() -> AccountWithMetadata {
        authorized(&holding_owned_by(
            BUYER_COLL_HOLDING_ID,
            COLLATERAL_DEF_ID,
            COLLATERAL_IN,
            EVIL_PROGRAM_ID,
        ))
    }

    /// Nothing in the account list pins the buyer's TOKEN holding, so a hostile
    /// one is the honest fixture here: the payout leg must still be dispatched on
    /// the token vault's program.
    fn buyer_token_holding() -> AccountWithMetadata {
        holding_owned_by(BUYER_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0, EVIL_PROGRAM_ID)
    }

    fn run_buy(
        buyer_collateral_holding: AccountWithMetadata,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        buy(
            sale_account(&open_sale()),
            token_vault_acct(),
            collateral_vault_acct(),
            holding_owned_by(TREASURY_ID, COLLATERAL_DEF_ID, 0, COLLATERAL_PROGRAM_ID),
            buyer_collateral_holding,
            buyer_token_holding(),
            COLLATERAL_IN,
            0,
            BC_PROGRAM_ID,
            0, // clock_ts; the fixture sale has no end timestamp
        )
    }

    fn run_disposable(
        buyer_collateral_holding: AccountWithMetadata,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        buy_disposable(
            sale_account(&open_sale()),
            token_vault_acct(),
            collateral_vault_acct(),
            buyer_collateral_holding,
            buyer_token_holding(),
            COLLATERAL_IN,
            0,
            BC_PROGRAM_ID,
        )
    }

    /// Half (1), public path. The token payout must run on the TOKEN vault's
    /// program even though the payer (and therefore both collateral legs) sits
    /// under a different one - so a leg re-pointed at `payer.account.program_owner`
    /// fails this.
    #[test]
    fn buy_dispatches_every_leg_on_the_vault_it_moves() {
        let (_, calls) = run_buy(buyer_collateral());
        assert_eq!(calls.len(), 3, "deposit, fee, then the token payout");
        assert_eq!(
            calls[0].program_id, COLLATERAL_PROGRAM_ID,
            "the deposit must run on the collateral VAULT's token program"
        );
        assert_eq!(
            calls[1].program_id, COLLATERAL_PROGRAM_ID,
            "so must the fee leg - it spends the same holding"
        );
        assert_eq!(
            calls[2].program_id, TOKEN_PROGRAM_ID,
            "the payout must run on the token VAULT's program, not the buyer's - neither the \
             payer nor the recipient may choose the program that moves the vault"
        );
    }

    /// Half (1), disposable path. Same anchor, one leg fewer (the fee is escrowed
    /// rather than swept).
    #[test]
    fn buy_disposable_dispatches_every_leg_on_the_vault_it_moves() {
        let (_, calls) = run_disposable(buyer_collateral());
        assert_eq!(calls.len(), 2, "one deposit, one payout");
        assert_eq!(
            calls[0].program_id, COLLATERAL_PROGRAM_ID,
            "the deposit must run on the collateral VAULT's token program"
        );
        assert_eq!(
            calls[1].program_id, TOKEN_PROGRAM_ID,
            "the payout must run on the token VAULT's program, not the buyer's"
        );
    }

    /// Half (2), public path: the holding's data says the right definition, so the
    /// definition check passes and only the program-owner anchor stands between
    /// this buy and a no-op deposit leg.
    #[test]
    #[should_panic(expected = "not owned by the sale's collateral token program")]
    fn buy_rejects_a_collateral_holding_owned_by_a_foreign_program() {
        let _ = run_buy(hostile_buyer_collateral());
    }

    /// Half (2), disposable path - and doubly load-bearing here: this path emits
    /// no separate fee leg, so nothing else in the transaction would notice.
    #[test]
    #[should_panic(expected = "not owned by the sale's collateral token program")]
    fn buy_disposable_rejects_a_collateral_holding_owned_by_a_foreign_program() {
        let _ = run_disposable(hostile_buyer_collateral());
    }
}

/// Close + withdraw lifecycle tests. Unlike the pure pricing tests above these
/// operate over `AccountWithMetadata` fixtures, exercising the creator-auth,
/// close-gate and closed-status guards, the escrow that withdrawal must leave
/// behind for `SweepTreasury`, and (critically) the drained-vault
/// second-withdraw guard that protects the creator's collateral from a double
/// payout.
mod lifecycle_tests {
    use bonding_curve_core::{
        compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, SaleState, SaleStatus,
    };
    use lee_core::{
        account::{AccountId, AccountWithMetadata},
        program::ChainedCall,
    };

    use super::fixtures::*;
    use crate::lifecycle::{close_sale, withdraw};

    fn sale_state(status: SaleStatus) -> SaleState {
        SaleState {
            creator: CREATOR_ID,
            token_vault_id: TOKEN_VAULT_ID,
            collateral_vault_id: COLLATERAL_VAULT_ID,
            treasury_id: TREASURY_ID,
            real_collateral: 1_000_000,
            dex_seed_reserve: 200,
            sale_reserve: 300,
            end_timestamp_ms: 5_000,
            status,
            ..Default::default()
        }
    }

    // ---- close_sale ----

    #[test]
    fn close_after_end_timestamp_flips_to_closed() {
        let state = sale_state(SaleStatus::Open);
        let (post_states, calls) = close_sale(
            sale_account(&state),
            creator_account(true, CREATOR_ID),
            BC_PROGRAM_ID,
            6_000, // > end_timestamp_ms
        );
        assert!(calls.is_empty(), "close emits no chained calls");
        let closed = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert!(matches!(closed.status, SaleStatus::Closed));
    }

    #[test]
    fn close_when_supply_done_flips_to_closed() {
        let mut state = sale_state(SaleStatus::Open);
        state.sale_reserve = 0; // supply target reached
        state.end_timestamp_ms = 0; // and the timeout is unset
        let (post_states, _) = close_sale(
            sale_account(&state),
            creator_account(true, CREATOR_ID),
            BC_PROGRAM_ID,
            1, // well before any end timestamp
        );
        let closed = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert!(matches!(closed.status, SaleStatus::Closed));
    }

    #[test]
    fn close_recovers_stalled_no_timeout_sale_after_floor() {
        // No configured timeout and supply not exhausted: closeable once the
        // recovery floor (7 days) has elapsed since creation, so the creator's
        // collateral is recoverable rather than locked forever.
        let mut state = sale_state(SaleStatus::Open);
        state.end_timestamp_ms = 0;
        state.created_ts_ms = 1_000;
        let floor_ms = 7i64 * 24 * 60 * 60 * 1_000;
        let (post_states, _) = close_sale(
            sale_account(&state),
            creator_account(true, CREATOR_ID),
            BC_PROGRAM_ID,
            state.created_ts_ms + floor_ms,
        );
        let closed = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert!(matches!(closed.status, SaleStatus::Closed));
    }

    #[test]
    #[should_panic(expected = "cannot close")]
    fn close_rejects_no_timeout_sale_before_floor() {
        let mut state = sale_state(SaleStatus::Open);
        state.end_timestamp_ms = 0;
        state.created_ts_ms = 1_000;
        let _ = close_sale(
            sale_account(&state),
            creator_account(true, CREATOR_ID),
            BC_PROGRAM_ID,
            state.created_ts_ms + 1, // long before the recovery floor
        );
    }

    #[test]
    #[should_panic(expected = "creator must authorize close")]
    fn close_rejects_unauthorized_creator() {
        let state = sale_state(SaleStatus::Open);
        let _ = close_sale(
            sale_account(&state),
            creator_account(false, CREATOR_ID),
            BC_PROGRAM_ID,
            6_000,
        );
    }

    #[test]
    #[should_panic(expected = "only the creator may close")]
    fn close_rejects_non_creator() {
        let state = sale_state(SaleStatus::Open);
        let _ = close_sale(
            sale_account(&state),
            creator_account(true, AccountId::new([99u8; 32])),
            BC_PROGRAM_ID,
            6_000,
        );
    }

    #[test]
    #[should_panic(expected = "cannot close")]
    fn close_rejects_before_end_and_before_supply_target() {
        let state = sale_state(SaleStatus::Open); // sale_reserve > 0, end = 5_000
        let _ = close_sale(
            sale_account(&state),
            creator_account(true, CREATOR_ID),
            BC_PROGRAM_ID,
            1, // before end_timestamp_ms and supply not exhausted
        );
    }

    #[test]
    #[should_panic(expected = "sale already closed")]
    fn close_rejects_already_closed() {
        let state = sale_state(SaleStatus::Closed);
        let _ = close_sale(
            sale_account(&state),
            creator_account(true, CREATOR_ID),
            BC_PROGRAM_ID,
            6_000,
        );
    }

    // ---- withdraw ----

    fn run_withdraw(
        state: &SaleState,
        creator: AccountWithMetadata,
        coll_balance: u128,
        token_balance: u128,
    ) -> (Vec<lee_core::program::AccountPostState>, Vec<ChainedCall>) {
        withdraw(
            sale_account(state),
            token_vault(token_balance),
            collateral_vault(coll_balance),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, 0),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0),
            creator,
            BC_PROGRAM_ID,
        )
    }

    #[test]
    fn withdraw_happy_path_chains_two_transfers_and_zeroes_accounting() {
        // Nothing escrowed (no disposable buys), so the treasury leg is skipped.
        let state = sale_state(SaleStatus::Closed);
        let (post_states, calls) =
            run_withdraw(&state, creator_account(true, CREATOR_ID), 1_000_000, 500);

        // Two transfers: collateral first, then tokens (lifecycle.rs order).
        assert_eq!(calls.len(), 2, "expected one collateral + one token transfer");

        let coll = &calls[0];
        assert_eq!(coll.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(coll.pda_seeds, vec![compute_collateral_vault_pda_seed(SALE_ID)]);
        assert!(coll.pre_states[0].is_authorized, "collateral vault must be authorized");
        assert_eq!(coll.pre_states[0].account_id, COLLATERAL_VAULT_ID);
        assert_eq!(coll.pre_states[1].account_id, CREATOR_COLL_HOLDING_ID);
        assert_eq!(coll.instruction_data, expected_transfer_data(1_000_000));

        let tok = &calls[1];
        assert_eq!(tok.pda_seeds, vec![compute_token_vault_pda_seed(SALE_ID)]);
        assert!(tok.pre_states[0].is_authorized, "token vault must be authorized");
        assert_eq!(tok.pre_states[0].account_id, TOKEN_VAULT_ID);
        assert_eq!(tok.pre_states[1].account_id, CREATOR_TOKEN_HOLDING_ID);
        assert_eq!(tok.instruction_data, expected_transfer_data(500));

        // Accounting buckets zeroed on the closed-sale post-state.
        let post = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.real_collateral, 0);
        assert_eq!(post.dex_seed_reserve, 0);
        assert_eq!(post.sale_reserve, 0);
    }

    /// Fees escrowed by disposable buys sit in the collateral vault but belong to
    /// the treasury, not the creator. Withdrawal must pay the creator the vault
    /// MINUS that escrow, leave the escrow's collateral where it is, and leave
    /// `treasury_owed` on the books for `SweepTreasury` - zeroing it here would
    /// hand the treasury's money to nobody.
    ///
    /// It must also name no treasury account at all. It used to, and that is what
    /// made the natural `treasury_id == creator` setup unrecoverable: LEZ dedups
    /// `message.account_ids` and rejects a duplicate before executing anything,
    /// and every id in the list is pinned by an assert, so no other account list
    /// could drain a closed sale.
    #[test]
    fn withdraw_leaves_the_escrow_behind_and_does_not_zero_it() {
        let mut state = sale_state(SaleStatus::Closed);
        state.treasury_owed = 30_000;
        let (post_states, calls) =
            run_withdraw(&state, creator_account(true, CREATOR_ID), 1_000_000, 500);

        assert_eq!(calls.len(), 2, "expected one creator-collateral leg and one token leg");

        // The creator gets the vault minus the escrow, out of the FULL vault
        // balance: no earlier leg has moved it, so there is nothing to shift.
        let to_creator = &calls[0];
        assert_eq!(to_creator.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(to_creator.pda_seeds, vec![compute_collateral_vault_pda_seed(SALE_ID)]);
        assert!(to_creator.pre_states[0].is_authorized, "collateral vault must be authorized");
        assert_eq!(to_creator.pre_states[0].account_id, COLLATERAL_VAULT_ID);
        assert_eq!(balance_of(&to_creator.pre_states[0]), 1_000_000);
        assert_eq!(to_creator.pre_states[1].account_id, CREATOR_COLL_HOLDING_ID);
        assert_eq!(to_creator.instruction_data, expected_transfer_data(970_000));

        // The token leg is unaffected by the escrow.
        assert_eq!(calls[1].instruction_data, expected_transfer_data(500));

        // No leg touches the treasury - settlement is its own instruction.
        assert!(
            calls
                .iter()
                .all(|c| c.pre_states.iter().all(|p| p.account_id != TREASURY_ID)),
            "Withdraw must not name the treasury anywhere"
        );

        let post = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, 30_000, "the escrow must survive the withdrawal");
        assert_eq!(post.real_collateral, 0, "the creator's own bucket is zeroed");
        assert_eq!(post_states.len(), 6, "one post-state per declared account");
    }

    /// The creator must never be paid out of the escrow, even if the vault is
    /// somehow short of it: the collateral leg is skipped entirely rather than
    /// underflowing or dipping in.
    #[test]
    fn withdraw_pays_no_collateral_when_the_vault_is_short_of_the_escrow() {
        let mut state = sale_state(SaleStatus::Closed);
        state.treasury_owed = 30_000;
        let (post_states, calls) =
            run_withdraw(&state, creator_account(true, CREATOR_ID), 20_000, 500);
        assert_eq!(calls.len(), 1, "only the project-token leg survives");
        assert_eq!(calls[0].instruction_data, expected_transfer_data(500));
        let post = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, 30_000, "the full debt stays claimable");
    }

    /// Second withdrawal against a vault holding nothing but the escrow: the
    /// drained-vault guard looks at the CREATOR's share, not the raw balance, so
    /// this must revert rather than pay the treasury's money to the creator.
    #[test]
    #[should_panic(expected = "nothing to withdraw")]
    fn withdraw_rejects_a_second_withdraw_that_would_eat_the_escrow() {
        let mut state = sale_state(SaleStatus::Closed);
        state.treasury_owed = 30_000;
        let _ = run_withdraw(&state, creator_account(true, CREATOR_ID), 30_000, 0);
    }

    #[test]
    #[should_panic(expected = "sale must be closed before withdrawal")]
    fn withdraw_requires_closed() {
        let state = sale_state(SaleStatus::Open);
        let _ = run_withdraw(&state, creator_account(true, CREATOR_ID), 1_000_000, 500);
    }

    #[test]
    #[should_panic(expected = "creator must authorize withdrawal")]
    fn withdraw_rejects_unauthorized_creator() {
        let state = sale_state(SaleStatus::Closed);
        let _ = run_withdraw(&state, creator_account(false, CREATOR_ID), 1_000_000, 500);
    }

    #[test]
    #[should_panic(expected = "only the creator may withdraw")]
    fn withdraw_rejects_non_creator() {
        let state = sale_state(SaleStatus::Closed);
        let _ = run_withdraw(
            &state,
            creator_account(true, AccountId::new([99u8; 32])),
            1_000_000,
            500,
        );
    }

    /// The drained-vault guard is the ONLY thing preventing a second withdrawal
    /// (the zeroed accounting fields are not consumed by the guard - see the
    /// comment at lifecycle.rs). A second withdraw against an emptied pair must
    /// revert so creator funds cannot be paid out twice.
    #[test]
    #[should_panic(expected = "nothing to withdraw")]
    fn withdraw_rejects_second_withdraw_when_drained() {
        let state = sale_state(SaleStatus::Closed);
        let _ = run_withdraw(&state, creator_account(true, CREATOR_ID), 0, 0);
    }
}

/// `SweepTreasury` tests. This is the ONLY path by which an escrowed
/// disposable-buy fee leaves the collateral vault, so what is covered here is the
/// exact-amount payout, the clear-by-what-moved bookkeeping, and the pins that
/// keep a permissionless submission from doing anything other than paying the
/// treasury the sale already named.
mod sweep_tests {
    use bonding_curve_core::{compute_collateral_vault_pda_seed, SaleState, SaleStatus};
    use lee_core::account::{AccountId, Nonce};

    use super::fixtures::*;
    use crate::lifecycle::sweep_treasury;

    const OWED: u128 = 30_000;
    /// The creator's share, which a sweep must never touch.
    const RAISED: u128 = 1_000_000;

    /// A sale whose vault holds `RAISED + owed` - i.e. one satisfying
    /// `vault_balance == real_collateral + treasury_owed`.
    fn sale_state(status: SaleStatus, owed: u128) -> SaleState {
        SaleState {
            creator: CREATOR_ID,
            token_vault_id: TOKEN_VAULT_ID,
            collateral_vault_id: COLLATERAL_VAULT_ID,
            treasury_id: TREASURY_ID,
            real_collateral: RAISED,
            treasury_owed: owed,
            status,
            ..Default::default()
        }
    }

    #[test]
    fn sweep_pays_exactly_the_owed_amount_and_zeroes_the_bucket() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let (post_states, calls) = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED + OWED),
            treasury(),
            BC_PROGRAM_ID,
        );

        assert_eq!(calls.len(), 1, "the sweep is exactly one transfer");
        let leg = &calls[0];
        assert_eq!(leg.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(leg.pda_seeds, vec![compute_collateral_vault_pda_seed(SALE_ID)]);
        assert!(leg.pre_states[0].is_authorized, "the vault must be authorized to send");
        assert_eq!(leg.pre_states[0].account_id, COLLATERAL_VAULT_ID);
        assert_eq!(leg.pre_states[1].account_id, TREASURY_ID);
        assert_eq!(
            leg.instruction_data,
            expected_transfer_data(OWED),
            "exactly the escrowed fee moves - not the raise sitting beside it"
        );

        let post = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, 0, "the bucket is cleared by what moved");
        assert_eq!(post.real_collateral, RAISED, "the creator's share is untouched");
        assert_eq!(post_states.len(), 3, "one post-state per declared account");
    }

    /// The escrow accrues while the sale runs, so settlement must not wait on the
    /// creator to close - and it moves only `treasury_owed` either way.
    #[test]
    fn sweep_works_while_the_sale_is_open() {
        let state = sale_state(SaleStatus::Open, OWED);
        let (post_states, calls) = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED + OWED),
            treasury(),
            BC_PROGRAM_ID,
        );
        assert_eq!(calls[0].instruction_data, expected_transfer_data(OWED));
        let post = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert!(matches!(post.status, SaleStatus::Open), "the sweep does not close the sale");
        assert_eq!(post.treasury_owed, 0);
    }

    /// Dispatch is anchored to the VAULT's program owner. The sweep is
    /// permissionless, so reading the id off the submitter-supplied treasury would
    /// let anyone route the vault PDA's authority into a program of their
    /// choosing - the critical bug the buy paths already fixed.
    #[test]
    fn sweep_dispatches_on_the_vault_program_not_the_treasury() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let mut hostile_treasury = treasury();
        hostile_treasury.account.program_owner = [42u32; 8];
        let (_, calls) = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED + OWED),
            hostile_treasury,
            BC_PROGRAM_ID,
        );
        assert_eq!(
            calls[0].program_id, TOKEN_PROGRAM_ID,
            "must dispatch on the collateral vault's token program"
        );
    }

    /// A vault short of the escrow pays what it has and keeps the rest owed: the
    /// bucket is never cleared by more than actually moved, so a later sweep can
    /// finish the job.
    #[test]
    fn sweep_clamps_to_the_vault_balance_and_keeps_the_remainder_owed() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let (post_states, calls) = sweep_treasury(
            sale_account(&state),
            collateral_vault(20_000),
            treasury(),
            BC_PROGRAM_ID,
        );
        assert_eq!(calls[0].instruction_data, expected_transfer_data(20_000));
        let post = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, OWED - 20_000, "the unpaid remainder stays claimable");
    }

    /// Nothing owed means nothing to do: reverting keeps a repeated sweep from
    /// emitting a zero-value transfer (and makes a double sweep a no-op failure
    /// rather than a second payout).
    #[test]
    #[should_panic(expected = "nothing owed to the treasury")]
    fn sweep_reverts_when_nothing_is_owed() {
        let state = sale_state(SaleStatus::Closed, 0);
        let _ = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED),
            treasury(),
            BC_PROGRAM_ID,
        );
    }

    /// The submitter picks every non-signer account id, and this instruction has
    /// no signer at all, so the pins against the sale state are the whole
    /// security model.
    #[test]
    #[should_panic(expected = "wrong treasury account")]
    fn sweep_rejects_a_substituted_treasury() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let _ = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED + OWED),
            holding(AccountId::new([98u8; 32]), COLLATERAL_DEF_ID, 0),
            BC_PROGRAM_ID,
        );
    }

    #[test]
    #[should_panic(expected = "wrong collateral vault")]
    fn sweep_rejects_a_substituted_collateral_vault() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let _ = sweep_treasury(
            sale_account(&state),
            holding(AccountId::new([97u8; 32]), COLLATERAL_DEF_ID, RAISED + OWED),
            treasury(),
            BC_PROGRAM_ID,
        );
    }

    /// The bootstrap remedy this used to assert is gone. `CreateSale` now pins an
    /// initialised treasury, so an unowned one at sweep time means chain state
    /// disagrees with the pin - and signing for yourself must NOT paper over that,
    /// because the account it would claim is not the treasury the sale committed to.
    #[test]
    #[should_panic(expected = "disagrees with chain state")]
    fn sweep_rejects_an_unowned_treasury_even_when_it_signs_for_itself() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let mut fresh_treasury = uninitialized(TREASURY_ID);
        fresh_treasury.is_authorized = true;
        let _ = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED + OWED),
            fresh_treasury,
            BC_PROGRAM_ID,
        );
    }

    /// A claim needs a pristine account, and LEZ bumps every signer's nonce - so
    /// an unowned treasury whose key has signed before can never be claimed by any
    /// program, and therefore can never receive a token. The escrow pointed at it
    /// is lost; say so rather than sending the operator back to re-sign.
    #[test]
    #[should_panic(expected = "disagrees with chain state")]
    fn sweep_rejects_an_unowned_treasury_that_can_never_be_claimed() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let mut stale_treasury = uninitialized(TREASURY_ID);
        stale_treasury.account.nonce = Nonce(1); // the key has signed at least once
        stale_treasury.is_authorized = true; // and even signs this sweep
        let _ = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED + OWED),
            stale_treasury,
            BC_PROGRAM_ID,
        );
    }

    /// Without that signature the transfer dies as `ClaimedUnauthorizedAccount`,
    /// deep in the token program and naming no remedy. Fail here instead, where
    /// the message can say what to re-submit.
    #[test]
    #[should_panic(expected = "disagrees with chain state")]
    fn sweep_rejects_an_uninitialised_treasury_nobody_authorised() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let _ = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED + OWED),
            uninitialized(TREASURY_ID),
            BC_PROGRAM_ID,
        );
    }

    /// Forwarded, never forced: an ordinary sweep is unsigned, and marking an
    /// account that is not in the signer set is `InvalidAccountAuthorization` -
    /// which would reject every permissionless sweep ever submitted.
    #[test]
    fn sweep_does_not_fabricate_authorization_for_the_treasury() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let (_, calls) = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED + OWED),
            treasury(), // initialised, unsigned - the normal case
            BC_PROGRAM_ID,
        );
        assert!(
            !calls[0].pre_states[1].is_authorized,
            "an unsigned treasury slot must stay unauthorized in the fee leg"
        );
    }

    /// A sale account owned by someone else could carry forged state, including a
    /// `treasury_owed` big enough to drain a vault it names.
    #[test]
    #[should_panic(expected = "must be owned by the bonding-curve program")]
    fn sweep_rejects_a_foreign_sale_account() {
        let state = sale_state(SaleStatus::Closed, OWED);
        let _ = sweep_treasury(
            sale_account(&state),
            collateral_vault(RAISED + OWED),
            treasury(),
            [77u32; 8], // some other program's id
        );
    }
}

/// `CreateSale` tests: the treasury-aliasing rejections, and the pin that ties a
/// sale to the token program owning the project token it advertises.
///
/// `treasury_id` arrives as a bare `AccountId` with no account slot behind it, so
/// creation is the only place an alias can be rejected - and an alias is
/// unrecoverable once fees have accrued: `SweepTreasury` names `[sale,
/// collateral_vault, treasury]`, all three pinned from the sale state, and LEZ
/// rejects a message whose account ids repeat.
mod create_sale_tests {
    use bonding_curve_core::{
        compute_collateral_vault_pda, compute_creator_index_pda, compute_creator_index_pda_seed,
        compute_sale_pda, compute_token_vault_pda, CreatorIndex, SaleState,
    };
    use lee_core::{
        account::{Account, AccountId, AccountWithMetadata, Data, Nonce},
        program::{AccountPostState, ChainedCall, Claim, ProgramId},
    };

    use super::fixtures::*;
    use crate::create_sale::create_sale;

    const NONCE: u64 = 7;
    /// Any non-default id: `create_sale` only rejects the default one.
    const ATA_PROGRAM_ID: ProgramId = [3u32; 8];
    const SALE_QUANTITY: u128 = 1_000_000_000;
    const DEX_SEED_QUANTITY: u128 = 100_000_000;

    /// `(sale, token_vault, collateral_vault)` PDAs for the fixture's parameters.
    /// Real derivations, because `create_sale` asserts every one of them before it
    /// ever reaches the treasury check.
    fn pdas() -> (AccountId, AccountId, AccountId) {
        let sale_id = compute_sale_pda(
            BC_PROGRAM_ID,
            TOKEN_DEF_ID,
            COLLATERAL_DEF_ID,
            CREATOR_ID,
            NONCE,
        );
        (
            sale_id,
            compute_token_vault_pda(BC_PROGRAM_ID, sale_id),
            compute_collateral_vault_pda(BC_PROGRAM_ID, sale_id),
        )
    }

    /// A token DEFINITION account. `create_sale` reads its id and its owning token
    /// program - which is what each chained call is dispatched on - and asserts the
    /// data really parses as a `TokenDefinition`, so this carries valid bytes.
    /// Use [`definition_with_data`] to build one that does not parse.
    fn definition(account_id: AccountId, program_owner: ProgramId) -> AccountWithMetadata {
        definition_with_data(
            account_id,
            program_owner,
            Data::from(&token_core::TokenDefinition::Fungible {
                name: "TEST".to_string(),
                total_supply: 1_000_000_000u128,
                metadata_id: None,
            }),
        )
    }

    /// The same, with caller-chosen `data` - for the negative cases where the
    /// account is not a token definition at all.
    fn definition_with_data(
        account_id: AccountId,
        program_owner: ProgramId,
        data: Data,
    ) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account {
                program_owner,
                balance: 0u128,
                data,
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id,
        }
    }

    /// A well-formed NonFungible definition - parses fine, which is exactly why
    /// `is_ok()` was not enough and the asserts match on the variant.
    fn nft_definition(account_id: AccountId, program_owner: ProgramId) -> AccountWithMetadata {
        definition_with_data(
            account_id,
            program_owner,
            Data::from(&token_core::TokenDefinition::NonFungible {
                name: "TEST-NFT".to_string(),
                printable_supply: 100u128,
                metadata_id: AccountId::new([44u8; 32]),
            }),
        )
    }

    /// The two definitions deliberately live under DIFFERENT token programs -
    /// legal, and the only way to tell which account a dispatch id was read off.
    fn token_definition() -> AccountWithMetadata {
        definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID)
    }

    fn collateral_definition() -> AccountWithMetadata {
        definition(COLLATERAL_DEF_ID, COLLATERAL_PROGRAM_ID)
    }

    /// A treasury that satisfies the pin: an already-initialized Fungible holding
    /// of the COLLATERAL definition, owned by the same token program that owns
    /// that definition. Note it is NOT `fixtures::treasury()`, which sits under
    /// `TOKEN_PROGRAM_ID` - the collateral definition here lives under
    /// `COLLATERAL_PROGRAM_ID`, and that difference is the point of the last of
    /// the three treasury asserts.
    fn treasury_holding(account_id: AccountId) -> AccountWithMetadata {
        holding_owned_by(account_id, COLLATERAL_DEF_ID, 0, COLLATERAL_PROGRAM_ID)
    }

    fn creator_holding() -> AccountWithMetadata {
        holding(
            CREATOR_TOKEN_HOLDING_ID,
            TOKEN_DEF_ID,
            SALE_QUANTITY + DEX_SEED_QUANTITY,
        )
    }

    fn run(treasury_id: AccountId) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        run_with(token_definition(), creator_holding(), treasury_id)
    }

    fn run_with(
        token_definition: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        treasury_id: AccountId,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        run_with_defs(
            token_definition,
            collateral_definition(),
            creator_token_holding,
            treasury_id,
        )
    }

    /// [`run_with`], but the collateral definition account can be varied too.
    /// The treasury account is the canonical one for `treasury_id`; use
    /// [`run_with_treasury`] to vary it.
    fn run_with_defs(
        token_definition: AccountWithMetadata,
        collateral_definition: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        treasury_id: AccountId,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        run_all(
            token_definition,
            collateral_definition,
            creator_token_holding,
            treasury_holding(treasury_id),
            treasury_id,
        )
    }

    /// Everything canonical except the treasury ACCOUNT, which is what the
    /// treasury pin inspects. `treasury_id` stays `TREASURY_ID` - i.e. the id
    /// declared in the instruction - so a mismatched `account_id` is testable.
    fn run_with_treasury(
        treasury: AccountWithMetadata,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        run_all(
            token_definition(),
            collateral_definition(),
            creator_holding(),
            treasury,
            TREASURY_ID,
        )
    }

    /// The creator's index PDA - the account the sale id gets appended to.
    fn creator_index_id() -> AccountId {
        compute_creator_index_pda(BC_PROGRAM_ID, CREATOR_ID)
    }

    /// An index account already claimed by this program, holding `sale_ids` for
    /// `creator`. The shape `create_sale` sees on a creator's second sale.
    fn creator_index_holding(
        account_id: AccountId,
        program_owner: ProgramId,
        creator: AccountId,
        sale_ids: &[AccountId],
    ) -> AccountWithMetadata {
        let mut index = CreatorIndex::new(creator);
        for id in sale_ids {
            index.push(*id);
        }
        AccountWithMetadata {
            account: Account {
                program_owner,
                balance: 0u128,
                data: Data::from(&index),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id,
        }
    }

    /// The one call site of `create_sale`. Everything above funnels here with an
    /// UNCLAIMED index (the first-sale case); [`run_with_index`] varies it.
    fn run_all(
        token_definition: AccountWithMetadata,
        collateral_definition: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        treasury: AccountWithMetadata,
        treasury_id: AccountId,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        run_everything(
            token_definition,
            collateral_definition,
            creator_token_holding,
            treasury,
            treasury_id,
            uninitialized(creator_index_id()),
        )
    }

    /// Everything canonical except the creator-index ACCOUNT.
    fn run_with_index(
        creator_index: AccountWithMetadata,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        run_everything(
            token_definition(),
            collateral_definition(),
            creator_holding(),
            treasury_holding(TREASURY_ID),
            TREASURY_ID,
            creator_index,
        )
    }

    fn run_everything(
        token_definition: AccountWithMetadata,
        collateral_definition: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        treasury: AccountWithMetadata,
        treasury_id: AccountId,
        creator_index: AccountWithMetadata,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        let (sale_id, token_vault_id, collateral_vault_id) = pdas();
        create_sale(
            uninitialized(sale_id),
            uninitialized(token_vault_id),
            uninitialized(collateral_vault_id),
            treasury,
            token_definition,
            collateral_definition,
            creator_token_holding,
            creator_account(true, CREATOR_ID),
            creator_index,
            COLLATERAL_DEF_ID,
            treasury_id,
            "Sample Token".to_string(),
            "SMPL".to_string(),
            SALE_QUANTITY,
            DEX_SEED_QUANTITY,
            1_100_000_000, // Vt
            30_000_000,    // Vc
            100,           // fee_bps (1%)
            false,         // one_directional
            0,             // end_timestamp_ms: no timeout, so the clock is not consulted
            0,             // min_duration_ms
            NONCE,
            ATA_PROGRAM_ID,
            BC_PROGRAM_ID,
            0, // clock_ts
        )
    }

    #[test]
    fn create_sale_accepts_a_standalone_treasury() {
        let (post_states, calls) = run(TREASURY_ID);
        let state = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(state.treasury_id, TREASURY_ID);
        assert_eq!(state.treasury_owed, 0, "a fresh sale owes the treasury nothing");
        assert_eq!(calls.len(), 2, "D+R deposit, then the collateral-vault init");
        // The guest pushes the clock on top of these, for ten in all. The order
        // is the on-chain ABI, so it is pinned here rather than left to the SDK.
        assert_eq!(
            post_states.len(),
            9,
            "[sale, token_vault, collateral_vault, treasury, token_definition, \
             collateral_definition, creator_token_holding, creator, creator_index]"
        );
        assert_eq!(
            post_states[3].account(),
            &treasury_holding(TREASURY_ID).account,
            "the treasury is read-only here and must be echoed byte-for-byte"
        );
    }

    /// The "just send the fees to me" setup. The creator slot is an identity
    /// signer, not a holding of the collateral definition, so every fee leg and
    /// every sweep would revert against it - and it is the configuration that
    /// bricked the seven-account `Withdraw` outright.
    #[test]
    #[should_panic(expected = "treasury must not alias")]
    fn create_sale_rejects_a_treasury_aliasing_the_creator() {
        let _ = run(CREATOR_ID);
    }

    #[test]
    #[should_panic(expected = "treasury must not alias")]
    fn create_sale_rejects_a_treasury_aliasing_the_sale_pda() {
        let _ = run(pdas().0);
    }

    #[test]
    #[should_panic(expected = "treasury must not alias")]
    fn create_sale_rejects_a_treasury_aliasing_the_token_vault() {
        let _ = run(pdas().1);
    }

    #[test]
    #[should_panic(expected = "treasury must not alias")]
    fn create_sale_rejects_a_treasury_aliasing_the_collateral_vault() {
        let _ = run(pdas().2);
    }

    /// Each leg is dispatched on the DEFINITION of the token it moves. The two
    /// definitions sit under different token programs here, so a deposit leg read
    /// off the creator's holding - or off the collateral side - shows up as the
    /// wrong id.
    #[test]
    fn create_sale_dispatches_each_leg_on_its_own_token_definition() {
        let (_, calls) = run(TREASURY_ID);
        assert_eq!(calls.len(), 2, "D+R deposit, then the collateral-vault init");
        assert_eq!(
            calls[0].program_id, TOKEN_PROGRAM_ID,
            "the D+R deposit must run on the program owning the PROJECT token definition"
        );
        assert_eq!(
            calls[1].program_id, COLLATERAL_PROGRAM_ID,
            "the vault init must run on the program owning the COLLATERAL definition"
        );
    }

    /// The sale's `token_definition_id` is read out of the creator's holding, so
    /// without this pin the advertised token and the program that actually moves
    /// it are unrelated: a creator could name a real, valuable definition and hand
    /// over a holding owned by a token program of their own, which then claims the
    /// token vault and mints buyers a holding that only *reads* as valuable.
    #[test]
    #[should_panic(expected = "token definition account does not match")]
    fn create_sale_rejects_a_token_definition_account_it_did_not_declare() {
        let _ = run_with(
            definition(AccountId::new([77u8; 32]), TOKEN_PROGRAM_ID),
            creator_holding(),
            TREASURY_ID,
        );
    }

    /// Nothing downstream parses the project-token definition: the deposit leg is
    /// a `Transfer` that never names this account, so an uninitialized or
    /// wrong-kind account would surface only as LEZ's opaque "Unknown program"
    /// rejection of a chained call whose program id was read off that same bad
    /// account. (The collateral side is parsed for free by its `InitializeAccount`
    /// leg - hence the asymmetry this test and the next one close.)
    #[test]
    #[should_panic(expected = "project token definition account does not hold a FUNGIBLE token")]
    fn create_sale_rejects_a_project_token_definition_that_is_not_a_definition() {
        let _ = run_with(
            definition_with_data(TOKEN_DEF_ID, TOKEN_PROGRAM_ID, Data::default()),
            creator_holding(),
            TREASURY_ID,
        );
    }

    /// The collateral twin. `InitializeAccount` would also reject this, but only
    /// from inside the token program and without naming the sale or the account -
    /// and the two launchpads are documented as identical in shape here.
    #[test]
    #[should_panic(expected = "collateral definition account does not hold a FUNGIBLE token")]
    fn create_sale_rejects_a_collateral_definition_that_is_not_a_definition() {
        let _ = run_with_defs(
            token_definition(),
            definition_with_data(COLLATERAL_DEF_ID, COLLATERAL_PROGRAM_ID, Data::default()),
            creator_holding(),
            TREASURY_ID,
        );
    }

    /// The other side of the same pin: the deposit's sender must belong to the
    /// program the deposit is dispatched on. (LEZ would also reject the chained
    /// call - a foreign sender's data cannot be modified by the executing program
    /// - but only from deep inside it, with no mention of the sale.)
    #[test]
    #[should_panic(expected = "not owned by the project token definition's own token program")]
    fn create_sale_rejects_a_creator_holding_under_a_foreign_token_program() {
        let _ = run_with(
            token_definition(),
            holding_owned_by(
                CREATOR_TOKEN_HOLDING_ID,
                TOKEN_DEF_ID,
                SALE_QUANTITY + DEX_SEED_QUANTITY,
                EVIL_PROGRAM_ID,
            ),
            TREASURY_ID,
        );
    }

    // ---- NonFungible definitions ----------------------------------------
    //
    // Both parse as a `TokenDefinition`, so both of these passed the old
    // `is_ok()` asserts. An NFT definition types its holdings as
    // `NftPrintedCopy`, which every `read_fungible` in this program panics on, so
    // a sale built on one takes the creator's `D + R` deposit and then has no
    // instruction that can give it back.

    #[test]
    #[should_panic(expected = "project token definition account does not hold a FUNGIBLE token")]
    fn create_sale_rejects_a_non_fungible_project_token_definition() {
        let _ = run_with(
            nft_definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            creator_holding(),
            TREASURY_ID,
        );
    }

    /// The collateral twin, and the reason this assert survives the treasury pin:
    /// no Fungible treasury can name a NonFungible definition, so the treasury
    /// check would reject this sale too - but with a message about the TREASURY,
    /// when the creator's mistake was the collateral definition they named.
    #[test]
    #[should_panic(expected = "collateral definition account does not hold a FUNGIBLE token")]
    fn create_sale_rejects_a_non_fungible_collateral_definition() {
        let _ = run_with_defs(
            token_definition(),
            nft_definition(COLLATERAL_DEF_ID, COLLATERAL_PROGRAM_ID),
            creator_holding(),
            TREASURY_ID,
        );
    }

    // ---- the treasury pin ------------------------------------------------
    //
    // `SweepTreasury` is the only instruction that pays out `treasury_owed`, and
    // `Withdraw` subtracts that bucket from the creator's payout, so a treasury
    // the fee leg can never reach strands the escrow permanently. Each shape
    // below was reachable while the treasury was only an `AccountId`.

    /// The declared `treasury_id` is what gets pinned into `SaleState` and what
    /// every later instruction validates against, so type-checking some *other*
    /// account would check nothing at all.
    #[test]
    #[should_panic(expected = "does not match the treasury_id declared in the instruction")]
    fn create_sale_rejects_a_treasury_account_that_is_not_the_declared_id() {
        let _ = run_with_treasury(treasury_holding(AccountId::new([99u8; 32])));
    }

    /// The unsettleable shape, and the reason this check exists. Settling into an
    /// uninitialised treasury has to CLAIM it, and `Claim::Authorized` is
    /// admitted only for an account that is itself authorized - so only the
    /// treasury's own signature could ever do it, and that same key bricks the id
    /// permanently by signing anything else first (a claim needs a pre-state that
    /// is `Account::default()` WHOLE, and LEZ bumps a signer's nonce even when
    /// its account is unowned). No stranger can reach the state: LEZ refuses any
    /// transaction that leaves a DEFAULT-owned account modified but unclaimed.
    #[test]
    #[should_panic(expected = "treasury account does not exist yet")]
    fn create_sale_rejects_an_uninitialised_treasury() {
        let _ = run_with_treasury(uninitialized(TREASURY_ID));
    }

    /// The fee leg moves the collateral vault's token; a treasury holding
    /// something else reverts on every sweep, forever.
    #[test]
    #[should_panic(expected = "holds a different token than the sale's collateral")]
    fn create_sale_rejects_a_treasury_holding_the_wrong_definition() {
        let _ = run_with_treasury(holding_owned_by(
            TREASURY_ID,
            TOKEN_DEF_ID,
            0,
            COLLATERAL_PROGRAM_ID,
        ));
    }

    /// The subtle one, and the reason the program-owner assert is not redundant:
    /// the definition id lives in DATA, which the owning token program wrote, so
    /// a program the creator deployed can mint a holding that reads as a perfect
    /// collateral treasury. The fee leg is dispatched on the collateral vault's
    /// program, so it can never pay this account either.
    #[test]
    #[should_panic(expected = "not owned by the collateral definition's own token program")]
    fn create_sale_rejects_a_treasury_under_a_foreign_token_program() {
        let _ = run_with_treasury(holding_owned_by(
            TREASURY_ID,
            COLLATERAL_DEF_ID,
            0,
            EVIL_PROGRAM_ID,
        ));
    }

    /// An NFT holding of the collateral definition is unconstructible in practice
    /// (the definition is Fungible), but `read_fungible` is what rules out every
    /// non-Fungible `TokenHolding` variant, so the treasury goes through it.
    #[test]
    #[should_panic(expected = "CreateSale: treasury: expected a Fungible Token Holding")]
    fn create_sale_rejects_a_treasury_that_is_not_a_fungible_holding() {
        let mut treasury = treasury_holding(TREASURY_ID);
        treasury.account.data = Data::from(&token_core::TokenHolding::NftPrintedCopy {
            definition_id: COLLATERAL_DEF_ID,
            owned: true,
        });
        let _ = run_with_treasury(treasury);
    }
    // ---- the per-creator sale index --------------------------------------
    //
    // The index is what turns `my-sales` from ~4,800 speculative PDA reads into
    // one read per creator, so these pin both halves: that the entry actually
    // lands, and that only the signing creator's own index can receive it.

    /// First sale: the PDA is unclaimed, so `create_sale` must claim it - with
    /// the index seed, since LEZ checks a `Claim::Pda` against the claiming
    /// program's own derivation - and seed it with this sale's id.
    #[test]
    fn create_sale_claims_the_creator_index_on_the_first_sale() {
        let (post_states, _) = run(TREASURY_ID);
        let index_post = &post_states[8];
        assert_eq!(
            index_post.required_claim(),
            Some(Claim::Pda(compute_creator_index_pda_seed(CREATOR_ID))),
            "an unclaimed index PDA must be claimed here, with its own seed"
        );
        assert_eq!(
            index_post.account().program_owner,
            [0u32; 8],
            "the claim leaves the owner default - LEZ sets it, and rule 4 rejects a \
             program that sets it itself"
        );
        let index = CreatorIndex::try_from(&index_post.account().data)
            .expect("the claimed account must hold a decodable creator index");
        assert_eq!(index.creator, CREATOR_ID);
        assert_eq!(index.sale_ids, vec![pdas().0], "the new sale id must be recorded");
    }

    /// Second sale: the account already belongs to this program, so the id is
    /// APPENDED and nothing is re-claimed (LEZ rejects a claim on an account that
    /// is no longer default-owned).
    #[test]
    fn create_sale_appends_to_an_existing_creator_index() {
        let earlier = AccountId::new([55u8; 32]);
        let (post_states, _) = run_with_index(creator_index_holding(
            creator_index_id(),
            BC_PROGRAM_ID,
            CREATOR_ID,
            &[earlier],
        ));
        let index_post = &post_states[8];
        assert_eq!(
            index_post.required_claim(),
            None,
            "an index this program already owns must not be re-claimed"
        );
        let index = CreatorIndex::try_from(&index_post.account().data).unwrap();
        assert_eq!(
            index.sale_ids,
            vec![earlier, pdas().0],
            "the append must keep the earlier sale and add the new one, in order"
        );
    }

    /// The pin that makes the index un-spoofable. The submitter chooses every
    /// non-signer account id, so without it a creator could append their sale
    /// into someone else's index - or, passing a stranger's index that this
    /// program owns, mutate an account that is not theirs.
    #[test]
    #[should_panic(expected = "not the index PDA of the signing creator")]
    fn create_sale_rejects_an_index_that_is_not_the_creators_pda() {
        let other = compute_creator_index_pda(BC_PROGRAM_ID, AccountId::new([42u8; 32]));
        let _ = run_with_index(creator_index_holding(other, BC_PROGRAM_ID, CREATOR_ID, &[]));
    }

    /// Same pin from the other direction: the right ACCOUNT ID cannot be paired
    /// with someone else's list. Unreachable through the PDA (the id is derived
    /// from the creator), which is exactly why the assert is worth having - a
    /// later seed change must not silently merge two creators' histories.
    #[test]
    #[should_panic(expected = "belongs to a different creator")]
    fn create_sale_rejects_an_index_recorded_against_another_creator() {
        let _ = run_with_index(creator_index_holding(
            creator_index_id(),
            BC_PROGRAM_ID,
            AccountId::new([42u8; 32]),
            &[],
        ));
    }

    /// The index PDA is non-default and still DEFAULT-owned - here, carrying a
    /// native balance and nothing else.
    ///
    /// Nobody can actually produce this. LEZ rejects any transaction that
    /// modifies a DEFAULT-owned account without claiming it, `Claim::Pda` is
    /// admitted only against the CLAIMING program's own derivation, and
    /// `Claim::Authorized` needs the account itself authorized - which for a
    /// keyless PDA means a `pda_seeds` delegation this program never issues for
    /// the index seed. A balance is stronger still: after genesis no transaction
    /// puts one on a DEFAULT-owned account at all. So this is NOT the dusting
    /// hazard two comments in this file used to claim; the assert is defence in
    /// depth, and what this test pins is that the guard holds and that its
    /// message describes an impossible state rather than telling the creator to
    /// abandon a working identity. Mirrors
    /// `lbp::create_sale_rejects_a_creator_index_that_is_unowned_and_non_default`.
    #[test]
    #[should_panic(expected = "exists but is not owned by this program")]
    fn create_sale_rejects_a_creator_index_that_is_unowned_and_non_default() {
        let _ = run_with_index(AccountWithMetadata {
            account: Account { balance: 1u128, ..Account::default() },
            is_authorized: false,
            account_id: creator_index_id(),
        });
    }

    /// Same assert, the other arm: an existing index account must be one of ours.
    /// A foreign-owned account at this id could not be written by this program
    /// anyway (LEZ rejects the data modification), but the rejection would come
    /// from the framework with no mention of the index.
    #[test]
    #[should_panic(expected = "not owned by this program")]
    fn create_sale_rejects_a_foreign_owned_creator_index() {
        let _ = run_with_index(creator_index_holding(
            creator_index_id(),
            EVIL_PROGRAM_ID,
            CREATOR_ID,
            &[],
        ));
    }

    /// ...and it must really hold an index. This is the discriminant doing its
    /// job: without the magic check in `CreatorIndex::try_from`, an account whose
    /// bytes happen to Borsh-decode would be appended to and rewritten.
    #[test]
    #[should_panic(expected = "does not hold a bonding-curve creator index")]
    fn create_sale_rejects_an_index_account_holding_something_else() {
        let mut acct = creator_index_holding(creator_index_id(), BC_PROGRAM_ID, CREATOR_ID, &[]);
        let mut bytes: Vec<u8> = acct.account.data.to_vec();
        bytes[0] ^= 0xff; // corrupt only the magic
        acct.account.data = Data::try_from(bytes).unwrap();
        let _ = run_with_index(acct);
    }

    /// The cap has to be reachable through `create_sale`, not just through
    /// `CreatorIndex::push`: past it the encode would panic with a
    /// `DataTooBigError` naming nothing, on every sale this creator ever tries.
    #[test]
    #[should_panic(expected = "creator sale index is full")]
    fn create_sale_rejects_an_index_that_is_already_full() {
        let full: Vec<AccountId> = (0..bonding_curve_core::MAX_INDEXED_SALES)
            .map(|i| AccountId::new([u8::try_from(i % 251).unwrap(); 32]))
            .collect();
        let _ = run_with_index(creator_index_holding(
            creator_index_id(),
            BC_PROGRAM_ID,
            CREATOR_ID,
            &full,
        ));
    }
}

/// `BuyDisposable` tests. The disposable path is the one with no treasury and no
/// clock account, so what is asserted here is what replaces them: the fee escrow,
/// the single combined collateral leg, and the vault-balance invariant that ties
/// those two together.
mod disposable_tests {
    use bonding_curve_core::{
        buy_tokens_out, compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, SaleState,
        SaleStatus,
    };
    use lee_core::{
        account::AccountId,
        program::{AccountPostState, ChainedCall},
    };

    use super::fixtures::*;
    use crate::buy::buy_disposable;
    use crate::util::authorized;

    const COLLATERAL_IN: u128 = 1_000_000;
    /// Collateral raised before the test's buy, matched by the vault balance so
    /// the fixtures start out satisfying the vault invariant.
    const RAISED: u128 = 4_000_000;

    fn open_sale() -> SaleState {
        SaleState {
            creator: CREATOR_ID,
            token_definition_id: TOKEN_DEF_ID,
            collateral_definition_id: COLLATERAL_DEF_ID,
            token_vault_id: TOKEN_VAULT_ID,
            collateral_vault_id: COLLATERAL_VAULT_ID,
            treasury_id: TREASURY_ID,
            fee_bps: 100, // 1%
            virt_token: 1_100_000_000,
            virt_collateral: 30_000_000,
            k: 1_100_000_000u128 * 30_000_000u128,
            sale_reserve_initial: 1_000_000_000,
            sale_reserve: 1_000_000_000,
            real_collateral: RAISED,
            status: SaleStatus::Open,
            ..Default::default()
        }
    }

    fn run_with(
        state: &SaleState,
        vault_balance: u128,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        buy_disposable(
            sale_account(state),
            token_vault(1_000_000_000),
            collateral_vault(vault_balance),
            // The buyer's holdings are private slots on-chain; from the program's
            // point of view they are ordinary authorized/plain accounts.
            authorized(&holding(BUYER_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_IN)),
            holding(BUYER_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0),
            COLLATERAL_IN,
            0,
            BC_PROGRAM_ID,
        )
    }

    fn run(state: &SaleState) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        run_with(state, RAISED)
    }

    #[test]
    fn buy_disposable_escrows_the_fee_and_moves_collateral_in_one_leg() {
        let state = open_sale();
        let (post_states, calls) = run(&state);
        let (tokens_out, fee, c_eff) =
            buy_tokens_out(state.virt_token, state.virt_collateral, state.fee_bps, COLLATERAL_IN);
        assert!(fee > 0, "test must exercise a non-zero fee");

        // Exactly two legs. The public path needs three because it sweeps the fee
        // to the treasury; here the fee rides along in the one collateral leg.
        assert_eq!(calls.len(), 2, "expected one collateral + one token transfer");
        assert_eq!(c_eff + fee, COLLATERAL_IN, "the collateral leg is exactly c_eff + fee");

        let coll = &calls[0];
        assert_eq!(coll.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(coll.pda_seeds, vec![compute_collateral_vault_pda_seed(SALE_ID)]);
        assert_eq!(coll.pre_states[0].account_id, BUYER_COLL_HOLDING_ID);
        assert_eq!(coll.pre_states[1].account_id, COLLATERAL_VAULT_ID);
        assert!(coll.pre_states[1].is_authorized, "collateral vault must be authorized");
        assert_eq!(coll.instruction_data, expected_transfer_data(COLLATERAL_IN));

        let tok = &calls[1];
        assert_eq!(tok.pda_seeds, vec![compute_token_vault_pda_seed(SALE_ID)]);
        assert!(tok.pre_states[0].is_authorized, "token vault must be authorized");
        assert_eq!(tok.pre_states[0].account_id, TOKEN_VAULT_ID);
        assert_eq!(tok.pre_states[1].account_id, BUYER_TOKEN_HOLDING_ID);
        assert_eq!(tok.instruction_data, expected_transfer_data(tokens_out));

        let post = SaleState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, fee, "the fee is escrowed, not sent");
        assert_eq!(post.real_collateral, RAISED + c_eff, "only c_eff is the creator's");
        assert_eq!(post.cum_fees, fee);
        assert_eq!(post_states.len(), 5, "one post-state per declared account");
        assert!(
            post.obs.is_empty(),
            "no clock account, so no timestamped observation is recorded"
        );
    }

    /// INVARIANT: `collateral_vault_balance == real_collateral + treasury_owed`.
    /// This is what makes the escrow safe: `Withdraw` pays the creator the vault
    /// minus the bucket, so if the two ever drifted apart one party would be paid
    /// out of the other's funds. Checked across repeated buys, since the bucket
    /// accumulates.
    #[test]
    fn vault_balance_equals_real_collateral_plus_treasury_owed() {
        let mut state = open_sale();
        let mut vault = RAISED;
        assert_eq!(vault, state.real_collateral + state.treasury_owed, "fixture starts balanced");

        for i in 0..3 {
            let (post_states, _) = run_with(&state, vault);
            // The single collateral leg credits the vault the FULL gross input.
            vault += COLLATERAL_IN;
            state = SaleState::try_from(&post_states[0].account().data).unwrap();
            assert_eq!(
                vault,
                state.real_collateral + state.treasury_owed,
                "vault balance must equal real_collateral + treasury_owed after buy {i}"
            );
        }
        assert!(state.treasury_owed > 0, "the escrow bucket must have accumulated");
    }

    /// The disposable path passes `None` for the clock, so a past
    /// `end_timestamp_ms` must NOT revert in-circuit: the guest's validity window
    /// is what refuses inclusion once the sale has ended (see `main.rs`).
    #[test]
    fn buy_disposable_does_not_check_the_end_timestamp_in_circuit() {
        let mut state = open_sale();
        state.end_timestamp_ms = 1; // long past
        let (_, calls) = run(&state);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    #[should_panic(expected = "does not match the sale's collateral definition")]
    fn buy_disposable_rejects_a_foreign_collateral_token() {
        // The first-buyer vault-poisoning guard: without it a zero-fee sale's
        // first buyer could pay a worthless token and drain the token vault.
        let state = open_sale();
        let _ = buy_disposable(
            sale_account(&state),
            token_vault(1_000_000_000),
            collateral_vault(RAISED),
            authorized(&holding(BUYER_COLL_HOLDING_ID, TOKEN_DEF_ID, COLLATERAL_IN)),
            holding(BUYER_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0),
            COLLATERAL_IN,
            0,
            BC_PROGRAM_ID,
        );
    }

    #[test]
    #[should_panic(expected = "buyer collateral holding must be authorized")]
    fn buy_disposable_rejects_an_unauthorized_buyer_holding() {
        let state = open_sale();
        let _ = buy_disposable(
            sale_account(&state),
            token_vault(1_000_000_000),
            collateral_vault(RAISED),
            holding(BUYER_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_IN),
            holding(BUYER_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0),
            COLLATERAL_IN,
            0,
            BC_PROGRAM_ID,
        );
    }

    #[test]
    #[should_panic(expected = "wrong collateral vault")]
    fn buy_disposable_rejects_a_substituted_collateral_vault() {
        // The submitter picks every non-signer account id, so the vault checks
        // carried over from `check_accounts` are load-bearing here too.
        let state = open_sale();
        let _ = buy_disposable(
            sale_account(&state),
            token_vault(1_000_000_000),
            holding(AccountId::new([97u8; 32]), COLLATERAL_DEF_ID, RAISED),
            authorized(&holding(BUYER_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_IN)),
            holding(BUYER_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0),
            COLLATERAL_IN,
            0,
            BC_PROGRAM_ID,
        );
    }

    #[test]
    #[should_panic(expected = "sale is not open")]
    fn buy_disposable_rejects_a_closed_sale() {
        let mut state = open_sale();
        state.status = SaleStatus::Closed;
        let _ = run(&state);
    }
}
