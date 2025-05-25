mod config;
mod strategies;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// 配置文件路径 (可选，默认使用当前目录下的config.toml)
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 现货套利
    Spot,
    /// 期现套利
    Futures,
    /// 三角套利
    Triangle,
    /// 网格交易
    Grid,
    /// 复制默认配置文件到当前目录
    InitConfig,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(|| PathBuf::from("config.toml"));
    let app_config = if matches!(cli.command, Commands::InitConfig) {
        None
    } else {
        Some(config::load_config(&config_path)?)
    };

    match cli.command {
        Commands::Spot => {
            let config = app_config.unwrap();
            println!("执行现货套利: 交易所1={}, 交易所2={}, 交易对={}", 
                config.spot.exchange1, 
                config.spot.exchange2, 
                config.spot.symbol
            );
            // TODO: 实现现货套利逻辑
        }
        Commands::Futures => {
            let config = app_config.unwrap();
            println!("执行期现套利: 现货交易所={}, 期货交易所={}, 交易对={}", 
                config.futures.spot_exchange, 
                config.futures.futures_exchange, 
                config.futures.symbol
            );
            // TODO: 实现期现套利逻辑
        }
        Commands::Triangle => {
            let config = app_config.unwrap();
            println!("执行三角套利: 交易所={}, 交易对1={}, 交易对2={}, 交易对3={}", 
                config.triangle.exchange, 
                config.triangle.pair1, 
                config.triangle.pair2, 
                config.triangle.pair3
            );
            // TODO: 实现三角套利逻辑
        }
        Commands::Grid => {
            let _config = app_config.unwrap();
            strategies::grid::run_grid_strategy().await?;
        }
        Commands::InitConfig => {
            use std::fs;
            let default_config_path = PathBuf::from("configs/default.toml");
            let target_config_path = PathBuf::from("config.toml");
            if target_config_path.exists() {
                println!("配置文件已存在: {}", target_config_path.display());
            } else {
                fs::copy(&default_config_path, &target_config_path)?;
                println!("已复制默认配置文件到: {}", target_config_path.display());
            }
        }
    }

    Ok(())
}
