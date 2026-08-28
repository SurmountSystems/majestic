# majestic

`just check` is fmt, clippy, and nextest. It does not install. `just install`
runs `just check` first, then `cargo install --path . --locked --force --root ~/.local`
and `strip --strip-unneeded` on `~/.local/bin/memex`. Builds are niced (19) with
idle ionice. Cargo and nextest pick CPU count. wild is in `.cargo/config.toml`.
Do not hardcode jobs or `target-cpu=native` (other CPUs).
Archives live at `~/memex/{service}/{account}.majestic`. Leftover
`{account}.archive` files are still listed and searched when there is no sibling
same-stem `.majestic`. Do not mmap `.zst`. After `just install`, rename leftover
v1 `~/memex/**/*.archive` to `.majestic`. Optional: `memex compress` writes
`FILE.majestic.zst` next to the uncompressed file (zstd level 3, no dictionary)
and does not delete the input. `memex decompress FILE.zst` writes `FILE` without
`.zst` and does not delete the `.zst`.

Crate **majestic**, command **memex**, on-disk format **majestic v1**
(magic ASCII `MAJESTIC` plus byte `0x01`). The crate stays at `~/majestic`. Agents look
in `~/memex/{service}/{account}` with the suffix above. Service is a folder (`agents/grok`).
Account is the username file stem (Grok export `user.xUsername`, or `--account`).
Do not rename or move this crate to `~/memex` unless the operator says the tool
lives there. If a path or name split is unclear: pencils down, ask, do not assume
(`~/.grok/AGENTS.md` § *Ambiguity → park*, 2026-08-25). Number 0012 is Majestic
Memex and is specified. The command at `~/majestic` is still a proof of concept.
Do not claim this crate is that numbered specification.

This crate archives official xAI Grok account export JSON into mmap-friendly
`.majestic` files and searches those files. crates.io already ships unrelated
`memex` and `memex-cli` binaries; that is why this **crate** is named majestic,
not why the **data directory** would move. The installed command is `memex`.

`memex ingest` with no paths scans `$HOME` for known export shapes only (official
Grok `prod-grok-backend.json`, ChatGPT `conversations-*.json` plus `user.json`
as a dir or zip, Facebook DYI `facebook-*.zip` and `your_facebook_activity/`
trees, X account archives (`data/account.js` plus `data/tweet.js` or
`data/tweets.js`), Telegram Desktop `result.json`, Obsidian vaults,
`.agents/reports`, `session_search.sqlite` with `session_docs`). Zip sources are
classified from the central directory and streamed; they are not extracted to
disk. Encrypted zips, entries larger than 8 GiB uncompressed, and zip-inside-zip
are skipped. ChatGPT account stem is process `$USER`, never an email from
`user.json`. It does not ingest arbitrary markdown trees; pass a markdown dir
explicitly. The scan walks
at most 8 directory levels, does not follow symlinks, does not walk `$HOME/memex`
as a source, and skips `.git`, `node_modules`, `target`, `.cache`, `.cargo`,
`.rustup`, `.npm`, `.nvm`, `Steam`, `.local/share/Steam`,
`.local/share/TelegramDesktop`, `nix`, `.Trash`, `proc`, and names that start
with `sandbox-blocked-dir`. The default skip is system trash. Extra skips
belong in `skip_directories` in `~/.config/majestic/memex.toml`. Do not skip
`~/.agents/trash` as a crate default. The scan does not walk `.grok` except
`.grok/sessions`. A `ChatExport_*` folder with `result.json` is Telegram. Home scan always
infers per source; pass explicit paths to force `--service` or `--account`. `-o`
with a scan is an error. `memex ingest PATH` with no `--service` and `--account`
infers from input shape. Official Grok dump (`ttl/30d/export_data/.../prod-grok-backend.json`
plus sibling `prod-mc-auth-mgmt-api.json`) infers `agents/grok` and
`user.xUsername`. Telegram `result.json` (single chat `id`/`messages`/`name`/`type`,
or `personal_information` plus `chats.list`) infers `social/telegram` and
`personal_information.username` when non-empty, else `$USER`, landing
`~/memex/social/telegram/<USER>.majestic` when the username is missing. A folder
that contains `.obsidian/` infers `notes/obsidian` and the vault directory name
(spaces allowed). A sqlite file named `session_search.sqlite`, or any sqlite
file with a `session_docs` table, infers `agents/grok-oss` and the process
`$USER`. A directory ending in `.agents/reports` infers `agents/reports` and
`$USER`. A ChatGPT export (`conversations-*.json` plus `user.json`, unzipped or
zip) infers `agents/chatgpt` and `$USER`. A Facebook DYI zip or dir whose paths
include `your_facebook_activity/` or `your_instagram_activity/` infers `social/meta` and the `{account}` in
`facebook-{account}-` when the name matches, else `$USER`, never email, landing
`~/memex/social/meta/<account>.majestic`. An X account archive zip or dir with
`data/account.js` plus a tweet payload infers `social/x` and
`account.username`, never email, landing `~/memex/social/x/<username>.majestic`.
Duplicate zip bytes in a home-scan group
are skipped (same size, then full-file hash). A markdown tree with no `.obsidian/`
infers `notes/markdown` and the directory name. `--service` and `--account`
override on explicit paths. `-o`
still wins on explicit paths. Session JSONL still needs those flags or `-o`. Do
not ingest `~/.grok/grok_oss.db` (usage tables, not chats). `memex search PATTERN` with no `--service`, no
`--account`, and no archive path is a systemwide search: every `*.majestic` under
`$HOME/memex`, plus leftover `*.archive` when there is no sibling
same-stem `.majestic`. It does not walk `$HOME` for live export dumps. Ingest
owns discovery. Search uses PCRE2 (grep-pcre2, same engine as
`rg -P`) on the mmap UTF-8 text blob. It does not copy that blob into a `Vec`.
Open **memory-maps** each `.majestic` file with `PROT_READ` and `MAP_SHARED`.
That is a virtual mapping of the file, not a heap allocation of the file size.
Virtual size can be tens of gigabytes for one person (C1). **Resident** size
(RSS) is only pages the CPU has faulted, usually because PCRE2 read them.
Mapping a file does not make RSS equal the file size. Do not say a 12GB memex
means 12GB resident just because it is mapped. Do not `MADV_DONTNEED` as if
mmap were a heap allocation to free. This is not a C10K server. Map each
archive file (about a hundred files for one person, C1), not ten thousand.
The intended page for the UTF-8 text is 2 MiB (2097152 bytes). New ingest pads
so text starts at a 2 MiB file offset. Open aligns the mapping's virtual
address the same way. Older short archives still search. On Linux with
transparent huge pages in always mode (this host: Linux 7,
`/sys/kernel/mm/transparent_hugepage/enabled` is `always`), open then calls
`madvise` `MADV_HUGEPAGE` and `madvise` `MADV_COLLAPSE` on the text. Collapse
errors are DEBUG; `Archive::open` still succeeds. Do not copy onto hugetlbfs.
Do not `MAP_HUGETLB` on the file descriptor. A systemwide search lists paths,
maps all listed archives and holds those maps, compiles PCRE2 once, then runs
PCRE2 on already-mapped `archive.text()` with
`min(file count, available parallelism)` workers. That is parallelism on
mapped slices. Concurrency may overlap the next mmap with a scan that has
already started. Do not `par_iter` the whole path list as the architecture
(open and search each path as a job). There is no four-map cap. Hits are
unique (same archive, conversation id, field, and snippet print once) and
grouped by archive path, then conversation id. Default print cap is 100 unique
hits per archive (`search.max_count`, `--max-count`; `0` means no cap). It does
not spawn `rg`. `--service` and `--account`
together, or an explicit archive path, still search one file and do not scan home.
Stats still need those flags or an
explicit archive path. There is no silent default of `~/memex/archive.majestic`.
Ingest creates the service folder under `$HOME/memex` if it is missing. Do not
copy live export JSON, live vaults, live agent reports, or live sqlite into this
crate or into `~/memex` as JSON. Do not print a live `xUsername` in docs or
reports.

Efficiency is the constraint. The schema names known Grok export fields and keeps every unknown key in a leftover map (`extra`). Do not parse a multi-gigabyte export into a `serde_json::Value` document object model. Stream top-level keys and one conversation object at a time. The `.majestic` file is mmap plus rkyv bytecheck on open.

Uploaded files are compared by hashing their contents, not by id, size, or date alone. Matching bytes keep one catalog row and one stored body inside `.majestic`. Same id with different bytes stay two bodies. Do not put a hash field on leftover JSON maps.

## Secrets and fixtures

Never commit live xAI or Grok export JSON, ChatGPT export zips, Facebook DYI zips, X account archive zips, auth files, session tokens, or API keys. Live trees under `~/Downloads` stay on the operator machine. They are not fixtures.

Tests use synthetic fixtures only (`tests/fixtures/`). Invented ids, invented timestamps, and invented message text. No real user identifiers and no copy-paste from a live export.

Do not print secret values in tests, comments, or agent reports. Auth and billing types preserve keys such as `api_keys` as leftover JSON. The CLI must not dump those values. v1 has no `--include-secrets` flag.

## Language and tools

Pure Rust. No Python. No new agent-authored Python or shell glue.

Rust is pinned at 1.98.0 in `rust-toolchain.toml` (components rustfmt, clippy,
rust-src) and as `rust-version = "1.98"` in `Cargo.toml`. `flake.nix` is a
small fenix+crane flake: nixpkgs `nixos-unstable`, fenix (follows nixpkgs),
and crane. `packages.default` is the `memex` binary. `nix develop` has the
toolchain, just, cargo-nextest, pkg-config, sqlite, and pcre2. Keep `flake.lock`
tracked. Do not ignore it. On x86_64, the devShell may set
`RUSTFLAGS=-C target-cpu=x86-64-v3` (AVX2 class). Do not bake `native` into the
nix package for all systems.

Do not add heed, lmdb, candle, or embedding crates. rusqlite is allowed only as
a read-only ingest reader for grok-oss `session_docs` (never `grok_oss.db`). The
mmap `.majestic` file is the store (leftover `.archive` is still readable). Do not use sqlite as the memex
store. Tries (FST maps) are still packed at ingest. Search runs PCRE2
(`grep-searcher` / `grep-pcre2` / `grep-matcher`, same engine as `rg -P`) on the
mmap UTF-8 text blob. It does not copy that blob into a `Vec`. Open maps the
`.majestic` file with `PROT_READ` and `MAP_SHARED`. Search reads that map. RSS
is the pages the CPU has faulted, not the file size at `mmap()`. The intended
page for text is 2 MiB. New ingest pads to that file offset. A systemwide
search maps all listed archives, holds those maps, compiles PCRE2 once, then
runs PCRE2 on already-mapped text with `min(file count, available parallelism)`
workers. Search does not call `MADV_DONTNEED` after every archive during a live
scan. Hits are unique and grouped by archive, then conversation. AND any
order uses PCRE2 lookaheads. Search does not use
rust-regex or PCRE1. It does not query those FST maps. It does not spawn the
`rg` binary. It is not a database and not a vector index.

Prose in this crate follows [Concise American Technical English](https://github.com/SurmountSystems/specs/blob/main/0005_CATE.md) (accessed: 2026-08-27). Write complete American English thoughts. Do not use half labels as sentences. Residual is written in full. Leftover unstack (deleting a hyphen from a legal-caption compound and leaving the two words) is nonconforming; rephrase into ordinary English.

On Linux, optional settings live at `~/.config/majestic/memex.toml`
(`$XDG_CONFIG_HOME/majestic/memex.toml`). That is not `~/.config/memex/` and
not `~/majestic/memex.toml`. A missing file uses crate defaults. You do not
have to create the file. `MEMEX_CONFIG` selects another TOML path (tilde
expanded). There is no `--config` flag. Layers are crate defaults, then that
file, then `MEMEX_*` environment variables (nested keys use a double
underscore, for example `MEMEX_SEARCH__IGNORE_CASE`), then CLI flags the user
passed. CLI flags override the file. `--memex-dir` overrides `memex_dir`.

Layers use figment 0.10 (toml plus env) with serde, and directories 5 for the
XDG config directory. Figment overlays defaults, a file, env, and CLI. confy
cannot overlay layers. Clap has no env feature; overlay uses
`ValueSource::CommandLine` so clap defaults do not wipe the file.

The default skip for a home scan is system trash (XDG Trash, directory name
`.Trash`, and names that start with `.Trash-`). Extra skips go in
`scan.skip_directories`. Skip lists have no CLI flags. Do not tell everyone to
skip `~/.agents/trash`. Do not put Hunter-only paths in crate defaults. The
committed example is `docs/memex.toml.example` (`skip_directories = []` plus a
commented extra path that is not machine-specific). Keys match the crate
README Configuration table (`memex_dir`, `scan.*`, `search.*`, `ingest.*`,
`scope.*`, `stats.archive`, `serve.bind`, `compress.level`,
`zip.max_uncompressed_bytes`, `log.filter`, `jobs.rayon_threads`,
`bench_zstd.*`). `MEMEX_LOG` and `RUST_LOG` set the terminal tracing filter;
they are not figment overlays of `[log]`. The crate README names the intended
git home https://github.com/SurmountSystems/majestic without claiming it is
already public or on crates.io.

### Always remember (operator 2026-08-27)

When the operator says **always remember** (or please remember, or I hate repeating myself), that is an agentic instruction. Pin it the same turn so it survives compaction and attention dilution. Put it where the next session will look: this file for crate law, `~/.grok/AGENTS.md` for cross-repo law, residual or a skill when those are the files that get loaded. Sometimes that means more than one place. Chat alone is not enough.

A chat **Next:** line is status, not residual. Casually saying next is not a substitute for documenting leftover work. Write the owed outcome on the session board and in `RESIDUAL.md` or `~/.agents/reports/` the same turn. If it lived only in chat Next, it was never documented.

### Logging (operator 2026-08-27)

Permission denied is **ERROR**, not INFO. The default terminal shows ERROR and INFO only. Events that already worked belong in WARN, DEBUG, or TRACE, not as INFO spam. Keep the full log in the systemd journal (`tracing-journald`, `journalctl`) so debugging has detail the user never sees on the console. Defaults stay helpful and short. Do not print one line per expected skip.

Syslog identifier is **`memex`**. Read the full log with:

```bash
journalctl --user SYSLOG_IDENTIFIER=memex
journalctl --user -t memex
```

If journald is missing, the terminal still works and the journal layer is skipped.

| Event | Terminal | Journal |
| --- | --- | --- |
| Permission denied (`EACCES` / os error 13) | ERROR (first path; if many, one count plus "see journalctl") | ERROR each path |
| Ingest started with source count; wrote archive with counts; search finished with hit count; MCP HTTP listening | INFO | INFO |
| Expected skip (system trash, sandbox-blocked dir, empty sqlite, skip-list name) | no | DEBUG |
| Duplicate zip / zip entry skip / per-request MCP or ACP | no | DEBUG |
| Unreadable archive that is not permission denied | no | WARN |
| Path not found while walking | no | TRACE |

Override the terminal with `MEMEX_LOG` or `RUST_LOG`. If those are unset, `[log] filter` in `~/.config/majestic/memex.toml` applies. Default `info` means error and info, not warn. The journal always keeps the full log.

## Search patterns

Agents MUST document memex search patterns with this table. Do not write "case fold", "caseless", or "caseful". Say **case insensitive**.

Search uses mmap text, grep-searcher, and grep-pcre2 only. Patterns are PCRE2. There is no PCRE1 and no rust-regex fallback. `-i` is case insensitive. `-w` is whole word. `-F` is a phrase or literal. `|` is OR. AND any order uses lookaheads.

| Want | Pattern |
| --- | --- |
| OR | `lizard\|catfooding` |
| AND, any order | `(?=.*lizard)(?=.*the)` |
| Phrase | `'hello world'` or `-F 'hello world'` |
| Case insensitive | `-i` |
| Whole word | `-w` |

CLI: `memex search PATTERN`. `memex search --help` prints this table. Manual pages: `man -l man/memex.1` or `just man` then `man memex`.

## How agents prove work

Agents do not run `cargo test`, `cargo build`, `cargo clippy`, or rustc on this laptop. The operator runs `just check`. File-level rustfmt is allowed when it does not invoke rustc.

Red then green is still the contract: write the failing test for the named behavior, then the smallest product types that make that test pass. Do not rewrite a test to match a lossy parse.

## Groups

1. Schema and scaffold: crate, known types, leftover map, synthetic lossless tests.
2. Ingest and archive: stream export JSON, write `.majestic`, mmap open with rkyv bytecheck.
3. Search CLI: PCRE2 always (`grep-pcre2`, same engine as `rg -P`) on the mmap UTF-8 text blob. Case sensitive unless `-i` (case insensitive, Unicode). Default pattern is PCRE2. Document patterns with the Search patterns table in this file (OR `lizard|catfooding`, AND any order `(?=.*lizard)(?=.*the)`, phrase `'hello world'` or `-F 'hello world'`, case insensitive `-i`, whole word `-w`). No `--rust-regex`. MCP/ACP have no pcre2 flag. Do not spawn `rg`. Ingest reads grok-oss `chat_history.jsonl` as chat turns. Other session `*.jsonl` stays leftover JSON on the export. Markdown files, Obsidian notes (skip `.obsidian/`), agent reports, and `session_docs` sqlite rows become the same conversation records. Unknown keys and YAML frontmatter stay in leftover maps.

`memex search PATTERN` (no `--service`, no `--account`, no archive path) is case-sensitive and is a systemwide search: every `*.majestic` under `$HOME/memex`, plus leftover `*.archive` when there is no sibling same-stem `.majestic`. A systemwide search lists paths, maps all listed archives and holds those maps, compiles PCRE2 once, then runs PCRE2 on already-mapped text with `min(file count, available parallelism)` workers. It does not walk `$HOME` for live export dumps. Ingest owns discovery. It does not mmap `.zst`. Hits are unique and grouped: archive path under `memex/`, then conversation id, then unique messages. Default print cap is 100 unique hits per archive (`--max-count`). `memex search --service agents/grok --account <xUsername> PATTERN` reads only `$HOME/memex/agents/grok/<xUsername>.majestic`. An explicit archive path also searches one file. Scoped search does not also scan home. Search and stats do not infer from an export dir. Stats still error if both scoped flags and an archive path are omitted. `memex search -i PATTERN` is case insensitive (Unicode). Default pattern is PCRE2. `-F` is a phrase or literal. `-w` is whole word. Without `-w`, a match may sit inside a stored word. AND any order is `(?=.*lizard)(?=.*the)`. It does not spawn `rg`. Unreadable or corrupt archives are skipped with a warning; no archives prints that no archives were found.

Do not `git add`, `git commit`, or `git init` unless the operator explicitly asks. Do not publish to crates.io unless the operator asks.

`memex mcp` is JSON-RPC 2.0 on stdin/stdout (default: one JSON object per line for humans; `--toon` is [TOON](https://github.com/toon-format/spec) (accessed: 2026-08-27) for language-model tools, one TOON document then a blank line; media type `text/toon`; terminal tracing is error and info on stderr; full logs `journalctl --user -t memex`) for Cursor, Claude, and Zed. `memex serve` is the same MCP JSON-RPC over HTTP (`POST http://127.0.0.1:8741/mcp`; JSON default; `Content-Type: text/toon`, `Accept: text/toon`, `?format=toon`, or `--toon` for TOON; default `--bind` is loopback `127.0.0.1:8741`, no auth; notifications with no `id` return 204 empty; `GET /health` returns 200 `ok`; same tracing). Do not invent a second tool list. `memex acp` is the same local functions as NDJSON JSON-RPC methods (JSON or TOON, same flags). In-process: `majestic::call_local`. Catalog: `docs/local-functions.md`. Agent skill: `skills/memex/SKILL.md` (host copy `~/.agents/skills/memex/SKILL.md`). Default search is every archive under `$HOME/memex`. Ingest infers from input shape. Prefer the CLI or MCP; do not reimplement search. Never copy live export JSON into git. Never print auth keys.
