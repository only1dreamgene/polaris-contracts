#![no_std]

//! Shared CTF/AMM math and the SEP-40 oracle client, extracted from
//! `polaris-market` once `polaris-perpetual` needed the exact same pieces.
//! Deliberately `rlib`-only with no `#[contract]`/`#[contractimpl]` in this
//! crate at all (see `Cargo.toml`'s doc comment) — that's what makes it
//! safe for both contracts to depend on normally, unlike `polaris-vault`'s
//! `contractimport!` workaround for a *whole contract's* callable
//! interface, which this isn't.
//!
//! Every function here is pure (no `Env`, no storage access) except the
//! `sep40` module's on-chain client and `verify_oracle_corroboration`
//! (which only ever read). Nothing in this crate decides
//! fund-safety-critical outcomes on its own — each contract's own
//! `lib.rs` still owns every state mutation and every `require_auth()`;
//! this crate just guarantees the *math* and *verification logic* those
//! decisions are based on can't independently drift between contracts.

use soroban_sdk::{contracttype, Env};

/// amount_out for a constant-product swap of `amount_in` (post-fee) against
/// reserves (reserve_in, reserve_out). Floors, per Soroban i128 division.
pub fn cpmm_out(reserve_in: i128, reserve_out: i128, effective_in: i128) -> i128 {
    let k = reserve_in * reserve_out;
    let new_reserve_in = reserve_in + effective_in;
    let new_reserve_out = k / new_reserve_in;
    reserve_out - new_reserve_out
}

/// Floor integer square root via Newton's method (`n` assumed >= 0).
pub fn isqrt(n: i128) -> i128 {
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
pub fn cpmm_sell_out(reserve_in: i128, reserve_out: i128, effective_in: i128) -> i128 {
    let s = reserve_in + effective_in + reserve_out;
    let p = effective_in * reserve_out;
    let disc = s * s - 4 * p;
    let sqrt_disc = isqrt(core::cmp::max(disc, 0));
    (s - sqrt_disc) / 2
}

pub fn apply_fee(amount: i128, fee_bps: u32) -> i128 {
    let fee = amount * fee_bps as i128 / 10_000;
    amount - fee
}

/// Cost-driven fee curve: `base_fee_bps` at zero volume (`total_supply ==
/// initial_liquidity`), decaying toward `min_fee_bps` as `total_supply`
/// grows. Takes plain scalars rather than a contract-specific `Market`
/// struct (the original, pre-extraction shape) precisely so both
/// `polaris-market` and `polaris-perpetual` — which have differently-shaped
/// state structs — can share this one implementation.
///
/// `effective = min + (base - min) * initial_liquidity / total_supply`.
/// Well-defined and bounded to `[min_fee_bps, base_fee_bps]` as long as the
/// caller's own invariant `total_supply >= initial_liquidity` holds while
/// open (true in both contracts: every `merge`/`sell`/`redeem` can only
/// unwind collateral that `split`/`buy` actually locked; seed liquidity is
/// never itself withdrawable pre-resolution/pre-termination).
pub fn effective_fee_bps(base_fee_bps: u32, min_fee_bps: u32, initial_liquidity: i128, total_supply: i128) -> u32 {
    let base = base_fee_bps as i128;
    let min = min_fee_bps as i128;
    let scaled = min + (base - min) * initial_liquidity / total_supply;
    scaled as u32
}

/// Converts a Pyth raw price (`price * 10^exponent` = USD) into integer
/// cents (`USD * 100`), staying in integer math throughout.
pub fn to_cents(price: i64, exponent: i16) -> i128 {
    let p = price as i128;
    let e = exponent as i32 + 2;
    if e >= 0 {
        p * 10i128.pow(e as u32)
    } else {
        p / 10i128.pow((-e) as u32)
    }
}

/// Converts Reflector's `PriceData.price` (scaled by `decimals`, USD-based
/// same as Lazer's `to_cents`) into whole cents, rounding to the nearest
/// cent rather than truncating — a floor bias would skew every divergence
/// comparison in one direction. `decimals` is bounds-checked by the caller
/// before this is called (guards the `10^(decimals - 2)` scaling from
/// overflow/underflow at the extremes).
pub fn reflector_price_to_cents(price: i128, decimals: u32) -> i128 {
    let divisor = 10i128.pow(decimals - 2);
    let half = divisor / 2;
    if price >= 0 {
        (price + half) / divisor
    } else {
        -((-price + half) / divisor)
    }
}

/// Stellar's SEP-40 oracle interface
/// (https://github.com/stellar/stellar-protocol/blob/master/ecosystem/sep-0040.md),
/// copied verbatim rather than reimplemented — the pattern the ecosystem
/// uses for calling a third-party contract by known interface when
/// there's no local wasm to `contractimport!` against. Originally added
/// for Reflector Network; RedStone's Stellar SEP-40 wrapper contract
/// implements the exact same interface (confirmed live — see
/// `contracts/market`'s README section on both providers), so this is
/// generic to *any* SEP-40 oracle now, not Reflector-specific — hence the
/// module and client names, not `reflector`/`ReflectorPulseClient`.
///
/// Declared exactly once, here, for the whole workspace. A second,
/// independent declaration of a nominally-identical type would compile
/// fine but produce mutually-incompatible XDR, since Soroban's wire
/// encoding for a `contracttype` is structural (variant/field order), not
/// identity-based — this is precisely the trap `polaris-market`'s test
/// mock already avoided by reusing this module's types instead of
/// redeclaring them, before this crate existed to hold the canonical copy.
pub mod sep40 {
    use soroban_sdk::{contracttype, Address, Symbol};

    // The macro only needs the trait to generate `Sep40Client` — the
    // trait itself is never referenced by name elsewhere, hence `allow`.
    #[allow(dead_code)]
    #[soroban_sdk::contractclient(name = "Sep40Client")]
    pub trait Contract {
        fn lastprice(asset: Asset) -> Option<PriceData>;
        fn decimals() -> u32;
        fn resolution() -> u32;
    }

    // Plain `#[contracttype]`, not `export = false` — `Asset` used to only
    // describe an *external* contract's call shape (Reflector/RedStone's
    // `lastprice`), where suppressing its own top-level spec entry seemed
    // right. It's since become a genuine field of `OracleFeedConfig`,
    // which *is* part of this contract's own public interface (an
    // `initialize` parameter) — `export = false` was found, live, to drop
    // the type's spec entry from the compiled wasm entirely, not just hide
    // it from "reachable from the public API" discovery, which broke every
    // spec-driven encoder (the `stellar` CLI's JSON arg parser, and
    // `@stellar/stellar-sdk`'s `contract.Spec.funcArgsToScVals` used by
    // `polaris-oracle`) with "Missing Entry Asset" the moment anything
    // tried to construct an `OracleFeedConfig` from outside Rust source.
    #[contracttype]
    #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
    pub enum Asset {
        Stellar(Address),
        Other(Symbol),
    }

    #[contracttype(export = false)]
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct PriceData {
        pub price: i128,
        pub timestamp: u64,
    }
}

/// Settle-time corroborating-oracle settings, bundled into one struct
/// rather than four flat scalars — shared by both contracts' `initialize`,
/// and by every SEP-40 oracle leg they corroborate against (originally
/// Reflector-only; RedStone joined as a second, genuinely independent
/// SEP-40 source using the exact same shape — see `contracts/market`'s
/// README section for why one struct serves both). Required (not
/// `Option`) at the type level here; each contract's own `initialize`
/// decides whether *having* a given leg at all is mandatory.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct OracleFeedConfig {
    pub contract: soroban_sdk::Address,
    /// This system is XLM-only today (see the vault's fixed-`collateral`
    /// precedent for this same "one deliberate limitation, stated
    /// plainly" shape). Different SEP-40 providers key XLM under
    /// different `Asset` variants — Reflector under
    /// `Asset::Other(symbol_short!("XLM"))`, RedStone under
    /// `Asset::Stellar(<native XLM SAC address>)` (confirmed live against
    /// each provider's real contract) — so the config carries the whole
    /// variant to construct, not just a bare symbol.
    pub asset: sep40::Asset,
    pub max_staleness_secs: u64,
    pub tolerance_bps: u32,
}

/// What can go wrong verifying one corroborating oracle leg against a
/// reference price — deliberately generic, not per-provider: the caller
/// (each contract's `settle`/`record_price_checkpoint`) already knows
/// which leg it just checked, so it maps this to its own typed, per-leg
/// `Error` variant rather than this shared crate needing contract-specific
/// error numbering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OracleCheckError {
    Unavailable,
    Stale,
    DecimalsInvalid,
    /// The oracle's live `decimals()` no longer matches the value pinned
    /// at `initialize()` time. Not a hardening nicety — RedStone's own
    /// SEP-40 wrapper contract documents, on-chain, that its `decimals()`
    /// returns the *maximum precision across every asset it has
    /// registered*, not this asset's own precision, and can change if a
    /// higher-precision feed is added later. Silently trusting a fresh
    /// `decimals()` on every call would let an unrelated RedStone
    /// admin action silently corrupt this leg's cents conversion; pinning
    /// it at `initialize()` and failing closed on drift turns that into a
    /// safe, detected rejection instead.
    DecimalsChanged,
    Divergent,
}

/// Verifies one corroborating SEP-40 oracle leg against `reference_cents`
/// (the already-verified Lazer price) — the exact check
/// `contracts/market`'s `settle` has always done for its Reflector leg,
/// generalized so both contracts can run it once per configured leg
/// (Reflector, RedStone) instead of duplicating the block per provider
/// per contract.
pub fn verify_oracle_corroboration(
    env: &Env,
    cfg: &OracleFeedConfig,
    expected_decimals: u32,
    now: u64,
    reference_cents: i128,
) -> Result<(), OracleCheckError> {
    let client = sep40::Sep40Client::new(env, &cfg.contract);
    let price = client.lastprice(&cfg.asset).ok_or(OracleCheckError::Unavailable)?;
    if price.timestamp + cfg.max_staleness_secs < now {
        return Err(OracleCheckError::Stale);
    }
    let decimals = client.decimals();
    if !(2..=30).contains(&decimals) {
        return Err(OracleCheckError::DecimalsInvalid);
    }
    if decimals != expected_decimals {
        return Err(OracleCheckError::DecimalsChanged);
    }
    let cents = reflector_price_to_cents(price.price, decimals);
    let divergence = (reference_cents - cents).abs();
    let divergence_bps: i128 = if cents == 0 {
        10_000
    } else {
        (divergence * 10_000) / cents.abs()
    };
    if divergence_bps > cfg.tolerance_bps as i128 {
        return Err(OracleCheckError::Divergent);
    }
    Ok(())
}
