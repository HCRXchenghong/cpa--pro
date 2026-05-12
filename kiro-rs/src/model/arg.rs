use clap::{Parser, Subcommand};

/// Anthropic <-> Kiro API 客户端
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// 配置文件路径
    #[arg(short, long)]
    pub config: Option<String>,

    /// 凭证文件路径
    #[arg(long)]
    pub credentials: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 从官方 Kiro CLI / Amazon Q CLI SQLite 登录库导入凭据
    ImportKiroCli {
        /// SQLite 数据库路径，默认自动查找官方 Kiro CLI / Amazon Q CLI 登录库
        #[arg(long)]
        db: Option<String>,

        /// 覆盖 credentials 文件，而不是合并追加
        #[arg(long)]
        replace: bool,

        /// 为导入的凭据设置优先级
        #[arg(long)]
        priority: Option<u32>,

        /// 覆盖凭据级 region
        #[arg(long)]
        region: Option<String>,

        /// 覆盖凭据级 authRegion
        #[arg(long)]
        auth_region: Option<String>,

        /// 覆盖凭据级 apiRegion
        #[arg(long)]
        api_region: Option<String>,
    },
}
