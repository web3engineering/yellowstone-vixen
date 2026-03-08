# Yellowstone Vixen OKX

OKX DEX Aggregator client library for Solana swaps.

## Features

- OKX DEX API integration with HMAC authentication
- Solana VersionedTransaction support
- Simple CLI tool for testing swaps

## Library Usage

```rust
use yellowstone_vixen_okx::OkxClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create OKX client
    let client = OkxClient::new(
        "your-api-key".to_string(),
        "your-secret-key".to_string(),
        "your-passphrase".to_string(),
        None, // Use default production URL
    );

    // Get swap instruction
    let response = client.get_swap_instruction(
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC mint
        "So11111111111111111111111111111111111111112",   // Wrapped SOL
        "1000000",                                        // 1 USDC (6 decimals)
        "YourWalletAddress",                              // Your wallet
        "1.0",                                            // 1% slippage
    ).await?;

    // Or get unsigned transaction directly
    let transaction = client.get_unsigned_transaction(
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        "So11111111111111111111111111111111111111112",
        "1000000",
        "YourWalletAddress",
        "1.0",
    ).await?;

    // Sign and submit the transaction...

    Ok(())
}
```

## CLI Tool

The `okx-swap` binary provides a simple command-line interface for executing swaps.

### Usage

```bash
cargo run --bin okx-swap --release -- \
  --from-mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --to-mint So11111111111111111111111111111111111111112 \
  --amount 1000000 \
  --slippage 1.0 \
  --private-key "YOUR_BASE58_PRIVATE_KEY" \
  --okx-api-key "YOUR_OKX_API_KEY" \
  --okx-secret-key "YOUR_OKX_SECRET_KEY" \
  --rpc-url https://api.mainnet-beta.solana.com
```

### Parameters

- `--from-mint`: Source token mint address
- `--to-mint`: Destination token mint address
- `--amount`: Amount in raw token units (e.g., 1000000 for 1 USDC with 6 decimals)
- `--slippage`: Slippage tolerance as percentage (default: "1.0")
- `--private-key`: Your wallet's private key in base58 format
- `--okx-api-key`: OKX API key
- `--okx-secret-key`: OKX API secret key
- `--okx-passphrase`: OKX API passphrase (optional, default: "")
- `--rpc-url`: Solana RPC URL (default: https://api.mainnet-beta.solana.com)
- `--okx-base-url`: Custom OKX API base URL (optional)
- `--dry-run`: Don't actually submit the transaction (default: false)

### Example with config file data

Using the data from `examples/copy-trader/config.toml`:

```bash
cargo run --bin okx-swap --release -- \
  --from-mint So11111111111111111111111111111111111111112 \
  --to-mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --amount 1000000 \
  --slippage 1.0 \
  --private-key "3CjPLEVPgeyydCgsKPcnN16y15aYiU4bpNcwNZP68bvyEP7PvaZXmkkct4cgT7aY2N6mAF5rK8CLmok9gniGQYxp" \
  --okx-api-key "72544cbd-78c3-4297-8ac2-4aec193cc0da" \
  --okx-secret-key "A041EC1D918A4306D53A91AC1D4EDD71" \
  --okx-passphrase "YOUR_OKX_PASSPHRASE" \
  --dry-run
```

**Important Notes:**
- The `swap-instruction` endpoint requires full authentication including a passphrase
- The passphrase is set when you create your OKX API key in your OKX account settings
- Always use `--dry-run` first to test without submitting transactions!
- If you don't have a passphrase, you'll need to create a new API key with one in your OKX account

## Common Token Mints

- **SOL (Wrapped)**: `So11111111111111111111111111111111111111112`
- **USDC**: `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`
- **USDT**: `Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB`

## Amount Calculations

Token amounts must include decimals:
- 1 USDC (6 decimals) = 1000000
- 1 USDT (6 decimals) = 1000000
- 0.1 SOL (9 decimals) = 100000000
- 1 SOL (9 decimals) = 1000000000

## Authentication vs No Authentication

This library uses the `/swap-instruction` endpoint which requires full authentication:
- API Key
- Secret Key
- Passphrase (created when generating your API key on OKX)

If you're looking for a simpler, non-authenticated API, consider using OKX's `/quote` or `/swap` endpoints instead (not implemented in this library).

## API Documentation

For more details on the OKX DEX API, see:
- V5 API: https://web3.okx.com/build/dev-docs-v5/dex-api/dex-solana-swap-instruction
- V6 API: https://web3.okx.com/build/dev-docs/dex-api/dex-solana-swap-instruction
