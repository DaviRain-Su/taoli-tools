mod config;
mod strategies;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// 配置文件路径
    #[arg(short, long, default_value = "configs/default.toml")]
    config: PathBuf,

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let app_config = config::load_config(&cli.config)?;

    match cli.command {
        Commands::Spot => {
            println!("执行现货套利: 交易所1={}, 交易所2={}, 交易对={}", 
                app_config.spot.exchange1, 
                app_config.spot.exchange2, 
                app_config.spot.symbol
            );
            // TODO: 实现现货套利逻辑
        }
        Commands::Futures => {
            println!("执行期现套利: 现货交易所={}, 期货交易所={}, 交易对={}", 
                app_config.futures.spot_exchange, 
                app_config.futures.futures_exchange, 
                app_config.futures.symbol
            );
            // TODO: 实现期现套利逻辑
        }
        Commands::Triangle => {
            println!("执行三角套利: 交易所={}, 交易对1={}, 交易对2={}, 交易对3={}", 
                app_config.triangle.exchange, 
                app_config.triangle.pair1, 
                app_config.triangle.pair2, 
                app_config.triangle.pair3
            );
            // TODO: 实现三角套利逻辑
        }
        Commands::Grid => {
            strategies::grid::run_grid_strategy().await?;
        }
    }

    Ok(())
}
