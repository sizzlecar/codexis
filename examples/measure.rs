//! Local Unix performance harness. It samples only the launched process tree.
//! Usage: measure METRICS.json PROGRAM [ARGS...]
//! RSS sampling is approximate (100 ms), not an OS-enforced memory limit.
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write,
    process::Command,
    thread,
    time::{Duration, Instant},
};

fn sample(root: u32) -> Result<(u64, u64, BTreeMap<u32, u64>)> {
    let result = Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss="])
        .output()?;
    if !result.status.success() {
        bail!("ps sampling failed");
    }
    let processes: Vec<(u32, u32, u64)> = String::from_utf8(result.stdout)?
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((
                fields.next()?.parse().ok()?,
                fields.next()?.parse().ok()?,
                fields.next()?.parse().ok()?,
            ))
        })
        .collect();
    let mut descendants = BTreeSet::from([root]);
    loop {
        let count = descendants.len();
        for (pid, parent, _) in &processes {
            if descendants.contains(parent) {
                descendants.insert(*pid);
            }
        }
        if count == descendants.len() {
            break;
        }
    }
    let mut own = 0;
    let mut children = BTreeMap::new();
    for (pid, _, rss) in processes {
        if pid == root {
            own = rss;
        } else if descendants.contains(&pid) {
            children.insert(pid, rss);
        }
    }
    Ok((own, children.values().sum(), children))
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        bail!("usage: measure METRICS.json PROGRAM [ARGS...]");
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[0])
        .context("create new metrics file")?;
    let started = Instant::now();
    let mut child = Command::new(&args[1]).args(&args[2..]).spawn()?;
    let mut peak_own = 0;
    let mut peak_children = 0;
    let mut peak_tree = 0;
    let mut peaks = BTreeMap::<u32, u64>::new();
    let mut samples = 0;
    let mut sampling_errors = 0;
    let status = loop {
        match sample(child.id()) {
            Ok((own, children, rss)) => {
                samples += 1;
                peak_own = peak_own.max(own);
                peak_children = peak_children.max(children);
                peak_tree = peak_tree.max(own + children);
                for (pid, memory) in rss {
                    peaks
                        .entry(pid)
                        .and_modify(|v| *v = (*v).max(memory))
                        .or_insert(memory);
                }
            }
            Err(_) => sampling_errors += 1,
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(100));
    };
    let metrics = json!({
        "command": &args[1..], "elapsed_ms": started.elapsed().as_millis(),
        "exit_code":status.code(), "peak_codexis_rss_kib_sampled":peak_own,
        "peak_children_sum_rss_kib_sampled":peak_children,"peak_tree_rss_kib_sampled":peak_tree,
        "child_peak_rss_kib_sampled":peaks,"samples":samples,"sampling_errors":sampling_errors,
        "method":"ps process-tree RSS sampled every ~100 ms; wall time includes sampling and polling overhead; transient peaks may be missed"
    });
    writeln!(output, "{}", serde_json::to_string_pretty(&metrics)?)?;
    std::process::exit(status.code().unwrap_or(1))
}
