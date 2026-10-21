//! Unpacking a task layer into the cache. The blob is checked against its digest first and hashed again while it
//! is read; entries go into a temp dir beside the cache entry, which is renamed into place only once the layer
//! digest and the task's package digest match. Absolute paths, `..`, links and device files are refused, as are
//! symlinks that leave the task directory.

use super::format::Layer;
use super::{Sha256Reader, verify_file};
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

const DIR_MODE: u32 = 0o755;
const EXECUTABLE_MODE: u32 = 0o755;
const FILE_MODE: u32 = 0o644;
const ANY_EXEC_BIT: u32 = 0o111;

/// Unpack `layer` from the blob at `blob` into `dest`, which is named after the task.
pub fn unpack(blob: &Path, layer: &Layer, dest: &Path) -> Result<()> {
    verify_file(blob, &layer.digest, layer.size).with_context(|| format!("task {}", layer.name))?;
    let parent = dest.parent().context("task cache dir has no parent")?;
    std::fs::create_dir_all(parent)?;
    let sibling = |tag: &str| parent.join(format!(".{}.{tag}-{}", layer.name, uuid::Uuid::new_v4().simple()));
    let tmp = sibling("tmp");
    let unpacked = extract(blob, layer, &tmp).and_then(|_| {
        let digest = crate::task::package_digest(&tmp);
        if digest != layer.package_digest {
            bail!(
                "task {}: the unpacked files do not match its package digest (expected {}, got {})",
                layer.name,
                layer.package_digest,
                if digest.is_empty() { "none" } else { &digest }
            );
        }
        Ok(())
    });
    if unpacked.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
        return unpacked;
    }
    let old = sibling("old");
    let _ = std::fs::rename(dest, &old);
    if std::fs::rename(&tmp, dest).is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

fn extract(blob: &Path, layer: &Layer, root: &Path) -> Result<()> {
    std::fs::create_dir(root).with_context(|| format!("creating {}", root.display()))?;
    let mut hashed = Sha256Reader::new(std::fs::File::open(blob).with_context(|| format!("reading {}", blob.display()))?);
    {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&mut hashed));
        for entry in archive.entries().with_context(|| format!("task {}: not a tar archive", layer.name))? {
            let mut entry = entry.with_context(|| format!("task {}: reading the layer", layer.name))?;
            write_entry(root, &mut entry).with_context(|| format!("task {}", layer.name))?;
        }
        std::io::copy(&mut archive.into_inner(), &mut std::io::sink())?;
    }
    std::io::copy(&mut hashed, &mut std::io::sink())?;
    let (digest, size) = hashed.finish();
    if digest != layer.digest || size != layer.size {
        bail!("task {}: the layer changed while it was unpacked ({digest}, {size} bytes; expected {})", layer.name, layer.digest);
    }
    check_symlinks(root, root).with_context(|| format!("task {}", layer.name))
}

/// `path` as a relative path of plain components; anything that could reach outside the task dir is refused.
fn confined(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => bail!("{}: absolute paths and .. are not allowed in a task layer", path.display()),
        }
    }
    Ok(out)
}

/// Create the directories above `rel` that are missing, refusing a parent that is a symlink or a file, so an
/// entry is never written through a link.
fn real_parents(root: &Path, rel: &Path) -> Result<()> {
    let mut dir = root.to_path_buf();
    for part in rel.parent().into_iter().flat_map(Path::components) {
        dir.push(part);
        match dir.symlink_metadata() {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{}: a parent is not a directory", rel.display()),
            Err(_) => {
                std::fs::create_dir(&dir)?;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(DIR_MODE))?;
            }
        }
    }
    Ok(())
}

fn write_entry<R: Read>(root: &Path, entry: &mut tar::Entry<R>) -> Result<()> {
    let kind = entry.header().entry_type();
    if kind == tar::EntryType::XGlobalHeader {
        return Ok(());
    }
    let rel = confined(&entry.path()?)?;
    if rel.as_os_str().is_empty() {
        return if kind.is_dir() { Ok(()) } else { bail!("an entry with an empty path") };
    }
    real_parents(root, &rel)?;
    let target = root.join(&rel);
    match kind {
        tar::EntryType::Directory => {
            match target.symlink_metadata() {
                Ok(m) if m.is_dir() => {}
                Ok(_) => bail!("{}: listed twice", rel.display()),
                Err(_) => std::fs::create_dir(&target)?,
            }
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(DIR_MODE))?;
        }
        tar::EntryType::Regular | tar::EntryType::Continuous => {
            let mode = if entry.header().mode()? & ANY_EXEC_BIT != 0 { EXECUTABLE_MODE } else { FILE_MODE };
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .with_context(|| format!("{}: listed twice or not writable", rel.display()))?;
            std::io::copy(entry, &mut file)?;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
        }
        tar::EntryType::Symlink => {
            let link = entry.link_name()?.with_context(|| format!("{}: symlink without a target", rel.display()))?;
            if !stays_inside(&rel, &link) {
                bail!("{}: symlink to {} leaves the task directory", rel.display(), link.display());
            }
            std::os::unix::fs::symlink(&link, &target).with_context(|| format!("{}: listed twice", rel.display()))?;
        }
        other => bail!("{}: {other:?} entries are not allowed in a task layer", rel.display()),
    }
    Ok(())
}

/// Whether a symlink at `rel` pointing at `link` resolves, by its path alone, inside the task dir.
pub(super) fn stays_inside(rel: &Path, link: &Path) -> bool {
    let mut depth = rel.components().count().saturating_sub(1);
    for c in link.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            _ => return false,
        }
    }
    true
}

/// Every symlink under `dir` resolves to an existing path inside `root`, following chains of links.
pub(super) fn check_symlinks(root: &Path, dir: &Path) -> Result<()> {
    let canonical_root = root.canonicalize()?;
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        let meta = p.symlink_metadata()?;
        if meta.file_type().is_symlink() {
            let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
            match p.canonicalize() {
                Ok(t) if t.starts_with(&canonical_root) => {}
                Ok(_) => bail!("{rel}: symlink leaves the task directory"),
                Err(_) => bail!("{rel}: symlink points at nothing"),
            }
        } else if meta.is_dir() {
            check_symlinks(root, &p)?;
        }
    }
    Ok(())
}
