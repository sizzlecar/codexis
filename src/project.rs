use crate::model::{identity, CompilationUnit, Dependency, Diagnostic, Package, ProjectModel};
use crate::source::SourceSet;
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSetBuilder};
use std::collections::BTreeMap;
use toml::Value;

pub mod python;

pub trait ProjectAdapter {
    fn id(&self) -> &str;
    fn detects(&self, source: &SourceSet) -> bool;
    fn discover(&self, source: &SourceSet) -> Result<ProjectModel>;
}

pub struct CargoAdapter;

/// Selects Cargo when present and preserves an explicit loose-source fallback.
pub struct RustProjectAdapter;

impl ProjectAdapter for RustProjectAdapter {
    fn id(&self) -> &str {
        "rust-project:2"
    }
    fn detects(&self, source: &SourceSet) -> bool {
        CargoAdapter.detects(source) || source.files.keys().any(|p| p.ends_with(".rs"))
    }
    fn discover(&self, source: &SourceSet) -> Result<ProjectModel> {
        discover(source)
    }
}

pub fn join_path(base: &str, relative: &str) -> Option<String> {
    if relative.starts_with('/') || relative.contains('\\') {
        return None;
    }
    let mut parts = Vec::new();
    for part in base.split('/').chain(relative.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

impl ProjectAdapter for CargoAdapter {
    fn id(&self) -> &str {
        "cargo:2"
    }

    fn detects(&self, source: &SourceSet) -> bool {
        source.files.contains_key("Cargo.toml")
    }

    fn discover(&self, source: &SourceSet) -> Result<ProjectModel> {
        let root_file = source
            .files
            .get("Cargo.toml")
            .context("Cargo.toml not found")?;
        let root: Value = toml::from_str(&root_file.content).context("parse root Cargo.toml")?;
        let workspace = root.get("workspace");
        let mut manifests = Vec::new();
        if root.get("package").is_some() {
            manifests.push("Cargo.toml".to_owned());
        }
        if let Some(ws) = workspace {
            let members = strings(ws.get("members"));
            let excluded = strings(ws.get("exclude"));
            let mut includes = GlobSetBuilder::new();
            let mut excludes = GlobSetBuilder::new();
            for pattern in &members {
                includes.add(Glob::new(pattern.trim_end_matches('/'))?);
            }
            for pattern in &excluded {
                excludes.add(Glob::new(pattern.trim_end_matches('/'))?);
            }
            let includes = includes.build()?;
            let excludes = excludes.build()?;
            for path in source.files.keys().filter(|p| p.ends_with("/Cargo.toml")) {
                let directory = parent(path);
                if includes.is_match(directory) && !excludes.is_match(directory) {
                    manifests.push(path.clone());
                }
            }
            for member in members.iter().filter(|m| !m.contains(['*', '?', '['])) {
                if !excludes.is_match(member)
                    && !source
                        .files
                        .contains_key(&format!("{}/Cargo.toml", member.trim_end_matches('/')))
                {
                    bail!("workspace member manifest missing or ignored: {member}/Cargo.toml");
                }
            }
        }
        manifests.sort();
        manifests.dedup();
        let mut result = ProjectModel {
            kind: if workspace.is_some() {
                "cargo_workspace"
            } else {
                "cargo_package"
            }
            .into(),
            ..ProjectModel::default()
        };
        for manifest in manifests {
            let file = &source.files[&manifest];
            let value: Value =
                toml::from_str(&file.content).with_context(|| format!("parse {manifest}"))?;
            let Some(package) = value.get("package") else {
                continue;
            };
            let name = package
                .get("name")
                .and_then(Value::as_str)
                .with_context(|| format!("missing package.name in {manifest}"))?;
            let package_root = parent(&manifest);
            let id = identity(&["package", "rust", package_root, name]);
            let edition = package
                .get("edition")
                .and_then(Value::as_str)
                .or_else(|| {
                    package
                        .get("edition")
                        .filter(|v| v.get("workspace").and_then(Value::as_bool) == Some(true))
                        .and_then(|_| workspace?.get("package")?.get("edition")?.as_str())
                })
                .unwrap_or("2015")
                .to_owned();
            let units = discover_units(source, &value, package_root, &id, name, &edition)?;
            let mut dependencies = Vec::new();
            for (key, kind) in [
                ("dependencies", "normal"),
                ("dev-dependencies", "dev"),
                ("build-dependencies", "build"),
            ] {
                collect_dependencies(
                    value.get(key),
                    workspace,
                    package_root,
                    kind,
                    None,
                    &mut dependencies,
                    &mut result.diagnostics,
                );
                if let Some(targets) = value.get("target").and_then(Value::as_table) {
                    for (condition, target) in targets {
                        collect_dependencies(
                            target.get(key),
                            workspace,
                            package_root,
                            kind,
                            Some(condition),
                            &mut dependencies,
                            &mut result.diagnostics,
                        );
                    }
                }
            }
            let features = value
                .get("features")
                .and_then(Value::as_table)
                .map(|t| {
                    t.iter()
                        .map(|(k, v)| (k.clone(), strings(Some(v))))
                        .collect()
                })
                .unwrap_or_default();
            result.packages.push(Package {
                id,
                name: name.into(),
                root: package_root.into(),
                language: "rust".into(),
                edition,
                units,
                dependencies,
                features,
            });
        }
        if result.packages.is_empty() {
            bail!("no Cargo packages found in the selected workspace");
        }
        Ok(result)
    }
}

fn collect_dependencies(
    value: Option<&Value>,
    workspace: Option<&Value>,
    root: &str,
    kind: &str,
    condition: Option<&str>,
    output: &mut Vec<Dependency>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(table) = value.and_then(Value::as_table) else {
        return;
    };
    for (alias, declared) in table {
        let inherited = declared.get("workspace").and_then(Value::as_bool) == Some(true);
        let definition = if inherited {
            let Some(value) = workspace
                .and_then(|w| w.get("dependencies"))
                .and_then(|d| d.get(alias))
            else {
                diagnostics.push(Diagnostic::warning(
                    "workspace_dependency_missing",
                    format!("workspace dependency {alias} could not be inherited"),
                    Some(root),
                ));
                continue;
            };
            value
        } else {
            declared
        };
        let path = definition.get("path").and_then(Value::as_str).map(|p| {
            join_path(if inherited { "" } else { root }, p)
                .unwrap_or_else(|| format!("external:{p}"))
        });
        output.push(Dependency {
            alias: alias.clone(),
            package: definition
                .get("package")
                .and_then(Value::as_str)
                .unwrap_or(alias)
                .into(),
            kind: kind.into(),
            path,
            condition: condition.map(str::to_owned),
            optional: declared
                .get("optional")
                .or_else(|| definition.get("optional"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            resolution: "declared".into(),
        });
    }
}

fn discover_units(
    source: &SourceSet,
    value: &Value,
    root: &str,
    package: &str,
    name: &str,
    edition: &str,
) -> Result<Vec<CompilationUnit>> {
    let automatic = |flag: &str| {
        value
            .get("package")
            .and_then(|p| p.get(flag))
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                let kind = flag.trim_start_matches("auto").trim_end_matches('s');
                edition != "2015" || value.get(kind).is_none()
            })
    };
    let mut units = BTreeMap::<String, CompilationUnit>::new();
    let mut add = |kind: &str,
                   unit_name: &str,
                   relative: &str,
                   required_features: Vec<String>|
     -> Result<()> {
        let path = join_path(root, relative)
            .with_context(|| format!("target path outside source root: {relative}"))?;
        if source.files.contains_key(&path) {
            let id = identity(&[package, kind, unit_name, &path]);
            units.insert(
                format!("{kind}:{unit_name}"),
                CompilationUnit {
                    id,
                    name: if kind == "lib" {
                        unit_name.replace('-', "_")
                    } else {
                        unit_name.into()
                    },
                    kind: kind.into(),
                    source: path,
                    required_features,
                },
            );
        }
        Ok(())
    };
    let lib = value.get("lib");
    if lib.is_some() || automatic("autolib") {
        add(
            "lib",
            lib.and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(name),
            lib.and_then(|v| v.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("src/lib.rs"),
            Vec::new(),
        )?;
    }
    let bins = value.get("bin").and_then(Value::as_array);
    let main_is_explicit = bins.into_iter().flatten().any(|bin| {
        bin.get("path").and_then(Value::as_str) == Some("src/main.rs")
            || (bin.get("path").is_none() && bin.get("name").and_then(Value::as_str) == Some(name))
    });
    if automatic("autobins") && !main_is_explicit {
        add("bin", name, "src/main.rs", Vec::new())?;
    }
    for (table_name, kind, directory, auto_flag) in [
        ("bin", "bin", "src/bin", "autobins"),
        ("test", "test", "tests", "autotests"),
        ("bench", "bench", "benches", "autobenches"),
        ("example", "example", "examples", "autoexamples"),
    ] {
        if automatic(auto_flag) {
            let directory_path = join_path(root, directory).context("invalid source directory")?;
            let prefix = format!("{directory_path}/");
            for path in source.files.keys().filter(|p| p.starts_with(&prefix)) {
                let suffix = &path[prefix.len()..];
                if !suffix.contains('/') && suffix.ends_with(".rs") {
                    add(
                        kind,
                        suffix.trim_end_matches(".rs"),
                        &format!("{directory}/{suffix}"),
                        Vec::new(),
                    )?;
                } else if suffix.ends_with("/main.rs") && suffix.matches('/').count() == 1 {
                    add(
                        kind,
                        suffix.trim_end_matches("/main.rs"),
                        &format!("{directory}/{suffix}"),
                        Vec::new(),
                    )?;
                }
            }
        }
        let entries = if table_name == "bin" {
            bins
        } else {
            value.get(table_name).and_then(Value::as_array)
        };
        for unit in entries.into_iter().flatten() {
            let unit_name = unit.get("name").and_then(Value::as_str).unwrap_or(name);
            let mut default_path = if kind == "bin" && unit_name == name {
                "src/main.rs".to_owned()
            } else {
                format!("{directory}/{unit_name}.rs")
            };
            if !source
                .files
                .contains_key(&join_path(root, &default_path).unwrap_or_default())
            {
                let nested = format!("{directory}/{unit_name}/main.rs");
                if source
                    .files
                    .contains_key(&join_path(root, &nested).unwrap_or_default())
                {
                    default_path = nested;
                }
            }
            add(
                kind,
                unit_name,
                unit.get("path")
                    .and_then(Value::as_str)
                    .unwrap_or(&default_path),
                strings(unit.get("required-features")),
            )?;
        }
    }
    match value.get("package").and_then(|p| p.get("build")) {
        Some(Value::Boolean(false)) => {}
        Some(Value::String(path)) => add("build", "build_script", path, Vec::new())?,
        _ => add("build", "build_script", "build.rs", Vec::new())?,
    }
    Ok(units.into_values().collect())
}

pub fn discover(source: &SourceSet) -> Result<ProjectModel> {
    if CargoAdapter.detects(source) {
        return CargoAdapter.discover(source);
    }
    let name = source
        .root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let package_id = identity(&["loose", name]);
    Ok(ProjectModel {
        kind: "source_directory".into(),
        packages: vec![Package {
            id: package_id,
            name: name.into(),
            root: "".into(),
            language: "rust".into(),
            edition: "2021".into(),
            units: Vec::new(),
            dependencies: Vec::new(),
            features: BTreeMap::new(),
        }],
        diagnostics: vec![Diagnostic::warning(
            "no_project_manifest",
            "No Cargo.toml; scanning Rust source without a Cargo build context",
            None,
        )],
    })
}
