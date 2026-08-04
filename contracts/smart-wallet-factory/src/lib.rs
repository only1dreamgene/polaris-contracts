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
//! the same passkey always resolves to the same address, so a frontend (or
//! this system's backend) can compute a user's wallet address locally
//! without an on-chain lookup, and `deploy` is naturally idempotent — a
//! second call with the same key fails on the child's own
//! `AlreadyInitialized` check rather than silently creating a duplicate.

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
}

#[cfg(test)]
mod test;
