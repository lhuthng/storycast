// assets/crawl/storya.js — the default crawler's JavaScript twin.
//
// Not the workspace default (that is `storya.lua`), but shipped so the second
// engine is a working example rather than a claim: same contract, same host
// functions, same output. Point `crawl.script` at this file to run the Storya
// crawl on QuickJS instead of Lua — the parity test asserts both engines
// reproduce the bytes in `rust/fixtures/crawl/` exactly.
//
// Like the Lua crawler, everything site-specific is in `SITE` below: the host
// knows nothing about Storya. `crawl` may be `async`, but every host function
// here is synchronous, so it is written plainly synchronous because that is the
// honest shape for this job.

// The site's shape. Facts about Storya, not about crawling.
const SITE = {
  // The body container: the inner HTML of the first of these that exists, in
  // this order. `body` is last because it always exists.
  container: ["article", "main", "body"],
  // Where the chapter starts: the hint line, plus the line before it.
  start: ["Chương 1", "Chương ", "Thượng bộ", "Một bộ"],
  // A hint longer than this is navigation that happens to mention a chapter,
  // not a headline. Characters, not bytes: Vietnamese is two bytes a letter.
  start_max: 120,
  // The escape hatch for a page whose first real line names the book instead of
  // the chapter. The second list is matched against the lowercased line.
  trigger: ["Thái Cực Quyền"],
  trigger_lower: ["xuyên không"],
  // Where it ends: the site's own navigation and footers, from the start on.
  stop: [
    "Chương trước",
    "Chương sau",
    "Truyện cùng thể loại",
    "Nền tảng đọc truyện",
    "Chính sách bảo mật",
  ],
  // Lines that are never the first paragraph of a chapter.
  junk: ["Storya", "thể loại", "Đọc online", "Cài đặt", "cập nhật", "miễn phí"],
  // The byline the site repeats at the top of every chapter.
  byline: "Người Trên Vạn Người - Chương",
  // The headline form. The numbered copy of it is dropped later, at the
  // chapter boundary.
  headline: "Chương ",
  // The first line longer than this is prose rather than a stray label.
  min_line: 40,
};

// ASCII-only lowering, so it preserves length exactly as the host's own
// `to_ascii_lowercase` does. `String.prototype.toLowerCase` is Unicode-aware and
// can *change* length (U+0130 becomes two code units), which would slide every
// index in the scans below.
function asciiLower(s) {
  return s.replace(/[A-Z]/g, (c) => c.toLowerCase());
}

// Characters, not code units: an astral character is two of the latter.
function chars(s) {
  return Array.from(s).length;
}

function anyOf(haystack, needles) {
  return needles.some((n) => haystack.indexOf(n) !== -1);
}

// `<tag>…</tag>` removed whole, content included.
function removeBlock(html, tag) {
  const lower = asciiLower(html);
  const open = "<" + tag;
  const close = "</" + tag;
  let out = "";
  let pos = 0;
  for (;;) {
    const rel = lower.indexOf(open, pos);
    if (rel === -1) break;
    out += html.slice(pos, rel);
    const relEnd = lower.indexOf(close, rel);
    if (relEnd === -1) {
      pos = html.length;
    } else {
      const gt = html.indexOf(">", relEnd);
      pos = gt === -1 ? html.length : gt + 1;
    }
  }
  return out + html.slice(pos);
}

// The inside of the first `<tag>…</tag>`, or null when the tag is not there.
// A substring scan and not a parsed tree, deliberately — see the Lua crawler.
function innerOf(html, tag) {
  const lower = asciiLower(html);
  const start = lower.indexOf("<" + tag);
  if (start === -1) return null;
  const gt = html.indexOf(">", start);
  if (gt === -1) return null;
  const bodyStart = gt + 1;
  const fin = lower.indexOf("</" + tag, bodyStart);
  if (fin === -1) return null;
  return html.slice(bodyStart, fin);
}

// Whether a line is the chapter headline itself, rather than the numbered copy
// of it or a label that merely begins with the same word.
function isHeadline(line) {
  if (!line.startsWith(SITE.headline)) return false;
  const c = line.charAt(SITE.headline.length);
  return c !== "" && c >= "0" && c <= "9";
}

// The old Rust extractor, as a script: one page of HTML in, one chapter out.
function extract(html) {
  // `<script>` and `<style>` bodies are not prose, and neither are the site's
  // own wrappers.
  let page = removeBlock(html, "script");
  page = removeBlock(page, "style");
  let scoped = null;
  for (const tag of SITE.container) {
    scoped = innerOf(page, tag);
    if (scoped !== null) break;
  }
  if (scoped === null) scoped = page;

  // Tags out with block boundaries as line breaks, then entities decoded. The
  // host's `strip_tags` is the same stripper the digest reads with.
  const text = decode_entities(strip_tags(scoped));
  const lines = [];
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line !== "") lines.push(line);
  }

  // Where the body starts: the hint line, the line before it, or the trigger.
  let start = 0;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (anyOf(line, SITE.start) && chars(line) < SITE.start_max) {
      start = Math.max(i - 1, 0);
      break;
    }
    if (anyOf(line, SITE.trigger) || anyOf(line.toLowerCase(), SITE.trigger_lower)) {
      start = i;
      break;
    }
  }

  // …and where the site's navigation takes over.
  let stopAt = lines.length;
  for (let i = start; i < lines.length; i++) {
    if (anyOf(lines[i], SITE.stop)) {
      stopAt = i;
      break;
    }
  }

  const body = [];
  for (let i = start; i < stopAt; i++) {
    const line = lines[i];
    if (!line.startsWith(SITE.byline)) body.push(line);
  }

  // The headline, then the first line that is really prose.
  let title = null;
  for (const line of body) {
    if (isHeadline(line)) {
      title = line;
      break;
    }
  }
  let from = 0;
  for (let i = 0; i < body.length; i++) {
    const line = body[i];
    if (chars(line) > SITE.min_line && !isHeadline(line) && !anyOf(line, SITE.junk)) {
      from = i;
      break;
    }
  }

  const out = title === null ? body.slice(from) : [title].concat(body.slice(from));
  // The chapter boundary — site metadata out, entities decoded — belongs to the
  // host, and is the same one a manual import and the digest preparer run.
  return sanitize(out.join("\n\n"));
}

// Which refusal this is, and whether another attempt is worth a worker.
function classify(status) {
  if (status === 429) return { class: "rate_limit", detail: "HTTP " + status, retry_after: 30 };
  if (status === 401 || status === 402 || status === 407) {
    return { class: "login_required", detail: "HTTP " + status };
  }
  if (status === 403 || status === 503) {
    return { class: "challenge", detail: "HTTP " + status };
  }
  return { class: "unknown", detail: "HTTP " + status };
}

function crawl(input) {
  let url = input.url;
  if (!url) {
    url = chapter_url(input.params.url_template || "", input.n);
  }
  if (!url) {
    throw new Error("no URL for ch" + input.n + " — set a url_template, or give this script a discover() function");
  }

  const page = fetch(url);
  if (page.status === 404 || page.status === 410) {
    return { none: true, reason: "HTTP " + page.status };
  }
  if (page.status !== 200) {
    return { blocked: classify(page.status) };
  }
  return { text: extract(page.body), url: page.url };
}
