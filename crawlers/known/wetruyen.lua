-- crawlers/known/wetruyen.lua — an *easy* site, same shape as truyencom.
--
--   1. **The chapter URL is a function of the number.**
--      `{slug}/chuong-{n}.html`. A `url_template` is a complete crawler on
--      its own and `discover` only reports the mapping's end.
--   2. **The body is one element**, `#chapter-c`, prose separated by `<br>`.
--      The host turns each `<br>` into a newline, so unlike truyencom there is
--      no paragraph separator to split on here: every line is already its own
--      paragraph by the time the chapter boundary sees it.
--   3. **No bot check** on chapter pages. A plain request with a browser UA
--      gets the chapter. The empty `<ins class="adsbygoogle">` placeholder and
--      its `<script>` sit inside the container but carry no text — the host
--      drops the script block and the placeholder contributes nothing — so
--      there is no tail marker to cut, either.
--
-- Setup, in full:
--
--   "crawl": { "mode": "script",
--              "script": "crawlers/known/wetruyen.lua",
--              "params": { "slug": "yeu-than-ky" } },
--   "url_template": "https://wetruyen.com/{slug}/chuong-{n}.html"
--
-- Then paste that chapter URL into `bm-inductor check` before you trust it.
--
-- The one thing to know before copying this file elsewhere: a missing chapter
-- is a 404, and a 404 is the end of the book — `none`, never `blocked`, so a
-- range that runs past the last chapter stops instead of burning strikes.

local L = {
  -- The body. First match wins; the rest are fallbacks for when the site moves.
  body = "#chapter-c, div.chapter-c",
  -- The headline. Present on the page, absent from the body container, so it is
  -- prepended below — the pipeline speaks a chapter's first line as its title.
  title = "h1 a.chapter-title, h1 a, h1",

  -- The number inside a chapter URL: `.../chuong-42.html`. Kept for the day a
  -- listing walk needs it; the template mapping does not.
  number_pattern = "chuong%-(%d+)%.html",
}

-- The book's end, found without an index: URLs are a function of n, so there
-- is nothing to walk. Returning nothing tells the host to fall back to the
-- url_template, and a 404 past the last chapter marks the range's end.
function discover(input)
  log("no index walk — the url_template is the whole mapping")
  return nil
end

function crawl(input)
  if not input.url then
    error("no URL for ch" .. tostring(input.n) .. " — set url_template")
  end
  local r = fetch(input.url)
  -- A chapter that is not there is not a failure, it is the end of the book.
  -- `none` is terminal; `blocked` would burn three strikes on a 404.
  if r.status == 404 or r.status == 410 then
    return { none = true, reason = "HTTP " .. r.status }
  end
  -- Past the last chapter this site 301s to the homepage and answers 200
  -- there, so a status test alone would store the front page as a chapter.
  -- A chapter that did not stay where it was asked for is not a chapter.
  if r.url ~= input.url then
    return { none = true, reason = "redirected to " .. r.url }
  end
  -- The 403 Cloudflare serves, and the 200 it sometimes serves instead.
  -- `challenge(r)` knows both: it reads the `cf-mitigated` header when there is
  -- one, and recognises the interstitial body when there is not. A status test
  -- alone walks straight into the second case and stores the bot check as the
  -- chapter. The family note in knownsites says this shape reads as success.
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

  -- `select_text`, not `select`: this container is prose, and the host turns
  -- each `<br>` into a newline on the way to us.
  local text = select_text(r.body, L.body)
  if text == "" then
    return { blocked = { class = "empty", detail = "no element matched " .. L.body } }
  end

  -- The headline lives in the <h1>, not in the body container, and the pipeline
  -- speaks a chapter's first line as its title — so put it back if it is not
  -- already there.
  local headline = select(r.body, L.title)
  if headline ~= "" and string.find(text, headline, 1, true) == nil then
    text = headline .. "\n\n" .. text
  end

  return { text = text, url = r.url }
end
