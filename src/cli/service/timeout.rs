use std::path::{Path, PathBuf};

use noth_core::config::{lookup, masked_raw_table};

use super::super::paths;
use super::{DEFAULT_WORKER_TIMEOUT_SECS, LABEL, STOP_MARGIN_SECS};

/// 停止を待つ時間。Worker の完了待ち(最長 `worker.timeout_secs`)に余裕を足す
pub(super) fn stop_timeout_secs(cfg: Option<&Path>) -> u64 {
    let timeout = paths(cfg)
        .ok()
        .and_then(|p| masked_raw_table(&p).ok())
        .and_then(|t| {
            lookup(&t, "worker.timeout_secs")
                .and_then(|v| v.as_integer())
                .and_then(|v| u64::try_from(v).ok())
        })
        .unwrap_or(DEFAULT_WORKER_TIMEOUT_SECS);
    timeout + STOP_MARGIN_SECS
}

/// 登録済みの定義に埋め込まれた停止タイムアウト(秒)。systemd は `TimeoutStopSec=`、launchd は `ExitTimeOut`
pub fn installed_stop_timeout(unit: &str) -> Option<u64> {
    if let Some(line) = unit.lines().find_map(|l| l.strip_prefix("TimeoutStopSec=")) {
        return line.trim().parse().ok();
    }
    let rest = unit.split("<key>ExitTimeOut</key>").nth(1)?;
    let rest = rest.split_once("<integer>")?.1;
    rest.split_once("</integer>")?.0.trim().parse().ok()
}

fn installed_unit_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?);
    Some(if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    } else {
        home.join(".config/systemd/user/noth-noth.service")
    })
}

/// 停止タイムアウトは install 時に定義へ固定されるため、`worker.timeout_secs` を変えたあとは差が出る
pub fn timeout_mismatch_notice(cfg: Option<&Path>) -> Option<String> {
    let installed = installed_stop_timeout(&std::fs::read_to_string(installed_unit_path()?).ok()?)?;
    let expected = stop_timeout_secs(cfg);
    (installed != expected).then(|| {
        format!(
            "登録済みの停止タイムアウト({installed} 秒)が worker.timeout_secs に合っていません(現在の設定では {expected} 秒)。反映するには `noth service uninstall && noth service install` で登録し直してください"
        )
    })
}

pub(super) fn print_timeout_notice(cfg: Option<&Path>) {
    if let Some(msg) = timeout_mismatch_notice(cfg) {
        println!("注意: {msg}");
    }
}
