//! Real macOS PTY interaction, including raw-mode and alternate-screen cleanup.
#![cfg(target_os = "macos")]

use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct TerminalSession {
    child: Child,
    input: Option<ChildStdin>,
    output: Receiver<Vec<u8>>,
    readers: Vec<JoinHandle<()>>,
    transcript: Vec<u8>,
    deadline: Instant,
    finished: bool,
}

impl TerminalSession {
    fn launch(project: &Path, cache: &Path, locale: &str) -> Self {
        // script creates the PTY; this shell only fixes its dimensions and
        // compares the terminal state before/after the actual Rust executable.
        let wrapper = "stty rows 36 cols 140; task_saved=$(stty -g); \"$@\"; task_code=$?; task_restored=$(stty -g); if [ \"$task_saved\" != \"$task_restored\" ]; then printf '\\n[terminal-mode-changed]\\n'; exit 99; fi; printf '\\n[terminal-restored:%s]\\n' \"$task_code\"; exit \"$task_code\"";
        let mut command = Command::new("/usr/bin/script");
        command
            .args(["-q", "/dev/null", "/bin/sh", "-c", wrapper, "codexis-pty"])
            .arg(env!("CARGO_BIN_EXE_codexis"))
            .arg("analyze")
            .arg(project)
            .args(["--locale", locale, "--cache-dir"])
            .arg(cache)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = command
            .spawn()
            .expect("macOS /usr/bin/script must provide the test PTY");
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, output) = mpsc::channel();
        let read = |mut stream: Box<dyn Read + Send>, sender: mpsc::Sender<Vec<u8>>| {
            thread::spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) if sender.send(buffer[..count].to_vec()).is_err() => break,
                        Ok(_) => {}
                    }
                }
            })
        };
        let readers = vec![
            read(Box::new(stdout), sender.clone()),
            read(Box::new(stderr), sender),
        ];
        Self {
            child,
            input,
            output,
            readers,
            transcript: Vec::new(),
            deadline: Instant::now() + Duration::from_secs(30),
            finished: false,
        }
    }

    fn wait_for(&mut self, start: usize, expected: &[&str]) {
        loop {
            let text = String::from_utf8_lossy(&self.transcript[start..]);
            if expected.iter().all(|needle| text.contains(needle)) {
                return;
            }
            assert!(
                Instant::now() < self.deadline,
                "PTY did not display {expected:?}; transcript tail:\n{}",
                self.tail()
            );
            match self.output.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => self.transcript.extend(chunk),
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "PTY ended before {expected:?}; transcript tail:\n{}",
                    self.tail()
                ),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn keys(&mut self, keys: &str, expected: &[&str]) {
        let start = self.transcript.len();
        let input = self.input.as_mut().unwrap();
        input.write_all(keys.as_bytes()).unwrap();
        input.flush().unwrap();
        self.wait_for(start, expected);
    }

    fn tail(&self) -> String {
        let start = self.transcript.len().saturating_sub(4_000);
        String::from_utf8_lossy(&self.transcript[start..]).into_owned()
    }

    fn screen_text(&self) -> String {
        // Each actual redraw clears the screen. Inspect the most recent frame,
        // so earlier navigation cannot make assertions about Home pass.
        let text = String::from_utf8_lossy(&self.transcript);
        let frame = text.rsplit("\u{1b}[2J").next().unwrap();
        let mut plain = String::new();
        let mut chars = frame.chars();
        while let Some(character) = chars.next() {
            if character == '\u{1b}' {
                if chars.next() == Some('[') {
                    for control in chars.by_ref() {
                        if ('@'..='~').contains(&control) {
                            break;
                        }
                    }
                }
            } else {
                plain.push(character);
            }
        }
        plain
    }

    fn finish(mut self, quit: &str) -> String {
        self.input
            .as_mut()
            .unwrap()
            .write_all(quit.as_bytes())
            .unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < self.deadline,
                "PTY failed to exit: {}",
                self.tail()
            );
            if let Ok(chunk) = self.output.recv_timeout(Duration::from_millis(30)) {
                self.transcript.extend(chunk)
            }
        };
        self.finished = true;
        self.input.take();
        for reader in self.readers.drain(..) {
            reader.join().unwrap();
        }
        self.transcript.extend(self.output.try_iter().flatten());
        let text = String::from_utf8_lossy(&self.transcript).into_owned();
        assert!(
            status.success(),
            "PTY failed with {status}: {}",
            self.tail()
        );
        assert!(
            text.contains("[terminal-restored:0]"),
            "raw mode was not restored: {}",
            self.tail()
        );
        assert!(
            text.contains("\u{1b}[?1049l"),
            "alternate screen was not left"
        );
        assert!(text.contains("\u{1b}[?25h"), "cursor was not restored");
        text
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(input) = self.input.as_mut() {
            let _ = input.write_all(b"q");
            let _ = input.flush();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                self.finished = true;
                break;
            }
            // Readiness waiting drains the pipe, allowing script to finish.
            let _ = self.output.recv_timeout(Duration::from_millis(20));
        }
        if !self.finished {
            let _ = Command::new("/bin/kill")
                .args(["-TERM", "--", &format!("-{}", self.child.id())])
                .status();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.input.take();
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

fn write(root: &Path, path: &str, text: &str) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, text).unwrap();
}

fn assert_simple_home(session: &TerminalSession, en: bool) {
    let screen = session.screen_text();
    for shown in if en {
        [
            "Static analysis · no model",
            "Trunk",
            "pty_workbench::main",
            "External systems",
        ]
    } else {
        [
            "静态分析 · 不调用模型",
            "主干",
            "pty_workbench::main",
            "外部系统",
        ]
    } {
        assert!(
            screen.contains(shown),
            "Flow home is missing {shown:?}: {screen}"
        );
    }
    for hidden in [
        "阅读记录",
        "Reading notes",
        "八维视角",
        "Eight perspectives",
        "构建包与文件",
        "Build packages and files",
        "分析范围与状态",
        "Analysis scope and status",
        "Snapshot:",
        "快照：",
        "对象：",
        "Object:",
        "问题：",
        "Question:",
        "c 结论",
        "c Conclusion",
        "v 疑问",
        "v Question",
        "h 历史",
        "h History",
    ] {
        assert!(!screen.contains(hidden), "Home leaked {hidden:?}: {screen}");
    }
    for key in if en {
        [
            "Enter Expand",
            "o Source",
            "e Errors",
            "c Config",
            "s State",
            "v Changes",
            "d Explore",
        ]
    } else {
        [
            "Enter 展开/收起",
            "o 源码",
            "e 错误码",
            "c 配置",
            "s 共享状态",
            "v 查看改动",
            "d 深入分析",
        ]
    } {
        assert!(screen.contains(key), "Home must expose {key:?}: {screen}");
    }
    assert!(screen.contains(if en { "q Exit" } else { "q 退出" }));
}

fn open_explore(session: &mut TerminalSession, en: bool) {
    let quit = if en { "q Exit" } else { "q 退出" };
    session.keys(
        "d",
        &[
            if en {
                "Explore further"
            } else {
                "深入分析"
            },
            if en {
                "Project explanation"
            } else {
                "项目解释"
            },
            if en {
                "Entries and calls"
            } else {
                "入口与调用"
            },
            if en {
                "Eight perspectives"
            } else {
                "八维视角"
            },
            if en {
                "Build packages and files"
            } else {
                "构建包与文件"
            },
            if en { "Reading notes" } else { "阅读记录" },
            if en {
                "Analysis scope and status"
            } else {
                "分析范围与状态"
            },
            quit,
        ],
    );
    // The explanation is first; later steps start from the entries item.
    session.keys(
        "j",
        &[
            if en {
                "› Entries and calls"
            } else {
                "› 入口与调用"
            },
            quit,
        ],
    );
}

#[test]
fn chinese_and_english_workbenches_link_objects_source_and_knowledge_history() {
    for locale in ["zh-CN", "en"] {
        let project = TempDir::new().unwrap();
        let cache = TempDir::new().unwrap();
        write(project.path(),"Cargo.toml","[package]\nname='pty-workbench'\nversion='0.1.0'\nedition='2021'\ndescription='Terminal workflow fixture'\n");
        write(
            project.path(),
            "src/main.rs",
            "mod engine;\nfn main() { engine::process(); }\n",
        );
        write(project.path(),"src/engine.rs","//! Executes one task.\npub enum Status { Ready, Done }\npub struct Job { pub status: Status }\npub fn process() {}\n#[cfg(test)] mod tests { #[test] fn runs() { super::process(); } }\n");
        write(
            project.path(),
            "README.md",
            "# PTY workbench\n\nExecutes one task through src/engine.rs.\n",
        );
        let en = locale == "en";
        let home = if en { "Trunk" } else { "主干" };
        let quit = if en { "q Exit" } else { "q 退出" };
        let dimensions = if en {
            [
                "Capabilities and intent",
                "Architecture and boundaries",
                "Behavior and control",
                "Data and state",
                "Configuration and runtime",
                "Changes and evolution",
                "Verification and constraints",
                "Knowledge and reading",
            ]
        } else {
            [
                "能力与意图",
                "架构与边界",
                "行为与控制",
                "数据与状态",
                "配置与运行",
                "变更与演进",
                "验证与约束",
                "认知与阅读",
            ]
        };
        let mut session = TerminalSession::launch(project.path(), cache.path(), locale);
        session.wait_for(0, &[home, "pty_workbench::main", quit]);
        assert_simple_home(&session, en);
        // A trunk line opens pinned source; back returns to the map.
        session.keys(
            "j\r",
            &[
                if en {
                    "Source · src/main.rs"
                } else {
                    "源码 · src/main.rs"
                },
                "engine::process();",
                quit,
            ],
        );
        session.keys("b", &[home, quit]);
        for (key, title) in if en {
            [
                ("e", "Error codes"),
                ("c", "Configuration"),
                ("s", "Shared state"),
                ("r", "Entries and trunks"),
            ]
        } else {
            [
                ("e", "错误码"),
                ("c", "配置项"),
                ("s", "共享状态"),
                ("r", "入口与主干"),
            ]
        } {
            session.keys(
                key,
                &[title, if en { "g Flow map" } else { "g 脉络图" }, quit],
            );
            session.keys("b", &[home, quit]);
        }
        session.keys(
            "?",
            &[
                if en { "Keyboard help" } else { "操作说明" },
                if en {
                    "Every error exit"
                } else {
                    "全部错误出口"
                },
                if en {
                    "Call order is source order"
                } else {
                    "调用顺序是源码顺序"
                },
                quit,
            ],
        );
        session.keys("b", &[home, quit]);
        assert_simple_home(&session, en);
        session.keys(
            "v",
            &[
                if en {
                    "Change batch · Select scope"
                } else {
                    "改动批次 · 选择范围"
                },
                quit,
            ],
        );
        session.keys("g", &[home, quit]);
        open_explore(&mut session, en);
        let explore = if en {
            "Explore further"
        } else {
            "深入分析"
        };
        session.keys(
            "\r",
            &[
                if en {
                    "Entries and calls"
                } else {
                    "入口与调用"
                },
                "pty_workbench::main",
                quit,
            ],
        );
        session.keys("b", &[explore, quit]);
        session.keys(
            "jj\r",
            &[
                if en {
                    "Modules and dependencies"
                } else {
                    "模块与依赖"
                },
                "pty-workbench",
                quit,
            ],
        );
        session.keys("b", &[explore, quit]);
        session.keys(
            "j\r",
            &[
                if en {
                    "Knowledge and reading"
                } else {
                    "认知与阅读"
                },
                quit,
            ],
        );
        session.keys("b", &[explore, quit]);
        session.keys(
            "j\r",
            &[
                if en {
                    "Analysis scope and status"
                } else {
                    "分析范围与状态"
                },
                if en {
                    "Syntax snapshot:"
                } else {
                    "结构快照："
                },
                quit,
            ],
        );
        session.keys("b", &[explore, quit]);
        session.keys("kkk\r", &dimensions);
        for (position, title) in dimensions.iter().enumerate() {
            let active = if position == 5 {
                if en {
                    "Change batch · Select scope"
                } else {
                    "改动批次 · 选择范围"
                }
            } else {
                title
            };
            session.keys(&(position + 1).to_string(), &[active, quit]);
            session.keys("b", &dimensions);
        }
        session.keys("2", &[dimensions[1], quit]);
        session.keys(
            "\r",
            &[
                if en {
                    "Object: pty-workbench / engine"
                } else {
                    "对象：pty-workbench / engine"
                },
                quit,
            ],
        );
        session.keys(
            "p",
            &[if en {
                "Current question:"
            } else {
                "当前问题："
            }],
        );
        session.keys(
            "How does process work?\r",
            &[
                if en {
                    "Question retained"
                } else {
                    "当前问题已保留"
                },
                quit,
            ],
        );
        session.keys(
            "4",
            &[
                dimensions[3],
                "pty-workbench / engine",
                "How does process work?",
                quit,
            ],
        );
        session.keys(
            "o",
            &[
                if en {
                    "Source · src/engine.rs"
                } else {
                    "源码 · src/engine.rs"
                },
                "pub fn process()",
                quit,
            ],
        );
        session.keys("c", &[if en { "Conclusion:" } else { "结论：" }]);
        session.keys(
            "checked workbench claim\r",
            &[
                if en {
                    "Knowledge saved"
                } else {
                    "已保存认知"
                },
                "checked workbench claim",
                quit,
            ],
        );
        session.keys("c", &[if en { "Conclusion:" } else { "结论：" }]);
        session.keys(
            "\u{15}rechecked workbench claim\r",
            &[
                if en {
                    "Knowledge saved"
                } else {
                    "已保存认知"
                },
                "rechecked workbench claim",
                quit,
            ],
        );
        session.keys(
            "h",
            &[
                if en {
                    "Knowledge record history"
                } else {
                    "认知记录历史"
                },
                "checked workbench claim",
                "rechecked workbench claim",
                "· v1",
                "· v2",
                quit,
            ],
        );
        session.keys("g", &[home, quit]);
        assert_simple_home(&session, en);
        open_explore(&mut session, en);
        session.keys(
            "jjj\r",
            &[
                if en {
                    "Knowledge and reading"
                } else {
                    "认知与阅读"
                },
                "rechecked workbench claim",
                quit,
            ],
        );
        let transcript = session.finish("q");
        assert!(!transcript.contains(if en {
            "Knowledge not saved"
        } else {
            "认知未保存"
        }));
        let output = Command::new(env!("CARGO_BIN_EXE_codexis"))
            .args(["--format", "json", "--locale", locale, "--project"])
            .arg(project.path())
            .arg("--cache-dir")
            .arg(cache.path())
            .args(["knowledge", "--history"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let records = report["data"]["records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["id"], records[1]["id"]);
        assert!(records
            .iter()
            .any(|r| r["revision"] == 1 && r["claim"] == "checked workbench claim"));
        assert!(records
            .iter()
            .any(|r| r["revision"] == 2 && r["claim"] == "rechecked workbench claim"));
        assert!(records.iter().all(|r| r["valid"] == true));
        // Ctrl-C takes the same terminal restoration path in raw mode.
        let mut interrupted = TerminalSession::launch(project.path(), cache.path(), locale);
        interrupted.wait_for(0, &[home, quit]);
        interrupted.finish("\u{3}");
    }
}

#[test]
#[ignore = "requires CODEXIS_BASELINE_ROOT and CODEXIS_BASELINE_CACHE containing a generated Chinese explanation"]
fn generated_baseline_leads_from_business_steps_to_snapshot_source() {
    let project = std::env::var_os("CODEXIS_BASELINE_ROOT").expect("set CODEXIS_BASELINE_ROOT");
    let cache = std::env::var_os("CODEXIS_BASELINE_CACHE").expect("set CODEXIS_BASELINE_CACHE");
    let project = Path::new(&project);
    let cache = Path::new(&cache);
    let output = Command::new(env!("CARGO_BIN_EXE_codexis"))
        .arg("--project")
        .arg(project)
        .arg("--cache-dir")
        .arg(cache)
        .args(["baseline", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["data"]["baseline"]["status"], "interpreted");
    let explanation = &report["data"]["baseline"]["explanation"];
    assert!((3..=5).contains(&explanation["scenario"]["steps"].as_array().unwrap().len()));
    assert!(!explanation["purpose"]["evidence"]
        .as_array()
        .unwrap()
        .is_empty());
    let mut session = TerminalSession::launch(project, cache, "zh-CN");
    session.wait_for(0, &["主干", "v 查看改动", "q 退出"]);
    session.keys("d", &["深入分析", "项目解释"]);
    session.keys("\r", &["项目如何工作", "代表场景", "关键状态", "重要边界"]);
    session.keys("\r", &["负责什么", "交付什么", "证据 1"]);
    session.keys("\r", &["源码 ·", "q 退出"]);
    session.keys("b", &["负责什么", "证据 1"]);
    session.keys("b", &["项目如何工作", "代表场景"]);
    session.finish("q");
}
