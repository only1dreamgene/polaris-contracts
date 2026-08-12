# polaris-contracts

Rust / Soroban smart contracts for **Polaris** — a fully-collateralized,
non-custodial binary prediction market on XLM/USD, settled by a Pyth Lazer
price update verified on-chain.

Five contracts, one workspace:

| Crate | Purpose |
|---|---|
| `contracts/market` | The market contract. Source of truth for all funds and rules. |
| `contracts/mock-lazer` | Testnet-only stand-in for the real `pyth-lazer-stellar` verifier — echoes a payload back unverified so settlement can be exercised end-to-end without real Pyth signatures. **Never deploy to mainnet.** |
| `contracts/smart-wallet` | A Soroban custom account contract authorized by a WebAuthn passkey (secp256r1) instead of a keypair — lets a passkey-backed address stand in anywhere a normal `Address` is expected, including as the market contract's `user`. |
| `contracts/smart-wallet-factory` | Deploys + initializes a `smart-wallet` instance in one atomic call, at a deterministic address derived from the passkey's public key. |
| `contracts/vault` | Capital-efficiency vault — centralizes LP custody and accounting for market seed liquidity, funded once per depositor rather than fragmented per market. See "The capital-efficiency vault" below. |

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
  `expiry + grace_period` has passed with no settlement. Every holder can
  then redeem — see "Cancellation payout" below for why it's *not* 1:1 on
  both sides.
- **`redeem(user)`** — pays out 1:1 collateral per winning share once
  resolved; on cancellation, 0.5 collateral per share held on either side
  (see below).

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

## Cancellation payout

`redeem()` on a cancelled market pays **0.5 collateral per share held on
either side** (`(balance_yes + balance_no) / 2`), not 1:1 on both — and
that 0.5 isn't a compromise, it's load-bearing.

`sum(all YES balances) == total_supply` and `sum(all NO balances) ==
total_supply` are both independently true (the same number) — but real
collateral only ever backs *one* `total_supply`'s worth, not two.
`settle()`'s resolved paths get this right by paying only the winning side
and discarding the other. `cancel()` has no winning side, so an earlier
version paid *both* in full — which double-counts against a single pool of
backing collateral.

Caught live, not in review: a single user who did nothing but `split()` a
plain matched pair (no AMM, no `buy`/`sell` involved at all) — cancel,
redeem — got back double what they'd locked. The treasury's own,
completely ordinary redemption of its pool-seeded share then panicked:
`"balance is not sufficient to spend"`. Existing tests only ever redeemed
one holder per test and stopped, so nobody had checked whether the *next*
legitimate holder could still get paid. See
`cancel_redeem_stays_solvent_for_every_holder_including_treasury` in
`contracts/market/src/test.rs`.

Paying each complementary token 0.5 is the standard answer for a voided
market in CTF-style systems generally (Polymarket, Gnosis), for exactly
this reason: it's the only per-holder formula that's *guaranteed* solvent
— summed across every holder, payouts equal `total_supply` exactly,
regardless of trading history — without tracking anything beyond the
balances that already exist. The alternative that was seriously considered
and rejected: track each address's actual net collateral contributed and
refund that. More intuitively "fair" (you get back what you put in), but
it only works if that tracked amount also moves proportionally through
`transfer()` — otherwise a sender who transfers away their shares still
shows a refundable balance for shares they no longer hold, while the
receiver holds real shares with no refundable balance behind them at all.
That's real new state that has to stay in perfect sync across
`split`/`buy`/`merge`/`sell`/`transfer`, which is exactly the kind of
surface area new bugs come from — not worth it for what a "voided market"
needs to guarantee.

## Pool-depth guard

`buy`/`sell` reject a trade outright (`Error::PoolDepthExceeded`) if it
would consume half or more of the reserve it's drawing from. This isn't
slippage protection — `min_shares_out`/`min_collateral_out` already cover
that, and a caller can set those to `0` if they want to. It's a correctness
fix for integer floor division: `cpmm_out`'s `k / new_reserve_in` can floor
hard enough that a large-but-entirely-plausible trade against a shallow
pool claims nearly the *entire* opposite reserve. Confirmed empirically,
not just reasoned about — a single 10.1 XLM buy against a 10,000-stroop
seeded pool drained it from 10,000 down to 1 before this guard existed (see
`buy_large_enough_to_exhaust_a_reserve_is_rejected` in
`contracts/market/src/test.rs`). The 50% cap is deliberately generous
(modeled on the same kind of per-trade concentration cap Balancer uses on
its weighted pools) — it exists to rule out the *pathological* case, not to
throttle ordinary large trades against a reasonably deep pool.

## Errors, not panics

Every entrypoint returns `Result<T, Error>` with a typed `#[contracterror]`
enum (23 variants) rather than trapping. A failed call still reverts the
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

### Portable identity

Because a wallet's address is `deploy()`'s deterministic function of
`sha256(public_key)`, the *same* passkey resolves to the *same* Soroban
address regardless of which frontend calls the factory — `resolve(public_key)`
computes that address without deploying anything, so a caller can check
whether a wallet already exists before prompting a "create wallet" flow.
That determinism is what makes a passkey issued on one site usable as a
sign-in credential anywhere else that talks to the same factory instance —
see `polaris-oracle`'s `GET /wallets/resolve` and the embeddable widget for
where this actually gets used, not just left as a latent property.

### Factory wasm pinning

`smart-wallet-factory`'s `deploy()` used to take `wasm_hash` as a caller-
supplied parameter, with no `require_auth()` either. That combination was
a real, confirmed-exploitable address-hijack vector, not just a theoretical
gap: a deployed wallet's address is a pure deterministic function of
`(this factory, sha256(public_key))` — the exact formula `resolve()`
exposes as a public, permissionless read for anyone to compute in advance.
An attacker who learned a victim's public key before the *legitimate*
deploy transaction landed (e.g. watching it sit in the network's public
mempool) could race a `deploy(victim_pk, attacker_wasm_hash)` call ahead
of it. `deploy_v2` at a given (deployer, salt) only ever succeeds once, so
whichever call lands first *permanently* owns that address — a contract
designed to look like a smart-wallet while authorizing whatever the
attacker wants would then be able to take anything later sent to what the
whole system believes is the victim's wallet.

Reproduced directly (no real mempool race needed to demonstrate it — the
vulnerability is that nothing stopped this from succeeding at all, for any
caller, for any already-uploaded wasm): see
`deploy_always_uses_the_pinned_wasm_never_a_caller_choice` in
`contracts/smart-wallet-factory/src/test.rs`.

Fixed by pinning the wasm hash once, at factory setup
(`initialize(admin, wasm_hash)`, admin-authenticated, one-time) — `deploy`
now takes only `public_key` and always runs the pinned hash. `deploy`
itself stays deliberately unauthenticated: "anyone can pay to deploy
anyone's wallet" is the intended, safe design `polaris-oracle` already
relies on to onboard a user who holds zero XLM — that's only ever safe
once the code being deployed can no longer be the caller's choice.

This is a breaking interface change from the previously-deployed testnet
factory. Verified live: deployed a fresh factory instance
(`CCLDHPEJBENBV3GRZRHO7RH5OZRTBWP7DWB36L3OCWLKEEAP6FJTTJ5U`), initialized
it, and confirmed via the running `polaris-oracle` (a real email-login
wallet deploy) plus a direct on-chain `get_public_key` read that the
deployed wallet's stored key matches exactly what the backend generated —
the whole path working end to end on the corrected contract, not just in
`cargo test`.

## The capital-efficiency vault

`contracts/vault` is additive and isolated — **zero changes to
`contracts/market`**. What it actually fixes: every new market's
`initial_liquidity` used to be wired from the admin's own keypair by hand
at `initialize()` time, and that seed capital sat siloed in that one
market's reserves for its entire life, recovered only at settlement into a
bare `treasury` wallet address with no accounting at all. Fragmented
capital, manual per-market funding, no record of what's deployed where.

This is deliberately *not* what "cross-margin" usually means in a
derivatives system — netting risk across open positions, a liquidation
engine, portfolio-level price marking. That model was considered and
rejected: it would reintroduce exactly the undercollateralized-position
risk class this repo's fund-safety work (bugs 2, 5 above) has spent real
effort eliminating from `contracts/market`. The vault centralizes *custody
and accounting* of LP capital while every individual market stays exactly
as fully, independently collateralized as it already was —
`Market.treasury` is already a generic `Address`; pointing it at this
contract's address is the entire integration.

- `deposit(from, amount)` — LP capital in, permissionless (same "anyone can
  pay" shape as the wallet factory's `deploy`).
- `withdraw(admin, to, amount)` — admin-gated capital out. Funds a new
  market's seed liquidity (what `polaris-oracle`'s `MarketFactoryService`
  draws on before calling the existing `deployMarket()`), or an LP
  redemption. Fails closed on the wrong signer, same as every other
  admin-gated entrypoint in this system (`AdminGuard`, the wallet factory's
  own `initialize`).
- `redeem_from_market(admin, market_id)` — collects the vault's own payout
  from a settled/cancelled market it was `treasury` for, by calling that
  market's existing, already-tested `redeem` on the vault's own behalf.
  The one cross-contract call in this design.

**v1 is deliberately simple, stated up front rather than glossed over**
(matching `contracts/smart-wallet`'s own "a property this build does not
implement"): single-admin-owned capital, not fractional LP shares.
Multiple depositors can `deposit`, but there's no proportional-withdrawal
accounting yet — a real multi-LP product needs share accounting before
this is safe to open past a single trusted operator.

**A real build issue, not just a design note — worth knowing if you touch
this contract:** `redeem_from_market` needs a client for `polaris-market`,
but depending on that crate directly (for its generated
`PolarisMarketClient`) pulls its `#[contract]`-exported wasm symbols into
the *vault's own* compiled output. Confirmed the hard way: `cargo build
--release --target wasm32v1-none` linked "successfully" but warned
`function signature mismatch: initialize`, because both contracts export a
same-named function and the linker was merging them into one binary.
Fixed with `soroban_sdk::contractimport!`, which reads the target wasm's
embedded interface spec at compile time — the same spec `polaris-oracle`'s
`contract.Spec.fromWasm` reads at runtime — to generate a byte-correct
client (including the right `Result<T, Error>` unwrapping) without linking
in any of that wasm's executable code. This makes `polaris-market`'s
*compiled release wasm* a build-time dependency of `polaris-vault` that
Cargo's own dependency graph doesn't know about — build `polaris-market`
first, on its own, before building anything else; see "Building &
testing" below and this repo's CI workflow for the explicit two-step
build this requires.

## Feed ID

`feed_id` is an `initialize` parameter, not hardcoded. For this build it
defaults to a testnet placeholder (`100`, see `polaris-oracle`'s config) —
production deployment must look up Pyth's actual registered Lazer feed ID
for XLM/USD before going live.

## Building & testing

```sh
# unit tests (native target; 51 across the whole workspace, 28 in polaris-market alone)
cargo test --workspace

# release WASM (requires `rustup target add wasm32v1-none`)
# polaris-market first, on its own — polaris-vault's contractimport! reads
# its compiled wasm as a file at build time, a dependency Cargo's own
# graph doesn't know about (see "The capital-efficiency vault" above).
cargo build --release --target wasm32v1-none -p polaris-market
cargo build --release --target wasm32v1-none -p polaris-mock-lazer \
  -p polaris-smart-wallet -p polaris-smart-wallet-factory -p polaris-vault
shasum -a 256 target/wasm32v1-none/release/*.wasm
```

Current build (recorded here for reference — regenerate after any contract
change; the backend's `MARKET_WASM_HASH` / mock-lazer deploy config must
track whatever hash is actually uploaded):

| Contract | Size | SHA-256 |
|---|---|---|
| `polaris_market.wasm` | 45,378 bytes | `082acedae464c0f65be4c27358847243e99990cd44263c98f805544b7896aa02` |
| `polaris_mock_lazer.wasm` | 649 bytes | `7840d96cc309b74e37b5ec22f37e978eaec0aef3feb00146a6e8ce3bdee7087d` |
| `polaris_smart_wallet.wasm` | 25,308 bytes | `7f03d5d0c640280a38b36b5fb7e4fa9b4d3d0cfb77764d4a66207e3812407616` |
| `polaris_smart_wallet_factory.wasm` | 6,427 bytes | `921a1e1dbf1b78b9928926cd0662e444feb480dc8f570d36b69509a55006e565` |
| `polaris_vault.wasm` | 10,568 bytes | `5847a7781556d7c4e727e63fe48a84d6bed9ff6c34686e6a0b09a03d3c925c3d` |

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
