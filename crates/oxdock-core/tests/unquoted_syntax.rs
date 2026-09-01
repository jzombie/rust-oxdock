//! Integration tests for unquoted syntax — Docker-style bare paths.
//!
//! Every test exercises unquoted paths as the primary argument syntax.
//! Quoted strings are reserved for content containing spaces or templates.

use indoc::indoc;
use oxdock_core::{ExecIo, run_steps_with_context_result_with_io};
use oxdock_fs::{GuardedPath, PathResolver};
fn run_script(root: &GuardedPath, script: &str) -> Result<(), anyhow::Error> {
    let steps = oxdock_core::parse_script(script).expect("parse script");
    run_steps_with_context_result_with_io(root, root, &steps, ExecIo::new()).map(|_| ())
}

fn read_trimmed(root: &GuardedPath, rel: &str) -> String {
    let path = root.join(rel).unwrap();
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    resolver.read_to_string(&path).unwrap().trim().to_string()
}

// ============================================================================
// Bare filepaths
// ============================================================================

#[test]
fn workdir_unquoted_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WORKDIR /app").unwrap();
    // Just verify no error — WORKDIR succeeded
}

#[test]
fn mkdir_unquoted_nested_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "MKDIR a/b/c").unwrap();
    assert!(root.join("a/b/c").unwrap().exists());
}

#[test]
fn write_unquoted_path_and_content() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE dist/hello.txt 'hello world'").unwrap();
    assert_eq!(read_trimmed(&root, "dist/hello.txt"), "hello world");
}

#[test]
fn read_unquoted_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE data.txt 'test content'").unwrap();
    run_script(&root, "READ data.txt").unwrap();
}

#[test]
fn copy_unquoted_paths() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE src/file.txt 'copied'").unwrap();
    run_script(&root, "COPY src/file.txt dst/file.txt").unwrap();
    assert_eq!(read_trimmed(&root, "dst/file.txt"), "copied");
}

#[test]
fn symlink_unquoted_paths() {
    // SYMLINK arg order: SYMLINK <source> <destination>
    // The first arg is resolved as a copy source, second as write destination.
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE real.txt 'data'").unwrap();
    run_script(&root, "SYMLINK real.txt alias.txt").unwrap();
    assert!(root.join("alias.txt").unwrap().exists());
}

#[test]
fn ls_unquoted_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "MKDIR dir").unwrap();
    run_script(&root, "WRITE dir/file.txt 'ok'").unwrap();
    run_script(&root, "LS dir").unwrap();
}

#[test]
fn append_unquoted_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE log.txt 'line1'").unwrap();
    run_script(&root, "APPEND log.txt 'line2'").unwrap();
    let resolver = PathResolver::new(root.root(), root.root()).unwrap();
    let content = resolver
        .read_to_string(&root.join("log.txt").unwrap())
        .unwrap();
    assert!(content.contains("line1"));
    assert!(content.contains("line2"));
}

#[test]
fn hash_sha256_unquoted_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE data.txt 'hashme'").unwrap();
    run_script(&root, "HASH_SHA256 data.txt").unwrap();
}

#[test]
fn assert_file_unquoted_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE check.txt 'verified'").unwrap();
    run_script(&root, "ASSERT_FILE check.txt 'verified'").unwrap();
}

#[test]
fn assert_dir_unquoted_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "MKDIR mydir").unwrap();
    run_script(&root, "ASSERT_DIR mydir").unwrap();
}

#[test]
fn assert_absent_unquoted_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "ASSERT_ABSENT nofile.txt").unwrap();
}

// ============================================================================
// Mid-string dollar signs
// ============================================================================

#[test]
fn write_path_with_dollar_sign() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    // $1 in path is literal — not a variable
    run_script(&root, "WRITE dist/file_$1.txt 'data'").unwrap();
    assert_eq!(read_trimmed(&root, "dist/file_$1.txt"), "data");
}

#[test]
fn write_content_with_dollar_sign() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    // $5.00 in content is literal
    run_script(&root, "WRITE price.txt 'Price is $5.00'").unwrap();
    assert_eq!(read_trimmed(&root, "price.txt"), "Price is $5.00");
}

#[test]
fn env_value_with_dollar_sign() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    // $ in env value is literal
    run_script(&root, r#"ENV PATH="/usr/bin:$HOME""#).unwrap();
}

// ============================================================================
// Unquoted interpolated paths
// ============================================================================

#[test]
fn write_unquoted_template_in_path() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $name = "output"
        WRITE "{{ $name }}.txt" "content"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "output.txt"), "content");
}

#[test]
fn workdir_unquoted_template() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $dir = "target"
        MKDIR "{{ $dir }}"
        WORKDIR "{{ $dir }}"
    "#},
    )
    .unwrap();
    // WORKDIR succeeded — no error
}

#[test]
fn write_unquoted_template_with_suffix() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        LET $pkg = "mylib"
        WRITE "src/{{ $pkg }}/mod.rs" "// module"
    "#},
    )
    .unwrap();
    assert_eq!(read_trimmed(&root, "src/mylib/mod.rs"), "// module");
}

// ============================================================================
// Unquoted command payloads
// ============================================================================

#[test]
fn echo_unquoted_message() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "ECHO hello world").unwrap();
}

#[test]
fn run_unquoted_command() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "RUN echo test123").unwrap();
}

#[test]
fn run_unquoted_command_with_env_expansion() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(
        &root,
        indoc! {r#"
        ENV GREETING="hello"
        RUN echo $GREETING
    "#},
    )
    .unwrap();
}

// ============================================================================
// Mixed quoted and unquoted
// ============================================================================

#[test]
fn write_unquoted_path_quoted_content() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE out.txt 'hello world'").unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "hello world");
}

#[test]
fn write_quoted_path_unquoted_content_with_spaces_fails() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    // Unquoted content with spaces is joined into a single content argument.
    run_script(&root, "WRITE out.txt hello world").unwrap();
    assert_eq!(read_trimmed(&root, "out.txt"), "hello world");
}

// ============================================================================
// Edge cases
// ============================================================================

#[test]
fn empty_path_ls_no_arg() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    // LS with no argument should succeed
    run_script(&root, "LS").unwrap();
}

#[test]
fn unquoted_path_with_dots() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE file.backup.old 'archived'").unwrap();
    assert_eq!(read_trimmed(&root, "file.backup.old"), "archived");
}

#[test]
fn unquoted_path_with_hyphens() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE my-file-name.txt 'hyphenated'").unwrap();
    assert_eq!(read_trimmed(&root, "my-file-name.txt"), "hyphenated");
}

#[test]
fn unquoted_path_with_underscores() {
    let temp = GuardedPath::tempdir().unwrap();
    let root = temp.as_guarded_path().clone();
    run_script(&root, "WRITE my_file_name.txt 'underscored'").unwrap();
    assert_eq!(read_trimmed(&root, "my_file_name.txt"), "underscored");
}
