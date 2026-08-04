#![cfg(test)]

use super::*;
use soroban_sdk::Env;

// Real compiled child-contract WASM, built by
// `cargo build --release --target wasm32v1-none -p polaris-smart-wallet`.
// A factory necessarily deploys *some* real contract code, so exercising it
// against a stub Rust struct wouldn't test the thing that actually matters
// here (deploy_v2 + cross-contract init in one atomic call).
const SMART_WALLET_WASM: &[u8] =
    include_bytes!("../../../target/wasm32v1-none/release/polaris_smart_wallet.wasm");

fn sample_pk(env: &Env, tag: u8) -> BytesN<65> {
    let mut bytes = [tag; 65];
    bytes[0] = 4; // uncompressed SEC1 point prefix
    BytesN::from_array(env, &bytes)
}

#[test]
fn deploy_produces_the_precomputed_deterministic_address() {
    let env = Env::default();
    env.mock_all_auths();
    let factory_id = env.register(SmartWalletFactory, ());
    let wasm_hash = env.deployer().upload_contract_wasm(SMART_WALLET_WASM);
    let pk = sample_pk(&env, 1);

    // Independently compute what address *should* result, the same way a
    // frontend or backend would to avoid an on-chain lookup after deploy.
    let salt = env.crypto().sha256(&pk.clone().into());
    let expected_address = env
        .as_contract(&factory_id, || env.deployer().with_current_contract(salt).deployed_address());

    let client = SmartWalletFactoryClient::new(&env, &factory_id);
    let deployed_address = client.deploy(&pk, &wasm_hash);

    assert_eq!(deployed_address, expected_address);
}

#[test]
fn deployed_wallet_is_initialized_with_the_given_key() {
    let env = Env::default();
    env.mock_all_auths();
    let factory_id = env.register(SmartWalletFactory, ());
    let wasm_hash = env.deployer().upload_contract_wasm(SMART_WALLET_WASM);
    let pk = sample_pk(&env, 2);

    let client = SmartWalletFactoryClient::new(&env, &factory_id);
    let wallet_address = client.deploy(&pk, &wasm_hash);

    // Not `SmartWalletClient` (that's the sibling crate, and pulling it in
    // just for a test would reintroduce the wasm-hash coupling this file is
    // trying to isolate) — invoke the deployed contract's `get_public_key`
    // by symbol directly, which is enough to prove `init` actually ran
    // against the right address with the right key.
    let stored_pk: BytesN<65> = env.invoke_contract(
        &wallet_address,
        &soroban_sdk::Symbol::new(&env, "get_public_key"),
        soroban_sdk::vec![&env],
    );
    assert_eq!(stored_pk, pk);
}

#[test]
fn same_public_key_cannot_deploy_twice() {
    let env = Env::default();
    env.mock_all_auths();
    let factory_id = env.register(SmartWalletFactory, ());
    let wasm_hash = env.deployer().upload_contract_wasm(SMART_WALLET_WASM);
    let pk = sample_pk(&env, 3);

    let client = SmartWalletFactoryClient::new(&env, &factory_id);
    client.deploy(&pk, &wasm_hash);

    let result = client.try_deploy(&pk, &wasm_hash);
    assert!(result.is_err(), "redeploying the same passkey must fail, not silently succeed");
}
