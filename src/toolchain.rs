use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Deserialize;

use crate::analysis::{InitializationOptions, RegistryState};

#[derive(Deserialize)]
struct ToolchainFile {
    toolchain: Channel,
}

#[derive(Deserialize)]
struct Channel {
    channel: String,
}

pub fn project_channel(project: &Path) -> Result<Option<String>, String> {
    let directory = if project.is_dir() {
        project
    } else {
        project.parent().unwrap_or(project)
    };
    for directory in directory.ancestors() {
        let path = directory.join("miden-toolchain.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("cannot read '{}': {error}", path.display())),
        };
        let file: ToolchainFile = toml::from_str(&text)
            .map_err(|error| format!("invalid '{}': {error}", path.display()))?;
        validate_channel(&file.toolchain.channel)?;
        return Ok(Some(file.toolchain.channel));
    }
    Ok(None)
}

fn validate_channel(channel: &str) -> Result<(), String> {
    if channel.is_empty()
        || channel == "."
        || channel == ".."
        || !channel
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(format!("invalid Miden toolchain channel '{channel}'"));
    }
    Ok(())
}

pub fn dispatch(
    project: &Path,
    channel: &str,
    miden: &Path,
) -> Result<std::process::ExitStatus, String> {
    validate_channel(channel)?;
    let mut command = Command::new(miden);
    if project_channel(project)?.as_deref() != Some(channel) {
        command.arg(format!("+{channel}")).env("MIDEN_LSP_TOOLCHAIN_OVERRIDE", channel);
    }
    command
        .arg("miden-lsp")
        .arg("--no-toolchain-dispatch")
        .arg("--project")
        .arg(project)
        .current_dir(project)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| {
            format!(
                "cannot launch the '{channel}' toolchain's language server: {error}. Install it \
                 with midenup, or use --no-toolchain-dispatch for a local development build"
            )
        })
}

pub(crate) fn registry_for_document(
    options: &InitializationOptions,
    default: &Arc<RegistryState>,
    document: &Path,
) -> Result<Arc<RegistryState>, String> {
    if options.sysroot.is_some() {
        return default.select(options);
    }
    let channel = std::env::var("MIDEN_LSP_TOOLCHAIN_OVERRIDE")
        .ok()
        .map(Some)
        .map(Ok)
        .unwrap_or_else(|| project_channel(document))?;
    let Some(channel) = channel else {
        if let Some(sysroot) = std::env::var_os("MIDEN_SYSROOT").filter(|path| !path.is_empty()) {
            let mut selected = options.clone();
            selected.sysroot = Some(sysroot.into());
            return default.select(&selected);
        }
        return default.select(options);
    };
    let home = match options
        .midenup_home
        .clone()
        .or_else(|| std::env::var_os("MIDENUP_HOME").map(PathBuf::from))
    {
        Some(home) => home,
        None => query_home(document)?,
    };
    let sysroot = home.join("toolchains").join(&channel);
    if !sysroot.is_dir() {
        return Err(format!(
            "Miden toolchain '{channel}' is not installed. Run `midenup install {channel}` and \
             restart the language server"
        ));
    }
    let mut selected = options.clone();
    selected.sysroot = Some(sysroot);
    default.select(&selected)
}

fn query_home(document: &Path) -> Result<PathBuf, String> {
    let directory = if document.is_dir() {
        document
    } else {
        document.parent().unwrap_or(document)
    };
    let mut child = Command::new("midenup")
        .args(["show", "home"])
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!(
                "cannot locate midenup: {error}. Configure midenupHome or sysroot in the language \
                 server initialization options"
            )
        })?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("midenup did not respond while resolving the active toolchain".into());
            }
        }
    }
    let output = child.wait_with_output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(
            "`midenup show home` failed; configure midenupHome or sysroot explicitly".into()
        );
    }
    let home =
        PathBuf::from(String::from_utf8(output.stdout).map_err(|error| error.to_string())?.trim());
    if !home.is_absolute() {
        return Err("midenup returned an invalid home directory".into());
    }
    Ok(home)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use miden_core::serde::Serializable;

    use super::*;

    fn directory() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "miden-lsp-toolchains-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn nearest_toolchain_file_wins_and_invalid_channels_are_rejected() {
        let root = directory();
        std::fs::create_dir_all(root.join("nested/src")).unwrap();
        std::fs::write(root.join("miden-toolchain.toml"), "[toolchain]\nchannel = \"0.18.0\"\n")
            .unwrap();
        assert_eq!(
            project_channel(&root.join("nested/src/mod.masm")).unwrap().as_deref(),
            Some("0.18.0")
        );
        std::fs::write(
            root.join("nested/miden-toolchain.toml"),
            "[toolchain]\nchannel = \"0.19.0\"\n",
        )
        .unwrap();
        assert_eq!(
            project_channel(&root.join("nested/src/mod.masm")).unwrap().as_deref(),
            Some("0.19.0")
        );
        std::fs::write(
            root.join("nested/miden-toolchain.toml"),
            "[toolchain]\nchannel = \"../../other\"\n",
        )
        .unwrap();
        assert!(
            project_channel(&root.join("nested/src/mod.masm"))
                .unwrap_err()
                .contains("invalid Miden toolchain")
        );
        std::fs::write(root.join("nested/miden-toolchain.toml"), "invalid toml").unwrap();
        assert!(
            project_channel(&root.join("nested/src/mod.masm"))
                .unwrap_err()
                .contains("invalid")
        );
    }

    #[test]
    fn workspace_toolchains_have_independent_package_registries() {
        let root = directory();
        let home = root.join("midenup");
        let default = Arc::new(RegistryState::default());
        let options = InitializationOptions {
            midenup_home: Some(home.clone()),
            ..Default::default()
        };
        for (channel, name) in [("0.18.0", "first"), ("0.19.0", "second")] {
            let project = root.join(name);
            let lib = home.join("toolchains").join(channel).join("lib");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::create_dir_all(&lib).unwrap();
            std::fs::write(
                project.join("miden-toolchain.toml"),
                format!("[toolchain]\nchannel = \"{channel}\"\n"),
            )
            .unwrap();
            let mut package = miden_assembly::Assembler::default()
                .assemble_library(
                    "dep",
                    format!("namespace dep\npub proc {name}\npush.1 drop\nend\n"),
                    core::iter::empty::<&str>(),
                )
                .unwrap();
            package.version = "1.0.0".parse().unwrap();
            std::fs::write(lib.join("dep.masp"), package.to_bytes()).unwrap();
            let selected =
                registry_for_document(&options, &default, &project.join("mod.masm")).unwrap();
            use miden_package_registry::PackageProvider;
            let id = miden_package_registry::PackageId::from("dep");
            let version = selected
                .registry()
                .packages()
                .get(&id)
                .unwrap()
                .values()
                .next()
                .unwrap()
                .version();
            let package = selected.registry().load_package(&id, version).unwrap();
            assert!(package.manifest.exports().any(|export| export.name() == name));
        }
        assert!(default.registry().packages().is_empty());
    }

    #[test]
    fn explicit_sysroot_overrides_project_toolchain_and_missing_installations_are_actionable() {
        let project = directory();
        std::fs::write(project.join("miden-toolchain.toml"), "[toolchain]\nchannel = \"0.99.0\"\n")
            .unwrap();
        let default = Arc::new(RegistryState::default());
        let mut options = InitializationOptions {
            midenup_home: Some(project.join("home")),
            ..Default::default()
        };
        let error = registry_for_document(&options, &default, &project.join("mod.masm"))
            .err()
            .unwrap();
        assert!(error.contains("midenup install 0.99.0"));
        std::fs::create_dir(project.join("lib")).unwrap();
        options.sysroot = Some(project);
        assert!(
            registry_for_document(&options, &default, Path::new("mod.masm"))
                .unwrap()
                .registry()
                .packages()
                .is_empty()
        );
    }

    #[test]
    fn selected_package_cache_is_reused_and_invalidated_when_artifacts_change() {
        let root = directory();
        std::fs::create_dir(root.join("lib")).unwrap();
        let options = InitializationOptions {
            sysroot: Some(root.clone()),
            ..Default::default()
        };
        let default = Arc::new(RegistryState::default());
        let first = registry_for_document(&options, &default, &root.join("mod.masm")).unwrap();
        let second = registry_for_document(&options, &default, &root.join("mod.masm")).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        std::fs::write(root.join("lib/broken.masp"), b"invalid package").unwrap();
        assert!(registry_for_document(&options, &default, &root.join("mod.masm")).is_err());
        std::fs::remove_file(root.join("lib/broken.masp")).unwrap();
        default.invalidate_selections();
        let third = registry_for_document(&options, &default, &root.join("mod.masm")).unwrap();
        assert!(!Arc::ptr_eq(&first, &third));
    }
}
