use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser as ClapParser;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use warp::Filter;
use yellowstone_vixen::Pipeline;
use yellowstone_vixen_core::{
    instruction::InstructionUpdate, ParseError, ParseResult, Parser, Prefilter,
    TransactionPrefilter, TransactionUpdate,
};
use yellowstone_vixen_proc_macro::include_vixen_parser;
use yellowstone_vixen_yellowstone_grpc_source::YellowstoneGrpcSource;
use borsh::BorshDeserialize;
use base64::Engine;

// Generate parsers from IDL
include_vixen_parser!("pump_amm_codama.json");

// Event structures from pump_amm IDL (complete with all fields)
#[derive(Debug, Clone, BorshDeserialize)]
pub struct BuyEvent {
    pub timestamp: i64,
    pub base_amount_out: u64,
    pub max_quote_amount_in: u64,
    pub user_base_token_reserves: u64,
    pub user_quote_token_reserves: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub quote_amount_in: u64,
    pub lp_fee_basis_points: u64,
    pub lp_fee: u64,
    pub protocol_fee_basis_points: u64,
    pub protocol_fee: u64,
    pub quote_amount_in_with_lp_fee: u64,
    pub user_quote_amount_in: u64,
    pub pool: [u8; 32],
    pub user: [u8; 32],
    pub user_base_token_account: [u8; 32],
    pub user_quote_token_account: [u8; 32],
    pub protocol_fee_recipient: [u8; 32],
    pub protocol_fee_recipient_token_account: [u8; 32],
    pub coin_creator: [u8; 32],
    pub coin_creator_token_account: [u8; 32],
}

#[derive(Debug, Clone, BorshDeserialize)]
pub struct SellEvent {
    pub timestamp: i64,
    pub base_amount_in: u64,
    pub min_quote_amount_out: u64,
    pub user_base_token_reserves: u64,
    pub user_quote_token_reserves: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub quote_amount_out: u64,
    pub lp_fee_basis_points: u64,
    pub lp_fee: u64,
    pub protocol_fee_basis_points: u64,
    pub protocol_fee: u64,
    pub quote_amount_out_without_lp_fee: u64,
    pub user_quote_amount_out: u64,
    pub pool: [u8; 32],
    pub user: [u8; 32],
    pub user_base_token_account: [u8; 32],
    pub user_quote_token_account: [u8; 32],
    pub protocol_fee_recipient: [u8; 32],
    pub protocol_fee_recipient_token_account: [u8; 32],
    pub coin_creator: [u8; 32],
    pub coin_creator_token_account: [u8; 32],
    pub coin_creator_fee_basis_points: u64,
    pub coin_creator_fee: u64,
    pub padding_or_unknown: [u8; 32], // Unknown field to match 416 byte size
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

        // Step 1: Buffer all swap instructions with their mint addresses
        // We need the mint from instructions to associate with events
        #[derive(Debug, Clone)]
        enum SwapType {
            Buy,
            Sell,
        }

        let mut buffered_swaps: Vec<(SwapType, String)> = vec![]; // (type, mint)

        for ix in ixs.iter().flat_map(|i| i.visit_all()) {
            if ix.program != pump_amm::ID {
                continue;
            }

            let parsed = match pump_amm::InstructionParser.parse(ix).await {
                Ok(p) => p,
                Err(_) => continue,
            };

            match parsed {
                pump_amm::PumpAmmInstruction::Buy { accounts, .. } => {
                    let mint = bs58::encode(accounts.base_mint).into_string();
                    buffered_swaps.push((SwapType::Buy, mint));
                }
                pump_amm::PumpAmmInstruction::BuyExactQuoteIn { accounts, .. } => {
                    let mint = bs58::encode(accounts.base_mint).into_string();
                    buffered_swaps.push((SwapType::Buy, mint));
                }
                pump_amm::PumpAmmInstruction::Sell { accounts, .. } => {
                    let mint = bs58::encode(accounts.base_mint).into_string();
                    buffered_swaps.push((SwapType::Sell, mint));
                }
                _ => {}
            }
        }

        // Step 2: Parse events from logs - events confirm successful execution
        let mut buy_events: Vec<BuyEvent> = vec![];
        let mut sell_events: Vec<SellEvent> = vec![];

        for log in &shared.log_messages {
            if let Some(data_str) = log.strip_prefix("Program data: ") {
                if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(data_str.trim()) {
                    if decoded.len() < 8 {
                        continue;
                    }

                    let event_data = &decoded[8..];

                    if let Ok(event) = BuyEvent::try_from_slice(event_data) {
                        buy_events.push(event);
                    } else if let Ok(event) = SellEvent::try_from_slice(event_data) {
                        sell_events.push(event);
                    }
                }
            }
        }

        // Step 3: Match events to buffered instructions by order
        // Only create swap records for instructions that have corresponding events
        let mut swaps: Vec<(String, SwapRecord)> = vec![];
        let mut buy_event_idx = 0;
        let mut sell_event_idx = 0;

        for (swap_type, mint) in buffered_swaps {
            match swap_type {
                SwapType::Buy => {
                    if buy_event_idx < buy_events.len() {
                        let event = &buy_events[buy_event_idx];
                        buy_event_idx += 1;
                        swaps.push((
                            mint,
                            SwapRecord {
                                swap_type: "buy".to_string(),
                                base_amount: event.base_amount_out,
                                quote_amount: event.quote_amount_in,
                                timestamp: event.timestamp,
                            },
                        ));
                    }
                    // No event = swap didn't complete successfully, skip it
                }
                SwapType::Sell => {
                    if sell_event_idx < sell_events.len() {
                        let event = &sell_events[sell_event_idx];
                        sell_event_idx += 1;
                        swaps.push((
                            mint,
                            SwapRecord {
                                swap_type: "sell".to_string(),
                                base_amount: event.base_amount_in,
                                quote_amount: event.quote_amount_out,
                                timestamp: event.timestamp,
                            },
                        ));
                    }
                    // No event = swap didn't complete successfully, skip it
                }
            }
        }

        if swaps.is_empty() {
            return Err(ParseError::Filtered);
        }

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
        _raw: &TransactionUpdate,
    ) -> yellowstone_vixen::HandlerResult<()> {
        let mut tracker = self.tracker.write().await;

        for (mint, swap) in &value.swaps {
            let swaps = tracker.entry(mint.clone()).or_insert_with(Vec::new);

            // Only keep first MAX_SWAPS_PER_MINT
            if swaps.len() < MAX_SWAPS_PER_MINT {
                swaps.push(swap.clone());
            }
        }

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
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let Opts { config, http_addr } = Opts::parse();
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
