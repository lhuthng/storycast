-- assets/crawl/templates/storya.lua — the crawler the pipeline shipped before scripted crawls.
--
-- **Everything site-specific is in this file.** Which element holds the chapter,
-- where the prose starts and stops, which lines are the site's chrome — all of
-- it is `SITE` below, and none of it is in Rust. The host gives a script bytes
-- and pure functions (`fetch`, `select`, `strip_tags`, `decode_entities`,
-- `sanitize`, `readable`, `abs_url`) and has no idea what Storya is; replacing
-- the pipeline's hardcoded Rust crawler meant moving the site's rules here, and
-- this is where they landed.
--
-- The pipeline's old URL crawl *was* a Rust function. `rust/fixtures/crawl/`
-- holds the pages it was tested against and the exact bytes it produced for
-- each, and `script_tests.rs` asserts this script still reproduces every one of
-- them byte for byte. That is what makes the migration an identity rather than
-- a rewrite: an existing workspace cannot tell the difference.
--
-- It works on any site whose chapters are addressable as `chapter-{n}`-style
-- URLs, because the manifest supplies `input.url`, expanded from `url_template`
-- by the host (`{n}`, or `{n:03}` when the site zero-pads).
--
-- For a site whose URLs are slugs, copy this file and add a `discover(input)`
-- function — see `templates/madara.lua` for a paginated listing walk — then
-- point `crawl.script` at the copy.

-- The site's shape. Facts about Storya, not about crawling.
local SITE = {
  -- The body container: the inner HTML of the first of these that exists, in
  -- this order. `body` is last because it always exists.
  container = { "article", "main", "body" },
  -- Where the chapter starts: the hint line, plus the line before it (the
  -- headline often sits above the first hint, and a stray label sometimes
  -- between the two).
  start = { "Chương 1", "Chương ", "Thượng bộ", "Một bộ" },
  -- A hint longer than this is navigation that happens to mention a chapter,
  -- not a headline. Characters, not bytes: Vietnamese is two bytes a letter.
  start_max = 120,
  -- The escape hatch for a page whose first real line names the book instead of
  -- the chapter: any line containing one of these starts the prose. The second
  -- list is matched against the lowercased line.
  trigger = { "Thái Cực Quyền" },
  trigger_lower = { "xuyên không" },
  -- Where it ends: the site's own navigation and footers, searched from the
  -- start on.
  stop = {
    "Chương trước",
    "Chương sau",
    "Truyện cùng thể loại",
    "Nền tảng đọc truyện",
    "Chính sách bảo mật",
  },
  -- Lines that are never the first paragraph of a chapter. Only used to find
  -- where the prose begins; the chapter boundary drops the rest.
  junk = { "Storya", "thể loại", "Đọc online", "Cài đặt", "cập nhật", "miễn phí" },
  -- The byline the site repeats at the top of every chapter.
  byline = "Người Trên Vạn Người - Chương",
  -- The headline form. Storya also repeats it as `81. Chương 81: …` beside the
  -- real one; that numbered copy is dropped at the chapter boundary.
  headline = "Chương ",
  -- The first line longer than this is prose rather than a stray label.
  min_line = 40,
}

local NBSP = "\194\160"

-- `trim`, for the bytes that reach this stage: ASCII whitespace plus U+00A0,
-- which is whitespace to the host's text boundary and not to Lua's `%s`.
local function trim(s)
  local i, j = 1, #s
  while true do
    local s_ = s:sub(i, j)
    if s_ == "" then
      return ""
    end
    if s:sub(i, i):match("%s") then
      i = i + 1
    elseif s:sub(i, i + 1) == NBSP then
      i = i + 2
    elseif s:sub(j, j):match("%s") then
      j = j - 1
    elseif s:sub(j - 1, j) == NBSP then
      j = j - 2
    else
      return s_
    end
  end
end

-- Characters, not bytes: `#s` counts bytes, so a length guard written with it
-- would fire a hundred and twenty *bytes* into a Vietnamese line.
local function chars(s)
  return utf8.len(s) or #s
end

local function any_of(haystack, needles)
  for _, n in ipairs(needles) do
    if string.find(haystack, n, 1, true) then
      return true
    end
  end
  return false
end

-- `<tag>…</tag>` removed whole, content included, with the match done on a
-- lowercased copy so `<SCRIPT>` counts too.
local function remove_block(html, tag)
  local lower = string.lower(html)
  local open, close = "<" .. tag, "</" .. tag
  local out, pos = {}, 1
  while true do
    local rel = string.find(lower, open, pos, true)
    if not rel then
      break
    end
    out[#out + 1] = html:sub(pos, rel - 1)
    local rel_end = string.find(lower, close, rel, true)
    if rel_end then
      local gt = string.find(html, ">", rel_end, true)
      pos = gt and gt + 1 or #html + 1
    else
      pos = #html + 1
    end
  end
  out[#out + 1] = html:sub(pos)
  return table.concat(out)
end

-- The inside of the first `<tag>…</tag>`, or nil when the tag is not there.
--
-- A substring scan and not a parsed tree, deliberately: the question is "what
-- did the site wrap the chapter in", and on the malformed pages that are half
-- the real web a parser's answer to that is a rearrangement, not an answer.
local function inner_of(html, tag)
  local lower = string.lower(html)
  local start = string.find(lower, "<" .. tag, 1, true)
  if not start then
    return nil
  end
  local gt = string.find(html, ">", start, true)
  if not gt then
    return nil
  end
  local body_start = gt + 1
  local fin = string.find(lower, "</" .. tag, body_start, true)
  if not fin then
    return nil
  end
  return html:sub(body_start, fin - 1)
end

-- Whether a line is the chapter headline itself, as opposed to the numbered
-- copy of it, or a label that merely begins with the same word.
local function is_headline(line)
  local head = SITE.headline
  if line:sub(1, #head) ~= head then
    return false
  end
  local c = line:sub(#head + 1, #head + 1)
  return c ~= "" and c:match("%d") ~= nil
end

-- The old Rust extractor, as a script: one page of HTML in, one chapter out.
local function extract(html)
  -- `<script>` and `<style>` bodies are not prose, and neither are the site's
  -- own wrappers. Both go before anything is measured.
  local page = remove_block(remove_block(html, "script"), "style")
  local scoped
  for _, tag in ipairs(SITE.container) do
    scoped = inner_of(page, tag)
    if scoped then
      break
    end
  end
  if not scoped then
    scoped = page
  end

  -- Tags out with block boundaries as line breaks, then entities decoded. The
  -- host's `strip_tags` is the crate's own stripper, so a line ends where it
  -- ends for the digest too.
  local text = decode_entities(strip_tags(scoped))
  local lines = {}
  for line in (text .. "\n"):gmatch("([^\n]*)\n") do
    local t = trim(line)
    if t ~= "" then
      lines[#lines + 1] = t
    end
  end

  -- Where the body starts: the hint line, the line before it, or — when the
  -- page names the book instead — the trigger line. Indices are 0-based to
  -- keep this readable against the code it was ported from.
  local start = 0
  for i = 0, #lines - 1 do
    local line = lines[i + 1]
    if any_of(line, SITE.start) and chars(line) < SITE.start_max then
      start = math.max(i - 1, 0)
      break
    end
    if any_of(line, SITE.trigger) or any_of(string.lower(line), SITE.trigger_lower) then
      start = i
      break
    end
  end

  -- …and where the site's navigation takes over.
  local stop_at = #lines
  for i = start, #lines - 1 do
    if any_of(lines[i + 1], SITE.stop) then
      stop_at = i
      break
    end
  end

  local body = {}
  for i = start, stop_at - 1 do
    local line = lines[i + 1]
    if line:sub(1, #SITE.byline) ~= SITE.byline then
      body[#body + 1] = line
    end
  end

  -- The headline, then the first line that is really prose. Labels, the
  -- byline's cousins and site chrome sit between the two.
  local title
  for _, line in ipairs(body) do
    if is_headline(line) then
      title = line
      break
    end
  end
  local from = 1
  for i, line in ipairs(body) do
    if chars(line) > SITE.min_line and not is_headline(line) and not any_of(line, SITE.junk) then
      from = i
      break
    end
  end

  local out = {}
  if title then
    out[#out + 1] = title
  end
  for i = from, #body do
    out[#out + 1] = body[i]
  end

  -- The chapter boundary — site metadata out, entities decoded — belongs to
  -- the host, and is the same one a manual import and the digest preparer run.
  return sanitize(table.concat(out, "\n\n"))
end

-- Which refusal this is, and whether another attempt is worth a worker.
local function classify(status)
  if status == 429 then
    return { class = "rate_limit", detail = "HTTP " .. status, retry_after = 30 }
  end
  if status == 401 or status == 402 or status == 407 then
    return { class = "login_required", detail = "HTTP " .. status }
  end
  if status == 403 or status == 503 then
    return { class = "challenge", detail = "HTTP " .. status }
  end
  return { class = "unknown", detail = "HTTP " .. status }
end

function crawl(input)
  -- `input.url` comes from the chapter index. The fallback covers the case
  -- where there is no index entry at all (a range past a stale manifest), and
  -- `chapter_url` is the host's own expansion so the padding rules can never
  -- drift between here and Rust.
  local url = input.url
  if not url or url == "" then
    url = chapter_url(input.params.url_template or "", input.n)
  end
  if url == "" then
    error(
      "no URL for ch"
        .. tostring(input.n)
        .. " — set a url_template, or give this script a discover() function"
    )
  end

  local page = fetch(url)
  -- The site has no such chapter: terminal, and not a failure. A range that
  -- runs past the end of a book must not shelve its tail.
  if page.status == 404 or page.status == 410 then
    return { none = true, reason = "HTTP " .. page.status }
  end
  if page.status ~= 200 then
    return { blocked = classify(page.status) }
  end

  return { text = extract(page.body), url = page.url }
end
