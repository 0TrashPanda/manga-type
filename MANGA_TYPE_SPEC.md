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

## Typing rules
- Strictness setting, **default: strict with keyboard equivalents**:
  - Lenient: only letters/digits count. Spaces, punctuation, case ignored.
  - Case-insensitive: spaces and punctuation must be typed.
  - Strict: exact characters, but keyboard equivalents are accepted: `'` for `’ ‘`, `"` for `“ ”`, `...` for `…`, `-` for `— –`.
- Backspace allowed. Progress and scroll move back with it.
- Tab skips the current bubble (sound effects, OCR garbage).
- "Edit text" button to fix a misread bubble before typing it.

## Reading / scroll mechanics
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
- Per chapter: OCR cache JSON, last reading position (bubble index + progress), stats.
- Settings: strictness, library path, sync mode, worker count.

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
1. Phone layer: connect via `adb_client`, list series/chapters under `Books/Tachiyomi/downloads`, pull a chapter with progress, all error states. Test against the real phone.
2. Mihon backup reader: newest `.tachibk` -> per-series read/unread.
3. Page loading (cbz + images), natural sort, split-page handling, slicing of tall images.
4. OCR + bubble merge + JSON cache, background workers.
5. Reader view: scroll strip, highlights, typing input, scroll mechanics.
6. Library UI: add-series popup, Sync / Sync all, chapter list with Mihon markers, next-chapter offer.
7. Stats, settings, position memory.
8. Polish: error states, packaging/launcher.
