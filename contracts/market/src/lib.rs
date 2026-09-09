#![no_std]

//! Polaris market contract.
//!
//! A fully-collateralized binary conditional-token market on XLM/USD,
//! settled by a Pyth Lazer price update verified on-chain.
//!
//! Design (see repo README for the full write-up):
//! - `split(amount)` locks `amount` collateral and mints `amount` YES-shares
//!   + `amount` NO-shares to the caller (1:1, always fully collateralized).
//! - `merge(amount)` is the inverse: burns equal YES+NO and returns collateral.
//! - `buy`/`sell` are split/merge composed with a constant-product swap
//!   against an on-contract YES/NO reserve pool, giving continuous pricing
//!   and one-sided exposure without ever under-collateralizing the contract.
//! - `transfer` moves share balances between addresses (the tradability
//!   primitive — a share is an internal ledger entry, not a separate token
//!   contract, so it composes with buy/sell but has no external SEP-41
//!   identity of its own in this build).
//! - `settle` verifies a Pyth Lazer update on-chain, cross-checks it
//!   on-chain against a second, genuinely independent oracle (Reflector
//!   Network — different node operators, different data pipeline, not
//!   just a different Pyth product line) before trusting it, and marks the
//!   winning side; `cancel` is the permissionless liveness backstop if no
//!   valid, mutually-agreeing update ever arrives.
//! - `redeem` pays out 1:1 collateral per winning share (or per share of
//!   either side, if cancelled).
//!
//! Invariant (see tests): `collateral_token.balance(contract) ==
//! market.total_supply` holds after *every* successful call, for the whole
//! lifecycle. Because every share is minted as a matched YES+NO pair, the
//! contract can never be short of collateral to pay winners — there is no
//! "empty winning pool" edge case to special-case.
//!
//! ## Why `settle` fails closed on the Reflector check, not gracefully
//!
//! An earlier, off-chain-only version of this idea (still present in
//! `polaris-oracle`, Lazer vs. Hermes) skips itself when the second source
//! is unavailable — correct there, because it's advisory on top of an
//! already-fully-trusted signature; skipping it leaves the system exactly
//! as safe as before that check existed. Once the check is *on-chain and
//! enforced*, as it is here, "skip when inconvenient" stops being neutral:
//! it turns the real rule into "N-of-2 unless an adversary times
//! submission around a stale window," which is worse than not having the
//! check at all. So `None`/stale/divergent Reflector data all reject the
//! call outright. This doesn't strand funds — the existing permissionless
//! `cancel` after `expiry + grace_period` is already the unconditional
//! liveness backstop, so a market that can never get Reflector agreement
//! cancels and refunds through the exact same well-tested path every other
//! unresolvable market already uses, preserving the "nobody unilaterally
//! decides" property even in the failure case.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Bytes, Env,
};

use pyth_lazer_stellar_sdk::PythLazerClient;

// See polaris-ctf-math's module doc: the CPMM/fee-curve math and the
// Reflector client are shared verbatim with polaris-perpetual now, not
// carried inline here — a pure, behavior-preserving extraction (re-run
// this crate's full test suite after touching this file; every number
// must come out identical).
use polaris_ctf_math::{
    apply_fee, cpmm_out, cpmm_sell_out, effective_fee_bps as shared_effective_fee_bps, sep40::Sep40Client,
    to_cents, verify_oracle_corroboration, OracleCheckError, OracleFeedConfig,
};

/// Protocol fee ceiling: 1000 bps = 10%.
const MAX_FEE_BPS: u32 = 1_000;
/// Settlement price must be timestamped within this many seconds of expiry.
const PRICE_FRESHNESS_WINDOW_SECS: u64 = 300;
/// Instance/persistent storage TTL bump (ledgers), applied on every write.
const BUMP_TO: u32 = 535_680; // ~31 days at 5s/ledger
const BUMP_THRESHOLD: u32 = 500_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    InvalidStrikePrice = 3,
    InvalidExpiry = 4,
    InvalidGracePeriod = 5,
    InvalidFeedId = 6,
    InvalidFeeBps = 7,
    InvalidLiquidity = 8,
    MarketNotOpen = 9,
    TradingClosed = 10,
    InvalidAmount = 11,
    InsufficientBalance = 12,
    SlippageExceeded = 13,
    ExpiryNotReached = 14,
    InvalidPayload = 15,
    FeedNotFound = 16,
    StalePrice = 17,
    FuturePrice = 18,
    GracePeriodNotElapsed = 19,
    AlreadyFinalized = 20,
    NotFinalized = 21,
    NothingToRedeem = 22,
    /// A trade would consume an entire AMM reserve (or, due to integer
    /// floor division, all but a negligible remainder of it) — rejected
    /// outright rather than letting a reserve hit nearly zero and permanently
    /// break that side's pricing. See `buy`/`sell`'s pool-depth guard.
    PoolDepthExceeded = 23,
    /// Reflector's `lastprice` returned `None` for this market's asset —
    /// fails closed, see the module doc's "why fail closed" section.
    ReflectorPriceUnavailable = 24,
    /// Reflector's most recent price is older than `reflector.max_staleness_secs`.
    ReflectorPriceStale = 25,
    /// Reflector's `decimals()` returned something outside a sane range —
    /// guards the `10^decimals` scaling math from overflow/underflow.
    ReflectorDecimalsInvalid = 26,
    /// Lazer's and Reflector's prices diverge by more than
    /// `reflector.tolerance_bps` — the on-chain N-of-2 enforcement itself.
    OracleDivergence = 27,
    /// `initialize`-time validation: `reflector.max_staleness_secs` is
    /// narrower than Reflector's own update `resolution()` — a window that
    /// could never realistically be met.
    InvalidReflectorConfig = 28,
    /// Reflector's live `decimals()` no longer matches the value pinned at
    /// `initialize()` — see `polaris_ctf_math::OracleCheckError::DecimalsChanged`.
    ReflectorDecimalsChanged = 29,
    /// RedStone's `lastprice` returned `None` for this market's asset —
    /// same fail-closed treatment as the Reflector leg. Only reachable if
    /// this market was configured with a `redstone` oracle at `initialize`.
    RedstonePriceUnavailable = 30,
    RedstonePriceStale = 31,
    RedstoneDecimalsInvalid = 32,
    /// RedStone's live `decimals()` no longer matches the value pinned at
    /// `initialize()` — the specific landmine RedStone's own SEP-40
    /// wrapper contract documents (its `decimals()` is the max across
    /// *every* asset it has registered, not this asset's own precision,
    /// and can change): see this repo's README for how this was found.
    RedstoneDecimalsChanged = 33,
    /// `initialize`-time validation for the `redstone` leg, mirroring
    /// `InvalidReflectorConfig`. A RedStone-vs-Lazer divergence beyond
    /// `redstone.tolerance_bps` reuses the shared `OracleDivergence`
    /// variant above — unanimous, not majority: either leg disagreeing
    /// rejects the call the same way.
    InvalidRedstoneConfig = 34,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[contracttype]
pub enum Prediction {
    Yes,
    No,
}

impl Prediction {
    fn other(&self) -> Prediction {
        match self {
            Prediction::Yes => Prediction::No,
            Prediction::No => Prediction::Yes,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[contracttype]
pub enum MarketStatus {
    Open,
    ResolvedYes,
    ResolvedNo,
    Cancelled,
}

// `OracleFeedConfig` itself now lives in `polaris_ctf_math` (imported
// above) — settle-time corroborating-oracle settings, bundled into one
// struct rather than four more flat `initialize()` scalars, shared
// verbatim with `polaris-perpetual` and across both the Reflector and
// RedStone legs, since it means the same thing for all of them.

#[derive(Clone)]
#[contracttype]
pub struct Market {
    pub admin: Address,
    pub collateral: Address,
    pub strike_price: i128, // cents
    pub expiry: u64,
    pub grace_period: u64,
    pub lazer_contract: Address,
    pub feed_id: u32,
    pub base_fee_bps: u32, // swap fee charged when total_supply == initial_liquidity (no volume yet)
    pub min_fee_bps: u32,  // swap fee approached as total_supply grows without bound
    pub treasury: Address,
    pub status: MarketStatus,
    pub final_price: i128, // cents; 0 until resolved
    pub pool_yes: i128,    // AMM reserve; 0 once trading has ended
    pub pool_no: i128,
    pub total_supply: i128, // == total YES outstanding == total NO outstanding, pre-resolution
    pub initial_liquidity: i128, // immutable reference scale for the fee curve; total_supply >= this always, pre-resolution
    pub reflector: OracleFeedConfig,
    /// Reflector's `decimals()` as observed once, live, at `initialize()`
    /// time — re-checked on every `settle()` call so a live drift fails
    /// closed instead of silently corrupting the cents conversion. Plain
    /// (non-`Option`) field: this leg is always configured, unlike
    /// `redstone` below.
    pub reflector_decimals_at_init: u32,
}

/// A configured RedStone corroboration leg plus the `decimals()` value
/// observed, live, at `initialize()` time — bundled together since both
/// are needed on every `settle()` call and neither is meaningful alone.
/// Stored under its own `DataKey::Redstone` instance-storage entry rather
/// than as an `Option<OracleFeedConfig>` field on `Market` itself: see
/// this repo's README ("A soroban-sdk gotcha worth recording") for why
/// `Option<CustomStruct>` can't be a `#[contracttype]` struct field.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct RedstoneOracle {
    pub config: OracleFeedConfig,
    pub decimals_at_init: u32,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Market,
    Balance(Prediction, Address),
    /// Present only if this market was `initialize`d with a `redstone`
    /// oracle configured — see `RedstoneOracle`'s doc comment.
    Redstone,
}

fn touch(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(BUMP_THRESHOLD, BUMP_TO);
}

fn load_market(env: &Env) -> Result<Market, Error> {
    env.storage()
        .instance()
        .get(&DataKey::Market)
        .ok_or(Error::NotInitialized)
}

fn save_market(env: &Env, m: &Market) {
    env.storage().instance().set(&DataKey::Market, m);
    touch(env);
}

fn load_redstone_oracle(env: &Env) -> Option<RedstoneOracle> {
    env.storage().instance().get(&DataKey::Redstone)
}

fn balance_of(env: &Env, side: Prediction, addr: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::Balance(side, addr.clone()))
        .unwrap_or(0)
}

fn set_balance(env: &Env, side: Prediction, addr: &Address, amount: i128) {
    let key = DataKey::Balance(side, addr.clone());
    if amount == 0 {
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, &amount);
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
    }
}

fn credit(env: &Env, side: Prediction, addr: &Address, amount: i128) {
    let b = balance_of(env, side, addr);
    set_balance(env, side, addr, b + amount);
}

fn debit(env: &Env, side: Prediction, addr: &Address, amount: i128) -> Result<(), Error> {
    let b = balance_of(env, side, addr);
    if b < amount {
        return Err(Error::InsufficientBalance);
    }
    set_balance(env, side, addr, b - amount);
    Ok(())
}

// cpmm_out/isqrt/cpmm_sell_out/apply_fee/to_cents/reflector_price_to_cents
// now live in polaris_ctf_math (imported above), shared verbatim with
// polaris-perpetual — see that crate's doc comment.

/// Thin wrapper preserving this file's existing `effective_fee_bps(&m)`
/// call sites unchanged — the actual curve is
/// `polaris_ctf_math::effective_fee_bps`, which takes plain scalars so
/// `polaris-perpetual` (a differently-shaped state struct) can share it too.
fn effective_fee_bps(m: &Market) -> u32 {
    shared_effective_fee_bps(m.base_fee_bps, m.min_fee_bps, m.initial_liquidity, m.total_supply)
}

fn reserves(m: &Market, side: Prediction) -> (i128, i128) {
    match side {
        Prediction::Yes => (m.pool_yes, m.pool_no),
        Prediction::No => (m.pool_no, m.pool_yes),
    }
}

fn set_reserve(m: &mut Market, side: Prediction, new_value: i128) {
    match side {
        Prediction::Yes => m.pool_yes = new_value,
        Prediction::No => m.pool_no = new_value,
    }
}

#[contract]
pub struct PolarisMarket;

#[contractimpl]
impl PolarisMarket {
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        collateral: Address,
        strike_price: i128,
        expiry: u64,
        grace_period: u64,
        lazer_contract: Address,
        feed_id: u32,
        base_fee_bps: u32,
        min_fee_bps: u32,
        treasury: Address,
        initial_liquidity: i128,
        reflector: OracleFeedConfig,
        // Genuinely independent of the same-shape `reflector` leg —
        // RedStone's SEP-40 wrapper on Stellar (confirmed live, see this
        // repo's README). `None` on testnet, where no RedStone contract
        // exists yet — see `RedstoneOracle`'s doc comment for why this
        // can't be a `Market` struct field. When present, corroboration
        // is unanimous: this leg disagreeing rejects `settle()` exactly
        // like the Reflector leg does, never a 2-of-3 majority.
        redstone: Option<OracleFeedConfig>,
    ) -> Result<(), Error> {
        admin.require_auth();

        if env.storage().instance().has(&DataKey::Market) {
            return Err(Error::AlreadyInitialized);
        }
        if strike_price <= 0 {
            return Err(Error::InvalidStrikePrice);
        }
        if expiry <= env.ledger().timestamp() {
            return Err(Error::InvalidExpiry);
        }
        if grace_period == 0 {
            return Err(Error::InvalidGracePeriod);
        }
        if feed_id == 0 {
            return Err(Error::InvalidFeedId);
        }
        if base_fee_bps > MAX_FEE_BPS || min_fee_bps > base_fee_bps {
            return Err(Error::InvalidFeeBps);
        }
        if initial_liquidity <= 0 {
            return Err(Error::InvalidLiquidity);
        }
        // An admin can't configure a staleness window narrower than
        // Reflector's own update cadence allows — that would reject every
        // settle attempt outright, not just unlucky ones.
        let reflector_client = Sep40Client::new(&env, &reflector.contract);
        if reflector.max_staleness_secs < reflector_client.resolution() as u64 {
            return Err(Error::InvalidReflectorConfig);
        }
        // Pinned once here, re-checked on every `settle()` call — see
        // `Market::reflector_decimals_at_init`'s doc comment.
        let reflector_decimals_at_init = reflector_client.decimals();
        if !(2..=30).contains(&reflector_decimals_at_init) {
            return Err(Error::ReflectorDecimalsInvalid);
        }

        if let Some(cfg) = &redstone {
            let redstone_client = Sep40Client::new(&env, &cfg.contract);
            if cfg.max_staleness_secs < redstone_client.resolution() as u64 {
                return Err(Error::InvalidRedstoneConfig);
            }
            let redstone_decimals_at_init = redstone_client.decimals();
            if !(2..=30).contains(&redstone_decimals_at_init) {
                return Err(Error::RedstoneDecimalsInvalid);
            }
            env.storage().instance().set(
                &DataKey::Redstone,
                &RedstoneOracle { config: cfg.clone(), decimals_at_init: redstone_decimals_at_init },
            );
        }

        token::Client::new(&env, &collateral).transfer(
            &admin,
            &env.current_contract_address(),
            &initial_liquidity,
        );

        let market = Market {
            admin,
            collateral,
            strike_price,
            expiry,
            grace_period,
            lazer_contract,
            feed_id,
            base_fee_bps,
            min_fee_bps,
            treasury,
            status: MarketStatus::Open,
            reflector,
            reflector_decimals_at_init,
            final_price: 0,
            pool_yes: initial_liquidity,
            pool_no: initial_liquidity,
            total_supply: initial_liquidity,
            initial_liquidity,
        };
        save_market(&env, &market);
        env.events().publish(
            (symbol_short!("init"),),
            (market.strike_price, market.expiry),
        );
        Ok(())
    }

    /// Lock `amount` collateral, mint `amount` YES + `amount` NO to `user`.
    pub fn split(env: Env, user: Address, amount: i128) -> Result<(), Error> {
        user.require_auth();
        let mut m = load_market(&env)?;
        if m.status != MarketStatus::Open {
            return Err(Error::MarketNotOpen);
        }
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        token::Client::new(&env, &m.collateral).transfer(
            &user,
            &env.current_contract_address(),
            &amount,
        );
        credit(&env, Prediction::Yes, &user, amount);
        credit(&env, Prediction::No, &user, amount);
        m.total_supply += amount;
        save_market(&env, &m);
        env.events()
            .publish((symbol_short!("split"), user), amount);
        Ok(())
    }

    /// Burn equal YES+NO from `user`, return `amount` collateral.
    pub fn merge(env: Env, user: Address, amount: i128) -> Result<(), Error> {
        user.require_auth();
        let mut m = load_market(&env)?;
        if m.status != MarketStatus::Open {
            return Err(Error::MarketNotOpen);
        }
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        debit(&env, Prediction::Yes, &user, amount)?;
        debit(&env, Prediction::No, &user, amount)?;

        token::Client::new(&env, &m.collateral).transfer(
            &env.current_contract_address(),
            &user,
            &amount,
        );
        m.total_supply -= amount;
        save_market(&env, &m);
        env.events()
            .publish((symbol_short!("merge"), user), amount);
        Ok(())
    }

    /// Split `collateral_amount`, then swap the unwanted side into more of
    /// `prediction` via the AMM. Returns total `prediction`-shares now held
    /// from this call (`collateral_amount` from the split + the swap bonus).
    pub fn buy(
        env: Env,
        user: Address,
        prediction: Prediction,
        collateral_amount: i128,
        min_shares_out: i128,
    ) -> Result<i128, Error> {
        user.require_auth();
        let mut m = load_market(&env)?;
        if m.status != MarketStatus::Open {
            return Err(Error::MarketNotOpen);
        }
        if env.ledger().timestamp() >= m.expiry {
            return Err(Error::TradingClosed);
        }
        if collateral_amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let unwanted = prediction.other();
        let (reserve_in, reserve_out) = reserves(&m, unwanted);
        let effective_in = apply_fee(collateral_amount, effective_fee_bps(&m));
        let amount_out = cpmm_out(reserve_in, reserve_out, effective_in);
        // Cap single-trade impact to half the reserve being drawn from. Not
        // just a slippage nicety: cpmm_out's integer floor division lets a
        // large enough trade against a shallow pool claim nearly the whole
        // reserve — a 10.1 XLM buy against a 10_000-stroop seeded pool once
        // drained it from 10_000 down to 1 in this build's own test suite.
        // `min_shares_out` doesn't catch this (the caller can set it to 0,
        // and did, by default); the contract needs its own floor.
        if amount_out * 2 >= reserve_out {
            return Err(Error::PoolDepthExceeded);
        }
        if amount_out < min_shares_out {
            return Err(Error::SlippageExceeded);
        }

        token::Client::new(&env, &m.collateral).transfer(
            &user,
            &env.current_contract_address(),
            &collateral_amount,
        );
        credit(&env, Prediction::Yes, &user, collateral_amount);
        credit(&env, Prediction::No, &user, collateral_amount);
        m.total_supply += collateral_amount;

        debit(&env, unwanted, &user, collateral_amount)?;
        set_reserve(&mut m, unwanted, reserve_in + collateral_amount);
        set_reserve(&mut m, prediction, reserve_out - amount_out);
        credit(&env, prediction, &user, amount_out);

        save_market(&env, &m);
        env.events().publish(
            (symbol_short!("buy"), user, prediction),
            (collateral_amount, amount_out),
        );
        Ok(collateral_amount + amount_out)
    }

    /// Sell `shares_in` of `prediction` directly for collateral in one call
    /// (see `cpmm_sell_out` for why this needs its own formula rather than
    /// "swap to the opposite side, then merge").
    pub fn sell(
        env: Env,
        user: Address,
        prediction: Prediction,
        shares_in: i128,
        min_collateral_out: i128,
    ) -> Result<i128, Error> {
        user.require_auth();
        let mut m = load_market(&env)?;
        if m.status != MarketStatus::Open {
            return Err(Error::MarketNotOpen);
        }
        if env.ledger().timestamp() >= m.expiry {
            return Err(Error::TradingClosed);
        }
        if shares_in <= 0 {
            return Err(Error::InvalidAmount);
        }
        if balance_of(&env, prediction, &user) < shares_in {
            return Err(Error::InsufficientBalance);
        }

        let opposite = prediction.other();
        let (reserve_in, reserve_out) = reserves(&m, prediction);
        let effective_in = apply_fee(shares_in, effective_fee_bps(&m));
        let collateral_out = cpmm_sell_out(reserve_in, reserve_out, effective_in);
        // Same reserve-depth cap as `buy`, against the same reserve
        // (`reserve_out`, the opposite side's pool — the one this trade only
        // ever *decreases*, unlike `reserve_in` which is inflated by
        // `shares_in` first and so has more headroom).
        if collateral_out * 2 >= reserve_out {
            return Err(Error::PoolDepthExceeded);
        }
        if collateral_out <= 0 || collateral_out < min_collateral_out {
            return Err(Error::SlippageExceeded);
        }

        debit(&env, prediction, &user, shares_in)?;
        set_reserve(&mut m, prediction, reserve_in + shares_in - collateral_out);
        set_reserve(&mut m, opposite, reserve_out - collateral_out);

        token::Client::new(&env, &m.collateral).transfer(
            &env.current_contract_address(),
            &user,
            &collateral_out,
        );
        m.total_supply -= collateral_out;

        save_market(&env, &m);
        env.events().publish(
            (symbol_short!("sell"), user, prediction),
            (shares_in, collateral_out),
        );
        Ok(collateral_out)
    }

    /// Move a share balance between addresses — the tradability primitive.
    pub fn transfer(
        env: Env,
        from: Address,
        to: Address,
        prediction: Prediction,
        amount: i128,
    ) -> Result<(), Error> {
        from.require_auth();
        load_market(&env)?; // must be initialized
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        debit(&env, prediction, &from, amount)?;
        credit(&env, prediction, &to, amount);
        env.events()
            .publish((symbol_short!("xfer"), from, to, prediction), amount);
        Ok(())
    }

    /// Permissionlessly settle with an oracle-signed payload once expiry has passed.
    pub fn settle(env: Env, payload: Bytes) -> Result<(), Error> {
        let mut m = load_market(&env)?;
        if m.status != MarketStatus::Open {
            return Err(Error::AlreadyFinalized);
        }
        let now = env.ledger().timestamp();
        if now < m.expiry {
            return Err(Error::ExpiryNotReached);
        }

        let client = PythLazerClient::new(&env, &m.lazer_contract);
        let verified = client
            .verify_update(&payload)
            .map_err(|_| Error::InvalidPayload)?;

        let feed = verified
            .feeds
            .iter()
            .find(|f| f.feed_id == m.feed_id)
            .ok_or(Error::FeedNotFound)?;
        let price = feed.price.ok_or(Error::InvalidPayload)?;
        if price <= 0 {
            return Err(Error::InvalidPayload);
        }
        let exponent = feed.exponent.ok_or(Error::InvalidPayload)?;
        let feed_ts_micros = feed.feed_update_timestamp.unwrap_or(verified.timestamp);
        let feed_ts_secs = feed_ts_micros / 1_000_000;

        if feed_ts_secs + PRICE_FRESHNESS_WINDOW_SECS < m.expiry {
            return Err(Error::StalePrice);
        }
        if feed_ts_secs > m.expiry + PRICE_FRESHNESS_WINDOW_SECS {
            return Err(Error::FuturePrice);
        }

        let final_cents = to_cents(price, exponent);

        // On-chain second- (and, if configured, third-) oracle enforcement
        // — see the module doc's "why fail closed" section. No mutation of
        // `m` has happened yet at this point, so every early return below
        // is a clean revert. Unanimous: any configured leg disagreeing or
        // unavailable rejects the whole call, never a majority vote.
        verify_oracle_corroboration(&env, &m.reflector, m.reflector_decimals_at_init, now, final_cents).map_err(
            |e| match e {
                OracleCheckError::Unavailable => Error::ReflectorPriceUnavailable,
                OracleCheckError::Stale => Error::ReflectorPriceStale,
                OracleCheckError::DecimalsInvalid => Error::ReflectorDecimalsInvalid,
                OracleCheckError::DecimalsChanged => Error::ReflectorDecimalsChanged,
                OracleCheckError::Divergent => Error::OracleDivergence,
            },
        )?;
        if let Some(redstone) = load_redstone_oracle(&env) {
            verify_oracle_corroboration(&env, &redstone.config, redstone.decimals_at_init, now, final_cents).map_err(
                |e| match e {
                    OracleCheckError::Unavailable => Error::RedstonePriceUnavailable,
                    OracleCheckError::Stale => Error::RedstonePriceStale,
                    OracleCheckError::DecimalsInvalid => Error::RedstoneDecimalsInvalid,
                    OracleCheckError::DecimalsChanged => Error::RedstoneDecimalsChanged,
                    OracleCheckError::Divergent => Error::OracleDivergence,
                },
            )?;
        }

        m.final_price = final_cents;
        m.status = if final_cents >= m.strike_price {
            MarketStatus::ResolvedYes
        } else {
            MarketStatus::ResolvedNo
        };

        credit(&env, Prediction::Yes, &m.treasury, m.pool_yes);
        credit(&env, Prediction::No, &m.treasury, m.pool_no);
        m.pool_yes = 0;
        m.pool_no = 0;

        save_market(&env, &m);
        env.events()
            .publish((symbol_short!("settle"),), (m.status, final_cents));
        Ok(())
    }

    /// Permissionless liveness backstop: if nobody ever settles, anyone can
    /// cancel after `expiry + grace_period`. Both sides then redeem 1:1.
    pub fn cancel(env: Env) -> Result<(), Error> {
        let mut m = load_market(&env)?;
        if m.status != MarketStatus::Open {
            return Err(Error::AlreadyFinalized);
        }
        let now = env.ledger().timestamp();
        if now < m.expiry + m.grace_period {
            return Err(Error::GracePeriodNotElapsed);
        }

        credit(&env, Prediction::Yes, &m.treasury, m.pool_yes);
        credit(&env, Prediction::No, &m.treasury, m.pool_no);
        m.pool_yes = 0;
        m.pool_no = 0;
        m.status = MarketStatus::Cancelled;

        save_market(&env, &m);
        env.events().publish((symbol_short!("cancel"),), ());
        Ok(())
    }

    /// Redeem `user`'s position after resolution or cancellation.
    pub fn redeem(env: Env, user: Address) -> Result<i128, Error> {
        user.require_auth();
        let mut m = load_market(&env)?;

        let payout = match m.status {
            MarketStatus::Open => return Err(Error::NotFinalized),
            MarketStatus::ResolvedYes => {
                let b = balance_of(&env, Prediction::Yes, &user);
                set_balance(&env, Prediction::Yes, &user, 0);
                set_balance(&env, Prediction::No, &user, 0);
                b
            }
            MarketStatus::ResolvedNo => {
                let b = balance_of(&env, Prediction::No, &user);
                set_balance(&env, Prediction::Yes, &user, 0);
                set_balance(&env, Prediction::No, &user, 0);
                b
            }
            MarketStatus::Cancelled => {
                // NOT `by + bn`: `sum(all YES balances) == total_supply` and
                // `sum(all NO balances) == total_supply` are both
                // independently true (same number) — real collateral only
                // backs ONE total_supply's worth, not two. Paying `by + bn`
                // to every holder double-counts and can insolvency-lock
                // whoever redeems last (confirmed live: a plain matched
                // split/cancel/redeem already overpaid 2x, then the
                // treasury's own ordinary pool-seeded redemption failed
                // outright — "balance is not sufficient to spend"). Paying
                // each complementary token 0.5 is the standard answer for a
                // voided market in CTF-style systems generally (Polymarket,
                // Gnosis) for exactly this reason: it's the only per-holder
                // formula where summing every payout is *guaranteed* to
                // equal total_supply exactly, regardless of trading
                // history — a directional bettor's AMM-subsidized bonus
                // shares are worth less than face value once nobody's
                // collateral is "eligible" to originate from resolution.
                let by = balance_of(&env, Prediction::Yes, &user);
                let bn = balance_of(&env, Prediction::No, &user);
                set_balance(&env, Prediction::Yes, &user, 0);
                set_balance(&env, Prediction::No, &user, 0);
                (by + bn) / 2
            }
        };

        if payout <= 0 {
            return Err(Error::NothingToRedeem);
        }

        token::Client::new(&env, &m.collateral).transfer(
            &env.current_contract_address(),
            &user,
            &payout,
        );
        m.total_supply -= payout;
        save_market(&env, &m);
        env.events()
            .publish((symbol_short!("redeem"), user), payout);
        Ok(payout)
    }

    pub fn get_market(env: Env) -> Result<Market, Error> {
        load_market(&env)
    }

    /// `None` if this market was never configured with a RedStone leg —
    /// see `RedstoneOracle`'s doc comment for why this lives outside
    /// `Market` itself.
    pub fn get_redstone_oracle(env: Env) -> Option<RedstoneOracle> {
        load_redstone_oracle(&env)
    }

    pub fn get_position(env: Env, addr: Address) -> (i128, i128) {
        (
            balance_of(&env, Prediction::Yes, &addr),
            balance_of(&env, Prediction::No, &addr),
        )
    }

    /// Implied YES/NO probability in bps (sums to ~10000), from AMM reserves.
    pub fn get_price(env: Env) -> Result<(u32, u32), Error> {
        let m = load_market(&env)?;
        if m.status != MarketStatus::Open {
            return Err(Error::MarketNotOpen);
        }
        let total = m.pool_yes + m.pool_no;
        let yes_bps = (m.pool_no * 10_000 / total) as u32;
        Ok((yes_bps, 10_000 - yes_bps))
    }

    /// Current swap fee (bps), per the volume-scaled curve in `effective_fee_bps` —
    /// `base_fee_bps` when the market is fresh, decaying toward `min_fee_bps`
    /// as `total_supply` grows. Authoritative source for what `buy`/`sell`
    /// will actually charge right now; callers shouldn't recompute the curve
    /// themselves against a possibly-stale `total_supply` read elsewhere.
    pub fn get_fee(env: Env) -> Result<u32, Error> {
        let m = load_market(&env)?;
        Ok(effective_fee_bps(&m))
    }
}

#[cfg(test)]
mod test;
