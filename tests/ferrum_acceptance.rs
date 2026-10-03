//! Opt-in assertions over reports from a real, read-only Ferrum analysis.
use codexis::model::digest;
use serde_json::Value;
use std::{fs, path::Path};

fn report(directory: &Path, name: &str) -> Value {
    serde_json::from_slice(&fs::read(directory.join(name)).unwrap()).unwrap()
}

fn evidence(root: &Path, item: &Value) {
    let e = &item["evidence"];
    let source = fs::read_to_string(root.join(e["path"].as_str().unwrap())).unwrap();
    assert_eq!(digest(&source), e["content_hash"].as_str().unwrap());
    let start = e["start_byte"].as_u64().unwrap() as usize;
    let end = e["end_byte"].as_u64().unwrap() as usize;
    assert!(source.get(start..end).is_some());
    assert_eq!(
        source.as_bytes()[..start]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1,
        e["start_line"].as_u64().unwrap() as usize
    );
}

fn target<'a>(trace: &'a Value, name: &str, resolution: &str) -> &'a Value {
    let edge = trace["data"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["target_name"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .collect::<String>()
                == name
        })
        .unwrap();
    assert_eq!(edge["resolution"], resolution, "{edge}");
    trace["data"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == edge["target"])
        .unwrap()
}

fn time_and_memory(directory: &Path, file: &str) -> (f64, u64) {
    let text = fs::read_to_string(directory.join(file)).unwrap();
    let seconds = text
        .lines()
        .find(|l| l.contains(" real "))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let memory = text
        .lines()
        .find(|l| l.contains("maximum resident set size"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    (seconds, memory)
}

#[test]
#[ignore = "requires CODEXIS_FERRUM_ROOT and CODEXIS_FERRUM_ARTIFACTS from the local validation harness"]
fn real_ferrum_reports_have_supported_relationships_immutable_evidence_and_bounded_costs() {
    let root = std::env::var_os("CODEXIS_FERRUM_ROOT").expect("set CODEXIS_FERRUM_ROOT");
    let root = Path::new(&root);
    let artifacts =
        std::env::var_os("CODEXIS_FERRUM_ARTIFACTS").expect("set CODEXIS_FERRUM_ARTIFACTS");
    let artifacts = Path::new(&artifacts);
    let cold = report(artifacts, "cold.json");
    assert_eq!(cold["completeness"]["status"], "complete");
    assert_eq!(
        cold["data"]["stats"]["parsed_files"],
        cold["data"]["stats"]["source_files"]
    );
    assert_eq!(cold["data"]["stats"]["failed_files"], 0);
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("Cargo.toml")).unwrap()).unwrap();
    assert_eq!(
        cold["data"]["project"]["packages"]
            .as_array()
            .unwrap()
            .len(),
        manifest["workspace"]["members"].as_array().unwrap().len()
    );
    assert!(cold["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|n| n["is_test"] == false));
    assert!(cold["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["qualified_name"] == "ferrum::main"));
    let warm = report(artifacts, "warm.json");
    assert_eq!(warm["data"]["stats"]["parsed_files"], 0);
    assert_eq!(
        warm["data"]["stats"]["reused_files"],
        warm["data"]["stats"]["source_files"]
    );
    let cli = report(artifacts, "cli-trace.json");
    assert!(target(&cli, "run::execute", "semantic")["evidence"]["path"]
        .as_str()
        .unwrap()
        .ends_with("/commands/run.rs"));
    assert!(
        target(&cli, "serve::execute_cli", "semantic")["evidence"]["path"]
            .as_str()
            .unwrap()
            .ends_with("/commands/serve.rs")
    );
    let chat = report(artifacts, "chat-trace.json");
    assert_eq!(
        target(&chat, "engine.infer", "interface")["qualified_name"],
        "ferrum_interfaces::engine::LlmInferenceEngine::infer"
    );
    let redirected = chat["data"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["target_name"] == "inference_request.id.to_string")
        .unwrap();
    assert_eq!(redirected["resolution"], "navigation_only");
    assert!(redirected["target"].is_null());
    let prefix = report(artifacts, "prefix-trace.json");
    for name in [
        "self.scheduler.prepare_prefix_restore",
        "self.scheduler.commit_prefix_restored",
    ] {
        assert!(target(&prefix, name, "semantic")["evidence"]["path"]
            .as_str()
            .unwrap()
            .starts_with("crates/ferrum-scheduler/"));
    }
    assert_eq!(
        target(
            &prefix,
            "self.model_executor.try_restore_plan_runtime_prefix",
            "interface"
        )["qualified_name"],
        "ferrum_interfaces::model_executor::ModelExecutor::try_restore_plan_runtime_prefix"
    );
    for trace in [&cli, &chat, &prefix] {
        for node in trace["data"]["nodes"].as_array().unwrap() {
            evidence(root, node);
        }
        for edge in trace["data"]["edges"].as_array().unwrap() {
            evidence(root, edge);
        }
    }
    let implementation = report(artifacts, "engine-impl.json");
    let interface = report(artifacts, "inspect.json");
    assert!(implementation["data"]["outgoing"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "implements"
            && e["resolution"] == "semantic"
            && e["target"] == interface["data"]["node"]["id"]));
    let history = report(artifacts, "history.json");
    assert_ne!(
        history["data"]["base_revision"],
        history["data"]["head_revision"]
    );
    assert!(history["data"]["total_changes"].as_u64().unwrap() > 0);
    assert!(history["data"]["impact_analysis"]
        .as_str()
        .unwrap()
        .starts_with("not computed"));
    for name in ["server.json", "prefix.json", "cli.json", "engine.json"] {
        assert_eq!(
            report(artifacts, name)["completeness"]["status"],
            "complete",
            "{name}"
        );
    }
    assert_eq!(
        fs::read(artifacts.join("git-status-before.bin")).unwrap(),
        fs::read(artifacts.join("git-status-after.bin")).unwrap()
    );
    let (seconds, bytes) = time_and_memory(artifacts, "cold.time.txt");
    assert!(
        seconds <= 120.0 && bytes <= 2 * 1024 * 1024 * 1024,
        "{seconds}s, {bytes} bytes"
    );
    for name in ["map.time.txt", "inspect.time.txt"] {
        assert!(time_and_memory(artifacts, name).0 <= 2.0, "{name}");
    }
    let metrics = report(artifacts, "server.metrics.json");
    assert_eq!(metrics["sampling_errors"], 0);
    assert!(
        metrics["peak_children_sum_rss_kib_sampled"]
            .as_u64()
            .unwrap()
            > 0
    );
}
