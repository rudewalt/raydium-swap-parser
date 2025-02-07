mod raydium_logs;

use crate::raydium_logs::{decode_ray_log, Log};
use anyhow::{Result};
use futures_util::StreamExt;
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::rpc_client::SerializableTransaction;
use solana_client::rpc_config::{RpcBlockSubscribeConfig, RpcBlockSubscribeFilter};
use solana_transaction_status_client_types::UiConfirmedBlock;
use std::env;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone)]
struct Config {
    pub ws_url: String,
    pub raydium_program_id: String,
    pub slot_count: u64,
    pub out_file: String,
}

#[derive(Debug, serde::Serialize)]
struct SwapBaseIn {
    pub transaction_signature: String,
    pub slot: u64,
    pub amount_in: u64,
    pub min_amount_out: u64,
}

const RAYDIUM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const DEFAULT_SLOT_COUNT: u64 = 100;
const DEFAULT_OUT_FILE: &str = "out.json";

#[tokio::main]
async fn main() {
    let config = Arc::new(Config {
        ws_url: env::var("WS_URL").expect("WS_URL environment variable not set"),
        raydium_program_id: env::var("PROGRAM_ID").unwrap_or(RAYDIUM_V4.to_string()),
        slot_count: env::var("SLOTS").map_or(DEFAULT_SLOT_COUNT, |v| {
            v.parse::<u64>()
                .expect("SLOTS environment variable wrong format")
        }),
        out_file: env::var("OUT_FILE").unwrap_or(DEFAULT_OUT_FILE.to_string()),
    });
    let ps_client = PubsubClient::new(&config.ws_url)
        .await
        .expect("Could not create Pubsub client");

    // channels for communication between tasks
    let (block_sender, mut block_receiver) = unbounded_channel::<UiConfirmedBlock>();
    let (swap_event_sender, mut swap_event_receiver) = unbounded_channel::<SwapBaseIn>();

    let mut join_handles = vec![];
    join_handles.push(tokio::spawn({
        let config = config.clone();
        async move { watch_blocks(&ps_client, &block_sender, &config).await }
    }));
    join_handles.push(tokio::spawn({
        let config = config.clone();
        async move { filter_swap_events(&config, &mut block_receiver, &swap_event_sender).await }
    }));

    join_handles.push(tokio::spawn({
        let config = config.clone();
        async move { write_json_result(&config, &mut swap_event_receiver).await }
    }));

    futures::future::join_all(join_handles).await;
}

/// The task monitors block updates with Raydium transactions and sends updates to the block channel.
async fn watch_blocks(
    ps_client: &PubsubClient,
    block_sender: &UnboundedSender<UiConfirmedBlock>,
    config: &Config,
) -> Result<()> {
    let mut start_slot: u64 = 0;

    let (mut block_updates, block_unsubscribe) = ps_client
        .block_subscribe(
            RpcBlockSubscribeFilter::MentionsAccountOrProgram(
                config.raydium_program_id.to_string(),
            ),
            Some(RpcBlockSubscribeConfig {
                commitment: None,
                encoding: None,
                transaction_details: None,
                show_rewards: None,
                max_supported_transaction_version: Some(0),
            }),
        )
        .await?;

    while let Some(block_update) = block_updates.next().await {
        if let Some(confirmed_block) = block_update.value.block {
            block_sender.send(confirmed_block)?;
        }

        if start_slot == 0 {
            start_slot = block_update.value.slot;
        }

        if start_slot + config.slot_count <= block_update.value.slot {
            let _ = block_unsubscribe().await;
            break;
        }
    }

    Ok(())
}

/// The task listens the block channel for a new blocks, extracts ray_log, decode it,
/// gets only SwapBaseIn events and sends to the swap_events channel
async fn filter_swap_events(
    config: &Config,
    block_receiver: &mut UnboundedReceiver<UiConfirmedBlock>,
    swap_events_sender: &UnboundedSender<SwapBaseIn>,
) -> Result<()> {
    // I assume that "ray_log" is always next to "Program {} invoke [2]" string in logs
    let search_pattern = format!("Program {} invoke [2]", config.raydium_program_id);
    let prefix_length = 22; // skip prefix "Program log: ray_log: " and get slice starting from log data

    while let Some(block) = block_receiver.recv().await {
        // iterate over decoded VersionTransaction and log messages
        for (vt, logs) in block
            .transactions
            .iter()
            .flat_map(|txs| txs)
            .filter_map(|tx| {
                tx.transaction.decode().and_then(|vt| {
                    tx.meta
                        .as_ref()
                        .and_then(|meta| meta.log_messages.as_ref().map(|logs| (vt, logs)))
                })
            })
        {
            let signature = vt.get_signature();

            for swap_event in logs
                .iter()
                .enumerate()
                .filter(|(_index, log)| log.eq(&&search_pattern))
                // use "+1" below, because ray_log next element to ray instruction in array of logs
                .flat_map(|(index, _)| logs.get(index + 1))
                .filter_map(|ray_log| {
                    match decode_ray_log(&ray_log[prefix_length..]) {
                        Ok(Log::SwapBaseIn(swap_base_in_log)) => Some(SwapBaseIn {
                            transaction_signature: signature.to_string(),
                            slot: block.parent_slot + 1,
                            amount_in: swap_base_in_log.amount_in,
                            min_amount_out: swap_base_in_log.minimum_out,
                        }),
                        _ => None,
                    }
                })
            {
                swap_events_sender.send(swap_event)?;
            }
        }
    }

    Ok(())
}

/// Listen swap_events channel and write it to out file
async fn write_json_result(
    config: &Config,
    swap_event_receiver: &mut UnboundedReceiver<SwapBaseIn>,
) -> Result<()> {
    let file = std::fs::File::create(&config.out_file)?;
    let mut writer = std::io::BufWriter::new(file);
    writer.write_all("[\n".as_bytes())?;

    let mut first = true;

    while let Some(swap_event) = swap_event_receiver.recv().await {
        let json_str = serde_json::to_string(&swap_event)?;
        if !first {
            writer.write_all(",\n".as_bytes())?;
        }

        writer.write_all(json_str.as_bytes())?;
        first = false;
        writer.flush()?;
    }

    writer.write_all("\n]\n".as_bytes())?;
    writer.flush()?;

    Ok(())
}
