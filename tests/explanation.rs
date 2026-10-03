#![cfg(unix)]

use codexis::{
    analysis,
    explanation::{self, ContextEvidence, EvidenceKind, ExplanationContext, Provider},
    index::Index,
    model::{AnalysisContext, Evidence},
    source::{SourceProvider, WorkingTreeSource},
};
use serde_json::{json, Value};
use std::{
    fs,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

fn claim(text: &str, basis: &str, evidence: &str) -> Value {
    json!({"text":text,"basis":basis,"evidence_ids":[evidence]})
}

fn file_evidence(path: &str, hash: &str, content: &str) -> Evidence {
    Evidence {
        path: path.into(),
        content_hash: hash.into(),
        start_byte: 0,
        end_byte: content.len(),
        start_line: 1,
        start_column: 0,
        end_line: content.bytes().filter(|b| *b == b'\n').count() + 1,
        end_column: content
            .rsplit_once('\n')
            .map_or(content.len(), |(_, tail)| tail.len()),
    }
}

#[test]
fn snapshot_provider_lifecycle_references_cache_and_cancellation_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='sample'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
    )
    .unwrap();
    let body = "pub fn request() -> i32 { engine() } // SNAPSHOT_ONLY_SENTINEL\npub fn engine() -> i32 { 1 }\n";
    fs::write(root.path().join("src/lib.rs"), body).unwrap();
    fs::write(
        root.path().join("README.md"),
        "Produces a request result.\n",
    )
    .unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let source = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let snapshot = analysis::analyze(&mut index, &source, AnalysisContext::rust(), false).unwrap();
    let hashes = index.file_hashes(&snapshot.id).unwrap();
    let context = ExplanationContext {
        snapshot_id: snapshot.id.clone(),
        locale: "zh".into(),
        project_name: "sample".into(),
        evidence: [
            ("source-id", "src/lib.rs", EvidenceKind::Source),
            ("doc-id", "README.md", EvidenceKind::Declared),
        ]
        .into_iter()
        .map(|(id, path, kind)| {
            let content = index.content(&hashes[path]).unwrap();
            ContextEvidence {
                id: id.into(),
                evidence: file_evidence(path, &hashes[path], &content),
                content,
                kind,
            }
        })
        .collect(),
        facts: json!({"reading_guide":"Follow request to engine; this is only a candidate."}),
    };
    let fact = claim("request calls engine", "source", "source-id#L1-L1");
    let interpretation = claim(
        "This capability returns a result",
        "interpretation",
        "source-id#L1-L2",
    );
    let response = json!({
        "purpose":claim("Produces a request result", "declared", "doc-id#L1-L1"),
        "scenario":{"goal":interpretation,"input":fact,"output":fact,
            "steps":[
                {"title":fact,"input":fact,"output":fact,"responsibility":interpretation},
                {"title":fact,"input":fact,"output":fact,"responsibility":interpretation},
                {"title":fact,"input":fact,"output":fact,"responsibility":interpretation}
            ]},
        "key_state":fact,"boundary":interpretation,
        "reading":{"target":fact,"why":interpretation},
        "questions":[claim("Is the return result consumed by an external caller?", "interpretation", "source-id#L1-L2")]
    });
    let counter = fixture.path().join("calls.txt");
    let fixture_source = format!(
        r###"
use std::{{fs, io::{{self, Read}}, path::PathBuf, thread, time::Duration}};
fn main() {{
    let args: Vec<String> = std::env::args().collect();
    assert_eq!(args[1], "exec");
    assert!(args.iter().any(|s| s == "read-only"));
    assert!(args.iter().any(|s| s == "--ephemeral"));
    assert!(args.iter().any(|s| s == "--ignore-user-config"));
    assert!(args.iter().any(|s| s == "--ignore-rules"));
    assert!(args.iter().any(|s| s == "web_search=\"disabled\""));
    assert!(args.iter().any(|s| s == "features.apps=false"));
    assert!(args.iter().any(|s| s == "features.plugins=false"));
    assert!(!args.iter().any(|s| s == "features.shell_tool=false"));
    assert_ne!(std::env::current_dir().unwrap(), PathBuf::from({live_root:?}));
    assert_eq!(fs::read_to_string("source/src/lib.rs").unwrap(), {body:?});
    let mut prompt = String::new(); io::stdin().read_to_string(&mut prompt).unwrap();
    assert!(prompt.contains("UNTRUSTED MATERIAL"));
    assert!(prompt.contains("source-id"));
    assert!(!prompt.contains("SNAPSHOT_ONLY_SENTINEL"));
    assert!(!prompt.contains({live_root:?}));
    let schema = &args[args.iter().position(|s| s == "--output-schema").unwrap()+1];
    assert!(fs::read_to_string(schema).unwrap().contains("responsibility"));
    let output = &args[args.iter().position(|s| s == "--output-last-message").unwrap()+1];
    let model = &args[args.iter().position(|s| s == "--model").unwrap()+1];
    let counter = PathBuf::from({counter:?});
    let calls: u32 = fs::read_to_string(&counter).unwrap_or("0".into()).parse().unwrap();
    fs::write(counter, (calls+1).to_string()).unwrap();
    if model == "timeout" || model == "cancel" {{
        fs::write(PathBuf::from({fixture_dir:?}).join(format!("{{model}}.pid")), std::process::id().to_string()).unwrap();
        thread::sleep(Duration::from_secs(20));
    }}
    let response = {response:?};
    fs::write(output, if model == "bad" {{ response.replace("source-id", "foreign-id") }} else {{ response.to_string() }}).unwrap();
}}
"###,
        live_root = root.path().to_str().unwrap(),
        body = body,
        counter = counter.to_str().unwrap(),
        fixture_dir = fixture.path().to_str().unwrap(),
        response = response.to_string()
    );
    let fixture_path = fixture.path().join("provider.rs");
    let program = fixture.path().join("provider");
    fs::write(&fixture_path, fixture_source).unwrap();
    let compile = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("--edition=2021")
        .arg(&fixture_path)
        .arg("-o")
        .arg(&program)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    // Live sources can diverge; the provider must continue to read snapshot bytes.
    fs::write(
        root.path().join("src/lib.rs"),
        "pub fn live_source_changed() {}\n",
    )
    .unwrap();
    let mut provider = Provider {
        program,
        model: Some("normal".into()),
        timeout: Duration::from_secs(3),
    };
    let cancel = AtomicBool::new(false);
    assert!(explanation::load(&index, &snapshot.id, "zh")
        .unwrap()
        .is_none());
    let generated = explanation::generate(&index, &context, &provider, &cancel).unwrap();
    assert_eq!(generated.scenario.steps.len(), 3);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "1");
    explanation::generate(&index, &context, &provider, &cancel).unwrap();
    assert_eq!(
        fs::read_to_string(&counter).unwrap(),
        "1",
        "same provider/context reuses cache"
    );
    assert!(explanation::load(&index, &snapshot.id, "zh")
        .unwrap()
        .is_some());
    assert!(explanation::load(&index, &snapshot.id, "en")
        .unwrap()
        .is_none());
    provider.model = Some("other-model".into());
    explanation::generate(&index, &context, &provider, &cancel).unwrap();
    assert_eq!(
        fs::read_to_string(&counter).unwrap(),
        "2",
        "model identity separates cache entries"
    );
    let precise = explanation::resolve_evidence(&context, "source-id#L1-L1").unwrap();
    assert_eq!(
        &body[precise.start_byte..precise.end_byte],
        body.lines().next().unwrap().to_owned() + "\n"
    );
    assert!(explanation::resolve_evidence(&context, "source-id#L3-L3").is_err());
    assert!(explanation::resolve_evidence(&context, "source-id#L100-L102").is_err());
    provider.model = Some("bad".into());
    assert!(explanation::generate(&index, &context, &provider, &cancel)
        .unwrap_err()
        .to_string()
        .contains("outside"));
    provider.model = Some("timeout".into());
    provider.timeout = Duration::from_millis(150);
    let started = Instant::now();
    assert!(explanation::generate(&index, &context, &provider, &cancel)
        .unwrap_err()
        .to_string()
        .contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_reaped(&fixture.path().join("timeout.pid"));
    provider.model = Some("cancel".into());
    provider.timeout = Duration::from_secs(3);
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    let marker = fixture.path().join("cancel.pid");
    let waiter = thread::spawn(move || {
        let started = Instant::now();
        while !marker.exists() && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(5));
        }
        signal.store(true, Ordering::Relaxed);
    });
    let started = Instant::now();
    assert!(explanation::generate(&index, &context, &provider, &cancel)
        .unwrap_err()
        .to_string()
        .contains("cancelled"));
    waiter.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_reaped(&fixture.path().join("cancel.pid"));
    let count: u64 = index
        .connection
        .query_row("SELECT COUNT(*) FROM explanation_cache", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "invalid, timeout, and cancelled results cannot publish"
    );
    let rows: Vec<(String, String)> = index
        .connection
        .prepare("SELECT cache_key,data FROM explanation_cache")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for (key, data) in rows {
        let mut stored: Value = serde_json::from_str(&data).unwrap();
        stored["context"]["evidence"][0]["content"] = json!("tampered snapshot");
        index
            .connection
            .execute(
                "UPDATE explanation_cache SET data=?1 WHERE cache_key=?2",
                rusqlite::params![stored.to_string(), key],
            )
            .unwrap();
    }
    assert!(
        explanation::load(&index, &snapshot.id, "zh")
            .unwrap()
            .is_none(),
        "cached provenance is revalidated"
    );
}

fn assert_reaped(marker: &std::path::Path) {
    let pid = fs::read_to_string(marker).unwrap();
    assert!(
        !Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "provider child must be killed and reaped"
    );
}
