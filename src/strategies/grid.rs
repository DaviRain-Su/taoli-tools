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

pub async fn run_grid_strategy(app_config: crate::config::AppConfig) -> Result<(), GridStrategyError> {
    env_logger::init();
    let grid_config = &app_config.grid;
    
    // 从配置文件读取私钥
    let private_key = &app_config.account.private_key;
    
    // 初始化钱包
    let wallet: LocalWallet = private_key
        .parse()
        .map_err(|e| GridStrategyError::WalletError(format!("私钥解析失败: {:?}", e)))?;
    let user_address = wallet.address();
    info!("钱包地址: {:?}", user_address);

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
    let mut buy_entry_prices: HashMap<u64, f64> = HashMap::new();
    let mut sell_entry_prices: HashMap<u64, f64> = HashMap::new();
    let mut max_equity = 0.0;
    let mut daily_pnl = 0.0;
    let mut last_daily_reset = SystemTime::now();
    let mut position_start_time: Option<SystemTime> = None;

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
                    
                    // 更新价格历史
                    price_history.push(current_price);
                    if price_history.len() > grid_config.history_length {
                        price_history.remove(0);
                    }
                    
                    // 计算波动率并调整网格间距
                    let (avg_up, avg_down) = calculate_amplitude(&price_history);
                    let volatility = (avg_up + avg_down) / 2.0;
                    let grid_spacing = volatility.max(grid_config.min_grid_spacing).min(grid_config.max_grid_spacing);
                    
                    // 打印价格变化
                    if let Some(last) = last_price {
                        let price_change = ((current_price - last) / last) * 100.0;
                        info!("价格变化: {:.2}% (从 {:.2} 到 {:.2})", 
                            price_change, last, current_price);
                        info!("当前波动率: {:.4}%, 网格间距: {:.4}%", 
                            volatility * 100.0, grid_spacing * 100.0);
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
                    let buy_threshold = grid_spacing + grid_config.grid_price_offset;
                    let sell_threshold = grid_spacing - grid_config.grid_price_offset;

                    // === 分批分层投入：只挂最近N个买/卖单 ===
                    let max_active_orders = grid_config.max_active_orders as usize;
                    // 统计当前未成交买单和卖单数量
                    let mut active_buy_orders = 0;
                    let mut active_sell_orders = 0;
                    for &oid in &active_orders {
                        if buy_entry_prices.contains_key(&oid) {
                            active_buy_orders += 1;
                        } else if sell_entry_prices.contains_key(&oid) {
                            active_sell_orders += 1;
                        }
                    }

                    // 买单网格：只挂N个未成交买单
                    if long_position < grid_config.max_position {
                        let mut buy_count = 0;
                        for i in 0..grid_config.grid_count {
                            if active_buy_orders + buy_count >= max_active_orders {
                                break;
                            }
                            let price = current_price * (1.0 - buy_threshold - i as f64 * grid_spacing);
                            let formatted_price = format_price(price, grid_config.price_precision);
                            let quantity = format_price(grid_config.trade_amount / formatted_price, grid_config.quantity_precision);
                            let order_capital = quantity * formatted_price;
                            if order_capital > available_capital {
                                info!("剩余资金不足，停止买单挂单");
                                break;
                            }
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
                                                info!("买单已提交: ID={}, 价格={}, 数量={}", 
                                                    order.oid, formatted_price, quantity);
                                                active_orders.push(order.oid);
                                                buy_entry_prices.insert(order.oid, formatted_price);
                                                buy_count += 1;
                                            }
                                        }
                                    }
                                },
                                Ok(ExchangeResponseStatus::Err(e)) => warn!("买单失败: {:?}", e),
                                Err(e) => warn!("买单失败: {:?}", e),
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
                            let price = current_price * (1.0 + sell_threshold + i as f64 * grid_spacing);
                            let formatted_price = format_price(price, grid_config.price_precision);
                            let quantity = format_price(grid_config.trade_amount / formatted_price, grid_config.quantity_precision);
                            let order_capital = quantity * formatted_price;
                            if order_capital > available_capital {
                                info!("剩余资金不足，停止卖单挂单");
                                break;
                            }
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
                                                info!("卖单已提交: ID={}, 价格={}, 数量={}", 
                                                    order.oid, formatted_price, quantity);
                                                active_orders.push(order.oid);
                                                sell_entry_prices.insert(order.oid, formatted_price);
                                                sell_count += 1;
                                            }
                                        }
                                    }
                                },
                                Ok(ExchangeResponseStatus::Err(e)) => warn!("卖单失败: {:?}", e),
                                Err(e) => warn!("卖单失败: {:?}", e),
                            }
                            available_capital -= order_capital;
                        }
                    }

                    // 打印当前状态
                    info!("\n=== 当前状态 ===");
                    info!("多头持仓: {}", long_position);
                    info!("空头持仓: {}", short_position);
                    info!("最大权益: {}", max_equity);
                    info!("当前权益: {}", current_equity);
                    info!("每日盈亏: {}", daily_pnl);
                    info!("活跃订单数量: {}", active_orders.len());
                }
            },
            Some(Message::User(user_event)) => {
                // 处理用户事件
                match user_event.data {
                    UserData::Fills(fills) => {
                        for fill in fills {
                            info!("订单成交: ID={}, 价格={}, 数量={}, 方向={}", 
                                fill.oid, fill.px, fill.sz, if fill.side == "B" { "买入" } else { "卖出" });
                            
                            // 更新持仓
                            let fill_size: f64 = fill.sz.parse()
                                .map_err(|e| GridStrategyError::QuantityParseError(format!("数量解析失败: {:?}", e)))?;
                            let fill_price: f64 = fill.px.parse()
                                .map_err(|e| GridStrategyError::PriceParseError(format!("价格解析失败: {:?}", e)))?;
                            
                            // 计算盈亏
                            let pnl = if fill.side == "B" {
                                // 买入订单的盈亏
                                if let Some(entry_price) = sell_entry_prices.get(&fill.oid) {
                                    (entry_price - fill_price) * fill_size
                                } else {
                                    0.0
                                }
                            } else {
                                // 卖出订单的盈亏
                                if let Some(entry_price) = buy_entry_prices.get(&fill.oid) {
                                    (fill_price - entry_price) * fill_size
                                } else {
                                    0.0
                                }
                            };
                            
                            // 更新每日盈亏
                            daily_pnl += pnl;
                            
                            // 检查单笔亏损限制
                            if pnl < -grid_config.total_capital * grid_config.max_single_loss {
                                info!("触发单笔最大亏损限制，执行清仓");
                                close_all_positions(&exchange_client, grid_config, long_position, short_position, fill_price).await?;
                                return Err(GridStrategyError::RiskControlTriggered(format!(
                                    "触发单笔最大亏损限制: {:.2}", pnl
                                )));
                            }
                            
                            if fill.side == "B" {
                                long_position += fill_size;
                            } else {
                                short_position += fill_size;
                            }
                            
                            // 更新持仓开始时间
                            if position_start_time.is_none() && (long_position > 0.0 || short_position > 0.0) {
                                position_start_time = Some(SystemTime::now());
                            }
                            
                            // 从活跃订单中移除
                            if let Some(pos) = active_orders.iter().position(|&x| x == fill.oid) {
                                active_orders.remove(pos);
                            }
                            
                            // 从价格记录中移除
                            if fill.side == "B" {
                                buy_entry_prices.remove(&fill.oid);
                            } else {
                                sell_entry_prices.remove(&fill.oid);
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