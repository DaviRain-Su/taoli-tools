pub mod grid;
pub mod error;

// 重新导出常用的错误类型
pub use error::{GridStrategyError, RetryStrategy, ErrorStatistics};
