#![no_std]

//! Capital-efficiency vault for **Polaris**.
//!
//! Additive, isolated, zero changes to `contracts/market`. What this
//! actually fixes: today, every new market's `initial_liquidity` is wired
//! from the admin's own keypair by hand at `initialize()` time, and that
//! seed capital sits siloed in that one market's own reserves for its
//! entire life — recovered only at settlement, into a bare `treasury`
//! wallet address with no accounting at all. Fragmented capital, manual
//! per-market funding, no record of what's deployed where.
//!
//! This is not what "cross-margin" usually means in a derivatives system
//! (net risk across open positions, a liquidation engine, portfolio-level
//! price marking) — that model was considered and deliberately rejected
//! for this build: it would reintroduce exactly the undercollateralized-
//! position risk class this codebase's fund-safety work has spent real
//! effort eliminating from `contracts/market` itself. This vault instead
//! centralizes *custody and accounting* of LP capital while leaving every
//! individual market exactly as fully, independently collateralized as it
//! already is — `Market.treasury` is already a generic `Address`; pointing
//! it at this contract's address is the entire integration, not a market-
//! side code change.
//!
//! v1 is deliberately simple, stated up front rather than glossed over
//! (matching this codebase's existing convention — see
//! `contracts/smart-wallet`'s "a property this build does not implement"):
//! single-admin-owned capital, not fractional LP shares. Multiple depositors
//! can `deposit`, but there's no proportional-withdrawal accounting yet —
//! `withdraw` is admin-gated and moves from the vault's own pooled balance,
//! not from any individual depositor's tracked share. A real multi-LP
//! product would need share accounting before this is safe to open past a
//! single trusted operator.

use soroban_sdk::{contract, contracterror, contractimpl, symbol_short, token, Address, Env, Symbol};

/// Imports `polaris-market`'s client from its *compiled wasm's own spec* —
/// deliberately not a source dependency on the `polaris-market` crate.
/// Depending on that crate directly (for its generated `PolarisMarketClient`)
/// pulls its `#[contract]`-exported wasm symbols into *this* contract's own
/// compiled output — confirmed the hard way: `cargo build --release
/// --target wasm32v1-none` linked "successfully" but warned `function
/// signature mismatch: initialize`, because both contracts export a
/// same-named function and the linker was merging them into one binary.
/// `contractimport!` reads the target wasm's embedded interface spec at
/// compile time (the same spec `polaris-oracle`'s `contract.Spec.fromWasm`
/// reads at runtime) to generate a byte-correct client — including the
/// right `Result<T, Error>` unwrapping for `redeem` — without linking in
/// any of that wasm's actual executable code. Path is relative to this
/// crate's own directory (`contracts/vault/`), confirmed empirically —
/// the SDK's own doc comment on `contractimport!` claims "relative to the
/// workspace root," which did not hold for this soroban-sdk version.
/// Requires `polaris-market`'s release wasm already built (`cargo build
/// --release --target wasm32v1-none -p polaris-market`), same build-order
/// dependency the top-level README already documents for wiring the
/// compiled wasm into `polaris-oracle`.
mod market_contract {
    soroban_sdk::contractimport!(file = "../../target/wasm32v1-none/release/polaris_market.wasm");
}

const STORAGE_KEY_ADMIN: Symbol = symbol_short!("admin");
const STORAGE_KEY_COLLATERAL: Symbol = symbol_short!("collat");
const STORAGE_KEY_TOTAL_DEPOSITED: Symbol = symbol_short!("deposit");

#[contracterror]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidAmount = 3,
    Unauthorized = 4,
}

#[contract]
pub struct Vault;

#[contractimpl]
impl Vault {
    /// One-time setup. `collateral` is the same asset every market this
    /// vault backs uses (native XLM SAC in this build, same as
    /// `contracts/market` — see its "Why native XLM" section) — fixed once,
    /// not re-specified per call, so a `deposit`/`withdraw` can never be
    /// pointed at the wrong asset by mistake.
    pub fn initialize(env: Env, admin: Address, collateral: Address) -> Result<(), Error> {
        admin.require_auth();
        if env.storage().instance().has(&STORAGE_KEY_ADMIN) {
            return Err(Error::AlreadyInitialized);
        }
        env.storage().instance().set(&STORAGE_KEY_ADMIN, &admin);
        env.storage().instance().set(&STORAGE_KEY_COLLATERAL, &collateral);
        env.storage().instance().set(&STORAGE_KEY_TOTAL_DEPOSITED, &0i128);
        Ok(())
    }

    /// LP capital in. Anyone can deposit (permissionless funding, same
    /// "anyone can pay" shape as the wallet factory's `deploy`) — v1 doesn't
    /// track per-depositor shares, so depositing is currently a one-way,
    /// admin-trusted contribution, not a redeemable LP position. Stated
    /// plainly in the module doc; not hidden in the implementation.
    pub fn deposit(env: Env, from: Address, amount: i128) -> Result<(), Error> {
        from.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        let collateral = Self::require_collateral(&env)?;
        token::Client::new(&env, &collateral).transfer(&from, &env.current_contract_address(), &amount);

        let total: i128 = env.storage().instance().get(&STORAGE_KEY_TOTAL_DEPOSITED).unwrap_or(0);
        env.storage().instance().set(&STORAGE_KEY_TOTAL_DEPOSITED, &(total + amount));
        env.events().publish((symbol_short!("deposit"), from), amount);
        Ok(())
    }

    /// Admin-gated capital out — funds a new market's seed liquidity, or an
    /// LP redemption. Fails closed the same way every other admin-gated
    /// entrypoint in this system does (`AdminGuard`, `AuthRelayService`,
    /// the wallet factory's `initialize`): wrong signer, no transfer. If
    /// `amount` exceeds what's actually here, the token transfer itself
    /// fails the whole call — no separate balance check needed, same as
    /// every other transfer in this system.
    pub fn withdraw(env: Env, admin: Address, to: Address, amount: i128) -> Result<(), Error> {
        Self::require_admin(&env, &admin)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        let collateral = Self::require_collateral(&env)?;
        token::Client::new(&env, &collateral).transfer(&env.current_contract_address(), &to, &amount);
        env.events().publish((symbol_short!("withdraw"), to), amount);
        Ok(())
    }

    /// Collects this vault's own payout from a settled/cancelled market it
    /// was `treasury` for, by calling that market's existing `redeem`
    /// on the vault's own behalf. The one cross-contract call in this
    /// design, and it's into an entrypoint that already exists, is already
    /// tested, and needs no `require_auth` from the vault side — `redeem`
    /// only ever pays out to whichever address it's called with, and only
    /// from that address's own on-chain share balance.
    pub fn redeem_from_market(env: Env, admin: Address, market_id: Address) -> Result<i128, Error> {
        Self::require_admin(&env, &admin)?;
        let market = market_contract::Client::new(&env, &market_id);
        let payout = market.redeem(&env.current_contract_address());
        env.events().publish((symbol_short!("mkt_rdm"), market_id), payout);
        Ok(payout)
    }

    pub fn get_balance(env: Env) -> Result<i128, Error> {
        let collateral = Self::require_collateral(&env)?;
        Ok(token::Client::new(&env, &collateral).balance(&env.current_contract_address()))
    }

    pub fn get_total_deposited(env: Env) -> Result<i128, Error> {
        env.storage()
            .instance()
            .get(&STORAGE_KEY_TOTAL_DEPOSITED)
            .ok_or(Error::NotInitialized)
    }

    fn require_admin(env: &Env, admin: &Address) -> Result<(), Error> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&STORAGE_KEY_ADMIN)
            .ok_or(Error::NotInitialized)?;
        if stored != *admin {
            // require_auth on the WRONG address doesn't help an attacker —
            // they'd need that address's own signature, which they don't
            // have — but check identity first anyway so a mismatched caller
            // gets a clear typed error rather than an auth trap.
            return Err(Error::Unauthorized);
        }
        admin.require_auth();
        Ok(())
    }

    fn require_collateral(env: &Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&STORAGE_KEY_COLLATERAL)
            .ok_or(Error::NotInitialized)
    }
}

#[cfg(test)]
mod test;
