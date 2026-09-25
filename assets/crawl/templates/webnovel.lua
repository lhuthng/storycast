-- assets/crawl/templates/webnovel.lua — the hard shape, and the one that
-- teaches the most.
--
-- Read this one *after* `truyencom.lua`, and only if you have to. WebNovel is
-- hard in three separate ways, and each of them is a thing a real site will do
-- to you eventually:
--
--   1. **The chapter URL is a slug, not a number.**
--      `/vi/book/<slug>_<bookId>/chương-<n>-<title-slug>_<chapterId>` — the
--      number is buried in the middle of a percent-encoded Vietnamese slug.
--      There is no `url_template` that produces it, so a `discover` is
--      *mandatory*: the catalog page is the only place the mapping exists.
--   2. **The container you are told about is not the chapter.**
--      `div.chapter_content` holds the book cover, the book title, "Tác giả:",
--      a "© WebNovel" line and the chapter title. The prose is one level
--      deeper, in `div.cha-content > div.cha-words`. Taking the outer one
--      gives you a chapter that opens with the author's name.
--   3. **Cloudflare refuses this crawler.** Not the markup — the client. The
--      same pages a `curl --http1.1` with a browser user agent can read are
--      `403 cf-mitigated: challenge` to this pipeline, and that is a fact about
--      the TLS fingerprint, not about the selectors. There is no TLS spoofing,
--      no browser engine and no challenge solver here, and this template does
--      not pretend otherwise. **The only route is a session cookie**: open the
--      page in a browser, solve the challenge, and put `cf_clearance` in
--      `crawl.headers`. See the setup below.
--
-- Setup, in full — and note step 2 is not optional:
--
--   "crawl": {
--     "mode": "script",
--     "user_agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) …Chrome/131…",
--     "headers": { "Cookie": "cf_clearance=<paste from your browser>" },
--     "script": "assets/crawl/templates/webnovel.lua",
--     "params": { "catalog": "https://www.webnovel.com/vi/book/<bookId>/catalog" }
--   },
--   "url_template": ""
--
--   1. find the book's numeric id from the book URL
--   2. get a cf_clearance cookie from a real browser for that host
--   3. `bm-inductor check <a chapter url>` — the cookie is read from the active
--      workspace, so this is the command that tells you whether step 2 worked
--
-- `url_template` is empty and must stay empty: there is nothing to template.
-- The index `discover` writes is the whole mapping, and it is frozen per range,
-- so a catalog that shifts under you mid-run cannot remap half a book.
--
-- **This template is here to be read, and it may not be usable.** It documents
-- the shape — a slug URL, a container one level deeper than the obvious one, a
-- paid-chapter flag — and all three of those recur on sites that are not behind
-- a bot check. Copy the structure; expect to replace the selectors.

local L = {
  -- ── the catalog ──
  -- WebNovel paginates its catalog as *fourteen separate* `<ol>` elements, one
  -- per batch, 2840 chapters in this book. A selector that names one of them
  -- finds 200 chapters and stops; a selector that names the class finds all of
  -- them in document order, which is what "all of them" means here.
  index_list = "ol.content-list li",
  -- The chapter number is not in the href (it is inside an encoded slug) and not
  -- in the link text (which also carries a "7 years ago" timestamp). It is in
  -- its own element, which is why this walks `li` and not `a`.
  index_number = "i._num",
  -- The title is an **attribute**, so it takes `select_all` and a `.attrs` read —
  -- `select` returns the element's *text*, which here is the chapter number, the
  -- title and an upload date run together: "1 Chương 1: … 7 years ago".
  index_link_el = "a[href]",

  -- ── the chapter ──
  -- NOT `div.chapter_content` — see the note at the top. This is the prose.
  body = "div.cha-words",
  -- The container we interrogate for the lock flag and the real title. Kept as a
  -- selector rather than hardcoded so the outer/inner distinction stays visible.
  container = "div.chapter_content",
  -- `data-islock="1"` on the container means this chapter is paid: the prose is
  -- not in the page at all, and no amount of retrying will produce it. Saying
  -- so is the difference between a chapter that shelves and a cluster that
  -- spends an afternoon on it.
  lock_attr = "data-islock",
  title_attr = "data-chaptername",
}

-- The one element whose attributes we need.
local function container_of(body)
  local els = select_all(body, L.container)
  return els[1]
end

function discover(input)
  local catalog = input.params.catalog
  if not catalog or catalog == "" then
    error("params.catalog is not set — point it at the book's /catalog page")
  end
  local r = fetch(catalog)
  if r.status ~= 200 then
    return { blocked = { class = "challenge", detail = "catalog: HTTP " .. r.status } }
  end
  local why = challenge(r)
  if why then
    return { blocked = { class = "challenge", detail = "the catalog page: " .. why } }
  end

  local items = select_all(r.body, L.index_list)
  if #items == 0 then
    log("catalog selector " .. L.index_list .. " matched nothing on " .. r.url)
    return { blocked = { class = "empty", detail = "no chapter list on the catalog page" } }
  end

  local out, seen, total = {}, {}, nil
  for _, li in ipairs(items) do
    -- The number first: an entry we cannot number is one we cannot place, and
    -- placing it wrongly is worse than skipping it.
    local n = tonumber(select(li.html, L.index_number) or "")
    -- The <a> is read once, and both the href and the title come off its
    -- *attributes*. `select` returns an element's text, which here is the
    -- chapter number, the title and an upload date run together — so
    -- `select(li.html, "a[href]")` is not a URL, it is a sentence, and
    -- `abs_url` will happily turn that sentence into one.
    local a = select_all(li.html, L.index_link_el)[1]
    local href = a and a.attrs.href or ""
    if n and href ~= "" and not seen[n] then
      seen[n] = true
      -- Root-relative on this site, so it has to be resolved against the page it
      -- was found on — `/vi/book/…`, not `https://www.webnovel.com/…`.
      out[#out + 1] = {
        n = n,
        url = abs_url(r.url, href),
        title = a.attrs.title or "",
      }
      total = n
    end
  end
  table.sort(out, function(a, b)
    return a.n < b.n
  end)
  log("catalog: " .. tostring(#out) .. " chapters, last is ch" .. tostring(total))
  return { chapters = out, total = total }
end

function crawl(input)
  if not input.url then
    error("no URL for ch" .. tostring(input.n) .. " — discover() is mandatory on this site; there is no url_template to fall back to")
  end
  local r = fetch(input.url)
  if r.status == 404 or r.status == 410 then
    return { none = true, reason = "HTTP " .. r.status }
  end
  -- `challenge(r)` catches BOTH shapes a bot check arrives in: the `403` with
  -- `cf-mitigated: challenge`, and the `200` whose body is an interstitial.
  -- The second is the dangerous half — nothing in the status says no, so a
  -- crawler without this line stores the bot check as `chNN.txt` and calls it a
  -- chapter. It is a host function rather than a per-template check so that no
  -- template, including an operator's own, can forget it.
  local why = challenge(r)
  if why then
    return {
      blocked = {
        class = "challenge",
        detail = why
          .. " — this crawler cannot pass a Cloudflare check on its own; a session cookie in crawl.headers is the only route",
      },
    }
  end
  if r.status == 403 or r.status == 503 then
    return {
      blocked = {
        class = "challenge",
        detail = "HTTP " .. r.status .. " from a bot check — this site needs crawl.http1 = true",
      },
    }
  end
  if r.status ~= 200 then
    return { blocked = { class = "unknown", detail = "HTTP " .. r.status } }
  end

  local box = container_of(r.body)
  if box == nil then
    return { blocked = { class = "empty", detail = "no " .. L.container .. " on the page" } }
  end
  -- A paid chapter renders its shell and stops. The prose is not in the page,
  -- so the honest answer is a refusal, not a short chapter.
  if box.attrs[L.lock_attr] == "1" then
    return {
      blocked = {
        class = "login_required",
        detail = "data-islock=1 — the chapter is paid and its text is not served",
      },
    }
  end

  -- `select_text` on the *inner* container. Each paragraph is its own `<p>`
  -- here, so the paragraph breaks come for free — the opposite of truyencom,
  -- which is why the two templates do not share a body-parsing step.
  local text = select_text(r.body, L.body)
  if text == "" then
    return { blocked = { class = "empty", detail = "no element matched " .. L.body } }
  end

  -- The title is an attribute, not an element, so it does not come along with
  -- the prose. The pipeline speaks a chapter's first line as its title.
  local title = box.attrs[L.title_attr]
  if title and title ~= "" and string.find(text, title, 1, true) == nil then
    text = title .. "\n\n" .. text
  end

  return { text = text, url = r.url }
end
