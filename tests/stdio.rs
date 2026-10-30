use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tower_lsp::lsp_types::Url;

struct Server {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    sequence: u64,
}

impl Server {
    fn start(project: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_miden-lsp"));
        command.args(["--no-toolchain-dispatch", "--project"]).arg(project);
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> Self {
        command
            .env_remove("MIDEN_SYSROOT")
            .env_remove("MIDENUP_HOME")
            .env_remove("MIDENUP_TOOLCHAIN");
        Self::spawn_configured(command)
    }

    fn spawn_configured(mut command: Command) -> Self {
        let mut child = command
            .env("RUST_MIN_STACK", "16777216")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Content-Length:") {
                        length = value.trim().parse::<usize>().ok();
                    } else {
                        assert!(line.starts_with("Content-Type:"), "non-protocol stdout: {line:?}");
                    }
                }
                let Some(length) = length else { return };
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                let message = serde_json::from_slice(&body).unwrap();
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin,
            messages,
            sequence: 0,
        }
    }

    fn send(&mut self, message: Value) {
        let body = serde_json::to_vec(&message).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        self.stdin.write_all(&body).unwrap();
        self.stdin.flush().unwrap();
    }

    fn receive(&mut self) -> Value {
        self.messages
            .recv_timeout(Duration::from_secs(20))
            .expect("language server did not send a valid LSP message")
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.sequence += 1;
        let sequence = self.sequence;
        let mut request = json!({"jsonrpc":"2.0", "id":sequence, "method":method, "params":params});
        if params.is_null() {
            request.as_object_mut().unwrap().remove("params");
        }
        self.send(request);
        loop {
            let message = self.receive();
            if message["id"] == sequence {
                assert!(message.get("error").is_none(), "{message:#}");
                return message["result"].clone();
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let mut notification = json!({"jsonrpc":"2.0", "method":method, "params":params});
        if params.is_null() {
            notification.as_object_mut().unwrap().remove("params");
        }
        self.send(notification);
    }

    fn initialize(&mut self, folders: &[&Path], options: Value) {
        let folders = folders
            .iter()
            .map(|folder| json!({"uri": Url::from_directory_path(folder).unwrap(), "name":"test"}))
            .collect::<Vec<_>>();
        let result = self.request(
            "initialize",
            json!({"capabilities":{}, "workspaceFolders":folders, "initializationOptions":options}),
        );
        assert_eq!(result["capabilities"]["textDocumentSync"], 2);
        self.notify("initialized", json!({}));
    }

    fn open(&mut self, path: &Path, text: &str) {
        self.notify("textDocument/didOpen", json!({"textDocument":{"uri":Url::from_file_path(path).unwrap(), "languageId":"masm", "version":1, "text":text}}));
    }

    fn diagnostics(&mut self, path: &Path, version: i64) -> Value {
        let uri = Url::from_file_path(path).unwrap();
        loop {
            let message = self.receive();
            if message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == uri.as_str()
                && message["params"]["version"] == version
            {
                return message["params"]["diagnostics"].clone();
            }
        }
    }

    fn finish(&mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        assert_eq!(self.wait_for_exit(), 0);
    }

    fn wait_for_exit(&mut self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().expect("language server exited abnormally");
            }
            assert!(Instant::now() < deadline, "language server did not exit after shutdown");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn project(root: &Path, text: &str) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("miden-project.toml"),
        "[package]\nname = \"app\"\nversion = \"1.0.0\"\n[lib]\npath = \"mod.masm\"\nnamespace = \
         \"app\"\n",
    )
    .unwrap();
    std::fs::write(root.join("mod.masm"), text).unwrap();
}

#[test]
fn real_stdio_server_reports_compiler_warnings_and_clears_them_after_an_unsaved_fix() {
    let root = tempfile::tempdir().unwrap();
    let unsafe_text = "pub proc entry() -> u32\nadv_push\npush.1\nu32wrapping_add\nend\n";
    project(root.path(), unsafe_text);
    let source = root.path().join("mod.masm");
    let mut server = Server::start(root.path());
    server.initialize(&[root.path()], json!({}));
    server.open(&source, unsafe_text);
    let findings = server.diagnostics(&source, 1);
    assert!(
        findings
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| finding["source"] == "miden-compiler"
                && finding["severity"] == 2
                && finding["message"].as_str().unwrap().contains("unconstrained")),
        "{findings:#}"
    );
    let safe_text = "pub proc entry() -> u32\nadv_push\nu32assert\npush.1\nu32wrapping_add\nend\n";
    server.notify("textDocument/didChange", json!({"textDocument":{"uri":Url::from_file_path(&source).unwrap(), "version":2}, "contentChanges":[{"text":safe_text}]}));
    let fixed = server.diagnostics(&source, 2);
    assert!(fixed.as_array().unwrap().is_empty(), "{fixed:#}");
    assert_eq!(std::fs::read_to_string(source).unwrap(), unsafe_text);
    assert!(!root.path().join("target").exists());
    server.finish();
}

#[test]
fn editing_an_unsaved_dependency_refreshes_diagnostics_in_its_open_caller() {
    let root = tempfile::tempdir().unwrap();
    let caller = "pub mod child\npub proc main\nexec.child::target\nend\n";
    project(root.path(), caller);
    let source = root.path().join("mod.masm");
    let child = root.path().join("child.masm");
    let initial = "pub proc other\npush.1 drop\nend\n";
    std::fs::write(&child, initial).unwrap();
    let mut server = Server::start(root.path());
    server.initialize(&[root.path()], json!({}));
    server.open(&source, caller);
    assert!(!server.diagnostics(&source, 1).as_array().unwrap().is_empty());
    server.open(&child, initial);
    server.diagnostics(&child, 1);
    assert!(!server.diagnostics(&source, 1).as_array().unwrap().is_empty());
    server.notify("textDocument/didChange", json!({"textDocument":{"uri":Url::from_file_path(&child).unwrap(), "version":2}, "contentChanges":[{"text":"pub proc target\npush.1 drop\nend\n"}]}));
    let fixed = server.diagnostics(&source, 1);
    assert!(fixed.as_array().unwrap().is_empty(), "{fixed:#}");
    assert_eq!(std::fs::read_to_string(child).unwrap(), initial);
    server.finish();
}

#[test]
fn multi_root_workspace_resolves_packages_from_each_projects_toolchain() {
    use miden_core::serde::Serializable;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("midenup");
    let mut projects = Vec::new();
    for (channel, procedure) in [("0.18.0", "first"), ("0.19.0", "second")] {
        let path = root.path().join(procedure);
        project(&path, &format!("use dep\npub proc main\nexec.dep::{procedure}\nend\n"));
        let manifest = path.join("miden-project.toml");
        let mut text = std::fs::read_to_string(&manifest).unwrap();
        text.push_str("[dependencies]\ndep = \"1.0.0\"\n");
        std::fs::write(manifest, text).unwrap();
        std::fs::write(
            path.join("miden-toolchain.toml"),
            format!("[toolchain]\nchannel = \"{channel}\"\n"),
        )
        .unwrap();
        let lib = home.join("toolchains").join(channel).join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let mut package = miden_assembly::Assembler::default()
            .assemble_library(
                "dep",
                format!("namespace dep\npub proc {procedure}\npush.1 drop\nend\n"),
                core::iter::empty::<&str>(),
            )
            .unwrap();
        package.version = "1.0.0".parse().unwrap();
        let artifact = lib.join("dep.masp");
        std::fs::write(&artifact, package.to_bytes()).unwrap();
        projects.push((path, artifact));
    }
    let mut server = Server::start(root.path());
    server.initialize(
        &projects.iter().map(|(path, _)| path.as_path()).collect::<Vec<_>>(),
        json!({"midenupHome":home}),
    );
    for (path, artifact) in &projects {
        let source = path.join("mod.masm");
        server.open(&source, &std::fs::read_to_string(&source).unwrap());
        let findings = server.diagnostics(&source, 1);
        assert!(
            !findings.as_array().unwrap().iter().any(|finding| finding["severity"] == 1),
            "{findings:#}"
        );
        let definition = server.request("textDocument/definition", json!({"textDocument":{"uri":Url::from_file_path(source).unwrap()}, "position":{"line":2,"character":11}}));
        assert_eq!(
            Url::parse(definition["uri"].as_str().unwrap()).unwrap().to_file_path().unwrap(),
            artifact.canonicalize().unwrap()
        );
    }
    let symbols = server.request("workspace/symbol", json!({"query":""}));
    for name in ["first", "second"] {
        assert!(
            symbols.as_array().unwrap().iter().any(|symbol| symbol["name"] == name),
            "{symbols:#}"
        );
    }
    server.finish();
}

#[cfg(unix)]
#[test]
fn toolchain_dispatch_keeps_the_stdio_protocol_intact() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    project(root.path(), "pub proc main\npush.1 drop\nend\n");
    std::fs::write(root.path().join("miden-toolchain.toml"), "[toolchain]\nchannel = \"0.18.0\"\n")
        .unwrap();
    let wrapper = root.path().join("miden");
    let arguments = root.path().join("arguments");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$MIDEN_LSP_TEST_ARGUMENTS\"\n[ \"$1\" = 'miden-lsp' \
         ] || exit 2\nshift\nexec \"$MIDEN_LSP_TEST_SERVER\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_miden-lsp"));
    command
        .arg("--project")
        .arg(root.path())
        .arg("--miden")
        .arg(wrapper)
        .env("MIDEN_LSP_TEST_SERVER", env!("CARGO_BIN_EXE_miden-lsp"))
        .env("MIDEN_LSP_TEST_ARGUMENTS", &arguments);
    let mut server = Server::spawn(command);
    server.initialize(&[root.path()], json!({}));
    let arguments = std::fs::read_to_string(arguments).unwrap();
    assert!(
        arguments.starts_with("miden-lsp\n--no-toolchain-dispatch\n--project\n"),
        "{arguments}"
    );
    server.finish();
}

#[cfg(unix)]
#[test]
#[ignore = "requires MIDEN_LSP_TEST_MIDENUP pointing to a real midenup executable"]
fn real_midenup_installs_and_dispatches_the_server_with_package_navigation() {
    use std::os::unix::fs::symlink;

    use miden_core::serde::Serializable;

    let manager = std::env::var_os("MIDEN_LSP_TEST_MIDENUP")
        .map(std::path::PathBuf::from)
        .expect("set MIDEN_LSP_TEST_MIDENUP to the real midenup executable")
        .canonicalize()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let application = root.path().join("project");
    let text = "use dep\npub proc entry() -> u32\nadv_push\npush.1\nu32wrapping_add\nend\npub \
                proc main\nexec.dep::helper\nend\n";
    project(&application, text);
    let manifest = application.join("miden-project.toml");
    let mut manifest_text = std::fs::read_to_string(&manifest).unwrap();
    manifest_text.push_str("[dependencies]\ndep = \"1.0.0\"\n");
    std::fs::write(manifest, manifest_text).unwrap();
    std::fs::write(
        application.join("miden-toolchain.toml"),
        "[toolchain]\nchannel = \"0.18.0\"\ncomponents = [\"miden-lsp\", \"dep\"]\nprofile = \
         \"minimal\"\n",
    )
    .unwrap();
    let mut dependency = miden_assembly::Assembler::default()
        .assemble_library(
            "dep",
            "namespace dep\npub proc helper\npush.1 drop\nend\n",
            core::iter::empty::<&str>(),
        )
        .unwrap();
    dependency.version = "1.0.0".parse().unwrap();
    let artifact = root.path().join("dep.masp");
    std::fs::write(&artifact, dependency.to_bytes()).unwrap();
    let upstream = root.path().join("manifest.json");
    let mut upstream_manifest = json!({
        "manifest_version":"3.0.0", "min_client_version":"1.3.0", "date":1793318400,
        "networks":{"devnet":"0.18.0", "mainnet":"0.18.0", "testnet":"0.18.0"},
        "channels":[{"name":"0.18.0", "components":[{
            "name":"miden-lsp", "kind":"executable",
            "version":{"kind":"registry", "version":"0.1.2"},
            "installation_method":{"kind":"prebuilt"},
            "installed-executable":"miden-lsp", "profiles":["complete"],
            "artifacts":{"miden-lsp":{"uri":Url::from_file_path(env!("CARGO_BIN_EXE_miden-lsp")).unwrap()}}
        }, {
            "name":"dep", "kind":"package",
            "version":{"kind":"registry", "version":"1.0.0"},
            "profiles":["minimal"],
            "artifacts":{"dep.masp":{"uri":Url::from_file_path(&artifact).unwrap()}}
        }]}]
    });
    let mut other_channel = upstream_manifest["channels"][0].clone();
    other_channel["name"] = json!("0.19.0");
    other_channel["components"][0]["profiles"] = json!(["minimal"]);
    upstream_manifest["channels"].as_array_mut().unwrap().push(other_channel);
    std::fs::write(&upstream, serde_json::to_vec(&upstream_manifest).unwrap()).unwrap();
    let cargo_home = root.path().join("cargo");
    let bin = cargo_home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    symlink(&manager, bin.join("midenup")).unwrap();
    symlink(&manager, bin.join("miden")).unwrap();
    let home = root.path().join("midenup");
    for (override_channel, expected_channel) in [(None, "0.18.0"), (Some("0.19.0"), "0.19.0")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_miden-lsp"));
        if let Some(channel) = override_channel {
            command.args(["--toolchain", channel]);
        }
        command
            .arg("--project")
            .arg(&application)
            .arg("--miden")
            .arg(bin.join("miden"))
            .env_remove("MIDEN_SYSROOT")
            .env_remove("MIDENUP_TOOLCHAIN")
            .env("MIDENUP_HOME", &home)
            .env("CARGO_HOME", &cargo_home)
            .env("MIDENUP_MANIFEST_URI", Url::from_file_path(&upstream).unwrap().as_str());
        let mut server = Server::spawn_configured(command);
        server.initialize(&[&application], json!({}));
        let source = application.join("mod.masm");
        server.open(&source, text);
        let findings = server.diagnostics(&source, 1);
        assert!(
            findings.as_array().unwrap().iter().any(|finding| {
                finding["source"] == "miden-compiler"
                    && finding["message"].as_str().unwrap().contains("unconstrained")
            }),
            "{findings:#}"
        );
        let definition = server.request("textDocument/definition", json!({"textDocument":{"uri":Url::from_file_path(&source).unwrap()}, "position":{"line":7,"character":11}}));
        let target =
            Url::parse(definition["uri"].as_str().unwrap()).unwrap().to_file_path().unwrap();
        assert!(
            target.starts_with(
                home.join("toolchains").join(expected_channel).canonicalize().unwrap()
            )
        );
        assert_eq!(target.file_name().unwrap(), "dep.masp");
        assert!(home.join("state.json").is_file());
        assert!(!application.join("target").exists());
        server.finish();
    }
}

#[test]
fn file_watch_registration_and_disk_changes_refresh_open_callers() {
    let root = tempfile::tempdir().unwrap();
    let caller = "pub mod child\npub proc main\nexec.child::target\nend\n";
    project(root.path(), caller);
    let source = root.path().join("mod.masm");
    let child = root.path().join("child.masm");
    std::fs::write(&child, "pub proc other\npush.1 drop\nend\n").unwrap();
    let mut server = Server::start(root.path());
    server.request("initialize", json!({"capabilities":{"workspace":{"didChangeWatchedFiles":{"dynamicRegistration":true}}}, "rootUri":Url::from_directory_path(root.path()).unwrap()}));
    server.notify("initialized", json!({}));
    loop {
        let request = server.receive();
        if request["method"] != "client/registerCapability" {
            continue;
        }
        let options = &request["params"]["registrations"][0];
        assert_eq!(options["method"], "workspace/didChangeWatchedFiles");
        let watchers = options["registerOptions"]["watchers"].as_array().unwrap();
        assert!(
            watchers
                .iter()
                .any(|watcher| watcher["globPattern"] == "**/miden-toolchain.toml")
        );
        assert!(watchers.iter().any(|watcher| watcher["globPattern"] == "**/*.masp"));
        server.send(json!({"jsonrpc":"2.0", "id":request["id"], "result":null}));
        break;
    }
    server.open(&source, caller);
    assert!(!server.diagnostics(&source, 1).as_array().unwrap().is_empty());
    std::fs::write(&child, "pub proc target\npush.1 drop\nend\n").unwrap();
    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":Url::from_file_path(child).unwrap(),"type":2}]}),
    );
    let fixed = server.diagnostics(&source, 1);
    assert!(fixed.as_array().unwrap().is_empty(), "{fixed:#}");
    server.finish();
}

#[test]
fn exiting_without_shutdown_returns_the_protocol_error_status() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server::start(root.path());
    server.notify("exit", Value::Null);
    assert_eq!(server.wait_for_exit(), 1);
}

#[test]
fn replacing_a_package_artifact_revalidates_open_documents_without_restarting() {
    use miden_core::serde::Serializable;
    let root = tempfile::tempdir().unwrap();
    let text = "use dep\npub proc main\nexec.dep::helper\nend\n";
    project(root.path(), text);
    let manifest = root.path().join("miden-project.toml");
    let mut contents = std::fs::read_to_string(&manifest).unwrap();
    contents.push_str("[dependencies]\ndep = \"1.0.0\"\n");
    std::fs::write(manifest, contents).unwrap();
    let artifact = root.path().join("dep.masp");
    let mut packages = Vec::new();
    for name in ["helper", "other"] {
        let mut package = miden_assembly::Assembler::default()
            .assemble_library(
                "dep",
                format!("namespace dep\npub proc {name}\npush.1 drop\nend\n"),
                core::iter::empty::<&str>(),
            )
            .unwrap();
        package.version = "1.0.0".parse().unwrap();
        packages.push(package.to_bytes());
    }
    std::fs::write(&artifact, &packages[0]).unwrap();
    let source = root.path().join("mod.masm");
    let mut server = Server::start(root.path());
    server.initialize(&[root.path()], json!({"registryArtifacts":[artifact]}));
    server.open(&source, text);
    let initial = server.diagnostics(&source, 1);
    assert!(
        !initial.as_array().unwrap().iter().any(|finding| finding["severity"] == 1),
        "{initial:#}"
    );
    for (bytes, should_fail) in [(&packages[1], true), (&packages[0], false)] {
        std::fs::write(&artifact, bytes).unwrap();
        server.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":Url::from_file_path(&artifact).unwrap(),"type":2}]}),
        );
        let findings = server.diagnostics(&source, 1);
        assert_eq!(
            findings.as_array().unwrap().iter().any(|finding| finding["severity"] == 1),
            should_fail,
            "{findings:#}"
        );
    }
    server.finish();
}
