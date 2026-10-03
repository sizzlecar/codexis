use crate::source::check_cancelled;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

pub struct LspClient {
    child: Child,
    input: ChildStdin,
    messages: Receiver<Result<Value, String>>,
    next_id: u64,
    configuration: Value,
    root_uri: String,
    open_files: BTreeSet<String>,
    pub status: Value,
    progress: bool,
    language: &'static str,
    configuration_section: &'static str,
}

pub struct ServerProfile {
    pub args: Vec<OsString>,
    pub language: &'static str,
    pub configuration_section: &'static str,
    pub initialization_options: Value,
}

impl LspClient {
    pub fn start(
        program: &Path,
        root: &Path,
        configuration: Value,
        progress: bool,
        timeout: Duration,
    ) -> Result<Self> {
        let profile = ServerProfile {
            args: Vec::new(),
            language: "rust",
            configuration_section: "rust-analyzer",
            initialization_options: configuration.clone(),
        };
        Self::start_with_profile(program, root, configuration, profile, progress, timeout)
    }

    pub fn start_with_profile(
        program: &Path,
        root: &Path,
        configuration: Value,
        profile: ServerProfile,
        progress: bool,
        timeout: Duration,
    ) -> Result<Self> {
        let mut child = Command::new(program)
            .args(&profile.args)
            .current_dir(root)
            .env("CARGO_NET_OFFLINE", "true")
            .env("RUSTUP_AUTO_INSTALL", "0")
            // JDT LS otherwise allows inherited variables to switch away from stdio.
            .env_remove("CLIENT_PORT")
            .env_remove("CLIENT_HOST")
            .env_remove("CLIENT_CONNECTION")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "start {} language server {}",
                    profile.language,
                    program.display()
                )
            })?;
        let input = child
            .stdin
            .take()
            .context("language server stdin missing")?;
        let output = child
            .stdout
            .take()
            .context("language server stdout missing")?;
        let errors = child
            .stderr
            .take()
            .context("language server stderr missing")?;
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                match read_message(&mut reader) {
                    Ok(Some(value)) => {
                        if sender.send(Ok(value)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                        break;
                    }
                }
            }
        });
        // Drain stderr without retaining unbounded output or mixing it into JSON.
        thread::spawn(move || {
            let _ = std::io::copy(&mut BufReader::new(errors), &mut std::io::sink());
        });
        let root_uri = Url::from_directory_path(root)
            .map_err(|_| anyhow!("invalid workspace URI"))?
            .to_string();
        let mut client = Self {
            child,
            input,
            messages,
            next_id: 1,
            configuration,
            root_uri,
            open_files: BTreeSet::new(),
            status: Value::Null,
            progress,
            language: profile.language,
            configuration_section: profile.configuration_section,
        };
        client.request(
            "initialize",
            json!({
                "processId":std::process::id(), "rootUri":client.root_uri,
                "workspaceFolders":[{"uri":client.root_uri,"name":"codexis"}],
                "capabilities":{
                    "workspace":{"configuration":true,"workspaceFolders":true},
                    "textDocument":{"definition":{"linkSupport":true}},
                    "general":{"positionEncodings":["utf-16"]},
                    "window":{"workDoneProgress":true},
                    "experimental":{"serverStatusNotification":true}
                },
                "initializationOptions":profile.initialization_options,
                "clientInfo":{"name":"codexis","version":env!("CARGO_PKG_VERSION")}
            }),
            timeout,
        )?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    pub fn wait_ready(&mut self, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        let mut announced = Instant::now();
        loop {
            check_cancelled()?;
            if self.status["quiescent"] == true {
                return Ok(());
            }
            if started.elapsed() >= timeout {
                bail!("language server workspace initialization timed out");
            }
            if announced.elapsed().as_secs() >= 15 && self.progress {
                eprintln!(
                    "Waiting for {} workspace analysis ({}s)",
                    self.language,
                    started.elapsed().as_secs()
                );
                announced = Instant::now();
            }
            match self.messages.recv_timeout(Duration::from_millis(200)) {
                Ok(value) => {
                    self.handle(value.map_err(anyhow::Error::msg)?)?;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("language server exited while loading workspace")
                }
            }
        }
    }

    pub fn open(&mut self, path: &Path, content: &str) -> Result<String> {
        let uri = Url::from_file_path(path)
            .map_err(|_| anyhow!("invalid source URI"))?
            .to_string();
        if self.open_files.insert(uri.clone()) {
            self.notify(
                "textDocument/didOpen",
                json!({"textDocument":{"uri":uri,"languageId":self.language,"version":1,"text":content}}),
            )?;
        }
        Ok(uri)
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(message)?;
        write!(self.input, "Content-Length: {}\r\n\r\n", bytes.len())?;
        self.input.write_all(&bytes)?;
        self.input.flush()?;
        Ok(())
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc":"2.0","method":method,"params":params}))
    }

    pub fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let started = Instant::now();
        loop {
            check_cancelled()?;
            if started.elapsed() >= timeout {
                self.notify("$/cancelRequest", json!({"id":id}))?;
                bail!("language server request {method} timed out");
            }
            match self.messages.recv_timeout(Duration::from_millis(200)) {
                Ok(value) => {
                    let value = value.map_err(anyhow::Error::msg)?;
                    if value["id"] == id && value.get("method").is_none() {
                        if let Some(error) = value.get("error") {
                            bail!("language server {method}: {error}");
                        }
                        return Ok(value["result"].clone());
                    }
                    self.handle(value)?;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("language server exited during {method}")
                }
            }
        }
    }

    fn handle(&mut self, message: Value) -> Result<()> {
        let Some(method) = message["method"].as_str() else {
            return Ok(());
        };
        if let Some(id) = message.get("id") {
            let result = match method {
                "workspace/configuration" => Value::Array(
                    message["params"]["items"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|item| {
                            let section = item["section"]
                                .as_str()
                                .unwrap_or(self.configuration_section);
                            if section == self.configuration_section {
                                self.configuration.clone()
                            } else {
                                section
                                    .trim_start_matches(&format!("{}.", self.configuration_section))
                                    .split('.')
                                    .fold(&self.configuration, |v, key| &v[key])
                                    .clone()
                            }
                        })
                        .collect(),
                ),
                "workspace/workspaceFolders" => json!([{"uri":self.root_uri,"name":"codexis"}]),
                "workspace/applyEdit" => {
                    json!({"applied":false,"failureReason":"Codexis is a read-only analyzer"})
                }
                _ => Value::Null,
            };
            self.send(&json!({"jsonrpc":"2.0","id":id,"result":result}))?;
        } else if method == "experimental/serverStatus" {
            self.status = message["params"].clone();
        }
        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.notify("exit", Value::Null);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                length = Some(value.trim().parse::<usize>()?);
            }
        }
    }
    let length = length.context("LSP response has no Content-Length")?;
    if length > 64 * 1024 * 1024 {
        bail!("LSP response exceeds 64 MiB limit");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub fn utf16_position(content: &str, byte: usize) -> Result<Value> {
    let prefix = content
        .get(..byte)
        .context("source position is not a UTF-8 boundary")?;
    let line = prefix.bytes().filter(|b| *b == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |v| v + 1);
    let character = prefix[line_start..].encode_utf16().count();
    Ok(json!({"line":line,"character":character}))
}

pub fn byte_position(content: &str, position: &Value) -> Option<usize> {
    let line = position["line"].as_u64()? as usize;
    let target = position["character"].as_u64()? as usize;
    let mut offset = 0;
    for (index, text) in content.split_inclusive('\n').enumerate() {
        if index == line {
            let mut units = 0;
            for (byte, c) in text.char_indices() {
                if units == target {
                    return Some(offset + byte);
                }
                units += c.len_utf16();
                if units > target {
                    return None;
                }
            }
            return (units == target).then_some(offset + text.len());
        }
        offset += text.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_positions_round_trip_without_splitting_surrogates() {
        let text = "fn x() {}\nlet s = \"😀你好\"; target();\n";
        let byte = text.find("target").unwrap();
        assert_eq!(
            byte_position(text, &utf16_position(text, byte).unwrap()),
            Some(byte)
        );
        assert!(byte_position("😀", &json!({"line":0,"character":1})).is_none());
    }
    #[test]
    fn frames_use_byte_length() {
        let value = json!({"jsonrpc":"2.0","id":1,"result":"你好"});
        let bytes = serde_json::to_vec(&value).unwrap();
        let mut frame = format!("Content-Length: {}\r\n\r\n", bytes.len()).into_bytes();
        frame.extend(bytes);
        assert_eq!(read_message(&mut &frame[..]).unwrap(), Some(value));
    }
}
