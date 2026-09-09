#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{token, Address, Env};

/// Same wire-format encoder as `polaris-market/src/test.rs` — duplicated
/// deliberately, not extracted: this is test-only scaffolding for a mock
/// payload format, not production logic, so there's no drift risk the
/// shared `polaris-ctf-math` crate exists to prevent.
fn build_payload(feed_id: u32, price: i64, exponent: i16, feed_ts_micros: u64) -> std::vec::Vec<u8> {
    let mut b = std::vec::Vec::new();
    b.extend_from_slice(&2_479_346_549u32.to_le_bytes()); // magic
    b.extend_from_slice(&feed_ts_micros.to_le_bytes());
    b.push(3); // channel = FixedRate200ms
    b.push(1); // num_feeds
    b.extend_from_slice(&feed_id.to_le_bytes());
    b.push(3); // num_properties
    b.push(0); // property: price
    b.extend_from_slice(&(price as u64).to_le_bytes());
    b.push(4); // property: exponent
    b.extend_from_slice(&(exponent as u16).to_le_bytes());
    b.push(12); // property: feed_update_timestamp
    b.push(1); // exists = true
    b.extend_from_slice(&feed_ts_micros.to_le_bytes());
    b
}

const FEED_ID: u32 = 100;
const REFLECTOR_DECIMALS: u32 = 14;
const REFLECTOR_RESOLUTION_SECS: u32 = 300;
const REFLECTOR_MAX_STALENESS_SECS: u64 = 600;
const REFLECTOR_TOLERANCE_BPS: u32 = 150;

struct Harness {
    env: Env,
    market_id: Address,
    lazer_id: Address,
    reflector_id: Address,
    token_id: Address,
    admin: Address,
    treasury: Address,
}

fn default_price_oracle(env: &Env, lazer_id: &Address, reflector_id: &Address) -> PriceOracleConfig {
    PriceOracleConfig {
        lazer_contract: lazer_id.clone(),
        feed_id: FEED_ID,
        reflector: ReflectorConfig {
            contract: reflector_id.clone(),
            asset: soroban_sdk::Symbol::new(env, "XLM"),
            max_staleness_secs: REFLECTOR_MAX_STALENESS_SECS,
            tolerance_bps: REFLECTOR_TOLERANCE_BPS,
        },
    }
}

fn setup(initial_liquidity: i128, with_oracle: bool) -> Harness {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000);

    let admin = Address::generate(&env);
    let treasury = Address::generate(&env);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let token_id = sac.address();
    let token_admin = token::StellarAssetClient::new(&env, &token_id);

    let lazer_id = env.register(mock_lazer::MockLazer, ());
    let reflector_id = env.register(mock_reflector::MockReflector, ());
    let market_id = env.register(PolarisPerpetual, ());

    token_admin.mint(&admin, &(initial_liquidity * 1000));

    let price_oracle = if with_oracle {
        Some(default_price_oracle(&env, &lazer_id, &reflector_id))
    } else {
        None
    };

    let client = PolarisPerpetualClient::new(&env, &market_id);
    client.initialize(&admin, &token_id, &100u32, &20u32, &treasury, &initial_liquidity, &price_oracle);

    Harness { env, market_id, lazer_id, reflector_id, token_id, admin, treasury }
}

fn fund(h: &Harness, who: &Address, amount: i128) {
    token::StellarAssetClient::new(&h.env, &h.token_id).mint(who, &amount);
}

fn set_reflector_price_cents(h: &Harness, cents: i128) {
    let client = mock_reflector::MockReflectorClient::new(&h.env, &h.reflector_id);
    let price = cents * 10i128.pow(REFLECTOR_DECIMALS - 2);
    client.set_price(&price, &h.env.ledger().timestamp());
}

mod mock_lazer {
    use soroban_sdk::{contract, contractimpl, Bytes, Env};

    #[contract]
    pub struct MockLazer;

    #[contractimpl]
    impl MockLazer {
        pub fn verify_update(_env: Env, data: Bytes) -> Bytes {
            data
        }
    }
}

mod mock_reflector {
    use polaris_ctf_math::reflector::{Asset, PriceData};
    use soroban_sdk::{contract, contractimpl, symbol_short, Env};

    const PRICE_KEY: soroban_sdk::Symbol = symbol_short!("px");

    #[contract]
    pub struct MockReflector;

    #[contractimpl]
    impl MockReflector {
        pub fn set_price(env: Env, price: i128, timestamp: u64) {
            env.storage().instance().set(&PRICE_KEY, &(price, timestamp));
        }

        pub fn lastprice(env: Env, _asset: Asset) -> Option<PriceData> {
            env.storage().instance().get::<_, (i128, u64)>(&PRICE_KEY).map(|(price, timestamp)| PriceData { price, timestamp })
        }

        pub fn decimals(_env: Env) -> u32 {
            super::REFLECTOR_DECIMALS
        }

        pub fn resolution(_env: Env) -> u32 {
            super::REFLECTOR_RESOLUTION_SECS
        }
    }
}

fn assert_solvent(h: &Harness) {
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    let m = client.get_market();
    let bal = token::Client::new(&h.env, &h.token_id).balance(&h.market_id);
    assert_eq!(bal, m.total_supply as i128, "collateral balance must always equal total_supply");
}

// ---------- initialize ----------

#[test]
fn init_seeds_pool_and_pulls_liquidity_with_no_strike_or_expiry() {
    let h = setup(10_000, true);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    let m = client.get_market();
    assert_eq!(m.pool_yes, 10_000);
    assert_eq!(m.pool_no, 10_000);
    assert_eq!(m.total_supply, 10_000);
    assert_eq!(m.status, PerpetualStatus::Open);
    assert_eq!(token::Client::new(&h.env, &h.token_id).balance(&h.market_id), 10_000);
}

#[test]
fn init_without_a_price_oracle_is_allowed() {
    let h = setup(10_000, false);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    assert_eq!(client.get_price_oracle(), None);
}

#[test]
fn double_initialize_rejected() {
    let h = setup(10_000, true);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    let res = client.try_initialize(
        &h.admin, &h.token_id, &100u32, &20u32, &h.treasury, &10_000i128,
        &Some(default_price_oracle(&h.env, &h.lazer_id, &h.reflector_id)),
    );
    assert_eq!(res, Err(Ok(Error::AlreadyInitialized)));
}

// ---------- split / merge / buy / sell — identical mechanics to polaris-market ----------

#[test]
fn split_then_merge_round_trips_collateral() {
    let h = setup(10_000, true);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);

    client.split(&user, &2_000);
    assert_eq!(client.get_position(&user), (2_000, 2_000));
    client.merge(&user, &2_000);
    assert_eq!(client.get_position(&user), (0, 0));
    assert_eq!(token::Client::new(&h.env, &h.token_id).balance(&user), 5_000);
    assert_solvent(&h);
}

#[test]
fn buy_yes_moves_price_up() {
    let h = setup(10_000, true);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);

    let total_yes = client.buy(&user, &Prediction::Yes, &1_000, &0);
    assert!(total_yes > 1_000);
    let (yes_bps, no_bps) = client.get_price();
    assert!(yes_bps > 5_000);
    assert_eq!(yes_bps + no_bps, 10_000);
    assert_solvent(&h);
}

#[test]
fn sell_full_one_sided_position_returns_nonzero_collateral() {
    let h = setup(10_000, true);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);

    let shares = client.buy(&user, &Prediction::Yes, &1_000, &0);
    let collateral_back = client.sell(&user, &Prediction::Yes, &shares, &0);
    assert!(collateral_back > 0);
    assert_eq!(client.get_position(&user), (0, 0));
    assert_solvent(&h);
}

#[test]
fn buy_large_enough_to_exhaust_a_reserve_is_rejected() {
    let h = setup(10_000, true);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    let user = Address::generate(&h.env);
    fund(&h, &user, 1_000_000_000);

    let res = client.try_buy(&user, &Prediction::Yes, &101_000_000, &0);
    assert_eq!(res, Err(Ok(Error::PoolDepthExceeded)));
    assert_solvent(&h);
}

// ---------- no fixed expiry: trading keeps working indefinitely ----------

#[test]
fn trading_continues_correctly_across_a_very_long_time_window_with_no_expiry_ever_set() {
    // Proves "no fixed expiry" isn't just an unenforced field on a struct
    // that happens not to have one — advance the ledger clock far past what
    // would have been a normal polaris-market's expiry+grace_period, and
    // confirm buy/sell/split/merge are all still unconditionally available.
    let h = setup(10_000, true);
    let user = Address::generate(&h.env);
    fund(&h, &user, 1_000_000);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);

    let one_year_secs: u64 = 365 * 24 * 60 * 60;
    h.env.ledger().set_timestamp(h.env.ledger().timestamp() + one_year_secs);

    client.split(&user, &1_000);
    let shares = client.buy(&user, &Prediction::Yes, &1_000, &0);
    let _ = client.sell(&user, &Prediction::Yes, &shares, &0);
    client.merge(&user, &1_000);

    assert_eq!(client.get_market().status, PerpetualStatus::Open);
    assert_solvent(&h);
}

// ---------- record_price_checkpoint: zero economic effect ----------

#[test]
fn checkpoint_records_price_and_changes_nothing_else() {
    let h = setup(10_000, true);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);

    // Build up some real state first, so "nothing else changes" is a
    // meaningful assertion, not just "zero stays zero".
    client.buy(&user, &Prediction::Yes, &1_000, &0);
    let before = client.get_market();
    let before_user_pos = client.get_position(&user);

    let now = h.env.ledger().timestamp();
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, now * 1_000_000);
    let bytes = Bytes::from_slice(&h.env, &payload);
    set_reflector_price_cents(&h, 1_000_000);
    client.record_price_checkpoint(&bytes);

    let after = client.get_market();
    assert_eq!(after.last_price_cents, 1_000_000);
    assert_eq!(after.last_price_at, now);

    // Every other field byte-identical.
    assert_eq!(after.pool_yes, before.pool_yes);
    assert_eq!(after.pool_no, before.pool_no);
    assert_eq!(after.total_supply, before.total_supply);
    assert_eq!(after.status, before.status);
    assert_eq!(client.get_position(&user), before_user_pos);
    assert_solvent(&h);
}

#[test]
fn checkpoint_without_a_configured_oracle_is_rejected() {
    let h = setup(10_000, false);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    let now = h.env.ledger().timestamp();
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, now * 1_000_000);
    let bytes = Bytes::from_slice(&h.env, &payload);
    let res = client.try_record_price_checkpoint(&bytes);
    assert_eq!(res, Err(Ok(Error::PriceOracleNotConfigured)));
}

#[test]
fn checkpoint_rejects_on_gross_lazer_reflector_divergence() {
    let h = setup(10_000, true);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    let now = h.env.ledger().timestamp();
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, now * 1_000_000); // Lazer: $10,000.00
    let bytes = Bytes::from_slice(&h.env, &payload);

    set_reflector_price_cents(&h, 2_000_000); // Reflector: $20,000.00 — gross divergence
    let res = client.try_record_price_checkpoint(&bytes);
    assert_eq!(res, Err(Ok(Error::OracleDivergence)));
    assert_eq!(client.get_market().last_price_cents, 0, "a rejected checkpoint must not record anything");
}

// ---------- terminate ----------

#[test]
fn terminate_by_non_admin_rejected() {
    let h = setup(10_000, true);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    let not_admin = Address::generate(&h.env);
    let res = client.try_terminate(&not_admin);
    assert_eq!(res, Err(Ok(Error::Unauthorized)));
    assert_eq!(client.get_market().status, PerpetualStatus::Open);
}

#[test]
fn terminate_needs_no_price_and_pays_every_holder_at_merge_parity() {
    // Mirrors polaris-market's cancel_after_grace_refunds_a_matched_pair_
    // at_merge_parity — same formula, same reasoning, reused verbatim.
    let h = setup(10_000, true);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    client.split(&user, &1_000);

    client.terminate(&h.admin);
    assert_eq!(client.get_market().status, PerpetualStatus::Terminated);

    let before = token::Client::new(&h.env, &h.token_id).balance(&user);
    let payout = client.redeem(&user);
    assert_eq!(payout, 1_000);
    let after = token::Client::new(&h.env, &h.token_id).balance(&user);
    assert_eq!(after - before, 1_000);
    assert_solvent(&h);
}

#[test]
fn terminate_stays_solvent_for_every_holder_including_treasury() {
    // Same regression shape as polaris-market's
    // cancel_redeem_stays_solvent_for_every_holder_including_treasury —
    // redeem *every* legitimate holder, not just the first, since that's
    // exactly the case that caught the historical double-count bug.
    let h = setup(10_000, true);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    client.split(&user, &1_000);

    client.terminate(&h.admin);

    let user_payout = client.redeem(&user);
    assert_eq!(user_payout, 1_000);
    let treasury_payout = client.redeem(&h.treasury);
    assert_eq!(treasury_payout, 10_000); // (10_000 seed YES + 10_000 seed NO) / 2

    assert_eq!(user_payout + treasury_payout, 11_000); // == total_supply at termination, exactly
    assert_solvent(&h);
}

#[test]
fn double_terminate_rejected() {
    let h = setup(10_000, true);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    client.terminate(&h.admin);
    let res = client.try_terminate(&h.admin);
    assert_eq!(res, Err(Ok(Error::AlreadyFinalized)));
}

#[test]
fn trading_rejected_once_terminated() {
    let h = setup(10_000, true);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    client.terminate(&h.admin);

    assert_eq!(client.try_split(&user, &100), Err(Ok(Error::MarketNotOpen)));
    assert_eq!(client.try_merge(&user, &100), Err(Ok(Error::MarketNotOpen)));
    assert_eq!(client.try_buy(&user, &Prediction::Yes, &100, &0), Err(Ok(Error::MarketNotOpen)));
    assert_eq!(client.try_sell(&user, &Prediction::Yes, &100, &0), Err(Ok(Error::MarketNotOpen)));
}

#[test]
fn redeem_before_termination_rejected() {
    let h = setup(10_000, true);
    let user = Address::generate(&h.env);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    let res = client.try_redeem(&user);
    assert_eq!(res, Err(Ok(Error::NotFinalized)));
}

// ---------- fee curve — same shared implementation as polaris-market ----------

#[test]
fn fee_starts_at_base_and_decays_toward_min_as_volume_grows() {
    let h = setup(10_000, true);
    let client = PolarisPerpetualClient::new(&h.env, &h.market_id);
    let user = Address::generate(&h.env);
    fund(&h, &user, 1_000_000);

    assert_eq!(client.get_fee(), 100);
    client.split(&user, &10_000);
    assert_eq!(client.get_fee(), 60);
    assert_solvent(&h);
}
