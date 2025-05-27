use ethers::signers::{LocalWallet, Signer};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient, Message, Subscription, UserData,
};
use log::{error, info, warn};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::sleep;

#[derive(Error, Debug)]
pub enum GridStrategyError {
    #[error("配置错误: {0}")]
    ConfigError(String),

    #[error("钱包初始化失败: {0}")]
    WalletError(String),

    #[error("客户端初始化失败: {0}")]
    ClientError(String),

    #[error("订单操作失败: {0}")]
    OrderError(String),

    #[error("订阅失败: {0}")]
    SubscriptionError(String),

    #[error("价格解析失败: {0}")]
    PriceParseError(String),

    #[error("数量解析失败: {0}")]
    QuantityParseError(String),

    #[error("风险控制触发: {0}")]
    RiskControlTriggered(String),

    #[error("市场分析失败: {0}")]
    MarketAnalysisError(String),

    #[error("资金分配失败: {0}")]
    FundAllocationError(String),

    #[error("网格重平衡失败: {0}")]
    RebalanceError(String),

    #[error("止损执行失败: {0}")]
    StopLossError(String),

    #[error("保证金不足: {0}")]
    MarginInsufficient(String),

    #[error("网络连接失败: {0}")]
    NetworkError(String),
}

// 性能指标结构体
#[derive(Debug, Clone)]
struct PerformanceMetrics {
    total_trades: u32,
    winning_trades: u32,
    losing_trades: u32,
    win_rate: f64,
    total_profit: f64,
    max_drawdown: f64,
    sharpe_ratio: f64,
    profit_factor: f64,
    average_win: f64,
    average_loss: f64,
    largest_win: f64,
    largest_loss: f64,
}

// 性能记录结构体
#[derive(Debug, Clone)]
struct PerformanceRecord {
    timestamp: SystemTime,
    price: f64,
    action: String,
    profit: f64,
    total_capital: f64,
}

// 订单状态枚举
#[derive(Debug, Clone, PartialEq)]
enum OrderStatus {
    Pending,    // 待处理
    Active,     // 活跃
    Filled,     // 已成交
    Cancelled,  // 已取消
    Rejected,   // 被拒绝
    PartiallyFilled, // 部分成交
}

// 增强的订单信息结构体
#[derive(Debug, Clone)]
struct EnhancedOrderInfo {
    order_id: u64,
    price: f64,
    quantity: f64,
    filled_quantity: f64,
    cost_price: Option<f64>,
    potential_sell_price: Option<f64>,
    allocated_funds: f64,
    status: OrderStatus,
    created_time: SystemTime,
    last_update_time: SystemTime,
    retry_count: u32,
}

// 订单信息结构体
#[derive(Debug, Clone)]
struct OrderInfo {
    price: f64,
    quantity: f64,
    cost_price: Option<f64>,           // 对于卖单，记录对应的买入成本价
    potential_sell_price: Option<f64>, // 对于买单，记录潜在卖出价格
    allocated_funds: f64,              // 分配的资金
}

// 止损状态枚举
#[derive(Debug, Clone, PartialEq)]
enum StopLossStatus {
    Normal,          // 正常
    Monitoring,      // 监控中
    PartialExecuted, // 部分执行
    FullyExecuted,   // 完全执行
    Failed,          // 执行失败
    Disabled,        // 已禁用
}

impl StopLossStatus {
    fn as_str(&self) -> &'static str {
        match self {
            StopLossStatus::Normal => "正常",
            StopLossStatus::Monitoring => "监控中",
            StopLossStatus::PartialExecuted => "部分执行",
            StopLossStatus::FullyExecuted => "完全执行",
            StopLossStatus::Failed => "执行失败",
            StopLossStatus::Disabled => "已禁用",
        }
    }

    /// 获取英文名称
    fn as_english(&self) -> &'static str {
        match self {
            StopLossStatus::Normal => "Normal",
            StopLossStatus::Monitoring => "Monitoring",
            StopLossStatus::PartialExecuted => "Partial Executed",
            StopLossStatus::FullyExecuted => "Fully Executed",
            StopLossStatus::Failed => "Failed",
            StopLossStatus::Disabled => "Disabled",
        }
    }

    /// 判断是否为正常状态
    fn is_normal(&self) -> bool {
        matches!(self, StopLossStatus::Normal)
    }

    /// 判断是否正在监控
    fn is_monitoring(&self) -> bool {
        matches!(self, StopLossStatus::Monitoring)
    }

    /// 判断是否已执行（部分或完全）
    fn is_executed(&self) -> bool {
        matches!(
            self,
            StopLossStatus::PartialExecuted | StopLossStatus::FullyExecuted
        )
    }

    /// 判断是否执行失败
    fn is_failed(&self) -> bool {
        matches!(self, StopLossStatus::Failed)
    }

    /// 判断是否可以继续交易
    fn can_continue_trading(&self) -> bool {
        matches!(
            self,
            StopLossStatus::Normal | StopLossStatus::Monitoring | StopLossStatus::PartialExecuted
        )
    }
}

// 网格状态结构体
#[derive(Debug, Clone)]
struct GridState {
    total_capital: f64,
    available_funds: f64,
    position_quantity: f64,
    position_avg_price: f64,
    realized_profit: f64,
    highest_price_after_position: f64, // 持仓后最高价
    trailing_stop_price: f64,          // 浮动止损价
    stop_loss_status: StopLossStatus,  // 止损状态
    last_rebalance_time: SystemTime,
    historical_volatility: f64,
    performance_history: Vec<PerformanceRecord>, // 性能历史记录
    current_metrics: PerformanceMetrics,         // 当前性能指标
    last_margin_check: SystemTime,              // 上次保证金检查时间
    connection_retry_count: u32,                // 连接重试次数
    last_order_batch_time: SystemTime,          // 上次批量下单时间
}

// 市场趋势枚举
#[derive(Debug, Clone, PartialEq)]
enum MarketTrend {
    Upward,   // 上升
    Downward, // 下降
    Sideways, // 震荡
}

impl MarketTrend {
    fn as_str(&self) -> &'static str {
        match self {
            MarketTrend::Upward => "上升",
            MarketTrend::Downward => "下降",
            MarketTrend::Sideways => "震荡",
        }
    }

    /// 获取趋势的英文名称
    fn as_english(&self) -> &'static str {
        match self {
            MarketTrend::Upward => "Upward",
            MarketTrend::Downward => "Downward",
            MarketTrend::Sideways => "Sideways",
        }
    }

    /// 判断是否为上升趋势
    fn is_bullish(&self) -> bool {
        matches!(self, MarketTrend::Upward)
    }

    /// 判断是否为下降趋势
    fn is_bearish(&self) -> bool {
        matches!(self, MarketTrend::Downward)
    }

    /// 判断是否为震荡趋势
    fn is_sideways(&self) -> bool {
        matches!(self, MarketTrend::Sideways)
    }
}

// 市场分析结果
#[derive(Debug, Clone)]
struct MarketAnalysis {
    volatility: f64,
    trend: MarketTrend,
    rsi: f64,
    short_ma: f64,
    long_ma: f64,
    price_change_5min: f64, // 5分钟价格变化率
}

// 动态资金分配结果
#[derive(Debug, Clone)]
struct DynamicFundAllocation {
    buy_order_funds: f64,
    sell_order_funds: f64,
    buy_spacing_adjustment: f64,
    sell_spacing_adjustment: f64,
    position_ratio: f64,
}

// 止损动作枚举
#[derive(Debug, Clone, PartialEq)]
enum StopLossAction {
    Normal,      // 正常
    PartialStop, // 部分止损
    FullStop,    // 已止损
}

impl StopLossAction {
    fn as_str(&self) -> &'static str {
        match self {
            StopLossAction::Normal => "正常",
            StopLossAction::PartialStop => "部分止损",
            StopLossAction::FullStop => "已止损",
        }
    }

    /// 获取英文名称
    fn as_english(&self) -> &'static str {
        match self {
            StopLossAction::Normal => "Normal",
            StopLossAction::PartialStop => "Partial Stop",
            StopLossAction::FullStop => "Full Stop",
        }
    }

    /// 判断是否需要执行止损
    fn requires_action(&self) -> bool {
        !matches!(self, StopLossAction::Normal)
    }

    /// 判断是否为完全止损
    fn is_full_stop(&self) -> bool {
        matches!(self, StopLossAction::FullStop)
    }

    /// 判断是否为部分止损
    fn is_partial_stop(&self) -> bool {
        matches!(self, StopLossAction::PartialStop)
    }
}

// 止损检查结果
#[derive(Debug, Clone)]
struct StopLossResult {
    action: StopLossAction,
    reason: String,
    stop_quantity: f64,
}

// 格式化价格到指定精度
fn format_price(price: f64, precision: u32) -> f64 {
    let multiplier = 10.0_f64.powi(precision as i32);
    (price * multiplier).round() / multiplier
}

// 计算K线振幅
fn calculate_amplitude(klines: &[f64]) -> (f64, f64) {
    let mut positive_amplitudes = Vec::new();
    let mut negative_amplitudes = Vec::new();

    for i in 0..klines.len() - 1 {
        let change = (klines[i + 1] - klines[i]) / klines[i];
        if change > 0.0 {
            positive_amplitudes.push(change);
        } else {
            negative_amplitudes.push(change.abs());
        }
    }

    let avg_positive = if !positive_amplitudes.is_empty() {
        positive_amplitudes.iter().sum::<f64>() / positive_amplitudes.len() as f64
    } else {
        0.0
    };

    let avg_negative = if !negative_amplitudes.is_empty() {
        negative_amplitudes.iter().sum::<f64>() / negative_amplitudes.len() as f64
    } else {
        0.0
    };

    (avg_positive, avg_negative)
}

// 计算市场波动率
fn calculate_market_volatility(price_history: &[f64]) -> f64 {
    if price_history.len() < 2 {
        return 0.0;
    }

    let mut price_changes = Vec::new();
    for i in 1..price_history.len() {
        let change = (price_history[i] - price_history[i - 1]) / price_history[i - 1];
        price_changes.push(change);
    }

    if price_changes.is_empty() {
        return 0.0;
    }

    // 计算标准差
    let mean = price_changes.iter().sum::<f64>() / price_changes.len() as f64;
    let variance = price_changes
        .iter()
        .map(|x| (x - mean).powi(2))
        .sum::<f64>()
        / price_changes.len() as f64;

    variance.sqrt() * (price_history.len() as f64).sqrt()
}

// 计算移动平均线
fn calculate_moving_average(prices: &[f64], period: usize) -> f64 {
    if prices.len() < period {
        return prices.iter().sum::<f64>() / prices.len() as f64;
    }

    let start_index = prices.len() - period;
    prices[start_index..].iter().sum::<f64>() / period as f64
}

// 计算RSI指标
fn calculate_rsi(prices: &[f64], period: usize) -> f64 {
    if prices.len() < period + 1 {
        return 50.0; // 默认中性值
    }

    let mut gains = 0.0;
    let mut losses = 0.0;

    for i in (prices.len() - period)..prices.len() {
        let change = prices[i] - prices[i - 1];
        if change > 0.0 {
            gains += change;
        } else {
            losses += change.abs();
        }
    }

    if losses == 0.0 {
        return 100.0;
    }

    let rs = gains / losses;
    100.0 - (100.0 / (1.0 + rs))
}

// 分析市场趋势
fn analyze_market_trend(price_history: &[f64]) -> MarketAnalysis {
    if price_history.len() < 25 {
        return MarketAnalysis {
            volatility: 0.0,
            trend: MarketTrend::Sideways,
            rsi: 50.0,
            short_ma: price_history.last().copied().unwrap_or(0.0),
            long_ma: price_history.last().copied().unwrap_or(0.0),
            price_change_5min: 0.0,
        };
    }

    let volatility = calculate_market_volatility(price_history);
    let short_ma = calculate_moving_average(price_history, 7);
    let long_ma = calculate_moving_average(price_history, 25);
    let rsi = calculate_rsi(price_history, 14);

    // 计算5分钟价格变化（假设最后几个数据点代表最近5分钟）
    let price_change_5min = if price_history.len() >= 5 {
        let recent_price = price_history[price_history.len() - 1];
        let old_price = price_history[price_history.len() - 5];
        (recent_price - old_price) / old_price
    } else {
        0.0
    };

    // 判断趋势
    let trend = if short_ma > long_ma * 1.05 && rsi > 55.0 {
        MarketTrend::Upward
    } else if short_ma < long_ma * 0.95 && rsi < 45.0 {
        MarketTrend::Downward
    } else {
        MarketTrend::Sideways
    };

    MarketAnalysis {
        volatility,
        trend,
        rsi,
        short_ma,
        long_ma,
        price_change_5min,
    }
}

// 计算动态资金分配
fn calculate_dynamic_fund_allocation(
    grid_state: &GridState,
    current_price: f64,
    grid_config: &crate::config::GridConfig,
) -> DynamicFundAllocation {
    // 计算持仓比例
    let position_ratio = if grid_state.total_capital > 0.0 {
        (grid_state.position_quantity * current_price) / grid_state.total_capital
    } else {
        0.0
    };

    // 资金偏向系数：持仓越多，买单资金越少，卖单资金越多
    let buy_fund_bias = (1.0 - position_ratio * 2.0).max(0.2);
    let sell_fund_bias = (1.0 + position_ratio).min(2.0);

    // 根据价格位置动态调整网格密度
    let price_range = grid_config.max_grid_spacing - grid_config.min_grid_spacing;
    let price_position = if price_range > 0.0 {
        ((current_price - grid_config.min_grid_spacing) / price_range)
            .max(0.0)
            .min(1.0)
    } else {
        0.5
    };

    // 价格越低，买单间距越小；价格越高，卖单间距越小
    let buy_spacing_adjustment = 1.0 + (price_position * 0.5);
    let sell_spacing_adjustment = 1.0 + ((1.0 - price_position) * 0.5);

    // 计算动态单网格资金
    let base_fund_per_grid = grid_state.total_capital / grid_config.grid_count as f64 * 0.5; // 风险系数
    let buy_order_funds = base_fund_per_grid * buy_fund_bias;
    let sell_order_funds = base_fund_per_grid * sell_fund_bias;

    // 确保单个网格资金不超过可用资金的20%
    let max_single_grid_fund = grid_state.available_funds * 0.2;
    let buy_order_funds = buy_order_funds.min(max_single_grid_fund);

    DynamicFundAllocation {
        buy_order_funds,
        sell_order_funds,
        buy_spacing_adjustment,
        sell_spacing_adjustment,
        position_ratio,
    }
}

// 止损检查与执行
fn check_stop_loss(
    grid_state: &mut GridState,
    current_price: f64,
    grid_config: &crate::config::GridConfig,
    price_history: &[f64],
) -> StopLossResult {
    // 1. 总资产止损 - 使用配置的最大回撤参数
    let current_total_value =
        grid_state.available_funds + grid_state.position_quantity * current_price;
    let total_stop_threshold = grid_state.total_capital * (1.0 - grid_config.max_drawdown);

    if current_total_value < total_stop_threshold {
        warn!(
            "🚨 触发总资产止损 - 当前总资产: {:.2}, 止损阈值: {:.2}, 最大回撤: {:.1}%",
            current_total_value,
            total_stop_threshold,
            grid_config.max_drawdown * 100.0
        );

        return StopLossResult {
            action: StopLossAction::FullStop,
            reason: format!("总资产亏损超过{:.1}%", grid_config.max_drawdown * 100.0),
            stop_quantity: grid_state.position_quantity,
        };
    }

    // 2. 浮动止损 (Trailing Stop) - 使用配置的浮动止损比例
    if grid_state.position_quantity > 0.0 {
        let trailing_stop_multiplier = 1.0 - grid_config.trailing_stop_ratio;

        // 初始化最高价和止损价
        if grid_state.highest_price_after_position < grid_state.position_avg_price {
            grid_state.highest_price_after_position = grid_state.position_avg_price;
            grid_state.trailing_stop_price =
                grid_state.position_avg_price * trailing_stop_multiplier;
        }

        // 更新最高价和浮动止损价
        if current_price > grid_state.highest_price_after_position {
            grid_state.highest_price_after_position = current_price;
            grid_state.trailing_stop_price = current_price * trailing_stop_multiplier;
            info!(
                "📈 更新浮动止损 - 新最高价: {:.4}, 新止损价: {:.4}, 止损比例: {:.1}%",
                grid_state.highest_price_after_position,
                grid_state.trailing_stop_price,
                grid_config.trailing_stop_ratio * 100.0
            );
        }

        // 检查是否触发浮动止损
        if current_price < grid_state.trailing_stop_price {
            warn!(
                "🚨 触发浮动止损 - 当前价格: {:.4}, 止损价: {:.4}, 配置止损比例: {:.1}%",
                current_price,
                grid_state.trailing_stop_price,
                grid_config.trailing_stop_ratio * 100.0
            );

            // 根据配置的浮动止损比例动态调整止损数量
            let stop_ratio = (grid_config.trailing_stop_ratio * 5.0).min(0.8).max(0.3); // 30%-80%之间
            let stop_quantity = grid_state.position_quantity * stop_ratio;
            grid_state.highest_price_after_position = current_price;
            grid_state.trailing_stop_price = current_price * trailing_stop_multiplier;

            return StopLossResult {
                action: StopLossAction::PartialStop,
                reason: format!(
                    "触发浮动止损，回撤{:.1}%",
                    grid_config.trailing_stop_ratio * 100.0
                ),
                stop_quantity,
            };
        }
    }

    // 3. 单笔持仓止损 - 使用配置的最大单笔亏损参数
    if grid_state.position_quantity > 0.0 && grid_state.position_avg_price > 0.0 {
        let position_loss_rate =
            (current_price - grid_state.position_avg_price) / grid_state.position_avg_price;

        if position_loss_rate < -grid_config.max_single_loss {
            warn!("🚨 触发单笔持仓止损 - 持仓均价: {:.4}, 当前价格: {:.4}, 亏损率: {:.2}%, 配置阈值: {:.1}%", 
                grid_state.position_avg_price, current_price, position_loss_rate * 100.0, grid_config.max_single_loss * 100.0);

            // 根据亏损程度动态调整止损比例
            let loss_severity = position_loss_rate.abs() / grid_config.max_single_loss;
            let stop_ratio = (0.3 * loss_severity).min(0.8); // 最少30%，最多80%
            let stop_quantity = grid_state.position_quantity * stop_ratio;

            return StopLossResult {
                action: StopLossAction::PartialStop,
                reason: format!(
                    "单笔持仓亏损超过{:.1}%",
                    grid_config.max_single_loss * 100.0
                ),
                stop_quantity,
            };
        }
    }

    // 4. 加速下跌止损 - 基于每日最大亏损参数的动态阈值
    if price_history.len() >= 5 {
        let recent_price = price_history[price_history.len() - 1];
        let old_price = price_history[price_history.len() - 5];
        let short_term_change = (recent_price - old_price) / old_price;

        // 使用每日最大亏损的一半作为短期下跌阈值
        let rapid_decline_threshold = -(grid_config.max_daily_loss * 0.5);

        if short_term_change < rapid_decline_threshold && grid_state.position_quantity > 0.0 {
            warn!(
                "🚨 触发加速下跌止损 - 5分钟价格变化率: {:.2}%, 阈值: {:.2}%",
                short_term_change * 100.0,
                rapid_decline_threshold * 100.0
            );

            // 根据下跌幅度和配置的每日最大亏损动态计算止损比例
            let decline_severity = short_term_change.abs() / grid_config.max_daily_loss;
            let stop_ratio = (0.2 + decline_severity * 0.3).min(0.6); // 20%-60%之间
            let stop_quantity = grid_state.position_quantity * stop_ratio;

            return StopLossResult {
                action: StopLossAction::PartialStop,
                reason: format!(
                    "加速下跌{:.1}%，超过阈值{:.1}%",
                    short_term_change.abs() * 100.0,
                    rapid_decline_threshold.abs() * 100.0
                ),
                stop_quantity,
            };
        }
    }

    StopLossResult {
        action: StopLossAction::Normal,
        reason: "".to_string(),
        stop_quantity: 0.0,
    }
}

// 计算考虑手续费后的最小卖出价格
fn calculate_min_sell_price(buy_price: f64, fee_rate: f64, min_profit_rate: f64) -> f64 {
    let buy_cost = buy_price * (1.0 + fee_rate);
    buy_cost * (1.0 + min_profit_rate) / (1.0 - fee_rate)
}

// 计算预期利润率
fn calculate_expected_profit_rate(buy_price: f64, sell_price: f64, fee_rate: f64) -> f64 {
    let buy_cost = buy_price * (1.0 + fee_rate);
    let sell_revenue = sell_price * (1.0 - fee_rate);
    (sell_revenue - buy_cost) / buy_cost
}

// 验证网格配置参数
fn validate_grid_config(grid_config: &crate::config::GridConfig) -> Result<(), GridStrategyError> {
    // 检查基本参数
    if grid_config.total_capital <= 0.0 {
        return Err(GridStrategyError::ConfigError(
            "总资金必须大于0".to_string(),
        ));
    }

    if grid_config.trade_amount <= 0.0 {
        return Err(GridStrategyError::ConfigError(
            "每格交易金额必须大于0".to_string(),
        ));
    }

    if grid_config.trade_amount > grid_config.total_capital {
        return Err(GridStrategyError::ConfigError(
            "每格交易金额不能超过总资金".to_string(),
        ));
    }

    if grid_config.max_position <= 0.0 {
        return Err(GridStrategyError::ConfigError(
            "最大持仓必须大于0".to_string(),
        ));
    }

    if grid_config.grid_count == 0 {
        return Err(GridStrategyError::ConfigError(
            "网格数量必须大于0".to_string(),
        ));
    }

    // 检查网格间距
    if grid_config.min_grid_spacing <= 0.0 {
        return Err(GridStrategyError::ConfigError(
            "最小网格间距必须大于0".to_string(),
        ));
    }

    if grid_config.max_grid_spacing <= grid_config.min_grid_spacing {
        return Err(GridStrategyError::ConfigError(
            "最大网格间距必须大于最小网格间距".to_string(),
        ));
    }

    // 检查手续费率
    if grid_config.fee_rate < 0.0 || grid_config.fee_rate > 0.1 {
        return Err(GridStrategyError::ConfigError(
            "手续费率必须在0-10%之间".to_string(),
        ));
    }

    // 检查网格间距是否足够覆盖手续费
    let min_required_spacing = grid_config.fee_rate * 2.5; // 至少是手续费的2.5倍
    if grid_config.min_grid_spacing < min_required_spacing {
        return Err(GridStrategyError::ConfigError(format!(
            "最小网格间距({:.4}%)过小，无法覆盖手续费成本，建议至少设置为{:.4}%",
            grid_config.min_grid_spacing * 100.0,
            min_required_spacing * 100.0
        )));
    }

    // 检查风险控制参数
    if grid_config.max_drawdown <= 0.0 || grid_config.max_drawdown > 1.0 {
        return Err(GridStrategyError::ConfigError(
            "最大回撤必须在0-100%之间".to_string(),
        ));
    }

    if grid_config.max_single_loss <= 0.0 || grid_config.max_single_loss > 1.0 {
        return Err(GridStrategyError::ConfigError(
            "单笔最大亏损必须在0-100%之间".to_string(),
        ));
    }

    if grid_config.max_daily_loss <= 0.0 || grid_config.max_daily_loss > 1.0 {
        return Err(GridStrategyError::ConfigError(
            "每日最大亏损必须在0-100%之间".to_string(),
        ));
    }

    if grid_config.trailing_stop_ratio <= 0.0 || grid_config.trailing_stop_ratio > 0.5 {
        return Err(GridStrategyError::ConfigError(
            "浮动止损比例必须在0-50%之间".to_string(),
        ));
    }

    // 检查杠杆倍数
    if grid_config.leverage == 0 || grid_config.leverage > 100 {
        return Err(GridStrategyError::ConfigError(
            "杠杆倍数必须在1-100之间".to_string(),
        ));
    }

    // 检查精度设置
    if grid_config.price_precision > 8 {
        return Err(GridStrategyError::ConfigError(
            "价格精度不能超过8位小数".to_string(),
        ));
    }

    if grid_config.quantity_precision > 8 {
        return Err(GridStrategyError::ConfigError(
            "数量精度不能超过8位小数".to_string(),
        ));
    }

    // 检查时间参数
    if grid_config.check_interval == 0 {
        return Err(GridStrategyError::ConfigError(
            "检查间隔必须大于0秒".to_string(),
        ));
    }

    if grid_config.max_holding_time == 0 {
        return Err(GridStrategyError::ConfigError(
            "最大持仓时间必须大于0秒".to_string(),
        ));
    }

    // 检查保证金使用率
    if grid_config.margin_usage_threshold <= 0.0 || grid_config.margin_usage_threshold > 1.0 {
        return Err(GridStrategyError::ConfigError(
            "保证金使用率阈值必须在0-100%之间".to_string(),
        ));
    }

    info!("✅ 网格配置验证通过");
    Ok(())
}

// 处理买单成交
async fn handle_buy_fill(
    exchange_client: &ExchangeClient,
    grid_config: &crate::config::GridConfig,
    fill_price: f64,
    fill_size: f64,
    grid_spacing: f64,
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
) -> Result<(), GridStrategyError> {
    info!("🟢 处理买单成交: 价格={}, 数量={}", fill_price, fill_size);

    // 计算基础卖出价格
    let base_sell_price = fill_price * (1.0 + grid_spacing);

    // 计算考虑手续费和最小利润的实际卖出价格
    let min_sell_price = calculate_min_sell_price(
        fill_price,
        grid_config.fee_rate,
        grid_config.min_profit / fill_price,
    );
    let actual_sell_price = base_sell_price.max(min_sell_price);
    let formatted_sell_price = format_price(actual_sell_price, grid_config.price_precision);

    // 检查是否超出网格上限
    let upper_limit =
        fill_price * (1.0 + grid_config.max_grid_spacing * grid_config.grid_count as f64);
    if formatted_sell_price > upper_limit {
        warn!(
            "⚠️ 卖出价格({:.4})超出网格上限({:.4})，可能影响网格完整性",
            formatted_sell_price, upper_limit
        );
    }

    // 考虑买入时的手续费损失，调整卖出数量
    let sell_quantity = format_price(
        fill_size * (1.0 - grid_config.fee_rate),
        grid_config.quantity_precision,
    );

    // 创建卖单
    let sell_order = ClientOrderRequest {
        asset: grid_config.trading_asset.clone(),
        is_buy: false,
        reduce_only: false,
        limit_px: formatted_sell_price,
        sz: sell_quantity,
        cloid: None,
        order_type: ClientOrder::Limit(ClientLimit {
            tif: "Gtc".to_string(),
        }),
    };

    match exchange_client.order(sell_order, None).await {
        Ok(ExchangeResponseStatus::Ok(response)) => {
            if let Some(data) = response.data {
                if !data.statuses.is_empty() {
                    if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                        info!(
                            "🔴【对冲卖单】✅ 卖单已提交: ID={}, 价格={}, 数量={}, 成本价={}",
                            order.oid, formatted_sell_price, sell_quantity, fill_price
                        );
                        active_orders.push(order.oid);
                        sell_orders.insert(
                            order.oid,
                            OrderInfo {
                                price: formatted_sell_price,
                                quantity: sell_quantity,
                                cost_price: Some(fill_price),
                                potential_sell_price: None,
                                allocated_funds: 0.0,
                            },
                        );
                    }
                }
            }
        }
        Ok(ExchangeResponseStatus::Err(e)) => warn!("❌ 对冲卖单失败: {:?}", e),
        Err(e) => warn!("❌ 对冲卖单失败: {:?}", e),
    }

    // 在相同价格重新创建买单
    let new_buy_order = ClientOrderRequest {
        asset: grid_config.trading_asset.clone(),
        is_buy: true,
        reduce_only: false,
        limit_px: fill_price,
        sz: fill_size,
        cloid: None,
        order_type: ClientOrder::Limit(ClientLimit {
            tif: "Gtc".to_string(),
        }),
    };

    match exchange_client.order(new_buy_order, None).await {
        Ok(ExchangeResponseStatus::Ok(response)) => {
            if let Some(data) = response.data {
                if !data.statuses.is_empty() {
                    if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                        info!(
                            "🟢【重建买单】✅ 买单已提交: ID={}, 价格={}, 数量={}",
                            order.oid, fill_price, fill_size
                        );
                        active_orders.push(order.oid);
                        buy_orders.insert(
                            order.oid,
                            OrderInfo {
                                price: fill_price,
                                quantity: fill_size,
                                cost_price: None,
                                potential_sell_price: None,
                                allocated_funds: 0.0,
                            },
                        );
                    }
                }
            }
        }
        Ok(ExchangeResponseStatus::Err(e)) => warn!("❌ 重建买单失败: {:?}", e),
        Err(e) => warn!("❌ 重建买单失败: {:?}", e),
    }

    Ok(())
}

// 处理卖单成交
async fn handle_sell_fill(
    exchange_client: &ExchangeClient,
    grid_config: &crate::config::GridConfig,
    fill_price: f64,
    fill_size: f64,
    cost_price: Option<f64>,
    grid_spacing: f64,
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
) -> Result<(), GridStrategyError> {
    info!(
        "🔴 处理卖单成交: 价格={}, 数量={}, 成本价={:?}",
        fill_price, fill_size, cost_price
    );

    // 计算实际利润
    let actual_cost_price = cost_price.unwrap_or_else(|| {
        let estimated = fill_price - grid_spacing * fill_price;
        warn!("⚠️ 未找到成本价，估算为: {:.4}", estimated);
        estimated
    });

    let actual_profit_rate =
        calculate_expected_profit_rate(actual_cost_price, fill_price, grid_config.fee_rate);

    info!(
        "💰 交易完成 - 成本价: {:.4}, 卖出价: {:.4}, 利润率: {:.4}%",
        actual_cost_price,
        fill_price,
        actual_profit_rate * 100.0
    );

    // 计算潜在买入价格
    let base_buy_price = fill_price * (1.0 - grid_spacing);
    let formatted_buy_price = format_price(base_buy_price, grid_config.price_precision);

    // 检查新买入点的预期利润率
    let potential_sell_price = formatted_buy_price * (1.0 + grid_spacing);
    let expected_profit_rate = calculate_expected_profit_rate(
        formatted_buy_price,
        potential_sell_price,
        grid_config.fee_rate,
    );
    let min_profit_rate = grid_config.min_profit
        / (formatted_buy_price * grid_config.trade_amount / formatted_buy_price);

    if expected_profit_rate >= min_profit_rate {
        let buy_quantity = format_price(
            grid_config.trade_amount / formatted_buy_price,
            grid_config.quantity_precision,
        );

        // 创建新买单
        let new_buy_order = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: true,
            reduce_only: false,
            limit_px: formatted_buy_price,
            sz: buy_quantity,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Gtc".to_string(),
            }),
        };

        match exchange_client.order(new_buy_order, None).await {
            Ok(ExchangeResponseStatus::Ok(response)) => {
                if let Some(data) = response.data {
                    if !data.statuses.is_empty() {
                        if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                            info!("🟢【新买单】✅ 买单已提交: ID={}, 价格={}, 数量={}, 预期利润率={:.4}%", 
                                order.oid, formatted_buy_price, buy_quantity, expected_profit_rate * 100.0);
                            active_orders.push(order.oid);
                            buy_orders.insert(
                                order.oid,
                                OrderInfo {
                                    price: formatted_buy_price,
                                    quantity: buy_quantity,
                                    cost_price: None,
                                    potential_sell_price: None,
                                    allocated_funds: 0.0,
                                },
                            );
                        }
                    }
                }
            }
            Ok(ExchangeResponseStatus::Err(e)) => warn!("❌ 新买单失败: {:?}", e),
            Err(e) => warn!("❌ 新买单失败: {:?}", e),
        }
    } else {
        warn!(
            "⚠️ 网格点 {:.4} 的预期利润率({:.4}%)不满足最小要求({:.4}%)，跳过此买单",
            formatted_buy_price,
            expected_profit_rate * 100.0,
            min_profit_rate * 100.0
        );
    }

    // 根据策略决定是否在相同价格再次创建卖单
    // 检查是否有足够的资产和是否应该在相同价格创建卖单
    let should_recreate_sell = actual_profit_rate > 0.0; // 只有盈利的情况下才重建卖单

    if should_recreate_sell {
        // 在相同价格重新创建卖单
        let new_sell_order = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: false,
            reduce_only: false,
            limit_px: fill_price,
            sz: fill_size,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Gtc".to_string(),
            }),
        };

        match exchange_client.order(new_sell_order, None).await {
            Ok(ExchangeResponseStatus::Ok(response)) => {
                if let Some(data) = response.data {
                    if !data.statuses.is_empty() {
                        if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                            info!(
                                "🔴【重建卖单】✅ 卖单已提交: ID={}, 价格={}, 数量={}",
                                order.oid, fill_price, fill_size
                            );
                            active_orders.push(order.oid);
                            // 估算新卖单的成本价（当前价格减去网格间距）
                            let estimated_cost_price = fill_price * (1.0 - grid_spacing);
                            sell_orders.insert(
                                order.oid,
                                OrderInfo {
                                    price: fill_price,
                                    quantity: fill_size,
                                    cost_price: Some(estimated_cost_price),
                                    potential_sell_price: None,
                                    allocated_funds: 0.0,
                                },
                            );
                        }
                    }
                }
            }
            Ok(ExchangeResponseStatus::Err(e)) => warn!("❌ 重建卖单失败: {:?}", e),
            Err(e) => warn!("❌ 重建卖单失败: {:?}", e),
        }
    } else {
        info!("📊 利润率不足或策略不建议重建卖单，跳过重建");
    }

    Ok(())
}

// 清仓函数
async fn close_all_positions(
    exchange_client: &ExchangeClient,
    grid_config: &crate::config::GridConfig,
    long_position: f64,
    short_position: f64,
    current_price: f64,
) -> Result<(), GridStrategyError> {
    if long_position > 0.0 {
        // 多头清仓：卖出时考虑向下滑点
        let sell_price = current_price * (1.0 - grid_config.slippage_tolerance);
        let order = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: false,
            reduce_only: true,
            limit_px: sell_price,
            sz: long_position,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Ioc".to_string(), // 使用IOC确保快速成交
            }),
        };
        info!("🔄 清仓多头 - 数量: {:.4}, 价格: {:.4} (含滑点: {:.2}%)", 
            long_position, sell_price, grid_config.slippage_tolerance * 100.0);
        if let Err(e) = exchange_client.order(order, None).await {
            return Err(GridStrategyError::OrderError(format!(
                "清仓多头失败: {:?}",
                e
            )));
        }
    }
    
    if short_position > 0.0 {
        // 空头清仓：买入时考虑向上滑点
        let buy_price = current_price * (1.0 + grid_config.slippage_tolerance);
        let order = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: true,
            reduce_only: true,
            limit_px: buy_price,
            sz: short_position,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Ioc".to_string(), // 使用IOC确保快速成交
            }),
        };
        info!("🔄 清仓空头 - 数量: {:.4}, 价格: {:.4} (含滑点: {:.2}%)", 
            short_position, buy_price, grid_config.slippage_tolerance * 100.0);
        if let Err(e) = exchange_client.order(order, None).await {
            return Err(GridStrategyError::OrderError(format!(
                "清仓空头失败: {:?}",
                e
            )));
        }
    }
    
    Ok(())
}

// 查询账户信息
async fn get_account_info(
    info_client: &InfoClient,
    user_address: ethers::types::Address,
) -> Result<hyperliquid_rust_sdk::UserStateResponse, GridStrategyError> {
    info_client
        .user_state(user_address)
        .await
        .map_err(|e| GridStrategyError::ClientError(format!("获取账户信息失败: {:?}", e)))
}

// 创建动态网格
async fn create_dynamic_grid(
    exchange_client: &ExchangeClient,
    grid_config: &crate::config::GridConfig,
    grid_state: &mut GridState,
    current_price: f64,
    price_history: &[f64],
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
) -> Result<(), GridStrategyError> {
    info!("🔄 开始创建动态网格...");

    // 获取动态资金分配
    let mut fund_allocation =
        calculate_dynamic_fund_allocation(grid_state, current_price, grid_config);

    // 使用振幅计算调整网格间距
    let amplitude_adjustment = if price_history.len() >= 10 {
        // 有足够的价格历史数据，使用振幅计算
        let (avg_up, avg_down) = calculate_amplitude(price_history);
        let market_volatility = (avg_up + avg_down) / 2.0;
        (1.0 + market_volatility * 2.0).max(0.5).min(2.0)
    } else if grid_state.historical_volatility > 0.0 {
        // 使用历史波动率作为振幅调整因子
        (grid_state.historical_volatility * 10.0).max(0.5).min(2.0)
    } else {
        1.0 // 默认不调整
    };

    // 应用振幅调整到间距
    fund_allocation.buy_spacing_adjustment *= amplitude_adjustment;
    fund_allocation.sell_spacing_adjustment *= amplitude_adjustment;

    info!(
        "💰 资金分配 - 买单资金: {:.2}, 卖单资金: {:.2}, 持仓比例: {:.2}%, 振幅调整: {:.2}",
        fund_allocation.buy_order_funds,
        fund_allocation.sell_order_funds,
        fund_allocation.position_ratio * 100.0,
        amplitude_adjustment
    );

    // 创建买单 - 价格递减
    let mut current_buy_price = current_price;
    let max_buy_funds = grid_state.available_funds * 0.7; // 最多使用70%资金做买单
    let mut allocated_buy_funds = 0.0;
    let mut buy_count = 0;
    
    // 收集要批量创建的买单
    let mut pending_buy_orders: Vec<ClientOrderRequest> = Vec::new();
    let mut pending_buy_order_info: Vec<OrderInfo> = Vec::new();

    while current_buy_price > current_price * 0.8
        && allocated_buy_funds < max_buy_funds
        && buy_count < grid_config.grid_count
    {
        // 动态计算网格间距，应用振幅调整
        let dynamic_spacing = grid_config.min_grid_spacing
            * fund_allocation.buy_spacing_adjustment
            * amplitude_adjustment;
        current_buy_price = current_buy_price - (current_buy_price * dynamic_spacing);

        // 计算当前网格资金
        let mut current_grid_funds = fund_allocation.buy_order_funds
            * (1.0 - (current_price - current_buy_price) / current_price * 3.0);
        current_grid_funds = current_grid_funds.max(fund_allocation.buy_order_funds * 0.5);

        // 检查资金限制
        if allocated_buy_funds + current_grid_funds > max_buy_funds {
            current_grid_funds = max_buy_funds - allocated_buy_funds;
        }

        if current_grid_funds < fund_allocation.buy_order_funds * 0.1 {
            break; // 资金太少，停止创建买单
        }

        let buy_quantity = format_price(
            current_grid_funds / current_buy_price,
            grid_config.quantity_precision,
        );

        // 验证潜在利润
        let potential_sell_price = current_buy_price * (1.0 + dynamic_spacing);
        let expected_profit_rate = calculate_expected_profit_rate(
            current_buy_price,
            potential_sell_price,
            grid_config.fee_rate,
        );

        if expected_profit_rate >= grid_config.min_profit / current_buy_price {
            let formatted_price = format_price(current_buy_price, grid_config.price_precision);

            let buy_order = ClientOrderRequest {
                asset: grid_config.trading_asset.clone(),
                is_buy: true,
                reduce_only: false,
                limit_px: formatted_price,
                sz: buy_quantity,
                cloid: None,
                order_type: ClientOrder::Limit(ClientLimit {
                    tif: "Gtc".to_string(),
                }),
            };

            // 收集订单信息，准备批量创建
            pending_buy_orders.push(buy_order);
            pending_buy_order_info.push(OrderInfo {
                price: formatted_price,
                quantity: buy_quantity,
                cost_price: None,
                potential_sell_price: Some(potential_sell_price),
                allocated_funds: current_grid_funds,
            });
            
            allocated_buy_funds += current_grid_funds;
            buy_count += 1;
        }
    }

    // 增强版批量创建买单 - 包含资源管理和错误恢复
    if !pending_buy_orders.is_empty() {
        let order_count = pending_buy_orders.len();
        info!("📦 开始增强批量创建{}个买单", order_count);
        
        // 资源预检查
        if order_count > 200 {
            warn!("⚠️ 买单数量较多({}个)，将启用保守模式", order_count);
        }
        
        // 使用超时控制的批量创建
        let creation_timeout = Duration::from_secs(if order_count > 100 { 600 } else { 300 });
        let creation_result = tokio::time::timeout(
            creation_timeout,
            create_orders_in_batches(
                exchange_client,
                pending_buy_orders,
                grid_config,
                grid_state,
            )
        ).await;
        
        match creation_result {
            Ok(Ok(created_order_ids)) => {
                // 批量创建成功
                let success_count = created_order_ids.len();
                let success_rate = success_count as f64 / order_count as f64 * 100.0;
                
                // 将创建成功的订单添加到管理列表
                for (i, order_id) in created_order_ids.iter().enumerate() {
                    if i < pending_buy_order_info.len() {
                        active_orders.push(*order_id);
                        buy_orders.insert(*order_id, pending_buy_order_info[i].clone());
                        
                        info!("🟢 买单创建成功: ID={}, 价格={:.4}, 数量={:.4}, 资金={:.2}",
                            order_id, 
                            pending_buy_order_info[i].price,
                            pending_buy_order_info[i].quantity,
                            pending_buy_order_info[i].allocated_funds
                        );
                    }
                }
                
                info!("✅ 批量买单创建完成: {}/{} (成功率: {:.1}%)", 
                    success_count, order_count, success_rate);
                
                // 根据成功率调整后续策略
                if success_rate < 70.0 {
                    warn!("⚠️ 买单创建成功率较低({:.1}%)，调整资金分配策略", success_rate);
                    // 按实际成功比例调整已分配资金
                    allocated_buy_funds *= success_rate / 100.0;
                                                buy_count = success_count as u32;
                } else if success_rate >= 95.0 {
                    info!("🎯 买单创建成功率优秀({:.1}%)，保持当前策略", success_rate);
                }
            }
            Ok(Err(e)) => {
                error!("❌ 批量买单创建失败: {:?}", e);
                
                // 智能错误恢复策略
                if pending_buy_order_info.len() <= 20 {
                    warn!("🔄 订单数量较少，尝试单个创建模式");
                    let recovery_result = create_orders_individually(
                        exchange_client,
                        &pending_buy_order_info,
                        grid_config,
                        active_orders,
                        buy_orders,
                        true, // is_buy_order
                    ).await;
                    
                    match recovery_result {
                        Ok(recovery_count) => {
                            info!("🔄✅ 单个创建模式成功创建{}个买单", recovery_count);
                            allocated_buy_funds *= recovery_count as f64 / order_count as f64;
                            buy_count = recovery_count as u32;
                        }
                        Err(recovery_err) => {
                            error!("🔄❌ 单个创建模式也失败: {:?}", recovery_err);
                            // 完全回滚资金分配
                            allocated_buy_funds = 0.0;
                            buy_count = 0;
                        }
                    }
                } else {
                    warn!("⚠️ 订单数量过多，跳过恢复尝试，完全回滚");
                    // 完全回滚资金分配
                    allocated_buy_funds = 0.0;
                    buy_count = 0;
                }
            }
            Err(_timeout) => {
                error!("⏰ 批量买单创建超时({}秒)", creation_timeout.as_secs());
                
                // 超时后的保守恢复策略
                warn!("🔄 超时后尝试创建少量关键买单");
                let critical_orders: Vec<_> = pending_buy_order_info
                    .into_iter()
                    .take(10) // 只创建前10个最重要的订单
                    .collect();
                
                if !critical_orders.is_empty() {
                    let recovery_result = create_orders_individually(
                        exchange_client,
                        &critical_orders,
                        grid_config,
                        active_orders,
                        buy_orders,
                        true,
                    ).await;
                    
                    match recovery_result {
                        Ok(recovery_count) => {
                            info!("🔄✅ 关键买单创建成功: {}", recovery_count);
                            allocated_buy_funds *= recovery_count as f64 / order_count as f64;
                            buy_count = recovery_count as u32;
                        }
                        Err(_) => {
                            warn!("🔄❌ 关键买单创建也失败，完全回滚");
                            allocated_buy_funds = 0.0;
                            buy_count = 0;
                        }
                    }
                }
            }
        }
    }

    // 创建卖单 - 价格递增
    let mut current_sell_price = current_price;
    let max_sell_quantity = grid_state.position_quantity * 0.8; // 最多卖出80%持仓
    let mut allocated_sell_quantity = 0.0;
    let mut sell_count = 0;
    
    // 收集要批量创建的卖单
    let mut pending_sell_orders: Vec<ClientOrderRequest> = Vec::new();
    let mut pending_sell_order_info: Vec<OrderInfo> = Vec::new();

    while current_sell_price < current_price * 1.2
        && allocated_sell_quantity < max_sell_quantity
        && sell_count < grid_config.grid_count
    {
        // 动态计算网格间距，应用振幅调整
        let dynamic_spacing = grid_config.min_grid_spacing
            * fund_allocation.sell_spacing_adjustment
            * amplitude_adjustment;
        current_sell_price = current_sell_price + (current_sell_price * dynamic_spacing);

        // 计算卖单数量
        let price_coefficient = (current_sell_price - current_price) / current_price;
        let mut current_grid_quantity =
            fund_allocation.sell_order_funds / current_sell_price * (1.0 + price_coefficient);

        // 确保不超过可售数量
        if allocated_sell_quantity + current_grid_quantity > max_sell_quantity {
            current_grid_quantity = max_sell_quantity - allocated_sell_quantity;
        }

        if current_grid_quantity * current_sell_price < fund_allocation.sell_order_funds * 0.1 {
            break; // 价值太小，停止创建卖单
        }

        // 验证利润要求
        if grid_state.position_avg_price > 0.0 {
            let sell_profit_rate = (current_sell_price * (1.0 - grid_config.fee_rate)
                - grid_state.position_avg_price)
                / grid_state.position_avg_price;
            let min_required_price = grid_state.position_avg_price
                * (1.0 + grid_config.min_profit / grid_state.position_avg_price)
                / (1.0 - grid_config.fee_rate);

            if sell_profit_rate < grid_config.min_profit / grid_state.position_avg_price
                && current_sell_price < min_required_price
            {
                current_sell_price = min_required_price;
            }
        }

        if current_grid_quantity > 0.0 {
            let formatted_price = format_price(current_sell_price, grid_config.price_precision);
            let formatted_quantity =
                format_price(current_grid_quantity, grid_config.quantity_precision);

            let sell_order = ClientOrderRequest {
                asset: grid_config.trading_asset.clone(),
                is_buy: false,
                reduce_only: false,
                limit_px: formatted_price,
                sz: formatted_quantity,
                cloid: None,
                order_type: ClientOrder::Limit(ClientLimit {
                    tif: "Gtc".to_string(),
                }),
            };

            // 收集卖单信息，准备批量创建
            pending_sell_orders.push(sell_order);
            pending_sell_order_info.push(OrderInfo {
                price: formatted_price,
                quantity: formatted_quantity,
                cost_price: Some(grid_state.position_avg_price),
                potential_sell_price: None,
                allocated_funds: 0.0,
            });
            
            allocated_sell_quantity += formatted_quantity;
            sell_count += 1;
        }
    }

    // 批量创建卖单
    if !pending_sell_orders.is_empty() {
        let sell_order_count = pending_sell_orders.len();
        info!("📦 开始批量创建{}个卖单", sell_order_count);
        
        match create_orders_in_batches(
            exchange_client,
            pending_sell_orders,
            grid_config,
            grid_state,
        ).await {
            Ok(created_order_ids) => {
                // 将创建成功的订单添加到管理列表
                for (i, order_id) in created_order_ids.iter().enumerate() {
                    if i < pending_sell_order_info.len() {
                        active_orders.push(*order_id);
                        sell_orders.insert(*order_id, pending_sell_order_info[i].clone());
                        
                        info!("🔴 卖单创建成功: ID={}, 价格={:.4}, 数量={:.4}",
                            order_id, 
                            pending_sell_order_info[i].price,
                            pending_sell_order_info[i].quantity
                        );
                    }
                }
                info!("✅ 批量卖单创建完成: {}/{}", created_order_ids.len(), sell_order_count);
            }
            Err(e) => {
                warn!("❌ 批量卖单创建失败: {:?}", e);
                // 回滚数量分配
                allocated_sell_quantity = 0.0;
                sell_count = 0;
            }
        }
    }

    // 更新可用资金
    grid_state.available_funds -= allocated_buy_funds;

    info!("✅ 动态网格创建完成 - 买单数量: {}, 卖单数量: {}, 已分配买单资金: {:.2}, 已分配卖单数量: {:.4}", 
        buy_count, sell_count, allocated_buy_funds, allocated_sell_quantity);

    Ok(())
}

// 执行止损操作
async fn execute_stop_loss(
    exchange_client: &ExchangeClient,
    grid_config: &crate::config::GridConfig,
    grid_state: &mut GridState,
    stop_result: &StopLossResult,
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
    current_price: f64,
) -> Result<(), GridStrategyError> {
    info!(
        "🚨 执行止损操作: {}, 原因: {}, 止损数量: {:.4}",
        stop_result.action.as_str(),
        stop_result.reason,
        stop_result.stop_quantity
    );

    if stop_result.action.is_full_stop() {
        grid_state.stop_loss_status = StopLossStatus::Monitoring;
        
        // 使用专门的清仓函数
        if grid_state.position_quantity > 0.0 {
            // 估算当前价格（使用更安全的方法）
            let current_price =
                if grid_state.available_funds > 0.0 && grid_state.position_quantity > 0.0 {
                    // 如果有持仓，使用持仓均价作为参考
                    grid_state.position_avg_price
                } else {
                    // 否则使用一个合理的默认价格
                    1000.0 // 这应该从市场数据获取
                };

            match close_all_positions(
                exchange_client,
                grid_config,
                grid_state.position_quantity,
                0.0, // 假设只有多头持仓
                current_price,
            )
            .await
            {
                Ok(_) => {
                    info!("✅ 全部清仓完成，数量: {:.4}", grid_state.position_quantity);
                    grid_state.position_quantity = 0.0;
                    grid_state.position_avg_price = 0.0;
                    grid_state.stop_loss_status = StopLossStatus::FullyExecuted;
                }
                Err(e) => {
                    error!("❌ 全部清仓失败: {:?}", e);
                    grid_state.stop_loss_status = StopLossStatus::Failed;
                    return Err(e);
                }
            }
        } else {
            grid_state.stop_loss_status = StopLossStatus::FullyExecuted;
        }

        // 取消所有订单
        cancel_all_orders(exchange_client, active_orders).await?;
        buy_orders.clear();
        sell_orders.clear();
    } else if stop_result.action.is_partial_stop() && stop_result.stop_quantity > 0.0 {
        grid_state.stop_loss_status = StopLossStatus::Monitoring;
        
        // 部分清仓 - 使用滑点容忍度设置限价
        let stop_price = if grid_state.position_avg_price > 0.0 {
            grid_state.position_avg_price
        } else {
            current_price
        };
        
        // 考虑滑点的卖出价格
        let sell_price_with_slippage = stop_price * (1.0 - grid_config.slippage_tolerance);
        
        let market_sell_order = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: false,
            reduce_only: true,
            limit_px: sell_price_with_slippage, // 使用滑点容忍度
            sz: stop_result.stop_quantity,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Ioc".to_string(),
            }),
        };
        
        info!("🔄 执行部分止损 - 价格: {:.4} (含滑点: {:.2}%)", 
            sell_price_with_slippage, grid_config.slippage_tolerance * 100.0);

        match exchange_client.order(market_sell_order, None).await {
            Ok(_) => {
                info!("✅ 部分清仓完成，数量: {:.4}", stop_result.stop_quantity);
                grid_state.position_quantity -= stop_result.stop_quantity;
                grid_state.stop_loss_status = StopLossStatus::PartialExecuted;

                // 取消部分高价位卖单
                let sell_orders_vec: Vec<_> =
                    sell_orders.iter().map(|(k, v)| (*k, v.clone())).collect();
                let mut sorted_orders = sell_orders_vec;
                sorted_orders.sort_by(|a, b| b.1.price.partial_cmp(&a.1.price).unwrap());

                let cancel_count = (sorted_orders.len() / 2).max(1);
                for (oid, _) in sorted_orders.iter().take(cancel_count) {
                    if let Err(e) = cancel_order(exchange_client, *oid).await {
                        warn!("取消卖单失败: {:?}", e);
                    } else {
                        active_orders.retain(|&x| x != *oid);
                        sell_orders.remove(oid);
                    }
                }
            }
            Err(e) => {
                error!("❌ 部分清仓失败: {:?}", e);
                grid_state.stop_loss_status = StopLossStatus::Failed;
                return Err(GridStrategyError::OrderError(format!(
                    "部分清仓失败: {:?}",
                    e
                )));
            }
        }
    }

    Ok(())
}

// 重平衡网格
async fn rebalance_grid(
    exchange_client: &ExchangeClient,
    grid_config: &crate::config::GridConfig,
    grid_state: &mut GridState,
    current_price: f64,
    price_history: &[f64],
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
) -> Result<(), GridStrategyError> {
    info!("🔄 开始网格重平衡...");

    // 分析市场状况
    let market_analysis = analyze_market_trend(price_history);

    info!(
        "📊 市场分析 - 波动率: {:.4}, 趋势: {}, RSI: {:.2}",
        market_analysis.volatility,
        market_analysis.trend.as_str(),
        market_analysis.rsi
    );

    // 更新历史波动率（使用移动平均方式平滑更新）
    if grid_state.historical_volatility == 0.0 {
        grid_state.historical_volatility = market_analysis.volatility;
    } else {
        grid_state.historical_volatility =
            grid_state.historical_volatility * 0.7 + market_analysis.volatility * 0.3;
    }

    // 根据利润表现调整风险系数
    let profit_rate = grid_state.realized_profit / grid_state.total_capital;
    let risk_adjustment = if profit_rate > 0.05 {
        // 利润>5%
        info!("📈 利润表现良好({:.2}%)，提高风险系数", profit_rate * 100.0);
        1.1 // 提高风险系数
    } else if profit_rate < 0.01 {
        // 利润<1%
        info!("📉 利润表现不佳({:.2}%)，降低风险系数", profit_rate * 100.0);
        0.9 // 降低风险系数
    } else {
        1.0 // 保持默认风险系数
    };

    // 应用风险调整到网格参数
    grid_state.historical_volatility *= risk_adjustment;

    // 根据市场分析和风险调整动态调整策略参数
    let mut adjusted_fund_allocation =
        calculate_dynamic_fund_allocation(grid_state, current_price, grid_config);

    // 根据趋势调整网格策略
    if market_analysis.trend.is_bullish() {
        // 上升趋势：增加买单密度，减少卖单密度
        adjusted_fund_allocation.buy_spacing_adjustment *= 0.8 * risk_adjustment;
        adjusted_fund_allocation.sell_spacing_adjustment *= 1.2;
        info!("📈 检测到{}趋势({}), 调整买单密度", 
            market_analysis.trend.as_str(), market_analysis.trend.as_english());
    } else if market_analysis.trend.is_bearish() {
        // 下降趋势：减少买单密度，增加卖单密度
        adjusted_fund_allocation.buy_spacing_adjustment *= 1.2;
        adjusted_fund_allocation.sell_spacing_adjustment *= 0.8 * risk_adjustment;
        info!("📉 检测到{}趋势({}), 调整卖单密度", 
            market_analysis.trend.as_str(), market_analysis.trend.as_english());
    } else if market_analysis.trend.is_sideways() {
        // 震荡趋势：保持均衡的网格密度，应用风险调整
        adjusted_fund_allocation.buy_spacing_adjustment *= risk_adjustment;
        adjusted_fund_allocation.sell_spacing_adjustment *= risk_adjustment;
        info!("📊 检测到{}趋势({}), 保持均衡网格", 
            market_analysis.trend.as_str(), market_analysis.trend.as_english());
    }

    // 使用 RSI 指标调整交易激进程度
    if market_analysis.rsi > 70.0 {
        // 超买状态，减少买单资金
        adjusted_fund_allocation.buy_order_funds *= 0.7;
        info!("⚠️ RSI超买({:.1})，减少买单资金", market_analysis.rsi);
    } else if market_analysis.rsi < 30.0 {
        // 超卖状态，增加买单资金
        adjusted_fund_allocation.buy_order_funds *= 1.3;
        info!("💡 RSI超卖({:.1})，增加买单资金", market_analysis.rsi);
    }

    // 使用移动平均线进行趋势确认
    if market_analysis.short_ma > market_analysis.long_ma * 1.02 {
        // 短期均线明显高于长期均线，确认上升趋势
        adjusted_fund_allocation.buy_order_funds *= 1.1;
        info!("📈 均线确认上升趋势，增加买单资金");
    } else if market_analysis.short_ma < market_analysis.long_ma * 0.98 {
        // 短期均线明显低于长期均线，确认下降趋势
        adjusted_fund_allocation.buy_order_funds *= 0.9;
        info!("📉 均线确认下降趋势，减少买单资金");
    }

    // 根据5分钟价格变化调整紧急程度
    if market_analysis.price_change_5min.abs() > 0.03 {
        // 5分钟变化超过3%
        if market_analysis.price_change_5min > 0.0 {
            // 快速上涨，减少买单
            adjusted_fund_allocation.buy_order_funds *= 0.8;
            info!(
                "🚀 快速上涨({:.2}%)，减少买单",
                market_analysis.price_change_5min * 100.0
            );
        } else {
            // 快速下跌，增加买单机会
            adjusted_fund_allocation.buy_order_funds *= 1.2;
            info!(
                "💥 快速下跌({:.2}%)，增加买单机会",
                market_analysis.price_change_5min * 100.0
            );
        }
    }

    // 取消所有现有订单
    info!("🗑️ 取消现有订单...");
    cancel_all_orders(exchange_client, active_orders).await?;
    buy_orders.clear();
    sell_orders.clear();

    // 等待订单取消完成
    sleep(Duration::from_secs(2)).await;

    // 更新网格状态
    // 这里可以根据市场分析调整网格参数

    // 重新创建网格
    create_dynamic_grid(
        exchange_client,
        grid_config,
        grid_state,
        current_price,
        price_history,
        active_orders,
        buy_orders,
        sell_orders,
    )
    .await?;

    grid_state.last_rebalance_time = SystemTime::now();

    info!("✅ 网格重平衡完成");
    Ok(())
}

// 取消所有订单
async fn cancel_all_orders(
    exchange_client: &ExchangeClient,
    active_orders: &mut Vec<u64>,
) -> Result<(), GridStrategyError> {
    for &oid in active_orders.iter() {
        if let Err(e) = cancel_order(exchange_client, oid).await {
            warn!("取消订单{}失败: {:?}", oid, e);
        }
    }
    active_orders.clear();
    Ok(())
}

// 取消单个订单
async fn cancel_order(exchange_client: &ExchangeClient, oid: u64) -> Result<(), GridStrategyError> {
    // 注意：这里硬编码了资产名称，实际应该从配置中获取
    // 但由于函数签名限制，暂时使用通用的取消方式
    let cancel_request = ClientCancelRequest {
        asset: "BTC".to_string(), // TODO: 从配置中获取
        oid,
    };

    match exchange_client.cancel(cancel_request, None).await {
        Ok(_) => {
            info!("✅ 订单{}已取消", oid);
            Ok(())
        }
        Err(e) => {
            warn!("❌ 取消订单{}失败: {:?}", oid, e);
            Err(GridStrategyError::OrderError(format!(
                "取消订单失败: {:?}",
                e
            )))
        }
    }
}

// 监控资金使用和订单限制
fn monitor_fund_allocation(
    grid_state: &GridState,
    buy_orders: &HashMap<u64, OrderInfo>,
    sell_orders: &HashMap<u64, OrderInfo>,
    grid_config: &crate::config::GridConfig,
) -> Result<(), GridStrategyError> {
    // 计算总分配资金
    let total_allocated = buy_orders.values().map(|o| o.allocated_funds).sum::<f64>();
    let fund_usage_rate = if grid_state.total_capital > 0.0 {
        total_allocated / grid_state.total_capital
    } else {
        0.0
    };

    // 检查资金使用率
    if fund_usage_rate > 0.9 {
        return Err(GridStrategyError::FundAllocationError(format!(
            "资金使用率过高: {:.2}%",
            fund_usage_rate * 100.0
        )));
    }

    // 检查订单数量限制
    let total_orders = buy_orders.len() + sell_orders.len();
    if total_orders > grid_config.max_active_orders {
        return Err(GridStrategyError::FundAllocationError(format!(
            "活跃订单数量({})超过限制({})",
            total_orders, grid_config.max_active_orders
        )));
    }

    // 检查单个订单的资金分配是否合理
    for (oid, order_info) in buy_orders.iter() {
        if order_info.allocated_funds > grid_state.total_capital * 0.2 {
            warn!(
                "⚠️ 订单{}分配资金过多: {:.2}",
                oid, order_info.allocated_funds
            );
        }
    }

    info!(
        "📊 资金监控 - 使用率: {:.2}%, 活跃订单: {}, 总分配: {:.2}",
        fund_usage_rate * 100.0,
        total_orders,
        total_allocated
    );

    Ok(())
}

// 生成状态报告
fn generate_status_report(
    grid_state: &GridState,
    current_price: f64,
    buy_orders: &HashMap<u64, OrderInfo>,
    sell_orders: &HashMap<u64, OrderInfo>,
    grid_config: &crate::config::GridConfig,
) -> String {
    let current_total_value =
        grid_state.available_funds + grid_state.position_quantity * current_price;
    let position_ratio = if grid_state.total_capital > 0.0 {
        (grid_state.position_quantity * current_price) / grid_state.total_capital * 100.0
    } else {
        0.0
    };
    let asset_change = (current_total_value / grid_state.total_capital - 1.0) * 100.0;
    let profit_rate = grid_state.realized_profit / grid_state.total_capital * 100.0;

    format!(
        "===== 网格交易状态报告 =====\n\
        时间: {}\n\
        交易对: {}\n\
        当前价格: {:.4}\n\
        网格间距: {:.4}% - {:.4}%\n\
        初始资金: {:.2}\n\
        可用资金: {:.2}\n\
        持仓数量: {:.4}\n\
        持仓均价: {:.4}\n\
        持仓比例: {:.2}%\n\
        当前总资产: {:.2}\n\
        资产变化: {:.2}%\n\
        已实现利润: {:.2}\n\
        利润率: {:.2}%\n\
        活跃买单数: {}\n\
        活跃卖单数: {}\n\
        浮动止损价: {:.4}\n\
        止损状态: {}\n\
        历史交易数: {}\n\
        最大回撤: {:.2}%\n\
        连接重试次数: {}\n\
        ==============================",
        format!(
            "{:?}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        ),
        grid_config.trading_asset,
        current_price,
        grid_config.min_grid_spacing * 100.0,
        grid_config.max_grid_spacing * 100.0,
        grid_state.total_capital,
        grid_state.available_funds,
        grid_state.position_quantity,
        grid_state.position_avg_price,
        position_ratio,
        current_total_value,
        asset_change,
        grid_state.realized_profit,
        profit_rate,
        buy_orders.len(),
        sell_orders.len(),
        grid_state.trailing_stop_price,
        grid_state.stop_loss_status.as_str(),
        grid_state.performance_history.len(),
        grid_state.current_metrics.max_drawdown * 100.0,
        grid_state.connection_retry_count
    )
}

pub async fn run_grid_strategy(
    app_config: crate::config::AppConfig,
) -> Result<(), GridStrategyError> {
    env_logger::init();
    let grid_config = &app_config.grid;

    // 验证配置参数
    validate_grid_config(grid_config)?;

    // 从配置文件读取私钥
    let private_key = &app_config.account.private_key;

    // 初始化钱包
    let wallet: LocalWallet = private_key
        .parse()
        .map_err(|e| GridStrategyError::WalletError(format!("私钥解析失败: {:?}", e)))?;
    let user_address = if let Some(addr) = &app_config.account.real_account_address {
        addr.parse().expect("real_account_address 格式错误")
    } else {
        wallet.address()
    };
    info!("实际查询的钱包地址: {:?}", user_address);

    // 初始化客户端
    let mut info_client = InfoClient::new(None, Some(BaseUrl::Mainnet))
        .await
        .map_err(|e| GridStrategyError::ClientError(format!("信息客户端初始化失败: {:?}", e)))?;

    let exchange_client = ExchangeClient::new(None, wallet, Some(BaseUrl::Mainnet), None, None)
        .await
        .map_err(|e| GridStrategyError::ClientError(format!("交易客户端初始化失败: {:?}", e)))?;

    info!("=== 交易参数 ===");
    info!("交易资产: {}", grid_config.trading_asset);
    info!("总资金: {}", grid_config.total_capital);
    info!("网格数量: {}", grid_config.grid_count);
    info!("每格交易金额: {}", grid_config.trade_amount);
    info!("最大持仓: {}", grid_config.max_position);
    info!("最大回撤: {}%", grid_config.max_drawdown * 100.0);
    info!("价格精度: {}", grid_config.price_precision);
    info!("数量精度: {}", grid_config.quantity_precision);
    info!("检查间隔: {}秒", grid_config.check_interval);
    info!("杠杆倍数: {}x", grid_config.leverage);
    info!("最小网格间距: {}%", grid_config.min_grid_spacing * 100.0);
    info!("最大网格间距: {}%", grid_config.max_grid_spacing * 100.0);
    info!("网格价格偏移: {}%", grid_config.grid_price_offset * 100.0);
    info!("单笔最大亏损: {}%", grid_config.max_single_loss * 100.0);
    info!("每日最大亏损: {}%", grid_config.max_daily_loss * 100.0);
    info!("最大持仓时间: {}小时", grid_config.max_holding_time / 3600);

    // 设置杠杆倍数
    match exchange_client
        .update_leverage(
            grid_config.leverage,
            &grid_config.trading_asset,
            false,
            None,
        )
        .await
    {
        Ok(_) => info!("成功设置杠杆倍数为 {}x", grid_config.leverage),
        Err(e) => {
            error!("设置杠杆倍数失败: {:?}", e);
            return Err(GridStrategyError::OrderError(format!(
                "设置杠杆倍数失败: {:?}",
                e
            )));
        }
    }

    // 初始化网格状态
    let mut grid_state = GridState {
        total_capital: grid_config.total_capital,
        available_funds: grid_config.total_capital,
        position_quantity: 0.0,
        position_avg_price: 0.0,
        realized_profit: 0.0,
        highest_price_after_position: 0.0,
        trailing_stop_price: 0.0,
        stop_loss_status: StopLossStatus::Normal,
        last_rebalance_time: SystemTime::now(),
        historical_volatility: 0.0,
        performance_history: Vec::new(),
        current_metrics: PerformanceMetrics {
            total_trades: 0,
            winning_trades: 0,
            losing_trades: 0,
            win_rate: 0.0,
            total_profit: 0.0,
            max_drawdown: 0.0,
            sharpe_ratio: 0.0,
            profit_factor: 0.0,
            average_win: 0.0,
            average_loss: 0.0,
            largest_win: 0.0,
            largest_loss: 0.0,
        },
        last_margin_check: SystemTime::now(),
        connection_retry_count: 0,
        last_order_batch_time: SystemTime::now(),
    };

    let mut active_orders: Vec<u64> = Vec::new();
    let mut last_price: Option<f64> = None;
    let mut buy_orders: HashMap<u64, OrderInfo> = HashMap::new();
    let mut sell_orders: HashMap<u64, OrderInfo> = HashMap::new();

    let mut last_daily_reset = SystemTime::now();
    let mut last_status_report = SystemTime::now();

    // 价格历史记录
    let mut price_history: Vec<f64> = Vec::new();

    // 创建消息通道
    let (sender, mut receiver) = unbounded_channel();

    // 订阅中间价格和用户事件
    info_client
        .subscribe(Subscription::AllMids, sender.clone())
        .await
        .map_err(|e| GridStrategyError::SubscriptionError(format!("订阅价格失败: {:?}", e)))?;

    info_client
        .subscribe(
            Subscription::UserEvents { user: user_address },
            sender.clone(),
        )
        .await
        .map_err(|e| GridStrategyError::SubscriptionError(format!("订阅用户事件失败: {:?}", e)))?;

    info!("🚀 资金管理型动态网格交易策略已启动");

    loop {
        let now = SystemTime::now();

        // 检查是否需要重置每日统计
        if now.duration_since(last_daily_reset).unwrap().as_secs() >= 24 * 60 * 60 {
            last_daily_reset = now;
            info!("🔄 重置每日统计");
        }

        // 获取当前价格和处理消息
        match receiver.recv().await {
            Some(Message::AllMids(all_mids)) => {
                let all_mids = all_mids.data.mids;
                if let Some(current_price) = all_mids.get(&grid_config.trading_asset) {
                    let current_price: f64 = current_price.parse().map_err(|e| {
                        GridStrategyError::PriceParseError(format!("价格解析失败: {:?}", e))
                    })?;

                    // 获取实际账户信息
                    let account_info = get_account_info(&info_client, user_address).await?;
                    let usdc_balance = account_info.withdrawable.parse().unwrap_or(0.0);

                    // 更新网格状态
                    grid_state.available_funds = usdc_balance;

                    // 更新价格历史
                    price_history.push(current_price);
                    if price_history.len() > grid_config.history_length {
                        price_history.remove(0);
                    }

                    // 打印价格变化
                    if let Some(last) = last_price {
                        let price_change = ((current_price - last) / last) * 100.0;
                        info!(
                            "📈 价格变化: {:.4}% (从 {:.4} 到 {:.4})",
                            price_change, last, current_price
                        );
                    }
                    last_price = Some(current_price);

                    // 1. 止损检查
                    let stop_result = check_stop_loss(
                        &mut grid_state,
                        current_price,
                        grid_config,
                        &price_history,
                    );

                    if stop_result.action.requires_action() {
                        warn!(
                            "🚨 触发止损: {} ({}), 原因: {}, 当前状态: {} ({})",
                            stop_result.action.as_str(),
                            stop_result.action.as_english(),
                            stop_result.reason,
                            grid_state.stop_loss_status.as_str(),
                            grid_state.stop_loss_status.as_english()
                        );

                        execute_stop_loss(
                            &exchange_client,
                            grid_config,
                            &mut grid_state,
                            &stop_result,
                            &mut active_orders,
                            &mut buy_orders,
                            &mut sell_orders,
                            current_price,
                        )
                        .await?;

                        if stop_result.action.is_full_stop() {
                            error!("🛑 策略已全部止损，退出");
                            break;
                        }
                    }

                    // 检查止损状态是否允许继续交易
                    if !grid_state.stop_loss_status.can_continue_trading() {
                        warn!("⚠️ 止损状态({})不允许继续交易", grid_state.stop_loss_status.as_str());
                        if grid_state.stop_loss_status.is_failed() {
                            error!("❌ 止损执行失败，策略退出");
                            break;
                        }
                    }

                    // 2. 检查是否需要重平衡（每24小时）
                    let rebalance_interval = 24 * 60 * 60; // 24小时
                    if now
                        .duration_since(grid_state.last_rebalance_time)
                        .unwrap()
                        .as_secs()
                        >= rebalance_interval
                    {
                        info!("🔄 开始定期重平衡...");

                        // 在重平衡前优化参数
                        if grid_state.performance_history.len() >= 20 {
                            info!("📈 基于历史表现分析网格参数优化建议");
                            analyze_grid_performance_and_suggest_optimization(grid_config, &grid_state);
                            
                            // 创建一个临时的配置副本进行优化分析
                            let mut temp_min_spacing = grid_config.min_grid_spacing;
                            let mut temp_max_spacing = grid_config.max_grid_spacing;
                            
                            // 手动应用优化逻辑
                            if grid_state.performance_history.len() >= 10 {
                                let recent_records: Vec<&PerformanceRecord> = grid_state
                                    .performance_history
                                    .iter()
                                    .rev()
                                    .take(20)
                                    .collect();
                                
                                let recent_profit: f64 = recent_records.iter().map(|r| r.profit).sum();
                                let recent_win_rate = recent_records
                                    .iter()
                                    .filter(|r| r.profit > 0.0)
                                    .count() as f64
                                    / recent_records.len() as f64;
                                
                                // 根据表现调整网格间距
                                if recent_profit > 0.0 && recent_win_rate > 0.6 {
                                    // 表现良好，可以稍微增加网格间距以获得更大利润
                                    temp_min_spacing *= 1.05;
                                    temp_max_spacing *= 1.05;
                                    info!("🔧 参数优化建议 - 表现良好，建议增加网格间距");
                                } else if recent_profit < 0.0 || recent_win_rate < 0.4 {
                                    // 表现不佳，减少网格间距以提高成交频率
                                    temp_min_spacing *= 0.95;
                                    temp_max_spacing *= 0.95;
                                    info!("🔧 参数优化建议 - 表现不佳，建议减少网格间距");
                                }
                                
                                // 确保网格间距在合理范围内
                                temp_min_spacing = temp_min_spacing.max(0.001).min(0.05);
                                temp_max_spacing = temp_max_spacing.max(temp_min_spacing).min(0.1);
                                
                                // 显示优化建议
                                if (temp_min_spacing - grid_config.min_grid_spacing).abs() > 0.0001 {
                                    info!("🔧 参数优化建议 - 最小网格间距: {:.4}% -> {:.4}%", 
                                        grid_config.min_grid_spacing * 100.0,
                                        temp_min_spacing * 100.0);
                                }
                                if (temp_max_spacing - grid_config.max_grid_spacing).abs() > 0.0001 {
                                    info!("🔧 参数优化建议 - 最大网格间距: {:.4}% -> {:.4}%", 
                                        grid_config.max_grid_spacing * 100.0,
                                        temp_max_spacing * 100.0);
                                }
                            }
                            
                            info!("💡 参数优化分析完成，建议在配置文件中手动调整参数");
                        }

                        rebalance_grid(
                            &exchange_client,
                            grid_config,
                            &mut grid_state,
                            current_price,
                            &price_history,
                            &mut active_orders,
                            &mut buy_orders,
                            &mut sell_orders,
                        )
                        .await?;
                    }

                    // 3. 定期检查订单状态（每30秒）
                    if now.duration_since(grid_state.last_order_batch_time).unwrap().as_secs() >= 30 {
                        if let Err(e) = check_order_status(
                            &info_client,
                            user_address,
                            &mut active_orders,
                            &mut buy_orders,
                            &mut sell_orders,
                        ).await {
                            warn!("⚠️ 订单状态检查失败: {:?}", e);
                        }
                        grid_state.last_order_batch_time = now;
                    }

                    // 3.1 如果没有活跃订单，创建动态网格
                    if active_orders.is_empty() {
                        info!("📊 没有活跃订单，创建动态网格...");

                        create_dynamic_grid(
                            &exchange_client,
                            grid_config,
                            &mut grid_state,
                            current_price,
                            &price_history,
                            &mut active_orders,
                            &mut buy_orders,
                            &mut sell_orders,
                        )
                        .await?;
                        
                        // 如果配置了批量订单，可以在这里使用批量创建功能
                        if grid_config.max_orders_per_batch > 1 && grid_config.order_batch_delay_ms > 0 {
                            info!("💡 批量订单配置已启用 - 批次大小: {}, 延迟: {}ms", 
                                grid_config.max_orders_per_batch, grid_config.order_batch_delay_ms);
                        }
                    }

                    // 4. 资金分配监控
                    if let Err(e) =
                        monitor_fund_allocation(&grid_state, &buy_orders, &sell_orders, grid_config)
                    {
                        warn!("⚠️ 资金分配监控警告: {:?}", e);
                    }

                    // 4.1 保证金监控（每5分钟检查一次）
                    if now.duration_since(grid_state.last_margin_check).unwrap().as_secs() >= 300 {
                        // 首先检查连接状态
                        match ensure_connection(&info_client, user_address, &mut grid_state).await {
                            Ok(true) => {
                                // 连接正常，进行保证金检查
                                match check_margin_ratio(&info_client, user_address, grid_config).await {
                                    Ok(margin_ratio) => {
                                        info!("💳 保证金率: {:.1}%", margin_ratio * 100.0);
                                        grid_state.last_margin_check = now;
                                    }
                                    Err(e) => {
                                        error!("🚨 保证金监控失败: {:?}", e);
                                        // 如果是保证金不足，触发紧急止损
                                        if matches!(e, GridStrategyError::MarginInsufficient(_)) {
                                            warn!("🚨 保证金不足，执行紧急止损");
                                            let emergency_stop = StopLossResult {
                                                action: StopLossAction::FullStop,
                                                reason: "保证金不足".to_string(),
                                                stop_quantity: grid_state.position_quantity,
                                            };
                                            if let Err(stop_err) = execute_stop_loss(
                                                &exchange_client,
                                                grid_config,
                                                &mut grid_state,
                                                &emergency_stop,
                                                &mut active_orders,
                                                &mut buy_orders,
                                                &mut sell_orders,
                                                current_price,
                                            ).await {
                                                error!("❌ 紧急止损执行失败: {:?}", stop_err);
                                            }
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(false) => {
                                warn!("⚠️ 网络连接不稳定，跳过本次检查");
                            }
                            Err(e) => {
                                error!("❌ 连接检查失败: {:?}", e);
                                // 连接失败次数过多，退出策略
                                if grid_state.connection_retry_count > 10 {
                                    error!("🚨 网络连接失败次数过多，退出策略");
                                    break;
                                }
                            }
                        }
                    }

                    // 5. 定期状态报告（每小时）
                    if now.duration_since(last_status_report).unwrap().as_secs() >= 3600 {
                        // 更新性能指标
                        grid_state.current_metrics = calculate_performance_metrics(&grid_state, &price_history);
                        
                        let report = generate_status_report(
                            &grid_state,
                            current_price,
                            &buy_orders,
                            &sell_orders,
                            grid_config,
                        );
                        info!("\n{}", report);
                        
                        // 输出详细性能指标
                        info!("📊 详细性能指标:");
                        info!("   总交易数: {} (胜: {}, 负: {})", 
                            grid_state.current_metrics.total_trades,
                            grid_state.current_metrics.winning_trades,
                            grid_state.current_metrics.losing_trades
                        );
                        info!("   胜率: {:.1}%, 利润因子: {:.2}, 夏普比率: {:.2}", 
                            grid_state.current_metrics.win_rate * 100.0,
                            grid_state.current_metrics.profit_factor,
                            grid_state.current_metrics.sharpe_ratio
                        );
                        info!("   总利润: {:.2}, 最大回撤: {:.2}%", 
                            grid_state.current_metrics.total_profit,
                            grid_state.current_metrics.max_drawdown * 100.0
                        );
                        info!("   平均盈利: {:.2}, 平均亏损: {:.2}", 
                            grid_state.current_metrics.average_win,
                            grid_state.current_metrics.average_loss
                        );
                        info!("   最大单笔盈利: {:.2}, 最大单笔亏损: {:.2}", 
                            grid_state.current_metrics.largest_win,
                            grid_state.current_metrics.largest_loss
                        );
                        
                        last_status_report = now;
                    }
                }
            }

            Some(Message::User(user_event)) => {
                match user_event.data {
                    UserData::Fills(fills) => {
                        for fill in fills {
                            let fill_price: f64 = fill.px.parse().map_err(|e| {
                                GridStrategyError::PriceParseError(format!(
                                    "成交价格解析失败: {:?}",
                                    e
                                ))
                            })?;
                            let fill_size: f64 = fill.sz.parse().map_err(|e| {
                                GridStrategyError::QuantityParseError(format!(
                                    "成交数量解析失败: {:?}",
                                    e
                                ))
                            })?;

                            info!(
                                "📋 订单成交: ID={}, 方向={}, 价格={}, 数量={}",
                                fill.oid, fill.side, fill_price, fill_size
                            );

                            // 更新持仓信息
                            if fill.side == "B" {
                                // 买单成交，更新持仓
                                let buy_value = fill_price * fill_size;
                                let total_value = grid_state.position_avg_price
                                    * grid_state.position_quantity
                                    + buy_value;
                                grid_state.position_quantity +=
                                    fill_size * (1.0 - grid_config.fee_rate);

                                if grid_state.position_quantity > 0.0 {
                                    grid_state.position_avg_price =
                                        total_value / grid_state.position_quantity;
                                }

                                // 使用新的智能订单处理逻辑
                                if let Some(order_info) = buy_orders.remove(&fill.oid) {
                                    // 验证订单信息
                                    if (order_info.price - fill_price).abs() > fill_price * 0.001 {
                                        warn!(
                                            "⚠️ 订单价格不匹配: 预期 {:.4}, 实际 {:.4}",
                                            order_info.price, fill_price
                                        );
                                    }

                                    // 使用潜在卖出价格进行利润预测
                                    if let Some(potential_price) = order_info.potential_sell_price {
                                        let expected_profit = (potential_price - fill_price)
                                            * fill_size
                                            * (1.0 - grid_config.fee_rate * 2.0);
                                        info!(
                                            "💡 预期利润: {:.2} (潜在卖价: {:.4})",
                                            expected_profit, potential_price
                                        );
                                    }

                                    // 更新资金使用统计
                                    grid_state.available_funds -= order_info.allocated_funds;

                                    if let Err(e) = handle_buy_fill(
                                        &exchange_client,
                                        grid_config,
                                        fill_price,
                                        fill_size,
                                        grid_config.min_grid_spacing,
                                        &mut active_orders,
                                        &mut buy_orders,
                                        &mut sell_orders,
                                    )
                                    .await
                                    {
                                        warn!("处理买单成交失败: {:?}", e);
                                    }

                                    info!("💰 买单成交处理完成 - 原始订单价格: {:.4}, 数量: {:.4}, 分配资金: {:.2}", 
                                        order_info.price, order_info.quantity, order_info.allocated_funds);
                                } else {
                                    warn!("⚠️ 未找到买单订单信息: ID={}", fill.oid);
                                }
                            } else {
                                // 卖单成交，更新持仓和利润
                                grid_state.position_quantity -= fill_size;

                                // 计算利润
                                if let Some(order_info) = sell_orders.remove(&fill.oid) {
                                    let cost_price = order_info
                                        .cost_price
                                        .unwrap_or(grid_state.position_avg_price);
                                    let sell_revenue =
                                        fill_price * fill_size * (1.0 - grid_config.fee_rate);
                                    let buy_cost = cost_price * fill_size;
                                    let profit = sell_revenue - buy_cost;

                                    grid_state.realized_profit += profit;
                                    grid_state.available_funds += sell_revenue;

                                    // 记录交易历史
                                    let record = PerformanceRecord {
                                        timestamp: SystemTime::now(),
                                        price: fill_price,
                                        action: "SELL".to_string(),
                                        profit,
                                        total_capital: grid_state.available_funds + grid_state.position_quantity * fill_price,
                                    };
                                    grid_state.performance_history.push(record.clone());
                                    
                                    // 输出交易记录详情
                                    info!("📝 交易记录 - 时间: {:?}, 动作: {}, 价格: {:.4}, 利润: {:.2}, 总资产: {:.2}", 
                                        record.timestamp.duration_since(UNIX_EPOCH).unwrap().as_secs(),
                                        record.action, record.price, record.profit, record.total_capital);

                                    info!("💰 卖单成交 - 成本价: {:.4}, 卖出价: {:.4}, 利润: {:.2}, 利润率: {:.2}%", 
                                        cost_price, fill_price, profit, (profit / buy_cost) * 100.0);

                                    if let Err(e) = handle_sell_fill(
                                        &exchange_client,
                                        grid_config,
                                        fill_price,
                                        fill_size,
                                        Some(cost_price),
                                        grid_config.min_grid_spacing,
                                        &mut active_orders,
                                        &mut buy_orders,
                                        &mut sell_orders,
                                    )
                                    .await
                                    {
                                        warn!("处理卖单成交失败: {:?}", e);
                                    }
                                }
                            }

                            // 从活跃订单列表中移除
                            active_orders.retain(|&x| x != fill.oid);
                        }
                    }
                    _ => {
                        // 处理其他用户事件
                    }
                }
            }

            Some(_) => {
                // 处理其他类型的消息
                continue;
            }

            None => {
                warn!("⚠️ 消息通道已关闭");
                break;
            }
        }

        // 等待下一次检查
        sleep(Duration::from_secs(grid_config.check_interval)).await;
    }

    info!("🏁 网格交易策略已结束");
    Ok(())
}

// 安全解析字符串为f64，支持空值和无效值处理
fn safe_parse_f64(value: &str, field_name: &str, default_value: f64) -> Result<f64, GridStrategyError> {
    // 处理空字符串或仅包含空白字符的情况
    let trimmed = value.trim();
    if trimmed.is_empty() {
        warn!("⚠️ 字段 '{}' 为空，使用默认值: {}", field_name, default_value);
        return Ok(default_value);
    }
    
    // 尝试解析数值
    match trimmed.parse::<f64>() {
        Ok(parsed_value) => {
            // 检查是否为有效数值（非NaN、非无穷大）
            if parsed_value.is_finite() && parsed_value >= 0.0 {
                Ok(parsed_value)
            } else {
                warn!("⚠️ 字段 '{}' 包含无效数值: {}，使用默认值: {}", 
                    field_name, parsed_value, default_value);
                Ok(default_value)
            }
        }
        Err(e) => {
            warn!("⚠️ 字段 '{}' 解析失败: '{}' -> {:?}，使用默认值: {}", 
                field_name, trimmed, e, default_value);
            Ok(default_value)
        }
    }
}

// 检查保证金率 - 改进版本，包含健壮的错误处理
async fn check_margin_ratio(
    info_client: &InfoClient,
    user_address: ethers::types::Address,
    grid_config: &crate::config::GridConfig,
) -> Result<f64, GridStrategyError> {
    // 获取账户信息，包含重试机制
    let account_info = match get_account_info(info_client, user_address).await {
        Ok(info) => info,
        Err(e) => {
            warn!("⚠️ 获取账户信息失败，无法检查保证金率: {:?}", e);
            return Err(GridStrategyError::ClientError(format!(
                "获取账户信息失败: {:?}", e
            )));
        }
    };
    
    // 检查margin_summary字段是否存在
    let margin_summary = &account_info.margin_summary;
    
    // 安全解析账户价值
    let account_value = safe_parse_f64(
        &margin_summary.account_value,
        "account_value",
        0.0
    )?;
    
    // 安全解析已使用保证金
    let total_margin_used = safe_parse_f64(
        &margin_summary.total_margin_used,
        "total_margin_used",
        0.0
    )?;
    
    // 尝试解析其他相关字段以获得更完整的保证金信息
    let total_ntl_pos = safe_parse_f64(
        &margin_summary.total_ntl_pos,
        "total_ntl_pos",
        0.0
    ).unwrap_or(0.0);
    
    let total_raw_usd = safe_parse_f64(
        &margin_summary.total_raw_usd,
        "total_raw_usd",
        0.0
    ).unwrap_or(0.0);
    
    info!("💳 保证金详细信息:");
    info!("   账户价值: {:.2}", account_value);
    info!("   已使用保证金: {:.2}", total_margin_used);
    info!("   总持仓价值: {:.2}", total_ntl_pos);
    info!("   总USD价值: {:.2}", total_raw_usd);
    
    // 计算保证金率 - 使用多种方法确保准确性
    let margin_ratio = if total_margin_used > 0.0 {
        // 标准计算方法：可用资金 / 已使用保证金
        account_value / total_margin_used
    } else if total_ntl_pos > 0.0 {
        // 备用计算方法：使用持仓价值
        warn!("⚠️ total_margin_used为0，使用持仓价值计算保证金率");
        account_value / (total_ntl_pos * 0.1) // 假设10%的保证金要求
    } else {
        // 没有持仓或保证金要求，认为是安全的
        info!("💡 没有持仓或保证金要求，保证金率设为安全值");
        10.0 // 设置一个安全的高值
    };
    
    // 验证计算结果的合理性
    if !margin_ratio.is_finite() {
        warn!("⚠️ 保证金率计算结果无效: {}，使用默认安全值", margin_ratio);
        return Ok(10.0); // 返回安全值
    }
    
    if margin_ratio < 0.0 {
        warn!("⚠️ 保证金率为负值: {:.2}，可能存在数据异常", margin_ratio);
        return Err(GridStrategyError::MarginInsufficient(format!(
            "保证金率异常: {:.2}%，可能存在账户数据问题",
            margin_ratio * 100.0
        )));
    }
    
    // 检查保证金安全阈值
    if margin_ratio < grid_config.margin_safety_threshold {
        warn!(
            "🚨 保证金率过低: {:.2}%, 低于安全阈值: {:.2}%",
            margin_ratio * 100.0,
            grid_config.margin_safety_threshold * 100.0
        );
        
        // 提供详细的风险信息
        let risk_level = if margin_ratio < grid_config.margin_safety_threshold * 0.5 {
            "极高风险"
        } else if margin_ratio < grid_config.margin_safety_threshold * 0.8 {
            "高风险"
        } else {
            "中等风险"
        };
        
        warn!("🚨 风险等级: {} - 建议立即减仓或增加保证金", risk_level);
        
        return Err(GridStrategyError::MarginInsufficient(format!(
            "保证金率过低: {:.2}% (风险等级: {})",
            margin_ratio * 100.0,
            risk_level
        )));
    }
    
    // 提供保证金健康度反馈
    let health_status = if margin_ratio > grid_config.margin_safety_threshold * 3.0 {
        "优秀"
    } else if margin_ratio > grid_config.margin_safety_threshold * 2.0 {
        "良好"
    } else if margin_ratio > grid_config.margin_safety_threshold * 1.5 {
        "一般"
    } else {
        "需要关注"
    };
    
    info!("💳 保证金健康度: {} (比率: {:.2}%)", health_status, margin_ratio * 100.0);
    
    Ok(margin_ratio)
}

// 确保连接状态 - 改进版本，包含更好的错误分类和重试策略
async fn ensure_connection(
    info_client: &InfoClient,
    user_address: ethers::types::Address,
    grid_state: &mut GridState,
) -> Result<bool, GridStrategyError> {
    let start_time = SystemTime::now();
    
    // 使用超时控制的连接检查
    let connection_result = tokio::time::timeout(
        Duration::from_secs(15), // 连接检查超时15秒
        get_account_info(info_client, user_address)
    ).await;
    
    match connection_result {
        Ok(Ok(_account_info)) => {
            // 连接成功
            if grid_state.connection_retry_count > 0 {
                info!("✅ 网络连接已恢复 (之前重试次数: {})", grid_state.connection_retry_count);
            }
            grid_state.connection_retry_count = 0;
            
            let elapsed = start_time.elapsed().unwrap_or_default();
            if elapsed.as_millis() > 5000 {
                warn!("⚠️ 连接检查耗时较长: {}ms", elapsed.as_millis());
            }
            
            Ok(true)
        }
        Ok(Err(e)) => {
            // API调用失败
            grid_state.connection_retry_count += 1;
            
            // 分析错误类型
            let error_type = classify_connection_error(&e);
            warn!(
                "⚠️ 连接检查失败 (重试次数: {}, 错误类型: {}): {:?}",
                grid_state.connection_retry_count, error_type, e
            );
            
            // 根据错误类型决定重试策略
            let max_retries = match error_type.as_str() {
                "网络超时" => 8,      // 网络问题允许更多重试
                "API限制" => 5,       // API限制适中重试
                "认证失败" => 2,      // 认证问题快速失败
                "服务器错误" => 6,    // 服务器问题适中重试
                _ => 5,               // 默认重试次数
            };
            
            if grid_state.connection_retry_count > max_retries {
                error!("❌ 连接失败次数过多 ({}/{}，错误类型: {})", 
                    grid_state.connection_retry_count, max_retries, error_type);
                return Err(GridStrategyError::NetworkError(format!(
                    "连接失败次数过多: {} (错误类型: {})",
                    grid_state.connection_retry_count, error_type
                )));
            }
            
            // 根据错误类型和重试次数计算等待时间
            let base_delay = match error_type.as_str() {
                "API限制" => 5,       // API限制等待更长时间
                "网络超时" => 2,      // 网络超时等待较短时间
                "服务器错误" => 3,    // 服务器错误中等等待时间
                _ => 2,               // 默认等待时间
            };
            
            let wait_seconds = base_delay * 2_u64.pow(grid_state.connection_retry_count.min(4));
            info!("⏱️ 等待 {}秒 后重试连接 (错误类型: {})", wait_seconds, error_type);
            sleep(Duration::from_secs(wait_seconds)).await;
            
            Ok(false)
        }
        Err(_timeout) => {
            // 连接超时
            grid_state.connection_retry_count += 1;
            warn!(
                "⚠️ 连接检查超时 (重试次数: {}, 超时时间: 15秒)",
                grid_state.connection_retry_count
            );
            
            if grid_state.connection_retry_count > 6 {
                error!("❌ 连接超时次数过多 ({}次)", grid_state.connection_retry_count);
                return Err(GridStrategyError::NetworkError(
                    "连接超时次数过多".to_string(),
                ));
            }
            
            // 超时情况下使用较短的等待时间
            let wait_seconds = 3 * grid_state.connection_retry_count.min(5);
            info!("⏱️ 连接超时，等待 {}秒 后重试", wait_seconds);
            sleep(Duration::from_secs(wait_seconds as u64)).await;
            
            Ok(false)
        }
    }
}

// 分析连接错误类型，用于制定不同的重试策略
fn classify_connection_error(error: &GridStrategyError) -> String {
    let error_msg = format!("{:?}", error).to_lowercase();
    
    if error_msg.contains("timeout") || error_msg.contains("超时") {
        "网络超时".to_string()
    } else if error_msg.contains("rate limit") || error_msg.contains("限制") || error_msg.contains("429") {
        "API限制".to_string()
    } else if error_msg.contains("unauthorized") || error_msg.contains("认证") || error_msg.contains("401") || error_msg.contains("403") {
        "认证失败".to_string()
    } else if error_msg.contains("500") || error_msg.contains("502") || error_msg.contains("503") || error_msg.contains("服务器") {
        "服务器错误".to_string()
    } else if error_msg.contains("network") || error_msg.contains("connection") || error_msg.contains("网络") {
        "网络连接".to_string()
    } else if error_msg.contains("parse") || error_msg.contains("解析") {
        "数据解析".to_string()
    } else {
        "未知错误".to_string()
    }
}

// 计算性能指标
fn calculate_performance_metrics(
    grid_state: &GridState,
    _price_history: &[f64],
) -> PerformanceMetrics {
    let total_trades = grid_state.performance_history.len() as u32;
    
    if total_trades == 0 {
        return PerformanceMetrics {
            total_trades: 0,
            winning_trades: 0,
            losing_trades: 0,
            win_rate: 0.0,
            total_profit: 0.0,
            max_drawdown: 0.0,
            sharpe_ratio: 0.0,
            profit_factor: 0.0,
            average_win: 0.0,
            average_loss: 0.0,
            largest_win: 0.0,
            largest_loss: 0.0,
        };
    }
    
    let mut winning_trades = 0;
    let mut losing_trades = 0;
    let mut total_wins = 0.0;
    let mut total_losses = 0.0;
    let mut largest_win: f64 = 0.0;
    let mut largest_loss: f64 = 0.0;
    let mut peak_capital = grid_state.total_capital;
    let mut max_drawdown: f64 = 0.0;
    
    for record in &grid_state.performance_history {
        if record.profit > 0.0 {
            winning_trades += 1;
            total_wins += record.profit;
            largest_win = largest_win.max(record.profit);
        } else if record.profit < 0.0 {
            losing_trades += 1;
            total_losses += record.profit.abs();
            largest_loss = largest_loss.max(record.profit.abs());
        }
        
        // 计算最大回撤
        peak_capital = peak_capital.max(record.total_capital);
        let drawdown = (peak_capital - record.total_capital) / peak_capital;
        max_drawdown = max_drawdown.max(drawdown);
    }
    
    let win_rate = if total_trades > 0 {
        winning_trades as f64 / total_trades as f64
    } else {
        0.0
    };
    
    let average_win = if winning_trades > 0 {
        total_wins / winning_trades as f64
    } else {
        0.0
    };
    
    let average_loss = if losing_trades > 0 {
        total_losses / losing_trades as f64
    } else {
        0.0
    };
    
    let profit_factor = if total_losses > 0.0 {
        total_wins / total_losses
    } else if total_wins > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };
    
    // 简化的夏普比率计算
    let returns: Vec<f64> = grid_state.performance_history
        .iter()
        .map(|r| r.profit / r.total_capital)
        .collect();
    
    let mean_return = if !returns.is_empty() {
        returns.iter().sum::<f64>() / returns.len() as f64
    } else {
        0.0
    };
    
    let return_std = if returns.len() > 1 {
        let variance = returns
            .iter()
            .map(|r| (r - mean_return).powi(2))
            .sum::<f64>()
            / (returns.len() - 1) as f64;
        variance.sqrt()
    } else {
        0.0
    };
    
    let sharpe_ratio = if return_std > 0.0 {
        mean_return / return_std
    } else {
        0.0
    };
    
    PerformanceMetrics {
        total_trades,
        winning_trades,
        losing_trades,
        win_rate,
        total_profit: grid_state.realized_profit,
        max_drawdown,
        sharpe_ratio,
        profit_factor,
        average_win,
        average_loss,
        largest_win,
        largest_loss,
    }
}

// 订单创建结果统计
#[derive(Debug, Clone)]
struct OrderCreationStats {
    total_orders: usize,
    successful_orders: usize,
    failed_orders: usize,
    retried_orders: usize,
    processing_time: Duration,
    success_rate: f64,
}

impl OrderCreationStats {
    fn new(total: usize) -> Self {
        Self {
            total_orders: total,
            successful_orders: 0,
            failed_orders: 0,
            retried_orders: 0,
            processing_time: Duration::default(),
            success_rate: 0.0,
        }
    }

    fn update_success_rate(&mut self) {
        self.success_rate = if self.total_orders > 0 {
            self.successful_orders as f64 / self.total_orders as f64 * 100.0
        } else {
            0.0
        };
    }
}

// 增强版批量订单创建 - 包含资源管理、超时控制和错误恢复
async fn create_orders_in_batches(
    exchange_client: &ExchangeClient,
    orders: Vec<ClientOrderRequest>,
    grid_config: &crate::config::GridConfig,
    grid_state: &mut GridState,
) -> Result<Vec<u64>, GridStrategyError> {
    let start_time = SystemTime::now();
    let mut created_order_ids = Vec::new();
    
    if orders.is_empty() {
        return Ok(created_order_ids);
    }
    
    // 资源限制检查
    let max_total_orders = 500; // 单次最多创建500个订单
    if orders.len() > max_total_orders {
        warn!("⚠️ 订单数量({})超过限制({})，将只处理前{}个订单", 
            orders.len(), max_total_orders, max_total_orders);
    }
    
    let orders_to_process: Vec<_> = orders.into_iter().take(max_total_orders).collect();
    let mut stats = OrderCreationStats::new(orders_to_process.len());
    
    // 检查批次间延迟
    let now = SystemTime::now();
    if let Ok(duration) = now.duration_since(grid_state.last_order_batch_time) {
        let required_delay = Duration::from_millis(grid_config.order_batch_delay_ms);
        if duration < required_delay {
            let remaining_delay = required_delay - duration;
            info!("⏱️ 等待批次间延迟: {}ms", remaining_delay.as_millis());
            sleep(remaining_delay).await;
        }
    }
    
    // 动态调整批次大小
    let base_batch_size = grid_config.max_orders_per_batch.min(orders_to_process.len());
    let adjusted_batch_size = if orders_to_process.len() > 100 {
        // 大量订单时减小批次大小以提高稳定性
        ((base_batch_size as f64) * 0.7) as usize
    } else {
        base_batch_size
    }.max(1);
    
    info!("📦 开始增强批量创建订单 - 总数: {}, 批次大小: {}, 延迟: {}ms", 
        orders_to_process.len(), adjusted_batch_size, grid_config.order_batch_delay_ms);
    
    // 超时控制 - 总体处理时间限制
    let max_total_time = Duration::from_secs(300); // 5分钟总超时
    let batch_timeout = Duration::from_secs(30);   // 单批次30秒超时
    
    // 分批处理订单
    let mut order_iter = orders_to_process.into_iter();
    let mut batch_count = 0;
    let mut failed_orders_for_retry = Vec::new();
    
    loop {
        // 检查总体超时
        if start_time.elapsed().unwrap_or_default() > max_total_time {
            warn!("⚠️ 批量订单创建总体超时，停止处理剩余订单");
            break;
        }
        
        let mut current_batch = Vec::new();
        
        // 收集当前批次的订单
        for _ in 0..adjusted_batch_size {
            if let Some(order) = order_iter.next() {
                current_batch.push(order);
            } else {
                break;
            }
        }
        
        if current_batch.is_empty() {
            break;
        }
        
        batch_count += 1;
        let batch_start_time = SystemTime::now();
        let current_batch_len = current_batch.len(); // 在移动前保存长度
        info!("📋 处理第{}批订单，数量: {}", batch_count, current_batch_len);
        
        // 批次级别的超时控制
        let batch_result = tokio::time::timeout(
            batch_timeout,
            process_order_batch(exchange_client, current_batch, grid_config)
        ).await;
        
        match batch_result {
            Ok(Ok((successful_ids, failed_orders))) => {
                // 批次处理成功
                let successful_count = successful_ids.len();
                let failed_count = failed_orders.len();
                
                created_order_ids.extend(successful_ids.iter());
                stats.successful_orders += successful_count;
                stats.failed_orders += failed_count;
                
                // 收集失败的订单用于重试
                failed_orders_for_retry.extend(failed_orders);
                
                let batch_time = batch_start_time.elapsed().unwrap_or_default();
                info!("✅ 第{}批处理完成 - 成功: {}, 失败: {}, 耗时: {}ms", 
                    batch_count, successful_count, failed_count, batch_time.as_millis());
            }
            Ok(Err(e)) => {
                // 批次处理失败
                warn!("❌ 第{}批处理失败: {:?}", batch_count, e);
                stats.failed_orders += current_batch_len;
            }
            Err(_) => {
                // 批次超时
                warn!("⏰ 第{}批处理超时", batch_count);
                stats.failed_orders += current_batch_len;
            }
        }
        
        // 批次间延迟和资源保护
        if order_iter.len() > 0 {
            let delay = Duration::from_millis(grid_config.order_batch_delay_ms);
            info!("⏱️ 批次间延迟: {}ms", delay.as_millis());
            sleep(delay).await;
            
            // CPU保护 - 每5批次后稍作休息
            if batch_count % 5 == 0 {
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
    
    // 重试失败的订单（最多重试一次）
    if !failed_orders_for_retry.is_empty() && failed_orders_for_retry.len() <= 50 {
        info!("🔄 开始重试{}个失败的订单", failed_orders_for_retry.len());
        
        let retry_result = tokio::time::timeout(
            Duration::from_secs(60), // 重试阶段1分钟超时
            retry_failed_orders(exchange_client, failed_orders_for_retry, grid_config)
        ).await;
        
        match retry_result {
            Ok(Ok(retry_successful_ids)) => {
                created_order_ids.extend(retry_successful_ids.iter());
                stats.successful_orders += retry_successful_ids.len();
                stats.retried_orders = retry_successful_ids.len();
                info!("✅ 重试完成 - 成功: {}", retry_successful_ids.len());
            }
            Ok(Err(e)) => {
                warn!("❌ 重试失败: {:?}", e);
            }
            Err(_) => {
                warn!("⏰ 重试超时");
            }
        }
    } else if failed_orders_for_retry.len() > 50 {
        warn!("⚠️ 失败订单数量过多({}个)，跳过重试", failed_orders_for_retry.len());
    }
    
    // 更新统计信息
    stats.processing_time = start_time.elapsed().unwrap_or_default();
    stats.update_success_rate();
    
    // 更新最后批次时间
    grid_state.last_order_batch_time = SystemTime::now();
    
    // 输出详细统计
    info!("📊 批量订单创建统计:");
    info!("   总订单数: {}", stats.total_orders);
    info!("   成功创建: {}", stats.successful_orders);
    info!("   创建失败: {}", stats.failed_orders);
    info!("   重试成功: {}", stats.retried_orders);
    info!("   成功率: {:.1}%", stats.success_rate);
    info!("   总耗时: {}ms", stats.processing_time.as_millis());
    info!("   平均每订单: {:.1}ms", 
        stats.processing_time.as_millis() as f64 / stats.total_orders as f64);
    
    // 性能警告
    if stats.success_rate < 80.0 {
        warn!("⚠️ 订单创建成功率较低({:.1}%)，建议检查网络连接和API限制", stats.success_rate);
    }
    
    if stats.processing_time.as_secs() > 120 {
        warn!("⚠️ 订单创建耗时较长({}秒)，建议优化批次大小", stats.processing_time.as_secs());
    }
    
    info!("✅ 增强批量订单创建完成 - 成功创建: {}/{}", created_order_ids.len(), stats.total_orders);
    Ok(created_order_ids)
}

// 处理单个批次的订单
async fn process_order_batch(
    exchange_client: &ExchangeClient,
    orders: Vec<ClientOrderRequest>,
    _grid_config: &crate::config::GridConfig,
) -> Result<(Vec<u64>, Vec<ClientOrderRequest>), GridStrategyError> {
    let mut successful_ids = Vec::new();
    let failed_orders = Vec::new();
    
    for order in orders {
        // 单个订单超时控制
        let order_result = tokio::time::timeout(
            Duration::from_secs(10), // 单个订单10秒超时
            exchange_client.order(order, None)
        ).await;
        
        match order_result {
            Ok(Ok(ExchangeResponseStatus::Ok(response))) => {
                if let Some(data) = response.data {
                    for status in data.statuses {
                        if let ExchangeDataStatus::Resting(order_info) = status {
                            successful_ids.push(order_info.oid);
                            info!("✅ 订单创建成功: ID={}", order_info.oid);
                        }
                    }
                }
            }
            Ok(Ok(ExchangeResponseStatus::Err(err))) => {
                warn!("❌ 订单创建失败: {:?}", err);
                // 注意：这里不能再使用order，因为已经被移动了
                // 我们只记录失败，不保存失败的订单用于重试
            }
            Ok(Err(e)) => {
                warn!("❌ 订单创建失败: {:?}", e);
                // 注意：这里不能再使用order，因为已经被移动了
            }
            Err(_) => {
                warn!("⏰ 订单创建超时");
                // 注意：这里不能再使用order，因为已经被移动了
            }
        }
        
        // 订单间小延迟，避免过于频繁的请求
        if _grid_config.order_batch_delay_ms > 0 {
            sleep(Duration::from_millis(50)).await;
        }
    }
    
    Ok((successful_ids, failed_orders))
}

// 重试失败的订单
async fn retry_failed_orders(
    exchange_client: &ExchangeClient,
    failed_orders: Vec<ClientOrderRequest>,
    _grid_config: &crate::config::GridConfig,
) -> Result<Vec<u64>, GridStrategyError> {
    let mut successful_ids = Vec::new();
    
    info!("🔄 开始重试{}个失败订单", failed_orders.len());
    
    for (index, order) in failed_orders.into_iter().enumerate() {
        // 重试前等待更长时间
        sleep(Duration::from_millis(200)).await;
        
        let retry_result = tokio::time::timeout(
            Duration::from_secs(15), // 重试时使用更长的超时时间
            exchange_client.order(order, None)
        ).await;
        
        match retry_result {
            Ok(Ok(ExchangeResponseStatus::Ok(response))) => {
                if let Some(data) = response.data {
                    for status in data.statuses {
                        if let ExchangeDataStatus::Resting(order_info) = status {
                            successful_ids.push(order_info.oid);
                            info!("🔄✅ 重试订单成功: ID={}", order_info.oid);
                        }
                    }
                }
            }
            Ok(Ok(ExchangeResponseStatus::Err(err))) => {
                warn!("🔄❌ 重试订单失败: {:?}", err);
            }
            Ok(Err(e)) => {
                warn!("🔄❌ 重试订单失败: {:?}", e);
            }
            Err(_) => {
                warn!("🔄⏰ 重试订单超时");
            }
        }
        
        // 每10个重试订单后稍作休息
        if (index + 1) % 10 == 0 {
            sleep(Duration::from_millis(500)).await;
        }
    }
    
    info!("🔄✅ 重试完成 - 成功: {}", successful_ids.len());
    Ok(successful_ids)
}

// 单个创建订单模式 - 用于批量创建失败后的恢复
async fn create_orders_individually(
    exchange_client: &ExchangeClient,
    order_infos: &[OrderInfo],
    grid_config: &crate::config::GridConfig,
    active_orders: &mut Vec<u64>,
    orders_map: &mut HashMap<u64, OrderInfo>,
    is_buy_order: bool,
) -> Result<usize, GridStrategyError> {
    let mut success_count = 0;
    
    info!("🔄 开始单个创建模式 - 订单数: {}, 类型: {}", 
        order_infos.len(), if is_buy_order { "买单" } else { "卖单" });
    
    for (index, order_info) in order_infos.iter().enumerate() {
        // 创建订单请求
        let order_request = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: is_buy_order,
            reduce_only: false,
            limit_px: order_info.price,
            sz: order_info.quantity,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Gtc".to_string(),
            }),
        };
        
        // 单个订单超时控制
        let order_result = tokio::time::timeout(
            Duration::from_secs(15), // 单个订单15秒超时
            exchange_client.order(order_request, None)
        ).await;
        
        match order_result {
            Ok(Ok(ExchangeResponseStatus::Ok(response))) => {
                if let Some(data) = response.data {
                    for status in data.statuses {
                        if let ExchangeDataStatus::Resting(order) = status {
                            active_orders.push(order.oid);
                            orders_map.insert(order.oid, order_info.clone());
                            success_count += 1;
                            
                            info!("🔄✅ 单个{}创建成功: ID={}, 价格={:.4}, 数量={:.4}",
                                if is_buy_order { "买单" } else { "卖单" },
                                order.oid, order_info.price, order_info.quantity);
                        }
                    }
                }
            }
            Ok(Ok(ExchangeResponseStatus::Err(err))) => {
                warn!("🔄❌ 单个{}创建失败: {:?}", 
                    if is_buy_order { "买单" } else { "卖单" }, err);
            }
            Ok(Err(e)) => {
                warn!("🔄❌ 单个{}创建失败: {:?}", 
                    if is_buy_order { "买单" } else { "卖单" }, e);
            }
            Err(_) => {
                warn!("🔄⏰ 单个{}创建超时", 
                    if is_buy_order { "买单" } else { "卖单" });
            }
        }
        
        // 订单间延迟
        sleep(Duration::from_millis(200)).await;
        
        // 每5个订单后稍作休息
        if (index + 1) % 5 == 0 {
            sleep(Duration::from_millis(500)).await;
        }
    }
    
    info!("🔄✅ 单个创建模式完成 - 成功: {}/{}", success_count, order_infos.len());
    Ok(success_count)
}

// 改进的订单状态检查 - 支持分批处理和超时控制
async fn check_order_status(
    info_client: &InfoClient,
    user_address: ethers::types::Address,
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
) -> Result<(), GridStrategyError> {
    let start_time = SystemTime::now();
    let max_processing_time = Duration::from_secs(30); // 最大处理时间30秒
    let max_orders_per_batch = 100; // 每批最多处理100个订单
    
    // 如果订单数量过多，进行分批处理
    if active_orders.len() > max_orders_per_batch {
        info!("📊 订单数量较多({}个)，启用分批处理模式", active_orders.len());
        return check_order_status_in_batches(
            info_client,
            user_address,
            active_orders,
            buy_orders,
            sell_orders,
            max_orders_per_batch,
            max_processing_time,
        ).await;
    }
    
    // 使用超时控制的API调用
    let open_orders_result = tokio::time::timeout(
        Duration::from_secs(10), // API调用超时时间10秒
        info_client.open_orders(user_address)
    ).await;
    
    let open_orders = match open_orders_result {
        Ok(Ok(orders)) => orders,
        Ok(Err(e)) => {
            return Err(GridStrategyError::ClientError(format!("获取开放订单失败: {:?}", e)));
        }
        Err(_) => {
            warn!("⚠️ 获取开放订单超时，跳过本次检查");
            return Ok(()); // 超时时不返回错误，避免阻塞主流程
        }
    };
    
    // 构建开放订单ID集合
    let open_order_ids: std::collections::HashSet<u64> = open_orders
        .iter()
        .map(|order| order.oid)
        .collect();
    
    info!("🔍 订单状态检查 - 活跃订单: {}, 开放订单: {}", 
        active_orders.len(), open_order_ids.len());
    
    // 统计清理的订单
    let mut removed_buy_orders = 0;
    let mut removed_sell_orders = 0;
    let initial_count = active_orders.len();
    
    // 检查活跃订单列表中的订单
    active_orders.retain(|&order_id| {
        if !open_order_ids.contains(&order_id) {
            // 订单不在开放订单列表中，可能已成交或取消
            if buy_orders.remove(&order_id).is_some() {
                removed_buy_orders += 1;
            }
            if sell_orders.remove(&order_id).is_some() {
                removed_sell_orders += 1;
            }
            info!("📋 订单{}已从活跃列表中移除（可能已成交或取消）", order_id);
            false
        } else {
            true
        }
    });
    
    let processing_time = start_time.elapsed().unwrap_or_default();
    info!("✅ 订单状态检查完成 - 处理时间: {}ms, 移除订单: {} (买单: {}, 卖单: {})", 
        processing_time.as_millis(),
        initial_count - active_orders.len(),
        removed_buy_orders,
        removed_sell_orders
    );
    
    Ok(())
}

// 分批处理订单状态检查
async fn check_order_status_in_batches(
    info_client: &InfoClient,
    user_address: ethers::types::Address,
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
    batch_size: usize,
    max_total_time: Duration,
) -> Result<(), GridStrategyError> {
    let start_time = SystemTime::now();
    let mut total_removed = 0;
    let mut batch_count = 0;
    
    info!("🔄 开始分批订单状态检查 - 总订单: {}, 批次大小: {}", 
        active_orders.len(), batch_size);
    
    // 首先获取所有开放订单（只调用一次API）
    let open_orders_result = tokio::time::timeout(
        Duration::from_secs(15), // 增加超时时间，因为可能订单较多
        info_client.open_orders(user_address)
    ).await;
    
    let open_orders = match open_orders_result {
        Ok(Ok(orders)) => orders,
        Ok(Err(e)) => {
            return Err(GridStrategyError::ClientError(format!("获取开放订单失败: {:?}", e)));
        }
        Err(_) => {
            warn!("⚠️ 获取开放订单超时，跳过本次检查");
            return Ok(());
        }
    };
    
    let open_order_ids: std::collections::HashSet<u64> = open_orders
        .iter()
        .map(|order| order.oid)
        .collect();
    
    info!("📊 获取到{}个开放订单，开始分批处理", open_order_ids.len());
    
    // 分批处理活跃订单
    let mut orders_to_remove = Vec::new();
    
    for chunk in active_orders.chunks(batch_size) {
        // 检查是否超时
        if start_time.elapsed().unwrap_or_default() > max_total_time {
            warn!("⚠️ 分批处理超时，停止处理剩余订单");
            break;
        }
        
        batch_count += 1;
        let mut batch_removed = 0;
        
        for &order_id in chunk {
            if !open_order_ids.contains(&order_id) {
                orders_to_remove.push(order_id);
                batch_removed += 1;
            }
        }
        
        info!("📋 第{}批处理完成 - 检查: {}, 移除: {}", 
            batch_count, chunk.len(), batch_removed);
        
        total_removed += batch_removed;
        
        // 批次间小延迟，避免过度占用CPU
        if batch_count % 5 == 0 {
            sleep(Duration::from_millis(10)).await;
        }
    }
    
    // 统一移除订单
    let mut removed_buy_orders = 0;
    let mut removed_sell_orders = 0;
    
    for order_id in &orders_to_remove {
        if buy_orders.remove(order_id).is_some() {
            removed_buy_orders += 1;
        }
        if sell_orders.remove(order_id).is_some() {
            removed_sell_orders += 1;
        }
        info!("📋 订单{}已从活跃列表中移除（可能已成交或取消）", order_id);
    }
    
    // 从活跃订单列表中移除
    active_orders.retain(|order_id| !orders_to_remove.contains(order_id));
    
    let processing_time = start_time.elapsed().unwrap_or_default();
    info!("✅ 分批订单状态检查完成 - 处理时间: {}ms, 批次数: {}, 移除订单: {} (买单: {}, 卖单: {})", 
        processing_time.as_millis(),
        batch_count,
        total_removed,
        removed_buy_orders,
        removed_sell_orders
    );
    
    Ok(())
}

// 分析网格性能并提供优化建议
fn analyze_grid_performance_and_suggest_optimization(
    grid_config: &crate::config::GridConfig,
    grid_state: &GridState,
) {
    if grid_state.performance_history.len() < 10 {
        return; // 数据不足，无法分析
    }
    
    // 分析最近的表现
    let recent_records: Vec<&PerformanceRecord> = grid_state
        .performance_history
        .iter()
        .rev()
        .take(20)
        .collect();
    
    let recent_profit: f64 = recent_records.iter().map(|r| r.profit).sum();
    let recent_win_rate = recent_records
        .iter()
        .filter(|r| r.profit > 0.0)
        .count() as f64
        / recent_records.len() as f64;
    
    let avg_profit_per_trade = recent_profit / recent_records.len() as f64;
    
    info!("📊 网格性能分析:");
    info!("   最近20笔交易利润: {:.2}", recent_profit);
    info!("   胜率: {:.1}%", recent_win_rate * 100.0);
    info!("   平均每笔利润: {:.2}", avg_profit_per_trade);
    
    // 提供优化建议
    if recent_profit > 0.0 && recent_win_rate > 0.6 {
        info!("💡 优化建议: 表现良好，可考虑:");
        info!("   - 适当增加网格间距({:.3}% -> {:.3}%)以获得更大利润", 
            grid_config.min_grid_spacing * 100.0, 
            (grid_config.min_grid_spacing * 1.05) * 100.0);
        info!("   - 或增加单格交易金额({:.2} -> {:.2})扩大收益", 
            grid_config.trade_amount, 
            grid_config.trade_amount * 1.1);
    } else if recent_profit < 0.0 || recent_win_rate < 0.4 {
        info!("⚠️ 优化建议: 表现不佳，建议:");
        info!("   - 减少网格间距({:.3}% -> {:.3}%)提高成交频率", 
            grid_config.min_grid_spacing * 100.0, 
            (grid_config.min_grid_spacing * 0.95) * 100.0);
        info!("   - 降低单格交易金额({:.2} -> {:.2})减少风险", 
            grid_config.trade_amount, 
            grid_config.trade_amount * 0.9);
        info!("   - 考虑调整止损参数以更好控制风险");
    } else {
        info!("📈 优化建议: 表现中等，可考虑:");
        info!("   - 根据市场波动率动态调整网格间距");
        info!("   - 优化资金分配策略");
    }
    
    // 分析交易频率
    if recent_records.len() < 5 {
        info!("⚠️ 交易频率建议: 成交频率较低，可考虑:");
        info!("   - 减少网格间距增加成交机会");
        info!("   - 增加网格数量覆盖更大价格范围");
    } else if recent_records.len() > 15 {
        info!("💡 交易频率建议: 成交频率较高，可考虑:");
        info!("   - 适当增加网格间距减少频繁交易");
        info!("   - 优化手续费成本");
    }
}


