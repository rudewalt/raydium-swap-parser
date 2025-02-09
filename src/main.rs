mod raydium_logs;

use crate::raydium_logs::{decode_ray_log, Log};
use anyhow::Result;
use futures_util::StreamExt;
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::rpc_client::SerializableTransaction;
use solana_client::rpc_config::{RpcBlockSubscribeConfig, RpcBlockSubscribeFilter};
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status_client_types::{EncodedTransactionWithStatusMeta, UiConfirmedBlock};
use std::env;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_util::task::TaskTracker;

type Slot = u64;

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
    pub slot: Slot,
    pub amount_in: u64,
    pub min_amount_out: u64,
}

impl SwapBaseIn {
    fn from_log(log: Log, transaction_signature: String, slot: Slot) -> Option<Self> {
        match log {
            Log::SwapBaseIn(sbi) => Some(Self {
                transaction_signature,
                slot,
                amount_in: sbi.amount_in,
                min_amount_out: sbi.minimum_out,
            }),
            _ => None,
        }
    }
}

const RAYDIUM_V4: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
const DEFAULT_SLOT_COUNT: u64 = 100;
const DEFAULT_OUT_FILE: &str = "out.json";

#[tokio::main]
async fn main() {
    let config = Arc::new(parse_config());
    let tracker = TaskTracker::new();

    // channels for communication between tasks
    let (block_sender, mut block_receiver) = unbounded_channel::<(UiConfirmedBlock, Slot)>();
    let (swap_event_sender, mut swap_event_receiver) = unbounded_channel::<SwapBaseIn>();

    tracker.spawn({
        let config = config.clone();
        async move { watch_blocks(&block_sender, &config).await }
    });
    tracker.spawn({
        let config = config.clone();
        async move { filter_swap_events(&config, &mut block_receiver, &swap_event_sender).await }
    });

    tracker.spawn({
        let config = config.clone();
        async move { write_json_result(&config, &mut swap_event_receiver).await }
    });

    tracker.close();

    tracker.wait().await;
}

fn parse_config() -> Config {
    Config {
        ws_url: env::var("WS_URL").expect("WS_URL environment variable not set"),
        raydium_program_id: env::var("PROGRAM_ID").unwrap_or(RAYDIUM_V4.to_string()),
        slot_count: env::var("SLOTS").map_or(DEFAULT_SLOT_COUNT, |v| {
            v.parse::<u64>()
                .expect("SLOTS environment variable wrong format")
        }),
        out_file: env::var("OUT_FILE").unwrap_or(DEFAULT_OUT_FILE.to_string()),
    }
}

/// Attempts to decode encoded transaction and extracts logs from meta.
/// Returns tuple of decoded transaction and logs
fn try_decode_transaction_and_get_logs(
    tx: &EncodedTransactionWithStatusMeta,
) -> Option<(VersionedTransaction, &Vec<String>)> {
    tx.transaction.decode().and_then(|vt| {
        tx.meta
            .as_ref()
            .and_then(|meta| meta.log_messages.as_ref().map(|logs| (vt, logs)))
    })
}

/// Attempts to parse a SwapBaseIn event from a log line.
fn try_parse_swap_base_in(log: &str, signature: String, slot: Slot) -> Option<SwapBaseIn> {
    const RAYDIUM_V4_LOG_PREFIX_LENGTH: usize = 22; // "Program log: ray_log: ";

    match decode_ray_log(&log[RAYDIUM_V4_LOG_PREFIX_LENGTH..]) {
        Ok(log) => SwapBaseIn::from_log(log, signature, slot),
        _ => None,
    }
}

/// It subscribes to block updates that mention the given program id,
/// and then filters out the blocks that don't have transactions.
/// It then sends the block, and it's slot number to the block channel.
///
/// It stops when it reaches the given slot count.
async fn watch_blocks(
    block_sender: &UnboundedSender<(UiConfirmedBlock, Slot)>,
    config: &Config,
) -> Result<()> {
    let ps_client = PubsubClient::new(&config.ws_url).await?;

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

    let mut end_slot: Option<Slot> = None;
    while let Some(block_update) = block_updates.next().await {
        let current_slot = block_update.value.slot;
        if let Some(confirmed_block) = block_update.value.block {
            block_sender.send((confirmed_block, current_slot))?;
        }

        match end_slot {
            None => end_slot = { Some(current_slot + config.slot_count - 1) },
            Some(target_slot) if current_slot >= target_slot => break,
            _ => {}
        }
    }
    let _ = block_unsubscribe().await;

    Ok(())
}

/// It filters out the blocks that don't have transactions,
/// and then parse the ray logs in the transactions.
/// It then sends the SwapBaseIn events to the swap_events_sender.
///
/// It stops when it reaches the given slot count.
async fn filter_swap_events(
    config: &Config,
    block_receiver: &mut UnboundedReceiver<(UiConfirmedBlock, Slot)>,
    swap_events_sender: &UnboundedSender<SwapBaseIn>,
) -> Result<()> {
    // I assume that "ray_log" is always next to "Program {} invoke [2]" string in logs
    let search_pattern = &format!("Program {} invoke [2]", config.raydium_program_id);

    while let Some((block, slot)) = block_receiver.recv().await {
        block
            .transactions
            .iter()
            .flatten()
            .flat_map(try_decode_transaction_and_get_logs)
            .flat_map(|(vt, logs)| {
                let signature = vt.get_signature().to_string();

                // iterate by window with size 2,
                // assumed that the first row is "program invoke [2]", the second row is ray_log
                logs.windows(2).filter_map(move |window| {
                    if window[0].eq(search_pattern) {
                        try_parse_swap_base_in(&window[1], signature.clone(), slot)
                    } else {
                        None
                    }
                })
            })
            .try_for_each(|swap_event| swap_events_sender.send(swap_event))?;
    }

    Ok(())
}

/// Writes received SwapBaseIn events as a json array to a file.
async fn write_json_result(
    config: &Config,
    swap_event_receiver: &mut UnboundedReceiver<SwapBaseIn>,
) -> Result<()> {
    let file = tokio::fs::File::create(&config.out_file).await?;

    let mut writer = tokio::io::BufWriter::new(file);
    writer.write_all(b"[\n").await?;

    let mut first = true;

    // Iterate over the swap events, and write each as a json string to the file.
    while let Some(swap_event) = swap_event_receiver.recv().await {
        let json_str = serde_json::to_string(&swap_event)?;
        if !first {
            writer.write_all(b",\n").await?;
        }

        writer.write_all(json_str.as_bytes()).await?;
        first = false;
        writer.flush().await?;
    }

    // Write the closing bracket of the json array.
    writer.write_all(b"\n]\n").await?;
    writer.flush().await?;

    Ok(())
}
