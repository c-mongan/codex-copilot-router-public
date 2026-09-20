use serde::Deserialize;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::Path;
use thiserror::Error;

const MAX_CATALOG_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Error)]
#[error("Copilot model catalog is unavailable or invalid")]
pub struct CatalogError;

#[derive(Deserialize)]
struct Catalog {
    models: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    slug: String,
}

pub fn load_copilot_catalog(path: &Path) -> Result<HashSet<String>, CatalogError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| CatalogError)?;
    if !metadata.is_file() || metadata.len() > MAX_CATALOG_BYTES {
        return Err(CatalogError);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| CatalogError)?;
    let opened = file.metadata().map_err(|_| CatalogError)?;
    if !opened.is_file() || opened.len() > MAX_CATALOG_BYTES {
        return Err(CatalogError);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(CatalogError);
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CatalogError)?;
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(CatalogError);
    }
    let catalog: Catalog = serde_json::from_slice(&bytes).map_err(|_| CatalogError)?;
    let mut slugs = HashSet::new();
    for model in catalog.models {
        if model.slug.is_empty()
            || model.slug.trim() != model.slug
            || model.slug.chars().any(char::is_control)
            || model.slug == "copilot/"
            || !slugs.insert(model.slug)
        {
            return Err(CatalogError);
        }
    }
    slugs.retain(|slug| slug.starts_with("copilot/"));
    Ok(slugs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{tempdir, NamedTempFile};

    fn catalog_file(contents: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents).unwrap();
        file
    }

    #[test]
    fn only_explicit_copilot_slugs_are_allowed() {
        let file = catalog_file(
            br#"{"models":[{"slug":"native","opaque":{"any":true}},{"slug":"copilot/new-model"}]}"#,
        );
        assert_eq!(
            load_copilot_catalog(file.path()).unwrap(),
            HashSet::from(["copilot/new-model".to_owned()])
        );
        let native_only = catalog_file(br#"{"models":[{"slug":"native"}]}"#);
        assert!(load_copilot_catalog(native_only.path()).unwrap().is_empty());
    }

    #[test]
    fn malformed_or_ambiguous_catalogs_are_rejected_as_a_whole() {
        for contents in [
            r#"{}"#,
            r#"{"models":null}"#,
            r#"{"models":[{}]}"#,
            r#"{"models":[{"slug":8}]}"#,
            r#"{"models":[{"slug":""}]}"#,
            r#"{"models":[{"slug":"copilot/"}]}"#,
            r#"{"models":[{"slug":"copilot/new"},{"slug":"copilot/new"}]}"#,
            r#"{"models":[{"slug":"copilot/new","slug":"native"}]}"#,
            r#"{"models":[],"models":[{"slug":"copilot/new"}]}"#,
            r#"{"models":[{"slug":"copilot/new"},{}]}"#,
        ] {
            let file = catalog_file(contents.as_bytes());
            assert!(load_copilot_catalog(file.path()).is_err());
        }
    }

    #[test]
    fn oversized_missing_and_nonregular_catalogs_fail_safely() {
        let file = catalog_file(&[]);
        file.as_file().set_len(MAX_CATALOG_BYTES + 1).unwrap();
        assert!(load_copilot_catalog(file.path()).is_err());
        let dir = tempdir().unwrap();
        assert!(load_copilot_catalog(&dir.path().join("absent")).is_err());
        assert!(load_copilot_catalog(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_catalogs() {
        let file = catalog_file(br#"{"models":[]}"#);
        let dir = tempdir().unwrap();
        let link = dir.path().join("catalog");
        std::os::unix::fs::symlink(file.path(), &link).unwrap();
        assert!(load_copilot_catalog(&link).is_err());
    }
}
