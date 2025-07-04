use config::Config as ConfigBuilder;
use serde::Deserialize;
use std::env;
use std::path::Path;

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

#[derive(Debug, Clone, Deserialize)]
pub struct GridConfig {
    // Configuration for grid trading strategy
    // 交易参数 (Trading parameters)
    pub trading_asset: String,
    pub grid_count: u32,
    pub trade_amount: f64,
    pub max_position: f64,
    pub max_drawdown: f64,
    pub price_precision: u32,
    pub quantity_precision: u32,
    pub check_interval: u64,
    pub max_order_age_minutes: f64,       // 订单最大存活时间（分钟）
    pub order_status_check_interval: u64, // 订单状态检查间隔（秒）
    pub leverage: u32,

    // 网格策略参数 (Grid strategy parameters)
    pub min_grid_spacing: f64,
    pub max_grid_spacing: f64,
    pub grid_price_offset: f64,

    // 风险控制参数 (Risk control parameters)
    pub max_single_loss: f64,
    pub max_daily_loss: f64,
    pub trailing_stop_ratio: f64,     // 浮动止损比例，默认0.1（10%）
    pub margin_safety_threshold: f64, // 保证金安全阈值，默认0.3（30%）
    pub slippage_tolerance: f64,      // 滑点容忍度，默认0.001（0.1%）
    pub max_orders_per_batch: usize,  // 每批最大订单数，默认5
    pub order_batch_delay_ms: u64,    // 批次间延迟毫秒数，默认200ms
    pub max_holding_time: u64,
    pub history_length: usize,
    pub max_active_orders: usize,    // 每次最多挂单数量（买/卖各自）
    pub fee_rate: f64,               // 手续费率
    pub min_profit: f64,             // 最小盈利阈值
    pub margin_usage_threshold: f64, // 保证金使用率阈值，默认0.8（80%）
    pub order_update_threshold: f64, // 订单更新阈值（价格变化百分比），默认0.02（2%）
}

#[derive(Debug, Deserialize)]
pub struct AccountConfig {
    // Configuration for account credentials
    pub private_key: String,
    pub real_account_address: Option<String>,
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

    let mut config: AppConfig = settings.try_deserialize()?;
    // 优先从环境变量读取 private_key
    if let Ok(pk) = env::var("PRIVATE_KEY") {
        config.account.private_key = pk;
    }
    Ok(config)
}
