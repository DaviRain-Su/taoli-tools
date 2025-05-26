use ethers::signers::{LocalWallet, Signer};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient, InfoClient,
    ClientCancelRequest, ExchangeDataStatus, ExchangeResponseStatus, Message, Subscription, UserData,
};
use log::{error, info, warn};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};
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
}

// 订单信息结构体
#[derive(Debug, Clone)]
struct OrderInfo {
    price: f64,
    quantity: f64,
    cost_price: Option<f64>, // 对于卖单，记录对应的买入成本价
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

    let mut active_orders: Vec<u64> = Vec::new();
    let mut last_price: Option<f64> = None;
    
    // 持仓管理
    let mut long_position = 0.0;
    let mut short_position = 0.0;
    let mut buy_orders: HashMap<u64, OrderInfo> = HashMap::new();
    let mut sell_orders: HashMap<u64, OrderInfo> = HashMap::new();
    let mut max_equity = 0.0;
    let mut daily_pnl = 0.0;
    let mut last_daily_reset = SystemTime::now();
    let mut position_start_time: Option<SystemTime> = None;
    let mut long_avg_price = 0.0;
    let mut short_avg_price = 0.0;
    let mut current_grid_spacing = grid_config.min_grid_spacing; // 当前网格间距

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

    loop {
        info!("=== 开始新一轮检查 ===");

        // 检查是否需要重置每日统计
        let now = SystemTime::now();
        if now.duration_since(last_daily_reset).unwrap().as_secs() >= 24 * 60 * 60 {
            daily_pnl = 0.0;
            last_daily_reset = now;
            info!("重置每日统计");
        }

        // 获取当前价格
        match receiver.recv().await {
            Some(Message::AllMids(all_mids)) => {
                let all_mids = all_mids.data.mids;
                if let Some(current_price) = all_mids.get(&grid_config.trading_asset) {
                    let current_price: f64 = current_price.parse()
                        .map_err(|e| GridStrategyError::PriceParseError(format!("价格解析失败: {:?}", e)))?;
                    
                    // 获取实际账户信息
                    let account_info = get_account_info(&info_client, user_address).await?;
                    //info!("完整账户信息: {:?}", account_info);

                    // 用 withdrawable 字段作为 USDC 可用余额
                    let usdc_balance = account_info.withdrawable.parse().unwrap_or(0.0);
                    info!("USDC 余额: {}", usdc_balance);

                    // 更新价格历史
                    price_history.push(current_price);
                    if price_history.len() > grid_config.history_length {
                        price_history.remove(0);
                    }
                    
                    // 计算波动率并调整网格间距
                    let (avg_up, avg_down) = calculate_amplitude(&price_history);
                    let volatility = (avg_up + avg_down) / 2.0;
                    current_grid_spacing = volatility.max(grid_config.min_grid_spacing).min(grid_config.max_grid_spacing);
                    
                    // 打印价格变化
                    if let Some(last) = last_price {
                        let price_change = ((current_price - last) / last) * 100.0;
                        info!("价格变化: {:.4}% (从 {:.4} 到 {:.4})", 
                            price_change, last, current_price);
                        info!("当前波动率: {:.8}%, 网格间距: {:.8}%", 
                            volatility * 100.0, current_grid_spacing * 100.0);
                    }
                    last_price = Some(current_price);

                    // 资金分配：每次循环时计算可用资金
                    let used_capital = (long_position + short_position) * current_price;
                    let mut available_capital = grid_config.total_capital - used_capital;

                    // 计算当前总权益
                    let current_equity = long_position * current_price + short_position * current_price + available_capital;

                    // 动态更新历史最大权益
                    if current_equity > max_equity {
                        max_equity = current_equity;
                    }

                    // 检查持仓时间
                    if let Some(start_time) = position_start_time {
                        if now.duration_since(start_time).unwrap().as_secs() >= grid_config.max_holding_time {
                            info!("触发最大持仓时间限制，执行清仓");
                            close_all_positions(&exchange_client, grid_config, long_position, short_position, current_price).await?;
                            position_start_time = None;
                        }
                    }

                    // 最大回撤风控
                    let drawdown = (max_equity - current_equity) / max_equity;
                    if drawdown > grid_config.max_drawdown {
                        info!("触发最大回撤保护，执行清仓");
                        close_all_positions(&exchange_client, grid_config, long_position, short_position, current_price).await?;
                        return Err(GridStrategyError::RiskControlTriggered(format!(
                            "触发最大回撤保护: {:.2}%", drawdown * 100.0
                        )));
                    }

                    // 检查每日亏损限制
                    if daily_pnl < -grid_config.total_capital * grid_config.max_daily_loss {
                        info!("触发每日最大亏损限制，执行清仓");
                        close_all_positions(&exchange_client, grid_config, long_position, short_position, current_price).await?;
                        return Err(GridStrategyError::RiskControlTriggered(format!(
                            "触发每日最大亏损限制: {:.2}", daily_pnl
                        )));
                    }

                    // 取消所有现有订单
                    for order_id in &active_orders {
                        info!("取消订单: {}", order_id);
                        match exchange_client.cancel(ClientCancelRequest { 
                            asset: grid_config.trading_asset.clone(), 
                            oid: *order_id 
                        }, None).await {
                            Ok(_) => info!("订单取消成功: {}", order_id),
                            Err(e) => warn!("取消订单失败: {:?}", e),
                        }
                    }
                    active_orders.clear();

                    // 计算网格价格
                    let buy_threshold = current_grid_spacing / 2.0;
                    let sell_threshold = current_grid_spacing / 2.0;

                    // === 分批分层投入：只挂最近N个买/卖单 ===
                    let max_active_orders = grid_config.max_active_orders as usize;
                    // 统计当前未成交买单和卖单数量，并计算所有挂单的保证金需求
                    let mut active_buy_orders = 0;
                    let mut active_sell_orders = 0;
                    let mut pending_buy_margin: f64 = 0.0;
                    let mut pending_sell_margin: f64 = 0.0;
                    for &oid in &active_orders {
                        if let Some(order_info) = buy_orders.get(&oid) {
                            active_buy_orders += 1;
                            pending_buy_margin += (order_info.quantity * order_info.price) / grid_config.leverage as f64;
                        } else if let Some(order_info) = sell_orders.get(&oid) {
                            active_sell_orders += 1;
                            pending_sell_margin += (order_info.quantity * order_info.price) / grid_config.leverage as f64;
                        }
                    }

                    // 买单网格：只挂N个未成交买单
                    if long_position < grid_config.max_position {
                   
                        let mut buy_count = 0;
                        for i in 0..grid_config.grid_count {
                            if active_buy_orders + buy_count >= max_active_orders {
                                break;
                            }
                            let price = current_price * (1.0 - buy_threshold - i as f64 * current_grid_spacing);
                            let formatted_price = format_price(price, grid_config.price_precision);
                            let quantity = format_price(grid_config.trade_amount / formatted_price, grid_config.quantity_precision);
                            let order_capital = quantity * formatted_price;
                            let order_margin = order_capital / grid_config.leverage as f64;
                            
                            // 使用实际账户数据检查保证金
                            let actual_margin_used = account_info.margin_summary.total_margin_used.parse().unwrap_or(0.0);
                            let margin_base = usdc_balance + actual_margin_used;
                            let margin_limit = margin_base * app_config.grid.margin_usage_threshold;
                            let total_margin = actual_margin_used + pending_buy_margin + pending_sell_margin + order_margin;

                            info!(
                                "\n🛡️ [风控检查] 保证金明细：\
                                \n   💰 已用保证金      : {:>12.4} USDC\
                                \n   🟢 待用买单保证金  : {:>12.4} USDC\
                                \n   🔴 待用卖单保证金  : {:>12.4} USDC\
                                \n   📝 新订单保证金    : {:>12.4} USDC\
                                \n   🧮 总计保证金需求  : {:>12.4} USDC\
                                \n   💵 可动用资金      : {:>12.4} USDC\
                                \n   💵 最大可用保证金  : {:>12.4} USDC\
                                \n   📊 资金使用率      : {:>12.2}%",
                                actual_margin_used, pending_buy_margin, pending_sell_margin, order_margin, 
                                total_margin, margin_base, margin_limit, total_margin / margin_limit * 100.0
                            );

                            
                            let future_position = long_position + quantity;
                            if future_position > grid_config.max_position {
                                info!("下单后多头持仓将超限，停止买单挂单");
                                break;
                            }

                            let fee_rate = grid_config.fee_rate;
                            let min_grid_spacing = 2.0 * fee_rate;
                            if current_grid_spacing < min_grid_spacing {
                                info!("❌ 网格间距({:.4}%)过小，无法覆盖手续费({:.4}%)，本次不挂单", current_grid_spacing * 100.0, min_grid_spacing * 100.0);
                                break;
                            }
                            // === 最小盈利阈值风控 ===
                            let order = ClientOrderRequest {
                                asset: grid_config.trading_asset.clone(),
                                is_buy: true,
                                reduce_only: false,
                                limit_px: formatted_price,
                                sz: quantity,
                                cloid: None,
                                order_type: ClientOrder::Limit(ClientLimit {
                                    tif: "Gtc".to_string(),
                                }),
                            };

                            match exchange_client.order(order, None).await {
                                Ok(ExchangeResponseStatus::Ok(response)) => {
                                    if let Some(data) = response.data {
                                        if !data.statuses.is_empty() {
                                            if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                                                info!("🟢【买单】✅ 买单已提交: ID={}, 价格={}, 数量={}", 
                                                    order.oid, formatted_price, quantity);
                                                active_orders.push(order.oid);
                                                buy_orders.insert(order.oid, OrderInfo {
                                                    price: formatted_price,
                                                    quantity,
                                                    cost_price: None,
                                                });
                                                buy_count += 1;
                                                pending_buy_margin += order_margin;
                                            }
                                        }
                                    }
                                },
                                Ok(ExchangeResponseStatus::Err(e)) => warn!("❌ 买单失败: {:?}", e),
                                Err(e) => warn!("❌ 买单失败: {:?}", e),
                            }
                            available_capital -= order_capital;
                        }
                    }

                    // 卖单网格：只挂N个未成交卖单
                    if short_position < grid_config.max_position {
                        let mut sell_count = 0;
                        for i in 0..grid_config.grid_count {
                            if active_sell_orders + sell_count >= max_active_orders {
                                break;
                            }
                            let price = current_price * (1.0 + sell_threshold + i as f64 * current_grid_spacing);
                            let formatted_price = format_price(price, grid_config.price_precision);
                            let quantity = format_price(grid_config.trade_amount / formatted_price, grid_config.quantity_precision);
                            let order_capital = quantity * formatted_price;
                            let order_margin = order_capital / grid_config.leverage as f64;
                            
                            // 使用实际账户数据检查保证金
                            let actual_margin_used = account_info.margin_summary.total_margin_used.parse().unwrap_or(0.0);
                            let margin_base = usdc_balance + actual_margin_used;
                            let margin_limit = margin_base * app_config.grid.margin_usage_threshold;
                            let total_margin = actual_margin_used + pending_buy_margin + pending_sell_margin + order_margin;

                            info!(
                                "\n🛡️ [风控检查] 保证金明细：\
                                \n   💰 已用保证金      : {:>12.4} USDC\
                                \n   🟢 待用买单保证金  : {:>12.4} USDC\
                                \n   🔴 待用卖单保证金  : {:>12.4} USDC\
                                \n   📝 新订单保证金    : {:>12.4} USDC\
                                \n   🧮 总计保证金需求  : {:>12.4} USDC\
                                \n   💵 可动用资金      : {:>12.4} USDC\
                                \n   💵 最大可用保证金  : {:>12.4} USDC\
                                \n   📊 资金使用率      : {:>12.2}%",
                                actual_margin_used, pending_buy_margin, pending_sell_margin, order_margin, 
                                total_margin, margin_base, margin_limit, total_margin / margin_limit * 100.0
                            );
                            
                            let future_position = short_position + quantity;
                            if future_position > grid_config.max_position {
                                info!("下单后空头持仓将超限，停止卖单挂单");
                                break;
                            }

                            let fee_rate = grid_config.fee_rate;
                            let min_grid_spacing = 2.0 * fee_rate;
                            if current_grid_spacing < min_grid_spacing {
                                info!("❌ 网格间距({:.4}%)过小，无法覆盖手续费({:.4}%)，本次不挂单", current_grid_spacing * 100.0, min_grid_spacing * 100.0);
                                break;
                            }
                            // === 最小盈利阈值风控 ===
                            let order = ClientOrderRequest {
                                asset: grid_config.trading_asset.clone(),
                                is_buy: false,
                                reduce_only: false,
                                limit_px: formatted_price,
                                sz: quantity,
                                cloid: None,
                                order_type: ClientOrder::Limit(ClientLimit {
                                    tif: "Gtc".to_string(),
                                }),
                            };

                            match exchange_client.order(order, None).await {
                                Ok(ExchangeResponseStatus::Ok(response)) => {
                                    if let Some(data) = response.data {
                                        if !data.statuses.is_empty() {
                                            if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                                                info!("🔴【卖单】✅ 卖单已提交: ID={}, 价格={}, 数量={}", 
                                                    order.oid, formatted_price, quantity);
                                                active_orders.push(order.oid);
                                                sell_orders.insert(order.oid, OrderInfo {
                                                    price: formatted_price,
                                                    quantity,
                                                    cost_price: None,
                                                });
                                                sell_count += 1;
                                                pending_sell_margin += order_margin;
                                            }
                                        }
                                    }
                                },
                                Ok(ExchangeResponseStatus::Err(e)) => warn!("❌ 卖单失败: {:?}", e),
                                Err(e) => warn!("❌ 卖单失败: {:?}", e),
                            }
                            available_capital -= order_capital;
                        }
                    }

                    // 打印当前状态
                    info!(
                        "\n📊 ====== 当前账户状态 ======\
                        \n  🟩 多头持仓      : {:>10.4}\
                        \n  🟥 空头持仓      : {:>10.4}\
                        \n  🏆 最大权益      : {:>10.2} USDC\
                        \n  💎 当前权益      : {:>10.2} USDC\
                        \n  📈 每日盈亏      : {:>10.2} USDC\
                        \n  📝 活跃订单数量  : {:>10}\
                        \n  💵 账户可用余额  : {:>10.2} USDC\
                        \n==============================",
                        long_position, short_position, max_equity, current_equity, daily_pnl, active_orders.len(), usdc_balance
                    );
                }
            },
            Some(Message::User(user_event)) => {
                // 处理用户事件
                match user_event.data {
                    UserData::Fills(fills) => {
                        for fill in fills {
                            info!(
                                "🎯 订单成交: ID={}, 价格={}, 数量={}, 方向={}",
                                fill.oid, fill.px, fill.sz, if fill.side == "B" { "🟩 买入" } else { "🟥 卖出" }
                            );
                            
                            // 更新持仓
                            let fill_size: f64 = fill.sz.parse()
                                .map_err(|e| GridStrategyError::QuantityParseError(format!("数量解析失败: {:?}", e)))?;
                            let fill_price: f64 = fill.px.parse()
                                .map_err(|e| GridStrategyError::PriceParseError(format!("价格解析失败: {:?}", e)))?;
                            

                            if fill.side == "B" {
                                long_avg_price = (long_avg_price * long_position + fill_price * fill_size) / (long_position + fill_size);
                                long_position += fill_size;
                            } else {
                                short_avg_price = (short_avg_price * short_position + fill_price * fill_size) / (short_position + fill_size);
                                short_position += fill_size;
                            }
                            
                            // 更新每日盈亏
                            daily_pnl += if fill.side == "B" {
                                (fill_price - long_avg_price) * fill_size
                            } else {
                                (short_avg_price - fill_price) * fill_size
                            };
                            
                            // 检查单笔亏损限制
                            if daily_pnl < -grid_config.total_capital * grid_config.max_daily_loss {
                                info!("触发每日最大亏损限制，执行清仓");
                                close_all_positions(&exchange_client, grid_config, long_position, short_position, fill_price).await?;
                                return Err(GridStrategyError::RiskControlTriggered(format!(
                                    "触发每日最大亏损限制: {:.2}", daily_pnl
                                )));
                            }
                            
                            // 检查单笔亏损限制
                            if if fill.side == "B" {
                                (fill_price - long_avg_price) * fill_size
                            } else {
                                (short_avg_price - fill_price) * fill_size
                            } < -grid_config.total_capital * grid_config.max_single_loss {
                                info!("触发单笔最大亏损限制，执行清仓");
                                close_all_positions(&exchange_client, grid_config, long_position, short_position, fill_price).await?;
                                return Err(GridStrategyError::RiskControlTriggered(format!(
                                    "触发单笔最大亏损限制: {:.2}", if fill.side == "B" {
                                        (fill_price - long_avg_price) * fill_size
                                    } else {
                                        (short_avg_price - fill_price) * fill_size
                                    }
                                )));
                            }
                            
                            // 更新持仓开始时间
                            if position_start_time.is_none() && (long_position > 0.0 || short_position > 0.0) {
                                position_start_time = Some(SystemTime::now());
                            }
                            
                            // 从活跃订单中移除
                            if let Some(pos) = active_orders.iter().position(|&x| x == fill.oid) {
                                active_orders.remove(pos);
                            }
                            
                            // 使用新的智能订单处理逻辑
                            if fill.side == "B" {
                                // 买单成交，使用新的处理逻辑
                                if let Some(order_info) = buy_orders.remove(&fill.oid) {
                                    info!("📋 原始买单信息: 价格={}, 数量={}", order_info.price, order_info.quantity);
                                    
                                    // 验证成交信息与原始订单是否匹配
                                    if (fill_price - order_info.price).abs() > 0.0001 {
                                        warn!("⚠️ 成交价格({})与订单价格({})不匹配", fill_price, order_info.price);
                                    }
                                    
                                    if let Err(e) = handle_buy_fill(
                                        &exchange_client,
                                        grid_config,
                                        fill_price,
                                        fill_size,
                                        current_grid_spacing,
                                        &mut active_orders,
                                        &mut buy_orders,
                                        &mut sell_orders,
                                    ).await {
                                        warn!("处理买单成交失败: {:?}", e);
                                    }
                                } else {
                                    warn!("⚠️ 未找到买单订单信息: ID={}", fill.oid);
                                }
                            } else {
                                // 卖单成交，使用新的处理逻辑
                                if let Some(order_info) = sell_orders.remove(&fill.oid) {
                                    info!("📋 原始卖单信息: 价格={}, 数量={}, 成本价={:?}", 
                                        order_info.price, order_info.quantity, order_info.cost_price);
                                    
                                    // 验证成交信息与原始订单是否匹配
                                    if (fill_price - order_info.price).abs() > 0.0001 {
                                        warn!("⚠️ 成交价格({})与订单价格({})不匹配", fill_price, order_info.price);
                                    }
                                    
                                    if let Err(e) = handle_sell_fill(
                                        &exchange_client,
                                        grid_config,
                                        fill_price,
                                        fill_size,
                                        order_info.cost_price,
                                        current_grid_spacing,
                                        &mut active_orders,
                                        &mut buy_orders,
                                        &mut sell_orders,
                                    ).await {
                                        warn!("处理卖单成交失败: {:?}", e);
                                    }
                                } else {
                                    warn!("⚠️ 未找到卖单订单信息: ID={}", fill.oid);
                                }
                            }

                            if fill.side == "S" && long_position > 0.0 {
                                // 卖出成交，且有多头持仓，视为平多
                                let pnl = (fill_price - long_avg_price) * fill_size;
                                let fee_rate = grid_config.fee_rate;
                                let fee = pnl * fee_rate * 2.0;
                                let max_acceptable_loss = fee;

                                if pnl < max_acceptable_loss {
                                    info!("⚠️ 平多操作将导致亏损({:.4} USDC)，已阻止本次平仓", pnl);
                                    continue;
                                }
                                long_position -= fill_size;
                                if long_position <= 0.0 {
                                    long_avg_price = 0.0;
                                }
                            }
                        }
                    },
                    _ => continue,
                }
            },
            Some(_) => continue,
            None => {
                error!("接收消息通道关闭");
                return Err(GridStrategyError::SubscriptionError("消息通道关闭".to_string()));
            }
        }

        // 等待一段时间再进行下一次检查
        info!("\n等待{}秒后进行下一次检查...", grid_config.check_interval);
        sleep(Duration::from_secs(grid_config.check_interval)).await;
    }
} 