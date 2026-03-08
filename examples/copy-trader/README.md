# Copy Trading Bot: Solana Wallet Monitor with OKX Auto-Buy

A Yellowstone Vixen application that monitors whitelisted Solana wallets for token sells and automatically executes buy orders via the OKX DEX aggregator API.

## Overview

When a whitelisted wallet sells a token for SOL/stablecoins, this bot:
1. **Detects** the sell by analyzing balance changes in successful transactions
2. **Executes** a buy order for the same token using native SOL via OKX DEX aggregator
3. **Deduplicates** to ensure each token is only bought once (globally)
4. **Tracks** all detected sells and copy trade execution status

## Features

- Real-time monitoring of whitelisted Solana wallet transactions
- Balance-based sell detection (works with any token/DEX)
- Automatic buy execution via OKX DEX aggregator API
- Global deduplication (buy each token only once)
- HTTP monitoring endpoints for status and trade history
- Comprehensive error handling and logging
- Configurable buy amounts and slippage

## Setup

### Prerequisites

1. Rust toolchain (latest stable)
2. Access to a Yellowstone gRPC source endpoint
3. Solana RPC endpoint (mainnet-beta)
4. Private key for signing transactions (with SOL balance)
5. OKX DEX API access (public endpoints, no API key required)

### Installation

1. Clone the repository and navigate to the example:
```bash
cd yellowstone-vixen/examples/copy-trader
```

2. Copy the config template and edit it:
```bash
cp config.toml config.local.toml
vim config.local.toml  # Edit with your settings
```

3. Configure your settings:
   - Add wallet addresses to `whitelist` (wallets to monitor)
   - Set your `private_key` (base58 encoded)
   - Set `buy_amount_sol` (start with 0.001 for testing)
   - Configure `solana_rpc_url` (use a reliable provider)

### Security Warning

**NEVER commit your `config.local.toml` with a real private key!**

Add to `.gitignore`:
```
config.local.toml
*.local.toml
```

## Configuration

### Whitelist
List of Solana wallet addresses (base58) to monitor for sells:
```toml
whitelist = [
    "ARu4n5mFdZogZAravu7CcizaojWnS6oqka37gdLT5SZn",
    "5t478CAxfUsDZDvCZAbuap6cFtv9qe6mb373YixLnoQx"
]
```

### Private Key
Base58-encoded private key for signing buy transactions:
```toml
private_key = "YOUR_PRIVATE_KEY_HERE"
```

To convert from byte array to base58:
```bash
echo "[123,45,67,...]" | python3 -c "import sys, json, base58; print(base58.b58encode(bytes(json.load(sys.stdin))).decode())"
```

### Buy Amount
Amount of SOL to spend on each buy:
```toml
buy_amount_sol = 0.001  # Start small!
```

### Interesting Currencies
Quote tokens to detect (when these increase = payment received):
```toml
[[copy_trading.interesting_currencies]]
name = "USDC"
mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
priority = 90
```

## Usage

### Running the Bot

```bash
cargo run --release -- --config config.local.toml
```

With debug logging:
```bash
RUST_LOG=debug cargo run --release -- --config config.local.toml
```

### HTTP Endpoints

The bot exposes monitoring endpoints on `http://0.0.0.0:8080`:

#### GET /health
Health check endpoint:
```bash
curl http://localhost:8080/health
```

Response:
```json
{"status": "ok"}
```

#### GET /status
Get copy trading statistics:
```bash
curl http://localhost:8080/status
```

Response:
```json
{
  "total_sells_detected": 42,
  "pending": 3,
  "tx_submitted": 15,
  "confirmed": 20,
  "failed": 2,
  "skipped": 2
}
```

#### GET /trades
Get all detected sells and copy trade details:
```bash
curl http://localhost:8080/trades
```

Response:
```json
[
  {
    "timestamp": 1709640123,
    "slot": 123456789,
    "signature": "5j7s...",
    "wallet": "ARu4n5mF...",
    "token_mint": "TokenMint123...",
    "token_amount": 1000.5,
    "quote_mint": "So111111...",
    "quote_amount": 0.05,
    "copy_trade_status": {
      "status": "TxSubmitted",
      "solana_signature": "3kd9..."
    }
  }
]
```

## How It Works

### 1. Sell Detection

The bot monitors transactions involving whitelisted wallets and analyzes balance changes:

```
For each successful transaction:
  1. Find whitelisted wallet account indices
  2. Calculate token balance deltas (post - pre)
  3. Identify decreased non-quote tokens (sold)
  4. Identify increased quote tokens (payment received)
  5. Match sold tokens with quote receipts = SELL EVENT
```

Example log:
```
Sell of 1234.56 token ABC...XYZ for 0.05 SOL detected from wallet ARu4n5mF...!
```

### 2. OKX Buy Execution

When a sell is detected:
1. Check deduplication set (skip if token already bought)
2. Call OKX `/quote` endpoint to get quote
3. Call OKX `/swap` endpoint to get unsigned transaction
4. Deserialize and sign transaction with private key
5. Submit signed transaction to Solana RPC
6. Add token to deduplication set on success

### 3. Global Deduplication

Each token mint is tracked in a global `HashSet`:
- Before buying: check if token already in set
- If yes: skip with reason "Token already bought"
- If no: proceed with buy and add to set on success

This ensures **you only buy each token once**, even if multiple whitelisted wallets sell it.

## Testing

### Safe Testing Strategy

1. **Start with very small amounts**:
   ```toml
   buy_amount_sol = 0.001  # ~$0.20-0.30
   ```

2. **Monitor low-activity wallets first**:
   - Choose wallets that trade infrequently
   - Monitor for a few hours to verify detection works

3. **Check HTTP endpoints**:
   ```bash
   watch -n 5 'curl -s http://localhost:8080/status'
   ```

4. **Review logs**:
   ```bash
   RUST_LOG=info cargo run --release -- --config config.local.toml | tee bot.log
   ```

5. **Gradually increase buy amount** after confidence is established

### Testnet Testing

For safe testing without real funds:
1. Switch `solana_rpc_url` to testnet
2. Use testnet private key with testnet SOL
3. Monitor testnet wallets
4. Note: OKX may not support testnet, may need to mock API

## Architecture

### Components

- **CopyTraderParser**: Implements `Parser` trait to detect sells from balance changes
- **CopyTraderHandler**: Implements `Handler` trait to execute OKX buy orders
- **OkxClient**: HTTP client for OKX DEX aggregator API
- **SellTracker**: Shared state tracking all detected sells
- **DedupSet**: Global set of bought token mints
- **HTTP Server**: Warp-based server for monitoring endpoints

### Data Flow

```
Yellowstone gRPC → Parser (detect sells) → Handler (execute buys)
                                                ↓
                                          Async tasks
                                                ↓
                         OKX Quote → OKX Swap → Sign → Submit → Update tracker
```

## Error Handling

The bot handles errors gracefully:

- **OKX API failures**: Logged with context, continue monitoring
- **Transaction submission failures**: Logged and marked as failed
- **Balance parsing errors**: Transaction skipped (filtered)
- **Deduplication**: Skipped with reason logged

**No retry logic** - fails fast and continues to next event.

## Monitoring

### Log Levels

```bash
RUST_LOG=error        # Only errors
RUST_LOG=warn         # Warnings and errors
RUST_LOG=info         # Important events (recommended)
RUST_LOG=debug        # Detailed debugging
RUST_LOG=trace        # Very verbose
```

### Key Log Messages

```
[RECV] slot=123456 sig=5j7s...          # Transaction received
Sell of X token Y for Z SOL detected!   # Sell detected
[DEDUP] Token already bought            # Deduplication skip
[OKX] Getting quote for buying...       # OKX quote request
[RPC] Submitting transaction to Solana  # Transaction submission
[SUCCESS] Transaction submitted: 3kd9... # Buy order successful
```

## Troubleshooting

### No sells detected
- Verify whitelisted wallets are active
- Check logs for "[RECV]" messages
- Ensure Yellowstone gRPC connection is working
- Test with known active wallets (see config.toml examples)

### OKX API errors
- Check internet connectivity
- Verify OKX API endpoint is accessible
- Review OKX response in error logs
- Ensure token has liquidity on Solana DEX

### Transaction submission failures
- Check SOL balance in signing wallet
- Verify Solana RPC endpoint is working
- Review transaction error in logs
- Try different RPC provider if persistent issues

### Bot buying wrong tokens
- Review "interesting currencies" configuration
- Check that quote token mints are correct
- Verify balance detection logic in logs

## Safety Considerations

1. **Private Key Security**
   - Never commit private keys to version control
   - Use separate wallet for bot (not your main wallet)
   - Keep only necessary SOL in bot wallet

2. **Amount Limits**
   - Start with very small `buy_amount_sol`
   - Set maximum limit in config
   - Monitor spending via `/status` endpoint

3. **RPC Reliability**
   - Use trusted Solana RPC provider
   - Consider rate limits and quotas
   - Have backup RPC endpoints ready

4. **OKX API**
   - Always validate responses before signing
   - Check transaction data before submission
   - Monitor for API changes/deprecations

5. **Monitoring**
   - Regularly check `/status` and `/trades` endpoints
   - Review logs for errors and anomalies
   - Set up alerts for failures if running 24/7

## Performance

- **Memory**: Minimal (tracks detected sells in memory)
- **CPU**: Low (async event-driven)
- **Network**: Dependent on:
  - Yellowstone gRPC stream bandwidth
  - OKX API latency
  - Solana RPC latency

## Limitations

1. **No retry logic**: Failed buys are not retried
2. **No confirmation wait**: Marks as submitted, doesn't wait for confirmation
3. **Global dedup only**: Can't buy same token again even if price changes
4. **No price limits**: Buys at market price via OKX routing
5. **No stop-loss**: No automatic selling mechanism

## Future Enhancements

Potential improvements:
- Add retry logic with exponential backoff
- Wait for transaction confirmation before marking success
- Implement per-wallet deduplication strategy
- Add price limit checks before executing buy
- Implement stop-loss/take-profit selling
- Add database persistence for tracker
- Support multiple DEX aggregators (Jupiter, 1inch)
- Add Telegram/Discord notifications

## License

MIT

## Disclaimer

**This software is provided for educational purposes only.**

Trading cryptocurrencies involves substantial risk of loss. This bot executes trades automatically based on configured logic. Always:
- Test thoroughly with small amounts first
- Never invest more than you can afford to lose
- Understand the risks of copy trading
- Monitor the bot continuously when running
- Use at your own risk

The authors are not responsible for any financial losses incurred through use of this software.
