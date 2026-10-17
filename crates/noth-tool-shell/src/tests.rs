use noth_core::types::Origin;

use super::*;

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext {
        run_id: "r".into(),
        origin: Origin::Request,
        work_dir: dir.to_path_buf(),
        meter: None,
        history: None,
        audience: Default::default(),
    }
}

fn shell() -> ShellTool {
    ShellTool {
        workspace: Some(std::env::temp_dir().join("noth-shell-test-workspace")),
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("noth-shell-test-{name}-{}", std::process::id()))
}

#[test]
fn spec_name_is_shell() {
    let spec = shell().spec();
    assert_eq!(spec.name, "shell");
    assert_eq!(spec.input_schema["required"], json!(["command"]));
}

#[tokio::test]
async fn echo_runs_in_work_dir() {
    let dir = scratch("echo");
    let out = shell()
        .call(json!({"command": "echo hello; pwd"}), &ctx(&dir))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("hello"));
    let real = std::fs::canonicalize(&dir).unwrap();
    assert!(out.text.contains(real.to_str().unwrap()), "{}", out.text);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn nonzero_exit_is_error() {
    let dir = scratch("exit");
    let out = shell()
        .call(json!({"command": "echo oops >&2; exit 3"}), &ctx(&dir))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("終了コード: 3"));
    assert!(out.text.contains("oops"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn timeout_kills_process() {
    let dir = scratch("timeout");
    let started = std::time::Instant::now();
    let out = shell()
        .call(
            json!({"command": "sleep 30", "timeout_secs": 1}),
            &ctx(&dir),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("タイムアウト"));
    assert!(started.elapsed() < Duration::from_secs(10));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn missing_command_is_error_output() {
    let dir = scratch("nocmd");
    let out = shell().call(json!({}), &ctx(&dir)).await.unwrap();
    assert!(out.is_error);
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn pid_from(dir: &std::path::Path, file: &str) -> i32 {
    std::fs::read_to_string(dir.join(file))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

async fn assert_dies(pid: i32) {
    for _ in 0..40 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("pid {pid} が残っています");
}

#[tokio::test]
async fn timeout_kills_grandchildren() {
    let dir = scratch("orphan");
    let out = shell()
        .call(
            json!({"command": "sleep 60 & echo $! > pid; sleep 60", "timeout_secs": 1}),
            &ctx(&dir),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert_dies(pid_from(&dir, "pid").await).await;
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn background_start_returns_quickly_and_kills_it() {
    let dir = scratch("bg");
    let started = std::time::Instant::now();
    let out = shell()
        .call(
            json!({"command": "sleep 60 & echo $! > pid; echo started"}),
            &ctx(&dir),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("started"));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_dies(pid_from(&dir, "pid").await).await;
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn large_output_is_bounded() {
    let dir = scratch("big");
    let out = shell()
        .call(
            json!({"command": "yes | head -c 5000000; echo err-line >&2"}),
            &ctx(&dir),
        )
        .await
        .unwrap();
    assert!(!out.is_error);
    assert!(out.text.len() < 100 * 1024, "{}", out.text.len());
    assert!(out.text.contains("省略"));
    assert!(out.text.contains("err-line"));
    assert!(out.text.find("[stderr]").unwrap() < out.text.find("[stdout]").unwrap());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn timeout_returns_partial_output() {
    let dir = scratch("partial");
    let out = shell()
        .call(
            json!({"command": "echo partial-out; echo partial-err >&2; sleep 30", "timeout_secs": 1}),
            &ctx(&dir),
        )
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text.contains("partial-out"));
    assert!(out.text.contains("partial-err"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn workspace_path_is_passed_by_env_and_work_dir_is_created_on_first_use() {
    let dir = scratch("env");
    assert!(!dir.exists());
    let out = shell()
        .call(json!({"command": "echo $NOTH_WORKSPACE"}), &ctx(&dir))
        .await
        .unwrap();
    assert!(
        out.text.contains("noth-shell-test-workspace"),
        "{}",
        out.text
    );
    assert!(dir.is_dir());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn workspace_env_is_not_passed_when_there_is_no_workspace() {
    let dir = scratch("no-ws");
    let tool = ShellTool { workspace: None };
    let out = tool
        .call(
            json!({"command": "echo \"[${NOTH_WORKSPACE-unset}]\""}),
            &ctx(&dir),
        )
        .await
        .unwrap();
    assert!(out.text.contains("[unset]"), "{}", out.text);
    std::fs::remove_dir_all(&dir).ok();
}
