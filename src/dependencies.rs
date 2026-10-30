use std::path::{Path, PathBuf};

use miden_assembly_syntax::debuginfo::Uri;
use miden_core::utils::{DisplayHex, hash_string_to_word};
use miden_project::GitRevision;

pub(crate) fn default_git_cache_root() -> PathBuf {
    std::env::var_os("MIDENUP_HOME")
        .map(PathBuf::from)
        .map(|home| home.join("git/checkouts"))
        .unwrap_or_else(|| std::env::temp_dir().join("midenup/git/checkouts"))
}

pub(crate) fn cached_manifest(text: &str, cache: &Path) -> Option<String> {
    let mut manifest = toml::from_str::<toml::Value>(text).ok()?;
    let mut changed = false;
    if let Some(dependencies) = manifest.get_mut("dependencies").and_then(toml::Value::as_table_mut)
    {
        changed |= reuse_checkouts(dependencies, cache);
    }
    if let Some(dependencies) = manifest
        .get_mut("workspace")
        .and_then(|workspace| workspace.get_mut("dependencies"))
        .and_then(toml::Value::as_table_mut)
    {
        changed |= reuse_checkouts(dependencies, cache);
    }
    changed.then(|| toml::to_string(&manifest).ok()).flatten()
}

fn reuse_checkouts(dependencies: &mut toml::map::Map<String, toml::Value>, cache: &Path) -> bool {
    let mut changed = false;
    for dependency in dependencies.iter_mut().filter_map(|(_, value)| value.as_table_mut()) {
        if dependency.contains_key("path")
            || dependency.contains_key("digest")
            || dependency.get("workspace").is_some_and(|value| value.as_bool() != Some(false))
            || ["branch", "rev"]
                .iter()
                .any(|key| dependency.get(*key).is_some_and(|value| !value.is_str()))
            || dependency.get("version").is_some_and(|value| {
                value
                    .as_str()
                    .is_none_or(|version| version.parse::<miden_project::VersionReq>().is_err())
            })
        {
            continue;
        }
        let Some(repo) = dependency.get("git").and_then(toml::Value::as_str) else {
            continue;
        };
        let revision = match (
            dependency.get("branch").and_then(toml::Value::as_str),
            dependency.get("rev").and_then(toml::Value::as_str),
        ) {
            (Some(branch), None) if !branch.is_empty() => GitRevision::Branch(branch.into()),
            (None, Some(revision)) if !revision.is_empty() => GitRevision::Commit(revision.into()),
            _ => continue,
        };
        let key = hash_string_to_word(format!("{}@{revision}", Uri::new(repo)).as_str());
        let checkout = cache.join(format!("0x{}", DisplayHex::new(&key.as_bytes())));
        if !checkout.join("miden-project.toml").is_file() {
            continue;
        }
        let Some(path) = checkout.to_str().map(str::to_owned) else {
            continue;
        };
        dependency.remove("git");
        dependency.remove("branch");
        dependency.remove("rev");
        dependency.insert("path".into(), path.into());
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_git_dependencies_keep_version_and_linkage_without_fetching_or_resetting() {
        let root = tempfile::tempdir().unwrap();
        let repo = Uri::new("https://example.invalid/dep");
        for revision in [GitRevision::Branch("main".into()), GitRevision::Commit("abc123".into())] {
            let key = hash_string_to_word(format!("{repo}@{revision}").as_str());
            let checkout = root.path().join(format!("0x{}", DisplayHex::new(&key.as_bytes())));
            std::fs::create_dir_all(&checkout).unwrap();
            std::fs::write(checkout.join("miden-project.toml"), "cached manifest").unwrap();
            let selector = match revision {
                GitRevision::Branch(_) => "branch = 'main'",
                GitRevision::Commit(_) => "rev = 'abc123'",
            };
            let text = format!(
                "[dependencies]\ndep = {{ git = '{repo}', {selector}, version = '1.0', linkage = \
                 'static' }}\n"
            );
            let frozen =
                toml::from_str::<toml::Value>(&cached_manifest(&text, root.path()).unwrap())
                    .unwrap();
            let dependency = &frozen["dependencies"]["dep"];
            assert_eq!(dependency["path"].as_str(), checkout.to_str());
            assert_eq!(dependency["version"].as_str(), Some("1.0"));
            assert_eq!(dependency["linkage"].as_str(), Some("static"));
            assert!(dependency.get("git").is_none());
            assert_eq!(
                std::fs::read_to_string(checkout.join("miden-project.toml")).unwrap(),
                "cached manifest"
            );
        }
    }

    #[test]
    fn uncached_and_invalid_git_dependencies_are_left_for_upstream_validation() {
        let root = tempfile::tempdir().unwrap();
        let uncached =
            "[dependencies]\ndep = { git = 'https://example.invalid/dep', branch = 'main' }";
        assert!(cached_manifest(uncached, root.path()).is_none());
        let key = hash_string_to_word("https://example.invalid/dep@main");
        let checkout = root.path().join(format!("0x{}", DisplayHex::new(&key.as_bytes())));
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("miden-project.toml"), "cached manifest").unwrap();
        for text in [
            "[dependencies]\ndep = { git = 'https://example.invalid/dep', branch = 'main', rev = \
             'abc' }",
            "[dependencies]\ndep = { git = 'https://example.invalid/dep', branch = 'main', digest \
             = 'invalid' }",
            "[dependencies]\ndep = { git = 'https://example.invalid/dep', branch = 'main', rev = \
             1 }",
            "[dependencies]\ndep = { git = 'https://example.invalid/dep', branch = 'main', \
             version = 'invalid' }",
        ] {
            assert!(cached_manifest(text, root.path()).is_none());
        }
    }

    #[test]
    fn workspace_dependencies_reuse_checkouts_without_changing_inheritance_or_metadata() {
        let root = tempfile::tempdir().unwrap();
        let key = hash_string_to_word("https://example.invalid/dep@main");
        let checkout = root.path().join(format!("0x{}", DisplayHex::new(&key.as_bytes())));
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("miden-project.toml"), "cached manifest").unwrap();
        let text = "[workspace.dependencies]\ndep = { git = 'https://example.invalid/dep', branch \
                    = 'main' }\n[dependencies]\ndep = { workspace = true \
                    }\n[package.metadata]\nmessage = 'unchanged'\n";
        let frozen =
            toml::from_str::<toml::Value>(&cached_manifest(text, root.path()).unwrap()).unwrap();
        assert_eq!(frozen["workspace"]["dependencies"]["dep"]["path"].as_str(), checkout.to_str());
        assert_eq!(frozen["dependencies"]["dep"]["workspace"].as_bool(), Some(true));
        assert_eq!(frozen["package"]["metadata"]["message"].as_str(), Some("unchanged"));
    }
}
