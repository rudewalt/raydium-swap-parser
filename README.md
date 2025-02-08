# Raydium SwapBaseIn events Monitor

A Rust program that monitors block updates with Raydium transactions and extracts swap(v4 SwapBaseIn) events.

## Overview

This program uses the Solana client library to subscribe to block updates and extract Raydium transaction logs. It then decodes the logs to extract SwapBaseIn events and writes them to a JSON file.

## Installation

To run this program, you'll need to have Rust and the Solana client library installed. You can install the dependencies using the following command:

```bash
cargo build
```

## Usage

To run the program, use the following command:

```bash
WS_URL=<solana-ws-endpoint> cargo run
```

The program will start monitoring block updates and writing swap events to a JSON file specified by the OUT_FILE environment variable. You can customize the program's behavior by setting the following environment variables:

- WS_URL: the URL of the Solana WebSocket endpoint, required
- PROGRAM_ID: the ID of the Raydium V4 AMM program, optional (by default Raydium v4 AMM on mainnet "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8")
- SLOTS: the number of slots to monitor, optional (default 100)
- OUT_FILE: the path to the output JSON file, optional (default "out.json")