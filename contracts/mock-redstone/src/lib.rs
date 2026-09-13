#![no_std]

//! Testnet-only stand-in for RedStone's real Stellar SEP-40 wrapper
//! contract (mainnet-only — see `polaris-contracts/README.md`'s "A third
//! oracle: RedStone" — confirmed live there against the real
//! `CBMGLKUQZVSAIL5CPDDAWSUY7MAKXISHMOZEVLMBUWBMFGHRJSR4WYRF`).
//!
//! `contracts/perpetual`'s `PriceOracleConfig` requires *both* the
//! Reflector and RedStone legs whenever a `price_oracle` is configured at
//! all (no Reflector-only option — see that struct's doc comment); since
//! RedStone genuinely doesn't exist on testnet, exercising
//! `record_price_checkpoint` there at all needs *some* SEP-40-shaped
//! contract standing in for the RedStone leg. This is exactly the same
//! "mock the third-party interface, not the logic under test" shape as
//! `contracts/mock-lazer` already established for Pyth Lazer — reusing the
//! canonical `polaris_ctf_math::sep40::{Asset, PriceData}` types (not
//! redeclaring them) so it's wire-compatible with the exact same
//! `Sep40Client` every real leg uses.
//!
//! Unlike Reflector's real testnet oracle (which updates itself on its own
//! ~300s cadence), this mock only ever returns whatever `set_price` last
//! set — nothing pushes fresh data into it automatically. Whoever exercises
//! `record_price_checkpoint` against a perpetual configured with this
//! contract is responsible for calling `set_price` with a fresh timestamp
//! first (see `polaris-oracle`'s checkpoint endpoint).
//!
//! MUST NEVER be deployed to mainnet or wired into anything but a
//! deliberately-testnet-only setup — a real perpetual pointed at this
//! contract would accept an operator-supplied price with no independent
//! corroboration at all, defeating the entire point of a third oracle leg.

use polaris_ctf_math::sep40::{Asset, PriceData};
use soroban_sdk::{contract, contractimpl, symbol_short, Env, Symbol};

const PRICE_KEY: Symbol = symbol_short!("px");
const DECIMALS_KEY: Symbol = symbol_short!("dec");
const RESOLUTION_KEY: Symbol = symbol_short!("res");

/// Matches the real RedStone wrapper's live-confirmed `decimals()` (8) —
/// see the README section referenced above — used unless overridden.
const DEFAULT_DECIMALS: u32 = 8;
/// The real wrapper's `resolution()` was confirmed live as 43200s (12h);
/// this mock defaults far lower so a testnet `max_staleness_secs` doesn't
/// need to accommodate a half-day-stale reading just to pass
/// `initialize`'s validation.
const DEFAULT_RESOLUTION_SECS: u32 = 300;

#[contract]
pub struct MockRedstone;

#[contractimpl]
impl MockRedstone {
    /// Unauthenticated by design, same as `mock-lazer`'s `verify_update` —
    /// this whole contract only exists to be poked freely on testnet.
    pub fn set_price(env: Env, price: i128, timestamp: u64) {
        env.storage().instance().set(&PRICE_KEY, &(price, timestamp));
    }

    pub fn set_none(env: Env) {
        env.storage().instance().remove(&PRICE_KEY);
    }

    pub fn set_decimals(env: Env, decimals: u32) {
        env.storage().instance().set(&DECIMALS_KEY, &decimals);
    }

    pub fn set_resolution(env: Env, resolution: u32) {
        env.storage().instance().set(&RESOLUTION_KEY, &resolution);
    }

    pub fn lastprice(env: Env, _asset: Asset) -> Option<PriceData> {
        env.storage().instance().get::<_, (i128, u64)>(&PRICE_KEY).map(|(price, timestamp)| PriceData { price, timestamp })
    }

    pub fn decimals(env: Env) -> u32 {
        env.storage().instance().get(&DECIMALS_KEY).unwrap_or(DEFAULT_DECIMALS)
    }

    pub fn resolution(env: Env) -> u32 {
        env.storage().instance().get(&RESOLUTION_KEY).unwrap_or(DEFAULT_RESOLUTION_SECS)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address};

    #[test]
    fn returns_none_until_a_price_is_set_then_echoes_it_back() {
        let env = Env::default();
        let id = env.register(MockRedstone, ());
        let client = MockRedstoneClient::new(&env, &id);
        let asset = Asset::Stellar(Address::generate(&env));

        assert_eq!(client.lastprice(&asset), None);

        client.set_price(&123_456i128, &1_000u64);
        assert_eq!(client.lastprice(&asset), Some(PriceData { price: 123_456, timestamp: 1_000 }));
        assert_eq!(client.decimals(), DEFAULT_DECIMALS);
        assert_eq!(client.resolution(), DEFAULT_RESOLUTION_SECS);
    }

    #[test]
    fn set_decimals_and_set_resolution_override_the_defaults() {
        let env = Env::default();
        let id = env.register(MockRedstone, ());
        let client = MockRedstoneClient::new(&env, &id);

        client.set_decimals(&14);
        client.set_resolution(&600);
        assert_eq!(client.decimals(), 14);
        assert_eq!(client.resolution(), 600);
    }

    #[test]
    fn set_none_clears_a_previously_set_price() {
        let env = Env::default();
        let id = env.register(MockRedstone, ());
        let client = MockRedstoneClient::new(&env, &id);
        let asset = Asset::Other(Symbol::new(&env, "XLM"));

        client.set_price(&1, &1);
        assert!(client.lastprice(&asset).is_some());
        client.set_none();
        assert_eq!(client.lastprice(&asset), None);
    }
}
