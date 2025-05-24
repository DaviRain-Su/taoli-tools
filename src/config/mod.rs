use serde::Deserialize;
use std::path::Path;
use config::Config as ConfigBuilder;

#[derive(Debug, Deserialize)]
pub struct SpotConfig {
    pub exchange1: String,
    pub exchange2: String,
    pub symbol: String,
}

#[derive(Debug, Deserialize)]
pub struct FuturesConfig {
    pub spot_exchange: String,
    pub futures_exchange: String,
    pub symbol: String,
}

#[derive(Debug, Deserialize)]
pub struct TriangleConfig {
    pub exchange: String,
    pub pair1: String,
    pub pair2: String,
    pub pair3: String,
}

#[derive(Debug, Deserialize)]
pub struct GridConfig {
    // 交易参数
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

    // 网格策略参数
    pub min_grid_spacing: f64,
    pub max_grid_spacing: f64,
    pub grid_price_offset: f64,

    // 风险控制参数
    pub max_single_loss: f64,
    pub max_daily_loss: f64,
    pub max_holding_time: u64,
    pub history_length: usize,
}

#[derive(Debug, Deserialize)]
pub struct AccountConfig {
    pub private_key: String,
}

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub spot: SpotConfig,
    pub futures: FuturesConfig,
    pub triangle: TriangleConfig,
    pub grid: GridConfig,
    pub account: AccountConfig,
}

pub fn load_config(config_path: &Path) -> Result<AppConfig, Box<dyn std::error::Error>> {
    let settings = ConfigBuilder::builder()
        .add_source(config::File::from(config_path))
        .build()?;

    let config: AppConfig = settings.try_deserialize()?;
    Ok(config)
} 