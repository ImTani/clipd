# Name Claiming & Go-To-Market Landscape

For the ORCHESTRATOR, not the coding agent. Two parts: (1) claiming the name,
(2) the distribution map for turning FOSS software into a product people find.
FOSS forever is the constraint AND the marketing strategy throughout.

---

## Part 1 — Name status and claiming

### 1.1 Zanshin collision report (as of 2026-07)
Known same-name software: KDE's Zanshin (GPL to-do app, packaged in Linux
distros — occupies the `zanshin` package/binary name in the FOSS namespace),
zanshin.sh (free macOS media-navigation app — media-adjacent category),
Tenchi Security's "Zanshin" (commercial cyber-risk SaaS — the most likely
holder of registered software-class marks), Zanshin Software (Brazilian SaaS).
None in gaming/capture. Verdict: usable as a niche FOSS project name via
common-law distinctiveness in "game clipping software"; a registered Class
9/42 mark is uncertain and needs an attorney's search opinion if pursued.
Kairos carries the same shape of problem (facial-recognition company of that
name). Kiroku scanned cleanest of the liked candidates. Compound forms
("Zanshin Clip") strengthen registrability if the sound of Zanshin wins.

### 1.2 Verification you must do personally (30–60 min, before anything else)
- [ ] USPTO trademark search (tmsearch at uspto.gov): "zanshin", classes 9, 42
- [ ] IP India public search (ipindia.gov.in) — you are India-based; local
      registration is the cheapest and covers your home jurisdiction
- [ ] EUIPO eSearch if EU distribution matters
- [ ] Steam store search + SteamDB for the exact name
- [ ] crates.io, GitHub org availability, winget package ID search
- [ ] Say "I clipped it with ___" aloud; ask two gamer friends to spell it
      after hearing it once

### 1.3 The claim checklist (execute within one day of final decision — squat
speed matters more than order)
- [ ] GitHub ORGANIZATION (not just repo) — org name is the identity
- [ ] Domain: .dev or .app primary (HTTPS-forced TLDs suit the trust story),
      .gg if taste allows; set up a one-page site immediately (name, one-line
      pitch, GitHub link) so the domain indexes early
- [ ] crates.io: publish a stub 0.0.1 under the name (binary crate) — crates.io
      names are first-come and unreclaimable
- [ ] Handles: X/Twitter, Bluesky, YouTube channel, TikTok (clip culture lives
      there), Reddit username + create r/<name> as a parking sub
- [ ] Discord server (even empty — the invite link goes in the README day one)
- [ ] winget package identifier convention decided (Publisher.Name)
- [ ] Steam: reserve nothing yet (app credit costs $100 and reveals plans);
      claim at M10 when the depot script lands
- [ ] Document FIRST PUBLIC USE date (blog post, repo publication) — this
      timestamp is your common-law evidence; use ™ (not ®) beside the name
      from day one, which requires no registration anywhere
- [ ] Trademark registration: decision deferred until the project has users.
      If pursued: India first (cheap, home turf), US later; narrow goods
      description ("downloadable game recording and replay software"); use an
      attorney for the search opinion — this checklist is not legal advice
- [ ] LICENSE (GPL-3.0 per earlier decision) + a TRADEMARK.md stating the
      code is free but the name/logo identify official builds (the standard
      FOSS play — Firefox/Rust model — and what makes "FOSS forever" coexist
      with a protectable identity and a paid convenience build)

---

## Part 2 — Distribution landscape (who covers this software, where users are)

Organized by lane, with the trend logic for each. Handles/channel names are
research targets to verify freshness at launch time — media half-lives are
short; re-check anyone here before pitching.

### 2.1 YouTube — the primary discovery engine for this category

**Tier 1: the capture/streaming authority**
- EposVox ("The Stream Professor") — THE technical authority on capture,
  encoders, OBS, NVENC/AV1. Millions taught; deep benchmark-style reviews. A
  single honest EposVox deep-dive validates the project to the entire
  ecosystem. Pitch: not "cover my app" but "here's a Rust replay buffer with
  measured 99th-percentile frametime impact and an A/V drift test rig —
  break it." He responds to engineering rigor and will find real flaws:
  that's the value.

**Tier 2: Windows-utility and "debloat culture" channels (huge overlap with
your exact audience: people who hate the Nvidia app)**
- ThioJoe — Windows utilities/tips at scale; has made hits out of exactly
  this kind of single-purpose tool
- Chris Titus Tech — debloat/utility culture personified; FOSS-friendly;
  his audience is philosophically pre-sold on "no login, no telemetry"
- TroubleChute — gaming tools, setup guides, quick utility spotlights
- Britec09 — Windows tools/repair audience

**Tier 3: hardware/enthusiast megachannels (cover only when there's a hook:
launch, benchmark story, or drama-adjacent angle like "Nvidia app problems")**
- Gamers Nexus (loves measurement rigor + has covered recording overhead),
  Linus Tech Tips / ShortCircuit (utility spotlights), Hardware Unboxed
  (encoder quality angles), JayzTwoCents, Dawid Does Tech Stuff (quirky-tool
  features), Level1Techs (Wendell is genuinely FOSS-literate; forum too)

**Tier 4: OBS/creator-education ecosystem**
- Nutty (nuttylmao) — OBS tooling and clip-workflow content; the exact
  "ShadowPlay-ify OBS" audience your product obsoletes
- Gaming Careers, Alpha Gaming — creator-setup channels; care about
  reliability stories
- Smaller OBS-plugin/tooling channels: collectively large, individually
  approachable pre-launch

**Format note (trend-perceptive):** the winning artifact is not a trailer,
it is a 60–90 second screen-recorded demo — hotkey press, instant file,
task-manager side-by-side vs the Nvidia app — cut vertical for
Shorts/TikTok/Reels. Clip culture marketing for a clipping tool is
self-demonstrating: every demo teaches the product.

### 2.2 Reddit & forums (in rough launch order; READ each sub's self-promo
rules — FOSS + dev-post-mortem framing is welcome where ads are not)
- r/rust — "built a ShadowPlay alternative in Rust (WGC + Media Foundation,
  no FFmpeg)" is beloved content; expect code review as marketing
- Hacker News — Show HN. Rust + systems + anti-bloat + FOSS is HN catnip;
  a front-page Show HN is realistically your single biggest launch-day lever
- r/pcmasterrace — FOSS utility drops periodically go viral here; meme-literate
  framing ("Nvidia app 1.2 GB vs this 8 MB") outperforms earnest framing
- r/pcgaming — more editorial; works with a benchmark/reliability story
- r/software, r/opensource, r/windows — steady long-tail
- r/obs, r/Twitch, r/streaming — the workflow-refugee audience
- Game-specific clip-heavy subs (r/GlobalOffensive, r/VALORANT,
  r/apexlegends...) — do NOT self-post; these convert via organic mention and
  via creators. Seed by answering "what clipper should I use" threads only
  where genuinely on-topic, disclosure always
- Forums proper: Linus Tech Tips forum, Level1Techs forum, OBS forums
  (tread respectfully — adjacent project, not competitor spam), guru3d forums,
  ResetEra PC threads (high editorial sensitivity)
- lobste.rs (dev), This Week in Rust (submit the release post — standard and
  effective for Rust projects), awesome-rust list PR

### 2.3 Press & download portals ("papers")
- Enthusiast press that covers small tools: Tom's Hardware, PC Gamer
  (hardware/software desk), Rock Paper Shotgun (utility features), XDA
  (now heavily Windows-apps focused), How-To Geek, Windows Central, Neowin,
  PCWorld. Pitch = story, not product: "solo dev replaces Nvidia's clipper
  with an 8 MB open-source tool" / "why your clips desync — and the fix"
- Download portals that still drive real traffic for utilities: TechPowerUp
  downloads (submit; their audience is exactly GPU-utility users), Guru3D
  downloads, MajorGeeks, Softpedia; AlternativeTo listing (create the entry,
  seed accurate alternative-to links: ShadowPlay, Medal, Outplayed, RePlays)
- Newsletters/aggregators: This Week in Rust (again), Console/oss newsletters
  featuring FOSS projects, Hacker Newsletter (rides HN success)
- Academic "papers" are not a channel here; the equivalent credibility asset
  is a technical blog post (the A/V sync spec makes an excellent public
  write-up — engineering posts recruit both users and contributors)

### 2.4 Trend map — position WITH these currents (2025–2026)
1. Nvidia-app resentment: login walls, telemetry, config resets — your origin
   story. Never bash; measure. Side-by-side numbers are the polite knife.
2. Debloat culture: single-purpose native tools as identity. "8 MB, no
   installer required, no account" is a headline, not a footnote.
3. Rust halo: "rewritten in Rust" retains meme-level goodwill in dev spaces
   and increasingly signals quality to enthusiast users.
4. Lossless Scaling precedent: Steam buyers pay single-digit dollars for
   one-job GPU utilities that just work. Your paid-convenience build rides a
   proven lane.
5. Anti-cheat anxiety: "no injection, no hooks, no overlay" is a safety claim
   competitors cannot all make. State it plainly and early.
6. Local-first/privacy: clips never leave the machine; contrast with
   cloud-watermark models without naming names in official copy.
7. Clip-culture growth: Shorts/TikTok demand means MORE people need a
   clipper than ever; the tool that is invisible until the hotkey wins.
8. AI fatigue pocket: "no AI highlights, it saves what YOU choose" lands as a
   feature with exactly this audience.

### 2.5 Launch sequence (the conversion plan)
- Phase 0 (pre-launch): claim checklist done; landing page; 20-user quiet
  beta recruited from r/obs + one Discord; collect the reliability
  testimonials ("survived a driver crash mid-match") — reliability stories
  are this product's reviews.
- Phase 1 (dev-cred launch): technical blog post (A/V sync write-up) + Show
  HN + r/rust + This Week in Rust, same week. Goal: stars, contributors,
  and the "impressive engineering" halo that later pitches cite.
- Phase 2 (user launch): 90-second demo video; ThioJoe/TroubleChute/Chris
  Titus tier pitches; r/pcmasterrace post; AlternativeTo + TechPowerUp +
  MajorGeeks listings live the same day so discovery has somewhere to land.
- Phase 3 (authority): EposVox deep-dive pitch with the measurement rig and
  an open invitation to break it; Gamers Nexus-tier only if a hook exists.
- Phase 4 (monetization, FOSS intact): Steam convenience build launch — the
  press angle "the free ShadowPlay alternative is now on Steam for $6, still
  open source" is itself a story. Steam reviews become the durable moat.
- Evergreen: every "what clipper should I use" thread on the internet is the
  product's permanent funnel; a pinned honest comparison page on the site
  (including when NOT to use this — no editor, no cloud) earns the links.

### 2.6 Rules of engagement (protect the only irreplaceable asset: trust)
- Disclosure always; never astroturf; never sockpuppet; never review-brigade.
- Creators get builds and benchmarks, never money-for-coverage (destroys the
  FOSS halo and most will refuse anyway).
- Criticism handling: bug reports in public threads answered fast and
  logged; the "dev showed up and fixed it in a day" comment is worth more
  than any ad.
- The competitor-bashing budget is zero. The measurement budget is unlimited.
