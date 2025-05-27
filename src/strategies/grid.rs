use ethers::signers::{LocalWallet, Signer};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient, InfoClient,
    ClientCancelRequest, ExchangeDataStatus, ExchangeResponseStatus, Message, Subscription, UserData,
};
use log::{error, info, warn};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::sleep;
use thiserror::Error;

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
}

// 订单信息结构体
#[derive(Debug, Clone)]
struct OrderInfo {
    price: f64,
    quantity: f64,
    cost_price: Option<f64>, // 对于卖单，记录对应的买入成本价
    potential_sell_price: Option<f64>, // 对于买单，记录潜在卖出价格
    allocated_funds: f64, // 分配的资金
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
    trailing_stop_price: f64, // 浮动止损价
    stop_loss_status: String, // 止损状态
    last_rebalance_time: SystemTime,
    historical_volatility: f64,
}

// 市场分析结果
#[derive(Debug, Clone)]
struct MarketAnalysis {
    volatility: f64,
    trend: String, // "上升", "下降", "震荡"
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

// 止损检查结果
#[derive(Debug, Clone)]
struct StopLossResult {
    action: String, // "正常", "部分止损", "已止损"
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
    
    for i in 0..klines.len()-1 {
        let change = (klines[i+1] - klines[i]) / klines[i];
        if change > 0.0 {
            positive_amplitudes.push(change);
        } else {
            negative_amplitudes.push(change.abs());
        }
    }
    
    let avg_positive = if !positive_amplitudes.is_empty() {
        positive_amplitudes.iter().sum::<f64>() / positive_amplitudes.len() as f64
    } else { 0.0 };
    
    let avg_negative = if !negative_amplitudes.is_empty() {
        negative_amplitudes.iter().sum::<f64>() / negative_amplitudes.len() as f64
    } else { 0.0 };
    
    (avg_positive, avg_negative)
}

// 计算市场波动率
fn calculate_market_volatility(price_history: &[f64]) -> f64 {
    if price_history.len() < 2 {
        return 0.0;
    }
    
    let mut price_changes = Vec::new();
    for i in 1..price_history.len() {
        let change = (price_history[i] - price_history[i-1]) / price_history[i-1];
        price_changes.push(change);
    }
    
    if price_changes.is_empty() {
        return 0.0;
    }
    
    // 计算标准差
    let mean = price_changes.iter().sum::<f64>() / price_changes.len() as f64;
    let variance = price_changes.iter()
        .map(|x| (x - mean).powi(2))
        .sum::<f64>() / price_changes.len() as f64;
    
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
        let change = prices[i] - prices[i-1];
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
            trend: "震荡".to_string(),
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
        "上升".to_string()
    } else if short_ma < long_ma * 0.95 && rsi < 45.0 {
        "下降".to_string()
    } else {
        "震荡".to_string()
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
        ((current_price - grid_config.min_grid_spacing) / price_range).max(0.0).min(1.0)
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
    _grid_config: &crate::config::GridConfig,
    price_history: &[f64],
) -> StopLossResult {
    // 1. 总资产止损
    let current_total_value = grid_state.available_funds + grid_state.position_quantity * current_price;
    let total_stop_threshold = grid_state.total_capital * 0.85; // 亏损15%触发止损
    
    if current_total_value < total_stop_threshold {
        warn!("🚨 触发总资产止损 - 当前总资产: {:.2}, 止损阈值: {:.2}", 
            current_total_value, total_stop_threshold);
        
        return StopLossResult {
            action: "已止损".to_string(),
            reason: "总资产亏损超过15%".to_string(),
            stop_quantity: grid_state.position_quantity,
        };
    }
    
    // 2. 浮动止损 (Trailing Stop)
    if grid_state.position_quantity > 0.0 {
        // 初始化最高价和止损价
        if grid_state.highest_price_after_position < grid_state.position_avg_price {
            grid_state.highest_price_after_position = grid_state.position_avg_price;
            grid_state.trailing_stop_price = grid_state.position_avg_price * 0.9;
        }
        
        // 更新最高价和浮动止损价
        if current_price > grid_state.highest_price_after_position {
            grid_state.highest_price_after_position = current_price;
            grid_state.trailing_stop_price = current_price * 0.9;
            info!("📈 更新浮动止损 - 新最高价: {:.4}, 新止损价: {:.4}", 
                grid_state.highest_price_after_position, grid_state.trailing_stop_price);
        }
        
        // 检查是否触发浮动止损
        if current_price < grid_state.trailing_stop_price {
            warn!("🚨 触发浮动止损 - 当前价格: {:.4}, 止损价: {:.4}", 
                current_price, grid_state.trailing_stop_price);
            
            let stop_quantity = grid_state.position_quantity * 0.5; // 止损一半持仓
            grid_state.highest_price_after_position = current_price;
            grid_state.trailing_stop_price = current_price * 0.9;
            
            return StopLossResult {
                action: "部分止损".to_string(),
                reason: "触发浮动止损".to_string(),
                stop_quantity,
            };
        }
    }
    
    // 3. 单笔持仓止损
    if grid_state.position_quantity > 0.0 && grid_state.position_avg_price > 0.0 {
        let position_loss_rate = (current_price - grid_state.position_avg_price) / grid_state.position_avg_price;
        
        if position_loss_rate < -0.1 { // 亏损超过10%
            warn!("🚨 触发单笔持仓止损 - 持仓均价: {:.4}, 当前价格: {:.4}, 亏损率: {:.2}%", 
                grid_state.position_avg_price, current_price, position_loss_rate * 100.0);
            
            let stop_quantity = grid_state.position_quantity * 0.3; // 止损30%持仓
            
            return StopLossResult {
                action: "部分止损".to_string(),
                reason: "单笔持仓亏损超过10%".to_string(),
                stop_quantity,
            };
        }
    }
    
    // 4. 加速下跌止损
    if price_history.len() >= 5 {
        let recent_price = price_history[price_history.len() - 1];
        let old_price = price_history[price_history.len() - 5];
        let short_term_change = (recent_price - old_price) / old_price;
        
        if short_term_change < -0.05 && grid_state.position_quantity > 0.0 { // 5分钟内下跌超过5%
            warn!("🚨 触发加速下跌止损 - 5分钟价格变化率: {:.2}%", short_term_change * 100.0);
            
            let stop_ratio = (short_term_change.abs() * 5.0).min(0.5); // 最大止损50%
            let stop_quantity = grid_state.position_quantity * stop_ratio;
            
            return StopLossResult {
                action: "部分止损".to_string(),
                reason: format!("加速下跌{}%", short_term_change.abs() * 100.0),
                stop_quantity,
            };
        }
    }
    
    StopLossResult {
        action: "正常".to_string(),
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
        return Err(GridStrategyError::ConfigError("总资金必须大于0".to_string()));
    }
    
    if grid_config.trade_amount <= 0.0 {
        return Err(GridStrategyError::ConfigError("每格交易金额必须大于0".to_string()));
    }
    
    if grid_config.trade_amount > grid_config.total_capital {
        return Err(GridStrategyError::ConfigError("每格交易金额不能超过总资金".to_string()));
    }
    
    if grid_config.max_position <= 0.0 {
        return Err(GridStrategyError::ConfigError("最大持仓必须大于0".to_string()));
    }
    
    if grid_config.grid_count == 0 {
        return Err(GridStrategyError::ConfigError("网格数量必须大于0".to_string()));
    }
    
    // 检查网格间距
    if grid_config.min_grid_spacing <= 0.0 {
        return Err(GridStrategyError::ConfigError("最小网格间距必须大于0".to_string()));
    }
    
    if grid_config.max_grid_spacing <= grid_config.min_grid_spacing {
        return Err(GridStrategyError::ConfigError("最大网格间距必须大于最小网格间距".to_string()));
    }
    
    // 检查手续费率
    if grid_config.fee_rate < 0.0 || grid_config.fee_rate > 0.1 {
        return Err(GridStrategyError::ConfigError("手续费率必须在0-10%之间".to_string()));
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
        return Err(GridStrategyError::ConfigError("最大回撤必须在0-100%之间".to_string()));
    }
    
    if grid_config.max_single_loss <= 0.0 || grid_config.max_single_loss > 1.0 {
        return Err(GridStrategyError::ConfigError("单笔最大亏损必须在0-100%之间".to_string()));
    }
    
    if grid_config.max_daily_loss <= 0.0 || grid_config.max_daily_loss > 1.0 {
        return Err(GridStrategyError::ConfigError("每日最大亏损必须在0-100%之间".to_string()));
    }
    
    // 检查杠杆倍数
    if grid_config.leverage == 0 || grid_config.leverage > 100 {
        return Err(GridStrategyError::ConfigError("杠杆倍数必须在1-100之间".to_string()));
    }
    
    // 检查精度设置
    if grid_config.price_precision > 8 {
        return Err(GridStrategyError::ConfigError("价格精度不能超过8位小数".to_string()));
    }
    
    if grid_config.quantity_precision > 8 {
        return Err(GridStrategyError::ConfigError("数量精度不能超过8位小数".to_string()));
    }
    
    // 检查时间参数
    if grid_config.check_interval == 0 {
        return Err(GridStrategyError::ConfigError("检查间隔必须大于0秒".to_string()));
    }
    
    if grid_config.max_holding_time == 0 {
        return Err(GridStrategyError::ConfigError("最大持仓时间必须大于0秒".to_string()));
    }
    
    // 检查保证金使用率
    if grid_config.margin_usage_threshold <= 0.0 || grid_config.margin_usage_threshold > 1.0 {
        return Err(GridStrategyError::ConfigError("保证金使用率阈值必须在0-100%之间".to_string()));
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
    let min_sell_price = calculate_min_sell_price(fill_price, grid_config.fee_rate, grid_config.min_profit / fill_price);
    let actual_sell_price = base_sell_price.max(min_sell_price);
    let formatted_sell_price = format_price(actual_sell_price, grid_config.price_precision);
    
    // 检查是否超出网格上限
    let upper_limit = fill_price * (1.0 + grid_config.max_grid_spacing * grid_config.grid_count as f64);
    if formatted_sell_price > upper_limit {
        warn!("⚠️ 卖出价格({:.4})超出网格上限({:.4})，可能影响网格完整性", formatted_sell_price, upper_limit);
    }
    
    // 考虑买入时的手续费损失，调整卖出数量
    let sell_quantity = format_price(fill_size * (1.0 - grid_config.fee_rate), grid_config.quantity_precision);
    
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
                        info!("🔴【对冲卖单】✅ 卖单已提交: ID={}, 价格={}, 数量={}, 成本价={}", 
                            order.oid, formatted_sell_price, sell_quantity, fill_price);
                        active_orders.push(order.oid);
                        sell_orders.insert(order.oid, OrderInfo {
                            price: formatted_sell_price,
                            quantity: sell_quantity,
                            cost_price: Some(fill_price),
                            potential_sell_price: None,
                            allocated_funds: 0.0,
                        });
                    }
                }
            }
        },
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
                        info!("🟢【重建买单】✅ 买单已提交: ID={}, 价格={}, 数量={}", 
                            order.oid, fill_price, fill_size);
                        active_orders.push(order.oid);
                        buy_orders.insert(order.oid, OrderInfo {
                            price: fill_price,
                            quantity: fill_size,
                            cost_price: None,
                            potential_sell_price: None,
                            allocated_funds: 0.0,
                        });
                    }
                }
            }
        },
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
    info!("🔴 处理卖单成交: 价格={}, 数量={}, 成本价={:?}", fill_price, fill_size, cost_price);
    
    // 计算实际利润
    let actual_cost_price = cost_price.unwrap_or_else(|| {
        let estimated = fill_price - grid_spacing * fill_price;
        warn!("⚠️ 未找到成本价，估算为: {:.4}", estimated);
        estimated
    });
    
    let actual_profit_rate = calculate_expected_profit_rate(actual_cost_price, fill_price, grid_config.fee_rate);
    
    info!("💰 交易完成 - 成本价: {:.4}, 卖出价: {:.4}, 利润率: {:.4}%", 
        actual_cost_price, fill_price, actual_profit_rate * 100.0);
    
    // 计算潜在买入价格
    let base_buy_price = fill_price * (1.0 - grid_spacing);
    let formatted_buy_price = format_price(base_buy_price, grid_config.price_precision);
    
    // 检查新买入点的预期利润率
    let potential_sell_price = formatted_buy_price * (1.0 + grid_spacing);
    let expected_profit_rate = calculate_expected_profit_rate(formatted_buy_price, potential_sell_price, grid_config.fee_rate);
    let min_profit_rate = grid_config.min_profit / (formatted_buy_price * grid_config.trade_amount / formatted_buy_price);
    
    if expected_profit_rate >= min_profit_rate {
        let buy_quantity = format_price(grid_config.trade_amount / formatted_buy_price, grid_config.quantity_precision);
        
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
                            buy_orders.insert(order.oid, OrderInfo {
                                price: formatted_buy_price,
                                quantity: buy_quantity,
                                cost_price: None,
                                potential_sell_price: None,
                                allocated_funds: 0.0,
                            });
                        }
                    }
                }
            },
            Ok(ExchangeResponseStatus::Err(e)) => warn!("❌ 新买单失败: {:?}", e),
            Err(e) => warn!("❌ 新买单失败: {:?}", e),
        }
    } else {
        warn!("⚠️ 网格点 {:.4} 的预期利润率({:.4}%)不满足最小要求({:.4}%)，跳过此买单", 
            formatted_buy_price, expected_profit_rate * 100.0, min_profit_rate * 100.0);
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
                            info!("🔴【重建卖单】✅ 卖单已提交: ID={}, 价格={}, 数量={}", 
                                order.oid, fill_price, fill_size);
                            active_orders.push(order.oid);
                            // 估算新卖单的成本价（当前价格减去网格间距）
                            let estimated_cost_price = fill_price * (1.0 - grid_spacing);
                            sell_orders.insert(order.oid, OrderInfo {
                                price: fill_price,
                                quantity: fill_size,
                                cost_price: Some(estimated_cost_price),
                                potential_sell_price: None,
                                allocated_funds: 0.0,
                            });
                        }
                    }
                }
            },
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
        let order = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: false,
            reduce_only: true,
            limit_px: current_price,
            sz: long_position,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Gtc".to_string(),
            }),
        };
        if let Err(e) = exchange_client.order(order, None).await {
            return Err(GridStrategyError::OrderError(format!("清仓多头失败: {:?}", e)));
        }
    }
    
    if short_position > 0.0 {
        let order = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: true,
            reduce_only: true,
            limit_px: current_price,
            sz: short_position,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Gtc".to_string(),
            }),
        };
        if let Err(e) = exchange_client.order(order, None).await {
            return Err(GridStrategyError::OrderError(format!("清仓空头失败: {:?}", e)));
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
    let mut fund_allocation = calculate_dynamic_fund_allocation(grid_state, current_price, grid_config);
    
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
    
    info!("💰 资金分配 - 买单资金: {:.2}, 卖单资金: {:.2}, 持仓比例: {:.2}%, 振幅调整: {:.2}", 
        fund_allocation.buy_order_funds, fund_allocation.sell_order_funds, 
        fund_allocation.position_ratio * 100.0, amplitude_adjustment);
    
    // 创建买单 - 价格递减
    let mut current_buy_price = current_price;
    let max_buy_funds = grid_state.available_funds * 0.7; // 最多使用70%资金做买单
    let mut allocated_buy_funds = 0.0;
    let mut buy_count = 0;
    
    while current_buy_price > current_price * 0.8 && allocated_buy_funds < max_buy_funds && buy_count < grid_config.grid_count {
        // 动态计算网格间距，应用振幅调整
        let dynamic_spacing = grid_config.min_grid_spacing * fund_allocation.buy_spacing_adjustment * amplitude_adjustment;
        current_buy_price = current_buy_price - (current_buy_price * dynamic_spacing);
        
        // 计算当前网格资金
        let mut current_grid_funds = fund_allocation.buy_order_funds * 
            (1.0 - (current_price - current_buy_price) / current_price * 3.0);
        current_grid_funds = current_grid_funds.max(fund_allocation.buy_order_funds * 0.5);
        
        // 检查资金限制
        if allocated_buy_funds + current_grid_funds > max_buy_funds {
            current_grid_funds = max_buy_funds - allocated_buy_funds;
        }
        
        if current_grid_funds < fund_allocation.buy_order_funds * 0.1 {
            break; // 资金太少，停止创建买单
        }
        
        let buy_quantity = format_price(current_grid_funds / current_buy_price, grid_config.quantity_precision);
        
        // 验证潜在利润
        let potential_sell_price = current_buy_price * (1.0 + dynamic_spacing);
        let expected_profit_rate = calculate_expected_profit_rate(current_buy_price, potential_sell_price, grid_config.fee_rate);
        
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
            
                         match exchange_client.order(buy_order, None).await {
                 Ok(ExchangeResponseStatus::Ok(response)) => {
                     if let Some(data) = response.data {
                         if !data.statuses.is_empty() {
                             if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                                 active_orders.push(order.oid);
                                 buy_orders.insert(order.oid, OrderInfo {
                                     price: formatted_price,
                                     quantity: buy_quantity,
                                     cost_price: None,
                                     potential_sell_price: Some(potential_sell_price),
                                     allocated_funds: current_grid_funds,
                                 });
                                 allocated_buy_funds += current_grid_funds;
                                 buy_count += 1;
                                 
                                 info!("🟢 创建买单: 价格={:.4}, 数量={:.4}, 资金={:.2}", 
                                     formatted_price, buy_quantity, current_grid_funds);
                             }
                         }
                     }
                 }
                 Ok(ExchangeResponseStatus::Err(err)) => {
                     warn!("❌ 创建买单失败: {:?}", err);
                 }
                 Err(e) => {
                     warn!("❌ 创建买单失败: {:?}", e);
                 }
             }
        }
    }
    
    // 创建卖单 - 价格递增
    let mut current_sell_price = current_price;
    let max_sell_quantity = grid_state.position_quantity * 0.8; // 最多卖出80%持仓
    let mut allocated_sell_quantity = 0.0;
    let mut sell_count = 0;
    
    while current_sell_price < current_price * 1.2 && allocated_sell_quantity < max_sell_quantity && sell_count < grid_config.grid_count {
        // 动态计算网格间距，应用振幅调整
        let dynamic_spacing = grid_config.min_grid_spacing * fund_allocation.sell_spacing_adjustment * amplitude_adjustment;
        current_sell_price = current_sell_price + (current_sell_price * dynamic_spacing);
        
        // 计算卖单数量
        let price_coefficient = (current_sell_price - current_price) / current_price;
        let mut current_grid_quantity = fund_allocation.sell_order_funds / current_sell_price * (1.0 + price_coefficient);
        
        // 确保不超过可售数量
        if allocated_sell_quantity + current_grid_quantity > max_sell_quantity {
            current_grid_quantity = max_sell_quantity - allocated_sell_quantity;
        }
        
        if current_grid_quantity * current_sell_price < fund_allocation.sell_order_funds * 0.1 {
            break; // 价值太小，停止创建卖单
        }
        
        // 验证利润要求
        if grid_state.position_avg_price > 0.0 {
            let sell_profit_rate = (current_sell_price * (1.0 - grid_config.fee_rate) - grid_state.position_avg_price) / grid_state.position_avg_price;
            let min_required_price = grid_state.position_avg_price * (1.0 + grid_config.min_profit / grid_state.position_avg_price) / (1.0 - grid_config.fee_rate);
            
            if sell_profit_rate < grid_config.min_profit / grid_state.position_avg_price && current_sell_price < min_required_price {
                current_sell_price = min_required_price;
            }
        }
        
        if current_grid_quantity > 0.0 {
            let formatted_price = format_price(current_sell_price, grid_config.price_precision);
            let formatted_quantity = format_price(current_grid_quantity, grid_config.quantity_precision);
            
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
            
                         match exchange_client.order(sell_order, None).await {
                 Ok(ExchangeResponseStatus::Ok(response)) => {
                     if let Some(data) = response.data {
                         if !data.statuses.is_empty() {
                             if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                                 active_orders.push(order.oid);
                                 sell_orders.insert(order.oid, OrderInfo {
                                     price: formatted_price,
                                     quantity: formatted_quantity,
                                     cost_price: Some(grid_state.position_avg_price),
                                     potential_sell_price: None,
                                     allocated_funds: 0.0,
                                 });
                                 allocated_sell_quantity += formatted_quantity;
                                 sell_count += 1;
                                 
                                 info!("🔴 创建卖单: 价格={:.4}, 数量={:.4}", 
                                     formatted_price, formatted_quantity);
                             }
                         }
                     }
                 }
                 Ok(ExchangeResponseStatus::Err(err)) => {
                     warn!("❌ 创建卖单失败: {:?}", err);
                 }
                 Err(e) => {
                     warn!("❌ 创建卖单失败: {:?}", e);
                 }
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
) -> Result<(), GridStrategyError> {
    info!("🚨 执行止损操作: {}, 原因: {}, 止损数量: {:.4}", 
        stop_result.action, stop_result.reason, stop_result.stop_quantity);
    
    if stop_result.action == "已止损" {
        // 使用专门的清仓函数
        if grid_state.position_quantity > 0.0 {
            // 估算当前价格（使用更安全的方法）
            let current_price = if grid_state.available_funds > 0.0 && grid_state.position_quantity > 0.0 {
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
            ).await {
                Ok(_) => {
                    info!("✅ 全部清仓完成，数量: {:.4}", grid_state.position_quantity);
                    grid_state.position_quantity = 0.0;
                    grid_state.position_avg_price = 0.0;
                    grid_state.stop_loss_status = "已清仓".to_string();
                }
                Err(e) => {
                    error!("❌ 全部清仓失败: {:?}", e);
                    grid_state.stop_loss_status = "清仓失败".to_string();
                    return Err(e);
                }
            }
        }
        
        // 取消所有订单
        cancel_all_orders(exchange_client, active_orders).await?;
        buy_orders.clear();
        sell_orders.clear();
        
    } else if stop_result.action == "部分止损" && stop_result.stop_quantity > 0.0 {
        // 部分清仓
        let market_sell_order = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: false,
            reduce_only: true,
            limit_px: 0.0, // 市价单
            sz: stop_result.stop_quantity,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Ioc".to_string(),
            }),
        };
        
        match exchange_client.order(market_sell_order, None).await {
            Ok(_) => {
                info!("✅ 部分清仓完成，数量: {:.4}", stop_result.stop_quantity);
                grid_state.position_quantity -= stop_result.stop_quantity;
                
                                 // 取消部分高价位卖单
                 let sell_orders_vec: Vec<_> = sell_orders.iter().map(|(k, v)| (*k, v.clone())).collect();
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
                return Err(GridStrategyError::OrderError(format!("部分清仓失败: {:?}", e)));
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
    
    info!("📊 市场分析 - 波动率: {:.4}, 趋势: {}, RSI: {:.2}", 
        market_analysis.volatility, market_analysis.trend, market_analysis.rsi);
    
    // 更新历史波动率（使用移动平均方式平滑更新）
    if grid_state.historical_volatility == 0.0 {
        grid_state.historical_volatility = market_analysis.volatility;
    } else {
        grid_state.historical_volatility = grid_state.historical_volatility * 0.7 + market_analysis.volatility * 0.3;
    }
    
    // 根据利润表现调整风险系数
    let profit_rate = grid_state.realized_profit / grid_state.total_capital;
    let risk_adjustment = if profit_rate > 0.05 { // 利润>5%
        info!("📈 利润表现良好({:.2}%)，提高风险系数", profit_rate * 100.0);
        1.1 // 提高风险系数
    } else if profit_rate < 0.01 { // 利润<1%
        info!("📉 利润表现不佳({:.2}%)，降低风险系数", profit_rate * 100.0);
        0.9 // 降低风险系数
    } else {
        1.0 // 保持默认风险系数
    };
    
    // 应用风险调整到网格参数
    grid_state.historical_volatility *= risk_adjustment;
    
    // 根据市场分析和风险调整动态调整策略参数
    let mut adjusted_fund_allocation = calculate_dynamic_fund_allocation(grid_state, current_price, grid_config);
    
    // 根据趋势调整网格策略
    match market_analysis.trend.as_str() {
        "上升" => {
            // 上升趋势：增加买单密度，减少卖单密度
            adjusted_fund_allocation.buy_spacing_adjustment *= 0.8 * risk_adjustment;
            adjusted_fund_allocation.sell_spacing_adjustment *= 1.2;
            info!("📈 检测到上升趋势，调整买单密度");
        }
        "下降" => {
            // 下降趋势：减少买单密度，增加卖单密度
            adjusted_fund_allocation.buy_spacing_adjustment *= 1.2;
            adjusted_fund_allocation.sell_spacing_adjustment *= 0.8 * risk_adjustment;
            info!("📉 检测到下降趋势，调整卖单密度");
        }
        "震荡" => {
            // 震荡趋势：保持均衡的网格密度，应用风险调整
            adjusted_fund_allocation.buy_spacing_adjustment *= risk_adjustment;
            adjusted_fund_allocation.sell_spacing_adjustment *= risk_adjustment;
            info!("📊 检测到震荡趋势，保持均衡网格");
        }
        _ => {}
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
    if market_analysis.price_change_5min.abs() > 0.03 { // 5分钟变化超过3%
        if market_analysis.price_change_5min > 0.0 {
            // 快速上涨，减少买单
            adjusted_fund_allocation.buy_order_funds *= 0.8;
            info!("🚀 快速上涨({:.2}%)，减少买单", market_analysis.price_change_5min * 100.0);
        } else {
            // 快速下跌，增加买单机会
            adjusted_fund_allocation.buy_order_funds *= 1.2;
            info!("💥 快速下跌({:.2}%)，增加买单机会", market_analysis.price_change_5min * 100.0);
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
    ).await?;
    
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
async fn cancel_order(
    exchange_client: &ExchangeClient,
    oid: u64,
) -> Result<(), GridStrategyError> {
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
            Err(GridStrategyError::OrderError(format!("取消订单失败: {:?}", e)))
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
            "资金使用率过高: {:.2}%", fund_usage_rate * 100.0
        )));
    }
    
    // 检查订单数量限制
    let total_orders = buy_orders.len() + sell_orders.len();
    if total_orders > grid_config.max_active_orders {
        return Err(GridStrategyError::FundAllocationError(format!(
            "活跃订单数量({})超过限制({})", total_orders, grid_config.max_active_orders
        )));
    }
    
    // 检查单个订单的资金分配是否合理
    for (oid, order_info) in buy_orders.iter() {
        if order_info.allocated_funds > grid_state.total_capital * 0.2 {
            warn!("⚠️ 订单{}分配资金过多: {:.2}", oid, order_info.allocated_funds);
        }
    }
    
    info!("📊 资金监控 - 使用率: {:.2}%, 活跃订单: {}, 总分配: {:.2}", 
        fund_usage_rate * 100.0, total_orders, total_allocated);
    
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
    let current_total_value = grid_state.available_funds + grid_state.position_quantity * current_price;
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
        ==============================",
                 format!("{:?}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()),
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
        grid_state.stop_loss_status
    )
}

pub async fn run_grid_strategy(app_config: crate::config::AppConfig) -> Result<(), GridStrategyError> {
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
    match exchange_client.update_leverage(grid_config.leverage, &grid_config.trading_asset, false, None).await {
        Ok(_) => info!("成功设置杠杆倍数为 {}x", grid_config.leverage),
        Err(e) => {
            error!("设置杠杆倍数失败: {:?}", e);
            return Err(GridStrategyError::OrderError(format!("设置杠杆倍数失败: {:?}", e)));
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
        stop_loss_status: "正常".to_string(),
        last_rebalance_time: SystemTime::now(),
        historical_volatility: 0.0,
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
        .subscribe(Subscription::UserEvents { user: user_address }, sender.clone())
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
                    let current_price: f64 = current_price.parse()
                        .map_err(|e| GridStrategyError::PriceParseError(format!("价格解析失败: {:?}", e)))?;
                    
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
                        info!("📈 价格变化: {:.4}% (从 {:.4} 到 {:.4})", 
                            price_change, last, current_price);
                    }
                    last_price = Some(current_price);

                    // 1. 止损检查
                    let stop_result = check_stop_loss(&mut grid_state, current_price, grid_config, &price_history);
                    
                    if stop_result.action != "正常" {
                        warn!("🚨 触发止损: {}, 原因: {}", stop_result.action, stop_result.reason);
                        
                        execute_stop_loss(
                            &exchange_client,
                            grid_config,
                            &mut grid_state,
                            &stop_result,
                            &mut active_orders,
                            &mut buy_orders,
                            &mut sell_orders,
                        ).await?;
                        
                        if stop_result.action == "已止损" {
                            error!("🛑 策略已全部止损，退出");
                            break;
                        }
                    }

                    // 2. 检查是否需要重平衡（每24小时）
                    let rebalance_interval = 24 * 60 * 60; // 24小时
                    if now.duration_since(grid_state.last_rebalance_time).unwrap().as_secs() >= rebalance_interval {
                        info!("🔄 开始定期重平衡...");
                        
                        rebalance_grid(
                            &exchange_client,
                            grid_config,
                            &mut grid_state,
                            current_price,
                            &price_history,
                            &mut active_orders,
                            &mut buy_orders,
                            &mut sell_orders,
                        ).await?;
                    }

                    // 3. 如果没有活跃订单，创建动态网格
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
                        ).await?;
                    }

                    // 4. 资金分配监控
                    if let Err(e) = monitor_fund_allocation(&grid_state, &buy_orders, &sell_orders, grid_config) {
                        warn!("⚠️ 资金分配监控警告: {:?}", e);
                    }

                    // 5. 定期状态报告（每小时）
                    if now.duration_since(last_status_report).unwrap().as_secs() >= 3600 {
                        let report = generate_status_report(&grid_state, current_price, &buy_orders, &sell_orders, grid_config);
                        info!("\n{}", report);
                        last_status_report = now;
                    }
                }
            }
            
                        Some(Message::User(user_event)) => {
                match user_event.data {
                    UserData::Fills(fills) => {
                        for fill in fills {
                            let fill_price: f64 = fill.px.parse()
                                .map_err(|e| GridStrategyError::PriceParseError(format!("成交价格解析失败: {:?}", e)))?;
                            let fill_size: f64 = fill.sz.parse()
                                .map_err(|e| GridStrategyError::QuantityParseError(format!("成交数量解析失败: {:?}", e)))?;

                            info!("📋 订单成交: ID={}, 方向={}, 价格={}, 数量={}", 
                                fill.oid, fill.side, fill_price, fill_size);

                            // 更新持仓信息
                            if fill.side == "B" {
                                // 买单成交，更新持仓
                                let buy_value = fill_price * fill_size;
                                let total_value = grid_state.position_avg_price * grid_state.position_quantity + buy_value;
                                grid_state.position_quantity += fill_size * (1.0 - grid_config.fee_rate);
                                
                                if grid_state.position_quantity > 0.0 {
                                    grid_state.position_avg_price = total_value / grid_state.position_quantity;
                                }

                                // 使用新的智能订单处理逻辑
                                if let Some(order_info) = buy_orders.remove(&fill.oid) {
                                    // 验证订单信息
                                    if (order_info.price - fill_price).abs() > fill_price * 0.001 {
                                        warn!("⚠️ 订单价格不匹配: 预期 {:.4}, 实际 {:.4}", order_info.price, fill_price);
                                    }
                                    
                                    // 使用潜在卖出价格进行利润预测
                                    if let Some(potential_price) = order_info.potential_sell_price {
                                        let expected_profit = (potential_price - fill_price) * fill_size * (1.0 - grid_config.fee_rate * 2.0);
                                        info!("💡 预期利润: {:.2} (潜在卖价: {:.4})", expected_profit, potential_price);
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
                                    ).await {
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
                                    let cost_price = order_info.cost_price.unwrap_or(grid_state.position_avg_price);
                                    let sell_revenue = fill_price * fill_size * (1.0 - grid_config.fee_rate);
                                    let buy_cost = cost_price * fill_size;
                                    let profit = sell_revenue - buy_cost;
                                    
                                    grid_state.realized_profit += profit;
                                    grid_state.available_funds += sell_revenue;
                                    
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
                                    ).await {
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