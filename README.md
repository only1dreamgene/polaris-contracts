# polaris-contracts

Rust / Soroban smart contracts for **Polaris** — a fully-collateralized,
non-custodial binary prediction market on XLM/USD, settled by a Pyth Lazer
price update verified on-chain.

Two contracts, one workspace:

| Crate | Purpose |
|---|---|
| `contracts/market` | The market contract. Source of truth for all funds and rules. |
| `contracts/mock-lazer` | Testnet-only stand-in for the real `pyth-lazer-stellar` verifier — echoes a payload back unverified so settlement can be exercised end-to-end without real Pyth signatures. **Never deploy to mainnet.** |
| `contracts/smart-wallet` | A Soroban custom account contract authorized by a WebAuthn passkey (secp256r1) instead of a keypair — lets a passkey-backed address stand in anywhere a normal `Address` is expected, including as the market contract's `user`. |
| `contracts/smart-wallet-factory` | Deploys + initializes a `smart-wallet` instance in one atomic call, at a deterministic address derived from the passkey's public key. |

## Why not the original parimutuel design

The obvious design for a binary market is parimutuel: everyone stakes into a
shared pool, the losing side's stakes fund the winners, split pro-rata at
settlement. That's simple, but it has two structural weaknesses:

1. **No price discovery before expiry.** A stake is frozen the moment it's
   placed; there's no live "odds" and no way to exit early.
2. **An edge case to special-case.** If literally nobody bet the winning
   side, there's nothing to pay winners with — you need a bespoke "empty
   winning pool → refund everyone" branch.

Polaris instead uses a **fully-collateralized conditional-token market**
(the design behind Gnosis's Conditional Tokens Framework and Polymarket),
with a **constant-product AMM** (Uniswap-style `x*y=k`, not LMSR — LMSR needs
floating-point `exp`/`ln`, which isn't safe to hand-roll in an integer-only
WASM contract) for continuous pricing between the two outcome shares.

## Core mechanism

- **`split(amount)`** locks `amount` collateral, mints `amount` YES-shares
  **and** `amount` NO-shares to the caller. Always 1:1, always fully
  collateralized — this can never be gamed or under-collateralized.
- **`merge(amount)`** is the inverse: burn equal YES+NO, get `amount`
  collateral back. Available any time before resolution.
- **`buy(prediction, collateral_amount)`** = split, then swap the unwanted
  side into more of the wanted side via the AMM. One call, standard "buy
  YES" UX; returns more shares than a plain split because of the swap bonus.
- **`sell(prediction, shares_in)`** sells directly to collateral in one
  call, using a closed-form solve of the constant-product invariant (see
  the doc comment on `cpmm_sell_out` in `contracts/market/src/lib.rs` for
  why a naive "swap to the other side, then merge" silently returns zero
  when a user sells an entire one-sided position — a bug this build's test
  suite catches and asserts against by name).
- **`transfer(from, to, prediction, amount)`** moves a share balance between
  addresses — the tradability primitive. Shares are internal ledger entries
  on the market contract itself, not a separate SEP-41 token contract per
  side; a factory that deploys child token contracts per market would give
  shares an independent on-chain identity (tradable on any DEX), but adds
  real deploy-time complexity for a testnet build. `transfer` plus AMM
  `buy`/`sell` gets the properties that matter — live pricing, early exit,
  peer-to-peer movement — without it.
- **`settle(payload)`** — permissionless once expiry has passed. Verifies
  the Pyth Lazer payload on-chain via `pyth-lazer-stellar-sdk`, reads the
  configured feed, checks the price is timestamped within ±5 minutes of
  expiry, and marks the winning side (`>=` strike → YES, inclusive).
- **`cancel()`** — the liveness backstop. Permissionless once
  `expiry + grace_period` has passed with no settlement. Both sides then
  redeem 1:1, at par.
- **`redeem(user)`** — pays out 1:1 collateral per winning share (or per
  share of either side, if cancelled).

## The invariant that matters

Because every share is minted as a matched YES+NO pair against locked
collateral, and every code path that changes a real collateral balance
changes `total_supply` by the exact same amount, this holds after **every
single successful call**, for the whole lifecycle:

```
collateral_token.balance(market_contract) == market.total_supply
```

The market can never be short of collateral to pay whoever's holding the
winning side — there's no "empty winning pool" branch to write or test,
because the invariant makes it structurally impossible. `assert_solvent()`
in the test suite checks this after every test.

## Errors, not panics

Every entrypoint returns `Result<T, Error>` with a typed `#[contracterror]`
enum (22 variants) rather than trapping. A failed call still reverts the
whole transaction — atomicity isn't lost — but callers get a structured
reason instead of an opaque panic.

## Cost-driven fee curve

The swap fee isn't a flat admin-set constant — it's computed per-trade from
a curve: `effective_fee_bps = min_fee_bps + (base_fee_bps - min_fee_bps) *
initial_liquidity / total_supply` (see `effective_fee_bps` in
`contracts/market/src/lib.rs`). A fresh market charges `base_fee_bps`; as
`total_supply` grows (more collateral locked via `split`/`buy`), the fee
compresses toward `min_fee_bps` automatically — the same "cost falls as
scale grows, and the price passes that through automatically" shape as
e.g. dynamic supercharger pricing, rather than a number an admin has to
notice and go reprice by hand. It's well-defined and bounded to
`[min_fee_bps, base_fee_bps]` because `total_supply >= initial_liquidity`
is a standing invariant while a market is Open — the admin's own seed
liquidity is never itself withdrawable pre-resolution, so the ratio driving
the curve is always in `(0, 1]`. Query the current value with `get_fee()`
rather than recomputing it against a possibly-stale `total_supply` read
elsewhere.

## Passkey smart wallets

Rather than requiring a browser extension (Freighter) for every bettor,
`contracts/smart-wallet` lets a WebAuthn passkey (Face ID / Touch ID /
Windows Hello / a hardware key) act as the signer for a Soroban `Address`
directly, via Soroban's account-abstraction `__check_auth` mechanism.

The verification logic — challenge binding via base64url, the
`sha256(authenticator_data ‖ sha256(client_data_json))` digest construction —
is adapted from [leighmcculloch/soroban-webauthn](https://github.com/leighmcculloch/soroban-webauthn),
the reference pattern the Stellar ecosystem uses for this. That repo
explicitly calls its own code "demo material only... not audited"; the same
applies here. It's grounded in real, independently-verified cryptography
(the test suite generates and self-verifies a genuine secp256r1 keypair and
signature — see `contracts/smart-wallet/src/test.rs` — rather than asserting
against a stub), but this has not had a professional security review and
should not hold real value.

One non-obvious thing the test suite caught empirically: Soroban's
`secp256r1_verify` **rejects high-S signatures**. A signature generated
without normalizing `s` to the curve's lower half verified fine locally
(plain ECDSA doesn't care about S's sign) but was rejected by the Soroban
host — the frontend's WebAuthn signing code has to replicate this
normalization (`s' = n - s` when `s > n/2`) on every real browser assertion,
not just this test's synthetic one.

## Feed ID

`feed_id` is an `initialize` parameter, not hardcoded. For this build it
defaults to a testnet placeholder (`100`, see `polaris-oracle`'s config) —
production deployment must look up Pyth's actual registered Lazer feed ID
for XLM/USD before going live.

## Building & testing

```sh
# unit tests (native target, 22 tests)
cargo test -p polaris-market

# release WASM (requires `rustup target add wasm32v1-none`)
cargo build --release --target wasm32v1-none -p polaris-market -p polaris-mock-lazer
shasum -a 256 target/wasm32v1-none/release/polaris_market.wasm
```

Current build (recorded here for reference — regenerate after any contract
change; the backend's `MARKET_WASM_HASH` / mock-lazer deploy config must
track whatever hash is actually uploaded):

| Contract | Size | SHA-256 |
|---|---|---|
| `polaris_market.wasm` | 44,910 bytes | `36210bc2233352b7b1c339fc26df30856829361966b04f455396ee144df85b90` |
| `polaris_mock_lazer.wasm` | 649 bytes | `7840d96cc309b74e37b5ec22f37e978eaec0aef3feb00146a6e8ce3bdee7087d` |
| `polaris_smart_wallet.wasm` | 25,308 bytes | `7f03d5d0c640280a38b36b5fb7e4fa9b4d3d0cfb77764d4a66207e3812407616` |
| `polaris_smart_wallet_factory.wasm` | 4,039 bytes | `2cad3757214adeccd89ad241eb0b30a1d7e92a92a9c27a2b3f7de2874ed7c0ed` |

## Deploying (needs the Stellar CLI, not available in this build environment)

```sh
stellar contract deploy --wasm target/wasm32v1-none/release/polaris_market.wasm \
  --source deployer --network testnet
stellar contract invoke --id <CONTRACT_ID> --source deployer --network testnet -- \
  initialize --admin <ADMIN> --collateral <XLM_SAC> --strike_price 1500000 \
  --expiry <UNIX_TS> --grace_period 3600 --lazer_contract <LAZER_ID> \
  --feed_id 100 --base_fee_bps 100 --min_fee_bps 20 \
  --treasury <TREASURY> --initial_liquidity 10000000000
```
