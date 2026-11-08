//! サンドボックスの起動役。
//!
//! 使い方：`otas-sandbox-exec <起動計画の JSON>`、または `otas-sandbox-exec --probe-userns`
//! （root なしの方式を使えるか確かめる）
//!
//! 自分自身に制限をかけてから、計画に書かれたプログラムを exec する。
//! 制限の適用か exec に失敗した場合は、終了コード 125 で終わる。
//! 通常は `LinuxSandbox::spawn` から起動され、人が直接使うことはない。

use std::process::ExitCode;

const FAILURE: u8 = 125;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "--probe-userns" {
        return match otas_adapter_linux::probe_userns() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(FAILURE)
            }
        };
    }
    // `--pid1` は、名前空間の中で起動し直された2段目（PID 1）
    let (pid1, json) = match args.as_slice() {
        [flag, json] if flag == "--pid1" => (true, json.clone()),
        [json] => (false, json.clone()),
        _ => {
            eprintln!("otas-sandbox-exec: 起動計画（JSON）を引数で渡してください");
            return ExitCode::from(FAILURE);
        }
    };
    let plan: otas_adapter_linux::LaunchPlan = match serde_json::from_str(&json) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("otas-sandbox-exec: 起動計画を読めません: {e}");
            return ExitCode::from(FAILURE);
        }
    };
    let result = if pid1 {
        otas_adapter_linux::run_pid1(&plan)
    } else {
        otas_adapter_linux::exec_sandboxed(&plan)
    };
    match result {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("otas-sandbox-exec: {e}");
            ExitCode::from(FAILURE)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("otas-sandbox-exec: Linux 専用です");
    ExitCode::from(FAILURE)
}
