# Crawling — scripts instead of a hardcoded fetcher

## If you only want your book

*You can stop reading after this section. The rest is for writing a crawler.*

There is one command that matters, and it is not the crawler:

```bash
bm-inductor check https://your-site.example/book/chapter-1
```

Paste in a real chapter URL. It fetches that one page and tells you, in a
sentence, whether the site will actually give you the chapter or something
else — a bot check, an error page, a page with no chapter on it. If the site is
one Storycast has a crawler for, it also prints the settings block to paste
into your workspace.

Do this **before** anything else. The alternative is a cluster that cannot tell
you a site is refusing it, and instead tells you one worker at a time, a minute
apart, all afternoon — while you debug something that was never broken.

What it says, and what it means:

| It says | It means |
| --- | --- |
| `ok: 13438 bytes of prose in 77 paragraph(s)` | Good. This site works; a crawler can read it. |
| `blocked … Cloudflare` | The site will not serve a program. Storycast cannot fix this, and neither can waiting. You need text files, a different site, or a cookie from a real browser. |
| `200 but the body is a Cloudflare interstitial` | The nastier one: a **success** status hiding a refusal. A crawler reading only the status would store the bot-check page as your chapter. |
| `no chapter on this page` | The site answered, but that URL is not a chapter. Usually the index is at a different address. |

If it says `ok` and the site is not one it knows, you need a crawler: a small
file that says which part of the page is the text. §4 below is a brief you can
paste straight into an AI chat together with a saved copy of the page, and it
comes back with a working one. `assets/crawl/templates/` has five crawlers to
start from, four of them written against a page captured from the live site
they are for.

Two things worth knowing before you start, because they are the ones that cost
people a day:

- **A new workspace fetches nothing.** You have to say where chapters come from.
  That is deliberate — a fresh install should not start hammering a website
  you never pointed it at.
- **A crawler can quietly truncate your book.** If it cannot find the full
  chapter list it may report "this book has 30 chapters" for a novel with
  2,730, and the machine believes it. This is the single nastiest bug in
  crawling and §4 is mostly about not writing one.

## The one thing worth understanding

If you take a single idea from this document, take this one, because it explains
almost every rule in it:

> **Your crawler decides what the AI is even asked to do. A cleaner chapter
> does not just sound better — it makes the rest of the program work.**

The chain is short and it has no gaps in it:

```
your script  ->  the chapter text  ->  the digest's split  ->  the voices
 (cleanliness)     (what it kept)      (narration/dialogue)     (who speaks)
```

Here is what each link actually does, because each one can fail quietly and none
of them reports an error when they do.

**1. What your script keeps decides what the digest can even see.** Before any AI
is involved, the chapter is cut into pieces and each piece is labelled
narration or dialogue. **The label is decided by quote marks and nothing else**
— `"`, `“` or `「`. There is no model in that decision. So if the text your
crawler returns has no quote marks in it, every piece is narration, and the book
is read start to finish in one voice. Nothing fails. The attribution answer is
complete, the checks pass, the audio renders, and every row on the dashboard
looks healthy. This is why the digest now prints the split, first thing, before
the model is asked anything — a real chapter from this project's own workspace:

```
prepared 52 event(s): 21 narration, 31 dialogue
```

**Zero dialogue on a chapter where people are talking is a crawler problem, not
an AI problem** — and now you can see it without reading a log.

**2. Left-over page furniture becomes work the AI has to do.** Navigation links,
"report this chapter", a duplicated chapter title, a site footer: none of these
are refused anywhere. They are simply handed to the digest, which turns every
one into a piece of the chapter that must be attributed to a speaker and must
appear exactly once, in order, in the finished script. So a page with fifty
lines of junk in it produces a script that must account for all fifty. The
digest gets *harder* in proportion to how dirty the crawl was, and the first
thing to break is the check that every piece was used exactly once.

**3. Paragraph breaks are not decoration.** The chapter is cut into sentences
using paragraph and sentence boundaries. A site whose paragraphs are separated
by two carriage returns and no `<p>` at all — which is real, and is what
`truyencom.lua` exists for — hands over one 8,000-character line. That line is
8,000 bytes, so it sails past the 200-byte "is this even a chapter" guard
without complaint. It is technically a successful crawl: one line, one breath,
and an audio file with no pause in it anywhere.

**4. A duplicated title is spoken twice.** ReadNovelFull writes the chapter
title into the body *twice*, glued to the front of the first sentence, and the
headline carries it again. Miss the cut and every chapter opens with its own
title three times, and the first real sentence of the book is never reached.

**What this means for how you write one:** the goal is not "fetch the page". It
is "hand over prose and nothing else" — the story, in paragraphs, with its
dialogue marked, its title said once, and no part of the website attached. A
script that does that is short. A script that fetches a container and returns it
is not a crawler, it is a way of generating 8,000-character lines that pass every
check this program has.

And the payoff runs the other way too: **the cleaner the input, the more the
digest's refusals mean "the model got this wrong" rather than "the input was
junk".** Every check in section 4 exists to catch a model disagreeing with the
text it was given. None of them can catch text that never offered a speaker to
disagree with. That is why the work is worth doing properly here rather than at
the far end of the pipeline.

## For people changing the machine

Stage 1 used to be Rust: expand `url_template`, GET it, run the Storya/LN
heuristics. That covered exactly one shape of site. Real ones come in two:

1. **The URL is a function of the chapter number** — `…/chuong-34`,
   `…/chapter-034`. Computable without touching the site.
2. **The URL is a slug** — `/truyen/x/dao-phay`. Not a function of `n` at all:
   something has to read an index page and follow links.

So crawling is now a **script** the operator supplies, and the rules the old
Rust crawler hardcoded now live in the bundled `assets/crawl/templates/storya.lua`: which
element holds the prose, where the body starts, which lines are the site's
chrome. Rust's part of this is "run `crawl`, take the text" — nothing else.
Nothing about the pipeline downstream changed either: the invariant
`data/chapters/chNN.txt` exists before the digest runs is what every other
stage is built on, and it still holds.

**A new workspace defaults to `crawl.mode: "manual"`** — nothing fetches until
the workspace names a crawler (or a selector for the built-in fetcher) and sets
`mode: "script"`. Chapters can always arrive by hand instead (`:import`, §6),
and §4 is the short crawler-writing guideline — written so it can be pasted
into an AI chat alongside a saved page. Workspaces whose settings predate the
whole `crawl` block keep the bundled scripted crawler, byte-identically: that
migration is the identity.

That the port is a *port* rather than a rewrite is checked, not claimed.
`rust/fixtures/crawl/` keeps the pages the Rust extractor was tested against
beside its byte-exact output for each, and a test runs both bundled crawlers
over all of them and demands the same bytes back.

---

## 1. The contract

Two function names, one request object in, one response object out. Lua and
JavaScript run the same contract — the file extension picks the engine
(`.js`/`.mjs` → QuickJS, anything else → Lua), and every host function means the
same thing in both.

```lua
function crawl(input)            -- required: one chapter, one invocation
  local r = fetch(input.url)
  return { text = select_text(r.body, "div.text-left"), url = r.url }
end

function discover(input)         -- optional: the n -> url mapping, once per range
  return { chapters = { { n = 34, url = "…" } }, total = 380 }
end
```

### `crawl(input)`

| input | meaning |
| --- | --- |
| `n` | the pipeline's chapter index — dense, 1-based, the number `chNN.txt` is named after. **Not** necessarily the site's chapter number; `discover` absorbs that difference |
| `url` | the URL from the chapter index, already substituted. `nil` when nothing computed one |
| `params` | the workspace's `crawl.params`, passed through verbatim and **opaque to the host**. A new site should be zero Rust changes |
| `attempt` | 1-based retry count, so a script can try a mirror on the second go |

Exactly one of three responses:

```lua
return { text = "Chương 34: …", url = "…" }              -- the chapter
return { none = true, reason = "site ends at ch380" }     -- terminal, NOT a failure
return { blocked = { class = "rate_limit",              -- the site refused, classified
                     detail = "HTTP 429", retry_after = 30 } }
```

The classes are `challenge`, `rate_limit`, `login_required`, `js_required`,
`gone`, `empty`, `unknown`. **`rate_limit`, `challenge`, `empty` and `unknown`
take the ordinary strike ladder; the rest shelve immediately** with the class
and detail on the ledger row. Three attempts at a login wall prove nothing a
single one did not.

`none` is a *success*: the crawl and digest rows close Done, strike-free, with
the reason on the row. A range that runs past the end of a book must not shelve
its tail.

`error(...)` / `throw` is a genuine failure — a broken crawler, not a blocked
one — and takes the ladder to three strikes.

### `discover(input)`

Optional. Detected by its presence, never configured. Gets `{ params, start,
count }` — a bounded range, so a pure mapping never has to walk from chapter 1 —
and returns:

```lua
return { chapters = { { n = 34, url = "…", title = "Chương 34: …" }, … },
         total = 380 }
```

* Omitting `n` is **silence**, not absence: the crawl is still attempted.
* `absent = true` on an entry is absence: the row closes without a fetch.
* `total` trims a range that runs past the end of the book.
* `title` is **display only**: it is written beside the URL in
  `data/crawl-index.json`, so the file reads like the site's own listing. A
  chapter's title stays the first line of its text, or the digest's own `title`
  — never the manifest.

`discover` runs **once per range on the inductor**, not once per chapter on a
worker: the mapping decides which tasks exist, which is scheduling.

---

## 2. The chapter index

`discover`'s output — or the built-in template expansion — is frozen into
`data/crawl-index.json`:

```json
{ "source": "template", "hash": "…", "start": 1, "count": 380,
  "chapters": { "34": { "url": "https://…/chuong-34", "title": "" } } }
```

Three ways it gets filled, one shape:

| source | filled by |
| --- | --- |
| `template` | `url_template`, expanded by the host: `{n}`, or `{n:03}` when the site zero-pads |
| `script` | the crawler's `discover()` |
| `hand` | you, editing the file. The escape hatch for arbitrary slugs — generating that mapping once beats re-deriving it every run, and it is the only form that survives a script change |

Two properties worth knowing:

* **A generated index is reused, not rebuilt.** A bot hitting the site once per
  run is a bot that gets banned; the fingerprint (engine + script + params +
  template + range) is what decides whether a rebuild is needed.
* **A frozen index is what makes pagination safe.** Append a chapter to a
  paginated listing and every `n` after it re-maps to its neighbour. Per-chapter
  discovery would silently crawl the wrong chapter for half a book; a frozen
  index turns that into a decision about the index rather than a corrupted run.
* **It rebuilds itself whenever anything visible changed**, and there is nothing
  to press for that: the fingerprint covers the engine, the script, the params,
  the template and the range, so editing a crawler invalidates it on the next
  `:crawl` or `:translate`. The one case it cannot see is a listing the *site*
  repaginated under an unchanged config — then delete `data/crawl-index.json`
  and the next command rebuilds it.

---

## 3. The host ABI

A script's whole world. There is no environment, no filesystem, no shell — Lua's
`io`, `os`, `package`, `require`, `dofile` and `loadfile` are removed, and there
is a test asserting they stay removed.

```
fetch(url, opts?)        -> { status, body, url, headers }  the only way out
challenge(page)          -> string | nil              a Cloudflare interstitial, or nil
select(html, sel)        -> string                    first match, squeezed to one line
select_all(html, sel)    -> [ { text, html, attrs } ] attributes included
select_text(html, sel)   -> string                    first match, as prose
strip_tags(html)         -> string
decode_entities(s)       -> string
sanitize(text)           -> string                    the chapter boundary
readable(html)           -> { title, text }           generic prose heuristic
abs_url(base, href)      -> string
chapter_url(template, n) -> string                    the host's own {n}/{n:03} expansion
log(msg)                 -> a line on the ledger row
```

`fetch` does **not** fail on a non-2xx status: 429 and 403 are exactly what a
script classifies, and an exception would throw that information away. It is
re-entrant — a listing walk makes as many calls as it needs, under one budget.
Its `headers` (names lowercased) are there because a status alone does not say
what went wrong: `cf-mitigated: challenge` is the difference between a bot check
and a dead link, and a crawler that has to guess retries the wrong thing three
times.

**`challenge(page)` is the one piece of site-shaped knowledge in the ABI, and it
earns its place by being a refusal rather than an extraction.** Cloudflare
serves its managed challenge as `403` *and* as `200 OK` with an interstitial
body, and the second one defeats every status check: a crawler reads `200`,
finds no chapter, and stores the bot check as `chNN.txt`. So `challenge(page)`
reads the `cf-mitigated` header when there is one and recognises the
interstitial body when there is not, and returns a string or `nil`:

```lua
local why = challenge(r)
if why then return { blocked = { class = "challenge", detail = why } } end
```

It is in the host rather than in each template because that is the only way no
template can forget it — the webnovel one did, and a test caught it storing a
challenge page. Two details it gets right that are easy to get wrong: markers
inside an **HTML comment** do not count (a page that merely mentions
`cf-mitigated` is a page), while **script bodies are not stripped** (that is
where `cf_chl_opt` lives). In Lua the "no challenge" answer is a real `nil`, not
mlua's `NULL` sentinel — that sentinel is a userdata, and userdata is **truthy**,
so a `NULL` return would make the guard above refuse every page it was given.

`readable` is the "I have not read this site's markup yet" path: it scores
candidate containers by text minus link text and returns the best. It is a
heuristic and labelled as one; a script that has looked at the page points
`select_text` at the container instead.

**There is no `clean_storya`, and that is the point.** The host offers
primitives — a CSS engine, a tag stripper, an entity decoder, a link resolver —
and nothing that knows what a chapter of *one particular site* looks like.
Which element holds the prose, which lines are navigation, which paragraph
begins the body: those are facts about a website, so they live in the script, in
one table at the top of `storya.lua`, where the operator who can read the page
will find them. A host function that already knows your site is exactly the
hardcoding this replaces.

What the host does insist on is the *shape*. `sanitize` — site metadata out,
entities decoded — runs over whatever a script returns, and the length guard
refuses a body too short to be a chapter, so a chapter is a chapter however it
was obtained and "the selector missed" fails at the crawl rather than three
stages downstream.

`select_text` is the one to reach for first: point it at a container and get
prose back, paragraphs separated by blank lines. `select` answers the same
question on a single line, which is right for a headline or a `next` link and
wrong for a body — squeezing is precisely what destroys a paragraph break, and
one paragraph per chapter is what the TTS would then read.

---

## 4. Writing a crawler — the short guideline

This section is written to be **pasted into an AI chat** together with one or
two saved chapter pages (`curl -A "Mozilla/5.0" -o ch1.html <url>`) and, for a
listing site, the index page. An agent that can read HTML can produce a working
crawler from it in one pass; a human can read it in three minutes.

> I need a crawler for a web-novel site. Write it as a **Lua** (or **JavaScript**)
> script for the pipeline documented below. The script must define
> `function crawl(input)` and return exactly one of:
> `{ text = "chapter prose", url = "…" }` — the chapter;
> `{ none = true, reason = "why" }` — the chapter does not exist (terminal, not
> an error); or
> `{ blocked = { class = "rate_limit" | "challenge" | "login_required" |
> "js_required" | "gone" | "empty" | "unknown", detail = "…", retry_after = n? } }`
> — the site refused. `input` is `{ n, url, params, attempt }`; `url` comes from
> the chapter index and may be `nil`.
> If chapter URLs are not computable from the number (slugs), also define
> `function discover(input)` receiving `{ params, start, count }` and returning
> `{ chapters = { { n = 1, url = "…", title? = "…", absent? = true }, … },
> total? = n }`.
> Only these host functions exist — no `io`, `os`, `require`, no network beyond
> `fetch`: `fetch(url)` → `{ status, body, url }` (does **not** throw on 4xx/5xx;
> classify it yourself); `select(html, css)` → first match squeezed to one line;
> `select_text(html, css)` → first match **as prose with paragraph breaks** (use
> this for the chapter body); `select_all(html, css)` → array of
> `{ text, html, attrs }`; `strip_tags`, `decode_entities`, `sanitize` (chapter
> boundary — the host runs it on your return value anyway); `readable(html)` →
> `{ title, text }` generic heuristic; `abs_url(base, href)`; `chapter_url(tpl,
> n)`; `log(msg)`.
> Requirements: 404/410 → `{ none = true }`; 429 → blocked `rate_limit` with
> `retry_after`; 403 → blocked `challenge`; return the **chapter body only** —
> no site navigation, no comments, no next/prev links, no duplicated headline;
> keep one blank line between paragraphs; never concatenate the whole page.
> Investigate the attached HTML and name the CSS selector(s) of the real
> chapter container in a comment at the top of the script.

Then tune by evidence, not by reading code: run it, and let the outputs argue.

* `bm-agent run --stage crawl --chapter 1` (or `:crawl` in the TUI) prints the
  verdict and every `log()` line. A *short* text is the length guard telling you
  the selector missed; "matched the whole page" means it matched too much.
* The first line of the returned text becomes the spoken headline. If the site's
  chapter list row (`34. Chương 34: …`) sits above the real `<h1>`, drop the
  numbered copy in the script — the host's boundary drops the common shapes, but
  your page may have a fifth.
* Save the wrong page beside `rust/fixtures/crawl/` and diff: the failure is a
  selector, not a mystery.

Put the file in the workspace — `workspaces/<name>/crawl/mysite.lua` — not in
`assets/crawl/`: it is then per-book, survives profile switches, syncs to every
worker with the next provision, and drifts the provision stamp when edited.
Point `crawl.script` at it (`"script": "crawl/mysite.lua"`) and set
`"mode": "script"` — **manual is the default mode**, so nothing fetches until a
workspace asks for it.

---

## 5. The bundled crawlers

| file | what it is |
| --- | --- |
| `assets/crawl/templates/storya.lua` | the crawler the pipeline shipped before scripted crawls existed, kept for the migration: the old Rust crawl, moved into a script. Fetch `input.url`, lift the body out of the page with the rules in its `SITE` table, classify the status. **Not a default** — a new workspace names no crawler at all; this is what a pre-`crawl` `settings.json` still points at |
| `assets/crawl/templates/storya.js` | the same crawler in JavaScript — the second engine as a working example, not a claim |
| `assets/crawl/templates/madara.lua` | a **listing** site: `discover` walks the index (paginated, `next` link), `crawl` extracts with selectors and falls back to `readable()` |
| `assets/crawl/templates/truyencom.lua` | the **easy** shape: the chapter URL is a function of `n`, so a `url_template` is the whole crawler. Read this one first |
| `assets/crawl/templates/readnovelfull.lua` | the **slug** shape: the number *is* in the URL but is not the last thing, so nothing can template it — and the book's own index stops at 30 chapters, so `discover` walks the `next_chap` chain |
| `assets/crawl/templates/webnovel.lua` | the **hard** shape: slug URLs, the container one level deeper than the obvious one, a paid-chapter flag, and a site behind a bot check |

### The three worked examples

The last three templates are not illustrations. Each is written against a page
captured from the live site, and each is tested against that capture in
`rust/fixtures/crawl/` — `truyencom-chapter.{html,txt}`,
`readnovelfull-chapter.{html,txt}`, `webnovel-chapter.{html,txt}`, plus the
listing pages — so "the template works" is checked against a real page rather
than against the author's idea of one. `cloudflare-403.html` is a real
challenge, kept as served.

`truyencom.lua` is the case worth reading closely, because its body is **plain
text with no `<p>` in it at all**: paragraphs are separated by `&#13; &#13;`,
two carriage returns, inside one text node. The chapter boundary splits on
newlines, and a lone CR is not a newline to it — so a crawler that just selects
the container returns the whole chapter as one 8,000-character line. Technically
a crawl, one breath, and a digest that cannot find a sentence in it. The split
happens *in the script*, next to the site knowledge that makes it necessary,
which is the whole division of labour between the two halves of this system.

Its listing is also paginated — 50 chapters a page, five pages, 224 chapters —
and that is a lesson of its own. A `discover` that reads page 1 and stops
reports `total = 50`, the host believes it, and every chapter past 50 is marked
**absent**, which is terminal. A long book silently becomes a short one and
every ledger row looks healthy. `total` must be the last chapter of the *book*.

`readnovelfull.lua` is the awkward middle, and it is the one worth having a
template for. Its chapter URL is `/{book}/chapter-{n}-{title-slug}.html`: the
number is right there, and it is useless, because no `{n}` expansion can invent
the title slug. So the URL cannot be templated at all. Worse, the book's own
index page is worse than no index — it lists the **first 30 chapters and stops**,
with no pagination, no "view all", and a 404 on every attempt at one; a
2730-chapter book lists chapters 2..30 and pretends. The only complete index is
the chain of `next_chap` links on the chapter pages themselves, and that is what
`discover` walks.

Which is also what makes its `total` honest. `total` is what the host uses to
mark a range past the end of a book **absent** — a terminal verdict — so it may
only be reported when the chain actually reached a chapter with no `next_chap`
at all. Stopping because the range was satisfied, or because a fetch failed, or
because the hop ceiling bit, is not the same thing and must not be reported as
one. Report the last chapter you happened to reach and a 2730-chapter novel
becomes a 30-chapter one: the host believes the number, marks 31..2730 absent,
never crawls them, and every ledger row looks healthy.

The template also cuts a quirk: ReadNovelFull writes the chapter title into
the **body**, twice, glued to the front of the first sentence, while the
`<h2>` headline carries the same text without the colon. The duplication is
lucky — the prose is exactly what follows the second copy, so the cut is exact
rather than guessed — but leave it in and every chapter opens with its own
title three times over.

`webnovel.lua` teaches the other half. The container everyone reaches for,
`div.chapter_content`, is not the chapter: it also holds the book cover, the
title, `Tác giả:` and a `© WebNovel` line, and taking it gives you a chapter
that opens with the author's name. The prose is one level deeper. It also has
`data-islock="1"` for a paid chapter whose text is not served at all — saying so
is the difference between a chapter that shelves and a cluster that spends an
afternoon on it.

**And its site refuses this crawler.** Not the markup — the client. The same
pages `curl --http1.1` can read answer `403 cf-mitigated: challenge` to this
pipeline. There is no TLS-fingerprint spoofing, no browser engine and no
challenge solver here, so the only route is a `cf_clearance` cookie obtained in
a real browser and pasted into `crawl.headers`. Keep the template for its
structure; expect to replace the selectors.

### Sites that work, as of 2026-09-25

Every row below is a **`bm-inductor check` on an actual chapter URL**, not a
homepage fetch — a site whose front page answers 200 can still serve its
chapters behind a bot check, and `truyenfull.vn` is exactly that. Re-run the
check before you rely on any of them; bot policies move.

| site | a chapter | template | note |
| --- | --- | --- | --- |
| **`storya.click`** | ✅ 11.8 KB, 117 ¶ | `storya.lua` | the site the pipeline was built for. Live and unchallenged. **Do not confuse it with `storya.vn`, which is NXDOMAIN** |
| `truyencom.com` | ✅ 13.4 KB, 77 ¶ | `templates/truyencom.lua` | the easy shape; no bot check |
| `readnovelfull.com` | ✅ 8.8 KB, 154 ¶ | `templates/readnovelfull.lua` | works; the URL carries a title slug, so `discover` walks the `next_chap` chain instead |
| `truyenfull.vn` | ❌ 200 interstitial | *none* | redirects to `truyenfull.live`, which serves a Cloudflare interstitial **as 200** — the case a status check cannot see |
| `lightnovel.vn` | ❌ 22 bytes | *none* | a Next.js SPA; the reader is `hub.lightnovel.vn/reader?book=…` and the text arrives via JavaScript. `js_required`, and nothing here runs JavaScript |
| `novelfull.com` | ❌ 403 | *none* | Cloudflare on the front page too |
| `truyenthanh.vn` | ❌ 500 | *none* | upstream broken |
| `webnovel.com` | ❌ 403 | `templates/webnovel.lua` | Cloudflare; cookie-only. The template is verified against captured pages and **cannot be run** |

**A note on the Storya domain, because it is an easy mistake.** `storya.vn` no
longer resolves — but the site has not gone anywhere: it is at **`storya.click`**,
which is what the `beyond-myriads` workspace's `url_template` has always
pointed at, and it serves chapters with no bot check:

```json
"url_template": "https://storya.click/truyen/nguoi-tren-van-nguoi/chuong-{n}"
```

So the bundled `storya.lua` is not a historical artefact. It is the crawler for
a site that still answers, and it is parity-tested against the Rust extractor it
replaced. There is no migration to do.

The `truyenfull.vn` row is the argument for the check in one line. Its chapter
URL answers **HTTP 200** and redirects, and the body is a Cloudflare
interstitial. A crawler that reads the status sees success; a check says
`HTTP 200 but the body is a Cloudflare interstitial` in one request. The
alternative is finding out when a digest fails on chapter 53.

### Retargeting the bundled Storya crawler

`storya.lua` opens with everything that is true about Storya and nothing else:
the container order (`article`, `main`, `body`), the lines that start a chapter,
the trigger lines for a page that names the book instead, the stop markers, the
byline, the junk, and the length at which a line stops being a label and starts
being prose. Copy the file, edit that table, point `crawl.script` at the copy:
no Rust, no rebuild — and the provision stamp moves, so every worker picks the
new crawler up.

Start from the template and copy it where the book can own it — a workspace
directory, not the shared profile tree (see the next section):

```bash
cp assets/crawl/templates/madara.lua workspaces/<name>/crawl/mysite.lua
```

```json
"crawl": {
  "script": "crawl/mysite.lua",
  "params": { "entry": "https://site.example/truyen/ten-truyen" }
}
```

`params.entry` is the whole site-specific configuration in that case; the
selectors live at the top of the script, in one table.

### Per-workspace crawlers

`assets/crawl/` is the **profile's**: every workspace on the root sees it, and
`:profile load` replaces it wholesale — a crawler edited there is one profile
switch away from vanishing. A book whose site needs its own crawler therefore
lives in the workspace instead:

```
workspaces/<name>/crawl/mysite.lua
```

Resolution order for `crawl.script`, first existing file wins:

1. `workspaces/<active>/crawl/<name>` — this book's own crawler;
2. `<name>` from the root — so `assets/crawl/…` still names the profile's;
3. `<name>` from `assets/`.

A same-named file in the workspace **shadows** the profile's, so one workspace
can retarget `crawl/site.lua` for its own site while the rest of the cluster
keeps the shipped crawler — and switching workspaces switches crawlers with no
edit at all, because each workspace's directory is searched first.

**It reaches the machines.** Provisioning rsyncs `workspaces/<active>/crawl/`
to `~/bm-worker/crawl` on every worker with the rest of the sources, the
provision stamp hashes its contents, and a removed directory is cleared from
the boxes it had reached. Edit the script, run `:prov` (or wait for the next
one), and the cluster crawls through the new bytes — the same guarantee the
profile's `assets/crawl/` has always had.

Without an active workspace (legacy mode) the root *is* the workspace, so its
directory is `<root>/crawl/` — the same path on both sides.

### Workspace settings

`workspaces/<name>/settings.json`:

| field | default | meaning |
| --- | --- | --- |
| `crawl.mode` | **`manual`** | `manual` never fetches — chapters arrive by `:import`; `script` fetches through `crawl.script`. **The default is manual**: a workspace that wants the automatic path says so |
| `crawl.script` | *(empty)* | only read in `script` mode. Resolved against the workspace's `crawl/`, then the root, then `assets/`. Empty → the built-in fetcher |
| `crawl.params` | `{}` | passed to the script verbatim. The host reads one key out of it, `extract`, and only when there is no script to read |
| `crawl.headers` | `{}` | extra headers on every fetch (a `Referer`, a pasted session cookie). `cf_clearance` goes here |
| `crawl.user_agent` | `Mozilla/5.0` | the default is thin. A site with any bot scoring at all wants a real browser string here — it is the cheapest thing to change and the first thing to try |
| `crawl.pace_ms` | `750` | minimum gap between two fetches of one host. `0` disables |
| `crawl.timeout_secs` | `60` | per request |
| `crawl.max_seconds` | `180` | per chapter, interpreter included |
| `crawl.max_fetches` | `64` | round trips per chapter |

`url_template` is unchanged and still the built-in mapping. **A workspace that
predates the whole `crawl` block needs no edit**: an absent block means an *old*
workspace, which keeps `mode: script` and the bundled Storya crawler — the same
URL the old Rust path expanded, asserted byte-identical by the fixture test. The
manual default applies to workspaces created now, whose settings name no site
yet; the wrong default there would fetch *something* the first time `:translate`
ran, and there is no site it could have been the right thing for.

`crawl.params` is opaque to the host with exactly **one** exception, and it is
for the workspace with no script at all:

```json
"crawl": { "mode": "script", "script": "", "params": { "extract": { "selector": "div.text-left" } } }
```

`extract` — a selector, a list of them, or an object carrying a `selector` of
either shape — is what the built-in fetcher points at. That fetcher is the
fallback for a script that cannot be read at all; it fetches the URL and takes
the text of the element named, or falls back to `readable()`'s guess when no
selector matches, saying so on the ledger row. An invalid selector is a
configuration error and fails named rather than guessing. A script never sees
the key unless it looks for it.

---

## 6. Manual import

For a chapter a script cannot fetch — a site behind a login, a page that moved,
a chapter you already have.

```
:import 34 /tmp/ch34.txt        # number + file
:import /tmp/ch217.txt          # number from the filename
```

* The number comes from **you or the filename**, never from the order things
  arrived in: `ch34.txt`, `34.txt`, `chapter-034.txt`, `034 - Tên chương.txt`
  all work, and a file with no number is refused rather than guessed at.
* The text goes through the **same boundary a crawl does** — site metadata out,
  entities decoded, anything under 200 bytes refused — so a truncated file fails
  during the import, named, instead of at the digest three stages later.
* A whole batch validates before any of it lands: a three-file import that fails
  on the second writes nothing.
* Importing over a chapter that is already digested is refused (it would leave
  the old script describing the new text). Remove `data/script-NN.json` first.
* An import **is** the crawl: the crawl row closes Done and a shelved digest is
  queued again. So one broken page in a 500-chapter book is one `:import`, and
  nothing else about the run changes.

**A caveat about drag-and-drop.** Most terminals never deliver an OS drop event:
they paste the dropped file's *path*. That is why the prompt takes a path (and
the API takes literal text too), and why a whole chapter pasted into a
single-line prompt is not supported — it would submit on the first newline. Save
it to a file, or post the text to `/api/op` (`op: "import"`, `chapter`, `paths`).

`crawl.mode: "manual"` makes the whole workspace operator-supplied: `:translate`
queues no crawl at all — a task no worker can run is a row that fails three times
and shelves — and the event log names the chapters waiting for `:import`.

---

## 7. Limits, pacing and safety

* **Budget.** Every chapter has a wall-clock budget (`max_seconds`) and a fetch
  cap, enforced by the host: Lua gets an instruction hook, QuickJS an interrupt
  handler, and every host call checks the clock. A runaway `next` loop costs one
  task, not a worker.
* **Pacing.** The fetch itself is spaced per host, `crawl.pace_ms` apart,
  process-wide. A cluster pointing ten workers at one site is what gets a novel
  scraper banned, and rotating addresses does not help — the new one is throttled
  the same, because the pacing was the problem. (What this does *not* do is
  coordinate across boxes; that needs the inductor to hold a per-host token when
  it offers a task.)
* **Charset.** Bodies are decoded UTF-8 first, then by the declared charset, then
  by the document's own `<meta charset>`. A GBK page decoded as UTF-8 is not
  "slightly wrong" — it is unreadable prose that passes a length guard and fails
  every stage after it.
* **Trust.** Scripts run in-process with the worker's privileges: this is a
  *boundary* that makes the documented ABI the true one, not a sandbox for
  hostile code. A crawler is profile content the operator authors — like
  `prompts/`, it ships in the profile, is rsynced to every worker, and drifts the
  provision stamp when it changes. Loading a stranger's crawler without asking is
  what WASM would be for, later.

---

## 8. Trying it before you spend a run

### Check the link first

```
bm-inductor check https://site.example/truyen/x/chuong-1.html
```

One request, and a verdict on whether a crawl of that page would produce a
chapter. **Do this before you write any settings**, because the alternative is
arithmetic you do not want: a cluster does not notice that a site is refusing
it, it notices one worker at a time, sixty seconds apart, for an afternoon. Ten
workers × three strikes × a 60s timeout before the first honest signal, which is
a ledger full of identical rows.

```
$ bm-inductor check https://truyencom.com/nga-thi-.../chuong-1.html
  HTTP 200  ·  61002 bytes  ·  13438 bytes of prose
  title: Chương 1: Thần giếng Khâu Bình
  ok: 13438 bytes of prose in 77 paragraph(s) under "Chương 1: …"
```

It reads the active workspace's `crawl.user_agent` and `crawl.headers`, so a
check goes out exactly as a real crawl would — a session cookie you are relying
on is part of what is being confirmed, and this is the command that tells you
whether it still works. It exits non-zero when the page is not a chapter, so a
setup script can gate on it, and it writes nothing.

What it cannot do is fix anything. It reports; a person decides. On a Cloudflare
403 it says so by name and stops, because the plausible-sounding wrong answer
here costs an afternoon.

### If the URL is one we already have a crawler for

`check` also looks the host up in the **known-sites registry**
(`bm_core::crawl::known`, `rust/crates/bm-core/src/crawl/known.rs`) and prints
the crawler written for it, with a settings block ready to paste:

```
$ bm-inductor check https://readnovelfull.com/the-sword-god-of-the-universe.html
https://readnovelfull.com/the-sword-god-of-the-universe.html
  HTTP 200  ·  35680 bytes  ·  4312 bytes of prose
  title: The Sword God of the Universe
  ok: 4312 bytes of prose in 159 paragraph(s) under "The Sword God of the Universe"

  known site: readnovelfull.com
    crawler: assets/crawl/templates/readnovelfull.lua
    shape:   `/{book}/chapter-{n}-{title-slug}.html` — the number is in the
                URL but not last, so nothing can invent the slug. The book
                page also lists only the first ~30 chapters with no
                pagination, so `discover` walks the `next_chap` chain instead.
    paste into this workspace's settings.json:
        "url_template": "",
        "crawl": {
          "mode": "script",
          "script": "assets/crawl/templates/readnovelfull.lua",
          "params": {
            "book": "https://readnovelfull.com/the-sword-god-of-the-universe.html"
          },
          "max_fetches": 400,
          "max_seconds": 900,
          "pace_ms": 750
        }
```

The block is **generated** from the registry rather than stored beside it, so it
cannot drift from the `CrawlSettings` it has to satisfy; a test parses every
one of them back.

The **TUI says the same thing while you type**. `:crawl` on a URL we recognise
shows the host, the crawler and the one fact that decides what you do next,
above the input line:

```
known site · readnovelfull.com · crawler assets/crawl/templates/readnovelfull.lua · no {n} in its URLs: submit empty and set crawl.script
```

and pressing Enter on a URL with no `{n}` in it — which the prompt otherwise
refuses, correctly — names the crawler and the way through instead of saying
`must contain {n}` a second time:

```
readnovelfull.com is known: its crawler is assets/crawl/templates/readnovelfull.lua — its
chapter URLs carry a title slug, so there is no chapter-number template to write. Submit an
empty line to probe, then set "crawl"."script" to that path (and "crawl"."params"."book" to
this URL).
```

**Sites that are blocked are in the registry too, and that is the point of
them.** `novelfull.com` says "Cloudflare: 403 to every request"; `lightnovel.vn`
says the text arrives via JavaScript; `truyenfull.vn` says the challenge is
served *as 200*. An entry that says only "no crawler" is a dead end; one that
says why sends the reader away on purpose, and stops the same afternoon being
spent twice.

The registry is a list, not a resolver: it never decides anything, and a site
that is missing from it is not a site that cannot be crawled — it is a site
nobody has written down yet. Add one with `check` on a real chapter URL, a
template copied from `truyencom.lua`, and a captured page in
`rust/fixtures/crawl/`.

### Then the in-app probe

```
:translate 1 50     # builds the index for the range, then queues crawl + digest
:crawl              # saves the URL template (if you type one), probes one chapter
```

`:crawl` probes through **the same provider a worker uses** — index first, so a
script's `discover()` has its say — and reports the verdict it reached: bytes and
the headline for a chapter, the class for a block, the reason for an absence.
Errors name the script file, the function, and what it returned.

Leave the line **empty** to probe without saving a template. That is what a
slug-site crawler needs: there is no chapter-number URL to save, `discover()`
builds the index, and the headline the probe prints is how you check that it
built the one you meant.

Debug from a shell the same way: `bm-agent run --root <root> --stage crawl
--chapter 34` runs one chapter standalone and prints the outcome — the verdict,
and every `log()` line the script wrote on the way.

`rust/fixtures/crawl/` is the other half of that: the pages the bundled
Storya crawler is tested against, and the bytes it must produce for each. Save
the page that came back wrong beside them, run the crawler over it, and the
difference is a selector rather than a mystery.
