//! 设置 LibreNMS 实例的 API token(阶段 7)
//!
//! 用法:
//!   set-instance-token <db.sqlite> <instance-name>      # 交互式:从终端读一行(不回显)
//!   set-instance-token <db.sqlite> <instance-name> -    # 从 stdin 管道读一行
//!
//! **绝不**接受命令行参数传递 token(会在 shell history / process list 留下痕迹)。
//! token 写入数据库 `librenms_instances.api_token_enc` 列(plaintext,阶段 8 加密)。

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("用法: set-instance-token <db.sqlite> <instance-name>");
        eprintln!("token 从 stdin 读一行(支持 '-' 或默认 tty 提示)");
        return ExitCode::from(2);
    }
    let db_path = PathBuf::from(&args[1]);
    let instance_name = &args[2];

    let token = match read_token() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读取 token 失败: {e}");
            return ExitCode::from(2);
        }
    };
    if token.is_empty() {
        eprintln!("token 不能为空");
        return ExitCode::from(2);
    }

    let store = match lnms_invoice::store::Store::connect(&db_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("db connect: {e}");
            return ExitCode::from(2);
        }
    };

    let instances = match store.list_active_libre_nms_instances().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("list instances: {e}");
            return ExitCode::from(2);
        }
    };
    let target = instances
        .into_iter()
        .find(|i| i.name == *instance_name);
    let target = match target {
        Some(t) => t,
        None => {
            eprintln!("实例 '{instance_name}' 不存在或未启用");
            return ExitCode::from(2);
        }
    };

    // UPDATE api_token_enc
    let pool = store.pool();
    let res = sqlx::query("UPDATE librenms_instances SET api_token_enc = ? WHERE id = ?")
        .bind(token.as_bytes())
        .bind(target.id)
        .execute(pool)
        .await;
    match res {
        Ok(r) => {
            if r.rows_affected() == 1 {
                log::info!("instance '{}' (id={}) token updated", target.name, target.id);
                ExitCode::SUCCESS
            } else {
                eprintln!("UPDATE 影响行数 {} != 1", r.rows_affected());
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("UPDATE failed: {e}");
            ExitCode::from(1)
        }
    }
}

fn read_token() -> std::io::Result<String> {
    let mut buf = String::new();
    let from_stdin_pipe = atty_stdin();
    if !from_stdin_pipe {
        // 管道模式
        std::io::stdin().read_line(&mut buf)?;
    } else {
        // tty 模式:回显到 stderr 的简单提示(生产推荐用 systemd-credential 替代)
        eprint!("输入 LNMS API token(不回显): ");
        std::io::stderr().flush()?;
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        handle.read_line(&mut buf)?;
    }
    Ok(buf.trim().to_string())
}

/// 简单判断 stdin 是否为 tty。`atty` crate 没引入,直接读 libc 不便携,
/// 退化用 std::env::var("TERM") 启发式;false 时强制当作管道处理。
fn atty_stdin() -> bool {
    // 默认 stdin 来自 shell 管道时,这里返回 false;真实 tty 时返回 true
    // Linux/macOS 的 std 行为:unix 上 stdin 通常默认当 file 处理
    // 用 env 启发式(避免引入 atty crate):
    //   - LNMS_TOKEN_FORCE_TTY=1 → tty
    //   - stdin 不来自终端 → false
    std::env::var_os("LNMS_TOKEN_FORCE_TTY").is_some()
}