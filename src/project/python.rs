//! Captured Python metadata only: no interpreter, setup.py, imports or build hooks.
use super::{join_path, parent, ProjectAdapter};
use crate::model::{identity, CompilationUnit, Dependency, Diagnostic, Package, ProjectModel};
use crate::source::{check_cancelled, SourceSet};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use toml::Value;

pub struct PythonProjectAdapter;

fn under(path: &str, root: &str) -> bool {
    root.is_empty() || path.strip_prefix(root).is_some_and(|s| s.starts_with('/'))
}

fn dependency(raw: &str, kind: &str, optional: bool) -> Option<Dependency> {
    let raw = raw.split(" #").next()?.trim();
    if raw.is_empty() || raw.starts_with('#') || raw.starts_with('-') {
        return None;
    }
    let end = raw
        .find(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.'))
        .unwrap_or(raw.len());
    let name = &raw[..end];
    if name.is_empty() {
        return None;
    }
    Some(Dependency {
        alias: name.into(),
        package: name.into(),
        kind: kind.into(),
        path: None,
        condition: raw.split_once(';').map(|(_, marker)| marker.trim().into()),
        optional,
        resolution: "declared".into(),
    })
}

fn dependencies(value: Option<&Value>, kind: &str, optional: bool, result: &mut Vec<Dependency>) {
    for raw in value.and_then(Value::as_array).into_iter().flatten() {
        if let Some(raw) = raw.as_str() {
            if let Some(dependency) = dependency(raw, kind, optional) {
                result.push(dependency);
            }
        }
    }
}

/// Returns literal INI values only; interpolation and executable configuration are ignored.
fn setup_values(content: &str) -> BTreeMap<(String, String), String> {
    let mut result = BTreeMap::<(String, String), String>::new();
    let mut section = String::new();
    let mut current = None;
    for line in content.lines() {
        let stripped = line.trim();
        if stripped.starts_with('#') || stripped.starts_with(';') || stripped.is_empty() {
            continue;
        }
        if stripped.starts_with('[') && stripped.ends_with(']') {
            section = stripped[1..stripped.len() - 1].into();
            current = None;
        } else if line.starts_with(char::is_whitespace) && current.is_some() {
            result
                .entry(current.clone().unwrap())
                .or_default()
                .push('\n');
            result
                .entry(current.clone().unwrap())
                .or_default()
                .push_str(stripped);
        } else if let Some((key, value)) = stripped.split_once('=') {
            let key = (section.clone(), key.trim().into());
            result.insert(key.clone(), value.trim().into());
            current = Some(key);
        }
    }
    result
}

impl ProjectAdapter for PythonProjectAdapter {
    fn id(&self) -> &str {
        "python-project:1"
    }

    fn detects(&self, source: &SourceSet) -> bool {
        source.files.keys().any(|p| {
            p.ends_with(".py")
                || p.ends_with(".pyi")
                || p == "pyproject.toml"
                || p == "setup.cfg"
                || p == "requirements.txt"
        })
    }

    fn discover(&self, source: &SourceSet) -> Result<ProjectModel> {
        let mut roots = BTreeMap::<String, Option<String>>::new();
        for path in source
            .files
            .keys()
            .filter(|p| p.as_str() == "pyproject.toml" || p.ends_with("/pyproject.toml"))
        {
            roots.insert(parent(path).into(), Some(path.clone()));
        }
        for path in source
            .files
            .keys()
            .filter(|p| p.as_str() == "setup.cfg" || p.ends_with("/setup.cfg"))
        {
            roots.entry(parent(path).into()).or_insert(None);
        }
        if roots.is_empty()
            || source.files.keys().any(|p| {
                (p.ends_with(".py") || p.ends_with(".pyi"))
                    && !roots.keys().any(|root| under(p, root))
            })
        {
            roots.entry(String::new()).or_insert(None);
        }
        let all_roots: Vec<_> = roots.keys().cloned().collect();
        let mut result = ProjectModel {
            kind: if roots.values().all(Option::is_none) {
                "python_loose"
            } else if roots.len() > 1 {
                "python_workspace"
            } else {
                "python_project"
            }
            .into(),
            ..ProjectModel::default()
        };
        for (root, manifest) in roots {
            check_cancelled()?;
            let metadata: Value = if let Some(path) = &manifest {
                toml::from_str(&source.files[path].content)
                    .with_context(|| format!("parse {path}"))?
            } else {
                Value::Table(Default::default())
            };
            let cfg_path = join_path(&root, "setup.cfg").unwrap();
            let cfg = source
                .files
                .get(&cfg_path)
                .map(|f| setup_values(&f.content))
                .unwrap_or_default();
            let project = metadata.get("project");
            let poetry = metadata.get("tool").and_then(|v| v.get("poetry"));
            let fallback_name = if root.is_empty() {
                source
                    .root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("python-source")
            } else {
                root.rsplit('/').next().unwrap_or("python-source")
            };
            let name = project
                .and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                .or_else(|| poetry.and_then(|v| v.get("name")).and_then(Value::as_str))
                .or_else(|| {
                    cfg.get(&("metadata".into(), "name".into()))
                        .map(String::as_str)
                })
                .unwrap_or(fallback_name);
            let id = identity(&["package", "python", &root, name]);
            let owned = |path: &str| {
                under(path, &root)
                    && !all_roots
                        .iter()
                        .any(|other| other.len() > root.len() && under(path, other))
            };
            let py_files: Vec<_> = source
                .files
                .keys()
                .filter(|p| (p.ends_with(".py") || p.ends_with(".pyi")) && owned(p))
                .cloned()
                .collect();
            let src = join_path(&root, "src").unwrap();
            let configured_root = metadata
                .get("tool")
                .and_then(|v| v.get("setuptools"))
                .and_then(|v| v.get("package-dir"))
                .and_then(|v| v.get(""))
                .and_then(Value::as_str)
                .or_else(|| {
                    cfg.get(&("options.packages.find".into(), "where".into()))
                        .map(String::as_str)
                });
            let source_root = if let Some(configured) = configured_root {
                match join_path(&root, configured.trim()) {
                    Some(path) if root.is_empty() || path == root || under(&path, &root) => path,
                    _ => {
                        result.diagnostics.push(Diagnostic::warning("python_source_root_outside",
                            "Configured Python source root is outside the captured package; kept as unknown", manifest.as_deref()));
                        root.clone()
                    }
                }
            } else if py_files.iter().any(|p| under(p, &src)) {
                src
            } else {
                root.clone()
            };
            let mut units = vec![CompilationUnit {
                id: identity(&[&id, "lib", &source_root]),
                name: name.into(),
                kind: "lib".into(),
                source: source_root,
                required_features: Vec::new(),
            }];
            let mut test_roots = BTreeSet::new();
            for path in &py_files {
                let relative = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .trim_start_matches('/');
                let components: Vec<_> = relative.split('/').collect();
                if let Some(position) = components
                    .iter()
                    .position(|s| matches!(*s, "test" | "tests"))
                {
                    test_roots
                        .insert(join_path(&root, &components[..=position].join("/")).unwrap());
                } else if path
                    .rsplit('/')
                    .next()
                    .is_some_and(|s| s.starts_with("test_") || s.ends_with("_test.py"))
                {
                    test_roots.insert(path.clone());
                }
                if path.ends_with("/__main__.py") || path == "__main__.py" {
                    units.push(CompilationUnit {
                        id: identity(&[&id, "bin", path]),
                        name: path.clone(),
                        kind: "bin".into(),
                        source: path.clone(),
                        required_features: Vec::new(),
                    });
                }
            }
            for path in test_roots {
                units.push(CompilationUnit {
                    id: identity(&[&id, "test", &path]),
                    name: path.clone(),
                    kind: "test".into(),
                    source: path,
                    required_features: Vec::new(),
                });
            }
            let mut declared = Vec::new();
            dependencies(
                project.and_then(|v| v.get("dependencies")),
                "normal",
                false,
                &mut declared,
            );
            if let Some(groups) = project
                .and_then(|v| v.get("optional-dependencies"))
                .and_then(Value::as_table)
            {
                for (group, values) in groups {
                    dependencies(
                        Some(values),
                        &format!("optional:{group}"),
                        true,
                        &mut declared,
                    );
                }
            }
            if let Some(groups) = metadata.get("dependency-groups").and_then(Value::as_table) {
                for (group, values) in groups {
                    dependencies(Some(values), &format!("group:{group}"), true, &mut declared);
                }
            }
            if let Some(values) = poetry
                .and_then(|v| v.get("dependencies"))
                .and_then(Value::as_table)
            {
                for name in values.keys().filter(|name| name.as_str() != "python") {
                    if let Some(dep) = dependency(name, "normal", false) {
                        declared.push(dep);
                    }
                }
            }
            if let Some(raw) = cfg.get(&("options".into(), "install_requires".into())) {
                declared.extend(
                    raw.lines()
                        .filter_map(|line| dependency(line, "normal", false)),
                );
            }
            for (path, file) in source.files.iter().filter(|(p, _)| {
                owned(p)
                    && p.rsplit('/')
                        .next()
                        .is_some_and(|s| s.starts_with("requirements") && s.ends_with(".txt"))
            }) {
                for line in file.content.lines() {
                    if let Some(dep) = dependency(line, "requirements", false) {
                        declared.push(dep);
                    }
                    if line.trim_start().starts_with('-') {
                        result.diagnostics.push(Diagnostic::warning("python_requirement_directive",
                            "Requirements directives are retained as evidence; includes, indexes and editable installs are not executed or fetched", Some(path)));
                    }
                }
            }
            declared.sort_by(|a, b| {
                (&a.alias, &a.kind, &a.condition).cmp(&(&b.alias, &b.kind, &b.condition))
            });
            declared.dedup_by(|a, b| {
                a.alias == b.alias && a.kind == b.kind && a.condition == b.condition
            });
            if project.and_then(|v| v.get("dynamic")).is_some()
                || source
                    .files
                    .contains_key(&join_path(&root, "setup.py").unwrap())
            {
                result.diagnostics.push(Diagnostic::warning("python_dynamic_metadata",
                    "Dynamic package metadata and setup.py are not evaluated; source discovery is static", manifest.as_deref()));
            }
            result.packages.push(Package {
                id,
                name: name.into(),
                root,
                language: "python".into(),
                edition: project
                    .and_then(|v| v.get("requires-python"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                units,
                dependencies: declared,
                features: BTreeMap::new(),
            });
        }
        Ok(result)
    }
}
