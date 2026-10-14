use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::Config;
use crate::traits::{Channel, Provider, Router};
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
}

pub struct Modules {
    pub channel: Arc<dyn Channel>,
    pub router: Arc<dyn Router>,
    pub provider: Arc<dyn Provider>,
    pub worker_enabled: bool,
    pub compactor_enabled: bool,
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

    pub fn is_built_in(&self, name: &str) -> bool {
        self.channels.contains_key(name)
            || self.routers.contains_key(name)
            || self.providers.contains_key(name)
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
            let implemented = self.is_built_in(name);
            let optional = matches!(
                kind_of(name),
                Some(ModuleKind::Worker | ModuleKind::Compactor | ModuleKind::Tool)
            );
            if !implemented && !optional {
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
        Ok(Modules {
            channel: cf(&ctx(cname))?,
            router: rf(&ctx(rname))?,
            provider: pf(&ctx(pname))?,
            // worker / compactor はまだ実装していないので、有効にしても無効時の動作を取る
            worker_enabled: false,
            compactor_enabled: false,
        })
    }
}
