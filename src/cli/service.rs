use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager, ServiceStartCtx,
    ServiceStatus, ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};

use super::{ServiceCmd, paths};

mod launchd;
mod systemd;
mod timeout;

use launchd::launchd_plist;
use systemd::systemd_unit;
pub use timeout::timeout_mismatch_notice;
use timeout::{print_timeout_notice, stop_timeout_secs};

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
    use super::timeout::installed_stop_timeout;
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
