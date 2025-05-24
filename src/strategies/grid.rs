use ethers::signers::{LocalWallet, Signer};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient, InfoClient,
    ClientCancelRequest, ExchangeDataStatus, ExchangeResponseStatus, Message, Subscription, UserData,
};
use log::{error, info};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::sleep;

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

pub async fn run_grid_strategy() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    
    // 加载配置文件
    let app_config = crate::config::load_config(std::path::Path::new("configs/default.toml"))?;
    let grid_config = &app_config.grid;
    
    // 从配置文件读取私钥
    let private_key = &app_config.account.private_key;
    
    // 初始化钱包
    let wallet: LocalWallet = private_key
        .parse()
        .unwrap();
    let user_address = wallet.address();
    info!("钱包地址: {:?}", user_address);

    // 初始化客户端
    let mut info_client = InfoClient::new(None, Some(BaseUrl::Mainnet)).await.unwrap();
    let exchange_client = ExchangeClient::new(None, wallet, Some(BaseUrl::Mainnet), None, None)
        .await
        .unwrap();

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
            return Ok(());
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
    let mut initial_equity = None;
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
        .unwrap();
    
    info_client
        .subscribe(Subscription::UserEvents { user: user_address }, sender.clone())
        .await
        .unwrap();

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
                    let current_price: f64 = current_price.parse().unwrap();
                    
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

                    // 更新最大权益和当前权益
                    let current_equity = long_position - short_position;
                    if current_equity > max_equity {
                        max_equity = current_equity;
                    }

                    // 检查持仓时间
                    if let Some(start_time) = position_start_time {
                        if now.duration_since(start_time).unwrap().as_secs() >= grid_config.max_holding_time {
                            info!("触发最大持仓时间限制，执行清仓");
                            // 清仓逻辑
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
                                    error!("清仓失败: {:?}", e);
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
                                    error!("清仓失败: {:?}", e);
                                }
                            }
                            position_start_time = None;
                        }
                    }

                    // 检查最大回撤
                    if let Some(init_equity) = initial_equity {
                        let drawdown = (init_equity - current_equity) / init_equity;
                        if drawdown > grid_config.max_drawdown {
                            info!("触发最大回撤保护，执行清仓");
                            // 清仓逻辑
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
                                    error!("清仓失败: {:?}", e);
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
                                    error!("清仓失败: {:?}", e);
                                }
                            }
                            return Ok(());
                        }
                    } else {
                        initial_equity = Some(current_equity);
                    }

                    // 检查每日亏损限制
                    if daily_pnl < -grid_config.total_capital * grid_config.max_daily_loss {
                        info!("触发每日最大亏损限制，执行清仓");
                        // 清仓逻辑
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
                                error!("清仓失败: {:?}", e);
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
                                error!("清仓失败: {:?}", e);
                            }
                        }
                        return Ok(());
                    }

                    // 取消所有现有订单
                    for order_id in &active_orders {
                        info!("取消订单: {}", order_id);
                        match exchange_client.cancel(ClientCancelRequest { 
                            asset: grid_config.trading_asset.clone(), 
                            oid: *order_id 
                        }, None).await {
                            Ok(_) => info!("订单取消成功: {}", order_id),
                            Err(e) => error!("取消订单失败: {:?}", e),
                        }
                    }
                    active_orders.clear();

                    // 计算网格价格
                    let buy_threshold = grid_spacing + grid_config.grid_price_offset;
                    let sell_threshold = grid_spacing - grid_config.grid_price_offset;

                    // 买单网格
                    if long_position < grid_config.max_position {
                        for i in 0..grid_config.grid_count {
                            let price = current_price * (1.0 - buy_threshold - i as f64 * grid_spacing);
                            let formatted_price = format_price(price, grid_config.price_precision);
                            let quantity = format_price(grid_config.trade_amount / formatted_price, grid_config.quantity_precision);
                            
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
                                            }
                                        }
                                    }
                                },
                                Ok(ExchangeResponseStatus::Err(e)) => error!("买单失败: {:?}", e),
                                Err(e) => error!("买单失败: {:?}", e),
                            }
                        }
                    }

                    // 卖单网格
                    if short_position < grid_config.max_position {
                        for i in 0..grid_config.grid_count {
                            let price = current_price * (1.0 + sell_threshold + i as f64 * grid_spacing);
                            let formatted_price = format_price(price, grid_config.price_precision);
                            let quantity = format_price(grid_config.trade_amount / formatted_price, grid_config.quantity_precision);
                            
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
                                            }
                                        }
                                    }
                                },
                                Ok(ExchangeResponseStatus::Err(e)) => error!("卖单失败: {:?}", e),
                                Err(e) => error!("卖单失败: {:?}", e),
                            }
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
                            let fill_size: f64 = fill.sz.parse().unwrap();
                            let fill_price: f64 = fill.px.parse().unwrap();
                            
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
                                // 清仓逻辑
                                if long_position > 0.0 {
                                    let order = ClientOrderRequest {
                                        asset: grid_config.trading_asset.clone(),
                                        is_buy: false,
                                        reduce_only: true,
                                        limit_px: fill_price,
                                        sz: long_position,
                                        cloid: None,
                                        order_type: ClientOrder::Limit(ClientLimit {
                                            tif: "Gtc".to_string(),
                                        }),
                                    };
                                    if let Err(e) = exchange_client.order(order, None).await {
                                        error!("清仓失败: {:?}", e);
                                    }
                                }
                                if short_position > 0.0 {
                                    let order = ClientOrderRequest {
                                        asset: grid_config.trading_asset.clone(),
                                        is_buy: true,
                                        reduce_only: true,
                                        limit_px: fill_price,
                                        sz: short_position,
                                        cloid: None,
                                        order_type: ClientOrder::Limit(ClientLimit {
                                            tif: "Gtc".to_string(),
                                        }),
                                    };
                                    if let Err(e) = exchange_client.order(order, None).await {
                                        error!("清仓失败: {:?}", e);
                                    }
                                }
                                return Ok(());
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
                break;
            }
        }

        // 等待一段时间再进行下一次检查
        info!("\n等待{}秒后进行下一次检查...", grid_config.check_interval);
        sleep(Duration::from_secs(grid_config.check_interval)).await;
    }

    Ok(())
} 