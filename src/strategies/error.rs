use thiserror::Error;

/// 网格交易策略错误类型
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

impl GridStrategyError {
    /// 创建配置错误
    pub fn config_error(msg: impl Into<String>) -> Self {
        Self::ConfigError(msg.into())
    }

    /// 创建钱包错误
    pub fn wallet_error(msg: impl Into<String>) -> Self {
        Self::WalletError(msg.into())
    }

    /// 创建客户端错误
    pub fn client_error(msg: impl Into<String>) -> Self {
        Self::ClientError(msg.into())
    }

    /// 创建订单错误
    pub fn order_error(msg: impl Into<String>) -> Self {
        Self::OrderError(msg.into())
    }

    /// 创建订阅错误
    pub fn subscription_error(msg: impl Into<String>) -> Self {
        Self::SubscriptionError(msg.into())
    }

    /// 创建价格解析错误
    pub fn price_parse_error(msg: impl Into<String>) -> Self {
        Self::PriceParseError(msg.into())
    }

    /// 创建数量解析错误
    pub fn quantity_parse_error(msg: impl Into<String>) -> Self {
        Self::QuantityParseError(msg.into())
    }

    /// 创建风险控制错误
    pub fn risk_control_triggered(msg: impl Into<String>) -> Self {
        Self::RiskControlTriggered(msg.into())
    }

    /// 创建市场分析错误
    pub fn market_analysis_error(msg: impl Into<String>) -> Self {
        Self::MarketAnalysisError(msg.into())
    }

    /// 创建资金分配错误
    pub fn fund_allocation_error(msg: impl Into<String>) -> Self {
        Self::FundAllocationError(msg.into())
    }

    /// 创建网格重平衡错误
    pub fn rebalance_error(msg: impl Into<String>) -> Self {
        Self::RebalanceError(msg.into())
    }

    /// 创建止损执行错误
    pub fn stop_loss_error(msg: impl Into<String>) -> Self {
        Self::StopLossError(msg.into())
    }

    /// 创建保证金不足错误
    pub fn margin_insufficient(msg: impl Into<String>) -> Self {
        Self::MarginInsufficient(msg.into())
    }

    /// 创建网络连接错误
    pub fn network_error(msg: impl Into<String>) -> Self {
        Self::NetworkError(msg.into())
    }

    /// 判断是否为致命错误（需要停止交易）
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::WalletError(_) 
            | Self::ClientError(_) 
            | Self::MarginInsufficient(_)
            | Self::RiskControlTriggered(_)
        )
    }

    /// 判断是否为网络相关错误
    pub fn is_network_error(&self) -> bool {
        matches!(
            self,
            Self::NetworkError(_) 
            | Self::SubscriptionError(_)
            | Self::ClientError(_)
        )
    }

    /// 判断是否为订单相关错误
    pub fn is_order_error(&self) -> bool {
        matches!(
            self,
            Self::OrderError(_) 
            | Self::PriceParseError(_)
            | Self::QuantityParseError(_)
        )
    }

    /// 判断是否为配置相关错误
    pub fn is_config_error(&self) -> bool {
        matches!(
            self,
            Self::ConfigError(_) 
            | Self::FundAllocationError(_)
        )
    }

    /// 获取错误的严重程度等级 (1-5, 5最严重)
    pub fn severity_level(&self) -> u8 {
        match self {
            Self::ConfigError(_) => 5,
            Self::WalletError(_) => 5,
            Self::ClientError(_) => 4,
            Self::MarginInsufficient(_) => 5,
            Self::RiskControlTriggered(_) => 4,
            Self::StopLossError(_) => 3,
            Self::OrderError(_) => 2,
            Self::NetworkError(_) => 3,
            Self::SubscriptionError(_) => 3,
            Self::PriceParseError(_) => 2,
            Self::QuantityParseError(_) => 2,
            Self::MarketAnalysisError(_) => 2,
            Self::FundAllocationError(_) => 3,
            Self::RebalanceError(_) => 2,
        }
    }

    /// 获取错误类型的字符串表示
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::ConfigError(_) => "配置错误",
            Self::WalletError(_) => "钱包错误",
            Self::ClientError(_) => "客户端错误",
            Self::OrderError(_) => "订单错误",
            Self::SubscriptionError(_) => "订阅错误",
            Self::PriceParseError(_) => "价格解析错误",
            Self::QuantityParseError(_) => "数量解析错误",
            Self::RiskControlTriggered(_) => "风险控制",
            Self::MarketAnalysisError(_) => "市场分析错误",
            Self::FundAllocationError(_) => "资金分配错误",
            Self::RebalanceError(_) => "重平衡错误",
            Self::StopLossError(_) => "止损错误",
            Self::MarginInsufficient(_) => "保证金不足",
            Self::NetworkError(_) => "网络错误",
        }
    }

    /// 获取建议的重试策略
    pub fn retry_strategy(&self) -> RetryStrategy {
        match self {
            Self::NetworkError(_) | Self::SubscriptionError(_) => RetryStrategy::ExponentialBackoff,
            Self::OrderError(_) => RetryStrategy::LinearBackoff,
            Self::MarketAnalysisError(_) => RetryStrategy::Immediate,
            Self::PriceParseError(_) | Self::QuantityParseError(_) => RetryStrategy::NoRetry,
            _ => RetryStrategy::NoRetry,
        }
    }
}

/// 重试策略
#[derive(Debug, Clone, PartialEq)]
pub enum RetryStrategy {
    /// 不重试
    NoRetry,
    /// 立即重试
    Immediate,
    /// 线性退避重试
    LinearBackoff,
    /// 指数退避重试
    ExponentialBackoff,
}

impl RetryStrategy {
    /// 计算重试延迟（毫秒）
    pub fn calculate_delay(&self, attempt: u32) -> u64 {
        match self {
            Self::NoRetry => 0,
            Self::Immediate => 0,
            Self::LinearBackoff => (attempt as u64) * 1000, // 1秒, 2秒, 3秒...
            Self::ExponentialBackoff => {
                let base_delay = 1000; // 1秒
                let max_delay = 30000; // 最大30秒
                let delay = base_delay * 2_u64.pow(attempt.min(5));
                delay.min(max_delay)
            }
        }
    }

    /// 获取最大重试次数
    pub fn max_retries(&self) -> u32 {
        match self {
            Self::NoRetry => 0,
            Self::Immediate => 3,
            Self::LinearBackoff => 5,
            Self::ExponentialBackoff => 10,
        }
    }
}

/// 错误统计信息
#[derive(Debug, Default)]
pub struct ErrorStatistics {
    pub total_errors: u64,
    pub config_errors: u64,
    pub wallet_errors: u64,
    pub client_errors: u64,
    pub order_errors: u64,
    pub subscription_errors: u64,
    pub price_parse_errors: u64,
    pub quantity_parse_errors: u64,
    pub risk_control_triggered: u64,
    pub market_analysis_errors: u64,
    pub fund_allocation_errors: u64,
    pub rebalance_errors: u64,
    pub stop_loss_errors: u64,
    pub margin_insufficient: u64,
    pub network_errors: u64,
}

impl ErrorStatistics {
    /// 记录错误
    pub fn record_error(&mut self, error: &GridStrategyError) {
        self.total_errors += 1;
        match error {
            GridStrategyError::ConfigError(_) => self.config_errors += 1,
            GridStrategyError::WalletError(_) => self.wallet_errors += 1,
            GridStrategyError::ClientError(_) => self.client_errors += 1,
            GridStrategyError::OrderError(_) => self.order_errors += 1,
            GridStrategyError::SubscriptionError(_) => self.subscription_errors += 1,
            GridStrategyError::PriceParseError(_) => self.price_parse_errors += 1,
            GridStrategyError::QuantityParseError(_) => self.quantity_parse_errors += 1,
            GridStrategyError::RiskControlTriggered(_) => self.risk_control_triggered += 1,
            GridStrategyError::MarketAnalysisError(_) => self.market_analysis_errors += 1,
            GridStrategyError::FundAllocationError(_) => self.fund_allocation_errors += 1,
            GridStrategyError::RebalanceError(_) => self.rebalance_errors += 1,
            GridStrategyError::StopLossError(_) => self.stop_loss_errors += 1,
            GridStrategyError::MarginInsufficient(_) => self.margin_insufficient += 1,
            GridStrategyError::NetworkError(_) => self.network_errors += 1,
        }
    }

    /// 获取错误率最高的类型
    pub fn most_frequent_error_type(&self) -> Option<&'static str> {
        let errors = [
            (self.config_errors, "配置错误"),
            (self.wallet_errors, "钱包错误"),
            (self.client_errors, "客户端错误"),
            (self.order_errors, "订单错误"),
            (self.subscription_errors, "订阅错误"),
            (self.price_parse_errors, "价格解析错误"),
            (self.quantity_parse_errors, "数量解析错误"),
            (self.risk_control_triggered, "风险控制"),
            (self.market_analysis_errors, "市场分析错误"),
            (self.fund_allocation_errors, "资金分配错误"),
            (self.rebalance_errors, "重平衡错误"),
            (self.stop_loss_errors, "止损错误"),
            (self.margin_insufficient, "保证金不足"),
            (self.network_errors, "网络错误"),
        ];

        errors.iter()
            .max_by_key(|(count, _)| *count)
            .filter(|(count, _)| *count > 0)
            .map(|(_, name)| *name)
    }

    /// 重置统计
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 生成错误报告
    pub fn generate_report(&self) -> String {
        format!(
            "错误统计报告:\n\
            总错误数: {}\n\
            配置错误: {}\n\
            钱包错误: {}\n\
            客户端错误: {}\n\
            订单错误: {}\n\
            订阅错误: {}\n\
            价格解析错误: {}\n\
            数量解析错误: {}\n\
            风险控制触发: {}\n\
            市场分析错误: {}\n\
            资金分配错误: {}\n\
            重平衡错误: {}\n\
            止损错误: {}\n\
            保证金不足: {}\n\
            网络错误: {}",
            self.total_errors,
            self.config_errors,
            self.wallet_errors,
            self.client_errors,
            self.order_errors,
            self.subscription_errors,
            self.price_parse_errors,
            self.quantity_parse_errors,
            self.risk_control_triggered,
            self.market_analysis_errors,
            self.fund_allocation_errors,
            self.rebalance_errors,
            self.stop_loss_errors,
            self.margin_insufficient,
            self.network_errors
        )
    }
} 