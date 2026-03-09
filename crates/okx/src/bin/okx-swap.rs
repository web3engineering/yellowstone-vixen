use clap::Parser;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::signature::{Keypair, Signer};
use tracing::info;
use yellowstone_vixen_okx::OkxClient;

#[derive(Parser, Debug)]
#[command(author, version, about = "OKX DEX Swap Tool for Solana", long_about = None)]
struct Args {
    /// Source token mint address
    #[arg(long)]
    from_mint: String,

    /// Destination token mint address
    #[arg(long)]
    to_mint: String,

    /// Amount in raw token units (e.g., 1000000 for 1 USDC with 6 decimals)
    #[arg(long)]
    amount: String,

    /// Slippage tolerance as percentage (e.g., "1.0" for 1%)
    #[arg(long, default_value = "1.0")]
    slippage: String,

    /// Private key in base58 format
    #[arg(long)]
    private_key: String,

    /// OKX API key (optional, for v6 API with authentication)
    #[arg(long)]
    okx_api_key: Option<String>,

    /// OKX API secret key (optional, for v6 API with authentication)
    #[arg(long)]
    okx_secret_key: Option<String>,

    /// OKX API passphrase (optional, for v6 API with authentication)
    #[arg(long)]
    okx_passphrase: Option<String>,

    /// Solana RPC URL
    #[arg(long, default_value = "https://api.mainnet-beta.solana.com")]
    rpc_url: String,

    /// OKX API base URL
    #[arg(long)]
    okx_base_url: Option<String>,

    /// Dry run - don't actually submit the transaction
    #[arg(long, default_value = "false")]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let args = Args::parse();

    info!("Starting OKX swap");
    info!("From: {}", args.from_mint);
    info!("To: {}", args.to_mint);
    info!("Amount: {}", args.amount);
    info!("Slippage: {}%", args.slippage);

    // Decode private key
    let keypair = Keypair::from_base58_string(&args.private_key);
    let wallet_address = keypair.pubkey().to_string();

    info!("Wallet address: {}", wallet_address);

    // Create OKX client
    let okx_client = match (args.okx_api_key, args.okx_secret_key, args.okx_passphrase) {
        (Some(key), Some(secret), Some(passphrase)) => {
            info!("Using authenticated OKX client (v6 API)");
            OkxClient::new(key, secret, passphrase, args.okx_base_url)
        }
        (Some(key), Some(secret), None) => {
            info!("Using authenticated OKX client (v6 API) with empty passphrase");
            OkxClient::new(key, secret, String::new(), args.okx_base_url)
        }
        _ => {
            info!("Using public OKX client (v5 API, no authentication)");
            OkxClient::new_public(args.okx_base_url)
        }
    };

    // Get swap transaction (using /swap-instruction endpoint which returns properly serialized tx)
    info!("Fetching swap instruction from OKX...");
    let mut transaction = okx_client
        .get_unsigned_transaction(
            &args.from_mint,
            &args.to_mint,
            &args.amount,
            &wallet_address,
            &args.slippage,
            &args.rpc_url,
        )
        .await?;

    info!("Transaction decoded successfully");

    if args.dry_run {
        info!("DRY RUN - Transaction not submitted");
        info!("Transaction: {:?}", transaction);
        return Ok(());
    }

    // Connect to RPC
    let rpc_client = RpcClient::new_with_commitment(args.rpc_url.clone(), CommitmentConfig::confirmed());

    // Sign transaction
    info!("Signing transaction...");

    // The transaction from OKX already has the recent blockhash, so we just need to sign it
    let serialized_message = transaction.message.serialize();
    let signature = keypair.sign_message(&serialized_message);
    transaction.signatures[0] = signature;

    info!("Transaction signed");

    // Send transaction
    info!("Submitting transaction to Solana...");
    let tx_signature = rpc_client.send_and_confirm_transaction(&transaction)?;

    info!("✅ Transaction submitted successfully!");
    info!("Signature: {}", tx_signature);
    info!(
        "Explorer: https://solscan.io/tx/{}",
        tx_signature
    );

    Ok(())
}
