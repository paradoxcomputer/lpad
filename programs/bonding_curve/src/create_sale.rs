//! `CreateSale` - open a new bonding-curve sale.
//!
//! Account order (top-level): `[sale, token_vault, collateral_vault,
//! collateral_definition, creator_token_holding, creator, clock]`.
//! - `sale`                  - uninitialized PDA, claimed by this program.
//! - `token_vault`           - uninitialized PDA; receives `D + R` project tokens.
//! - `collateral_vault`      - uninitialized PDA; initialized empty here so the
//!   ATA buy path (`ata::Transfer`, which rejects a default recipient) works on
//!   the very first buy, and so the vault is typed to the collateral definition
//!   up-front (no lazy-init token-type inheritance). Mirrors the LBP pre-seed.
//! - `collateral_definition` - the sale's collateral token definition (needed to
//!   initialize the empty collateral vault holding).
//! - `creator_token_holding` - creator's project-token holding (sender, signer).
//! - `creator`               - creator identity account (signer); stored for withdrawal auth.
//! - `clock`                 - read-only CLOCK_01, echoed unchanged.

use bonding_curve_core::{
    compute_collateral_vault_pda, compute_collateral_vault_pda_seed, compute_sale_pda,
    compute_token_vault_pda, compute_token_vault_pda_seed, read_fungible, SaleState, SaleStatus,
    MAX_FEE_BPS, MAX_VIRT_COLLATERAL,
};
use nssa_core::{
    account::{Account, AccountId, AccountWithMetadata, Data},
    program::{AccountPostState, ChainedCall, Claim, ProgramId, DEFAULT_PROGRAM_ID},
};

use crate::util::authorized;

#[expect(clippy::too_many_arguments, reason = "fixed protocol account/param list")]
#[must_use]
pub fn create_sale(
    sale: AccountWithMetadata,
    token_vault: AccountWithMetadata,
    collateral_vault: AccountWithMetadata,
    collateral_definition: AccountWithMetadata,
    creator_token_holding: AccountWithMetadata,
    creator: AccountWithMetadata,
    collateral_definition_id: AccountId,
    treasury_id: AccountId,
    token_name: String,
    token_symbol: String,
    sale_quantity: u128,
    dex_seed_quantity: u128,
    virt_token: u128,
    virt_collateral: u128,
    fee_bps: u128,
    one_directional: bool,
    end_timestamp_ms: u64,
    min_duration_ms: u64,
    nonce: u64,
    ata_program_id: ProgramId,
    self_program_id: ProgramId,
    clock_ts: i64,
) -> (Vec<AccountPostState>, Vec<ChainedCall>) {
    // --- parameter validation -------------------------------------------------
    assert!(fee_bps <= MAX_FEE_BPS, "fee_bps exceeds the maximum allowed");
    assert!(sale_quantity > 0, "sale quantity D must be positive");
    assert!(virt_collateral > 0, "virtual collateral Vc must be positive");
    assert!(
        virt_token > sale_quantity,
        "virtual token reserve Vt must exceed sale quantity D"
    );
    let k = virt_token
        .checked_mul(virt_collateral)
        .expect("k = Vt * Vc overflows u128 - choose smaller virtual reserves");
    // Vc is synthetic (no real deposit backs it), so a creator can set it freely.
    // Bound it to the Q64.64 spot-price domain here: spot_price_q64 (run on every
    // public buy/sell via record_observation) asserts the same bound, so an
    // out-of-domain Vc would otherwise revert every public swap once the sale is
    // live. Reject it at creation instead. Mirrors the LBP MAX_RESERVE defense.
    //
    // The live Vc *grows* on every buy (apply_buy does Vc += c_eff), heading toward
    // its peak when all D tokens are sold and Vt has fallen to `virt_token -
    // sale_quantity`. `ceil_div(k, virt_token - sale_quantity)` is only the
    // *idealized* peak that assumes the constant product stays exactly at k; in the
    // discrete engine each buy floors tokens_out (buy_tokens_out) while adding the
    // full c_eff, so Vt*Vc grows strictly above k and the *realized* live Vc
    // overshoots that idealized value (empirically by up to ~1.62x, independent of
    // scale). Bounding only the idealized peak would let a sale whose params pass
    // still drive Vc across 2^64 mid-curve, where every further buy/sell/close
    // would revert (and, with end_timestamp_ms == 0, the sale could never close),
    // permanently locking deposited collateral. Reserve 2x headroom - which covers
    // the worst-case ~1.62x rounding overshoot - so the realized Vc stays inside
    // the Q64.64 domain on every reachable trade path. (The spot-price oracle also
    // saturates rather than asserting, so an observation can never wrap; this bound
    // additionally protects the private path, which records no observation.)
    assert!(
        bonding_curve_core::ceil_div(k, virt_token - sale_quantity)
            .checked_mul(2)
            .is_some_and(|v| v < MAX_VIRT_COLLATERAL),
        "max reachable Vc exceeds the Q64.64 spot-price domain (choose larger Vt-D or smaller Vc)"
    );
    assert!(creator.is_authorized, "creator must authorize sale creation");
    // SECURITY: BuyAta/SellAta dispatch the user's token/collateral leg through
    // this pinned program (an owner-authorized PDA spend). A zero/default id can
    // never name a real ATA program, so it could only resolve to a no-op leg that
    // skips the actual token::Transfer while the vault legs still pay out -
    // draining the collateral vault. Reject it here. (The canonical-ATA image id
    // is not a compile-time constant in this crate - programs are addressed by
    // guest image id, unlike the sequencer-seeded CLOCK_01 AccountId - so the SDK
    // validates the pinned id against the deployed ATA program for participants.)
    assert!(
        ata_program_id != DEFAULT_PROGRAM_ID,
        "ata_program_id must be a real ATA program (not the default/zero id)"
    );
    if end_timestamp_ms != 0 {
        let now = u64::try_from(clock_ts.max(0)).expect("clock timestamp must be non-negative");
        assert!(
            end_timestamp_ms >= now.saturating_add(min_duration_ms),
            "end timestamp must be at least min_duration_ms in the future"
        );
    }

    // --- identity / definitions ----------------------------------------------
    let (token_definition_id, creator_balance) =
        read_fungible(&creator_token_holding, "CreateSale: creator token holding");
    let total_deposit = sale_quantity
        .checked_add(dex_seed_quantity)
        .expect("D + R overflows u128");
    assert!(
        creator_balance >= total_deposit,
        "creator holding does not cover D + R project tokens"
    );
    assert!(
        token_definition_id != collateral_definition_id,
        "project and collateral tokens must differ"
    );

    // --- PDA checks -----------------------------------------------------------
    let sale_id = compute_sale_pda(
        self_program_id,
        token_definition_id,
        collateral_definition_id,
        creator.account_id,
        nonce,
    );
    assert_eq!(sale.account_id, sale_id, "sale account id does not match PDA");
    assert_eq!(
        sale.account,
        Account::default(),
        "sale account must be uninitialized"
    );
    let token_vault_id = compute_token_vault_pda(self_program_id, sale_id);
    assert_eq!(
        token_vault.account_id, token_vault_id,
        "token vault id does not match PDA"
    );
    let collateral_vault_id = compute_collateral_vault_pda(self_program_id, sale_id);
    assert_eq!(
        collateral_vault.account_id, collateral_vault_id,
        "collateral vault id does not match PDA"
    );
    assert_eq!(
        collateral_vault.account,
        Account::default(),
        "collateral vault must be uninitialized"
    );
    assert_eq!(
        collateral_definition.account_id, collateral_definition_id,
        "collateral definition account does not match the declared collateral definition id"
    );
    // Fees must not sweep into the protocol's own vaults (which would corrupt the
    // raised-collateral accounting). The treasury is a separate fee sink.
    assert!(
        treasury_id != collateral_vault_id && treasury_id != token_vault_id,
        "treasury must differ from the sale's token and collateral vaults"
    );

    // --- build sale state -----------------------------------------------------
    let state = SaleState {
        creator: creator.account_id,
        token_definition_id,
        collateral_definition_id,
        token_vault_id,
        collateral_vault_id,
        treasury_id, // admin-configured fee sink; every buy/sell validates the passed treasury against this
        ata_program_id, // pinned here (rejected if default); BuyAta/SellAta reject any other ATA program, the SDK rejects a non-canonical pin
        fee_bps,
        one_directional,
        end_timestamp_ms,
        min_duration_ms,
        nonce,
        created_ts_ms: clock_ts,
        virt_token,
        virt_collateral,
        k,
        sale_reserve_initial: sale_quantity,
        dex_seed_reserve: dex_seed_quantity,
        sale_reserve: sale_quantity,
        real_collateral: 0,
        status: SaleStatus::Open,
        cum_collateral_in: 0,
        cum_fees: 0,
        buy_count: 0,
        sell_count: 0,
        obs: Vec::new(),
        token_name,
        token_symbol,
    };

    let mut sale_post = sale.account.clone();
    sale_post.data = Data::from(&state);
    let sale_post = AccountPostState::new_claimed(
        sale_post,
        Claim::Pda(bonding_curve_core::compute_sale_pda_seed(
            token_definition_id,
            collateral_definition_id,
            creator.account_id,
            nonce,
        )),
    );

    // --- chained call: deposit D + R into the token vault --------------------
    let token_program_id = creator_token_holding.account.program_owner;
    let deposit = ChainedCall::new(
        token_program_id,
        vec![creator_token_holding.clone(), authorized(&token_vault)],
        &token_core::Instruction::Transfer {
            amount_to_transfer: total_deposit,
        },
    )
    .with_pda_seeds(vec![compute_token_vault_pda_seed(sale_id)]);

    // --- chained call: initialize the (empty) collateral vault ----------------
    // Claims + initializes the collateral-vault PDA as an empty Fungible holding
    // of the sale's collateral definition (mirrors LBP's pre-seed). This is what
    // lets the ATA buy path - `ata::Transfer`, which rejects a default recipient
    // - deposit into the vault on the very first buy.
    let collateral_token_program_id = collateral_definition.account.program_owner;
    let init_collateral_vault = ChainedCall::new(
        collateral_token_program_id,
        vec![collateral_definition.clone(), authorized(&collateral_vault)],
        &token_core::Instruction::InitializeAccount,
    )
    .with_pda_seeds(vec![compute_collateral_vault_pda_seed(sale_id)]);

    let post_states = vec![
        sale_post,
        AccountPostState::new(token_vault.account),
        AccountPostState::new(collateral_vault.account),
        AccountPostState::new(collateral_definition.account),
        AccountPostState::new(creator_token_holding.account),
        AccountPostState::new(creator.account),
    ];

    (post_states, vec![deposit, init_collateral_vault])
}
