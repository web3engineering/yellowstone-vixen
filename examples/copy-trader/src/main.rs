use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use clap::Parser as ClapParser;
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;
use tokio::sync::RwLock;
use tracing::{debug, error, info};
use tracing_subscriber;
use warp::Filter;
use yellowstone_vixen::Pipeline;
use yellowstone_vixen_core::{
    instruction::InstructionUpdate, ParseError, ParseResult, Parser, Prefilter,
    TransactionPrefilter, TransactionUpdate,
};
use yellowstone_vixen_okx::OkxClient;
use yellowstone_vixen_yellowstone_grpc_source::YellowstoneGrpcSource;

#[derive(clap::Parser)]
#[command(version, author, about)]
pub struct Opts {
    #[arg(long, short)]
    config: PathBuf,
}

/// Configuration for interesting currencies (quote tokens)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterestingCurrency {
    pub name: String,
    pub mint: String,
    pub priority: u32,
}

/// Copy trading specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyTradingConfig {
    pub whitelist: Vec<String>,
    pub private_key: String,
    pub buy_amount_sol: f64,
    pub http_addr: String,
    pub okx_api_key: String,
    pub okx_secret_key: String,
    pub okx_passphrase: String,
    #[serde(default)]
    pub okx_base_url: Option<String>,
    pub slippage_percent: String,
    pub solana_rpc_url: String,
    pub interesting_currencies: Vec<InterestingCurrency>,
    /// SOL to leave unwrapped in wallet (for fees). Excess will be wrapped to WSOL on startup.
    #[serde(default = "default_sol_leave")]
    pub sol_leave_on_wallet: f64,
    /// Optional: Maximum number of trades to execute before exiting (for debugging)
    #[serde(default)]
    pub max_trades: Option<usize>,
}

fn default_sol_leave() -> f64 {
    0.1 // Default to keeping 0.1 SOL for fees
}

/// Status of a copy trade execution
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum CopyTradeStatus {
    Pending,
    Skipped { reason: String },
    TxSubmitted { solana_signature: String },
    Confirmed { solana_signature: String },
    Failed { error: String },
}

/// A detected sell event from a whitelisted wallet
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedSell {
    pub timestamp: i64,
    pub slot: u64,
    pub signature: String,
    pub wallet: String,
    pub token_mint: String,
    pub token_amount: f64,
    pub quote_mint: String,
    pub quote_amount: f64,
    pub copy_trade_status: CopyTradeStatus,
}

/// Tracker for detected sells
pub type SellTracker = Arc<RwLock<Vec<DetectedSell>>>;

/// Global deduplication set for bought tokens
pub type DedupSet = Arc<RwLock<HashSet<String>>>;

/// Parse result containing detected sell events
#[derive(Debug)]
pub struct SellDetectionResult {
    pub sells: Vec<DetectedSell>,
}

/// Parser that detects token sells from whitelisted wallets
#[derive(Clone, Debug)]
pub struct CopyTraderParser {
    whitelist: HashSet<Vec<u8>>,
    quote_tokens: HashMap<String, InterestingCurrency>,
    wsol_mint: String,
}

impl CopyTraderParser {
    pub fn new(config: &CopyTradingConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let mut whitelist = HashSet::new();
        for addr in &config.whitelist {
            let decoded = bs58::decode(addr).into_vec()?;
            whitelist.insert(decoded);
        }

        let mut quote_tokens = HashMap::new();
        let mut wsol_mint = String::new();

        for currency in &config.interesting_currencies {
            quote_tokens.insert(currency.mint.clone(), currency.clone());
            if currency.name == "WSOL" {
                wsol_mint = currency.mint.clone();
            }
        }

        Ok(Self {
            whitelist,
            quote_tokens,
            wsol_mint,
        })
    }

    /// Build full account list from transaction
    fn build_account_list(&self, shared: &Arc<yellowstone_vixen_core::instruction::InstructionShared>) -> Vec<Vec<u8>> {
        let mut accounts = Vec::new();

        // Add static keys
        accounts.extend(shared.accounts.static_keys.clone());

        // Add dynamic writable keys
        accounts.extend(shared.accounts.dynamic_rw.clone());

        // Add dynamic readonly keys
        accounts.extend(shared.accounts.dynamic_ro.clone());

        accounts
    }

    /// Detect sells from balance changes
    fn detect_sells_from_balances(
        &self,
        shared: &Arc<yellowstone_vixen_core::instruction::InstructionShared>,
        txn_sig: &str,
    ) -> Vec<DetectedSell> {
        let mut sells = Vec::new();
        let accounts = self.build_account_list(shared);

        // Find whitelisted wallets in this transaction
        let mut whitelisted_indices = HashSet::new();
        for (idx, account) in accounts.iter().enumerate() {
            if self.whitelist.contains(account) {
                let wallet_str = bs58::encode(account).into_string();
                debug!("[{}] Found whitelisted wallet {} at index {}", txn_sig, wallet_str, idx);
                whitelisted_indices.insert(idx as u8);
            }
        }

        if whitelisted_indices.is_empty() {
            debug!("[{}] No whitelisted wallets found in transaction", txn_sig);
            return sells;
        }

        debug!("[{}] Analyzing {} whitelisted wallet(s)", txn_sig, whitelisted_indices.len());

        // Build balance maps for each whitelisted wallet
        for &wallet_idx in &whitelisted_indices {
            let wallet_pubkey = &accounts[wallet_idx as usize];
            let wallet_str = bs58::encode(wallet_pubkey).into_string();

            // Track token balance changes for this wallet
            let mut token_deltas: HashMap<String, (f64, u8)> = HashMap::new(); // mint -> (delta, decimals)

            // Process token balances - check OWNER not account_index
            // Token accounts are owned by the wallet, so we need to check the owner field
            for pre_bal in &shared.pre_token_balances {
                // Check if this token account is owned by our whitelisted wallet
                if pre_bal.owner == wallet_str {
                    let mint = pre_bal.mint.clone();
                    let pre_amount = pre_bal.ui_token_amount.as_ref()
                        .map(|a| a.ui_amount)
                        .unwrap_or(0.0);
                    let decimals = pre_bal.ui_token_amount.as_ref()
                        .map(|a| a.decimals as u8)
                        .unwrap_or(9);

                    token_deltas.entry(mint).or_insert((0.0, decimals)).0 -= pre_amount;
                }
            }

            for post_bal in &shared.post_token_balances {
                // Check if this token account is owned by our whitelisted wallet
                if post_bal.owner == wallet_str {
                    let mint = post_bal.mint.clone();
                    let post_amount = post_bal.ui_token_amount.as_ref()
                        .map(|a| a.ui_amount)
                        .unwrap_or(0.0);
                    let decimals = post_bal.ui_token_amount.as_ref()
                        .map(|a| a.decimals as u8)
                        .unwrap_or(9);

                    token_deltas.entry(mint).or_insert((0.0, decimals)).0 += post_amount;
                }
            }

            // Check for native SOL balance change
            let native_sol_delta = if (wallet_idx as usize) < shared.pre_balances.len()
                && (wallet_idx as usize) < shared.post_balances.len() {
                let pre_sol = shared.pre_balances[wallet_idx as usize];
                let post_sol = shared.post_balances[wallet_idx as usize];
                (post_sol as f64 - pre_sol as f64) / 1_000_000_000.0
            } else {
                0.0
            };

            // Find decreased non-quote tokens (sold)
            let mut sold_tokens = Vec::new();
            let mut received_quotes = Vec::new();

            // Log all balance changes for this whitelisted wallet
            info!("[{}] Wallet {}: Analyzing balance changes", txn_sig, wallet_str);
            info!("[{}]   Native SOL delta: {}", txn_sig, native_sol_delta);

            if token_deltas.is_empty() {
                info!("[{}]   No token balance changes", txn_sig);
            } else {
                info!("[{}]   Token balance changes:", txn_sig);
                for (mint, (delta, _decimals)) in &token_deltas {
                    let token_name = if let Some(currency) = self.quote_tokens.get(mint) {
                        format!("{} (quote)", currency.name)
                    } else {
                        format!("{}..{}", &mint[..8], &mint[mint.len()-8..])
                    };
                    info!("[{}]     {} delta: {}", txn_sig, token_name, delta);
                }
            }

            debug!("[{}] Wallet {}: {} token balance changes, SOL delta: {}",
                txn_sig, wallet_str, token_deltas.len(), native_sol_delta);

            for (mint, (delta, decimals)) in &token_deltas {
                if *delta < -0.000001 { // Decreased = sold
                    if !self.quote_tokens.contains_key(mint) {
                        // Non-quote token sold
                        debug!("[{}] Wallet {}: Token {} DECREASED by {} (non-quote = SOLD)",
                            txn_sig, wallet_str, &mint[..8], -delta);
                        sold_tokens.push((mint.clone(), -delta, *decimals));
                    } else {
                        debug!("[{}] Wallet {}: Quote token {} decreased by {} (ignored)",
                            txn_sig, wallet_str, self.quote_tokens.get(mint).unwrap().name, -delta);
                    }
                } else if *delta > 0.000001 { // Increased = received
                    if self.quote_tokens.contains_key(mint) {
                        // Quote token received
                        debug!("[{}] Wallet {}: Quote token {} INCREASED by {} (payment received)",
                            txn_sig, wallet_str, self.quote_tokens.get(mint).unwrap().name, *delta);
                        received_quotes.push((mint.clone(), *delta));
                    } else {
                        debug!("[{}] Wallet {}: Token {} increased by {} (non-quote, ignored)",
                            txn_sig, wallet_str, &mint[..8], *delta);
                    }
                }
            }

            // Also check native SOL as quote
            if native_sol_delta > 0.000001 {
                debug!("[{}] Wallet {}: Native SOL INCREASED by {} (payment received)",
                    txn_sig, wallet_str, native_sol_delta);
                received_quotes.push((self.wsol_mint.clone(), native_sol_delta));
            } else if native_sol_delta < -0.000001 {
                debug!("[{}] Wallet {}: Native SOL decreased by {} (spent/fee)",
                    txn_sig, wallet_str, -native_sol_delta);
            }

            // Match sells with quote receipts
            debug!("[{}] Wallet {}: Found {} sold tokens, {} quote receipts",
                txn_sig, wallet_str, sold_tokens.len(), received_quotes.len());

            if !sold_tokens.is_empty() && received_quotes.is_empty() {
                info!("[{}] Wallet {}: Tokens DECREASED but NO quote tokens received - not a sell (might be burn/transfer/stake)",
                    txn_sig, wallet_str);
            } else if sold_tokens.is_empty() && !received_quotes.is_empty() {
                info!("[{}] Wallet {}: Quote tokens RECEIVED but no tokens sold - likely a buy or receipt",
                    txn_sig, wallet_str);
            } else if sold_tokens.is_empty() && received_quotes.is_empty() {
                info!("[{}] Wallet {}: No token sells, no quote receipts - not a trading transaction",
                    txn_sig, wallet_str);
            }

            for (token_mint, token_amount, _decimals) in sold_tokens {
                for (quote_mint, quote_amount) in &received_quotes {
                    // Found a sell!
                    info!("Sell of {} token {} for {} {} detected from wallet {}!",
                        token_amount, token_mint, quote_amount,
                        self.quote_tokens.get(quote_mint).map(|c| c.name.as_str()).unwrap_or("SOL"),
                        wallet_str);

                    sells.push(DetectedSell {
                        timestamp: chrono::Utc::now().timestamp(),
                        slot: shared.slot,
                        signature: txn_sig.to_string(),
                        wallet: wallet_str.clone(),
                        token_mint: token_mint.clone(),
                        token_amount,
                        quote_mint: quote_mint.clone(),
                        quote_amount: *quote_amount,
                        copy_trade_status: CopyTradeStatus::Pending,
                    });
                }
            }
        }

        sells
    }
}

impl Parser for CopyTraderParser {
    type Input = TransactionUpdate;
    type Output = SellDetectionResult;

    fn id(&self) -> std::borrow::Cow<'static, str> {
        "CopyTraderParser".into()
    }

    fn prefilter(&self) -> Prefilter {
        let mut accounts_include = HashSet::new();

        // Add whitelisted wallets to filter
        for wallet in &self.whitelist {
            if let Ok(pubkey) = yellowstone_vixen_core::Pubkey::try_from(wallet.as_slice()) {
                accounts_include.insert(pubkey);
            }
        }

        Prefilter {
            account: None,
            transaction: Some(TransactionPrefilter {
                accounts_include,
                accounts_required: HashSet::new(),
                failed: Some(false), // Only successful transactions
            }),
            block_meta: None,
            block: None,
            slot: None,
        }
    }

    async fn parse(&self, txn: &TransactionUpdate) -> ParseResult<SellDetectionResult> {
        let txn_sig = txn.transaction.as_ref()
            .map(|t| bs58::encode(&t.signature).into_string())
            .unwrap_or_else(|| "unknown".to_string());

        debug!("[RECV] slot={} sig={}", txn.slot, txn_sig);

        let ixs = InstructionUpdate::parse_from_txn(txn)
            .map_err(|e| {
                debug!("[{}] Failed to parse instructions: {}", txn_sig, e);
                ParseError::Other(Box::new(e))
            })?;

        let shared = ixs
            .first()
            .map(|ix| Arc::clone(&ix.shared))
            .ok_or_else(|| {
                debug!("[{}] No instructions found", txn_sig);
                ParseError::Filtered
            })?;

        // Skip failed transactions
        if shared.err.is_some() {
            debug!("[{}] Transaction failed, skipping", txn_sig);
            return Err(ParseError::Filtered);
        }

        debug!("[{}] Analyzing transaction with {} pre_token_balances, {} post_token_balances",
            txn_sig, shared.pre_token_balances.len(), shared.post_token_balances.len());

        // Detect sells from balance changes
        let sells = self.detect_sells_from_balances(&shared, &txn_sig);

        if sells.is_empty() {
            debug!("[{}] No sells detected in this transaction", txn_sig);
            return Err(ParseError::Filtered);
        }

        info!("[{}] Detected {} sell event(s)", txn_sig, sells.len());
        Ok(SellDetectionResult { sells })
    }
}

/// Handler that executes copy trades via OKX
#[derive(Debug)]
pub struct CopyTraderHandler {
    tracker: SellTracker,
    dedup_set: DedupSet,
    okx_client: OkxClient,
    keypair: Arc<Keypair>,
    buy_amount_lamports: u64,
    solana_rpc_url: String,
    wsol_mint: String,
    trade_counter: Arc<AtomicUsize>,
    max_trades: Option<usize>,
}

impl CopyTraderHandler {
    pub fn new(
        tracker: SellTracker,
        dedup_set: DedupSet,
        config: &CopyTradingConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let okx_client = OkxClient::new(
            config.okx_api_key.clone(),
            config.okx_secret_key.clone(),
            config.okx_passphrase.clone(),
            config.okx_base_url.clone(),
        );

        // Parse base58 private key - use from_base58_string which handles both formats
        let keypair = Keypair::from_base58_string(&config.private_key);

        let buy_amount_lamports = (config.buy_amount_sol * 1_000_000_000.0) as u64;

        let wsol_mint = config.interesting_currencies.iter()
            .find(|c| c.name == "WSOL")
            .map(|c| c.mint.clone())
            .unwrap_or_else(|| "So11111111111111111111111111111111111111112".to_string());

        if let Some(max) = config.max_trades {
            info!("[CONFIG] Max trades limit: {} (bot will exit after reaching this limit)", max);
        }

        Ok(Self {
            tracker,
            dedup_set,
            okx_client,
            keypair: Arc::new(keypair),
            buy_amount_lamports,
            solana_rpc_url: config.solana_rpc_url.clone(),
            wsol_mint,
            trade_counter: Arc::new(AtomicUsize::new(0)),
            max_trades: config.max_trades,
        })
    }

    async fn execute_buy_order(&self, sell: &DetectedSell) -> CopyTradeStatus {
        // Check if max trades limit reached
        if let Some(max) = self.max_trades {
            let current_count = self.trade_counter.load(Ordering::Relaxed);
            if current_count >= max {
                let reason = format!("Max trades limit ({}) reached", max);
                info!("[LIMIT] {}", reason);
                return CopyTradeStatus::Skipped { reason };
            }
        }

        // Check deduplication
        {
            let dedup = self.dedup_set.read().await;
            if dedup.contains(&sell.token_mint) {
                let reason = format!("Token {} already bought", sell.token_mint);
                info!("[DEDUP] {}", reason);
                return CopyTradeStatus::Skipped { reason };
            }
        }

        let amount_str = self.buy_amount_lamports.to_string();
        let user_wallet = self.keypair.pubkey().to_string();

        // Get swap transaction from OKX
        info!("[OKX] Getting swap transaction for buying {} with {} SOL", sell.token_mint, self.buy_amount_lamports as f64 / 1_000_000_000.0);
        let mut transaction = match self.okx_client.get_unsigned_transaction(
            &self.wsol_mint,
            &sell.token_mint,
            &amount_str,
            &user_wallet,
            &"1.0", // slippage percent
        ).await {
            Ok(tx) => tx,
            Err(e) => {
                let error = format!("Failed to get OKX swap transaction: {}", e);
                error!("[OKX] {}", error);
                return CopyTradeStatus::Failed { error };
            }
        };

        info!("[OKX] Transaction built successfully");

        // Connect to RPC and get recent blockhash
        let rpc_client = RpcClient::new_with_commitment(
            self.solana_rpc_url.clone(),
            CommitmentConfig::confirmed(),
        );

        let recent_blockhash = match rpc_client.get_latest_blockhash() {
            Ok(hash) => hash,
            Err(e) => {
                let error = format!("Failed to get recent blockhash: {}", e);
                error!("[RPC] {}", error);
                return CopyTradeStatus::Failed { error };
            }
        };

        // Update blockhash in transaction
        transaction.message.set_recent_blockhash(recent_blockhash);

        // Sign transaction
        let serialized_message = transaction.message.serialize();
        let signature = self.keypair.sign_message(&serialized_message);
        transaction.signatures[0] = signature;

        info!("[TX] Transaction signed");

        // Submit to Solana RPC
        info!("[RPC] Submitting transaction to Solana");
        let tx_signature = match rpc_client.send_and_confirm_transaction(&transaction) {
            Ok(sig) => sig.to_string(),
            Err(e) => {
                let error = format!("Failed to send transaction: {}", e);
                error!("[RPC] {}", error);
                return CopyTradeStatus::Failed { error };
            }
        };

        // Increment trade counter
        let trade_count = self.trade_counter.fetch_add(1, Ordering::Relaxed) + 1;

        // Print transaction signature prominently
        info!("====================================================================");
        info!("[SUCCESS] Trade #{} executed!", trade_count);
        info!("[TX SIGNATURE] {}", tx_signature);
        info!("[SOLSCAN] https://solscan.io/tx/{}", tx_signature);
        info!("====================================================================");

        // Check if we've reached the max trades limit
        if let Some(max) = self.max_trades {
            if trade_count >= max {
                info!("");
                info!("╔═══════════════════════════════════════════════════════════════╗");
                info!("║  MAX TRADES LIMIT REACHED ({}/{})                            ║", trade_count, max);
                info!("║  Exiting in 3 seconds...                                      ║");
                info!("╚═══════════════════════════════════════════════════════════════╝");
                info!("");

                // Wait a bit for logs to flush
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                std::process::exit(0);
            }
        }

        // Add to dedup set
        {
            let mut dedup = self.dedup_set.write().await;
            dedup.insert(sell.token_mint.clone());
        }

        CopyTradeStatus::TxSubmitted { solana_signature: tx_signature }
    }
}

impl yellowstone_vixen::Handler<SellDetectionResult, TransactionUpdate> for CopyTraderHandler {
    async fn handle(
        &self,
        value: &SellDetectionResult,
        _raw: &TransactionUpdate,
    ) -> yellowstone_vixen::HandlerResult<()> {
        info!("[HANDLER] Processing {} sell event(s)", value.sells.len());

        // Store sells in tracker with pending status
        {
            let mut tracker = self.tracker.write().await;
            for sell in &value.sells {
                tracker.push(sell.clone());
            }
        }

        // Execute buy orders asynchronously
        for sell in &value.sells {
            let sell_clone = sell.clone();
            let tracker = self.tracker.clone();
            let dedup = self.dedup_set.clone();
            let okx = self.okx_client.clone();
            let keypair = self.keypair.clone();
            let buy_amount = self.buy_amount_lamports;
            let rpc_url = self.solana_rpc_url.clone();
            let wsol = self.wsol_mint.clone();
            let counter = self.trade_counter.clone();
            let max = self.max_trades;

            tokio::spawn(async move {
                let handler = CopyTraderHandler {
                    tracker: tracker.clone(),
                    dedup_set: dedup,
                    okx_client: okx,
                    keypair,
                    buy_amount_lamports: buy_amount,
                    solana_rpc_url: rpc_url,
                    wsol_mint: wsol,
                    trade_counter: counter,
                    max_trades: max,
                };

                let status = handler.execute_buy_order(&sell_clone).await;

                // Update tracker with result
                let mut tracker = tracker.write().await;
                if let Some(entry) = tracker.iter_mut().find(|s|
                    s.signature == sell_clone.signature && s.token_mint == sell_clone.token_mint
                ) {
                    entry.copy_trade_status = status;
                }
            });
        }

        Ok(())
    }
}

// HTTP handlers
async fn handle_status(tracker: SellTracker) -> Result<impl warp::Reply, warp::Rejection> {
    let tracker = tracker.read().await;
    let total_sells = tracker.len();

    let pending = tracker.iter().filter(|s| matches!(s.copy_trade_status, CopyTradeStatus::Pending)).count();
    let submitted = tracker.iter().filter(|s| matches!(s.copy_trade_status, CopyTradeStatus::TxSubmitted { .. })).count();
    let confirmed = tracker.iter().filter(|s| matches!(s.copy_trade_status, CopyTradeStatus::Confirmed { .. })).count();
    let failed = tracker.iter().filter(|s| matches!(s.copy_trade_status, CopyTradeStatus::Failed { .. })).count();
    let skipped = tracker.iter().filter(|s| matches!(s.copy_trade_status, CopyTradeStatus::Skipped { .. })).count();

    let response = serde_json::json!({
        "total_sells_detected": total_sells,
        "pending": pending,
        "tx_submitted": submitted,
        "confirmed": confirmed,
        "failed": failed,
        "skipped": skipped,
    });

    Ok(warp::reply::json(&response))
}

async fn handle_trades(tracker: SellTracker) -> Result<impl warp::Reply, warp::Rejection> {
    let tracker = tracker.read().await;
    Ok(warp::reply::json(&*tracker))
}

async fn handle_health() -> Result<impl warp::Reply, warp::Rejection> {
    Ok(warp::reply::json(&serde_json::json!({"status": "ok"})))
}

/// Wrap SOL to WSOL on startup to ensure WSOL account exists and has balance
async fn wrap_sol_to_wsol(
    keypair: &Keypair,
    rpc_url: &str,
    sol_leave_on_wallet: f64,
    wsol_mint: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let rpc_client = RpcClient::new_with_commitment(
        rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    );

    let wallet_pubkey = keypair.pubkey();
    let wsol_pubkey = Pubkey::from_str(wsol_mint)?;

    // Get current SOL balance
    let balance_lamports = rpc_client.get_balance(&wallet_pubkey)?;
    let balance_sol = balance_lamports as f64 / LAMPORTS_PER_SOL as f64;

    info!("[STARTUP] Wallet balance: {} SOL", balance_sol);
    info!("[STARTUP] Will keep {} SOL unwrapped for fees", sol_leave_on_wallet);

    let leave_lamports = (sol_leave_on_wallet * LAMPORTS_PER_SOL as f64) as u64;

    if balance_lamports <= leave_lamports {
        info!("[STARTUP] Not enough SOL to wrap. Need at least {} SOL for operations.", sol_leave_on_wallet);
        return Ok(());
    }

    let wrap_lamports = balance_lamports.saturating_sub(leave_lamports);
    let wrap_sol = wrap_lamports as f64 / LAMPORTS_PER_SOL as f64;

    info!("[STARTUP] Will wrap {} SOL to WSOL", wrap_sol);

    // Derive associated token account address for WSOL manually
    let ata_program_id = Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")?;
    let system_program_id = Pubkey::from_str("11111111111111111111111111111111")?;

    let (wsol_ata, _bump) = Pubkey::find_program_address(
        &[
            wallet_pubkey.as_ref(),
            spl_token::id().as_ref(),
            wsol_pubkey.as_ref(),
        ],
        &ata_program_id,
    );

    info!("[STARTUP] WSOL account: {}", wsol_ata);

    let mut instructions = vec![];

    // Check if WSOL account exists
    let wsol_account_exists = rpc_client.get_account(&wsol_ata).is_ok();

    if !wsol_account_exists {
        info!("[STARTUP] Creating WSOL associated token account");

        // Manually create the ATA instruction
        let create_ata_ix = Instruction {
            program_id: ata_program_id,
            accounts: vec![
                AccountMeta::new(wallet_pubkey, true),
                AccountMeta::new(wsol_ata, false),
                AccountMeta::new_readonly(wallet_pubkey, false),
                AccountMeta::new_readonly(wsol_pubkey, false),
                AccountMeta::new_readonly(system_program_id, false),
                AccountMeta::new_readonly(spl_token::id(), false),
            ],
            data: vec![1], // CreateIdempotent instruction
        };
        instructions.push(create_ata_ix);
    } else {
        info!("[STARTUP] WSOL account already exists");
    }

    // Transfer SOL to WSOL account (manually create transfer instruction)
    let transfer_ix = Instruction {
        program_id: system_program_id,
        accounts: vec![
            AccountMeta::new(wallet_pubkey, true),
            AccountMeta::new(wsol_ata, false),
        ],
        data: {
            let mut data = vec![2, 0, 0, 0]; // Transfer instruction discriminator
            data.extend_from_slice(&wrap_lamports.to_le_bytes());
            data
        },
    };
    instructions.push(transfer_ix);

    // Sync native (wraps SOL to WSOL)
    instructions.push(
        spl_token::instruction::sync_native(&spl_token::id(), &wsol_ata)?,
    );

    // Get recent blockhash
    let recent_blockhash = rpc_client.get_latest_blockhash()?;

    // Create and sign transaction
    let transaction = Transaction::new_signed_with_payer(
        &instructions,
        Some(&wallet_pubkey),
        &[keypair],
        recent_blockhash,
    );

    // Send and confirm transaction
    info!("[STARTUP] Sending wrap transaction...");
    let signature = rpc_client.send_and_confirm_transaction(&transaction)?;

    info!("====================================================================");
    info!("[STARTUP] Successfully wrapped {} SOL to WSOL!", wrap_sol);
    info!("[TX SIGNATURE] {}", signature);
    info!("[SOLSCAN] https://solscan.io/tx/{}", signature);
    info!("====================================================================");

    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(true)
        .with_line_number(true)
        .init();

    info!("Starting copy-trader bot");

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let Opts { config } = Opts::parse();
    info!("Config file: {:?}", config);

    let config_str = std::fs::read_to_string(&config).expect("Error reading config file");

    // Parse full config for Vixen runtime
    let vixen_config = toml::from_str(&config_str).expect("Error parsing config");

    // Parse copy_trading section separately
    let toml_value: toml::Value = toml::from_str(&config_str).expect("Error parsing config");
    let copy_config: CopyTradingConfig = toml_value
        .get("copy_trading")
        .expect("Missing copy_trading section")
        .clone()
        .try_into()
        .expect("Invalid copy_trading config");

    let http_addr: SocketAddr = copy_config.http_addr.parse().expect("Invalid http_addr");

    let tracker: SellTracker = Arc::new(RwLock::new(Vec::new()));
    let dedup_set: DedupSet = Arc::new(RwLock::new(HashSet::new()));

    let tracker_for_http = tracker.clone();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    rt.block_on(async move {
        // Spawn HTTP server
        let http_tracker = tracker_for_http.clone();
        tokio::spawn(async move {
            let tracker_filter = warp::any().map(move || http_tracker.clone());

            let status_route = warp::path("status")
                .and(warp::path::end())
                .and(tracker_filter.clone())
                .and_then(handle_status);

            let trades_route = warp::path("trades")
                .and(warp::path::end())
                .and(tracker_filter.clone())
                .and_then(handle_trades);

            let health_route = warp::path("health")
                .and(warp::path::end())
                .and_then(handle_health);

            let routes = status_route.or(trades_route).or(health_route);

            println!("HTTP server listening on {}", http_addr);
            println!("Endpoints:");
            println!("  GET /status - Get copy trading statistics");
            println!("  GET /trades - Get all detected sells and copy trades");
            println!("  GET /health - Health check");
            warp::serve(routes).run(http_addr).await;
        });

        // Wrap SOL to WSOL before starting to ensure we can trade
        let keypair = Keypair::from_base58_string(&copy_config.private_key);
        let wsol_mint = copy_config.interesting_currencies.iter()
            .find(|c| c.name == "WSOL")
            .map(|c| c.mint.clone())
            .unwrap_or_else(|| "So11111111111111111111111111111111111111112".to_string());

        if let Err(e) = wrap_sol_to_wsol(
            &keypair,
            &copy_config.solana_rpc_url,
            copy_config.sol_leave_on_wallet,
            &wsol_mint,
        ).await {
            error!("[STARTUP] Failed to wrap SOL: {}", e);
            error!("[STARTUP] Continuing anyway, but trades may fail...");
        }

        info!("[STARTUP] Ready to process trades!");
        info!("");

        // Run vixen pipeline
        let parser = CopyTraderParser::new(&copy_config).expect("Failed to create parser");
        let handler = CopyTraderHandler::new(tracker, dedup_set, &copy_config)
            .expect("Failed to create handler");

        yellowstone_vixen::Runtime::<YellowstoneGrpcSource>::builder()
            .transaction(Pipeline::new(parser, [handler]))
            .build(vixen_config)
            .run_async()
            .await;
    });
}
