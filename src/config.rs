// ========== DOSYA: sentinel-wallet/src/config.rs ==========

#[derive(Clone, Debug)]
pub struct WalletConfig {
    pub nats_url: String,
    pub initial_balance: f64,
}

impl WalletConfig {
    pub fn from_env() -> Self {
        Self {
            nats_url: std::env::var("NATS_URL")
                .unwrap_or_else(|_| "nats://localhost:4222".to_string()),
            initial_balance: std::env::var("INITIAL_BALANCE")
                .unwrap_or_else(|_| "50.0".to_string())
                .parse()
                .expect("ENV ERROR: INITIAL_BALANCE"),
        }
    }
}
