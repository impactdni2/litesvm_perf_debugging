use std::{hint::black_box, time::Duration};

use agave_feature_set::FeatureSet;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hashbrown::HashMap;
use jupiter_swap_api_client::{
    JupiterSwapApiClient,
    quote::{QuoteRequest, QuoteResponse},
    swap::SwapRequest,
    transaction_config::TransactionConfig,
};
use litesvm::LiteSVM;
use minstant::Instant;
use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_client::{client_error, nonblocking::rpc_client::RpcClient};
use solana_message::{AccountMeta, AddressLookupTableAccount, Instruction};
use solana_pubkey::Pubkey;
use solana_sdk::{
    clock::Clock, pubkey, signature::read_keypair_file, signer::Signer,
    transaction::VersionedTransaction,
};
use solana_sdk_ids::address_lookup_table;
use tokio::runtime::Runtime;
use tracing::*;

// !! FILL THESE IN !!
const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
// !! Replace with your own keypair !!  Needs to have at least SWAP_AMOUNT of SOL in it.
// If you don't want to use a real keypair, you can generate a dummy one, but will need to turn off the RPC simulation below.
const KEYPAIR_FILE: &str = "xxxx.json";

const JUPITER_API_URL: &str = "https://lite-api.jup.ag/swap/v1";
const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const SWAP_AMOUNT: u64 = 1_000_000;

const SVM_PROGRAMS_LIST: &[Pubkey] = &[
    spl_token::ID,
    spl_token_2022::ID,
    pubkey!("Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo"),
    pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"),
    spl_associated_token_account::ID,
    address_lookup_table::ID,
    solana_sdk_ids::config::ID,
    pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"),
    pubkey!("AddressLookupTab1e1111111111111111111111111"),
    pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ"),
    pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"),
    pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
    pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),
    pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),
];

fn bench_simulate(c: &mut Criterion) {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let mut group = c.benchmark_group("sim_perf");

    // let payer = Keypair::new();
    let payer = read_keypair_file(KEYPAIR_FILE).unwrap();
    let payer_pubkey = payer.pubkey();
    let client = RpcClient::new(RPC_URL.to_string());

    for protocol in ["Meteora DLMM", "Raydium CLMM", "Whirlpool", "Pump.fun Amm"] {
        let runtime = Runtime::new().unwrap();
        let (txn, instructions, txn_accounts, instruction_accounts, clock) =
            runtime.block_on(async {
                let quote =
                    get_api_quote(protocol.to_string(), WSOL_MINT, USDC_MINT, SWAP_AMOUNT).await;
                let instructions = quote_to_instructions(&payer_pubkey, quote.clone()).await;
                let txn = quote_to_txn(&payer_pubkey, quote).await;
                // warn!("Txn: {:#?}", txn);

                let txn_account_keys = txn_accounts_to_list(&client, &txn).await;
                let mut txn_accounts = load_accounts(&client, txn_account_keys).await;
                let mut instruction_accounts = load_accounts(
                    &client,
                    instructions
                        .iter()
                        .map(|instruction| {
                            instruction
                                .accounts
                                .iter()
                                .map(|account| account.pubkey)
                                .collect::<Vec<_>>()
                        })
                        .flatten()
                        .collect::<Vec<_>>(),
                )
                .await;

                let payer_account = txn_accounts.get(&payer_pubkey);
                if payer_account.is_none()
                    || payer_account.as_ref().unwrap().is_none()
                    || payer_account.as_ref().unwrap().as_ref().unwrap().lamports == 0
                {
                    let mut payer_account = Account::default();
                    payer_account.lamports = SWAP_AMOUNT * 10;

                    txn_accounts.insert(payer_pubkey, Some(payer_account.clone()));
                    instruction_accounts.insert(payer_pubkey, Some(payer_account.clone()));
                }

                let clock = load_clock(&client).await;

                (txn, instructions, txn_accounts, instruction_accounts, clock)
            });

        debug!(
            "Loaded accounts: {:#?}",
            txn_accounts
                .iter()
                .map(|(pubkey, account)| (pubkey, account.as_ref().map(|account| account.lamports)))
                .collect::<Vec<_>>()
        );

        let mut svm = make_svm();
        svm.set_sysvar(&clock);

        group.bench_with_input(
            BenchmarkId::new("litesvm", protocol),
            &(txn.clone(), txn_accounts.clone()),
            |b, (txn, txn_accounts)| {
                b.iter_custom(|n| {
                    let mut duration = Duration::ZERO;

                    for _ in 0..n {
                        for (pubkey, account) in txn_accounts {
                            // Don't overwrite the programs we added...
                            if SVM_PROGRAMS_LIST.contains(pubkey) {
                                continue;
                            }

                            if let Some(account) = account.clone() {
                                svm.set_account(*pubkey, account).unwrap();
                            } else {
                                svm.set_account(*pubkey, Account::default()).unwrap();
                            }
                        }

                        let start = Instant::now();
                        let _ = black_box({
                            let result = svm.simulate_transaction(txn.clone());
                            // warn!("Simulate: {:#?}", result);
                            result
                        });
                        duration += start.elapsed();
                    }
                    duration
                });
            },
        );

        let mut mollusk = Mollusk::default();
        mollusk.sysvars.clock = clock;
        for program in SVM_PROGRAMS_LIST.iter() {
            debug!("Adding program: {}", program);
            mollusk.add_program(
                &program,
                &format!("programs/{program}"),
                &solana_sdk_ids::bpf_loader_upgradeable::id(),
            );
        }

        group.bench_with_input(
            BenchmarkId::new("mollusk", protocol),
            &(instructions, instruction_accounts),
            |b, (instructions, instruction_accounts)| {
                b.iter_custom(|n| {
                    let mut duration = Duration::ZERO;

                    for _ in 0..n {
                        let accounts = instruction_accounts
                            .into_iter()
                            .map(|(pubkey, account)| (*pubkey, account.clone().unwrap_or_default()))
                            .collect::<Vec<_>>();

                        mollusk.logger = Some(Default::default());

                        let start = Instant::now();
                        let _ = black_box({
                            let result =
                                mollusk.process_instruction_chain(&instructions, &accounts);
                            // warn!("Simulate: {:#?}", result);
                            result
                        });
                        duration += start.elapsed();
                    }
                    duration
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("rpc", protocol), &txn, |b, txn| {
            b.iter_custom(|n| {
                let mut duration = Duration::ZERO;

                for _ in 0..n {
                    let start = Instant::now();
                    let _ = black_box({
                        let result =
                            runtime.block_on(async { client.simulate_transaction(txn).await });
                        // warn!("Simulate: {:#?}", result);
                        result
                    });
                    duration += start.elapsed();
                }
                duration
            });
        });
    }

    group.finish();
}

criterion_group! {
  name = benches;
  config = Criterion::default();
  targets = bench_simulate
}
criterion_main!(benches);

fn make_svm() -> LiteSVM {
    let start = Instant::now();

    let mut svm = LiteSVM::new()
        .with_builtins()
        .with_sysvars()
        .with_precompiles()
        .with_sigverify(false)
        .with_blockhash_check(false)
        .with_feature_set(FeatureSet::all_enabled());

    debug!("Intial svm init time: {:?}", start.elapsed());

    for program in SVM_PROGRAMS_LIST.iter() {
        debug!("Adding program: {}", program);
        match svm.add_program_from_file(program, format!("programs/{program}.so")) {
            Ok(_) => {}
            Err(e) => {
                panic!("Failed to load program: {} -> {:?}", program, e);
            }
        }
    }

    svm
}

async fn get_api_quote(
    protocols: String,
    input_mint: Pubkey,
    output_mint: Pubkey,
    amount_in: u64,
) -> QuoteResponse {
    let jupiter_swap_api_client = JupiterSwapApiClient::new(JUPITER_API_URL.to_string());

    let quote_request = QuoteRequest {
        amount: amount_in,
        input_mint,
        output_mint,
        slippage_bps: 0,
        only_direct_routes: Some(true),
        dexes: Some(protocols),
        ..QuoteRequest::default()
    };

    let quote = jupiter_swap_api_client.quote(&quote_request).await.unwrap();
    quote
}

async fn quote_to_txn(payer: &Pubkey, quote_response: QuoteResponse) -> VersionedTransaction {
    let jupiter_swap_api_client = JupiterSwapApiClient::new(JUPITER_API_URL.to_string());

    let swap_response = jupiter_swap_api_client
        .swap(
            &SwapRequest {
                user_public_key: *payer,
                quote_response,
                config: TransactionConfig {
                    use_shared_accounts: Some(true),
                    wrap_and_unwrap_sol: true,
                    ..Default::default()
                },
            },
            None,
        )
        .await
        .unwrap();

    let txn: VersionedTransaction = bincode::deserialize(&swap_response.swap_transaction).unwrap();

    txn
}

async fn quote_to_instructions(payer: &Pubkey, quote_response: QuoteResponse) -> Vec<Instruction> {
    let jupiter_swap_api_client = JupiterSwapApiClient::new(JUPITER_API_URL.to_string());

    let swap_response = jupiter_swap_api_client
        .swap_instructions(&SwapRequest {
            user_public_key: *payer,
            quote_response,
            config: TransactionConfig {
                use_shared_accounts: Some(true),
                wrap_and_unwrap_sol: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();

    let mut instructions = vec![];
    if let Some(token_ledger_instruction) = swap_response.token_ledger_instruction {
        instructions.push(token_ledger_instruction);
    }
    // Mollusk doesn't support compute budget instructions.
    // instructions.extend(swap_response.compute_budget_instructions);
    instructions.extend(swap_response.setup_instructions);
    instructions.push(swap_response.swap_instruction);
    if let Some(cleanup_instruction) = swap_response.cleanup_instruction {
        instructions.push(cleanup_instruction);
    }

    instructions
}

async fn get_lookup_table_remote(
    client: &RpcClient,
    lut_address: &Pubkey,
) -> client_error::Result<AddressLookupTableAccount> {
    let result = client.get_account(lut_address).await?;
    let address_lookup_table = AddressLookupTable::deserialize(&result.data)
        .expect("Couldn't deserialize address lookup table account");
    Ok(AddressLookupTableAccount {
        key: *lut_address,
        addresses: address_lookup_table.addresses.to_vec(),
    })
}

fn txn_static_accounts(txn: &VersionedTransaction) -> Vec<AccountMeta> {
    let mut accounts = Vec::with_capacity(txn.message.static_account_keys().len());

    let static_accounts = txn.message.static_account_keys();
    for i in 0..static_accounts.len() {
        let is_signer = txn.message.is_signer(i);
        let is_writable = txn.message.is_maybe_writable(i, None);
        accounts.push(AccountMeta {
            pubkey: static_accounts[i],
            is_signer,
            is_writable,
        });
    }

    accounts
}

async fn txn_accounts_with_client(
    client: &RpcClient,
    txn: &VersionedTransaction,
) -> Vec<AccountMeta> {
    let mut accounts = txn_static_accounts(txn);

    if let Some(address_table_lookups) = txn.message.address_table_lookups() {
        for lookup_table_addr in address_table_lookups {
            let lookup_table = get_lookup_table_remote(client, &lookup_table_addr.account_key)
                .await
                .unwrap();
            for index in &lookup_table_addr.writable_indexes {
                accounts.push(AccountMeta {
                    pubkey: lookup_table.addresses[*index as usize],
                    is_signer: false,
                    is_writable: true,
                });
            }
            for index in &lookup_table_addr.readonly_indexes {
                accounts.push(AccountMeta {
                    pubkey: lookup_table.addresses[*index as usize],
                    is_signer: false,
                    is_writable: false,
                });
            }
        }
    }

    accounts
}

async fn txn_accounts_to_list(client: &RpcClient, txn: &VersionedTransaction) -> Vec<Pubkey> {
    let account_metas = txn_accounts_with_client(client, &txn).await;
    let mut account_keys = account_metas
        .iter()
        .map(|meta| meta.pubkey)
        .collect::<Vec<_>>();

    if let Some(lookup_tables) = txn.message.address_table_lookups() {
        let luts = lookup_tables
            .iter()
            .map(|lut| lut.account_key)
            .collect::<Vec<_>>();
        account_keys.extend(luts);
    }

    account_keys
}

async fn load_accounts(
    client: &RpcClient,
    account_keys: Vec<Pubkey>,
) -> HashMap<Pubkey, Option<Account>> {
    let mut loaded_accounts: HashMap<Pubkey, Option<Account>> = HashMap::new();

    let start = Instant::now();
    let accounts = client.get_multiple_accounts(&account_keys).await.unwrap();
    let load_accounts_time = start.elapsed();

    let start = Instant::now();
    let accounts_with_pubkeys = account_keys
        .into_iter()
        .zip(accounts.into_iter())
        .map(|(pubkey, account)| (pubkey, account));
    loaded_accounts.extend(accounts_with_pubkeys);
    let set_accounts_time = start.elapsed();

    debug!(
        "load_accounts_time: {:?}, set accounts time: {:?}",
        load_accounts_time, set_accounts_time
    );

    loaded_accounts
}

async fn load_clock(client: &RpcClient) -> Clock {
    let (slot, epoch_info) = tokio::join!(client.get_slot(), client.get_epoch_info());
    let slot = slot.unwrap();
    let epoch_info = epoch_info.unwrap();

    let epoch_start_slot = epoch_info.epoch * epoch_info.slots_in_epoch;
    let block_time = client.get_block_time(slot).await.unwrap();

    // Our server doesn't have archive data, so we just estimate here...
    let slot_diff = slot - epoch_start_slot;
    let epoch_start_block_time = block_time as f64 - (slot_diff as f64 / 2.5); // 400ms per slot, 2.5 slots per second

    Clock {
        slot,
        epoch_start_timestamp: epoch_start_block_time as i64,
        epoch: epoch_info.epoch,
        leader_schedule_epoch: epoch_info.epoch + 1,
        unix_timestamp: block_time,
    }
}
