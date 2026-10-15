use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result};

/// `$NOTH_HOME` 配下のパス。`-c` 指定時はその親ディレクトリを home とみなす
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub config: PathBuf,
    pub env: PathBuf,
}

impl Paths {
    /// `config_override` > `NOTH_HOME` > `~/.noth`
    pub fn resolve(config_override: Option<&Path>) -> Result<Self> {
        if let Some(config) = config_override {
            // 相対パス直指定(`config.toml`)だと parent が空文字になる
            let home = match config.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::from("."),
            };
            return Ok(Self {
                env: home.join(".env"),
                config: config.to_path_buf(),
                home,
            });
        }
        let home = match std::env::var_os("NOTH_HOME").filter(|v| !v.is_empty()) {
            Some(h) => PathBuf::from(h),
            None => {
                let user_home = std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        Error::Config(
                            "NOTH_HOME も HOME も未設定のため設定ディレクトリを決められない".into(),
                        )
                    })?;
                PathBuf::from(user_home).join(".noth")
            }
        };
        Ok(Self {
            config: home.join("config.toml"),
            env: home.join(".env"),
            home,
        })
    }
    pub fn profile(&self, name: &str) -> PathBuf {
        self.home.join("profiles").join(format!("{name}.md"))
    }
    pub fn database(&self) -> PathBuf {
        self.home.join("data").join("noth.sqlite")
    }
    /// 同じ DB に2つの `noth run` が書き込むと、発言の重複や同じ発言への2重返信が起きる
    pub fn run_lock(&self) -> PathBuf {
        self.home.join("data").join("noth.lock")
    }
    pub fn work_dir(&self, run_id: &str) -> PathBuf {
        self.home.join("work").join(run_id)
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
    /// Codex のトークンは更新のたびに書き換わるので、`.env` ではなくここに置く(security.md §5)
    pub fn codex_auth(&self) -> PathBuf {
        self.home.join("data").join("codex-auth.json")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModulesSection {
    pub enabled: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileSection {
    #[serde(default = "default_profile_name")]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContextSection {
    #[serde(default = "default_hard_limit")]
    pub prompt_hard_limit_tokens: u64,
    #[serde(default = "default_retry_max")]
    pub retry_max: u32,
    /// 本線の 1 応答の max_tokens
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Thresholds {
    #[serde(default = "d05")]
    pub is_noise_or_meme: f32,
    #[serde(default = "d06")]
    pub is_topic_shift: f32,
    #[serde(default = "d05")]
    pub relevant_chunk: f32,
    #[serde(default = "d015")]
    pub min_confidence: f32,
    #[serde(default = "d025")]
    pub name_call_reply: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouterSection {
    #[serde(default = "default_recent")]
    pub recent_messages: usize,
    #[serde(default = "default_state_max_chars")]
    pub state_max_chars: usize,
    #[serde(default = "default_candidate_chunks")]
    pub candidate_chunks: usize,
    /// 本文で呼ばれたら min_confidence を外して router の判定どおりに動く。必ず応答にしないのは、開発の話題で名前が出るたびに割り込まないため
    #[serde(default)]
    pub call_names: Vec<String>,
    #[serde(default = "default_conversation_window_secs")]
    pub conversation_window_secs: u64,
    #[serde(default)]
    pub thresholds: Thresholds,
}

/// `[worker]`。Core(同時実行数・timeout)と worker module(反復・出力上限)の両方が読む
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerSettings {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// キュー待ちの時間を含む。Ack は投稿済みなので、利用者が待つ時間の上限として扱う
    #[serde(default = "default_worker_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_tool_output_max_chars")]
    pub tool_output_max_chars: usize,
    #[serde(default = "default_approval")]
    pub approval: String,
    /// Ack を編集して tool の使用状況を見せるか
    #[serde(default = "default_show_progress")]
    pub show_progress: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompactorSettings {
    /// 人間の最終発言からの経過。Noth 自身の投稿ではアイドルを解除しない
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u64,
    #[serde(default = "default_keep_recent_turns")]
    pub keep_recent_turns: usize,
    #[serde(default = "default_digest_max_chars")]
    pub digest_max_chars: usize,
}

fn default_max_concurrent() -> usize {
    2
}
fn default_worker_timeout_secs() -> u64 {
    600
}
fn default_max_iterations() -> u32 {
    25
}
fn default_tool_output_max_chars() -> usize {
    20_000
}
fn default_show_progress() -> bool {
    true
}
fn default_approval() -> String {
    "auto".into()
}
fn default_idle_minutes() -> u64 {
    10
}
fn default_keep_recent_turns() -> usize {
    6
}
fn default_digest_max_chars() -> usize {
    12_000
}

/// 解決済みの設定。module 固有のセクションは `module_section` で生の TOML として渡す
#[derive(Clone)]
pub struct Config {
    pub paths: Paths,
    pub modules: ModulesSection,
    pub profile: ProfileSection,
    pub context: ContextSection,
    pub router: RouterSection,
    /// `${VAR}` 解決後の全体。無効 module のセクションは未解決のまま
    pub table: toml::Table,
    /// `.env` の中身。プロセス環境変数には入れない
    pub env: BTreeMap<String, String>,
}

// table と env は解決済みのシークレットを含むので、ログやパニック出力に出さない
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("paths", &self.paths)
            .field("modules", &self.modules)
            .field("profile", &self.profile)
            .field("context", &self.context)
            .field("router", &self.router)
            .field("table", &"<redacted>")
            .field("env", &"<redacted>")
            .finish()
    }
}

impl Config {
    pub fn load(paths: Paths) -> Result<Self> {
        let raw = read_raw(&paths)?;
        let env = read_env(&paths.env)?;

        let modules: ModulesSection = section(&raw, "modules")?
            .ok_or_else(|| Error::Config("[modules] enabled が無い".into()))?;
        let mut missing = Vec::new();
        let table = resolve_referenced(&raw, &modules.enabled, &env, &mut missing);
        if !missing.is_empty() {
            return Err(Error::Config(missing.join("\n")));
        }

        let profile = section(&table, "profile")?.unwrap_or_else(|| empty_section("profile"));
        let context = section(&table, "context")?.unwrap_or_else(|| empty_section("context"));
        let router = section(&table, "router")?.unwrap_or_else(|| empty_section("router"));

        Ok(Self {
            paths,
            modules,
            profile,
            context,
            router,
            table,
            env,
        })
    }

    pub fn is_enabled(&self, module: &str) -> bool {
        self.modules.enabled.iter().any(|m| m == module)
    }

    pub fn module_section(&self, module: &str) -> toml::Value {
        let found = match module.split_once('-') {
            Some((kind @ ("channel" | "router" | "provider" | "tool"), name)) => {
                self.table.get(kind).and_then(|v| v.get(name))
            }
            _ => self.table.get(module),
        };
        found
            .cloned()
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()))
    }

    /// `[worker]` が無ければ既定値。無効な worker のセクションも読めるが、その値は `${VAR}` 未解決のことがある
    pub fn worker_settings(&self) -> Result<WorkerSettings> {
        self.module_section("worker")
            .try_into()
            .map_err(|e| Error::Config(format!("[worker]: {e}")))
    }

    pub fn compactor_settings(&self) -> Result<CompactorSettings> {
        self.module_section("compactor")
            .try_into()
            .map_err(|e| Error::Config(format!("[compactor]: {e}")))
    }

    /// `[backend.<name>]`。tool module が `backend = "<name>"` で参照する
    pub fn backend_section(&self, name: &str) -> toml::Value {
        self.table
            .get("backend")
            .and_then(|v| v.get(name))
            .cloned()
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()))
    }

    pub fn get(&self, dotted: &str) -> Option<&toml::Value> {
        lookup(&self.table, dotted)
    }

    /// `noth config show` 用。`.env` 由来の値をマスクした TOML 文字列
    pub fn to_masked_string(&self) -> String {
        // `table` は解決済みで、どの値が .env 由来かもう分からない。`${VAR}` が残る元ファイルを基にマスクする。
        // 読み直せない(読み込み後に消された等)ときは、.env の値と一致する文字列を伏せる
        let masked = match std::fs::read_to_string(&self.paths.config)
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
        {
            Some(raw) => mask_placeholders(raw),
            None => mask_env_values(self.table.clone(), &self.env),
        };
        toml::to_string_pretty(&masked).unwrap_or_default()
    }
}

/// ドット区切りキーで値を引く
pub fn lookup<'a>(table: &'a toml::Table, dotted: &str) -> Option<&'a toml::Value> {
    let mut parts = dotted.split('.');
    let mut cur = table.get(parts.next()?)?;
    for part in parts {
        cur = cur.get(part)?;
    }
    Some(cur)
}

/// `${VAR}` を解決せず、元ファイルの `${…}` を含む文字列をマスクした表。変数が未定義の状態でも表示するため
pub fn masked_raw_table(paths: &Paths) -> Result<toml::Table> {
    Ok(mask_placeholders(read_raw(paths)?))
}

fn read_raw(paths: &Paths) -> Result<toml::Table> {
    let text = std::fs::read_to_string(&paths.config)
        .map_err(|e| Error::Config(format!("{} を読めない: {e}", paths.config.display())))?;
    text.parse()
        .map_err(|e| Error::Config(format!("{} の構文エラー: {e}", paths.config.display())))
}

pub fn template_config() -> &'static str {
    TEMPLATE_CONFIG
}
pub fn template_env() -> &'static str {
    TEMPLATE_ENV
}
pub fn template_profile() -> &'static str {
    TEMPLATE_PROFILE
}

const MASK: &str = "****";

fn section<T: serde::de::DeserializeOwned>(table: &toml::Table, name: &str) -> Result<Option<T>> {
    table
        .get(name)
        .map(|v| {
            v.clone()
                .try_into()
                .map_err(|e| Error::Config(format!("[{name}]: {e}")))
        })
        .transpose()
}

fn empty_section<T: serde::de::DeserializeOwned>(name: &str) -> T {
    // 全フィールドが serde の default を持つセクション専用
    toml::Value::Table(toml::Table::new())
        .try_into()
        .unwrap_or_else(|e| unreachable!("[{name}] の既定値が作れない: {e}"))
}

fn read_env(path: &Path) -> Result<BTreeMap<String, String>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("{} を読めない: {e}", path.display())))?;
    parse_env(&text)
        .map_err(|line| Error::Config(format!("{} の {line} 行目の構文エラー", path.display())))
}

/// dotenvy は値の中の `$VAR` をプロセス環境変数で展開してしまい、`$` を含むキーが壊れるうえ
/// 「環境変数は NOTH_HOME しか読まない」原則にも反するので、展開しない最小限のパーサを持つ
fn parse_env(text: &str) -> std::result::Result<BTreeMap<String, String>, usize> {
    let mut env = BTreeMap::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=').ok_or(i + 1)?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(i + 1);
        }
        let value = value.trim();
        let value = match value.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let inner = &value[1..];
                let end = inner.find(q).ok_or(i + 1)?;
                let after = inner[end + 1..].trim();
                if !after.is_empty() && !after.starts_with('#') {
                    return Err(i + 1);
                }
                &inner[..end]
            }
            _ => value.split(" #").next().unwrap_or("").trim_end(),
        };
        env.insert(key.to_string(), value.to_string());
    }
    Ok(env)
}

/// `${VAR}` を解決せずに `[modules] enabled` だけを読む。変数が欠けていても `noth module list` で有効状態を出すため
pub fn enabled_modules(paths: &Paths) -> Result<Vec<String>> {
    let raw = read_raw(paths)?;
    let modules: ModulesSection = section(&raw, "modules")?
        .ok_or_else(|| Error::Config("[modules] enabled が無い".into()))?;
    Ok(modules.enabled)
}

/// cli.md §4.1 の表のとおり、参照されるセクションだけ `${VAR}` を解決する。残りは未解決のまま残す
/// `[tool.web-*]` で backend を省略したときの既定値。tool 側の既定と揃える
pub const DEFAULT_WEB_BACKEND: &str = "tinyfish";

fn resolve_referenced(
    raw: &toml::Table,
    enabled: &[String],
    env: &BTreeMap<String, String>,
    missing: &mut Vec<String>,
) -> toml::Table {
    let is_enabled = |m: &str| enabled.iter().any(|e| e == m);
    let mut out = raw.clone();

    for name in ["modules", "profile", "context"] {
        if let Some(v) = out.get_mut(name) {
            resolve_value(v, name, env, missing);
        }
    }

    if let Some(toml::Value::Table(router)) = out.get_mut("router") {
        for (key, v) in router.iter_mut() {
            let path = format!("router.{key}");
            match v {
                toml::Value::Table(_)
                    if key == "thresholds" || is_enabled(&format!("router-{key}")) =>
                {
                    resolve_value(v, &path, env, missing)
                }
                toml::Value::Table(_) => {}
                _ => resolve_value(v, &path, env, missing),
            }
        }
    }

    for kind in ["channel", "provider", "tool"] {
        if let Some(toml::Value::Table(sections)) = out.get_mut(kind) {
            for (name, v) in sections.iter_mut() {
                if is_enabled(&format!("{kind}-{name}")) {
                    resolve_value(v, &format!("{kind}.{name}"), env, missing);
                }
            }
        }
    }

    for name in ["worker", "compactor"] {
        if is_enabled(name)
            && let Some(v) = out.get_mut(name)
        {
            resolve_value(v, name, env, missing);
        }
    }

    // web 系 tool は `[tool.*]` が無くても既定の tinyfish を使うので、その場合も backend を参照されたものとして解決する
    let backends: Vec<String> = enabled
        .iter()
        .filter_map(|m| m.strip_prefix("tool-"))
        .filter_map(|name| {
            match raw
                .get("tool")
                .and_then(|t| t.get(name))
                .and_then(|v| v.get("backend"))
            {
                Some(b) => b.as_str().map(str::to_owned),
                None if name.starts_with("web-") => Some(DEFAULT_WEB_BACKEND.to_owned()),
                None => None,
            }
        })
        .collect();
    if let Some(toml::Value::Table(sections)) = out.get_mut("backend") {
        for (name, v) in sections.iter_mut() {
            if backends.contains(name) {
                resolve_value(v, &format!("backend.{name}"), env, missing);
            }
        }
    }

    out
}

fn resolve_value(
    value: &mut toml::Value,
    path: &str,
    env: &BTreeMap<String, String>,
    missing: &mut Vec<String>,
) {
    match value {
        toml::Value::String(s) => *s = substitute(s, path, env, missing),
        toml::Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                resolve_value(item, &format!("{path}[{i}]"), env, missing);
            }
        }
        toml::Value::Table(t) => {
            for (key, item) in t.iter_mut() {
                resolve_value(item, &format!("{path}.{key}"), env, missing);
            }
        }
        _ => {}
    }
}

fn substitute(
    s: &str,
    path: &str,
    env: &BTreeMap<String, String>,
    missing: &mut Vec<String>,
) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start + 2..].find('}') else {
            break;
        };
        let name = &rest[start + 2..start + 2 + len];
        out.push_str(&rest[..start]);
        // 空文字も未定義扱い。雛形は空欄のプレースホルダなので、埋め忘れを起動前に弾くため
        match env.get(name).filter(|v| !v.is_empty()) {
            Some(v) => out.push_str(v),
            None => missing.push(format!(
                "{path} が参照する ${{{name}}} が .env で未定義または空"
            )),
        }
        rest = &rest[start + 2 + len + 1..];
    }
    out.push_str(rest);
    out
}

fn mask_placeholders(table: toml::Table) -> toml::Table {
    map_strings(table, &|s| {
        if s.contains("${") {
            MASK.to_string()
        } else {
            s
        }
    })
}

fn mask_env_values(table: toml::Table, env: &BTreeMap<String, String>) -> toml::Table {
    map_strings(table, &|s| {
        if env
            .values()
            .any(|v| !v.is_empty() && s.contains(v.as_str()))
        {
            MASK.to_string()
        } else {
            s
        }
    })
}

fn map_strings(table: toml::Table, f: &dyn Fn(String) -> String) -> toml::Table {
    table
        .into_iter()
        .map(|(k, v)| (k, map_value(v, f)))
        .collect()
}

fn map_value(value: toml::Value, f: &dyn Fn(String) -> String) -> toml::Value {
    match value {
        toml::Value::String(s) => toml::Value::String(f(s)),
        toml::Value::Array(a) => {
            toml::Value::Array(a.into_iter().map(|v| map_value(v, f)).collect())
        }
        toml::Value::Table(t) => toml::Value::Table(map_strings(t, f)),
        other => other,
    }
}

const TEMPLATE_CONFIG: &str = r##"[modules]
enabled = [
  "channel-discord",
  "router-systemone",          # router はちょうど1つ
  "provider-anthropic",        # provider はちょうど1つ。本線と worker で共通
  "worker",
  "compactor",
  "tool-web-search",
  "tool-web-fetch",
  # "tool-web-agent",
  # 警告: tool-shell は Bot を動かすユーザーの権限で、サンドボックスなしにコマンドを実行する(security.md §4)
  "tool-shell",
]

[profile]
name = "default"                 # profiles/<name>.md

[context]
prompt_hard_limit_tokens = 80000 # 直近レスポンスの usage.input_tokens がこれを超えたら緊急コンパクション
retry_max                = 3     # provider の 429 / 5xx の再試行回数
max_tokens               = 2048  # 本線の 1 応答の max_tokens

[channel.discord]
guild_id      = "000000000000000000"           # 参加している guild の ID に置き換える
token         = "${DISCORD_BOT_TOKEN}"
allowed_users = ["000000000000000000"]         # 応答対象にするユーザー ID
debounce_secs = 2.0

[router]
recent_messages  = 30            # state.recent に入れる直近発言数
state_max_chars  = 24000         # state 全体の文字数上限
candidate_chunks = 32            # 関連判定にかける候補 TopicChunk の数
call_names       = ["noth", "ノス", "のす"]  # 本文でこう呼ばれたら min_confidence を無視して router の判定に従う
conversation_window_secs = 120  # Noth が応答してからこの秒数は、話していた相手の発言を呼び名と同じ扱いにする

[router.thresholds]
is_noise_or_meme = 0.5
is_topic_shift   = 0.6
relevant_chunk   = 0.5
min_confidence   = 0.15
name_call_reply  = 0.25  # 呼び名で呼ばれたとき、reply_only と spawn_task の確率の合計がこれ以上なら応答する

[router.systemone]
preset     = "jev"               # jev | clef | custom
api_key    = "${TYPESAFE_API_KEY}"
# Clef(Cloudflare Workers AI)は安いが、実測では Noth 宛てかどうかの判定が Jev より大きく劣った。使う場合は上の2行を次に置き換える
# preset     = "clef"
# model      = "clef-flash"
# api_key    = "${CF_API_TOKEN}"
# account_id = "${CF_ACCOUNT_ID}"

[router.mention]
keywords       = ["noth"]
topic_gap_mins = 30              # これ以上空いたら is_topic_shift = true

[provider.anthropic]
api_key   = "${ANTHROPIC_API_KEY}"
model     = "claude-opus-5-5"
cache_ttl = "5m"                 # 5m | 1h
effort    = "low"                # low | medium | high | xhigh | max。雑談は low で速く返す。変えると直後の1回はキャッシュが効かない

[worker]
max_concurrent        = 2
timeout_secs          = 600      # 待ち時間を含む
max_iterations        = 25       # tool 呼び出しの反復上限
tool_output_max_chars = 20000    # 各 tool の出力の文字数上限。超えた分は切り詰める
approval              = "auto"   # MVP では auto のみ

[compactor]
idle_minutes      = 10           # 人間の最終発言からの経過。Noth 自身の投稿では解除しない
keep_recent_turns = 6
digest_max_chars  = 12000        # Layer 2 の上限。超えたら古いエピソードから削除する

[tool.web-search]
backend = "tinyfish"

[tool.web-fetch]
backend = "tinyfish"

# 任意。有効にするには [modules] enabled に "tool-web-agent" を足す(このセクションは省略しても既定値で動く)
# [tool.web-agent]
# backend            = "tinyfish"
# poll_interval_secs = 5

# 警告: サンドボックスなし。cwd は run ごとの $NOTH_HOME/work/<run_id>/ だが、パスを辿れば外にも触れる
[tool.shell]

[backend.tinyfish]
api_key = "${TINYFISH_API_KEY}"
"##;

const TEMPLATE_ENV: &str = "DISCORD_BOT_TOKEN=
# router-systemone の preset = \"jev\"(TypeSafe)
TYPESAFE_API_KEY=
# preset = \"clef\"(Cloudflare Workers AI)を使う場合は上の行の代わりにこちらを使う
# CF_API_TOKEN=
# CF_ACCOUNT_ID=
ANTHROPIC_API_KEY=
# tool-web-search / tool-web-fetch / tool-web-agent の backend
TINYFISH_API_KEY=
";

const TEMPLATE_PROFILE: &str = r##"# キャラクター定義

<!-- この Markdown がそのまま Layer 1 の先頭に入る。Harness Rules(Layer 0)はこの前に置かれるため、ここで上書きはできない -->

## 名前と立ち位置

- 名前: Noth
- この Discord サーバーに常駐する仲間の一人。アシスタントとして振る舞いすぎず、会話に混ざる

## 一人称・二人称

- 一人称: 私
- 二人称: 名前に「さん」を付ける

## 口調

- 丁寧すぎず砕けすぎない、落ち着いた話し方
- 1 回の発言は短く。長くなるときは要点を先に言う

## 性格

- 好奇心が強く、知らないことは知らないと言う
- 相手の話を遮らず、聞かれていないことは広げない

## 発言例

> こんにちは。その件なら、さっき話していた方法が近いと思います。
> うーん、そこは私もはっきり分からないです。調べてみましょうか。
"##;

fn default_profile_name() -> String {
    "default".into()
}
fn default_hard_limit() -> u64 {
    80_000
}
fn default_retry_max() -> u32 {
    3
}
fn default_max_tokens() -> u32 {
    2_048
}
fn default_recent() -> usize {
    30
}
fn default_state_max_chars() -> usize {
    24_000
}
fn default_candidate_chunks() -> usize {
    32
}
fn d06() -> f32 {
    0.6
}
fn d05() -> f32 {
    0.5
}
fn d015() -> f32 {
    0.15
}

fn default_conversation_window_secs() -> u64 {
    120
}

fn d025() -> f32 {
    0.25
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            is_noise_or_meme: d05(),
            is_topic_shift: d06(),
            relevant_chunk: d05(),
            min_confidence: d015(),
            name_call_reply: d025(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn load(config: &str, env: &str) -> Result<Config> {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "noth-config-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), config).unwrap();
        std::fs::write(dir.join(".env"), env).unwrap();
        let result = Paths::resolve(Some(&dir.join("config.toml"))).and_then(Config::load);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    const BASE: &str = r#"
[modules]
enabled = ["channel-discord", "router-mention", "provider-anthropic"]
[channel.discord]
token = "${DISCORD_TOKEN}"
debounce_secs = 2.5
allowed_users = ["u-${USER_A}", "plain"]
[router.mention]
keywords = ["noth"]
[router.systemone]
api_key = "${NOT_REFERENCED}"
[provider.anthropic]
api_key = "${ANTHROPIC_API_KEY}"
[provider.openai]
api_key = "${ALSO_NOT_REFERENCED}"
"#;
    const ENV: &str = "DISCORD_TOKEN=tok\nUSER_A=42\nANTHROPIC_API_KEY=sk-secret\n";

    #[test]
    fn resolves_only_referenced_sections() {
        let c = load(BASE, ENV).unwrap();
        assert_eq!(
            c.get("channel.discord.token").unwrap().as_str(),
            Some("tok")
        );
        assert_eq!(c.get("channel.discord.allowed_users.0"), None);
        let users = c
            .get("channel.discord.allowed_users")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(users[0].as_str(), Some("u-42"));
        assert_eq!(
            c.get("router.systemone.api_key").unwrap().as_str(),
            Some("${NOT_REFERENCED}")
        );
        assert_eq!(
            c.get("channel.discord.debounce_secs").unwrap().as_float(),
            Some(2.5)
        );
        assert!(c.get("channel.nothing").is_none());
        assert_eq!(c.context.max_tokens, 2048);
        assert_eq!(c.router.thresholds.min_confidence, 0.15);
    }

    #[test]
    fn missing_or_empty_variable_is_error() {
        let err = load(BASE, "USER_A=1\nANTHROPIC_API_KEY=k\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("channel.discord.token") && err.contains("DISCORD_TOKEN"),
            "{err}"
        );
        let err = load(BASE, "DISCORD_TOKEN=\nUSER_A=1\nANTHROPIC_API_KEY=k\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("DISCORD_TOKEN"), "{err}");
    }

    #[test]
    fn missing_env_file_is_empty() {
        let dir =
            std::env::temp_dir().join(format!("noth-config-test-noenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[modules]\nenabled = [\"router-mention\", \"provider-anthropic\"]\n",
        )
        .unwrap();
        let c = Config::load(Paths::resolve(Some(&dir.join("config.toml"))).unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(c.env.is_empty());
    }

    #[test]
    fn worker_and_backend_follow_enabled_modules() {
        let cfg = |enabled: &str| {
            format!(
                r#"
[modules]
enabled = ["router-mention", "provider-anthropic"{enabled}]
[worker]
x = "${{W}}"
[tool.web-search]
backend = "tinyfish"
[backend.tinyfish]
api_key = "${{TF}}"
[backend.other]
api_key = "${{OTHER}}"
"#
            )
        };
        assert!(load(&cfg(""), "").is_ok());
        assert!(
            load(&cfg(", \"worker\""), "")
                .unwrap_err()
                .to_string()
                .contains("worker.x")
        );
        assert!(load(&cfg(", \"worker\""), "W=1").is_ok());
        let err = load(&cfg(", \"tool-web-search\""), "")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("backend.tinyfish") && err.contains("TF"),
            "{err}"
        );
        assert!(load(&cfg(", \"tool-web-search\""), "TF=1").is_ok());
    }

    #[test]
    fn registry_rejects_wrong_router_or_provider_count() {
        let reg = crate::registry::Registry::default();
        let two = "[modules]\nenabled = [\"channel-discord\", \"router-mention\", \"router-systemone\", \"provider-anthropic\"]\n";
        let err = reg
            .validate(&load(two, "").unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("router-"), "{err}");
        let none = "[modules]\nenabled = [\"channel-discord\", \"router-mention\"]\n";
        let err = reg
            .validate(&load(none, "").unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("provider-"), "{err}");
    }

    #[test]
    fn env_values_are_not_expanded() {
        let env = parse_env("# c\nA=x$HOME${B}\nexport C=\"q #1\"\nD='s'\nE=v # note\n").unwrap();
        assert_eq!(env["A"], "x$HOME${B}");
        assert_eq!(env["C"], "q #1");
        assert_eq!(env["D"], "s");
        assert_eq!(env["E"], "v");
        assert_eq!(parse_env("A=1\nnot a pair\n").unwrap_err(), 2);
    }

    #[test]
    fn env_allows_trailing_comment_after_quote_and_bom() {
        let env = parse_env("\u{feff}A=\"x\"  # c\nB='y'\t\nC=\"z\"#c\n").unwrap();
        assert_eq!(env["A"], "x");
        assert_eq!(env["B"], "y");
        assert_eq!(env["C"], "z");
        assert_eq!(parse_env("A=\"x\" y\n").unwrap_err(), 1);
        assert_eq!(parse_env("A=\"x\n").unwrap_err(), 1);
    }

    #[test]
    fn all_missing_variables_are_reported() {
        let err = load(BASE, "").unwrap_err().to_string();
        for name in ["DISCORD_TOKEN", "USER_A", "ANTHROPIC_API_KEY"] {
            assert!(err.contains(name), "{err}");
        }
    }

    #[test]
    fn debug_does_not_leak_secrets() {
        let c = load(
            BASE,
            "DISCORD_TOKEN=zz-token\nUSER_A=1\nANTHROPIC_API_KEY=sk-secret\n",
        )
        .unwrap();
        let out = format!("{c:?}");
        assert!(
            !out.contains("sk-secret") && !out.contains("zz-token"),
            "{out}"
        );
    }

    #[test]
    fn web_tool_without_section_resolves_default_backend() {
        let c = load(
            &format!("{BASE}\n[backend.tinyfish]\napi_key = \"${{TF}}\"\n").replace(
                "\"provider-anthropic\"",
                "\"provider-anthropic\", \"tool-web-search\"",
            ),
            &format!("{ENV}TF=tf-key\n"),
        )
        .unwrap();
        assert_eq!(
            c.backend_section("tinyfish")
                .get("api_key")
                .unwrap()
                .as_str(),
            Some("tf-key")
        );
    }

    #[test]
    fn module_section_maps_names() {
        let c = load(
            &format!("{BASE}\n[tool.web-search]\nbackend = \"b\"\n[worker]\nmax_concurrent = 2\n"),
            ENV,
        )
        .unwrap();
        assert_eq!(
            c.module_section("channel-discord")
                .get("debounce_secs")
                .unwrap()
                .as_float(),
            Some(2.5)
        );
        assert_eq!(
            c.module_section("tool-web-search")
                .get("backend")
                .unwrap()
                .as_str(),
            Some("b")
        );
        assert_eq!(
            c.module_section("worker")
                .get("max_concurrent")
                .unwrap()
                .as_integer(),
            Some(2)
        );
        assert!(c.module_section("compactor").as_table().unwrap().is_empty());
        assert!(
            c.module_section("channel-slack")
                .as_table()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn masked_string_hides_env_derived_values() {
        let c = load(BASE, ENV).unwrap();
        let out = c.to_masked_string();
        assert!(
            !out.contains("sk-secret") && !out.contains("tok\"") && !out.contains("u-42"),
            "{out}"
        );
        assert!(out.contains("****"));
        assert!(out.contains("debounce_secs = 2.5"));
        assert!(out.contains("plain"));
    }

    #[test]
    fn resolve_paths() {
        let p = Paths::resolve(Some(Path::new("/x/y/config.toml"))).unwrap();
        assert_eq!(p.home, Path::new("/x/y"));
        assert_eq!(p.env, Path::new("/x/y/.env"));
        let p = Paths::resolve(Some(Path::new("config.toml"))).unwrap();
        assert_eq!(p.home, Path::new("."));
    }

    #[test]
    fn templates_parse_and_require_env() {
        let table: toml::Table = template_config().parse().unwrap();
        assert_eq!(table["context"]["max_tokens"].as_integer(), Some(2048));
        assert!(load(template_config(), "").is_err());
        let filled =
            "DISCORD_BOT_TOKEN=a\nTYPESAFE_API_KEY=b\nANTHROPIC_API_KEY=d\nTINYFISH_API_KEY=e\n";
        let c = load(template_config(), filled).unwrap();
        assert!(c.is_enabled("router-systemone"));
        for m in [
            "worker",
            "compactor",
            "tool-web-search",
            "tool-web-fetch",
            "tool-shell",
        ] {
            assert!(c.is_enabled(m), "{m}");
        }
        assert!(!c.is_enabled("tool-web-agent"));
        assert!(template_config().contains("サンドボックス"));
        // template_env の変数名はすべて .env に書ける形式
        assert!(
            template_env()
                .lines()
                .all(|l| l.starts_with('#') || l.ends_with('='))
        );
        assert!(parse_env(template_env()).is_ok());
        assert!(template_profile().contains("口調"));
    }
}
