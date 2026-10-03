//! Load an engine's ONNX graphs and run one forward pass.
//! diagnostic, not scaffolding — "does the model dir actually load and run on
//! this box" is the first question every worker brings.

use anyhow::{bail, Context, Result};
use ort::session::Session;
use ort::value::{DynValue, Outlet, Tensor, TensorElementType, ValueType};
use std::path::{Path, PathBuf};

/// How many rows a *dynamic* dimension gets. A short prompt is enough to prove
const DEFAULT_SEQ: usize = 4;

struct Opts {
    dirs: Vec<PathBuf>,
    dump: Option<PathBuf>,
    /// An exact file stem, to pick a graph when the choice is not obvious.
    graph: Option<String>,
    seq: usize,
    /// Every input name, not the first four.
    all: bool,
    run: bool,
}

fn main() -> Result<()> {
    let opts = parse()?;
    if opts.dirs.is_empty() {
        bail!(
            "usage: bm-tts-probe <dir>... [--dump <path>] [--graph <stem>] [--seq <n>] [--all] [--no-run]"
        );
    }

    let mut graphs = Vec::new();
    for d in &opts.dirs {
        collect_onnx(d, &mut graphs)?;
    }
    graphs.sort();
    if graphs.is_empty() {
        bail!("no .onnx files under {:?}", opts.dirs);
    }

    println!("{} graph(s):", graphs.len());
    let mut loaded: Vec<(PathBuf, Session)> = Vec::new();
    for g in &graphs {
        match Session::builder().and_then(|mut b| b.commit_from_file(g)) {
            Ok(s) => {
                let ins: Vec<String> = s.inputs().iter().map(signature).collect();
                let outs = s.outputs().len();
                println!(
                    "  OK   {:<44} {} in / {} out",
                    file_name(g),
                    ins.len(),
                    outs
                );
                for line in ins.iter().take(if opts.all { usize::MAX } else { 4 }) {
                    println!("         in  {line}");
                }
                if !opts.all && ins.len() > 4 {
                    println!("         in  … {} more (--all)", ins.len() - 4);
                }
                loaded.push((g.clone(), s));
            }
            Err(e) => println!("  FAIL {:<44} {e}", file_name(g)),
        }
    }

    if !opts.run {
        println!("\n(--no-run: signatures only)");
        return Ok(());
    }

    // An index, not a reference: the session is taken out of `loaded` so it can
    let idx = match pick(&loaded, opts.graph.as_deref()) {
        Some(i) => i,
        None if opts.graph.is_some() => {
            let names: Vec<&str> = loaded.iter().map(|(p, _)| file_name(p)).collect();
            bail!(
                "--graph {} is not among the graphs; {} loaded: {}",
                opts.graph.as_deref().unwrap_or_default(),
                names.len(),
                names.join(", ")
            )
        }
        None => bail!("no graph loaded, so there is nothing to run"),
    };
    let (path, mut session) = loaded.swap_remove(idx);

    // The feed is built from the signature, so the values are not the point:
    let vals: Vec<(Vec<i64>, DynValue)> = session
        .inputs()
        .iter()
        .map(|o| synth(o, opts.seq))
        .collect::<Result<_>>()?;

    println!("\n{} on a synthetic feed:", file_name(&path));
    for (o, (dims, _)) in session.inputs().iter().zip(&vals) {
        println!("  in  {:<24} {dims:?}", o.name());
    }
    let vals: Vec<DynValue> = vals.into_iter().map(|(_, v)| v).collect();

    let mut feed: Vec<(String, &DynValue)> = Vec::with_capacity(vals.len());
    for (o, v) in session.inputs().iter().zip(&vals) {
        feed.push((o.name().to_string(), v));
    }
    let started = std::time::Instant::now();
    let outputs = session
        .run(feed)
        .with_context(|| format!("running {}", file_name(&path)))?;
    let elapsed = started.elapsed();

    println!("  {} output(s) in {elapsed:?}", outputs.len());
    // The first *float* output is the one worth dumping. For every engine this
    let mut picked: Option<(usize, String, Vec<i64>, Vec<f32>)> = None;
    for (i, (name, out)) in outputs.iter().enumerate() {
        match out.try_extract_tensor::<f32>() {
            Ok((shape, data)) => {
                let dims: Vec<i64> = shape.as_ref().to_vec();
                println!("  out[{i}] {name:<16} {dims:?}");
                if picked.is_none() {
                    picked = Some((i, name.to_string(), dims, data.to_vec()));
                }
            }
            // Not a float — an int code, a bool, a sequence. Printed and passed
            Err(_) => println!("  out[{i}] {name:<16} (not f32)"),
        }
    }

    let Some((idx, name, dims, data)) = picked else {
        bail!(
            "{} returned no float output, so there is nothing to dump",
            file_name(&path)
        );
    };

    let mut bytes = Vec::with_capacity(data.len() * 4);
    for v in &data {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let sum: f64 = data.iter().map(|v| *v as f64).sum();
    println!(
        "  out[{idx}] {name} — {} values, sum {sum:.6}, first {:?}",
        data.len(),
        &data[..data.len().min(3)]
    );

    match opts.dump {
        Some(p) => {
            std::fs::write(&p, &bytes).with_context(|| format!("writing {}", p.display()))?;
            let sidecar = p.with_extension("shape");
            std::fs::write(&sidecar, serde_json::to_string(&dims)?)?;
            println!("\ndumped {} bytes to {}", bytes.len(), p.display());
            println!("shape {dims:?} -> {}", sidecar.display());
        }
        None => println!("\n(pass --dump <path> to write the tensor for a reference comparison)"),
    }
    Ok(())
}

fn parse() -> Result<Opts> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut o = Opts {
        dirs: Vec::new(),
        dump: None,
        graph: None,
        seq: DEFAULT_SEQ,
        all: false,
        run: true,
    };
    let mut i = 0;
    while i < args.len() {
        let value = |i: &mut usize| -> Result<String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .with_context(|| format!("{} needs a value", args[*i - 1]))
        };
        match args[i].as_str() {
            "--dump" => o.dump = Some(PathBuf::from(value(&mut i)?)),
            "--graph" => o.graph = Some(value(&mut i)?),
            "--seq" => {
                let v = value(&mut i)?;
                o.seq = v
                    .parse()
                    .with_context(|| format!("--seq {v} is not a number"))?;
            }
            "--all" => o.all = true,
            "--no-run" => o.run = false,
            other => o.dirs.push(PathBuf::from(other)),
        }
        i += 1;
    }
    Ok(o)
}

/// The graph that gets the forward pass.
fn pick(graphs: &[(PathBuf, Session)], want: Option<&str>) -> Option<usize> {
    if let Some(w) = want {
        return graphs
            .iter()
            .position(|(p, _)| p.file_stem().and_then(|s| s.to_str()) == Some(w));
    }
    graphs
        .iter()
        .enumerate()
        .filter_map(|(i, (p, s))| {
            rank(&stem(p), s.inputs().iter().any(is_seq_float)).map(|r| (r, i))
        })
        .min_by_key(|(r, _)| *r)
        .map(|(_, i)| i)
        .or(if graphs.is_empty() { None } else { Some(0) })
}

/// How good a graph is as the forward pass, lower first. Split out from
fn rank(stem: &str, has_seq_float: bool) -> Option<u8> {
    if stem.contains("prefill") {
        Some(0)
    } else if stem.contains("main") {
        Some(1)
    } else if has_seq_float {
        Some(2)
    } else {
        None
    }
}

/// A rank-3 float input: `[1, T, H]`, which is what both backbones take.
fn is_seq_float(o: &Outlet) -> bool {
    matches!(
        o.dtype(),
        ValueType::Tensor {
            ty: TensorElementType::Float32,
            shape,
            ..
        } if shape.as_ref().len() == 3
    )
}

/// `name:element-type shape`, e.g. `audio_codes:Int32 [1, -1, 16]`. A `-1` is
fn signature(o: &Outlet) -> String {
    match o.dtype() {
        ValueType::Tensor { ty, shape, .. } => {
            let dims: Vec<i64> = shape.as_ref().to_vec();
            format!("{}:{ty:?} {dims:?}", o.name())
        }
        // Not a tensor — a sequence, a map. All there is to say is the type.
        other => format!("{}:{other:?}", o.name()),
    }
}

/// The dtype of an outlet, as a short string. `Debug` is used deliberately: it
fn dtype_of(outlet: &Outlet) -> String {
    match outlet.dtype() {
        ValueType::Tensor { ty, .. } => format!("{ty:?}"),
        other => format!("{other:?}"),
    }
}

/// The fixed ramp this probe has always used: a 97-period sawtooth in
fn ramp(n: usize) -> Vec<f32> {
    (0..n).map(|i| ((i % 97) as f32 - 48.0) / 97.0).collect()
}

/// A dynamic dimension becomes `seq`. A declared `0` stays `0`: a cache input
fn shape_for(dims: &[i64], seq: usize) -> Vec<i64> {
    dims.iter()
        .map(|d| if *d < 0 { seq as i64 } else { *d })
        .collect()
}

/// One input, built from its own signature: the dims it will be fed at, and the
fn synth(o: &Outlet, seq: usize) -> Result<(Vec<i64>, DynValue)> {
    let name = o.name();
    let ValueType::Tensor { ty, shape, .. } = o.dtype() else {
        bail!(
            "{name} is {} and not a tensor; --no-run and read the signature instead",
            dtype_of(o)
        );
    };
    let dims = shape_for(shape.as_ref(), seq);
    let n: usize = dims.iter().map(|d| *d as usize).product();
    let value = match ty {
        TensorElementType::Float32 => Tensor::from_array((dims.clone(), ramp(n)))?.into_dyn(),
        // A small ramp, not zeros: a zero token id is usually a real id (a pad,
        TensorElementType::Int64 => Tensor::from_array((
            dims.clone(),
            (0..n).map(|i| (i % 8) as i64).collect::<Vec<i64>>(),
        ))?
        .into_dyn(),
        TensorElementType::Int32 => Tensor::from_array((
            dims.clone(),
            (0..n).map(|i| (i % 8) as i32).collect::<Vec<i32>>(),
        ))?
        .into_dyn(),
        TensorElementType::Bool => Tensor::from_array((dims.clone(), vec![true; n]))?.into_dyn(),
        other => bail!(
            "{name} is {other:?}, which this probe will not synthesise; \
             --no-run and read the signature instead"
        ),
    };
    Ok((dims, value))
}

fn stem(p: &Path) -> String {
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn file_name(p: &Path) -> &str {
    p.file_name().and_then(|n| n.to_str()).unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_prefill_wins_over_a_generic_sequence_graph() {
        assert_eq!(rank("vieneu_prefill", false), Some(0));
        assert_eq!(rank("flow_lm_main_int8", true), Some(1));
        // `main` beats the shape heuristic, because a stem that names the
        assert_eq!(rank("flow_lm_main_int8", false), Some(1));
        assert_eq!(rank("mimi_decoder", true), Some(2));
        assert_eq!(rank("mimi_decoder", false), None);
    }

    #[test]
    fn a_dynamic_dimension_is_filled_and_a_declared_zero_is_not() {
        // `[1, -1, 768]` is the prefill's `inputs_embeds`; `[1, 8, 0, 64]` is an
        assert_eq!(shape_for(&[1, -1, 768], 4), vec![1, 4, 768]);
        assert_eq!(shape_for(&[1, 8, 0, 64], 4), vec![1, 8, 0, 64]);
        assert_eq!(shape_for(&[-1, -1], 0), vec![0, 0]);
    }

    #[test]
    fn the_ramp_is_bounded_deterministic_and_not_all_zero() {
        let r = ramp(97 * 3);
        assert_eq!(r, ramp(97 * 3));
        assert!(r.iter().all(|v| (-0.5..0.5).contains(v)));
        assert!(r.iter().any(|v| *v != 0.0), "a flat feed proves nothing");
        // The old constant's property, kept: the value at index i depends only on
        assert_eq!(r[0], r[97]);
    }

    #[test]
    fn the_stem_is_matched_case_insensitively() {
        // `rank` is fed a lowercased stem, so a bundle that ships
        assert_eq!(rank(&stem(Path::new("FlowLM_Main.onnx")), false), Some(1));
        assert_eq!(stem(Path::new("vieneu_prefill.onnx")), "vieneu_prefill");
        assert_eq!(stem(Path::new("no_extension")), "no_extension");
    }
}
