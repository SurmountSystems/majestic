---
name: memex
description: >
  A personal memex with agentic support for lossless ingest, efficient encoding, and ripgrep-style search of account data exports
---

# memex

Prefer the installed `memex` CLI, `memex mcp`, `memex serve`, or `memex acp`.
Do not reimplement search or JSON ingest in a subagent.

| Item | Path |
|------|------|
| Crate | `~/majestic` |
| Data | `$HOME/memex/{service}/{account}.majestic`. Leftover `.archive` is listed only when there is no sibling same-stem `.majestic`. Do not mmap `.zst`. |
| Command | `memex` |
| Catalog | `~/majestic/docs/local-functions.md` |
| Skill copy | `~/majestic/skills/memex/SKILL.md` |

## Defaults

1. **Search** with no service, account, or archive path is systemwide: every
   `*.majestic` under `memex_dir` (default `$HOME/memex`), plus leftover `*.archive` when
   there is no sibling same-stem `.majestic`. It does not walk `$HOME` for live
   export dumps. Ingest owns discovery. Does not mmap
   `.zst`. Open maps the `.majestic` file with `PROT_READ` and `MAP_SHARED`.
   That is a virtual mapping of the file, not a heap allocation of the file
   size. Search reads that map. It does not copy the text blob into a `Vec`.
   RSS is the pages the CPU has faulted, not the file size at `mmap()`. The
   intended page for text is 2 MiB. New ingest pads to that file offset. A
   systemwide search maps all listed archives, holds those maps, compiles
   PCRE2 once, then runs PCRE2 on each packed span of already-mapped text with
   `min(file count, available parallelism)` workers. Search does not call
   `MADV_DONTNEED` after every archive during a live scan.
   Hits are unique (conversation id, field, entire packed span text). After
   uniqueness, identical packed bodies print once. Each snippet group lists
   every place that body appeared: archive path, conversation id, and field.
   Two conversation ids or two archives with the same sentence are one snippet
   and two occurrence rows. Two different bodies stay two snippet blocks.
   One packed message is one hit even when PCRE2 matches it many times.
   Duplicate packed copies of the same body in one conversation print once.
   Default print cap is 100 snippet groups (`--max-count`). Uses PCRE2 always (same
   engine as `rg -P`). Does not spawn `rg`.
   `--service`+`--account` or an archive path searches one file only.
2. **Ingest** with no paths scans `$HOME` for known export shapes (Grok dump,
   ChatGPT zip or dir, Meta DYI `facebook-*.zip` / `instagram-*.zip` and
   `your_facebook_activity/` / `your_instagram_activity/` trees, X account archive zips and
   `data/account.js` trees, Telegram `result.json`, Obsidian,
   `.agents/reports`, `session_search.sqlite` with `session_docs`). Duplicate
   Meta zip bytes in a group are skipped. Archives land at
   `social/meta/<account>.majestic`.
   Max 8 directory levels.
   No symlinks. Home scan does not ingest arbitrary markdown trees. Does not walk `$HOME/memex` as a source. Home scan always infers
   per source; pass explicit paths to force `--service` / `--account`. `-o` with
   a scan is an error. With paths and no `--service` / `--account`, infer from
   input shape (including markdown dirs). Session JSONL still needs those flags
   or `-o`. Do not ingest `grok_oss.db`.
3. **Stats** still need `--service` and `--account`, or an archive path.

## Configuration

Optional settings live at `~/.config/majestic/memex.toml`
(`$XDG_CONFIG_HOME/majestic/memex.toml`). That is not `~/.config/memex/` and
not `~/majestic/memex.toml`. A missing file uses crate defaults. `MEMEX_CONFIG`
selects another TOML path. There is no `--config` flag. CLI flags override the
file. `--memex-dir` overrides `memex_dir`.

Home scan skips system trash by default (XDG Trash, directory name `.Trash`,
and names that start with `.Trash-`). Extra skips belong in
`scan.skip_directories` on that machine. Crate defaults do not skip
`~/.agents/trash`. Skip lists have no CLI flags. Example with every key:
`~/majestic/docs/memex.toml.example`.

Layers: crate defaults, then the TOML file (figment), then `MEMEX_*`
environment variables, then flags you passed. Figment overlays layers. The
`directories` crate supplies the XDG config directory. confy cannot overlay
layers.

## Commands

```bash
just install   # in ~/majestic; just check, then cargo install + strip to ~/.local/bin/memex
# rename leftover v1 ~/memex/**/*.archive to .majestic, then optional:
memex compress FILE.majestic   # writes FILE.majestic.zst (zstd, default level 3, compress.level); keeps FILE
memex decompress FILE.majestic.zst   # writes FILE.majestic; keeps the .zst
memex ingest   # no args: scan $HOME for known export shapes
memex search -i catfooding   # every archive under memex_dir (default $HOME/memex)
memex search -F 'C.tfooding'
memex search -w food
memex search 'lizard AND the'   # human AND: both words in the same title or message
memex search '/Catfooding/i'
memex search --format json 'lizard AND the'
memex search --service agents/grok --account <xUsername> PATTERN
memex ingest /path/to/dump
memex ingest /path/to/ChatExport_synth/result.json
memex ingest /path/to/tiny-chatgpt.zip
memex ingest /path/to/facebook-synthuser-2026-01-01-aaaa.zip
memex ingest /path/to/twitter-2026-01-01-aaaa.zip
just bench-zstd
memex stats --service agents/grok --account <xUsername>
memex mcp      # Cursor / Claude / Zed stdio MCP (JSON default; --toon for TOON)
memex serve    # HTTP MCP, POST http://127.0.0.1:8741/mcp ; GET /health ; no auth; --toon or Accept text/toon
memex acp      # ACP-shaped NDJSON JSON-RPC (JSON default; --toon for TOON)
nix develop    # toolchain, just, cargo-nextest, pkg-config, sqlite, pcre2
nix build      # result/bin/memex
```

## Search patterns

Same engine as `rg -P` after compile. A pattern that is not slash-wrapped is a human query. `/regex/flags` is PCRE2. Search uses mmap text, grep-searcher, and grep-pcre2 only. PCRE2 runs on each packed span (one title or one message), not the whole archive blob. AND requires both words in that span. No rust-regex. No PCRE1. MCP/ACP have no pcre2 flag. Document patterns with this table. Do not write "case fold". Say case insensitive. Stdout is the report (`--format human|json|toon`). Status is tracing INFO on stderr.

| Want | Pattern |
| --- | --- |
| OR | `lizard OR catfooding` or `lizard\|catfooding` |
| AND, any order | `lizard AND the` |
| Phrase | `"hello world"` or `-F 'hello world'` |
| Case insensitive | `-i` or `/Catfooding/i` |
| Whole word | `-w` |
| Regex | `/<regex>/` flags: `i`, `g`, `m`, `s`, `x` |

Needs system `pcre2` (libpcre2; pacman/nix). Optional local speed on this Ryzen:
`RUSTFLAGS='-C target-cpu=native'`. Not in repo config (other CPUs). Nix
`devShell` on x86_64 may set `x86-64-v3` (AVX2 class). Package build does not.

## Nix

From `~/majestic`: `nix develop` is the flake shell (Rust 1.98.0, just,
cargo-nextest, pkg-config, sqlite, pcre2). `nix build` produces `result/bin/memex`.
Prefer `just install` for `~/.local/bin/memex`.

## MCP (stdio)

JSON-RPC 2.0. Default is JSON, one object per line, for humans. `--toon` is
[TOON](https://github.com/toon-format/spec) (accessed: 2026-08-27) for
language-model tools: one TOON document then a blank line. Media type
`text/toon`. Tracing on stderr.

Host config: command `memex`, args `["mcp"]` (add `"--toon"` for TOON).

Tools: `list_archives`, `search`, `stats`, `ingest`, `scoped_path`,
`infer_grok_export`. Default `search` is all archives under `memex_dir`.
Search compiles a human query or `/regex/flags` (`ignore_case`, `fixed_strings`,
`word_regexp`, optional `max_count`). Hits are unique, then grouped by packed
body into one snippet plus an `occurrences` array. No pcre2
flag. It does not spawn `rg`.
`ingest` infers service and account from input shape when those fields are omitted.
Empty `inputs` scans `$HOME` for known export shapes.

## MCP (HTTP)

`memex serve` listens on `127.0.0.1:8741` by default (loopback, no auth).
`POST /mcp` is one JSON-RPC 2.0 object (same tools as stdio). JSON is the
default. TOON uses `Content-Type: text/toon`, `Accept: text/toon`,
`?format=toon`, or `--toon`. Notifications
(no id) return 204. `GET /health` returns 200 `ok`. Tracing on stderr.
`--bind` changes the listen address. Do not bind `0.0.0.0` unless you mean to.

## ACP

`memex acp` uses those names as JSON-RPC methods. JSON default. `--toon` is TOON.
In-process: `majestic::call_local(&RpcContext::new(home), method, &params)`.

## Safety

- Never copy live export JSON, live Facebook DYI zips, or live X archive zips into git, this crate, or `~/memex` as JSON.
- Never print auth keys, tokens, or live `xUsername` values.
- Tests and examples use synthetic fixtures only.

## Sub-agents

L1 coordinates. Spawn L2, then L3, for multi-file diagnosis or ingest/archive
work. Do not reimplement search; run `memex` or MCP.

| Depth | Does |
|-------|------|
| **L1** | Status, spawn L2, one named `memex` command, read a named report. |
| **L2** | Parallelize. Spawn L3 for multi-file diagnosis. |
| **L3** | Tools. Run `memex` / MCP. Do not spawn. |

Hard stop: CI, regressions, multi-file archive diagnosis: L1 spawns L2 first;
L2 spawns L3 for greps and reads. Hierarchical fast path on L1: one named
`memex` command or one already-named archive path.

## Don't

- Do not pull grok-build crates into majestic.
- Do not treat `$HOME/memex` as the crate home (`~/majestic` is the crate).
- Do not invent a second search implementation.
