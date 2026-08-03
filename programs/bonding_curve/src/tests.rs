//! Host-logic unit tests for the bonding-curve state machine (pure, no accounts).

use bonding_curve_core::{sell_collateral_out, SaleState, SaleStatus};

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
    // The private path passes None for the clock; end-timestamp is enforced by the
    // SDK's validity window, not in-circuit. apply_buy must not panic on time here.
    let mut s = sample_sale();
    s.end_timestamp_ms = 5_000;
    let out = apply_buy(s, 1_000_000, 0, None);
    assert!(out.tokens_out > 0);
    assert!(out.state.obs.is_empty(), "private path records no timestamped observation");
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

/// Close + withdraw lifecycle tests. Unlike the pure pricing tests above these
/// operate over `AccountWithMetadata` fixtures, exercising the creator-auth,
/// close-gate, closed-status, and (critically) the drained-vault second-withdraw
/// guard that protects the creator's collateral from a double payout.
mod lifecycle_tests {
    use bonding_curve_core::{
        compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, SaleState, SaleStatus,
    };
    use lee_core::{
        account::{Account, AccountId, AccountWithMetadata, Data, Nonce},
        program::{ChainedCall, ProgramId},
    };
    use token_core::TokenHolding;

    use crate::lifecycle::{close_sale, withdraw};

    const BC_PROGRAM_ID: ProgramId = [7u32; 8];
    const TOKEN_PROGRAM_ID: ProgramId = [5u32; 8];

    const CREATOR_ID: AccountId = AccountId::new([1u8; 32]);
    const SALE_ID: AccountId = AccountId::new([2u8; 32]);
    const TOKEN_VAULT_ID: AccountId = AccountId::new([3u8; 32]);
    const COLLATERAL_VAULT_ID: AccountId = AccountId::new([4u8; 32]);
    const TOKEN_DEF_ID: AccountId = AccountId::new([8u8; 32]);
    const COLLATERAL_DEF_ID: AccountId = AccountId::new([9u8; 32]);
    const CREATOR_COLL_HOLDING_ID: AccountId = AccountId::new([10u8; 32]);
    const CREATOR_TOKEN_HOLDING_ID: AccountId = AccountId::new([11u8; 32]);

    fn sale_state(status: SaleStatus) -> SaleState {
        SaleState {
            creator: CREATOR_ID,
            token_vault_id: TOKEN_VAULT_ID,
            collateral_vault_id: COLLATERAL_VAULT_ID,
            real_collateral: 1_000_000,
            dex_seed_reserve: 200,
            sale_reserve: 300,
            end_timestamp_ms: 5_000,
            status,
            ..Default::default()
        }
    }

    fn sale_account(state: &SaleState) -> AccountWithMetadata {
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

    fn creator_account(authorized: bool, id: AccountId) -> AccountWithMetadata {
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

    fn holding(id: AccountId, definition_id: AccountId, balance: u128) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account {
                program_owner: TOKEN_PROGRAM_ID,
                balance: 0u128,
                data: Data::from(&TokenHolding::Fungible { definition_id, balance }),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id: id,
        }
    }

    fn collateral_vault(balance: u128) -> AccountWithMetadata {
        holding(COLLATERAL_VAULT_ID, COLLATERAL_DEF_ID, balance)
    }

    fn token_vault(balance: u128) -> AccountWithMetadata {
        holding(TOKEN_VAULT_ID, TOKEN_DEF_ID, balance)
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

    /// Expected `instruction_data` for a `token::Transfer`, encoded exactly as the
    /// production code does via `ChainedCall::new`, so the test asserts the real
    /// transfer amount rather than just that a call exists.
    fn expected_transfer_data(amount: u128) -> Vec<u32> {
        ChainedCall::new(
            TOKEN_PROGRAM_ID,
            vec![],
            &token_core::Instruction::Transfer { amount_to_transfer: amount },
        )
        .instruction_data
    }

    #[test]
    fn withdraw_happy_path_chains_two_transfers_and_zeroes_accounting() {
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
