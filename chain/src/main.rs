pub mod appstate;
pub mod config;
pub mod entity;
pub mod services;

use std::collections::BTreeMap;
use std::env;
use std::sync::atomic::{self, AtomicBool};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use shared::error::{IntoMainError, MainError};
use shared::height::{BlockHeight, FollowingHeights};
use shared::indexed_tx::IndexedTx;
use shared::transaction::Transaction;
use shared::tx_index::{MaspTxIndex, TxIndex};
use tendermint_rpc::client::CompatMode;
use tendermint_rpc::HttpClient;
use tokio::signal;
use tokio::time::sleep;
use tokio_retry::strategy::{jitter, FixedInterval};
use tokio_retry::RetryIf;

use crate::appstate::AppState;
use crate::config::AppConfig;
use crate::entity::chain_state::ChainState;
use crate::entity::commitment_tree::CommitmentTree;
use crate::entity::tx_notes_index::TxNoteMap;
use crate::entity::witness_map::WitnessMap;
use crate::services::masp::update_witness_map;
use crate::services::{
    cometbft as cometbft_service, db as db_service, rpc as rpc_service,
};

const VERSION_STRING: &str = env!("VERGEN_GIT_SHA");
const DEFAULT_INTERVAL: u64 = 5;

#[tokio::main]
async fn main() -> Result<(), MainError> {
    let AppConfig {
        cometbft_url,
        database_url,
        interval,
        verbosity,
    } = AppConfig::parse();

    config::install_tracing_subscriber(verbosity);

    tracing::info!(version = VERSION_STRING, "Started the namada-masp-indexer");
    let exit_handle = must_exit_handle();

    let app_state = AppState::new(database_url).await.into_db_error()?;

    run_migrations(&app_state).await?;

    let (last_block_height, commitment_tree, witness_map) =
        load_committed_state(&app_state).await?;

    let client = HttpClient::builder(cometbft_url.as_str().parse().unwrap())
        .compat_mode(CompatMode::V0_37)
        .build()
        .unwrap();
    let client = Arc::new(client);

    let internal = interval
        .map(|millis| millis * 1000)
        .unwrap_or(DEFAULT_INTERVAL * 1000);
    let retry_strategy = FixedInterval::from_millis(internal).map(jitter);

    for block_height in FollowingHeights::after(last_block_height) {
        if must_exit(&exit_handle) {
            break;
        }

        _ = RetryIf::spawn(
            retry_strategy.clone(),
            || {
                let client = client.clone();
                let witness_map = witness_map.clone();
                let commitment_tree = commitment_tree.clone();
                let app_state = app_state.clone();
                let chain_state = ChainState::new(block_height);

                build_and_commit_masp_data_at_height(
                    block_height,
                    &exit_handle,
                    client,
                    witness_map,
                    commitment_tree,
                    app_state,
                    chain_state,
                )
            },
            |_: &MainError| !must_exit(&exit_handle),
        )
        .await
    }

    Ok(())
}

#[inline]
fn must_exit(handle: &AtomicBool) -> bool {
    handle.load(atomic::Ordering::Relaxed)
}

fn must_exit_handle() -> Arc<AtomicBool> {
    let handle = Arc::new(AtomicBool::new(false));
    let task_handle = Arc::clone(&handle);
    tokio::spawn(async move {
        signal::ctrl_c()
            .await
            .expect("Error receiving interrupt signal");
        tracing::info!("Ctrl-c received");
        task_handle.store(true, atomic::Ordering::Relaxed);
    });
    handle
}

async fn run_migrations(app_state: &AppState) -> Result<(), MainError> {
    let mut max_retries = env::var("DATABASE_MAX_MIGRATION_RETRY")
        .unwrap_or_else(|_| 5.to_string())
        .parse::<u64>()
        .unwrap_or(5_u64);
    loop {
        let migration_res = db_service::run_migrations(
            app_state.get_db_connection().await.into_db_error()?,
        )
        .await;

        match &migration_res {
            Ok(_) => {
                return migration_res
                    .context("Failed to run db migrations")
                    .into_db_error();
            }
            Err(e) => {
                tracing::debug!(
                    "Failed runnign migrations: {} ({}/5)",
                    e.to_string(),
                    max_retries
                );
                if max_retries == 0 {
                    return migration_res
                        .context("Failed to run db migrations")
                        .into_db_error();
                }
                max_retries -= 1;
                sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn load_committed_state(
    app_state: &AppState,
) -> Result<(Option<BlockHeight>, CommitmentTree, WitnessMap), MainError> {
    tracing::info!("Loading last committed state from db...");

    let last_block_height = db_service::get_last_synced_block(
        app_state.get_db_connection().await.into_db_error()?,
    )
    .await
    .into_db_error()?;

    let commitment_tree = db_service::get_last_commitment_tree(
        app_state.get_db_connection().await.into_db_error()?,
    )
    .await
    .into_db_error()?
    .unwrap_or_default();

    let witness_map = db_service::get_last_witness_map(
        app_state.get_db_connection().await.into_db_error()?,
    )
    .await
    .into_db_error()?;

    let commitment_tree_len = commitment_tree.size();
    let witness_map_len = witness_map.size();

    if commitment_tree_len == 0 && witness_map_len != 0
        || commitment_tree_len != 0 && witness_map_len == 0
    {
        return Err(anyhow::anyhow!(
            "Invalid database state: Commitment tree size is \
             {commitment_tree_len}, and witness map size is {witness_map_len}"
        ))
        .into_db_error();
    }
    tracing::info!(?last_block_height, "Last state has been loaded");

    shared::error::ok((last_block_height, commitment_tree, witness_map))
}

async fn build_and_commit_masp_data_at_height(
    block_height: BlockHeight,
    exit_handle: &AtomicBool,
    client: Arc<HttpClient>,
    witness_map: WitnessMap,
    commitment_tree: CommitmentTree,
    app_state: AppState,
    chain_state: ChainState,
) -> Result<(), MainError> {
    if must_exit(exit_handle) {
        return Ok(());
    }

    // NB: rollback changes from previous failed commit attempts
    witness_map.rollback();
    commitment_tree.rollback();

    let conn_obj = app_state.get_db_connection().await.into_db_error()?;

    tracing::info!(
        %block_height,
        "Attempting to process new block"
    );

    if !rpc_service::is_block_committed(&client, &block_height)
        .await
        .into_rpc_error()?
    {
        tracing::warn!(
            %block_height,
            "Block was not processed, retrying..."
        );
        return Err(MainError);
    }

    let block_data = {
        tracing::info!(
            %block_height,
            "Fetching block data from CometBFT"
        );
        let block_data =
            cometbft_service::query_masp_txs_in_block(&client, block_height)
                .await
                .into_rpc_error()?;
        tracing::info!(
            %block_height,
            "Acquired block data from CometBFT"
        );
        block_data
    };

    let mut shielded_txs = BTreeMap::new();
    let mut tx_notes_index = TxNoteMap::default();

    tracing::info!(
        %block_height,
        num_transactions = block_data.transactions.len(),
        "Processing new masp transactions...",
    );

    for (idx, Transaction { masp_txs, .. }) in
        block_data.transactions.into_iter()
    {
        for (masp_tx_index, masp_tx) in masp_txs.into_iter().enumerate() {
            let indexed_tx = IndexedTx {
                block_height,
                block_index: TxIndex(idx as u32),
                masp_tx_index: MaspTxIndex(masp_tx_index),
            };

            update_witness_map(
                &commitment_tree,
                &mut tx_notes_index,
                &witness_map,
                indexed_tx,
                &masp_tx,
            )
            .into_masp_error()?;

            shielded_txs.insert(indexed_tx, masp_tx);
        }
    }

    db_service::commit(
        &conn_obj,
        chain_state,
        commitment_tree,
        witness_map,
        tx_notes_index,
        shielded_txs,
    )
    .await
    .into_db_error()?;

    Ok(())
}
