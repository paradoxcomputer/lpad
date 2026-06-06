//! Probe: learn harness behaviour (is CLOCK_01 seeded at genesis? what is a
//! fresh account's default?). Informs the clock handling in real tests.

use nssa::V03State;
use nssa_core::account::Account;

#[test]
fn probe_clock_and_defaults() {
    let state = V03State::new_with_genesis_accounts(&[], vec![], 0);
    let clock = state.get_account_by_id(bonding_curve_core::CLOCK_01);
    println!(
        "CLOCK_01: owner={:?} data_len={} is_default={}",
        clock.program_owner,
        clock.data.as_ref().len(),
        clock == Account::default()
    );
    // Decode if it looks like ClockData (16 bytes).
    if clock.data.as_ref().len() >= 16 {
        let d = clock.data.as_ref();
        let block_id = u64::from_le_bytes(d[0..8].try_into().unwrap());
        let ts = i64::from_le_bytes(d[8..16].try_into().unwrap());
        println!("CLOCK_01 decoded: block_id={block_id} timestamp={ts}");
    }
    println!("bonding_curve program id = {:?}", bonding_curve_methods::BONDING_CURVE_ID);
    println!("token program id = {:?}", token_methods::TOKEN_ID);
}
