# The guides

*You can stop reading after the first table.* Everything below this line is for
someone changing the machine or the art, not for someone who wants an audiobook.

## Start here

[README.md](../README.md) is the guide you want. It is the whole path from a
clone to `output/Ch.1.mp3`, in the order you do it: check the site, start the
book, take the files.

You are in the wrong place if all you want is audio.

## Which guide, for which job

| If you want to… | Read |
| --- | --- |
| Get an audiobook out of a book | [../README.md](../README.md) — nothing else first |
| Fix something that stopped | [TROUBLESHOOTING.md](TROUBLESHOOTING.md) — symptom to fix, in the order you hit it |
| Understand why it is built this way | [ARCHITECTURE.md](ARCHITECTURE.md) — the four stages, then the code |
| Fetch chapters from a website | [CRAWLING.md](CRAWLING.md) — §1 is the contract, §4 is the brief you hand an AI |
| Use another language or genre | [PROFILES.md](PROFILES.md) — a profile is a pack × adapter × engine |
| Make it faster with rented machines | [AWS-WORKERS.md](AWS-WORKERS.md), then [AWS-IAM-USER.md](AWS-IAM-USER.md) |
| Set the machines up on AWS | [AWS-IAM-USER.md](AWS-IAM-USER.md) — the console clicks, once per account |
| Understand the AWS identity | [AWS-CREDENTIALS.md](AWS-CREDENTIALS.md) — what it is, where it lives, what it may do |
| Record or generate sound | [SOUND.md](SOUND.md) — the studio reference, written for someone holding a microphone |
| Understand how sound is packaged | [ASSETS.md](ASSETS.md) — a dependency tree of art, released per piece |
| Build a sound pack | [ASSET-PACKS.md](ASSET-PACKS.md) is the recipe; [COMPLETING-A-PACK.md](COMPLETING-A-PACK.md) is the runbook for the two in progress |
| Know which sounds a pack still needs | [AUDIO-NEEDS.md](AUDIO-NEEDS.md) — places, moments, tracks, and what is missing |
| Shrink what provisioning sends | [ARTIFACTS.md](ARTIFACTS.md) — why a new worker needs about 886 MB, and how to make it less |
| See what is being built next | [ROADMAP.md](ROADMAP.md) |

## The order things go wrong in

Worth knowing before you need it, because the order is the order you will meet
them.

1. **The site refuses you.** Most common by a long way, and the one thing a
   group of machines cannot tell you. Run `bm-inductor check <url>` first,
   every time.
2. **The chapter has no quote marks**, so everything is narration and the book
   comes out in one voice. That is the crawler, not the AI.
3. **A box looks healthy and is never given work**, because the task port is
   closed while port 22 is open.
4. **A character is the wrong gender, or is three people.** The digest invents
   the cast; audit the bible while the book is small.

Each has a fix, and the fix is written down. None of them is a mystery, but all
four fail quietly, which is why they are worth reading about before they happen
rather than after.

## How these are written

Every guide opens with an **In plain words** section and a line telling you
where you may stop reading. That part is for anyone. The rest is for whoever is
changing the thing, and it is written against code that exists in this repo,
not against a plan.

Where a guide has to be exhaustive to be useful, it is: [ARCHITECTURE.md](ARCHITECTURE.md)
is 1,600 lines and [CRAWLING.md](CRAWLING.md) is 870. That is deliberate and it
is not a first read.
