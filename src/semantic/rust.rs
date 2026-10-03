use super::lsp::{byte_position, utf16_position, LspClient};
use super::{SemanticOutcome, SemanticResolver};
use crate::model::{AnalysisContext, Capability, Diagnostic, FileFacts, Node, ProjectModel};
use crate::source::{check_cancelled, SourceSet};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use url::Url;

pub struct RustAnalyzerResolver;

fn executable() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    let candidate = std::env::split_paths(&path)
        .map(|p| p.join("rust-analyzer"))
        .find(|p| p.is_file())
        .context("rust-analyzer is not installed")?;
    let canonical = candidate.canonicalize()?;
    if canonical.file_stem().is_some_and(|n| n == "rustup") {
        let output = Command::new(&canonical)
            .args(["which", "rust-analyzer"])
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()?;
        if !output.status.success() {
            bail!("rust-analyzer component is not installed in the active toolchain");
        }
        Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
    } else {
        Ok(canonical)
    }
}

impl SemanticResolver for RustAnalyzerResolver {
    fn provider(&self) -> Result<String> {
        let output = Command::new(executable()?)
            .arg("--version")
            .env("RUSTUP_AUTO_INSTALL", "0")
            .output()
            .context("rust-analyzer is not installed")?;
        if !output.status.success() {
            bail!("rust-analyzer --version failed; install the rust-analyzer component");
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn enrich(
        &self,
        source: &SourceSet,
        project: &ProjectModel,
        context: &AnalysisContext,
        facts: &mut FileFacts,
        progress: bool,
    ) -> Result<SemanticOutcome> {
        let provider = self.provider()?;
        let started = Instant::now();
        let budget = Duration::from_secs(context.semantic_timeout_secs.max(1));
        let configuration = json!({
            "cargo":{
                "buildScripts":{"enable":false}, "autoreload":true, "allTargets":false,
                "extraArgs":["--offline","--locked"], "features":context.features,
                "noDefaultFeatures":context.no_default_features,"target":context.target
            },
            "procMacro":{"enable":false},"checkOnSave":false,"check":{"enable":false},
            "cachePriming":{"enable":false},"diagnostics":{"enable":false},
            "cfg":{"setTest":false},"numThreads":2,
            "files":{"watcher":"client"}
        });
        if progress {
            eprintln!("Starting {provider} (offline; build scripts and proc macros disabled)");
        }
        let mut client = LspClient::start(
            &executable()?,
            &source.root,
            configuration,
            progress,
            budget.min(Duration::from_secs(30)),
        )?;
        client.wait_ready(
            budget
                .saturating_sub(started.elapsed())
                .min(Duration::from_secs(90)),
        )?;
        let mut diagnostics = Vec::new();
        let mut partial = false;
        if client.status["health"] != "ok" {
            partial = true;
            diagnostics.push(Diagnostic::warning(
                "semantic_workspace_health",
                format!("rust-analyzer workspace status: {}", client.status),
                None,
            ));
        }
        // Overlay the complete captured source set so both caller and callee
        // positions refer to this snapshot, not later edits on disk.
        let mut uris = BTreeMap::new();
        for file in source.files.values().filter(|f| f.path.ends_with(".rs")) {
            check_cancelled()?;
            if started.elapsed() >= budget {
                bail!("semantic budget exhausted while opening snapshot files");
            }
            uris.insert(
                file.path.clone(),
                client.open(&source.root.join(&file.path), &file.content)?,
            );
        }
        let nodes: HashMap<&str, &Node> = facts.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut by_position = BTreeMap::<(String, usize), Vec<&Node>>::new();
        for node in &facts.nodes {
            if let Some(position) = node
                .attributes
                .get("definition_start")
                .and_then(|s| s.parse::<usize>().ok())
            {
                by_position
                    .entry((node.evidence.path.clone(), position))
                    .or_default()
                    .push(node);
            }
        }
        let scope_packages: BTreeSet<_> = project
            .packages
            .iter()
            .filter(|p| {
                context.scope.as_ref().is_none_or(|scope| {
                    p.name == *scope || p.name.replace('-', "_") == *scope || p.root == *scope
                })
            })
            .map(|p| p.id.as_str())
            .collect();
        let is_selected = |node: &Node| {
            !node.is_test
                && !node.unit.starts_with("unlinked:")
                && (context.scope.is_none()
                    || scope_packages.contains(node.package.as_str())
                    || context.scope.as_ref().is_some_and(|s| {
                        node.qualified_name.starts_with(s) || node.evidence.path.starts_with(s)
                    }))
        };
        let selected_count = facts
            .edges
            .iter()
            .filter(|e| {
                matches!(e.kind.as_str(), "calls" | "implements")
                    && nodes.get(e.source.as_str()).is_some_and(|n| is_selected(n))
            })
            .count();
        if context.scope.is_some() && selected_count == 0 {
            bail!("the selected scope contains no eligible Rust call or implementation sites");
        }
        let mut requested = 0;
        let mut resolved = 0;
        let mut external = 0;
        let mut errors = 0;
        let mut cache = HashMap::<(String, usize), Value>::new();
        for edge in &mut facts.edges {
            check_cancelled()?;
            if !matches!(edge.kind.as_str(), "calls" | "implements") {
                continue;
            }
            let Some(caller) = nodes.get(edge.source.as_str()) else {
                continue;
            };
            if !is_selected(caller) {
                continue;
            }
            if started.elapsed() >= budget || requested >= context.semantic_request_limit {
                partial = true;
                diagnostics.push(Diagnostic::warning("semantic_budget",format!("Stopped after {requested} requests / {}s; scope has {selected_count} call and implementation sites",started.elapsed().as_secs()),None));
                break;
            }
            let file = &source.files[&edge.evidence.path];
            let position = utf16_position(&file.content, edge.evidence.start_byte)?;
            let key = (file.path.clone(), edge.evidence.start_byte);
            let result = if let Some(result) = cache.get(&key) {
                result.clone()
            } else {
                let params = json!({"textDocument":{"uri":uris[&file.path]},"position":position});
                let timeout = budget
                    .saturating_sub(started.elapsed())
                    .min(Duration::from_secs(15));
                let mut response =
                    client.request("textDocument/definition", params.clone(), timeout);
                // ContentModified/RequestCancelled can occur while the initial
                // overlays settle. Retry in the same live server, within budget.
                for _ in 0..2 {
                    if response
                        .as_ref()
                        .is_err_and(|e| e.to_string().contains("-3280"))
                        && started.elapsed() < budget
                    {
                        response = client.request(
                            "textDocument/definition",
                            params.clone(),
                            budget
                                .saturating_sub(started.elapsed())
                                .min(Duration::from_secs(15)),
                        );
                    } else {
                        break;
                    }
                }
                requested += 1;
                match response {
                    Ok(result) => {
                        cache.insert(key, result.clone());
                        result
                    }
                    Err(error) => {
                        errors += 1;
                        partial = true;
                        if errors <= 5 {
                            diagnostics.push(Diagnostic::warning(
                                "semantic_request_failed",
                                error.to_string(),
                                Some(&file.path),
                            ));
                        }
                        if errors >= 10 {
                            break;
                        }
                        continue;
                    }
                }
            };
            let locations: Vec<&Value> = match &result {
                Value::Array(items) => items.iter().collect(),
                Value::Object(_) => vec![&result],
                _ => Vec::new(),
            };
            let mut targets = BTreeMap::<String, &Node>::new();
            let mut outside = false;
            let method_call = edge.kind == "calls"
                && file.content[..edge.evidence.start_byte]
                    .trim_end()
                    .ends_with('.');
            let reference_name = file.content[edge.evidence.start_byte..edge.evidence.end_byte]
                .trim_start_matches("r#");
            let mut redirected = false;
            for location in locations {
                let Some(uri) = location
                    .get("targetUri")
                    .or_else(|| location.get("uri"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let Ok(path) = Url::parse(uri).and_then(|u| {
                    u.to_file_path()
                        .map_err(|_| url::ParseError::RelativeUrlWithoutBase)
                }) else {
                    outside = true;
                    continue;
                };
                let Ok(relative) = path.strip_prefix(&source.root) else {
                    outside = true;
                    continue;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                let Some(target_file) = source.files.get(&relative) else {
                    outside = true;
                    continue;
                };
                let range = location
                    .get("targetSelectionRange")
                    .or_else(|| location.get("range"));
                let Some(byte) =
                    range.and_then(|r| byte_position(&target_file.content, &r["start"]))
                else {
                    continue;
                };
                if let Some(candidates) = by_position.get(&(relative, byte)) {
                    let compatible: Vec<_> = candidates
                        .iter()
                        .filter(|n| {
                            if edge.kind == "calls" {
                                let callable = n.kind == "function";
                                if callable
                                    && method_call
                                    && n.name.trim_start_matches("r#") != reference_name
                                {
                                    redirected = true;
                                    false
                                } else {
                                    callable
                                }
                            } else {
                                n.kind == "trait"
                            }
                        })
                        .collect();
                    let same_unit: Vec<_> = compatible
                        .iter()
                        .filter(|n| n.unit == caller.unit)
                        .collect();
                    if same_unit.len() == 1 {
                        let node = ***same_unit.first().unwrap();
                        targets.insert(node.id.clone(), node);
                    } else {
                        for node in compatible {
                            targets.insert(node.id.clone(), node);
                        }
                    }
                }
            }
            edge.provider = provider.clone();
            if targets.len() == 1 {
                let target = targets.into_values().next().unwrap();
                edge.target = Some(target.id.clone());
                let parent = target.parent.as_deref().and_then(|id| nodes.get(id));
                edge.resolution =
                    if edge.kind == "calls" && parent.is_some_and(|n| n.kind == "trait") {
                        "interface"
                    } else {
                        "semantic"
                    }
                    .into();
                resolved += 1;
            } else if targets.len() > 1 {
                edge.resolution = "ambiguous".into();
            } else if redirected {
                edge.resolution = "navigation_only".into();
            } else if outside {
                edge.resolution = "external".into();
                external += 1;
            }
            if progress && requested % 100 == 0 && requested > 0 {
                eprintln!(
                    "Rust semantic queries: {requested}; resolved {resolved}; external {external}"
                );
            }
        }
        diagnostics.push(Diagnostic::warning("semantic_scope",format!("Analyzed {requested} unique locations in production code; {resolved} sites resolved, {external} external, {errors} request failures. Tests and macro-generated code are outside this semantic profile."),context.scope.as_deref()));
        Ok(SemanticOutcome {
            capability:Capability{name:"rust_semantics".into(),provider,scope:context.scope.clone().unwrap_or_else(||"workspace production code".into()),limitations:vec![
                "Trait method targets identify an interface, not a unique runtime implementation".into(),
                "Build scripts, proc macros, test configuration and generated code are not executed or expanded".into(),
                "External definitions are boundary relations; missing targets remain unresolved".into(),
                "Redirected method navigation (for example to_string to Display::fmt) is not a direct call edge".into(),
                "Changes in external dependency sources outside this snapshot are not monitored".into(),
            ]}, diagnostics, partial,
        })
    }
}
