// ========== DOSYA: sentinel-wallet/src/main.rs ==========
use anyhow::{Context, Result};
use futures_util::StreamExt;
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

pub mod sentinel_protos {
    pub mod execution {
        include!(concat!(env!("OUT_DIR"), "/sentinel.execution.v1.rs"));
    }
    pub mod market {
        include!(concat!(env!("OUT_DIR"), "/sentinel.market.v1.rs"));
    }
    pub mod wallet {
        include!(concat!(env!("OUT_DIR"), "/sentinel.wallet.v1.rs"));
    }
}

use sentinel_protos::execution::ExecutionReport;
use sentinel_protos::market::AggTrade;
use sentinel_protos::wallet::EquitySnapshot;

#[derive(Clone, Default)]
struct Position {
    quantity: f64,
    avg_price: f64,
}

struct WalletState {
    balance: f64, // Realize edilmiş K/Z ve komisyonlar dahil net bakiye
    positions: HashMap<String, Position>,
}

impl WalletState {
    fn new(initial_balance: f64) -> Self {
        Self {
            balance: initial_balance,
            positions: HashMap::new(),
        }
    }

    fn process_report(&mut self, report: &ExecutionReport) {
        // Net bakiyeye etki
        self.balance += report.realized_pnl; // realized_pnl komisyon düşülmüş haliyle gelir

        let pos = self.positions.entry(report.symbol.clone()).or_default();

        if report.side == "SELL" && pos.quantity > 0.0 {
            let close_qty = report.quantity.min(pos.quantity);
            pos.quantity -= close_qty;
            if pos.quantity <= 0.000001 {
                pos.avg_price = 0.0;
            }
        } else if report.side == "BUY" && pos.quantity < 0.0 {
            let close_qty = report.quantity.min(pos.quantity.abs());
            pos.quantity += close_qty;
            if pos.quantity.abs() <= 0.000001 {
                pos.avg_price = 0.0;
            }
        } else {
            let new_qty = if report.side == "BUY" {
                pos.quantity + report.quantity
            } else {
                pos.quantity - report.quantity
            };
            let total_value =
                (pos.quantity.abs() * pos.avg_price) + (report.quantity * report.execution_price);
            pos.avg_price = total_value / new_qty.abs();
            pos.quantity = new_qty;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let init_bal: f64 = std::env::var("INITIAL_BALANCE")
        .unwrap_or_else(|_| "10.0".to_string())
        .parse()
        .unwrap_or(10.0);

    let nats_client = async_nats::connect(&nats_url)
        .await
        .context("CRITICAL: NATS bağlanılamadı")?;

    info!(
        "🏦 Sentinel Wallet (Merkezi Hazine & VCA) Devrede. Başlangıç Kasası: {:.2} USD",
        init_bal
    );

    let state = Arc::new(RwLock::new(WalletState::new(init_bal)));
    let live_prices = Arc::new(RwLock::new(HashMap::<String, f64>::new()));

    // 1. DİNLEYİCİ: Market Fiyatları (Unrealized PnL hesaplamak için)
    let prices_clone = live_prices.clone();
    let nats_sub_market = nats_client.clone();
    tokio::spawn(async move {
        match nats_sub_market.subscribe("market.trade.>").await {
            Ok(mut sub) => {
                while let Some(msg) = sub.next().await {
                    if let Ok(trade) = AggTrade::decode(msg.payload) {
                        let mut cache = prices_clone.write().await;
                        cache.insert(trade.symbol, trade.price);
                    }
                }
            }
            Err(e) => error!("Market data aboneliği kurulamadı: {}", e),
        }
    });

    // 2. DİNLEYİCİ: Execution Raporları (Realized PnL ve Pozisyonlar için)
    let state_clone = state.clone();
    let nats_sub_exec = nats_client.clone();
    tokio::spawn(async move {
        match nats_sub_exec.subscribe("execution.report.>").await {
            Ok(mut sub) => {
                while let Some(msg) = sub.next().await {
                    if let Ok(report) = ExecutionReport::decode(msg.payload) {
                        let mut st = state_clone.write().await;
                        st.process_report(&report);
                        info!(
                            "📘 Muhasebe Kaydı: {} | K/Z: {:.4}$ | Yeni Kasa: {:.4}$",
                            report.symbol, report.realized_pnl, st.balance
                        );
                    }
                }
            }
            Err(e) => error!("Execution report aboneliği kurulamadı: {}", e),
        }
    });

    // 3. YAYINCI (PUBLISHER): Equity Snapshot Yayını (Her 500ms)
    let state_pub = state.clone();
    let prices_pub = live_prices.clone();
    let nats_pub = nats_client.clone();

    tokio::spawn(async move {
        loop {
            sleep(Duration::from_millis(500)).await;

            let (total_equity, available_margin, unrealized_pnl) = {
                let st = state_pub.read().await;
                let pr = prices_pub.read().await;

                let mut un_pnl = 0.0;

                for (sym, pos) in &st.positions {
                    if pos.quantity.abs() < 0.000001 {
                        continue;
                    }
                    if let Some(&current_price) = pr.get(sym) {
                        if pos.quantity > 0.0 {
                            un_pnl += (current_price - pos.avg_price) * pos.quantity;
                        } else {
                            un_pnl += (pos.avg_price - current_price) * pos.quantity.abs();
                        }
                    }
                }

                let equity = st.balance + un_pnl;
                let margin = st.balance; // Şimdilik çapraz marjin varsayımı ile sadece net bakiyeyi kullandırıyoruz

                (equity, margin, un_pnl)
            };

            let snapshot = EquitySnapshot {
                total_equity_usd: total_equity,
                available_margin_usd: available_margin,
                total_unrealized_pnl: unrealized_pnl,
                timestamp: chrono::Utc::now().timestamp_millis(),
                is_reconciled: false, // Gerçek borsaya bağlanana kadar false (Local VCA Mode)
            };

            let mut buf = Vec::new();
            if snapshot.encode(&mut buf).is_ok() {
                if let Err(e) = nats_pub
                    .publish("wallet.equity.snapshot".to_string(), buf.into())
                    .await
                {
                    warn!("⚠️ Equity Snapshot yayını başarısız: {}", e);
                }
            }
        }
    });

    // Ana thread'i canlı tut
    tokio::signal::ctrl_c().await?;
    info!("🛑 Sentinel Wallet Kapatılıyor...");
    Ok(())
}
