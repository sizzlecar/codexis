//! The static flow map through the real executable on small Rust and Python
//! projects: entries, trunk, exits, configuration, shared state and systems.
use serde_json::Value;
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn write(root: &Path, path: &str, text: &str) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, text).unwrap();
}

fn codexis(root: &Path, cache: &Path, arguments: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_codexis"))
        .args(["--locale", "zh-CN", "--project"])
        .arg(root)
        .arg("--cache-dir")
        .arg(cache)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "codexis {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn flow(root: &Path, cache: &Path) -> Value {
    codexis(root, cache, &["analyze", "--format", "json"]);
    let report: Value =
        serde_json::from_str(&codexis(root, cache, &["--format", "json", "flow"])).unwrap();
    report["data"]["flow"].clone()
}

fn step<'a>(map: &'a Value, label: &str) -> &'a Value {
    map["trunk"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["label"].as_str().is_some_and(|l| l.starts_with(label)))
        .unwrap_or_else(|| panic!("no trunk step {label}: {:#}", map["trunk"]))
}

fn exits(step: &Value) -> Vec<String> {
    step["exits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{} {}",
                e["status"].as_str().unwrap(),
                e["code"].as_str().unwrap_or("")
            )
        })
        .collect()
}

#[test]
fn rust_flow_follows_declared_types_from_routes_to_exits() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[package]\nname='shop-api'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nserde={version='1',features=['derive']}\nserde_yaml='0.9'\nsqlx-mysql='0.8'\nreqwest='0.12'\narc-swap='1'\n",
    );
    write(
        root.path(),
        "src/main.rs",
        "mod config;\nmod engine;\nmod routes;\nfn main() {}\n",
    );
    write(
        root.path(),
        "src/config.rs",
        r#"use serde::Deserialize;
#[derive(Deserialize)]
pub struct Settings { pub limits: Limits, pub retry: Retry }
#[derive(Deserialize)]
pub struct Limits { pub max_inflight: usize }
#[derive(Deserialize)]
pub struct Retry { pub max_attempts: u32 }
pub struct Store { current: arc_swap::ArcSwap<Settings> }
impl Store {
    pub fn load(text: &str) -> Settings {
        let settings: Settings = serde_yaml::from_str(text).unwrap();
        settings
    }
    pub fn current(&self) -> std::sync::Arc<Settings> { self.current.load_full() }
}
"#,
    );
    write(
        root.path(),
        "src/engine.rs",
        r#"use crate::config::{Settings, Store};
use std::sync::Mutex;
pub struct Error { pub status: u16, pub code: &'static str }
impl Error {
    pub fn new(status: u16, code: &'static str) -> Self { Self { status, code } }
    fn busy() -> Self { Self::new(429, "too_busy") }
}
pub enum Failure { Missing, Broken }
impl Failure {
    pub fn status(&self) -> (u16, &'static str) {
        match self { Failure::Missing => (404, "not_found"), Failure::Broken => (500, "broken") }
    }
}
pub struct Pool { slots: Mutex<Vec<u32>> }
impl Pool {
    pub fn select(&self, settings: &Settings) -> Result<u32, Error> {
        let slots = self.slots.lock().unwrap();
        if slots.len() >= settings.limits.max_inflight {
            return Err(Error::new(503, "no_backend"));
        }
        Ok(1)
    }
}
pub struct Engine { pool: Pool, store: Store }
impl Engine {
    pub fn execute(&self, request: &str) -> Result<(), Error> {
        let settings = self.store.current();
        self.attempts(&settings, request)
    }
    fn attempts(&self, settings: &Settings, request: &str) -> Result<(), Error> {
        if request.is_empty() {
            return Err(Error::busy());
        }
        for attempt in 0..settings.retry.max_attempts {
            let slot = self.pool.select(settings)?;
            self.send(slot + attempt)?;
        }
        Ok(())
    }
    fn send(&self, slot: u32) -> Result<(), Error> {
        if slot == 0 {
            let _ = Failure::Missing;
        }
        Ok(())
    }
}
"#,
    );
    write(
        root.path(),
        "src/routes.rs",
        r#"use crate::engine::Engine;
use std::sync::Arc;
pub trait Handler { fn handle(&self, body: &str); }
pub struct Api { engine: Arc<Engine> }
impl Handler for Api { fn handle(&self, body: &str) { let _ = self.engine.execute(body); } }
pub struct Health;
impl Handler for Health { fn handle(&self, _body: &str) {} }
pub struct Router;
impl Router { pub fn register(&mut self, _method: &str, _path: &str, _handler: Box<dyn Handler>) {} }
pub fn routes(router: &mut Router, engine: Arc<Engine>) {
    for path in ["/orders", "/payments"] {
        router.register("POST", path, Box::new(Api { engine: engine.clone() }));
    }
    router.register("GET", "/health", Box::new(Health));
}
"#,
    );
    let map = flow(root.path(), cache.path());
    let routes: Vec<String> = map["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{} {} {}",
                r["method"].as_str().unwrap(),
                r["path"].as_str().unwrap(),
                r["internal"]
            )
        })
        .collect();
    assert!(routes.contains(&"POST /orders false".into()), "{routes:?}");
    assert!(
        routes.contains(&"POST /payments false".into()),
        "{routes:?}"
    );
    assert!(routes.contains(&"GET /health true".into()), "{routes:?}");
    assert_eq!(map["trunk"]["label"], "Api::handle");
    assert_eq!(map["trunk"]["routes"], 2);
    assert_eq!(step(&map, "Engine::execute")["kind"], "inline");
    assert_eq!(step(&map, "Engine::attempts")["kind"], "inline");
    assert_eq!(step(&map, "if request.is_empty()")["kind"], "guard");
    assert_eq!(exits(step(&map, "if request.is_empty()")), ["429 too_busy"]);
    let header = step(&map, "for attempt in");
    assert_eq!(header["kind"], "loop");
    let select = step(&map, "Pool::select");
    assert_eq!(select["in_loop"], true);
    assert_eq!(exits(select), ["503 no_backend"]);
    assert!(select["reads"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "limits.max_inflight"));
    assert_eq!(exits(step(&map, "Engine::send")), ["404 not_found"]);
    let config = map["config"].as_array().unwrap();
    let attempts = config
        .iter()
        .find(|f| f["path"] == "retry.max_attempts")
        .expect("configuration from the YAML-loaded root");
    assert!(attempts["reads"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["mode"] == "passed"));
    let shared: Vec<&str> = map["shared"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(
        shared.contains(&"Pool") && shared.contains(&"Store"),
        "{shared:?}"
    );
    let external: Vec<&str> = map["external"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["category"].as_str().unwrap())
        .collect();
    assert!(
        external.contains(&"mysql") && external.contains(&"http_client"),
        "{external:?}"
    );
    let plain = codexis(root.path(), cache.path(), &["--plain", "analyze"]);
    for needed in [
        "主干  Api::handle · 2 个入口",
        "⟳ for attempt in",
        "→ 503 no_backend",
        "[配置]",
    ] {
        assert!(plain.contains(needed), "missing {needed}:\n{plain}");
    }
    let errors = codexis(
        root.path(),
        cache.path(),
        &["--plain", "flow", "--view", "errors"],
    );
    assert!(
        errors.contains("no_backend") && errors.contains("engine.rs"),
        "{errors}"
    );
}

#[test]
fn python_flow_reads_decorators_aliases_and_http_exceptions() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(
        root.path(),
        "pyproject.toml",
        "[project]\nname='items'\nversion='0.1.0'\ndependencies=['fastapi','redis','httpx']\n",
    );
    write(root.path(), "app/__init__.py", "");
    write(
        root.path(),
        "app/config.py",
        "from pydantic_settings import BaseSettings\n\n\nclass Settings(BaseSettings):\n    max_items: int = 3\n",
    );
    write(
        root.path(),
        "app/service.py",
        r#"import threading
from fastapi import HTTPException
from app.config import Settings


class Store:
    def __init__(self, settings: Settings):
        self.settings = settings
        self.lock = threading.Lock()
        self.items = {}

    def save(self, key):
        if len(self.items) >= self.settings.max_items:
            raise HTTPException(status_code=409, detail="store_full")
        with self.lock:
            self.items[key] = True


class Service:
    def __init__(self, store: Store = None):
        self.store = store

    def create(self, key: str):
        if not key:
            raise HTTPException(status_code=400, detail="missing_key")
        self.store.save(key)
        return key
"#,
    );
    write(
        root.path(),
        "app/api.py",
        r#"from typing import Annotated
from fastapi import APIRouter, Depends
from app.service import Service

router = APIRouter(prefix="/v1/items")
Deps = Annotated[Service, Depends(Service)]


@router.post("/{key}")
def create(key: str, service: Deps):
    return service.create(key)


@router.get("/health")
def health():
    return {"ok": True}
"#,
    );
    let map = flow(root.path(), cache.path());
    let routes: Vec<String> = map["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{} {}",
                r["method"].as_str().unwrap(),
                r["path"].as_str().unwrap()
            )
        })
        .collect();
    assert!(
        routes.contains(&"POST /v1/items/{key}".into()),
        "{routes:?}"
    );
    assert_eq!(map["trunk"]["label"], "api.create");
    let create = step(&map, "Service.create");
    let found = exits(create);
    assert!(
        found.contains(&"400 missing_key".into()) && found.contains(&"409 store_full".into()),
        "{found:?}"
    );
    let max_items = map["config"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "max_items")
        .expect("BaseSettings field");
    assert_eq!(max_items["reads"][0]["mode"], "held");
    assert!(map["shared"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"] == "Store" && s["fields"][0]["name"] == "lock"));
    assert!(map["external"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["category"] == "redis"));
}
