/// Simple example showing how to use the OKX client
///
/// Note: This example requires valid OKX API credentials with a passphrase.
/// To run: cargo run --example simple_swap
use yellowstone_vixen_okx::OkxClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    // Example: Create an authenticated client (v5/v6 API)
    let client = OkxClient::new(
        std::env::var("OKX_API_KEY")?,
        std::env::var("OKX_SECRET_KEY")?,
        std::env::var("OKX_PASSPHRASE")?,
        Some("https://web3.okx.com/api/v5/dex/aggregator".to_string()),
    );

    // Get swap instruction
    let response = client
        .get_swap_instruction(
            "So11111111111111111111111111111111111111112",   // Wrapped SOL
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
            "100000",                                         // 0.0001 SOL
            &std::env::var("WALLET_ADDRESS")?,               // Your wallet
            "1.0",                                            // 1% slippage
        )
        .await?;

    println!("Swap instruction response: {:#?}", response);

    if let Some(data) = response.data.first() {
        println!("Expected output amount: {}", data.router_result.to_token_amount);
        println!("Price impact: {}%", data.router_result.price_impact_percent);
    }

    Ok(())
}
