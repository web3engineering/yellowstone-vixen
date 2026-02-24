use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser as ClapParser;
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

// Whitelisted addresses to track transfers to (e.g., Jito tip accounts)
// These are the 8 Jito tip accounts
const WHITELISTED_TIP_ACCOUNTS: &[[u8; 32]] = &[
    // 96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5
    [0x78, 0x52, 0x1c, 0xb1, 0x79, 0xce, 0xbb, 0x85, 0x89, 0xb5, 0x56, 0xa2, 0xd5, 0xec, 0x94, 0xd2,
     0x49, 0x86, 0x82, 0xfd, 0xf9, 0xbb, 0x2a, 0xf5, 0xad, 0x64, 0xe4, 0x91, 0xcc, 0x41, 0x53, 0xda],
    // HFqU5x63VTqvQss8hp11i4bVmkdzGHSYtkfkRrUmYaFp
    [0xf1, 0x87, 0xec, 0x87, 0xd1, 0xf7, 0x45, 0xcb, 0x3a, 0x03, 0x38, 0x4a, 0x26, 0xa6, 0x9e, 0xd9,
     0x6a, 0xb6, 0x4a, 0x07, 0x57, 0x03, 0xac, 0xd9, 0x17, 0x61, 0xa2, 0x09, 0x46, 0x06, 0x6f, 0xb7],
    // Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY
    [0xb1, 0x4e, 0x0d, 0xe5, 0x5e, 0x9f, 0xba, 0x86, 0x39, 0x6e, 0xbf, 0xd5, 0x48, 0xcf, 0xf8, 0xc9,
     0x20, 0x11, 0xea, 0xc7, 0xb7, 0x5b, 0xaa, 0x9b, 0x2d, 0x9c, 0x6a, 0x86, 0xf5, 0xa1, 0x71, 0x41],
    // ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49
    [0x88, 0xf1, 0xff, 0xa3, 0xa2, 0xdf, 0xe6, 0x17, 0xbd, 0xc4, 0xe3, 0x57, 0x32, 0x51, 0xa3, 0x22,
     0xe3, 0xfc, 0xae, 0x81, 0xe5, 0xa4, 0x57, 0x39, 0x0e, 0x64, 0x75, 0x1c, 0x00, 0xa4, 0x65, 0xe2],
    // DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh
    [0xbc, 0x2b, 0x57, 0x06, 0x5e, 0xf1, 0xdd, 0x66, 0x54, 0x30, 0xbe, 0x60, 0x6b, 0xa6, 0x59, 0x6c,
     0x02, 0x95, 0x30, 0x1b, 0xad, 0xef, 0x8b, 0x5a, 0xfc, 0x41, 0x01, 0x41, 0x50, 0xf4, 0x12, 0x74],
    // ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt
    [0x89, 0x07, 0x7d, 0x55, 0xa5, 0xbb, 0x13, 0x30, 0x76, 0x3e, 0xb7, 0x67, 0xf5, 0x5e, 0xc0, 0x77,
     0xb4, 0x1a, 0x0d, 0x07, 0x5f, 0x7d, 0xe1, 0xd7, 0x3f, 0xba, 0xca, 0x3c, 0x63, 0xd5, 0x54, 0x71],
    // DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL
    [0xbf, 0x97, 0x1b, 0x59, 0x10, 0x8b, 0x5b, 0x85, 0xa0, 0x4f, 0xb0, 0x93, 0xf1, 0xe2, 0x1b, 0x4e,
     0x3f, 0xd4, 0xc4, 0xc8, 0xf4, 0x87, 0xdd, 0x09, 0xb9, 0x57, 0x52, 0x76, 0x9f, 0x0d, 0xd8, 0xc3],
    // 3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT
    [0x20, 0x26, 0x10, 0x1e, 0xc2, 0x03, 0x28, 0x96, 0x4a, 0x32, 0xab, 0xab, 0x13, 0x6c, 0x54, 0x05,
     0xb9, 0x1f, 0x3a, 0xe3, 0x8e, 0xe4, 0xf6, 0x4c, 0xb6, 0xbd, 0xe8, 0x79, 0xb8, 0x68, 0x38, 0xd2],
];

#[derive(clap::Parser)]
#[command(version, author, about)]
pub struct Opts {
    #[arg(long, short)]
    config: PathBuf,
}

// Track a transfer to a whitelisted address
#[derive(Debug)]
pub struct WhitelistedTransfer {
    pub to: String,
    pub lamports: u64,
}

// Combined output for all parsed instructions in a transaction
#[derive(Debug)]
pub struct PumpTxResult {
    pub signature: String,
    pub slot: u64,
    pub fee: u64,
    pub compute_units: Option<u64>,
    pub succeeded: bool,
    pub pump_fun_events: Vec<PumpFunEvent>,
    pub pump_amm_events: Vec<PumpAmmEvent>,
    // Track unparseable instructions (program was called but couldn't parse details)
    pub pump_fun_unparsed: u32,
    pub pump_amm_unparsed: u32,
    // Transfers to whitelisted addresses (top-level only)
    pub whitelisted_transfers: Vec<WhitelistedTransfer>,
    pub total_tip_lamports: u64,
}

#[derive(Debug)]
pub enum PumpFunEvent {
    Buy { mint: String, amount: u64, max_sol_cost: u64 },
    BuyExactSolIn { mint: String, spendable_sol_in: u64, min_tokens_out: u64 },
    Sell { mint: String, amount: u64, min_sol_output: u64 },
}

#[derive(Debug)]
pub enum PumpAmmEvent {
    Buy { base_mint: String, base_amount_out: u64, max_quote_amount_in: u64 },
    BuyExactQuoteIn { base_mint: String, spendable_quote_in: u64, min_base_amount_out: u64 },
    Sell { base_mint: String, base_amount_in: u64, min_quote_amount_out: u64 },
}

/// Parse a System Program transfer instruction
/// Returns (from, to, lamports) if this is a transfer instruction
fn parse_system_transfer(ix: &InstructionUpdate) -> Option<(Pubkey, Pubkey, u64)> {
    // System Program transfer instruction layout:
    // - 4 bytes: instruction discriminator (2 = Transfer)
    // - 8 bytes: lamports (u64 little-endian)
    if ix.data.len() < 12 {
        return None;
    }

    let discriminator = u32::from_le_bytes(ix.data[0..4].try_into().ok()?);
    if discriminator != 2 {
        return None; // Not a transfer instruction
    }

    let lamports = u64::from_le_bytes(ix.data[4..12].try_into().ok()?);

    // Transfer accounts: [from, to]
    if ix.accounts.len() < 2 {
        return None;
    }

    let from = ix.accounts[0];
    let to = ix.accounts[1];

    Some((from, to, lamports))
}

// Transaction-level parser
#[derive(Debug, Copy, Clone)]
pub struct PumpCombinedParser;

impl Parser for PumpCombinedParser {
    type Input = TransactionUpdate;
    type Output = PumpTxResult;

    fn id(&self) -> std::borrow::Cow<'static, str> {
        "PumpCombinedParser".into()
    }

    fn prefilter(&self) -> Prefilter {
        use std::collections::HashSet;

        // Create prefilter manually to set failed: None (include both success and failed txs)
        let mut accounts_include = HashSet::new();
        accounts_include.insert(pump::ID);
        accounts_include.insert(pump_amm::ID);

        Prefilter {
            account: None,
            transaction: Some(TransactionPrefilter {
                accounts_include,
                accounts_required: HashSet::new(),
                failed: None, // Include BOTH successful and failed transactions
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

        // Build whitelist HashSet for fast lookup
        let whitelist: HashSet<Pubkey> = WHITELISTED_TIP_ACCOUNTS
            .iter()
            .map(|&arr| KeyBytes(arr))
            .collect();

        let mut result = PumpTxResult {
            signature: bs58::encode(&shared.signature).into_string(),
            slot: shared.slot,
            fee: shared.fee,
            compute_units: shared.compute_units_consumed,
            succeeded: shared.err.is_none(),
            pump_fun_events: vec![],
            pump_amm_events: vec![],
            pump_fun_unparsed: 0,
            pump_amm_unparsed: 0,
            whitelisted_transfers: vec![],
            total_tip_lamports: 0,
        };

        // First pass: Check TOP-LEVEL instructions only for transfers to whitelisted addresses
        for ix in ixs.iter() {
            // Only check System Program transfers
            if ix.program == SYSTEM_PROGRAM_ID {
                if let Some((_from, to, lamports)) = parse_system_transfer(ix) {
                    if whitelist.contains(&to) {
                        result.whitelisted_transfers.push(WhitelistedTransfer {
                            to: bs58::encode(to).into_string(),
                            lamports,
                        });
                        result.total_tip_lamports += lamports;
                    }
                }
            }
        }

        // Second pass: Iterate ALL instructions (outer + inner) for pump parsing
        for ix in ixs.iter().flat_map(|i| i.visit_all()) {
            // Try PumpFun parser
            if ix.program == pump::ID {
                match pump::InstructionParser.parse(ix).await {
                    Ok(parsed) => match parsed {
                        pump::PumpInstruction::Buy { accounts, args } => {
                            result.pump_fun_events.push(PumpFunEvent::Buy {
                                mint: bs58::encode(accounts.mint).into_string(),
                                amount: args.amount,
                                max_sol_cost: args.max_sol_cost,
                            });
                        }
                        pump::PumpInstruction::BuyExactSolIn { accounts, args } => {
                            result.pump_fun_events.push(PumpFunEvent::BuyExactSolIn {
                                mint: bs58::encode(accounts.mint).into_string(),
                                spendable_sol_in: args.spendable_sol_in,
                                min_tokens_out: args.min_tokens_out,
                            });
                        }
                        pump::PumpInstruction::Sell { accounts, args } => {
                            result.pump_fun_events.push(PumpFunEvent::Sell {
                                mint: bs58::encode(accounts.mint).into_string(),
                                amount: args.amount,
                                min_sol_output: args.min_sol_output,
                            });
                        }
                        _ => {
                            // Other PumpFun instruction (create, migrate, etc.)
                            result.pump_fun_unparsed += 1;
                        }
                    },
                    Err(_) => {
                        // Couldn't parse - unknown discriminator or malformed data
                        result.pump_fun_unparsed += 1;
                    }
                }
            }

            // Try PumpAMM parser
            if ix.program == pump_amm::ID {
                match pump_amm::InstructionParser.parse(ix).await {
                    Ok(parsed) => match parsed {
                        pump_amm::PumpAmmInstruction::Buy { accounts, args } => {
                            result.pump_amm_events.push(PumpAmmEvent::Buy {
                                base_mint: bs58::encode(accounts.base_mint).into_string(),
                                base_amount_out: args.base_amount_out,
                                max_quote_amount_in: args.max_quote_amount_in,
                            });
                        }
                        pump_amm::PumpAmmInstruction::BuyExactQuoteIn { accounts, args } => {
                            result.pump_amm_events.push(PumpAmmEvent::BuyExactQuoteIn {
                                base_mint: bs58::encode(accounts.base_mint).into_string(),
                                spendable_quote_in: args.spendable_quote_in,
                                min_base_amount_out: args.min_base_amount_out,
                            });
                        }
                        pump_amm::PumpAmmInstruction::Sell { accounts, args } => {
                            result.pump_amm_events.push(PumpAmmEvent::Sell {
                                base_mint: bs58::encode(accounts.base_mint).into_string(),
                                base_amount_in: args.base_amount_in,
                                min_quote_amount_out: args.min_quote_amount_out,
                            });
                        }
                        _ => {
                            // Other PumpAMM instruction (deposit, withdraw, create_pool, etc.)
                            result.pump_amm_unparsed += 1;
                        }
                    },
                    Err(_) => {
                        // Couldn't parse - unknown discriminator or malformed data
                        result.pump_amm_unparsed += 1;
                    }
                }
            }
        }

        // Emit if we found any pump-related activity (parsed or unparsed) OR if the transaction failed
        let has_pump_activity = !result.pump_fun_events.is_empty()
            || !result.pump_amm_events.is_empty()
            || result.pump_fun_unparsed > 0
            || result.pump_amm_unparsed > 0;

        if !has_pump_activity && result.succeeded {
            return Err(ParseError::Filtered);
        }

        Ok(result)
    }
}

// Handler that prints the results
#[derive(Debug)]
pub struct PrintHandler;

impl yellowstone_vixen::Handler<PumpTxResult, TransactionUpdate> for PrintHandler {
    async fn handle(
        &self,
        value: &PumpTxResult,
        _raw: &TransactionUpdate,
    ) -> yellowstone_vixen::HandlerResult<()> {
        // Print PumpFun events
        for event in &value.pump_fun_events {
            match event {
                PumpFunEvent::Buy { mint, amount, max_sol_cost } => {
                    println!(
                        "PUMP FUN, BUY: token={} amount={} max_sol_cost={}",
                        mint, amount, max_sol_cost
                    );
                }
                PumpFunEvent::BuyExactSolIn { mint, spendable_sol_in, min_tokens_out } => {
                    println!(
                        "PUMP FUN, BUY_EXACT: token={} sol_in={} min_tokens={}",
                        mint, spendable_sol_in, min_tokens_out
                    );
                }
                PumpFunEvent::Sell { mint, amount, min_sol_output } => {
                    println!(
                        "PUMP FUN, SELL: token={} amount={} min_sol_output={}",
                        mint, amount, min_sol_output
                    );
                }
            }
        }

        // Print PumpAMM events
        for event in &value.pump_amm_events {
            match event {
                PumpAmmEvent::Buy { base_mint, base_amount_out, max_quote_amount_in } => {
                    println!(
                        "PUMP SWAP, BUY: token={} amount_out={} max_quote_in={}",
                        base_mint, base_amount_out, max_quote_amount_in
                    );
                }
                PumpAmmEvent::BuyExactQuoteIn { base_mint, spendable_quote_in, min_base_amount_out } => {
                    println!(
                        "PUMP SWAP, BUY_EXACT: token={} quote_in={} min_base_out={}",
                        base_mint, spendable_quote_in, min_base_amount_out
                    );
                }
                PumpAmmEvent::Sell { base_mint, base_amount_in, min_quote_amount_out } => {
                    println!(
                        "PUMP SWAP, SELL: token={} amount_in={} min_quote_out={}",
                        base_mint, base_amount_in, min_quote_amount_out
                    );
                }
            }
        }

        // Print unparsed instruction counts (other pump instructions like create, migrate, etc.)
        if value.pump_fun_unparsed > 0 {
            println!("PUMP FUN, OTHER: {} instruction(s)", value.pump_fun_unparsed);
        }
        if value.pump_amm_unparsed > 0 {
            println!("PUMP SWAP, OTHER: {} instruction(s)", value.pump_amm_unparsed);
        }

        // Print tip information if any transfers to whitelisted addresses
        if value.total_tip_lamports > 0 {
            let tip_sol = value.total_tip_lamports as f64 / 1_000_000_000.0;
            println!(
                "TIPS: {} transfer(s), total={} lamports ({:.9} SOL)",
                value.whitelisted_transfers.len(),
                value.total_tip_lamports,
                tip_sol
            );
        }

        // Print transaction summary
        let status = if value.succeeded { "SUCCESS" } else { "FAILED" };
        println!(
            "Signature: {}, gas spent: {}, status: {}",
            value.signature,
            value.compute_units.unwrap_or(0),
            status
        );
        println!("---");

        Ok(())
    }
}

fn main() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let Opts { config } = Opts::parse();
    let config = std::fs::read_to_string(config).expect("Error reading config file");
    let config = toml::from_str(&config).expect("Error parsing config");

    yellowstone_vixen::Runtime::<YellowstoneGrpcSource>::builder()
        .transaction(Pipeline::new(PumpCombinedParser, [PrintHandler]))
        .build(config)
        .run();
}
