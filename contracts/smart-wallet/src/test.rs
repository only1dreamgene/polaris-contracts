#![cfg(test)]

use super::*;
use soroban_sdk::{vec, BytesN, Env, IntoVal};

/// A real, self-verified secp256r1 keypair + WebAuthn assertion, generated
/// with Node's `crypto` module (P-256, IEEE-P1363/raw signature encoding,
/// **normalized to low-S** — Soroban's `secp256r1_verify` rejects
/// high-S signatures outright, which OpenSSL/Node's default ECDSA signer
/// does not produce consistently; confirmed empirically by this vector
/// failing against the host until normalized, hence the frontend's
/// `lib/webauthn.ts` must do the same to real WebAuthn assertions) against
/// the exact digest construction `__check_auth` uses —
/// `sha256(authenticator_data ‖ sha256(client_data_json))` — for a
/// `client_data_json.challenge` of `base64url([0u8; 32])` (43 `'A'`
/// characters).
fn known_good_vector(env: &Env) -> (BytesN<65>, BytesN<32>, Signature) {
    let signature_payload = BytesN::from_array(env, &[0u8; 32]);

    let public_key = BytesN::from_array(
        env,
        &[
            4, 129, 171, 176, 184, 34, 29, 157, 155, 216, 249, 15, 211, 176, 204, 200, 215, 149,
            125, 105, 190, 228, 254, 205, 165, 244, 97, 186, 87, 250, 160, 81, 103, 39, 149, 72,
            133, 45, 138, 30, 132, 29, 239, 151, 38, 81, 222, 90, 95, 5, 17, 39, 197, 74, 30, 224,
            147, 250, 130, 134, 145, 229, 116, 113, 180,
        ],
    );
    let authenticator_data = Bytes::from_array(
        env,
        &[
            163, 121, 166, 246, 238, 175, 185, 165, 94, 55, 140, 17, 128, 52, 226, 117, 30, 104,
            47, 171, 159, 45, 48, 171, 19, 210, 18, 85, 134, 206, 25, 71, 5, 0, 0, 0, 5,
        ],
    );
    let client_data_json = Bytes::from_array(
        env,
        &[
            123, 34, 116, 121, 112, 101, 34, 58, 34, 119, 101, 98, 97, 117, 116, 104, 110, 46,
            103, 101, 116, 34, 44, 34, 99, 104, 97, 108, 108, 101, 110, 103, 101, 34, 58, 34, 65,
            65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65,
            65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65,
            34, 44, 34, 111, 114, 105, 103, 105, 110, 34, 58, 34, 104, 116, 116, 112, 58, 47, 47,
            108, 111, 99, 97, 108, 104, 111, 115, 116, 58, 52, 53, 48, 55, 34, 44, 34, 99, 114,
            111, 115, 115, 79, 114, 105, 103, 105, 110, 34, 58, 102, 97, 108, 115, 101, 125,
        ],
    );
    let sig = BytesN::from_array(
        env,
        &[
            251, 123, 13, 207, 48, 114, 101, 181, 179, 223, 159, 57, 202, 251, 52, 248, 123, 67,
            179, 133, 29, 240, 216, 183, 181, 31, 163, 43, 117, 196, 236, 62, 106, 162, 192, 74,
            238, 161, 250, 160, 222, 126, 15, 204, 234, 98, 215, 206, 181, 14, 6, 65, 16, 36, 36,
            61, 136, 68, 37, 125, 125, 63, 245, 168,
        ],
    );

    (
        public_key,
        signature_payload,
        Signature {
            authenticator_data,
            client_data_json,
            signature: sig,
        },
    )
}

#[test]
fn init_then_valid_assertion_passes_check_auth() {
    let env = Env::default();
    let contract_id = env.register(SmartWallet, ());
    let client = SmartWalletClient::new(&env, &contract_id);

    let (pk, payload, signature) = known_good_vector(&env);
    client.init(&pk);

    let result: Result<(), Result<Error, _>> = env.try_invoke_contract_check_auth::<Error>(
        &contract_id,
        &payload,
        signature.into_val(&env),
        &vec![&env],
    );
    assert_eq!(result, Ok(()));
}

#[test]
fn wrong_challenge_rejected() {
    let env = Env::default();
    let contract_id = env.register(SmartWallet, ());
    let client = SmartWalletClient::new(&env, &contract_id);

    let (pk, _payload, signature) = known_good_vector(&env);
    client.init(&pk);

    // The signature is cryptographically valid for *a* challenge — just not
    // this one. The host would only ever ask for this payload if it were
    // actually the current transaction's hash, so this models an attacker
    // replaying a signature captured for a different transaction.
    let wrong_payload = BytesN::from_array(&env, &[7u8; 32]);

    let result: Result<(), Result<Error, _>> = env.try_invoke_contract_check_auth::<Error>(
        &contract_id,
        &wrong_payload,
        signature.into_val(&env),
        &vec![&env],
    );
    assert_eq!(result, Err(Ok(Error::ChallengeMismatch)));
}

#[test]
fn tampered_signature_rejected() {
    let env = Env::default();
    let contract_id = env.register(SmartWallet, ());
    let client = SmartWalletClient::new(&env, &contract_id);

    let (pk, payload, mut signature) = known_good_vector(&env);
    client.init(&pk);

    let mut bytes = signature.signature.to_array();
    bytes[0] ^= 0xFF; // flip bits in r — no longer a valid signature over anything
    signature.signature = BytesN::from_array(&env, &bytes);

    // secp256r1_verify has no recoverable-error path in the contract itself
    // — an invalid signature traps the host call. `try_invoke_contract_
    // check_auth`'s test harness catches that trap and surfaces it as
    // `Err(Err(InvokeError::Abort))` rather than unwinding the test process,
    // which is still unambiguously "authorization rejected".
    let result: Result<(), Result<Error, soroban_sdk::InvokeError>> = env
        .try_invoke_contract_check_auth::<Error>(
            &contract_id,
            &payload,
            signature.into_val(&env),
            &vec![&env],
        );
    assert_eq!(result, Err(Err(soroban_sdk::InvokeError::Abort)));
}

#[test]
fn check_auth_before_init_rejected() {
    let env = Env::default();
    let contract_id = env.register(SmartWallet, ());
    let (_pk, payload, signature) = known_good_vector(&env);

    let result: Result<(), Result<Error, _>> = env.try_invoke_contract_check_auth::<Error>(
        &contract_id,
        &payload,
        signature.into_val(&env),
        &vec![&env],
    );
    assert_eq!(result, Err(Ok(Error::NotInitialized)));
}

#[test]
fn double_init_rejected() {
    let env = Env::default();
    let contract_id = env.register(SmartWallet, ());
    let client = SmartWalletClient::new(&env, &contract_id);
    let (pk, _, _) = known_good_vector(&env);

    client.init(&pk);
    let result = client.try_init(&pk);
    assert_eq!(result, Err(Ok(Error::AlreadyInitialized)));
}

#[test]
fn get_public_key_returns_stored_key() {
    let env = Env::default();
    let contract_id = env.register(SmartWallet, ());
    let client = SmartWalletClient::new(&env, &contract_id);
    let (pk, _, _) = known_good_vector(&env);

    client.init(&pk);
    assert_eq!(client.get_public_key(), pk);
}
