# PAMM First Swaps Tracker

This binary tracks the first 100 swaps (buys and sells) for each token mint on Pump AMM, exposing the data via an HTTP API.

## Features

- Monitors Pump AMM transactions via Yellowstone gRPC
- Parses actual trade amounts from buyEvent and sellEvent
- Extracts mint addresses from instruction accounts
- Tracks first 100 trades per mint (buy/sell, base amount, quote amount, timestamp)
- HTTP server providing JSON API to query swap history

## Building

```bash
cargo build --package yellowstone-vixen-example-pamm-first-swaps --release
```

## Running

```bash
cargo run --package yellowstone-vixen-example-pamm-first-swaps -- --config config.toml
```

### Configuration

Edit `config.toml` to configure the Yellowstone gRPC endpoint:

```toml
[source]
endpoint = "http://your-yellowstone-grpc-endpoint:port"
x_token = "your_token_here"
timeout = 30

[buffer]
jobs = 10
sources-channel-size = 100000
```

## HTTP API

The server runs on `0.0.0.0:8080` by default (configurable via `--http-addr` flag).

### Endpoints

#### Get all swaps
```bash
curl http://localhost:8080/swaps
```

Returns a JSON object mapping mints to their first 10 swaps:
```json
{
  "mint_address_1": [
    {
      "swap_type": "buy",
      "base_amount": 1000000,
      "quote_amount": 500000,
      "timestamp": 123456789
    },
    ...
  ],
  ...
}
```

#### Get swaps for a specific mint
```bash
curl http://localhost:8080/swaps/YOUR_MINT_ADDRESS
```

Returns an array of swap records for the specified mint:
```json
[
  {
    "swap_type": "buy",
    "base_amount": 1000000,
    "quote_amount": 500000,
    "timestamp": 123456789
  },
  {
    "swap_type": "sell",
    "base_amount": 500000,
    "quote_amount": 250000,
    "timestamp": 123456790
  }
]
```

## Implementation Details

### Event Parsing

This implementation properly parses actual traded amounts from Solana events:

1. **Instructions** → Mint address extraction
   - Parses `Buy`, `BuyExactQuoteIn`, and `Sell` instructions
   - Extracts `base_mint` from instruction accounts

2. **Events** → Actual trade amounts
   - Parses "Program data:" logs containing base64-encoded events
   - Decodes event discriminators to identify `buyEvent` vs `sellEvent`
   - Deserializes event data using borsh
   - Extracts actual amounts:
     - `buyEvent`: `base_amount_out` and `quote_amount_in` (actual paid)
     - `sellEvent`: `base_amount_in` and `quote_amount_out` (actual received)

3. **Matching**
   - Matches events with their parent instructions by order
   - Combines mint from instruction with amounts from events

This ensures accurate tracking of actual executed amounts, not just min/max from instruction arguments.

## Schema

### SwapRecord
- `swap_type`: String - "buy" or "sell"
- `base_amount`: u64 - Amount of base token (the mint being traded)
- `quote_amount`: u64 - Amount of quote token (typically SOL/USDC)
- `timestamp`: i64 - Slot number (can be converted to actual timestamp if needed)
