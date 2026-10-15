use super::{LABEL, UnitSpec};

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `Disabled` は install 時の `launchctl load` で起動しないための印。service-manager の start が外してから load し直す
pub(super) fn launchd_plist(spec: &UnitSpec) -> String {
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
