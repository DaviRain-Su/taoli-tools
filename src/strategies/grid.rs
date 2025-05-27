#![allow(dead_code)]

use ethers::signers::{LocalWallet, Signer};
use hyperliquid_rust_sdk::{
    BaseUrl, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient, Message, Subscription, UserData,
};
use log::{debug, error, info, warn};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PerformanceRecord {
    #[serde(with = "system_time_serde")]
    timestamp: SystemTime,
    price: f64,
    action: String,
    profit: f64,
    total_capital: f64,
}

// SystemTime 序列化辅助模块
mod system_time_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let duration = time.duration_since(UNIX_EPOCH).unwrap();
        duration.as_secs().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(UNIX_EPOCH + std::time::Duration::from_secs(secs))
    }
}

/// 安全的时间差计算，处理时间倒退的情况
fn safe_duration_since(now: SystemTime, earlier: SystemTime) -> Duration {
    match now.duration_since(earlier) {
        Ok(duration) => duration,
        Err(e) => {
            // 时间倒退了，记录警告并返回一个合理的默认值
            warn!("⚠️ 检测到系统时间倒退: {:?}", e);
            // 返回一个较大的值，确保时间检查会触发
            Duration::from_secs(3600) // 1小时，确保定期检查会执行
        }
    }
}

/// 安全的Unix时间戳获取
fn safe_unix_timestamp() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => {
            warn!("⚠️ 系统时间早于Unix纪元，使用备用时间戳");
            // 使用一个合理的默认时间戳（2024年1月1日）
            1704067200 // 2024-01-01 00:00:00 UTC
        }
    }
}

/// 安全的时间间隔检查
fn should_execute_periodic_task(
    last_execution: SystemTime,
    interval_seconds: u64,
    task_name: &str,
) -> bool {
    let now = SystemTime::now();
    let duration = safe_duration_since(now, last_execution);
    let should_execute = duration.as_secs() >= interval_seconds;
    
    if should_execute {
        debug!("⏰ 执行定期任务: {} (间隔: {}秒)", task_name, duration.as_secs());
    }
    
    should_execute
}

// 批处理任务优化器
#[derive(Debug, Clone)]
struct BatchTaskOptimizer {
    last_execution_times: VecDeque<Duration>,
    optimal_batch_size: usize,
    adjustment_factor: f64,
    min_batch_size: usize,
    max_batch_size: usize,
    target_execution_time: Duration,
    performance_window_size: usize,
    consecutive_adjustments: u32,
    last_adjustment_time: Instant,
    adjustment_cooldown: Duration,
    performance_trend: f64, // 正值表示性能改善，负值表示性能下降
}

impl BatchTaskOptimizer {
    /// 创建新的批处理优化器
    fn new(initial_batch_size: usize, target_execution_time: Duration) -> Self {
        Self {
            last_execution_times: VecDeque::new(),
            optimal_batch_size: initial_batch_size,
            adjustment_factor: 0.1, // 10%的调整幅度
            min_batch_size: 1,
            max_batch_size: 200,
            target_execution_time,
            performance_window_size: 10,
            consecutive_adjustments: 0,
            last_adjustment_time: Instant::now(),
            adjustment_cooldown: Duration::from_secs(30), // 30秒调整冷却时间
            performance_trend: 0.0,
        }
    }

    /// 基于历史执行时间自动调整最优批次大小
    fn optimize_batch_size(&mut self, task_count: usize) -> usize {
        // 如果任务数量小于最小批次大小，直接返回任务数量
        if task_count <= self.min_batch_size {
            return task_count;
        }

        // 检查是否在调整冷却期内
        if self.last_adjustment_time.elapsed() < self.adjustment_cooldown {
            return self.optimal_batch_size.min(task_count);
        }

        // 如果没有足够的历史数据，使用当前最优批次大小
        if self.last_execution_times.len() < 3 {
            return self.optimal_batch_size.min(task_count);
        }

        // 计算平均执行时间和性能趋势
        let avg_execution_time = self.calculate_average_execution_time();
        let performance_variance = self.calculate_performance_variance();

        // 更新性能趋势
        self.update_performance_trend(avg_execution_time);

        // 决定是否需要调整批次大小
        let should_adjust = self.should_adjust_batch_size(avg_execution_time, performance_variance);

        if should_adjust {
            let new_batch_size = self.calculate_new_batch_size(avg_execution_time, task_count);

            if new_batch_size != self.optimal_batch_size {
                info!(
                    "📊 批处理优化器调整: {} -> {} (平均执行时间: {:.2}秒, 目标: {:.2}秒)",
                    self.optimal_batch_size,
                    new_batch_size,
                    avg_execution_time.as_secs_f64(),
                    self.target_execution_time.as_secs_f64()
                );

                self.optimal_batch_size = new_batch_size;
                self.last_adjustment_time = Instant::now();
                self.consecutive_adjustments += 1;

                // 如果连续调整次数过多，增加调整冷却时间
                if self.consecutive_adjustments > 5 {
                    self.adjustment_cooldown = Duration::from_secs(60);
                    info!("⚠️ 连续调整次数过多，增加冷却时间到60秒");
                }
            }
        } else {
            // 重置连续调整计数
            if self.consecutive_adjustments > 0 {
                self.consecutive_adjustments = 0;
                self.adjustment_cooldown = Duration::from_secs(30); // 重置冷却时间
            }
        }

        self.optimal_batch_size.min(task_count)
    }

    /// 记录执行时间，用于未来优化
    fn record_execution_time(&mut self, duration: Duration) {
        self.last_execution_times.push_back(duration);

        // 保持窗口大小
        if self.last_execution_times.len() > self.performance_window_size {
            self.last_execution_times.pop_front();
        }

        // 记录性能统计
        if self.last_execution_times.len() >= 3 {
            let avg_time = self.calculate_average_execution_time();
            let variance = self.calculate_performance_variance();

            // 每10次记录输出一次性能统计
            if self.last_execution_times.len() % 10 == 0 {
                info!(
                    "📈 批处理性能统计: 平均时间={:.2}秒, 方差={:.4}, 当前批次大小={}, 趋势={}",
                    avg_time.as_secs_f64(),
                    variance,
                    self.optimal_batch_size,
                    if self.performance_trend > 0.0 {
                        "改善"
                    } else if self.performance_trend < 0.0 {
                        "下降"
                    } else {
                        "稳定"
                    }
                );
            }
        }
    }

    /// 计算平均执行时间
    fn calculate_average_execution_time(&self) -> Duration {
        if self.last_execution_times.is_empty() {
            return self.target_execution_time;
        }

        let total_duration: Duration = self.last_execution_times.iter().sum();
        total_duration / self.last_execution_times.len() as u32
    }

    /// 计算性能方差
    fn calculate_performance_variance(&self) -> f64 {
        if self.last_execution_times.len() < 2 {
            return 0.0;
        }

        let avg = self.calculate_average_execution_time().as_secs_f64();
        let variance = self
            .last_execution_times
            .iter()
            .map(|d| {
                let diff = d.as_secs_f64() - avg;
                diff * diff
            })
            .sum::<f64>()
            / self.last_execution_times.len() as f64;

        variance.sqrt()
    }

    /// 更新性能趋势
    fn update_performance_trend(&mut self, _current_avg: Duration) {
        if self.last_execution_times.len() < 5 {
            return;
        }

        // 计算最近一半和前一半的平均时间
        let mid_point = self.last_execution_times.len() / 2;
        let recent_times: Vec<Duration> = self
            .last_execution_times
            .iter()
            .skip(mid_point)
            .cloned()
            .collect();
        let earlier_times: Vec<Duration> = self
            .last_execution_times
            .iter()
            .take(mid_point)
            .cloned()
            .collect();

        let recent_avg = recent_times.iter().sum::<Duration>() / recent_times.len() as u32;
        let earlier_avg = earlier_times.iter().sum::<Duration>() / earlier_times.len() as u32;

        // 计算趋势（负值表示性能改善，正值表示性能下降）
        self.performance_trend =
            (recent_avg.as_secs_f64() - earlier_avg.as_secs_f64()) / earlier_avg.as_secs_f64();
    }

    /// 判断是否应该调整批次大小
    fn should_adjust_batch_size(&self, avg_execution_time: Duration, variance: f64) -> bool {
        let target_time = self.target_execution_time.as_secs_f64();
        let current_time = avg_execution_time.as_secs_f64();

        // 如果执行时间偏离目标时间超过20%，或者方差过大，则需要调整
        let time_deviation = (current_time - target_time).abs() / target_time;
        let high_variance = variance > target_time * 0.3; // 方差超过目标时间的30%

        time_deviation > 0.2 || high_variance
    }

    /// 计算新的批次大小
    fn calculate_new_batch_size(&self, avg_execution_time: Duration, task_count: usize) -> usize {
        let target_time = self.target_execution_time.as_secs_f64();
        let current_time = avg_execution_time.as_secs_f64();

        let mut new_size = self.optimal_batch_size;

        if current_time > target_time * 1.2 {
            // 执行时间过长，减少批次大小
            let reduction_factor = 1.0 - self.adjustment_factor;
            new_size = ((self.optimal_batch_size as f64) * reduction_factor) as usize;

            // 如果性能趋势持续下降，加大调整幅度
            if self.performance_trend > 0.1 {
                new_size = ((new_size as f64) * 0.9) as usize;
            }
        } else if current_time < target_time * 0.8 {
            // 执行时间过短，可以增加批次大小
            let increase_factor = 1.0 + self.adjustment_factor;
            new_size = ((self.optimal_batch_size as f64) * increase_factor) as usize;

            // 如果性能趋势持续改善，可以更积极地增加批次大小
            if self.performance_trend < -0.1 {
                new_size = ((new_size as f64) * 1.1) as usize;
            }
        }

        // 应用边界限制
        new_size = new_size
            .max(self.min_batch_size)
            .min(self.max_batch_size)
            .min(task_count);

        // 避免过于频繁的小幅调整
        let change_ratio = (new_size as f64 - self.optimal_batch_size as f64).abs()
            / self.optimal_batch_size as f64;
        if change_ratio < 0.05 {
            // 变化小于5%，不进行调整
            return self.optimal_batch_size;
        }

        new_size
    }

    /// 获取当前性能报告
    fn get_performance_report(&self) -> String {
        if self.last_execution_times.is_empty() {
            return "批处理优化器: 暂无性能数据".to_string();
        }

        let avg_time = self.calculate_average_execution_time();
        let variance = self.calculate_performance_variance();
        let efficiency = if avg_time.as_secs_f64() > 0.0 {
            self.target_execution_time.as_secs_f64() / avg_time.as_secs_f64() * 100.0
        } else {
            100.0
        };

        format!(
            "批处理优化器性能报告:\n\
            - 当前批次大小: {}\n\
            - 平均执行时间: {:.2}秒\n\
            - 目标执行时间: {:.2}秒\n\
            - 性能方差: {:.4}\n\
            - 执行效率: {:.1}%\n\
            - 性能趋势: {}\n\
            - 连续调整次数: {}\n\
            - 历史记录数: {}",
            self.optimal_batch_size,
            avg_time.as_secs_f64(),
            self.target_execution_time.as_secs_f64(),
            variance,
            efficiency,
            if self.performance_trend > 0.05 {
                "下降"
            } else if self.performance_trend < -0.05 {
                "改善"
            } else {
                "稳定"
            },
            self.consecutive_adjustments,
            self.last_execution_times.len()
        )
    }

    /// 重置优化器状态
    fn reset(&mut self) {
        self.last_execution_times.clear();
        self.consecutive_adjustments = 0;
        self.performance_trend = 0.0;
        self.adjustment_cooldown = Duration::from_secs(30);
        info!("🔄 批处理优化器已重置");
    }

    /// 设置新的目标执行时间
    fn set_target_execution_time(&mut self, target: Duration) {
        self.target_execution_time = target;
        info!(
            "🎯 批处理优化器目标时间已更新为: {:.2}秒",
            target.as_secs_f64()
        );
    }

    /// 设置批次大小范围
    fn set_batch_size_range(&mut self, min_size: usize, max_size: usize) {
        self.min_batch_size = min_size;
        self.max_batch_size = max_size;

        // 确保当前批次大小在新范围内
        self.optimal_batch_size = self.optimal_batch_size.max(min_size).min(max_size);

        info!("📏 批处理优化器大小范围已更新: {} - {}", min_size, max_size);
    }
}

// 订单状态枚举
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum OrderStatus {
    Pending,         // 待处理
    Active,          // 活跃
    Filled,          // 已成交
    Cancelled,       // 已取消
    Rejected,        // 被拒绝
    PartiallyFilled, // 部分成交
}

// 增强的订单信息结构体
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct EnhancedOrderInfo {
    order_id: u64,
    price: f64,
    quantity: f64,
    filled_quantity: f64,
    cost_price: Option<f64>,
    potential_sell_price: Option<f64>,
    allocated_funds: f64,
    status: OrderStatus,
    #[serde(with = "system_time_serde")]
    created_time: SystemTime,
    #[serde(with = "system_time_serde")]
    last_update_time: SystemTime,
    retry_count: u32,
}

// 订单信息结构体
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OrderInfo {
    price: f64,
    quantity: f64,
    cost_price: Option<f64>,           // 对于卖单，记录对应的买入成本价
    potential_sell_price: Option<f64>, // 对于买单，记录潜在卖出价格
    allocated_funds: f64,              // 分配的资金
}

// ============================================================================
// 订单优先级和过期管理模块
// ============================================================================

/// 订单优先级枚举
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
enum OrderPriority {
    High,   // 高优先级，如止损单、紧急平仓单
    Normal, // 普通网格单
    Low,    // 低优先级，如远离当前价格的网格单
}

impl OrderPriority {
    /// 获取中文描述
    fn as_str(&self) -> &'static str {
        match self {
            OrderPriority::High => "高优先级",
            OrderPriority::Normal => "普通优先级",
            OrderPriority::Low => "低优先级",
        }
    }

    /// 获取英文描述
    fn as_english(&self) -> &'static str {
        match self {
            OrderPriority::High => "High",
            OrderPriority::Normal => "Normal",
            OrderPriority::Low => "Low",
        }
    }

    /// 获取优先级数值（数值越大优先级越高）
    fn priority_value(&self) -> u8 {
        match self {
            OrderPriority::High => 3,
            OrderPriority::Normal => 2,
            OrderPriority::Low => 1,
        }
    }

    /// 判断是否为高优先级
    fn is_high(&self) -> bool {
        matches!(self, OrderPriority::High)
    }

    /// 判断是否为低优先级
    fn is_low(&self) -> bool {
        matches!(self, OrderPriority::Low)
    }

    /// 获取建议的超时时间（秒）
    fn suggested_timeout_seconds(&self) -> u64 {
        match self {
            OrderPriority::High => 30,    // 高优先级订单30秒超时
            OrderPriority::Normal => 300, // 普通订单5分钟超时
            OrderPriority::Low => 1800,   // 低优先级订单30分钟超时
        }
    }
}

/// 订单过期策略
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum ExpiryStrategy {
    Cancel,          // 过期后取消订单
    Reprice,         // 过期后重新定价
    Extend,          // 延长过期时间
    ConvertToMarket, // 转换为市价单（仅限高优先级）
}

impl ExpiryStrategy {
    /// 获取中文描述
    fn as_str(&self) -> &'static str {
        match self {
            ExpiryStrategy::Cancel => "取消订单",
            ExpiryStrategy::Reprice => "重新定价",
            ExpiryStrategy::Extend => "延长时间",
            ExpiryStrategy::ConvertToMarket => "转市价单",
        }
    }

    /// 获取英文描述
    fn as_english(&self) -> &'static str {
        match self {
            ExpiryStrategy::Cancel => "Cancel",
            ExpiryStrategy::Reprice => "Reprice",
            ExpiryStrategy::Extend => "Extend",
            ExpiryStrategy::ConvertToMarket => "Convert to Market",
        }
    }

    /// 判断是否需要立即处理
    fn requires_immediate_action(&self) -> bool {
        matches!(self, ExpiryStrategy::ConvertToMarket)
    }
}

/// 带优先级的订单信息
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PrioritizedOrderInfo {
    // 基础订单信息
    base_info: OrderInfo,

    // 优先级管理
    priority: OrderPriority,

    // 过期管理
    #[serde(with = "system_time_serde")]
    created_time: SystemTime,
    expiry_time: Option<SystemTime>,
    expiry_strategy: ExpiryStrategy,

    // 订单状态
    order_id: Option<u64>,
    retry_count: u32,
    last_retry_time: Option<SystemTime>,

    // 市场条件
    distance_from_current_price: f64, // 与当前价格的距离（百分比）
    market_urgency: f64,              // 市场紧急度评分 (0-100)

    // 执行统计
    execution_attempts: u32,
    total_wait_time: Duration,
    average_fill_time: Option<Duration>,
}

impl PrioritizedOrderInfo {
    /// 创建新的优先级订单
    fn new(
        base_info: OrderInfo,
        priority: OrderPriority,
        expiry_strategy: ExpiryStrategy,
        current_price: f64,
    ) -> Self {
        let created_time = SystemTime::now();
        let expiry_time =
            Some(created_time + Duration::from_secs(priority.suggested_timeout_seconds()));

        // 计算与当前价格的距离
        let distance_from_current_price =
            ((base_info.price - current_price) / current_price * 100.0).abs();

        Self {
            base_info,
            priority,
            created_time,
            expiry_time,
            expiry_strategy,
            order_id: None,
            retry_count: 0,
            last_retry_time: None,
            distance_from_current_price,
            market_urgency: 50.0, // 默认中等紧急度
            execution_attempts: 0,
            total_wait_time: Duration::new(0, 0),
            average_fill_time: None,
        }
    }

    /// 创建高优先级订单（止损单等）
    fn new_high_priority(
        base_info: OrderInfo,
        current_price: f64,
        timeout_seconds: Option<u64>,
    ) -> Self {
        let mut order = Self::new(
            base_info,
            OrderPriority::High,
            ExpiryStrategy::ConvertToMarket,
            current_price,
        );

        if let Some(timeout) = timeout_seconds {
            order.expiry_time = Some(order.created_time + Duration::from_secs(timeout));
        }

        order.market_urgency = 90.0; // 高紧急度
        order
    }

    /// 创建低优先级订单（远离价格的网格单）
    fn new_low_priority(base_info: OrderInfo, current_price: f64) -> Self {
        let mut order = Self::new(
            base_info,
            OrderPriority::Low,
            ExpiryStrategy::Cancel,
            current_price,
        );
        order.market_urgency = 20.0; // 低紧急度
        order
    }

    /// 检查订单是否过期
    fn is_expired(&self) -> bool {
        if let Some(expiry_time) = self.expiry_time {
            SystemTime::now() > expiry_time
        } else {
            false
        }
    }

    /// 获取剩余时间（秒）
    fn remaining_seconds(&self) -> Option<u64> {
        if let Some(expiry_time) = self.expiry_time {
            expiry_time
                .duration_since(SystemTime::now())
                .ok()
                .map(|d| d.as_secs())
        } else {
            None
        }
    }

    /// 延长过期时间
    fn extend_expiry(&mut self, additional_seconds: u64) {
        if let Some(expiry_time) = self.expiry_time {
            self.expiry_time = Some(expiry_time + Duration::from_secs(additional_seconds));
        } else {
            self.expiry_time = Some(SystemTime::now() + Duration::from_secs(additional_seconds));
        }
    }

    /// 更新市场紧急度
    fn update_market_urgency(&mut self, volatility: f64, price_change: f64) {
        // 基于市场波动率和价格变化计算紧急度
        let volatility_factor = (volatility * 100.0).min(50.0);
        let price_change_factor = (price_change.abs() * 100.0).min(30.0);
        let distance_factor = (100.0 - self.distance_from_current_price).max(0.0) * 0.2;

        self.market_urgency =
            (volatility_factor + price_change_factor + distance_factor).min(100.0);
    }

    /// 记录执行尝试
    fn record_execution_attempt(&mut self) {
        self.execution_attempts += 1;
        self.total_wait_time += self.created_time.elapsed().unwrap_or_default();
    }

    /// 设置订单ID
    fn set_order_id(&mut self, order_id: u64) {
        self.order_id = Some(order_id);
    }

    /// 记录重试
    fn record_retry(&mut self) {
        self.retry_count += 1;
        self.last_retry_time = Some(SystemTime::now());
    }

    /// 获取综合优先级评分
    fn get_priority_score(&self) -> f64 {
        let base_priority = self.priority.priority_value() as f64 * 30.0;
        let urgency_score = self.market_urgency * 0.4;
        let distance_penalty = self.distance_from_current_price * 0.1;
        let time_bonus = if self.is_expired() { 20.0 } else { 0.0 };

        (base_priority + urgency_score - distance_penalty + time_bonus).max(0.0)
    }

    /// 判断是否需要立即处理
    fn needs_immediate_attention(&self) -> bool {
        self.priority.is_high()
            || self.is_expired()
            || self.market_urgency > 80.0
            || self.retry_count > 3
    }

    /// 获取建议的处理策略
    fn get_suggested_action(&self, _current_price: f64) -> String {
        if self.is_expired() {
            format!("订单已过期，建议{}", self.expiry_strategy.as_str())
        } else if self.distance_from_current_price > 5.0 {
            "订单距离当前价格较远，建议降低优先级".to_string()
        } else if self.market_urgency > 80.0 {
            "市场紧急度高，建议提高优先级".to_string()
        } else {
            "正常处理".to_string()
        }
    }
}

/// 订单管理器
#[derive(Debug)]
struct OrderManager {
    prioritized_orders: Vec<PrioritizedOrderInfo>,
    max_orders: usize,
    last_cleanup_time: SystemTime,
    cleanup_interval: Duration,

    // 统计信息
    total_orders_created: u64,
    total_orders_expired: u64,
    total_orders_repriced: u64,
    total_high_priority_orders: u64,

    // 性能指标
    average_execution_time: Duration,
    success_rate: f64,
    priority_distribution: HashMap<OrderPriority, u32>,
}

impl OrderManager {
    /// 创建新的订单管理器
    fn new(max_orders: usize) -> Self {
        Self {
            prioritized_orders: Vec::new(),
            max_orders,
            last_cleanup_time: SystemTime::now(),
            cleanup_interval: Duration::from_secs(60), // 每分钟清理一次
            total_orders_created: 0,
            total_orders_expired: 0,
            total_orders_repriced: 0,
            total_high_priority_orders: 0,
            average_execution_time: Duration::new(0, 0),
            success_rate: 100.0,
            priority_distribution: HashMap::new(),
        }
    }

    /// 添加订单
    fn add_order(&mut self, order: PrioritizedOrderInfo) -> Result<(), GridStrategyError> {
        // 检查是否超过最大订单数
        if self.prioritized_orders.len() >= self.max_orders {
            // 尝试清理过期订单
            self.cleanup_expired_orders();

            // 如果仍然超过限制，移除最低优先级的订单
            if self.prioritized_orders.len() >= self.max_orders {
                self.remove_lowest_priority_order();
            }
        }

        // 更新统计信息
        self.total_orders_created += 1;
        if order.priority.is_high() {
            self.total_high_priority_orders += 1;
        }

        // 更新优先级分布
        *self
            .priority_distribution
            .entry(order.priority.clone())
            .or_insert(0) += 1;

        // 插入订单（按优先级排序）
        let insert_pos = self
            .prioritized_orders
            .binary_search_by(|a| {
                order
                    .get_priority_score()
                    .partial_cmp(&a.get_priority_score())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or_else(|pos| pos);

        self.prioritized_orders.insert(insert_pos, order);

        info!(
            "📋 添加订单到管理器 - 当前订单数: {}, 总创建数: {}",
            self.prioritized_orders.len(),
            self.total_orders_created
        );

        Ok(())
    }

    /// 获取下一个要处理的订单
    fn get_next_order(&mut self) -> Option<&mut PrioritizedOrderInfo> {
        // 按优先级评分排序，返回最高优先级的订单
        self.prioritized_orders.sort_by(|a, b| {
            b.get_priority_score()
                .partial_cmp(&a.get_priority_score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        self.prioritized_orders.first_mut()
    }

    /// 获取所有需要立即处理的订单
    fn get_urgent_orders(&mut self) -> Vec<&mut PrioritizedOrderInfo> {
        self.prioritized_orders
            .iter_mut()
            .filter(|order| order.needs_immediate_attention())
            .collect()
    }

    /// 获取过期订单
    fn get_expired_orders(&self) -> Vec<&PrioritizedOrderInfo> {
        self.prioritized_orders
            .iter()
            .filter(|order| order.is_expired())
            .collect()
    }

    /// 清理过期订单
    fn cleanup_expired_orders(&mut self) -> Vec<PrioritizedOrderInfo> {
        let now = SystemTime::now();

        // 如果还没到清理时间，跳过
        if now
            .duration_since(self.last_cleanup_time)
            .unwrap_or_default()
            < self.cleanup_interval
        {
            return Vec::new();
        }

        let (expired, remaining): (Vec<_>, Vec<_>) = self
            .prioritized_orders
            .drain(..)
            .partition(|order| order.is_expired());

        self.prioritized_orders = remaining;
        self.total_orders_expired += expired.len() as u64;
        self.last_cleanup_time = now;

        if !expired.is_empty() {
            info!(
                "🧹 清理过期订单 - 清理数量: {}, 剩余订单: {}",
                expired.len(),
                self.prioritized_orders.len()
            );
        }

        expired
    }

    /// 移除最低优先级的订单
    fn remove_lowest_priority_order(&mut self) -> Option<PrioritizedOrderInfo> {
        if self.prioritized_orders.is_empty() {
            return None;
        }

        // 找到优先级最低的订单
        let min_pos = self
            .prioritized_orders
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.get_priority_score()
                    .partial_cmp(&b.get_priority_score())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(pos, _)| pos)?;

        let removed = self.prioritized_orders.remove(min_pos);

        warn!(
            "⚠️ 移除最低优先级订单 - 优先级: {}, 剩余订单: {}",
            removed.priority.as_str(),
            self.prioritized_orders.len()
        );

        Some(removed)
    }

    /// 更新所有订单的市场紧急度
    fn update_market_conditions(&mut self, current_price: f64, volatility: f64, price_change: f64) {
        for order in &mut self.prioritized_orders {
            // 更新与当前价格的距离
            order.distance_from_current_price =
                ((order.base_info.price - current_price) / current_price * 100.0).abs();

            // 更新市场紧急度
            order.update_market_urgency(volatility, price_change);
        }
    }

    /// 根据订单ID查找订单
    fn find_order_by_id(&mut self, order_id: u64) -> Option<&mut PrioritizedOrderInfo> {
        self.prioritized_orders
            .iter_mut()
            .find(|order| order.order_id == Some(order_id))
    }

    /// 移除订单
    fn remove_order(&mut self, order_id: u64) -> Option<PrioritizedOrderInfo> {
        if let Some(pos) = self
            .prioritized_orders
            .iter()
            .position(|order| order.order_id == Some(order_id))
        {
            Some(self.prioritized_orders.remove(pos))
        } else {
            None
        }
    }

    /// 获取订单统计报告
    fn get_statistics_report(&self) -> String {
        let high_priority_count = self
            .prioritized_orders
            .iter()
            .filter(|o| o.priority.is_high())
            .count();
        let normal_priority_count = self
            .prioritized_orders
            .iter()
            .filter(|o| o.priority == OrderPriority::Normal)
            .count();
        let low_priority_count = self
            .prioritized_orders
            .iter()
            .filter(|o| o.priority.is_low())
            .count();
        let expired_count = self
            .prioritized_orders
            .iter()
            .filter(|o| o.is_expired())
            .count();
        let urgent_count = self
            .prioritized_orders
            .iter()
            .filter(|o| o.needs_immediate_attention())
            .count();

        format!(
            "📊 订单管理器统计报告\n\
            ├─ 当前订单数: {}\n\
            ├─ 高优先级: {} | 普通: {} | 低优先级: {}\n\
            ├─ 过期订单: {} | 紧急订单: {}\n\
            ├─ 总创建数: {} | 总过期数: {} | 重定价数: {}\n\
            ├─ 成功率: {:.1}% | 平均执行时间: {:.2}秒\n\
            └─ 最大容量: {} | 使用率: {:.1}%",
            self.prioritized_orders.len(),
            high_priority_count,
            normal_priority_count,
            low_priority_count,
            expired_count,
            urgent_count,
            self.total_orders_created,
            self.total_orders_expired,
            self.total_orders_repriced,
            self.success_rate,
            self.average_execution_time.as_secs_f64(),
            self.max_orders,
            (self.prioritized_orders.len() as f64 / self.max_orders as f64) * 100.0
        )
    }

    /// 获取优先级分布
    fn get_priority_distribution(&self) -> &HashMap<OrderPriority, u32> {
        &self.priority_distribution
    }

    /// 重置统计信息
    fn reset_statistics(&mut self) {
        self.total_orders_created = 0;
        self.total_orders_expired = 0;
        self.total_orders_repriced = 0;
        self.total_high_priority_orders = 0;
        self.priority_distribution.clear();
        self.success_rate = 100.0;
        self.average_execution_time = Duration::new(0, 0);
    }
}

/// 创建带优先级的订单
async fn create_order_with_priority(
    exchange_client: &ExchangeClient,
    order_info: PrioritizedOrderInfo,
    grid_config: &crate::config::GridConfig,
) -> Result<u64, GridStrategyError> {
    let start_time = SystemTime::now();

    // 记录订单创建尝试
    info!(
        "🎯 创建{}订单 - 价格: {:.4}, 数量: {:.4}, 优先级: {}",
        if order_info.base_info.price > 0.0 {
            "买入"
        } else {
            "卖出"
        },
        order_info.base_info.price,
        order_info.base_info.quantity,
        order_info.priority.as_str()
    );

    // 根据优先级调整订单参数
    let (timeout, retry_count) = match order_info.priority {
        OrderPriority::High => (Duration::from_secs(10), 5), // 高优先级：10秒超时，5次重试
        OrderPriority::Normal => (Duration::from_secs(30), 3), // 普通：30秒超时，3次重试
        OrderPriority::Low => (Duration::from_secs(60), 1),  // 低优先级：60秒超时，1次重试
    };

    // 执行订单创建（带重试机制）
    let mut last_error = None;
    for attempt in 1..=retry_count {
        // 重新创建订单请求（因为ClientOrderRequest不支持clone）
        let order_request = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: order_info.base_info.quantity > 0.0,
            reduce_only: false,
            limit_px: order_info.base_info.price,
            sz: order_info.base_info.quantity.abs(),
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Gtc".to_string(),
            }),
            cloid: None,
        };

        match tokio::time::timeout(timeout, exchange_client.order(order_request, None)).await {
            Ok(Ok(ExchangeResponseStatus::Ok(response))) => {
                if let Some(data) = response.data {
                    if !data.statuses.is_empty() {
                        if let ExchangeDataStatus::Resting(order) = &data.statuses[0] {
                            let execution_time = start_time.elapsed().unwrap_or_default();
                            info!(
                                "✅ {}订单创建成功 - ID: {}, 执行时间: {:.2}秒, 尝试次数: {}",
                                order_info.priority.as_str(),
                                order.oid,
                                execution_time.as_secs_f64(),
                                attempt
                            );

                            return Ok(order.oid);
                        } else {
                            let error_msg = format!("订单响应中未找到订单ID");
                            warn!(
                                "⚠️ {}订单创建失败 - 尝试 {}/{}: {}",
                                order_info.priority.as_str(),
                                attempt,
                                retry_count,
                                error_msg
                            );
                            last_error = Some(GridStrategyError::OrderError(error_msg));
                        }
                    } else {
                        let error_msg = format!("订单响应中未找到数据");
                        warn!(
                            "⚠️ {}订单创建失败 - 尝试 {}/{}: {}",
                            order_info.priority.as_str(),
                            attempt,
                            retry_count,
                            error_msg
                        );
                        last_error = Some(GridStrategyError::OrderError(error_msg));
                    }
                } else {
                    let error_msg = format!("订单响应中未找到订单ID");
                    warn!(
                        "⚠️ {}订单创建失败 - 尝试 {}/{}: {}",
                        order_info.priority.as_str(),
                        attempt,
                        retry_count,
                        error_msg
                    );
                    last_error = Some(GridStrategyError::OrderError(error_msg));
                }
            }
            Ok(Ok(ExchangeResponseStatus::Err(err_response))) => {
                let error_msg = format!("订单被交易所拒绝: {:?}", err_response);
                warn!(
                    "⚠️ {}订单创建失败 - 尝试 {}/{}: {}",
                    order_info.priority.as_str(),
                    attempt,
                    retry_count,
                    error_msg
                );
                last_error = Some(GridStrategyError::OrderError(error_msg));
            }
            Ok(Err(e)) => {
                let error_msg = format!("订单创建失败: {}", e);
                warn!(
                    "⚠️ {}订单创建失败 - 尝试 {}/{}: {}",
                    order_info.priority.as_str(),
                    attempt,
                    retry_count,
                    error_msg
                );
                last_error = Some(GridStrategyError::OrderError(error_msg));
            }
            Err(_) => {
                let error_msg = format!("订单创建超时 ({:.1}秒)", timeout.as_secs_f64());
                warn!(
                    "⚠️ {}订单创建超时 - 尝试 {}/{}: {}",
                    order_info.priority.as_str(),
                    attempt,
                    retry_count,
                    error_msg
                );
                last_error = Some(GridStrategyError::OrderError(error_msg));
            }
        }

        // 如果不是最后一次尝试，等待一段时间再重试
        if attempt < retry_count {
            let delay = Duration::from_millis(500 * attempt as u64); // 递增延迟
            tokio::time::sleep(delay).await;
        }
    }

    // 所有尝试都失败了
    let final_error =
        last_error.unwrap_or_else(|| GridStrategyError::OrderError("未知订单创建错误".to_string()));

    error!(
        "❌ {}订单创建最终失败 - 已尝试{}次: {}",
        order_info.priority.as_str(),
        retry_count,
        final_error
    );

    Err(final_error)
}

/// 检查过期订单并处理
async fn check_expired_orders(
    exchange_client: &ExchangeClient,
    order_manager: &mut OrderManager,
    grid_config: &crate::config::GridConfig,
    current_price: f64,
) -> Result<(), GridStrategyError> {
    let expired_orders = order_manager.cleanup_expired_orders();

    if expired_orders.is_empty() {
        return Ok(());
    }

    info!("⏰ 检查到{}个过期订单，开始处理", expired_orders.len());

    for mut expired_order in expired_orders {
        match expired_order.expiry_strategy {
            ExpiryStrategy::Cancel => {
                // 取消订单
                if let Some(order_id) = expired_order.order_id {
                    match cancel_order(exchange_client, order_id).await {
                        Ok(_) => {
                            info!("✅ 成功取消过期订单 - ID: {}", order_id);
                        }
                        Err(e) => {
                            warn!("⚠️ 取消过期订单失败 - ID: {}, 错误: {}", order_id, e);
                        }
                    }
                }
            }

            ExpiryStrategy::Reprice => {
                // 重新定价订单
                if let Some(order_id) = expired_order.order_id {
                    // 先取消原订单
                    if let Err(e) = cancel_order(exchange_client, order_id).await {
                        warn!("⚠️ 取消待重定价订单失败 - ID: {}, 错误: {}", order_id, e);
                        continue;
                    }

                    // 根据当前市场价格重新定价
                    let price_adjustment = if expired_order.base_info.quantity > 0.0 {
                        // 买单：降低价格以提高成交概率
                        -0.001 * current_price
                    } else {
                        // 卖单：提高价格以提高成交概率
                        0.001 * current_price
                    };

                    expired_order.base_info.price += price_adjustment;
                    let new_price = expired_order.base_info.price; // 保存价格用于日志
                    expired_order.expiry_time = Some(SystemTime::now() + Duration::from_secs(300)); // 延长5分钟
                    expired_order.record_retry();

                    // 重新创建订单
                    match create_order_with_priority(
                        exchange_client,
                        expired_order.clone(),
                        grid_config,
                    )
                    .await
                    {
                        Ok(new_order_id) => {
                            expired_order.set_order_id(new_order_id);
                            order_manager.add_order(expired_order)?;
                            order_manager.total_orders_repriced += 1;
                            info!(
                                "✅ 成功重定价订单 - 新ID: {}, 新价格: {:.4}",
                                new_order_id, new_price
                            );
                        }
                        Err(e) => {
                            warn!("⚠️ 重定价订单失败: {}", e);
                        }
                    }
                }
            }

            ExpiryStrategy::Extend => {
                // 延长过期时间
                let order_id = expired_order.order_id; // 保存订单ID用于日志
                expired_order.extend_expiry(expired_order.priority.suggested_timeout_seconds());
                order_manager.add_order(expired_order)?;
                info!("⏰ 延长订单过期时间 - ID: {:?}", order_id);
            }

            ExpiryStrategy::ConvertToMarket => {
                // 转换为市价单（仅限高优先级）
                if expired_order.priority.is_high() {
                    warn!("🚨 高优先级订单过期，转换为市价单处理");
                    // 这里可以实现市价单逻辑
                    // 由于hyperliquid的限制，我们暂时记录警告
                    error!("⚠️ 市价单转换功能需要根据交易所API实现");
                }
            }
        }
    }

    Ok(())
}

// 止损状态枚举
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    #[allow(dead_code)]
    fn is_normal(&self) -> bool {
        matches!(self, StopLossStatus::Normal)
    }

    /// 判断是否正在监控
    #[allow(dead_code)]
    fn is_monitoring(&self) -> bool {
        matches!(self, StopLossStatus::Monitoring)
    }

    /// 判断是否已执行（部分或完全）
    #[allow(dead_code)]
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

// 参数回滚检查点
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ParameterCheckpoint {
    min_spacing: f64,
    max_spacing: f64,
    trade_amount: f64,
    checkpoint_time: u64, // Unix timestamp
    performance_before: f64,
    reason: String,
}

#[derive(Debug, Clone)]
struct AdaptiveOrderConfig {
    // 基础配置
    base_max_age_minutes: f64,          // 基础最大存活时间
    min_age_minutes: f64,               // 最小存活时间
    max_age_minutes: f64,               // 最大存活时间
    
    // 市场状况适应
    volatility_factor: f64,             // 波动率因子 (0.5-2.0)
    trend_factor: f64,                  // 趋势因子 (0.5-2.0)
    liquidity_factor: f64,              // 流动性因子 (0.5-2.0)
    
    // 性能适应
    success_rate_factor: f64,           // 成功率因子
    profit_factor: f64,                 // 盈利因子
    
    // 动态调整历史
    adjustment_history: Vec<f64>,       // 调整历史记录
    last_adjustment_time: u64,          // 上次调整时间
    adjustment_count: u32,              // 调整次数
    
    // 统计信息
    average_fill_time_minutes: f64,     // 平均成交时间
    order_success_rate: f64,            // 订单成功率
    recent_volatility: f64,             // 最近波动率
}

impl serde::Serialize for AdaptiveOrderConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("AdaptiveOrderConfig", 12)?;
        state.serialize_field("base_max_age_minutes", &self.base_max_age_minutes)?;
        state.serialize_field("min_age_minutes", &self.min_age_minutes)?;
        state.serialize_field("max_age_minutes", &self.max_age_minutes)?;
        state.serialize_field("volatility_factor", &self.volatility_factor)?;
        state.serialize_field("trend_factor", &self.trend_factor)?;
        state.serialize_field("liquidity_factor", &self.liquidity_factor)?;
        state.serialize_field("success_rate_factor", &self.success_rate_factor)?;
        state.serialize_field("profit_factor", &self.profit_factor)?;
        state.serialize_field("adjustment_history", &self.adjustment_history)?;
        state.serialize_field("last_adjustment_time", &self.last_adjustment_time)?;
        state.serialize_field("adjustment_count", &self.adjustment_count)?;
        state.serialize_field("average_fill_time_minutes", &self.average_fill_time_minutes)?;
        state.serialize_field("order_success_rate", &self.order_success_rate)?;
        state.serialize_field("recent_volatility", &self.recent_volatility)?;
        state.end()
    }
}

impl<'de> serde::Deserialize<'de> for AdaptiveOrderConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};
        use std::fmt;

        struct AdaptiveOrderConfigVisitor;

        impl<'de> Visitor<'de> for AdaptiveOrderConfigVisitor {
            type Value = AdaptiveOrderConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct AdaptiveOrderConfig")
            }

            fn visit_map<V>(self, mut map: V) -> Result<AdaptiveOrderConfig, V::Error>
            where
                V: MapAccess<'de>,
            {
                let mut base_max_age_minutes = None;
                let mut min_age_minutes = None;
                let mut max_age_minutes = None;
                let mut volatility_factor = None;
                let mut trend_factor = None;
                let mut liquidity_factor = None;
                let mut success_rate_factor = None;
                let mut profit_factor = None;
                let mut adjustment_history = None;
                let mut last_adjustment_time = None;
                let mut adjustment_count = None;
                let mut average_fill_time_minutes = None;
                let mut order_success_rate = None;
                let mut recent_volatility = None;

                while let Some(key) = map.next_key()? {
                    match key {
                        "base_max_age_minutes" => {
                            base_max_age_minutes = Some(map.next_value()?);
                        }
                        "min_age_minutes" => {
                            min_age_minutes = Some(map.next_value()?);
                        }
                        "max_age_minutes" => {
                            max_age_minutes = Some(map.next_value()?);
                        }
                        "volatility_factor" => {
                            volatility_factor = Some(map.next_value()?);
                        }
                        "trend_factor" => {
                            trend_factor = Some(map.next_value()?);
                        }
                        "liquidity_factor" => {
                            liquidity_factor = Some(map.next_value()?);
                        }
                        "success_rate_factor" => {
                            success_rate_factor = Some(map.next_value()?);
                        }
                        "profit_factor" => {
                            profit_factor = Some(map.next_value()?);
                        }
                        "adjustment_history" => {
                            adjustment_history = Some(map.next_value()?);
                        }
                        "last_adjustment_time" => {
                            last_adjustment_time = Some(map.next_value()?);
                        }
                        "adjustment_count" => {
                            adjustment_count = Some(map.next_value()?);
                        }
                        "average_fill_time_minutes" => {
                            average_fill_time_minutes = Some(map.next_value()?);
                        }
                        "order_success_rate" => {
                            order_success_rate = Some(map.next_value()?);
                        }
                        "recent_volatility" => {
                            recent_volatility = Some(map.next_value()?);
                        }
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                Ok(AdaptiveOrderConfig {
                    base_max_age_minutes: base_max_age_minutes.unwrap_or(30.0),
                    min_age_minutes: min_age_minutes.unwrap_or(0.5),
                    max_age_minutes: max_age_minutes.unwrap_or(120.0),
                    volatility_factor: volatility_factor.unwrap_or(1.0),
                    trend_factor: trend_factor.unwrap_or(1.0),
                    liquidity_factor: liquidity_factor.unwrap_or(1.0),
                    success_rate_factor: success_rate_factor.unwrap_or(1.0),
                    profit_factor: profit_factor.unwrap_or(1.0),
                    adjustment_history: adjustment_history.unwrap_or_default(),
                    last_adjustment_time: last_adjustment_time.unwrap_or_else(safe_unix_timestamp),
                    adjustment_count: adjustment_count.unwrap_or(0),
                    average_fill_time_minutes: average_fill_time_minutes.unwrap_or(15.0),
                    order_success_rate: order_success_rate.unwrap_or(0.8),
                    recent_volatility: recent_volatility.unwrap_or(0.02),
                })
            }
        }

        deserializer.deserialize_struct(
            "AdaptiveOrderConfig",
            &[
                "base_max_age_minutes",
                "min_age_minutes", 
                "max_age_minutes",
                "volatility_factor",
                "trend_factor",
                "liquidity_factor",
                "success_rate_factor",
                "profit_factor",
                "adjustment_history",
                "last_adjustment_time",
                "adjustment_count",
                "average_fill_time_minutes",
                "order_success_rate",
                "recent_volatility",
            ],
            AdaptiveOrderConfigVisitor,
        )
    }
}

impl AdaptiveOrderConfig {
    fn new() -> Self {
        Self {
            base_max_age_minutes: 30.0,     // 基础30分钟
            min_age_minutes: 0.5,           // 最小30秒
            max_age_minutes: 120.0,         // 最大2小时
            
            volatility_factor: 1.0,
            trend_factor: 1.0,
            liquidity_factor: 1.0,
            
            success_rate_factor: 1.0,
            profit_factor: 1.0,
            
            adjustment_history: Vec::new(),
            last_adjustment_time: safe_unix_timestamp(),
            adjustment_count: 0,
            
            average_fill_time_minutes: 15.0,
            order_success_rate: 0.8,
            recent_volatility: 0.02,
        }
    }
    
    /// 根据市场状况计算自适应的订单最大存活时间
    fn calculate_adaptive_max_age(
        &mut self,
        market_analysis: &MarketAnalysis,
        grid_state: &GridState,
        current_success_rate: f64,
    ) -> f64 {
        let now = safe_unix_timestamp();
        
        // 1. 基于市场波动率调整
        let volatility_adjustment = if market_analysis.volatility > 0.05 {
            // 高波动市场：缩短订单存活时间，快速响应
            0.3 + (0.05 - market_analysis.volatility.min(0.05)) * 10.0
        } else if market_analysis.volatility < 0.01 {
            // 低波动市场：延长订单存活时间，减少频繁更新
            1.5 + (0.01 - market_analysis.volatility) * 50.0
        } else {
            // 正常波动：基础倍数
            1.0
        };
        
        // 2. 基于市场趋势调整
        let trend_adjustment = match market_analysis.trend {
            MarketTrend::Upward | MarketTrend::Downward => {
                // 明显趋势：缩短存活时间，快速跟随趋势
                0.6
            }
            MarketTrend::Sideways => {
                // 震荡市场：延长存活时间，减少无效更新
                1.4
            }
        };
        
        // 3. 基于市场状态调整
        let market_state_adjustment = match market_analysis.market_state {
            MarketState::Normal => 1.0,
            MarketState::HighVolatility => 0.4,     // 高波动：快速更新
            MarketState::Extreme => 0.2,            // 极端市场：极快更新
            MarketState::ThinLiquidity => 2.0,      // 流动性不足：延长等待
            MarketState::Flash => 0.1,              // 闪崩/闪涨：立即更新
            MarketState::Consolidation => 1.8,      // 盘整：延长等待
        };
        
        // 4. 基于订单成功率调整
        let success_rate_adjustment = if current_success_rate > 0.9 {
            // 高成功率：可以延长等待时间
            1.2
        } else if current_success_rate < 0.5 {
            // 低成功率：缩短等待时间，快速调整
            0.6
        } else {
            1.0
        };
        
        // 5. 基于盈利情况调整
        let profit_adjustment = if grid_state.realized_profit > 0.0 {
            // 盈利状态：稍微保守，延长等待
            1.1
        } else if grid_state.realized_profit < -grid_state.total_capital * 0.02 {
            // 亏损超过2%：激进调整，缩短等待
            0.7
        } else {
            1.0
        };
        
        // 6. 综合计算
        let combined_factor = volatility_adjustment 
            * trend_adjustment 
            * market_state_adjustment 
            * success_rate_adjustment 
            * profit_adjustment;
        
        let adaptive_age = self.base_max_age_minutes * combined_factor;
        
        // 7. 应用边界限制
        let final_age = adaptive_age.max(self.min_age_minutes).min(self.max_age_minutes);
        
        // 8. 记录调整历史
        if (now - self.last_adjustment_time) > 300 {  // 5分钟记录一次
            self.adjustment_history.push(final_age);
            if self.adjustment_history.len() > 20 {
                self.adjustment_history.remove(0);
            }
            self.last_adjustment_time = now;
            self.adjustment_count += 1;
        }
        
        // 9. 更新统计信息
        self.volatility_factor = volatility_adjustment;
        self.trend_factor = trend_adjustment;
        self.liquidity_factor = market_state_adjustment;
        self.success_rate_factor = success_rate_adjustment;
        self.profit_factor = profit_adjustment;
        self.recent_volatility = market_analysis.volatility;
        self.order_success_rate = current_success_rate;
        
        final_age
    }
    
    /// 获取自适应配置报告
    fn get_adaptive_report(&self) -> String {
        let avg_age = if self.adjustment_history.is_empty() {
            self.base_max_age_minutes
        } else {
            self.adjustment_history.iter().sum::<f64>() / self.adjustment_history.len() as f64
        };
        
        format!(
            "📊 自适应订单配置状态:\n\
             ├─ 当前基础存活时间: {:.1}分钟\n\
             ├─ 平均调整后时间: {:.1}分钟\n\
             ├─ 调整范围: {:.1}-{:.1}分钟\n\
             ├─ 波动率因子: {:.2}x\n\
             ├─ 趋势因子: {:.2}x\n\
             ├─ 流动性因子: {:.2}x\n\
             ├─ 成功率因子: {:.2}x\n\
             ├─ 盈利因子: {:.2}x\n\
             ├─ 调整次数: {}\n\
             ├─ 订单成功率: {:.1}%\n\
             └─ 最近波动率: {:.2}%",
            self.base_max_age_minutes,
            avg_age,
            self.min_age_minutes,
            self.max_age_minutes,
            self.volatility_factor,
            self.trend_factor,
            self.liquidity_factor,
            self.success_rate_factor,
            self.profit_factor,
            self.adjustment_count,
            self.order_success_rate * 100.0,
            self.recent_volatility * 100.0
        )
    }
    
    /// 重置统计信息
    fn reset_stats(&mut self) {
        self.adjustment_history.clear();
        self.adjustment_count = 0;
        self.last_adjustment_time = safe_unix_timestamp();
    }
    
    /// 手动调整基础参数
    fn adjust_base_parameters(&mut self, new_base_minutes: f64, new_min: f64, new_max: f64) {
        self.base_max_age_minutes = new_base_minutes.max(0.1).min(240.0);
        self.min_age_minutes = new_min.max(0.1).min(60.0);
        self.max_age_minutes = new_max.max(1.0).min(480.0);
        
        // 确保逻辑关系正确
        if self.min_age_minutes > self.base_max_age_minutes {
            self.min_age_minutes = self.base_max_age_minutes * 0.1;
        }
        if self.max_age_minutes < self.base_max_age_minutes {
            self.max_age_minutes = self.base_max_age_minutes * 3.0;
        }
    }
}

// 退出原因枚举
#[derive(Debug, Clone, PartialEq)]
enum ShutdownReason {
    UserSignal,         // 用户信号 (SIGINT/SIGTERM)
    StopLossTriggered,  // 止损触发
    MarginInsufficient, // 保证金不足
    NetworkError,       // 网络错误
    #[allow(dead_code)]
    ConfigurationError, // 配置错误
    EmergencyShutdown,  // 紧急关闭
    NormalExit,         // 正常退出
}

impl ShutdownReason {
    fn as_str(&self) -> &'static str {
        match self {
            ShutdownReason::UserSignal => "用户信号",
            ShutdownReason::StopLossTriggered => "止损触发",
            ShutdownReason::MarginInsufficient => "保证金不足",
            ShutdownReason::NetworkError => "网络错误",
            ShutdownReason::ConfigurationError => "配置错误",
            ShutdownReason::EmergencyShutdown => "紧急关闭",
            ShutdownReason::NormalExit => "正常退出",
        }
    }

    fn requires_position_close(&self) -> bool {
        matches!(
            self,
            ShutdownReason::StopLossTriggered
                | ShutdownReason::MarginInsufficient
                | ShutdownReason::EmergencyShutdown
        )
    }

    fn is_emergency(&self) -> bool {
        matches!(
            self,
            ShutdownReason::MarginInsufficient | ShutdownReason::EmergencyShutdown
        )
    }
}

// 性能数据保存结构体
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PerformanceSnapshot {
    timestamp: u64,
    total_capital: f64,
    available_funds: f64,
    position_quantity: f64,
    position_avg_price: f64,
    realized_profit: f64,
    total_trades: u32,
    winning_trades: u32,
    win_rate: f64,
    max_drawdown: f64,
    sharpe_ratio: f64,
    profit_factor: f64,
    trading_duration_hours: f64,
    final_roi: f64,
}

// 动态网格参数结构体
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DynamicGridParams {
    current_min_spacing: f64,
    current_max_spacing: f64,
    current_trade_amount: f64,
    last_optimization_time: u64, // 改为Unix timestamp便于序列化
    optimization_count: u32,
    performance_window: Vec<f64>,          // 滑动窗口性能记录
    checkpoints: Vec<ParameterCheckpoint>, // 回滚检查点
    last_checkpoint_time: u64,
    rollback_threshold: f64, // 回滚阈值（性能下降超过此值时回滚）
}

impl DynamicGridParams {
    fn new(grid_config: &crate::config::GridConfig) -> Self {
        Self {
            current_min_spacing: grid_config.min_grid_spacing,
            current_max_spacing: grid_config.max_grid_spacing,
            current_trade_amount: grid_config.trade_amount,
            last_optimization_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            optimization_count: 0,
            performance_window: Vec::new(),
            checkpoints: Vec::new(),
            last_checkpoint_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            rollback_threshold: 15.0, // 性能下降超过15分时触发回滚
        }
    }

    // 从文件加载参数
    fn load_from_file(file_path: &str, grid_config: &crate::config::GridConfig) -> Self {
        match std::fs::read_to_string(file_path) {
            Ok(content) => {
                match serde_json::from_str::<DynamicGridParams>(&content) {
                    Ok(mut params) => {
                        info!(
                            "✅ 成功加载动态参数 - 优化次数: {}, 检查点数: {}",
                            params.optimization_count,
                            params.checkpoints.len()
                        );

                        // 使用增强的参数验证
                        let validation_result =
                            validate_dynamic_parameters(&params, grid_config, "加载时");
                        validation_result.log_results("动态参数加载");

                        // 如果验证失败，进行参数修复
                        if !validation_result.is_valid {
                            warn!("⚠️ 加载的动态参数验证失败，进行自动修复");

                            // 修复最小间距
                            if params.current_min_spacing < grid_config.min_grid_spacing * 0.1
                                || params.current_min_spacing > grid_config.max_grid_spacing
                            {
                                warn!(
                                    "🔧 修复最小间距: {:.4}% -> {:.4}%",
                                    params.current_min_spacing * 100.0,
                                    grid_config.min_grid_spacing * 100.0
                                );
                                params.current_min_spacing = grid_config.min_grid_spacing;
                            }

                            // 修复最大间距
                            if params.current_max_spacing < params.current_min_spacing
                                || params.current_max_spacing > grid_config.max_grid_spacing * 2.0
                            {
                                warn!(
                                    "🔧 修复最大间距: {:.4}% -> {:.4}%",
                                    params.current_max_spacing * 100.0,
                                    grid_config.max_grid_spacing * 100.0
                                );
                                params.current_max_spacing = grid_config.max_grid_spacing;
                            }

                            // 修复交易金额
                            if params.current_trade_amount < grid_config.trade_amount * 0.1
                                || params.current_trade_amount > grid_config.total_capital * 0.2
                            {
                                warn!(
                                    "🔧 修复交易金额: {:.2} -> {:.2}",
                                    params.current_trade_amount, grid_config.trade_amount
                                );
                                params.current_trade_amount = grid_config.trade_amount;
                            }

                            // 修复回滚阈值
                            if params.rollback_threshold < 5.0 || params.rollback_threshold > 50.0 {
                                warn!("🔧 修复回滚阈值: {:.1} -> 15.0", params.rollback_threshold);
                                params.rollback_threshold = 15.0;
                            }

                            // 清理过多的检查点
                            if params.checkpoints.len() > 20 {
                                let excess = params.checkpoints.len() - 10;
                                params.checkpoints.drain(0..excess);
                                warn!("🔧 清理{}个过期检查点", excess);
                            }

                            // 清理过多的性能窗口数据
                            if params.performance_window.len() > 50 {
                                let excess = params.performance_window.len() - 20;
                                params.performance_window.drain(0..excess);
                                warn!("🔧 清理{}个过期性能数据", excess);
                            }

                            // 重新验证修复后的参数
                            let fixed_validation =
                                validate_dynamic_parameters(&params, grid_config, "修复后");
                            if fixed_validation.is_valid {
                                info!("✅ 参数修复成功");
                            } else {
                                warn!("⚠️ 参数修复后仍有问题，将使用默认参数");
                                return Self::new(grid_config);
                            }
                        }

                        params
                    }
                    Err(e) => {
                        warn!("⚠️ 解析动态参数文件失败: {:?}，使用默认参数", e);
                        Self::new(grid_config)
                    }
                }
            }
            Err(_) => {
                info!("📄 动态参数文件不存在，创建新的参数配置");
                Self::new(grid_config)
            }
        }
    }

    // 保存参数到文件
    fn save_to_file(&self, file_path: &str) -> Result<(), GridStrategyError> {
        match serde_json::to_string_pretty(self) {
            Ok(content) => match std::fs::write(file_path, content) {
                Ok(_) => {
                    info!("💾 动态参数已保存到文件: {}", file_path);
                    Ok(())
                }
                Err(e) => {
                    error!("❌ 保存动态参数失败: {:?}", e);
                    Err(GridStrategyError::ConfigError(format!(
                        "保存参数失败: {:?}",
                        e
                    )))
                }
            },
            Err(e) => {
                error!("❌ 序列化动态参数失败: {:?}", e);
                Err(GridStrategyError::ConfigError(format!(
                    "序列化参数失败: {:?}",
                    e
                )))
            }
        }
    }

    // 创建检查点
    fn create_checkpoint(&mut self, reason: String, current_performance: f64) {
        let checkpoint = ParameterCheckpoint {
            min_spacing: self.current_min_spacing,
            max_spacing: self.current_max_spacing,
            trade_amount: self.current_trade_amount,
            checkpoint_time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            performance_before: current_performance,
            reason: reason.clone(),
        };

        self.checkpoints.push(checkpoint);
        self.last_checkpoint_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // 保持最多10个检查点
        if self.checkpoints.len() > 10 {
            self.checkpoints.remove(0);
        }

        info!(
            "📍 创建参数检查点 - 原因: {}, 性能: {:.1}, 检查点数: {}",
            reason,
            current_performance,
            self.checkpoints.len()
        );
    }

    // 检查是否需要回滚
    fn should_rollback(&self, current_performance: f64) -> Option<&ParameterCheckpoint> {
        if self.checkpoints.is_empty() {
            return None;
        }

        let latest_checkpoint = self.checkpoints.last().unwrap();
        let performance_decline = latest_checkpoint.performance_before - current_performance;

        // 检查时间条件：优化后至少6小时才考虑回滚
        let time_since_checkpoint = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - latest_checkpoint.checkpoint_time;

        if time_since_checkpoint >= 6 * 60 * 60 && performance_decline > self.rollback_threshold {
            info!(
                "🔄 检测到性能下降 {:.1}分，超过阈值 {:.1}分，建议回滚",
                performance_decline, self.rollback_threshold
            );
            Some(latest_checkpoint)
        } else {
            None
        }
    }

    // 执行回滚
    fn rollback_to_checkpoint(&mut self, checkpoint: &ParameterCheckpoint) {
        info!("🔄 执行参数回滚:");
        info!("   回滚原因: {}", checkpoint.reason);
        info!("   回滚前性能: {:.1}", checkpoint.performance_before);
        info!(
            "   最小间距: {:.4}% -> {:.4}%",
            self.current_min_spacing * 100.0,
            checkpoint.min_spacing * 100.0
        );
        info!(
            "   最大间距: {:.4}% -> {:.4}%",
            self.current_max_spacing * 100.0,
            checkpoint.max_spacing * 100.0
        );
        info!(
            "   交易金额: {:.2} -> {:.2}",
            self.current_trade_amount, checkpoint.trade_amount
        );

        self.current_min_spacing = checkpoint.min_spacing;
        self.current_max_spacing = checkpoint.max_spacing;
        self.current_trade_amount = checkpoint.trade_amount;

        // 移除已回滚的检查点
        self.checkpoints.pop();

        info!("✅ 参数回滚完成");
    }
}

// 网格状态结构体
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GridState {
    total_capital: f64,
    available_funds: f64,
    position_quantity: f64,
    position_avg_price: f64,
    realized_profit: f64,
    highest_price_after_position: f64, // 持仓后最高价
    trailing_stop_price: f64,          // 浮动止损价
    stop_loss_status: StopLossStatus,  // 止损状态
    #[serde(with = "system_time_serde")]
    last_rebalance_time: SystemTime,
    historical_volatility: f64,
    performance_history: Vec<PerformanceRecord>, // 性能历史记录
    current_metrics: PerformanceMetrics,         // 当前性能指标
    #[serde(with = "system_time_serde")]
    last_margin_check: SystemTime, // 上次保证金检查时间
    connection_retry_count: u32,                 // 连接重试次数
    #[serde(with = "system_time_serde")]
    last_order_batch_time: SystemTime, // 上次批量下单时间
    dynamic_params: DynamicGridParams,           // 动态网格参数
    // 智能订单更新相关字段
    #[serde(with = "system_time_serde")]
    last_price_update: SystemTime,              // 上次价格更新时间
    last_grid_price: f64,                       // 上次网格创建时的价格
    order_update_threshold: f64,                // 订单更新阈值（价格变化百分比）
    max_order_age_minutes: f64,                 // 订单最大存活时间（分钟）
    // 自适应订单管理
    adaptive_order_config: AdaptiveOrderConfig, // 自适应订单配置
}

// 市场趋势枚举
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum MarketTrend {
    Upward,   // 上升
    Downward, // 下降
    Sideways, // 震荡
}

// 市场状态枚举
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum MarketState {
    Normal,         // 正常市场
    HighVolatility, // 高波动市场
    Extreme,        // 极端市场状况
    ThinLiquidity,  // 流动性不足
    Flash,          // 闪崩/闪涨
    Consolidation,  // 盘整状态
}

impl MarketState {
    fn as_str(&self) -> &'static str {
        match self {
            MarketState::Normal => "正常市场",
            MarketState::HighVolatility => "高波动市场",
            MarketState::Extreme => "极端市场状况",
            MarketState::ThinLiquidity => "流动性不足",
            MarketState::Flash => "闪崩/闪涨",
            MarketState::Consolidation => "盘整状态",
        }
    }

    fn as_english(&self) -> &'static str {
        match self {
            MarketState::Normal => "Normal",
            MarketState::HighVolatility => "High Volatility",
            MarketState::Extreme => "Extreme",
            MarketState::ThinLiquidity => "Thin Liquidity",
            MarketState::Flash => "Flash Move",
            MarketState::Consolidation => "Consolidation",
        }
    }

    fn risk_level(&self) -> u8 {
        match self {
            MarketState::Normal => 1,
            MarketState::Consolidation => 1,
            MarketState::HighVolatility => 3,
            MarketState::ThinLiquidity => 4,
            MarketState::Extreme => 5,
            MarketState::Flash => 5,
        }
    }

    fn requires_conservative_strategy(&self) -> bool {
        matches!(
            self,
            MarketState::Extreme | MarketState::Flash | MarketState::ThinLiquidity
        )
    }

    fn should_pause_trading(&self) -> bool {
        matches!(self, MarketState::Extreme | MarketState::Flash)
    }

    fn grid_reduction_factor(&self) -> f64 {
        match self {
            MarketState::Normal => 1.0,
            MarketState::Consolidation => 1.0,
            MarketState::HighVolatility => 0.8,
            MarketState::ThinLiquidity => 0.6,
            MarketState::Extreme => 0.4,
            MarketState::Flash => 0.2,
        }
    }
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
    price_change_5min: f64,    // 5分钟价格变化率
    market_state: MarketState, // 市场状态
    liquidity_score: f64,      // 流动性评分 (0-100)
    price_stability: f64,      // 价格稳定性 (0-100)
    #[allow(dead_code)]
    volume_anomaly: f64, // 成交量异常度 (0-100)
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

/// 自适应双向网格策略
#[derive(Debug, Clone)]
enum GridStrategy {
    Neutral,     // 中性网格：50%买单 + 50%卖单
    BullishBias, // 偏多网格：70%买单 + 30%卖单
    BearishBias, // 偏空网格：30%买单 + 70%卖单
    PureBull,    // 纯多网格：90%买单 + 10%卖单
    PureBear,    // 纯空网格：10%买单 + 90%卖单
}

impl GridStrategy {
    fn as_str(&self) -> &'static str {
        match self {
            GridStrategy::Neutral => "中性网格",
            GridStrategy::BullishBias => "偏多网格",
            GridStrategy::BearishBias => "偏空网格", 
            GridStrategy::PureBull => "纯多网格",
            GridStrategy::PureBear => "纯空网格",
        }
    }

    fn buy_ratio(&self) -> f64 {
        match self {
            GridStrategy::Neutral => 0.5,
            GridStrategy::BullishBias => 0.7,
            GridStrategy::BearishBias => 0.3,
            GridStrategy::PureBull => 0.9,
            GridStrategy::PureBear => 0.1,
        }
    }

    fn sell_ratio(&self) -> f64 {
        1.0 - self.buy_ratio()
    }
}

/// 增强的资金分配结构
#[derive(Debug, Clone)]
struct AdaptiveFundAllocation {
    buy_order_funds: f64,
    sell_order_funds: f64,
    buy_spacing_adjustment: f64,
    sell_spacing_adjustment: f64,
    position_ratio: f64,
    grid_strategy: GridStrategy,
    // 做空相关
    short_position_funds: f64,  // 做空持仓资金
    max_short_exposure: f64,    // 最大做空敞口
    // 风险控制
    risk_adjustment_factor: f64, // 风险调整因子
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

// ===== 增强风险控制模块 =====

/// 风险事件类型
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum RiskEventType {
    MarginInsufficient,   // 保证金不足
    MaxDrawdownExceeded,  // 最大回撤超限
    DailyLossExceeded,    // 每日亏损超限
    PositionSizeExceeded, // 持仓规模超限
    VolatilitySpike,      // 波动率激增
    LiquidityDrop,        // 流动性下降
    NetworkIssue,         // 网络问题
    OrderFailure,         // 订单失败
    PriceGap,             // 价格跳空
    SystemOverload,       // 系统过载
}

impl RiskEventType {
    fn as_str(&self) -> &'static str {
        match self {
            RiskEventType::MarginInsufficient => "保证金不足",
            RiskEventType::MaxDrawdownExceeded => "最大回撤超限",
            RiskEventType::DailyLossExceeded => "每日亏损超限",
            RiskEventType::PositionSizeExceeded => "持仓规模超限",
            RiskEventType::VolatilitySpike => "波动率激增",
            RiskEventType::LiquidityDrop => "流动性下降",
            RiskEventType::NetworkIssue => "网络问题",
            RiskEventType::OrderFailure => "订单失败",
            RiskEventType::PriceGap => "价格跳空",
            RiskEventType::SystemOverload => "系统过载",
        }
    }

    #[allow(dead_code)]
    fn as_english(&self) -> &'static str {
        match self {
            RiskEventType::MarginInsufficient => "Margin Insufficient",
            RiskEventType::MaxDrawdownExceeded => "Max Drawdown Exceeded",
            RiskEventType::DailyLossExceeded => "Daily Loss Exceeded",
            RiskEventType::PositionSizeExceeded => "Position Size Exceeded",
            RiskEventType::VolatilitySpike => "Volatility Spike",
            RiskEventType::LiquidityDrop => "Liquidity Drop",
            RiskEventType::NetworkIssue => "Network Issue",
            RiskEventType::OrderFailure => "Order Failure",
            RiskEventType::PriceGap => "Price Gap",
            RiskEventType::SystemOverload => "System Overload",
        }
    }

    fn severity_level(&self) -> u8 {
        match self {
            RiskEventType::MarginInsufficient => 5,   // 最高风险
            RiskEventType::MaxDrawdownExceeded => 5,  // 最高风险
            RiskEventType::DailyLossExceeded => 4,    // 高风险
            RiskEventType::PositionSizeExceeded => 4, // 高风险
            RiskEventType::VolatilitySpike => 3,      // 中等风险
            RiskEventType::LiquidityDrop => 3,        // 中等风险
            RiskEventType::PriceGap => 3,             // 中等风险
            RiskEventType::NetworkIssue => 2,         // 低风险
            RiskEventType::OrderFailure => 2,         // 低风险
            RiskEventType::SystemOverload => 2,       // 低风险
        }
    }

    #[allow(dead_code)]
    fn requires_immediate_action(&self) -> bool {
        self.severity_level() >= 4
    }

    #[allow(dead_code)]
    fn should_pause_trading(&self) -> bool {
        matches!(
            self,
            RiskEventType::MarginInsufficient
                | RiskEventType::MaxDrawdownExceeded
                | RiskEventType::DailyLossExceeded
                | RiskEventType::VolatilitySpike
        )
    }
}

/// 风险事件
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RiskEvent {
    event_type: RiskEventType,
    #[serde(with = "system_time_serde")]
    timestamp: SystemTime,
    description: String,
    current_value: f64,
    threshold_value: f64,
    severity: u8,
    handled: bool,
    action_taken: Option<String>,
}

impl RiskEvent {
    fn new(
        event_type: RiskEventType,
        description: String,
        current_value: f64,
        threshold_value: f64,
    ) -> Self {
        Self {
            severity: event_type.severity_level(),
            event_type,
            timestamp: SystemTime::now(),
            description,
            current_value,
            threshold_value,
            handled: false,
            action_taken: None,
        }
    }

    fn mark_handled(&mut self, action: String) {
        self.handled = true;
        self.action_taken = Some(action);
    }

    fn is_critical(&self) -> bool {
        self.severity >= 4
    }

    #[allow(dead_code)]
    fn age_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(self.timestamp)
            .unwrap_or_default()
            .as_secs()
    }
}

/// 风险检查结果
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct RiskCheckResult {
    overall_risk_level: u8,       // 1-5级风险等级
    should_pause_trading: bool,   // 是否应暂停交易
    should_reduce_position: bool, // 是否应减仓
    should_emergency_exit: bool,  // 是否应紧急退出
    new_events: Vec<RiskEvent>,   // 新发现的风险事件
    recommendations: Vec<String>, // 风险控制建议
    margin_ratio: f64,            // 当前保证金率
    drawdown_ratio: f64,          // 当前回撤率
    daily_loss_ratio: f64,        // 当前日亏损率
    position_risk_score: f64,     // 持仓风险评分 (0-100)
}

impl RiskCheckResult {
    fn new() -> Self {
        Self {
            overall_risk_level: 1,
            should_pause_trading: false,
            should_reduce_position: false,
            should_emergency_exit: false,
            new_events: Vec::new(),
            recommendations: Vec::new(),
            margin_ratio: 100.0,
            drawdown_ratio: 0.0,
            daily_loss_ratio: 0.0,
            position_risk_score: 0.0,
        }
    }

    fn add_event(&mut self, event: RiskEvent) {
        if event.is_critical() {
            self.overall_risk_level = self.overall_risk_level.max(event.severity);
        }

        if event.event_type.should_pause_trading() {
            self.should_pause_trading = true;
        }

        if event.event_type.requires_immediate_action() {
            self.should_reduce_position = true;
        }

        if event.severity >= 5 {
            self.should_emergency_exit = true;
        }

        self.new_events.push(event);
    }

    fn add_recommendation(&mut self, recommendation: String) {
        self.recommendations.push(recommendation);
    }

    fn has_critical_events(&self) -> bool {
        self.new_events.iter().any(|e| e.is_critical())
    }
}

/// 增强风险控制模块
#[derive(Debug)]
#[allow(dead_code)]
struct RiskControlModule {
    grid_state: Arc<Mutex<GridState>>,
    grid_config: Arc<crate::config::GridConfig>,
    stop_trading: Arc<AtomicBool>,
    risk_events: Vec<RiskEvent>,
    last_check_time: SystemTime,
    check_interval: Duration,
    daily_start_capital: f64,
    daily_start_time: SystemTime,
    consecutive_failures: u32,
    last_margin_ratio: f64,
    risk_metrics_history: Vec<(SystemTime, f64, f64, f64)>, // (时间, 保证金率, 回撤率, 日亏损率)
}

impl RiskControlModule {
    /// 创建新的风险控制模块
    fn new(
        grid_state: Arc<Mutex<GridState>>,
        grid_config: Arc<crate::config::GridConfig>,
        stop_trading: Arc<AtomicBool>,
    ) -> Self {
        let daily_start_capital = {
            let state = grid_state.lock().unwrap();
            state.total_capital
        };

        Self {
            grid_state,
            grid_config,
            stop_trading,
            risk_events: Vec::new(),
            last_check_time: SystemTime::now(),
            check_interval: Duration::from_secs(30), // 30秒检查一次
            daily_start_capital,
            daily_start_time: SystemTime::now(),
            consecutive_failures: 0,
            last_margin_ratio: 100.0,
            risk_metrics_history: Vec::new(),
        }
    }

    /// 运行风险检查
    async fn run_checks(
        &mut self,
        current_price: f64,
        price_history: &[f64],
        info_client: &InfoClient,
        user_address: ethers::types::Address,
    ) -> Result<RiskCheckResult, GridStrategyError> {
        let now = SystemTime::now();

        // 检查是否到了检查时间
        if now.duration_since(self.last_check_time).unwrap_or_default() < self.check_interval {
            return Ok(RiskCheckResult::new());
        }

        self.last_check_time = now;

        let mut result = RiskCheckResult::new();

        // 获取当前状态
        let (liquid_capital, position_quantity, _realized_profit, max_drawdown, _total_capital) = {
            let state = self.grid_state.lock().unwrap();
            (
                state.available_funds + state.position_quantity * current_price, // 流动资产
                state.position_quantity,
                state.realized_profit,
                state.current_metrics.max_drawdown,
                state.total_capital, // 初始总资产
            )
        };

        // 1. 检查保证金率
        match self.check_margin_ratio(info_client, user_address).await {
            Ok(margin_ratio) => {
                result.margin_ratio = margin_ratio;
                self.last_margin_ratio = margin_ratio;

                if margin_ratio < self.grid_config.margin_safety_threshold {
                    let event = RiskEvent::new(
                        RiskEventType::MarginInsufficient,
                        format!(
                            "保证金率({:.1}%)低于安全阈值({:.1}%)",
                            margin_ratio * 100.0,
                            self.grid_config.margin_safety_threshold * 100.0
                        ),
                        margin_ratio,
                        self.grid_config.margin_safety_threshold,
                    );
                    result.add_event(event);
                }
            }
            Err(e) => {
                warn!("⚠️ 保证金率检查失败: {:?}", e);
                self.consecutive_failures += 1;

                if self.consecutive_failures >= 3 {
                    let event = RiskEvent::new(
                        RiskEventType::NetworkIssue,
                        format!("连续{}次保证金检查失败", self.consecutive_failures),
                        self.consecutive_failures as f64,
                        3.0,
                    );
                    result.add_event(event);
                }
            }
        }

        // 2. 检查最大回撤
        result.drawdown_ratio = max_drawdown;
        if max_drawdown > self.grid_config.max_drawdown {
            let event = RiskEvent::new(
                RiskEventType::MaxDrawdownExceeded,
                format!(
                    "最大回撤({:.2}%)超过限制({:.2}%)",
                    max_drawdown * 100.0,
                    self.grid_config.max_drawdown * 100.0
                ),
                max_drawdown,
                self.grid_config.max_drawdown,
            );
            result.add_event(event);
        }

        // 3. 检查每日亏损
        let daily_loss_ratio =
            (self.daily_start_capital - liquid_capital) / self.daily_start_capital;
        result.daily_loss_ratio = daily_loss_ratio;

        if daily_loss_ratio > self.grid_config.max_daily_loss {
            let event = RiskEvent::new(
                RiskEventType::DailyLossExceeded,
                format!(
                    "每日亏损({:.2}%)超过限制({:.2}%)",
                    daily_loss_ratio * 100.0,
                    self.grid_config.max_daily_loss * 100.0
                ),
                daily_loss_ratio,
                self.grid_config.max_daily_loss,
            );
            result.add_event(event);
        }

        // 4. 检查持仓规模
        let position_value = position_quantity.abs() * current_price;
        let position_ratio = position_value / liquid_capital;
        result.position_risk_score = position_ratio * 100.0;

        if position_value > self.grid_config.max_position {
            let event = RiskEvent::new(
                RiskEventType::PositionSizeExceeded,
                format!(
                    "持仓价值({:.2})超过最大限制({:.2})",
                    position_value, self.grid_config.max_position
                ),
                position_value,
                self.grid_config.max_position,
            );
            result.add_event(event);
        }

        // 5. 检查市场波动率
        if price_history.len() >= 10 {
            let volatility = calculate_market_volatility(price_history);
            if volatility > 0.15 {
                // 15%的波动率阈值
                let event = RiskEvent::new(
                    RiskEventType::VolatilitySpike,
                    format!("市场波动率({:.2}%)异常高", volatility * 100.0),
                    volatility,
                    0.15,
                );
                result.add_event(event);
            }
        }

        // 6. 检查价格跳空
        if price_history.len() >= 2 {
            let last_price = price_history[price_history.len() - 2];
            let price_gap = ((current_price - last_price) / last_price).abs();

            if price_gap > 0.05 {
                // 5%的价格跳空阈值
                let event = RiskEvent::new(
                    RiskEventType::PriceGap,
                    format!("价格跳空({:.2}%)过大", price_gap * 100.0),
                    price_gap,
                    0.05,
                );
                result.add_event(event);
            }
        }

        // 7. 生成风险控制建议
        self.generate_recommendations(&mut result);

        // 8. 记录风险指标历史
        self.risk_metrics_history.push((
            now,
            result.margin_ratio,
            result.drawdown_ratio,
            result.daily_loss_ratio,
        ));

        // 保留最近100条记录
        if self.risk_metrics_history.len() > 100 {
            self.risk_metrics_history.remove(0);
        }

        // 9. 检查是否需要重置每日统计
        if now
            .duration_since(self.daily_start_time)
            .unwrap_or_default()
            .as_secs()
            >= 24 * 60 * 60
        {
            self.reset_daily_stats();
        }

        Ok(result)
    }

    /// 处理风险事件
    async fn handle_risk_event(&mut self, mut event: RiskEvent) -> Result<(), GridStrategyError> {
        info!(
            "🚨 处理风险事件: {} - {}",
            event.event_type.as_str(),
            event.description
        );

        let action = match event.event_type {
            RiskEventType::MarginInsufficient => {
                self.stop_trading.store(true, Ordering::SeqCst);
                "暂停交易，等待保证金补充".to_string()
            }
            RiskEventType::MaxDrawdownExceeded => {
                self.stop_trading.store(true, Ordering::SeqCst);
                "触发最大回撤保护，暂停交易".to_string()
            }
            RiskEventType::DailyLossExceeded => {
                self.stop_trading.store(true, Ordering::SeqCst);
                "每日亏损超限，暂停交易".to_string()
            }
            RiskEventType::PositionSizeExceeded => "建议减仓，降低持仓风险".to_string(),
            RiskEventType::VolatilitySpike => "市场波动加剧，建议减少网格密度".to_string(),
            RiskEventType::LiquidityDrop => "流动性下降，建议暂停新订单".to_string(),
            RiskEventType::NetworkIssue => "网络问题，增加重试机制".to_string(),
            RiskEventType::OrderFailure => "订单失败，检查订单参数".to_string(),
            RiskEventType::PriceGap => "价格跳空，暂停交易等待市场稳定".to_string(),
            RiskEventType::SystemOverload => "系统过载，降低交易频率".to_string(),
        };

        event.mark_handled(action.clone());
        self.risk_events.push(event);

        // 保留最近50个风险事件
        if self.risk_events.len() > 50 {
            self.risk_events.remove(0);
        }

        info!("✅ 风险事件处理完成: {}", action);
        Ok(())
    }

    /// 生成风险控制建议
    fn generate_recommendations(&self, result: &mut RiskCheckResult) {
        if result.margin_ratio < 1.5 {
            result.add_recommendation("保证金率过低，建议立即补充保证金或减仓".to_string());
        } else if result.margin_ratio < 2.0 {
            result.add_recommendation("保证金率偏低，建议适当减仓".to_string());
        }

        if result.drawdown_ratio > 0.8 * self.grid_config.max_drawdown {
            result.add_recommendation("回撤接近限制，建议降低风险敞口".to_string());
        }

        if result.daily_loss_ratio > 0.8 * self.grid_config.max_daily_loss {
            result.add_recommendation("日亏损接近限制，建议暂停交易".to_string());
        }

        if result.position_risk_score > 80.0 {
            result.add_recommendation("持仓风险过高，建议分批减仓".to_string());
        }

        if self.consecutive_failures > 0 {
            result.add_recommendation(format!(
                "连续{}次检查失败，建议检查网络连接",
                self.consecutive_failures
            ));
        }
    }

    /// 检查保证金率
    async fn check_margin_ratio(
        &self,
        info_client: &InfoClient,
        user_address: ethers::types::Address,
    ) -> Result<f64, GridStrategyError> {
        match get_account_info(info_client, user_address).await {
            Ok(account_info) => {
                let margin_summary = &account_info.margin_summary;
                let account_value = margin_summary.account_value.parse().unwrap_or(0.0);
                let total_margin_used = margin_summary.total_margin_used.parse().unwrap_or(0.0);

                if total_margin_used > 0.0 {
                    Ok(account_value / total_margin_used)
                } else {
                    Ok(f64::INFINITY) // 无持仓时保证金率为无穷大
                }
            }
            Err(e) => {
                // 注意：这里不能修改self，因为方法是&self
                // consecutive_failures的增加在run_checks方法中处理
                Err(e)
            }
        }
    }

    /// 重置每日统计
    fn reset_daily_stats(&mut self) {
        let current_capital = {
            let state = self.grid_state.lock().unwrap();
            state.available_funds + state.position_quantity * 0.0 // 需要当前价格
        };

        self.daily_start_capital = current_capital;
        self.daily_start_time = SystemTime::now();
        self.consecutive_failures = 0;

        info!("🔄 每日风险统计已重置");
    }

    /// 获取风险事件历史
    fn get_recent_events(&self, hours: u64) -> Vec<&RiskEvent> {
        let cutoff_time = SystemTime::now() - Duration::from_secs(hours * 3600);
        self.risk_events
            .iter()
            .filter(|event| event.timestamp > cutoff_time)
            .collect()
    }

    /// 获取风险统计报告
    fn get_risk_report(&self) -> String {
        let recent_events = self.get_recent_events(24);
        let critical_events = recent_events.iter().filter(|e| e.is_critical()).count();

        format!(
            "=== 风险控制报告 ===\n\
            最近24小时风险事件: {}\n\
            其中严重事件: {}\n\
            连续失败次数: {}\n\
            最近保证金率: {:.2}%\n\
            风险指标历史记录: {} 条\n\
            ==================",
            recent_events.len(),
            critical_events,
            self.consecutive_failures,
            self.last_margin_ratio * 100.0,
            self.risk_metrics_history.len()
        )
    }
}

// ============================================================================
// WebSocket 连接管理模块
// ============================================================================

/// 连接状态枚举
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum ConnectionStatus {
    Connected,    // 已连接
    Disconnected, // 已断开
    Connecting,   // 连接中
    Reconnecting, // 重连中
    Failed,       // 连接失败
    Unstable,     // 连接不稳定
}

impl ConnectionStatus {
    /// 获取中文描述
    fn as_str(&self) -> &'static str {
        match self {
            ConnectionStatus::Connected => "已连接",
            ConnectionStatus::Disconnected => "已断开",
            ConnectionStatus::Connecting => "连接中",
            ConnectionStatus::Reconnecting => "重连中",
            ConnectionStatus::Failed => "连接失败",
            ConnectionStatus::Unstable => "连接不稳定",
        }
    }

    /// 获取英文描述
    fn as_english(&self) -> &'static str {
        match self {
            ConnectionStatus::Connected => "Connected",
            ConnectionStatus::Disconnected => "Disconnected",
            ConnectionStatus::Connecting => "Connecting",
            ConnectionStatus::Reconnecting => "Reconnecting",
            ConnectionStatus::Failed => "Failed",
            ConnectionStatus::Unstable => "Unstable",
        }
    }

    /// 判断是否为健康状态
    fn is_healthy(&self) -> bool {
        matches!(self, ConnectionStatus::Connected)
    }

    /// 判断是否需要重连
    fn needs_reconnect(&self) -> bool {
        matches!(
            self,
            ConnectionStatus::Disconnected | ConnectionStatus::Failed | ConnectionStatus::Unstable
        )
    }

    /// 判断是否正在连接
    fn is_connecting(&self) -> bool {
        matches!(
            self,
            ConnectionStatus::Connecting | ConnectionStatus::Reconnecting
        )
    }
}

/// 连接事件类型
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum ConnectionEventType {
    Connected,        // 连接成功
    Disconnected,     // 连接断开
    ReconnectAttempt, // 重连尝试
    ReconnectSuccess, // 重连成功
    ReconnectFailed,  // 重连失败
    HeartbeatTimeout, // 心跳超时
    DataReceived,     // 数据接收
    ErrorOccurred,    // 错误发生
    QualityDegraded,  // 连接质量下降
    QualityImproved,  // 连接质量改善
}

impl ConnectionEventType {
    /// 获取中文描述
    fn as_str(&self) -> &'static str {
        match self {
            ConnectionEventType::Connected => "连接成功",
            ConnectionEventType::Disconnected => "连接断开",
            ConnectionEventType::ReconnectAttempt => "重连尝试",
            ConnectionEventType::ReconnectSuccess => "重连成功",
            ConnectionEventType::ReconnectFailed => "重连失败",
            ConnectionEventType::HeartbeatTimeout => "心跳超时",
            ConnectionEventType::DataReceived => "数据接收",
            ConnectionEventType::ErrorOccurred => "错误发生",
            ConnectionEventType::QualityDegraded => "连接质量下降",
            ConnectionEventType::QualityImproved => "连接质量改善",
        }
    }

    /// 获取英文描述
    fn as_english(&self) -> &'static str {
        match self {
            ConnectionEventType::Connected => "Connected",
            ConnectionEventType::Disconnected => "Disconnected",
            ConnectionEventType::ReconnectAttempt => "Reconnect Attempt",
            ConnectionEventType::ReconnectSuccess => "Reconnect Success",
            ConnectionEventType::ReconnectFailed => "Reconnect Failed",
            ConnectionEventType::HeartbeatTimeout => "Heartbeat Timeout",
            ConnectionEventType::DataReceived => "Data Received",
            ConnectionEventType::ErrorOccurred => "Error Occurred",
            ConnectionEventType::QualityDegraded => "Quality Degraded",
            ConnectionEventType::QualityImproved => "Quality Improved",
        }
    }

    /// 获取严重程度 (1-5)
    fn severity_level(&self) -> u8 {
        match self {
            ConnectionEventType::Connected => 1,
            ConnectionEventType::ReconnectSuccess => 1,
            ConnectionEventType::QualityImproved => 1,
            ConnectionEventType::DataReceived => 1,
            ConnectionEventType::ReconnectAttempt => 2,
            ConnectionEventType::QualityDegraded => 3,
            ConnectionEventType::HeartbeatTimeout => 4,
            ConnectionEventType::Disconnected => 4,
            ConnectionEventType::ReconnectFailed => 4,
            ConnectionEventType::ErrorOccurred => 5,
        }
    }

    /// 判断是否为错误事件
    fn is_error(&self) -> bool {
        matches!(
            self,
            ConnectionEventType::Disconnected
                | ConnectionEventType::ReconnectFailed
                | ConnectionEventType::HeartbeatTimeout
                | ConnectionEventType::ErrorOccurred
        )
    }
}

/// 连接事件记录
#[derive(Debug, Clone)]
struct ConnectionEvent {
    event_type: ConnectionEventType,
    timestamp: Instant,
    description: String,
    error_message: Option<String>,
    latency_ms: Option<u64>,
    retry_count: u32,
}

impl ConnectionEvent {
    /// 创建新的连接事件
    fn new(event_type: ConnectionEventType, description: String) -> Self {
        Self {
            event_type,
            timestamp: Instant::now(),
            description,
            error_message: None,
            latency_ms: None,
            retry_count: 0,
        }
    }

    /// 创建带错误信息的事件
    fn with_error(event_type: ConnectionEventType, description: String, error: String) -> Self {
        Self {
            event_type,
            timestamp: Instant::now(),
            description,
            error_message: Some(error),
            latency_ms: None,
            retry_count: 0,
        }
    }

    /// 创建带延迟信息的事件
    fn with_latency(event_type: ConnectionEventType, description: String, latency_ms: u64) -> Self {
        Self {
            event_type,
            timestamp: Instant::now(),
            description,
            error_message: None,
            latency_ms: Some(latency_ms),
            retry_count: 0,
        }
    }

    /// 设置重试次数
    fn with_retry_count(mut self, retry_count: u32) -> Self {
        self.retry_count = retry_count;
        self
    }

    /// 获取事件年龄（秒）
    fn age_seconds(&self) -> u64 {
        self.timestamp.elapsed().as_secs()
    }

    /// 判断是否为最近事件（5分钟内）
    fn is_recent(&self) -> bool {
        self.age_seconds() < 300
    }
}

/// 连接质量指标
#[derive(Debug, Clone)]
struct ConnectionQuality {
    average_latency_ms: f64,   // 平均延迟
    packet_loss_rate: f64,     // 丢包率 (0-1)
    connection_stability: f64, // 连接稳定性 (0-100)
    data_throughput: f64,      // 数据吞吐量
    error_rate: f64,           // 错误率 (0-1)
    uptime_percentage: f64,    // 在线时间百分比 (0-100)
}

impl ConnectionQuality {
    /// 创建默认连接质量
    fn new() -> Self {
        Self {
            average_latency_ms: 0.0,
            packet_loss_rate: 0.0,
            connection_stability: 100.0,
            data_throughput: 0.0,
            error_rate: 0.0,
            uptime_percentage: 100.0,
        }
    }

    /// 更新延迟指标
    fn update_latency(&mut self, latency_ms: u64) {
        // 使用指数移动平均
        let alpha = 0.1;
        self.average_latency_ms =
            alpha * latency_ms as f64 + (1.0 - alpha) * self.average_latency_ms;
    }

    /// 记录错误
    fn record_error(&mut self) {
        let alpha = 0.1;
        self.error_rate = alpha * 1.0 + (1.0 - alpha) * self.error_rate;
        self.connection_stability = (self.connection_stability * 0.95).max(0.0);
    }

    /// 记录成功
    fn record_success(&mut self) {
        let alpha = 0.05;
        self.error_rate = (1.0 - alpha) * self.error_rate;
        self.connection_stability = (self.connection_stability * 1.01).min(100.0);
    }

    /// 获取总体质量评分 (0-100)
    fn overall_score(&self) -> f64 {
        let latency_score = if self.average_latency_ms < 50.0 {
            100.0
        } else if self.average_latency_ms < 100.0 {
            80.0
        } else if self.average_latency_ms < 200.0 {
            60.0
        } else if self.average_latency_ms < 500.0 {
            40.0
        } else {
            20.0
        };

        let error_score = (1.0 - self.error_rate) * 100.0;
        let stability_score = self.connection_stability;

        (latency_score * 0.3 + error_score * 0.4 + stability_score * 0.3)
            .min(100.0)
            .max(0.0)
    }

    /// 判断连接质量是否良好
    fn is_good(&self) -> bool {
        self.overall_score() >= 70.0
    }

    /// 判断连接质量是否较差
    fn is_poor(&self) -> bool {
        self.overall_score() < 40.0
    }
}

/// WebSocket 连接管理器
#[allow(dead_code)]
struct ConnectionManager {
    // 客户端引用（注意：这里我们不直接持有客户端，而是通过参数传递）
    last_heartbeat: Instant,
    last_data_received: Instant,
    reconnect_count: u32,
    status: ConnectionStatus,

    // 连接配置
    heartbeat_interval: Duration,
    heartbeat_timeout: Duration,
    max_reconnect_attempts: u32,
    reconnect_base_delay: Duration,
    max_reconnect_delay: Duration,

    // 连接质量监控
    quality: ConnectionQuality,
    events: Vec<ConnectionEvent>,
    max_events: usize,

    // 统计信息
    total_connections: u32,
    total_disconnections: u32,
    total_reconnect_attempts: u32,
    successful_reconnects: u32,
    connection_start_time: Instant,
    total_downtime: Duration,
    last_disconnect_time: Option<Instant>,

    // 自适应参数
    adaptive_heartbeat: bool,
    dynamic_timeout: bool,
    connection_degraded: bool,
}

impl ConnectionManager {
    /// 创建新的连接管理器
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_heartbeat: now,
            last_data_received: now,
            reconnect_count: 0,
            status: ConnectionStatus::Disconnected,

            // 默认配置
            heartbeat_interval: Duration::from_secs(30),
            heartbeat_timeout: Duration::from_secs(60),
            max_reconnect_attempts: 10,
            reconnect_base_delay: Duration::from_secs(1),
            max_reconnect_delay: Duration::from_secs(60),

            quality: ConnectionQuality::new(),
            events: Vec::new(),
            max_events: 100,

            total_connections: 0,
            total_disconnections: 0,
            total_reconnect_attempts: 0,
            successful_reconnects: 0,
            connection_start_time: now,
            total_downtime: Duration::ZERO,
            last_disconnect_time: None,

            adaptive_heartbeat: true,
            dynamic_timeout: true,
            connection_degraded: false,
        }
    }

    /// 检查连接状态
    async fn check_connection(
        &mut self,
        info_client: &InfoClient,
        user_address: ethers::types::Address,
    ) -> Result<bool, GridStrategyError> {
        let check_start = Instant::now();

        // 1. 检查心跳超时
        if self.last_heartbeat.elapsed() > self.heartbeat_timeout {
            self.record_event(ConnectionEvent::new(
                ConnectionEventType::HeartbeatTimeout,
                format!("心跳超时: {}秒", self.last_heartbeat.elapsed().as_secs()),
            ));

            self.status = ConnectionStatus::Unstable;
            self.quality.record_error();

            // 尝试重连
            return self.attempt_reconnect(info_client, user_address).await;
        }

        // 2. 检查数据接收超时
        if self.last_data_received.elapsed() > self.heartbeat_timeout * 2 {
            self.record_event(ConnectionEvent::new(
                ConnectionEventType::QualityDegraded,
                format!(
                    "数据接收超时: {}秒",
                    self.last_data_received.elapsed().as_secs()
                ),
            ));

            self.connection_degraded = true;
            self.quality.record_error();
        }

        // 3. 执行实际连接测试
        match self.test_connection(info_client, user_address).await {
            Ok(latency_ms) => {
                let check_duration = check_start.elapsed();

                // 更新连接状态
                if self.status != ConnectionStatus::Connected {
                    self.on_connection_established();
                }

                // 更新质量指标
                self.quality.update_latency(latency_ms);
                self.quality.record_success();

                // 记录数据接收
                self.last_data_received = Instant::now();
                self.last_heartbeat = Instant::now();

                // 记录事件
                self.record_event(ConnectionEvent::with_latency(
                    ConnectionEventType::DataReceived,
                    format!("连接检查成功，延迟: {}ms", latency_ms),
                    latency_ms,
                ));

                // 自适应调整
                if self.adaptive_heartbeat {
                    self.adjust_heartbeat_interval(latency_ms);
                }

                // 重置连接降级标志
                if self.connection_degraded && self.quality.is_good() {
                    self.connection_degraded = false;
                    self.record_event(ConnectionEvent::new(
                        ConnectionEventType::QualityImproved,
                        "连接质量已恢复".to_string(),
                    ));
                }

                info!(
                    "连接检查成功 - 状态: {}, 延迟: {}ms, 质量评分: {:.1}, 检查耗时: {}ms",
                    self.status.as_str(),
                    latency_ms,
                    self.quality.overall_score(),
                    check_duration.as_millis()
                );

                Ok(true)
            }
            Err(e) => {
                // 连接失败
                self.on_connection_lost(&e);

                // 尝试重连
                self.attempt_reconnect(info_client, user_address).await
            }
        }
    }

    /// 尝试重连
    async fn attempt_reconnect(
        &mut self,
        info_client: &InfoClient,
        user_address: ethers::types::Address,
    ) -> Result<bool, GridStrategyError> {
        while self.reconnect_count < self.max_reconnect_attempts {
            self.reconnect_count += 1;
            self.total_reconnect_attempts += 1;
            self.status = ConnectionStatus::Reconnecting;

            // 计算重连延迟（指数退避）
            let delay = self.calculate_reconnect_delay();

            self.record_event(
                ConnectionEvent::new(
                    ConnectionEventType::ReconnectAttempt,
                    format!(
                        "开始第{}次重连尝试，延迟{}秒",
                        self.reconnect_count,
                        delay.as_secs()
                    ),
                )
                .with_retry_count(self.reconnect_count),
            );

            info!(
                "开始重连尝试 - 第{}/{}次，延迟: {}秒",
                self.reconnect_count,
                self.max_reconnect_attempts,
                delay.as_secs()
            );

            // 等待重连延迟
            sleep(delay).await;

            // 执行重连
            match self.reconnect(info_client, user_address).await {
                Ok(()) => {
                    self.on_reconnect_success();
                    return Ok(true);
                }
                Err(e) => {
                    self.record_event(ConnectionEvent::with_error(
                        ConnectionEventType::ReconnectFailed,
                        format!("第{}次重连失败", self.reconnect_count),
                        e.to_string(),
                    ));

                    warn!(
                        "重连失败 - 第{}/{}次: {}",
                        self.reconnect_count, self.max_reconnect_attempts, e
                    );

                    // 继续下一次重连尝试
                }
            }
        }

        // 达到最大重试次数
        self.status = ConnectionStatus::Failed;
        self.record_event(
            ConnectionEvent::new(
                ConnectionEventType::ReconnectFailed,
                format!(
                    "重连失败，已达到最大重试次数: {}",
                    self.max_reconnect_attempts
                ),
            )
            .with_retry_count(self.reconnect_count),
        );

        error!(
            "连接重连失败 - 已达到最大重试次数: {}, 总重连尝试: {}",
            self.max_reconnect_attempts, self.total_reconnect_attempts
        );

        Err(GridStrategyError::NetworkError(format!(
            "连接重连失败，已达到最大重试次数: {}",
            self.max_reconnect_attempts
        )))
    }

    /// 执行重连
    async fn reconnect(
        &mut self,
        info_client: &InfoClient,
        user_address: ethers::types::Address,
    ) -> Result<(), GridStrategyError> {
        // 注意：这里我们不能重新创建客户端，因为客户端是在外部创建的
        // 我们只能测试现有连接是否恢复

        match self.test_connection(info_client, user_address).await {
            Ok(latency_ms) => {
                self.quality.update_latency(latency_ms);
                self.quality.record_success();
                Ok(())
            }
            Err(e) => {
                self.quality.record_error();
                Err(e)
            }
        }
    }

    /// 测试连接
    async fn test_connection(
        &self,
        info_client: &InfoClient,
        user_address: ethers::types::Address,
    ) -> Result<u64, GridStrategyError> {
        let start_time = Instant::now();

        // 使用账户信息查询作为连接测试
        match get_account_info(info_client, user_address).await {
            Ok(_) => {
                let latency_ms = start_time.elapsed().as_millis() as u64;
                Ok(latency_ms)
            }
            Err(e) => Err(e),
        }
    }

    /// 连接建立时的处理
    fn on_connection_established(&mut self) {
        let was_disconnected = matches!(
            self.status,
            ConnectionStatus::Disconnected | ConnectionStatus::Failed
        );

        self.status = ConnectionStatus::Connected;
        self.last_heartbeat = Instant::now();
        self.last_data_received = Instant::now();

        if was_disconnected {
            self.total_connections += 1;

            // 计算停机时间
            if let Some(disconnect_time) = self.last_disconnect_time {
                self.total_downtime += disconnect_time.elapsed();
                self.last_disconnect_time = None;
            }
        }

        self.record_event(ConnectionEvent::new(
            ConnectionEventType::Connected,
            "连接已建立".to_string(),
        ));

        info!(
            "连接已建立 - 总连接次数: {}, 质量评分: {:.1}",
            self.total_connections,
            self.quality.overall_score()
        );
    }

    /// 连接丢失时的处理
    fn on_connection_lost(&mut self, error: &GridStrategyError) {
        if self.status == ConnectionStatus::Connected {
            self.total_disconnections += 1;
            self.last_disconnect_time = Some(Instant::now());
        }

        self.status = ConnectionStatus::Disconnected;
        self.quality.record_error();

        self.record_event(ConnectionEvent::with_error(
            ConnectionEventType::Disconnected,
            "连接已断开".to_string(),
            error.to_string(),
        ));

        warn!(
            "连接已断开 - 总断开次数: {}, 错误: {}",
            self.total_disconnections, error
        );
    }

    /// 重连成功时的处理
    fn on_reconnect_success(&mut self) {
        self.successful_reconnects += 1;
        self.reconnect_count = 0; // 重置重连计数
        self.status = ConnectionStatus::Connected;
        self.last_heartbeat = Instant::now();
        self.last_data_received = Instant::now();

        // 计算停机时间
        if let Some(disconnect_time) = self.last_disconnect_time {
            self.total_downtime += disconnect_time.elapsed();
            self.last_disconnect_time = None;
        }

        self.record_event(
            ConnectionEvent::new(
                ConnectionEventType::ReconnectSuccess,
                format!("重连成功，尝试次数: {}", self.total_reconnect_attempts),
            )
            .with_retry_count(self.total_reconnect_attempts),
        );

        info!(
            "重连成功 - 成功重连次数: {}/{}, 质量评分: {:.1}",
            self.successful_reconnects,
            self.total_reconnect_attempts,
            self.quality.overall_score()
        );
    }

    /// 计算重连延迟（指数退避）
    fn calculate_reconnect_delay(&self) -> Duration {
        let base_delay_ms = self.reconnect_base_delay.as_millis() as u64;
        let max_delay_ms = self.max_reconnect_delay.as_millis() as u64;

        // 指数退避：delay = base * 2^(retry_count - 1)
        let delay_ms = base_delay_ms * 2_u64.pow((self.reconnect_count - 1).min(10));
        let final_delay_ms = delay_ms.min(max_delay_ms);

        Duration::from_millis(final_delay_ms)
    }

    /// 自适应调整心跳间隔
    fn adjust_heartbeat_interval(&mut self, latency_ms: u64) {
        if !self.adaptive_heartbeat {
            return;
        }

        let new_interval = if latency_ms < 50 {
            Duration::from_secs(45) // 延迟低，可以延长心跳间隔
        } else if latency_ms < 100 {
            Duration::from_secs(30) // 正常延迟
        } else if latency_ms < 200 {
            Duration::from_secs(20) // 延迟较高，缩短心跳间隔
        } else {
            Duration::from_secs(15) // 延迟很高，频繁检查
        };

        if new_interval != self.heartbeat_interval {
            self.heartbeat_interval = new_interval;
            info!(
                "自适应调整心跳间隔: {}秒 (基于延迟: {}ms)",
                new_interval.as_secs(),
                latency_ms
            );
        }
    }

    /// 记录连接事件
    fn record_event(&mut self, event: ConnectionEvent) {
        self.events.push(event);

        // 限制事件数量
        if self.events.len() > self.max_events {
            self.events.remove(0);
        }
    }

    /// 获取连接状态
    fn get_status(&self) -> &ConnectionStatus {
        &self.status
    }

    /// 获取连接质量
    fn get_quality(&self) -> &ConnectionQuality {
        &self.quality
    }

    /// 判断是否需要检查连接
    fn should_check_connection(&self) -> bool {
        self.last_heartbeat.elapsed() >= self.heartbeat_interval
    }

    /// 判断连接是否健康
    fn is_healthy(&self) -> bool {
        self.status.is_healthy() && self.quality.is_good() && !self.connection_degraded
    }

    /// 获取最近的错误事件
    fn get_recent_errors(&self, minutes: u64) -> Vec<&ConnectionEvent> {
        let cutoff_time = Instant::now() - Duration::from_secs(minutes * 60);
        self.events
            .iter()
            .filter(|event| event.timestamp > cutoff_time && event.event_type.is_error())
            .collect()
    }

    /// 获取连接统计报告
    fn get_connection_report(&self) -> String {
        let uptime_percentage = if self.connection_start_time.elapsed().as_secs() > 0 {
            let total_time = self.connection_start_time.elapsed();
            let uptime = total_time - self.total_downtime;
            (uptime.as_secs_f64() / total_time.as_secs_f64()) * 100.0
        } else {
            100.0
        };

        let recent_errors = self.get_recent_errors(60); // 最近1小时的错误

        format!(
            "=== 连接管理报告 ===\n\
            当前状态: {} ({})\n\
            连接质量评分: {:.1}/100\n\
            平均延迟: {:.1}ms\n\
            错误率: {:.2}%\n\
            连接稳定性: {:.1}%\n\
            在线时间: {:.1}%\n\
            \n\
            === 统计信息 ===\n\
            总连接次数: {}\n\
            总断开次数: {}\n\
            重连尝试次数: {}\n\
            成功重连次数: {}\n\
            重连成功率: {:.1}%\n\
            总运行时间: {}小时\n\
            总停机时间: {}分钟\n\
            \n\
            === 最近状态 ===\n\
            最后心跳: {}秒前\n\
            最后数据接收: {}秒前\n\
            连接是否降级: {}\n\
            最近1小时错误次数: {}\n\
            当前重连次数: {}/{}",
            self.status.as_str(),
            self.status.as_english(),
            self.quality.overall_score(),
            self.quality.average_latency_ms,
            self.quality.error_rate * 100.0,
            self.quality.connection_stability,
            uptime_percentage,
            self.total_connections,
            self.total_disconnections,
            self.total_reconnect_attempts,
            self.successful_reconnects,
            if self.total_reconnect_attempts > 0 {
                (self.successful_reconnects as f64 / self.total_reconnect_attempts as f64) * 100.0
            } else {
                100.0
            },
            self.connection_start_time.elapsed().as_secs() / 3600,
            self.total_downtime.as_secs() / 60,
            self.last_heartbeat.elapsed().as_secs(),
            self.last_data_received.elapsed().as_secs(),
            if self.connection_degraded {
                "是"
            } else {
                "否"
            },
            recent_errors.len(),
            self.reconnect_count,
            self.max_reconnect_attempts
        )
    }

    /// 重置统计信息
    fn reset_stats(&mut self) {
        self.total_connections = 0;
        self.total_disconnections = 0;
        self.total_reconnect_attempts = 0;
        self.successful_reconnects = 0;
        self.connection_start_time = Instant::now();
        self.total_downtime = Duration::ZERO;
        self.events.clear();
        self.quality = ConnectionQuality::new();

        info!("连接管理器统计信息已重置");
    }

    /// 强制重连
    async fn force_reconnect(
        &mut self,
        info_client: &InfoClient,
        user_address: ethers::types::Address,
    ) -> Result<(), GridStrategyError> {
        info!("强制重连开始");

        self.status = ConnectionStatus::Reconnecting;
        self.reconnect_count = 0; // 重置重连计数

        self.record_event(ConnectionEvent::new(
            ConnectionEventType::ReconnectAttempt,
            "强制重连".to_string(),
        ));

        match self.reconnect(info_client, user_address).await {
            Ok(()) => {
                self.on_reconnect_success();
                info!("强制重连成功");
                Ok(())
            }
            Err(e) => {
                self.on_connection_lost(&e);
                error!("强制重连失败: {}", e);
                Err(e)
            }
        }
    }
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

// 检测市场状态
fn detect_market_state(
    price_history: &[f64],
    volatility: f64,
    price_change_5min: f64,
    rsi: f64,
) -> (MarketState, f64, f64, f64) {
    let mut liquidity_score = 100.0;
    let mut volume_anomaly = 0.0;

    // 1. 基于波动率判断
    let (volatility_state, mut price_stability) = if volatility > 0.08 {
        // 极端波动 (日波动率 > 8%)
        volume_anomaly = 80.0;
        (MarketState::Extreme, 10.0)
    } else if volatility > 0.05 {
        // 高波动 (日波动率 > 5%)
        volume_anomaly = 60.0;
        (MarketState::HighVolatility, 30.0)
    } else if volatility > 0.03 {
        // 中等波动 (日波动率 > 3%)
        volume_anomaly = 30.0;
        (MarketState::HighVolatility, 60.0)
    } else if volatility < 0.005 {
        // 极低波动，可能是盘整
        (MarketState::Consolidation, 95.0)
    } else {
        // 正常波动
        (MarketState::Normal, 80.0)
    };

    // 2. 基于短期价格变化判断闪崩/闪涨
    let flash_threshold = 0.05; // 5分钟内5%的变化
    if price_change_5min.abs() > flash_threshold {
        price_stability = 5.0;
        volume_anomaly = 95.0;
        liquidity_score = 20.0;
        return (
            MarketState::Flash,
            liquidity_score,
            price_stability,
            volume_anomaly,
        );
    }

    // 3. 基于RSI判断极端状态
    if rsi > 85.0 || rsi < 15.0 {
        // RSI极端值，可能是超买超卖
        price_stability = (price_stability * 0.7_f64).max(20.0_f64);
        volume_anomaly = (volume_anomaly + 20.0_f64).min(100.0_f64);

        if volatility > 0.05 {
            return (
                MarketState::Extreme,
                liquidity_score,
                price_stability,
                volume_anomaly,
            );
        }
    }

    // 4. 流动性评估
    if price_history.len() >= 10 {
        let recent_prices = &price_history[price_history.len() - 10..];
        let price_gaps: Vec<f64> = recent_prices
            .windows(2)
            .map(|w| (w[1] / w[0] - 1.0).abs())
            .collect();

        let avg_gap = price_gaps.iter().sum::<f64>() / price_gaps.len() as f64;
        let max_gap = price_gaps.iter().fold(0.0_f64, |a, &b| a.max(b));

        // 如果价格跳跃过大，可能是流动性不足
        if max_gap > 0.02 || avg_gap > 0.005 {
            liquidity_score = f64::max(100.0 - max_gap * 2000.0, 10.0);
            if liquidity_score < 40.0 {
                return (
                    MarketState::ThinLiquidity,
                    liquidity_score,
                    price_stability,
                    volume_anomaly,
                );
            }
        }
    }

    // 5. 综合判断
    let final_state = match volatility_state {
        MarketState::Extreme => {
            if liquidity_score < 50.0 {
                MarketState::Extreme
            } else {
                MarketState::HighVolatility
            }
        }
        other => other,
    };

    (
        final_state,
        liquidity_score,
        price_stability,
        volume_anomaly,
    )
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
            market_state: MarketState::Normal,
            liquidity_score: 100.0,
            price_stability: 100.0,
            volume_anomaly: 0.0,
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

    // 检测市场状态
    let (market_state, liquidity_score, price_stability, volume_anomaly) =
        detect_market_state(price_history, volatility, price_change_5min, rsi);

    MarketAnalysis {
        volatility,
        trend,
        rsi,
        short_ma,
        long_ma,
        price_change_5min,
        market_state,
        liquidity_score,
        price_stability,
        volume_anomaly,
    }
}

// 计算动态资金分配
/// 智能网格策略选择
fn determine_adaptive_grid_strategy(
    market_analysis: &MarketAnalysis,
    grid_state: &GridState,
    price_history: &[f64],
) -> GridStrategy {
    let trend_strength = calculate_trend_strength(price_history);
    let volatility_level = market_analysis.volatility;
    let rsi = market_analysis.rsi;
    
    // 计算综合市场信号
    let mut bullish_score = 0.0;
    let mut bearish_score = 0.0;
    
    // 1. 趋势信号 (权重: 40%)
    match market_analysis.trend {
        MarketTrend::Upward => bullish_score += 0.4 * trend_strength,
        MarketTrend::Downward => bearish_score += 0.4 * trend_strength,
        MarketTrend::Sideways => {
            // 震荡市场，偏向中性
            bullish_score += 0.2;
            bearish_score += 0.2;
        }
    }
    
    // 2. RSI信号 (权重: 20%)
    if rsi < 30.0 {
        bullish_score += 0.2; // 超卖，偏多
    } else if rsi > 70.0 {
        bearish_score += 0.2; // 超买，偏空
    }
    
    // 3. 价格变化信号 (权重: 20%)
    let price_change = market_analysis.price_change_5min;
    if price_change > 0.02 {
        bullish_score += 0.2;
    } else if price_change < -0.02 {
        bearish_score += 0.2;
    }
    
    // 4. 持仓偏向调整 (权重: 20%)
    let position_bias = if grid_state.total_capital > 0.0 {
        (grid_state.position_quantity * grid_state.position_avg_price) / grid_state.total_capital
    } else {
        0.0
    };
    
    if position_bias > 0.6 {
        bearish_score += 0.2; // 持仓过多，偏向卖出
    } else if position_bias < 0.2 {
        bullish_score += 0.2; // 持仓过少，偏向买入
    }
    
    // 根据得分确定策略
    let score_diff = bullish_score - bearish_score;
    
    info!(
        "📊 网格策略评分 - 看多: {:.2}, 看空: {:.2}, 差值: {:.2}, 趋势强度: {:.2}",
        bullish_score, bearish_score, score_diff, trend_strength
    );
    
    if score_diff > 0.3 {
        if volatility_level > 0.05 {
            GridStrategy::BullishBias
        } else {
            GridStrategy::PureBull
        }
    } else if score_diff < -0.3 {
        if volatility_level > 0.05 {
            GridStrategy::BearishBias
        } else {
            GridStrategy::PureBear
        }
    } else {
        GridStrategy::Neutral
    }
}

/// 计算趋势强度
fn calculate_trend_strength(price_history: &[f64]) -> f64 {
    if price_history.len() < 10 {
        return 0.5; // 默认中等强度
    }
    
    let recent_prices = &price_history[price_history.len()-10..];
    let first_price = recent_prices[0];
    let _last_price = recent_prices[recent_prices.len()-1];
    
    // 计算线性回归斜率作为趋势强度
    let n = recent_prices.len() as f64;
    let sum_x: f64 = (0..recent_prices.len()).map(|i| i as f64).sum();
    let sum_y: f64 = recent_prices.iter().sum();
    let sum_xy: f64 = recent_prices.iter().enumerate()
        .map(|(i, &price)| i as f64 * price).sum();
    let sum_x2: f64 = (0..recent_prices.len()).map(|i| (i as f64).powi(2)).sum();
    
    let slope = (n * sum_xy - sum_x * sum_y) / (n * sum_x2 - sum_x.powi(2));
    let trend_strength = (slope / first_price).abs().min(1.0);
    
    trend_strength
}

/// 增强的动态资金分配
fn calculate_adaptive_fund_allocation(
    grid_state: &GridState,
    current_price: f64,
    grid_config: &crate::config::GridConfig,
    market_analysis: &MarketAnalysis,
    price_history: &[f64],
) -> AdaptiveFundAllocation {
    // 确定网格策略
    let grid_strategy = determine_adaptive_grid_strategy(market_analysis, grid_state, price_history);
    
    // 计算持仓比例
    let position_ratio = if grid_state.total_capital > 0.0 {
        (grid_state.position_quantity * current_price) / grid_state.total_capital
    } else {
        0.0
    };
    
    // 风险调整因子
    let risk_adjustment = match market_analysis.market_state {
        MarketState::Normal => 1.0,
        MarketState::HighVolatility => 0.8,
        MarketState::Extreme => 0.6,
        MarketState::ThinLiquidity => 0.7,
        MarketState::Flash => 0.5,
        MarketState::Consolidation => 1.1,
    };
    
    // 基础资金分配
    let total_grid_funds = grid_state.available_funds * 0.8 * risk_adjustment; // 80%资金用于网格
    let buy_funds = total_grid_funds * grid_strategy.buy_ratio();
    let sell_funds = total_grid_funds * grid_strategy.sell_ratio();
    
    // 计算单网格资金
    let grid_count = grid_config.grid_count as f64;
    let buy_order_funds = buy_funds / (grid_count * grid_strategy.buy_ratio()).max(1.0);
    let sell_order_funds = sell_funds / (grid_count * grid_strategy.sell_ratio()).max(1.0);
    
    // 做空相关计算
    let max_short_exposure = grid_state.total_capital * 0.3; // 最大30%做空敞口
    let short_position_funds = sell_order_funds * grid_strategy.sell_ratio();
    
    // 间距调整
    let volatility_factor = (1.0 + market_analysis.volatility * 2.0).min(2.0);
    let buy_spacing_adjustment = match grid_strategy {
        GridStrategy::PureBull | GridStrategy::BullishBias => 0.8 * volatility_factor,
        GridStrategy::Neutral => 1.0 * volatility_factor,
        _ => 1.2 * volatility_factor,
    };
    
    let sell_spacing_adjustment = match grid_strategy {
        GridStrategy::PureBear | GridStrategy::BearishBias => 0.8 * volatility_factor,
        GridStrategy::Neutral => 1.0 * volatility_factor,
        _ => 1.2 * volatility_factor,
    };
    
    info!(
        "🎯 自适应网格策略: {} - 买单资金: {:.2} ({:.0}%), 卖单资金: {:.2} ({:.0}%), 风险调整: {:.2}",
        grid_strategy.as_str(),
        buy_order_funds,
        grid_strategy.buy_ratio() * 100.0,
        sell_order_funds,
        grid_strategy.sell_ratio() * 100.0,
        risk_adjustment
    );
    
    AdaptiveFundAllocation {
        buy_order_funds,
        sell_order_funds,
        buy_spacing_adjustment,
        sell_spacing_adjustment,
        position_ratio,
        grid_strategy,
        short_position_funds,
        max_short_exposure,
        risk_adjustment_factor: risk_adjustment,
    }
}

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
    // 1. 总资产止损 - 区分持仓亏损和手续费损失
    // 注意：这里只计算可用资金和持仓价值，不包括挂单占用的资金
    // 挂单占用的资金仍然属于总资产，只是暂时锁定
    let liquid_total_value =
        grid_state.available_funds + grid_state.position_quantity * current_price;
    
    // 计算基于流动资产的亏损率（用于信息显示）
    let liquid_loss_rate = if grid_state.total_capital > 0.0 {
        (grid_state.total_capital - liquid_total_value) / grid_state.total_capital
    } else {
        0.0
    };

    // 区分持仓亏损和手续费损失
    let has_significant_position = grid_state.position_quantity.abs() > 0.001; // 持仓大于0.001才算有持仓
    let position_value = grid_state.position_quantity * current_price;
    let unrealized_pnl = if grid_state.position_avg_price > 0.0 && has_significant_position {
        position_value - (grid_state.position_quantity * grid_state.position_avg_price)
    } else {
        0.0
    };
    
    // 估算手续费损失（基于已实现利润的负值部分）
    let estimated_fee_loss = if grid_state.realized_profit < 0.0 {
        grid_state.realized_profit.abs()
    } else {
        (grid_state.total_capital - liquid_total_value - unrealized_pnl.min(0.0)).max(0.0)
    };

    // 只有在有显著持仓且持仓亏损超过阈值时才触发总资产止损
    if has_significant_position && liquid_loss_rate > grid_config.max_drawdown {
        warn!(
            "🚨 触发总资产止损 - 流动资产: {:.2}, 初始资产: {:.2}, 流动资产亏损率: {:.2}%, 持仓价值: {:.2}, 未实现盈亏: {:.2}, 最大回撤限制: {:.1}%",
            liquid_total_value,
            grid_state.total_capital,
            liquid_loss_rate * 100.0,
            position_value,
            unrealized_pnl,
            grid_config.max_drawdown * 100.0
        );

        return StopLossResult {
            action: StopLossAction::FullStop,
            reason: format!("总资产亏损{:.2}%，超过{:.1}%限制", liquid_loss_rate * 100.0, grid_config.max_drawdown * 100.0),
            stop_quantity: grid_state.position_quantity,
        };
    } else if !has_significant_position && liquid_loss_rate > 0.0 {
        // 无持仓时的资金减少主要是手续费和挂单占用，记录但不触发止损
        info!(
            "📊 无持仓状态 - 流动资产: {:.2}, 初始资产: {:.2}, 流动资产减少: {:.2} ({:.2}%), 主要原因: 手续费损失约{:.2} + 挂单占用资金",
            liquid_total_value,
            grid_state.total_capital,
            grid_state.total_capital - liquid_total_value,
            liquid_loss_rate * 100.0,
            estimated_fee_loss
        );
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

// 参数验证结果结构体
#[derive(Debug, Clone)]
struct ValidationResult {
    is_valid: bool,
    warnings: Vec<String>,
    errors: Vec<String>,
    suggestions: Vec<String>,
}

impl ValidationResult {
    fn new() -> Self {
        Self {
            is_valid: true,
            warnings: Vec::new(),
            errors: Vec::new(),
            suggestions: Vec::new(),
        }
    }

    fn add_error(&mut self, error: String) {
        self.is_valid = false;
        self.errors.push(error);
    }

    fn add_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    fn add_suggestion(&mut self, suggestion: String) {
        self.suggestions.push(suggestion);
    }

    fn log_results(&self, context: &str) {
        if !self.errors.is_empty() {
            error!("❌ {}参数验证失败:", context);
            for error in &self.errors {
                error!("   - {}", error);
            }
        }

        if !self.warnings.is_empty() {
            warn!("⚠️ {}参数验证警告:", context);
            for warning in &self.warnings {
                warn!("   - {}", warning);
            }
        }

        if !self.suggestions.is_empty() {
            info!("💡 {}参数优化建议:", context);
            for suggestion in &self.suggestions {
                info!("   - {}", suggestion);
            }
        }

        if self.is_valid && self.warnings.is_empty() && self.suggestions.is_empty() {
            info!("✅ {}参数验证通过", context);
        }
    }
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

    // 进行增强的一致性检查
    let validation_result = validate_grid_config_enhanced(grid_config);
    validation_result.log_results("网格配置");

    if !validation_result.is_valid {
        return Err(GridStrategyError::ConfigError(
            "网格配置验证失败，请检查参数设置".to_string(),
        ));
    }

    info!("✅ 网格配置验证通过");
    Ok(())
}

// 增强的网格配置验证
fn validate_grid_config_enhanced(grid_config: &crate::config::GridConfig) -> ValidationResult {
    let mut result = ValidationResult::new();

    // 1. 网格间距与手续费的关系验证
    let min_profitable_spacing = grid_config.fee_rate * 3.0; // 至少是手续费的3倍才能盈利
    if grid_config.min_grid_spacing < min_profitable_spacing {
        result.add_error(format!(
            "最小网格间距({:.4}%)过小，无法覆盖手续费成本，建议至少设置为{:.4}%",
            grid_config.min_grid_spacing * 100.0,
            min_profitable_spacing * 100.0
        ));
    } else if grid_config.min_grid_spacing < min_profitable_spacing * 1.5 {
        result.add_warning(format!(
            "最小网格间距({:.4}%)较小，利润空间有限，建议设置为{:.4}%以上",
            grid_config.min_grid_spacing * 100.0,
            min_profitable_spacing * 1.5 * 100.0
        ));
    }

    // 2. 网格间距比例验证
    let spacing_ratio = grid_config.max_grid_spacing / grid_config.min_grid_spacing;
    if spacing_ratio > 10.0 {
        result.add_warning(format!(
            "网格间距范围过大(比例: {:.1}:1)，可能导致策略不稳定",
            spacing_ratio
        ));
        result.add_suggestion("建议将最大网格间距控制在最小间距的5倍以内".to_string());
    } else if spacing_ratio < 1.5 {
        result.add_warning(format!(
            "网格间距范围过小(比例: {:.1}:1)，可能限制策略适应性",
            spacing_ratio
        ));
        result.add_suggestion("建议将最大网格间距设置为最小间距的2-5倍".to_string());
    }

    // 3. 资金分配合理性验证
    let max_possible_orders = (grid_config.total_capital / grid_config.trade_amount) as u32;
    if grid_config.grid_count > max_possible_orders {
        result.add_error(format!(
            "网格数量({})超过资金支持的最大订单数({})",
            grid_config.grid_count, max_possible_orders
        ));
    } else if grid_config.grid_count > max_possible_orders / 2 {
        result.add_warning(format!(
            "网格数量({})较多，可能导致资金过度分散",
            grid_config.grid_count
        ));
        result.add_suggestion(format!(
            "建议将网格数量控制在{}以内",
            max_possible_orders / 2
        ));
    }

    // 4. 风险参数一致性验证
    if grid_config.max_single_loss > grid_config.max_drawdown {
        result.add_error(format!(
            "单笔最大亏损({:.1}%)不应超过最大回撤({:.1}%)",
            grid_config.max_single_loss * 100.0,
            grid_config.max_drawdown * 100.0
        ));
    }

    if grid_config.max_daily_loss > grid_config.max_drawdown {
        result.add_warning(format!(
            "每日最大亏损({:.1}%)超过最大回撤({:.1}%)，可能过于激进",
            grid_config.max_daily_loss * 100.0,
            grid_config.max_drawdown * 100.0
        ));
    }

    // 5. 杠杆与风险的匹配验证
    if grid_config.leverage > 5 && grid_config.max_drawdown > 0.2 {
        result.add_warning(format!(
            "高杠杆({})配合高回撤容忍度({:.1}%)风险较大",
            grid_config.leverage,
            grid_config.max_drawdown * 100.0
        ));
        result.add_suggestion("建议在高杠杆时降低最大回撤阈值".to_string());
    }

    // 6. 保证金安全阈值验证
    let recommended_margin_threshold = 1.0 / grid_config.leverage as f64 * 3.0; // 杠杆倒数的3倍
    if grid_config.margin_safety_threshold < recommended_margin_threshold {
        result.add_warning(format!(
            "保证金安全阈值({:.1}%)可能过低，建议设置为{:.1}%以上",
            grid_config.margin_safety_threshold * 100.0,
            recommended_margin_threshold * 100.0
        ));
    }

    // 7. 最小利润与网格间距的关系验证
    let min_profit_rate = grid_config.min_profit / grid_config.trade_amount;
    if min_profit_rate > grid_config.min_grid_spacing * 0.5 {
        result.add_warning(format!(
            "最小利润要求({:.4}%)相对于网格间距({:.4}%)过高，可能影响成交频率",
            min_profit_rate * 100.0,
            grid_config.min_grid_spacing * 100.0
        ));
        result.add_suggestion("建议降低最小利润要求或增加网格间距".to_string());
    }

    // 8. 时间参数合理性验证
    if grid_config.check_interval < 5 {
        result.add_warning(format!(
            "检查间隔({} 秒)过短，可能导致过度频繁的API调用",
            grid_config.check_interval
        ));
        result.add_suggestion("建议将检查间隔设置为10-30秒".to_string());
    } else if grid_config.check_interval > 300 {
        result.add_warning(format!(
            "检查间隔({} 秒)过长，可能错过重要的市场变化",
            grid_config.check_interval
        ));
        result.add_suggestion("建议将检查间隔设置为10-60秒".to_string());
    }

    // 9. 批量订单参数验证
    if grid_config.max_orders_per_batch > 50 {
        result.add_warning(format!(
            "批量订单数量({})较大，可能导致API限制",
            grid_config.max_orders_per_batch
        ));
        result.add_suggestion("建议将批量订单数量控制在20-50之间".to_string());
    }

    if grid_config.order_batch_delay_ms < 100 {
        result.add_warning(format!(
            "批量订单延迟({} ms)过短，可能触发API限制",
            grid_config.order_batch_delay_ms
        ));
        result.add_suggestion("建议将批量订单延迟设置为200ms以上".to_string());
    }

    result
}

// 验证动态参数的合理性和一致性
fn validate_dynamic_parameters(
    dynamic_params: &DynamicGridParams,
    grid_config: &crate::config::GridConfig,
    context: &str,
) -> ValidationResult {
    let mut result = ValidationResult::new();

    // 1. 基本范围验证
    if dynamic_params.current_min_spacing <= 0.0 {
        result.add_error("动态最小网格间距必须大于0".to_string());
    }

    if dynamic_params.current_max_spacing <= dynamic_params.current_min_spacing {
        result.add_error("动态最大网格间距必须大于最小间距".to_string());
    }

    if dynamic_params.current_trade_amount <= 0.0 {
        result.add_error("动态交易金额必须大于0".to_string());
    }

    // 2. 与原始配置的偏离度验证
    let min_spacing_deviation =
        (dynamic_params.current_min_spacing / grid_config.min_grid_spacing - 1.0).abs();
    let max_spacing_deviation =
        (dynamic_params.current_max_spacing / grid_config.max_grid_spacing - 1.0).abs();
    let trade_amount_deviation =
        (dynamic_params.current_trade_amount / grid_config.trade_amount - 1.0).abs();

    if min_spacing_deviation > 0.5 {
        result.add_warning(format!(
            "最小网格间距偏离原始配置{:.1}%，当前: {:.4}%，原始: {:.4}%",
            min_spacing_deviation * 100.0,
            dynamic_params.current_min_spacing * 100.0,
            grid_config.min_grid_spacing * 100.0
        ));
    }

    if max_spacing_deviation > 0.5 {
        result.add_warning(format!(
            "最大网格间距偏离原始配置{:.1}%，当前: {:.4}%，原始: {:.4}%",
            max_spacing_deviation * 100.0,
            dynamic_params.current_max_spacing * 100.0,
            grid_config.max_grid_spacing * 100.0
        ));
    }

    if trade_amount_deviation > 0.3 {
        result.add_warning(format!(
            "交易金额偏离原始配置{:.1}%，当前: {:.2}，原始: {:.2}",
            trade_amount_deviation * 100.0,
            dynamic_params.current_trade_amount,
            grid_config.trade_amount
        ));
    }

    // 3. 安全边界验证
    let min_safe_spacing = grid_config.fee_rate * 2.5;
    let max_safe_spacing = grid_config.max_grid_spacing * 2.0;
    let min_safe_amount = grid_config.trade_amount * 0.1;
    let max_safe_amount = grid_config.total_capital * 0.2;

    if dynamic_params.current_min_spacing < min_safe_spacing {
        result.add_error(format!(
            "动态最小网格间距({:.4}%)低于安全下限({:.4}%)",
            dynamic_params.current_min_spacing * 100.0,
            min_safe_spacing * 100.0
        ));
    }

    if dynamic_params.current_max_spacing > max_safe_spacing {
        result.add_error(format!(
            "动态最大网格间距({:.4}%)超过安全上限({:.4}%)",
            dynamic_params.current_max_spacing * 100.0,
            max_safe_spacing * 100.0
        ));
    }

    if dynamic_params.current_trade_amount < min_safe_amount {
        result.add_error(format!(
            "动态交易金额({:.2})低于安全下限({:.2})",
            dynamic_params.current_trade_amount, min_safe_amount
        ));
    }

    if dynamic_params.current_trade_amount > max_safe_amount {
        result.add_error(format!(
            "动态交易金额({:.2})超过安全上限({:.2})",
            dynamic_params.current_trade_amount, max_safe_amount
        ));
    }

    // 4. 手续费覆盖验证
    let min_profitable_spacing = grid_config.fee_rate * 3.0;
    if dynamic_params.current_min_spacing < min_profitable_spacing {
        result.add_error(format!(
            "动态最小网格间距({:.4}%)无法覆盖手续费成本，需要至少{:.4}%",
            dynamic_params.current_min_spacing * 100.0,
            min_profitable_spacing * 100.0
        ));
    }

    // 5. 网格间距比例验证
    let spacing_ratio = dynamic_params.current_max_spacing / dynamic_params.current_min_spacing;
    if spacing_ratio > 15.0 {
        result.add_warning(format!(
            "动态网格间距比例过大({:.1}:1)，可能导致策略不稳定",
            spacing_ratio
        ));
        result.add_suggestion("建议调整参数使间距比例控制在10:1以内".to_string());
    } else if spacing_ratio < 1.2 {
        result.add_warning(format!(
            "动态网格间距比例过小({:.1}:1)，可能限制策略灵活性",
            spacing_ratio
        ));
        result.add_suggestion("建议调整参数使间距比例至少为1.5:1".to_string());
    }

    // 6. 优化频率验证
    if dynamic_params.optimization_count > 100 {
        result.add_warning(format!(
            "参数优化次数过多({}次)，可能存在过度优化",
            dynamic_params.optimization_count
        ));
        result.add_suggestion("建议检查优化逻辑，避免过度频繁的参数调整".to_string());
    }

    // 7. 检查点数量验证
    if dynamic_params.checkpoints.len() > 20 {
        result.add_warning(format!(
            "检查点数量过多({}个)，建议清理旧的检查点",
            dynamic_params.checkpoints.len()
        ));
    }

    // 8. 性能窗口验证
    if dynamic_params.performance_window.len() > 50 {
        result.add_warning(format!(
            "性能窗口数据过多({}个)，可能影响计算效率",
            dynamic_params.performance_window.len()
        ));
    }

    // 9. 回滚阈值验证
    if dynamic_params.rollback_threshold > 50.0 {
        result.add_warning(format!(
            "回滚阈值过高({:.1})，可能错过回滚时机",
            dynamic_params.rollback_threshold
        ));
        result.add_suggestion("建议将回滚阈值设置在10-30之间".to_string());
    } else if dynamic_params.rollback_threshold < 5.0 {
        result.add_warning(format!(
            "回滚阈值过低({:.1})，可能导致过度频繁的回滚",
            dynamic_params.rollback_threshold
        ));
        result.add_suggestion("建议将回滚阈值设置在10-30之间".to_string());
    }

    // 10. 上下文特定验证
    match context {
        "优化前" => {
            // 优化前的特殊检查
            if dynamic_params.optimization_count == 0 {
                result.add_suggestion("首次优化，建议保守调整参数".to_string());
            }
        }
        "优化后" => {
            // 优化后的特殊检查
            if dynamic_params.checkpoints.is_empty() {
                result.add_warning("优化后未创建检查点，无法回滚".to_string());
            }
        }
        "加载时" => {
            // 加载时的特殊检查
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            if current_time - dynamic_params.last_optimization_time > 7 * 24 * 60 * 60 {
                result.add_suggestion("参数已超过7天未优化，建议检查是否需要更新".to_string());
            }
        }
        _ => {}
    }

    result
}

// 验证参数优化的合理性
fn validate_parameter_optimization(
    old_params: &DynamicGridParams,
    new_params: &DynamicGridParams,
    grid_config: &crate::config::GridConfig,
    performance_score: f64,
) -> ValidationResult {
    let mut result = ValidationResult::new();

    // 1. 计算参数变化幅度
    let min_spacing_change =
        (new_params.current_min_spacing / old_params.current_min_spacing - 1.0).abs();
    let max_spacing_change =
        (new_params.current_max_spacing / old_params.current_max_spacing - 1.0).abs();
    let trade_amount_change =
        (new_params.current_trade_amount / old_params.current_trade_amount - 1.0).abs();

    // 2. 验证变化幅度的合理性
    if min_spacing_change > 0.2 {
        result.add_warning(format!(
            "最小网格间距变化幅度过大({:.1}%)，可能导致策略不稳定",
            min_spacing_change * 100.0
        ));
        result.add_suggestion("建议单次优化的参数调整幅度控制在10%以内".to_string());
    }

    if max_spacing_change > 0.2 {
        result.add_warning(format!(
            "最大网格间距变化幅度过大({:.1}%)，可能导致策略不稳定",
            max_spacing_change * 100.0
        ));
    }

    if trade_amount_change > 0.15 {
        result.add_warning(format!(
            "交易金额变化幅度过大({:.1}%)，可能影响风险控制",
            trade_amount_change * 100.0
        ));
    }

    // 3. 验证优化方向的合理性
    if performance_score < 30.0 {
        // 低性能时应该保守调整
        if min_spacing_change > 0.1 || max_spacing_change > 0.1 || trade_amount_change > 0.1 {
            result.add_warning("性能较差时建议更保守地调整参数".to_string());
        }

        if new_params.current_trade_amount > old_params.current_trade_amount {
            result.add_warning("性能较差时不建议增加交易金额".to_string());
        }
    } else if performance_score > 70.0 {
        // 高性能时可以适度激进
        if min_spacing_change < 0.02 && max_spacing_change < 0.02 && trade_amount_change < 0.02 {
            result.add_suggestion("性能良好时可以适度增加参数调整幅度以获得更好收益".to_string());
        }
    }

    // 4. 验证新参数的一致性
    let new_validation = validate_dynamic_parameters(new_params, grid_config, "优化后");
    if !new_validation.is_valid {
        result.add_error("优化后的参数不满足基本要求".to_string());
        result.errors.extend(new_validation.errors);
    }

    // 5. 验证优化频率
    let time_since_last_optimization =
        new_params.last_optimization_time - old_params.last_optimization_time;
    if time_since_last_optimization < 12 * 60 * 60 {
        result.add_warning(format!(
            "距离上次优化时间过短({:.1}小时)，可能存在过度优化",
            time_since_last_optimization as f64 / 3600.0
        ));
    }

    // 6. 验证检查点创建
    if new_params.checkpoints.len() <= old_params.checkpoints.len() {
        result.add_warning("优化后未创建新的检查点，无法回滚".to_string());
    }

    result
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
        info!(
            "🔄 清仓多头 - 数量: {:.4}, 价格: {:.4} (含滑点: {:.2}%)",
            long_position,
            sell_price,
            grid_config.slippage_tolerance * 100.0
        );
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
        info!(
            "🔄 清仓空头 - 数量: {:.4}, 价格: {:.4} (含滑点: {:.2}%)",
            short_position,
            buy_price,
            grid_config.slippage_tolerance * 100.0
        );
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
    _order_manager: &mut OrderManager,
) -> Result<(), GridStrategyError> {
    info!("🔄 开始创建动态网格...");

    // 分析市场状态
    let market_analysis = analyze_market_trend(price_history);

    info!(
        "📊 市场状态检测 - 状态: {}, 风险等级: {}, 流动性: {:.1}, 稳定性: {:.1}",
        market_analysis.market_state.as_str(),
        market_analysis.market_state.risk_level(),
        market_analysis.liquidity_score,
        market_analysis.price_stability
    );

    // 检查是否应暂停交易
    if market_analysis.market_state.should_pause_trading() {
        warn!(
            "🚨 市场状态异常，暂停网格交易: {} ({})",
            market_analysis.market_state.as_str(),
            market_analysis.market_state.as_english()
        );
        return Ok(());
    }

    // 获取自适应资金分配
    let mut fund_allocation = calculate_adaptive_fund_allocation(
        grid_state, 
        current_price, 
        grid_config, 
        &market_analysis, 
        price_history
    );

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

    // 基于市场状态调整网格策略
    let grid_reduction = market_analysis.market_state.grid_reduction_factor();
    let adjusted_grid_count = (grid_config.grid_count as f64 * grid_reduction) as u32;

    if market_analysis
        .market_state
        .requires_conservative_strategy()
    {
        // 保守策略：减少资金使用，增加间距
        fund_allocation.buy_spacing_adjustment *= 1.2;
        fund_allocation.sell_spacing_adjustment *= 1.2;
        fund_allocation.buy_order_funds *= 0.8;
        fund_allocation.sell_order_funds *= 0.8;

        warn!(
            "⚠️ 启用保守策略 - 网格缩减: {:.0}%, 间距增加: 20%",
            (1.0 - grid_reduction) * 100.0
        );
    }

    info!(
        "💰 资金分配 - 买单资金: {:.2}, 卖单资金: {:.2}, 持仓比例: {:.2}%, 振幅调整: {:.2}",
        fund_allocation.buy_order_funds,
        fund_allocation.sell_order_funds,
        fund_allocation.position_ratio * 100.0,
        amplitude_adjustment
    );

    // 添加详细的调试信息
    info!(
        "🔍 网格创建调试信息 - 当前价格: {:.4}, 总资金: {:.2}, 可用资金: {:.2}, 网格数量: {}",
        current_price, grid_state.total_capital, grid_state.available_funds, grid_config.grid_count
    );

    info!(
        "🔍 动态参数 - 最小间距: {:.6}, 最大间距: {:.6}, 交易金额: {:.2}",
        grid_state.dynamic_params.current_min_spacing,
        grid_state.dynamic_params.current_max_spacing,
        grid_state.dynamic_params.current_trade_amount
    );

    info!(
        "🔍 配置参数 - 最小利润: {:.4}, 手续费率: {:.6}, 价格精度: {}, 数量精度: {}",
        grid_config.min_profit,
        grid_config.fee_rate,
        grid_config.price_precision,
        grid_config.quantity_precision
    );

    // 创建买单 - 价格递减
    let mut current_buy_price = current_price;
    let max_buy_funds = grid_state.available_funds * 0.7; // 最多使用70%资金做买单
    let mut allocated_buy_funds = 0.0;
    let mut buy_count = 0;

    // 收集要批量创建的买单
    let mut pending_buy_orders: Vec<ClientOrderRequest> = Vec::new();
    let mut pending_buy_order_info: Vec<OrderInfo> = Vec::new();

    info!(
        "🔄 开始买单循环 - 初始买入价: {:.4}, 价格下限: {:.4}, 最大资金: {:.2}, 最大网格数: {}",
        current_buy_price,
        current_price * 0.8,
        max_buy_funds,
        adjusted_grid_count
    );

    while current_buy_price > current_price * 0.8
        && allocated_buy_funds < max_buy_funds
        && buy_count < adjusted_grid_count
    {
        // 动态计算网格间距，使用优化后的参数和振幅调整
        let dynamic_spacing = grid_state.dynamic_params.current_min_spacing
            * fund_allocation.buy_spacing_adjustment
            * amplitude_adjustment;
        current_buy_price = current_buy_price - (current_buy_price * dynamic_spacing);

        // 计算当前网格资金，使用动态交易金额
        let dynamic_trade_amount = grid_state.dynamic_params.current_trade_amount;
        let mut current_grid_funds = (fund_allocation.buy_order_funds * dynamic_trade_amount
            / grid_config.trade_amount)
            * (1.0 - (current_price - current_buy_price) / current_price * 3.0);
        current_grid_funds = current_grid_funds.max(fund_allocation.buy_order_funds * 0.5);

        // 检查资金限制
        if allocated_buy_funds + current_grid_funds > max_buy_funds {
            current_grid_funds = max_buy_funds - allocated_buy_funds;
        }

        if current_grid_funds < fund_allocation.buy_order_funds * 0.1 {
            info!(
                "🚫 买单资金不足 - 当前网格资金: {:.2}, 最小要求: {:.2}, 停止创建买单",
                current_grid_funds,
                fund_allocation.buy_order_funds * 0.1
            );
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

        info!(
            "🔍 买单利润检查 - 买入价: {:.4}, 潜在卖出价: {:.4}, 预期利润率: {:.4}%, 最小要求: {:.4}%",
            current_buy_price,
            potential_sell_price,
            expected_profit_rate * 100.0,
            (grid_config.min_profit / current_buy_price) * 100.0
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

            info!(
                "✅ 买单准备就绪 - 价格: {:.4}, 数量: {:.4}, 资金: {:.2}, 累计资金: {:.2}",
                formatted_price, buy_quantity, current_grid_funds, allocated_buy_funds
            );
        } else {
            info!(
                "❌ 买单利润不足 - 预期利润率: {:.4}% < 最小要求: {:.4}%, 跳过此价格",
                expected_profit_rate * 100.0,
                (grid_config.min_profit / current_buy_price) * 100.0
            );
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
        let mut temp_batch_optimizer = BatchTaskOptimizer::new(
            grid_config.max_orders_per_batch.max(5),
            Duration::from_secs(3),
        );
        let creation_result = tokio::time::timeout(
            creation_timeout,
            create_orders_in_batches(
                exchange_client,
                pending_buy_orders,
                grid_config,
                grid_state,
                &mut temp_batch_optimizer,
            ),
        )
        .await;

        match creation_result {
            Ok(Ok((created_order_ids, failed_order_infos))) => {
                // 批量创建成功
                let success_count = created_order_ids.len();
                let success_rate = success_count as f64 / order_count as f64 * 100.0;

                // 将创建成功的订单添加到管理列表
                for (i, order_id) in created_order_ids.iter().enumerate() {
                    if i < pending_buy_order_info.len() {
                        active_orders.push(*order_id);
                        buy_orders.insert(*order_id, pending_buy_order_info[i].clone());

                        info!(
                            "🟢 买单创建成功: ID={}, 价格={:.4}, 数量={:.4}, 资金={:.2}",
                            order_id,
                            pending_buy_order_info[i].price,
                            pending_buy_order_info[i].quantity,
                            pending_buy_order_info[i].allocated_funds
                        );
                    }
                }

                info!(
                    "✅ 批量买单创建完成: {}/{} (成功率: {:.1}%)",
                    success_count, order_count, success_rate
                );

                // 处理失败的订单进行重试
                if !failed_order_infos.is_empty() && failed_order_infos.len() <= 20 {
                    info!("🔄 开始重试{}个失败的买单", failed_order_infos.len());

                    let retry_result =
                        retry_failed_order_infos(exchange_client, failed_order_infos, grid_config)
                            .await;

                    match retry_result {
                        Ok(retry_successful_ids) => {
                            // 将重试成功的订单也添加到管理列表
                            for order_id in retry_successful_ids {
                                active_orders.push(order_id);
                                // 注意：这里我们使用默认的OrderInfo，因为重试时没有详细信息
                                buy_orders.insert(
                                    order_id,
                                    OrderInfo {
                                        price: 0.0, // 这些值需要从重试的订单信息中获取
                                        quantity: 0.0,
                                        cost_price: None,
                                        potential_sell_price: None,
                                        allocated_funds: 0.0,
                                    },
                                );
                                info!("🔄✅ 重试买单成功: ID={}", order_id);
                            }
                        }
                        Err(e) => {
                            warn!("🔄❌ 重试买单失败: {:?}", e);
                        }
                    }
                }

                // 根据成功率调整后续策略
                if success_rate < 70.0 {
                    warn!(
                        "⚠️ 买单创建成功率较低({:.1}%)，调整资金分配策略",
                        success_rate
                    );
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
                    )
                    .await;

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
                    )
                    .await;

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

    // 创建卖单 - 支持做空交易
    let mut current_sell_price = current_price;
    
    // 自适应卖单数量计算
    let max_sell_quantity = match fund_allocation.grid_strategy {
        GridStrategy::PureBear | GridStrategy::BearishBias => {
            // 做空策略：允许超过持仓的卖单（做空）
            let existing_position = grid_state.position_quantity * 0.8;
            let short_allowance = fund_allocation.max_short_exposure / current_price;
            existing_position + short_allowance
        },
        _ => {
            // 其他策略：基于持仓和资金的卖单
            let existing_position = grid_state.position_quantity * 0.8;
            let cash_based_quantity = fund_allocation.sell_order_funds * grid_config.grid_count as f64 / current_price;
            existing_position.max(cash_based_quantity)
        }
    };
    
    let mut allocated_sell_quantity = 0.0;
    let mut sell_count = 0;

    // 收集要批量创建的卖单
    let mut pending_sell_orders: Vec<ClientOrderRequest> = Vec::new();
    let mut pending_sell_order_info: Vec<OrderInfo> = Vec::new();

    while current_sell_price < current_price * 1.2
        && allocated_sell_quantity < max_sell_quantity
        && sell_count < adjusted_grid_count
    {
        // 动态计算网格间距，使用优化后的参数和振幅调整
        let dynamic_spacing = grid_state.dynamic_params.current_min_spacing
            * fund_allocation.sell_spacing_adjustment
            * amplitude_adjustment;
        current_sell_price = current_sell_price + (current_sell_price * dynamic_spacing);

        // 自适应卖单数量计算
        let price_coefficient = (current_sell_price - current_price) / current_price;
        let mut current_grid_quantity = match fund_allocation.grid_strategy {
            GridStrategy::PureBear | GridStrategy::BearishBias => {
                // 做空策略：基于资金计算数量
                fund_allocation.sell_order_funds / current_sell_price * (1.0 + price_coefficient * 0.5)
            },
            _ => {
                // 其他策略：基于持仓和资金
                let position_based = if grid_state.position_quantity > 0.0 {
                    grid_state.position_quantity / grid_config.grid_count as f64 * 0.8
                } else {
                    0.0
                };
                let fund_based = fund_allocation.sell_order_funds / current_sell_price;
                position_based.max(fund_based)
            }
        };

        // 确保不超过可售数量
        if allocated_sell_quantity + current_grid_quantity > max_sell_quantity {
            current_grid_quantity = max_sell_quantity - allocated_sell_quantity;
        }

        // 最小数量检查
        let min_order_value = fund_allocation.sell_order_funds * 0.05; // 最小5%
        if current_grid_quantity * current_sell_price < min_order_value {
            info!(
                "🚫 卖单价值过小 - 当前价值: {:.2}, 最小要求: {:.2}, 停止创建卖单",
                current_grid_quantity * current_sell_price,
                min_order_value
            );
            break;
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

        let mut temp_batch_optimizer = BatchTaskOptimizer::new(
            grid_config.max_orders_per_batch.max(5),
            Duration::from_secs(3),
        );
        match create_orders_in_batches(
            exchange_client,
            pending_sell_orders,
            grid_config,
            grid_state,
            &mut temp_batch_optimizer,
        )
        .await
        {
            Ok((created_order_ids, _failed_sell_order_infos)) => {
                // 将创建成功的订单添加到管理列表
                for (i, order_id) in created_order_ids.iter().enumerate() {
                    if i < pending_sell_order_info.len() {
                        active_orders.push(*order_id);
                        sell_orders.insert(*order_id, pending_sell_order_info[i].clone());

                        info!(
                            "🔴 卖单创建成功: ID={}, 价格={:.4}, 数量={:.4}",
                            order_id,
                            pending_sell_order_info[i].price,
                            pending_sell_order_info[i].quantity
                        );
                    }
                }
                info!(
                    "✅ 批量卖单创建完成: {}/{}",
                    created_order_ids.len(),
                    sell_order_count
                );
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

    info!("✅ 自适应网格创建完成 - 策略: {}, 买单数量: {}, 卖单数量: {}, 已分配买单资金: {:.2}, 已分配卖单数量: {:.4}, 最大做空敞口: {:.2}", 
        fund_allocation.grid_strategy.as_str(), buy_count, sell_count, allocated_buy_funds, allocated_sell_quantity, fund_allocation.max_short_exposure);

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
        cancel_all_orders(exchange_client, active_orders, &grid_config.trading_asset).await?;
        buy_orders.clear();
        sell_orders.clear();
    } else if stop_result.action.is_partial_stop() && stop_result.stop_quantity > 0.0 {
        grid_state.stop_loss_status = StopLossStatus::Monitoring;

        // 部分清仓 - 智能滑点处理
        let base_price = if grid_state.position_avg_price > 0.0 {
            grid_state.position_avg_price
        } else {
            current_price
        };

        // 智能滑点计算：根据市场波动率和紧急程度调整
        let market_volatility = grid_state.historical_volatility.max(0.001); // 最小波动率0.1%
        let urgency_multiplier = match stop_result.action {
            StopLossAction::FullStop => 2.0,    // 全部止损时使用更大滑点
            StopLossAction::PartialStop => 1.5, // 部分止损时使用中等滑点
            _ => 1.0,
        };

        // 动态滑点 = 基础滑点 + 市场波动率调整 + 紧急程度调整
        let dynamic_slippage = grid_config.slippage_tolerance
            + (market_volatility * 0.5)
            + (grid_config.slippage_tolerance * (urgency_multiplier - 1.0));
        let final_slippage = dynamic_slippage.min(0.05); // 最大滑点5%

        let sell_price_with_slippage = base_price * (1.0 - final_slippage);

        info!("🎯 智能滑点计算 - 基础价格: {:.4}, 基础滑点: {:.2}%, 市场波动率: {:.2}%, 紧急系数: {:.1}, 最终滑点: {:.2}%, 目标价格: {:.4}",
            base_price,
            grid_config.slippage_tolerance * 100.0,
            market_volatility * 100.0,
            urgency_multiplier,
            final_slippage * 100.0,
            sell_price_with_slippage
        );

        let market_sell_order = ClientOrderRequest {
            asset: grid_config.trading_asset.clone(),
            is_buy: false,
            reduce_only: true,
            limit_px: sell_price_with_slippage,
            sz: stop_result.stop_quantity,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Ioc".to_string(), // IOC确保快速成交或取消
            }),
        };

        info!(
            "🔄 执行部分止损 - 价格: {:.4} (含滑点: {:.2}%)",
            sell_price_with_slippage,
            grid_config.slippage_tolerance * 100.0
        );

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
// 智能订单更新函数
async fn smart_update_orders(
    exchange_client: &ExchangeClient,
    grid_config: &crate::config::GridConfig,
    grid_state: &mut GridState,
    current_price: f64,
    price_history: &[f64],
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
    _batch_optimizer: &mut BatchTaskOptimizer,
) -> Result<bool, GridStrategyError> {
    let now = SystemTime::now();
    
    // 分析市场状况
    let market_analysis = analyze_market_trend(price_history);
    
    // 计算订单成功率
    let total_orders = buy_orders.len() + sell_orders.len();
    let current_success_rate = if total_orders > 0 {
        // 简化的成功率计算，实际应该基于历史成交数据
        0.8 // 默认80%成功率，可以根据实际情况调整
    } else {
        0.8
    };
    
    // 使用自适应配置计算动态订单存活时间
    let adaptive_max_age = {
        let mut adaptive_config = grid_state.adaptive_order_config.clone();
        let result = adaptive_config.calculate_adaptive_max_age(
            &market_analysis,
            grid_state,
            current_success_rate,
        );
        grid_state.adaptive_order_config = adaptive_config;
        result
    };
    
    // 更新 max_order_age_minutes 为自适应值
    grid_state.max_order_age_minutes = adaptive_max_age;
    
    // 检查是否需要更新订单
    let price_change_ratio = (current_price - grid_state.last_grid_price).abs() / grid_state.last_grid_price;
    let time_since_last_update = now.duration_since(grid_state.last_price_update)
        .unwrap_or(Duration::from_secs(0));
    
    // 检查订单年龄
    let order_age_minutes = time_since_last_update.as_secs() as f64 / 60.0;
    let orders_too_old = order_age_minutes >= adaptive_max_age;
    
    // 检查买单是否远离当前价格
    let mut orders_too_far = false;
    if !buy_orders.is_empty() {
        let highest_buy_price = buy_orders.values()
            .map(|order| order.price)
            .fold(0.0, f64::max);
        
        // 如果最高买单价格低于当前价格的95%，认为订单太远
        if highest_buy_price < current_price * 0.95 {
            orders_too_far = true;
            info!(
                "🔄 买单价格过远 - 最高买单: {:.4}, 当前价格: {:.4}, 差距: {:.2}%",
                highest_buy_price,
                current_price,
                (current_price - highest_buy_price) / current_price * 100.0
            );
        }
    }
    
    let should_update = price_change_ratio >= grid_state.order_update_threshold 
        || orders_too_old 
        || orders_too_far;
    
    if should_update {
        info!(
            "🔄 触发智能订单更新 - 价格变化: {:.2}%, 订单年龄: {:.1}分钟, 订单过远: {}, 阈值: {:.2}%, 自适应存活时间: {:.1}分钟",
            price_change_ratio * 100.0,
            order_age_minutes,
            orders_too_far,
            grid_state.order_update_threshold * 100.0,
            adaptive_max_age
        );
        
        // 取消现有订单
        if !active_orders.is_empty() {
            info!("🗑️ 取消 {} 个现有订单...", active_orders.len());
            cancel_all_orders(exchange_client, active_orders, &grid_config.trading_asset).await?;
            buy_orders.clear();
            sell_orders.clear();
            
            // 等待订单取消完成
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        
        // 重新创建网格
        let mut temp_order_manager = OrderManager::new(100);
        create_dynamic_grid(
            exchange_client,
            grid_config,
            grid_state,
            current_price,
            price_history,
            active_orders,
            buy_orders,
            sell_orders,
            &mut temp_order_manager,
        ).await?;
        
                                // 更新状态
                        grid_state.last_price_update = now;
                        grid_state.last_grid_price = current_price;
                        grid_state.last_order_batch_time = now;
        
        info!("✅ 智能订单更新完成");
        return Ok(true);
    }
    
    Ok(false)
}

// 检查并清理过期订单
async fn cleanup_expired_orders(
    exchange_client: &ExchangeClient,
    grid_config: &crate::config::GridConfig,
    grid_state: &GridState,
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
) -> Result<(), GridStrategyError> {
    let now = SystemTime::now();
    let max_age = Duration::from_secs((grid_state.max_order_age_minutes * 60.0) as u64);
    
    let time_since_creation = now.duration_since(grid_state.last_order_batch_time)
        .unwrap_or(Duration::from_secs(0));
    
    if time_since_creation >= max_age {
        let expired_count = active_orders.len();
        if expired_count > 0 {
            info!("⏰ 发现 {} 个过期订单，开始清理...", expired_count);
            
            // 取消过期订单
            for &order_id in active_orders.iter() {
                match cancel_order_with_asset(exchange_client, order_id, &grid_config.trading_asset).await {
                    Ok(_) => info!("✅ 过期订单 {} 已取消", order_id),
                    Err(e) => warn!("❌ 取消过期订单 {} 失败: {:?}", order_id, e),
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            
            // 清理本地记录
            active_orders.clear();
            buy_orders.clear();
            sell_orders.clear();
            
            info!("🧹 过期订单清理完成");
        }
    }
    
    Ok(())
}

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
        info!(
            "📈 检测到{}趋势({}), 调整买单密度",
            market_analysis.trend.as_str(),
            market_analysis.trend.as_english()
        );
    } else if market_analysis.trend.is_bearish() {
        // 下降趋势：减少买单密度，增加卖单密度
        adjusted_fund_allocation.buy_spacing_adjustment *= 1.2;
        adjusted_fund_allocation.sell_spacing_adjustment *= 0.8 * risk_adjustment;
        info!(
            "📉 检测到{}趋势({}), 调整卖单密度",
            market_analysis.trend.as_str(),
            market_analysis.trend.as_english()
        );
    } else if market_analysis.trend.is_sideways() {
        // 震荡趋势：保持均衡的网格密度，应用风险调整
        adjusted_fund_allocation.buy_spacing_adjustment *= risk_adjustment;
        adjusted_fund_allocation.sell_spacing_adjustment *= risk_adjustment;
        info!(
            "📊 检测到{}趋势({}), 保持均衡网格",
            market_analysis.trend.as_str(),
            market_analysis.trend.as_english()
        );
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
    cancel_all_orders(exchange_client, active_orders, &grid_config.trading_asset).await?;
    buy_orders.clear();
    sell_orders.clear();

    // 等待订单取消完成
    sleep(Duration::from_secs(2)).await;

    // 更新网格状态
    // 这里可以根据市场分析调整网格参数

    // 重新创建网格
    // 注意：这里需要传递订单管理器，但rebalance_grid函数没有接收它
    // 暂时使用一个临时的订单管理器
    let mut temp_order_manager = OrderManager::new(100);
    create_dynamic_grid(
        exchange_client,
        grid_config,
        grid_state,
        current_price,
        price_history,
        active_orders,
        buy_orders,
        sell_orders,
        &mut temp_order_manager,
    )
    .await?;

    grid_state.last_rebalance_time = SystemTime::now();

    info!("✅ 网格重平衡完成");
    Ok(())
}

// 取消所有订单 - 改进版本，接受交易资产参数
async fn cancel_all_orders(
    exchange_client: &ExchangeClient,
    active_orders: &mut Vec<u64>,
    trading_asset: &str,
) -> Result<(), GridStrategyError> {
    if active_orders.is_empty() {
        info!("📝 无活跃订单需要取消");
        return Ok(());
    }

    info!("🗑️ 开始取消 {} 个活跃订单...", active_orders.len());

    let mut canceled_count = 0;
    let mut failed_count = 0;

    // 批量取消订单，每批最多10个，使用顺序处理避免生命周期问题
    for chunk in active_orders.chunks(10) {
        for &oid in chunk {
            match cancel_order_with_asset(exchange_client, oid, trading_asset).await {
                Ok(_) => {
                    canceled_count += 1;
                    info!("✅ 订单 {} 已成功取消", oid);
                }
                Err(e) => {
                    failed_count += 1;
                    warn!("❌ 取消订单 {} 失败: {:?}", oid, e);
                }
            }

            // 每个订单间稍微延迟，避免请求过于频繁
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // 批次间延迟
        if chunk.len() == 10 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    info!(
        "📊 订单取消统计: 成功 {}, 失败 {}, 总计 {}",
        canceled_count,
        failed_count,
        active_orders.len()
    );

    // 清空订单列表
    active_orders.clear();

    if failed_count > 0 {
        warn!("⚠️ 有 {} 个订单取消失败，可能需要手动处理", failed_count);
    }

    Ok(())
}

// 取消单个订单 - 带资产参数的版本
async fn cancel_order_with_asset(
    exchange_client: &ExchangeClient,
    oid: u64,
    trading_asset: &str,
) -> Result<(), GridStrategyError> {
    let cancel_request = ClientCancelRequest {
        asset: trading_asset.to_string(),
        oid,
    };

    match exchange_client.cancel(cancel_request, None).await {
        Ok(_) => {
            info!("✅ 订单 {} ({}) 已取消", oid, trading_asset);
            Ok(())
        }
        Err(e) => {
            warn!("❌ 取消订单 {} ({}) 失败: {:?}", oid, trading_asset, e);
            Err(GridStrategyError::OrderError(format!(
                "取消订单失败: {:?}",
                e
            )))
        }
    }
}

// 保持向后兼容的旧版本函数
async fn cancel_order(exchange_client: &ExchangeClient, oid: u64) -> Result<(), GridStrategyError> {
    // 使用默认资产名称的后备方案
    cancel_order_with_asset(exchange_client, oid, "BTC").await
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
        自适应订单存活时间: {:.1}分钟\n\
        订单成功率: {:.1}%\n\
        平均成交时间: {:.1}分钟\n\
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
        grid_state.connection_retry_count,
        grid_state.max_order_age_minutes,
        grid_state.adaptive_order_config.order_success_rate * 100.0,
        grid_state.adaptive_order_config.average_fill_time_minutes
    )
}

pub async fn run_grid_strategy(
    app_config: crate::config::AppConfig,
) -> Result<(), GridStrategyError> {
    env_logger::init();
    let grid_config = &app_config.grid;

    // 设置信号处理
    let (shutdown_flag, cancellation_token) = setup_signal_handler();
    let start_time = SystemTime::now();

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

    // ===== 状态恢复与初始化 =====

    // 1. 创建状态备份
    if let Err(e) = backup_state_files() {
        warn!("⚠️ 创建状态备份失败: {:?}", e);
    }

    // 2. 清理过期备份文件（保留7天）
    if let Err(e) = cleanup_old_backups(7) {
        warn!("⚠️ 清理过期备份失败: {:?}", e);
    }

    // 3. 尝试加载网格状态
    let mut grid_state = match load_grid_state("grid_state.json")? {
        Some(loaded_state) => {
            info!("🔄 检测到已保存的网格状态，正在恢复...");

            // 验证加载的状态是否与当前配置兼容
            if let Err(e) = validate_loaded_state(&loaded_state, grid_config) {
                warn!("⚠️ 状态验证失败: {:?}", e);
                warn!("将使用默认状态重新开始");
                GridState {
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
                    dynamic_params: DynamicGridParams::load_from_file(
                        "dynamic_grid_params.json",
                        grid_config,
                    ),
                                    // 智能订单更新相关字段
                last_price_update: SystemTime::now(),
                last_grid_price: 0.0,
                order_update_threshold: grid_config.order_update_threshold, // 从配置文件读取价格变化触发更新阈值
                // 修改为存活10s
                max_order_age_minutes: 0.1,     // 订单最大存活10s  TODO(需要修改进配置文件)
                    // 自适应订单管理
                    adaptive_order_config: AdaptiveOrderConfig::new(),
                }
            } else {
                info!("✅ 网格状态验证通过，继续使用已保存状态");
                info!("📊 恢复状态摘要:");
                info!("   - 总资金: {:.2}", loaded_state.total_capital);
                info!("   - 可用资金: {:.2}", loaded_state.available_funds);
                info!("   - 持仓数量: {:.4}", loaded_state.position_quantity);
                info!("   - 持仓均价: {:.4}", loaded_state.position_avg_price);
                info!("   - 已实现利润: {:.2}", loaded_state.realized_profit);
                info!(
                    "   - 历史交易数: {}",
                    loaded_state.performance_history.len()
                );
                info!("   - 止损状态: {}", loaded_state.stop_loss_status.as_str());

                // 更新一些时间相关的字段
                let mut state = loaded_state;
                state.last_margin_check = SystemTime::now();
                state.last_order_batch_time = SystemTime::now();
                state.connection_retry_count = 0; // 重置连接重试计数
                state
            }
        }
        None => {
            info!("📄 未找到已保存的网格状态，使用默认配置初始化");
            GridState {
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
                dynamic_params: DynamicGridParams::load_from_file(
                    "dynamic_grid_params.json",
                    grid_config,
                ),
                // 智能订单更新相关字段
                last_price_update: SystemTime::now(),
                last_grid_price: 0.0,
                order_update_threshold: grid_config.order_update_threshold, // 从配置文件读取价格变化触发更新阈值
                max_order_age_minutes: 0.1,     // 订单最大存活10s TODO(需要修改进配置文件)
                // 自适应订单管理
                adaptive_order_config: AdaptiveOrderConfig::new(),
            }
        }
    };

    // 4. 尝试加载订单状态
    let (mut active_orders, mut buy_orders, mut sell_orders) =
        match load_orders_state("orders_state.json")? {
            Some((orders, buys, sells)) => {
                info!("🔄 检测到已保存的订单状态，正在恢复...");
                info!("📊 恢复订单摘要:");
                info!("   - 活跃订单: {}", orders.len());
                info!("   - 买单: {}", buys.len());
                info!("   - 卖单: {}", sells.len());

                // 注意：这里恢复的订单可能已经不存在或状态已改变
                // 在后续的订单状态检查中会自动同步
                (orders, buys, sells)
            }
            None => {
                info!("📄 未找到已保存的订单状态，使用空状态初始化");
                (Vec::new(), HashMap::new(), HashMap::new())
            }
        };

    // ===== 初始化风险控制模块 =====

    // 创建风险控制标志
    let stop_trading_flag = Arc::new(AtomicBool::new(false));

    info!("🛡️ 风险控制模块已初始化");
    info!("   - 检查间隔: 30秒");
    info!(
        "   - 保证金安全阈值: {:.1}%",
        grid_config.margin_safety_threshold * 100.0
    );
    info!(
        "   - 最大回撤限制: {:.1}%",
        grid_config.max_drawdown * 100.0
    );
    info!(
        "   - 每日最大亏损: {:.1}%",
        grid_config.max_daily_loss * 100.0
    );

    // 风险控制状态
    let mut last_risk_check = SystemTime::now();
    let mut risk_events: Vec<RiskEvent> = Vec::new();
    let mut daily_start_capital = grid_state.total_capital;
    let mut daily_start_time = SystemTime::now();
    let mut consecutive_failures = 0u32;
    let mut last_margin_ratio = 100.0f64;

    // ===== 初始化订单优先级管理器 =====

    let mut order_manager = OrderManager::new((grid_config.grid_count * 2) as usize); // 最大订单数为网格数的2倍

    info!("📋 订单优先级管理器已初始化");
    info!("   - 最大订单数: {}", order_manager.max_orders);
    info!(
        "   - 清理间隔: {}分钟",
        order_manager.cleanup_interval.as_secs() / 60
    );
    info!(
        "   - 优先级分布: {:?}",
        order_manager.get_priority_distribution()
    );

    // ===== 初始化批处理优化器 =====

    let mut batch_optimizer = BatchTaskOptimizer::new(
        grid_config.max_orders_per_batch.max(5), // 初始批次大小，最少5个
        Duration::from_secs(3),                  // 目标执行时间3秒
    );

    // 根据配置设置批次大小范围
    batch_optimizer.set_batch_size_range(1, grid_config.max_orders_per_batch.max(100));

    info!("⚡ 批处理优化器已初始化");
    info!("   - 初始批次大小: {}", batch_optimizer.optimal_batch_size);
    info!(
        "   - 目标执行时间: {:.2}秒",
        batch_optimizer.target_execution_time.as_secs_f64()
    );
    info!(
        "   - 批次大小范围: {} - {}",
        batch_optimizer.min_batch_size, batch_optimizer.max_batch_size
    );

    // ===== 初始化连接管理器 =====

    let mut connection_manager = ConnectionManager::new();

    info!("🔗 连接管理器已初始化");
    info!(
        "   - 心跳间隔: {}秒",
        connection_manager.heartbeat_interval.as_secs()
    );
    info!(
        "   - 心跳超时: {}秒",
        connection_manager.heartbeat_timeout.as_secs()
    );
    info!(
        "   - 最大重连次数: {}",
        connection_manager.max_reconnect_attempts
    );
    info!(
        "   - 重连基础延迟: {}秒",
        connection_manager.reconnect_base_delay.as_secs()
    );
    info!(
        "   - 最大重连延迟: {}秒",
        connection_manager.max_reconnect_delay.as_secs()
    );
    info!(
        "   - 自适应心跳: {}",
        if connection_manager.adaptive_heartbeat {
            "启用"
        } else {
            "禁用"
        }
    );

    // 初始连接检查
    match connection_manager
        .check_connection(&info_client, user_address)
        .await
    {
        Ok(true) => {
            info!("✅ 初始连接检查成功");
        }
        Ok(false) => {
            warn!("⚠️ 初始连接检查失败，但系统将继续运行");
        }
        Err(e) => {
            warn!("⚠️ 初始连接检查出错: {}, 系统将继续运行", e);
        }
    }

    let mut last_connection_check = Instant::now();
    let mut last_connection_report = Instant::now();

    let mut last_price: Option<f64> = None;

    let mut last_daily_reset = SystemTime::now();
    let mut last_status_report = SystemTime::now();
    let mut last_state_save = SystemTime::now(); // 添加状态保存时间跟踪

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

        // 检查是否收到退出信号
        if shutdown_flag.load(Ordering::SeqCst) {
            info!("🔔 检测到退出信号，开始安全退出流程...");

            // 获取当前价格用于清仓
            let current_price = last_price.unwrap_or(0.0);

            if let Err(e) = safe_shutdown(
                &exchange_client,
                grid_config,
                &mut grid_state,
                &mut active_orders,
                &mut buy_orders,
                &mut sell_orders,
                current_price,
                ShutdownReason::UserSignal,
                start_time,
            )
            .await
            {
                error!("❌ 安全退出过程中发生错误: {:?}", e);
            }

            break;
        }

        // 检查是否需要重置每日统计
        if should_execute_periodic_task(last_daily_reset, 24 * 60 * 60, "每日统计重置") {
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

                    // 0. 定期状态保存（每5分钟保存一次）
                    if let Err(e) = periodic_state_save(
                        &grid_state,
                        &active_orders,
                        &buy_orders,
                        &sell_orders,
                        &mut last_state_save,
                        300, // 5分钟 = 300秒
                    ) {
                        warn!("⚠️ 定期状态保存失败: {:?}", e);
                    }

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
                            error!("🛑 策略已全部止损，开始安全退出");

                            if let Err(e) = safe_shutdown(
                                &exchange_client,
                                grid_config,
                                &mut grid_state,
                                &mut active_orders,
                                &mut buy_orders,
                                &mut sell_orders,
                                current_price,
                                ShutdownReason::StopLossTriggered,
                                start_time,
                            )
                            .await
                            {
                                error!("❌ 安全退出过程中发生错误: {:?}", e);
                            }

                            break;
                        }
                    }

                    // 检查止损状态是否允许继续交易
                    if !grid_state.stop_loss_status.can_continue_trading() {
                        warn!(
                            "⚠️ 止损状态({})不允许继续交易",
                            grid_state.stop_loss_status.as_str()
                        );
                        if grid_state.stop_loss_status.is_failed() {
                            error!("❌ 止损执行失败，策略退出");
                            break;
                        }
                    }

                    // 1.5. 风险控制检查
                    let risk_check_interval = Duration::from_secs(30); // 30秒检查一次
                    if now.duration_since(last_risk_check).unwrap_or_default()
                        >= risk_check_interval
                    {
                        last_risk_check = now;

                        // 执行风险检查
                        let mut new_risk_events = Vec::new();
                        let mut should_pause_trading = false;
                        let mut should_emergency_exit = false;

                        // 检查保证金率
                        match check_margin_ratio(&info_client, user_address, grid_config).await {
                            Ok(margin_ratio) => {
                                last_margin_ratio = margin_ratio;
                                consecutive_failures = 0; // 重置失败计数

                                if margin_ratio < grid_config.margin_safety_threshold {
                                    let event = RiskEvent::new(
                                        RiskEventType::MarginInsufficient,
                                        format!(
                                            "保证金率({:.1}%)低于安全阈值({:.1}%)",
                                            margin_ratio * 100.0,
                                            grid_config.margin_safety_threshold * 100.0
                                        ),
                                        margin_ratio,
                                        grid_config.margin_safety_threshold,
                                    );
                                    new_risk_events.push(event);
                                    should_pause_trading = true;

                                    if margin_ratio < grid_config.margin_safety_threshold * 0.8 {
                                        should_emergency_exit = true;
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("⚠️ 保证金率检查失败: {:?}", e);
                                consecutive_failures += 1;

                                if consecutive_failures >= 3 {
                                    let event = RiskEvent::new(
                                        RiskEventType::NetworkIssue,
                                        format!("连续{}次保证金检查失败", consecutive_failures),
                                        consecutive_failures as f64,
                                        3.0,
                                    );
                                    new_risk_events.push(event);
                                }
                            }
                        }

                        // 检查最大回撤
                        if grid_state.current_metrics.max_drawdown > grid_config.max_drawdown {
                            let event = RiskEvent::new(
                                RiskEventType::MaxDrawdownExceeded,
                                format!(
                                    "最大回撤({:.2}%)超过限制({:.2}%)",
                                    grid_state.current_metrics.max_drawdown * 100.0,
                                    grid_config.max_drawdown * 100.0
                                ),
                                grid_state.current_metrics.max_drawdown,
                                grid_config.max_drawdown,
                            );
                            new_risk_events.push(event);
                            should_pause_trading = true;
                        }

                        // 检查每日亏损
                        let current_capital = grid_state.available_funds
                            + grid_state.position_quantity * current_price;
                        let daily_loss_ratio =
                            (daily_start_capital - current_capital) / daily_start_capital;

                        if daily_loss_ratio > grid_config.max_daily_loss {
                            let event = RiskEvent::new(
                                RiskEventType::DailyLossExceeded,
                                format!(
                                    "每日亏损({:.2}%)超过限制({:.2}%)",
                                    daily_loss_ratio * 100.0,
                                    grid_config.max_daily_loss * 100.0
                                ),
                                daily_loss_ratio,
                                grid_config.max_daily_loss,
                            );
                            new_risk_events.push(event);
                            should_pause_trading = true;
                        }

                        // 检查持仓规模
                        let position_value = grid_state.position_quantity.abs() * current_price;
                        if position_value > grid_config.max_position {
                            let event = RiskEvent::new(
                                RiskEventType::PositionSizeExceeded,
                                format!(
                                    "持仓价值({:.2})超过最大限制({:.2})",
                                    position_value, grid_config.max_position
                                ),
                                position_value,
                                grid_config.max_position,
                            );
                            new_risk_events.push(event);
                        }

                        // 检查市场波动率
                        if price_history.len() >= 10 {
                            let volatility = calculate_market_volatility(&price_history);
                            if volatility > 0.15 {
                                // 15%的波动率阈值
                                let event = RiskEvent::new(
                                    RiskEventType::VolatilitySpike,
                                    format!("市场波动率({:.2}%)异常高", volatility * 100.0),
                                    volatility,
                                    0.15,
                                );
                                new_risk_events.push(event);
                            }
                        }

                        // 检查价格跳空
                        if price_history.len() >= 2 {
                            let last_price_val = price_history[price_history.len() - 2];
                            let price_gap =
                                ((current_price - last_price_val) / last_price_val).abs();

                            if price_gap > 0.05 {
                                // 5%的价格跳空阈值
                                let event = RiskEvent::new(
                                    RiskEventType::PriceGap,
                                    format!("价格跳空({:.2}%)过大", price_gap * 100.0),
                                    price_gap,
                                    0.05,
                                );
                                new_risk_events.push(event);
                                should_pause_trading = true;
                            }
                        }

                        // 处理新的风险事件
                        for mut event in new_risk_events {
                            info!(
                                "🚨 检测到风险事件: {} - {}",
                                event.event_type.as_str(),
                                event.description
                            );

                            let action = match event.event_type {
                                RiskEventType::MarginInsufficient => {
                                    stop_trading_flag.store(true, Ordering::SeqCst);
                                    "暂停交易，等待保证金补充".to_string()
                                }
                                RiskEventType::MaxDrawdownExceeded => {
                                    stop_trading_flag.store(true, Ordering::SeqCst);
                                    "触发最大回撤保护，暂停交易".to_string()
                                }
                                RiskEventType::DailyLossExceeded => {
                                    stop_trading_flag.store(true, Ordering::SeqCst);
                                    "每日亏损超限，暂停交易".to_string()
                                }
                                RiskEventType::PositionSizeExceeded => {
                                    "建议减仓，降低持仓风险".to_string()
                                }
                                RiskEventType::VolatilitySpike => {
                                    "市场波动加剧，建议减少网格密度".to_string()
                                }
                                RiskEventType::PriceGap => {
                                    "价格跳空，暂停交易等待市场稳定".to_string()
                                }
                                _ => "风险事件已记录".to_string(),
                            };

                            event.mark_handled(action.clone());
                            risk_events.push(event);

                            info!("✅ 风险事件处理完成: {}", action);
                        }

                        // 保留最近50个风险事件
                        if risk_events.len() > 50 {
                            risk_events.drain(0..risk_events.len() - 50);
                        }

                        // 检查是否需要紧急退出
                        if should_emergency_exit {
                            error!("🚨 触发紧急风险控制，立即退出");

                            if let Err(e) = safe_shutdown(
                                &exchange_client,
                                grid_config,
                                &mut grid_state,
                                &mut active_orders,
                                &mut buy_orders,
                                &mut sell_orders,
                                current_price,
                                ShutdownReason::EmergencyShutdown,
                                start_time,
                            )
                            .await
                            {
                                error!("❌ 紧急退出过程中发生错误: {:?}", e);
                            }

                            break;
                        }

                        // 检查是否需要暂停交易
                        if should_pause_trading && !stop_trading_flag.load(Ordering::SeqCst) {
                            warn!("⚠️ 风险控制触发，暂停新的交易操作");
                            stop_trading_flag.store(true, Ordering::SeqCst);
                        }

                        // 1.6. 订单优先级管理
                        // 更新市场条件
                        if price_history.len() >= 2 {
                            let volatility = calculate_market_volatility(&price_history);
                            let price_change = ((current_price
                                - price_history[price_history.len() - 2])
                                / price_history[price_history.len() - 2])
                                .abs();
                            order_manager.update_market_conditions(
                                current_price,
                                volatility,
                                price_change,
                            );
                        }

                        // 处理过期订单
                        if let Err(e) = check_expired_orders(
                            &exchange_client,
                            &mut order_manager,
                            grid_config,
                            current_price,
                        )
                        .await
                        {
                            warn!("⚠️ 处理过期订单失败: {:?}", e);
                        }

                        // 处理紧急订单
                        let urgent_orders = order_manager.get_urgent_orders();
                        if !urgent_orders.is_empty() {
                            info!("🚨 检测到{}个紧急订单需要处理", urgent_orders.len());
                            for urgent_order in urgent_orders {
                                if urgent_order.needs_immediate_attention() {
                                    info!(
                                        "⚡ 处理紧急订单: {} - 剩余时间: {:?}秒",
                                        urgent_order.priority.as_str(),
                                        urgent_order.remaining_seconds()
                                    );

                                    // 这里可以添加紧急订单的特殊处理逻辑
                                    // 例如：提高订单优先级、使用市价单等
                                }
                            }
                        }

                        // 定期清理过期订单（每5分钟）
                        let cleanup_interval = Duration::from_secs(300);
                        if SystemTime::now()
                            .duration_since(order_manager.last_cleanup_time)
                            .unwrap_or_default()
                            >= cleanup_interval
                        {
                            let expired_count = order_manager.cleanup_expired_orders().len();
                            if expired_count > 0 {
                                info!("🧹 清理了{}个过期订单", expired_count);
                            }
                        }

                        // 检查是否需要重置每日统计
                        if now
                            .duration_since(daily_start_time)
                            .unwrap_or_default()
                            .as_secs()
                            >= 24 * 60 * 60
                        {
                            daily_start_capital = current_capital;
                            daily_start_time = now;
                            consecutive_failures = 0;
                            info!("🔄 每日风险统计已重置");
                        }

                        // 定期显示风险报告（每小时一次）
                        if risk_events.len() > 0
                            && now
                                .duration_since(daily_start_time)
                                .unwrap_or_default()
                                .as_secs()
                                % 3600
                                < 30
                        {
                            let recent_events: Vec<_> = risk_events
                                .iter()
                                .filter(|e| {
                                    now.duration_since(e.timestamp)
                                        .unwrap_or_default()
                                        .as_secs()
                                        < 24 * 3600
                                })
                                .collect();
                            let critical_events =
                                recent_events.iter().filter(|e| e.is_critical()).count();

                            info!("📊 风险控制报告:");
                            info!("   - 最近24小时风险事件: {}", recent_events.len());
                            info!("   - 其中严重事件: {}", critical_events);
                            info!("   - 连续失败次数: {}", consecutive_failures);
                            info!("   - 最近保证金率: {:.2}%", last_margin_ratio * 100.0);
                        }
                    }

                    // 检查风险控制标志
                    if stop_trading_flag.load(Ordering::SeqCst) {
                        warn!("⚠️ 风险控制已激活，跳过交易操作");
                        sleep(Duration::from_secs(grid_config.check_interval)).await;
                        continue;
                    }

                    // 1.6. 智能订单更新检查
                    if let Err(e) = smart_update_orders(
                        &exchange_client,
                        grid_config,
                        &mut grid_state,
                        current_price,
                        &price_history,
                        &mut active_orders,
                        &mut buy_orders,
                        &mut sell_orders,
                        &mut batch_optimizer,
                    ).await {
                        warn!("⚠️ 智能订单更新失败: {:?}", e);
                    }

                    // 1.7. 过期订单清理
                    if let Err(e) = cleanup_expired_orders(
                        &exchange_client,
                        grid_config,
                        &grid_state,
                        &mut active_orders,
                        &mut buy_orders,
                        &mut sell_orders,
                    ).await {
                        warn!("⚠️ 过期订单清理失败: {:?}", e);
                    }

                    // 1.8. 连接管理器检查
                    let connection_check_interval = Duration::from_secs(60); // 每分钟检查一次连接
                    if last_connection_check.elapsed() >= connection_check_interval {
                        last_connection_check = Instant::now();

                        match connection_manager
                            .check_connection(&info_client, user_address)
                            .await
                        {
                            Ok(is_healthy) => {
                                if !is_healthy {
                                    warn!("⚠️ 连接质量下降，尝试重连");

                                    match connection_manager
                                        .attempt_reconnect(&info_client, user_address)
                                        .await
                                    {
                                        Ok(true) => {
                                            info!("✅ 连接重连成功");
                                        }
                                        Ok(false) => {
                                            warn!("⚠️ 连接重连失败，但系统继续运行");
                                        }
                                        Err(e) => {
                                            error!("❌ 连接重连过程出错: {}", e);

                                            // 如果连接完全失败，考虑暂停交易
                                            if connection_manager.get_status()
                                                == &ConnectionStatus::Failed
                                            {
                                                warn!("🚨 连接完全失败，暂停交易操作");
                                                stop_trading_flag.store(true, Ordering::SeqCst);

                                                // 记录网络风险事件
                                                let network_event = RiskEvent::new(
                                                    RiskEventType::NetworkIssue,
                                                    format!("网络连接失败: {}", e),
                                                    0.0,
                                                    1.0,
                                                );
                                                risk_events.push(network_event);
                                            }
                                        }
                                    }
                                } else {
                                    // 连接健康，记录数据接收事件
                                    connection_manager.last_data_received = Instant::now();
                                }
                            }
                            Err(e) => {
                                warn!("⚠️ 连接检查失败: {}", e);
                                connection_manager.on_connection_lost(&e);
                            }
                        }

                        // 定期显示连接报告（每10分钟一次）
                        if last_connection_report.elapsed() >= Duration::from_secs(600) {
                            last_connection_report = Instant::now();
                            let report = connection_manager.get_connection_report();
                            info!("📡 连接状态报告:\n{}", report);
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

                        // 在重平衡前自动优化参数
                        if grid_state.performance_history.len() >= 20 {
                            info!("📈 开始自动网格参数优化");
                            let optimization_applied =
                                auto_optimize_grid_parameters(&mut grid_state, grid_config);

                            if !optimization_applied {
                                // 如果没有应用自动优化，则显示建议
                                info!("📊 显示网格参数优化建议");
                                analyze_grid_performance_and_suggest_optimization(
                                    grid_config,
                                    &grid_state,
                                );
                            }

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

                                let recent_profit: f64 =
                                    recent_records.iter().map(|r| r.profit).sum();
                                let recent_win_rate =
                                    recent_records.iter().filter(|r| r.profit > 0.0).count() as f64
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
                                if (temp_min_spacing - grid_config.min_grid_spacing).abs() > 0.0001
                                {
                                    info!(
                                        "🔧 参数优化建议 - 最小网格间距: {:.4}% -> {:.4}%",
                                        grid_config.min_grid_spacing * 100.0,
                                        temp_min_spacing * 100.0
                                    );
                                }
                                if (temp_max_spacing - grid_config.max_grid_spacing).abs() > 0.0001
                                {
                                    info!(
                                        "🔧 参数优化建议 - 最大网格间距: {:.4}% -> {:.4}%",
                                        grid_config.max_grid_spacing * 100.0,
                                        temp_max_spacing * 100.0
                                    );
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
                    if should_execute_periodic_task(
                        grid_state.last_order_batch_time,
                        30,
                        "订单状态检查"
                    ) {
                        if let Err(e) = check_order_status(
                            &info_client,
                            user_address,
                            &mut active_orders,
                            &mut buy_orders,
                            &mut sell_orders,
                        )
                        .await
                        {
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
                            &mut order_manager,
                        )
                        .await?;

                        // 如果配置了批量订单，可以在这里使用批量创建功能
                        if grid_config.max_orders_per_batch > 1
                            && grid_config.order_batch_delay_ms > 0
                        {
                            info!(
                                "💡 批量订单配置已启用 - 批次大小: {}, 延迟: {}ms",
                                grid_config.max_orders_per_batch, grid_config.order_batch_delay_ms
                            );
                        }
                    }

                    // 4. 资金分配监控
                    if let Err(e) =
                        monitor_fund_allocation(&grid_state, &buy_orders, &sell_orders, grid_config)
                    {
                        warn!("⚠️ 资金分配监控警告: {:?}", e);
                    }

                    // 4.1 保证金监控（每5分钟检查一次）
                    if should_execute_periodic_task(
                        grid_state.last_margin_check,
                        300,
                        "保证金监控"
                    ) {
                        // 首先检查连接状态
                        match ensure_connection(&info_client, user_address, &mut grid_state).await {
                            Ok(true) => {
                                // 连接正常，进行保证金检查
                                match check_margin_ratio(&info_client, user_address, grid_config)
                                    .await
                                {
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
                                            )
                                            .await
                                            {
                                                error!("❌ 紧急止损执行失败: {:?}", stop_err);
                                            }

                                            // 保证金不足时安全退出
                                            if let Err(e) = safe_shutdown(
                                                &exchange_client,
                                                grid_config,
                                                &mut grid_state,
                                                &mut active_orders,
                                                &mut buy_orders,
                                                &mut sell_orders,
                                                current_price,
                                                ShutdownReason::MarginInsufficient,
                                                start_time,
                                            )
                                            .await
                                            {
                                                error!("❌ 安全退出过程中发生错误: {:?}", e);
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
                                    error!("🚨 网络连接失败次数过多，开始安全退出");

                                    let current_price = last_price.unwrap_or(0.0);
                                    if let Err(e) = safe_shutdown(
                                        &exchange_client,
                                        grid_config,
                                        &mut grid_state,
                                        &mut active_orders,
                                        &mut buy_orders,
                                        &mut sell_orders,
                                        current_price,
                                        ShutdownReason::NetworkError,
                                        start_time,
                                    )
                                    .await
                                    {
                                        error!("❌ 安全退出过程中发生错误: {:?}", e);
                                    }

                                    break;
                                }
                            }
                        }
                    }

                    // 5. 定期状态报告和参数管理（每小时）
                    if should_execute_periodic_task(last_status_report, 3600, "状态报告") {
                        // 更新性能指标
                        grid_state.current_metrics =
                            calculate_performance_metrics(&grid_state, &price_history);

                        // 检查是否需要回滚（基于当前性能）
                        let current_performance_score =
                            if grid_state.current_metrics.total_profit > 0.0 {
                                50.0
                            } else {
                                0.0
                            } + grid_state.current_metrics.win_rate * 30.0
                                + if grid_state.current_metrics.total_profit
                                    / grid_state.current_metrics.total_trades.max(1) as f64
                                    > 0.0
                                {
                                    20.0
                                } else {
                                    0.0
                                };

                        if let Some(checkpoint) = grid_state
                            .dynamic_params
                            .should_rollback(current_performance_score)
                        {
                            warn!("🔄 定期检查发现性能下降，执行参数回滚");
                            let checkpoint_clone = checkpoint.clone();
                            grid_state
                                .dynamic_params
                                .rollback_to_checkpoint(&checkpoint_clone);

                            // 保存回滚后的参数
                            if let Err(e) = grid_state
                                .dynamic_params
                                .save_to_file("dynamic_grid_params.json")
                            {
                                warn!("⚠️ 保存回滚参数失败: {:?}", e);
                            }

                            // 回滚后需要重新创建网格
                            info!("🔄 参数回滚后重新创建网格");
                            cancel_all_orders(
                                &exchange_client,
                                &mut active_orders,
                                &grid_config.trading_asset,
                            )
                            .await?;
                            buy_orders.clear();
                            sell_orders.clear();
                        } else {
                            // 定期保存当前参数状态
                            if let Err(e) = grid_state
                                .dynamic_params
                                .save_to_file("dynamic_grid_params.json")
                            {
                                warn!("⚠️ 定期保存动态参数失败: {:?}", e);
                            }
                        }

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
                        info!(
                            "   总交易数: {} (胜: {}, 负: {})",
                            grid_state.current_metrics.total_trades,
                            grid_state.current_metrics.winning_trades,
                            grid_state.current_metrics.losing_trades
                        );
                        info!(
                            "   胜率: {:.1}%, 利润因子: {:.2}, 夏普比率: {:.2}",
                            grid_state.current_metrics.win_rate * 100.0,
                            grid_state.current_metrics.profit_factor,
                            grid_state.current_metrics.sharpe_ratio
                        );
                        info!(
                            "   总利润: {:.2}, 最大回撤: {:.2}%",
                            grid_state.current_metrics.total_profit,
                            grid_state.current_metrics.max_drawdown * 100.0
                        );
                        info!(
                            "   平均盈利: {:.2}, 平均亏损: {:.2}",
                            grid_state.current_metrics.average_win,
                            grid_state.current_metrics.average_loss
                        );
                        info!(
                            "   最大单笔盈利: {:.2}, 最大单笔亏损: {:.2}",
                            grid_state.current_metrics.largest_win,
                            grid_state.current_metrics.largest_loss
                        );

                        // 订单优先级管理状态报告
                        let order_stats = order_manager.get_statistics_report();
                        info!("📋 订单优先级管理状态:");
                        for line in order_stats.lines() {
                            info!("   {}", line);
                        }

                        // 批处理优化器状态报告
                        let batch_report = batch_optimizer.get_performance_report();
                        info!("⚡ 批处理优化器状态:");
                        for line in batch_report.lines() {
                            info!("   {}", line);
                        }

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
                                        total_capital: grid_state.available_funds
                                            + grid_state.position_quantity * fill_price,
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
        tokio::select! {
            _ = sleep(Duration::from_secs(grid_config.check_interval)) => {},
            _ = cancellation_token.cancelled() => {
                info!("🔔 收到取消信号，退出主循环");
                break;
            }
        }
    }

    // 执行安全退出流程 - 无论退出原因如何都需要取消订单
    info!("🏁 开始策略安全退出流程");
    let current_price = last_price.unwrap_or(0.0);

    // 确定退出原因
    let shutdown_reason = if shutdown_flag.load(Ordering::SeqCst) {
        ShutdownReason::UserSignal
    } else {
        ShutdownReason::NormalExit
    };

    if let Err(e) = safe_shutdown(
        &exchange_client,
        grid_config,
        &mut grid_state,
        &mut active_orders,
        &mut buy_orders,
        &mut sell_orders,
        current_price,
        shutdown_reason,
        start_time,
    )
    .await
    {
        error!("❌ 安全退出过程中发生错误: {:?}", e);

        // 如果安全退出失败，至少尝试取消所有订单
        if !active_orders.is_empty() {
            warn!("⚠️ 安全退出失败，尝试紧急取消所有订单");

            if let Err(cancel_err) =
                cancel_all_orders(&exchange_client, &mut active_orders, "FARTCOIN").await
            {
                error!("❌ 紧急取消订单也失败: {:?}", cancel_err);
                error!("🚨 请手动在交易所界面取消剩余订单!");

                // 输出剩余订单ID供手动取消
                if !active_orders.is_empty() {
                    error!("📝 剩余未取消订单ID: {:?}", active_orders);
                }
            } else {
                info!("✅ 紧急订单取消成功");
                active_orders.clear();
                buy_orders.clear();
                sell_orders.clear();
            }
        }
    }

    info!("🏁 网格交易策略已安全结束");
    Ok(())
}

// 安全解析字符串为f64，支持空值和无效值处理
fn safe_parse_f64(
    value: &str,
    field_name: &str,
    default_value: f64,
) -> Result<f64, GridStrategyError> {
    // 处理空字符串或仅包含空白字符的情况
    let trimmed = value.trim();
    if trimmed.is_empty() {
        warn!(
            "⚠️ 字段 '{}' 为空，使用默认值: {}",
            field_name, default_value
        );
        return Ok(default_value);
    }

    // 尝试解析数值
    match trimmed.parse::<f64>() {
        Ok(parsed_value) => {
            // 检查是否为有效数值（非NaN、非无穷大）
            if parsed_value.is_finite() && parsed_value >= 0.0 {
                Ok(parsed_value)
            } else {
                warn!(
                    "⚠️ 字段 '{}' 包含无效数值: {}，使用默认值: {}",
                    field_name, parsed_value, default_value
                );
                Ok(default_value)
            }
        }
        Err(e) => {
            warn!(
                "⚠️ 字段 '{}' 解析失败: '{}' -> {:?}，使用默认值: {}",
                field_name, trimmed, e, default_value
            );
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
                "获取账户信息失败: {:?}",
                e
            )));
        }
    };

    // 检查margin_summary字段是否存在
    let margin_summary = &account_info.margin_summary;

    // 安全解析账户价值
    let account_value = safe_parse_f64(&margin_summary.account_value, "account_value", 0.0)?;

    // 安全解析已使用保证金
    let total_margin_used =
        safe_parse_f64(&margin_summary.total_margin_used, "total_margin_used", 0.0)?;

    // 尝试解析其他相关字段以获得更完整的保证金信息
    let total_ntl_pos =
        safe_parse_f64(&margin_summary.total_ntl_pos, "total_ntl_pos", 0.0).unwrap_or(0.0);

    let total_raw_usd =
        safe_parse_f64(&margin_summary.total_raw_usd, "total_raw_usd", 0.0).unwrap_or(0.0);

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

    info!(
        "💳 保证金健康度: {} (比率: {:.2}%)",
        health_status,
        margin_ratio * 100.0
    );

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
        get_account_info(info_client, user_address),
    )
    .await;

    match connection_result {
        Ok(Ok(_account_info)) => {
            // 连接成功
            if grid_state.connection_retry_count > 0 {
                info!(
                    "✅ 网络连接已恢复 (之前重试次数: {})",
                    grid_state.connection_retry_count
                );
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
                "网络超时" => 8,   // 网络问题允许更多重试
                "API限制" => 5,    // API限制适中重试
                "认证失败" => 2,   // 认证问题快速失败
                "服务器错误" => 6, // 服务器问题适中重试
                _ => 5,            // 默认重试次数
            };

            if grid_state.connection_retry_count > max_retries {
                error!(
                    "❌ 连接失败次数过多 ({}/{}，错误类型: {})",
                    grid_state.connection_retry_count, max_retries, error_type
                );
                return Err(GridStrategyError::NetworkError(format!(
                    "连接失败次数过多: {} (错误类型: {})",
                    grid_state.connection_retry_count, error_type
                )));
            }

            // 根据错误类型和重试次数计算等待时间 - 改进的指数退避机制
            let (base_delay, max_backoff) = match error_type.as_str() {
                "API限制" => (5, 600),    // API限制：基础5秒，最多等待10分钟
                "网络超时" => (2, 120),   // 网络超时：基础2秒，最多等待2分钟
                "服务器错误" => (3, 300), // 服务器错误：基础3秒，最多等待5分钟
                "认证失败" => (2, 60),    // 认证失败：基础2秒，最多等待1分钟
                _ => (2, 180),            // 默认：基础2秒，最多等待3分钟
            };

            let (wait_seconds, backoff_seconds, max_backoff_used) = calculate_exponential_backoff(
                base_delay,
                grid_state.connection_retry_count,
                max_backoff,
                &error_type,
            );

            info!(
                "⏱️ 等待 {}秒 后重试连接 (错误类型: {}, 基础延迟: {}s, 指数退避: {}s, 上限: {}s)",
                wait_seconds, error_type, base_delay, backoff_seconds, max_backoff_used
            );

            // 如果等待时间较长，提供额外信息
            if wait_seconds >= 60 {
                info!(
                    "💡 等待时间较长({:.1}分钟)，这是为了避免频繁重试对服务器造成压力",
                    wait_seconds as f64 / 60.0
                );
            }

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
                error!(
                    "❌ 连接超时次数过多 ({}次)",
                    grid_state.connection_retry_count
                );
                return Err(GridStrategyError::NetworkError(
                    "连接超时次数过多".to_string(),
                ));
            }

            // 超时情况下的改进指数退避机制
            let base_timeout_delay = 3;
            let max_timeout_backoff = 120; // 超时情况最多等待2分钟

            let (wait_seconds, timeout_backoff_seconds, max_backoff_used) =
                calculate_exponential_backoff(
                    base_timeout_delay,
                    grid_state.connection_retry_count,
                    max_timeout_backoff,
                    "连接超时",
                );

            info!(
                "⏱️ 连接超时，等待 {}秒 后重试 (基础延迟: {}s, 指数退避: {}s, 上限: {}s)",
                wait_seconds, base_timeout_delay, timeout_backoff_seconds, max_backoff_used
            );

            if wait_seconds >= 60 {
                info!(
                    "💡 超时重试等待时间较长({:.1}分钟)，建议检查网络连接",
                    wait_seconds as f64 / 60.0
                );
            }

            sleep(Duration::from_secs(wait_seconds)).await;

            Ok(false)
        }
    }
}

// 计算指数退避延迟时间，包含上限控制
fn calculate_exponential_backoff(
    base_delay: u64,
    retry_count: u32,
    max_backoff: u64,
    _error_type: &str,
) -> (u64, u64, u64) {
    // 指数退避计算，限制最大指数为4（避免过长等待）
    let backoff_seconds = base_delay * 2_u64.pow(retry_count.min(4));

    // 应用上限
    let actual_wait_seconds = backoff_seconds.min(max_backoff);

    // 返回 (实际等待时间, 指数退避计算值, 上限值)
    (actual_wait_seconds, backoff_seconds, max_backoff)
}

// 分析连接错误类型，用于制定不同的重试策略
fn classify_connection_error(error: &GridStrategyError) -> String {
    let error_msg = format!("{:?}", error).to_lowercase();

    if error_msg.contains("timeout") || error_msg.contains("超时") {
        "网络超时".to_string()
    } else if error_msg.contains("rate limit")
        || error_msg.contains("限制")
        || error_msg.contains("429")
    {
        "API限制".to_string()
    } else if error_msg.contains("unauthorized")
        || error_msg.contains("认证")
        || error_msg.contains("401")
        || error_msg.contains("403")
    {
        "认证失败".to_string()
    } else if error_msg.contains("500")
        || error_msg.contains("502")
        || error_msg.contains("503")
        || error_msg.contains("服务器")
    {
        "服务器错误".to_string()
    } else if error_msg.contains("network")
        || error_msg.contains("connection")
        || error_msg.contains("网络")
    {
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
    let returns: Vec<f64> = grid_state
        .performance_history
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
    batch_optimizer: &mut BatchTaskOptimizer,
) -> Result<(Vec<u64>, Vec<OrderRequestInfo>), GridStrategyError> {
    let start_time = SystemTime::now();
    let mut created_order_ids = Vec::new();
    let mut all_failed_order_infos = Vec::new();

    if orders.is_empty() {
        return Ok((created_order_ids, all_failed_order_infos));
    }

    // 资源限制检查
    let max_total_orders = 500; // 单次最多创建500个订单
    if orders.len() > max_total_orders {
        warn!(
            "⚠️ 订单数量({})超过限制({})，将只处理前{}个订单",
            orders.len(),
            max_total_orders,
            max_total_orders
        );
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

    // 使用批处理优化器确定最优批次大小
    let optimal_batch_size = batch_optimizer.optimize_batch_size(orders_to_process.len());

    info!(
        "📦 开始智能批量创建订单 - 总数: {}, 优化批次大小: {}, 延迟: {}ms",
        orders_to_process.len(),
        optimal_batch_size,
        grid_config.order_batch_delay_ms
    );
    info!(
        "⚡ 批处理优化器状态: 目标时间={:.2}秒, 历史记录={}次",
        batch_optimizer.target_execution_time.as_secs_f64(),
        batch_optimizer.last_execution_times.len()
    );

    // 超时控制 - 总体处理时间限制
    let max_total_time = Duration::from_secs(300); // 5分钟总超时
    let batch_timeout = Duration::from_secs(30); // 单批次30秒超时

    // 分批处理订单
    let mut order_iter = orders_to_process.into_iter();
    let mut batch_count = 0;

    loop {
        // 检查总体超时
        if start_time.elapsed().unwrap_or_default() > max_total_time {
            warn!("⚠️ 批量订单创建总体超时，停止处理剩余订单");
            break;
        }

        let mut current_batch = Vec::new();

        // 收集当前批次的订单
        for _ in 0..optimal_batch_size {
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
        info!(
            "📋 处理第{}批订单，数量: {}",
            batch_count, current_batch_len
        );

        // 批次级别的超时控制
        let batch_result = tokio::time::timeout(
            batch_timeout,
            process_order_batch(exchange_client, current_batch, grid_config),
        )
        .await;

        match batch_result {
            Ok(Ok((successful_ids, failed_order_infos))) => {
                // 批次处理成功
                let successful_count = successful_ids.len();
                let failed_count = failed_order_infos.len();

                created_order_ids.extend(successful_ids.iter());
                stats.successful_orders += successful_count;
                stats.failed_orders += failed_count;

                // 收集失败的订单信息用于重试
                all_failed_order_infos.extend(failed_order_infos);

                let batch_time = batch_start_time.elapsed().unwrap_or_default();
                info!(
                    "✅ 第{}批处理完成 - 成功: {}, 失败: {}, 耗时: {}ms",
                    batch_count,
                    successful_count,
                    failed_count,
                    batch_time.as_millis()
                );
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
    if !all_failed_order_infos.is_empty() && all_failed_order_infos.len() <= 50 {
        info!("🔄 开始重试{}个失败的订单", all_failed_order_infos.len());

        let retry_result = tokio::time::timeout(
            Duration::from_secs(60), // 重试阶段1分钟超时
            retry_failed_order_infos(exchange_client, all_failed_order_infos.clone(), grid_config),
        )
        .await;

        match retry_result {
            Ok(Ok(retry_successful_ids)) => {
                created_order_ids.extend(retry_successful_ids.iter());
                stats.successful_orders += retry_successful_ids.len();
                stats.retried_orders = retry_successful_ids.len();
                info!("✅ 重试完成 - 成功: {}", retry_successful_ids.len());
                // 清空已重试的失败订单
                all_failed_order_infos.clear();
            }
            Ok(Err(e)) => {
                warn!("❌ 重试失败: {:?}", e);
            }
            Err(_) => {
                warn!("⏰ 重试超时");
            }
        }
    } else if all_failed_order_infos.len() > 50 {
        warn!(
            "⚠️ 失败订单数量过多({}个)，跳过重试",
            all_failed_order_infos.len()
        );
    }

    // 更新统计信息
    stats.processing_time = start_time.elapsed().unwrap_or_default();
    stats.update_success_rate();

    // 记录批处理执行时间到优化器
    batch_optimizer.record_execution_time(stats.processing_time);

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
    info!(
        "   平均每订单: {:.1}ms",
        stats.processing_time.as_millis() as f64 / stats.total_orders as f64
    );

    // 性能警告
    if stats.success_rate < 80.0 {
        warn!(
            "⚠️ 订单创建成功率较低({:.1}%)，建议检查网络连接和API限制",
            stats.success_rate
        );
    }

    if stats.processing_time.as_secs() > 120 {
        warn!(
            "⚠️ 订单创建耗时较长({}秒)，建议优化批次大小",
            stats.processing_time.as_secs()
        );
    }

    info!(
        "✅ 增强批量订单创建完成 - 成功创建: {}/{}",
        created_order_ids.len(),
        stats.total_orders
    );
    Ok((created_order_ids, all_failed_order_infos))
}

// 订单信息结构体，用于重建失败的订单
#[derive(Debug, Clone)]
struct OrderRequestInfo {
    asset: String,
    is_buy: bool,
    reduce_only: bool,
    limit_px: f64,
    sz: f64,
}

impl OrderRequestInfo {
    fn from_client_order_request(order: &ClientOrderRequest) -> Self {
        Self {
            asset: order.asset.clone(),
            is_buy: order.is_buy,
            reduce_only: order.reduce_only,
            limit_px: order.limit_px,
            sz: order.sz,
        }
    }

    fn to_client_order_request(&self) -> ClientOrderRequest {
        ClientOrderRequest {
            asset: self.asset.clone(),
            is_buy: self.is_buy,
            reduce_only: self.reduce_only,
            limit_px: self.limit_px,
            sz: self.sz,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit {
                tif: "Gtc".to_string(),
            }),
        }
    }
}

// 处理单个批次的订单
async fn process_order_batch(
    exchange_client: &ExchangeClient,
    orders: Vec<ClientOrderRequest>,
    _grid_config: &crate::config::GridConfig,
) -> Result<(Vec<u64>, Vec<OrderRequestInfo>), GridStrategyError> {
    let mut successful_ids = Vec::new();
    let mut failed_order_infos = Vec::new();

    for order in orders {
        // 保存订单信息用于失败重试
        let order_info = OrderRequestInfo::from_client_order_request(&order);

        // 单个订单超时控制
        let order_result = tokio::time::timeout(
            Duration::from_secs(10), // 单个订单10秒超时
            exchange_client.order(order, None),
        )
        .await;

        match order_result {
            Ok(Ok(ExchangeResponseStatus::Ok(response))) => {
                if let Some(data) = response.data {
                    let mut order_created = false;
                    for status in data.statuses {
                        if let ExchangeDataStatus::Resting(order_info) = status {
                            successful_ids.push(order_info.oid);
                            info!("✅ 订单创建成功: ID={}", order_info.oid);
                            order_created = true;
                        }
                    }

                    // 如果响应成功但没有创建订单，也算作失败
                    if !order_created {
                        warn!("⚠️ 订单响应成功但未创建订单");
                        failed_order_infos.push(order_info);
                    }
                } else {
                    warn!("⚠️ 订单响应成功但无数据");
                    failed_order_infos.push(order_info);
                }
            }
            Ok(Ok(ExchangeResponseStatus::Err(err))) => {
                warn!("❌ 订单创建失败: {:?}", err);
                failed_order_infos.push(order_info);
            }
            Ok(Err(e)) => {
                warn!("❌ 订单创建失败: {:?}", e);
                failed_order_infos.push(order_info);
            }
            Err(_) => {
                warn!("⏰ 订单创建超时");
                failed_order_infos.push(order_info);
            }
        }

        // 订单间小延迟，避免过于频繁的请求
        if _grid_config.order_batch_delay_ms > 0 {
            sleep(Duration::from_millis(50)).await;
        }
    }

    info!(
        "📊 批次处理完成 - 成功: {}, 失败: {}",
        successful_ids.len(),
        failed_order_infos.len()
    );
    Ok((successful_ids, failed_order_infos))
}

// 重试失败的订单
#[allow(dead_code)]
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
            exchange_client.order(order, None),
        )
        .await;

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

// 重试失败的订单信息
async fn retry_failed_order_infos(
    exchange_client: &ExchangeClient,
    failed_order_infos: Vec<OrderRequestInfo>,
    _grid_config: &crate::config::GridConfig,
) -> Result<Vec<u64>, GridStrategyError> {
    let mut successful_ids = Vec::new();

    info!("🔄 开始重试{}个失败订单", failed_order_infos.len());

    for (index, order_info) in failed_order_infos.into_iter().enumerate() {
        // 重试前等待更长时间
        sleep(Duration::from_millis(200)).await;

        // 重建订单请求
        let order = order_info.to_client_order_request();

        let retry_result = tokio::time::timeout(
            Duration::from_secs(15), // 重试时使用更长的超时时间
            exchange_client.order(order, None),
        )
        .await;

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

    info!(
        "🔄 开始单个创建模式 - 订单数: {}, 类型: {}",
        order_infos.len(),
        if is_buy_order { "买单" } else { "卖单" }
    );

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
            exchange_client.order(order_request, None),
        )
        .await;

        match order_result {
            Ok(Ok(ExchangeResponseStatus::Ok(response))) => {
                if let Some(data) = response.data {
                    for status in data.statuses {
                        if let ExchangeDataStatus::Resting(order) = status {
                            active_orders.push(order.oid);
                            orders_map.insert(order.oid, order_info.clone());
                            success_count += 1;

                            info!(
                                "🔄✅ 单个{}创建成功: ID={}, 价格={:.4}, 数量={:.4}",
                                if is_buy_order { "买单" } else { "卖单" },
                                order.oid,
                                order_info.price,
                                order_info.quantity
                            );
                        }
                    }
                }
            }
            Ok(Ok(ExchangeResponseStatus::Err(err))) => {
                warn!(
                    "🔄❌ 单个{}创建失败: {:?}",
                    if is_buy_order { "买单" } else { "卖单" },
                    err
                );
            }
            Ok(Err(e)) => {
                warn!(
                    "🔄❌ 单个{}创建失败: {:?}",
                    if is_buy_order { "买单" } else { "卖单" },
                    e
                );
            }
            Err(_) => {
                warn!(
                    "🔄⏰ 单个{}创建超时",
                    if is_buy_order { "买单" } else { "卖单" }
                );
            }
        }

        // 订单间延迟
        sleep(Duration::from_millis(200)).await;

        // 每5个订单后稍作休息
        if (index + 1) % 5 == 0 {
            sleep(Duration::from_millis(500)).await;
        }
    }

    info!(
        "🔄✅ 单个创建模式完成 - 成功: {}/{}",
        success_count,
        order_infos.len()
    );
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
        info!(
            "📊 订单数量较多({}个)，启用分批处理模式",
            active_orders.len()
        );
        return check_order_status_in_batches(
            info_client,
            user_address,
            active_orders,
            buy_orders,
            sell_orders,
            max_orders_per_batch,
            max_processing_time,
        )
        .await;
    }

    // 使用超时控制的API调用
    let open_orders_result = tokio::time::timeout(
        Duration::from_secs(10), // API调用超时时间10秒
        info_client.open_orders(user_address),
    )
    .await;

    let open_orders = match open_orders_result {
        Ok(Ok(orders)) => orders,
        Ok(Err(e)) => {
            return Err(GridStrategyError::ClientError(format!(
                "获取开放订单失败: {:?}",
                e
            )));
        }
        Err(_) => {
            warn!("⚠️ 获取开放订单超时，跳过本次检查");
            return Ok(()); // 超时时不返回错误，避免阻塞主流程
        }
    };

    // 构建开放订单ID集合
    let open_order_ids: std::collections::HashSet<u64> =
        open_orders.iter().map(|order| order.oid).collect();

    info!(
        "🔍 订单状态检查 - 活跃订单: {}, 开放订单: {}",
        active_orders.len(),
        open_order_ids.len()
    );

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
    info!(
        "✅ 订单状态检查完成 - 处理时间: {}ms, 移除订单: {} (买单: {}, 卖单: {})",
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

    info!(
        "🔄 开始分批订单状态检查 - 总订单: {}, 批次大小: {}",
        active_orders.len(),
        batch_size
    );

    // 首先获取所有开放订单（只调用一次API）
    let open_orders_result = tokio::time::timeout(
        Duration::from_secs(15), // 增加超时时间，因为可能订单较多
        info_client.open_orders(user_address),
    )
    .await;

    let open_orders = match open_orders_result {
        Ok(Ok(orders)) => orders,
        Ok(Err(e)) => {
            return Err(GridStrategyError::ClientError(format!(
                "获取开放订单失败: {:?}",
                e
            )));
        }
        Err(_) => {
            warn!("⚠️ 获取开放订单超时，跳过本次检查");
            return Ok(());
        }
    };

    let open_order_ids: std::collections::HashSet<u64> =
        open_orders.iter().map(|order| order.oid).collect();

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

        info!(
            "📋 第{}批处理完成 - 检查: {}, 移除: {}",
            batch_count,
            chunk.len(),
            batch_removed
        );

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
    info!(
        "✅ 分批订单状态检查完成 - 处理时间: {}ms, 批次数: {}, 移除订单: {} (买单: {}, 卖单: {})",
        processing_time.as_millis(),
        batch_count,
        total_removed,
        removed_buy_orders,
        removed_sell_orders
    );

    Ok(())
}

// 自动优化网格参数
fn auto_optimize_grid_parameters(
    grid_state: &mut GridState,
    grid_config: &crate::config::GridConfig,
) -> bool {
    // 保存优化前的参数状态
    let old_params = grid_state.dynamic_params.clone();
    
    // 检查是否需要优化（每24小时最多优化一次）
    let current_timestamp = safe_unix_timestamp();
    if current_timestamp - grid_state.dynamic_params.last_optimization_time < 24 * 60 * 60 {
        return false;
    }

    // 需要足够的历史数据
    if grid_state.performance_history.len() < 20 {
        info!(
            "📊 历史数据不足({})，跳过自动优化",
            grid_state.performance_history.len()
        );
        return false;
    }

    // 分析最近的表现
    let recent_records: Vec<&PerformanceRecord> = grid_state
        .performance_history
        .iter()
        .rev()
        .take(30) // 分析最近30笔交易
        .collect();

    let recent_profit: f64 = recent_records.iter().map(|r| r.profit).sum();
    let recent_win_rate = recent_records.iter().filter(|r| r.profit > 0.0).count() as f64
        / recent_records.len() as f64;

    let avg_profit_per_trade = recent_profit / recent_records.len() as f64;

    // 计算性能评分 (0-100)
    let profit_score = if recent_profit > 0.0 { 50.0 } else { 0.0 };
    let win_rate_score = recent_win_rate * 30.0;
    let consistency_score = if avg_profit_per_trade > 0.0 {
        20.0
    } else {
        0.0
    };
    let performance_score = profit_score + win_rate_score + consistency_score;

    info!("📊 性能评分分析:");
    info!("   最近30笔交易利润: {:.2}", recent_profit);
    info!("   胜率: {:.1}%", recent_win_rate * 100.0);
    info!("   平均每笔利润: {:.2}", avg_profit_per_trade);
    info!("   综合评分: {:.1}/100", performance_score);

    // 根据性能评分决定优化策略
    let mut optimization_applied = false;
    let original_min_spacing = grid_state.dynamic_params.current_min_spacing;
    let original_max_spacing = grid_state.dynamic_params.current_max_spacing;
    let original_trade_amount = grid_state.dynamic_params.current_trade_amount;

    if performance_score >= 70.0 {
        // 表现优秀：适度增加网格间距和交易金额以获得更大利润
        let spacing_multiplier = 1.03; // 增加3%
        let amount_multiplier = 1.02; // 增加2%

        grid_state.dynamic_params.current_min_spacing =
            (original_min_spacing * spacing_multiplier).min(grid_config.max_grid_spacing * 0.8); // 不超过最大间距的80%

        grid_state.dynamic_params.current_max_spacing =
            (original_max_spacing * spacing_multiplier).min(grid_config.max_grid_spacing);

        grid_state.dynamic_params.current_trade_amount =
            (original_trade_amount * amount_multiplier).min(grid_state.total_capital * 0.1); // 不超过总资金的10%

        info!("🚀 性能优秀，执行积极优化策略");
        optimization_applied = true;
    } else if performance_score <= 30.0 {
        // 表现不佳：减少网格间距和交易金额以降低风险
        let spacing_multiplier = 0.97; // 减少3%
        let amount_multiplier = 0.95; // 减少5%

        grid_state.dynamic_params.current_min_spacing =
            (original_min_spacing * spacing_multiplier).max(grid_config.min_grid_spacing * 0.5); // 不低于最小间距的50%

        grid_state.dynamic_params.current_max_spacing = (original_max_spacing * spacing_multiplier)
            .max(grid_state.dynamic_params.current_min_spacing * 1.5);

        grid_state.dynamic_params.current_trade_amount =
            (original_trade_amount * amount_multiplier).max(grid_config.trade_amount * 0.3); // 不低于原始金额的30%

        info!("⚠️ 性能不佳，执行保守优化策略");
        optimization_applied = true;
    } else {
        // 表现中等：微调参数
        let market_volatility = grid_state.historical_volatility;

        if market_volatility > 0.02 {
            // 高波动市场
            // 增加网格间距以适应波动
            let spacing_multiplier = 1.01;
            grid_state.dynamic_params.current_min_spacing =
                (original_min_spacing * spacing_multiplier).min(grid_config.max_grid_spacing * 0.8);
            grid_state.dynamic_params.current_max_spacing =
                (original_max_spacing * spacing_multiplier).min(grid_config.max_grid_spacing);

            info!("📈 高波动市场，微调网格间距");
            optimization_applied = true;
        } else if market_volatility < 0.005 {
            // 低波动市场
            // 减少网格间距以增加交易频率
            let spacing_multiplier = 0.99;
            grid_state.dynamic_params.current_min_spacing =
                (original_min_spacing * spacing_multiplier).max(grid_config.min_grid_spacing * 0.8);
            grid_state.dynamic_params.current_max_spacing = (original_max_spacing
                * spacing_multiplier)
                .max(grid_state.dynamic_params.current_min_spacing * 1.5);

            info!("📉 低波动市场，微调网格间距");
            optimization_applied = true;
        }
    }

    if optimization_applied {
        // 创建优化前的检查点
        let optimization_reason = if performance_score >= 70.0 {
            "积极优化策略".to_string()
        } else if performance_score <= 30.0 {
            "保守优化策略".to_string()
        } else {
            "微调优化策略".to_string()
        };

        grid_state
            .dynamic_params
            .create_checkpoint(optimization_reason, performance_score);

        grid_state.dynamic_params.last_optimization_time = current_timestamp;
        grid_state.dynamic_params.optimization_count += 1;

        info!(
            "✅ 自动优化完成 (第{}次):",
            grid_state.dynamic_params.optimization_count
        );
        info!(
            "   最小网格间距: {:.4}% -> {:.4}% ({:+.2}%)",
            original_min_spacing * 100.0,
            grid_state.dynamic_params.current_min_spacing * 100.0,
            (grid_state.dynamic_params.current_min_spacing / original_min_spacing - 1.0) * 100.0
        );
        info!(
            "   最大网格间距: {:.4}% -> {:.4}% ({:+.2}%)",
            original_max_spacing * 100.0,
            grid_state.dynamic_params.current_max_spacing * 100.0,
            (grid_state.dynamic_params.current_max_spacing / original_max_spacing - 1.0) * 100.0
        );
        info!(
            "   交易金额: {:.2} -> {:.2} ({:+.2}%)",
            original_trade_amount,
            grid_state.dynamic_params.current_trade_amount,
            (grid_state.dynamic_params.current_trade_amount / original_trade_amount - 1.0) * 100.0
        );

        // 记录性能评分到滑动窗口
        grid_state
            .dynamic_params
            .performance_window
            .push(performance_score);
        if grid_state.dynamic_params.performance_window.len() > 10 {
            grid_state.dynamic_params.performance_window.remove(0);
        }

        // 验证优化后的参数
        let optimization_validation = validate_parameter_optimization(
            &old_params,
            &grid_state.dynamic_params,
            grid_config,
            performance_score,
        );

        optimization_validation.log_results("参数优化");

        // 如果验证失败，回滚参数
        if !optimization_validation.is_valid {
            error!("❌ 参数优化验证失败，回滚到优化前状态");
            grid_state.dynamic_params = old_params;
            return false;
        }

        // 保存参数到文件
        if let Err(e) = grid_state
            .dynamic_params
            .save_to_file("dynamic_grid_params.json")
        {
            warn!("⚠️ 保存动态参数失败: {:?}", e);
        }

        true
    } else {
        // 即使没有优化，也检查是否需要回滚
        if let Some(checkpoint) = grid_state.dynamic_params.should_rollback(performance_score) {
            warn!("🔄 性能下降，执行参数回滚");
            let checkpoint_clone = checkpoint.clone();
            grid_state
                .dynamic_params
                .rollback_to_checkpoint(&checkpoint_clone);

            // 保存回滚后的参数
            if let Err(e) = grid_state
                .dynamic_params
                .save_to_file("dynamic_grid_params.json")
            {
                warn!("⚠️ 保存回滚参数失败: {:?}", e);
            }

            return true; // 回滚也算是一种优化
        }

        info!("📊 性能中等，暂不执行自动优化");
        false
    }
}

// 安全退出函数
async fn safe_shutdown(
    exchange_client: &ExchangeClient,
    grid_config: &crate::config::GridConfig,
    grid_state: &mut GridState,
    active_orders: &mut Vec<u64>,
    buy_orders: &mut HashMap<u64, OrderInfo>,
    sell_orders: &mut HashMap<u64, OrderInfo>,
    current_price: f64,
    reason: ShutdownReason,
    start_time: SystemTime,
) -> Result<(), GridStrategyError> {
    info!("🛑 开始安全退出 - 原因: {}", reason.as_str());

    let shutdown_start = SystemTime::now();

    // 1. 取消所有未成交订单
    if !active_orders.is_empty() {
        info!("🗑️ 取消所有活跃订单 ({} 个)...", active_orders.len());

        // 紧急情况下使用更短的超时时间
        let cancel_timeout = if reason.is_emergency() {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(30)
        };

        let cancel_result = tokio::time::timeout(
            cancel_timeout,
            cancel_all_orders(exchange_client, active_orders, &grid_config.trading_asset),
        )
        .await;

        match cancel_result {
            Ok(Ok(_)) => {
                info!("✅ 所有订单已成功取消");
                buy_orders.clear();
                sell_orders.clear();
            }
            Ok(Err(e)) => {
                warn!("⚠️ 部分订单取消失败: {:?}", e);
            }
            Err(_) => {
                warn!("⚠️ 订单取消超时，继续执行后续步骤");
            }
        }
    }

    // 2. 根据退出原因和配置决定是否清仓
    // 注意：这里假设默认在退出时清仓，可以根据需要添加配置选项
    let close_positions_on_exit = true; // 可以从配置中读取
    let should_close_positions = reason.requires_position_close()
        || (close_positions_on_exit && grid_state.position_quantity > 0.0);

    if should_close_positions && grid_state.position_quantity > 0.0 {
        info!(
            "📉 执行清仓操作 - 持仓数量: {:.4}",
            grid_state.position_quantity
        );

        let close_timeout = if reason.is_emergency() {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(60)
        };

        let close_result = tokio::time::timeout(
            close_timeout,
            close_all_positions(
                exchange_client,
                grid_config,
                grid_state.position_quantity,
                0.0, // 假设只有多头持仓
                current_price,
            ),
        )
        .await;

        match close_result {
            Ok(Ok(_)) => {
                info!("✅ 清仓操作完成");
                grid_state.position_quantity = 0.0;
                grid_state.position_avg_price = 0.0;
            }
            Ok(Err(e)) => {
                error!("❌ 清仓操作失败: {:?}", e);
                // 记录失败但继续执行后续步骤
            }
            Err(_) => {
                error!("❌ 清仓操作超时");
            }
        }
    } else if grid_state.position_quantity > 0.0 {
        warn!(
            "⚠️ 退出时仍有持仓 {:.4}，根据配置未执行清仓",
            grid_state.position_quantity
        );
    }

    // 3. 保存性能数据和状态
    info!("💾 保存性能数据和状态...");

    if let Err(e) = save_performance_data(grid_state, start_time, reason.clone()).await {
        warn!("⚠️ 保存性能数据失败: {:?}", e);
    }

    // 4. 保存动态参数
    if let Err(e) = grid_state
        .dynamic_params
        .save_to_file("dynamic_grid_params.json")
    {
        warn!("⚠️ 保存动态参数失败: {:?}", e);
    }

    // 5. 生成最终报告
    let final_report = generate_final_report(grid_state, current_price, start_time, reason.clone());
    info!("\n{}", final_report);

    let shutdown_duration = shutdown_start.elapsed().unwrap_or_default();
    info!(
        "✅ 安全退出完成 - 耗时: {:.2}秒",
        shutdown_duration.as_secs_f64()
    );

    Ok(())
}

// 保存性能数据
async fn save_performance_data(
    grid_state: &GridState,
    start_time: SystemTime,
    reason: ShutdownReason,
) -> Result<(), GridStrategyError> {
    let current_time = SystemTime::now();
    let trading_duration = current_time.duration_since(start_time).unwrap_or_default();

    // 计算最终性能指标
    let final_metrics = calculate_performance_metrics(grid_state, &[]);
    let final_total_value =
        grid_state.available_funds + grid_state.position_quantity * grid_state.position_avg_price;
    let final_roi = if grid_state.total_capital > 0.0 {
        (final_total_value / grid_state.total_capital - 1.0) * 100.0
    } else {
        0.0
    };

    let snapshot = PerformanceSnapshot {
        timestamp: safe_unix_timestamp(),
        total_capital: grid_state.total_capital,
        available_funds: grid_state.available_funds,
        position_quantity: grid_state.position_quantity,
        position_avg_price: grid_state.position_avg_price,
        realized_profit: grid_state.realized_profit,
        total_trades: final_metrics.total_trades,
        winning_trades: final_metrics.winning_trades,
        win_rate: final_metrics.win_rate,
        max_drawdown: final_metrics.max_drawdown,
        sharpe_ratio: final_metrics.sharpe_ratio,
        profit_factor: final_metrics.profit_factor,
        trading_duration_hours: trading_duration.as_secs_f64() / 3600.0,
        final_roi,
    };

    // 保存到文件
    let filename = format!(
        "performance_snapshot_{}.json",
        safe_unix_timestamp()
    );

    match serde_json::to_string_pretty(&snapshot) {
        Ok(json_data) => {
            match std::fs::write(&filename, json_data) {
                Ok(_) => {
                    info!("💾 性能快照已保存到: {}", filename);

                    // 同时保存详细的交易历史
                    save_trading_history(grid_state, reason).await?;
                }
                Err(e) => {
                    return Err(GridStrategyError::ConfigError(format!(
                        "保存性能快照失败: {:?}",
                        e
                    )));
                }
            }
        }
        Err(e) => {
            return Err(GridStrategyError::ConfigError(format!(
                "序列化性能数据失败: {:?}",
                e
            )));
        }
    }

    Ok(())
}

// 保存交易历史
async fn save_trading_history(
    grid_state: &GridState,
    reason: ShutdownReason,
) -> Result<(), GridStrategyError> {
    if grid_state.performance_history.is_empty() {
        return Ok(());
    }

    let filename = format!(
        "trading_history_{}.json",
        safe_unix_timestamp()
    );

    #[derive(serde::Serialize)]
    struct TradingHistoryExport {
        shutdown_reason: String,
        export_time: u64,
        total_trades: usize,
        trades: Vec<PerformanceRecord>,
    }

    let export_data = TradingHistoryExport {
        shutdown_reason: reason.as_str().to_string(),
        export_time: safe_unix_timestamp(),
        total_trades: grid_state.performance_history.len(),
        trades: grid_state.performance_history.clone(),
    };

    match serde_json::to_string_pretty(&export_data) {
        Ok(json_data) => match std::fs::write(&filename, json_data) {
            Ok(_) => {
                info!(
                    "📊 交易历史已保存到: {} ({} 笔交易)",
                    filename,
                    grid_state.performance_history.len()
                );
            }
            Err(e) => {
                warn!("⚠️ 保存交易历史失败: {:?}", e);
            }
        },
        Err(e) => {
            warn!("⚠️ 序列化交易历史失败: {:?}", e);
        }
    }

    Ok(())
}

// 生成最终报告
fn generate_final_report(
    grid_state: &GridState,
    current_price: f64,
    start_time: SystemTime,
    reason: ShutdownReason,
) -> String {
    let current_time = SystemTime::now();
    let trading_duration = current_time.duration_since(start_time).unwrap_or_default();
    let final_metrics = calculate_performance_metrics(grid_state, &[]);

    let final_total_value =
        grid_state.available_funds + grid_state.position_quantity * current_price;
    let total_return = final_total_value - grid_state.total_capital;
    let roi = if grid_state.total_capital > 0.0 {
        (final_total_value / grid_state.total_capital - 1.0) * 100.0
    } else {
        0.0
    };

    let annualized_return = if trading_duration.as_secs() > 0 {
        let years = trading_duration.as_secs_f64() / (365.25 * 24.0 * 3600.0);
        if years > 0.0 {
            ((final_total_value / grid_state.total_capital).powf(1.0 / years) - 1.0) * 100.0
        } else {
            0.0
        }
    } else {
        0.0
    };

    format!(
        "===== 网格交易策略最终报告 =====\n\
        退出原因: {}\n\
        退出时间: {}\n\
        运行时长: {:.2} 小时\n\
        \n\
        === 资金状况 ===\n\
        初始资金: {:.2}\n\
        最终资产: {:.2}\n\
        绝对收益: {:.2}\n\
        投资回报率: {:.2}%\n\
        年化收益率: {:.2}%\n\
        已实现利润: {:.2}\n\
        \n\
        === 持仓状况 ===\n\
        当前价格: {:.4}\n\
        持仓数量: {:.4}\n\
        持仓均价: {:.4}\n\
        持仓价值: {:.2}\n\
        可用资金: {:.2}\n\
        \n\
        === 交易统计 ===\n\
        总交易数: {}\n\
        盈利交易: {}\n\
        亏损交易: {}\n\
        胜率: {:.1}%\n\
        利润因子: {:.2}\n\
        夏普比率: {:.2}\n\
        最大回撤: {:.2}%\n\
        平均盈利: {:.2}\n\
        平均亏损: {:.2}\n\
        最大单笔盈利: {:.2}\n\
        最大单笔亏损: {:.2}\n\
        \n\
        === 风险指标 ===\n\
        最大回撤: {:.2}%\n\
        波动率: {:.2}%\n\
        风险调整收益: {:.2}\n\
        \n\
        === 策略参数 ===\n\
        当前最小网格间距: {:.4}%\n\
        当前最大网格间距: {:.4}%\n\
        当前交易金额: {:.2}\n\
        参数优化次数: {}\n\
        \n\
        ==============================",
        reason.as_str(),
        format!("{:?}", safe_unix_timestamp()),
        trading_duration.as_secs_f64() / 3600.0,
        grid_state.total_capital,
        final_total_value,
        total_return,
        roi,
        annualized_return,
        grid_state.realized_profit,
        current_price,
        grid_state.position_quantity,
        grid_state.position_avg_price,
        grid_state.position_quantity * current_price,
        grid_state.available_funds,
        final_metrics.total_trades,
        final_metrics.winning_trades,
        final_metrics.losing_trades,
        final_metrics.win_rate * 100.0,
        final_metrics.profit_factor,
        final_metrics.sharpe_ratio,
        final_metrics.max_drawdown * 100.0,
        final_metrics.average_win,
        final_metrics.average_loss,
        final_metrics.largest_win,
        final_metrics.largest_loss,
        final_metrics.max_drawdown * 100.0,
        grid_state.historical_volatility * 100.0,
        if final_metrics.max_drawdown > 0.0 {
            roi / (final_metrics.max_drawdown * 100.0)
        } else {
            0.0
        },
        grid_state.dynamic_params.current_min_spacing * 100.0,
        grid_state.dynamic_params.current_max_spacing * 100.0,
        grid_state.dynamic_params.current_trade_amount,
        grid_state.dynamic_params.optimization_count,
    )
}

// 设置信号处理
fn setup_signal_handler() -> (Arc<AtomicBool>, CancellationToken) {
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let cancellation_token = CancellationToken::new();

    let flag_clone = shutdown_flag.clone();
    let token_clone = cancellation_token.clone();

    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            let mut sigint =
                signal(SignalKind::interrupt()).expect("Failed to setup SIGINT handler");
            let mut sigterm =
                signal(SignalKind::terminate()).expect("Failed to setup SIGTERM handler");

            tokio::select! {
                _ = sigint.recv() => {
                    info!("🔔 接收到 SIGINT 信号，开始安全退出...");
                }
                _ = sigterm.recv() => {
                    info!("🔔 接收到 SIGTERM 信号，开始安全退出...");
                }
            }
        }

        #[cfg(windows)]
        {
            use tokio::signal;

            let mut ctrl_c = signal::ctrl_c().expect("Failed to setup Ctrl+C handler");
            ctrl_c.recv().await;
            info!("🔔 接收到 Ctrl+C 信号，开始安全退出...");
        }

        flag_clone.store(true, Ordering::SeqCst);
        token_clone.cancel();
    });

    (shutdown_flag, cancellation_token)
}

// ===== 状态持久化与恢复功能 =====

/// 保存网格状态到文件
fn save_grid_state(grid_state: &GridState, file_path: &str) -> Result<(), GridStrategyError> {
    let serialized = serde_json::to_string_pretty(grid_state)
        .map_err(|e| GridStrategyError::ConfigError(format!("序列化状态失败: {:?}", e)))?;

    std::fs::write(file_path, serialized)
        .map_err(|e| GridStrategyError::ConfigError(format!("写入状态文件失败: {:?}", e)))?;

    info!("✅ 网格状态已保存到: {}", file_path);
    Ok(())
}

/// 从文件加载网格状态
fn load_grid_state(file_path: &str) -> Result<Option<GridState>, GridStrategyError> {
    match std::fs::read_to_string(file_path) {
        Ok(contents) => {
            let grid_state = serde_json::from_str(&contents).map_err(|e| {
                GridStrategyError::ConfigError(format!("解析状态文件失败: {:?}", e))
            })?;

            info!("✅ 成功加载网格状态");
            Ok(Some(grid_state))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("📄 未找到状态文件，将使用默认设置");
            Ok(None)
        }
        Err(e) => Err(GridStrategyError::ConfigError(format!(
            "读取状态文件失败: {:?}",
            e
        ))),
    }
}

/// 保存订单状态到文件
fn save_orders_state(
    active_orders: &[u64],
    buy_orders: &HashMap<u64, OrderInfo>,
    sell_orders: &HashMap<u64, OrderInfo>,
    file_path: &str,
) -> Result<(), GridStrategyError> {
    #[derive(serde::Serialize)]
    struct OrdersState {
        active_orders: Vec<u64>,
        buy_orders: HashMap<u64, OrderInfo>,
        sell_orders: HashMap<u64, OrderInfo>,
        save_time: u64,
    }

    let orders_state = OrdersState {
        active_orders: active_orders.to_vec(),
        buy_orders: buy_orders.clone(),
        sell_orders: sell_orders.clone(),
        save_time: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    };

    let serialized = serde_json::to_string_pretty(&orders_state)
        .map_err(|e| GridStrategyError::ConfigError(format!("序列化订单状态失败: {:?}", e)))?;

    std::fs::write(file_path, serialized)
        .map_err(|e| GridStrategyError::ConfigError(format!("写入订单状态文件失败: {:?}", e)))?;

    info!(
        "✅ 订单状态已保存到: {} (活跃订单: {}, 买单: {}, 卖单: {})",
        file_path,
        active_orders.len(),
        buy_orders.len(),
        sell_orders.len()
    );
    Ok(())
}

/// 从文件加载订单状态
fn load_orders_state(
    file_path: &str,
) -> Result<Option<(Vec<u64>, HashMap<u64, OrderInfo>, HashMap<u64, OrderInfo>)>, GridStrategyError>
{
    #[derive(serde::Deserialize)]
    struct OrdersState {
        active_orders: Vec<u64>,
        buy_orders: HashMap<u64, OrderInfo>,
        sell_orders: HashMap<u64, OrderInfo>,
        save_time: u64,
    }

    match std::fs::read_to_string(file_path) {
        Ok(contents) => {
            let orders_state: OrdersState = serde_json::from_str(&contents).map_err(|e| {
                GridStrategyError::ConfigError(format!("解析订单状态文件失败: {:?}", e))
            })?;

            // 检查状态文件的时效性（超过1小时的状态文件可能已过期）
            let current_time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let state_age = current_time - orders_state.save_time;

            if state_age > 3600 {
                // 1小时
                warn!(
                    "⚠️ 订单状态文件已过期 ({:.1} 小时前)，将忽略",
                    state_age as f64 / 3600.0
                );
                return Ok(None);
            }

            info!(
                "✅ 成功加载订单状态 (活跃订单: {}, 买单: {}, 卖单: {})",
                orders_state.active_orders.len(),
                orders_state.buy_orders.len(),
                orders_state.sell_orders.len()
            );

            Ok(Some((
                orders_state.active_orders,
                orders_state.buy_orders,
                orders_state.sell_orders,
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("📄 未找到订单状态文件，将使用空状态");
            Ok(None)
        }
        Err(e) => Err(GridStrategyError::ConfigError(format!(
            "读取订单状态文件失败: {:?}",
            e
        ))),
    }
}

/// 定期保存状态（在主循环中调用）
fn periodic_state_save(
    grid_state: &GridState,
    active_orders: &[u64],
    buy_orders: &HashMap<u64, OrderInfo>,
    sell_orders: &HashMap<u64, OrderInfo>,
    last_save_time: &mut SystemTime,
    save_interval_seconds: u64,
) -> Result<(), GridStrategyError> {
    let now = SystemTime::now();

    // 检查是否到了保存时间
    if now
        .duration_since(*last_save_time)
        .unwrap_or_default()
        .as_secs()
        >= save_interval_seconds
    {
        // 保存网格状态
        if let Err(e) = save_grid_state(grid_state, "grid_state.json") {
            warn!("⚠️ 保存网格状态失败: {:?}", e);
        }

        // 保存订单状态
        if let Err(e) =
            save_orders_state(active_orders, buy_orders, sell_orders, "orders_state.json")
        {
            warn!("⚠️ 保存订单状态失败: {:?}", e);
        }

        *last_save_time = now;
        info!("💾 定期状态保存完成");
    }

    Ok(())
}

/// 验证加载的状态是否与当前配置兼容
fn validate_loaded_state(
    grid_state: &GridState,
    grid_config: &crate::config::GridConfig,
) -> Result<bool, GridStrategyError> {
    let is_valid = true;
    let mut warnings = Vec::new();

    // 检查总资金是否匹配
    if (grid_state.total_capital - grid_config.total_capital).abs() > 0.01 {
        warnings.push(format!(
            "总资金不匹配: 状态文件={:.2}, 配置文件={:.2}",
            grid_state.total_capital, grid_config.total_capital
        ));
    }

    // 检查动态参数是否在合理范围内
    if grid_state.dynamic_params.current_min_spacing < grid_config.min_grid_spacing * 0.5
        || grid_state.dynamic_params.current_min_spacing > grid_config.max_grid_spacing
    {
        warnings.push(format!(
            "最小网格间距超出范围: {:.4}% (配置范围: {:.4}%-{:.4}%)",
            grid_state.dynamic_params.current_min_spacing * 100.0,
            grid_config.min_grid_spacing * 100.0,
            grid_config.max_grid_spacing * 100.0
        ));
    }

    // 检查交易金额是否合理
    if grid_state.dynamic_params.current_trade_amount > grid_config.total_capital * 0.5 {
        warnings.push(format!(
            "交易金额过大: {:.2} (总资金的{:.1}%)",
            grid_state.dynamic_params.current_trade_amount,
            grid_state.dynamic_params.current_trade_amount / grid_config.total_capital * 100.0
        ));
    }

    // 输出警告信息
    if !warnings.is_empty() {
        warn!("⚠️ 加载的状态存在以下问题:");
        for warning in warnings {
            warn!("   - {}", warning);
        }
        warn!("建议检查状态文件或重新开始");
    }

    Ok(is_valid)
}

/// 创建状态备份
fn backup_state_files() -> Result<(), GridStrategyError> {
    let timestamp = safe_unix_timestamp();

    // 备份网格状态
    if std::path::Path::new("grid_state.json").exists() {
        let backup_name = format!("grid_state_backup_{}.json", timestamp);
        std::fs::copy("grid_state.json", &backup_name)
            .map_err(|e| GridStrategyError::ConfigError(format!("备份网格状态失败: {:?}", e)))?;
        info!("📋 网格状态已备份到: {}", backup_name);
    }

    // 备份订单状态
    if std::path::Path::new("orders_state.json").exists() {
        let backup_name = format!("orders_state_backup_{}.json", timestamp);
        std::fs::copy("orders_state.json", &backup_name)
            .map_err(|e| GridStrategyError::ConfigError(format!("备份订单状态失败: {:?}", e)))?;
        info!("📋 订单状态已备份到: {}", backup_name);
    }

    // 备份动态参数
    if std::path::Path::new("dynamic_grid_params.json").exists() {
        let backup_name = format!("dynamic_grid_params_backup_{}.json", timestamp);
        std::fs::copy("dynamic_grid_params.json", &backup_name)
            .map_err(|e| GridStrategyError::ConfigError(format!("备份动态参数失败: {:?}", e)))?;
        info!("📋 动态参数已备份到: {}", backup_name);
    }

    Ok(())
}

/// 清理过期的备份文件
fn cleanup_old_backups(max_backup_age_days: u64) -> Result<(), GridStrategyError> {
    let current_time = safe_unix_timestamp();
    let max_age_seconds = max_backup_age_days * 24 * 60 * 60;

    let backup_patterns = [
        "grid_state_backup_",
        "orders_state_backup_",
        "dynamic_grid_params_backup_",
    ];

    for pattern in &backup_patterns {
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.flatten() {
                if let Some(filename) = entry.file_name().to_str() {
                    if filename.starts_with(pattern) && filename.ends_with(".json") {
                        // 从文件名提取时间戳
                        if let Some(timestamp_str) = filename
                            .strip_prefix(pattern)
                            .and_then(|s| s.strip_suffix(".json"))
                        {
                            if let Ok(timestamp) = timestamp_str.parse::<u64>() {
                                if current_time - timestamp > max_age_seconds {
                                    if let Err(e) = std::fs::remove_file(entry.path()) {
                                        warn!("⚠️ 删除过期备份文件失败: {} - {:?}", filename, e);
                                    } else {
                                        info!("🗑️ 已删除过期备份文件: {}", filename);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

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
    let recent_win_rate = recent_records.iter().filter(|r| r.profit > 0.0).count() as f64
        / recent_records.len() as f64;

    let avg_profit_per_trade = recent_profit / recent_records.len() as f64;

    info!("📊 网格性能分析:");
    info!("   最近20笔交易利润: {:.2}", recent_profit);
    info!("   胜率: {:.1}%", recent_win_rate * 100.0);
    info!("   平均每笔利润: {:.2}", avg_profit_per_trade);

    // 提供优化建议
    if recent_profit > 0.0 && recent_win_rate > 0.6 {
        info!("💡 优化建议: 表现良好，可考虑:");
        info!(
            "   - 适当增加网格间距({:.3}% -> {:.3}%)以获得更大利润",
            grid_config.min_grid_spacing * 100.0,
            (grid_config.min_grid_spacing * 1.05) * 100.0
        );
        info!(
            "   - 或增加单格交易金额({:.2} -> {:.2})扩大收益",
            grid_config.trade_amount,
            grid_config.trade_amount * 1.1
        );
    } else if recent_profit < 0.0 || recent_win_rate < 0.4 {
        info!("⚠️ 优化建议: 表现不佳，建议:");
        info!(
            "   - 减少网格间距({:.3}% -> {:.3}%)提高成交频率",
            grid_config.min_grid_spacing * 100.0,
            (grid_config.min_grid_spacing * 0.95) * 100.0
        );
        info!(
            "   - 降低单格交易金额({:.2} -> {:.2})减少风险",
            grid_config.trade_amount,
            grid_config.trade_amount * 0.9
        );
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
