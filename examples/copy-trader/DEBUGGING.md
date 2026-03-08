# Debugging Guide for Copy Trader

## Quick Fixes

### 1. Fix the Private Key Error

The panic `InvalidChar(79)` means your private key is invalid. Here's how to fix it:

**Option A: Generate a new test keypair**

```bash
# Generate a new keypair
solana-keygen new --no-bip39-passphrase --outfile ~/copy-trader-test.json

# Display your public key (for funding)
solana-keygen pubkey ~/copy-trader-test.json

# Convert the keypair to base58 format
python3 << 'EOF'
import json
import base58

with open('/root/copy-trader-test.json', 'r') as f:
    keypair_bytes = json.load(f)
    base58_key = base58.b58encode(bytes(keypair_bytes)).decode()
    print(f"Your base58 private key:\n{base58_key}")
EOF
```

**Option B: Convert existing Solana keypair to base58**

If you have an existing keypair JSON file:

```bash
python3 << 'EOF'
import json
import base58
import sys

keypair_path = input("Enter path to your keypair.json: ")
with open(keypair_path, 'r') as f:
    keypair_bytes = json.load(f)
    base58_key = base58.b58encode(bytes(keypair_bytes)).decode()
    print(f"\nYour base58 private key:\n{base58_key}")
    print("\nCopy this into your config.toml under 'private_key'")
EOF
```

Then update `examples/copy-trader/config.toml`:

```toml
private_key = "YOUR_BASE58_KEY_FROM_ABOVE"
```

### 2. Understanding Why No Sells Are Detected

Run with detailed debug logging:

```bash
RUST_LOG=yellowstone_vixen_example_copy_trader=debug,info cargo run --release --bin yellowstone-vixen-example-copy-trader -- --config examples/copy-trader/config.toml
```

## What to Look For in Logs

### Good Signs (Bot is Working)

```
[INFO] Starting copy-trader bot
HTTP server listening on 0.0.0.0:8180
[DEBUG] [RECV] slot=123456789 sig=5j7s...
[DEBUG] [5j7s...] Found whitelisted wallet ARu4n5mF... at index 2
[DEBUG] [5j7s...] Analyzing 1 whitelisted wallet(s)
[DEBUG] [5j7s...] Wallet ARu4n5mF...: 3 token balance changes, SOL delta: 0.05
[DEBUG] [5j7s...] Wallet ARu4n5mF...: Token ABC12345 DECREASED by 1234.5 (non-quote = SOLD)
[DEBUG] [5j7s...] Wallet ARu4n5mF...: Native SOL INCREASED by 0.05 (payment received)
[DEBUG] [5j7s...] Wallet ARu4n5mF...: Found 1 sold tokens, 1 quote receipts
[INFO] Sell of 1234.5 token ABC... for 0.05 SOL detected from wallet ARu4n5mF...!
```

### Problem Signs

#### No Transactions at All
```
HTTP server listening on 0.0.0.0:8180
(nothing else)
```

**Solution**: Check your Yellowstone gRPC connection in config.toml:
```toml
[source]
endpoint = "http://62.197.45.101:11000"  # Verify this is correct
x_token = ""  # Add token if required
```

#### Transactions but No Whitelisted Wallets
```
[DEBUG] [RECV] slot=123 sig=abc...
[DEBUG] [abc...] Analyzing transaction with 5 pre_token_balances, 5 post_token_balances
[DEBUG] [abc...] No whitelisted wallets found in transaction
```

**Solution**: Your whitelisted wallets aren't in these transactions. Either:
1. Wait longer (they may not trade frequently)
2. Add more active wallets to your whitelist
3. Verify wallet addresses are correct (base58 format)

Check your whitelist:
```bash
grep -A 5 "whitelist" examples/copy-trader/config.toml
```

#### Whitelisted Wallet Found but No Sell Pattern
```
[DEBUG] [abc...] Found whitelisted wallet ARu4n5mF... at index 2
[DEBUG] [abc...] Analyzing 1 whitelisted wallet(s)
[DEBUG] [abc...] Wallet ARu4n5mF...: 0 token balance changes, SOL delta: -0.001
[DEBUG] [abc...] Wallet ARu4n5mF...: Found 0 sold tokens, 0 quote receipts
```

**Reason**: The transaction doesn't involve token sales (might be a buy, transfer, or fee payment).

#### Token Sold but No Quote Received
```
[DEBUG] [abc...] Wallet ARu4n5mF...: Token ABC12345 DECREASED by 100 (non-quote = SOLD)
[DEBUG] [abc...] Wallet ARu4n5mF...: Found 1 sold tokens, 0 quote receipts
[DEBUG] [abc...] Wallet ARu4n5mF...: Tokens sold but NO quote tokens received - not a sell (might be burn/transfer)
```

**Reason**: Token balance decreased but no SOL/USDC/USDT increased. This is likely:
- A token burn
- A transfer to another wallet
- A failed swap
- Not a sell transaction

## Testing with Known Active Wallets

The default config includes two very active wallets:

```toml
whitelist = [
    "ARu4n5mFdZogZAravu7CcizaojWnS6oqka37gdLT5SZn",
    "5t478CAxfUsDZDvCZAbuap6cFtv9qe6mb373YixLnoQx"
]
```

These wallets should generate activity. If you see NO transactions with these wallets, check:

1. **Yellowstone gRPC connection**: Is it connected to mainnet-beta?
2. **Network connectivity**: Can you reach the endpoint?
3. **Prefilter issue**: Check if prefilter is too restrictive

## Manual Testing

### Check if Wallets Are Active

Use Solscan or Solana Explorer:

```
https://solscan.io/account/ARu4n5mFdZogZAravu7CcizaojWnS6oqka37gdLT5SZn
https://solscan.io/account/5t478CAxfUsDZDvCZAbuap6cFtv9qe6mb373YixLnoQx
```

Look for recent transactions. If they're trading, your bot should see them.

### Test HTTP Endpoints

While the bot is running:

```bash
# Health check
curl http://localhost:8180/health

# Status (should show stats once sells are detected)
curl http://localhost:8180/status | jq

# All trades
curl http://localhost:8180/trades | jq
```

### Verify Prefilter

The bot only receives transactions involving whitelisted wallets. If your wallet isn't active, you won't see any transactions.

Try adding a wallet you control and make a test transaction:

```bash
# Get your wallet address
solana-keygen pubkey ~/copy-trader-test.json

# Add to config.toml whitelist
# Make a small token swap using your wallet
# You should see it in the logs
```

## Common Misunderstandings

### "I see transactions but no sells"

**This is normal!** Not every transaction is a sell. The bot only detects:
- Token balance **DECREASED** (sold)
- SOL/USDC/USDT balance **INCREASED** (payment received)
- Both conditions in the **same transaction**

Most transactions are:
- Buys (opposite pattern)
- Transfers (no quote token change)
- Failed swaps (no balance changes)
- Fee payments (SOL decreased)

### "The bot isn't buying"

First, make sure a sell was actually detected:
```
[INFO] Sell of X token Y for Z SOL detected!
```

If you see this, then check:
1. OKX API is accessible
2. Private key is valid
3. Wallet has SOL balance
4. Token isn't already in dedup set

Check logs for:
```
[OKX] Getting quote...
[OKX] Executing swap...
[RPC] Submitting transaction...
[SUCCESS] Transaction submitted: ...
```

Or errors:
```
[OKX] Failed to get OKX quote: ...
[RPC] Failed to send transaction: ...
```

### "Deduplication is preventing all buys"

The bot tracks bought tokens in-memory. If you restart the bot, the dedup set is cleared.

To see if tokens are being skipped:
```
[DEDUP] Token ABC123... already bought
```

This is by design - the bot only buys each token once.

## Enabling Extra Verbose Logging

For maximum detail:

```bash
RUST_LOG=trace cargo run --release --bin yellowstone-vixen-example-copy-trader -- --config examples/copy-trader/config.toml
```

This will show every transaction, every balance change, and every decision the bot makes.

## Getting Help

If you're still stuck:

1. **Capture logs**:
   ```bash
   RUST_LOG=debug cargo run --release --bin yellowstone-vixen-example-copy-trader -- --config examples/copy-trader/config.toml 2>&1 | tee debug.log
   ```

2. **Check for the specific patterns above** in the logs

3. **Verify your configuration**:
   - Correct private key (base58 format, 88 chars long typically)
   - Valid whitelist addresses
   - Working Yellowstone gRPC endpoint
   - Correct quote token mints (USDC, USDT, WSOL)

4. **Test with your own wallet** to ensure sell detection works

5. **Be patient** - some wallets trade infrequently. Leave it running for a few hours.
