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
            if cfg!(target_os = "macos") {
                launchd_stop(cfg).await?;
            }
            m.uninstall(ServiceUninstallCtx { label: label()? })?;
            println!("サービスの登録を解除しました");
        }
        ServiceCmd::Start => {
            let m = manager()?;
            require_installed(&*m)?;
            start_service(&*m)?;
            println!("サービスを開始しました");
            print_timeout_notice(cfg);
        }
        ServiceCmd::Stop => {
            let m = manager()?;
            match current_status(&*m)? {
                ServiceStatus::NotInstalled => println!("サービスは未登録です"),
                ServiceStatus::Stopped(_) => println!("サービスは既に停止しています"),
                ServiceStatus::Running => {
                    stop_service(&*m, cfg).await?;
                    if cfg!(target_os = "macos") {
                        println!("サービスを停止しました");
                    } else {
                        println!(
                            "サービスの停止を要求しました(進行中の Worker を待ってから終了します)"
                        );
                    }
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
    if cfg!(target_os = "macos") {
        return Ok(launchd_status());
    }
    Ok(m.status(ServiceStatusCtx { label: label()? })?)
}

// service-manager の launchd status はドメインなしのラベルで print するため判定できない。
// https://github.com/chipsenkbeil/service-manager-rs/issues/41 が直れば crate の status に戻せる
fn launchd_status() -> ServiceStatus {
    if let Some(out) = launchctl_print() {
        return parse_launchctl_print(&out);
    }
    if launchd_plist_path().is_some_and(|p| p.exists()) {
        ServiceStatus::Stopped(None)
    } else {
        ServiceStatus::NotInstalled
    }
}

fn launchd_plist_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(home).join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

fn launchd_domain() -> String {
    // SAFETY: getuid は常に成功し、副作用がない
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchd_target() -> String {
    format!("{}/{LABEL}", launchd_domain())
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("launchctl")
        .args(args)
        .output()
        .context("launchctl を実行できません")
}

fn launchctl_ok(args: &[&str]) -> Result<()> {
    let out = launchctl(args)?;
    if !out.status.success() {
        bail!(
            "launchctl {} が失敗しました: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn launchctl_print() -> Option<String> {
    let out = launchctl(&["print", &launchd_target()]).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn launchd_start() -> Result<()> {
    if launchctl_print().is_some() {
        return launchctl_ok(&["kickstart", &launchd_target()]);
    }
    let plist = launchd_plist_path().context("HOME を取得できません")?;
    // plist の Disabled=true のままだと bootstrap が I/O エラーになる
    launchctl_ok(&["enable", &launchd_target()])?;
    launchctl_ok(&["bootstrap", &launchd_domain(), &plist.to_string_lossy()])
}

// KeepAlive があるので `launchctl stop` は再起動されてしまう。bootout は終了を待たずに戻るため pid の消滅まで待つ。
// disable は、上書き DB に enabled が残ると install の `launchctl load` で起動してしまうのを防ぐ
async fn launchd_stop(cfg: Option<&Path>) -> Result<()> {
    let target = launchd_target();
    if let Some(printed) = launchctl_print() {
        let pid = parse_launchctl_pid(&printed);
        let out = launchctl(&["bootout", &target])?;
        let already_gone = matches!(out.status.code(), Some(3 | 113));
        if !out.status.success() && !already_gone {
            bail!(
                "launchctl bootout が失敗しました: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if let Some(pid) = pid {
            wait_pid_exit(pid, Duration::from_secs(stop_timeout_secs(cfg))).await?;
        }
    }
    launchctl_ok(&["disable", &target])
}

async fn wait_pid_exit(pid: i32, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    // SAFETY: シグナル 0 は存在確認だけで、プロセスに影響しない
    while unsafe { libc::kill(pid, 0) } == 0 {
        if std::time::Instant::now() >= deadline {
            bail!(
                "サービスが停止しませんでした(pid {pid})。`noth service status` を確認してください"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

fn parse_launchctl_pid(out: &str) -> Option<i32> {
    out.lines()
        .find_map(|l| l.strip_prefix("\tpid = "))
        .and_then(|v| v.trim().parse().ok())
}

// spawn scheduled は KeepAlive による再起動待ちで、止まったわけではない
fn parse_launchctl_print(out: &str) -> ServiceStatus {
    let running = out.lines().any(|l| {
        l.trim()
            .strip_prefix("state = ")
            .is_some_and(|s| s == "running" || s == "spawn scheduled")
    });
    if running {
        ServiceStatus::Running
    } else {
        ServiceStatus::Stopped(None)
    }
}

fn start_service(m: &dyn ServiceManager) -> Result<()> {
    if cfg!(target_os = "macos") {
        return launchd_start();
    }
    Ok(m.start(ServiceStartCtx { label: label()? })?)
}

async fn stop_service(m: &dyn ServiceManager, cfg: Option<&Path>) -> Result<()> {
    if cfg!(target_os = "macos") {
        return launchd_stop(cfg).await;
    }
    Ok(m.stop(ServiceStopCtx { label: label()? })?)
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

/// 停止要求のあと、実際に止まるまで待つ。systemd の stop は要求を出して戻る(macOS は `launchd_stop` が待つ)
async fn stop_and_wait(m: &dyn ServiceManager, cfg: Option<&Path>) -> Result<()> {
    stop_service(m, cfg).await?;
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
    start_service(&*m)?;
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
    fn launchctl_print_state_is_parsed() {
        let running = "dev.noth.noth = {\n\tactive count = 1\n\tstate = running\n\tpid = 123\n}";
        assert!(matches!(
            parse_launchctl_print(running),
            ServiceStatus::Running
        ));
        let waiting = "dev.noth.noth = {\n\tstate = not running\n}";
        assert!(matches!(
            parse_launchctl_print(waiting),
            ServiceStatus::Stopped(None)
        ));
        let respawn = "dev.noth.noth = {\n\tstate = spawn scheduled\n}";
        assert!(matches!(
            parse_launchctl_print(respawn),
            ServiceStatus::Running
        ));
        assert!(matches!(
            parse_launchctl_print(""),
            ServiceStatus::Stopped(None)
        ));
    }

    #[test]
    fn launchctl_print_pid_is_top_level_only() {
        let out =
            "dev.noth.noth = {\n\tstate = running\n\tpid = 123\n\tchild = {\n\t\tpid = 9\n\t}\n}";
        assert_eq!(parse_launchctl_pid(out), Some(123));
        assert_eq!(parse_launchctl_pid("\tstate = spawn scheduled\n"), None);
    }

    #[test]
    fn launchd_plist_escapes_xml() {
        let mut s = spec();
        s.path_env = "/a&b:/c<d".into();
        let p = launchd_plist(&s);
        assert!(p.contains("/a&amp;b:/c&lt;d"), "{p}");
    }
}
