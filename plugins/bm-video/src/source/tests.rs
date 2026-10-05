use super::*;

/// A scratch tree, emptied first so a rerun starts clean.
fn tree(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bm-video-source-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join("src")).expect("temp tree");
    root
}

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().expect("a parent")).expect("temp dir");
    std::fs::write(p, body).expect("write");
}

#[test]
fn the_local_tree_always_hashes_the_same() {
    let d = digest().expect("digest");
    assert_eq!(d.len(), 64);
    assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(d, digest().expect("digest"));
    assert_eq!(short(&d).len(), 12);
}

#[test]
fn a_doc_edit_does_not_look_like_a_different_renderer() {
    let root = tree("docs");
    put(&root, "src/main.rs", "fn main() {}");
    put(&root, "Cargo.toml", "[package]");
    put(&root, "Cargo.lock", "# lock");
    let before = digest_tree(&root).expect("digest");
    put(&root, "README.md", "a longer README");
    put(&root, "Makefile", "test:\n\tcargo test\n");
    assert_eq!(digest_tree(&root).expect("digest"), before);
}

#[test]
fn the_code_and_its_pins_are_what_the_digest_is_about() {
    let root = tree("code");
    put(&root, "src/render.rs", "fn render() {}");
    put(&root, "Cargo.toml", "[package]");
    put(&root, "Cargo.lock", "# lock");
    let before = digest_tree(&root).expect("digest");

    put(&root, "src/render.rs", "fn render() { /* one more frame */ }");
    let edited = digest_tree(&root).expect("digest");
    assert_ne!(edited, before);

    put(&root, "Cargo.lock", "# lock with a different pin");
    let repinned = digest_tree(&root).expect("digest");
    assert_ne!(repinned, edited);

    // A file the crate gained is part of it.
    put(&root, "src/source.rs", "// new");
    assert_ne!(digest_tree(&root).expect("digest"), repinned);
}

#[test]
fn the_same_tree_under_another_root_hashes_the_same() {
    // A box holds the source at a path of its own. The digest is over relative
    // paths, so where the tree sits must not enter into it.
    let copied = tree("copied");
    for rel in files().expect("files") {
        if is_code(&rel) {
            let body = std::fs::read_to_string(super::root().join(&rel)).expect("read");
            put(&copied, &rel, &body);
        }
    }
    assert_eq!(digest_tree(&copied).expect("digest"), digest().expect("digest"));
}

#[test]
fn build_output_is_not_source() {
    let root = tree("target");
    put(&root, "src/main.rs", "fn main() {}");
    put(&root, "Cargo.toml", "[package]");
    let before = digest_tree(&root).expect("digest");
    put(&root, "target/release/bm-video", "an old binary");
    put(&root, ".git/HEAD", "ref: refs/heads/main");
    assert_eq!(digest_tree(&root).expect("digest"), before);
}

#[test]
fn the_file_list_is_sorted_and_skips_build_output() {
    let root = tree("list");
    put(&root, "src/b.rs", "b");
    put(&root, "src/a.rs", "a");
    put(&root, "Cargo.toml", "[package]");
    put(&root, "target/debug/x", "junk");
    assert_eq!(
        list(&root).expect("list"),
        vec!["Cargo.toml", "src/a.rs", "src/b.rs"]
    );
}
