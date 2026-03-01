use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser as ClapParser;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info};
use tracing_subscriber;
use warp::Filter;
use yellowstone_vixen::Pipeline;
use yellowstone_vixen_core::{
    instruction::InstructionUpdate, ParseError, ParseResult, Parser, Prefilter,
    TransactionPrefilter, TransactionUpdate,
};
use yellowstone_vixen_proc_macro::include_vixen_parser;
use yellowstone_vixen_yellowstone_grpc_source::YellowstoneGrpcSource;

// Generate parsers from IDL
include_vixen_parser!("pump_amm_codama.json");

// Event discriminators (first 8 bytes of sha256("event:EventName"))
const BUY_EVENT_DISC: [u8; 8] = [0x67, 0xf4, 0x52, 0x1f, 0x2c, 0xf5, 0x77, 0x77];
const SELL_EVENT_DISC: [u8; 8] = [0x3e, 0x2f, 0x37, 0x0a, 0xa5, 0x03, 0xdc, 0x2a];

/// Parsed swap event data
#[derive(Debug, Clone)]
pub struct ParsedSwapEvent {
    pub is_buy: bool,
    pub timestamp: i64,
    pub base_amount: u64,
    pub quote_amount: u64,
}

/// Try to parse an anchor event from CPI inner instruction data
/// Format: [8 bytes ix discriminator][8 bytes event discriminator][event data]
fn try_parse_event_from_cpi(data: &[u8]) -> Option<ParsedSwapEvent> {
    // CPI event format: [8 bytes ix discriminator][8 bytes event discriminator][event data]
    // We need at least 8 + 8 + 64 bytes
    if data.len() < 16 + 64 {
        return None;
    }

    // Skip the first 8 bytes (instruction discriminator), event discriminator is at bytes 8-16
    let discriminator = &data[8..16];
    let event_data = &data[16..];

    if discriminator == BUY_EVENT_DISC {
        // BuyEvent layout: timestamp(i64), base_amount_out(u64), max_quote_amount_in(u64),
        //   user_base_token_reserves(u64), user_quote_token_reserves(u64),
        //   pool_base_token_reserves(u64), pool_quote_token_reserves(u64),
        //   quote_amount_in(u64)
        let timestamp = i64::from_le_bytes(event_data[0..8].try_into().ok()?);
        let base_amount_out = u64::from_le_bytes(event_data[8..16].try_into().ok()?);
        let quote_amount_in = u64::from_le_bytes(event_data[56..64].try_into().ok()?);
        Some(ParsedSwapEvent {
            is_buy: true,
            timestamp,
            base_amount: base_amount_out,
            quote_amount: quote_amount_in,
        })
    } else if discriminator == SELL_EVENT_DISC {
        // SellEvent layout: timestamp(i64), base_amount_in(u64), min_quote_amount_out(u64),
        //   user_base_token_reserves(u64), user_quote_token_reserves(u64),
        //   pool_base_token_reserves(u64), pool_quote_token_reserves(u64),
        //   quote_amount_out(u64)
        let timestamp = i64::from_le_bytes(event_data[0..8].try_into().ok()?);
        let base_amount_in = u64::from_le_bytes(event_data[8..16].try_into().ok()?);
        let quote_amount_out = u64::from_le_bytes(event_data[56..64].try_into().ok()?);
        Some(ParsedSwapEvent {
            is_buy: false,
            timestamp,
            base_amount: base_amount_in,
            quote_amount: quote_amount_out,
        })
    } else {
        None
    }
}

#[derive(clap::Parser)]
#[command(version, author, about)]
pub struct Opts {
    #[arg(long, short)]
    config: PathBuf,

    /// HTTP server bind address
    #[arg(long, default_value = "0.0.0.0:8080")]
    http_addr: SocketAddr,
}

/// A single swap/trade record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapRecord {
    pub swap_type: String, // "buy" or "sell"
    pub base_amount: u64,
    pub quote_amount: u64,
    pub timestamp: i64,
    pub signature: String,
    pub slot: u64,
}

/// Tracker for first N swaps per mint
pub type SwapTracker = Arc<RwLock<HashMap<String, Vec<SwapRecord>>>>;

const MAX_SWAPS_PER_MINT: usize = 100;

/// Parse result from a single transaction containing pump AMM swaps
#[derive(Debug)]
pub struct PumpAmmSwapResult {
    pub swaps: Vec<(String, SwapRecord)>, // (mint, swap_record)
}

// Transaction-level parser
#[derive(Debug, Clone)]
pub struct PumpAmmSwapParser;

impl Parser for PumpAmmSwapParser {
    type Input = TransactionUpdate;
    type Output = PumpAmmSwapResult;

    fn id(&self) -> std::borrow::Cow<'static, str> {
        "PumpAmmSwapParser".into()
    }

    fn prefilter(&self) -> Prefilter {
        let mut accounts_include = std::collections::HashSet::new();
        accounts_include.insert(pump_amm::ID);

        Prefilter {
            account: None,
            transaction: Some(TransactionPrefilter {
                accounts_include,
                accounts_required: std::collections::HashSet::new(),
                failed: Some(false), // Only track successful transactions
            }),
            block_meta: None,
            block: None,
            slot: None,
        }
    }

    async fn parse(&self, txn: &TransactionUpdate) -> ParseResult<PumpAmmSwapResult> {
        let txn_sig = txn.transaction.as_ref()
            .map(|t| bs58::encode(&t.signature).into_string())
            .unwrap_or_else(|| "unknown".to_string());

        debug!("[RECV] slot={} sig={}", txn.slot, txn_sig);

        let ixs = InstructionUpdate::parse_from_txn(txn)
            .map_err(|e| ParseError::Other(Box::new(e)))?;

        let shared = ixs
            .first()
            .map(|ix| Arc::clone(&ix.shared))
            .ok_or(ParseError::Filtered)?;

        // Skip failed transactions
        if shared.err.is_some() {
            return Err(ParseError::Filtered);
        }

        let slot = txn.slot;
        let mut swaps: Vec<(String, SwapRecord)> = vec![];

        // Process each top-level instruction
        for ix in &ixs {
            self.process_instruction_tree(ix, &txn_sig, slot, &mut swaps).await;
        }

        if swaps.is_empty() {
            return Err(ParseError::Filtered);
        }

        info!("[{}] Found {} swaps via CPI events", txn_sig, swaps.len());
        Ok(PumpAmmSwapResult { swaps })
    }
}

impl PumpAmmSwapParser {
    /// Recursively process instruction tree, looking for swap instructions
    /// and extracting events from their CPI inner instructions
    async fn process_instruction_tree(
        &self,
        ix: &InstructionUpdate,
        txn_sig: &str,
        slot: u64,
        swaps: &mut Vec<(String, SwapRecord)>,
    ) {
        // Check if this is a pump_amm instruction
        if ix.program == pump_amm::ID {
            if let Ok(parsed) = pump_amm::InstructionParser.parse(ix).await {
                let mint = match &parsed {
                    pump_amm::PumpAmmInstruction::Buy { accounts, .. } => {
                        Some(bs58::encode(accounts.base_mint).into_string())
                    }
                    pump_amm::PumpAmmInstruction::BuyExactQuoteIn { accounts, .. } => {
                        Some(bs58::encode(accounts.base_mint).into_string())
                    }
                    pump_amm::PumpAmmInstruction::Sell { accounts, .. } => {
                        Some(bs58::encode(accounts.base_mint).into_string())
                    }
                    _ => None,
                };

                if let Some(mint) = mint {
                    // Look for event in CPI inner instructions
                    if let Some(event) = self.find_event_in_inner(&ix.inner) {
                        let swap_type = if event.is_buy { "buy" } else { "sell" };
                        info!("[{}] Found {} swap for mint {} via CPI event: base={}, quote={}",
                            txn_sig, swap_type, mint, event.base_amount, event.quote_amount);

                        swaps.push((
                            mint,
                            SwapRecord {
                                swap_type: swap_type.to_string(),
                                base_amount: event.base_amount,
                                quote_amount: event.quote_amount,
                                timestamp: event.timestamp,
                                signature: txn_sig.to_string(),
                                slot,
                            },
                        ));
                    } else {
                        // No event found = instruction likely failed
                        debug!("[{}] Swap instruction for mint {} has no CPI event", txn_sig, mint);
                    }
                }
            }
        }

        // Recurse into inner instructions (for nested CPIs)
        for inner_ix in &ix.inner {
            Box::pin(self.process_instruction_tree(inner_ix, txn_sig, slot, swaps)).await;
        }
    }

    /// Find anchor event in CPI inner instructions
    fn find_event_in_inner(&self, inner: &[InstructionUpdate]) -> Option<ParsedSwapEvent> {
        for inner_ix in inner {
            // Try to parse event from this instruction's data
            if let Some(event) = try_parse_event_from_cpi(&inner_ix.data) {
                return Some(event);
            }
            // Recurse into nested inner instructions
            if let Some(event) = self.find_event_in_inner(&inner_ix.inner) {
                return Some(event);
            }
        }
        None
    }
}

/// Handler that updates the swap tracker
#[derive(Debug)]
pub struct SwapTrackerHandler {
    tracker: SwapTracker,
}

impl SwapTrackerHandler {
    pub fn new(tracker: SwapTracker) -> Self {
        Self { tracker }
    }
}

impl yellowstone_vixen::Handler<PumpAmmSwapResult, TransactionUpdate> for SwapTrackerHandler {
    async fn handle(
        &self,
        value: &PumpAmmSwapResult,
        raw: &TransactionUpdate,
    ) -> yellowstone_vixen::HandlerResult<()> {
        let txn_sig = raw.transaction.as_ref()
            .map(|t| bs58::encode(&t.signature).into_string())
            .unwrap_or_else(|| "unknown".to_string());
        info!("[HANDLER][{}] Received {} swaps to store", txn_sig, value.swaps.len());

        let mut tracker = self.tracker.write().await;

        for (mint, swap) in &value.swaps {
            let swaps = tracker.entry(mint.clone()).or_insert_with(Vec::new);

            // Only keep first MAX_SWAPS_PER_MINT
            if swaps.len() < MAX_SWAPS_PER_MINT {
                swaps.push(swap.clone());
                info!("[HANDLER][{}] STORED swap for mint {}: type={}, base={}, quote={} (total for mint: {})",
                    txn_sig, mint, swap.swap_type, swap.base_amount, swap.quote_amount, swaps.len());
            } else {
                debug!("[HANDLER][{}] Skipped swap for mint {} (already at max {})",
                    txn_sig, mint, MAX_SWAPS_PER_MINT);
            }
        }

        let total_mints = tracker.len();
        let total_swaps: usize = tracker.values().map(|v| v.len()).sum();
        info!("[HANDLER][{}] Tracker stats: {} mints, {} total swaps", txn_sig, total_mints, total_swaps);

        Ok(())
    }
}

async fn handle_swaps_query(
    tracker: SwapTracker,
) -> Result<impl warp::Reply, warp::Rejection> {
    let tracker = tracker.read().await;

    Ok(warp::reply::with_status(
        warp::reply::json(&*tracker),
        warp::http::StatusCode::OK,
    ))
}

async fn handle_mint_swaps_query(
    mint: String,
    tracker: SwapTracker,
) -> Result<impl warp::Reply, warp::Rejection> {
    let tracker = tracker.read().await;

    let swaps = tracker.get(&mint).cloned().unwrap_or_default();

    Ok(warp::reply::with_status(
        warp::reply::json(&swaps),
        warp::http::StatusCode::OK,
    ))
}

fn main() {
    // Initialize tracing with env filter
    // Use RUST_LOG=info or RUST_LOG=debug or RUST_LOG=trace for more detail
    // Example: RUST_LOG=yellowstone_vixen_example_pamm_first_swaps=debug,info
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(true)
        .with_line_number(true)
        .init();

    info!("Starting pamm-first-swaps tracker");

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let Opts { config, http_addr } = Opts::parse();
    info!("Config file: {:?}", config);
    info!("HTTP address: {}", http_addr);

    let config_str = std::fs::read_to_string(&config).expect("Error reading config file");
    let config = toml::from_str(&config_str).expect("Error parsing config");

    let tracker: SwapTracker = Arc::new(RwLock::new(HashMap::new()));
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

            // Route to get all swaps
            let all_swaps_route = warp::path("swaps")
                .and(warp::path::end())
                .and(tracker_filter.clone())
                .and_then(handle_swaps_query);

            // Route to get swaps for a specific mint
            let mint_swaps_route = warp::path!("swaps" / String)
                .and(tracker_filter)
                .and_then(handle_mint_swaps_query);

            let routes = all_swaps_route.or(mint_swaps_route);

            println!("HTTP server listening on {}", http_addr);
            println!("Endpoints:");
            println!("  GET /swaps - Get all swaps");
            println!("  GET /swaps/<mint> - Get swaps for specific mint");
            warp::serve(routes).run(http_addr).await;
        });

        // Run vixen pipeline
        let parser = PumpAmmSwapParser;
        yellowstone_vixen::Runtime::<YellowstoneGrpcSource>::builder()
            .transaction(Pipeline::new(parser, [SwapTrackerHandler::new(tracker)]))
            .build(config)
            .run_async()
            .await;
    });
}
