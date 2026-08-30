//! Name-to-constructor map, so chains can be built from config or the UI.

use crate::param::ParamValue;
use crate::node::Node;
use common::{Error, Result};
use std::collections::BTreeMap;

/// Settings passed to a stage constructor.
pub type Settings = BTreeMap<String, ParamValue>;

type Factory = Box<dyn Fn(&Settings) -> Result<Box<dyn Node>> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct StageDesc {
    pub name: &'static str,
    pub summary: &'static str,
    /// Grouping for the UI: "filter", "demod", "decode", "sink".
    pub category: &'static str,
}

/// Every stage type known to this build.
///
/// Registration is explicit rather than via a linker-section trick, because
/// inventory-style auto-registration makes it impossible to tell what a binary
/// actually contains, and cargo features already give per-decoder opt-out.
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<&'static str, (StageDesc, Factory)>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<F>(&mut self, desc: StageDesc, f: F)
    where
        F: Fn(&Settings) -> Result<Box<dyn Node>> + Send + Sync + 'static,
    {
        let name = desc.name;
        let prev = self.entries.insert(name, (desc, Box::new(f)));
        assert!(prev.is_none(), "duplicate stage registration: {name}");
    }

    pub fn build(&self, name: &str, settings: &Settings) -> Result<Box<dyn Node>> {
        let (_, f) = self
            .entries
            .get(name)
            .ok_or_else(|| Error::other(format!("no stage registered as {name:?}")))?;
        f(settings)
    }

    pub fn list(&self) -> impl Iterator<Item = &StageDesc> {
        self.entries.values().map(|(d, _)| d)
    }

    pub fn by_category<'a>(&'a self, cat: &'a str) -> impl Iterator<Item = &'a StageDesc> {
        self.list().filter(move |d| d.category == cat)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }
}

/// Helpers for pulling typed settings out with a default.
pub trait SettingsExt {
    fn f64_or(&self, key: &str, default: f64) -> f64;
    fn i64_or(&self, key: &str, default: i64) -> i64;
    fn bool_or(&self, key: &str, default: bool) -> bool;
    fn str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str;
}

impl SettingsExt for Settings {
    fn f64_or(&self, key: &str, default: f64) -> f64 {
        self.get(key).and_then(|v| v.as_f64()).unwrap_or(default)
    }
    fn i64_or(&self, key: &str, default: i64) -> i64 {
        self.get(key).and_then(|v| v.as_i64()).unwrap_or(default)
    }
    fn bool_or(&self, key: &str, default: bool) -> bool {
        self.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
    }
    fn str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).and_then(|v| v.as_str()).unwrap_or(default)
    }
}
