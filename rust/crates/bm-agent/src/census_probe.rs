use super::*;

/// Kill the child however this test leaves, including on a panic. A model
/// left behind is 2.85 GB held by a box nobody is driving.
struct Kill(std::process::Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The repo root, derived rather than hardcoded: `rust/crates/bm-agent`.
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("rust/crates/bm-agent is three levels below the root")
        .to_path_buf()
}

#[test]
#[ignore = "starts the real bm-tts and loads the model — run deliberately, see the doc comment"]
fn the_census_finds_a_real_bm_tts_and_reads_its_rss() {
    let layout = Layout::new(repo_root());
    let (bin, args) = layout.sidecar_command(SIDECAR_PORT, 0);
    // Fail loudly rather than skipping: a check that quietly does nothing
    // when it cannot run is worse than no check.
    assert!(
        bin.is_file(),
        "no sidecar at {} — build it first (`make build`)",
        bin.display()
    );

    let child = std::process::Command::new(&bin)
        .args(&args)
        .env("LD_LIBRARY_PATH", layout.tts_lib_dir())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawning bm-tts");
    let _kill = Kill(child);

    // Sample while it loads: the process exists immediately and its RSS
    // climbs, so the first non-zero reading is the answer and there is no
    // need to wait for `/health`.
    let mut sys = sysinfo::System::new();
    let mut seen = (0u32, 0u64);
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_secs(3));
        seen = sidecar_processes(&mut sys);
        println!(
            "census: count={} rss={:.0} MiB",
            seen.0,
            seen.1 as f64 / 1_048_576.0
        );
        if seen.0 > 0 && seen.1 > 0 {
            break;
        }
    }
    assert_eq!(
        seen.0, 1,
        "the census must find exactly one bm-tts — a name match that never matches \
         would make the whole guard inert"
    );
    assert!(
        seen.1 > 0,
        "the census must report a non-zero RSS — a zero would leave the guard's \
         primary trigger dead on this platform"
    );
}
