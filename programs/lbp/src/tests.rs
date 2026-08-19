//! Host-logic unit tests for the LBP buy state machine (pure, no accounts).

use lbp_core::{PoolState, SaleStatus};

use crate::buy::apply_buy;

fn w(num: u128, den: u128) -> u128 {
    (num << 64) / den
}

fn sample_pool() -> PoolState {
    PoolState {
        fee_bps: 500,
        w_start_q64: w(99, 100),
        w_end_q64: w(1, 100),
        t_start_ms: 1_000,
        t_end_ms: 1_000_000,
        min_duration_ms: 0,
        reserve_token: 1_000_000_000,
        reserve_collateral: 50_000,
        status: SaleStatus::Open,
        ..Default::default()
    }
}

#[test]
fn buy_decreases_token_reserve_and_records() {
    let p = sample_pool();
    let out = apply_buy(p.clone(), 10_000, 0, 2_000, 1, true, true);
    assert!(out.tokens_out > 0);
    assert_eq!(out.state.reserve_token, p.reserve_token - out.tokens_out);
    assert_eq!(out.state.reserve_collateral, p.reserve_collateral + 10_000);
    assert_eq!(out.state.buy_count, 1);
    assert_eq!(out.state.cum_collateral_in, 10_000);
    assert_eq!(out.state.obs.len(), 1);
}

#[test]
fn buy_without_record_obs_leaves_the_ring_empty() {
    // The private path passes `record_obs = false`: `t_ms` is a timestamp the
    // BUYER chose, so recording it would let anyone write arbitrary times into an
    // oracle other contracts may read, and a conspicuously stale observation is
    // itself a fingerprint marking that trade as the private one. Everything else
    // about the transition must be unchanged.
    let p = sample_pool();
    let public = apply_buy(p.clone(), 10_000, 0, 2_000, 1, true, true);
    let private = apply_buy(p, 10_000, 0, 2_000, 0, false, false);
    assert!(private.state.obs.is_empty(), "private buy must record no observation");
    assert_eq!(public.state.obs.len(), 1, "public buy still records one");
    assert_eq!(private.tokens_out, public.tokens_out, "same price either way");
    assert_eq!(private.state.reserve_token, public.state.reserve_token);
    assert_eq!(private.state.reserve_collateral, public.state.reserve_collateral);
    assert_eq!(private.state.buy_count, public.state.buy_count);
}

#[test]
#[should_panic(expected = "slippage")]
fn buy_reverts_on_slippage() {
    let p = sample_pool();
    let _ = apply_buy(p, 10_000, u128::MAX, 2_000, 1, true, true);
}

#[test]
#[should_panic(expected = "not started")]
fn buy_reverts_before_start() {
    let p = sample_pool();
    let _ = apply_buy(p, 10_000, 0, 500, 1, true, true);
}

#[test]
#[should_panic(expected = "ended")]
fn buy_reverts_after_end() {
    let p = sample_pool();
    let _ = apply_buy(p, 10_000, 0, 2_000_000, 1, true, true);
}

#[test]
#[should_panic(expected = "paused")]
fn buy_reverts_when_paused() {
    let mut p = sample_pool();
    p.paused = true;
    let _ = apply_buy(p, 10_000, 0, 2_000, 1, true, true);
}

#[test]
fn same_collateral_buys_more_later_as_price_falls() {
    // Two equal buys at different times against a fresh pool: the later one (lower
    // weight => lower price) should yield strictly more tokens.
    let p = sample_pool();
    let early = apply_buy(p.clone(), 10_000, 0, 100_000, 1, true, true).tokens_out;
    let late = apply_buy(p, 10_000, 0, 900_000, 1, true, true).tokens_out;
    assert!(late > early, "later buy gets more tokens as the LBP price falls: early={early} late={late}");
}

#[test]
fn per_block_ceiling_enforced_and_resets_each_block() {
    let mut p = sample_pool();
    let one_buy = apply_buy(p.clone(), 10_000, 0, 2_000, 1, true, true).tokens_out;
    p.block_token_ceiling = one_buy + 5;
    let s1 = apply_buy(p.clone(), 10_000, 0, 2_000, 1, true, true).state;
    assert_eq!(s1.block_window_id, 1);
    assert!(s1.block_sold <= p.block_token_ceiling);
    // A second buy in the SAME block exceeds the ceiling.
    let res = std::panic::catch_unwind(|| apply_buy(s1.clone(), 10_000, 0, 2_000, 1, true, true));
    assert!(res.is_err(), "second buy in same block must exceed the ceiling");
    // The same buy in a NEW block resets the window and succeeds.
    let s2 = apply_buy(s1, 10_000, 0, 2_000, 2, true, true).state;
    assert_eq!(s2.block_window_id, 2);
}

#[test]
fn fixed_price_mode_is_constant() {
    let mut p = sample_pool();
    p.fixed_price = true;
    p.fixed_price_q64 = (5u128 << 64) / 100; // 0.05 collateral per token
    let early = apply_buy(p.clone(), 10_000, 0, 100_000, 1, true, true).tokens_out;
    let late = apply_buy(p, 10_000, 0, 900_000, 1, true, true).tokens_out;
    assert_eq!(early, late, "fixed-price output independent of time");
    assert_eq!(early, (10_000u128 << 64) / ((5u128 << 64) / 100));
}

/// Poke + close lifecycle tests. Unlike the pure pricing tests above these
/// operate over `AccountWithMetadata` fixtures, exercising poke's stored-weight
/// advance and close_sale's end-timestamp gate, creator-auth asserts, and
/// already-closed guard.
mod lifecycle_tests {
    use lbp_core::{
        compute_weight_obs_pda, weight_token_q64, PoolState, SaleStatus, WeightObs,
    };
    use lee_core::{
        account::{Account, AccountId, AccountWithMetadata, Data, Nonce},
        program::ProgramId,
    };

    use crate::lifecycle::{close_sale, poke, set_paused};

    const LBP_PROGRAM_ID: ProgramId = [7u32; 8];

    const CREATOR_ID: AccountId = AccountId::new([1u8; 32]);
    const POOL_ID: AccountId = AccountId::new([2u8; 32]);

    fn pool_state(status: SaleStatus) -> PoolState {
        PoolState {
            creator: CREATOR_ID,
            fee_bps: 500,
            w_start_q64: super::w(99, 100),
            w_end_q64: super::w(1, 100),
            t_start_ms: 1_000,
            t_end_ms: 1_000_000,
            reserve_token: 1_000_000_000,
            reserve_collateral: 50_000,
            status,
            ..Default::default()
        }
    }

    fn pool_account(state: &PoolState) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account {
                program_owner: LBP_PROGRAM_ID,
                balance: 0u128,
                data: Data::from(state),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id: POOL_ID,
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

    // ---- pause / resume ----
    //
    // These had NO test of any kind - not here, not in the guest, not in the
    // integration suite - which is why they are written out at length. `Pause`
    // and `Resume` are the emergency stop (RFP-016 Functionality #6), and
    // `lbp_core`'s own instruction-set comment names them as the sharp edge of
    // the append-only wire format: they are byte-identical apart from the
    // discriminant and take identical account lists, so swapping the two arms
    // "would resume a paused sale (or pause a live one) with no error anywhere".
    // A hazard documented in a comment and asserted nowhere is a hazard.

    /// The pause flag is the ONLY thing that moves. Asserted field by field
    /// rather than with a blanket equality, because the failure that matters is
    /// a pause that quietly rewrites reserves, weights or the schedule while
    /// the operator believes it only stopped trading.
    #[test]
    fn pause_sets_only_the_pause_flag() {
        let state = pool_state(SaleStatus::Open);
        assert!(!state.paused, "fixture starts live");
        let (post, calls) =
            set_paused(pool_account(&state), creator_account(true, CREATOR_ID), true, LBP_PROGRAM_ID);
        assert!(calls.is_empty(), "pause emits no chained calls");
        let after = PoolState::try_from(&post[0].account().data).expect("pool state");
        assert!(after.paused, "pause must set the flag");
        assert_eq!(after.reserve_token, state.reserve_token, "reserves must not move");
        assert_eq!(after.reserve_collateral, state.reserve_collateral, "reserves must not move");
        assert_eq!(after.status, state.status, "pause is not a close");
        assert_eq!(after.t_start_ms, state.t_start_ms, "schedule must not move");
        assert_eq!(after.t_end_ms, state.t_end_ms, "schedule must not move");
        assert_eq!(after.w_start_q64, state.w_start_q64, "weights must not move");
        assert_eq!(after.w_end_q64, state.w_end_q64, "weights must not move");
        assert_eq!(after.cum_collateral_in, state.cum_collateral_in, "pause raises nothing");
    }

    /// Resume is pause's exact inverse, and round-tripping is what pins the two
    /// dispatch arms to their meanings: if they were swapped, one of these two
    /// assertions fails instead of the pool silently doing the opposite of what
    /// the operator asked.
    #[test]
    fn resume_is_the_inverse_of_pause_and_round_trips() {
        let live = pool_state(SaleStatus::Open);

        let (paused_post, _) =
            set_paused(pool_account(&live), creator_account(true, CREATOR_ID), true, LBP_PROGRAM_ID);
        let paused = PoolState::try_from(&paused_post[0].account().data).expect("pool state");
        assert!(paused.paused);

        let (resumed_post, _) =
            set_paused(pool_account(&paused), creator_account(true, CREATOR_ID), false, LBP_PROGRAM_ID);
        let resumed = PoolState::try_from(&resumed_post[0].account().data).expect("pool state");
        assert!(!resumed.paused, "resume must clear the flag");
        assert_eq!(resumed, live, "pause then resume must return the pool to exactly its prior state");
    }

    /// RFP-016 Functionality #6: "Pausing does not affect weight progression;
    /// the weight schedule continues during a pause." Asserted against the
    /// weight function at three points while paused, because the natural way to
    /// implement an emergency stop - freeze the clock, or stamp the weight at
    /// pause time - would violate it silently and only show up as mispricing on
    /// resume.
    #[test]
    fn pausing_does_not_halt_weight_progression() {
        let live = pool_state(SaleStatus::Open);
        let (post, _) =
            set_paused(pool_account(&live), creator_account(true, CREATOR_ID), true, LBP_PROGRAM_ID);
        let paused = PoolState::try_from(&post[0].account().data).expect("pool state");

        for t in [1_000u64, 500_000, 1_000_000] {
            assert_eq!(
                paused.weight_token_q64(t),
                live.weight_token_q64(t),
                "the weight at {t} must not depend on whether the pool is paused"
            );
        }
        // And it is genuinely still progressing, not merely equal-because-frozen.
        assert!(
            paused.weight_token_q64(1_000) > paused.weight_token_q64(1_000_000),
            "a declining schedule must still decline while paused"
        );
    }

    #[test]
    #[should_panic(expected = "creator must authorize")]
    fn pause_rejects_an_unauthorized_creator() {
        let state = pool_state(SaleStatus::Open);
        let _ = set_paused(
            pool_account(&state),
            creator_account(false, CREATOR_ID),
            true,
            LBP_PROGRAM_ID,
        );
    }

    #[test]
    #[should_panic(expected = "not the pool creator")]
    fn pause_rejects_a_stranger() {
        let state = pool_state(SaleStatus::Open);
        let _ = set_paused(
            pool_account(&state),
            creator_account(true, AccountId::new([99u8; 32])),
            true,
            LBP_PROGRAM_ID,
        );
    }

    /// The owner guard: a pool account this program does not own must not be
    /// pausable, or anyone could ship a look-alike account and stop a sale.
    #[test]
    #[should_panic(expected = "pool not owned by LBP")]
    fn pause_rejects_a_pool_owned_by_another_program() {
        let state = pool_state(SaleStatus::Open);
        let mut foreign = pool_account(&state);
        foreign.account.program_owner = [9u32; 8];
        let _ = set_paused(foreign, creator_account(true, CREATOR_ID), true, LBP_PROGRAM_ID);
    }

    // ---- poke ----

    /// The weight-observation PDA, uninitialized (as it is before the first poke).
    fn weight_obs_account() -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account::default(),
            is_authorized: false,
            account_id: compute_weight_obs_pda(LBP_PROGRAM_ID, POOL_ID),
        }
    }

    #[test]
    fn poke_advances_stored_weight_to_clock_time() {
        let state = pool_state(SaleStatus::Open);
        let t_ms = 500_000u64; // mid-schedule
        let (post_states, calls) =
            poke(pool_account(&state), weight_obs_account(), LBP_PROGRAM_ID, t_ms as i64);
        assert!(calls.is_empty(), "poke emits no chained calls");
        let poked = WeightObs::try_from(&post_states[1].account().data).unwrap();
        let expected = weight_token_q64(
            state.w_start_q64,
            state.w_end_q64,
            state.t_start_ms,
            state.t_end_ms,
            t_ms,
        );
        assert_eq!(poked.w_token_q64, expected);
        assert_eq!(poked.ts_ms, t_ms);
        // The advanced weight is strictly between the schedule endpoints.
        assert!(poked.w_token_q64 < state.w_start_q64);
        assert!(poked.w_token_q64 > state.w_end_q64);
        // First poke claims the PDA; later pokes find it already owned.
        assert!(
            post_states[1].required_claim().is_some(),
            "the first poke must claim the weight-observation PDA"
        );
    }

    /// The whole point of moving the stored weight out of `PoolState`: a poke is
    /// permissionless, and a `BuyDisposable` proof pins the pool account
    /// byte-for-byte. If this ever fails, anyone can invalidate every in-flight
    /// private buy for the price of one cheap transaction that changes nothing.
    #[test]
    fn poke_leaves_the_pool_account_byte_identical() {
        let state = pool_state(SaleStatus::Open);
        let before = pool_account(&state);
        let (post_states, _) =
            poke(before.clone(), weight_obs_account(), LBP_PROGRAM_ID, 500_000);
        assert_eq!(
            post_states[0].account(),
            &before.account,
            "poke must echo the pool account unchanged"
        );
        assert!(
            post_states[0].required_claim().is_none(),
            "poke must not re-claim the pool account"
        );
    }

    #[test]
    #[should_panic(expected = "weight_obs: wrong PDA")]
    fn poke_rejects_a_foreign_weight_obs_account() {
        let state = pool_state(SaleStatus::Open);
        let wrong = AccountWithMetadata {
            account: Account::default(),
            is_authorized: false,
            account_id: AccountId::new([42u8; 32]),
        };
        let _ = poke(pool_account(&state), wrong, LBP_PROGRAM_ID, 500_000);
    }

    /// The weight-observation PDA as the chain hands it back on every poke after
    /// the first. The claim is what transfers ownership: LEZ applies
    /// `post.account_mut().program_owner = program_id` to a claimed post state,
    /// so the account arrives already initialized and LBP-owned - the exact
    /// shape the `new_claimed_if_default` / ownership asserts have to survive.
    fn claimed_weight_obs(after_first_poke: &Account) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account { program_owner: LBP_PROGRAM_ID, ..after_first_poke.clone() },
            is_authorized: false,
            account_id: compute_weight_obs_pda(LBP_PROGRAM_ID, POOL_ID),
        }
    }

    /// A second poke over the already-claimed PDA. `Poke` is permissionless and
    /// meant to be called repeatedly, so the interesting case is the one no
    /// first-poke test can reach: the account is no longer default.
    ///
    /// It must NOT re-claim - LEZ rejects a claim on an initialized account
    /// (`ClaimedNonDefaultAccount`), which would make every poke after the first
    /// revert and freeze the published weight forever - and the pool account

    /// RFP-016 §Supportability names "weight poke at multiple points in the
    /// schedule" as required coverage. The existing pokes reach at most TWO
    /// times (300_000 and 700_000) and assert only relative movement, so neither
    /// endpoint is ever poked and `weight_token_q64`'s two clamp branches are
    /// unreachable through the instruction.
    ///
    /// This walks one observation account across five points - both endpoints
    /// included - and asserts ABSOLUTE values at the ends: at `t_start` the
    /// stored weight must be exactly `w_start_q64`, at `t_end` exactly
    /// `w_end_q64`. A relative-only assertion would pass on a schedule that
    /// interpolated to the wrong endpoints entirely.
    #[test]
    fn poke_walks_the_whole_schedule_including_both_endpoints() {
        let state = pool_state(SaleStatus::Open);
        let (t0, t1) = (state.t_start_ms, state.t_end_ms);
        let span = t1 - t0;
        let points = [t0, t0 + span / 4, t0 + span / 2, t0 + (span * 3) / 4, t1];

        let mut obs_acct = weight_obs_account();
        let mut prev: Option<u128> = None;
        for (n, t) in points.into_iter().enumerate() {
            let (post, calls) =
                poke(pool_account(&state), obs_acct, LBP_PROGRAM_ID, i64::try_from(t).unwrap());
            assert!(calls.is_empty(), "poke emits no chained calls (point {n})");
            let o = WeightObs::try_from(&post[1].account().data).unwrap();
            assert_eq!(o.ts_ms, t, "poke must record the time it was poked at (point {n})");

            // The endpoints are the clamp branches, and they are exact.
            if n == 0 {
                assert_eq!(
                    o.w_token_q64, state.w_start_q64,
                    "at t_start the weight must be exactly w_start_q64"
                );
            }
            if n == points.len() - 1 {
                assert_eq!(
                    o.w_token_q64, state.w_end_q64,
                    "at t_end the weight must be exactly w_end_q64"
                );
            }
            // Strictly declining across the whole walk.
            if let Some(p) = prev {
                assert!(
                    o.w_token_q64 < p,
                    "the weight must strictly decline from point {} to {n}",
                    n - 1
                );
            }
            prev = Some(o.w_token_q64);
            obs_acct = claimed_weight_obs(post[1].account());
        }
    }

    /// The clamps themselves, through `poke`: a clock before the sale opens or
    /// after it ends must pin to the endpoint rather than extrapolating off the
    /// schedule. Extrapolation here would price a buy at a weight the schedule
    /// never defines.
    #[test]
    fn poke_clamps_outside_the_schedule_instead_of_extrapolating() {
        let state = pool_state(SaleStatus::Open);

        let (before, _) = poke(
            pool_account(&state),
            weight_obs_account(),
            LBP_PROGRAM_ID,
            i64::try_from(state.t_start_ms).unwrap() - 500_000,
        );
        let o = WeightObs::try_from(&before[1].account().data).unwrap();
        assert_eq!(o.w_token_q64, state.w_start_q64, "before t_start clamps to w_start_q64");

        let (after, _) = poke(
            pool_account(&state),
            weight_obs_account(),
            LBP_PROGRAM_ID,
            i64::try_from(state.t_end_ms).unwrap() + 500_000,
        );
        let o = WeightObs::try_from(&after[1].account().data).unwrap();
        assert_eq!(o.w_token_q64, state.w_end_q64, "after t_end clamps to w_end_q64");
    }

    /// must still come back byte-identical, on this path too.
    #[test]
    fn poke_over_an_already_claimed_obs_advances_it_without_re_claiming() {
        let state = pool_state(SaleStatus::Open);
        let (first, _) =
            poke(pool_account(&state), weight_obs_account(), LBP_PROGRAM_ID, 300_000);
        let first_obs = WeightObs::try_from(&first[1].account().data).unwrap();

        let (second, calls) = poke(
            pool_account(&state),
            claimed_weight_obs(first[1].account()),
            LBP_PROGRAM_ID,
            700_000,
        );
        assert!(calls.is_empty(), "poke emits no chained calls");
        assert!(
            second[1].required_claim().is_none(),
            "a poke over an already-claimed weight-obs PDA must not re-claim it: LEZ \
             rejects a claim on an initialized account, which would make every poke \
             after the first revert"
        );
        assert_eq!(
            second[1].account().program_owner,
            LBP_PROGRAM_ID,
            "the observation account keeps its owner across pokes"
        );
        let second_obs = WeightObs::try_from(&second[1].account().data).unwrap();
        assert_eq!(second_obs.ts_ms, 700_000, "the second poke advances the timestamp");
        assert!(
            second_obs.w_token_q64 < first_obs.w_token_q64,
            "the second poke advances the weight further down the declining schedule"
        );
        assert_eq!(
            second[0].account(),
            &pool_account(&state).account,
            "poke must echo the pool account unchanged on EVERY poke, not just the \
             first - a pool that drifts here invalidates in-flight BuyDisposable proofs"
        );
        assert!(
            second[0].required_claim().is_none(),
            "poke must not re-claim the pool account"
        );
    }

    /// Idempotent within a block, which is what the instruction doc promises:
    /// replaying the same poke at the same clock time rewrites the same bytes,
    /// so a duplicate submission cannot move the published weight.
    #[test]
    fn poke_repeated_at_the_same_time_is_byte_identical() {
        let state = pool_state(SaleStatus::Open);
        let (first, _) =
            poke(pool_account(&state), weight_obs_account(), LBP_PROGRAM_ID, 500_000);
        let claimed = claimed_weight_obs(first[1].account());
        let (second, _) =
            poke(pool_account(&state), claimed.clone(), LBP_PROGRAM_ID, 500_000);
        assert_eq!(
            second[1].account(),
            &claimed.account,
            "re-poking at the same clock time must leave the observation account \
             byte-identical"
        );
        assert_eq!(
            second[0].account(),
            &pool_account(&state).account,
            "poke must echo the pool account unchanged"
        );
    }

    /// The ownership half of the weight-obs check, which the PDA-id test above
    /// cannot reach: right id, wrong owner.
    ///
    /// On chain today this should be unreachable: the id is a public PDA of THIS
    /// program, a `Claim::Pda` is validated against the claiming program's own
    /// id, and a `Claim::Authorized` needs a signature no PDA has, so no other
    /// program can come to own it. That is exactly why the assert is worth a
    /// test - it is the thing keeping the unreachable case unreachable. Without
    /// it a foreign-owned slot would be emitted as a post state and refused
    /// downstream by `validate_execution` rule 6
    /// (`UnauthorizedDataModification`), which names no cause.
    #[test]
    #[should_panic(expected = "weight_obs not owned by LBP")]
    fn poke_rejects_an_initialized_weight_obs_owned_by_another_program() {
        let state = pool_state(SaleStatus::Open);
        let foreign = AccountWithMetadata {
            account: Account {
                program_owner: [9u32; 8],
                balance: 0u128,
                data: Data::from(&WeightObs { w_token_q64: 1, ts_ms: 1 }),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id: compute_weight_obs_pda(LBP_PROGRAM_ID, POOL_ID),
        };
        let _ = poke(pool_account(&state), foreign, LBP_PROGRAM_ID, 500_000);
    }

    // ---- close_sale ----

    #[test]
    fn close_sale_flips_to_closed_after_t_end() {
        let state = pool_state(SaleStatus::Open);
        let (post_states, calls) = close_sale(
            pool_account(&state),
            creator_account(true, CREATOR_ID),
            LBP_PROGRAM_ID,
            1_000_001, // >= t_end_ms
        );
        assert!(calls.is_empty(), "close emits no chained calls");
        let closed = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert!(matches!(closed.status, SaleStatus::Closed));
    }

    #[test]
    #[should_panic(expected = "not yet t_end_ms")]
    fn close_sale_rejects_before_t_end() {
        let state = pool_state(SaleStatus::Open);
        let _ = close_sale(
            pool_account(&state),
            creator_account(true, CREATOR_ID),
            LBP_PROGRAM_ID,
            999_999, // < t_end_ms
        );
    }

    #[test]
    #[should_panic(expected = "creator must authorize")]
    fn close_sale_rejects_unauthorized_creator() {
        let state = pool_state(SaleStatus::Open);
        let _ = close_sale(
            pool_account(&state),
            creator_account(false, CREATOR_ID),
            LBP_PROGRAM_ID,
            1_000_001,
        );
    }

    #[test]
    #[should_panic(expected = "not the pool creator")]
    fn close_sale_rejects_non_creator() {
        let state = pool_state(SaleStatus::Open);
        let _ = close_sale(
            pool_account(&state),
            creator_account(true, AccountId::new([99u8; 32])),
            LBP_PROGRAM_ID,
            1_000_001,
        );
    }

    #[test]
    #[should_panic(expected = "already closed")]
    fn close_sale_rejects_already_closed() {
        let state = pool_state(SaleStatus::Closed);
        let _ = close_sale(
            pool_account(&state),
            creator_account(true, CREATOR_ID),
            LBP_PROGRAM_ID,
            1_000_001,
        );
    }
}

/// `BuyDisposable`: the validity-window helpers, and every guard the private
/// path adds over the public one. These operate over `AccountWithMetadata`
/// fixtures because the guards read the pinned pool account, not just the
/// pricing state.
mod disposable_tests {
    use lbp_core::{
        private_window_hi, PoolState, SaleStatus, MAX_PRIVATE_WINDOW_MS,
    };
    use lee_core::{
        account::{Account, AccountId, AccountWithMetadata, Data, Nonce},
        program::{AccountPostState, ChainedCall, ProgramId},
    };
    use token_core::TokenHolding;

    use crate::buy::{buy_disposable, disposable_window};

    const LBP_PROGRAM_ID: ProgramId = [7u32; 8];
    const TOKEN_PROGRAM_ID: ProgramId = [3u32; 8];

    const POOL_ID: AccountId = AccountId::new([2u8; 32]);
    const TOKEN_VAULT_ID: AccountId = AccountId::new([3u8; 32]);
    const COLLATERAL_VAULT_ID: AccountId = AccountId::new([4u8; 32]);
    const COLLATERAL_DEF_ID: AccountId = AccountId::new([5u8; 32]);
    const BUYER_COLLATERAL_ID: AccountId = AccountId::new([6u8; 32]);
    const BUYER_TOKEN_ID: AccountId = AccountId::new([8u8; 32]);

    const T_START: u64 = 1_000;
    const T_END: u64 = 1_000_000;
    const T_BUY: u64 = 2_000;

    fn open_pool() -> PoolState {
        PoolState {
            token_vault_id: TOKEN_VAULT_ID,
            collateral_vault_id: COLLATERAL_VAULT_ID,
            collateral_definition_id: COLLATERAL_DEF_ID,
            w_start_q64: super::w(99, 100),
            w_end_q64: super::w(1, 100),
            t_start_ms: T_START,
            t_end_ms: T_END,
            reserve_token: 1_000_000_000,
            reserve_collateral: 50_000,
            status: SaleStatus::Open,
            ..Default::default()
        }
    }

    fn account(
        account_id: AccountId,
        program_owner: ProgramId,
        data: Data,
        is_authorized: bool,
    ) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account { program_owner, balance: 0u128, data, nonce: Nonce(0) },
            is_authorized,
            account_id,
        }
    }

    /// The buyer's collateral leg. Authorized: a private authorized slot carries
    /// `is_authorized == true` in its pre-state (the privacy circuit asserts it),
    /// exactly like a keypair holding, so the shared finalizer's check holds on
    /// this path too.
    fn buyer_collateral() -> AccountWithMetadata {
        let holding = TokenHolding::Fungible {
            definition_id: COLLATERAL_DEF_ID,
            balance: 1_000_000,
        };
        account(BUYER_COLLATERAL_ID, TOKEN_PROGRAM_ID, Data::from(&holding), true)
    }

    fn buy(state: &PoolState, t_buy_ms: u64) -> (Vec<AccountPostState>, Vec<ChainedCall>, u64) {
        buy_disposable(
            account(POOL_ID, LBP_PROGRAM_ID, Data::from(state), false),
            account(TOKEN_VAULT_ID, TOKEN_PROGRAM_ID, Data::default(), false),
            account(COLLATERAL_VAULT_ID, TOKEN_PROGRAM_ID, Data::default(), false),
            buyer_collateral(),
            account(BUYER_TOKEN_ID, TOKEN_PROGRAM_ID, Data::default(), false),
            10_000,
            0,
            t_buy_ms,
            LBP_PROGRAM_ID,
        )
    }

    // ---- the happy path ----

    #[test]
    fn disposable_buy_moves_both_legs_and_records_no_observation() {
        let state = open_pool();
        let (post_states, calls, t_end_ms) = buy(&state, T_BUY);
        assert_eq!(t_end_ms, T_END, "t_end_ms must come from the pinned pool state");
        assert_eq!(post_states.len(), 5, "five accounts in, five post states out");
        assert_eq!(calls.len(), 2, "collateral in, tokens out - no fee leg on an LBP");
        let after = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert!(after.reserve_token < state.reserve_token, "tokens left the pool");
        assert_eq!(after.reserve_collateral, state.reserve_collateral + 10_000);
        assert_eq!(after.buy_count, 1);
        assert!(
            after.obs.is_empty(),
            "a buyer-chosen timestamp must never enter the observation ring"
        );
    }

    // ---- the four guards ----

    #[test]
    #[should_panic(expected = "allowlist-gated and cannot be bought privately")]
    fn disposable_rejects_a_gated_pool() {
        let mut state = open_pool();
        state.allowlist_root = [9u8; 32];
        let _ = buy(&state, T_BUY);
    }

    #[test]
    #[should_panic(expected = "per-block ceiling; buy publicly")]
    fn disposable_rejects_a_per_block_ceiling() {
        let mut state = open_pool();
        state.block_token_ceiling = 1;
        let _ = buy(&state, T_BUY);
    }

    #[test]
    #[should_panic(expected = "weight does not decline")]
    fn disposable_rejects_a_flat_weight_schedule() {
        // Not reachable through `create_sale` today - which is the point: this
        // instruction's safety must not rest on a check in another function.
        let mut state = open_pool();
        state.w_end_q64 = state.w_start_q64;
        let _ = buy(&state, T_BUY);
    }

    #[test]
    fn disposable_accepts_a_fixed_price_pool() {
        // A flat schedule is fine when the pool is fixed-price: the price does not
        // depend on `t` at all, so `t_buy_ms` carries no pricing freedom.
        let mut state = open_pool();
        state.fixed_price = true;
        state.fixed_price_q64 = (5u128 << 64) / 100;
        state.w_end_q64 = state.w_start_q64;
        let (_, calls, _) = buy(&state, T_BUY);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    #[should_panic(expected = "sale is paused")]
    fn disposable_rejects_a_paused_pool() {
        let mut state = open_pool();
        state.paused = true;
        let _ = buy(&state, T_BUY);
    }

    #[test]
    #[should_panic(expected = "sale has not started")]
    fn disposable_rejects_a_t_buy_before_the_start() {
        let _ = buy(&open_pool(), T_START - 1);
    }

    #[test]
    #[should_panic(expected = "sale has ended")]
    fn disposable_rejects_a_t_buy_at_or_past_the_end() {
        // The window is half-open, so `t_buy_ms == t_end_ms` is already outside it.
        let _ = buy(&open_pool(), T_END);
    }

    // ---- the validity window ----

    #[test]
    fn private_window_hi_takes_the_earliest_of_its_three_bounds() {
        // 1. the caller's deadline is the binding one.
        assert_eq!(private_window_hi(5_000, 1_000, 1_000_000), 5_000, "deadline binds");
        // 2. the one-hour staleness cap is the binding one.
        assert_eq!(
            private_window_hi(u64::MAX, 1_000, u64::MAX),
            1_000 + MAX_PRIVATE_WINDOW_MS,
            "staleness cap binds"
        );
        // 3. the sale's own end is the binding one - the only end-of-sale check a
        //    clockless path has.
        assert_eq!(
            private_window_hi(u64::MAX, 1_000, 4_000),
            4_000,
            "t_end_ms binds"
        );
        // A t_buy near u64::MAX must not wrap the cap into a bound below t_buy.
        assert_eq!(
            private_window_hi(u64::MAX, u64::MAX - 1, u64::MAX),
            u64::MAX,
            "the staleness cap saturates instead of wrapping"
        );
    }

    #[test]
    fn disposable_window_is_half_open_from_the_price_timestamp() {
        let window = disposable_window(u64::MAX, T_BUY, u64::MAX);
        assert_eq!(window.start, T_BUY, "priced-at time is the inclusive lower bound");
        assert_eq!(window.end, T_BUY + MAX_PRIVATE_WINDOW_MS);
        assert!(!window.contains(&window.end), "the upper bound is exclusive");
        // This sale ends well inside the staleness cap, so the end clamps it.
        let clamped = disposable_window(u64::MAX, T_BUY, T_END);
        assert_eq!(clamped.end, T_END, "the window can never outlive the sale");
    }

    #[test]
    #[should_panic(expected = "empty validity window")]
    fn disposable_window_rejects_an_expired_deadline() {
        // deadline <= t_buy_ms leaves nothing between the bounds; LEZ has no
        // RangeInclusive conversion and rejects `from >= to`, so this must revert
        // here with something the submitter can act on.
        let _ = disposable_window(T_BUY, T_BUY, T_END);
    }

    #[test]
    #[should_panic(expected = "empty validity window")]
    fn disposable_window_rejects_a_t_buy_at_the_sale_end() {
        let _ = disposable_window(u64::MAX, T_END, T_END);
    }
}

/// Shared account fixtures for the tests below that operate over
/// `AccountWithMetadata` rather than pure pricing state. Mirrors the bonding
/// curve's `tests::fixtures`, ids and all, so the two suites read the same.
mod fixtures {
    use lbp_core::PoolState;
    use lee_core::{
        account::{Account, AccountId, AccountWithMetadata, Data, Nonce},
        program::{ChainedCall, ProgramId},
    };
    use token_core::{TokenDefinition, TokenHolding};

    pub const LBP_PROGRAM_ID: ProgramId = [7u32; 8];
    /// The collateral token's program. Also the project token's, except where a
    /// test deliberately splits the two.
    pub const TOKEN_PROGRAM_ID: ProgramId = [5u32; 8];
    /// A second, unrelated token program - two tokens under distinct programs is
    /// a legal pool, and it is what makes a mis-anchored dispatch observable.
    pub const PROJECT_TOKEN_PROGRAM_ID: ProgramId = [6u32; 8];
    /// A program the attacker deployed. LEZ deployment is permissionless, so any
    /// account a submitter supplies may be owned by one of these.
    pub const HOSTILE_PROGRAM_ID: ProgramId = [42u32; 8];

    pub const CREATOR_ID: AccountId = AccountId::new([1u8; 32]);
    pub const POOL_ID: AccountId = AccountId::new([2u8; 32]);
    pub const TOKEN_VAULT_ID: AccountId = AccountId::new([3u8; 32]);
    pub const COLLATERAL_VAULT_ID: AccountId = AccountId::new([4u8; 32]);
    pub const TREASURY_ID: AccountId = AccountId::new([6u8; 32]);
    pub const TOKEN_DEF_ID: AccountId = AccountId::new([8u8; 32]);
    pub const COLLATERAL_DEF_ID: AccountId = AccountId::new([9u8; 32]);
    pub const CREATOR_COLL_HOLDING_ID: AccountId = AccountId::new([10u8; 32]);
    pub const CREATOR_TOKEN_HOLDING_ID: AccountId = AccountId::new([11u8; 32]);
    pub const BUYER_COLL_HOLDING_ID: AccountId = AccountId::new([12u8; 32]);
    pub const BUYER_TOKEN_HOLDING_ID: AccountId = AccountId::new([13u8; 32]);

    pub fn pool_account(state: &PoolState) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account {
                program_owner: LBP_PROGRAM_ID,
                balance: 0u128,
                data: Data::from(state),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id: POOL_ID,
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

    /// A Fungible holding owned by `program_owner`. Which program owns a holding
    /// is the whole subject of the dispatch tests, so it is a parameter.
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

    pub fn holding(id: AccountId, definition_id: AccountId, balance: u128) -> AccountWithMetadata {
        holding_owned_by(id, definition_id, balance, TOKEN_PROGRAM_ID)
    }

    /// A token DEFINITION account (not a holding). `create_sale` reads its id and
    /// its owning token program, and asserts the data really is a definition.
    pub fn definition(id: AccountId, program_owner: ProgramId) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account {
                program_owner,
                balance: 0u128,
                data: Data::from(&TokenDefinition::Fungible {
                    name: "Sample".to_string(),
                    total_supply: u128::MAX,
                    metadata_id: None,
                }),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id: id,
        }
    }

    /// The same, with caller-chosen `data` - for the cases where the account is
    /// owned by a token program but is not a definition at all.
    pub fn definition_with_data(
        id: AccountId,
        program_owner: ProgramId,
        data: Data,
    ) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account { program_owner, balance: 0u128, data, nonce: Nonce(0) },
            is_authorized: false,
            account_id: id,
        }
    }

    /// An account that has never been touched - the shape of an uninitialized
    /// PDA, and of a treasury id nothing has ever paid into.
    pub fn uninitialized(account_id: AccountId) -> AccountWithMetadata {
        AccountWithMetadata { account: Account::default(), is_authorized: false, account_id }
    }

    pub fn collateral_vault(balance: u128) -> AccountWithMetadata {
        holding(COLLATERAL_VAULT_ID, COLLATERAL_DEF_ID, balance)
    }

    pub fn token_vault(balance: u128) -> AccountWithMetadata {
        holding(TOKEN_VAULT_ID, TOKEN_DEF_ID, balance)
    }

    /// The fee sink. It holds the collateral token, since the at-close fee is
    /// paid in collateral.
    pub fn treasury() -> AccountWithMetadata {
        treasury_holding(TREASURY_ID)
    }

    /// The same, under a caller-chosen id - what `CreateSale` requires a treasury
    /// to already be: an initialised `Fungible` holding of the collateral
    /// definition, under the collateral definition's own token program.
    pub fn treasury_holding(id: AccountId) -> AccountWithMetadata {
        holding(id, COLLATERAL_DEF_ID, 0)
    }

    /// A holding that parses but is NOT `Fungible`. A treasury of this shape can
    /// never receive the at-close fee, and `create_sale` must say so.
    pub fn nft_holding(id: AccountId, definition_id: AccountId) -> AccountWithMetadata {
        AccountWithMetadata {
            account: Account {
                program_owner: TOKEN_PROGRAM_ID,
                balance: 0u128,
                data: Data::from(&TokenHolding::NftPrintedCopy { definition_id, owned: true }),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id: id,
        }
    }

    /// A NonFungible token DEFINITION - the F27 shape. A pool prices divisible
    /// reserves, so neither side may name one.
    pub fn nft_definition(id: AccountId, program_owner: ProgramId) -> AccountWithMetadata {
        definition_with_data(
            id,
            program_owner,
            Data::from(&TokenDefinition::NonFungible {
                name: "Sample Collection".to_string(),
                printable_supply: 10,
                metadata_id: AccountId::new([99u8; 32]),
            }),
        )
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

    /// Fungible balance of a chained call's pre-state - used to assert a leg was
    /// built against the balance it will actually see.
    pub fn balance_of(awm: &AccountWithMetadata) -> u128 {
        match TokenHolding::try_from(&awm.account.data) {
            Ok(TokenHolding::Fungible { balance, .. }) => balance,
            _ => panic!("expected a Fungible token holding"),
        }
    }
}

/// `Withdraw` tests. The at-close fee used to be paid to the treasury in the
/// same transaction as the creator's principal, which meant an unusable treasury
/// (uninitialised, wrong definition, wrong token program) reverted the whole
/// instruction and locked the raise AND every unsold token forever - a closed
/// pool has no other drain. What is covered here is the fix: the fee is escrowed
/// into `treasury_owed`, no treasury account is named at all, and the escrow is
/// charged exactly once no matter how often withdrawal is retried.
mod withdraw_tests {
    use lbp_core::{
        close_fee, compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, PoolState,
        SaleStatus,
    };
    use lee_core::{
        account::{AccountId, AccountWithMetadata},
        program::{AccountPostState, ChainedCall},
    };

    use super::fixtures::*;
    use crate::lifecycle::withdraw;

    /// The verifier's construction: a closed pool holding 15 000 collateral and
    /// 200 000 unsold tokens at a 5% at-close fee.
    const RAISED: u128 = 15_000;
    const UNSOLD: u128 = 200_000;
    const FEE_BPS: u128 = 500;
    /// 5% of the raise, rounded up.
    const FEE: u128 = 750;

    fn pool_state(status: SaleStatus) -> PoolState {
        PoolState {
            creator: CREATOR_ID,
            token_definition_id: TOKEN_DEF_ID,
            collateral_definition_id: COLLATERAL_DEF_ID,
            token_vault_id: TOKEN_VAULT_ID,
            collateral_vault_id: COLLATERAL_VAULT_ID,
            treasury_id: TREASURY_ID,
            fee_bps: FEE_BPS,
            cum_collateral_in: RAISED,
            reserve_collateral: RAISED,
            reserve_token: UNSOLD,
            status,
            ..Default::default()
        }
    }

    fn run(
        state: &PoolState,
        creator: AccountWithMetadata,
        collateral_balance: u128,
        token_balance: u128,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        withdraw(
            pool_account(state),
            token_vault(token_balance),
            collateral_vault(collateral_balance),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, 0),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0),
            creator,
            LBP_PROGRAM_ID,
        )
    }

    #[test]
    fn withdraw_pays_the_creator_escrows_the_fee_and_names_no_treasury() {
        assert_eq!(FEE, close_fee(RAISED, FEE_BPS), "fixture fee must match close_fee");
        let state = pool_state(SaleStatus::Closed);
        let (post_states, calls) = run(&state, creator_account(true, CREATOR_ID), RAISED, UNSOLD);

        assert_eq!(calls.len(), 2, "one creator-collateral leg and one project-token leg");

        // The creator's collateral, out of the FULL vault balance: no fee leg
        // moves it first any more, so there is nothing to shift.
        let coll = &calls[0];
        assert_eq!(coll.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(coll.pda_seeds, vec![compute_collateral_vault_pda_seed(POOL_ID)]);
        assert!(coll.pre_states[0].is_authorized, "collateral vault must be authorized");
        assert_eq!(coll.pre_states[0].account_id, COLLATERAL_VAULT_ID);
        assert_eq!(balance_of(&coll.pre_states[0]), RAISED);
        assert_eq!(coll.pre_states[1].account_id, CREATOR_COLL_HOLDING_ID);
        assert_eq!(coll.instruction_data, expected_transfer_data(RAISED - FEE));

        // The unsold project tokens are unaffected by the escrow.
        let tok = &calls[1];
        assert_eq!(tok.pda_seeds, vec![compute_token_vault_pda_seed(POOL_ID)]);
        assert!(tok.pre_states[0].is_authorized, "token vault must be authorized");
        assert_eq!(tok.pre_states[0].account_id, TOKEN_VAULT_ID);
        assert_eq!(tok.pre_states[1].account_id, CREATOR_TOKEN_HOLDING_ID);
        assert_eq!(tok.instruction_data, expected_transfer_data(UNSOLD));

        // Nothing anywhere touches the treasury: that is what makes an unusable
        // treasury cost the fee instead of the entire pool.
        assert!(
            calls
                .iter()
                .all(|c| c.pre_states.iter().all(|p| p.account_id != TREASURY_ID)),
            "Withdraw must not name the treasury anywhere"
        );

        let post = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, FEE, "the fee is escrowed, not paid");
        assert_eq!(post.reserve_collateral, 0, "the creator's bucket is zeroed");
        assert_eq!(post.reserve_token, 0);
        assert_eq!(post_states.len(), 6, "one post-state per declared account");
    }

    /// INVARIANT: `collateral_vault_balance == reserve_collateral +
    /// treasury_owed`. It is what makes the escrow safe - `Withdraw` pays the
    /// creator the vault minus the bucket, so if the two ever drifted apart one
    /// party would be paid out of the other's funds.
    #[test]
    fn vault_balance_equals_reserve_collateral_plus_treasury_owed() {
        let state = pool_state(SaleStatus::Closed);
        assert_eq!(
            RAISED,
            state.reserve_collateral + state.treasury_owed,
            "fixture starts balanced (no per-swap fee, so the bucket is empty while the sale runs)"
        );
        let (post_states, calls) = run(&state, creator_account(true, CREATOR_ID), RAISED, UNSOLD);
        let paid_out = RAISED - FEE;
        assert_eq!(calls[0].instruction_data, expected_transfer_data(paid_out));
        let vault_after = RAISED - paid_out;
        let post = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(
            vault_after,
            post.reserve_collateral + post.treasury_owed,
            "the collateral left behind must be exactly what the books still owe"
        );
    }

    /// A second withdrawal against a vault holding nothing but the escrow: the
    /// drained-vault guard looks at the CREATOR's share, not the raw balance, so
    /// this reverts rather than handing the treasury's money to the creator.
    #[test]
    #[should_panic(expected = "nothing to withdraw")]
    fn withdraw_rejects_a_second_withdraw_that_would_eat_the_escrow() {
        let mut state = pool_state(SaleStatus::Closed);
        state.treasury_owed = FEE;
        let _ = run(&state, creator_account(true, CREATOR_ID), FEE, 0);
    }

    /// The fee is charged against the creator's share, which is zero once the
    /// collateral is out - so a later withdrawal (here, one that still has tokens
    /// to move) cannot credit the bucket a second time.
    #[test]
    fn withdraw_charges_the_fee_once_even_if_it_runs_again() {
        let mut state = pool_state(SaleStatus::Closed);
        state.treasury_owed = FEE;
        let (post_states, calls) = run(&state, creator_account(true, CREATOR_ID), FEE, UNSOLD);
        assert_eq!(calls.len(), 1, "only the project-token leg survives");
        assert_eq!(calls[0].instruction_data, expected_transfer_data(UNSOLD));
        let post = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, FEE, "the escrow is not charged twice");
    }

    /// The creator must never be paid out of the escrow, even if the vault is
    /// somehow short of it: the collateral leg is skipped entirely rather than
    /// underflowing or dipping in.
    #[test]
    fn withdraw_pays_no_collateral_when_the_vault_is_short_of_the_escrow() {
        let mut state = pool_state(SaleStatus::Closed);
        state.treasury_owed = 30_000;
        let (post_states, calls) = run(&state, creator_account(true, CREATOR_ID), 20_000, UNSOLD);
        assert_eq!(calls.len(), 1, "only the project-token leg survives");
        assert_eq!(calls[0].instruction_data, expected_transfer_data(UNSOLD));
        let post = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, 30_000, "the full debt stays claimable");
    }

    #[test]
    #[should_panic(expected = "nothing to withdraw")]
    fn withdraw_rejects_a_second_withdraw_when_drained() {
        let state = pool_state(SaleStatus::Closed);
        let _ = run(&state, creator_account(true, CREATOR_ID), 0, 0);
    }

    #[test]
    #[should_panic(expected = "sale must be closed first")]
    fn withdraw_requires_closed() {
        let state = pool_state(SaleStatus::Open);
        let _ = run(&state, creator_account(true, CREATOR_ID), RAISED, UNSOLD);
    }

    #[test]
    #[should_panic(expected = "creator must authorize")]
    fn withdraw_rejects_unauthorized_creator() {
        let state = pool_state(SaleStatus::Closed);
        let _ = run(&state, creator_account(false, CREATOR_ID), RAISED, UNSOLD);
    }

    #[test]
    #[should_panic(expected = "not the pool creator")]
    fn withdraw_rejects_non_creator() {
        let state = pool_state(SaleStatus::Closed);
        let _ = run(
            &state,
            creator_account(true, AccountId::new([99u8; 32])),
            RAISED,
            UNSOLD,
        );
    }
}

/// `SweepTreasury` tests. This is the ONLY path by which the escrowed at-close
/// fee leaves the collateral vault, so what is covered here is the exact-amount
/// payout, the clear-by-what-moved bookkeeping, and the pins that keep a
/// permissionless submission from doing anything other than paying the treasury
/// the pool already named.
mod sweep_tests {
    use lbp_core::{compute_collateral_vault_pda_seed, PoolState, SaleStatus};
    use lee_core::account::{AccountId, Nonce};

    use super::fixtures::*;
    use crate::lifecycle::sweep_treasury;

    const OWED: u128 = 750;
    /// Collateral that is NOT the treasury's - a sweep must never touch it.
    const CREATOR_SHARE: u128 = 14_250;

    fn pool_state(owed: u128) -> PoolState {
        PoolState {
            creator: CREATOR_ID,
            token_vault_id: TOKEN_VAULT_ID,
            collateral_vault_id: COLLATERAL_VAULT_ID,
            treasury_id: TREASURY_ID,
            reserve_collateral: CREATOR_SHARE,
            treasury_owed: owed,
            status: SaleStatus::Closed,
            ..Default::default()
        }
    }

    #[test]
    fn sweep_pays_exactly_the_owed_amount_and_zeroes_the_bucket() {
        let state = pool_state(OWED);
        let (post_states, calls) = sweep_treasury(
            pool_account(&state),
            collateral_vault(CREATOR_SHARE + OWED),
            treasury(),
            LBP_PROGRAM_ID,
        );

        assert_eq!(calls.len(), 1, "the sweep is exactly one transfer");
        let leg = &calls[0];
        assert_eq!(leg.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(leg.pda_seeds, vec![compute_collateral_vault_pda_seed(POOL_ID)]);
        assert!(leg.pre_states[0].is_authorized, "the vault must be authorized to send");
        assert_eq!(leg.pre_states[0].account_id, COLLATERAL_VAULT_ID);
        assert_eq!(leg.pre_states[1].account_id, TREASURY_ID);
        assert_eq!(
            leg.instruction_data,
            expected_transfer_data(OWED),
            "exactly the escrowed fee moves - not the collateral sitting beside it"
        );

        let post = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, 0, "the bucket is cleared by what moved");
        assert_eq!(post.reserve_collateral, CREATOR_SHARE, "the creator's books are untouched");
        assert_eq!(post_states.len(), 3, "one post-state per declared account");
    }

    /// Dispatch is anchored to the VAULT's program owner. The sweep is
    /// permissionless, so reading the id off the submitter-supplied treasury would
    /// let anyone route the vault PDA's authority into a program of their
    /// choosing - the critical bug the buy paths already close.
    #[test]
    fn sweep_dispatches_on_the_vault_program_not_the_treasury() {
        let state = pool_state(OWED);
        let mut hostile_treasury = treasury();
        hostile_treasury.account.program_owner = HOSTILE_PROGRAM_ID;
        let (_, calls) = sweep_treasury(
            pool_account(&state),
            collateral_vault(CREATOR_SHARE + OWED),
            hostile_treasury,
            LBP_PROGRAM_ID,
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
        let state = pool_state(OWED);
        let (post_states, calls) =
            sweep_treasury(pool_account(&state), collateral_vault(500), treasury(), LBP_PROGRAM_ID);
        assert_eq!(calls[0].instruction_data, expected_transfer_data(500));
        let post = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(post.treasury_owed, OWED - 500, "the unpaid remainder stays claimable");
    }

    /// Nothing owed means nothing to do: reverting keeps a repeated sweep from
    /// emitting a zero-value transfer (and makes a double sweep a no-op failure
    /// rather than a second payout).
    #[test]
    #[should_panic(expected = "nothing owed to the treasury")]
    fn sweep_reverts_when_nothing_is_owed() {
        let state = pool_state(0);
        let _ = sweep_treasury(
            pool_account(&state),
            collateral_vault(CREATOR_SHARE),
            treasury(),
            LBP_PROGRAM_ID,
        );
    }

    /// The submitter picks every non-signer account id, and this instruction has
    /// no signer at all, so the pins against the pool state are the whole security
    /// model.
    #[test]
    #[should_panic(expected = "wrong treasury account")]
    fn sweep_rejects_a_substituted_treasury() {
        let state = pool_state(OWED);
        let _ = sweep_treasury(
            pool_account(&state),
            collateral_vault(CREATOR_SHARE + OWED),
            holding(AccountId::new([98u8; 32]), COLLATERAL_DEF_ID, 0),
            LBP_PROGRAM_ID,
        );
    }

    #[test]
    #[should_panic(expected = "wrong collateral vault")]
    fn sweep_rejects_a_substituted_collateral_vault() {
        let state = pool_state(OWED);
        let _ = sweep_treasury(
            pool_account(&state),
            holding(AccountId::new([97u8; 32]), COLLATERAL_DEF_ID, CREATOR_SHARE + OWED),
            treasury(),
            LBP_PROGRAM_ID,
        );
    }

    /// The other half of the F26 fix, from the sweep side. `create_sale` pins the
    /// treasury to an already-initialised holding and LEZ has no instruction that
    /// un-initialises one, so a DEFAULT-owned treasury here means the pool's
    /// pinned state disagrees with chain state - impossible, and asserted anyway
    /// because the alternative is emitting a fee leg into an unowned account.
    ///
    /// What stood here was a bootstrap path that let the treasury CLAIM itself on
    /// its own signature. It is gone because that signature was both the only key
    /// to it and the thing that broke it: a claim needs the pre-state to be
    /// `Account::default()` WHOLE, and LEZ bumps a signer's nonce even when its
    /// account is unowned, so the treasury's own key locked this escrow forever
    /// the moment it signed anything else. The account is signed here to prove
    /// that path really is gone - a signature no longer rescues it.
    #[test]
    #[should_panic(expected = "pool treasury is unowned")]
    fn sweep_rejects_a_treasury_create_sale_could_never_have_accepted() {
        let state = pool_state(OWED);
        let mut fresh_treasury = uninitialized(TREASURY_ID);
        fresh_treasury.is_authorized = true;
        let _ = sweep_treasury(
            pool_account(&state),
            collateral_vault(CREATOR_SHARE + OWED),
            fresh_treasury,
            LBP_PROGRAM_ID,
        );
    }

    /// The other unowned shape: an id that is non-default (a bumped nonce, a
    /// balance) yet still DEFAULT-owned. Under the old bootstrap branch this was
    /// the permanent lock - no program may claim a non-pristine account, so the
    /// escrow could never be settled. It is now rejected at `CreateSale` instead,
    /// and unreachable here.
    ///
    /// NOT something a stranger can do, which is the part this comment used to
    /// get wrong. LEZ refuses any transaction that leaves a DEFAULT-owned account
    /// modified without a claim, and a third party has neither the treasury's
    /// signature (`Claim::Authorized`) nor a program whose PDA derivation is this
    /// id (`Claim::Pda`). The bumped nonce is what the treasury's OWN key leaves
    /// behind by signing anything; the balance is stronger still - after genesis
    /// no transaction can put one on a DEFAULT-owned account at all. Both are set
    /// here so the assert is pinned against the whole shape, however it arose.
    #[test]
    #[should_panic(expected = "pool treasury is unowned")]
    fn sweep_rejects_a_treasury_that_is_non_default_but_unowned() {
        let state = pool_state(OWED);
        let mut unowned = uninitialized(TREASURY_ID);
        unowned.account.balance = 1; // genesis-only; no transaction can do this
        unowned.account.nonce = Nonce(1); // one signature by the treasury's own key
        let _ = sweep_treasury(
            pool_account(&state),
            collateral_vault(CREATOR_SHARE + OWED),
            unowned,
            LBP_PROGRAM_ID,
        );
    }

    /// Forwarded, never forced: an ordinary sweep is unsigned, and marking an
    /// account that is not in the signer set is `InvalidAccountAuthorization` -
    /// which would reject every permissionless sweep ever submitted.
    #[test]
    fn sweep_does_not_fabricate_authorization_for_the_treasury() {
        let state = pool_state(OWED);
        let (_, calls) = sweep_treasury(
            pool_account(&state),
            collateral_vault(CREATOR_SHARE + OWED),
            treasury(), // initialised, unsigned - the normal case
            LBP_PROGRAM_ID,
        );
        assert!(
            !calls[0].pre_states[1].is_authorized,
            "an unsigned treasury slot must stay unauthorized in the fee leg"
        );
    }

    /// A pool account owned by someone else could carry forged state, including a
    /// `treasury_owed` big enough to drain a vault it names.
    #[test]
    #[should_panic(expected = "pool not owned by LBP")]
    fn sweep_rejects_a_foreign_pool_account() {
        let state = pool_state(OWED);
        let _ = sweep_treasury(
            pool_account(&state),
            collateral_vault(CREATOR_SHARE + OWED),
            treasury(),
            [77u32; 8], // some other program's id
        );
    }
}

/// Buy-path dispatch anchors. `buy_transfers` reads each leg's program off the
/// VAULT that leg moves, and `finalize_buy` binds the buyer's collateral holding
/// to the collateral vault's program. A verifier deleted the anchor assert and
/// re-pointed both legs at `payer.account.program_owner`, and all 32 unit tests
/// still passed - the guard for the critical drain had no in-process coverage at
/// all. These are that coverage; they are modelled on the bonding curve's
/// `sweep_dispatches_on_the_vault_program_not_the_treasury`, which does the same
/// job for the sweep.
mod buy_dispatch_tests {
    use lbp_core::{
        compute_collateral_vault_pda_seed, compute_token_vault_pda_seed, PoolState, SaleStatus,
    };

    use super::fixtures::*;
    use crate::buy::{buy, buy_disposable};
    use crate::util::authorized;

    const COLLATERAL_IN: u128 = 10_000;
    const T_BUY: u64 = 2_000;

    fn open_pool() -> PoolState {
        PoolState {
            token_definition_id: TOKEN_DEF_ID,
            collateral_definition_id: COLLATERAL_DEF_ID,
            token_vault_id: TOKEN_VAULT_ID,
            collateral_vault_id: COLLATERAL_VAULT_ID,
            w_start_q64: super::w(99, 100),
            w_end_q64: super::w(1, 100),
            t_start_ms: 1_000,
            t_end_ms: 1_000_000,
            reserve_token: 1_000_000_000,
            reserve_collateral: 50_000,
            status: SaleStatus::Open,
            ..Default::default()
        }
    }

    /// The pool's two tokens under DISTINCT token programs - a legal pool, and
    /// the only configuration in which a leg pointed at the wrong account is
    /// observable at all: while both vaults share one program, an anchor read off
    /// the buyer's accounts returns the same id and nothing can tell them apart.
    #[test]
    fn buy_dispatches_each_leg_on_its_own_vault_not_on_the_buyer_accounts() {
        let state = open_pool();
        let (_, calls) = buy(
            pool_account(&state),
            holding_owned_by(
                TOKEN_VAULT_ID,
                TOKEN_DEF_ID,
                1_000_000_000,
                PROJECT_TOKEN_PROGRAM_ID,
            ),
            collateral_vault(50_000),
            authorized(&holding(BUYER_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_IN)),
            // The recipient is the submitter's free choice: nothing anchors it,
            // exactly like the treasury slot on the sweep path.
            holding_owned_by(BUYER_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0, HOSTILE_PROGRAM_ID),
            COLLATERAL_IN,
            0,
            LBP_PROGRAM_ID,
            T_BUY as i64,
            1,
        );

        assert_eq!(calls.len(), 2, "collateral in, tokens out");
        assert_eq!(
            calls[0].program_id, TOKEN_PROGRAM_ID,
            "the collateral leg must dispatch on the collateral VAULT's token program, never on \
             the buyer-supplied payer: a payer owned by a no-op program would make this leg move \
             nothing while the pool still hands out tokens"
        );
        assert_eq!(calls[0].pda_seeds, vec![compute_collateral_vault_pda_seed(POOL_ID)]);
        assert_eq!(
            calls[1].program_id, PROJECT_TOKEN_PROGRAM_ID,
            "the token leg must dispatch on the token VAULT's program, never on an id read off \
             the buyer's holding"
        );
        assert_eq!(calls[1].pda_seeds, vec![compute_token_vault_pda_seed(POOL_ID)]);
    }

    /// The other half of the anchor: the payer must be a holding of the same
    /// token program that owns the vault it pays into. Without it, a buyer hands
    /// in a holding owned by a program they deployed whose `Transfer` echoes its
    /// pre-states - the collateral leg moves nothing while `apply_buy` still
    /// debits `reserve_token`, driving the pool's token reserve to zero for free.
    #[test]
    #[should_panic(expected = "buyer collateral: wrong program")]
    fn buy_rejects_a_buyer_collateral_holding_owned_by_a_foreign_program() {
        let state = open_pool();
        let _ = buy(
            pool_account(&state),
            token_vault(1_000_000_000),
            collateral_vault(50_000),
            // Its DATA names the pool's collateral definition, so the definition
            // check passes; the owner is what gives it away, and the owner is the
            // one field the buyer cannot fake by writing bytes.
            authorized(&holding_owned_by(
                BUYER_COLL_HOLDING_ID,
                COLLATERAL_DEF_ID,
                COLLATERAL_IN,
                HOSTILE_PROGRAM_ID,
            )),
            holding(BUYER_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0),
            COLLATERAL_IN,
            0,
            LBP_PROGRAM_ID,
            T_BUY as i64,
            1,
        );
    }

    /// Same anchor, private path: `buy_disposable` shares `finalize_buy`, and a
    /// private buy is where a free token-reserve drain would be hardest to spot.
    #[test]
    #[should_panic(expected = "buyer collateral: wrong program")]
    fn disposable_buy_rejects_a_buyer_collateral_holding_owned_by_a_foreign_program() {
        let state = open_pool();
        let _ = buy_disposable(
            pool_account(&state),
            token_vault(1_000_000_000),
            collateral_vault(50_000),
            authorized(&holding_owned_by(
                BUYER_COLL_HOLDING_ID,
                COLLATERAL_DEF_ID,
                COLLATERAL_IN,
                HOSTILE_PROGRAM_ID,
            )),
            holding(BUYER_TOKEN_HOLDING_ID, TOKEN_DEF_ID, 0),
            COLLATERAL_IN,
            0,
            T_BUY,
            LBP_PROGRAM_ID,
        );
    }
}

/// `CreateSale` definition-pin tests. Both deposit legs are dispatched to the
/// program that owns a pinned DEFINITION account, never to one read off a
/// creator-supplied holding: the leg is what claims and types the vault, so the
/// program that runs it owns that vault for the pool's whole life.
mod create_sale_tests {
    use lbp_core::{
        compute_collateral_vault_pda, compute_creator_index_pda, compute_creator_index_pda_seed,
        compute_pool_pda, compute_token_vault_pda, CreatorIndex, PoolState, MAX_INDEXED_POOLS,
    };
    use lee_core::{
        account::{Account, AccountId, AccountWithMetadata, Data, Nonce},
        program::{AccountPostState, ChainedCall, Claim, ProgramId},
    };

    use token_core::TokenHolding;

    use super::fixtures::*;
    use crate::create_sale::create_sale;

    const NONCE: u64 = 7;
    /// Any non-default id: `create_sale` only rejects the default one.
    const ATA_PROGRAM_ID: ProgramId = [3u32; 8];
    const TOKEN_DEPOSIT: u128 = 1_000_000_000;
    const COLLATERAL_SEED: u128 = 50_000;

    /// `(pool, token_vault, collateral_vault)` PDAs for the fixture's parameters.
    /// Real derivations, because `create_sale` asserts every one of them before it
    /// reaches the definition checks.
    fn pdas() -> (AccountId, AccountId, AccountId) {
        let pool_id =
            compute_pool_pda(LBP_PROGRAM_ID, TOKEN_DEF_ID, COLLATERAL_DEF_ID, CREATOR_ID, NONCE);
        (
            pool_id,
            compute_token_vault_pda(LBP_PROGRAM_ID, pool_id),
            compute_collateral_vault_pda(LBP_PROGRAM_ID, pool_id),
        )
    }

    fn run(
        token_definition: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        run_full(
            token_definition,
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            creator_token_holding,
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
        )
    }

    /// [`run`], with the accounts its security asserts actually guard exposed:
    /// the collateral definition, the creator's collateral holding, and the
    /// treasury - both the id declared in the instruction and the ACCOUNT now
    /// passed alongside it, which are separate parameters precisely so the assert
    /// that binds them can be tested.
    fn run_full(
        token_definition: AccountWithMetadata,
        collateral_definition: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        creator_collateral_holding: AccountWithMetadata,
        treasury_id: AccountId,
        treasury: AccountWithMetadata,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        run_indexed(
            token_definition,
            collateral_definition,
            creator_token_holding,
            creator_collateral_holding,
            treasury_id,
            treasury,
            uninitialized(creator_index_id()),
        )
    }

    /// The creator's own index PDA - the account discovery reads instead of
    /// deriving and probing thousands of candidates.
    fn creator_index_id() -> AccountId {
        compute_creator_index_pda(LBP_PROGRAM_ID, CREATOR_ID)
    }

    /// An index this program already owns, holding `pool_ids` - the shape every
    /// pool after a creator's first one appends to.
    fn claimed_index(pool_ids: Vec<AccountId>) -> AccountWithMetadata {
        let mut index = CreatorIndex::new(CREATOR_ID);
        index.pool_ids = pool_ids;
        AccountWithMetadata {
            account: Account {
                program_owner: LBP_PROGRAM_ID,
                balance: 0u128,
                data: Data::from(&index),
                nonce: Nonce(0),
            },
            is_authorized: false,
            account_id: creator_index_id(),
        }
    }

    /// [`run_full`] with the creator-index slot exposed.
    fn run_indexed(
        token_definition: AccountWithMetadata,
        collateral_definition: AccountWithMetadata,
        creator_token_holding: AccountWithMetadata,
        creator_collateral_holding: AccountWithMetadata,
        treasury_id: AccountId,
        treasury: AccountWithMetadata,
        creator_index: AccountWithMetadata,
    ) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
        let (pool_id, token_vault_id, collateral_vault_id) = pdas();
        create_sale(
            uninitialized(pool_id),
            uninitialized(token_vault_id),
            uninitialized(collateral_vault_id),
            token_definition,
            collateral_definition,
            creator_token_holding,
            creator_collateral_holding,
            creator_account(true, CREATOR_ID),
            treasury,
            creator_index,
            COLLATERAL_DEF_ID,
            treasury_id,
            "Sample Token".to_string(),
            "SMPL".to_string(),
            TOKEN_DEPOSIT,
            COLLATERAL_SEED,
            super::w(99, 100),
            super::w(1, 100),
            1_000,     // t_start_ms
            1_000_000, // t_end_ms
            500,       // fee_bps
            0,         // block_token_ceiling
            [0u8; 32], // allowlist_root: open sale
            false,     // fixed_price
            0,         // min_duration_ms
            NONCE,
            ATA_PROGRAM_ID,
            LBP_PROGRAM_ID,
            0, // clock_ts
        )
    }

    /// The collateral definition must really be a definition. Nothing else in
    /// this path parses it - the seed leg is a `Transfer` - so without the assert
    /// an uninitialized account surfaces only as LEZ's opaque "Unknown program"
    /// rejection of a chained call dispatched off that very account.
    #[test]
    #[should_panic(expected = "collateral_definition: not Fungible")]
    fn create_sale_rejects_a_collateral_definition_that_is_not_a_definition() {
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition_with_data(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID, Data::default()),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
        );
    }

    /// The seed leg's sender must belong to the program that leg is dispatched
    /// on, or LEZ rejects it deep inside the chained call as a foreign program's
    /// write - naming nothing about the pool.
    #[test]
    #[should_panic(expected = "creator collateral: wrong program")]
    fn create_sale_rejects_a_creator_collateral_holding_under_a_foreign_program() {
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding_owned_by(
                CREATOR_COLL_HOLDING_ID,
                COLLATERAL_DEF_ID,
                COLLATERAL_SEED,
                HOSTILE_PROGRAM_ID,
            ),
            TREASURY_ID,
            treasury(),
        );
    }

    /// The treasury is a standalone fee sink. Aliasing it onto the pool or either
    /// vault would write token-holding bytes over live state, or corrupt the
    /// pool's own accounting - the bonding curve has one test per alias and the
    /// LBP had none.
    #[test]
    #[should_panic(expected = "treasury")]
    fn create_sale_rejects_a_treasury_aliasing_the_pool() {
        let (pool_id, _, _) = pdas();
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            pool_id,
            treasury_holding(pool_id),
        );
    }

    #[test]
    #[should_panic(expected = "treasury")]
    fn create_sale_rejects_a_treasury_aliasing_the_token_vault() {
        let (_, token_vault_id, _) = pdas();
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            token_vault_id,
            treasury_holding(token_vault_id),
        );
    }

    #[test]
    #[should_panic(expected = "treasury")]
    fn create_sale_rejects_a_treasury_aliasing_the_collateral_vault() {
        let (_, _, collateral_vault_id) = pdas();
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            collateral_vault_id,
            treasury_holding(collateral_vault_id),
        );
    }

    /// The project token under its own program, the collateral under another:
    /// each deposit leg follows its own definition account.
    #[test]
    fn create_sale_dispatches_each_deposit_on_its_pinned_definition() {
        let (post_states, calls) = run(
            definition(TOKEN_DEF_ID, PROJECT_TOKEN_PROGRAM_ID),
            holding_owned_by(
                CREATOR_TOKEN_HOLDING_ID,
                TOKEN_DEF_ID,
                TOKEN_DEPOSIT,
                PROJECT_TOKEN_PROGRAM_ID,
            ),
        );
        assert_eq!(calls.len(), 2, "the token deposit, then the collateral seed");
        assert_eq!(
            calls[0].program_id, PROJECT_TOKEN_PROGRAM_ID,
            "the token deposit - the leg that CLAIMS the token vault - must run under the \
             program that owns the pinned project-token definition"
        );
        assert_eq!(calls[1].program_id, TOKEN_PROGRAM_ID);
        let state = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(state.token_definition_id, TOKEN_DEF_ID);
        assert_eq!(state.treasury_owed, 0, "a fresh pool owes the treasury nothing");
        assert_eq!(post_states.len(), 10, "one post-state per declared account (the guest echoes the clock)");
    }

    /// `token_definition_id` is read out of the creator's holding, which a program
    /// the creator deployed may write freely. Pinning the definition account to it
    /// is what stops the id in the pool from naming one token while the vault is
    /// claimed by another program.
    #[test]
    #[should_panic(expected = "token_definition: wrong definition id")]
    fn create_sale_rejects_a_token_definition_that_is_not_the_one_the_holding_declares() {
        let _ = run(
            definition(AccountId::new([77u8; 32]), PROJECT_TOKEN_PROGRAM_ID),
            holding_owned_by(
                CREATOR_TOKEN_HOLDING_ID,
                TOKEN_DEF_ID,
                TOKEN_DEPOSIT,
                PROJECT_TOKEN_PROGRAM_ID,
            ),
        );
    }

    /// The attack the pin closes: a creator names a real, valuable project token
    /// and deposits from a holding owned by a program they deployed, so the token
    /// vault - and everything it hands buyers - would belong to that program. A
    /// buyer whose token holding is uninitialised would have it claimed by that
    /// program and filled with a holding that reads as the real token and is not.
    #[test]
    #[should_panic(expected = "creator token: wrong program")]
    fn create_sale_rejects_a_creator_token_holding_owned_by_another_program() {
        let _ = run(
            definition(TOKEN_DEF_ID, PROJECT_TOKEN_PROGRAM_ID),
            holding_owned_by(
                CREATOR_TOKEN_HOLDING_ID,
                TOKEN_DEF_ID,
                TOKEN_DEPOSIT,
                HOSTILE_PROGRAM_ID,
            ),
        );
    }

    /// The deposit leg is a `Transfer`, which never parses the definition account,
    /// so without this assert an account that is not a definition at all would
    /// surface only as LEZ's opaque "Unknown program" rejection of the chained
    /// call.
    #[test]
    #[should_panic(expected = "token_definition: not Fungible")]
    fn create_sale_rejects_a_token_definition_account_that_is_not_a_definition() {
        let _ = run(
            holding_owned_by(TOKEN_DEF_ID, TOKEN_DEF_ID, 0, PROJECT_TOKEN_PROGRAM_ID),
            holding_owned_by(
                CREATOR_TOKEN_HOLDING_ID,
                TOKEN_DEF_ID,
                TOKEN_DEPOSIT,
                PROJECT_TOKEN_PROGRAM_ID,
            ),
        );
    }

    // ---- F27: an NFT definition on either side ------------------------------
    //
    // A pool prices divisible reserves. The parse asserts used to take any
    // `TokenDefinition`, so an NFT collection could be named on either side and
    // the pool would be created; what went wrong only surfaced later, in the
    // token program, against a deposit the creator could no longer get back.

    /// The collateral side. The treasury pin below makes this unconstructible on
    /// its own - a `TokenHolding::Fungible` can never name a NonFungible
    /// definition - but the assert says it where the definition is read, so the
    /// two cannot drift apart.
    #[test]
    #[should_panic(expected = "collateral_definition: not Fungible")]
    fn create_sale_rejects_a_nonfungible_collateral_definition() {
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            nft_definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
        );
    }

    /// The project-token side: an NFT collection cannot be sold by this program
    /// either, and a pool that accepted one would take the creator's deposit and
    /// never hand a buyer anything.
    #[test]
    #[should_panic(expected = "token_definition: not Fungible")]
    fn create_sale_rejects_a_nonfungible_project_token_definition() {
        let _ = run(
            nft_definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
        );
    }

    // ---- F26: the treasury must already be able to receive ------------------
    //
    // `SweepTreasury` is the only instruction that can ever pay the escrowed
    // at-close fee out. Every shape below would make it fail forever, and none of
    // them could be caught anywhere later, because nothing rewrites
    // `treasury_id`. The uninitialised case is the sharp one: settling into it
    // meant CLAIMING it, a claim LEZ admits only for an account that signed for
    // itself and only while its pre-state is pristine - so the treasury's own key
    // was both the only way in and, one unrelated signature later, the thing that
    // locked the escrow for good. (No stranger could: LEZ refuses to modify a
    // DEFAULT-owned account without a claim.)

    /// The declared id is what goes into the pool state and what every later
    /// sweep is pinned to, so validating some other account would validate
    /// nothing at all.
    #[test]
    #[should_panic(expected = "does not match the treasury_id declared in the instruction")]
    fn create_sale_rejects_a_treasury_account_that_is_not_the_declared_id() {
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury_holding(AccountId::new([66u8; 32])),
        );
    }

    /// The lock itself: an id nothing has initialised. Accepting it is what left
    /// the escrow one stray signature by the treasury's own key away from being
    /// bricked permanently, so it is refused here, where the creator can still
    /// pick a different treasury for free.
    #[test]
    #[should_panic(expected = "not an initialised Fungible token holding")]
    fn create_sale_rejects_an_uninitialised_treasury() {
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            uninitialized(TREASURY_ID),
        );
    }

    /// Same assert, the NFT arm: a holding that parses but is not `Fungible`
    /// cannot receive a divisible fee.
    #[test]
    #[should_panic(expected = "not an initialised Fungible token holding")]
    fn create_sale_rejects_an_nft_treasury() {
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            nft_holding(TREASURY_ID, COLLATERAL_DEF_ID),
        );
    }

    /// The at-close fee is paid in collateral, so a treasury holding the PROJECT
    /// token reverts every sweep on the token program's own definition check.
    #[test]
    #[should_panic(expected = "treasury holds a different token")]
    fn create_sale_rejects_a_treasury_holding_the_wrong_token() {
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            holding(TREASURY_ID, TOKEN_DEF_ID, 0),
        );
    }

    /// Right definition, wrong program. The sweep's fee leg is dispatched on the
    /// collateral vault's owner - the collateral definition's program - and LEZ
    /// refuses that program's write to an account it does not own, so this shape
    /// is just as permanently unsweepable as the wrong definition.
    #[test]
    #[should_panic(expected = "not owned by the collateral definition's own token program")]
    fn create_sale_rejects_a_treasury_under_a_foreign_token_program() {
        let _ = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            holding_owned_by(TREASURY_ID, COLLATERAL_DEF_ID, 0, HOSTILE_PROGRAM_ID),
        );
    }

    /// The accepting case, and the one thing this instruction must NOT do to the
    /// treasury: it validates the account and echoes it byte for byte. A
    /// `CreateSale` that wrote to the fee sink would be a creator-controlled
    /// write to a protocol account.
    #[test]
    fn create_sale_accepts_an_initialised_treasury_and_echoes_it_unchanged() {
        let expected = treasury();
        let (post_states, _) = run_full(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
        );
        assert_eq!(
            post_states.len(),
            10,
            "pool, two vaults, two definitions, two creator holdings, creator, treasury, \
             creator_index"
        );
        assert_eq!(
            *post_states[8].account(),
            expected.account,
            "the treasury is read-only here and must be echoed unchanged"
        );
        let state = PoolState::try_from(&post_states[0].account().data).unwrap();
        assert_eq!(state.treasury_id, TREASURY_ID, "the validated id is what the pool pins");
        // The type pin is what makes `sweep_treasury`'s DEFAULT-owned branch
        // unreachable: a Fungible holding is owned by the token program, and no
        // LEZ instruction un-initialises one.
        assert!(matches!(
            TokenHolding::try_from(&post_states[8].account().data),
            Ok(TokenHolding::Fungible { .. })
        ));
    }

    // ---- the creator index --------------------------------------------------
    //
    // The append that turns discovery from thousands of derived probes into one
    // read per creator. Every test below fails if the guard it names is removed.

    /// The first pool: the index does not exist yet, so this claims it.
    #[test]
    fn create_sale_claims_a_fresh_creator_index_and_records_the_pool() {
        let (pool_id, _, _) = pdas();
        let (post_states, _) = run(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
        );
        let index_post = &post_states[9];
        assert_eq!(
            index_post.required_claim(),
            Some(Claim::Pda(compute_creator_index_pda_seed(CREATOR_ID))),
            "the first pool must claim the index PDA, or the write is rejected"
        );
        let index = CreatorIndex::try_from(&index_post.account().data).unwrap();
        assert_eq!(
            index.pool_ids,
            vec![pool_id],
            "the pool this instruction creates is what gets recorded"
        );
        assert_eq!(index.creator, CREATOR_ID, "a fresh index says whose it is");
    }

    /// The second pool onward: the account is already ours, so the append is an
    /// ordinary write and must NOT re-claim (LEZ only claims a pristine account,
    /// so a claim here would reject the whole transaction).
    #[test]
    fn create_sale_appends_to_an_existing_index_without_re_claiming() {
        let (pool_id, _, _) = pdas();
        let earlier = AccountId::new([55u8; 32]);
        let (post_states, _) = run_indexed(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
            claimed_index(vec![earlier]),
        );
        let index_post = &post_states[9];
        assert_eq!(index_post.required_claim(), None, "an owned account is not re-claimed");
        let index = CreatorIndex::try_from(&index_post.account().data).unwrap();
        assert_eq!(
            index.pool_ids,
            vec![earlier, pool_id],
            "an append, not an overwrite: an index that dropped earlier pools would \
             lose exactly the history discovery reads it for"
        );
    }

    /// The submitter picks non-signer account ids. Without the pin, a create
    /// could be aimed at another creator's index and write this pool into it -
    /// so `my-pools` would list a pool its reader never made.
    #[test]
    #[should_panic(expected = "not the index PDA of the signing creator")]
    fn create_sale_rejects_an_index_belonging_to_another_creator() {
        let mut foreign = claimed_index(vec![]);
        foreign.account_id = compute_creator_index_pda(LBP_PROGRAM_ID, AccountId::new([88u8; 32]));
        let _ = run_indexed(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
            foreign,
        );
    }

    /// An index PDA that is non-default and still DEFAULT-owned - here, one
    /// carrying a native balance and nothing else.
    ///
    /// A stranger cannot actually produce this: LEZ rejects any transaction that
    /// modifies a DEFAULT-owned account without claiming it, and only this
    /// program can claim its own derivation. Nor can anyone else - after genesis
    /// no transaction puts a balance on a DEFAULT-owned account at all, whoever
    /// sends it. The state is unreachable and the
    /// assert is defence in depth, so what this test pins is that the guard
    /// holds and that the message describes the state instead of telling the
    /// creator to abandon their identity for a PDA nobody could have taken.
    #[test]
    #[should_panic(expected = "exists but is not owned by this program")]
    fn create_sale_rejects_a_creator_index_that_is_unowned_and_non_default() {
        let unowned = AccountWithMetadata {
            account: Account { balance: 1u128, ..Account::default() },
            is_authorized: false,
            account_id: creator_index_id(),
        };
        let _ = run_indexed(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
            unowned,
        );
    }

    /// Same assert, the other arm: an initialised account owned by someone else.
    /// Unconstructible for a PDA of this program - `Claim::Pda` is admitted only
    /// against the claiming program's own derivation - and refused anyway, since
    /// writing index bytes into a foreign account is a data modification LEZ
    /// would reject deeper down with no mention of the index at all.
    #[test]
    #[should_panic(expected = "exists but is not owned by this program")]
    fn create_sale_rejects_a_creator_index_owned_by_another_program() {
        let mut foreign = claimed_index(vec![]);
        foreign.account.program_owner = HOSTILE_PROGRAM_ID;
        let _ = run_indexed(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
            foreign,
        );
    }

    /// The length bound, reached through the real instruction: without it the
    /// failure is `DataTooBigError` raised inside a `Data` conversion, naming
    /// neither the account nor anything the creator could do about it.
    #[test]
    #[should_panic(expected = "creator pool index is full")]
    fn create_sale_rejects_an_index_that_is_already_full() {
        let _ = run_indexed(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
            claimed_index(vec![AccountId::new([55u8; 32]); MAX_INDEXED_POOLS]),
        );
    }

    /// Unreachable while the PDA pin holds, and asserted anyway: a future seed
    /// change or a collision must never merge one creator's pool list into
    /// another's. The stored `creator` is what makes that checkable at all.
    #[test]
    #[should_panic(expected = "creator index belongs to a different creator")]
    fn create_sale_rejects_an_index_whose_stored_creator_is_someone_else() {
        let mut other = CreatorIndex::new(AccountId::new([88u8; 32]));
        other.pool_ids = vec![AccountId::new([55u8; 32])];
        let mut account = claimed_index(vec![]);
        account.account.data = Data::from(&other);
        let _ = run_indexed(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
            account,
        );
    }

    /// The magic is what stops bytes that merely Borsh-decode from being read as
    /// a pool list and appended to. Without the authenticating decode, this
    /// account's contents would be silently reinterpreted and rewritten.
    #[test]
    #[should_panic(expected = "does not hold an LBP creator index")]
    fn create_sale_rejects_an_index_account_holding_something_else() {
        let mut account = claimed_index(vec![]);
        account.account.data = Data::from(&PoolState::default());
        let _ = run_indexed(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            definition(COLLATERAL_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
            holding(CREATOR_COLL_HOLDING_ID, COLLATERAL_DEF_ID, COLLATERAL_SEED),
            TREASURY_ID,
            treasury(),
            account,
        );
    }

    /// The index records IDS, never state: a copy of the pool's reserves would be
    /// stale after the first buy, and a reader that trusted it would quote from
    /// it. What the account holds must stay derivable-from-nothing-else.
    #[test]
    fn the_creator_index_holds_only_ids() {
        let (pool_id, _, _) = pdas();
        let (post_states, _) = run(
            definition(TOKEN_DEF_ID, TOKEN_PROGRAM_ID),
            holding(CREATOR_TOKEN_HOLDING_ID, TOKEN_DEF_ID, TOKEN_DEPOSIT),
        );
        let data = &post_states[9].account().data;
        assert_eq!(
            data.as_ref().len(),
            8 + 2 + 32 + 4 + 32,
            "magic, version, creator, and one id - no pool STATE belongs in this account"
        );
        assert_eq!(CreatorIndex::try_from(data).unwrap().pool_ids, vec![pool_id]);
    }
}
