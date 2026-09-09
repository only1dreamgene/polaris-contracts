#![no_std]

//! Polaris perpetual market contract.
//!
//! A fully-collateralized binary conditional-token market with **no fixed
//! expiry** — trading, split/merge, and redemption-after-termination all
//! use the exact same mechanics as `polaris-market` (same shared math, see
//! `polaris_ctf_math`), just without a forced terminal settlement under
//! normal operation.
//!
//! ## What "perpetual" means here, and what it deliberately doesn't
//!
//! The original brainstorm for this asked for crypto-perpetual-futures
//! mechanics: margin, leverage, funding payments between the two sides to
//! keep the contract's price anchored to a reference. That mechanism was
//! designed, then rejected, in two stages, both recorded in this repo's
//! project history rather than silently dropped:
//!
//! 1. A paper (arXiv 2605.10400, "Resolution-Aware Perpetual Futures on
//!    Binary Prediction Markets") proves that applying margin/leverage
//!    (`L > 1`) to a binary 0/1-payout claim creates a *structural,
//!    guaranteed* insolvency mode on the adverse outcome, and that real
//!    mitigations don't reliably fix it against real market data. So: no
//!    margin, no leverage, ever, here — every position is fully
//!    collateralized 1:1 at every instant, identical in spirit to
//!    `polaris-market`'s own invariant.
//! 2. A *fully-collateralized* attempt at "funding" (transferring a bounded
//!    fraction of the losing side's AMM pool to the winning side's pool
//!    each period) was designed and independently reviewed. The review
//!    found it breaks the actual conservation invariant —
//!    `pool_yes`/`pool_no` are two **independent** share ledgers, each of
//!    which must satisfy `pool_side + Σ(balances_side) == total_supply` on
//!    its own; editing both pool totals without a matching balance/
//!    `total_supply` change manufactures unbacked claims on one side and
//!    destroys real backing on the other — the same bug *class* as this
//!    project's historical cancellation-payout double-count (see
//!    `polaris-market`'s `redeem` doc comment), different code path. The
//!    honest fix (a lazy, cumulative-index funding accrual) is itself a
//!    well-known bug-prone pattern (rebasing-token accounting) that would
//!    need its own dedicated adversarial-review round before being trusted.
//!
//! **What "perpetual" delivers instead**: no fixed expiry, and continuous
//! exit liquidity via the already-audited `buy`/`sell` — a holder never
//! waits for a terminal event to realize a price move, they just `sell()`
//! at the current AMM-implied price, any time. The anchor to reality is
//! organic arbitrage, the same mechanism Polymarket itself (this project's
//! own stated design inspiration) already relies on with no funding rate
//! at all. `record_price_checkpoint` gives a concrete, safe stand-in for
//! "reference price" — it reuses the exact dual-oracle (Lazer + Reflector)
//! verification `polaris-market`'s `settle` already uses, but has **zero**
//! economic effect: it only records `last_price_cents`/`last_price_at` for
//! observability, never touches a pool, a balance, or `total_supply`.
//!
//! ## Winding down: `terminate`, not `settle`
//!
//! There is deliberately no `ResolvedYes`/`ResolvedNo` concept here. This
//! contract never had a strike price to resolve a claim against — it's an
//! open-ended, continuously-traded instrument, not "will X be above Y" —
//! so at wind-down there's no reference to decide a winner *by*.
//! `terminate` (admin-gated, v1 scope — see its own doc comment) instead
//! reuses `polaris-market`'s already-audited `Cancelled`-redemption
//! treatment exactly: every complementary YES+NO pair is worth 0.5
//! collateral each, the only per-holder formula that's guaranteed solvent
//! regardless of trading history (see `redeem`'s doc comment for why).
//! Because that treatment needs no price at all, `terminate` has **no**
//! oracle dependency whatsoever — a pure administrative wind-down, which
//! is a strictly safer shape than a price-dependent one for a safety valve.

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Bytes, Env};

use pyth_lazer_stellar_sdk::PythLazerClient;

use polaris_ctf_math::{
    apply_fee, cpmm_out, cpmm_sell_out, effective_fee_bps as shared_effective_fee_bps,
    sep40::Sep40Client,
    to_cents, verify_oracle_corroboration, OracleCheckError, OracleFeedConfig,
};

/// Protocol fee ceiling: 1000 bps = 10%. Same figure as `polaris-market`.
const MAX_FEE_BPS: u32 = 1_000;
/// A submitted checkpoint price must be timestamped within this many
/// seconds of "now" — same figure and same reasoning as `polaris-market`'s
/// `PRICE_FRESHNESS_WINDOW_SECS`, just checked against the call time
/// instead of a fixed expiry (there isn't one here).
const PRICE_FRESHNESS_WINDOW_SECS: u64 = 300;
const BUMP_TO: u32 = 535_680; // ~31 days at 5s/ledger
const BUMP_THRESHOLD: u32 = 500_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    InvalidFeeBps = 3,
    InvalidLiquidity = 4,
    InvalidFeedId = 5,
    MarketNotOpen = 6,
    InvalidAmount = 7,
    InsufficientBalance = 8,
    SlippageExceeded = 9,
    /// Same reserve-depth guard as `polaris-market` — see `buy`/`sell`.
    PoolDepthExceeded = 10,
    NotFinalized = 11,
    NothingToRedeem = 12,
    AlreadyFinalized = 13,
    /// `terminate` called by an address other than the stored admin.
    Unauthorized = 14,
    /// `record_price_checkpoint` called on a contract initialized with no
    /// `price_oracle` — checkpointing is opt-in, not every perpetual market
    /// needs one configured.
    PriceOracleNotConfigured = 15,
    InvalidPayload = 16,
    FeedNotFound = 17,
    StalePrice = 18,
    FuturePrice = 19,
    /// Reflector's `lastprice` returned `None` for this market's asset —
    /// fails closed, same as `polaris-market`'s `settle`.
    ReflectorPriceUnavailable = 20,
    ReflectorPriceStale = 21,
    ReflectorDecimalsInvalid = 22,
    OracleDivergence = 23,
    InvalidReflectorConfig = 24,
    /// Reflector's live `decimals()` no longer matches the value pinned at
    /// `initialize()` — see `polaris_ctf_math::OracleCheckError::DecimalsChanged`.
    ReflectorDecimalsChanged = 25,
    /// RedStone's `lastprice` returned `None` — same fail-closed treatment
    /// as the Reflector leg. RedStone is a required part of the
    /// `price_oracle` bundle whenever one is configured at all (see
    /// `PriceOracleConfig`'s doc comment) — unlike `polaris-market`, where
    /// it's independently optional, here it's all-or-nothing with Lazer +
    /// Reflector.
    RedstonePriceUnavailable = 26,
    RedstonePriceStale = 27,
    RedstoneDecimalsInvalid = 28,
    /// See `Error::ReflectorDecimalsChanged` — the same landmine RedStone's
    /// own SEP-40 wrapper contract documents on-chain (its `decimals()` is
    /// the max across every asset it has registered, not this asset's own
    /// precision, and can change): see this repo's README.
    RedstoneDecimalsChanged = 29,
    InvalidRedstoneConfig = 30,
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
pub enum PerpetualStatus {
    Open,
    Terminated,
}

/// Bundles the oracle wiring `record_price_checkpoint` needs — optional at
/// the contract level (a perpetual market that never wants a checkpoint
/// doesn't need to configure one), required as a whole if present (no
/// half-configured state: either every field needed to verify a price is
/// set, or none of them are). Unlike `polaris-market` (where RedStone is
/// independently optional, since testnet has no RedStone deployment to
/// point at), `redstone` here is required *whenever this bundle exists at
/// all* — a perpetual market's checkpoint feature is opt-in as a whole
/// unit already, so there's no separate "testnet needs checkpointing but
/// can't have RedStone" case to accommodate the way `polaris-market`'s
/// unconditionally-required Reflector leg does.
///
/// `reflector_decimals_at_init`/`redstone_decimals_at_init` are not
/// caller-supplied — `initialize` overwrites whatever a caller passes with
/// a live `decimals()` reading from each oracle, the same "fetched live,
/// not trusted from input" treatment already used for `resolution()`. See
/// `polaris_ctf_math::OracleCheckError::DecimalsChanged`'s doc comment for
/// why this pinning exists at all.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct PriceOracleConfig {
    pub lazer_contract: Address,
    pub feed_id: u32,
    pub reflector: OracleFeedConfig,
    pub reflector_decimals_at_init: u32,
    pub redstone: OracleFeedConfig,
    pub redstone_decimals_at_init: u32,
}

#[derive(Clone)]
#[contracttype]
pub struct Perpetual {
    pub admin: Address,
    pub collateral: Address,
    pub base_fee_bps: u32,
    pub min_fee_bps: u32,
    pub treasury: Address,
    pub status: PerpetualStatus,
    pub pool_yes: i128,
    pub pool_no: i128,
    pub total_supply: i128,
    pub initial_liquidity: i128,
    /// Purely informational — see the module doc's "what perpetual means
    /// here" section. `0` until the first successful checkpoint.
    pub last_price_cents: i128,
    pub last_price_at: u64,
}

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Market,
    Balance(Prediction, Address),
    /// Stored separately from `Perpetual` rather than as a
    /// `price_oracle: Option<PriceOracleConfig>` field on it: soroban-sdk
    /// 26.1's `#[contracttype]` macro generates each struct field's XDR
    /// (`ScVal`) conversion via a fallible `TryFrom<&FieldType>`, but
    /// `Option<T>` only gets an `ScVal` conversion through a blanket
    /// `From<Option<T>>` impl requiring an *infallible* `T: Into<ScVal>` —
    /// which a custom struct's generated (fallible) conversion never
    /// satisfies. `Option<CustomStruct>` fields on a `#[contracttype]`
    /// struct fail to compile under the `testutils` feature as a result
    /// (a `cargo test` build always enables it via Cargo's feature
    /// unification, even though it's only a dev-dependency here). Instance
    /// storage's own `get()` sidesteps this entirely — it returns
    /// `Option<T>` via the always-supported `Val`-level conversion, never
    /// the struct-field `ScVal` path — so the optional config lives under
    /// its own key instead.
    PriceOracle,
}

fn touch(env: &Env) {
    env.storage().instance().extend_ttl(BUMP_THRESHOLD, BUMP_TO);
}

fn load_market(env: &Env) -> Result<Perpetual, Error> {
    env.storage().instance().get(&DataKey::Market).ok_or(Error::NotInitialized)
}

fn save_market(env: &Env, m: &Perpetual) {
    env.storage().instance().set(&DataKey::Market, m);
    touch(env);
}

fn load_price_oracle(env: &Env) -> Option<PriceOracleConfig> {
    env.storage().instance().get(&DataKey::PriceOracle)
}

fn balance_of(env: &Env, side: Prediction, addr: &Address) -> i128 {
    env.storage().persistent().get(&DataKey::Balance(side, addr.clone())).unwrap_or(0)
}

fn set_balance(env: &Env, side: Prediction, addr: &Address, amount: i128) {
    let key = DataKey::Balance(side, addr.clone());
    if amount == 0 {
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, &amount);
        env.storage().persistent().extend_ttl(&key, BUMP_THRESHOLD, BUMP_TO);
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

/// Same thin-wrapper shape as `polaris-market`'s — see
/// `polaris_ctf_math::effective_fee_bps`'s doc comment for why the curve
/// itself takes plain scalars instead of a contract-specific struct.
fn effective_fee_bps(m: &Perpetual) -> u32 {
    shared_effective_fee_bps(m.base_fee_bps, m.min_fee_bps, m.initial_liquidity, m.total_supply)
}

fn reserves(m: &Perpetual, side: Prediction) -> (i128, i128) {
    match side {
        Prediction::Yes => (m.pool_yes, m.pool_no),
        Prediction::No => (m.pool_no, m.pool_yes),
    }
}

fn set_reserve(m: &mut Perpetual, side: Prediction, new_value: i128) {
    match side {
        Prediction::Yes => m.pool_yes = new_value,
        Prediction::No => m.pool_no = new_value,
    }
}

#[contract]
pub struct PolarisPerpetual;

#[contractimpl]
impl PolarisPerpetual {
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        collateral: Address,
        base_fee_bps: u32,
        min_fee_bps: u32,
        treasury: Address,
        initial_liquidity: i128,
        price_oracle: Option<PriceOracleConfig>,
    ) -> Result<(), Error> {
        admin.require_auth();

        if env.storage().instance().has(&DataKey::Market) {
            return Err(Error::AlreadyInitialized);
        }
        if base_fee_bps > MAX_FEE_BPS || min_fee_bps > base_fee_bps {
            return Err(Error::InvalidFeeBps);
        }
        if initial_liquidity <= 0 {
            return Err(Error::InvalidLiquidity);
        }
        if let Some(oracle) = &price_oracle {
            if oracle.feed_id == 0 {
                return Err(Error::InvalidFeedId);
            }
            // Same "a staleness window narrower than the oracle's own
            // update cadence would reject every checkpoint outright" guard
            // as polaris-market's initialize, applied to both legs — this
            // bundle is all-or-nothing (see `PriceOracleConfig`'s doc
            // comment), so both must validate for the whole thing to be
            // accepted.
            let reflector_client = Sep40Client::new(&env, &oracle.reflector.contract);
            if oracle.reflector.max_staleness_secs < reflector_client.resolution() as u64 {
                return Err(Error::InvalidReflectorConfig);
            }
            let reflector_decimals_at_init = reflector_client.decimals();
            if !(2..=30).contains(&reflector_decimals_at_init) {
                return Err(Error::ReflectorDecimalsInvalid);
            }

            let redstone_client = Sep40Client::new(&env, &oracle.redstone.contract);
            if oracle.redstone.max_staleness_secs < redstone_client.resolution() as u64 {
                return Err(Error::InvalidRedstoneConfig);
            }
            let redstone_decimals_at_init = redstone_client.decimals();
            if !(2..=30).contains(&redstone_decimals_at_init) {
                return Err(Error::RedstoneDecimalsInvalid);
            }

            // Pinned live here, not trusted from the caller's own struct
            // literal — see `PriceOracleConfig`'s doc comment.
            env.storage().instance().set(
                &DataKey::PriceOracle,
                &PriceOracleConfig { reflector_decimals_at_init, redstone_decimals_at_init, ..oracle.clone() },
            );
        }

        token::Client::new(&env, &collateral).transfer(&admin, &env.current_contract_address(), &initial_liquidity);

        let market = Perpetual {
            admin,
            collateral,
            base_fee_bps,
            min_fee_bps,
            treasury,
            status: PerpetualStatus::Open,
            pool_yes: initial_liquidity,
            pool_no: initial_liquidity,
            total_supply: initial_liquidity,
            initial_liquidity,
            last_price_cents: 0,
            last_price_at: 0,
        };
        save_market(&env, &market);
        env.events().publish((symbol_short!("init"),), initial_liquidity);
        Ok(())
    }

    /// Lock `amount` collateral, mint `amount` YES + `amount` NO to `user`.
    /// Identical to `polaris-market`'s `split`.
    pub fn split(env: Env, user: Address, amount: i128) -> Result<(), Error> {
        user.require_auth();
        let mut m = load_market(&env)?;
        if m.status != PerpetualStatus::Open {
            return Err(Error::MarketNotOpen);
        }
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        token::Client::new(&env, &m.collateral).transfer(&user, &env.current_contract_address(), &amount);
        credit(&env, Prediction::Yes, &user, amount);
        credit(&env, Prediction::No, &user, amount);
        m.total_supply += amount;
        save_market(&env, &m);
        env.events().publish((symbol_short!("split"), user), amount);
        Ok(())
    }

    /// Burn equal YES+NO from `user`, return `amount` collateral. Identical
    /// to `polaris-market`'s `merge`.
    pub fn merge(env: Env, user: Address, amount: i128) -> Result<(), Error> {
        user.require_auth();
        let mut m = load_market(&env)?;
        if m.status != PerpetualStatus::Open {
            return Err(Error::MarketNotOpen);
        }
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        debit(&env, Prediction::Yes, &user, amount)?;
        debit(&env, Prediction::No, &user, amount)?;

        token::Client::new(&env, &m.collateral).transfer(&env.current_contract_address(), &user, &amount);
        m.total_supply -= amount;
        save_market(&env, &m);
        env.events().publish((symbol_short!("merge"), user), amount);
        Ok(())
    }

    /// Split `collateral_amount`, then swap the unwanted side into more of
    /// `prediction` via the AMM. No expiry gate — the one behavioral
    /// difference from `polaris-market`'s `buy`, which this is otherwise
    /// identical to (same reserve-depth guard, same slippage floor).
    pub fn buy(
        env: Env,
        user: Address,
        prediction: Prediction,
        collateral_amount: i128,
        min_shares_out: i128,
    ) -> Result<i128, Error> {
        user.require_auth();
        let mut m = load_market(&env)?;
        if m.status != PerpetualStatus::Open {
            return Err(Error::MarketNotOpen);
        }
        if collateral_amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let unwanted = prediction.other();
        let (reserve_in, reserve_out) = reserves(&m, unwanted);
        let effective_in = apply_fee(collateral_amount, effective_fee_bps(&m));
        let amount_out = cpmm_out(reserve_in, reserve_out, effective_in);
        if amount_out * 2 >= reserve_out {
            return Err(Error::PoolDepthExceeded);
        }
        if amount_out < min_shares_out {
            return Err(Error::SlippageExceeded);
        }

        token::Client::new(&env, &m.collateral).transfer(&user, &env.current_contract_address(), &collateral_amount);
        credit(&env, Prediction::Yes, &user, collateral_amount);
        credit(&env, Prediction::No, &user, collateral_amount);
        m.total_supply += collateral_amount;

        debit(&env, unwanted, &user, collateral_amount)?;
        set_reserve(&mut m, unwanted, reserve_in + collateral_amount);
        set_reserve(&mut m, prediction, reserve_out - amount_out);
        credit(&env, prediction, &user, amount_out);

        save_market(&env, &m);
        env.events().publish((symbol_short!("buy"), user, prediction), (collateral_amount, amount_out));
        Ok(collateral_amount + amount_out)
    }

    /// Sell `shares_in` of `prediction` directly for collateral in one call.
    /// No expiry gate; otherwise identical to `polaris-market`'s `sell`.
    pub fn sell(
        env: Env,
        user: Address,
        prediction: Prediction,
        shares_in: i128,
        min_collateral_out: i128,
    ) -> Result<i128, Error> {
        user.require_auth();
        let mut m = load_market(&env)?;
        if m.status != PerpetualStatus::Open {
            return Err(Error::MarketNotOpen);
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
        if collateral_out * 2 >= reserve_out {
            return Err(Error::PoolDepthExceeded);
        }
        if collateral_out <= 0 || collateral_out < min_collateral_out {
            return Err(Error::SlippageExceeded);
        }

        debit(&env, prediction, &user, shares_in)?;
        set_reserve(&mut m, prediction, reserve_in + shares_in - collateral_out);
        set_reserve(&mut m, opposite, reserve_out - collateral_out);

        token::Client::new(&env, &m.collateral).transfer(&env.current_contract_address(), &user, &collateral_out);
        m.total_supply -= collateral_out;

        save_market(&env, &m);
        env.events().publish((symbol_short!("sell"), user, prediction), (shares_in, collateral_out));
        Ok(collateral_out)
    }

    /// Move a share balance between addresses. Identical to
    /// `polaris-market`'s `transfer`.
    pub fn transfer(env: Env, from: Address, to: Address, prediction: Prediction, amount: i128) -> Result<(), Error> {
        from.require_auth();
        load_market(&env)?; // must be initialized
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        debit(&env, prediction, &from, amount)?;
        credit(&env, prediction, &to, amount);
        env.events().publish((symbol_short!("xfer"), from, to, prediction), amount);
        Ok(())
    }

    /// Permissionless, zero-economic-effect price checkpoint — see the
    /// module doc. Reuses the exact dual-oracle verification
    /// `polaris-market`'s `settle` uses (same freshness window, same
    /// Lazer + Reflector cross-check, same fail-closed behavior on any
    /// staleness/divergence problem), but the *only* state this writes is
    /// `last_price_cents`/`last_price_at` — no pool, balance, or
    /// `total_supply` mutation, ever.
    pub fn record_price_checkpoint(env: Env, payload: Bytes) -> Result<(), Error> {
        let mut m = load_market(&env)?;
        let oracle = load_price_oracle(&env).ok_or(Error::PriceOracleNotConfigured)?;
        let now = env.ledger().timestamp();

        let client = PythLazerClient::new(&env, &oracle.lazer_contract);
        let verified = client.verify_update(&payload).map_err(|_| Error::InvalidPayload)?;

        let feed = verified.feeds.iter().find(|f| f.feed_id == oracle.feed_id).ok_or(Error::FeedNotFound)?;
        let price = feed.price.ok_or(Error::InvalidPayload)?;
        if price <= 0 {
            return Err(Error::InvalidPayload);
        }
        let exponent = feed.exponent.ok_or(Error::InvalidPayload)?;
        let feed_ts_micros = feed.feed_update_timestamp.unwrap_or(verified.timestamp);
        let feed_ts_secs = feed_ts_micros / 1_000_000;

        if feed_ts_secs + PRICE_FRESHNESS_WINDOW_SECS < now {
            return Err(Error::StalePrice);
        }
        if feed_ts_secs > now + PRICE_FRESHNESS_WINDOW_SECS {
            return Err(Error::FuturePrice);
        }

        let final_cents = to_cents(price, exponent);

        // Unanimous dual corroboration — see the module doc and
        // `PriceOracleConfig`'s doc comment. No mutation of `m` has
        // happened yet, so every early return below is a clean revert.
        verify_oracle_corroboration(&env, &oracle.reflector, oracle.reflector_decimals_at_init, now, final_cents)
            .map_err(|e| match e {
                OracleCheckError::Unavailable => Error::ReflectorPriceUnavailable,
                OracleCheckError::Stale => Error::ReflectorPriceStale,
                OracleCheckError::DecimalsInvalid => Error::ReflectorDecimalsInvalid,
                OracleCheckError::DecimalsChanged => Error::ReflectorDecimalsChanged,
                OracleCheckError::Divergent => Error::OracleDivergence,
            })?;
        verify_oracle_corroboration(&env, &oracle.redstone, oracle.redstone_decimals_at_init, now, final_cents)
            .map_err(|e| match e {
                OracleCheckError::Unavailable => Error::RedstonePriceUnavailable,
                OracleCheckError::Stale => Error::RedstonePriceStale,
                OracleCheckError::DecimalsInvalid => Error::RedstoneDecimalsInvalid,
                OracleCheckError::DecimalsChanged => Error::RedstoneDecimalsChanged,
                OracleCheckError::Divergent => Error::OracleDivergence,
            })?;

        m.last_price_cents = final_cents;
        m.last_price_at = now;
        save_market(&env, &m);
        env.events().publish((symbol_short!("chkpt"),), (final_cents, now));
        Ok(())
    }

    /// Admin-gated wind-down safety valve — v1 scope, stated explicitly
    /// rather than inventing an artificial liveness heartbeat this design
    /// has no natural signal to hook a permissionless backstop to (unlike
    /// `polaris-market`'s `cancel`, which has a real deadline —
    /// `expiry + grace_period` — to be permissionless *after*; a perpetual
    /// market by definition has no such deadline). Needs no price at all:
    /// see the module doc's "winding down" section for why reusing
    /// `polaris-market`'s `Cancelled` treatment sidesteps that entirely.
    pub fn terminate(env: Env, admin: Address) -> Result<(), Error> {
        let mut m = load_market(&env)?;
        if m.admin != admin {
            // require_auth on the wrong address doesn't help an attacker —
            // they'd need that address's own signature — but check identity
            // first so a mismatched caller gets a clear typed error rather
            // than an auth trap. Same pattern as polaris-vault's
            // require_admin.
            return Err(Error::Unauthorized);
        }
        admin.require_auth();
        if m.status != PerpetualStatus::Open {
            return Err(Error::AlreadyFinalized);
        }

        credit(&env, Prediction::Yes, &m.treasury, m.pool_yes);
        credit(&env, Prediction::No, &m.treasury, m.pool_no);
        m.pool_yes = 0;
        m.pool_no = 0;
        m.status = PerpetualStatus::Terminated;

        save_market(&env, &m);
        env.events().publish((symbol_short!("term"),), ());
        Ok(())
    }

    /// Redeem `user`'s position after `terminate`. Every complementary
    /// YES+NO pair is worth 0.5 collateral each — the same formula
    /// `polaris-market`'s `Cancelled` branch uses, for the same reason (see
    /// that contract's `redeem` doc comment): `sum(all YES balances) ==
    /// total_supply` and `sum(all NO balances) == total_supply` are both
    /// independently true, so paying each holder `by + bn` in full would
    /// double-count against the single `total_supply`'s worth of real
    /// collateral. Paying 0.5 each is the only per-holder formula
    /// guaranteed solvent regardless of trading history.
    pub fn redeem(env: Env, user: Address) -> Result<i128, Error> {
        user.require_auth();
        let mut m = load_market(&env)?;

        if m.status != PerpetualStatus::Terminated {
            return Err(Error::NotFinalized);
        }
        let by = balance_of(&env, Prediction::Yes, &user);
        let bn = balance_of(&env, Prediction::No, &user);
        set_balance(&env, Prediction::Yes, &user, 0);
        set_balance(&env, Prediction::No, &user, 0);
        let payout = (by + bn) / 2;

        if payout <= 0 {
            return Err(Error::NothingToRedeem);
        }

        token::Client::new(&env, &m.collateral).transfer(&env.current_contract_address(), &user, &payout);
        m.total_supply -= payout;
        save_market(&env, &m);
        env.events().publish((symbol_short!("redeem"), user), payout);
        Ok(payout)
    }

    pub fn get_market(env: Env) -> Result<Perpetual, Error> {
        load_market(&env)
    }

    /// `None` if this instance was never configured with one — see
    /// `DataKey::PriceOracle`'s doc comment for why this lives outside
    /// `Perpetual` itself.
    pub fn get_price_oracle(env: Env) -> Option<PriceOracleConfig> {
        load_price_oracle(&env)
    }

    pub fn get_position(env: Env, addr: Address) -> (i128, i128) {
        (balance_of(&env, Prediction::Yes, &addr), balance_of(&env, Prediction::No, &addr))
    }

    /// Implied YES/NO probability in bps (sums to ~10000), from AMM
    /// reserves. Identical to `polaris-market`'s `get_price`.
    pub fn get_price(env: Env) -> Result<(u32, u32), Error> {
        let m = load_market(&env)?;
        if m.status != PerpetualStatus::Open {
            return Err(Error::MarketNotOpen);
        }
        let total = m.pool_yes + m.pool_no;
        let yes_bps = (m.pool_no * 10_000 / total) as u32;
        Ok((yes_bps, 10_000 - yes_bps))
    }

    pub fn get_fee(env: Env) -> Result<u32, Error> {
        let m = load_market(&env)?;
        Ok(effective_fee_bps(&m))
    }
}

#[cfg(test)]
mod test;
