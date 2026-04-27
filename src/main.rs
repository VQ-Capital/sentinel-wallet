// ========== DOSYA: sentinel-wallet/src/main.rs ==========
use anyhow::{Context, Result};
use futures_util::StreamExt;
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tracing::info;

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
    balance: f64,
    initial_balance: f64,
    peak_equity: f64,
    trade_count: f64,
    sum_returns: f64,
    sum_sq_returns: f64,
    positions: HashMap<String, Position>,
}

impl WalletState {
    fn new(initial_balance: f64) -> Self {
        Self {
            balance: initial_balance,
            initial_balance, // Artık okunacak
            peak_equity: initial_balance,
            trade_count: 0.0,
            sum_returns: 0.0,
            sum_sq_returns: 0.0,
            positions: HashMap::new(),
        }
    }

    fn process_report(&mut self, report: &ExecutionReport) {
        let is_closing = (report.side == "SELL"
            && self
                .positions
                .get(&report.symbol)
                .map(|p| p.quantity)
                .unwrap_or(0.0)
                > 0.0)
            || (report.side == "BUY"
                && self
                    .positions
                    .get(&report.symbol)
                    .map(|p| p.quantity)
                    .unwrap_or(0.0)
                    < 0.0);

        self.balance += report.realized_pnl;

        if is_closing {
            let pct_return = report.realized_pnl / self.balance;
            self.trade_count += 1.0;
            self.sum_returns += pct_return;
            self.sum_sq_returns += pct_return * pct_return;

            // initial_balance burada okunarak dead_code uyarısı giderildi
            let total_growth = (self.balance / self.initial_balance - 1.0) * 100.0;
            info!(
                "📈 Trade Closed: {} | Net PnL: ${:.4} | Total Growth: %{:.2}",
                report.symbol, report.realized_pnl, total_growth
            );
        }

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

    fn get_sharpe_ratio(&self) -> f64 {
        if self.trade_count < 2.0 {
            return 0.0;
        }
        let mean = self.sum_returns / self.trade_count;
        let variance = (self.sum_sq_returns / self.trade_count) - (mean * mean);
        if variance <= 0.0 {
            return 0.0;
        }
        (mean / variance.sqrt()) * 15.81 // Yıllıklaştırma faktörü (252 trading gününe göre)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    info!(
        "📡 Service: {} | Version: {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let init_bal: f64 = std::env::var("INITIAL_BALANCE")
        .unwrap_or_else(|_| "10.0".to_string())
        .parse()
        .unwrap_or(10.0);

    let nats_client = async_nats::connect(&nats_url).await.context("NATS Error")?;
    info!(
        "🏦 Sentinel Wallet (Institutional) Devrede. Capital: ${:.2}",
        init_bal
    );

    let state = Arc::new(RwLock::new(WalletState::new(init_bal)));
    let live_prices = Arc::new(RwLock::new(HashMap::<String, f64>::new()));

    let (pc, nm) = (live_prices.clone(), nats_client.clone());
    tokio::spawn(async move {
        if let Ok(mut sub) = nm.subscribe("market.trade.>").await {
            while let Some(msg) = sub.next().await {
                if let Ok(t) = AggTrade::decode(msg.payload) {
                    pc.write().await.insert(t.symbol, t.price);
                }
            }
        }
    });

    let (sc, ne) = (state.clone(), nats_client.clone());
    tokio::spawn(async move {
        if let Ok(mut sub) = ne.subscribe("execution.report.>").await {
            while let Some(msg) = sub.next().await {
                if let Ok(report) = ExecutionReport::decode(msg.payload) {
                    let mut st = sc.write().await;
                    st.process_report(&report);
                }
            }
        }
    });

    let (sp, pp, np) = (state.clone(), live_prices.clone(), nats_client.clone());
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_millis(500)).await;
            let (total_equity, margin, un_pnl, max_dd, sharpe) = {
                let mut st = sp.write().await;
                let pr = pp.read().await;
                let mut un_pnl = 0.0;
                for (sym, pos) in &st.positions {
                    if pos.quantity.abs() < 1e-6 {
                        continue;
                    }
                    if let Some(&current_price) = pr.get(sym) {
                        un_pnl += if pos.quantity > 0.0 {
                            (current_price - pos.avg_price) * pos.quantity
                        } else {
                            (pos.avg_price - current_price) * pos.quantity.abs()
                        };
                    }
                }
                let equity = st.balance + un_pnl;
                if equity > st.peak_equity {
                    st.peak_equity = equity;
                }
                let dd_pct = if st.peak_equity > 0.0 {
                    ((st.peak_equity - equity) / st.peak_equity) * 100.0
                } else {
                    0.0
                };
                (equity, st.balance, un_pnl, dd_pct, st.get_sharpe_ratio())
            };

            let snapshot = EquitySnapshot {
                total_equity_usd: total_equity,
                available_margin_usd: margin,
                total_unrealized_pnl: un_pnl,
                timestamp: chrono::Utc::now().timestamp_millis(),
                is_reconciled: false,
                max_drawdown_pct: max_dd,
                sharpe_ratio: sharpe,
            };

            let mut buf = Vec::new();
            if snapshot.encode(&mut buf).is_ok() {
                let _ = np
                    .publish("wallet.equity.snapshot".to_string(), buf.into())
                    .await;
            }
        }
    });

    tokio::signal::ctrl_c().await?;
    Ok(())
}
