//! Reflector Network's published SEP-40-compatible interface
//! (https://github.com/reflector-network/reflector-contract), copied
//! verbatim rather than reimplemented — the pattern their own docs
//! describe for calling a third-party contract by known interface when
//! there's no local wasm to `contractimport!` against (unlike
//! `contracts/vault`'s call into `polaris-market`, which does have one).
//!
//! Declared exactly once here. `test.rs`'s `mod mock_reflector` reuses
//! these same types rather than redeclaring them — two independent
//! declarations of a nominally-identical type would each compile fine but
//! produce mutually-incompatible XDR, since Soroban's wire encoding for a
//! `contracttype` is structural (variant/field order), not identity-based.

use soroban_sdk::{contracttype, Address, Symbol};

// The macro only needs the trait to generate `ReflectorPulseClient` — the
// trait itself is never referenced by name elsewhere, hence `allow`.
#[allow(dead_code)]
#[soroban_sdk::contractclient(name = "ReflectorPulseClient")]
pub trait Contract {
    fn lastprice(asset: Asset) -> Option<PriceData>;
    fn decimals() -> u32;
    fn resolution() -> u32;
}

#[contracttype(export = false)]
#[derive(Clone, Debug, Eq, PartialEq)]
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
