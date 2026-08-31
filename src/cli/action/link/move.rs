//! `link move` action — relocate one filesystem object while preserving
//! every supported `[[file:...]]` edge in an explicitly scanned corpus.
//!
//! Planning is read-only: it validates boundaries, reads every `.org` source,
//! resolves links through [`crate::org::link`], and prepares complete rewritten
//! contents. A write requires the corpus and path tree to stay quiescent. One
//! nonblocking lock on the canonical ROOT directory serializes cooperating
//! `org-cli link move` writers using that same canonical ROOT across planning,
//! execution, and postflight. Nested or overlapping different ROOT values do
//! not share a lock. Freshness checks catch common stale-file or symlink
//! substitutions already present at each write boundary. They cannot protect
//! against an uncooperative actor changing paths between system calls.
//! Writing renames the object, then atomically replaces each changed org file.
//! Ordinary I/O or graph-verification failures trigger a best-effort rollback.
//! There is deliberately no adversarial-atomicity or crash-recovery claim: a
//! durable dirfd transaction/journal would be required for either.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use similar::TextDiff;

use crate::cli::action::walk::collect_org_files;
use crate::org::heading_edit::write_atomic;
use crate::org::link::{
    Link, LinkPathRewrite, OriginPolicy, authored_target, extract_file_links, resolve_target,
    rewrite_file_link_paths,
};

use super::check;

const POLICY: OriginPolicy = OriginPolicy::AllFollowable;

/// CLI options received from `LinkCommands::Move`.
#[derive(Debug, Clone)]
pub struct MoveOptions {
    pub old: PathBuf,
    pub new: PathBuf,
    pub root: PathBuf,
    pub write: bool,
}

#[derive(Debug, Clone)]
struct PreparedFile {
    source_path: PathBuf,
    post_path: PathBuf,
    original: String,
    rewritten: String,
}

#[derive(Debug, Clone)]
struct MovePlan {
    root: PathBuf,
    old: PathBuf,
    new: PathBuf,
    files: Vec<PreparedFile>,
    link_count: usize,
}

#[derive(Debug)]
struct RootLock {
    _directory: fs::File,
}

impl RootLock {
    #[cfg(target_os = "linux")]
    fn acquire(root: &Path) -> Result<Self, String> {
        use std::os::fd::AsRawFd;

        let directory = fs::File::open(root).map_err(|error| {
            format!("cannot open ROOT {} for locking: {}", root.display(), error)
        })?;
        // SAFETY: flock only reads the valid directory fd owned by `directory`.
        // The file stays alive in `RootLock` for the full write execution.
        let result = unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK)
                || error.raw_os_error() == Some(libc::EAGAIN)
            {
                return Err(format!(
                    "another org-cli link move holds the cooperative lock on ROOT {}",
                    root.display()
                ));
            }
            return Err(format!(
                "cannot acquire cooperative lock on ROOT {}: {}",
                root.display(),
                error
            ));
        }
        Ok(Self {
            _directory: directory,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn acquire(_root: &Path) -> Result<Self, String> {
        Err("link move --write requires Linux flock and renameat2 support".to_string())
    }
}

/// Plan and either display or apply one link-preserving move.
pub fn run(opts: MoveOptions) -> Result<i32, String> {
    let _root_lock = if opts.write {
        let root = canonical_root(&opts.root)?;
        Some(RootLock::acquire(&root)?)
    } else {
        None
    };
    let plan = prepare_plan(&opts)?;
    if opts.write {
        execute_plan(&plan)?;
        println!("Moved {} -> {}", plan.old.display(), plan.new.display());
        println!(
            "Rewrote {} link(s) in {} org file(s)",
            plan.link_count,
            plan.files.len()
        );
    } else {
        present_dry_run(&plan)?;
    }
    Ok(0)
}

fn prepare_plan(opts: &MoveOptions) -> Result<MovePlan, String> {
    let root = canonical_root(&opts.root)?;

    let old_input = absolute_authored(&opts.old)?;
    let old_meta = fs::symlink_metadata(&old_input).map_err(|e| {
        format!(
            "OLD does not exist or cannot be inspected: {}: {}",
            old_input.display(),
            e
        )
    })?;
    if old_meta.file_type().is_symlink() {
        return Err(format!("OLD may not be a symlink: {}", old_input.display()));
    }
    if !old_meta.is_file() && !old_meta.is_dir() {
        return Err(format!(
            "OLD is neither a file nor directory: {}",
            old_input.display()
        ));
    }
    let old = fs::canonicalize(&old_input)
        .map_err(|e| format!("cannot resolve OLD {}: {}", old_input.display(), e))?;

    let new_input = absolute_authored(&opts.new)?;
    if fs::symlink_metadata(&new_input).is_ok() {
        return Err(format!("NEW already exists: {}", new_input.display()));
    }
    let new_name = new_input
        .file_name()
        .ok_or_else(|| format!("NEW has no final path component: {}", new_input.display()))?;
    let new_parent_input = new_input
        .parent()
        .ok_or_else(|| format!("NEW has no destination parent: {}", new_input.display()))?;
    let new_parent = fs::canonicalize(new_parent_input).map_err(|e| {
        format!(
            "destination parent does not exist or cannot be resolved: {}: {}",
            new_parent_input.display(),
            e
        )
    })?;
    if !new_parent.is_dir() {
        return Err(format!(
            "destination parent is not a directory: {}",
            new_parent.display()
        ));
    }
    let new = new_parent.join(new_name);

    ensure_within("OLD", &old, &root)?;
    ensure_within("NEW", &new, &root)?;
    if old == root {
        return Err("OLD may not be the scanned root itself".to_string());
    }
    if new == old {
        return Err("OLD and NEW resolve to the same path".to_string());
    }
    if old_meta.is_dir() && new.starts_with(&old) {
        return Err(format!(
            "NEW may not be inside OLD: {} is under {}",
            new.display(),
            old.display()
        ));
    }

    let org_files = collect_org_files(std::slice::from_ref(&root))?;
    check::validate_corpus(&org_files)
        .map_err(|error| format!("initial link validation failed: {}", error))?;
    let mut prepared = Vec::new();
    let mut post_paths = HashSet::new();
    let mut total_links = 0;

    for source_path in org_files {
        let original = fs::read_to_string(&source_path).map_err(|e| {
            format!(
                "cannot read affected corpus source {}: {}",
                source_path.display(),
                e
            )
        })?;
        let post_path = map_moved_path(&source_path, &old, &new);
        if !post_paths.insert(post_path.clone()) {
            return Err(format!(
                "ambiguous move maps multiple org sources to {}",
                post_path.display()
            ));
        }

        let source_moved = source_path != post_path;
        let source_dir = source_path.parent().unwrap_or(Path::new("."));
        let post_source_dir = post_path.parent().unwrap_or(Path::new("."));
        let mut rewrites = Vec::new();

        for link in extract_file_links(&original) {
            if !POLICY.admits(&link.origin) {
                continue;
            }
            let resolved_pre = resolve_target(source_dir, &link).path;
            let authored_unresolved = authored_target(source_dir, &link);
            let authored_tilde = link.raw_target == "~" || link.raw_target.starts_with("~/");
            let source_rebased = source_moved && !link.target_path.is_absolute() && !authored_tilde;
            let syntactically_moved = authored_unresolved.starts_with(&old);
            let resolved_moved = resolved_pre.starts_with(&old);
            if traverses_symlink_before_parent(source_dir, &link)?
                && (syntactically_moved || resolved_moved || source_rebased)
            {
                return Err(format!(
                    "cannot safely rewrite link at {}:{}:{}: symlink component followed by '..'",
                    source_path.display(),
                    link.line,
                    link.column
                ));
            }
            if let Some((symlink, terminal)) =
                relative_symlink_component_under(&authored_unresolved, &old)?
            {
                let reason = if terminal {
                    "relative terminal symlink under OLD"
                } else {
                    "relative symlink component under OLD"
                };
                return Err(format!(
                    "cannot safely rewrite link at {}:{}:{}: {}: {}",
                    source_path.display(),
                    link.line,
                    link.column,
                    reason,
                    symlink.display()
                ));
            }

            let authored_pre = lexical_normalize(&authored_unresolved);
            let authored_moved = authored_pre.starts_with(&old);
            let resolved_post = if authored_moved {
                map_moved_path(&authored_pre, &old, &new)
            } else if resolved_moved {
                map_moved_path(&resolved_pre, &old, &new)
            } else {
                resolved_pre.clone()
            };
            let target_moved = authored_moved || resolved_moved;

            if !target_moved && !source_rebased {
                continue;
            }
            if authored_tilde && target_moved {
                return Err(format!(
                    "cannot safely rewrite moved target expressed with '~' at {}:{}:{}",
                    source_path.display(),
                    link.line,
                    link.column
                ));
            }

            let replacement_path = if link.target_path.is_absolute() {
                resolved_post.clone()
            } else {
                relative_path(post_source_dir, &resolved_post)?
            };
            let replacement = render_target(
                &replacement_path,
                &link.raw_target,
                link.target_is_directory,
            )?;
            if replacement == link.raw_target {
                continue;
            }
            verify_rendered_target(&replacement, post_source_dir, &resolved_post).map_err(
                |error| {
                    format!(
                        "cannot safely rewrite link at {}:{}:{}: {}",
                        source_path.display(),
                        link.line,
                        link.column,
                        error
                    )
                },
            )?;
            rewrites.push(LinkPathRewrite {
                span: link.target_span,
                replacement,
            });
        }

        if !rewrites.is_empty() {
            let rewritten = rewrite_file_link_paths(&original, &rewrites)?;
            total_links += rewrites.len();
            prepared.push(PreparedFile {
                source_path,
                post_path,
                original,
                rewritten,
            });
        }
    }

    prepared.sort_by(|a, b| a.post_path.cmp(&b.post_path));
    Ok(MovePlan {
        root,
        old,
        new,
        files: prepared,
        link_count: total_links,
    })
}

fn canonical_root(path: &Path) -> Result<PathBuf, String> {
    let root = fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve root {}: {}", path.display(), error))?;
    if !root.is_dir() {
        return Err(format!("root is not a directory: {}", root.display()));
    }
    Ok(root)
}

fn ensure_within(label: &str, path: &Path, root: &Path) -> Result<(), String> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(format!(
            "{} is outside ROOT: {} is not under {}",
            label,
            path.display(),
            root.display()
        ))
    }
}

fn absolute_authored(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|e| format!("cannot determine current directory: {}", e))?
            .join(path))
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn traverses_symlink_before_parent(source_dir: &Path, link: &Link) -> Result<bool, String> {
    let authored = authored_target(source_dir, link);
    let mut current = PathBuf::new();
    let mut traversed_symlink = false;

    for component in authored.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                current.push(component.as_os_str());
                if matches!(component, Component::Normal(_)) {
                    let metadata = fs::symlink_metadata(&current).map_err(|error| {
                        format!(
                            "cannot inspect link path component {}: {}",
                            current.display(),
                            error
                        )
                    })?;
                    traversed_symlink |= metadata.file_type().is_symlink();
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if traversed_symlink {
                    return Ok(true);
                }
                current.pop();
            }
        }
    }
    Ok(false)
}

fn relative_symlink_component_under(
    path: &Path,
    old: &Path,
) -> Result<Option<(PathBuf, bool)>, String> {
    let terminal = lexical_normalize(path);
    let mut current = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                current.push(component.as_os_str());
                if !matches!(component, Component::Normal(_))
                    || !lexical_normalize(&current).starts_with(old)
                {
                    continue;
                }
                let metadata = fs::symlink_metadata(&current).map_err(|error| {
                    format!(
                        "cannot inspect authored link component {}: {}",
                        current.display(),
                        error
                    )
                })?;
                if !metadata.file_type().is_symlink() {
                    continue;
                }
                let payload = fs::read_link(&current).map_err(|error| {
                    format!(
                        "cannot read symlink component {}: {}",
                        current.display(),
                        error
                    )
                })?;
                if !payload.is_absolute() {
                    let is_terminal = lexical_normalize(&current) == terminal;
                    return Ok(Some((current, is_terminal)));
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                current.pop();
            }
        }
    }
    Ok(None)
}

fn verify_rendered_target(
    rendered: &str,
    source_dir: &Path,
    intended_target: &Path,
) -> Result<(), String> {
    let synthetic = format!("[[file:{}]]", rendered);
    let links = extract_file_links(&synthetic);
    let [parsed] = links.as_slice() else {
        return Err("generated target does not parse as exactly one file link".to_string());
    };
    let actual = lexical_normalize(&authored_target(source_dir, parsed));
    let intended = lexical_normalize(intended_target);
    if actual != intended {
        return Err(format!(
            "generated target resolves to {}, expected {}",
            actual.display(),
            intended.display()
        ));
    }
    Ok(())
}

fn map_moved_path(path: &Path, old: &Path, new: &Path) -> PathBuf {
    path.strip_prefix(old)
        .map(|suffix| new.join(suffix))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn relative_path(from_dir: &Path, target: &Path) -> Result<PathBuf, String> {
    if !from_dir.is_absolute() || !target.is_absolute() {
        return Err("internal error: relative-path inputs must be absolute".to_string());
    }
    let from: Vec<_> = from_dir.components().collect();
    let to: Vec<_> = target.components().collect();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return Err(format!(
            "cannot compute relative path from {} to {}",
            from_dir.display(),
            target.display()
        ));
    }
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for component in &to[common..] {
        out.push(component.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    Ok(out)
}

fn percent_style_bytes(raw: &str) -> HashSet<u8> {
    raw.as_bytes()
        .windows(3)
        .filter_map(|bytes| {
            if bytes[0] != b'%' {
                return None;
            }
            let pair = std::str::from_utf8(&bytes[1..3]).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

fn render_percent_style(raw: &str, original_raw: &str) -> String {
    let styled = percent_style_bytes(original_raw);
    let mut out = String::new();
    for character in raw.chars() {
        let mut encoded = [0; 4];
        let bytes = character.encode_utf8(&mut encoded).as_bytes();
        let must_encode = bytes.iter().any(|byte| {
            styled.contains(byte)
                || byte.is_ascii_control()
                || *byte == 0x7f
                || matches!(*byte, b' ' | b'%' | b':' | b'[' | b']' | b'~')
        });
        if must_encode {
            for byte in bytes {
                out.push_str(&format!("%{:02X}", byte));
            }
        } else {
            out.push(character);
        }
    }
    out
}

fn render_target(path: &Path, original_raw: &str, directory: bool) -> Result<String, String> {
    let raw = path
        .to_str()
        .ok_or_else(|| format!("cannot represent non-UTF-8 link target: {}", path.display()))?;
    let mut rendered = render_percent_style(raw, original_raw);
    if directory && !rendered.ends_with('/') {
        rendered.push('/');
    }
    Ok(rendered)
}

fn present_dry_run(plan: &MovePlan) -> Result<(), String> {
    let mut stdout = io::BufWriter::new(io::stdout());
    writeln!(
        stdout,
        "Move: {} -> {}",
        plan.old.display(),
        plan.new.display()
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        stdout,
        "Rewrites: {} link(s) in {} org file(s)",
        plan.link_count,
        plan.files.len()
    )
    .map_err(|e| e.to_string())?;
    for file in &plan.files {
        let old_header = format!("a/{}", file.source_path.display());
        let new_header = format!("b/{}", file.post_path.display());
        let diff = TextDiff::from_lines(&file.original, &file.rewritten)
            .unified_diff()
            .header(&old_header, &new_header)
            .to_string();
        write!(stdout, "{}", diff).map_err(|e| e.to_string())?;
    }
    writeln!(stdout, "(dry run; pass --write to apply)").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())
}

fn execute_plan(plan: &MovePlan) -> Result<(), String> {
    let mut writer =
        |path: &Path, content: &str| write_atomic(path, content).map_err(|e| e.to_string());
    let mut mover = |old: &Path, new: &Path| rename_no_replace(old, new);
    execute_plan_with_io(plan, &mut writer, &mut mover)
}

#[cfg(test)]
fn execute_plan_with_writer(
    plan: &MovePlan,
    writer: &mut impl FnMut(&Path, &str) -> Result<(), String>,
) -> Result<(), String> {
    let mut mover = |old: &Path, new: &Path| rename_no_replace(old, new);
    execute_plan_with_io(plan, writer, &mut mover)
}

fn execute_plan_with_io(
    plan: &MovePlan,
    writer: &mut impl FnMut(&Path, &str) -> Result<(), String>,
    mover: &mut impl FnMut(&Path, &Path) -> Result<(), String>,
) -> Result<(), String> {
    mover(&plan.old, &plan.new).map_err(|e| {
        format!(
            "cannot move {} to {}: {}",
            plan.old.display(),
            plan.new.display(),
            e
        )
    })?;

    let mut applied = Vec::new();
    for (index, file) in plan.files.iter().enumerate() {
        if let Err(error) = ensure_fresh(
            &file.post_path,
            file.original.as_bytes(),
            "changed after planning",
        ) {
            let rollback = rollback(plan, &applied, writer, mover);
            return Err(transaction_error(&error, rollback));
        }
        if let Err(error) = writer(&file.post_path, &file.rewritten) {
            let rollback = rollback(plan, &applied, writer, mover);
            return Err(transaction_error(
                &format!("cannot rewrite {}: {}", file.post_path.display(), error),
                rollback,
            ));
        }
        applied.push(index);
    }

    let verification = collect_org_files(std::slice::from_ref(&plan.root))
        .and_then(|files| check::validate_corpus(&files));
    if let Err(error) = verification {
        let all: Vec<usize> = (0..plan.files.len()).collect();
        let rollback = rollback(plan, &all, writer, mover);
        return Err(transaction_error(
            &format!("post-move link verification failed: {}", error),
            rollback,
        ));
    }
    Ok(())
}

fn rollback(
    plan: &MovePlan,
    applied: &[usize],
    writer: &mut impl FnMut(&Path, &str) -> Result<(), String>,
    mover: &mut impl FnMut(&Path, &Path) -> Result<(), String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for index in applied.iter().rev() {
        let file = &plan.files[*index];
        if let Err(error) = ensure_fresh(
            &file.post_path,
            file.rewritten.as_bytes(),
            "changed before rollback restore",
        ) {
            errors.push(error);
            continue;
        }
        if let Err(error) = writer(&file.post_path, &file.original) {
            errors.push(format!("restore {}: {}", file.post_path.display(), error));
        }
    }
    if let Err(error) = mover(&plan.new, &plan.old) {
        errors.push(format!(
            "move {} back to {}: {}",
            plan.new.display(),
            plan.old.display(),
            error
        ));
    }
    errors
}

fn ensure_fresh(path: &Path, expected: &[u8], context: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect path entry {}: {}", path.display(), error))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{}: {} is not a regular file entry",
            context,
            path.display()
        ));
    }
    let current =
        fs::read(path).map_err(|error| format!("cannot re-read {}: {}", path.display(), error))?;
    if current != expected {
        return Err(format!("{}: {}", context, path.display()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_no_replace(old: &Path, new: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let old_bytes = CString::new(old.as_os_str().as_bytes())
        .map_err(|_| format!("path contains NUL: {}", old.display()))?;
    let new_bytes = CString::new(new.as_os_str().as_bytes())
        .map_err(|_| format!("path contains NUL: {}", new.display()))?;
    // SAFETY: both pointers come from live CStrings, both directory fds are
    // AT_FDCWD, and RENAME_NOREPLACE asks the kernel for atomic no-clobber
    // semantics rather than relying on a check-then-rename race.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old_bytes.as_ptr(),
            libc::AT_FDCWD,
            new_bytes.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_no_replace(_old: &Path, _new: &Path) -> Result<(), String> {
    Err("link move --write requires Linux renameat2(RENAME_NOREPLACE)".to_string())
}

fn transaction_error(primary: &str, rollback_errors: Vec<String>) -> String {
    if rollback_errors.is_empty() {
        format!("{}; operation rolled back", primary)
    } else {
        format!(
            "{}; rollback encountered error(s): {}",
            primary,
            rollback_errors.join("; ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn forced_ordinary_write_failure_rolls_back_move_and_prior_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let old = root.join("old.org");
        let new = root.join("new.org");
        let first = root.join("a.org");
        let second = root.join("b.org");
        fs::write(&old, "* H\n").unwrap();
        fs::write(&first, "[[file:old.org][a]]\n").unwrap();
        fs::write(&second, "[[file:old.org][b]]\n").unwrap();
        let plan = prepare_plan(&MoveOptions {
            old: old.clone(),
            new: new.clone(),
            root: root.to_path_buf(),
            write: true,
        })
        .unwrap();

        let mut calls = 0;
        let mut writer = |path: &Path, content: &str| {
            calls += 1;
            if calls == 2 {
                Err("injected ordinary I/O failure".to_string())
            } else {
                write_atomic(path, content).map_err(|e| e.to_string())
            }
        };
        let error = execute_plan_with_writer(&plan, &mut writer).unwrap_err();

        assert!(error.contains("injected ordinary I/O failure"));
        assert!(error.contains("operation rolled back"));
        assert!(old.exists());
        assert!(!new.exists());
        assert_eq!(fs::read_to_string(first).unwrap(), "[[file:old.org][a]]\n");
        assert_eq!(fs::read_to_string(second).unwrap(), "[[file:old.org][b]]\n");
    }

    #[test]
    fn destination_created_after_planning_is_not_clobbered() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let old = root.join("old.txt");
        let new = root.join("new.txt");
        fs::write(&old, "owned by move\n").unwrap();
        let plan = prepare_plan(&MoveOptions {
            old: old.clone(),
            new: new.clone(),
            root: root.to_path_buf(),
            write: true,
        })
        .unwrap();
        let mut writer = |path: &Path, content: &str| {
            write_atomic(path, content).map_err(|error| error.to_string())
        };
        let mut first_move = true;
        let mut mover = |from: &Path, to: &Path| {
            if first_move {
                first_move = false;
                fs::write(to, "foreign destination\n").map_err(|error| error.to_string())?;
            }
            rename_no_replace(from, to)
        };
        let error = execute_plan_with_io(&plan, &mut writer, &mut mover).unwrap_err();

        assert!(error.contains("cannot move"));
        assert_eq!(fs::read_to_string(old).unwrap(), "owned by move\n");
        assert_eq!(fs::read_to_string(new).unwrap(), "foreign destination\n");
    }

    #[test]
    fn source_changed_after_planning_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let old = root.join("old.org");
        let new = root.join("new.org");
        let source = root.join("source.org");
        fs::write(&old, "* H\n").unwrap();
        fs::write(&source, "[[file:old.org][old]]\n").unwrap();
        let plan = prepare_plan(&MoveOptions {
            old: old.clone(),
            new: new.clone(),
            root: root.to_path_buf(),
            write: true,
        })
        .unwrap();
        fs::write(&source, "concurrent edit\n").unwrap();

        let mut writer = |path: &Path, content: &str| {
            write_atomic(path, content).map_err(|error| error.to_string())
        };
        let error = execute_plan_with_writer(&plan, &mut writer).unwrap_err();

        assert!(error.contains("changed after planning"));
        assert_eq!(fs::read_to_string(source).unwrap(), "concurrent edit\n");
        assert!(old.exists());
        assert!(!new.exists());
    }

    #[test]
    fn rollback_does_not_overwrite_concurrent_post_write_edit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let old = root.join("old.org");
        let new = root.join("new.org");
        let first = root.join("a.org");
        let second = root.join("b.org");
        fs::write(&old, "* H\n").unwrap();
        fs::write(&first, "[[file:old.org][a]]\n").unwrap();
        fs::write(&second, "[[file:old.org][b]]\n").unwrap();
        let plan = prepare_plan(&MoveOptions {
            old: old.clone(),
            new: new.clone(),
            root: root.to_path_buf(),
            write: true,
        })
        .unwrap();

        let mut calls = 0;
        let first_for_writer = first.clone();
        let mut writer = |path: &Path, content: &str| {
            calls += 1;
            if calls == 2 {
                fs::write(&first_for_writer, "concurrent post-write edit\n")
                    .map_err(|error| error.to_string())?;
                Err("injected second write failure".to_string())
            } else {
                write_atomic(path, content).map_err(|error| error.to_string())
            }
        };
        let error = execute_plan_with_writer(&plan, &mut writer).unwrap_err();

        assert!(error.contains("rollback encountered error"));
        assert!(error.contains("changed before rollback restore"));
        assert_eq!(
            fs::read_to_string(first).unwrap(),
            "concurrent post-write edit\n"
        );
        assert!(old.exists());
        assert!(!new.exists());
    }

    #[test]
    fn same_byte_symlink_substitution_is_refused_without_following_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let old = root.join("old.org");
        let new = root.join("new.org");
        let source = root.join("source.org");
        let outside = root.join("outside.org");
        let source_original = "[[file:old.org][old]]\n";
        fs::write(&old, "* H\n").unwrap();
        fs::write(&source, source_original).unwrap();
        fs::write(&outside, source_original).unwrap();
        let plan = prepare_plan(&MoveOptions {
            old: old.clone(),
            new: new.clone(),
            root: root.to_path_buf(),
            write: true,
        })
        .unwrap();
        fs::remove_file(&source).unwrap();
        symlink(&outside, &source).unwrap();

        let mut writer = |path: &Path, content: &str| {
            write_atomic(path, content).map_err(|error| error.to_string())
        };
        let error = execute_plan_with_writer(&plan, &mut writer).unwrap_err();

        assert!(error.contains("not a regular file entry"));
        assert!(error.contains("operation rolled back"));
        assert_eq!(fs::read_to_string(&outside).unwrap(), source_original);
        assert!(
            fs::symlink_metadata(&source)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(old.exists());
        assert!(!new.exists());
    }

    #[test]
    fn root_lock_is_exclusive_and_nonblocking() {
        let temp = tempfile::tempdir().unwrap();
        let first = RootLock::acquire(temp.path()).unwrap();
        let error = RootLock::acquire(temp.path()).unwrap_err();
        assert!(error.contains("another org-cli link move"));
        drop(first);
        RootLock::acquire(temp.path()).unwrap();
    }

    #[test]
    fn write_lock_is_acquired_before_invalid_corpus_is_planned() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let old = root.join("old.org");
        let new = root.join("new.org");
        fs::write(&old, "* Old\n").unwrap();
        fs::write(root.join("invalid.org"), [0xff, 0xfe]).unwrap();
        let _held = RootLock::acquire(&root).unwrap();

        let error = run(MoveOptions {
            old: old.clone(),
            new: new.clone(),
            root,
            write: true,
        })
        .unwrap_err();

        assert!(error.contains("another org-cli link move"));
        assert!(old.exists());
        assert!(!new.exists());
    }

    #[test]
    fn relative_path_rebases_moved_source_against_unchanged_target() {
        assert_eq!(
            relative_path(Path::new("/root/deep/new"), Path::new("/root/out.org")).unwrap(),
            PathBuf::from("../../out.org")
        );
    }

    #[test]
    fn percent_encoded_style_is_preserved_when_target_changes() {
        assert_eq!(
            render_target(Path::new("new file.org"), "old%20file.org", false).unwrap(),
            "new%20file.org"
        );
        assert_eq!(
            render_target(Path::new("new.name"), "old%2Ename", false).unwrap(),
            "new%2Ename"
        );
    }
}
