-- assets/crawl/templates/madara.lua — a crawler for a listing site.
--
-- The second kind of crawl: the chapter URL is **not** a function of `n`. The
-- site exposes an index, so the crawler enters at `params.entry`, reads the
-- listing, and follows links. That is what `discover` is for — and it runs
-- once per range on the inductor, not once per chapter, so ten workers do not
-- each re-read the index.
--
-- Copy it, point `params.entry` at your site, and adjust the selectors:
--
--   "crawl": { "script": "assets/crawl/templates/madara.lua",
--              "params": { "entry": "https://site.example/truyen/ten-truyen" } }
--
-- Everything site-specific is in `L`. If your site is a Madara/WordPress novel
-- theme (the shape most of them are), the defaults usually work as-is, and
-- `log()` lines say which selector matched so the first run tells you what to
-- fix.

local L = {
  -- Chapter links on the index page, and the "next page" link if the listing
  -- is paginated. Comma-separated alternatives, first match wins per selector.
  listing = "li.wp-manga-chapter a, .chapter-item a, a.chapter-link",
  next = "a.next_page, .nav-links a.next, a[rel=next]",
  -- The chapter body, in order of preference, then the headline.
  body = "div.text-left, .reading-content, div#chapter-content, article",
  title = "h1, .entry-title",
  -- How many index pages to walk before giving up. A `next` link that loops
  -- back to the first page is a real bug on real sites.
  max_pages = 60,
}

-- The chapter number inside a chapter URL. Madara themes put it last:
-- `/truyen/x/chuong-34/`, `/x/chapter-34`, `/x/c-34-5`. When the number is not
-- the last run of digits, this is the one line to change — no heuristic will
-- ever guess a site's slug scheme, which is exactly why it is the operator's.
local function chapter_number(url)
  return tonumber(string.match(url, "(%d+)%D*$") or "")
end

function discover(input)
  local entry = input.params.entry
  if not entry or entry == "" then
    error("params.entry is not set — point it at the book's index page")
  end
  local out, seen = {}, {}
  local url = entry
  local pages = 0
  while url and pages < (input.params.max_pages or L.max_pages) do
    pages = pages + 1
    local r = fetch(url)
    if r.status ~= 200 then
      log("index page " .. tostring(pages) .. " -> HTTP " .. tostring(r.status) .. "; stopping the walk")
      break
    end
    local links = select_all(r.body, L.listing)
    if #links == 0 then
      log("listing selector matched nothing on " .. r.url .. " — check L.listing")
      break
    end
    for _, l in ipairs(links) do
      local href = l.attrs.href
      if href and href ~= "" and not seen[href] then
        seen[href] = true
        local n = chapter_number(href)
        if n then
          out[#out + 1] = { n = n, url = abs_url(r.url, href), title = l.text }
        end
      end
    end
    local nxt = select(r.body, L.next)
    url = (nxt ~= "" and abs_url(r.url, nxt)) or nil
  end
  table.sort(out, function(a, b)
    return a.n < b.n
  end)
  -- `total` is what lets the host mark a range that runs past the end of the
  -- book as absent, instead of enqueueing chapter 381..400 against a 380-chapter
  -- site and shelving twenty rows.
  local total = #out > 0 and out[#out].n or nil
  log("index: " .. tostring(#out) .. " chapters over " .. tostring(pages) .. " page(s)")
  return { chapters = out, total = total }
end

function crawl(input)
  if not input.url then
    error(
      "no URL for ch"
        .. tostring(input.n)
        .. " — discover() mapped no URL for it; the index it writes is what this reads (see L.listing)"
    )
  end
  local r = fetch(input.url)
  if r.status == 404 or r.status == 410 then
    return { none = true, reason = "HTTP " .. r.status }
  end
  if r.status ~= 200 then
    return { blocked = { class = "unknown", detail = "HTTP " .. r.status } }
  end

  -- `select_text` and not `select`: the container is prose, and it has to come
  -- back with its paragraph breaks. `select` would squeeze the whole chapter
  -- onto one line and the narrator would read it in a single breath.
  local text = select_text(r.body, L.body)
  if text == "" then
    -- The container is not one this template knows. Rather than write nothing
    -- (which the length guard would refuse anyway), fall back to the generic
    -- heuristic — that is what `readable` is for — and say so in the ledger.
    local fallback = readable(r.body)
    text = fallback.text
    log("body selector matched nothing; readable() returned " .. tostring(#text) .. " bytes")
  end

  -- The first line is the chapter's headline, and the pipeline speaks it as the
  -- title. The listing's own text is only a label; the page's <h1> is the real
  -- one.
  local headline = select(r.body, L.title)
  if headline ~= "" and string.find(text, headline, 1, true) == nil then
    text = headline .. "\n\n" .. text
  end
  return { text = text, url = r.url }
end
