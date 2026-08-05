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
//! - `settle` verifies a Pyth Lazer update on-chain and marks the winning
//!   side; `cancel` is the permissionless liveness backstop if no valid
//!   update ever arrives.
//! - `redeem` pays out 1:1 collateral per winning share (or per share of
//!   either side, if cancelled).
//!
//! Invariant (see tests): `collateral_token.balance(contract) ==
//! market.total_supply` holds after *every* successful call, for the whole
//! lifecycle. Because every share is minted as a matched YES+NO pair, the
//! contract can never be short of collateral to pay winners — there is no
//! "empty winning pool" edge case to special-case.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Bytes, Env,
};

use pyth_lazer_stellar_sdk::PythLazerClient;

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
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Market,
    Balance(Prediction, Address),
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

/// amount_out for a constant-product swap of `amount_in` (post-fee) against
/// reserves (reserve_in, reserve_out). Floors, per Soroban i128 division.
fn cpmm_out(reserve_in: i128, reserve_out: i128, effective_in: i128) -> i128 {
    let k = reserve_in * reserve_out;
    let new_reserve_in = reserve_in + effective_in;
    let new_reserve_out = k / new_reserve_in;
    reserve_out - new_reserve_out
}

/// Floor integer square root via Newton's method (`n` assumed >= 0).
fn isqrt(n: i128) -> i128 {
    if n < 2 {
        return n;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Collateral payout for selling `effective_in` (post-fee) `prediction`
/// shares straight to collateral in one call.
///
/// A naive "swap then merge" sell breaks whenever a user swaps away an
/// *entire* one-sided position: they end up holding pure opposite-side
/// shares with nothing left to merge against, and the trade silently
/// returns zero collateral. This solves the constant-product invariant
/// directly for the payout instead: find `x` (collateral out) such that
/// merging `x` of both sides out of the pool, after `effective_in` shares
/// were swapped in, preserves `k`:
///   (reserve_in + effective_in - x) * (reserve_out - x) == reserve_in * reserve_out
/// which is the quadratic `x^2 - x*S + P = 0` with `S = reserve_in +
/// effective_in + reserve_out`, `P = effective_in * reserve_out`; the
/// economically valid root is the smaller one, `x = (S - sqrt(S^2 - 4P)) / 2`.
/// This is the standard FPMM "sell" formula (as used by Gnosis's
/// conditional-token market makers), not something bespoke to this build.
fn cpmm_sell_out(reserve_in: i128, reserve_out: i128, effective_in: i128) -> i128 {
    let s = reserve_in + effective_in + reserve_out;
    let p = effective_in * reserve_out;
    let disc = s * s - 4 * p;
    let sqrt_disc = isqrt(core::cmp::max(disc, 0));
    (s - sqrt_disc) / 2
}

fn apply_fee(amount: i128, fee_bps: u32) -> i128 {
    let fee = amount * fee_bps as i128 / 10_000;
    amount - fee
}

/// Cost-driven fee curve: `base_fee_bps` at zero volume (`total_supply ==
/// initial_liquidity`), decaying toward `min_fee_bps` as `total_supply`
/// grows — the same "the more it's used, the cheaper it gets" shape as a
/// marginal-cost-based repricing curve, computed automatically per trade
/// rather than set by hand.
///
/// `effective = min + (base - min) * initial_liquidity / total_supply`.
/// Well-defined and bounded to `[min_fee_bps, base_fee_bps]` because
/// `total_supply >= initial_liquidity` is a standing invariant while a
/// market is Open (every `merge`/`sell`/`redeem` can only unwind collateral
/// that `split`/`buy` actually locked; the admin's own seed liquidity is
/// never itself withdrawable pre-resolution — see the pool-to-treasury
/// credit in `settle`/`cancel`), so the ratio is always in `(0, 1]`.
fn effective_fee_bps(m: &Market) -> u32 {
    let base = m.base_fee_bps as i128;
    let min = m.min_fee_bps as i128;
    let scaled = min + (base - min) * m.initial_liquidity / m.total_supply;
    scaled as u32
}

/// Converts a Pyth raw price (`price * 10^exponent` = USD) into integer
/// cents (`USD * 100`), staying in integer math throughout.
fn to_cents(price: i64, exponent: i16) -> i128 {
    let p = price as i128;
    let e = exponent as i32 + 2;
    if e >= 0 {
        p * 10i128.pow(e as u32)
    } else {
        p / 10i128.pow((-e) as u32)
    }
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
                let by = balance_of(&env, Prediction::Yes, &user);
                let bn = balance_of(&env, Prediction::No, &user);
                set_balance(&env, Prediction::Yes, &user, 0);
                set_balance(&env, Prediction::No, &user, 0);
                by + bn
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
