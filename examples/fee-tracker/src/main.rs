use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser as ClapParser;
use serde::Serialize;
use tokio::sync::RwLock;
use warp::Filter;
use yellowstone_vixen::Pipeline;
use yellowstone_vixen_core::{
    instruction::InstructionUpdate, KeyBytes, ParseError, ParseResult, Parser, Prefilter, Pubkey,
    TransactionPrefilter, TransactionUpdate,
};
use yellowstone_vixen_proc_macro::include_vixen_parser;
use yellowstone_vixen_yellowstone_grpc_source::YellowstoneGrpcSource;

// Generate parsers from IDLs
include_vixen_parser!("pump_fun_codama.json");
include_vixen_parser!("pump_amm_codama.json");

// System Program ID for native SOL transfers
const SYSTEM_PROGRAM_ID: Pubkey = KeyBytes([0u8; 32]);

/// Load tip addresses from a CSV file
/// CSV format: recipient,total_sol,total_lamports,transfer_count,unique_signers
fn load_tip_addresses_from_csv(path: &std::path::Path) -> Vec<Pubkey> {
    let file = std::fs::File::open(path).expect("Failed to open tip addresses CSV file");
    let reader = std::io::BufReader::new(file);

    let mut addresses = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.expect("Failed to read line from CSV");
        // Skip header
        if i == 0 {
            continue;
        }
        // Parse the first column (recipient address)
        let address_str = line.split(',').next().unwrap_or("").trim();
        if address_str.is_empty() {
            continue;
        }
        // Decode base58 address to bytes
        match bs58::decode(address_str).into_vec() {
            Ok(bytes) if bytes.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                addresses.push(KeyBytes(arr));
            }
            Ok(_) => eprintln!("Warning: Invalid address length for {}", address_str),
            Err(e) => eprintln!("Warning: Failed to decode address {}: {}", address_str, e),
        }
    }
    println!("Loaded {} tip addresses from CSV", addresses.len());
    addresses
}

#[derive(clap::Parser)]
#[command(version, author, about)]
pub struct Opts {
    #[arg(long, short)]
    config: PathBuf,

    /// HTTP server bind address
    #[arg(long, default_value = "0.0.0.0:8080")]
    http_addr: SocketAddr,

    /// Path to CSV file containing tip addresses (format: recipient,...)
    #[arg(long, short = 't')]
    tip_addresses: PathBuf,
}

/// Fee statistics for a single mint
#[derive(Debug, Clone, Default, Serialize)]
pub struct MintFeeStats {
    pub success_gas_paid: u64,
    pub error_gas_paid: u64,
    pub success_swqos_tips: u64,
    pub failed_swqos_tips: u64,
}

/// Shared state for fee tracking across all mints
pub type FeeTracker = Arc<RwLock<HashMap<String, MintFeeStats>>>;

/// Parse result from a single transaction
#[derive(Debug)]
pub struct PumpTxResult {
    pub succeeded: bool,
    pub fee: u64,
    pub total_tip_lamports: u64,
    pub mints: Vec<String>,
}

/// Parse a System Program transfer instruction
/// Returns (from, to, lamports) if this is a transfer instruction
fn parse_system_transfer(ix: &InstructionUpdate) -> Option<(Pubkey, Pubkey, u64)> {
    if ix.data.len() < 12 {
        return None;
    }

    let discriminator = u32::from_le_bytes(ix.data[0..4].try_into().ok()?);
    if discriminator != 2 {
        return None;
    }

    let lamports = u64::from_le_bytes(ix.data[4..12].try_into().ok()?);

    if ix.accounts.len() < 2 {
        return None;
    }

    let from = ix.accounts[0];
    let to = ix.accounts[1];

    Some((from, to, lamports))
}

// Transaction-level parser
#[derive(Debug, Clone)]
pub struct PumpCombinedParser {
    tip_whitelist: Arc<HashSet<Pubkey>>,
}

impl PumpCombinedParser {
    pub fn new(tip_addresses: Vec<Pubkey>) -> Self {
        Self {
            tip_whitelist: Arc::new(tip_addresses.into_iter().collect()),
        }
    }
}

impl Parser for PumpCombinedParser {
    type Input = TransactionUpdate;
    type Output = PumpTxResult;

    fn id(&self) -> std::borrow::Cow<'static, str> {
        "PumpCombinedParser".into()
    }

    fn prefilter(&self) -> Prefilter {
        let mut accounts_include = HashSet::new();
        accounts_include.insert(pump::ID);
        accounts_include.insert(pump_amm::ID);

        Prefilter {
            account: None,
            transaction: Some(TransactionPrefilter {
                accounts_include,
                accounts_required: HashSet::new(),
                failed: None,
            }),
            block_meta: None,
            block: None,
            slot: None,
        }
    }

    async fn parse(&self, txn: &TransactionUpdate) -> ParseResult<PumpTxResult> {
        let ixs = InstructionUpdate::parse_from_txn(txn)
            .map_err(|e| ParseError::Other(Box::new(e)))?;

        let shared = ixs
            .first()
            .map(|ix| Arc::clone(&ix.shared))
            .ok_or(ParseError::Filtered)?;

        let succeeded = shared.err.is_none();
        let fee = shared.fee;
        let mut total_tip_lamports: u64 = 0;
        let mut mints: Vec<String> = vec![];

        // Check top-level instructions for transfers to whitelisted addresses
        for ix in ixs.iter() {
            if ix.program == SYSTEM_PROGRAM_ID {
                if let Some((_from, to, lamports)) = parse_system_transfer(ix) {
                    if self.tip_whitelist.contains(&to) {
                        total_tip_lamports += lamports;
                    }
                }
            }
        }

        // Iterate all instructions for pump parsing to extract mints
        for ix in ixs.iter().flat_map(|i| i.visit_all()) {
            if ix.program == pump::ID {
                if let Ok(parsed) = pump::InstructionParser.parse(ix).await {
                    match parsed {
                        pump::PumpInstruction::Buy { accounts, .. } => {
                            mints.push(bs58::encode(accounts.mint).into_string());
                        }
                        pump::PumpInstruction::BuyExactSolIn { accounts, .. } => {
                            mints.push(bs58::encode(accounts.mint).into_string());
                        }
                        pump::PumpInstruction::Sell { accounts, .. } => {
                            mints.push(bs58::encode(accounts.mint).into_string());
                        }
                        _ => {}
                    }
                }
            }

            if ix.program == pump_amm::ID {
                if let Ok(parsed) = pump_amm::InstructionParser.parse(ix).await {
                    match parsed {
                        pump_amm::PumpAmmInstruction::Buy { accounts, .. } => {
                            mints.push(bs58::encode(accounts.base_mint).into_string());
                        }
                        pump_amm::PumpAmmInstruction::BuyExactQuoteIn { accounts, .. } => {
                            mints.push(bs58::encode(accounts.base_mint).into_string());
                        }
                        pump_amm::PumpAmmInstruction::Sell { accounts, .. } => {
                            mints.push(bs58::encode(accounts.base_mint).into_string());
                        }
                        _ => {}
                    }
                }
            }
        }

        if mints.is_empty() {
            return Err(ParseError::Filtered);
        }

        // Deduplicate mints
        mints.sort();
        mints.dedup();

        Ok(PumpTxResult {
            succeeded,
            fee,
            total_tip_lamports,
            mints,
        })
    }
}

/// Handler that updates the fee tracker
#[derive(Debug)]
pub struct FeeTrackerHandler {
    tracker: FeeTracker,
}

impl FeeTrackerHandler {
    pub fn new(tracker: FeeTracker) -> Self {
        Self { tracker }
    }
}

impl yellowstone_vixen::Handler<PumpTxResult, TransactionUpdate> for FeeTrackerHandler {
    async fn handle(
        &self,
        value: &PumpTxResult,
        _raw: &TransactionUpdate,
    ) -> yellowstone_vixen::HandlerResult<()> {
        let mut tracker = self.tracker.write().await;

        for mint in &value.mints {
            let stats = tracker.entry(mint.clone()).or_default();

            if value.succeeded {
                stats.success_gas_paid += value.fee;
                stats.success_swqos_tips += value.total_tip_lamports;
            } else {
                stats.error_gas_paid += value.fee;
                stats.failed_swqos_tips += value.total_tip_lamports;
            }
        }

        Ok(())
    }
}

async fn handle_fees_query(
    params: HashMap<String, String>,
    tracker: FeeTracker,
) -> Result<impl warp::Reply, warp::Rejection> {
    let mint = match params.get("mint") {
        Some(m) => m,
        None => {
            return Ok(warp::reply::with_status(
                warp::reply::json(&serde_json::json!({"error": "missing 'mint' query parameter"})),
                warp::http::StatusCode::BAD_REQUEST,
            ));
        }
    };

    let tracker = tracker.read().await;
    let stats = tracker.get(mint).cloned().unwrap_or_default();

    Ok(warp::reply::with_status(
        warp::reply::json(&stats),
        warp::http::StatusCode::OK,
    ))
}

fn main() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let Opts {
        config,
        http_addr,
        tip_addresses,
    } = Opts::parse();
    let config_str = std::fs::read_to_string(&config).expect("Error reading config file");
    let config = toml::from_str(&config_str).expect("Error parsing config");

    // Load tip addresses from CSV
    let tip_address_list = load_tip_addresses_from_csv(&tip_addresses);
    if tip_address_list.is_empty() {
        eprintln!("Warning: No tip addresses loaded from CSV file");
    }

    let tracker: FeeTracker = Arc::new(RwLock::new(HashMap::new()));
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

            let fees_route = warp::path("fees")
                .and(warp::query::<HashMap<String, String>>())
                .and(tracker_filter)
                .and_then(handle_fees_query);

            println!("HTTP server listening on {}", http_addr);
            warp::serve(fees_route).run(http_addr).await;
        });

        // Run vixen pipeline
        let parser = PumpCombinedParser::new(tip_address_list);
        yellowstone_vixen::Runtime::<YellowstoneGrpcSource>::builder()
            .transaction(Pipeline::new(parser, [FeeTrackerHandler::new(tracker)]))
            .build(config)
            .run_async()
            .await;
    });
}
