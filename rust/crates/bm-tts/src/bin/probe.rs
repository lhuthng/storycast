//! Load the VieNeu-TTS ONNX graphs and run one forward pass.
//!
//! This is the gate for the whole "no Python" plan: if ONNX Runtime cannot
//! host these graphs from Rust, nothing downstream matters. It is a permanent
//! diagnostic, not scaffolding — "does the model dir actually load and run on
//! this box" is the first question every worker brings.
//!
//!     bm-tts-probe <dir>... [--dump <path>]
//!
//! Each directory is walked for `*.onnx`; every graph is loaded and its
//! signature printed. `vieneu_prefill.onnx` is then run on a deterministic
//! input, and with `--dump` the output is written as raw f32 + a sidecar
//! `.shape` file so the Python engine can be asked for the same tensor and the
//! two compared. Same runtime, different binding: any difference is the
//! binding's fault, which is exactly what this is here to rule out.

use anyhow::{bail, Context, Result};
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};

/// The backbone's hidden size, from the model's `config.json`. Hard-coded
/// because the probe's job is to be independent of the loader it is testing.
const HIDDEN: usize = 768;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dump: Option<PathBuf> = None;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dump" => {
                i += 1;
                dump = Some(PathBuf::from(args.get(i).context("--dump needs a path")?));
            }
            other => dirs.push(PathBuf::from(other)),
        }
        i += 1;
    }
    if dirs.is_empty() {
        bail!("usage: bm-tts-probe <dir>... [--dump <path>]");
    }

    let mut graphs = Vec::new();
    for d in &dirs {
        collect_onnx(d, &mut graphs)?;
    }
    graphs.sort();
    if graphs.is_empty() {
        bail!("no .onnx files under {dirs:?}");
    }

    println!("{} graph(s):", graphs.len());
    let mut prefill = None;
    for g in &graphs {
        match Session::builder().and_then(|mut b| b.commit_from_file(g)) {
            Ok(s) => {
                let ins: Vec<String> = s
                    .inputs()
                    .iter()
                    .map(|o| format!("{}:{}", o.name(), dtype_of(o)))
                    .collect();
                let outs = s.outputs().len();
                println!(
                    "  OK   {:<44} {} in / {} out",
                    g.file_name().unwrap_or_default().to_string_lossy(),
                    ins.len(),
                    outs
                );
                for line in ins.iter().take(4) {
                    println!("         in  {line}");
                }
                if ins.len() > 4 {
                    println!("         in  … {} more", ins.len() - 4);
                }
                if g.file_name().and_then(|n| n.to_str()) == Some("vieneu_prefill.onnx") {
                    prefill = Some((g.clone(), s));
                }
            }
            Err(e) => println!(
                "  FAIL {:<44} {e}",
                g.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }

    let Some((path, mut session)) = prefill else {
        bail!("vieneu_prefill.onnx was not among the graphs — cannot run a forward pass");
    };

    // A short, fully deterministic prompt: 4 rows of a fixed pattern. The
    // values are irrelevant to the question (does the graph run and agree with
    // the other binding); what matters is that both sides are handed the exact
    // same bytes.
    let t = 4usize;
    let embeds: Vec<f32> = (0..t * HIDDEN)
        .map(|i| ((i % 97) as f32 - 48.0) / 97.0)
        .collect();
    let input = Tensor::from_array((vec![1i64, t as i64, HIDDEN as i64], embeds.clone()))
        .context("building inputs_embeds")?;

    println!("\nprefill {} on (1, {t}, {HIDDEN})…", path.display());
    let started = std::time::Instant::now();
    let outputs = session
        .run(ort::inputs!["inputs_embeds" => input])
        .context("running prefill")?;
    let elapsed = started.elapsed();

    println!("  {} output(s) in {elapsed:?}", outputs.len());
    // The first output is the hidden state — the one worth comparing. Copied
    // out here because the borrow lives only as long as `outputs`.
    let mut first: Option<(Vec<i64>, Vec<f32>)> = None;
    for (i, (name, out)) in outputs.iter().enumerate() {
        let (shape, data) = out
            .try_extract_tensor::<f32>()
            .with_context(|| format!("output {i} ({name}) is not f32"))?;
        let dims: Vec<i64> = shape.as_ref().to_vec();
        println!("  out[{i}] {name:<16} {dims:?}");
        if i == 0 {
            first = Some((dims, data.to_vec()));
        }
    }

    let (dims, data) = first.context("prefill returned no outputs")?;
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for v in &data {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let sum: f64 = data.iter().map(|v| *v as f64).sum();
    println!(
        "  out[0] {} values, sum {sum:.6}, first {:?}",
        data.len(),
        &data[..data.len().min(3)]
    );

    match dump {
        Some(p) => {
            std::fs::write(&p, &bytes).with_context(|| format!("writing {}", p.display()))?;
            let sidecar = p.with_extension("shape");
            std::fs::write(&sidecar, serde_json::to_string(&dims)?)?;
            println!("\ndumped {} bytes to {}", bytes.len(), p.display());
            println!("shape {dims:?} -> {}", sidecar.display());
        }
        None => println!("\n(pass --dump <path> to write the tensor for a Python comparison)"),
    }
    Ok(())
}

/// The dtype of an outlet, as a short string. `ValueType`'s `Debug` is the only
/// stable view of it that does not pin this probe to an ort minor version.
fn dtype_of(outlet: &ort::value::Outlet) -> String {
    format!("{:?}", outlet.dtype())
}

fn collect_onnx(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for e in entries.filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            collect_onnx(&p, out)?;
        } else if p.extension().and_then(|x| x.to_str()) == Some("onnx") {
            out.push(p);
        }
    }
    Ok(())
}
