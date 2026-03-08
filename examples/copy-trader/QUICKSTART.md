# Quick Start Guide: Copy Trading Bot

Get the copy trading bot up and running in 5 minutes.

## Prerequisites

- Rust toolchain (1.70+)
- Solana CLI (for generating keypairs)
- Access to Yellowstone gRPC endpoint
- SOL balance in your trading wallet

## Step 1: Generate a Trading Keypair

Create a new keypair for the bot (don't use your main wallet):

```bash
solana-keygen new --outfile ~/copy-trader-keypair.json
```

Get the base58 private key:

```bash
cat ~/copy-trader-keypair.json
# Copy the array of numbers, e.g., [123,45,67,...]
```

Convert to base58 (Python one-liner):

```bash
python3 -c "import json, base58; print(base58.b58encode(bytes(json.load(open('~/copy-trader-keypair.json')))).decode())"
```

Fund the wallet with SOL:

```bash
solana-keygen pubkey ~/copy-trader-keypair.json
# Send SOL to this address
```

## Step 2: Configure the Bot

Create your local config:

```bash
cd examples/copy-trader
cp config.toml config.local.toml
vim config.local.toml
```

Edit these critical fields:

```toml
[copy_trading]
# Add wallets to monitor (example active wallets provided)
whitelist = [
    "ARu4n5mFdZogZAravu7CcizaojWnS6oqka37gdLT5SZn",
]

# YOUR base58 private key from step 1
private_key = "PASTE_YOUR_BASE58_KEY_HERE"

# Start small!
buy_amount_sol = 0.001

# Your Solana RPC (use reliable provider)
solana_rpc_url = "https://api.mainnet-beta.solana.com"
```

**IMPORTANT**: Never commit `config.local.toml` to git!

## Step 3: Build and Run

Build the bot:

```bash
cargo build --release -p yellowstone-vixen-example-copy-trader
```

Run with logging:

```bash
RUST_LOG=info cargo run --release -p yellowstone-vixen-example-copy-trader -- --config config.local.toml
```

You should see:

```
HTTP server listening on 0.0.0.0:8080
Endpoints:
  GET /status - Get copy trading statistics
  GET /trades - Get all detected sells and copy trades
  GET /health - Health check
```

## Step 4: Monitor Activity

Open another terminal and watch the status:

```bash
# Check health
curl http://localhost:8080/health

# Watch statistics
watch -n 5 'curl -s http://localhost:8080/status | jq'

# View all trades
curl -s http://localhost:8080/trades | jq
```

## Step 5: Verify It's Working

When a whitelisted wallet sells a token, you'll see:

```
[INFO] Sell of 1234.56 token ABC...XYZ for 0.05 SOL detected from wallet ARu4n5m...!
[INFO] [OKX] Getting quote for buying ABC...XYZ with 0.001 SOL
[INFO] [OKX] Quote successful
[INFO] [OKX] Executing swap
[INFO] [RPC] Submitting transaction to Solana
[INFO] [SUCCESS] Transaction submitted: 5kD9p2w...
```

Check the status endpoint:

```bash
curl -s http://localhost:8080/status | jq
```

Expected output:

```json
{
  "total_sells_detected": 1,
  "pending": 0,
  "tx_submitted": 1,
  "confirmed": 0,
  "failed": 0,
  "skipped": 0
}
```

## Common Issues

### No sells detected

**Problem**: Bot is running but no sells are being detected.

**Solutions**:
- Verify whitelisted wallets are active traders
- Check Yellowstone gRPC connection: look for `[RECV]` log messages
- Try the example wallets in the default config (very active)

### OKX API errors

**Problem**: `OKX quote failed` or `OKX swap failed` errors.

**Solutions**:
- Check internet connectivity
- Verify token has liquidity (OKX may not route all tokens)
- Check OKX API status: https://www.okx.com/status
- Try a different token/wallet

### Transaction submission failures

**Problem**: `RPC error` or `Failed to send transaction`.

**Solutions**:
- Check SOL balance: `solana balance ~/copy-trader-keypair.json`
- Verify RPC endpoint is working
- Try a different RPC provider (Helius, QuickNode, etc.)
- Check Solana network status

### "Token already bought" messages

**Problem**: All buys are being skipped with this reason.

**Solutions**:
- This is normal! It means the bot already bought those tokens
- The bot uses global deduplication (buys each token only once)
- Restart the bot to clear dedup set (in-memory only)

## Safety Checklist

Before running on mainnet with real funds:

- [ ] Generated a dedicated keypair (not your main wallet)
- [ ] Set `buy_amount_sol` to a small test amount (0.001)
- [ ] Never committed `config.local.toml` to git
- [ ] Funded wallet with only enough SOL for testing
- [ ] Tested with low-activity wallets first
- [ ] Verified HTTP endpoints are working
- [ ] Understand the bot buys at market price with no limits

## Next Steps

Once comfortable:

1. Gradually increase `buy_amount_sol`
2. Add more wallets to `whitelist`
3. Adjust `slippage_percent` if needed
4. Set up monitoring/alerts
5. Review README.md for detailed documentation

## Stop the Bot

Press `Ctrl+C` to gracefully shut down.

The bot state (detected sells, dedup set) is in-memory only and will be lost on restart.

## Support

For issues or questions:
- Read the full [README.md](README.md)
- Check [IMPLEMENTATION.md](IMPLEMENTATION.md) for technical details
- Review logs with `RUST_LOG=debug` for more detail

## Disclaimer

This bot executes real trades with real money. Always:
- Start with tiny amounts
- Monitor continuously
- Never invest more than you can afford to lose
- Understand the risks of automated copy trading

Use at your own risk!
