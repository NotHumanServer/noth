use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::Config;
use crate::traits::{Channel, Compactor, Provider, Router, Tool, Worker};
use crate::{Error, Result};

pub struct ModuleCtx<'a> {
    pub name: &'a str,
    /// `${VAR}` 解決済みの対応セクション
    pub section: toml::Value,
    pub config: &'a Config,
}

pub type ChannelFactory = fn(&ModuleCtx) -> Result<Arc<dyn Channel>>;
pub type RouterFactory = fn(&ModuleCtx) -> Result<Arc<dyn Router>>;
pub type ProviderFactory = fn(&ModuleCtx) -> Result<Arc<dyn Provider>>;
pub type ToolFactory = fn(&ModuleCtx) -> Result<Arc<dyn Tool>>;
/// main と同一 provider・同一 tool 定義で動かすため、Core が組み立てたものを受け取る(caching.md §2)
pub type WorkerFactory = fn(&ModuleCtx, WorkerDeps) -> Result<Arc<dyn Worker>>;
pub type CompactorFactory = fn(&ModuleCtx, Arc<dyn Provider>) -> Result<Arc<dyn Compactor>>;

pub struct WorkerDeps {
    /// Core が計測用に包んだもの。Worker はそのまま使えば呼び出しが記録される
    pub provider: Arc<dyn Provider>,
    /// tool に渡す `ToolContext::meter`
    pub meter: crate::types::MeterSink,
    /// KNOWN_MODULES の順に並ぶ。tool 定義の順序を毎回同じにしてキャッシュを保つため
    pub tools: Vec<Arc<dyn Tool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Channel,
    Router,
    Provider,
    Worker,
    Compactor,
    Tool,
}

/// 設計書にある全 module 名。`noth module list` で built-in でないものも表示するため
pub const KNOWN_MODULES: &[(&str, ModuleKind)] = &[
    ("channel-discord", ModuleKind::Channel),
    ("router-systemone", ModuleKind::Router),
    ("router-mention", ModuleKind::Router),
    ("provider-openai", ModuleKind::Provider),
    ("provider-anthropic", ModuleKind::Provider),
    ("provider-google", ModuleKind::Provider),
    ("worker", ModuleKind::Worker),
    ("compactor", ModuleKind::Compactor),
    ("tool-web-search", ModuleKind::Tool),
    ("tool-web-fetch", ModuleKind::Tool),
    ("tool-web-agent", ModuleKind::Tool),
    ("tool-shell", ModuleKind::Tool),
    ("tool-discord", ModuleKind::Tool),
];

pub fn kind_of(name: &str) -> Option<ModuleKind> {
    KNOWN_MODULES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, k)| *k)
}

#[derive(Default)]
pub struct Registry {
    channels: BTreeMap<&'static str, ChannelFactory>,
    routers: BTreeMap<&'static str, RouterFactory>,
    providers: BTreeMap<&'static str, ProviderFactory>,
    tools: BTreeMap<&'static str, ToolFactory>,
    worker: Option<WorkerFactory>,
    compactor: Option<CompactorFactory>,
}

pub struct Modules {
    pub channel: Arc<dyn Channel>,
    pub router: Arc<dyn Router>,
    pub provider: Arc<dyn Provider>,
    /// None なら spawn_task を reply_only に読み替え、main に tool 定義を渡さない
    pub worker: Option<Arc<dyn Worker>>,
    /// None ならハード上限超過時に古い TopicChunk を要約せず破棄する
    pub compactor: Option<Arc<dyn Compactor>>,
    /// `noth doctor` の疎通確認用。worker が None でも有効な tool は入る
    pub tools: Vec<(&'static str, Arc<dyn Tool>)>,
    /// provider の呼び出しと tool の課金が届く。`noth run` が受け取って SQLite に保存する
    pub meter: crate::types::MeterSink,
    pub meter_rx: tokio::sync::mpsc::UnboundedReceiver<crate::types::MeterEvent>,
}

impl Registry {
    pub fn register_channel(&mut self, name: &'static str, f: ChannelFactory) {
        self.channels.insert(name, f);
    }
    pub fn register_router(&mut self, name: &'static str, f: RouterFactory) {
        self.routers.insert(name, f);
    }
    pub fn register_provider(&mut self, name: &'static str, f: ProviderFactory) {
        self.providers.insert(name, f);
    }
    pub fn register_tool(&mut self, name: &'static str, f: ToolFactory) {
        self.tools.insert(name, f);
    }
    pub fn register_worker(&mut self, f: WorkerFactory) {
        self.worker = Some(f);
    }
    pub fn register_compactor(&mut self, f: CompactorFactory) {
        self.compactor = Some(f);
    }

    pub fn is_built_in(&self, name: &str) -> bool {
        self.channels.contains_key(name)
            || self.routers.contains_key(name)
            || self.providers.contains_key(name)
            || self.tools.contains_key(name)
            || (name == "worker" && self.worker.is_some())
            || (name == "compactor" && self.compactor.is_some())
    }

    pub fn validate(&self, config: &Config) -> Result<()> {
        self.validate_enabled(&config.modules.enabled)
    }

    /// `${VAR}` を解決できない状態でも `noth module list` から構成を検査できるよう、名前の一覧だけを受ける
    pub fn validate_enabled(&self, enabled: &[String]) -> Result<()> {
        for name in enabled {
            if kind_of(name).is_none() {
                return Err(Error::Config(format!("未知の module です: `{name}`")));
            }
        }
        let count = |kind| enabled.iter().filter(|n| kind_of(n) == Some(kind)).count();
        if count(ModuleKind::Router) != 1 {
            return Err(Error::Config(
                "router-* module はちょうど1つ有効にしてください".into(),
            ));
        }
        if count(ModuleKind::Provider) != 1 {
            return Err(Error::Config(
                "provider-* module はちょうど1つ有効にしてください".into(),
            ));
        }
        // MVP の actor は channel を1つしか扱わない
        if count(ModuleKind::Channel) != 1 {
            return Err(Error::Config(
                "channel-* module はちょうど1つ有効にしてください".into(),
            ));
        }
        for name in enabled {
            if !self.is_built_in(name) {
                return Err(Error::Config(format!(
                    "module `{name}` はこのバイナリに組み込まれていません"
                )));
            }
        }
        Ok(())
    }

    pub fn build(&self, config: &Config) -> Result<Modules> {
        self.validate(config)?;
        let ctx = |name: &'static str| ModuleCtx {
            name,
            section: config.module_section(name),
            config,
        };
        let pick = |kind| {
            config
                .modules
                .enabled
                .iter()
                .find(|n| kind_of(n) == Some(kind))
                .cloned()
                .expect("validated")
        };
        let (cname, cf) = self
            .channels
            .get_key_value(pick(ModuleKind::Channel).as_str())
            .expect("validated");
        let (rname, rf) = self
            .routers
            .get_key_value(pick(ModuleKind::Router).as_str())
            .expect("validated");
        let (pname, pf) = self
            .providers
            .get_key_value(pick(ModuleKind::Provider).as_str())
            .expect("validated");
        let (meter, meter_rx) = tokio::sync::mpsc::unbounded_channel();
        let provider: Arc<dyn Provider> = Arc::new(crate::metering::MeteredProvider::new(
            pf(&ctx(pname))?,
            pname,
            meter.clone(),
        ));
        let mut tools = Vec::new();
        for (name, kind) in KNOWN_MODULES {
            if *kind == ModuleKind::Tool && config.is_enabled(name) {
                let (n, f) = self.tools.get_key_value(name).expect("validated");
                tools.push((*n, f(&ctx(n))?));
            }
        }
        let worker = match self.worker {
            Some(f) if config.is_enabled("worker") => Some(f(
                &ctx("worker"),
                WorkerDeps {
                    provider: provider.clone(),
                    meter: meter.clone(),
                    tools: tools.iter().map(|(_, t)| t.clone()).collect(),
                },
            )?),
            _ => None,
        };
        let compactor = match self.compactor {
            Some(f) if config.is_enabled("compactor") => {
                Some(f(&ctx("compactor"), provider.clone())?)
            }
            _ => None,
        };
        Ok(Modules {
            channel: cf(&ctx(cname))?,
            router: rf(&ctx(rname))?,
            provider,
            worker,
            compactor,
            tools,
            meter,
            meter_rx,
        })
    }
}
