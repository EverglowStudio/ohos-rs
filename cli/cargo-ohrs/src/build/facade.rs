//! Validation and publication of the UniFFI public ArkTS facade.
//!
//! `cargo-ohrs` still generates `index.d.ts` from the native N-API type
//! definition stream.  That declaration is an implementation input and is
//! deliberately kept separate from the public UniFFI facade.  The latter is
//! supplied explicitly as a pair of ordinary files and is copied byte-for-
//! byte into the package distribution directory.

use anyhow::{bail, Context as _};
use std::fs;
use std::path::{Path, PathBuf};

const PUBLIC_FACADE_FILES: [&str; 2] = ["Index.ets", "Index.d.ets"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicFacade {
  source_dir: PathBuf,
}

impl PublicFacade {
  /// Validate an explicitly supplied public facade directory.
  ///
  /// This intentionally does not inspect or parse either file.  The pair is
  /// an opaque generated input owned by UniFFI and must be copied unchanged.
  pub(crate) fn from_dir(source_dir: PathBuf) -> anyhow::Result<Self> {
    if !source_dir.is_dir() {
      bail!(
        "public facade directory does not exist or is not a directory: {}",
        source_dir.display()
      );
    }

    for file_name in PUBLIC_FACADE_FILES {
      let path = source_dir.join(file_name);
      let metadata = fs::symlink_metadata(&path).with_context(|| {
        format!(
          "public facade requires {} as an ordinary file: {}",
          file_name,
          path.display()
        )
      })?;
      if !metadata.file_type().is_file() {
        bail!(
          "public facade requires {} as an ordinary file: {}",
          file_name,
          path.display()
        );
      }
    }

    Ok(Self { source_dir })
  }

  pub(crate) fn source_dir(&self) -> &Path {
    &self.source_dir
  }

  /// Check the destination before a native build starts.
  ///
  /// A pre-existing entry is a publication conflict.  Refusing it avoids
  /// overwriting a facade that belongs to another generation and, more
  /// importantly, makes all errors happen before any cargo build work.
  pub(crate) fn validate_destination(&self, dist: &Path) -> anyhow::Result<()> {
    for file_name in PUBLIC_FACADE_FILES {
      let path = dist.join(file_name);
      if fs::symlink_metadata(&path).is_ok() {
        bail!(
          "public facade destination conflict: {} already exists; choose an empty dist or remove the existing facade",
          path.display()
        );
      }
    }
    Ok(())
  }

  /// Copy both public facade files to `dist` without changing their bytes.
  ///
  /// Validation is repeated immediately before copy so a destination created
  /// after the pre-build check still fails rather than being overwritten.
  pub(crate) fn copy_to(&self, dist: &Path) -> anyhow::Result<()> {
    self.validate_destination(dist)?;

    for file_name in PUBLIC_FACADE_FILES {
      let source = self.source_dir.join(file_name);
      let destination = dist.join(file_name);
      fs::copy(&source, &destination).with_context(|| {
        format!(
          "copy public facade {} to {} failed",
          source.display(),
          destination.display()
        )
      })?;
    }
    Ok(())
  }
}

/// A single public facade directory cannot safely describe more than one
/// package's generated API.  Callers must select one package explicitly when
/// building from a multi-package workspace.
pub(crate) fn validate_workspace_selection(
  facade: Option<&PublicFacade>,
  package_count: usize,
) -> anyhow::Result<()> {
  if facade.is_some() && package_count > 1 {
    bail!(
      "--public-facade-dir is ambiguous for a workspace build selecting {} packages; pass --package to select exactly one package",
      package_count
    );
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicU64, Ordering};
  use std::time::{SystemTime, UNIX_EPOCH};

  static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

  struct TempTree {
    root: PathBuf,
  }

  impl TempTree {
    fn new() -> Self {
      let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
      let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
      let root = std::env::temp_dir().join(format!(
        "cargo-ohrs-public-facade-{}-{}-{}",
        std::process::id(),
        nonce,
        suffix
      ));
      fs::create_dir_all(&root).expect("create test temp tree");
      Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
      self.root.join(name)
    }

    fn source(&self, ets: &[u8], d_ets: &[u8]) -> PathBuf {
      let source = self.path("source");
      fs::create_dir_all(&source).expect("create source");
      fs::write(source.join("Index.ets"), ets).expect("write Index.ets");
      fs::write(source.join("Index.d.ets"), d_ets).expect("write Index.d.ets");
      source
    }
  }

  impl Drop for TempTree {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.root);
    }
  }

  #[test]
  fn copies_the_pair_byte_for_byte() {
    let tree = TempTree::new();
    let ets = b"export const \0 ark = '\xE4\xB8\xAD';\n";
    let d_ets = b"export declare const ark: string;\n";
    let facade = PublicFacade::from_dir(tree.source(ets, d_ets)).expect("valid facade");
    let dist = tree.path("dist");
    fs::create_dir_all(&dist).expect("create dist");
    fs::write(dist.join("index.d.ts"), b"raw native declaration").expect("write raw dts");
    facade
      .validate_destination(&dist)
      .expect("empty destination");
    facade.copy_to(&dist).expect("copy facade");
    assert_eq!(fs::read(dist.join("Index.ets")).unwrap(), ets);
    assert_eq!(fs::read(dist.join("Index.d.ets")).unwrap(), d_ets);
    assert_eq!(
      fs::read(dist.join("index.d.ts")).unwrap(),
      b"raw native declaration"
    );
  }

  #[test]
  fn rejects_missing_index_ets() {
    let tree = TempTree::new();
    let source = tree.path("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("Index.d.ets"), b"declare").unwrap();
    let error = PublicFacade::from_dir(source).unwrap_err().to_string();
    assert!(error.contains("Index.ets"), "{error}");
  }

  #[test]
  fn rejects_missing_index_d_ets() {
    let tree = TempTree::new();
    let source = tree.path("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("Index.ets"), b"source").unwrap();
    let error = PublicFacade::from_dir(source).unwrap_err().to_string();
    assert!(error.contains("Index.d.ets"), "{error}");
  }

  #[test]
  fn rejects_missing_source_directory() {
    let tree = TempTree::new();
    let error = PublicFacade::from_dir(tree.path("missing"))
      .unwrap_err()
      .to_string();
    assert!(error.contains("directory"), "{error}");
  }

  #[test]
  fn rejects_destination_conflict_before_copy() {
    let tree = TempTree::new();
    let facade = PublicFacade::from_dir(tree.source(b"new", b"new types")).unwrap();
    let dist = tree.path("dist");
    fs::create_dir_all(&dist).unwrap();
    fs::write(dist.join("Index.ets"), b"old").unwrap();
    let error = facade.copy_to(&dist).unwrap_err().to_string();
    assert!(error.contains("destination conflict"), "{error}");
    assert_eq!(fs::read(dist.join("Index.ets")).unwrap(), b"old");
    assert!(!dist.join("Index.d.ets").exists());
  }

  #[test]
  fn unspecified_facade_keeps_existing_behavior() {
    validate_workspace_selection(None, 4).expect("no facade means no new workspace restriction");
  }

  #[test]
  fn rejects_ambiguous_workspace_selection() {
    let tree = TempTree::new();
    let facade = PublicFacade::from_dir(tree.source(b"source", b"declaration")).unwrap();
    let error = validate_workspace_selection(Some(&facade), 2)
      .unwrap_err()
      .to_string();
    assert!(error.contains("--package"), "{error}");
  }
}
