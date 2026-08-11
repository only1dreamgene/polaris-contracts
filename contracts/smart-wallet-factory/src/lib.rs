#![no_std]

//! Deploys and initializes a `polaris-smart-wallet` instance in one
//! transaction.
//!
//! Soroban doesn't support atomically deploying an arbitrary contract *and*
//! calling an arbitrary constructor from off-chain in one step for wasm
//! deployed this way — but a contract itself can deploy a child contract
//! and immediately invoke it, since `deploy` + `invoke_contract` inside a
//! single contract call is one atomic host invocation. So the standard
//! pattern (used by the reference this is adapted from) is a tiny factory
//! that does both.
//!
//! The deploy salt is `sha256(public_key)`, which makes the wallet's
//! contract address a deterministic function of the passkey's public key:
//! the same passkey always resolves to the same address, on any frontend
//! that points at this same factory instance — a genuinely portable
//! identity, not just an implementation detail. `resolve` computes that
//! address via the exact same on-chain formula `deploy` uses, without
//! deploying anything — a caller (this system's own backend, or any third
//! party embedding a market) can check whether a wallet already exists for
//! a given passkey before prompting a "create wallet" flow. `deploy` itself
//! is also naturally idempotent: a second call for the same key fails on
//! the child's own `AlreadyInitialized` check rather than silently
//! creating (or, worse, silently reusing) a duplicate.
//!
//! Deliberately *not* reimplemented off-chain (in the backend or frontend):
//! Soroban's exact deployer-address-plus-salt hash isn't something worth
//! hand-rolling and hoping matches the host's real derivation when the
//! genuine on-chain computation is one cheap, free simulated call away.
//!
//! ## The wasm this factory deploys is pinned, not caller-chosen
//!
//! `wasm_hash` used to be a `deploy()` parameter — any caller could deploy
//! *any* already-uploaded wasm at any public key's deterministic address,
//! unauthenticated. Confirmed exploitable, not just theoretical: since the
//! deployed address is a pure function of (this factory, sha256(public_key))
//! — the same formula `resolve()` exposes as a public read for anyone to
//! compute in advance — an attacker who learns a victim's public key before
//! the legitimate deploy transaction lands (e.g. watching it sit in the
//! network's public mempool) could race a `deploy(victim_pk,
//! attacker_wasm_hash)` ahead of it. `deploy_v2` at a given (deployer, salt)
//! only ever succeeds once, so whichever call lands first *permanently*
//! owns that address — a malicious contract designed to look like a
//! smart-wallet while authorizing whatever the attacker wants would then
//! be able to take anything later sent to what the whole system believes
//! is the victim's wallet. See `bug_deploy_used_to_let_anyone_pick_the_
//! wasm_and_hijack_a_victims_address` in `test.rs` for the reproduction.
//!
//! Fixed by pinning the wasm hash once, at factory setup
//! (`initialize(admin, wasm_hash)`, admin-authenticated, one-time), and
//! having `deploy` always use that stored hash. `deploy` itself stays
//! deliberately unauthenticated — "anyone can pay to deploy anyone's
//! wallet" is the intended, safe design the backend already relies on to
//! onboard users with zero XLM — but now that's only ever safe because the
//! code being deployed is always this factory's own known-good wallet
//! implementation, never a caller's choice.

use soroban_sdk::{contract, contracterror, contractimpl, symbol_short, vec, Address, BytesN, Env, Symbol};

const STORAGE_KEY_WASM_HASH: Symbol = symbol_short!("wasm");

#[contracterror]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
}

#[contract]
pub struct SmartWalletFactory;

#[contractimpl]
impl SmartWalletFactory {
    /// One-time setup pinning which wallet code this factory will ever
    /// deploy. `admin` is whoever controls this factory's configuration —
    /// not a role any deployed wallet answers to, just the authority to set
    /// this once. Must run before `deploy` will do anything.
    pub fn initialize(env: Env, admin: Address, wasm_hash: BytesN<32>) -> Result<(), Error> {
        admin.require_auth();
        if env.storage().instance().has(&STORAGE_KEY_WASM_HASH) {
            return Err(Error::AlreadyInitialized);
        }
        env.storage().instance().set(&STORAGE_KEY_WASM_HASH, &wasm_hash);
        Ok(())
    }

    /// Deploys (and atomically initializes) a smart-wallet for
    /// `public_key`, always running this factory's own pinned wasm — see
    /// the module doc for why `wasm_hash` isn't a parameter anymore.
    /// Deliberately unauthenticated: anyone can pay to deploy anyone's
    /// wallet, which is what lets this system's backend onboard a user who
    /// holds zero XLM.
    pub fn deploy(env: Env, public_key: BytesN<65>) -> Result<Address, Error> {
        let wasm_hash: BytesN<32> = env
            .storage()
            .instance()
            .get(&STORAGE_KEY_WASM_HASH)
            .ok_or(Error::NotInitialized)?;
        let salt = env.crypto().sha256(&public_key.clone().into());
        let address = env
            .deployer()
            .with_current_contract(salt)
            .deploy_v2(wasm_hash, ());
        let _: () = env.invoke_contract(
            &address,
            &symbol_short!("init"),
            vec![&env, public_key.to_val()],
        );
        Ok(address)
    }

    /// The address `deploy(public_key)` would produce, computed without
    /// deploying anything — a pure read, safe to call speculatively.
    pub fn resolve(env: Env, public_key: BytesN<65>) -> Address {
        let salt = env.crypto().sha256(&public_key.into());
        env.deployer().with_current_contract(salt).deployed_address()
    }
}

#[cfg(test)]
mod test;
