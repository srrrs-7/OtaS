//! OtaS のデーモン。
//!
//! 使い方：`otasd --config <設定ファイル>`
//!
//! 設定の例は `config/otasd.example.toml` を参照。Ctrl-C で止まる。
//! デーモンが止まると、サンドボックスで動いているエージェントも道連れで止まる。

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    use otasd::{Config, Daemon};

    let args: Vec<String> = std::env::args().collect();
    let config_path = match args.as_slice() {
        [_, flag, path] if flag == "--config" => std::path::PathBuf::from(path),
        _ => {
            eprintln!("使い方: otasd --config <設定ファイル>");
            return ExitCode::from(2);
        }
    };
    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("otasd: {e}");
            return ExitCode::FAILURE;
        }
    };
    let sandbox = match otasd::sandbox_from_config(&config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("otasd: {e}");
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("otasd: 実行環境を作れません: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = tokio::task::LocalSet::new().block_on(&runtime, async move {
        let socket = config.console_socket();
        let daemon = Daemon::new(config, sandbox)?;
        let listener = daemon.bind()?;
        eprintln!("otasd: {} で待ち受けています", socket.display());
        tokio::select! {
            r = daemon.serve(listener) => r,
            _ = tokio::signal::ctrl_c() => {
                eprintln!("otasd: 止めます");
                Ok(())
            }
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("otasd: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("otasd: 現在は Linux 専用です");
    ExitCode::FAILURE
}
