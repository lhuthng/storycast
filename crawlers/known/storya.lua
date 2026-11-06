-- crawlers/known/storya.lua — the crawler the pipeline shipped before scripted crawls.
--
-- **Everything site-specific is in this file.** Which element holds the chapter,
-- where the prose starts and stops, which lines are the site's chrome — all of
-- it is `SITE` below, and none of it is in Rust. The host gives a script bytes
-- and pure functions (`fetch`, `select`, `strip_tags`, `decode_entities`,
-- `sanitize`, `readable`, `abs_url`) and has no idea what Storya is; replacing
-- the pipeline's hardcoded Rust crawler meant moving the site's rules here, and
-- this is where they landed.
--
-- The pipeline's old URL crawl *was* a Rust function. `backend/fixtures/crawl/`
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
  -- real one; that numbered copy is dropped below.
  headline = "Chương ",
  -- The site's own furniture, dropped from anywhere in the chapter. This is
  -- here, and not in the host, because it is a fact about Storya: `sanitize`
  -- decodes entities and tidies whitespace and nothing else, and the day it
  -- learned the word "Storya" the host would be a second, hardercoded crawler
  -- nobody can edit without touching Rust.
  artifact = {
    -- Whole lines, compared whitespace-insensitively and case-insensitively
    -- over ASCII — which is all `string.lower` folds.
    exact = { "cài đặt đọc", "người trên vạn người" },
    -- A promo line only when it *also* names the site. A story may say a task
    -- was completed, carry an author's `PS:`, or mention a platform; only the
    -- site-shaped combinations go.
    --
    -- Both spellings are listed where a capital falls on a Vietnamese letter,
    -- because the case fold does not reach it: the site writes `Đọc online`
    -- and `Nền tảng đọc truyện` with a capital `Đ`/`Nền`, and `Đ` is not the
    -- same byte as `đ`. Spelling the needles the way the site prints them is
    -- more honest than teaching a crawler file a Vietnamese case table.
    promo_any = {
      "đọc online", "Đọc online",
      "cập nhật nhanh nhất", "Cập nhật nhanh nhất",
      "nền tảng đọc truyện", "Nền tảng đọc truyện",
    },
    site_word = "storya",
    -- The false "the story is finished" footer. All of them, deliberately: any
    -- one of them alone is ordinary prose.
    completion = {
      "hệ thống", "Hệ thống",
      "chiếc đỉnh", "Chiếc đỉnh",
      "hậu cung", "Hậu cung",
      "truyện đã hoàn thành", "Truyện đã hoàn thành",
    },
    -- `ps:` / `p/s:` at the start of a line, as the sites write them.
    post = { "ps:", "p/s:" },
    -- The cross-promo tail names a different book every time, so the prefix is
    -- the only stable part of it. `Mời ` alone is not enough: prose can start
    -- that way, and one chapter's dialogue does.
    promo_prefix = { "mời đọc", "mời các bạn", "mời mọi người" },
    -- The ad the site appends after the last paragraph is plain paragraphs with
    -- no wrapper, so only the tail is searched, and this holds promo copy — never
    -- a book or character name. The `Name: …` form is recognised by shape.
    promo_tail = {
      "kỹ năng bị động",
      "điệp viên 007", "Điệp viên 007",
      "hãy đến ngay với",
      "xuất phẩm",
      "bộ này không phải truyện hay",
      "biên tập lại nhân sinh",
      "xuyên qua vô tận thứ nguyên",
      "cùng chư thiên vạn giới",
      "nghiền nát mọi kẻ thù",
      "nghịch thiên cải mệnh",
      "mời đọc", "mời mọi người", "đón đọc", "đón xem", "đọc ngay",
      "đề cử đọc", "giới thiệu truyện", "truyện của ta", "truyện giải trí",
      "cẩu đạo", "bạo chương", "xuyên nhanh", "vô địch lưu",
    },
  },
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

-- `81. Chương 81: …` beside the real headline: the site's numbered copy, which
-- is metadata and is not spoken twice.
local function is_numbered_heading(line)
  if not line:match("^%d") then
    return false
  end
  local rest = line:match("^%d+%. (.+)$")
  return rest ~= nil and is_headline(rest)
end

-- A cross-promo line names another book; the prefix is the stable part.
local function is_promo(line)
  local lower = string.lower(line)
  for _, p in ipairs(SITE.artifact.promo_prefix) do
    if lower:sub(1, #p) == p then
      return true
    end
  end
  return false
end

-- `Name: …` with a one- or two-word label, unquoted: the ad's dialogue form.
-- Prose reports speech that way too (`Hắn nói: …`), so one line is never
-- enough — the block below needs two, with different labels.
local function ad_label(line)
  if string.find(line, '"', 1, true) then
    return nil
  end
  local label = line:match("^([^%s:][^:]-):%s")
  if not label then
    return nil
  end
  local _, words = label:gsub("%S+", "")
  if words > 2 or chars(label) > 24 then
    return nil
  end
  return label
end

-- Whether a line ends its sentence. A multi-byte last character (`…`, a curly
-- quote) is an ending too.
local function ends_closed(line)
  local last = line:sub(-1)
  if string.find('.!?:)]"\'', last, 1, true) then
    return true
  end
  return last:byte() >= 128
end

-- Any quote means the line is reported speech, not a pitch. The curly forms are
-- spelled in UTF-8 so `string.lower` cannot disturb them.
local QUOTES = { '"', "'", "\226\128\156", "\226\128\157", "\226\128\152", "\226\128\153" }

local function has_quote(line)
  for _, q in ipairs(QUOTES) do
    if string.find(line, q, 1, true) then
      return true
    end
  end
  return false
end

-- Short enough to be a pitch, so a whole paragraph of prose that merely carries
-- an ad keeps the prose.
local function is_tagline(line)
  return chars(line) <= 160 and not ends_closed(line) and not has_quote(line)
end

-- `…! Name: … Name: …` — the ad typed into the last paragraph behind the prose.
-- One tag is how prose reports speech, so two are required.
local function glued_at(line)
  if has_quote(line) then
    return nil
  end
  local init, count, first = 1, 0, nil
  while true do
    local s = string.find(line, "[%.!?]%s+[^%s:][^:]-:%s", init)
    if not s then
      break
    end
    count = count + 1
    first = first or s
    init = s + 1
  end
  if count < 2 then
    return nil
  end
  return first
end

local function is_promo_line(line)
  return any_of(string.lower(line), SITE.artifact.promo_tail) or is_tagline(line)
end

-- Drop the ad from the end: a block of `Name:` lines, then any trailing promo
-- line, then a dialogue line glued onto the prose (`…! Name: …`).
local function strip_promo_tail(out)
  while #out > 2 do
    local a, b = ad_label(out[#out]), ad_label(out[#out - 1])
    if not a or not b or a == b then
      break
    end
    table.remove(out)
    table.remove(out)
  end
  while #out > 1 do
    local last = out[#out]
    if is_promo_line(last) then
      table.remove(out)
    else
      local at = glued_at(last)
      if at then
        out[#out] = last:sub(1, at)
      end
      break
    end
  end
end

-- Whether a line is the site's furniture rather than the novel. Every word
-- checked here is a fact about Storya, which is why this function exists in
-- this file and not in the host.
local function is_artifact(line)
  local lower = string.lower(line)
  local flat = string.gsub(lower, "%s+", " ")
  for _, exact in ipairs(SITE.artifact.exact) do
    if flat == exact then
      return true
    end
  end
  if any_of(lower, SITE.artifact.promo_any) and any_of(lower, { SITE.artifact.site_word }) then
    return true
  end
  if any_of(lower, SITE.artifact.completion) then
    return true
  end
  for _, p in ipairs(SITE.artifact.post) do
    if lower:sub(1, #p) == p then
      return true
    end
  end
  if is_promo(line) then
    return true
  end
  -- The reader header between the headline and the prose: the date with its
  -- reading-time estimate, and the read counters printed under it.
  if string.find(lower, "chương này khoảng", 1, true)
    and string.find(lower, "%d%d/%d%d/%d%d%d%d") then
    return true
  end
  if string.find(lower, "lượt đọc chương", 1, true)
    or string.find(lower, "lượt đọc truyện", 1, true) then
    return true
  end
  return false
end

-- The translator's one-letter stub, which Storya leaves at the end of some
-- paragraphs: `…cũng đều kinh hãi khi chứng kiến cảnh tượng đó. m.` (ch386) and
-- `"Không, ngươi không xứng." m.` (ch362), both confirmed on the live page.
--
-- It is not a word and not part of the sentence, but the digest's source gate
-- requires every word of the chapter to be spoken exactly once, and a model
-- drops the stub on sight — so the chapter is refused for ever: ch386 burned 15
-- attempts on this one fragment and was shelved. Cheaper and more honest to
-- never hand it to the model.
--
-- Only the letters the site actually stubs with are listed. A paragraph ending
-- `y.`, `a.` or `u.` is the pronoun, the sentence particle and `âm u` — real
-- words, five of them in this corpus — so "drop a dangling letter" would delete
-- prose to clean up two paragraphs.
local STUB_LETTERS = { "m" }

local function strip_stub(line)
  for _, letter in ipairs(STUB_LETTERS) do
    local stub = " " .. letter .. "."
    if #line > #stub and line:sub(-#stub) == stub then
      return (string.gsub(line:sub(1, -#stub - 1), "%s+$", ""))
    end
  end
  return line
end

-- The old Rust extractor, as a script: one page of HTML in, one chapter out.
local function extract(html)
  -- The live reader header splits the number and the subtitle into separate
  -- elements; the shape below would see only `Chương N` and lose the title.
  local header_hint = select(html, "article header > p")
  local header_subtitle = select(html, "article header h1")

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
  if header_hint ~= "" and header_subtitle ~= "" and is_headline(header_hint) then
    title = header_hint .. ": " .. header_subtitle
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
    local line = body[i]
    if line ~= header_hint and line ~= header_subtitle then
      out[#out + 1] = line
    end
  end

  -- The cross-promo is a block: `Mời đọc:` introduces the titles it
  -- recommends, so everything from that line on is the site's, not the book's.
  for i, line in ipairs(out) do
    if is_promo(line) then
      while #out >= i do
        table.remove(out)
      end
      break
    end
  end
  strip_promo_tail(out)

  -- The site's own lines go here, where the site is described: the numbered
  -- headline copy, the promo strip, the completion footer, the author's PS.
  -- What is left is prose, and the host's `sanitize` is then only tidying —
  -- entities, whitespace, paragraph shape — the same boundary a manual import
  -- and the digest preparer run.
  local kept = {}
  for _, line in ipairs(out) do
    if not is_artifact(line) and not is_numbered_heading(line) then
      kept[#kept + 1] = strip_stub(line)
    end
  end
  return sanitize(table.concat(kept, "\n\n"))
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
