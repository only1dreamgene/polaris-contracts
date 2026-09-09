#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{token, Address, Env};

/// Wire-format payload matching pyth-lazer-stellar-sdk's parser: magic(4) +
/// timestamp(8, LE µs) + channel(1) + num_feeds(1) + [feed_id(4) +
/// num_props(1) + properties...]. We only encode price(0), exponent(4), and
/// feed_update_timestamp(12) — the three properties `settle` reads.
fn build_payload(feed_id: u32, price: i64, exponent: i16, feed_ts_micros: u64) -> std::vec::Vec<u8> {
    let mut b = std::vec::Vec::new();
    b.extend_from_slice(&2_479_346_549u32.to_le_bytes()); // magic
    b.extend_from_slice(&feed_ts_micros.to_le_bytes()); // top-level timestamp
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

const FEED_ID: u32 = 100; // XLM/USD, testnet placeholder
const DAY: u64 = 86_400;
/// Matches the real Reflector testnet oracle's own `decimals()`, confirmed
/// live via the `stellar` CLI before writing any of this — see the plan
/// doc / README for the exact call.
const REFLECTOR_DECIMALS: u32 = 14;
const REFLECTOR_RESOLUTION_SECS: u32 = 300;
const REFLECTOR_MAX_STALENESS_SECS: u64 = 600; // comfortably above resolution
const REFLECTOR_TOLERANCE_BPS: u32 = 150; // matches the off-chain Lazer-vs-Hermes check's figure

struct Harness {
    env: Env,
    market_id: Address,
    lazer_id: Address,
    reflector_id: Address,
    token_id: Address,
    admin: Address,
    treasury: Address,
    expiry: u64,
    grace: u64,
}

fn default_reflector_config(env: &Env, reflector_id: &Address) -> ReflectorConfig {
    ReflectorConfig {
        contract: reflector_id.clone(),
        asset: Symbol::new(env, "XLM"),
        max_staleness_secs: REFLECTOR_MAX_STALENESS_SECS,
        tolerance_bps: REFLECTOR_TOLERANCE_BPS,
    }
}

fn setup(strike_cents: i128, initial_liquidity: i128) -> Harness {
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
    let market_id = env.register(PolarisMarket, ());

    let expiry = env.ledger().timestamp() + DAY;
    let grace = 3_600u64;

    token_admin.mint(&admin, &(initial_liquidity * 1000));

    let client = PolarisMarketClient::new(&env, &market_id);
    client.initialize(
        &admin,
        &token_id,
        &strike_cents,
        &expiry,
        &grace,
        &lazer_id,
        &FEED_ID,
        &100u32, // base fee: 1%
        &20u32,  // min fee: 0.2%
        &treasury,
        &initial_liquidity,
        &default_reflector_config(&env, &reflector_id),
    );

    Harness {
        env,
        market_id,
        lazer_id,
        reflector_id,
        token_id,
        admin,
        treasury,
        expiry,
        grace,
    }
}

fn fund(h: &Harness, who: &Address, amount: i128) {
    token::StellarAssetClient::new(&h.env, &h.token_id).mint(who, &amount);
}

/// Sets the mock Reflector's price to exactly agree with `cents` (converted
/// through the same `REFLECTOR_DECIMALS` scaling `reflector_price_to_cents`
/// inverts), timestamped at the current ledger time — for tests that need
/// `settle` to pass the cross-check without being *about* the cross-check
/// itself.
fn set_reflector_price_cents(h: &Harness, cents: i128) {
    let client = mock_reflector::MockReflectorClient::new(&h.env, &h.reflector_id);
    let price = cents * 10i128.pow(REFLECTOR_DECIMALS - 2);
    client.set_price(&price, &h.env.ledger().timestamp());
}

mod mock_lazer {
    // Re-declare the mock verifier inline so the market crate's tests don't
    // need a path dependency on the sibling crate.
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
    // Function names/signatures must match `reflector::Contract`'s exactly
    // (Soroban dispatches by name + XDR shape at runtime, not Rust trait
    // identity — see `reflector.rs`'s module doc) — but reuses its `Asset`/
    // `PriceData` types via `super::` rather than redeclaring them, closing
    // off the XDR-shape-drift trap that redeclaring would open.
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

        pub fn set_none(env: Env) {
            env.storage().instance().remove(&PRICE_KEY);
        }

        pub fn lastprice(env: Env, _asset: Asset) -> Option<PriceData> {
            env.storage()
                .instance()
                .get::<_, (i128, u64)>(&PRICE_KEY)
                .map(|(price, timestamp)| PriceData { price, timestamp })
        }

        pub fn decimals(_env: Env) -> u32 {
            super::REFLECTOR_DECIMALS
        }

        pub fn resolution(_env: Env) -> u32 {
            super::REFLECTOR_RESOLUTION_SECS
        }
    }
}

// ---------- initialize ----------

#[test]
fn init_rejects_bad_params() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000);
    let admin = Address::generate(&env);
    let treasury = Address::generate(&env);
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let token_id = sac.address();
    token::StellarAssetClient::new(&env, &token_id).mint(&admin, &1_000_000);
    let lazer_id = env.register(mock_lazer::MockLazer, ());
    let reflector_id = env.register(mock_reflector::MockReflector, ());
    let reflector_config = default_reflector_config(&env, &reflector_id);
    let expiry = env.ledger().timestamp() + DAY;

    let bad_strike = env.register(PolarisMarket, ());
    let c = PolarisMarketClient::new(&env, &bad_strike);
    assert_eq!(
        c.try_initialize(&admin, &token_id, &0i128, &expiry, &3600u64, &lazer_id, &FEED_ID, &100u32, &20u32, &treasury, &1000i128, &reflector_config),
        Err(Ok(Error::InvalidStrikePrice))
    );

    let bad_expiry = env.register(PolarisMarket, ());
    let c = PolarisMarketClient::new(&env, &bad_expiry);
    assert_eq!(
        c.try_initialize(&admin, &token_id, &6_000_000i128, &1u64, &3600u64, &lazer_id, &FEED_ID, &100u32, &20u32, &treasury, &1000i128, &reflector_config),
        Err(Ok(Error::InvalidExpiry))
    );

    let bad_fee = env.register(PolarisMarket, ());
    let c = PolarisMarketClient::new(&env, &bad_fee);
    assert_eq!(
        c.try_initialize(&admin, &token_id, &6_000_000i128, &expiry, &3600u64, &lazer_id, &FEED_ID, &1001u32, &20u32, &treasury, &1000i128, &reflector_config),
        Err(Ok(Error::InvalidFeeBps))
    );

    let bad_fee_range = env.register(PolarisMarket, ());
    let c = PolarisMarketClient::new(&env, &bad_fee_range);
    assert_eq!(
        c.try_initialize(&admin, &token_id, &6_000_000i128, &expiry, &3600u64, &lazer_id, &FEED_ID, &50u32, &100u32, &treasury, &1000i128, &reflector_config),
        Err(Ok(Error::InvalidFeeBps)),
        "min_fee_bps > base_fee_bps must be rejected"
    );

    let bad_reflector = env.register(PolarisMarket, ());
    let c = PolarisMarketClient::new(&env, &bad_reflector);
    let too_narrow = ReflectorConfig {
        contract: reflector_id.clone(),
        asset: Symbol::new(&env, "XLM"),
        max_staleness_secs: REFLECTOR_RESOLUTION_SECS as u64 - 1, // narrower than resolution()
        tolerance_bps: REFLECTOR_TOLERANCE_BPS,
    };
    assert_eq!(
        c.try_initialize(&admin, &token_id, &6_000_000i128, &expiry, &3600u64, &lazer_id, &FEED_ID, &100u32, &20u32, &treasury, &1000i128, &too_narrow),
        Err(Ok(Error::InvalidReflectorConfig)),
        "a staleness window narrower than Reflector's own update resolution must be rejected"
    );
}

#[test]
fn init_seeds_pool_and_pulls_liquidity() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let m = client.get_market();
    assert_eq!(m.pool_yes, 10_000);
    assert_eq!(m.pool_no, 10_000);
    assert_eq!(m.total_supply, 10_000);
    assert_eq!(
        token::Client::new(&h.env, &h.token_id).balance(&h.market_id),
        10_000
    );
    let (yes_bps, no_bps) = client.get_price();
    assert_eq!(yes_bps, 5_000);
    assert_eq!(no_bps, 5_000);
}

#[test]
fn double_initialize_rejected() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let res = client.try_initialize(
        &h.admin, &h.token_id, &1_000_000i128, &h.expiry, &h.grace, &h.lazer_id,
        &FEED_ID, &100u32, &20u32, &h.treasury, &10_000i128,
        &default_reflector_config(&h.env, &h.reflector_id),
    );
    assert_eq!(res, Err(Ok(Error::AlreadyInitialized)));
}

// ---------- split / merge ----------

#[test]
fn split_then_merge_round_trips_collateral() {
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);

    client.split(&user, &2_000);
    assert_eq!(client.get_position(&user), (2_000, 2_000));
    assert_eq!(
        token::Client::new(&h.env, &h.token_id).balance(&user),
        3_000
    );

    client.merge(&user, &2_000);
    assert_eq!(client.get_position(&user), (0, 0));
    assert_eq!(
        token::Client::new(&h.env, &h.token_id).balance(&user),
        5_000
    );

    assert_solvent(&h);
}

#[test]
fn merge_more_than_held_rejected() {
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    client.split(&user, &1_000);
    assert_eq!(
        client.try_merge(&user, &2_000),
        Err(Ok(Error::InsufficientBalance))
    );
}

// ---------- buy / sell (AMM) ----------

#[test]
fn buy_yes_moves_price_up_and_costs_more_than_split_alone() {
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);

    let total_yes = client.buy(&user, &Prediction::Yes, &1_000, &0);
    // buyer should receive MORE than 1,000 YES shares (bonus from the swap)
    assert!(total_yes > 1_000, "expected AMM bonus, got {total_yes}");
    assert_eq!(client.get_position(&user), (total_yes, 0));

    let (yes_bps, no_bps) = client.get_price();
    assert!(yes_bps > 5_000, "YES should now be more expensive: {yes_bps}");
    assert_eq!(yes_bps + no_bps, 10_000);

    assert_solvent(&h);
}

#[test]
fn buy_respects_slippage_floor() {
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let res = client.try_buy(&user, &Prediction::Yes, &1_000, &1_000_000_000);
    assert_eq!(res, Err(Ok(Error::SlippageExceeded)));
}

#[test]
fn buy_then_sell_returns_close_to_original_minus_fees() {
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);

    let shares = client.buy(&user, &Prediction::Yes, &1_000, &0);
    let collateral_back = client.sell(&user, &Prediction::Yes, &shares, &0);

    // round-trip with two fee/slippage-bearing trades must return strictly
    // less than staked, but shouldn't be devastating for a 10%-of-pool trade
    assert!(collateral_back < 1_000);
    assert!(collateral_back > 800, "got {collateral_back}");

    assert_solvent(&h);
}

#[test]
fn sell_full_one_sided_position_returns_nonzero_collateral() {
    // Regression: a naive "swap to opposite side then merge" sell returns
    // ZERO when the seller's entire prediction-side balance is swapped away
    // (they end up on the pure opposite side with nothing left to merge).
    // The closed-form FPMM sell must not have this failure mode.
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);

    let shares = client.buy(&user, &Prediction::Yes, &1_000, &0);
    assert_eq!(client.get_position(&user), (shares, 0)); // pure one-sided position

    let collateral_back = client.sell(&user, &Prediction::Yes, &shares, &0);
    assert!(collateral_back > 0, "sell of a full one-sided position returned zero");
    assert_eq!(client.get_position(&user), (0, 0));

    assert_solvent(&h);
}

#[test]
fn buy_after_expiry_rejected() {
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    h.env.ledger().set_timestamp(h.expiry);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let res = client.try_buy(&user, &Prediction::Yes, &1_000, &0);
    assert_eq!(res, Err(Ok(Error::TradingClosed)));
}

// ---------- transfer ----------

#[test]
fn transfer_moves_shares_between_addresses() {
    let h = setup(1_000_000, 10_000);
    let alice = Address::generate(&h.env);
    let bob = Address::generate(&h.env);
    fund(&h, &alice, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);

    client.split(&alice, &1_000);
    client.transfer(&alice, &bob, &Prediction::Yes, &400);

    assert_eq!(client.get_position(&alice), (600, 1_000));
    assert_eq!(client.get_position(&bob), (400, 0));
    assert_solvent(&h);
}

#[test]
fn transfer_insufficient_balance_rejected() {
    let h = setup(1_000_000, 10_000);
    let alice = Address::generate(&h.env);
    let bob = Address::generate(&h.env);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let res = client.try_transfer(&alice, &bob, &Prediction::Yes, &1);
    assert_eq!(res, Err(Ok(Error::InsufficientBalance)));
}

// ---------- settle ----------

#[test]
fn settle_before_expiry_rejected() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let payload = build_payload(FEED_ID, 60_000_00000000, -8, h.env.ledger().timestamp() as u64 * 1_000_000);
    let bytes = Bytes::from_slice(&h.env, &payload);
    let res = client.try_settle(&bytes);
    assert_eq!(res, Err(Ok(Error::ExpiryNotReached)));
}

#[test]
fn settle_yes_wins_on_price_at_or_above_strike() {
    let h = setup(1_000_000, 10_000); // strike = $10,000.00
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let shares = client.buy(&user, &Prediction::Yes, &1_000, &0);

    h.env.ledger().set_timestamp(h.expiry);
    // price exactly at strike: $10,000.00 == 1_000_000_000_00 * 10^-8
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, h.expiry * 1_000_000);
    let bytes = Bytes::from_slice(&h.env, &payload);
    set_reflector_price_cents(&h, 1_000_000); // agrees with Lazer's $10,000.00
    client.settle(&bytes);

    let m = client.get_market();
    assert_eq!(m.status, MarketStatus::ResolvedYes); // inclusive >= rule
    assert_eq!(m.final_price, 1_000_000);

    let before = token::Client::new(&h.env, &h.token_id).balance(&user);
    let payout = client.redeem(&user);
    assert_eq!(payout, shares);
    let after = token::Client::new(&h.env, &h.token_id).balance(&user);
    assert_eq!(after - before, shares);

    assert_solvent(&h);
}

#[test]
fn settle_no_wins_below_strike_and_yes_side_gets_nothing() {
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let shares = client.buy(&user, &Prediction::Yes, &1_000, &0);

    h.env.ledger().set_timestamp(h.expiry);
    let payload = build_payload(FEED_ID, 9_999_00000000, -8, h.expiry * 1_000_000);
    let bytes = Bytes::from_slice(&h.env, &payload);
    set_reflector_price_cents(&h, 999_900); // agrees with Lazer's $9,999.00
    client.settle(&bytes);

    let m = client.get_market();
    assert_eq!(m.status, MarketStatus::ResolvedNo);

    let res = client.try_redeem(&user);
    assert_eq!(res, Err(Ok(Error::NothingToRedeem)));
    let _ = shares;
    assert_solvent(&h);
}

#[test]
fn settle_rejects_stale_price() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    h.env.ledger().set_timestamp(h.expiry);
    let stale_ts = (h.expiry - 1_000) * 1_000_000; // > 5 min before expiry
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, stale_ts);
    let bytes = Bytes::from_slice(&h.env, &payload);
    let res = client.try_settle(&bytes);
    assert_eq!(res, Err(Ok(Error::StalePrice)));
}

#[test]
fn settle_rejects_wrong_feed_id() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    h.env.ledger().set_timestamp(h.expiry);
    let payload = build_payload(FEED_ID + 1, 10_000_00000000, -8, h.expiry * 1_000_000);
    let bytes = Bytes::from_slice(&h.env, &payload);
    let res = client.try_settle(&bytes);
    assert_eq!(res, Err(Ok(Error::FeedNotFound)));
}

#[test]
fn double_settle_rejected() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    h.env.ledger().set_timestamp(h.expiry);
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, h.expiry * 1_000_000);
    let bytes = Bytes::from_slice(&h.env, &payload);
    set_reflector_price_cents(&h, 1_000_000);
    client.settle(&bytes);
    let res = client.try_settle(&bytes);
    assert_eq!(res, Err(Ok(Error::AlreadyFinalized)));
}

// ---------- on-chain Reflector cross-check ----------

#[test]
fn settle_rejects_when_reflector_has_no_price_yet() {
    // Fails closed, not gracefully-degrades — the whole point of moving
    // this on-chain (see the module doc's "why fail closed" section). Mock
    // Reflector starts with no price set at all (`lastprice` returns
    // `None`), same as a fresh/never-updated real oracle instance would.
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    h.env.ledger().set_timestamp(h.expiry);
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, h.expiry * 1_000_000);
    let bytes = Bytes::from_slice(&h.env, &payload);
    let res = client.try_settle(&bytes);
    assert_eq!(res, Err(Ok(Error::ReflectorPriceUnavailable)));
    assert_eq!(client.get_market().status, MarketStatus::Open, "a failed cross-check must not finalize the market");
}

#[test]
fn settle_rejects_a_stale_reflector_price() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    h.env.ledger().set_timestamp(h.expiry);
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, h.expiry * 1_000_000);
    let bytes = Bytes::from_slice(&h.env, &payload);

    let stale_ts = h.expiry - REFLECTOR_MAX_STALENESS_SECS - 1;
    let reflector_client = mock_reflector::MockReflectorClient::new(&h.env, &h.reflector_id);
    reflector_client.set_price(&(1_000_000i128 * 10i128.pow(REFLECTOR_DECIMALS - 2)), &stale_ts);

    let res = client.try_settle(&bytes);
    assert_eq!(res, Err(Ok(Error::ReflectorPriceStale)));
    assert_eq!(client.get_market().status, MarketStatus::Open);
}

#[test]
fn settle_rejects_on_gross_lazer_reflector_divergence_and_is_not_stranded() {
    // This is the whole point of the feature: a Lazer payload that, before
    // this change, would have settled the market successfully (see
    // `settle_yes_wins_on_price_at_or_above_strike`, identical inputs)
    // instead gets rejected once a genuinely independent second oracle
    // disagrees with it. Then proves the rejection didn't strand
    // anything — correcting the mock price and re-calling `settle`
    // succeeds normally, same as it always would have.
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    h.env.ledger().set_timestamp(h.expiry);
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, h.expiry * 1_000_000); // Lazer: $10,000.00
    let bytes = Bytes::from_slice(&h.env, &payload);

    set_reflector_price_cents(&h, 2_000_000); // Reflector: $20,000.00 — 100% apart, way past 150bps
    let res = client.try_settle(&bytes);
    assert_eq!(res, Err(Ok(Error::OracleDivergence)));
    assert_eq!(client.get_market().status, MarketStatus::Open, "a divergence rejection must not finalize the market");

    set_reflector_price_cents(&h, 1_000_000); // corrected to agree
    client.settle(&bytes);
    assert_eq!(client.get_market().status, MarketStatus::ResolvedYes);
}

#[test]
fn settle_accepts_a_small_divergence_within_tolerance() {
    // 150bps = 1.5% default tolerance — a real but small cross-path
    // discrepancy (the two oracles' own aggregation/latency differences,
    // not a data error) must not block a healthy settlement.
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    h.env.ledger().set_timestamp(h.expiry);
    let payload = build_payload(FEED_ID, 10_000_00000000, -8, h.expiry * 1_000_000); // $10,000.00
    let bytes = Bytes::from_slice(&h.env, &payload);

    set_reflector_price_cents(&h, 1_001_000); // $10,010.00 — 100bps apart, under the 150bps default
    client.settle(&bytes);
    assert_eq!(client.get_market().status, MarketStatus::ResolvedYes);
}

// ---------- cancel (liveness backstop) ----------

#[test]
fn cancel_before_grace_elapsed_rejected() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    h.env.ledger().set_timestamp(h.expiry);
    let res = client.try_cancel();
    assert_eq!(res, Err(Ok(Error::GracePeriodNotElapsed)));
}

#[test]
fn cancel_after_grace_refunds_a_matched_pair_at_merge_parity() {
    // A matched pair (equal YES+NO from a plain split, no AMM involved) is
    // worth exactly what merge() would return for burning it together — one
    // unit of collateral per unit locked, not two. Paying `by + bn` here
    // (the pre-fix behavior) double-paid this exact case: 2_000 back for
    // 1_000 actually locked. See `cancel_redeem_stays_solvent_for_every_
    // holder_including_treasury` for why that matters beyond just this one
    // user overpaying — it can insolvency-lock whoever redeems next.
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    client.split(&user, &1_000); // simplest case: matched pair, no swap

    h.env.ledger().set_timestamp(h.expiry + h.grace);
    client.cancel();

    let m = client.get_market();
    assert_eq!(m.status, MarketStatus::Cancelled);

    let before = token::Client::new(&h.env, &h.token_id).balance(&user);
    let payout = client.redeem(&user);
    assert_eq!(payout, 1_000); // (1000 YES + 1000 NO) / 2 — matches merge()'s value for the same holding
    let after = token::Client::new(&h.env, &h.token_id).balance(&user);
    assert_eq!(after - before, 1_000);

    assert_solvent(&h);
}

#[test]
fn cancel_refunds_directional_bettor_half_value_no_counterparty_needed() {
    // The scenario the original parimutuel design special-cased ("empty
    // winning pool"): a single bettor takes a directional position and the
    // oracle never resolves. Because they're actually holding a CTF share
    // backed by locked collateral (not a parimutuel claim on a shared
    // pool), a refund falls out of `cancel` + `redeem` with no special
    // casing at all — but only ever half the shares' face value, same as
    // any other holder: a voided market pays each complementary token 0.5,
    // the standard CTF answer (Polymarket, Gnosis) for exactly this reason
    // — it's the only per-holder formula that's solvent for every possible
    // holder no matter their trading history, without tracking anything new.
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let shares = client.buy(&user, &Prediction::Yes, &1_000, &0);

    h.env.ledger().set_timestamp(h.expiry + h.grace);
    client.cancel();
    let payout = client.redeem(&user);
    assert_eq!(payout, shares / 2);

    assert_solvent(&h);
}

// ---------- fee bounded ----------

#[test]
fn fee_only_taken_on_swaps_never_on_split_merge_or_cancel() {
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);

    client.split(&user, &1_000);
    client.merge(&user, &1_000);
    // split+merge round-trip returns exactly what was put in, no fee skimmed
    assert_eq!(
        token::Client::new(&h.env, &h.token_id).balance(&user),
        5_000
    );
    assert_solvent(&h);
}

// ---------- fee curve ----------

#[test]
fn fee_starts_at_base_and_decays_toward_min_as_volume_grows() {
    let h = setup(1_000_000, 10_000); // base=100 (1%), min=20 (0.2%), seeded at 10_000
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let user = Address::generate(&h.env);
    fund(&h, &user, 1_000_000);

    // Fresh market: total_supply == initial_liquidity, fee == base exactly.
    assert_eq!(client.get_fee(), 100);

    // Split alone grows total_supply without any swap — enough on its own
    // to move the curve, independent of buy/sell behavior.
    client.split(&user, &10_000); // total_supply: 10_000 -> 20_000, doubled
    assert_eq!(client.get_fee(), 60); // 20 + (100-20)*10_000/20_000 = 20+40 = 60

    client.split(&user, &30_000); // total_supply: 20_000 -> 50_000 (5x seed)
    assert_eq!(client.get_fee(), 36); // 20 + 80*10_000/50_000 = 20+16 = 36

    // As total_supply grows without bound, fee strictly decreases and never
    // drops below min_fee_bps.
    client.split(&user, &950_000); // total_supply: 50_000 -> 1_000_000 (100x seed)
    let fee_at_100x = client.get_fee();
    assert!(fee_at_100x < 36 && fee_at_100x >= 20, "got {fee_at_100x}");

    assert_solvent(&h);
}

#[test]
fn fee_never_exceeds_base_even_if_total_supply_dips_back_toward_seed() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let user = Address::generate(&h.env);
    fund(&h, &user, 100_000);

    client.split(&user, &40_000); // total_supply: 10_000 -> 50_000, fee compresses
    assert!(client.get_fee() < 100);

    client.merge(&user, &40_000); // back down to exactly initial_liquidity
    assert_eq!(client.get_fee(), 100); // curve is a pure function of current total_supply, not a ratchet

    assert_solvent(&h);
}

// ---------- global solvency invariant ----------

fn assert_solvent(h: &Harness) {
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let m = client.get_market();
    let bal = token::Client::new(&h.env, &h.token_id).balance(&h.market_id);
    assert_eq!(
        bal, m.total_supply as i128,
        "collateral balance must always equal total_supply"
    );
}

// ---------- pool-depth guard ----------

#[test]
fn buy_large_enough_to_exhaust_a_reserve_is_rejected() {
    // Integer floor division in cpmm_out means, for a big enough trade
    // against a shallow pool, `new_reserve_out` can floor all the way to 0
    // — reserve_out - 0 = reserve_out, so the trade would claim the ENTIRE
    // opposite reserve. Confirmed empirically before this guard existed: a
    // single 101_000_000-stroop (10.1 XLM) buy against this exact
    // 10_000-stroop seeded pool drained pool_yes from 10_000 down to 1.
    // That's not a whale-only exploit — it's a plausible trade size against
    // a modest (or organically lopsided) pool. `buy`/`sell` must reject
    // outright rather than let a reserve get driven to (near-)zero.
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let user = Address::generate(&h.env);
    fund(&h, &user, 1_000_000_000);

    let res = client.try_buy(&user, &Prediction::Yes, &101_000_000, &0);
    assert_eq!(res, Err(Ok(Error::PoolDepthExceeded)));

    // Rejected means rejected — no state change at all.
    let m = client.get_market();
    assert_eq!(m.pool_yes, 10_000);
    assert_eq!(m.pool_no, 10_000);
    assert_solvent(&h);
}

#[test]
fn sell_large_enough_to_exhaust_a_reserve_is_rejected() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let user = Address::generate(&h.env);
    fund(&h, &user, 1_000_000_000);

    // Give the user a large YES position without going through buy (which
    // would itself now be capped by the same guard) — split it directly.
    client.split(&user, &500_000_000);

    let res = client.try_sell(&user, &Prediction::Yes, &101_000_000, &0);
    assert_eq!(res, Err(Ok(Error::PoolDepthExceeded)));
    assert_solvent(&h);
}

#[test]
fn normal_sized_trades_are_unaffected_by_the_pool_depth_guard() {
    let h = setup(1_000_000, 10_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);

    // Same trade size as buy_yes_moves_price_up_and_costs_more_than_split_alone
    // — well under the pool's depth, must still succeed exactly as before.
    let shares = client.buy(&user, &Prediction::Yes, &1_000, &0);
    assert!(shares > 1_000);
    assert_solvent(&h);
}

// ---------- regression: Cancelled redeem must stay solvent for every
// legitimate holder, not just whoever redeems first ----------

#[test]
fn cancel_redeem_stays_solvent_for_every_holder_including_treasury() {
    // Regression for a real bug: redeem's Cancelled branch used to pay
    // `by + bn` (both balances summed) per holder. `sum(all YES balances)
    // == total_supply` and `sum(all NO balances) == total_supply` are both
    // independently true (same number) — real collateral only backs ONE
    // total_supply's worth, not two. Paying `by + bn` to every holder
    // double-counted: a plain matched split (no AMM/buy/sell at all) paid
    // 2_000 back for 1_000 locked, which then starved the treasury's own,
    // completely ordinary pool-seeded redemption right after — confirmed
    // live, it panicked with "balance is not sufficient to spend".
    // Existing cancel tests only ever redeemed ONE holder and stopped, so
    // this never surfaced. This test is the one that would have caught it:
    // redeem *every* legitimate holder and confirm each one actually gets
    // paid, not just that the first one's own numbers add up.
    let h = setup(1_000_000, 10_000);
    let user = Address::generate(&h.env);
    fund(&h, &user, 5_000);
    let client = PolarisMarketClient::new(&h.env, &h.market_id);
    client.split(&user, &1_000); // locks 1_000 collateral for a matched 1_000 YES + 1_000 NO

    h.env.ledger().set_timestamp(h.expiry + h.grace);
    client.cancel(); // treasury credited pool_yes=pool_no=10_000 (the seed liquidity), same mechanism as settle

    let user_payout = client.redeem(&user);
    assert_eq!(user_payout, 1_000);

    // The treasury's own, otherwise-uncontroversial redemption must not be
    // starved by an earlier holder's redemption.
    let treasury_payout = client.redeem(&h.treasury);
    assert_eq!(treasury_payout, 10_000); // (10_000 + 10_000) / 2

    assert_eq!(user_payout + treasury_payout, 11_000); // == total_supply at cancellation, exactly
    assert_solvent(&h);
}
