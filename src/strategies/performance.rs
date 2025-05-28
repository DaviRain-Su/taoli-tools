#![allow(dead_code)]

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::{SystemTime, UNIX_EPOCH};

/// 性能指标结构体
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PerformanceMetrics {
    pub total_trades: u32,
    pub winning_trades: u32,
    pub losing_trades: u32,
    pub win_rate: f64,
    pub total_profit: f64,
    pub max_drawdown: f64,
    pub sharpe_ratio: f64,
    pub profit_factor: f64,
    pub average_win: f64,
    pub average_loss: f64,
    pub largest_win: f64,
    pub largest_loss: f64,
}

impl PerformanceMetrics {
    /// 创建新的性能指标实例
    pub fn new() -> Self {
        Self {
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
        }
    }

    /// 更新交易统计
    pub fn update_trade(&mut self, profit: f64) {
        self.total_trades += 1;
        self.total_profit += profit;

        if profit > 0.0 {
            self.winning_trades += 1;
            if profit > self.largest_win {
                self.largest_win = profit;
            }
        } else if profit < 0.0 {
            self.losing_trades += 1;
            if profit < self.largest_loss {
                self.largest_loss = profit;
            }
        }

        self.calculate_derived_metrics();
    }

    /// 计算衍生指标
    pub fn calculate_derived_metrics(&mut self) {
        // 计算胜率
        if self.total_trades > 0 {
            self.win_rate = (self.winning_trades as f64) / (self.total_trades as f64) * 100.0;
        }

        // 计算平均盈利和亏损
        if self.winning_trades > 0 {
            self.average_win = self.get_total_wins() / (self.winning_trades as f64);
        }
        if self.losing_trades > 0 {
            self.average_loss = self.get_total_losses() / (self.losing_trades as f64);
        }

        // 计算盈利因子
        if self.get_total_losses().abs() > 0.0 {
            self.profit_factor = self.get_total_wins() / self.get_total_losses().abs();
        }
    }

    /// 获取总盈利
    pub fn get_total_wins(&self) -> f64 {
        if self.winning_trades > 0 && self.average_win > 0.0 {
            self.average_win * (self.winning_trades as f64)
        } else {
            self.total_profit.max(0.0)
        }
    }

    /// 获取总亏损
    pub fn get_total_losses(&self) -> f64 {
        if self.losing_trades > 0 && self.average_loss < 0.0 {
            self.average_loss * (self.losing_trades as f64)
        } else {
            self.total_profit.min(0.0)
        }
    }

    /// 更新最大回撤
    pub fn update_drawdown(&mut self, current_drawdown: f64) {
        if current_drawdown > self.max_drawdown {
            self.max_drawdown = current_drawdown;
        }
    }

    /// 计算夏普比率
    pub fn calculate_sharpe_ratio(&mut self, returns: &[f64], risk_free_rate: f64) {
        if returns.len() < 2 {
            self.sharpe_ratio = 0.0;
            return;
        }

        let mean_return = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns
            .iter()
            .map(|r| (r - mean_return).powi(2))
            .sum::<f64>()
            / (returns.len() - 1) as f64;
        let std_dev = variance.sqrt();

        if std_dev > 0.0 {
            self.sharpe_ratio = (mean_return - risk_free_rate) / std_dev;
        } else {
            self.sharpe_ratio = 0.0;
        }
    }

    /// 获取性能摘要
    pub fn get_summary(&self) -> String {
        format!(
            "性能摘要:\n\
            总交易数: {}\n\
            盈利交易: {} ({:.1}%)\n\
            亏损交易: {} ({:.1}%)\n\
            总盈利: {:.4}\n\
            最大回撤: {:.2}%\n\
            夏普比率: {:.2}\n\
            盈利因子: {:.2}\n\
            平均盈利: {:.4}\n\
            平均亏损: {:.4}\n\
            最大单笔盈利: {:.4}\n\
            最大单笔亏损: {:.4}",
            self.total_trades,
            self.winning_trades,
            self.win_rate,
            self.losing_trades,
            100.0 - self.win_rate,
            self.total_profit,
            self.max_drawdown * 100.0,
            self.sharpe_ratio,
            self.profit_factor,
            self.average_win,
            self.average_loss,
            self.largest_win,
            self.largest_loss
        )
    }

    /// 重置所有指标
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// 检查性能是否良好
    pub fn is_performing_well(&self) -> bool {
        self.win_rate >= 50.0
            && self.profit_factor >= 1.2
            && self.max_drawdown <= 0.2
            && self.total_profit > 0.0
    }

    /// 获取风险评分 (0-100, 100为最高风险)
    pub fn get_risk_score(&self) -> f64 {
        let mut risk_score = 0.0;

        // 回撤风险 (40%权重)
        risk_score += (self.max_drawdown * 100.0).min(100.0) * 0.4;

        // 胜率风险 (30%权重)
        let win_rate_risk = if self.win_rate >= 60.0 {
            0.0
        } else if self.win_rate >= 40.0 {
            (60.0 - self.win_rate) * 2.0
        } else {
            40.0 + (40.0 - self.win_rate)
        };
        risk_score += win_rate_risk * 0.3;

        // 盈利因子风险 (20%权重)
        let profit_factor_risk = if self.profit_factor >= 1.5 {
            0.0
        } else if self.profit_factor >= 1.0 {
            (1.5 - self.profit_factor) * 40.0
        } else {
            60.0 + (1.0 - self.profit_factor) * 40.0
        };
        risk_score += profit_factor_risk * 0.2;

        // 夏普比率风险 (10%权重)
        let sharpe_risk = if self.sharpe_ratio >= 1.0 {
            0.0
        } else if self.sharpe_ratio >= 0.0 {
            (1.0 - self.sharpe_ratio) * 30.0
        } else {
            30.0 + self.sharpe_ratio.abs() * 20.0
        };
        risk_score += sharpe_risk * 0.1;

        risk_score.min(100.0)
    }
}

impl Default for PerformanceMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// 性能记录结构体
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PerformanceRecord {
    #[serde(with = "system_time_serde")]
    pub timestamp: SystemTime,
    pub price: f64,
    pub action: String,
    pub profit: f64,
    pub total_capital: f64,
}

impl PerformanceRecord {
    /// 创建新的性能记录
    pub fn new(price: f64, action: String, profit: f64, total_capital: f64) -> Self {
        Self {
            timestamp: SystemTime::now(),
            price,
            action,
            profit,
            total_capital,
        }
    }

    /// 创建买入记录
    pub fn buy_record(price: f64, quantity: f64, total_capital: f64) -> Self {
        Self::new(
            price,
            format!("买入 {:.4} @ {:.4}", quantity, price),
            0.0, // 买入时利润为0
            total_capital,
        )
    }

    /// 创建卖出记录
    pub fn sell_record(price: f64, quantity: f64, profit: f64, total_capital: f64) -> Self {
        Self::new(
            price,
            format!("卖出 {:.4} @ {:.4}", quantity, price),
            profit,
            total_capital,
        )
    }

    /// 获取记录的年龄（秒）
    pub fn age_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(self.timestamp)
            .unwrap_or_default()
            .as_secs()
    }

    /// 检查记录是否在指定时间内
    pub fn is_within_hours(&self, hours: u64) -> bool {
        self.age_seconds() <= hours * 3600
    }
}

/// 性能快照结构体
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PerformanceSnapshot {
    pub timestamp: u64,
    pub total_capital: f64,
    pub available_funds: f64,
    pub position_quantity: f64,
    pub position_avg_price: f64,
    pub realized_profit: f64,
    pub total_trades: u32,
    pub winning_trades: u32,
    pub win_rate: f64,
    pub max_drawdown: f64,
    pub sharpe_ratio: f64,
    pub profit_factor: f64,
    pub trading_duration_hours: f64,
    pub final_roi: f64,
}

impl PerformanceSnapshot {
    /// 从性能指标和状态创建快照
    pub fn from_metrics(
        metrics: &PerformanceMetrics,
        total_capital: f64,
        available_funds: f64,
        position_quantity: f64,
        position_avg_price: f64,
        realized_profit: f64,
        trading_duration_hours: f64,
        initial_capital: f64,
    ) -> Self {
        let final_roi = if initial_capital > 0.0 {
            ((total_capital - initial_capital) / initial_capital) * 100.0
        } else {
            0.0
        };

        Self {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            total_capital,
            available_funds,
            position_quantity,
            position_avg_price,
            realized_profit,
            total_trades: metrics.total_trades,
            winning_trades: metrics.winning_trades,
            win_rate: metrics.win_rate,
            max_drawdown: metrics.max_drawdown,
            sharpe_ratio: metrics.sharpe_ratio,
            profit_factor: metrics.profit_factor,
            trading_duration_hours,
            final_roi,
        }
    }

    /// 生成快照报告
    pub fn generate_report(&self) -> String {
        format!(
            "性能快照报告 ({})\n\
            ==================\n\
            总资金: {:.4}\n\
            可用资金: {:.4}\n\
            持仓数量: {:.4}\n\
            持仓均价: {:.4}\n\
            已实现利润: {:.4}\n\
            总交易数: {}\n\
            盈利交易: {}\n\
            胜率: {:.1}%\n\
            最大回撤: {:.2}%\n\
            夏普比率: {:.2}\n\
            盈利因子: {:.2}\n\
            交易时长: {:.1}小时\n\
            总收益率: {:.2}%",
            chrono::DateTime::from_timestamp(self.timestamp as i64, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| "未知时间".to_string()),
            self.total_capital,
            self.available_funds,
            self.position_quantity,
            self.position_avg_price,
            self.realized_profit,
            self.total_trades,
            self.winning_trades,
            self.win_rate,
            self.max_drawdown * 100.0,
            self.sharpe_ratio,
            self.profit_factor,
            self.trading_duration_hours,
            self.final_roi
        )
    }
}

/// 性能分析器
#[derive(Debug)]
pub struct PerformanceAnalyzer {
    pub metrics: PerformanceMetrics,
    pub records: Vec<PerformanceRecord>,
    pub snapshots: Vec<PerformanceSnapshot>,
    pub max_records: usize,
    pub max_snapshots: usize,
}

impl PerformanceAnalyzer {
    /// 创建新的性能分析器
    pub fn new(max_records: usize, max_snapshots: usize) -> Self {
        Self {
            metrics: PerformanceMetrics::new(),
            records: Vec::new(),
            snapshots: Vec::new(),
            max_records,
            max_snapshots,
        }
    }

    /// 添加交易记录
    pub fn add_trade_record(&mut self, record: PerformanceRecord) {
        // 更新指标
        self.metrics.update_trade(record.profit);

        // 添加记录
        self.records.push(record);

        // 限制记录数量
        if self.records.len() > self.max_records {
            self.records.remove(0);
        }
    }

    /// 添加性能快照
    pub fn add_snapshot(&mut self, snapshot: PerformanceSnapshot) {
        self.snapshots.push(snapshot);

        // 限制快照数量
        if self.snapshots.len() > self.max_snapshots {
            self.snapshots.remove(0);
        }
    }

    /// 获取最近的记录
    pub fn get_recent_records(&self, hours: u64) -> Vec<&PerformanceRecord> {
        self.records
            .iter()
            .filter(|record| record.is_within_hours(hours))
            .collect()
    }

    /// 计算收益率序列
    pub fn calculate_returns(&self) -> Vec<f64> {
        if self.records.len() < 2 {
            return Vec::new();
        }

        let mut returns = Vec::new();
        for i in 1..self.records.len() {
            let prev_capital = self.records[i - 1].total_capital;
            let curr_capital = self.records[i].total_capital;
            if prev_capital > 0.0 {
                let return_rate = (curr_capital - prev_capital) / prev_capital;
                returns.push(return_rate);
            }
        }
        returns
    }

    /// 更新夏普比率
    pub fn update_sharpe_ratio(&mut self, risk_free_rate: f64) {
        let returns = self.calculate_returns();
        self.metrics
            .calculate_sharpe_ratio(&returns, risk_free_rate);
    }

    /// 生成详细报告
    pub fn generate_detailed_report(&self) -> String {
        let recent_trades = self.get_recent_records(24); // 最近24小时
        let recent_profit: f64 = recent_trades.iter().map(|r| r.profit).sum();

        format!(
            "{}\n\n\
            最近24小时统计:\n\
            交易次数: {}\n\
            净利润: {:.4}\n\n\
            风险评分: {:.1}/100\n\
            性能状态: {}",
            self.metrics.get_summary(),
            recent_trades.len(),
            recent_profit,
            self.metrics.get_risk_score(),
            if self.metrics.is_performing_well() {
                "良好"
            } else {
                "需要关注"
            }
        )
    }

    /// 重置分析器
    pub fn reset(&mut self) {
        self.metrics.reset();
        self.records.clear();
        self.snapshots.clear();
    }
}

impl Default for PerformanceAnalyzer {
    fn default() -> Self {
        Self::new(1000, 100) // 默认保留1000条记录和100个快照
    }
}

/// SystemTime 序列化辅助模块
pub mod system_time_serde {
    use super::*;

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
