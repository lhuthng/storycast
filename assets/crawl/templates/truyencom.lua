-- assets/crawl/templates/truyencom.lua — an *easy* site, and the shape most
-- sites have.
--
-- Read this one first, and copy it when a site turns out to be easy. Three
-- things make a site easy, and truyencom has all three:
--
--   1. **The chapter URL is a function of the number.** `{slug}/chuong-{n}.html`.
--      So a `url_template` is a complete crawler on its own and `discover` is
--      optional. If your site is this shape you do not need a listing walk at
--      all, which is the single biggest thing that makes a site easy.
--   2. **The body is one element** and nothing else on the page looks like it.
--   3. **No bot check.** A plain request with a browser UA gets the chapter.
--
-- Setup, in full:
--
--   "crawl": { "mode": "script",
--              "script": "assets/crawl/templates/truyencom.lua",
--              "params": { "slug": "nga-thi-nhan-gian-tinh-long-vuong" } },
--   "url_template": "https://truyencom.com/{slug}/chuong-{n}.html"
--
-- Then paste that chapter URL into `bm-inductor check` before you trust it.
--
-- ── The one thing worth copying from this file ──────────────────────────────────
-- The body is **plain text with no <p> in it at all**. Truyencom separates its
-- paragraphs with `&#13; &#13;` — two carriage returns — inside a single text
-- node. The host's chapter boundary splits on newlines, and a lone CR is not a
-- newline to it, so a crawler that just selects the container hands the whole
-- chapter back as one 8,000-character paragraph: technically a crawl, one
-- breath, and a digest that cannot find a sentence in it.
--
-- So the paragraph split happens *here*, in the script, next to the site
-- knowledge that makes it necessary. That is the whole point of the split
-- between this file and Rust.

local L = {
  -- The body. First match wins; the rest are fallbacks for when the site moves.
  body = "#chapter-c, div.chapter-c",
  -- The headline. Present on the page, absent from the body container, so it is
  -- prepended below — the pipeline speaks a chapter's first line as its title.
  title = "h1 a.chapter-title, h1 a, h1",

  -- ── the site's paragraph separator ──
  -- What sits between two paragraphs, as a Lua pattern. `&#13;` reaches us
  -- already decoded, as a CR, with the site's own spacing around it.
  paragraph_break = "\r[ \t]*",

  -- ── the tail the site appends ──
  -- Every chapter ends `( bản chương xong )` — "chapter draft finished". It is
  -- site chrome in the site's own voice, and the host knows nothing about it,
  -- so this file drops it. Matched as a plain find: the first hit is the real
  -- one, and a novel that *mentions* the phrase inside a paragraph would not be
  -- cut at it.
  tail = "( bản chương xong )",

  -- ── the index, for titles and for knowing where the book ends ──
  -- Optional. Without `params.index` this file crawls fine from `url_template`
  -- alone; with it, `discover` also fills in chapter titles and marks a range
  -- that runs past the last chapter as absent instead of 404-ing every row.
  --
  -- `ul.list-chapter` and NOT `#list-chapter li`: the pagination row lives
  -- inside the same container, and its links are also bare numbers. Ask for the
  -- whole container and you index "2" and "Last" as chapters.
  index_link = "#list-chapter ul.list-chapter li a",
  -- …and the listing is PAGINATED, 50 chapters to a page, with the page links in
  -- the third `<ul>` of that same container. Following them is not optional: a
  -- discover that reads page 1 and stops reports `total = 50`, and the host
  -- believes it — so every chapter past 50 is marked *absent* and never crawled.
  -- Silent truncation, wearing a success message.
  index_next = "#list-chapter ul:not(.list-chapter) li a",
  max_pages = 60,
  -- The number inside a chapter URL: `.../chuong-42.html`. One line to change
  -- for a different slug scheme — no heuristic will guess it.
  number_pattern = "chuong%-(%d+)%.html",

  -- The book page prefixes every title with "<book> - ". The chapter title is
  -- what follows.
  title_after = " %- (Chương.*)$",
}

-- Cut `text` at the first literal occurrence of `marker`, if it is there.
local function cut_at(text, marker)
  if marker == nil or marker == "" then
    return text
  end
  local at = string.find(text, marker, 1, true)
  if at then
    return string.sub(text, 1, at - 1)
  end
  return text
end

-- The book's index, for titles and for the end of the book.
--
-- `params.index` is the book page: `https://truyencom.com/{slug}.19988/`. Note
-- the numeric id — the bare slug is a different page, and reading *that* one
-- silently yields the "truyện cùng thể loại" sidebar instead, which is a list
-- of other books' chapters with links that look exactly like the real ones.
--
-- Walks every page of the listing, because a book is longer than one page and a
-- truncated index is the most expensive kind of quiet bug there is here: it
-- reports success, and calls everything it did not see "absent".
function discover(input)
  local entry = input.params.index
  if not entry or entry == "" then
    -- Not an error. A site whose URLs are a function of n does not need this
    -- function, and returning nothing is how the host is told to fall back to
    -- the url_template rather than failing the run.
    log("no params.index — the url_template is the whole mapping")
    return nil
  end
  local out, seen, total = {}, {}, nil
  local visited, url, pages = {}, entry, 0
  local max_pages = tonumber(input.params.max_pages) or L.max_pages

  while url and pages < max_pages do
    -- A `next` link that loops back to page 1 is a real bug on real sites, and
    -- this is what stops one page of history from becoming an afternoon.
    if visited[url] then
      log("index: " .. url .. " was already read; the pagination loops")
      break
    end
    visited[url] = true
    pages = pages + 1

    local r = fetch(url)
    if r.status ~= 200 then
      log("index page " .. pages .. " -> HTTP " .. r.status .. "; stopping the walk")
      break
    end
    local why = challenge(r)
    if why then
      return { blocked = { class = "challenge", detail = "the index page: " .. why } }
    end
    local links = select_all(r.body, L.index_link)
    if #links == 0 then
      log("index selector " .. L.index_link .. " matched nothing on " .. r.url)
      break
    end
    local added = 0
    for _, l in ipairs(links) do
      local href = l.attrs.href
      local n = href and tonumber(string.match(href, L.number_pattern))
      if n and not seen[n] then
        seen[n] = true
        added = added + 1
        -- The <a title> is "<book> - Chương N: <title>"; the link text is only
        -- the number, so the title has to come from the attribute.
        local title = l.attrs.title
        if title and title ~= "" then
          local tail = string.match(title, L.title_after)
          title = tail or title
        end
        out[#out + 1] = { n = n, url = abs_url(r.url, href), title = title }
        total = n
      end
    end
    log("index page " .. pages .. ": +" .. added .. " chapters (" .. #out .. " so far)")

    -- The "next" page: the highest page number offered after the one we are on.
    -- Taking the *last* such link rather than the first is what walks the whole
    -- book; the pagination is numbered, so page 5 is five fetches, not five
    -- hundred.
    local best
    for _, l in ipairs(select_all(r.body, L.index_next)) do
      local p = tonumber(string.match(l.attrs.href or "", "trang%-(%d+)"))
      if p and p > pages and (not best or p < best.n) then
        best = { n = p, url = abs_url(r.url, l.attrs.href) }
      end
    end
    url = best and best.url or nil
  end

  table.sort(out, function(a, b)
    return a.n < b.n
  end)
  if pages >= max_pages and url then
    log("index: stopped at the " .. max_pages .. "-page limit with pages left")
  end
  log("index: " .. tostring(#out) .. " chapters over " .. tostring(pages) .. " page(s), last is ch" .. tostring(total))
  return { chapters = out, total = total }
end

function crawl(input)
  if not input.url then
    error("no URL for ch" .. tostring(input.n) .. " — set url_template, or give params.index so discover() can map it")
  end
  local r = fetch(input.url)
  -- A chapter that is not there is not a failure, it is the end of the book.
  -- `none` is terminal; `blocked` would burn three strikes on a 404.
  if r.status == 404 or r.status == 410 then
    return { none = true, reason = "HTTP " .. r.status }
  end
  -- The 403 Cloudflare serves, and the 200 it sometimes serves instead.
  -- `challenge(r)` knows both: it reads the `cf-mitigated` header when there is
  -- one, and recognises the interstitial body when there is not. A status test
  -- alone walks straight into the second case and stores the bot check as the
  -- chapter.
  local why = challenge(r)
  if why then
    return { blocked = { class = "challenge", detail = why } }
  end
  if r.status == 403 or r.status == 503 then
    return { blocked = { class = "challenge", detail = "HTTP " .. r.status .. " from a bot check" } }
  end
  if r.status ~= 200 then
    return { blocked = { class = "unknown", detail = "HTTP " .. r.status } }
  end

  -- `select_text`, not `select`: this container is prose.
  local text = select_text(r.body, L.body)
  if text == "" then
    return { blocked = { class = "empty", detail = "no element matched " .. L.body } }
  end

  -- ── the paragraph split, done here and nowhere else ──
  -- The host decoded the entities on the way to us, so what is in `text` now is
  -- a real CR. Turn each run of them into a blank line, which is what the
  -- chapter boundary splits on.
  text = string.gsub(text, L.paragraph_break, "\n\n")
  -- …and drop the site's own end-of-draft marker, which is now its own line.
  text = cut_at(text, L.tail)

  -- The headline lives in the <h1>, not in the body container, and the pipeline
  -- speaks a chapter's first line as its title — so put it back if it is not
  -- already there.
  local headline = select(r.body, L.title)
  if headline ~= "" and string.find(text, headline, 1, true) == nil then
    text = headline .. "\n\n" .. text
  end

  return { text = text, url = r.url }
end
