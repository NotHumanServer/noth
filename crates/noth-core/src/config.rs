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
    pub fn work_root(&self) -> PathBuf {
        self.home.join("work")
    }
    /// 作るのは shell などが初めて使うとき。ここでは作らない
    pub fn work_dir(&self, run_id: &str) -> PathBuf {
        self.work_root().join(run_id)
    }
    /// 残しておく成果物の置き場所。run ごとの `work/<run_id>/` は下書き用で、並行する Worker が衝突しないよう作業ごとのサブディレクトリを切る
    pub fn workspace(&self) -> PathBuf {
        self.home.join("workspace")
    }
    /// 他のユーザーに成果物を読ませないよう 0700 にする。起動のたびに権限も直すが、直せなくても起動は止めない。
    /// 利用者が置いたシンボリックリンクの先の権限を勝手に変えないよう、リンクのときはたどって chmod しない
    pub fn ensure_workspace(&self) -> std::io::Result<()> {
        let path = self.workspace();
        let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if is_link {
            return if path.is_dir() {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    "シンボリックリンクの先がディレクトリではありません",
                ))
            };
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            {
                tracing::warn!(path = %path.display(), error = %e, "workspace の権限を 0700 にできませんでした。そのまま続けます");
            }
        }
        Ok(())
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
    pub fn memory_dir(&self) -> PathBuf {
        self.home.join("profiles").join("memory")
    }
    /// Layer 1.5 の長期記憶。人が直接編集してもよい
    pub fn memory_file(&self) -> PathBuf {
        self.memory_dir().join("MEMORY.md")
    }
    pub fn daily_dir(&self) -> PathBuf {
        self.memory_dir().join("daily")
    }
    /// Codex のトークンは更新のたびに書き換わるので、`.env` ではなくここに置く(security.md §5)
    pub fn codex_auth(&self) -> PathBuf {
        self.home.join("data").join("codex-auth.json")
    }
}

/// 相対パスのままだと、プロセスの cwd が違う場所を指して見える。LLM に渡すパスは絶対パスにする
pub fn absolute(path: &std::path::Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// shell が動くマシンの情報。起動時に1回だけ取り、Layer 0 に固定の文字列として入れる(実行中に変えるとキャッシュの先頭が揺れる)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    pub os: String,
    pub hostname: String,
    pub home: PathBuf,
    /// 作れなかったときは None。存在しない場所を成果物の置き場所として案内しないため
    pub workspace: Option<PathBuf>,
}

impl HostInfo {
    /// workspace もここで用意する。main と Worker がそれぞれ呼ぶが、どちらが先でも同じ結果になる
    pub fn detect(paths: &Paths) -> Self {
        // NOTH_HOME 以外の環境変数は読まない方針なので、OS に聞く
        let hostname = std::process::Command::new("hostname")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "不明".to_string());
        let workspace = match paths.ensure_workspace() {
            Ok(()) => Some(absolute(&paths.workspace())),
            Err(e) => {
                tracing::warn!(path = %paths.workspace().display(), error = %e, "workspace を用意できません。Worker には置き場所を案内しません");
                None
            }
        };
        Self {
            os: std::env::consts::OS.to_string(),
            hostname,
            home: absolute(&paths.home),
            workspace,
        }
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
    /// main の 1 応答の max_tokens
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Thresholds {
    /// 文脈価値のないノイズ。記録だけして Layer 3 には昇格させない
    #[serde(default = "d05")]
    pub is_noise_or_meme: f32,
    #[serde(default = "d06")]
    pub is_topic_shift: f32,
    /// noth_request でこれ以上なら Worker に回す。取りこぼさない側に倒す
    #[serde(default = "d05")]
    pub needs_outside_work: f32,
    /// Noth の誤りの指摘とみなす(訂正)
    #[serde(default = "d05")]
    pub corrects_noth: f32,
    /// 黙って補足の Worker を起こす
    #[serde(default = "d05")]
    pub worth_supplement: f32,
    /// 合流判定の `abort` だけに使う
    #[serde(default = "d015")]
    pub min_confidence: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouterSection {
    #[serde(default = "default_recent")]
    pub recent_messages: usize,
    #[serde(default = "default_state_max_chars")]
    pub state_max_chars: usize,
    /// 本文で呼ばれたことを router に伝える(`calls_noth_by_name`)。決定的に noth_request とするのはメンションとリプライだけ。
    /// 開発の話題で名前が出るたびに割り込まないため
    #[serde(default)]
    pub call_names: Vec<String>,
    #[serde(default = "default_conversation_window_secs")]
    pub conversation_window_secs: u64,
    /// 黙って補足する Worker のチャンネルごとの最短間隔。訂正には適用しない
    #[serde(default = "default_supplement_min_interval_secs")]
    pub supplement_min_interval_secs: u64,
    /// 黙って補足する Worker の 1 日の上限(guild 全体)。訂正は数えない
    #[serde(default = "default_supplement_max_per_day")]
    pub supplement_max_per_day: u32,
    #[serde(default)]
    pub thresholds: Thresholds,
    /// router が選べる絵文字リアクション。空にするとリアクションで反応しなくなる
    #[serde(default = "default_reactions")]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub questions: QuestionTexts,
}

/// `[router.questions]`。router に渡す問いの文言。省略した項目は評価で確定した既定値(v2_1)になる。
/// choice の選択肢の並びは固定(同点は先頭が選ばれるため、安全な側が先頭)で、変えられるのは説明だけ
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuestionTexts {
    pub is_noise_or_meme: String,
    pub addressee: AddresseeTexts,
    pub needs_outside_work: String,
    pub corrects_noth: String,
    pub worth_supplement: String,
    pub reaction: ReactionTexts,
    pub is_topic_shift: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AddresseeTexts {
    pub instructions: String,
    pub other: String,
    pub about_noth: String,
    pub other_bot: String,
    pub noth_request: String,
}

/// 絵文字ごとの説明は `[[router.reactions]]` の `meaning` から作るので、ここは問いの文言と `none` の説明だけ
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReactionTexts {
    pub instructions: String,
    pub none: String,
}

impl Default for QuestionTexts {
    fn default() -> Self {
        Self {
            is_noise_or_meme: "今回のトリガー発言群が、スタンプや絵文字の連打、同じ文面のコピペ連投、意味のない短い反応の羅列など、後から読み返しても文脈上の価値がないノイズやミームかを判定してください。短くても質問・意見・情報・感情の表明を含む発言はノイズではありません。迷うときはノイズではないとしてください。".into(),
            addressee: AddresseeTexts::default(),
            needs_outside_work: "この発言に応えるには、会話に出ている情報だけでは足りず、会話の外での作業が必要である。たとえば Web で調べる、チャンネルの過去ログを読む、ファイルやコマンドを扱う、長い文書を書く、モデルの知識では古いかもしれない最近の事柄（新しい製品、サービス、モデルの版、料金など）を確かめる。一般的な知識（料理名など）で答えられるなら作業は要らない".into(),
            corrects_noth: "この発言は、Noth が前に述べたことや調べた結果の誤りを指摘している、または反論している".into(),
            worth_supplement: "この発言には、確かめたり補ったりすると会話の役に立つ点がある。次のどちらかがあるときだけ「はい」とする。(1) 数字、仕様、料金、日付、固有名詞についての具体的な主張がある。(2) 人どうしで出たまま答えのない、事実についての問いがある。リンクだけの発言、相づち、感想、感嘆、冗談は「いいえ」。「〜なら」という仮定の発言や、発言者が最近の出来事(料金の改定、無料化、新しい版)を前提にしている発言も「いいえ」寄りにする".into(),
            reaction: ReactionTexts::default(),
            is_topic_shift: "今回のトリガー発言群が、同じチャンネルの直前の会話(recent のうち同じ channel_id の発言)から話題が切り替わったかを判定してください。同じ話題の続きや、少し脱線しただけの発言は切り替わりではありません。別のチャンネルの発言は無関係として扱い、そのチャンネルに直前の発言がなければ切り替わりではないとしてください。".into(),
        }
    }
}

impl Default for AddresseeTexts {
    fn default() -> Self {
        Self {
            instructions: "最後の発言は誰に向けたものか".into(),
            other: "人どうしの会話、全体への呼びかけ、独り言。Noth への合図（名前での呼びかけ、メンション、Noth の発言への返信、Noth とのやりとりの直後の続き）がないもの。人どうしの質問（例「いまってsonnetだっけ」）は、Noth が答えられそうでもこれ".into(),
            about_noth: "Noth の設計や挙動について人どうしで話していて、名前が話題として出ているだけ。Noth に返事や作業を求めていない（Noth の挙動の感想、開発の相談、ほかの人への説明）。文頭や文末での呼びかけはこれではなく noth_request".into(),
            other_bot: "Noth 以外の Bot に話しかけている、または Bot の発言に返している".into(),
            noth_request: "Noth に返事、回答、作業を求める合図がある。合図とは、名前による呼びかけ（文頭や文末の「のす」「の、す」「…よのす」などの愛称や崩した表記を含む）、メンション、Noth の発言への返信、Noth とのやりとりの直後の続き。trigger の in_conversation_window が true の発言は、Noth とのやりとりの直後の続きの合図がある".into(),
        }
    }
}

impl Default for ReactionTexts {
    fn default() -> Self {
        Self {
            instructions: "Noth がトリガー発言群の最後の発言に絵文字リアクションを付けるとしたら、どれが一番自然かを選んでください。発言の内容と、それを読んだ Noth の気持ちに合うものを選びます。".into(),
            none: "リアクションを付けない（話すほどでも、絵文字を付けるほどでもない）".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    /// router が絵文字を選ぶ手がかり。どんな気持ちで付けるか
    pub meaning: String,
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
    /// run ごとの作業ディレクトリを、最終更新からこの日数で消す。0 なら期限では消さない(空のものは消す)
    #[serde(default = "default_work_retention_days")]
    pub work_retention_days: u64,
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

/// これより小さいと雛形の 4 見出しすら収まらず、Dreaming も記憶を整理できない
const MIN_MEMORY_MAX_CHARS: usize = 500;

/// 長期記憶 MEMORY.md(Layer 1.5)と Dreaming
#[derive(Debug, Clone, Deserialize)]
pub struct MemorySettings {
    #[serde(default = "default_true")]
    pub memo: bool,
    #[serde(default = "default_memo_max_per_reply")]
    pub memo_max_per_reply: usize,
    #[serde(default = "default_memory_max_chars")]
    pub memory_max_chars: usize,
    #[serde(default = "default_true")]
    pub dreaming: bool,
    #[serde(default = "default_dreaming_hour")]
    pub dreaming_hour: u8,
    #[serde(default = "default_dream_input_max_chars")]
    pub dream_input_max_chars: usize,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            memo: true,
            memo_max_per_reply: default_memo_max_per_reply(),
            memory_max_chars: default_memory_max_chars(),
            dreaming: true,
            dreaming_hour: default_dreaming_hour(),
            dream_input_max_chars: default_dream_input_max_chars(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_memo_max_per_reply() -> usize {
    3
}
fn default_memory_max_chars() -> usize {
    6_000
}
fn default_dreaming_hour() -> u8 {
    4
}
fn default_dream_input_max_chars() -> usize {
    60_000
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
fn default_work_retention_days() -> u64 {
    7
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
    pub memory: MemorySettings,
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
            .field("memory", &self.memory)
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
        let memory: MemorySettings =
            section(&table, "memory")?.unwrap_or_else(|| empty_section("memory"));
        if memory.memory_max_chars < MIN_MEMORY_MAX_CHARS {
            return Err(Error::Config(format!(
                "[memory] memory_max_chars は {MIN_MEMORY_MAX_CHARS} 以上: {}",
                memory.memory_max_chars
            )));
        }
        if memory.memo && memory.memo_max_per_reply == 0 {
            return Err(Error::Config(
                "[memory] memo = true のとき memo_max_per_reply は 1 以上".into(),
            ));
        }
        if memory.dreaming_hour > 23 {
            return Err(Error::Config(format!(
                "[memory] dreaming_hour は 0〜23: {}",
                memory.dreaming_hour
            )));
        }

        Ok(Self {
            paths,
            modules,
            profile,
            context,
            router,
            memory,
            table,
            env,
        })
    }

    pub fn is_enabled(&self, module: &str) -> bool {
        self.modules.enabled.iter().any(|m| m == module)
    }

    pub fn module_section(&self, module: &str) -> toml::Value {
        let found = match module.split_once('-') {
            Some((kind @ ("channel" | "router" | "provider" | "tool" | "command"), name)) => {
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

    for name in ["modules", "profile", "context", "memory"] {
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
  "provider-anthropic",        # provider はちょうど1つ。main と worker で共通
  "worker",
  "compactor",
  "tool-web-search",
  "tool-web-fetch",
  # "tool-web-agent",
  # 警告: tool-shell は Bot を動かすユーザーの権限で、サンドボックスなしにコマンドを実行する(security.md §4)
  "tool-shell",
  "tool-discord",              # 読み取り専用の Discord 参照。[channel.discord] のトークンと guild_id を使う
  "command-context",           # /noth context(master だけ。使用量の表示)。[channel.discord] master が要る
]

[profile]
name = "default"                 # profiles/<name>.md

[context]
prompt_hard_limit_tokens = 80000 # 直近レスポンスの usage.input_tokens がこれを超えたら緊急コンパクション
retry_max                = 2     # provider の 429 / 5xx・タイムアウトの再試行回数(初回と合わせて最大 retry_max+1 回)
max_tokens               = 2048  # main の 1 応答の max_tokens

[channel.discord]
guild_id      = "000000000000000000"           # 参加している guild の ID に置き換える
token         = "${DISCORD_BOT_TOKEN}"
allowed_users = ["000000000000000000"]         # 応答対象にするユーザー ID
# master      = "000000000000000000"           # 管理コマンド(/noth ...)を使える唯一のユーザー ID。省略するとコマンドは誰にも使えない
debounce_secs = 2.0

[router]
recent_messages  = 30            # state.recent に入れる直近発言数
state_max_chars  = 24000         # state 全体の文字数上限
call_names       = ["noth", "ノス", "のす"]  # 本文でこう呼ばれたことを router に伝える(calls_noth_by_name)。決定的に noth_request とするのはメンションとリプライだけ
conversation_window_secs = 120  # Noth が応答してからこの秒数の、話していた相手の発言を、router に in_conversation_window で伝える(決定的には noth_request にしない)
supplement_min_interval_secs = 300  # 黙って補足の Worker を起動する、チャンネルごとの最短間隔(秒)。訂正には適用しない
supplement_max_per_day       = 50   # 同じ起動の 1 日の上限回数(guild 全体。訂正は数えない)

[router.thresholds]
is_noise_or_meme   = 0.5  # ノイズは記録だけして Layer 3 には昇格させない。Noth への依頼(noth_request)なら応答を優先
is_topic_shift     = 0.6
needs_outside_work = 0.5  # noth_request でこれ以上なら Worker に回す。取りこぼさない側に倒す
corrects_noth      = 0.5  # Noth の誤りの指摘とみなす(訂正)
worth_supplement   = 0.5  # 黙って補足の Worker を起動する
min_confidence     = 0.15 # 合流判定の abort だけに使う

# 問いの文言は [router.questions] で差し替えられる(省略時は評価で確定した既定値)。
# 例: [router.questions.addressee] の instructions / other / about_noth / other_bot / noth_request、
#     [router.questions.reaction] の instructions / none、
#     needs_outside_work / corrects_noth / worth_supplement / is_topic_shift は文字列

[router.systemone]
preset     = "jev"               # jev | clef | custom
model      = "jev-1.14.0"        # 版を固定した名前。jev-latest などの別名は中身が変わるので避ける
api_key    = "${TYPESAFE_API_KEY}"
# 入力の料金は Jev $0.042、Clef $0.240、Clef-flash $0.038(100万トークンあたり)で、安いのは Clef-flash だけ(Clef は Jev より高い)。
# 実測では Noth 宛てかどうかの判定が Jev より大きく劣った。使う場合は上の3行を次に置き換える
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
approval              = "auto"   # MVP では auto のみ。有効な tool はどの run でも承認なしで呼べる(security.md §3)

[compactor]
idle_minutes      = 10           # 人間の最終発言からの経過。Noth 自身の投稿では解除しない
keep_recent_turns = 6
digest_max_chars  = 12000        # Layer 2 の上限。超えたら古いエピソードから削除する

[memory]                         # 長期記憶 MEMORY.md(Layer 1.5)と Dreaming
memo               = true        # main の [Memo] 行を MEMORY.md に追記する
memo_max_per_reply = 3
memory_max_chars   = 6000        # 超えている間は [Memo] を追記しない。Dreaming がこの長さに整理する
dreaming           = true        # compactor module が必要
dreaming_hour      = 4           # ローカル時刻。この時刻を過ぎた最初のアイドルで前日分を処理する
dream_input_max_chars = 60000    # 日次レポートに渡す発言ログの上限

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
    2
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
fn default_supplement_min_interval_secs() -> u64 {
    300
}
fn default_supplement_max_per_day() -> u32 {
    50
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

fn default_reactions() -> Vec<Reaction> {
    [
        ("👍", "同意・了解"),
        ("😂", "笑える"),
        ("👀", "気になる・続きを見ていたい"),
        ("🙏", "感謝・お疲れさま"),
        ("🎉", "お祝い・うれしい知らせ"),
        ("🤔", "考えさせられる・ちょっと疑問"),
        ("❤️", "共感・いいね"),
        ("😢", "残念・つらい"),
    ]
    .into_iter()
    .map(|(emoji, meaning)| Reaction {
        emoji: emoji.into(),
        meaning: meaning.into(),
    })
    .collect()
}

fn default_conversation_window_secs() -> u64 {
    120
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            is_noise_or_meme: d05(),
            is_topic_shift: d06(),
            needs_outside_work: d05(),
            corrects_noth: d05(),
            worth_supplement: d05(),
            min_confidence: d015(),
        }
    }
}

/// `[channel.discord] api_base` を検証して返す。末尾の `/` は取り除く。
/// fakecord 専用の設定で、Bot トークンがそのホストへ送られるため、loopback の http(s)://host[:port] だけを許す
pub fn parse_api_base(section: &toml::Value) -> Result<Option<String>> {
    let Some(v) = section.get("api_base") else {
        return Ok(None);
    };
    let s = v.as_str().ok_or_else(|| {
        Error::Config("channel.discord.api_base は文字列で指定してください".into())
    })?;
    let trimmed = s.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(Error::Config("channel.discord.api_base が空です".into()));
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(Error::Config(
            "channel.discord.api_base は http:// または https:// で始まる必要があります".into(),
        ));
    }
    let url = url::Url::parse(trimmed).map_err(|e| {
        Error::Config(format!(
            "channel.discord.api_base が URL として不正です: {e}"
        ))
    })?;
    if url.host().is_none() {
        return Err(Error::Config(
            "channel.discord.api_base にホストがありません".into(),
        ));
    }
    if !is_loopback(&url) {
        return Err(Error::Config(
            "channel.discord.api_base のホストは 127.0.0.1、localhost、[::1] のどれかにしてください(fakecord 専用で、Bot トークンが送られます)".into(),
        ));
    }
    if url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::Config(
            "channel.discord.api_base にはホストとポートまでを書いてください(/api/v10 は付けない)"
                .into(),
        ));
    }
    Ok(Some(trimmed.to_string()))
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    }
}

/// http / https / ws / wss の URL から (host, port) を取り出す。port が無ければ scheme の既定値。
/// api_base と Gateway URL が同じ接続先かを比べるのに使う
pub fn url_endpoint(raw: &str) -> Option<(String, u16)> {
    let url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
        return None;
    }
    let host = url.host_str()?.to_string();
    Some((host, url.port_or_known_default()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn work_retention_days_defaults_to_a_week_and_can_be_changed() {
        assert_eq!(
            load(BASE, ENV)
                .unwrap()
                .worker_settings()
                .unwrap()
                .work_retention_days,
            7
        );
        let custom = format!("{BASE}\n[worker]\nwork_retention_days = 30\n");
        assert_eq!(
            load(&custom, ENV)
                .unwrap()
                .worker_settings()
                .unwrap()
                .work_retention_days,
            30
        );
        let off = format!("{BASE}\n[worker]\nwork_retention_days = 0\n");
        assert_eq!(
            load(&off, ENV)
                .unwrap()
                .worker_settings()
                .unwrap()
                .work_retention_days,
            0
        );
    }

    #[test]
    fn relative_home_gives_absolute_paths_to_the_llm() {
        let rel = format!("noth-rel-home-{}", std::process::id());
        let paths = Paths {
            config: PathBuf::from(&rel).join("config.toml"),
            env: PathBuf::from(&rel).join(".env"),
            home: PathBuf::from(&rel),
        };
        let host = HostInfo::detect(&paths);
        assert!(host.home.is_absolute());
        assert!(host.workspace.as_ref().is_some_and(|w| w.is_absolute()));
        assert!(absolute(&paths.work_dir("1792170173263-1")).is_absolute());
        std::fs::remove_dir_all(&rel).ok();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_symlink_is_followed_without_chmod_and_failures_are_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("noth-ws-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let target = base.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::os::unix::fs::symlink(&target, home.join("workspace")).unwrap();
        let paths = Paths {
            config: home.join("config.toml"),
            env: home.join(".env"),
            home: home.clone(),
        };
        paths.ensure_workspace().unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        // 先が無いリンクは作れないので、Worker には置き場所を案内しない
        std::fs::remove_dir_all(&target).unwrap();
        assert!(paths.ensure_workspace().is_err());
        assert_eq!(HostInfo::detect(&paths).workspace, None);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn workspace_is_created_with_0700_and_fixed_in_host_info() {
        let home = std::env::temp_dir().join(format!("noth-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let paths = Paths {
            config: home.join("config.toml"),
            env: home.join(".env"),
            home: home.clone(),
        };
        paths.ensure_workspace().unwrap();
        paths.ensure_workspace().unwrap();
        assert_eq!(paths.workspace(), home.join("workspace"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.workspace())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let host = HostInfo::detect(&paths);
        assert_eq!(host, HostInfo::detect(&paths));
        assert_eq!(
            host.workspace,
            Some(std::path::absolute(home.join("workspace")).unwrap())
        );
        std::fs::remove_dir_all(&home).ok();
    }

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
    fn memory_defaults_and_paths() {
        let c = load("[modules]\nenabled = []\n", "").unwrap();
        assert!(c.memory.memo && c.memory.dreaming);
        assert_eq!(c.memory.memo_max_per_reply, 3);
        assert_eq!(c.memory.memory_max_chars, 6000);
        assert_eq!(c.memory.dreaming_hour, 4);
        assert_eq!(c.memory.dream_input_max_chars, 60_000);
        assert_eq!(c.router.thresholds.needs_outside_work, 0.5);
        assert_eq!(c.router.supplement_min_interval_secs, 300);
        assert_eq!(c.router.supplement_max_per_day, 50);
        assert!(c.router.questions.addressee.noth_request.contains("合図"));
        let home = &c.paths.home;
        assert_eq!(c.paths.memory_dir(), home.join("profiles/memory"));
        assert_eq!(
            c.paths.memory_file(),
            home.join("profiles/memory/MEMORY.md")
        );
        assert_eq!(c.paths.daily_dir(), home.join("profiles/memory/daily"));
    }

    #[test]
    fn memory_section_is_read_and_hour_is_validated() {
        let c = load(
            "[modules]\nenabled = []\n[memory]\nmemo = false\ndreaming_hour = 23\nmemory_max_chars = 500\n",
            "",
        )
        .unwrap();
        assert!(!c.memory.memo);
        assert_eq!(c.memory.dreaming_hour, 23);
        assert_eq!(c.memory.memory_max_chars, 500);
        let err = load(
            "[modules]\nenabled = []\n[memory]\ndreaming_hour = 24\n",
            "",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("dreaming_hour"), "{err}");
        assert!(
            load(
                "[modules]\nenabled = []\n[memory]\ndreaming_hour = -1\n",
                ""
            )
            .is_err()
        );
    }

    #[test]
    fn memory_limits_are_validated() {
        let err = |memory: &str| {
            load(&format!("[modules]\nenabled = []\n[memory]\n{memory}"), "")
                .unwrap_err()
                .to_string()
        };
        assert!(err("memory_max_chars = 499\n").contains("memory_max_chars"));
        assert!(err("memo_max_per_reply = 0\n").contains("memo_max_per_reply"));
        // memo を無効にしていれば件数の検査はしない
        assert!(
            load(
                "[modules]\nenabled = []\n[memory]\nmemo = false\nmemo_max_per_reply = 0\n",
                ""
            )
            .is_ok()
        );
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
            "tool-discord",
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

#[cfg(test)]
mod api_base_tests {
    use super::*;

    fn parse(api_base: &str) -> Result<Option<String>> {
        let mut section = toml::Table::new();
        section.insert("api_base".into(), toml::Value::String(api_base.into()));
        parse_api_base(&toml::Value::Table(section))
    }

    #[test]
    fn missing_key_means_the_real_discord() {
        let section = toml::Value::Table(toml::Table::new());
        assert_eq!(parse_api_base(&section).unwrap(), None);
    }

    #[test]
    fn non_string_value_is_rejected() {
        let mut section = toml::Table::new();
        section.insert("api_base".into(), toml::Value::Integer(123));
        assert!(matches!(
            parse_api_base(&toml::Value::Table(section)),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn loopback_hosts_are_accepted() {
        for ok in [
            "http://127.0.0.1:3900",
            "http://localhost:3900",
            "http://[::1]:3900",
            "https://127.0.0.1",
        ] {
            assert_eq!(parse(ok).unwrap().as_deref(), Some(ok), "{ok}");
        }
    }

    #[test]
    fn trailing_slashes_and_spaces_are_trimmed() {
        assert_eq!(
            parse("  http://127.0.0.1:3900/ ").unwrap().as_deref(),
            Some("http://127.0.0.1:3900")
        );
        assert_eq!(
            parse("http://127.0.0.1:3900//").unwrap().as_deref(),
            Some("http://127.0.0.1:3900")
        );
    }

    #[test]
    fn invalid_values_are_rejected() {
        for (bad, why) in [
            ("", "空文字"),
            ("   ", "空白だけ"),
            ("127.0.0.1:3900", "scheme なし"),
            ("localhost:3900", "scheme なし(localhost)"),
            ("ftp://127.0.0.1:3900", "ftp"),
            ("http://127.0.0.1:3900/api/v10", "パス付き"),
            ("http://127.0.0.1:3900/api/v10/", "パス付き(末尾スラッシュ)"),
            ("http://127.0.0.1:3900?x=1", "クエリ付き"),
            ("http://user:pw@127.0.0.1:3900", "userinfo 付き"),
            ("http://example.com:3900", "外部ホスト"),
            ("http://192.168.0.5:3900", "private アドレス"),
            ("http://127.0.0.2:3900", "127.0.0.1 以外の loopback"),
            ("https://discord.com", "discord.com"),
            ("http://", "host なし"),
            ("http:///", "host なし(スラッシュ)"),
            ("http://:3900", "host が空でポートだけ"),
        ] {
            assert!(
                matches!(parse(bad), Err(Error::Config(_))),
                "{why}: `{bad}` が通ってしまった"
            );
        }
    }

    #[test]
    fn path_error_mentions_the_fix() {
        let Err(Error::Config(m)) = parse("http://127.0.0.1:3900/api/v10") else {
            panic!("通ってしまった");
        };
        assert!(m.contains("ホストとポートまで"), "{m}");
    }

    #[test]
    fn url_endpoint_compares_host_and_port() {
        assert_eq!(
            url_endpoint("ws://127.0.0.1:3900/gateway"),
            Some(("127.0.0.1".into(), 3900))
        );
        assert_eq!(
            url_endpoint("wss://gateway.discord.gg"),
            Some(("gateway.discord.gg".into(), 443))
        );
        assert_eq!(
            url_endpoint("http://localhost"),
            Some(("localhost".into(), 80))
        );
        assert_eq!(url_endpoint("ftp://127.0.0.1:21"), None);
        assert_eq!(url_endpoint("not a url"), None);
    }
}
