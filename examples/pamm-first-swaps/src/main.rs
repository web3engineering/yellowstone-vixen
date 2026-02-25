use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser as ClapParser;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn, trace};
use tracing_subscriber;
use warp::Filter;
use yellowstone_vixen::Pipeline;
use yellowstone_vixen_core::{
    instruction::InstructionUpdate, ParseError, ParseResult, Parser, Prefilter,
    TransactionPrefilter, TransactionUpdate,
};
use yellowstone_vixen_proc_macro::include_vixen_parser;
use yellowstone_vixen_yellowstone_grpc_source::YellowstoneGrpcSource;
use base64::Engine;

// Generate parsers from IDL
include_vixen_parser!("pump_amm_codama.json");

// Event discriminators (first 8 bytes of sha256("event:EventName"))
const BUY_EVENT_DISC: [u8; 8] = [0x67, 0xf4, 0x52, 0x1f, 0x2c, 0xf5, 0x77, 0x77];
const SELL_EVENT_DISC: [u8; 8] = [0x3e, 0x2f, 0x37, 0x0a, 0xa5, 0x03, 0xdc, 0x2a];

/// Parsed swap event data - we only extract the fields we need
#[derive(Debug, Clone)]
pub struct ParsedSwapEvent {
    pub timestamp: i64,
    pub base_amount: u64,
    pub quote_amount: u64,
}

/// Parse a BuyEvent from raw bytes (after discriminator)
/// Layout: timestamp(i64), base_amount_out(u64), max_quote_amount_in(u64),
///         user_base_token_reserves(u64), user_quote_token_reserves(u64),
///         pool_base_token_reserves(u64), pool_quote_token_reserves(u64),
///         quote_amount_in(u64), ...
fn parse_buy_event(data: &[u8]) -> Option<ParsedSwapEvent> {
    // Need at least 64 bytes: 8 fields * 8 bytes each
    if data.len() < 64 {
        return None;
    }

    let timestamp = i64::from_le_bytes(data[0..8].try_into().ok()?);
    let base_amount_out = u64::from_le_bytes(data[8..16].try_into().ok()?);
    // quote_amount_in is at offset 56 (after 7 u64 fields)
    let quote_amount_in = u64::from_le_bytes(data[56..64].try_into().ok()?);

    Some(ParsedSwapEvent {
        timestamp,
        base_amount: base_amount_out,
        quote_amount: quote_amount_in,
    })
}

/// Parse a SellEvent from raw bytes (after discriminator)
/// Layout: timestamp(i64), base_amount_in(u64), min_quote_amount_out(u64),
///         user_base_token_reserves(u64), user_quote_token_reserves(u64),
///         pool_base_token_reserves(u64), pool_quote_token_reserves(u64),
///         quote_amount_out(u64), ...
fn parse_sell_event(data: &[u8]) -> Option<ParsedSwapEvent> {
    // Need at least 64 bytes: 8 fields * 8 bytes each
    if data.len() < 64 {
        return None;
    }

    let timestamp = i64::from_le_bytes(data[0..8].try_into().ok()?);
    let base_amount_in = u64::from_le_bytes(data[8..16].try_into().ok()?);
    // quote_amount_out is at offset 56 (after 7 u64 fields)
    let quote_amount_out = u64::from_le_bytes(data[56..64].try_into().ok()?);

    Some(ParsedSwapEvent {
        timestamp,
        base_amount: base_amount_in,
        quote_amount: quote_amount_out,
    })
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
        info!("========== PARSING TRANSACTION ==========");
        info!("Transaction signature: {}", txn_sig);
        info!("Slot: {}", txn.slot);

        let ixs = InstructionUpdate::parse_from_txn(txn)
            .map_err(|e| {
                warn!("[{}] Failed to parse instructions: {:?}", txn_sig, e);
                ParseError::Other(Box::new(e))
            })?;

        info!("[{}] Parsed {} top-level instructions", txn_sig, ixs.len());

        let shared = ixs
            .first()
            .map(|ix| Arc::clone(&ix.shared))
            .ok_or_else(|| {
                warn!("[{}] No instructions found, filtering", txn_sig);
                ParseError::Filtered
            })?;

        // Skip failed transactions
        if let Some(ref err) = shared.err {
            warn!("[{}] Transaction failed with error: {:?}, filtering", txn_sig, err);
            return Err(ParseError::Filtered);
        }
        info!("[{}] Transaction succeeded (no error)", txn_sig);

        // Log all log messages for debugging
        debug!("[{}] Log messages ({} total):", txn_sig, shared.log_messages.len());
        for (i, log) in shared.log_messages.iter().enumerate() {
            trace!("[{}]   log[{}]: {}", txn_sig, i, log);
        }

        // Step 1: Buffer all swap instructions with their mint addresses
        // We need the mint from instructions to associate with events
        #[derive(Debug, Clone)]
        enum SwapType {
            Buy,
            Sell,
        }

        let mut buffered_swaps: Vec<(SwapType, String)> = vec![]; // (type, mint)

        let total_ixs: Vec<_> = ixs.iter().flat_map(|i| i.visit_all()).collect();
        info!("[{}] Total instructions (including inner): {}", txn_sig, total_ixs.len());

        for (ix_idx, ix) in total_ixs.iter().enumerate() {
            let program_str = bs58::encode(&ix.program).into_string();
            trace!("[{}] ix[{}] program: {}", txn_sig, ix_idx, program_str);

            if ix.program != pump_amm::ID {
                continue;
            }

            debug!("[{}] ix[{}] is pump_amm instruction, attempting to parse", txn_sig, ix_idx);

            let parsed = match pump_amm::InstructionParser.parse(ix).await {
                Ok(p) => {
                    debug!("[{}] ix[{}] parsed successfully", txn_sig, ix_idx);
                    p
                },
                Err(e) => {
                    // This is expected for instructions not in our IDL (e.g., newer instructions)
                    // or non-swap instructions we don't care about
                    debug!("[{}] ix[{}] pump_amm instruction not recognized (possibly newer or non-swap): {:?}", txn_sig, ix_idx, e);
                    continue;
                }
            };

            match parsed {
                pump_amm::PumpAmmInstruction::Buy { accounts, args } => {
                    let mint = bs58::encode(accounts.base_mint).into_string();
                    info!("[{}] ix[{}] FOUND BUY instruction, mint: {}, args: {:?}", txn_sig, ix_idx, mint, args);
                    buffered_swaps.push((SwapType::Buy, mint));
                }
                pump_amm::PumpAmmInstruction::BuyExactQuoteIn { accounts, args } => {
                    let mint = bs58::encode(accounts.base_mint).into_string();
                    info!("[{}] ix[{}] FOUND BUY_EXACT_QUOTE_IN instruction, mint: {}, args: {:?}", txn_sig, ix_idx, mint, args);
                    buffered_swaps.push((SwapType::Buy, mint));
                }
                pump_amm::PumpAmmInstruction::Sell { accounts, args } => {
                    let mint = bs58::encode(accounts.base_mint).into_string();
                    info!("[{}] ix[{}] FOUND SELL instruction, mint: {}, args: {:?}", txn_sig, ix_idx, mint, args);
                    buffered_swaps.push((SwapType::Sell, mint));
                }
                other => {
                    debug!("[{}] ix[{}] pump_amm instruction but not a swap: {:?}", txn_sig, ix_idx, std::mem::discriminant(&other));
                }
            }
        }

        info!("[{}] Buffered {} swap instructions", txn_sig, buffered_swaps.len());
        for (i, (swap_type, mint)) in buffered_swaps.iter().enumerate() {
            info!("[{}]   buffered_swap[{}]: {:?} mint={}", txn_sig, i, swap_type, mint);
        }

        // Step 2: Parse events from logs - events confirm successful execution
        let mut buy_events: Vec<ParsedSwapEvent> = vec![];
        let mut sell_events: Vec<ParsedSwapEvent> = vec![];

        info!("[{}] Parsing events from logs...", txn_sig);
        for (log_idx, log) in shared.log_messages.iter().enumerate() {
            if let Some(data_str) = log.strip_prefix("Program data: ") {
                debug!("[{}] log[{}] Found 'Program data:' prefix", txn_sig, log_idx);

                match base64::engine::general_purpose::STANDARD.decode(data_str.trim()) {
                    Ok(decoded) => {
                        debug!("[{}] log[{}] Decoded {} bytes", txn_sig, log_idx, decoded.len());

                        // Event format: [8 bytes discriminator][event data]
                        // Discriminator is at bytes 0-8, event data starts at byte 8
                        if decoded.len() < 8 {
                            debug!("[{}] log[{}] Too short (<8 bytes), skipping", txn_sig, log_idx);
                            continue;
                        }

                        let event_discriminator = &decoded[0..8];
                        let event_data = &decoded[8..];

                        debug!("[{}] log[{}] Discriminator: {:02x?}, data len: {} bytes",
                            txn_sig, log_idx, event_discriminator, event_data.len());

                        if event_discriminator == BUY_EVENT_DISC {
                            match parse_buy_event(event_data) {
                                Some(event) => {
                                    info!("[{}] log[{}] PARSED BUY EVENT: timestamp={}, base_amount={}, quote_amount={}",
                                        txn_sig, log_idx, event.timestamp, event.base_amount, event.quote_amount);
                                    buy_events.push(event);
                                }
                                None => {
                                    warn!("[{}] log[{}] BuyEvent discriminator matched but failed to parse (data too short: {} bytes)",
                                        txn_sig, log_idx, event_data.len());
                                }
                            }
                        } else if event_discriminator == SELL_EVENT_DISC {
                            match parse_sell_event(event_data) {
                                Some(event) => {
                                    info!("[{}] log[{}] PARSED SELL EVENT: timestamp={}, base_amount={}, quote_amount={}",
                                        txn_sig, log_idx, event.timestamp, event.base_amount, event.quote_amount);
                                    sell_events.push(event);
                                }
                                None => {
                                    warn!("[{}] log[{}] SellEvent discriminator matched but failed to parse (data too short: {} bytes)",
                                        txn_sig, log_idx, event_data.len());
                                }
                            }
                        } else {
                            debug!("[{}] log[{}] Unknown event discriminator {:02x?}, skipping", txn_sig, log_idx, event_discriminator);
                        }
                    }
                    Err(e) => {
                        warn!("[{}] log[{}] Failed to decode base64: {:?}", txn_sig, log_idx, e);
                    }
                }
            }
        }

        info!("[{}] Found {} buy events, {} sell events", txn_sig, buy_events.len(), sell_events.len());

        // Step 3: Match events to buffered instructions by order
        // Only create swap records for instructions that have corresponding events
        info!("[{}] Matching events to instructions...", txn_sig);
        let mut swaps: Vec<(String, SwapRecord)> = vec![];
        let mut buy_event_idx = 0;
        let mut sell_event_idx = 0;

        for (i, (swap_type, mint)) in buffered_swaps.iter().enumerate() {
            match swap_type {
                SwapType::Buy => {
                    if buy_event_idx < buy_events.len() {
                        let event = &buy_events[buy_event_idx];
                        buy_event_idx += 1;
                        info!("[{}] MATCHED buy instruction[{}] mint={} with buy_event[{}]",
                            txn_sig, i, mint, buy_event_idx - 1);
                        swaps.push((
                            mint.clone(),
                            SwapRecord {
                                swap_type: "buy".to_string(),
                                base_amount: event.base_amount,
                                quote_amount: event.quote_amount,
                                timestamp: event.timestamp,
                            },
                        ));
                    } else {
                        warn!("[{}] NO MATCHING EVENT for buy instruction[{}] mint={} (no more buy events)",
                            txn_sig, i, mint);
                    }
                }
                SwapType::Sell => {
                    if sell_event_idx < sell_events.len() {
                        let event = &sell_events[sell_event_idx];
                        sell_event_idx += 1;
                        info!("[{}] MATCHED sell instruction[{}] mint={} with sell_event[{}]",
                            txn_sig, i, mint, sell_event_idx - 1);
                        swaps.push((
                            mint.clone(),
                            SwapRecord {
                                swap_type: "sell".to_string(),
                                base_amount: event.base_amount,
                                quote_amount: event.quote_amount,
                                timestamp: event.timestamp,
                            },
                        ));
                    } else {
                        warn!("[{}] NO MATCHING EVENT for sell instruction[{}] mint={} (no more sell events)",
                            txn_sig, i, mint);
                    }
                }
            }
        }

        info!("[{}] Final result: {} matched swaps", txn_sig, swaps.len());
        for (i, (mint, record)) in swaps.iter().enumerate() {
            info!("[{}]   swap[{}]: mint={}, type={}, base={}, quote={}, ts={}",
                txn_sig, i, mint, record.swap_type, record.base_amount, record.quote_amount, record.timestamp);
        }

        if swaps.is_empty() {
            warn!("[{}] No swaps matched, filtering transaction", txn_sig);
            return Err(ParseError::Filtered);
        }

        info!("[{}] ========== PARSE COMPLETE ==========", txn_sig);
        Ok(PumpAmmSwapResult { swaps })
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
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("yellowstone_vixen_example_pamm_first_swaps=info".parse().unwrap())
        )
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
