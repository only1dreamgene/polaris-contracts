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

use soroban_sdk::{contract, contractimpl, symbol_short, vec, Address, BytesN, Env};

#[contract]
pub struct SmartWalletFactory;

#[contractimpl]
impl SmartWalletFactory {
    pub fn deploy(env: Env, public_key: BytesN<65>, wasm_hash: BytesN<32>) -> Address {
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
        address
    }

    /// The address `deploy(public_key, ..)` would produce, computed without
    /// deploying anything — a pure read, safe to call speculatively.
    pub fn resolve(env: Env, public_key: BytesN<65>) -> Address {
        let salt = env.crypto().sha256(&public_key.into());
        env.deployer().with_current_contract(salt).deployed_address()
    }
}

#[cfg(test)]
mod test;
