-- Crawl chapters out of a local EPUB.
--
-- The whole point of this file: **the book is already on this machine**, and a
-- crawler is still the right way to read it. Naming the file in
-- `crawl.params.epub` is enough — there is no URL, no listing page and no site
-- to be refused by, so the crawl becomes a lookup into the book.
--
-- ## Setup
--
--   workspaces/<book>/crawl/epub.lua          this file
--   workspaces/<book>/tmp/book.epub            the book
--   workspaces/<book>/settings.json            { "crawl": { "script": "crawl/epub.lua",
--                                                        "params": { "epub": "tmp/book.epub" } } }
--
-- The path is relative to the workspace and is **confined to it**: a book
-- outside is refused by name, so the one local read this crawler does not
-- become a way to read the worker. Drop the file in the workspace's tmp
-- directory and name it as `tmp/book.epub`; a bare `book.epub` beside this
-- script works too.
--
-- ## Several volumes
--
-- Name a **directory** in `crawl.params.books` instead of one file, and every
-- `.epub` directly inside it is one volume, in file-name order (name them
-- `vol-01.epub`, `vol-02.epub`). The volumes are numbered as **one book** — the
-- running count continues across them — so the pipeline's dense index stays
-- dense: `chNN.txt` is the book's Nth chapter, and a chapter's locator in the
-- index says which volume and spine range it is. The bytes of those volumes are
-- part of the index fingerprint, so replacing one rebuilds the tree by itself.
--
-- ## Why this is Lua and not Rust
--
-- A crawl script runs with `io` and `os` removed — `fetch` is the only way out
-- — so a script cannot open a file even in principle, and an EPUB is a ZIP of
-- DEFLATE-compressed XHTML besides. The host reads the container; this file
-- decides what to ask it for. That is the whole division: the book is
-- structure, the crawl is a policy.
--
-- ## The part that is this book's knowledge, and not EPUB's
--
-- **A spine entry is not a chapter.** That sentence is the reason this file
-- exists, and the reason it is not in `adapters/`: it is not about any site.
--
-- Two publishers make EPUBs, and they make them differently.
--
-- * One **types** the book: the spine holds one entry per chapter, and entry
--   4 is chapter 4. Almost every EPUB in the world is this.
--
-- * One **scans** it: each page of the print book becomes a file with a
--   paragraph of OCR beside a JPEG, and the spine is the page order. The
--   Internet Archive's *Apothecary Diaries* volume 1 is one of these — 219
--   spine entries, 32 chapters. Entry 1 is the navigation document, 2 is the
--   Archive's own copyright notice, 4 to 7 are decorative pages whose OCR came
--   back at 23% accuracy, 8 to 11 are the title page, the contents, an
--   illustration list and an app advertisement, and only at entry 12 does
--   `Chapter 1: Maomao` begin. A crawler that treated the spine as a chapter
--   list would narrate the Archive's copyright notice as chapter 1.
--
-- Nothing in the file format says which kind of book this is. `nav.xhtml` and
-- `toc.ncx` are both *empty* in that book, so the publisher's own chapter list
-- — the thing that would have answered the question — is not there to be read.
-- What is there is the prose, and the prose says `Chapter 1`, `Chapter 2`, at
-- the points where the chapters begin.
--
-- So the rule below is: **an entry whose text begins a chapter heading starts
-- a chapter, and everything before the first one is front matter.** That is a
-- fact about how this publisher's books are put together, so it lives here,
-- next to the rest of what a script knows about its source — not in Rust,
-- where somebody reviewing the host would not expect to find it.
--
-- `crawl.params.heading` overrides the pattern, and
-- `crawl.params.heading = false` turns the rule off and goes back to one
-- entry being one chapter, which is right for every typed book.
--
-- ## What this still cannot do
--
-- A book that splits one chapter across files the *other* way — `ch1a`,
-- `ch1b` — is not helped by this, and the heading rule makes it worse: it
-- would find `Chapter 1` in `ch1a` and nothing in `ch1b`, which is correct, but
-- it would also find `Chapter 2` in `ch2a` and stop there, so `ch1b` and the
-- rest of the file run would be folded into the chapter after it. Reading that
-- book wants a per-book rule written for it, which is what this file is.

--- Which book. `params` is passed through verbatim, so this is the operator's
--- `crawl.params.epub` and nothing else.
local function book_path(input)
  local p = input.params or {}
  local path = p.epub or p.book or p.path
  if not path or path == "" then
    error("crawl.params.epub (or crawl.params.books) is not set — name the book")
  end
  return path
end

--- The volumes to read.
---
--- `crawl.params.books` names a **directory** holding one `.epub` per volume;
--- otherwise this is the single `crawl.params.epub` as one book. The listing is
--- the *host's* — a script has no filesystem — and the paths it returns are
--- workspace-relative, which is exactly what `epub_index`/`epub_text` take.
---
--- Sorted by file name, so the volume order (and therefore the chapter numbers)
--- are a property of the library rather than of the filesystem's mood. Name the
--- volumes so that order is the reading order: `vol-01.epub`, `vol-02.epub`.
local function book_paths(input)
  local p = input.params or {}
  local dir = p.books
  if dir and dir ~= "" then
    local found = epub_books(dir)
    if not found or #found == 0 then
      error(("no .epub in the books directory %q"):format(dir))
    end
    return found
  end
  return { book_path(input) }
end

--- What counts as the start of a chapter, in this book's prose.
---
--- Anchored at the start (`^`) because the heading is the first thing on the
--- page, and case-insensitive because the OCR is inconsistent about it
--- (`Chapter 1:` and `CHAPTER 12:` both occur). The number is required so a
--- sentence that merely mentions a chapter mid-paragraph does not match.
local function heading_pattern(input)
  local p = input.params or {}
  if p.heading == false then
    return nil
  end
  if type(p.heading) == "string" and p.heading ~= "" then
    return p.heading
  end
  return "^%s*[Cc][Hh][Aa][Pp][Tt][Ee][Rr]%s+%d"
end

--- Where each chapter begins, as a list of spine positions.
---
--- `nil` means "this book has no chapter headings", and the caller then falls
--- back to one entry per chapter — which is the right answer for every typed
--- book, and the reason the rule is opt-out rather than opt-in.
---
--- One match is treated as none. A book with a single `Chapter 1` and no other
--- heading is a one-chapter book or a book that merely mentions one, and
--- merging everything after it into "chapter 1" would be a worse guess than
--- leaving a well-formed spine alone.
local function chapter_starts(input, items)
  local pattern = heading_pattern(input)
  if not pattern or not items then
    return nil
  end
  local starts = {}
  for _, item in ipairs(items) do
    -- `chars > 0` so an image-only page is never a chapter, whatever its head.
    if item.chars > 0 and string.find(item.head, pattern) then
      starts[#starts + 1] = item.n
    end
  end
  if #starts < 2 then
    return nil
  end
  return starts, #items
end

--- The spine positions that make up chapter `n`, or `nil` past the end.
local function chapter_range(input, path, n, items)
  local starts, total = chapter_starts(input, items)
  if not starts then
    -- A typed book: the spine *is* the chapter list.
    if n < 1 or n > (epub_total(path) or 0) then
      return nil
    end
    return n, n
  end
  if n < 1 or n > #starts then
    return nil
  end
  -- A chapter runs to the spine entry before the next heading. The last one
  -- runs to the end of the book, and `epub_text` clamps a range that is one
  -- too far, so `total + 1` needs no special case here.
  return starts[n], (starts[n + 1] or (total + 1)) - 1
end

--- One book's chapters, as `{from, to}` spine ranges in reading order.
---
--- A scanned book's chapters are the entries that begin a heading; a typed
--- book's are the spine itself. This is the one place the two shapes are folded
--- into a list, so a multi-volume `discover` can number them as one book.
local function book_ranges(input, path)
  local items = epub_index(path)
  local starts = chapter_starts(input, items)
  local out = {}
  if starts then
    local total = (items and #items) or 0
    for i = 1, #starts do
      out[#out + 1] = { from = starts[i], to = (starts[i + 1] or (total + 1)) - 1 }
    end
  else
    local total = epub_total(path) or 0
    for n = 1, total do
      out[#out + 1] = { from = n, to = n }
    end
  end
  return out
end

--- Every `Chapter N: …` heading in a run-on string, as `{n = , heading = }`.
---
--- The contents page of a scanned book is one paragraph, not a list: this
--- book prints `Table of Contents Cover Chapter 1: Maomao Chapter 2: The Two
--- Consorts Chapter 3: Jinshi …` with nothing between the entries. So the
--- entries are cut at the *next* `Chapter N:`, which is the only delimiter the
--- page has.
local function headings_in(text)
  local out, pos = {}, 1
  while pos <= #text do
    local s, e, num = string.find(text, "Chapter%s+(%d+)%s*:", pos)
    if not s then
      break
    end
    local nxt = string.find(text, "Chapter%s+%d+%s*:", e + 1)
    local stop = (nxt and nxt - 1) or #text
    out[#out + 1] = { n = tonumber(num), heading = (text:sub(s, stop):gsub("%s+$", "")) }
    pos = stop + 1
  end
  return out
end

--- The book's own table of contents, as `{n = , heading = }` per chapter.
---
--- **A clue, and the only reliable one available.** `nav.xhtml` and `toc.ncx`
--- are both empty in a scanned book, so the publisher's structured chapter list
--- does not exist; what exists is the printed contents page, one paragraph of
--- run-on text, and it names every heading exactly.
---
--- It matters because the OCR glues each heading to the prose after it with a
--- single space and no markup of any kind — there is not one `<h1>` in 166
--- pages — so `Chapter 1: Maomao` and `What I wouldn't give for some
--- good street-stall meat skewers.` arrive as one 79-character sentence and the
--- narrator says the title aloud. Guessing where a title ends is a guess: the
--- run of capitalised words after the colon could be a four-word title or a
--- four-word sentence. The contents says, and this file only has to agree with
--- it.
---
--- The page is found by asking which spine entry mentions "contents" **and**
--- yields at least two headings — a chapter that happens to use the word is
--- not a contents page, and one heading is a sentence that mentions a chapter.
--- Several candidates are allowed and the richest wins.
local function contents(input, path, items)
  local p = input.params or {}
  if p.toc == false then
    return nil
  end
  local best
  for _, item in ipairs(items or {}) do
    if item.chars > 0 and string.find(string.lower(item.head), "contents", 1, true) then
      local found = headings_in(epub_text(path, item.n, item.n) or "")
      if #found > 1 and (not best or #found > #best) then
        best = found
      end
    end
  end
  return best
end

--- A chapter's heading off the front of its text, using the contents as the
--- clue. Returns the text unchanged when the contents do not name it.
---
--- Matching is a **prefix test against the exact string the book printed**, and
--- the longest entry that matches wins. No match means no change, which is the
--- right default: this book's own contents is OCR too, and it has mangled two
--- of its own titles (`Chapter 16: The Garden Pa Part One`), so a chapter whose
--- heading the contents got wrong must be left alone rather than split at a
--- position this file invented.
local function split_heading(toc, text)
  if not toc then
    return text
  end
  local best
  for _, entry in ipairs(toc) do
    local h = entry.heading
    if string.sub(text, 1, #h) == h and (not best or #h > #best) then
      best = h
    end
  end
  if not best then
    return text
  end
  local body = (string.sub(text, #best + 1):gsub("^%s+", ""))
  if body == "" then
    return text
  end
  -- Heading on its own line, so the shared boundary keeps it its own paragraph
  -- and the digest makes it a short title segment instead of folding it into
  -- the first sentence.
  return best .. "\n\n" .. body
end

--- What to delete from the text before it becomes a chapter.
---
--- A scanned book has the scanner's furniture in its OCR. In the Apothecary
--- Diaries the page number and the hosting site are typeset into the last line
--- of every page, and the OCR glues them onto the end of a real sentence on
--- 200 of its 217 pages:
---
---     She wiped at a window frame with a rag as she spoke. 7 Goldenagato | mp4directs.com
---
--- A pipeline that does not delete that narrates "Goldenagato | mp4directs dot
--- com" out loud, mid-clause, and once a chapter in five it lands alone on a
--- line and becomes a segment of its own. It is 1% of the book's characters and
--- it is in every chapter.
---
--- `crawl.params.junk` replaces the list: one Lua pattern, or an array of
--- them. `false` or an empty array turns the rule off, which is right for a
--- typed book with no scan furniture in it.
---
--- The page number is part of the default pattern on purpose: it is always
--- printed immediately before the site name, so one pattern removes both. It
--- is `%d*` and not `%d+` because one page in this book — page 132 — carries
--- the name with no number in front of it at all, and a pattern that required
--- the number left it in exactly as loudly as the other 199. A book whose
--- numbers sit elsewhere wants its own pattern — and a bare page number on its
--- own line is *not* stripped by default, because in a book where numbers do
--- stand alone, a line of digits is as likely to be a table or a date as
--- furniture.
local function junk_patterns(input)
  local p = input.params or {}
  if p.junk == false then
    return {}
  end
  if p.junk ~= nil then
    if type(p.junk) == "string" then
      return { p.junk }
    end
    return p.junk
  end
  return { "%s*%d*%s*Goldenagato%s*|%s*mp4directs%.com" }
end

--- Delete every pattern in `patterns` from `text`.
---
--- Only the deletion happens here. The host runs the shared crawl boundary
--- over whatever a script returns — see `provider.rs` — so the trailing space
--- a deletion leaves and the blank line a wholly-junk line leaves behind are
--- tidied once, in one place, by the same code a crawled page goes through. A
--- second copy of that tidying in a script is how the same chapter ends up
--- shaped differently depending on which one produced it.
---
--- A pattern that does not compile is skipped rather than raised: a typo in
--- `crawl.params.junk` would otherwise cost three retries and shelve the row
--- for a book that is sitting right there, perfectly readable.
local function strip_junk(text, patterns)
  for _, pattern in ipairs(patterns) do
    local ok, stripped = pcall(string.gsub, text, pattern, "")
    if ok then
      text = stripped
    end
  end
  return text
end

--- One chapter, or `nil` when the book does not have it.
---
--- `nil` rather than an error is the important part: a range that runs past
--- the end of a book is the ordinary shape of asking for a chapter that is
--- not there, and the contract has a word for exactly that — `none`, a terminal
--- non-failure that must not cost a strike. Raising here would burn three
--- retries and then shelve the row.
function crawl(input)
  -- A locator from `discover`: which volume, and which spine range. That is the
  -- multi-volume path — and it is why the tree is carried in the index rather
  -- than rebuilt: a chapter names its own place, so nothing re-walks the
  -- library to find it.
  local path, from, to
  local loc = input.url
  if type(loc) == "string" then
    -- `#` is literal; `%-` is an escaped `-`, which is otherwise the lazy
    -- modifier and would never match the separator before `to`.
    local p, f, t = string.match(loc, "^epub:(.+)#(%d+)%-(%d+)$")
    if p then
      path, from, to = p, tonumber(f), tonumber(t)
    end
  end

  local items
  if not path then
    -- No locator: the single-book shape, and what a hand-written index still
    -- hands in.
    path = book_path(input)
    -- One walk of the book per crawl: the chapter starts and the contents are
    -- both read off the same index, and `epub_index` is the expensive call.
    items = epub_index(path)
    from, to = chapter_range(input, path, input.n, items)
    if not from then
      log(("book has no chapter %d"):format(input.n))
      local starts = chapter_starts(input, items)
      local total = starts and #starts or (epub_total(path) or 0)
      return { none = true, reason = ("the book has %d chapters"):format(total) }
    end
  end

  local text = from == to and (epub_chapter(path, from) or {}).text or epub_text(path, from, to)
  if text then
    text = strip_junk(text, junk_patterns(input))
    if items then
      -- The scanned-book heading split needs the book's own contents page, so
      -- it runs only on the single-book path, where `epub_index` was walked
      -- anyway. A multi-volume library numbers chapters; it does not re-walk
      -- every volume per chapter to find one contents page.
      text = split_heading(contents(input, path, items), text)
    end
  end
  if not text or text == "" then
    return { none = true, reason = ("chapter %d is spine %d..%d, which has no text"):format(input.n, from, to) }
  end
  return {
    text = text,
    -- Echo only. It lands in the ledger so a walk can say where a chapter came
    -- from; nothing parses it for a number.
    url = ("epub:%s#%d-%d"):format(path, from, to),
  }
end

--- The book's own chapter count, once for the whole range.
---
--- Optional: a plain `url_template` is a degenerate mapping the host can
--- compute, and this exists because a book knows its own length. Returning the
--- total lets a range that runs off the end be trimmed *before* twenty tasks
--- are enqueued, rather than after twenty of them come back absent.
function discover(input)
  -- The library is numbered as **one book**: volume by volume, in name order,
  -- and a volume's first chapter continues the running count. So the pipeline's
  -- index stays dense and `ch01.txt` is the book's first chapter, not whichever
  -- volume the filesystem happened to list first.
  local start = input.start or 1
  local count = input.count or 0
  local p = input.params or {}
  local multi = p.books ~= nil and p.books ~= ""
  local chapters = {}
  local total = 0
  for _, path in ipairs(book_paths(input)) do
    for _, r in ipairs(book_ranges(input, path)) do
      total = total + 1
      if total >= start and total < start + count then
        local entry = { n = total }
        if multi then
          -- The locator `crawl(n)` reads back: which volume and which spine
          -- entries. It rides the index's `url`, which the offer hands to the
          -- worker. Single-file keeps `{ n = n }`, the shape it has always
          -- had — there is nothing to fetch, only a book to read.
          entry.url = ("epub:%s#%d-%d"):format(path, r.from, r.to)
        end
        chapters[#chapters + 1] = entry
      end
    end
  end
  return { chapters = chapters, total = total }
end
