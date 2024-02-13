use std::num::NonZeroU32;

use anyhow::Context;
use mimalloc::MiMalloc;
use pathfinder_common::receipt::Receipt;
use pathfinder_common::transaction::Transaction;
use pathfinder_common::{BlockHeader, BlockNumber, ChainId};
use pathfinder_executor::ExecutionState;
use pathfinder_storage::{BlockId, JournalMode, Storage};

// Due to the amount of JSON parsing that gets done during execution we use an alternate
// allocator here: mimalloc. According to benchmarks re_execute performs roughly 25% better
// when using mimalloc.
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Re-execute transactions in a range of blocks.
///
/// Iterates over specified blocks in the database and re-executes all transactions within
/// those blocks
///
/// Usage:
/// `cargo run --release -p pathfinder --example re_execute ./mainnet.sqlite 50000 51000`
fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .compact()
        .init();

    let n_cpus = std::thread::available_parallelism().unwrap().get();

    let database_path = std::env::args().nth(1).unwrap();
    let storage = Storage::migrate(database_path.into(), JournalMode::WAL, 1)?
        .create_pool(NonZeroU32::new(n_cpus as u32 * 2).unwrap())?;
    let mut db = storage
        .connection()
        .context("Opening database connection")?;

    let first_block = std::env::args().nth(2).unwrap();
    let first_block: u64 = first_block.parse().unwrap();

    let (latest_block, chain_id) = {
        let tx = db.transaction().unwrap();
        let (latest_block, _) = tx.block_id(BlockId::Latest)?.unwrap();
        let latest_block = latest_block.get();
        let chain_id = get_chain_id(&tx).unwrap();
        (latest_block, chain_id)
    };

    let last_block = std::env::args()
        .nth(3)
        .map(|s| str::parse(&s).unwrap())
        .unwrap_or(latest_block);

    let (tx, rx) = crossbeam_channel::bounded::<Work>(10);

    let executors = (0..n_cpus)
        .map(|_| {
            let storage = storage.clone();
            let rx = rx.clone();
            std::thread::spawn(move || execute(storage, chain_id, rx))
        })
        .collect::<Vec<_>>();

    tracing::info!(%first_block, %last_block, "Re-executing blocks");

    let start_time = std::time::Instant::now();
    let mut num_transactions: usize = 0;

    for block_number in first_block..=last_block {
        let transaction = db.transaction().unwrap();
        let block_id = BlockId::Number(BlockNumber::new_or_panic(block_number));
        let block_header = transaction.block_header(block_id)?.unwrap();
        let transactions_and_receipts = transaction
            .transaction_data_for_block(block_id)?
            .context("Getting transactions for block")?;
        drop(transaction);

        let (transactions, receipts): (Vec<_>, Vec<_>) =
            transactions_and_receipts.into_iter().unzip();

        num_transactions += transactions.len();

        tracing::debug!(%block_number, num_transactions=%transactions.len(), "Submitting block");

        tx.send(Work {
            header: block_header,
            transactions,
            receipts,
        })
        .unwrap();
    }

    drop(tx);

    for executor in executors {
        executor.join().expect("Executor expected to shut down");
    }

    let elapsed = start_time.elapsed().as_millis();

    tracing::debug!(%num_transactions, %elapsed, "Finished");

    Ok(())
}

fn get_chain_id(tx: &pathfinder_storage::Transaction<'_>) -> anyhow::Result<ChainId> {
    use pathfinder_common::consts::{
        GOERLI_INTEGRATION_GENESIS_HASH, GOERLI_TESTNET_GENESIS_HASH, MAINNET_GENESIS_HASH,
    };

    let (_, genesis_hash) = tx
        .block_id(BlockNumber::GENESIS.into())
        .unwrap()
        .context("Getting genesis hash")?;

    let chain = match genesis_hash {
        MAINNET_GENESIS_HASH => ChainId::MAINNET,
        GOERLI_TESTNET_GENESIS_HASH => ChainId::GOERLI_TESTNET,
        GOERLI_INTEGRATION_GENESIS_HASH => ChainId::GOERLI_INTEGRATION,
        _ => anyhow::bail!("Unknown chain"),
    };

    Ok(chain)
}

#[derive(Debug)]
struct Work {
    header: BlockHeader,
    transactions: Vec<Transaction>,
    receipts: Vec<Receipt>,
}

fn execute(storage: Storage, chain_id: ChainId, rx: crossbeam_channel::Receiver<Work>) {
    while let Ok(work) = rx.recv() {
        let start_time = std::time::Instant::now();
        let num_transactions = work.transactions.len();

        let mut connection = storage.connection().unwrap();

        let db_tx = connection.transaction().expect("Create transaction");

        let execution_state = ExecutionState::trace(&db_tx, chain_id, work.header.clone(), None);

        let transactions = work
            .transactions
            .into_iter()
            .map(|tx| pathfinder_rpc::compose_executor_transaction(&tx, &db_tx))
            .collect::<Result<Vec<_>, _>>();

        let transactions = match transactions {
            Ok(transactions) => transactions,
            Err(error) => {
                tracing::error!(block_number=%work.header.number, %error, "Transaction conversion failed");
                continue;
            }
        };

        match pathfinder_executor::estimate(execution_state, transactions, false) {
            Ok(fee_estimates) => {
                for (estimate, receipt) in fee_estimates.iter().zip(work.receipts.iter()) {
                    if let Some(actual_fee) = receipt.actual_fee {
                        let actual_fee = u128::from_be_bytes(
                            actual_fee.0.to_be_bytes()[16..].try_into().unwrap(),
                        );

                        // L1 handler transactions have a fee of zero in the receipt.
                        if actual_fee == 0 {
                            continue;
                        }

                        let gas_price = work.header.eth_l1_gas_price.0;
                        let actual_gas_consumed = actual_fee / gas_price.max(1);

                        let estimated_gas_consumed = estimate.gas_consumed.as_u128();

                        let diff = actual_gas_consumed.abs_diff(estimated_gas_consumed);

                        if diff > (actual_gas_consumed * 2 / 10) {
                            tracing::warn!(block_number=%work.header.number, transaction_hash=%receipt.transaction_hash, %estimated_gas_consumed, %actual_gas_consumed, estimated_fee=%estimate.overall_fee, %actual_fee, "Estimation mismatch");
                        }
                    }
                }
            }
            Err(error) => {
                tracing::error!(block_number=%work.header.number, ?error, "Transaction re-execution failed");
            }
        }

        let elapsed = start_time.elapsed().as_millis();

        tracing::debug!(block_number=%work.header.number, %num_transactions, %elapsed, "Re-executed block");
    }
}
