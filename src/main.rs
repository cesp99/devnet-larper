// devnet-larper: fast batched miner for the Solana devnet proof-of-work faucet.
// Copyright (C) 2026 Carlo Esposito <carlo@aploi.de>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, either version 3 of the License, or (at your option) any later
// version. See the LICENSE file for details.

//! devnet-larper: a fast batched client for the Solana devnet proof-of-work faucet.
//!
//! The faucet program (`PoWSNH2hEZogtCg1Zgm51FnkmJperzYDgPK4fvs8taL`) pays out
//! SOL to anyone who can present a signature from a keypair whose base58 pubkey
//! starts with a run of `A`s. This client grinds such keypairs on every core using
//! incremental point stepping with batched field inversion, packs several claims
//! into a single transaction, pushes transactions straight to the current leaders
//! over QUIC, and resends anything that gets dropped.
mod consts;
mod fastgrind;

use clap::Parser;
use sha2::{Digest, Sha256};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_client::tpu_client::{TpuClient, TpuClientConfig};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Signature, Signer};
use solana_sdk::signer::SignerError;
use solana_sdk::system_instruction;
use solana_sdk::system_program;
use solana_sdk::transaction::Transaction;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type QuicTpu = TpuClient<
    solana_quic_client::QuicPool,
    solana_quic_client::QuicConnectionManager,
    solana_quic_client::QuicConfig,
>;

/// Address of the proof-of-work faucet program on devnet.
const PROGRAM: &str = "PoWSNH2hEZogtCg1Zgm51FnkmJperzYDgPK4fvs8taL";
/// Payout (in lamports) of the faucet specs this client claims from.
const AMOUNT: u64 = 20_000_000; // 0.02 SOL
/// Difficulties (number of leading `A`s) that have a 0.02 SOL spec on devnet.
const DIFFICULTIES: [u8; 2] = [3, 4];
const LAMPORTS_PER_SOL: f64 = 1e9;

#[derive(Parser, Debug)]
#[command(
    name = "devnet-larper",
    version,
    about = "Fast batched miner for the Solana devnet proof-of-work faucet",
    after_help = "Mining continues until the destination (or the payer, when no \
                  destination is given) holds at least --target SOL."
)]
struct Args {
    /// Path to the payer keypair (JSON). Pays fees and receives faucet payouts.
    #[arg(short = 'k', long, value_name = "FILE")]
    payer: String,

    /// Forward mined SOL to this pubkey in batches of --batch SOL.
    /// When omitted, everything stays in the payer wallet.
    #[arg(short, long, value_name = "PUBKEY")]
    dest: Option<String>,

    /// Stop once the destination (or payer) holds this many SOL.
    #[arg(short, long, default_value_t = 1000.0, value_name = "SOL")]
    target: f64,

    /// Size of each transfer from payer to destination, in SOL.
    #[arg(short, long, default_value_t = 100.0, value_name = "SOL")]
    batch: f64,

    /// SOL to always keep in the payer wallet for fees.
    #[arg(long, default_value_t = 5.0, value_name = "SOL")]
    reserve: f64,

    /// Number of sender threads.
    #[arg(long, default_value_t = 4)]
    senders: usize,

    /// Aggregate send rate, transactions per second.
    #[arg(long, default_value_t = 20.0, value_name = "TX/S")]
    rate: f64,

    /// Faucet claims packed into one transaction (bounded by the 1232-byte tx limit).
    #[arg(long, default_value_t = 6, value_name = "N")]
    claims_per_tx: usize,

    /// Maximum number of unconfirmed transactions in flight.
    #[arg(long, default_value_t = 400, value_name = "N")]
    max_inflight: usize,

    /// Grinder threads (defaults to all cores).
    #[arg(long, value_name = "N")]
    threads: Option<usize>,

    /// JSON RPC endpoint.
    #[arg(
        long,
        env = "RPC_URL",
        default_value = "https://api.devnet.solana.com",
        value_name = "URL"
    )]
    rpc: String,

    /// Websocket endpoint (used for leader tracking when sending via TPU).
    #[arg(
        long,
        env = "WS_URL",
        default_value = "wss://api.devnet.solana.com/",
        value_name = "URL"
    )]
    ws: String,

    /// Send through the RPC node instead of directly to leaders over QUIC.
    #[arg(long)]
    no_tpu: bool,

    /// Compute unit limit per transaction.
    #[arg(long, default_value_t = 320_000, value_name = "CU")]
    cu_limit: u32,

    /// Optional priority fee in micro-lamports per compute unit.
    #[arg(long, value_name = "MICROLAMPORTS")]
    priority_fee: Option<u64>,
}

/// A keypair defined directly by its ed25519 scalar (found by incremental grinding).
pub struct GroundKey {
    pub pk: Pubkey,
    pub expanded: ed25519_dalek::ExpandedSecretKey,
    pub dalek_pk: ed25519_dalek::PublicKey,
}

impl Signer for GroundKey {
    fn pubkey(&self) -> Pubkey {
        self.pk
    }
    fn try_pubkey(&self) -> Result<Pubkey, SignerError> {
        Ok(self.pk)
    }
    fn sign_message(&self, message: &[u8]) -> Signature {
        Signature::from(self.expanded.sign(message, &self.dalek_pk).to_bytes())
    }
    fn try_sign_message(&self, message: &[u8]) -> Result<Signature, SignerError> {
        Ok(self.sign_message(message))
    }
    fn is_interactive(&self) -> bool {
        false
    }
}

/// Byte range [lo, hi) of 32-byte big-endian values whose base58 encoding starts with "AAA".
pub fn aaa_range() -> ([u8; 32], [u8; 32]) {
    fn pad(v: Vec<u8>) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[32 - v.len()..].copy_from_slice(&v);
        out
    }
    let lo = bs58::decode(format!("AAA{}", "1".repeat(41)))
        .into_vec()
        .unwrap();
    let hi = bs58::decode(format!("AAB{}", "1".repeat(41)))
        .into_vec()
        .unwrap();
    (pad(lo), pad(hi))
}

struct Spec {
    difficulty: u8,
    spec: Pubkey,
    source: Pubkey,
}

fn leading_a(pk: &Pubkey) -> usize {
    bs58::encode(pk.as_ref())
        .into_string()
        .chars()
        .take_while(|c| *c == 'A')
        .count()
}

fn spawn_grinders(threads: usize) -> Receiver<GroundKey> {
    let (tx, rx) = sync_channel::<GroundKey>(4096);
    for _ in 0..threads {
        let tx = tx.clone();
        std::thread::spawn(move || fastgrind::grind_thread(tx));
    }
    rx
}

struct Pending {
    keys: Vec<GroundKey>,
    sig: Signature,
    sent_at: Instant,
    tries: u32,
}

fn sol(lamports: u64) -> f64 {
    lamports as f64 / LAMPORTS_PER_SOL
}

fn main() {
    let args = Args::parse();
    let payer = Arc::new(read_keypair_file(&args.payer).unwrap_or_else(|e| {
        eprintln!("cannot read payer keypair {}: {}", args.payer, e);
        std::process::exit(1);
    }));
    let dest: Option<Pubkey> = args.dest.as_deref().map(|d| {
        Pubkey::from_str(d).unwrap_or_else(|_| {
            eprintln!("invalid destination pubkey: {}", d);
            std::process::exit(1);
        })
    });
    let program = Pubkey::from_str(PROGRAM).unwrap();
    let max_ix = args.claims_per_tx.max(1);
    let batch_lamports = (args.batch * LAMPORTS_PER_SOL) as u64;
    let reserve = (args.reserve * LAMPORTS_PER_SOL) as u64;
    let use_tpu = !args.no_tpu;

    let specs: Vec<Spec> = DIFFICULTIES
        .iter()
        .map(|&d| {
            let (spec, _) = Pubkey::find_program_address(
                &[b"spec", &d.to_le_bytes(), &AMOUNT.to_le_bytes()],
                &program,
            );
            let (source, _) = Pubkey::find_program_address(&[b"source", spec.as_ref()], &program);
            Spec {
                difficulty: d,
                spec,
                source,
            }
        })
        .collect();
    let disc: [u8; 8] = Sha256::digest(b"global:airdrop")[..8].try_into().unwrap();

    let client = Arc::new(RpcClient::new_with_commitment(
        args.rpc.clone(),
        CommitmentConfig::confirmed(),
    ));
    let tpu: Option<Arc<QuicTpu>> = if use_tpu {
        Some(Arc::new(
            TpuClient::new(
                client.clone(),
                &args.ws,
                TpuClientConfig { fanout_slots: 8 },
            )
            .unwrap_or_else(|e| {
                eprintln!("cannot create TPU client ({}); retry with --no-tpu", e);
                std::process::exit(1);
            }),
        ))
    } else {
        None
    };

    fastgrind::self_test();
    let threads = args.threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });
    let rx = spawn_grinders(threads);
    println!(
        "grinding on {} threads, payer {}, dest {}, tpu={}",
        threads,
        payer.pubkey(),
        dest.map(|d| d.to_string())
            .unwrap_or_else(|| "(payer)".into()),
        use_tpu
    );

    // Sender pool: receives signed transactions and pushes them to the cluster.
    let (stx, srx) = sync_channel::<Transaction>(64);
    let srx = Arc::new(Mutex::new(srx));
    let sent = Arc::new(AtomicU64::new(0));
    let send_fail = Arc::new(AtomicU64::new(0));
    for _ in 0..args.senders.max(1) {
        let srx = srx.clone();
        let sent = sent.clone();
        let send_fail = send_fail.clone();
        let tpu = tpu.clone();
        let url = args.rpc.clone();
        let per_sender = Duration::from_secs_f64(args.senders.max(1) as f64 / args.rate.max(0.1));
        std::thread::spawn(move || {
            let rpc = RpcClient::new_with_commitment(url, CommitmentConfig::confirmed());
            let cfg = RpcSendTransactionConfig {
                skip_preflight: true,
                ..Default::default()
            };
            loop {
                let transaction = match srx.lock().unwrap().recv() {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let t0 = Instant::now();
                let ok = match &tpu {
                    Some(t) => t.send_transaction(&transaction),
                    None => rpc.send_transaction_with_config(&transaction, cfg).is_ok(),
                };
                if ok {
                    sent.fetch_add(1, Ordering::Relaxed);
                } else {
                    send_fail.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(rem) = per_sender.checked_sub(t0.elapsed()) {
                    std::thread::sleep(rem);
                }
            }
        });
    }

    let build = |keys: &Vec<GroundKey>, blockhash: Hash| -> Transaction {
        let mut ixs: Vec<Instruction> = vec![];
        for kp in keys {
            let prefix = leading_a(&kp.pubkey());
            for s in specs.iter().filter(|s| s.difficulty as usize <= prefix) {
                let (receipt, _) = Pubkey::find_program_address(
                    &[
                        b"receipt",
                        kp.pubkey().as_ref(),
                        &s.difficulty.to_le_bytes(),
                    ],
                    &program,
                );
                ixs.push(Instruction {
                    program_id: program,
                    accounts: vec![
                        AccountMeta::new(payer.pubkey(), true),
                        AccountMeta::new_readonly(kp.pubkey(), true),
                        AccountMeta::new(receipt, false),
                        AccountMeta::new_readonly(s.spec, false),
                        AccountMeta::new(s.source, false),
                        AccountMeta::new_readonly(system_program::id(), false),
                    ],
                    data: disc.to_vec(),
                });
            }
        }
        ixs.insert(
            0,
            ComputeBudgetInstruction::set_compute_unit_limit(args.cu_limit),
        );
        if let Some(p) = args.priority_fee {
            ixs.insert(1, ComputeBudgetInstruction::set_compute_unit_price(p));
        }
        let mut signers: Vec<&dyn Signer> = vec![payer.as_ref()];
        for k in keys {
            signers.push(k);
        }
        Transaction::new_signed_with_payer(&ixs, Some(&payer.pubkey()), &signers, blockhash)
    };

    let mut blockhash = client.get_latest_blockhash().expect("blockhash");
    let mut bh_time = Instant::now();
    let mut balance = client.get_balance(&payer.pubkey()).expect("payer balance");
    let mut dest_balance = match dest {
        Some(d) => client.get_balance(&d).expect("dest balance"),
        None => balance,
    };
    let start_dest = dest_balance;
    let start_balance = balance;
    let mut last_bal = Instant::now();
    let mut last_check = Instant::now();
    let start = Instant::now();
    let mut resent: u64 = 0;
    let mut landed: u64 = 0;
    let mut dropped: u64 = 0;
    let mut transferred: u64 = 0;
    let mut pending: Vec<Pending> = vec![];
    let mut carry: Option<GroundKey> = None;
    let mut printed_size = false;
    let check_after = Duration::from_secs(15);
    let max_tries = 6;
    let transfer_busy = Arc::new(AtomicBool::new(false));

    loop {
        if sol(dest_balance) >= args.target {
            println!(
                "target reached: {} holds {:.4} SOL",
                if dest.is_some() {
                    "destination"
                } else {
                    "payer"
                },
                sol(dest_balance)
            );
            break;
        }
        if bh_time.elapsed() > Duration::from_secs(15) {
            match client.get_latest_blockhash() {
                Ok(b) => {
                    blockhash = b;
                    bh_time = Instant::now();
                }
                Err(e) => eprintln!("blockhash err: {}", e),
            }
        }
        if last_bal.elapsed() > Duration::from_secs(20) {
            if let Ok(b) = client.get_balance(&payer.pubkey()) {
                balance = b;
            }
            dest_balance = match dest {
                Some(d) => client.get_balance(&d).unwrap_or(dest_balance),
                None => balance,
            };
            last_bal = Instant::now();
            let mined =
                (balance as f64 - start_balance as f64 + transferred as f64) / LAMPORTS_PER_SOL;
            let mins = start.elapsed().as_secs_f64() / 60.0;
            println!(
                "[{:.1} min] dest {:.2} SOL (+{:.2})  payer {:.2} SOL  mined {:.2} SOL ({:.2} SOL/min)  sent {} fail {} resent {} landed {} dropped {} pending {}",
                mins,
                sol(dest_balance),
                (dest_balance as f64 - start_dest as f64) / LAMPORTS_PER_SOL,
                sol(balance),
                mined,
                mined / mins.max(0.01),
                sent.load(Ordering::Relaxed),
                send_fail.load(Ordering::Relaxed),
                resent,
                landed,
                dropped,
                pending.len()
            );
            // Batch transfer to destination when a full batch has accumulated.
            if let Some(dest) = dest {
                if balance >= batch_lamports + reserve && !transfer_busy.load(Ordering::Relaxed) {
                    transfer_busy.store(true, Ordering::Relaxed);
                    let client = client.clone();
                    let payer = payer.clone();
                    let busy = transfer_busy.clone();
                    transferred += batch_lamports;
                    balance -= batch_lamports;
                    std::thread::spawn(move || {
                        for attempt in 1..=5 {
                            let bh = match client.get_latest_blockhash() {
                                Ok(b) => b,
                                Err(_) => {
                                    std::thread::sleep(Duration::from_secs(3));
                                    continue;
                                }
                            };
                            let ix = system_instruction::transfer(
                                &payer.pubkey(),
                                &dest,
                                batch_lamports,
                            );
                            let t = Transaction::new_signed_with_payer(
                                &[ix],
                                Some(&payer.pubkey()),
                                &[payer.as_ref()],
                                bh,
                            );
                            match client.send_and_confirm_transaction(&t) {
                                Ok(sig) => {
                                    println!(
                                        "transferred {:.2} SOL to {}: {}",
                                        sol(batch_lamports),
                                        dest,
                                        sig
                                    );
                                    break;
                                }
                                Err(e) => {
                                    let msg: String = e.to_string().chars().take(160).collect();
                                    eprintln!("transfer attempt {} failed: {}", attempt, msg);
                                    std::thread::sleep(Duration::from_secs(3));
                                }
                            }
                        }
                        busy.store(false, Ordering::Relaxed);
                    });
                }
            }
        }

        // Check pending signatures; resend the ones that vanished.
        if last_check.elapsed() > Duration::from_secs(5) {
            last_check = Instant::now();
            let due: Vec<usize> = pending
                .iter()
                .enumerate()
                .filter(|(_, p)| p.sent_at.elapsed() > check_after)
                .map(|(i, _)| i)
                .collect();
            if !due.is_empty() {
                let sigs: Vec<_> = due.iter().map(|&i| pending[i].sig).collect();
                let mut statuses = vec![];
                for chunk in sigs.chunks(256) {
                    match client.get_signature_statuses(chunk) {
                        Ok(r) => statuses.extend(r.value),
                        Err(e) => {
                            eprintln!("status err: {}", e);
                            for _ in chunk {
                                statuses.push(None);
                            }
                        }
                    }
                }
                let mut to_remove = vec![];
                for (k, &i) in due.iter().enumerate() {
                    match statuses.get(k) {
                        Some(Some(st)) => {
                            if let Some(err) = &st.err {
                                dropped += 1;
                                if dropped <= 10 {
                                    eprintln!("tx error: {:?}", err);
                                }
                            } else {
                                landed += 1;
                            }
                            to_remove.push(i);
                        }
                        Some(None) => {
                            if pending[i].tries >= max_tries {
                                dropped += 1;
                                to_remove.push(i);
                            } else {
                                let transaction = build(&pending[i].keys, blockhash);
                                pending[i].sig = transaction.signatures[0];
                                pending[i].sent_at = Instant::now();
                                pending[i].tries += 1;
                                resent += 1;
                                stx.send(transaction).unwrap();
                            }
                        }
                        None => {}
                    }
                }
                to_remove.sort_unstable_by(|a, b| b.cmp(a));
                for i in to_remove {
                    pending.swap_remove(i);
                }
            }
            continue;
        }

        // Throttle when the in-flight transactions already cover the remaining target,
        // so we do not overshoot by hundreds of claims.
        let expected = dest_balance + pending.len() as u64 * max_ix as u64 * AMOUNT;
        if pending.len() >= args.max_inflight || sol(expected) >= args.target {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        // Gather keys for a new transaction.
        let mut keys: Vec<GroundKey> = vec![];
        let mut count = 0usize;
        loop {
            let kp = match carry.take() {
                Some(k) => k,
                None => rx.recv().unwrap(),
            };
            let prefix = leading_a(&kp.pubkey());
            let needed = specs
                .iter()
                .filter(|s| s.difficulty as usize <= prefix)
                .count();
            if count + needed > max_ix && !keys.is_empty() {
                carry = Some(kp);
                break;
            }
            count += needed;
            keys.push(kp);
            if count >= max_ix {
                break;
            }
        }
        let transaction = build(&keys, blockhash);
        if !printed_size {
            let size = bincode::serialize(&transaction).unwrap().len();
            println!("tx size with {} claims: {} bytes (limit 1232)", count, size);
            printed_size = true;
        }
        let sig = transaction.signatures[0];
        stx.send(transaction).unwrap();
        pending.push(Pending {
            keys,
            sig,
            sent_at: Instant::now(),
            tries: 1,
        });
    }
}
