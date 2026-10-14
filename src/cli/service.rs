use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use noth_core::config::{lookup, masked_raw_table};
use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager, ServiceStartCtx,
    ServiceStatus, ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};

use super::{ServiceCmd, paths};

const LABEL: &str = "dev.noth.noth";
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
const DEFAULT_WORKER_TIMEOUT_SECS: u64 = 600;
/// Worker の待機が `worker.timeout_secs` で終わったあとの後始末の余裕
const STOP_MARGIN_SECS: u64 = 30;
const RESTART_DELAY_SECS: u32 = 5;

/// 定義ファイルに埋め込む値。生成を純粋関数にしてテストするためにまとめる
pub struct UnitSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub home: PathBuf,
    pub path_env: String,
    pub stop_timeout_secs: u64,
    pub logs_dir: PathBuf,
}

/// systemd は `%` を指定子、`$` を変数として展開し、引用符内では `\` と `"` をエスケープする
fn systemd_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// `Environment=` では `$` は展開されない
fn systemd_env(key: &str, value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("Environment=\"{key}={escaped}\"")
}

pub fn systemd_unit(spec: &UnitSpec) -> String {
    let exec = std::iter::once(spec.program.to_string_lossy().into_owned())
        .chain(spec.args.iter().cloned())
        .map(|a| systemd_quote(&a))
        .collect::<Vec<_>>()
        .join(" ");
    let home = spec.home.to_string_lossy();
    format!(
        "[Unit]\n\
         Description=Noth Discord agent\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         WorkingDirectory={wd}\n\
         {env_home}\n\
         {env_path}\n\
         Restart=on-failure\n\
         RestartSec={RESTART_DELAY_SECS}\n\
         TimeoutStopSec={timeout}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        wd = home.replace('%', "%%"),
        env_home = systemd_env("NOTH_HOME", &home),
        env_path = systemd_env("PATH", &spec.path_env),
        timeout = spec.stop_timeout_secs,
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `Disabled` は install 時の `launchctl load` で起動しないための印。service-manager の start が外してから load し直す
pub fn launchd_plist(spec: &UnitSpec) -> String {
    let args = std::iter::once(spec.program.to_string_lossy().into_owned())
        .chain(spec.args.iter().cloned())
        .map(|a| format!("    <string>{}</string>\n", xml_escape(&a)))
        .collect::<String>();
    let home = xml_escape(&spec.home.to_string_lossy());
    let logs = |name: &str| xml_escape(&spec.logs_dir.join(name).to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>WorkingDirectory</key>
  <string>{home}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>NOTH_HOME</key>
    <string>{home}</string>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>Disabled</key>
  <true/>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ExitTimeOut</key>
  <integer>{timeout}</integer>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
</dict>
</plist>
"#,
        path = xml_escape(&spec.path_env),
        timeout = spec.stop_timeout_secs,
        out = logs("service.out.log"),
        err = logs("service.err.log"),
    )
}

fn unit_contents(spec: &UnitSpec) -> String {
    if cfg!(target_os = "macos") {
        launchd_plist(spec)
    } else {
        systemd_unit(spec)
    }
}

fn manager() -> Result<Box<dyn ServiceManager>> {
    let mut m = <dyn ServiceManager>::native()
        .context("この OS ではサービス管理を使えません(launchd / systemd のみ対応)")?;
    m.set_level(ServiceLevel::User)
        .context("ユーザーレベルのサービスを使えません")?;
    if !m.available().unwrap_or(false) {
        bail!("サービスマネージャが利用できません(launchd / systemd --user が動いていません)");
    }
    Ok(m)
}

fn label() -> Result<ServiceLabel> {
    Ok(LABEL.parse()?)
}

/// 停止を待つ時間。Worker の完了待ち(最長 `worker.timeout_secs`)に余裕を足す
fn stop_timeout_secs(cfg: Option<&Path>) -> u64 {
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

fn build_spec(cfg: Option<&Path>) -> Result<UnitSpec> {
    let p = paths(cfg)?;
    let home = std::path::absolute(&p.home)?;
    let mut args = Vec::new();
    if let Some(c) = cfg {
        args.push("-c".to_string());
        args.push(std::path::absolute(c)?.to_string_lossy().into_owned());
    }
    args.push("run".to_string());
    Ok(UnitSpec {
        program: std::env::current_exe().context("実行ファイルのパスを取得できません")?,
        args,
        logs_dir: home.join("logs"),
        home,
        path_env: std::env::var("PATH")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_PATH.to_string()),
        stop_timeout_secs: stop_timeout_secs(cfg),
    })
}

pub async fn run(cfg: Option<&Path>, cmd: ServiceCmd) -> Result<ExitCode> {
    match cmd {
        ServiceCmd::Install => install(cfg)?,
        ServiceCmd::Uninstall => {
            let m = manager()?;
            match current_status(&*m)? {
                ServiceStatus::NotInstalled => {
                    println!("サービスは未登録です");
                    return Ok(ExitCode::SUCCESS);
                }
                // systemd は unit を消しても動いているプロセスを止めないので、先に止める
                ServiceStatus::Running => stop_and_wait(&*m, cfg).await?,
                ServiceStatus::Stopped(_) => {}
            }
            m.uninstall(ServiceUninstallCtx { label: label()? })?;
            println!("サービスの登録を解除しました");
        }
        ServiceCmd::Start => {
            let m = manager()?;
            require_installed(&*m)?;
            m.start(ServiceStartCtx { label: label()? })?;
            println!("サービスを開始しました");
            print_timeout_notice(cfg);
        }
        ServiceCmd::Stop => {
            let m = manager()?;
            match current_status(&*m)? {
                ServiceStatus::NotInstalled => println!("サービスは未登録です"),
                ServiceStatus::Stopped(_) => println!("サービスは既に停止しています"),
                ServiceStatus::Running => {
                    m.stop(ServiceStopCtx { label: label()? })?;
                    println!(
                        "サービスの停止を要求しました(進行中の Worker を待ってから終了します)"
                    );
                }
            }
        }
        ServiceCmd::Restart => restart(cfg).await?,
        ServiceCmd::Status => {
            let status = current_status(&*manager()?)?;
            println!("{}", status_label(&status));
            if !matches!(status, ServiceStatus::NotInstalled) {
                print_timeout_notice(cfg);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn current_status(m: &dyn ServiceManager) -> Result<ServiceStatus> {
    Ok(m.status(ServiceStatusCtx { label: label()? })?)
}

fn require_installed(m: &dyn ServiceManager) -> Result<()> {
    if matches!(current_status(m)?, ServiceStatus::NotInstalled) {
        bail!("サービスは未登録です(`noth service install` を先に実行してください)");
    }
    Ok(())
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

fn print_timeout_notice(cfg: Option<&Path>) {
    if let Some(msg) = timeout_mismatch_notice(cfg) {
        println!("注意: {msg}");
    }
}

fn status_label(status: &ServiceStatus) -> String {
    match status {
        ServiceStatus::NotInstalled => "サービス: 未登録".to_string(),
        ServiceStatus::Running => "サービス: 実行中".to_string(),
        ServiceStatus::Stopped(Some(reason)) => format!("サービス: 停止中({reason})"),
        ServiceStatus::Stopped(None) => "サービス: 停止中".to_string(),
    }
}

fn install(cfg: Option<&Path>) -> Result<()> {
    let spec = build_spec(cfg)?;
    let m = manager()?;
    m.install(ServiceInstallCtx {
        label: label()?,
        program: spec.program.clone(),
        args: spec.args.iter().map(OsString::from).collect(),
        // 停止タイムアウトは ctx で指定できないので、定義ファイルを丸ごと渡す
        contents: Some(unit_contents(&spec)),
        username: None,
        working_directory: Some(spec.home.clone()),
        environment: Some(vec![
            ("NOTH_HOME".into(), spec.home.to_string_lossy().into_owned()),
            ("PATH".into(), spec.path_env.clone()),
        ]),
        autostart: true,
        restart_policy: RestartPolicy::OnFailure {
            delay_secs: Some(RESTART_DELAY_SECS),
            max_retries: None,
            reset_after_secs: None,
        },
    })?;
    println!("サービスを登録しました(ラベル: {LABEL})");
    println!("  実行ファイル: {}", spec.program.display());
    println!("  NOTH_HOME:    {}", spec.home.display());
    println!("  停止タイムアウト: {} 秒", spec.stop_timeout_secs);
    println!("開始するには `noth service start` を実行してください");
    if cfg!(target_os = "linux") {
        println!(
            "ログアウト後も動かすには `loginctl enable-linger $USER` を実行してください(未実行だとログアウトで停止します)"
        );
    }
    Ok(())
}

/// 停止要求のあと、実際に止まるまで待つ。launchd / systemd とも stop は要求を出して戻るため
async fn stop_and_wait(m: &dyn ServiceManager, cfg: Option<&Path>) -> Result<()> {
    m.stop(ServiceStopCtx { label: label()? })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(stop_timeout_secs(cfg));
    while matches!(current_status(m)?, ServiceStatus::Running) {
        if std::time::Instant::now() >= deadline {
            bail!("サービスが停止しませんでした。`noth service status` を確認してください");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(())
}

pub async fn restart(cfg: Option<&Path>) -> Result<()> {
    let m = manager()?;
    require_installed(&*m)?;
    if matches!(current_status(&*m)?, ServiceStatus::Running) {
        stop_and_wait(&*m, cfg).await?;
    }
    m.start(ServiceStartCtx { label: label()? })?;
    println!("サービスを再起動しました");
    print_timeout_notice(cfg);
    Ok(())
}

/// 設定の書き込みは済んでいるので、サービスが無いときはエラーにせず再起動しなかったことだけ伝える
pub async fn restart_if_installed(cfg: Option<&Path>) -> Result<()> {
    let registered = manager()
        .and_then(|m| current_status(&*m))
        .is_ok_and(|s| !matches!(s, ServiceStatus::NotInstalled));
    if registered {
        restart(cfg).await
    } else {
        println!("サービスが未登録のため再起動しませんでした");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> UnitSpec {
        UnitSpec {
            program: PathBuf::from("/opt/noth bin/noth"),
            args: vec!["run".into()],
            home: PathBuf::from("/home/u/.noth"),
            path_env: "/usr/local/bin:/usr/bin".into(),
            stop_timeout_secs: 630,
            logs_dir: PathBuf::from("/home/u/.noth/logs"),
        }
    }

    #[test]
    fn systemd_unit_has_required_directives() {
        let u = systemd_unit(&spec());
        assert!(
            u.contains("ExecStart=\"/opt/noth bin/noth\" \"run\"\n"),
            "{u}"
        );
        assert!(u.contains("Environment=\"NOTH_HOME=/home/u/.noth\"\n"));
        assert!(u.contains("Environment=\"PATH=/usr/local/bin:/usr/bin\"\n"));
        assert!(u.contains("Restart=on-failure\n"));
        assert!(u.contains("TimeoutStopSec=630\n"));
        assert!(u.contains("WantedBy=default.target\n"));
    }

    #[test]
    fn systemd_escapes_specifiers() {
        let mut s = spec();
        s.program = PathBuf::from("/a%b/n$x");
        let u = systemd_unit(&s);
        assert!(u.contains("ExecStart=\"/a%%b/n$$x\" \"run\""), "{u}");
    }

    #[test]
    fn launchd_plist_has_required_keys() {
        let p = launchd_plist(&spec());
        for needle in [
            "<string>dev.noth.noth</string>",
            "<string>/opt/noth bin/noth</string>",
            "<string>run</string>",
            "<key>NOTH_HOME</key>\n    <string>/home/u/.noth</string>",
            "<key>PATH</key>\n    <string>/usr/local/bin:/usr/bin</string>",
            "<key>Disabled</key>\n  <true/>",
            "<key>RunAtLoad</key>\n  <true/>",
            "<key>SuccessfulExit</key>\n    <false/>",
            "<key>ExitTimeOut</key>\n  <integer>630</integer>",
            "<string>/home/u/.noth/logs/service.out.log</string>",
            "<string>/home/u/.noth/logs/service.err.log</string>",
        ] {
            assert!(p.contains(needle), "{needle}\n{p}");
        }
    }

    #[test]
    fn installed_timeout_is_read_back_from_both_formats() {
        assert_eq!(installed_stop_timeout(&systemd_unit(&spec())), Some(630));
        assert_eq!(installed_stop_timeout(&launchd_plist(&spec())), Some(630));
        assert_eq!(installed_stop_timeout("[Service]\n"), None);
    }

    #[test]
    fn launchd_plist_escapes_xml() {
        let mut s = spec();
        s.path_env = "/a&b:/c<d".into();
        let p = launchd_plist(&s);
        assert!(p.contains("/a&amp;b:/c&lt;d"), "{p}");
    }
}
