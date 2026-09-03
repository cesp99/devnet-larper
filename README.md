# devnet-larper

**Larpmaxxing for Solana devs.** A fast, batched miner for the Solana **devnet**
proof-of-work faucet, so you can hold seven figures of SOL, feel like a
millionaire, and devmog every developer friend who is still begging the
public airdrop for 1 SOL every 8 hours.

```
[92.8 min] dest 1004.26 SOL (+831.50)  payer 1004.26 SOL  mined 831.50 SOL (8.96 SOL/min) ...
target reached: destination holds 1004.2577 SOL
```

Devnet SOL is worth exactly nothing. That is the point. Nobody can tell the
difference on a screenshot.

## Why this exists

The [devnet-pow](https://github.com/Ellipsis-Labs/devnet-pow) faucet program
(`PoWSNH2hEZogtCg1Zgm51FnkmJperzYDgPK4fvs8taL`) pays 0.02 SOL to anyone who
presents a signature from a keypair whose base58 public key starts with `AAA`.
The reference CLI grinds keys one at a time and makes several RPC calls per
claim, so it tops out at a few SOL per minute and stalls whenever the public
RPC rate-limits you. At that pace a million devnet SOL takes months.

`devnet-larper` gets there in a couple of hours:

- **Fast key grinding.** Instead of hashing random seeds, each thread walks
  the curve by repeatedly adding the base point (one extended-coordinate
  addition per candidate) and compresses 512 candidates with a single field
  inversion. The field arithmetic comes from `fiat-crypto`, and every hit is
  cross-checked against `curve25519-dalek` before it is used.
- **Batched claims.** Six faucet claims are packed into one transaction
  (about 1170 of the 1232 available bytes). Keys with an `AAAA` prefix claim
  the difficulty-3 and difficulty-4 specs in one go.
- **Direct-to-leader sending.** Transactions go straight to the upcoming
  leaders over QUIC (TPU) with preflight skipped, so the RPC node is only used
  for blockhashes, balances and status checks.
- **Resends and accounting.** Signatures that have not landed after 15 s are
  re-signed with a fresh blockhash and resent (up to 6 tries). Mined SOL can be
  forwarded to your flex wallet in fixed-size batches.

On a 24-core machine against the public devnet RPC this sustained roughly
800 SOL per minute.

> This only works on devnet. The faucet is a shared resource funded by
> Ellipsis Labs, so mine what you need for the larp and leave some for the
> next guy.

## Requirements

- Rust 1.75 or newer (`cargo`).
- A payer keypair JSON file with a little devnet SOL for fees. Each claim costs
  a transaction-fee share plus rent for a small receipt account, so 0.1 SOL is
  plenty to bootstrap. Create one with `solana-keygen new -o payer.json` and
  fund it from the [web faucet](https://faucet.solana.com) or with
  `solana airdrop 1 <pubkey> -u devnet`. This is the last time you will ever
  have to do that.
- Outbound UDP/QUIC to devnet validators for TPU sending. If your network
  blocks it, pass `--no-tpu` to send through the RPC node instead.

## Install

Prebuilt binaries for Linux (x86_64, aarch64), macOS (Apple Silicon)
and Windows are on the [releases page](https://github.com/cesp99/devnet-larper/releases).
Each archive contains the `devnet-larper` binary, this README and the license.
The CI builds target a portable CPU baseline (AVX2 on x86_64); building from
source on your own machine is a bit faster because it tunes for your CPU.

## Build

```sh
cargo build --release
```

`.cargo/config.toml` sets `-C target-cpu=native`, so the binary is tuned for
the machine it is built on and may not run elsewhere. Remove that file if you
need a portable build.

## Usage

Become a devnet millionaire. Mine into your wallet until it holds 1,000,000 SOL:

```sh
./target/release/devnet-larper -k payer.json -t 1000000
```

Keep the miner wallet separate and forward the loot to your main wallet in
1000 SOL batches:

```sh
./target/release/devnet-larper -k payer.json -d <YOUR_FLEX_WALLET> -t 1000000 -b 1000
```

Full option list:

```
  -k, --payer <FILE>                  Path to the payer keypair (JSON)
  -d, --dest <PUBKEY>                 Forward mined SOL to this pubkey in batches of --batch SOL
  -t, --target <SOL>                  Stop once the destination (or payer) holds this many SOL [default: 1000]
  -b, --batch <SOL>                   Size of each transfer from payer to destination [default: 100]
      --reserve <SOL>                 SOL to always keep in the payer wallet for fees [default: 5]
      --senders <SENDERS>             Number of sender threads [default: 4]
      --rate <TX/S>                   Aggregate send rate, transactions per second [default: 20]
      --claims-per-tx <N>             Faucet claims packed into one transaction [default: 6]
      --max-inflight <N>              Maximum number of unconfirmed transactions in flight [default: 400]
      --threads <N>                   Grinder threads (defaults to all cores)
      --rpc <URL>                     JSON RPC endpoint [env: RPC_URL]
      --ws <URL>                      Websocket endpoint for leader tracking [env: WS_URL]
      --no-tpu                        Send through the RPC node instead of directly to leaders
      --cu-limit <CU>                 Compute unit limit per transaction [default: 320000]
      --priority-fee <MICROLAMPORTS>  Optional priority fee per compute unit
```

Example output:

```
fast grinder self-test passed
grinding on 24 threads, payer AuDN...Ujvw, dest 9qVM...jNC5, tpu=true
tx size with 6 claims: 1172 bytes (limit 1232)
[0.3 min] dest 1000.00 SOL (+0.00)  payer 36.35 SOL  mined 31.35 SOL (93.54 SOL/min)  sent 278 fail 0 resent 0 landed 48 dropped 0 pending 297
[1.0 min] dest 1100.00 SOL (+100.00)  payer 12.10 SOL  mined 107.10 SOL (107.10 SOL/min)  ...
transferred 100.00 SOL to 9qVM...jNC5: 5Kj...
```

## Notes and tuning

- **Rate.** The default 20 tx/s (120 claims/s, about 144 SOL/min) is
  deliberately gentle. Raise `--rate` and `--max-inflight` if the `dropped`
  counter stays at zero; back off if it climbs.
- **Compute limit.** Six claims need around 300k compute units. Values below
  that fail with `ComputationalBudgetExceeded`; the default leaves headroom.
- **Public RPC.** `api.devnet.solana.com` rate-limits aggressively. The client
  keeps RPC use low (one status poll per 5 s, balances every 20 s), but for
  long runs a private RPC endpoint via `--rpc`/`--ws` is more reliable.
- **Batching.** With `--dest`, SOL first accumulates in the payer and is moved
  in `--batch` sized transfers once the payer holds `batch + reserve`. Whatever
  is left below that threshold stays in the payer when the target is reached.
- **Stopping.** Balances are polled every 20 s and new transactions are held
  back once the in-flight ones would cover the target, so the final balance
  overshoots by at most a handful of claims.

## How the grinder works

An ed25519 public key is `s·B` for a scalar `s`. Starting from a random `s`,
the grinder computes `(s+1)·B, (s+2)·B, …` with one mixed point addition each,
which is far cheaper than a full scalar multiplication per candidate. Points
are kept in extended projective coordinates; converting a batch to the affine
`y` coordinate that base58 encodes requires one inversion per point, which is
replaced by a single inversion plus three multiplications per point
(Montgomery's trick). A candidate is a hit when its 32-byte encoding falls in
the byte range that base58-encodes with an `AAA` prefix. The scalar is then
wrapped in an `ExpandedSecretKey` so it can sign the claim without ever
existing as a seed.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE). Fork it, improve it, share it back.
