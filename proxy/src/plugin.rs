use anyhow::{anyhow, Result};
use home::home_dir;
use hotwatch::Hotwatch;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;
use toml;
use wasmer::ChainableNamedResolver;
use wasmer::ImportObject;
use wasmer::Store;
use wasmer::WasmerEnv;
use wasmer_wasi::Pipe;
use wasmer_wasi::WasiEnv;
use wasmer_wasi::WasiState;

use crate::dispatch::Dispatcher;

pub type PluginName = String;

#[derive(Clone, Debug, Default)]
pub struct Counter(usize);

impl Counter {
    pub fn next(&mut self) -> usize {
        let n = self.0;
        self.0 = n + 1;
        n + 1
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct PluginId(pub usize);

#[derive(Deserialize, Clone, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginDescription {
    pub name: String,
    pub version: String,
    pub display_name: String,
    pub author: String,
    pub description: String,
    pub repository: String,
    pub wasm: String,
    dir: Option<PathBuf>,
    configuration: Option<Value>,
}

#[derive(Serialize, Clone)]
pub struct PluginInfo {
    arch: String,
    os: String,
    configuration: Option<Value>,
}

#[derive(WasmerEnv, Clone)]
pub(crate) struct PluginEnv {
    wasi_env: WasiEnv,
    desc: PluginDescription,
    dispatcher: Dispatcher,
}

#[derive(Clone)]
pub(crate) struct PluginNew {
    instance: wasmer::Instance,
    env: PluginEnv,
}

pub struct PluginCatalog {
    id_counter: Counter,
    pub items: HashMap<PluginName, PluginDescription>,
    plugins: HashMap<PluginName, PluginNew>,
    store: wasmer::Store,
}

impl PluginCatalog {
    pub fn new() -> PluginCatalog {
        PluginCatalog {
            id_counter: Counter::default(),
            items: HashMap::new(),
            plugins: HashMap::new(),
            store: wasmer::Store::default(),
        }
    }

    pub fn stop(&mut self) {
        self.items.clear();
        self.plugins.clear();
    }

    pub fn reload(&mut self) {
        eprintln!("plugin reload from paths");
        self.items.clear();
        self.plugins.clear();
        self.load();
    }

    pub fn load(&mut self) {
        let all_plugins = find_all_plugins();
        for plugin_path in &all_plugins {
            match load_plugin(plugin_path) {
                Err(e) => eprintln!("load plugin err {}", e),
                Ok(plugin) => {
                    self.items.insert(plugin.name.clone(), plugin);
                }
            }
        }
    }

    pub fn install_plugin(
        &mut self,
        dispatcher: Dispatcher,
        plugin: PluginDescription,
    ) -> Result<()> {
        let home = home_dir().unwrap();
        let path = home.join(".lapce").join("plugins").join(&plugin.name);
        std::fs::remove_dir_all(&path);

        std::fs::create_dir_all(&path)?;

        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path.join("plugin.toml"))?;
            file.write_all(&toml::to_vec(&plugin)?)?;
        }

        {
            let url = format!(
                "https://raw.githubusercontent.com/{}/master/{}",
                plugin.repository, plugin.wasm
            );
            let mut resp = reqwest::blocking::get(url)?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path.join(&plugin.wasm))?;
            std::io::copy(&mut resp, &mut file)?;
        }

        let mut plugin = plugin.clone();
        plugin.dir = Some(path.clone());
        plugin.wasm = path
            .join(&plugin.wasm)
            .to_str()
            .ok_or(anyhow!("path can't to string"))?
            .to_string();
        let p = self.start_plugin(dispatcher, plugin.clone())?;
        self.items.insert(plugin.name.clone(), plugin.clone());
        self.plugins.insert(plugin.name.clone(), p);
        Ok(())
    }

    pub fn start_all(&mut self, dispatcher: Dispatcher) {
        for (_, plugin) in self.items.clone().iter() {
            if let Ok(p) = self.start_plugin(dispatcher.clone(), plugin.clone()) {
                self.plugins.insert(plugin.name.clone(), p);
            }
        }
    }

    fn start_plugin(
        &mut self,
        dispatcher: Dispatcher,
        plugin_desc: PluginDescription,
    ) -> Result<PluginNew> {
        let module = wasmer::Module::from_file(&self.store, &plugin_desc.wasm)?;

        let output = Pipe::new();
        let input = Pipe::new();
        let mut wasi_env = WasiState::new("Lapce")
            .map_dir("/", plugin_desc.dir.clone().unwrap())?
            .stdin(Box::new(input))
            .stdout(Box::new(output))
            .finalize()?;
        let wasi = wasi_env.import_object(&module)?;

        let plugin_env = PluginEnv {
            wasi_env,
            desc: plugin_desc.clone(),
            dispatcher,
        };
        let lapce = lapce_exports(&self.store, &plugin_env);
        let instance = wasmer::Instance::new(&module, &lapce.chain_back(wasi))?;
        let plugin = PluginNew {
            instance,
            env: plugin_env,
        };

        let local_plugin = plugin.clone();
        thread::spawn(move || {
            let initialize = local_plugin
                .instance
                .exports
                .get_function("initialize")
                .unwrap();
            wasi_write_object(
                &local_plugin.env.wasi_env,
                &PluginInfo {
                    os: std::env::consts::OS.to_string(),
                    arch: std::env::consts::ARCH.to_string(),
                    configuration: plugin_desc.configuration,
                },
            );
            initialize.call(&[]).unwrap();
        });

        Ok(plugin)
    }

    pub fn next_plugin_id(&mut self) -> PluginId {
        PluginId(self.id_counter.next())
    }
}

pub(crate) fn lapce_exports(store: &Store, plugin_env: &PluginEnv) -> ImportObject {
    macro_rules! lapce_export {
        ($($host_function:ident),+ $(,)?) => {
            wasmer::imports! {
                "lapce" => {
                    $(stringify!($host_function) =>
                        wasmer::Function::new_native_with_env(store, plugin_env.clone(), $host_function),)+
                }
            }
        }
    }

    lapce_export! {
        host_handle_notification,
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "method", content = "params")]
pub enum PluginNotification {
    StartLspServer {
        exec_path: String,
        language_id: String,
        options: Option<Value>,
    },
    DownloadFile {
        url: String,
        path: PathBuf,
    },
    LockFile {
        path: PathBuf,
    },
    MakeFileExecutable {
        path: PathBuf,
    },
}

fn host_handle_notification(plugin_env: &PluginEnv) {
    let notification: Result<PluginNotification> =
        wasi_read_object(&plugin_env.wasi_env);
    if let Ok(notification) = notification {
        match notification {
            PluginNotification::StartLspServer {
                exec_path,
                language_id,
                options,
            } => {
                plugin_env.dispatcher.lsp.lock().start_server(
                    plugin_env
                        .desc
                        .dir
                        .clone()
                        .unwrap()
                        .join(&exec_path)
                        .to_str()
                        .unwrap(),
                    &language_id,
                    options.clone(),
                );
            }
            PluginNotification::DownloadFile { url, path } => {
                let mut resp = reqwest::blocking::get(url).expect("request failed");
                let mut out = std::fs::File::create(
                    plugin_env.desc.dir.clone().unwrap().join(path),
                )
                .expect("failed to create file");
                std::io::copy(&mut resp, &mut out).expect("failed to copy content");
                eprintln!("donwload file finished");
            }
            PluginNotification::LockFile { path } => {
                let path = plugin_env.desc.dir.clone().unwrap().join(path);
                eprintln!("lock file path {:?}", path);
                let mut n = 0;
                loop {
                    if let Ok(file) = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                    {
                        return;
                    }
                    if n > 10 {
                        return;
                    }
                    n += 1;
                    let mut hotwatch =
                        Hotwatch::new().expect("hotwatch failed to initialize!");
                    let (tx, rx) = crossbeam_channel::bounded(1);
                    hotwatch.watch(&path, move |event| {
                        tx.send(0);
                    });
                    rx.recv_timeout(Duration::from_secs(10));
                }
            }
            PluginNotification::MakeFileExecutable { path } => {
                Command::new("chmod")
                    .arg("+x")
                    .arg(&plugin_env.desc.dir.clone().unwrap().join(path))
                    .output();
            }
        }
    }
}

pub fn wasi_read_string(wasi_env: &WasiEnv) -> Result<String> {
    let mut state = wasi_env.state();
    let wasi_file = state
        .fs
        .stdout_mut()?
        .as_mut()
        .ok_or(anyhow!("can't get stdout"))?;
    let mut buf = String::new();
    wasi_file.read_to_string(&mut buf)?;
    Ok(buf)
}

pub fn wasi_read_object<T: DeserializeOwned>(wasi_env: &WasiEnv) -> Result<T> {
    let json = wasi_read_string(wasi_env)?;
    Ok(serde_json::from_str(&json)?)
}

pub fn wasi_write_string(wasi_env: &WasiEnv, buf: &str) {
    let mut state = wasi_env.state();
    let wasi_file = state.fs.stdin_mut().unwrap().as_mut().unwrap();
    writeln!(wasi_file, "{}\r", buf).unwrap();
}

pub fn wasi_write_object(wasi_env: &WasiEnv, object: &(impl Serialize + ?Sized)) {
    wasi_write_string(wasi_env, &serde_json::to_string(&object).unwrap());
}

#[derive(Serialize, Deserialize, Debug)]
pub enum PluginRequest {}

pub struct PluginHandler {
    dispatcher: Dispatcher,
}

fn find_all_plugins() -> Vec<PathBuf> {
    let mut plugin_paths = Vec::new();
    let home = home_dir().unwrap();
    let path = home.join(".lapce").join("plugins");
    path.read_dir().map(|dir| {
        dir.flat_map(|item| item.map(|p| p.path()).ok())
            .map(|dir| dir.join("plugin.toml"))
            .filter(|f| f.exists())
            .for_each(|f| plugin_paths.push(f))
    });
    eprintln!("proxy plugin paths {:?}", plugin_paths);
    plugin_paths
}

fn load_plugin(path: &PathBuf) -> Result<PluginDescription> {
    let mut file = fs::File::open(&path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let mut plugin: PluginDescription = toml::from_str(&contents)?;
    plugin.dir = Some(path.parent().unwrap().canonicalize()?);
    plugin.wasm = path
        .parent()
        .unwrap()
        .join(&plugin.wasm)
        .canonicalize()?
        .to_str()
        .ok_or(anyhow!("path can't to string"))?
        .to_string();
    Ok(plugin)
}
