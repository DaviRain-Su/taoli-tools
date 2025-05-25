use serde::Deserialize;
use std::path::Path;
use config::Config as ConfigBuilder;

#[derive(Debug, Deserialize)]
pub struct SpotConfig {
    // Configuration for spot trading between two exchanges
    pub exchange1: String,
    pub exchange2: String,
    pub symbol: String,
}

#[derive(Debug, Deserialize)]
pub struct FuturesConfig {
    // Configuration for futures trading involving a spot and futures exchange
    pub spot_exchange: String,
    pub futures_exchange: String,
    pub symbol: String,
}

#[derive(Debug, Deserialize)]
pub struct TriangleConfig {
    // Configuration for triangular arbitrage within a single exchange
    pub exchange: String,
    pub pair1: String,
    pub pair2: String,
    pub pair3: String,
}

#[derive(Debug, Deserialize)]
pub struct GridConfig {
    // Configuration for grid trading strategy
    // 交易参数 (Trading parameters)
    pub trading_asset: String,
    pub total_capital: f64,
    pub grid_count: u32,
    pub trade_amount: f64,
    pub max_position: f64,
    pub max_drawdown: f64,
    pub price_precision: u32,
    pub quantity_precision: u32,
    pub check_interval: u64,
    pub leverage: u32,

    // 网格策略参数 (Grid strategy parameters)
    pub min_grid_spacing: f64,
    pub max_grid_spacing: f64,
    pub grid_price_offset: f64,

    // 风险控制参数 (Risk control parameters)
    pub max_single_loss: f64,
    pub max_daily_loss: f64,
    pub max_holding_time: u64,
    pub history_length: usize,
    pub max_active_orders: usize, // 每次最多挂单数量（买/卖各自）
}

#[derive(Debug, Deserialize)]
pub struct AccountConfig {
    // Configuration for account credentials
    pub private_key: String,
}

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    // Main application configuration encompassing all trading strategies and account settings
    pub spot: SpotConfig,
    pub futures: FuturesConfig,
    pub triangle: TriangleConfig,
    pub grid: GridConfig,
    pub account: AccountConfig,
}

pub fn load_config(config_path: &Path) -> Result<AppConfig, Box<dyn std::error::Error>> {
    // Load configuration from a file path and deserialize it into an AppConfig struct
    let settings = ConfigBuilder::builder()
        .add_source(config::File::from(config_path))
        .build()?;

    let config: AppConfig = settings.try_deserialize()?;
    Ok(config)
} 