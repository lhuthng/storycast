"""Apply the PyO3-optional patch to the vendored sea-g2p copy.

Re-runnable and anchor-checked: every replacement asserts its anchor exists
exactly once, so a future upstream merge that moves one of these lines fails
loudly here instead of silently producing a build that links libpython.

    python3 tools/vendor-sea-g2p-patch.py <path-to-vendored-crate>
"""
import pathlib
import sys

root = pathlib.Path(
    sys.argv[1] if len(sys.argv) > 1 else "rust/vendor/sea-g2p"
).resolve()
if not (root / "Cargo.toml").is_file():
    sys.exit(f"not a crate: {root}")

applied = []


def sub(path: str, old: str, new: str) -> None:
    p = root / path
    t = p.read_text()
    if old not in t:
        sys.exit(f"ANCHOR MISSING in {path}:\n{old!r}")
    if t.count(old) != 1:
        sys.exit(f"ANCHOR AMBIGUOUS ({t.count(old)}x) in {path}:\n{old!r}")
    p.write_text(t.replace(old, new))
    applied.append(path)


# ── Cargo.toml: PyO3 becomes opt-in ────────────────────────────────────────
# `default = []`, not `["python"]`: the whole point of the port is that a Rust
# consumer gets no PyO3 at all, and `cargo build --workspace` must not drag it
# in. Building the wheel needs `--features python` (maturin is told so in
# pyproject.toml upstream).
sub(
    "Cargo.toml",
    """[dependencies]
pyo3 = { version = "0.23", features = ["extension-module", "abi3-py310", "generate-import-lib"] }""",
    """[features]
default = []
python = ["dep:pyo3"]

[dependencies]
# Optional on purpose: the core is a plain Rust library. Only the wheel needs
# the bindings, and only the wheel turns them on.
pyo3 = { version = "0.23", features = ["extension-module", "abi3-py310", "generate-import-lib"], optional = true }""",
)
# No build.rs exists in the tree, so this build-dependency was already inert.
sub(
    "Cargo.toml",
    """
[build-dependencies]
pyo3-build-config = "0.23"
""",
    "",
)
# Declares the crate its own workspace root. Without it cargo refuses to use a
# package that sits inside our workspace tree but is not a member.
sub(
    "Cargo.toml",
    "\n[lib]\nname = \"sea_g2p_rs\"",
    "\n[workspace]\n\n[lib]\nname = \"sea_g2p_rs\"",
)

# ── src/lib.rs: gate the module tail ───────────────────────────────────────
sub(
    "src/lib.rs",
    "use pyo3::prelude::*;\nuse pyo3::wrap_pyfunction;",
    '#[cfg(feature = "python")]\nuse pyo3::prelude::*;\n#[cfg(feature = "python")]\nuse pyo3::wrap_pyfunction;',
)
sub(
    "src/lib.rs",
    "#[pyfunction]\nfn punc_norm",
    '#[cfg(feature = "python")]\n#[pyfunction]\nfn punc_norm',
)
sub(
    "src/lib.rs",
    "#[pyclass]\nstruct G2P",
    '#[cfg(feature = "python")]\n#[pyclass]\nstruct G2P',
)
sub(
    "src/lib.rs",
    "#[pymethods]\nimpl G2P",
    '#[cfg(feature = "python")]\n#[pymethods]\nimpl G2P',
)
sub(
    "src/lib.rs",
    "#[pymodule]\nfn sea_g2p_rs",
    '#[cfg(feature = "python")]\n#[pymodule]\nfn sea_g2p_rs',
)

# ── src/lang/vi/mod.rs: `cfg_attr`, so the bodies stay ordinary Rust ───────
sub(
    "src/lang/vi/mod.rs",
    "use pyo3::prelude::*;",
    '#[cfg(feature = "python")]\nuse pyo3::prelude::*;',
)
sub(
    "src/lang/vi/mod.rs",
    "#[pyclass]\npub struct Normalizer",
    '#[cfg_attr(feature = "python", pyclass)]\npub struct Normalizer',
)
sub(
    "src/lang/vi/mod.rs",
    "    #[pyo3(get)]\n    pub lang: String,",
    '    #[cfg_attr(feature = "python", pyo3(get))]\n    pub lang: String,',
)
sub(
    "src/lang/vi/mod.rs",
    "#[pymethods]\nimpl Normalizer",
    '#[cfg_attr(feature = "python", pymethods)]\nimpl Normalizer',
)
sub(
    "src/lang/vi/mod.rs",
    '    #[new]\n    #[pyo3(signature = (lang="vi", dict_path=None))]',
    '    #[cfg_attr(feature = "python", new)]\n'
    '    #[cfg_attr(feature = "python", pyo3(signature = (lang="vi", dict_path=None)))]',
)
sub(
    "src/lang/vi/mod.rs",
    "    #[pyo3(signature = (text, punc_norm=false))]\n    pub fn normalize(",
    '    #[cfg_attr(feature = "python", pyo3(signature = (text, punc_norm=false)))]\n'
    "    pub fn normalize(",
)
# The one method whose *signature* needs PyO3: it takes a GIL handle and
# returns PyResult. Gated whole — nothing here needs the batch form, and a Rust
# caller has rayon directly.
sub(
    "src/lang/vi/mod.rs",
    '    #[pyo3(signature = (texts, punc_norm=false))]\n    pub fn normalize_batch(',
    '    #[cfg(feature = "python")]\n'
    '    #[pyo3(signature = (texts, punc_norm=false))]\n'
    "    pub fn normalize_batch(",
)

print(f"patched {len(applied)} sites in {root}:")
for a in applied:
    print("  " + a)
