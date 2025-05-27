pub mod grid;
pub mod error;
pub mod performance;

// 重新导出常用的错误类型
pub use error::{GridStrategyError, RetryStrategy, ErrorStatistics};

// 重新导出常用的性能类型
pub use performance::{
    PerformanceMetrics, PerformanceRecord, PerformanceSnapshot, PerformanceAnalyzer
};
