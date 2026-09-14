"""Fetch chapter text from a story URL or a local file, clean it, save to data/chapters/."""
from __future__ import annotations

import re
import urllib.request
from pathlib import Path

CHAPTERS_DIR = Path("data/chapters")

# Markers seen on storya.click pages (also work as generic fallbacks).
_START_HINTS = ("Chương 1", "Chương ", "Thượng bộ", "Một bộ")
_STOP_MARKERS = (
    "Chương trước", "Chương sau", "Truyện cùng thể loại",
    "Nền tảng đọc truyện", "Chính sách bảo mật",
)


def fetch_html(url: str, timeout: int = 30) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read().decode("utf-8", errors="replace")


def clean_storya_html(html: str) -> str:
    """Extract readable chapter text. bs4 if available, else regex fallback. No new deps required."""
    try:
        from bs4 import BeautifulSoup  # type: ignore
    except ImportError:
        text = re.sub(r"<script.*?</script>|<style.*?</style>", " ", html, flags=re.S | re.I)
        text = re.sub(r"<[^>]+>", "\n", text)
    else:
        soup = BeautifulSoup(html, "html.parser")
        for tag in soup(["script", "style", "nav", "header", "footer", "aside"]):
            tag.decompose()
        main = soup.find("article") or soup.find("main") or soup.body or soup
        text = main.get_text("\n")
    lines = [ln.strip() for ln in text.splitlines()]
    lines = [ln for ln in lines if ln]
    # Slice between chapter body start and nav/footer junk.
    start = 0
    for i, ln in enumerate(lines):
        if any(h in ln for h in _START_HINTS) and len(ln) < 120:
            start = max(0, i - 1)
            break
        if "Thái Cực Quyền" in ln or "xuyên không" in ln.lower():
            start = i
            break
    end = len(lines)
    for i in range(start, len(lines)):
        if any(m in lines[i] for m in _STOP_MARKERS):
            end = i
            break
    body = [ln for ln in lines[start:end] if len(ln) > 1]
    # Drop the site boilerplate first line if present.
    body = [ln for ln in body if not ln.startswith("Người Trên Vạn Người - Chương")]
    # Drop site boilerplate header (settings button, SEO blurb), keep chapter title.
    junk = ("Storya", "thể loại", "Đọc online", "Cài đặt", "cập nhật", "miễn phí")
    title = next((ln for ln in body if re.match(r"^Chương \d+", ln)), None)
    prose = [ln for ln in body if len(ln) > 30 and not any(k in ln for k in junk)]
    start_idx = next(
        (
            n
            for n, ln in enumerate(body)
            if len(ln) > 40 and not re.match(r"^Chương \d+", ln) and not any(k in ln for k in junk)
        ),
        0,
    )
    body = ([title] if title else []) + body[start_idx:]
    return "\n\n".join(body).strip() + "\n"


def read_local(path: str | Path) -> str:
    text = Path(path).read_text(encoding="utf-8")
    return text.strip() + "\n"


def ingest(url: str | None = None, file: str | None = None, out: str = "data/chapters/ch01.txt") -> Path:
    if not url and not file:
        raise SystemExit("provide --url or --file")
    text = clean_storya_html(fetch_html(url)) if url else read_local(file)  # type: ignore[arg-type]
    if len(text) < 200:
        raise SystemExit(f"ingested text suspiciously short ({len(text)} chars) — selector may have missed")
    from synthesize import atomic_write  # local import: synthesize never imports ingest (no cycle)

    atomic_write(out, text)
    dest = Path(out)
    print(f"saved {len(text)} chars -> {dest}")
    return dest


if __name__ == "__main__":  # ponytail: one assert self-check, no test framework
    sample = "<html><body><article><h1>Chương 1</h1><p>Thái Cực Quyền hay.</p><p>\"Chào!\" nàng nói.</p><nav>Chương trước</nav></article></body></html>"
    out = clean_storya_html(sample)
    assert "Thái Cực Quyền hay." in out and '"Chào!"' in out and "Chương trước" not in out, out
    print("ingest demo OK")
