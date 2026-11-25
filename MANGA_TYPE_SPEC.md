# manga-type: handoff spec for Claude Code

Native desktop app. Read manhua/manga/webtoons copied from a Mihon library, but the page only scrolls as you retype the dialogue from each speech bubble.

## Working notes for Claude Code
- All decisions below were settled in chat (2026-11-23). New open questions: ask before coding them.
- The user decides technical choices. Present options with pros/cons; don't just pick.
- Casual tone, direct answers, concrete commands. No step-by-step card widgets.
- Environment: Arch Linux, Hyprland (Wayland). Comics are English only.

## Core idea
1. Phone (Pixel 7, Android, Mihon) connects over USB.
2. Add a series from the phone's Mihon downloads to the PC library, then Sync copies its chapters to the PC.
3. Once copied, chapters are read only on the PC. Nothing is ever written back to the phone or Mihon.
4. OCR finds the speech bubbles in the pages.
5. Reading screen: vertical page strip, scrolling locked. The current bubble is highlighted and its text shown. Each correct typed character scrolls the view smoothly toward that bubble. Finishing a bubble moves to the next. Wrong key: flash red, rejected, no progress.

## Stack (decided)
- Rust.
- GUI: `egui` / `eframe`.
- OCR: `tesseract` CLI as a subprocess (`--psm 11`, TSV output, `eng`), behind a small internal OCR trait (image in -> lines with box, text, confidence out) so `ocrs` can be tried later.
- Phone: `adb_client` crate (pure Rust), direct USB, reusing the existing key `~/.android/adbkey`. No external `adb` binary.
- Other crates (expected): `image` (webp/jpg/png), `zip` (cbz), `natord`, `serde` + `serde_json`, `directories`, `flate2` (Mihon backup), std threads + channels for workers.
- System deps: `sudo pacman -S tesseract tesseract-data-eng`.

## Phone / Mihon (verified on the user's phone)
- Mihon storage root: `/storage/emulated/0/Books/Tachiyomi/` (NOT `Mihon/`). Contains `downloads/`, `local/`, `autobackup/`. No SD card.
- Layout: `downloads/<Source (EN)>/<Series>/<Chapter>_<hash>.cbz`. ~6 sources, ~50 series, 15 GB.
- Chapters are `.cbz`. Loose folders ending in `_tmp` are unfinished downloads: skip them. Still support loose image folders in case.
- Inside a cbz: `.webp` and `.jpg` mixed, zip entries in random order, plus `ComicInfo.xml`. Sort pages naturally.
- Mihon pre-splits tall pages: `016__001.jpg`, `016__002.jpg`, ... (900x4000 each). Treat as consecutive pieces of page 016; OCR/bubble merge must handle a bubble cut across a piece boundary.
- Page sizes vary: ~690x1500 (4.5 MB/chapter) up to 900x4000 pieces (50 MB/chapter). Copy speed over USB ~33 MB/s.
- Phone setup: USB debugging on. Direct USB via `adb_client` needed its own one-time "Allow USB debugging?" approval (2026-11-23), even with the same key the SDK adb used; the crate only waits 10 s for it. Right after approval the phone may send a stray AUTH message; `Phone::connect` retries once.
- Direct-USB copy speed measured ~17-19 MB/s (SDK adb did ~33 MB/s). Fine for now: a 51 MB chapter takes 3 s.
- Device errors to handle: no device, unauthorized (RSA prompt), disconnected mid-copy, storage path not found, and **adb server holding the device** -> show an error with a button to stop the adb server.

## Mihon progress (read-only)
- Mihon's DB is private (not rooted, not debuggable). Read the newest `autobackup/*.tachibk` instead (written about every 12h).
- Format: gzip'd protobuf. `Backup.field1` = repeated manga. Manga: `3` title, `2` url, `1` source id, `100` favorite (absent = true), `16` repeated chapter, `104` history. Chapter: `1` url, `2` name, `4` read (bool), `6` lastPageRead, `9` chapterNumber (float32), `10` sourceOrder.
- Chapter numbers can be odd (e.g. 5478); order by chapter list order, not trust the number blindly.
- Backup also has `101` repeated source {`1` name, `2` id}. Folder `Asura Scans (EN)` = source name + ` (LANG)`.
- The same title can appear several times for one source (old entries, non-library history). Pick the library entry, then most chapters.
- Series match: source name + title run through Mihon's `buildValidFilename` (`"*/:<>?\|` and control chars -> `_`, trim dots/spaces).
- Chapter match (implemented in `src/mihon.rs`): file suffix `_xxxxxx` = first 6 hex of md5(chapter url). Fallback by name (`<scanlator>_<name>`, then `<name>` alone), because sources change domains/URLs after download (seen on Manganato/Eleceed).
- Verified 2026-11-23: 716/744 downloaded chapters matched; all 28 misses were chapters newer than the backup. Chapters not in the backup count as unread.
- Used for: read/unread markers in the chapter list, the "Mihon unread" sync mode, and the add-series popup.

## Library and sync
- Add series: pick from the phone's downloads. Popup asks which chapters to copy: all / pick a starting chapter / start from Mihon's first unread.
- Sync: manual only. A Sync button per series plus a Sync all button. Show copy progress.
- What Sync copies is a setting:
  - only chapters Mihon hasn't marked read (**default**)
  - every downloaded chapter missing on the PC
  - only chapters after the last one finished in manga-type
- Phone files are never modified or deleted.
- Finishing a chapter: offer the next chapter.
- Implemented (2026-11-24, `src/library.rs`, `src/sync.rs`, `src/app.rs`):
  - Library folder (setting, default `~/Manga`): `<Source (EN)>/<Series>/<chapter>.cbz`, same names as on the phone. A series = a folder; no separate index.
  - `<library>/.mihon-backup.tachibk`: copy of the newest Mihon backup, saved on every phone connect, so read/unread markers work without the phone.
  - `<series>/.manga-type-series.json`: `start_from` (phone file name) picked in the add-series popup; Sync never copies chapters before it.
  - Chapter order: Mihon's source order when the backup knows every chapter (survives scanlator-prefix changes), else natural sort.
  - Phone work runs on one background thread that keeps the USB connection and reconnects once if it dropped. Copy progress, Cancel, and the adb-server-busy error with a "Stop adb server" button.
  - "After last finished" sync mode = phone chapters after the last chapter finished in manga-type.

## Typing rules
- Strictness setting (punctuation), **default: strict with keyboard equivalents**:
  - Lenient: only letters/digits count. Spaces and punctuation ignored.
  - Strict: spaces and punctuation must be typed, but keyboard equivalents are accepted: `'` for `’ ‘`, `"` for `“ ”`, `...` for `…`, `-` for `— –`.
- Case setting (separate from strictness; dropdown in the reader's bottom bar), **default: first letter capitalized**:
  - First letter capitalized: all-caps bubbles become sentence case (capital after `.`/`!`/`?`, not after a mid-bubble `...`; `I`, `I'm`, `I'll`, `I'd`, `I've` stay capital) and are typed exactly like that. Bubbles that already have lowercase keep their case. Names come out lowercase mid-sentence (no way to tell them apart yet).
  - Case sensitive: type the case on the page (all caps).
  - Case insensitive: any case accepted.
- Settings are saved to `~/.config/manga-type/settings.json` (`src/settings.rs`).
- Wrong letters setting (`typing::ErrorMode`, 2026-11-24), **default: block**:
  - Block: a wrong key flashes red and is rejected.
  - Must fix: the wrong letter goes in (red); the bubble only finishes once every red letter is backspaced and fixed.
  - Keep going: the wrong letter goes in (red); each one left when the bubble ends costs one word of net WPM (`Stats::uncorrected`).
- Backspace allowed. Progress and scroll move back with it.
- Tab skips the current bubble (sound effects, OCR garbage).
- "Edit text" button to fix a misread bubble before typing it.

## Reading / scroll mechanics
- Scrolling setting (2026-11-24, user's choice), **default: manual**. Typing never moves the page.
  - Arrows, Page Up/Down, mouse wheel always scroll. j / k / Space (step: page, half, quarter; setting) and Shift+Space keep scrolling until you type the first letter of the current bubble (with the bubble on screen); from then until the bubble is done they type. If the bubble really starts with j or k, that key starts typing. Typing is blocked while the bubble is off screen (its text is dimmed with a hint).
- You can't scroll past the bubble you're on: its top sticks to the top of the window until it's typed or skipped.
- Zoom: Ctrl + / Ctrl - / Ctrl 0 (reset) and Ctrl + mouse wheel zoom the page (30-400%, saved as `zoom`), keeping the middle of the view in place. Left / Right pan when zoomed wider than the window. egui's own interface zoom is turned off.
  - "Follow typing" keeps the original mechanics below, plus: a bubble is first brought on screen (bottom at 85% of the view) before typing it.
- Pages stacked vertically in one scroll area, user input scrolling disabled. Only typing moves it.
- Per bubble: start scroll position = where the previous bubble left off. End position = bubble center at about 38% of viewport height. Scroll target = lerp(start, end, typedChars / totalChars). Ease toward the target each frame (about 0.14 per frame).
- Bubble highlight: current = accent outline, done = faint green, skipped = dashed grey.
- Boxes are stored in **strip coordinates** (`src/pages.rs`): every page scaled to the chapter's most common page width and stacked top to bottom. Resize/zoom is a single scale factor, and a bubble cut across two Mihon split pieces stays one box.
- Bottom bar: bubble text with typed part coloured, bubble counter, live WPM and accuracy, skip + edit buttons.

## OCR details
- OCR runs in the background, in parallel (one worker per core, capped), while the user starts reading. The scanner stays ahead of the reader.
- Tesseract page segmentation mode 11 (sparse text).
- Use line-level results, then merge lines into bubbles: sort by y, join a line to a group if the vertical gap is under about 1.3x line height and it overlaps the group horizontally by at least 30%.
- Filter: word confidence >= 45; words that are mostly stray marks (`|\/_~=<>[]{}`...) dropped, marks trimmed off word ends.
- Bubble junk filter (`looks_like_text` in `src/ocr.rs`): letters >= 75% of non-punctuation chars, plus 2+ words of 3+ letters at conf >= 60, or a 3+-letter word at conf >= 80, or one shouted word with `!`/`?`/`...` at conf >= 60. On Eleceed ch 1.2: 271 candidates -> 122 kept, nearly all real dialogue; drops some sound effects.
- Every bubble found by the sparse pass is re-read on its own as a text block (`--psm 6`, crop padded ~1 line so missed lines are picked up). psm 11 reads some clean bubbles badly depending on scale (Regressor Instruction Manual ch 2: "LET'S GET OUT" -> "LET;:S:bEEE ouT"; the block read gets "AH, YES. LET'S GET OUT OF HERE..."). The re-read wins when the sparse text is junk, when it keeps every letter at >= 90% of the score, or when it scores 25% higher (score = (letters - 3 x junk chars) x conf). 1-2 character words outside the sparse text box are dropped (the bubble outline read as "4 N ... Y"). Overlapping results are de-duplicated. Costs ~1.5x scan time. Cache version 11.
- Token fixes after word correction (`fix::fix_tokens`): `#` as H, tripled / edge-doubled letters ("I'LLL", "YYOU"), glued words split when both halves are words and one is common ("HOWPATHETIC"; also when the glued form is only a name in the list: "CANDO", "ANDIS"), "70" / "50" between words -> TO / SO, two non-words that form a word joined ("EY ESIGHT"), lone letters other than A / I / O dropped, a lone digit dropped at the end (or at the start before a lone letter).
- Dark bands (mean grey < 100) get a second pass on a thresholded copy (pixels >= 60% grey -> black text on white); where the passes overlap, the line with more letters x confidence wins. Fixes glowing light-on-dark lettering: ASS-style "IF IT WISHES TO SHINE" over a starry sky went from "'HM SHINE" to "IF IT WISHES SHINE", and a whole missed caption was found. About 1 band in 15 on Star-Embracing Swordmaster ch 1. Cache version 10.
- Upscale bands before OCR toward ~1600 px wide (max 2.5x). On 690 px Eleceed this fixed dropped words and misreads (e.g. "TO" conf 41 -> 87) at ~1.6x the OCR time.
- Measured (6-core PC, 5 workers): Eleceed 48 bands in ~20 s, Nano Machine 75 bands in ~25 s; first bubbles ready after ~2 s. Cache reload: ~25 ms.
- Cache holds the **raw** OCR bubbles; `ocr::finish` (ad/credit removal + word correction) runs on every load in milliseconds, so rule changes never need a rescan.
- Word correction (`src/fix.rs`), against Tesseract's own `eng` word list (built once into `~/.cache/manga-type/eng-words.txt` with `combine_tessdata` + `dawg2wordlist`):
  - non-word -> word by lookalike letters P->D, L->U, F->P, H->U, and multi-letter LI/LU/CLI/LL->U, PD/DP->D, FP->P;
  - real-but-rare word -> very common word (PO->DO, PIP->DID, HAP->HAD) only when the chapter shows that confusion 3+ times;
  - `?` read as trailing `Z`/`2` (THATZ -> THAT?) when the rest is a common/4+ letter word or ends the bubble (keeps names like LUAZ);
  - digits in words (T0, S0, 7O), lone 1/T/l/| -> I, `|` touching a letter -> I, I glued to a word (INOW -> I NOW);
  - mostly-caps bubbles are fully uppercased (strict typing shouldn't make you type OCR's stray lowercase).
- Ads/credits: a bubble with scanlator/site text is always dropped (watermarks like FLAMESCANS.ORG sit inside story pages); non-watermark ad/credit text on the first/last two page images drops that whole page.
- Priority series (user, 2026-11-23): **Absolute Sword Sense** (Asura Scans) and **Star-Embracing Swordmaster** (ManhuaTop; ch 1-15 from FlameScans). Measured non-dictionary words before -> after correction: ASS ch1 8.2% -> 4.0%, ch2 8.3% -> 3.1%; SES ch1 3.9% -> 3.1%, ch130 3.7% -> 3.4%. What remains is mostly character names. Asura lettering is the hard one (D/P, U/L, U read as LI/LU).
- Known left: `50` for SO and `5` for IS as separate tokens, occasional dropped words, PAY vs DAY ambiguity in D/P fonts.
- Reading order: top to bottom, ties left to right.
- OCR runs on bands of the strip, not on single images: ~2000 px tall with 200 px overlap, ignoring image boundaries, so split pieces are OCR'd as one picture (seams verified invisible). Bubbles found twice in an overlap must be de-duplicated.
- Rendering all bands of a 150,000 px chapter takes ~2 s (small page cache per worker; dev builds use opt-level 1 because generic image code is ~10x slower unoptimized).
- Cache results as JSON next to each chapter (bubble boxes, text, edits). Re-opening a chapter skips the scan.

## Persistence
- Local library folder (configurable).
- Per chapter: OCR cache JSON, last reading position, stats.
  - `<chapter>.manga-type-progress.json` (`library::ChapterProgress`): current bubble, done/skipped marks, finished flag, chapter totals (correct, wrong, active seconds). Saved after every bubble and when leaving the reader. Not written if you open and close a chapter without typing.
  - Reopening resumes at the bubble start (not mid-bubble). A finished chapter ("Read again") starts from the first bubble.
- Settings (`~/.config/manga-type/settings.json`, settings dialog in the library): case, strictness, library path, sync mode, OCR worker count (0 = auto).

## Stats
- Live WPM (5 chars = 1 word) and accuracy (correct keystrokes / total keystrokes), per bubble and per chapter. Store per-chapter totals.

## Not in v1
- Bubble review/delete screen.
- Any sync back to Mihon (Suwayomi, AniList, Komga/Kavita were looked at and set aside).
- Automatic sync on plug-in.

## Next step: speed up compilation
Builds got slow once eframe was added (step 5): hundreds of GUI crates (winit, Wayland/X11, OpenGL, accesskit, zbus) compile from source, and `[profile.dev.package."*"] opt-level = 3` fully optimizes all of them, not just the image crates it was added for. Linking with the default linker is slow on every rebuild.

Options (user picks; not decided yet):
- **mold linker** (`pacman -S mold`, set in `.cargo/config.toml`): much faster linking on every rebuild.
- **Narrow the opt-level override** to the image crates (`image`, `zune-jpeg`, `image-webp`, `png`, `zip`...) instead of `"*"`: faster first build, same runtime speed.
- **Trim eframe features** (e.g. accesskit, x11 if only Hyprland is used): fewer crates; loses screen-reader support / X11 fallback.
- **`cargo check`** while editing: type-check only, no binary.
- **sccache**: caches compiled crates across projects and after `cargo clean`.

## Build order
Status 2026-11-24: steps 1-8 done in a first version. Not tested against the real phone since step 6 (no phone at hand): Connect, Add series, Sync / Sync all.

1. Phone layer: connect via `adb_client`, list series/chapters under `Books/Tachiyomi/downloads`, pull a chapter with progress, all error states. Test against the real phone.
2. Mihon backup reader: newest `.tachibk` -> per-series read/unread.
3. Page loading (cbz + images), natural sort, split-page handling, slicing of tall images.
4. OCR + bubble merge + JSON cache, background workers.
5. Reader view: scroll strip, highlights, typing input, scroll mechanics.
6. Library UI: add-series popup, Sync / Sync all, chapter list with Mihon markers, next-chapter offer.
7. Stats, settings, position memory.
8. Polish: error states, packaging/launcher.
   - `manga-type` with no arguments opens the app; `manga-type read <chapter>` opens the app on that chapter. The other subcommands stay as test tools.
   - `scripts/install.sh`: builds, links `~/.local/bin/manga-type`, installs `packaging/manga-type.desktop` + icon into `~/.local/share` (user only, no sudo). Re-run after `git pull`.
