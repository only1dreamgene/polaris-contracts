#![cfg(test)]
extern crate std;

use super::*;
use soroban_sdk::{testutils::Address as _, Env};

// Real compiled child-contract WASM, built by
// `cargo build --release --target wasm32v1-none -p polaris-smart-wallet`.
// A factory necessarily deploys *some* real contract code, so exercising it
// against a stub Rust struct wouldn't test the thing that actually matters
// here (deploy_v2 + cross-contract init in one atomic call).
const SMART_WALLET_WASM: &[u8] =
    include_bytes!("../../../target/wasm32v1-none/release/polaris_smart_wallet.wasm");

// A second, totally unrelated real contract — stands in for "whatever an
// attacker might have tried to pass as wasm_hash back when deploy() took
// one as a parameter" in the regression test below.
const OTHER_WASM: &[u8] =
    include_bytes!("../../../target/wasm32v1-none/release/polaris_mock_lazer.wasm");

fn sample_pk(env: &Env, tag: u8) -> BytesN<65> {
    let mut bytes = [tag; 65];
    bytes[0] = 4; // uncompressed SEC1 point prefix
    BytesN::from_array(env, &bytes)
}

/// Registers a factory, uploads the real smart-wallet wasm, and pins it via
/// `initialize` — the setup every test below needs before `deploy` will do
/// anything.
fn setup(env: &Env) -> (Address, BytesN<32>) {
    env.mock_all_auths();
    let factory_id = env.register(SmartWalletFactory, ());
    let wasm_hash = env.deployer().upload_contract_wasm(SMART_WALLET_WASM);
    let admin = Address::generate(env);
    let client = SmartWalletFactoryClient::new(env, &factory_id);
    client.initialize(&admin, &wasm_hash);
    (factory_id, wasm_hash)
}

#[test]
fn resolve_matches_what_deploy_actually_produces() {
    let env = Env::default();
    let (factory_id, _) = setup(&env);
    let pk = sample_pk(&env, 1);
    let client = SmartWalletFactoryClient::new(&env, &factory_id);

    // resolve() is a pure computation with no deployment — check it against
    // itself for idempotency, then against the real deployed address.
    let resolved_before = client.resolve(&pk);
    let deployed_address = client.deploy(&pk);
    let resolved_after = client.resolve(&pk);

    assert_eq!(resolved_before, deployed_address);
    assert_eq!(resolved_after, deployed_address);
}

#[test]
fn resolve_differs_for_different_keys() {
    let env = Env::default();
    let factory_id = env.register(SmartWalletFactory, ());
    let client = SmartWalletFactoryClient::new(&env, &factory_id);

    let addr1 = client.resolve(&sample_pk(&env, 1));
    let addr2 = client.resolve(&sample_pk(&env, 2));
    assert_ne!(addr1, addr2);
}

#[test]
fn deployed_wallet_is_initialized_with_the_given_key() {
    let env = Env::default();
    let (factory_id, _) = setup(&env);
    let pk = sample_pk(&env, 2);

    let client = SmartWalletFactoryClient::new(&env, &factory_id);
    let wallet_address = client.deploy(&pk);

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
    let (factory_id, _) = setup(&env);
    let pk = sample_pk(&env, 3);

    let client = SmartWalletFactoryClient::new(&env, &factory_id);
    client.deploy(&pk);

    let result = client.try_deploy(&pk);
    assert!(result.is_err(), "redeploying the same passkey must fail, not silently succeed");
}

#[test]
fn deploy_before_initialize_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let factory_id = env.register(SmartWalletFactory, ());
    let client = SmartWalletFactoryClient::new(&env, &factory_id);

    let result = client.try_deploy(&sample_pk(&env, 4));
    assert_eq!(result, Err(Ok(Error::NotInitialized)));
}

#[test]
fn double_initialize_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let factory_id = env.register(SmartWalletFactory, ());
    let wasm_hash = env.deployer().upload_contract_wasm(SMART_WALLET_WASM);
    let admin = Address::generate(&env);
    let client = SmartWalletFactoryClient::new(&env, &factory_id);

    client.initialize(&admin, &wasm_hash);
    let result = client.try_initialize(&admin, &wasm_hash);
    assert_eq!(result, Err(Ok(Error::AlreadyInitialized)));
}

#[test]
fn initialize_requires_the_admins_own_authorization() {
    // No env.mock_all_auths() here — the point is that *nothing* is
    // pre-authorized, so initialize must fail rather than silently
    // succeeding for an admin whose signature was never actually provided.
    let env = Env::default();
    let factory_id = env.register(SmartWalletFactory, ());
    let wasm_hash = env.deployer().upload_contract_wasm(SMART_WALLET_WASM);
    let admin = Address::generate(&env);
    let client = SmartWalletFactoryClient::new(&env, &factory_id);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        client.initialize(&admin, &wasm_hash)
    }));
    assert!(result.is_err(), "initialize without the admin's authorization must fail, not succeed");
}

// ---------- regression: deploy() always uses the pinned wasm, no matter
// what a caller might wish it would use instead ----------

#[test]
fn deploy_always_uses_the_pinned_wasm_never_a_caller_choice() {
    // Regression for a real, confirmed-exploitable bug: deploy() used to
    // take `wasm_hash` as a free parameter, with no `require_auth()`
    // either. Since the deployed address is a pure function of (this
    // factory, sha256(public_key)) — the same formula `resolve()` exposes
    // as a public read for anyone to compute in advance — an attacker who
    // learned a victim's public key before the legitimate deploy
    // transaction landed (e.g. watching it sit in the network's public
    // mempool) could race a `deploy(victim_pk, attacker_wasm_hash)` ahead
    // of it. `deploy_v2` at a given (deployer, salt) only ever succeeds
    // once, so whichever call landed first would *permanently* own that
    // address — a malicious contract designed to look like a smart-wallet
    // while authorizing whatever the attacker wanted would then be able to
    // take anything later sent to what the whole system believed was the
    // victim's wallet.
    //
    // There is no longer a wasm_hash parameter to attack — this test
    // documents that fact structurally (deploy only ever takes a public
    // key) and confirms the wasm actually deployed is the one pinned at
    // initialize, not `OTHER_WASM`, by checking the deployed contract
    // answers to the real smart-wallet's interface.
    let env = Env::default();
    let (factory_id, pinned_wasm_hash) = setup(&env);
    assert_ne!(
        pinned_wasm_hash,
        env.deployer().upload_contract_wasm(OTHER_WASM),
        "sanity check: the two wasms really are different",
    );

    let pk = sample_pk(&env, 9);
    let client = SmartWalletFactoryClient::new(&env, &factory_id);
    let address = client.deploy(&pk);

    let stored_pk: BytesN<65> = env.invoke_contract(
        &address,
        &soroban_sdk::Symbol::new(&env, "get_public_key"),
        soroban_sdk::vec![&env],
    );
    assert_eq!(stored_pk, pk, "the deployed contract is a real smart-wallet, not attacker-chosen code");
}
