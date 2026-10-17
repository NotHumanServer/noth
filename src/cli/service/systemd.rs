use super::{RESTART_DELAY_SECS, UnitSpec};

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

pub(super) fn systemd_unit(spec: &UnitSpec) -> String {
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
