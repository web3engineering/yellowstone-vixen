use base64::{
    engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD, URL_SAFE_NO_PAD},
    Engine,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use solana_client::nonblocking::rpc_client::RpcClient as NonblockingRpcClient;
use solana_sdk::{
    hash::Hash,
    instruction::Instruction,
    message::{v0, AddressLookupTableAccount, VersionedMessage},
    pubkey::Pubkey,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use thiserror::Error;

use bincode;

type HmacSha256 = Hmac<Sha256>;

#[derive(Error, Debug)]
pub enum OkxError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("API error: {code} - {msg}")]
    ApiError { code: String, msg: String },

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Base64 decode error: {0}")]
    Base64Error(#[from] base64::DecodeError),

    #[error("Transaction decode error: {0}")]
    TransactionError(#[from] bincode::Error),

    #[error("Empty response data")]
    EmptyResponse,

    #[error("Invalid response format: {0}")]
    InvalidResponse(String),
}

/// OKX DEX Aggregator client for Solana swaps
#[derive(Clone, Debug)]
pub struct OkxClient {
    client: reqwest::Client,
    api_key: Option<String>,
    secret_key: Option<String>,
    passphrase: Option<String>,
    base_url: String,
}

/// Response from OKX /swap endpoint
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapResponse {
    #[serde(deserialize_with = "deserialize_string_or_int")]
    pub code: String,
    pub msg: String,
    pub data: Vec<SwapData>,
}

fn deserialize_string_or_int<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct StringOrInt;

    impl<'de> Visitor<'de> for StringOrInt {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or integer")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.to_string())
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.to_string())
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.to_string())
        }
    }

    deserializer.deserialize_any(StringOrInt)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapData {
    pub tx: SwapTransactionData,
    #[serde(default)]
    pub router_result: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapTransactionData {
    /// Base64 encoded unsigned transaction
    pub data: String,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub min_receive_amount: Option<String>,
    #[serde(default)]
    pub gas: Option<String>,
}

/// Response from OKX swap-instruction endpoint (V6 API)
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionResponse {
    pub code: String,
    pub msg: String,
    pub data: SwapInstructionData, // V6: Object, not array
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInstructionData {
    pub tx: TransactionMetadata,
    pub router_result: RouterResult,
    #[serde(default)]
    pub address_lookup_table_account: Vec<String>,
    pub instruction_lists: Vec<InstructionData>,
    #[serde(default)]
    pub create_token_account_list: Vec<String>,
    #[serde(default)]
    pub wsol_rent_fee: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionMetadata {
    pub from: String,
    pub to: String,
    pub min_receive_amount: String,
    pub slippage_percent: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstructionData {
    pub data: String, // Base64 encoded instruction data
    pub accounts: Vec<AccountMeta>,
    pub program_id: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccountMeta {
    pub pubkey: String,
    pub is_signer: bool,
    pub is_writable: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterResult {
    pub from_token_amount: String,
    pub to_token_amount: String,
    #[serde(default)]
    pub route_type: Option<String>,
    #[serde(default)]
    pub percent_diff: Option<String>,
    pub price_impact_percent: String,
    #[serde(default)]
    pub trade_fee: Option<String>,
    #[serde(default)]
    pub estimate_gas_fee: Option<String>,
}

impl OkxClient {
    /// Create a new OKX client with authentication (v6 API)
    ///
    /// # Arguments
    /// * `api_key` - OKX API key
    /// * `secret_key` - OKX API secret key
    /// * `passphrase` - OKX API passphrase
    /// * `base_url` - Optional base URL (defaults to v6 production)
    pub fn new(
        api_key: String,
        secret_key: String,
        passphrase: String,
        base_url: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: Some(api_key),
            secret_key: Some(secret_key),
            passphrase: Some(passphrase),
            base_url: base_url.unwrap_or_else(|| {
                "https://web3.okx.com/api/v6/dex/aggregator".to_string()
            }),
        }
    }

    /// Create a new OKX client without authentication (v5 API)
    ///
    /// # Arguments
    /// * `base_url` - Optional base URL (defaults to v5 production)
    pub fn new_public(base_url: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: None,
            secret_key: None,
            passphrase: None,
            base_url: base_url.unwrap_or_else(|| {
                "https://www.okx.com/api/v5/dex/aggregator".to_string()
            }),
        }
    }

    /// Check if client is configured for authenticated requests
    fn is_authenticated(&self) -> bool {
        self.api_key.is_some() && self.secret_key.is_some()
    }

    /// Generate HMAC SHA256 signature for OKX API
    fn generate_signature(&self, timestamp: &str, method: &str, request_path: &str) -> Option<String> {
        let secret_key = self.secret_key.as_ref()?;
        let prehash = format!("{}{}{}", timestamp, method, request_path);
        let mut mac = HmacSha256::new_from_slice(secret_key.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(prehash.as_bytes());
        Some(BASE64.encode(mac.finalize().into_bytes()))
    }

    /// Get swap instruction for a token swap
    ///
    /// # Arguments
    /// * `from_token` - Source token mint address
    /// * `to_token` - Destination token mint address
    /// * `amount` - Amount in raw token units (including decimals, e.g., 1000000 for 1 USDC)
    /// * `user_wallet` - User's wallet address
    /// * `slippage_percent` - Slippage tolerance as a percentage string (e.g., "1.0")
    pub async fn get_swap_instruction(
        &self,
        from_token: &str,
        to_token: &str,
        amount: &str,
        user_wallet: &str,
        slippage_percent: &str,
    ) -> Result<SwapInstructionResponse, OkxError> {
        // Build query parameters
        let params = [
            ("chainIndex", "501"), // Solana chain ID
            ("fromTokenAddress", from_token),
            ("toTokenAddress", to_token),
            ("amount", amount),
            ("userWalletAddress", user_wallet),
            ("slippagePercent", slippage_percent),
        ];

        // Build query string
        let query_string = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("{}/swap-instruction?{}", self.base_url, query_string);

        // Build request
        let mut request = self.client.get(&url);

        // Add authentication headers if configured
        if self.is_authenticated() {
            let request_path = if self.base_url.contains("/v6/") {
                format!("/api/v6/dex/aggregator/swap-instruction?{}", query_string)
            } else {
                format!("/api/v5/dex/aggregator/swap-instruction?{}", query_string)
            };

            let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

            if let Some(signature) = self.generate_signature(&timestamp, "GET", &request_path) {
                if let Some(api_key) = &self.api_key {
                    request = request.header("OK-ACCESS-KEY", api_key);
                }
                request = request.header("OK-ACCESS-SIGN", &signature);
                if let Some(passphrase) = &self.passphrase {
                    request = request.header("OK-ACCESS-PASSPHRASE", passphrase);
                }
                request = request.header("OK-ACCESS-TIMESTAMP", &timestamp);
            }
        }

        request = request.header("Content-Type", "application/json");

        // Make request
        let response = request.send().await?;

        let status = response.status();
        let response_text = response.text().await?;

        if !status.is_success() {
            return Err(OkxError::InvalidResponse(format!(
                "HTTP {}: {}",
                status, response_text
            )));
        }

        let swap_response: SwapInstructionResponse = serde_json::from_str(&response_text)?;

        // Check API response code
        if swap_response.code != "0" {
            return Err(OkxError::ApiError {
                code: swap_response.code,
                msg: swap_response.msg,
            });
        }

        Ok(swap_response)
    }

    /// Get swap transaction (simpler endpoint, may not require auth)
    ///
    /// This uses the `/swap` endpoint which returns a pre-built transaction
    pub async fn get_swap(
        &self,
        from_token: &str,
        to_token: &str,
        amount: &str,
        user_wallet: &str,
        slippage_percent: &str,
    ) -> Result<SwapResponse, OkxError> {
        // Build query parameters - V6 API uses chainIndex and slippagePercent
        let params = if self.base_url.contains("/v6/") {
            vec![
                ("chainIndex", "501"),
                ("fromTokenAddress", from_token),
                ("toTokenAddress", to_token),
                ("amount", amount),
                ("userWalletAddress", user_wallet),
                ("slippagePercent", slippage_percent),
            ]
        } else {
            vec![
                ("chainId", "501"),
                ("fromTokenAddress", from_token),
                ("toTokenAddress", to_token),
                ("amount", amount),
                ("userWalletAddress", user_wallet),
                ("slippage", slippage_percent),
            ]
        };

        // Build query string
        let query_string = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");

        let url = format!("{}/swap?{}", self.base_url, query_string);

        // Build request (using GET for v5 API)
        let mut request = self.client.get(&url);

        // Add authentication headers if configured
        if self.is_authenticated() {
            let request_path = if self.base_url.contains("/v6/") {
                format!("/api/v6/dex/aggregator/swap?{}", query_string)
            } else {
                format!("/api/v5/dex/aggregator/swap?{}", query_string)
            };

            let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

            if let Some(signature) = self.generate_signature(&timestamp, "GET", &request_path) {
                if let Some(api_key) = &self.api_key {
                    request = request.header("OK-ACCESS-KEY", api_key);
                }
                request = request.header("OK-ACCESS-SIGN", &signature);
                if let Some(passphrase) = &self.passphrase {
                    request = request.header("OK-ACCESS-PASSPHRASE", passphrase);
                }
                request = request.header("OK-ACCESS-TIMESTAMP", &timestamp);
            }
        }

        request = request.header("Content-Type", "application/json");

        // Make request
        let response = request.send().await?;

        let status = response.status();
        let response_text = response.text().await?;

        if !status.is_success() {
            return Err(OkxError::InvalidResponse(format!(
                "HTTP {}: {}",
                status, response_text
            )));
        }

        let swap_response: SwapResponse = serde_json::from_str(&response_text)
            .map_err(|e| {
                OkxError::InvalidResponse(format!(
                    "Failed to parse response: {}. Raw response: {}",
                    e, response_text
                ))
            })?;

        // Check API response code
        if swap_response.code != "0" {
            return Err(OkxError::ApiError {
                code: swap_response.code,
                msg: swap_response.msg,
            });
        }

        Ok(swap_response)
    }

    /// Get swap instruction and build the transaction (V6 API)
    ///
    /// Returns the unsigned transaction ready to be signed.
    /// `rpc_url` is required to fetch Address Lookup Tables when the swap route uses them.
    pub async fn get_unsigned_transaction(
        &self,
        from_token: &str,
        to_token: &str,
        amount: &str,
        user_wallet: &str,
        slippage_percent: &str,
        rpc_url: &str,
    ) -> Result<VersionedTransaction, OkxError> {
        let response = self
            .get_swap_instruction(from_token, to_token, amount, user_wallet, slippage_percent)
            .await?;

        self.build_transaction_from_instructions(&response.data, user_wallet, rpc_url)
            .await
    }

    /// Build a VersionedTransaction from V6 API instruction data.
    /// Fetches Address Lookup Tables from RPC so the transaction stays within the 1232-byte limit.
    async fn build_transaction_from_instructions(
        &self,
        data: &SwapInstructionData,
        user_wallet: &str,
        rpc_url: &str,
    ) -> Result<VersionedTransaction, OkxError> {
        // Parse user wallet pubkey
        let payer = Pubkey::from_str(user_wallet)
            .map_err(|e| OkxError::InvalidResponse(format!("Invalid wallet address: {}", e)))?;

        // Convert instruction data to Solana Instructions
        let mut instructions = Vec::new();
        for inst_data in &data.instruction_lists {
            let ix_data = BASE64.decode(&inst_data.data)?;

            let program_id = Pubkey::from_str(&inst_data.program_id)
                .map_err(|e| OkxError::InvalidResponse(format!("Invalid program ID: {}", e)))?;

            let mut accounts = Vec::new();
            for acc in &inst_data.accounts {
                let pubkey = Pubkey::from_str(&acc.pubkey)
                    .map_err(|e| OkxError::InvalidResponse(format!("Invalid account pubkey: {}", e)))?;

                accounts.push(solana_sdk::instruction::AccountMeta {
                    pubkey,
                    is_signer: acc.is_signer,
                    is_writable: acc.is_writable,
                });
            }

            instructions.push(Instruction {
                program_id,
                accounts,
                data: ix_data,
            });
        }

        // Fetch Address Lookup Tables from RPC so accounts can be compressed as indices.
        // Without this the transaction exceeds Solana's 1232-byte limit for complex routes.
        let lookup_tables = if !data.address_lookup_table_account.is_empty() {
            let rpc = NonblockingRpcClient::new(rpc_url.to_string());
            let mut tables = Vec::new();
            for alt_address in &data.address_lookup_table_account {
                let alt_pubkey = Pubkey::from_str(alt_address)
                    .map_err(|e| OkxError::InvalidResponse(format!("Invalid ALT address: {}", e)))?;

                match rpc.get_account_data(&alt_pubkey).await {
                    Ok(account_data) => {
                        // Addresses are stored after the 56-byte metadata header, 32 bytes each
                        const META_SIZE: usize = 56;
                        if account_data.len() > META_SIZE {
                            let addresses: Vec<Pubkey> = account_data[META_SIZE..]
                                .chunks_exact(32)
                                .filter_map(|chunk| {
                                    <[u8; 32]>::try_from(chunk).ok().map(Pubkey::from)
                                })
                                .collect();
                            tables.push(AddressLookupTableAccount {
                                key: alt_pubkey,
                                addresses,
                            });
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch ALT {}: {}", alt_address, e);
                    }
                }
            }
            tables
        } else {
            vec![]
        };

        let message = v0::Message::try_compile(
            &payer,
            &instructions,
            &lookup_tables,
            Hash::default(), // Blockhash will be set by the caller
        )
        .map_err(|e| OkxError::InvalidResponse(format!("Failed to compile V0 message: {}", e)))?;

        let transaction = VersionedTransaction {
            signatures: vec![solana_sdk::signature::Signature::default()],
            message: VersionedMessage::V0(message),
        };

        Ok(transaction)
    }

    /// Get swap transaction using the simpler /swap endpoint
    ///
    /// Returns the unsigned transaction ready to be signed
    pub async fn get_unsigned_transaction_from_swap(
        &self,
        from_token: &str,
        to_token: &str,
        amount: &str,
        user_wallet: &str,
        slippage_percent: &str,
    ) -> Result<VersionedTransaction, OkxError> {
        let response = self
            .get_swap(from_token, to_token, amount, user_wallet, slippage_percent)
            .await?;

        let data = response.data.first().ok_or(OkxError::EmptyResponse)?;

        // V6 API uses base64 without padding - add padding if needed
        let mut base64_data = data.tx.data.clone();
        let padding_needed = (4 - (base64_data.len() % 4)) % 4;
        for _ in 0..padding_needed {
            base64_data.push('=');
        }

        // Try different base64 decodings
        let tx_bytes = BASE64
            .decode(&base64_data)
            .or_else(|_| URL_SAFE_NO_PAD.decode(&data.tx.data))
            .or_else(|_| STANDARD_NO_PAD.decode(&data.tx.data))?;

        // Deserialize transaction
        let transaction: VersionedTransaction = bincode::deserialize(&tx_bytes)?;

        Ok(transaction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = OkxClient::new(
            "test_key".to_string(),
            "test_secret".to_string(),
            "test_pass".to_string(),
            None,
        );
        assert_eq!(
            client.base_url,
            "https://web3.okx.com/api/v6/dex/aggregator"
        );
    }
}
