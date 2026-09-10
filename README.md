# polaris-contracts

[![CI](https://github.com/samuel2926i39-art/polaris-contracts/actions/workflows/ci.yml/badge.svg)](https://github.com/samuel2926i39-art/polaris-contracts/actions/workflows/ci.yml)

Rust / Soroban smart contracts for **Polaris** — a fully-collateralized,
non-custodial binary prediction market on XLM/USD, settled by a Pyth Lazer
price update verified on-chain.

Seven crates, one workspace:

| Crate | Purpose |
|---|---|
| `contracts/market` | The market contract. Source of truth for all funds and rules. `settle` requires a second, genuinely independent oracle (Reflector Network, optionally a third — RedStone) to agree with Pyth Lazer on-chain before finalizing — see "On-chain second-oracle: Reflector Network" and "A third oracle: RedStone" below. |
| `contracts/ctf-math` | `rlib`-only shared crate — no `#[contract]`/`#[contractimpl]` — holding the pure CTF/AMM math (`cpmm_out`, `cpmm_sell_out`, `apply_fee`, the fee curve), the generic SEP-40 oracle client, and the shared corroboration-check logic (`verify_oracle_corroboration`) both contracts' `settle`/`record_price_checkpoint` call once per configured oracle leg. Extracted out of `contracts/market` so `contracts/perpetual` can reuse the exact same, already-audited implementations rather than a second hand-copied one. Safe as a normal dependency of `contracts/market`, `contracts/perpetual`, *and* `contracts/vault` (which needs `Asset` in scope for its `contractimport!`-generated bindings, not for any math): the wasm-symbol-collision hazard described in `contracts/vault`'s doc comment is specific to depending on a crate that itself exports a full `#[contract]` into the same wasm target, which this deliberately never does. |
| `contracts/perpetual` | A no-expiry, no-leverage sibling to `contracts/market` — continuous trading with no forced terminal settlement. See "The perpetual contract" below. |
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

Verified live on testnet, the full loop, not just `cargo test`: deployed
a fresh vault (`CDDZCX5PT7FURKHTHNKXGNJJNURS4M7BLLNS6BHRV4RJM6CJT25SNCF5`),
deposited 20,000,000 stroops, withdrew 10,000,000 of it to seed a real
market with the vault as `treasury`, settled that market, and called
`redeem_from_market` — a real transaction
(`401ddfea828d7050b367c9a9c8507ff509f0e5d3c6558a772b6bccf9b7d61fd0`,
confirmed `SUCCESS`) that moved exactly 10,000,000 stroops back from the
market to the vault, landing its balance back at the original 20,000,000.
Capital seeded a market, then came home, at par, with no manual
bookkeeping — the entire point of this contract, proven end to end.

## On-chain second-oracle: Reflector Network

`settle` no longer trusts Pyth Lazer's signed payload alone. It also reads
[Reflector Network](https://reflector.network) — genuinely independent
node operators and data pipeline, not just a different Pyth product line
(unlike `polaris-oracle`'s earlier, off-chain-only Lazer-vs-Hermes check,
which is Pyth-internal defense-in-depth) — on-chain, via a real
cross-contract call, and requires the two to agree within
`reflector.tolerance_bps` before finalizing. This is enforced by the
contract, not the backend: a compromised or buggy `polaris-oracle` process
can't bypass it.

Confirmed live before writing any of this, not assumed from docs: Reflector
publishes a free, [SEP-40](https://github.com/stellar/stellar-protocol/blob/master/ecosystem/sep-0040.md)-standardized
testnet oracle at `CCYOZJCOPG34LLQQ7N24YXBM7LL62R7ONMZ3G6WZAAYPB5OYKOMJRN63`
— `stellar contract invoke ... -- lastprice --asset '{"Other":"XLM"}'`
returned a real, fresh price; `decimals()` is 14, `resolution()` is 300s
(a 5-minute update cadence), and the `timestamp` field is Unix seconds
(confirmed against `date +%s`, not assumed — Lazer's own
`feed_update_timestamp` is microseconds, a unit mismatch this codebase has
already been bitten by once, in the settlement-freshness check itself).

**`OracleFeedConfig`** (`initialize`'s 12th parameter for the Reflector
leg, required, not optional — no two-tier trust model where some markets
skip this leg and others silently don't) bundles the corroborating-oracle
settings into one `contracttype` struct rather than four more flat
scalars — `initialize` was already at 11 positional args, and growing to
15+ anonymous ones is exactly the kind of thing that let
`base_fee_bps`/`min_fee_bps` almost get swapped at a call site once
already. Originally named `ReflectorConfig`; renamed once RedStone joined
as a second, independent SEP-40 source using the exact same shape (see
"A third oracle: RedStone" below) — one struct, reused per leg, not
provider-specific:
```rust
pub struct OracleFeedConfig {
    pub contract: Address,       // which oracle instance to read
    pub asset: Asset,            // Stellar(Address) or Other(Symbol) — which
                                  // variant a given provider expects varies
                                  // (Reflector: Other("XLM"); RedStone:
                                  // Stellar(<native XLM SAC>), confirmed live
                                  // against each provider's real contract)
    pub max_staleness_secs: u64, // validated at initialize() against the
                                  // oracle's own live resolution() — can't be
                                  // configured narrower than the oracle updates
    pub tolerance_bps: u32,      // default 150 (1.5%), matching the earlier
                                  // off-chain check's figure
}
```
This build is still XLM-only (see the vault's fixed-`collateral`
precedent for this same "one deliberate limitation, stated plainly"
shape) — only *which* `Asset` variant encodes that differs per provider.

**Fails closed, not gracefully-degrades** — the opposite of the off-chain
check's behavior, and deliberately so (see `lib.rs`'s module doc for the
full reasoning): `None`/stale/divergent Reflector data all reject `settle`
outright. An enforced on-chain invariant that silently skips itself when
inconvenient isn't actually enforcing anything — staleness timing isn't
fully outside an adversary's control, since whoever submits `settle` picks
which side of Reflector's next update they land on. This doesn't strand
funds: the existing permissionless `cancel` after `expiry + grace_period`
is already the unconditional liveness backstop, so a market that can never
get Reflector agreement cancels and refunds through the exact same
well-tested path every other unresolvable market already uses — preserving
"nobody unilaterally decides" even in the failure case.

**Tests** (`contracts/market/src/test.rs`) use a stateful `mod mock_sep40`
(unlike `mock_lazer`, which is stateless — divergence/staleness/
unavailability tests need per-test-controlled return values), a mock of
the *generic* SEP-40 interface reused for both the Reflector and RedStone
legs (two independent registered instances of the same mock — both real
providers implement the identical interface, confirmed live for each).
Its `Asset`/`PriceData` types come from `contracts/ctf-math/src/lib.rs`'s
`sep40` module rather than being redeclared, closing off an
XDR-shape-drift trap: two independent declarations of a
nominally-identical `contracttype` would each compile fine but produce
mutually-incompatible wire encoding. Covers: agreement settles normally; a
small in-tolerance divergence (real cross-path noise between two
aggregations, not a data error) doesn't block settlement; a gross
divergence rejects and leaves the market `Open`, then — concrete proof
nothing is stranded, not just an assertion about it — correcting the mock
price and re-calling `settle` succeeds normally; `None` and a stale
timestamp both fail closed; and one explicit regression using the *exact
same inputs* as `settle_yes_wins_on_price_at_or_above_strike` (which
predates this feature) to prove a Lazer payload that used to settle
successfully now correctly gets rejected once Reflector disagrees with it.

**This is a new contract version**, not a patch to already-deployed
instances — deployed wasm is immutable, so any market already live on the
previous `initialize` signature keeps running exactly as it always has;
only markets created after this ships get N-of-2 protection, same as every
other contract change in this repo.

## A third oracle: RedStone

`settle`/`record_price_checkpoint` can optionally corroborate against a
**third**, genuinely independent SEP-40 source — RedStone — on top of
Reflector, closing the gap between "N-of-2" and real multi-provider
redundancy. Design research (not just implementation) went into this
before any code was written:

- Of the candidate providers investigated (DIA, Band Protocol, Chainlink,
  RedStone), RedStone is the only one with a real, live, queryable
  SEP-40-compatible contract on Stellar today. Confirmed directly
  on-chain, not from docs: reading
  `CBMGLKUQZVSAIL5CPDDAWSUY7MAKXISHMOZEVLMBUWBMFGHRJSR4WYRF.lastprice(Asset::Stellar(<native XLM SAC>))`
  on **mainnet** returned a genuinely fresh price (`18789745` @
  `decimals()=8` → $0.18789745, timestamp within seconds of `date +%s` at
  query time), matching Reflector's own testnet price to within noise.
- **Unanimous, not majority.** A market configured with a `redstone` leg
  requires *every* configured oracle to agree — any one disagreeing or
  unavailable rejects the call, exactly like the Reflector leg already
  does alone. This was a deliberate choice, not the default: Polymarket's
  UMA optimistic oracle had an $85M dispute gamed via permissionless
  quorum voting, and UMA's fix was to *restrict* its voter set rather than
  broaden it — the industry pattern for binary-outcome oracle
  agreement is "loosen the rule and it gets gamed," not "more sources is
  strictly safer." A 2-of-3 majority would also mean an adversary only
  needs to control the *same* two sources as today's N-of-2, gaining
  nothing from the third source's presence; unanimous agreement is the
  only rule where adding a genuinely independent third source actually
  raises the bar.
- **RedStone ships two different Stellar contracts** — this mattered for
  which to integrate against. The one matching the existing SEP-40 client
  shape (`lastprice`/`decimals`/`resolution`) — the wrapper above —
  **admits, in its own on-chain help text, two deliberate deviations from
  the SEP-40 spec**: `decimals()` returns the maximum precision across
  *every* asset RedStone has registered on that contract, not XLM's own
  precision, and can change if a higher-precision feed is added later;
  `resolution()` is owner-mutable, not fixed. (RedStone's other contract,
  a dedicated per-feed `redstone_price_feed-XLM`, has clean per-asset
  `decimals()` but a completely different interface —
  `read_price`/`read_timestamp` in **milliseconds**, no `resolution()` at
  all.) The wrapper was chosen anyway — it reuses the existing client code
  — with the specific landmine closed by a guard (`OracleCheckError::DecimalsChanged`,
  in `contracts/ctf-math/src/lib.rs`'s `verify_oracle_corroboration`):
  each leg's `decimals()` is fetched live exactly once, at `initialize()`,
  and pinned; every later `settle()`/`record_price_checkpoint()` call
  re-checks the live value against that pin and fails closed on any
  drift, regardless of cause.
- **RedStone's Stellar deployment is mainnet-only** — no testnet contract
  exists (confirmed by listing RedStone's own
  `deployments/stellarMultiFeed` directory in
  `redstone-finance/redstone-oracles-monorepo`: every file is `.mainnet`,
  none `.testnet`). This is why `contracts/market`'s `redstone` parameter
  is `Option<OracleFeedConfig>` rather than required like `reflector` —
  making it mandatory would silently break every testnet market this
  project's own dev workflow depends on. This isn't a retreat from the
  "no two-tier trust model" principle above (about not letting an admin
  cheaply opt out of an equally-available check) — it's a structural fact
  that RedStone isn't equally available everywhere yet, stated plainly.
  `contracts/perpetual`'s checkpoint feature doesn't have this asymmetry:
  it was already opt-in as a whole bundle, so `redstone` is a *required*
  field of `PriceOracleConfig` whenever that bundle is configured at all —
  no independent "Reflector-only" checkpoint configuration exists there.
- **Live-verified this round**: the RedStone reads above (real mainnet
  contract, real fresh price, correct `Asset::Stellar` encoding). **Not
  live-verified**: a funded `settle()`/`record_price_checkpoint()` call
  against real RedStone data end-to-end — that needs a real, funded
  mainnet deployment, a separate and explicitly-authorized decision given
  real financial exposure, not bundled into this round. The verification
  logic itself is fully covered by unit tests against a mock (see below).

**Tests**: `mod mock_sep40` is registered as two independent instances per
test that needs both legs — the Reflector-shaped one and a RedStone-
shaped one, same mock, since both real providers implement the same
interface. New coverage on top of the existing Reflector tests: unanimous
3-way agreement settles/checkpoints normally; a RedStone-only divergence
rejects even when Reflector agrees (proving unanimous, not majority);
RedStone unavailable/stale fail closed the same way Reflector's do; and
the decimals-pin guard specifically — a mock RedStone that reports a
different `decimals()` on a later call than the value observed at
`initialize()` gets rejected with `RedstoneDecimalsChanged`, proving the
landmine described above is actually closed. `contracts/market`'s
existing 32 pre-RedStone tests (unconfigured `redstone: None`) all pass
unmodified — the regression bar this whole feature is built around.

## The perpetual contract

`contracts/perpetual` started as a request for crypto-perpetual-futures
mechanics — margin, leverage, funding payments against a reference price.
That design was rejected in two stages before any of `contracts/perpetual`
was written, both recorded in the contract's own module doc rather than
silently dropped:

1. A paper (arXiv 2605.10400, "Resolution-Aware Perpetual Futures on
   Binary Prediction Markets") proves that any
   leverage `L > 1` applied to a binary 0/1-payout claim creates a
   *structural, guaranteed* insolvency mode on the adverse outcome, and
   that real mitigations (dynamic margin, leverage compression, staged
   halts) don't reliably fix it against real Polymarket price data. So:
   no margin, no leverage, ever, here — every position is fully
   collateralized 1:1 at every instant, identical in spirit to
   `contracts/market`'s own invariant.
2. A fully-collateralized attempt at "funding" (transferring a bounded
   fraction of the losing side's AMM pool to the winning side's pool each
   period) was designed, then found — by adversarial review, with a
   concrete numeric counterexample — to break the actual conservation
   invariant this codebase depends on everywhere else: `pool_yes`/
   `pool_no` are two **independent** share ledgers, each of which must
   satisfy `pool_side + Σ(balances_side) == total_supply` on its own.
   Editing both pool totals without a matching balance/`total_supply`
   change manufactures unbacked claims on one side and destroys real
   backing on the other — the same bug *class* as the cancellation-payout
   double-count this contract's `redeem` doc comment warns about, a
   different code path. The honest fix (lazy, cumulative-index funding
   accrual) is itself a well-known bug-prone pattern (rebasing-token
   accounting) that would need its own dedicated review round — dropped
   rather than shipped half-trusted.

**What shipped instead**: no fixed expiry, and continuous exit liquidity
via the same already-audited `buy`/`sell` mechanics as `contracts/market`
(reused verbatim from `contracts/ctf-math`, not reimplemented) — a holder
never waits for a terminal event to realize a price move, they just
`sell()` at the current AMM-implied price, any time. The anchor to reality
is organic arbitrage, the same mechanism Polymarket itself already relies
on with no funding rate at all.

- `initialize` takes no `strike_price`/`expiry`/`grace_period` — `status`
  starts `Open` and, under normal operation, never leaves it.
- `record_price_checkpoint(payload)` is permissionless and reuses the
  exact dual-oracle (Lazer + Reflector) verification `settle` uses, but
  has **zero economic effect** — it only records `last_price_cents`/
  `last_price_at` for observability. No pool, balance, or `total_supply`
  mutation, ever; a dedicated test (`checkpoint_records_price_and_changes_nothing_else`)
  asserts every other field is byte-identical before and after a real
  checkpoint call.
- `terminate(admin)` is the admin-gated wind-down safety valve — stated
  explicitly as v1 scope (single admin, same convention as the vault's own
  "not fractional LP shares yet"). A perpetual market has no strike price,
  so there's nothing to resolve YES/NO *against*; `terminate` reuses
  `contracts/market`'s already-audited `Cancelled`-redemption treatment
  exactly instead — every complementary YES+NO pair is worth 0.5
  collateral each, the only per-holder formula guaranteed solvent
  regardless of trading history — which needs **no oracle call at all**.
  `status` only ever has two values, `Open`/`Terminated`.
- `trading_continues_correctly_across_a_very_long_time_window_with_no_expiry_ever_set`
  advances the ledger timestamp by a full simulated year mid-test, then
  keeps trading — proving "no fixed expiry" is genuinely absent from every
  gate, not just an unenforced field.

**A soroban-sdk gotcha worth recording**: the natural first draft stored
the optional oracle config as a `price_oracle: Option<PriceOracleConfig>`
field directly on the contract's main `#[contracttype]` struct. That fails
to compile — but only under `cargo test`, not a plain release build, which
made it briefly confusing. Reason: soroban-sdk 26.1's `#[contracttype]`
macro generates each struct field's XDR (`ScVal`) conversion via a
fallible `TryFrom<&FieldType>`, but `Option<T>`'s only route to `ScVal` is
a blanket `From<Option<T>>` impl requiring an *infallible* `T:
Into<ScVal>` — which a custom struct's generated (fallible) conversion
never satisfies. That `ScVal` codegen path is itself gated behind
soroban-sdk's `testutils` feature, which a `cargo test` build always
enables via Cargo's feature unification even though it's only ever a
dev-dependency — hence "compiles in release, fails in test." Fix: store
the optional config under its own separate instance-storage key instead
(`DataKey::PriceOracle`, read via a `get_price_oracle()` accessor) —
storage's own `get()` returns `Option<T>` through the always-supported
`Val`-level conversion, sidestepping the struct-field `ScVal` path
entirely. Applies to any `#[contracttype]` struct with an `Option<CustomStruct>`
field, not just this one.

## A second soroban-sdk gotcha: `export = false` can hide a type entirely

Found integrating `polaris-oracle` against the RedStone-updated contracts:
`sep40::Asset` (in `contracts/ctf-math/src/lib.rs`) was declared
`#[contracttype(export = false)]` from when it only ever described an
*external* contract's call shape (Reflector/RedStone's `lastprice`) —
suppressing its own top-level spec entry seemed right for a type that
wasn't part of *this* contract's own interface. Once `Asset` became a
genuine field of `OracleFeedConfig` (an `initialize` parameter), that
stopped being true — but the compiled wasm still had no resolvable spec
entry for it, confirmed live: both the `stellar` CLI's JSON arg parser and
(by the same mechanism — `@stellar/stellar-sdk`'s `contract.Spec`, which
`polaris-oracle` builds its Soroban calls from) any spec-driven encoder
reject a value for that field with `Missing Entry Asset`, even given the
exact correct shape (`{"Other":"XLM"}`/`{"Stellar":"<addr>"}`). This is
different from the `#[contracttype]`/`testutils` gotcha above: that one
only broke a `cargo test` build reading Rust source; this one broke every
*non-Rust* caller (CLI, TypeScript SDK) of an otherwise-fully-working
contract, silently, until something outside this repository tried to
construct the value from JSON. Fix: plain `#[contracttype]` on `Asset`
(dropped `export = false`) — confirmed live afterward, both the `Other`
and `Stellar` variants parse and round-trip correctly through
`get_market`/`get_redstone_oracle`. Rule of thumb: `export = false` is
only safe for a type that will *never* need to be constructed from outside
Rust source (a pure call-out shape) — the moment it becomes a field of any
type in this contract's own `initialize`/public struct surface, it needs a
real spec entry.

## Feed ID

`feed_id` is an `initialize` parameter, not hardcoded. For this build it
defaults to a testnet placeholder (`100`, see `polaris-oracle`'s config) —
production deployment must look up Pyth's actual registered Lazer feed ID
for XLM/USD before going live.

## Building & testing

```sh
# unit tests (native target; 83 across the whole workspace, 38 in
# polaris-market, 22 in polaris-perpetual)
cargo test --workspace

# release WASM (requires `rustup target add wasm32v1-none`)
# polaris-market first, on its own — polaris-vault's contractimport! reads
# its compiled wasm as a file at build time, a dependency Cargo's own
# graph doesn't know about (see "The capital-efficiency vault" above).
cargo build --release --target wasm32v1-none -p polaris-market
cargo build --release --target wasm32v1-none -p polaris-mock-lazer \
  -p polaris-smart-wallet -p polaris-smart-wallet-factory -p polaris-vault \
  -p polaris-perpetual
shasum -a 256 target/wasm32v1-none/release/*.wasm
```

`contracts/ctf-math` produces no wasm of its own (`rlib` only, no
`#[contract]`) — it's compiled into whichever of `polaris-market` /
`polaris-perpetual` depend on it, not deployed independently.

Current build (recorded here for reference — regenerate after any contract
change; the backend's `MARKET_WASM_HASH` / mock-lazer deploy config must
track whatever hash is actually uploaded):

| Contract | Size | SHA-256 |
|---|---|---|
| `polaris_market.wasm` | 57,617 bytes | `6c99075c91ed438833595bd032b8ec6024a1d638256504aa6c7c6837f01fa9fc` |
| `polaris_perpetual.wasm` | 57,367 bytes | `b62909fa2c78b083a8973b7b733b0ce9e4b8cdb1efd1d0169f61c93370af9727` |
| `polaris_mock_lazer.wasm` | 649 bytes | `7840d96cc309b74e37b5ec22f37e978eaec0aef3feb00146a6e8ce3bdee7087d` |
| `polaris_smart_wallet.wasm` | 25,308 bytes | `7f03d5d0c640280a38b36b5fb7e4fa9b4d3d0cfb77764d4a66207e3812407616` |
| `polaris_smart_wallet_factory.wasm` | 6,427 bytes | `c004f94b67dabab142804abc924cf28b0f159b3c4d4c18a3f26e017f642caf9e` |
| `polaris_vault.wasm` | 10,568 bytes | `5847a7781556d7c4e727e63fe48a84d6bed9ff6c34686e6a0b09a03d3c925c3d` |

## Deploying (needs the Stellar CLI, not available in this build environment)

```sh
stellar contract deploy --wasm target/wasm32v1-none/release/polaris_market.wasm \
  --source deployer --network testnet
stellar contract invoke --id <CONTRACT_ID> --source deployer --network testnet -- \
  initialize --admin <ADMIN> --collateral <XLM_SAC> --strike_price 1500000 \
  --expiry <UNIX_TS> --grace_period 3600 --lazer_contract <LAZER_ID> \
  --feed_id 100 --base_fee_bps 100 --min_fee_bps 20 \
  --treasury <TREASURY> --initial_liquidity 10000000000 \
  --reflector '{"contract":"CCYOZJCOPG34LLQQ7N24YXBM7LL62R7ONMZ3G6WZAAYPB5OYKOMJRN63","asset":{"Other":"XLM"},"max_staleness_secs":600,"tolerance_bps":150}' \
  --redstone null
# --redstone is optional (Option<OracleFeedConfig>) — omit/null on testnet,
# where no RedStone contract exists yet (see "A third oracle: RedStone"
# above). On mainnet, pass e.g.:
#   --redstone '{"contract":"CBMGLKUQZVSAIL5CPDDAWSUY7MAKXISHMOZEVLMBUWBMFGHRJSR4WYRF","asset":{"Stellar":"<native XLM SAC>"},"max_staleness_secs":600,"tolerance_bps":150}'
```
