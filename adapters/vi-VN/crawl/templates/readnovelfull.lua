-- assets/crawl/templates/readnovelfull.lua — the third kind of crawl.
--
-- `truyencom.lua` templates the URL, `madara.lua` walks a paginated listing,
-- and this one needs **neither**, which is the case worth having a template for:
--
--   * the chapter URL is `/{book}/chapter-{n}-{title-slug}.html`, so the number
--     is in it but is *not* the last thing — no `{n}` expansion can invent a
--     title slug, and the URL cannot be templated;
--   * the book's index page lists only the first ~30 chapters, with no
--     pagination and no "view all" (a 2730-chapter book lists 2..30 and stops);
--   * so the only complete index is the **chain of `next_chap` links** on the
--     chapter pages themselves, and `discover` walks it.
--
-- That chain is also what makes the `total` claim honest, which is the whole
-- subtlety here. See the note on `total` below — getting it wrong truncates a
-- long book and calls it a success.
--
-- Setup, in full:
--
--   "crawl": { "mode": "script",
--              "script": "assets/crawl/templates/readnovelfull.lua",
--              "params": { "book": "https://readnovelfull.com/the-sword-god-of-the-universe.html" },
--              "max_fetches": 400, "max_seconds": 900 },
--   "url_template": ""
--
-- `url_template` is empty and must stay empty. **`max_fetches` and
-- `max_seconds` have to be raised**, because a discover that walks `next_chap`
-- spends one fetch per chapter and the defaults (64 fetches, 180s) are sized
-- for a single chapter, not for building an index. This runs once per range on
-- the inductor, not once per worker — but it is O(chapters) fetches, which is
-- the price of a site with no complete index and no derivable URL. Pace it with
-- `crawl.pace_ms` and the walk costs about 0.75s a chapter.

local L = {
  -- ── the book index (the first ~30 chapters, and their titles) ──
  -- `ul.list-chapter` and NOT `#list-chapter li`: the container also holds the
  -- book description, and on some books a genre sidebar whose links look like
  -- chapter links. Scoping to the class is the whole difference.
  index_link = "#list-chapter ul.list-chapter li a",
  -- The number is in the href, before the title slug.
  number_pattern = "chapter%-(%d+)%-",

  -- ── the chain ──
  -- The next chapter's own link, which carries the full URL including the slug
  -- that made this URL untemplatable in the first place.
  next_chap = "a#next_chap",
  -- A sane ceiling. A `next_chap` that loops is a real bug on real sites, and a
  -- walk with no ceiling is a way to spend an afternoon.
  max_hops = 4000,

  -- ── the chapter ──
  body = "#chr-content",
  title = "a.chr-title",
  -- The first `<p>` of the body is the chapter title, TWICE, glued to the first
  -- sentence. The site renders `Chapter N: Title` into the body while the
  -- headline carries the same text without the colon, so the body's form is
  -- rebuilt from the headline rather than guessed at.
  body_title_pattern = "^(Chapter %d+)%s+",
}

-- The form the site writes the title in *inside the body* — the headline's, with
-- a colon after the number.
local function body_form(headline)
  return string.gsub(headline, L.body_title_pattern, "%1: ", 1)
end

-- Cut a leading run of repeated title off the front of the text.
--
-- The duplication is the useful part: the site writes the title twice, so the
-- prose is exactly what follows the second copy, and there is no guessing about
-- where a title ends and a sentence begins. A single copy is handled too,
-- because a one-off un-duplicated chapter should not be the thing that breaks.
local function cut_leading_title(text, headline)
  if headline == nil or headline == "" then
    return text
  end
  local one = body_form(headline)
  for _, prefix in ipairs({ one .. " " .. one, one }) do
    local at = string.find(text, prefix, 1, true)
    -- Plain find anywhere would be wrong: only a *leading* run is the artifact,
    -- and a chapter that quotes its own title in the prose must survive.
    if at == 1 then
      return string.sub(text, #prefix + 1)
    end
  end
  return text
end

-- Read the book page: the first chapters, with their titles.
--
-- Returns the chapters, the highest number seen, and that chapter's URL — which
-- is where the `next_chap` walk starts.
local function seed_from_book(entry)
  local r = fetch(entry)
  if r.status ~= 200 then
    return nil, nil, nil, "book page: HTTP " .. r.status
  end
  local why = challenge(r)
  if why then
    return nil, nil, nil, why
  end
  local out, by_n, high, high_url = {}, {}, 0, nil
  for _, l in ipairs(select_all(r.body, L.index_link)) do
    local href = l.attrs.href
    local n = href and tonumber(string.match(href, L.number_pattern))
    if n and not by_n[n] then
      local c = {
        n = n,
        url = abs_url(r.url, href),
        title = l.attrs.title or "",
      }
      by_n[n] = c
      out[#out + 1] = c
      if n > high then
        high, high_url = n, c.url
      end
    end
  end
  return out, by_n, { n = high, url = high_url }, nil
end

function discover(input)
  local entry = input.params.book
  if not entry or entry == "" then
    error("params.book is not set — point it at the book's page, e.g. https://readnovelfull.com/the-sword-god-of-the-universe.html")
  end
  local want_from = tonumber(input.start) or 1
  local want_count = tonumber(input.count) or 1
  local want_to = want_from + want_count - 1
  local max_hops = tonumber(input.params.max_hops) or L.max_hops

  local chapters, by_n, tip, err = seed_from_book(entry)
  if not chapters then
    return { blocked = { class = "unknown", detail = err } }
  end
  if #chapters == 0 then
    log("index selector " .. L.index_link .. " matched nothing on " .. entry .. " — check it")
    return { blocked = { class = "empty", detail = "no chapter list on the book page" } }
  end
  local highest = chapters[#chapters].n
  log("book page listed " .. #chapters .. " chapters (up to ch" .. highest .. ")")

  -- ── the chain walk ──
  --
  -- Only when the range reaches past what the book page listed, and only as far
  -- as it does. A range inside the first 30 costs one fetch.
  --
  -- `reached_end` is the important one: it is true only when a chapter page had
  -- no `next_chap` at all, which is the site's way of saying the book is
  -- finished. Stopping because we hit the range, or the hop ceiling, is not the
  -- same thing and must not be reported as one.
  -- The loop is written so that **the page just fetched is the chapter it is
  -- filed under**. The obvious shape — read the next link, then file *it* with
  -- the title off *this* page — is off by one, and hands chapter N the headline
  -- of chapter N-1. So each iteration does two things, in this order: record
  -- what the page we are holding says about *itself*, then ask it for the next.
  local reached_end, hops, url = false, 0, tip and tip.url or nil
  while url and highest < want_to and hops < max_hops do
    local r = fetch(url)
    if r.status ~= 200 then
      log("chain: " .. url .. " -> HTTP " .. r.status .. "; stopping the walk")
      break
    end

    -- The chapters the book page listed came with their titles from the list.
    -- The ones this walk found have none — the list never mentioned them — and
    -- the headline of their own page is the only place the title is written.
    local this_n = tonumber(string.match(url, L.number_pattern))
    local mine = this_n and by_n[this_n]
    if mine and mine.title == "" then
      mine.title = select(r.body, L.title)
    end

    -- `select` would hand back the link's *label* ("Next chapter") rather than
    -- its href, and the label resolves against the base URL into the page we are
    -- already on. `select_all(...).attrs.href` is the address.
    local links = select_all(r.body, L.next_chap)
    local href = links[1] and links[1].attrs.href
    if not href or href == "" then
      reached_end = true
      break
    end
    local next_url = abs_url(r.url, href)
    local n = tonumber(string.match(next_url, L.number_pattern))
    hops = hops + 1
    if not n then
      log("chain: " .. next_url .. " has no chapter number in it; stopping the walk")
      break
    end
    if n > highest then
      highest = n
      -- Filed from the previous page's own `next_chap`, which is the site's
      -- word that it exists. Its title is filled in on the next iteration, or
      -- never, if the fetch of it fails — an untitled chapter is an honest
      -- unknown, and crawl() will come back 404 and mark it absent.
      local c = { n = n, url = next_url, title = "" }
      by_n[n] = c
      chapters[#chapters + 1] = c
    end
    url = next_url
  end
  if url and hops >= max_hops then
    log("chain: stopped at the " .. max_hops .. "-hop limit with chapters left")
  end

  table.sort(chapters, function(a, b)
    return a.n < b.n
  end)

  -- ── `total`, and the one rule that matters here ──
  --
  -- `total` is what the host uses to mark a range past the end of the book
  -- *absent* — a terminal verdict. So it may only be reported when the book was
  -- actually walked to its end. Reporting the last chapter we happened to reach
  -- instead is how a 2730-chapter novel becomes a 30-chapter one: the host
  -- believes the number, marks 31..2730 absent, never crawls them, and every
  -- ledger row looks healthy.
  --
  -- So: reached the end, or stopped on an HTTP error at the end of the chain →
  -- report it. Stopped because the range was satisfied → say nothing, and let
  -- the next range walk further.
  local total = nil
  if reached_end then
    total = highest
    log("index: " .. #chapters .. " chapters, the book ends at ch" .. highest)
  else
    log("index: " .. #chapters .. " chapters up to ch" .. highest .. " (more may follow; total withheld)")
  end
  return { chapters = chapters, total = total }
end

function crawl(input)
  if not input.url then
    error("no URL for ch" .. tostring(input.n) .. " — this site's URLs carry a title slug, so discover() is the only way to get one")
  end
  local r = fetch(input.url)
  if r.status == 404 or r.status == 410 then
    return { none = true, reason = "HTTP " .. r.status }
  end
  local why = challenge(r)
  if why then
    return { blocked = { class = "challenge", detail = why } }
  end
  if r.status ~= 200 then
    return { blocked = { class = "unknown", detail = "HTTP " .. r.status } }
  end

  -- `select_text`: `<p>…</p><br>` throughout, so the paragraph breaks are the
  -- markup's own and nothing has to be invented.
  local text = select_text(r.body, L.body)
  if text == "" then
    return { blocked = { class = "empty", detail = "no element matched " .. L.body } }
  end

  local headline = select(r.body, L.title)
  -- The doubled title at the head of the body, cut before anything else: the
  -- headline is prepended afterwards, so leaving it would make every chapter
  -- open with its own title three times over.
  text = cut_leading_title(text, headline)
  if headline ~= "" and string.find(text, headline, 1, true) == nil then
    text = headline .. "\n\n" .. text
  end

  return { text = text, url = r.url }
end
