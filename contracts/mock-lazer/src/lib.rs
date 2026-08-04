#![no_std]

//! Testnet-only stand-in for the real `pyth-lazer-stellar` verifier contract.
//!
//! `verify_update` normally checks an ECDSA signature over the payload and
//! returns the verified bytes unchanged; this mock skips verification
//! entirely and echoes the payload straight back, so settlement logic can be
//! exercised end-to-end on testnet without real Pyth Lazer signatures.
//!
//! MUST NEVER be deployed to mainnet — a real market pointed at this
//! contract would accept an attacker-supplied price with no signature check
//! at all. Deploy tooling must refuse to wire this contract in as a
//! market's `lazer_contract` outside testnet.

use soroban_sdk::{contract, contractimpl, Bytes, Env};

#[contract]
pub struct MockLazer;

#[contractimpl]
impl MockLazer {
    pub fn verify_update(_env: Env, data: Bytes) -> Bytes {
        data
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::Env;

    #[test]
    fn echoes_payload_unchanged() {
        let env = Env::default();
        let id = env.register(MockLazer, ());
        let client = MockLazerClient::new(&env, &id);
        let data = Bytes::from_slice(&env, &[1, 2, 3, 4, 5]);
        assert_eq!(client.verify_update(&data), data);
    }
}
