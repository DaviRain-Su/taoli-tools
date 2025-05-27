use log::{info, warn};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 批处理任务优化器
/// 
/// 该优化器通过分析历史执行时间来动态调整批处理大小，
/// 以达到最佳的执行性能和资源利用率。
#[derive(Debug, Clone)]
pub struct BatchTaskOptimizer {
    /// 最近执行时间的滑动窗口
    last_execution_times: VecDeque<Duration>,
    /// 当前最优批次大小
    optimal_batch_size: usize,
    /// 调整因子（每次调整的幅度）
    adjustment_factor: f64,
    /// 最小批次大小
    min_batch_size: usize,
    /// 最大批次大小
    max_batch_size: usize,
    /// 目标执行时间
    target_execution_time: Duration,
    /// 性能窗口大小（保留多少个历史记录）
    performance_window_size: usize,
    /// 连续调整次数
    consecutive_adjustments: u32,
    /// 上次调整时间
    last_adjustment_time: Instant,
    /// 调整冷却时间
    adjustment_cooldown: Duration,
    /// 性能趋势（正值表示性能改善，负值表示性能下降）
    performance_trend: f64,
}

impl BatchTaskOptimizer {
    /// 创建新的批处理优化器
    /// 
    /// # 参数
    /// * `initial_batch_size` - 初始批次大小
    /// * `target_execution_time` - 目标执行时间
    pub fn new(initial_batch_size: usize, target_execution_time: Duration) -> Self {
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
    /// 
    /// # 参数
    /// * `task_count` - 当前待处理的任务数量
    /// 
    /// # 返回值
    /// 建议的批次大小
    pub fn optimize_batch_size(&mut self, task_count: usize) -> usize {
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
    /// 
    /// # 参数
    /// * `duration` - 本次执行的时间
    pub fn record_execution_time(&mut self, duration: Duration) {
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

        let avg_time = self.calculate_average_execution_time().as_secs_f64();
        let variance = self.last_execution_times
            .iter()
            .map(|t| {
                let diff = t.as_secs_f64() - avg_time;
                diff * diff
            })
            .sum::<f64>() / (self.last_execution_times.len() - 1) as f64;

        variance.sqrt() / avg_time // 变异系数
    }

    /// 更新性能趋势
    fn update_performance_trend(&mut self, current_avg: Duration) {
        if self.last_execution_times.len() < 5 {
            return;
        }

        // 计算最近一半和前一半的平均时间
        let mid = self.last_execution_times.len() / 2;
        let recent_times: Vec<Duration> = self.last_execution_times
            .iter()
            .skip(mid)
            .cloned()
            .collect();
        let earlier_times: Vec<Duration> = self.last_execution_times
            .iter()
            .take(mid)
            .cloned()
            .collect();

        if !recent_times.is_empty() && !earlier_times.is_empty() {
            let recent_avg = recent_times.iter().sum::<Duration>().as_secs_f64() / recent_times.len() as f64;
            let earlier_avg = earlier_times.iter().sum::<Duration>().as_secs_f64() / earlier_times.len() as f64;

            // 计算趋势：负值表示性能改善（时间减少），正值表示性能下降
            self.performance_trend = (recent_avg - earlier_avg) / earlier_avg;
        }
    }

    /// 判断是否应该调整批次大小
    fn should_adjust_batch_size(&self, avg_execution_time: Duration, variance: f64) -> bool {
        let time_diff_ratio = (avg_execution_time.as_secs_f64() - self.target_execution_time.as_secs_f64()).abs() 
            / self.target_execution_time.as_secs_f64();
        
        // 如果时间差异超过20%或方差过大，则需要调整
        time_diff_ratio > 0.2 || variance > 0.3
    }

    /// 计算新的批次大小
    fn calculate_new_batch_size(&self, avg_execution_time: Duration, task_count: usize) -> usize {
        let current_time = avg_execution_time.as_secs_f64();
        let target_time = self.target_execution_time.as_secs_f64();
        
        let mut new_size = self.optimal_batch_size;

        if current_time > target_time * 1.2 {
            // 执行时间过长，减少批次大小
            let reduction_factor = 1.0 - self.adjustment_factor;
            new_size = ((self.optimal_batch_size as f64) * reduction_factor) as usize;
        } else if current_time < target_time * 0.8 {
            // 执行时间过短，增加批次大小
            let increase_factor = 1.0 + self.adjustment_factor;
            new_size = ((self.optimal_batch_size as f64) * increase_factor) as usize;
        }

        // 考虑性能趋势进行微调
        if self.performance_trend > 0.1 {
            // 性能下降，保守调整
            new_size = (new_size as f64 * 0.95) as usize;
        } else if self.performance_trend < -0.1 {
            // 性能改善，可以更积极调整
            new_size = (new_size as f64 * 1.05) as usize;
        }

        // 确保在合理范围内
        new_size = new_size
            .max(self.min_batch_size)
            .min(self.max_batch_size)
            .min(task_count);

        new_size
    }



    /// 获取性能报告
    pub fn get_performance_report(&self) -> String {
        if self.last_execution_times.is_empty() {
            return "暂无性能数据".to_string();
        }

        let avg_time = self.calculate_average_execution_time();
        let variance = self.calculate_performance_variance();
        let efficiency = if avg_time <= self.target_execution_time {
            100.0
        } else {
            (self.target_execution_time.as_secs_f64() / avg_time.as_secs_f64()) * 100.0
        };

        format!(
            "批处理优化器性能报告:\n\
            ========================\n\
            当前批次大小: {}\n\
            目标执行时间: {:.2}秒\n\
            平均执行时间: {:.2}秒\n\
            性能方差: {:.4}\n\
            执行效率: {:.1}%\n\
            性能趋势: {}\n\
            连续调整次数: {}\n\
            历史记录数: {}\n\
            调整因子: {:.1}%\n\
            批次范围: {}-{}\n\
            冷却时间: {}秒",
            self.optimal_batch_size,
            self.target_execution_time.as_secs_f64(),
            avg_time.as_secs_f64(),
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
            self.last_execution_times.len(),
            self.adjustment_factor * 100.0,
            self.min_batch_size,
            self.max_batch_size,
            self.adjustment_cooldown.as_secs()
        )
    }

    /// 重置优化器状态
    pub fn reset(&mut self) {
        self.last_execution_times.clear();
        self.consecutive_adjustments = 0;
        self.last_adjustment_time = Instant::now();
        self.adjustment_cooldown = Duration::from_secs(30);
        self.performance_trend = 0.0;
        info!("🔄 批处理优化器已重置");
    }

    /// 设置目标执行时间
    pub fn set_target_execution_time(&mut self, target: Duration) {
        self.target_execution_time = target;
        info!("🎯 目标执行时间已更新为: {:.2}秒", target.as_secs_f64());
    }

    /// 设置批次大小范围
    pub fn set_batch_size_range(&mut self, min_size: usize, max_size: usize) {
        if min_size > 0 && max_size >= min_size {
            self.min_batch_size = min_size;
            self.max_batch_size = max_size;
            
            // 确保当前批次大小在新范围内
            self.optimal_batch_size = self.optimal_batch_size
                .max(min_size)
                .min(max_size);
                
            info!("📏 批次大小范围已更新为: {}-{}", min_size, max_size);
        } else {
            warn!("⚠️ 无效的批次大小范围: {}-{}", min_size, max_size);
        }
    }

    /// 获取当前最优批次大小
    pub fn get_optimal_batch_size(&self) -> usize {
        self.optimal_batch_size
    }

    /// 获取平均执行时间
    pub fn get_average_execution_time(&self) -> Duration {
        self.calculate_average_execution_time()
    }

    /// 获取性能趋势
    pub fn get_performance_trend(&self) -> f64 {
        self.performance_trend
    }

    /// 检查是否需要调整
    pub fn needs_adjustment(&self) -> bool {
        if self.last_execution_times.len() < 3 {
            return false;
        }

        let avg_time = self.calculate_average_execution_time();
        let variance = self.calculate_performance_variance();
        self.should_adjust_batch_size(avg_time, variance)
    }

    /// 强制调整批次大小
    pub fn force_adjust_batch_size(&mut self, new_size: usize) {
        if new_size >= self.min_batch_size && new_size <= self.max_batch_size {
            let old_size = self.optimal_batch_size;
            self.optimal_batch_size = new_size;
            self.last_adjustment_time = Instant::now();
            info!("🔧 强制调整批次大小: {} -> {}", old_size, new_size);
        } else {
            warn!("⚠️ 强制调整失败，批次大小超出范围: {}", new_size);
        }
    }

    /// 获取调整建议
    pub fn get_adjustment_suggestion(&self) -> Option<String> {
        if !self.needs_adjustment() {
            return None;
        }

        let avg_time = self.calculate_average_execution_time();
        let target_time = self.target_execution_time;

        let avg_time_secs = avg_time.as_secs_f64();
        let target_time_secs = target_time.as_secs_f64();

        if avg_time_secs > target_time_secs * 1.2 {
            Some(format!(
                "建议减少批次大小，当前执行时间({:.2}秒)超出目标时间({:.2}秒)20%以上",
                avg_time_secs,
                target_time_secs
            ))
        } else if avg_time_secs < target_time_secs * 0.8 {
            Some(format!(
                "建议增加批次大小，当前执行时间({:.2}秒)低于目标时间({:.2}秒)20%以上",
                avg_time_secs,
                target_time_secs
            ))
        } else {
            Some("性能方差较大，建议观察执行稳定性".to_string())
        }
    }

    /// 获取目标执行时间
    pub fn get_target_execution_time(&self) -> Duration {
        self.target_execution_time
    }

    /// 获取历史记录数量
    pub fn get_execution_history_count(&self) -> usize {
        self.last_execution_times.len()
    }

    /// 获取批次大小范围
    pub fn get_batch_size_range(&self) -> (usize, usize) {
        (self.min_batch_size, self.max_batch_size)
    }
}

impl Default for BatchTaskOptimizer {
    fn default() -> Self {
        Self::new(10, Duration::from_secs(5))
    }
} 