# majestic

Crate `majestic` is a personal memex with agent support for lossless ingest,
efficient encoding, and search of account data exports. Search uses the same
PCRE2 engine as ripgrep (`rg -P`).

The crate name is **majestic**. The installed command is **memex**. Archives
use the **majestic v1** format on disk (magic `MAJESTIC` plus byte `0x01`) and
the file suffix `*.majestic`. Data lives under `~/memex` (that is `$HOME/memex`).
Number 0012 is Majestic Memex and is specified. This crate at `~/majestic` is
still a proof of concept. It is not that numbered specification.

Prose in this README follows [Concise American Technical English](https://github.com/SurmountSystems/specs/blob/main/0005_CATE.md)
(accessed: 2026-08-27).

> "If Vannevar Bush had technology like this, maybe we'd all be flying around in UFOs by now." - Hunter Beast

## Purpose

This crate ingests export formats from many services and keeps them in one
place.

1. Ingest is lossless. Export fields stay in the archive. This is not a
   traditional vector store.
2. Data is scoped to each service and account. Multiple imports of the same
   account can accumulate. Matching bytes keep one stored body.
3. Archives use rkyv mapped into memory and reads that do not copy the mapped
   bytes. Search uses PCRE2 (the same engine as `rg -P`) on each packed span
   of that mapped UTF-8 text blob (one title or one message). Open maps the
   `.majestic` file with `PROT_READ` and `MAP_SHARED`.
   That is a virtual mapping of the file, not a heap allocation of the file
   size. Search reads that map. It does not copy the text blob into a `Vec`.
   RSS is the pages the CPU has faulted, usually because PCRE2 read them.
   Mapping a file does not make RSS equal the file size. The intended page for
   the UTF-8 text is 2 MiB (2097152 bytes). New ingest pads so text starts at
   a 2 MiB file offset. Open aligns the mapping's virtual address the same way.
   Older short archives still search. On Linux with transparent huge pages in
   always mode (this host: Linux 7,
   `/sys/kernel/mm/transparent_hugepage/enabled` is `always`), open then calls
   `madvise` `MADV_HUGEPAGE` and `madvise` `MADV_COLLAPSE` on the text. Collapse
   errors are DEBUG; `Archive::open` still succeeds. Do not copy onto
   hugetlbfs. Do not `MAP_HUGETLB` on the file descriptor. A systemwide search
   lists paths, maps all listed archives and holds those maps, compiles PCRE2
   once, then runs PCRE2 on each packed span of already-mapped text with
   `min(file count, available parallelism)` workers. Search does not ask the
   kernel to drop pages (`MADV_DONTNEED`) after every archive while the search
   is still running. Hits are unique (same archive, conversation id, field, and entire
   packed span text print once). Many PCRE2 matches in one message are one hit.
   Duplicate packed copies of the same body stay one hit. The unique key is not
   the snippet window and is not the packed start offset. After uniqueness,
   identical packed bodies print once. Each snippet group lists every place
   that body appeared: archive path, conversation id, and field. Two
   conversation ids or two archives with the same sentence are one snippet and
   two occurrence rows. Two different bodies stay two snippet blocks.

## Layout

The installed command is `memex`. The crate stays at `~/majestic`. Archives
live at `~/memex/{service}/{account}.majestic`. Older `{account}.archive` files
are still listed and searched when there is no sibling `.majestic` with the
same stem. Do not map `.zst` files into memory. After `just install`, rename
majestic v1 files under `~/memex/**/*.archive` to `.majestic`. Optional:
`memex compress` writes `FILE.majestic.zst` next to the uncompressed file
(zstd level 3, no dictionary) and does not delete the input.

| Source | Infer when | Archive |
|--------|------------|---------|
| Official xAI Grok dump | `prod-grok-backend.json` (file, dir, or zip) | `agents/grok/<xUsername>.majestic` |
| ChatGPT export | `conversations-*.json` plus `user.json` (dir or zip). Account is `$USER`. Account names never come from an email address. | `agents/chatgpt/<USER>.majestic` |
| Meta Download Your Information | zip or dir whose paths include `your_facebook_activity/` or `your_instagram_activity/`. Filename `facebook-{account}-*.zip` or `instagram-{account}-*.zip` sets the account, else `$USER`. Account names never come from an email address. Duplicate zip bytes in a group are skipped. | `social/meta/<account>.majestic` |
| X account archive | zip or dir with `data/account.js` plus `data/tweet.js` or `data/tweets.js`. Account is `account.username`. Account names never come from an email address. Duplicate zip bytes in a group are skipped. | `social/x/<username>.majestic` |
| Telegram Desktop JSON | `result.json` with `id`/`messages`/`name`/`type`, or `personal_information` plus `chats.list` (file, dir, or zip) | `social/telegram/<user>.majestic` |
| grok-oss session JSONL | `chat_history.jsonl` (still needs flags) | `agents/grok-oss/<user>.majestic` |
| grok-oss sqlite chats | `session_docs` table, or file named `session_search.sqlite` | `agents/grok-oss/<user>.majestic` |
| Agent reports | directory ending in `.agents/reports` | `agents/reports/<user>.majestic` |
| Obsidian vault | folder that contains `.obsidian/` | `notes/obsidian/<vault-dir-name>.majestic` |
| Markdown tree | `.md` files and no `.obsidian/` (explicit path only; home scan skips arbitrary markdown) | `notes/markdown/<dir-name>.majestic` |
| Story-card JSON array | JSON array of objects with `title`, `type`, `keys`, and `value`. Explicit path. Needs `--service` and `--account` (or `-o`). Home scan does not treat a random JSON array as story cards. | `{service}/{account}.majestic` |
| Character TOML | `characters/{id}.toml`. Explicit path. Needs `--service` and `--account` (or `-o`). JSON character files are not ingested. | `{service}/{account}.majestic` |

`<user>` is the process `$USER` unless `--account` is set. Telegram uses
`personal_information.username` when that field is present and not empty,
otherwise `$USER`. Meta (Facebook and Instagram) uses the `{account}` in
`facebook-{account}-` or `instagram-{account}-` when the zip or directory name
matches, otherwise `$USER`. An X account archive uses `account.username` from
`data/account.js`, otherwise `$USER`. Those account names never come from an
email address. Obsidian vault names may contain spaces. Do not ingest
`grok_oss.db` (usage tables, not chats). Home scan also skips empty 0-byte
`session_search.sqlite` files. Home scan picks up `facebook-*.zip`,
`instagram-*.zip`, and `your_facebook_activity/` / `your_instagram_activity/`
trees, and X archive zips plus unzipped `data/account.js` trees. A zip whose
hash of the whole file matches another zip already in that account group is
skipped. Do not copy live export JSON, live vaults, live Facebook zips, live
X archive zips, or live sqlite into this crate.

## Configuration

On Linux, optional settings live at `~/.config/majestic/memex.toml` (that is
`$XDG_CONFIG_HOME/majestic/memex.toml` when `XDG_CONFIG_HOME` is set). That is
not `~/.config/memex/` and not `~/majestic/memex.toml`. memex loads crate
defaults, then that TOML file, then `MEMEX_*` environment variables, then
command-line flags you actually passed. If the file is missing, memex uses
crate defaults. You do not have to create the file.

`MEMEX_CONFIG` selects another TOML path. A leading `~` in that path, in
`memex_dir`, and in `skip_directories` entries expands to `$HOME`. Nested
environment keys use a double underscore after the `MEMEX_` prefix (for
example `MEMEX_SEARCH__IGNORE_CASE`). `MEMEX_CONFIG` is the file path only; it
is not a TOML key. `MEMEX_LOG` and `RUST_LOG` set the terminal tracing filter
and are not figment overlays of `[log]`.

Layers use [figment](https://docs.rs/figment) (accessed: 2026-08-27) with
serde and toml, plus the [directories](https://docs.rs/directories) crate
(accessed: 2026-08-27) for the XDG config directory. Figment overlays
defaults, a file, env, and CLI. confy cannot overlay layers. Clap has no env
feature. Flags overlay only when clap records that you passed them, so clap
defaults do not wipe the file.

CLI flags override the file. `--memex-dir` is the flag for `memex_dir`. There
is no `--config` flag; set `MEMEX_CONFIG`. Skip lists have no CLI flags; they
live under `[scan]`.

A committed example with every key is [`docs/memex.toml.example`](docs/memex.toml.example).
Copy it to `~/.config/majestic/memex.toml` and edit. Extra `skip_directories`
are per machine. Crate defaults skip system trash (XDG Trash such as
`$XDG_DATA_HOME/Trash`, usually `~/.local/share/Trash`, directory name
`.Trash`, and names that start with `.Trash-`) and do not skip extra personal
directories. The example uses `skip_directories = []` plus a commented extra
path that is not machine-specific.

| Key | Default | Meaning |
| --- | --- | --- |
| `memex_dir` | `~/memex` | Archive directory. `--memex-dir` overrides it. Home scan does not walk this directory as a source. |
| `scan.home_scan_max_depth` | `8` | Directory levels below `$HOME` for `memex ingest` with no paths. |
| `scan.skip_directories` | `[]` | Extra paths to skip on this machine (prefix match when absolute, consecutive path components when relative). |
| `scan.skip_system_trash` | `true` | Skip XDG Trash, directory name `.Trash`, and names that start with `.Trash-`. Does not skip all of `/tmp`. |
| `search.ignore_case` | `false` | Case insensitive (`-i`). |
| `search.fixed_strings` | `false` | Phrase or literal (`-F`). |
| `search.word_regexp` | `false` | Whole word (`-w`). |
| `search.max_count` | `100` | Snippet groups printed (`-m` / `--max-count`). `0` means no cap. |
| `search.format` | `human` | Search report on stdout (`human`, `json`, or `toon`). Status stays on stderr. |
| `search.archive` | unset | One archive path. Unset means every archive under the data directory. |
| `ingest.output` | unset | Output archive (`-o`). Unset means infer or home scan. |
| `ingest.inputs` | `[]` | Input dumps. Empty means scan `$HOME`. |
| `scope.service` | unset | Folder under the memex directory (`agents/grok`). |
| `scope.account` | unset | Username file stem. |
| `stats.archive` | unset | Stats archive path. Unset means `--service` and `--account`, or an error. |
| `serve.bind` | `127.0.0.1:8741` | `memex serve` listen address. |
| `compress.level` | `3` | zstd level for `memex compress`. No dictionary. |
| `zip.max_uncompressed_bytes` | `8589934592` (8 GiB) | Skip zip entries larger than this uncompressed size. |
| `log.filter` | `info` | Terminal tracing when `MEMEX_LOG` and `RUST_LOG` are unset. Default `info` means error and info, not warn. |
| `jobs.rayon_threads` | unset | Rayon worker count for ingest hashing. Unset leaves the rayon default. Search does not use this. |
| `bench_zstd.dir` | `~/memex` | Directory of uncompressed archives for `memex bench-zstd`. |
| `bench_zstd.jsonl` | `~/memex/bench-zstd/report.jsonl` | JSONL report path. |
| `bench_zstd.report` | `~/memex/bench-zstd/report.md` | Markdown report path. |
| `bench_zstd.re_ingest` | `false` | Time first versus second ingest. |

## Logging

The terminal shows error and info by default. It does not show warn, debug, or
trace. Permission denied is an error. Expected skips (system trash, empty
sqlite, sandbox-blocked directories) are debug and go to the journal, not the
terminal.

Full logs use syslog identifier `memex`:

```bash
journalctl --user SYSLOG_IDENTIFIER=memex
journalctl --user -t memex
```

Override the terminal with `MEMEX_LOG` or `RUST_LOG`. If those are unset,
`[log] filter` in `~/.config/majestic/memex.toml` applies (default `info`
means error and info, not warn). The journal always keeps the full log. If
journald is missing, the terminal still works and the journal layer is skipped.

## just recipes

Run these from `~/majestic` (or pass `-f ~/majestic/justfile`). Recipes `cd` to
that directory. Cargo is niced (19) with idle ionice. Cargo and nextest pick
CPU count. Do not hardcode jobs. For optional local speed on this Ryzen, set
`RUSTFLAGS='-C target-cpu=native'` (not in repo config; other CPUs).

| Recipe | What it does |
|--------|----------------|
| `just` | Lists recipes. Does not format, check, or install. |
| `just check` | `cargo fmt --all -- --check`, then clippy `--all-targets -D warnings`, then `cargo nextest run`. Does **not** install. |
| `just install` | Runs `just check`, then `cargo install --path . --locked --force --root ~/.local`, then `strip --strip-unneeded` on `~/.local/bin/memex`. |
| `just fmt` | rustfmt the crate (`cargo fmt --all`). |
| `just update` | `cargo update`. Writes `Cargo.lock`. Does not compile. |
| `just bench-zstd` | Niced release `memex bench-zstd`. Compresses uncompressed archives under `$HOME/memex` at zstd levels 1-22 (one pass with no dictionary, and two passes that train a dictionary). JSONL plus markdown land under `$HOME/memex/bench-zstd/`. Temps are moved with `gio trash` after each level. Does not delete source archives or Downloads. |
| `just man` | Copies `man/memex.1` and `man/memex-search.1` to `~/.local/share/man/man1/`. `just install` runs this after the binary install. View without installing: `man -l man/memex.1`. |

## Commands

The binary name is `memex`. There is no `memex PATTERN` alias. Use the `search`
subcommand so `ingest` / `mcp` / `serve` stay unambiguous.

### ingest

`memex ingest` with no paths scans `$HOME` for known export shapes only (the
table above), including `.zip` files whose central directory lists those names.
It does not extract zips to disk. It does not ingest arbitrary markdown trees.
`--service` and `--account` override on explicit paths. `-o` still wins on
explicit paths. Home scan always infers per source. Pass explicit paths to
force those flags. `-o` or `--service` / `--account` with a home scan is an
error, because a scan writes many archives.

The scan walks at most 8 directory levels below `$HOME`, does not follow
symlinks, and does not walk `$HOME/memex` as a source. The default skip is
system trash, as in Configuration. Extra directories to skip belong in
`skip_directories` in `~/.config/majestic/memex.toml`. Do not skip
`~/.agents/trash` as a crate default. The scan also skips bulky trees that are
not export sources, including directory names `.git`, `node_modules`, `target`,
`.cache`, `.cargo`, `.rustup`, `.npm`, `.nvm`, `Steam`, `nix`, `proc`,
`.config`, names that start with `sandbox-blocked-dir` (Grok OSS blocked
sandbox directories), and paths under `.local/share/Steam` and
`.local/share/TelegramDesktop` (tdata, not a JSON export). Directory names
match exactly, including letter case. The scan does not walk `.grok` except
`.grok/sessions` (for `session_search.sqlite`). Permission denied while walking
is an error (see Logging). Empty sqlite files, or sqlite without a
`session_docs` table, skip at debug in the journal, not on the terminal. A directory and a zip of the same
ChatGPT, Grok, or Telegram export payload keep one copy (source dumps stay on
disk). A folder named `ChatExport_*` with `result.json` is Telegram. It is
never treated as a Grok export.

Telegram `memex ingest` stats print export dumps (how many `result.json` files
were ingested) and unique chats (same chat id and the same bytes stay one row
when overlapping dumps share a chat). Other services print unique conversations
the same way.

```bash
just install
memex ingest
memex ingest /path/to/dump
memex ingest --account personal /path/to/dump
memex ingest ~/.agents/reports
memex ingest "/path/to/Obsidian Vault"
memex ingest ~/.grok/sessions/session_search.sqlite
memex ingest /path/to/ChatExport_synth/result.json
memex ingest /path/to/facebook-synthuser-2026-01-01-aaaa.zip
memex ingest /path/to/twitter-2026-01-01-aaaa.zip
```

Session JSONL still needs `--service` and `--account`, or `-o`.

### search

`memex search PATTERN` with no `--service`, no `--account`, and no archive path
searches every `*.majestic` under `$HOME/memex`, plus older `*.archive` files
when there is no sibling `.majestic` with the same stem. A systemwide search
lists paths, maps all listed archives and holds those maps, compiles PCRE2
once, then runs PCRE2 on each packed span of already-mapped text with
`min(file count, available parallelism)` workers. It does not walk `$HOME`
for live export dumps. Ingest owns discovery (`memex ingest` with no paths). It does not map
`.zst` files into memory. Search uses PCRE2 always (`grep-searcher` /
`grep-pcre2`, same engine as `rg -P`). AND any order uses PCRE2 lookaheads
on one packed span (one title or one message). Both words must appear in
that span. It is not enough for them to appear anywhere in the archive.
Search does not use rust-regex. It does not spawn `rg`. Hits are unique
(conversation id, field, and the entire packed span text). After uniqueness,
identical packed bodies print once, then occurrence rows list archive path,
conversation id, and field. Two conversation ids or two archives with the same
sentence are one snippet and two occurrence rows. Two different bodies stay two
snippet blocks. MCP, ACP, HTTP, and TOON use the same `occurrences` array.
Many PCRE2 matches in one packed message print once. Duplicate packed
copies of the same body in one conversation print once. Default print
cap is 100 snippet groups (`--max-count`, `search.max_count`;
`0` means no cap).

`--service` and `--account` together, or an explicit archive path, search one
file and do not scan home. Search matches letter case unless you pass `-i`,
which is case insensitive (Unicode). A pattern that is not slash-wrapped is
a human query. `/regex/flags` is PCRE2. `-F` / `--fixed-strings` is a phrase
or literal string. `-w` / `--word-regexp` is whole word. Without `-w`, a match
may sit inside a stored word.

A pattern that is not slash-wrapped is a human query. Slash-wrapped
`/regex/flags` is PCRE2. Search uses mmap text, grep-searcher, and grep-pcre2
only. There is no PCRE1 and no rust-regex fallback. Human `OR` and `|` are OR.
Human `AND` (and implicit AND of bare words) compiles to lookaheads and
matches only when both words sit in the same title or the same message.
Double quotes mark a contiguous phrase. `--format human` (default), `json`,
or `toon` writes the report on stdout. Status (searching N archives, then
searching k/N path and a running unique-hit count) goes to stderr.

| Want | Pattern |
| --- | --- |
| OR | `lizard OR catfooding` or `lizard\|catfooding` |
| AND any order | `lizard AND the` |
| Phrase | `"hello world"` or `-F 'hello world'` |
| Case insensitive | `-i` or `/Catfooding/i` |
| Whole word | `-w` |
| Regex | `/<regex>/` flags: `i`, `g` (all matches; already unique by message), `m`, `s`, `x` |
| PCRE2 AND | `/(?=.*lizard)(?=.*the)/` |

MCP and ACP search compile the same human or slash-wrapped query. There is no
pcre2 flag on those surfaces. Search needs the system `pcre2` library
(libpcre2). `memex search --help` prints the same table. Manual pages: `man
memex` after `just man`, or `man -l man/memex.1` from this crate.

```bash
just install
memex search PATTERN
memex search -i catfooding
memex search -F 'C.tfooding'
memex search -w food
memex search 'lizard AND the'
memex search 'lizard OR catfooding'
memex search '"hello world"'
memex search '/Catfooding/i'
memex search --format json 'lizard AND the'
memex search --service agents/grok --account <xUsername> PATTERN
```

### stats

`memex stats` needs `--service` and `--account`, or an explicit archive path.
There is no silent default of `~/memex/archive.majestic`. The command prints
counts. It does not print auth key values.

```bash
memex stats --service agents/grok --account <xUsername>
memex stats /path/to/file.majestic
```

### compress

`memex compress FILE...` writes `FILE.zst` next to each uncompressed archive
(zstd level 3, no dictionary). Input `foo.majestic` becomes `foo.majestic.zst`.
It never deletes the input. Default ingest still writes uncompressed `.majestic`.

```bash
just install
memex compress ~/memex/agents/grok/<xUsername>.majestic
```

### decompress

`memex decompress FILE.zst` writes `FILE` without `.zst` (so `foo.majestic.zst`
becomes `foo.majestic`). It does not require a dict file. It does not delete the
`.zst`.

```bash
memex decompress ~/memex/agents/grok/<xUsername>.majestic.zst
```

### mcp

`memex mcp` is JSON-RPC 2.0 on stdin/stdout. Default is JSON, one object per
line, for humans. `--toon` is [Token-Oriented Object Notation (TOON)](https://github.com/toon-format/spec)
(accessed: 2026-08-27) for language-model tools: one TOON document then a blank
line. Media type `text/toon`. Tracing on the terminal is error and info on
stderr. Full logs: `journalctl --user -t memex`. Host config: command `memex`,
args `["mcp"]` (add `"--toon"` for TOON). Tools:
`list_archives`, `search`, `stats`, `ingest`, `scoped_path`,
`infer_grok_export`. Default `search` is every archive under `$HOME/memex`.
Empty `ingest` inputs scan `$HOME` for known export shapes.

### acp

`memex acp` is the same local functions as NDJSON JSON-RPC methods. Default is
JSON, one object per line. `--toon` is TOON for language-model tools. Tracing
on the terminal is error and info on stderr. In-process:
`majestic::call_local(&RpcContext::new(home), method, &params)`. The catalog is
`docs/local-functions.md`.

### serve

`memex serve` is the same MCP JSON-RPC over HTTP. Default `--bind` is loopback
`127.0.0.1:8741`. There is no authentication and no TLS. Tracing on the
terminal is error and info on stderr. JSON is the default encoding. TOON uses
`Content-Type: text/toon` on the request, and `Accept: text/toon`,
`?format=toon`, or `--toon` on the response.

| Method | Path | Result |
|--------|------|--------|
| POST | `/mcp` | One JSON-RPC 2.0 object (JSON or TOON). Same tools as `memex mcp`. Notifications with no `id` return 204 empty. |
| GET | `/health` | 200 `ok` |

`--bind` changes the listen address. Do not bind `0.0.0.0` unless you mean to.

```bash
memex serve
memex serve --bind 127.0.0.1:8741
memex serve --toon
curl -sS -X POST http://127.0.0.1:8741/mcp -H 'content-type: text/toon' -H 'accept: text/toon' --data-binary @request.toon
```

### bench-zstd

`memex bench-zstd` reads uncompressed `*.archive` and `*.majestic` files under
a directory (default `$HOME/memex`). It writes one JSON object per row (jq) and
a markdown report. The header embeds `powerprofilesctl get` and `garuda-inxi`.
Dict size is `clamp(64KiB, S/100, 100MiB)` for uncompressed size `S`. Levels
are 1 through 22 (ultra 20 through 22). One pass compresses with no trained
dictionary. Two passes train a dictionary on samples, then compress with that
dictionary. Queries `lizard` and `the`. Temps under `$HOME/memex/bench-zstd/`
are moved with `gio trash` after each level. `--re-ingest` times first versus
second ingest of discovered home exports. Never deletes source archives or
Downloads.

```bash
just bench-zstd
memex bench-zstd
memex bench-zstd --re-ingest
```

## Nix

From `~/majestic`. Rust is pinned at 1.98.0 in `rust-toolchain.toml`.

| Command | What it does |
|---------|----------------|
| `nix develop` | Dev shell: that toolchain, just, cargo-nextest, pkg-config, sqlite, pcre2. On x86_64, optional `RUSTFLAGS=-C target-cpu=x86-64-v3` (AVX2 class). Not `native`. |
| `nix build` | Builds the `memex` binary (`packages.default`). Result is `result/bin/memex`. Does not bake `target-cpu=native`. |

`flake.lock` is tracked. Prefer `just install` for `~/.local/bin/memex`.

## Limits

Search is PCRE2 (`grep-pcre2`, same as `rg -P`) on archive text. It is not the
full `rg` CLI (no `--type`, `--glob`, or a spawned `rg`). Search needs the
system `pcre2` library (libpcre2; pacman/nix).

`~/.grok/grok_oss.db` is usage tables, not chats. Do not ingest it. Use
`session_search.sqlite` (or another sqlite file with `session_docs`).

Home scan walks at most 8 directory levels, does not follow symlinks, and does
not walk `$HOME/memex` as a source. `memex serve` has no authentication and no
TLS. Bind loopback unless you mean to expose it.

## Contributing

Pull requests for additional import formats are accepted. Other improvements
will probably be considered so long as defaults stay sensible. This README is
the contributing note. There is no separate contributor license agreement and
no separate code of conduct file.

## Repository

Once published, this project lives at
[https://github.com/SurmountSystems/majestic](https://github.com/SurmountSystems/majestic).
That sentence names the intended git home. It does not claim the repository is
already public, and it does not claim this crate is on crates.io. Agents do
not publish the crate.

## License

This software is released into the public domain under the Unlicense. See
`UNLICENSE.md` in this crate.

## Residual

1. Number 0012 is Majestic Memex and is specified. This command at `~/majestic`
   is still a proof of concept. It is not that numbered specification.
2. The intended git home is https://github.com/SurmountSystems/majestic . That
   location is not claimed as already public. This crate is not on crates.io
   yet. Agents do not publish it.
3. Optional settings live at `~/.config/majestic/memex.toml`. A missing file
   uses crate defaults. Home scan skips system trash by default. Extra skips
   belong in `skip_directories` on that machine. Crate defaults do not skip
   `~/.agents/trash`. `memex_dir` is the archive directory for list, search,
   stats, RPC, MCP, ACP, and CLI. `compress.level` is the zstd level for
   `memex compress` (default 3). `zip.max_uncompressed_bytes` is the zip
   ingest skip (default 8 GiB). Explicit dump ingest honors extra
   `skip_directories`.
4. Older archives that still use the `.archive` suffix are listed and searched
   when there is no sibling `.majestic` with the same stem. Rename majestic v1
   files under `~/memex` to `.majestic` after `just install`. Do not map `.zst`
   files into memory.
5. Additional import formats are remaining work in the sense that only the
   shapes in the Layout table are inferred today. Pull requests for more
   formats are accepted, as in Contributing.
