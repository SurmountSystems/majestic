# Local functions

A personal memex with agentic support for lossless ingest, efficient encoding, and ripgrep-style search of account data exports

These are the in-process APIs. CLI flags, MCP tools, and ACP methods use this set.

In-process Rust: `majestic::call_local(&RpcContext::new(home), method, &params)`.
`RpcContext::from_home_and_config(home, config)` uses loaded `memex_dir`.
`memex mcp` wraps the same names as MCP tools (`initialize`, `tools/list`, `tools/call`).
Default encoding is JSON (one object per line, for humans). `--toon` is
[TOON](https://github.com/toon-format/spec) (accessed: 2026-08-27) for
language-model tools (one TOON document then a blank line). Media type `text/toon`.
`memex serve` is the same MCP JSON-RPC over HTTP: `POST http://127.0.0.1:8741/mcp`
(default `--bind` is loopback `127.0.0.1:8741`, no auth). JSON is the default.
TOON uses `Content-Type: text/toon`, `Accept: text/toon`, `?format=toon`, or `--toon`.
Notifications with no `id` return 204 empty. `GET /health` returns 200 `ok`. Tracing stays on stderr.
`memex acp` uses the **ACP method** names as JSON-RPC methods (NDJSON JSON, or TOON with `--toon`).
Tracing stays on stderr so stdout stays the wire encoding.

Default search is every `*.majestic` under `memex_dir` (default `$HOME/memex`), plus leftover
`*.archive` when there is no sibling same-stem `.majestic`. It does not walk
`$HOME` for live export dumps. Ingest owns discovery. Scoped `--service`+`--account`
or an explicit archive path still search one file. Ingest with no service or account infers from input shape:
ChatGPT zip or dir (`agents/chatgpt`, account `$USER`, never email), Facebook
DYI zip or dir (`social/meta`, account from `facebook-{account}-` / `instagram-{account}-` or `$USER`,
never email), X account archive zip or dir (`social/x`, account from
`account.username`, never email), official Grok dump, Telegram Desktop `result.json`, Obsidian
(`.obsidian/`), `session_docs` sqlite or `session_search.sqlite`,
`.agents/reports`, then a markdown tree. `memex ingest` with no paths scans
`$HOME` for those known shapes only (not arbitrary markdown trees), including
zips whose central directory lists known inner names, at most 8 directory
levels, no symlinks, and does not walk `$HOME/memex` as a source. Home scan
picks up `facebook-*.zip` and X archive zips; duplicate zip bytes in a group are skipped. Home scan
always infers per source; pass explicit paths to force `--service` or
`--account`. `-o` with a home scan is an error. Session JSONL still needs
service and account, or an explicit output path. Optional settings live at
`~/.config/majestic/memex.toml` (`$XDG_CONFIG_HOME/majestic/memex.toml`). A
missing file uses crate defaults. Home scan skips system trash by default.
Extra skips belong in `scan.skip_directories`. Crate defaults do not skip
`~/.agents/trash`. Never copy live export JSON into git. Never print auth
keys. Do not ingest `grok_oss.db`.

| Plain name | What it does | CLI | MCP tool | ACP method | HTTP |
|------------|--------------|-----|----------|------------|------|
| List archives | Lists `*.majestic` under `memex_dir` (default `$HOME/memex`), plus leftover `*.archive` with no sibling `.majestic`. Does not list `.zst`. Skips README files. | none (MCP/ACP/HTTP only) | `list_archives` | `list_archives` | `tools/call` `list_archives` |
| Search all archives | Systemwide: every archive under `memex_dir`. Maps all listed archives, holds those maps, compiles PCRE2 once, then runs PCRE2 on each packed span of already-mapped text with `min(file count, available parallelism)` workers. AND requires both words in the same title or the same message. Unique hits, then identical packed bodies print once with an `occurrences` array (archive path, conversation id, field). Two conversation ids or two archives with the same sentence are one snippet and two occurrence rows. One packed message is one hit. Duplicate packed copies of the same body in one conversation print once. Default print cap 100 snippet groups (`--max-count`). Unreadable files are skipped. Pattern is PCRE2 always. | `memex search PATTERN` (`-i` case insensitive, `-F` phrase or literal, `-w` whole word, `-m` snippet-group print cap) | `search` (omit service, account, archive; no pcre2 flag) | `search` | `tools/call` `search` |
| Search one archive | Searches one scoped file or an explicit archive path. | `memex search --service FOLDER --account NAME PATTERN` or `memex search PATTERN ARCHIVE` | `search` with `service`+`account` or `archive` | `search` | `tools/call` `search` |
| Stats | Prints archive counts. No auth key values. Needs `--service`+`--account` or an archive path. | `memex stats --service FOLDER --account NAME` or `memex stats ARCHIVE` | `stats` | `stats` | `tools/call` `stats` |
| Ingest | Streams Grok JSON, ChatGPT conversations zips/dirs, Facebook DYI zips/dirs, X account archives, Telegram `result.json`, session JSONL, markdown, Obsidian, agent reports, or session_docs sqlite into an archive. Empty inputs scan `$HOME` for known export shapes. | `memex ingest` or `memex ingest INPUTS...` (`-o` wins on explicit paths; `--service` / `--account` override explicit paths; omitted infers from shape) | `ingest` | `ingest` | `tools/call` `ingest` |
| Scoped path | Resolves service folder plus account stem to a path under `memex_dir`. | library `scoped_archive_path` | `scoped_path` | `scoped_path` | `tools/call` `scoped_path` |
| Infer from a Grok dump | Infers service `agents/grok` and the account file stem from `user.xUsername`. | `memex ingest INPUTS...` with `--service` and `--account` omitted | `infer_grok_export` | `infer_grok_export` | `tools/call` `infer_grok_export` |
| Compress | zstd, no dictionary. Default level 3 (`compress.level`). Writes `FILE.zst` next to the input. Never deletes the input. Default ingest still writes uncompressed `.majestic`. | `memex compress FILE...` | n/a | n/a | n/a |
| Decompress | Writes `FILE` without `.zst`. Does not require a dict file. Never deletes the `.zst`. | `memex decompress FILE.zst` | n/a | n/a | n/a |
| Serve MCP over HTTP | Same MCP JSON-RPC as `memex mcp`, over HTTP. Default `--bind 127.0.0.1:8741` (loopback). No auth. No TLS. Tracing on stderr. | `memex serve` (`--bind`) | n/a (this is the HTTP transport) | n/a | `POST /mcp` |
| Health | Liveness. | n/a | n/a | n/a | `GET /health` returns 200 `ok` |

## Search patterns

Search uses mmap text, grep-searcher, and grep-pcre2 only. A pattern that is not slash-wrapped is a human query. `/regex/flags` is PCRE2. There is no PCRE1 and no rust-regex fallback. Human `OR` and `|` are OR. Human `AND` (bare words are implicit AND) compiles to lookaheads and matches only when both words sit in the same packed span (one title or one message). It does not mean both words appear anywhere in the archive. `-i` / `ignore_case` is case insensitive. `-w` / `word_regexp` is whole word. `-F` / `fixed_strings` is a phrase or literal.

| Want | Pattern |
| --- | --- |
| OR | `lizard OR catfooding` or `lizard\|catfooding` |
| AND, any order | `lizard AND the` |
| Phrase | `"hello world"` or `-F 'hello world'` |
| Case insensitive | `-i` or `/Catfooding/i` |
| Whole word | `-w` |
| Regex | `/<regex>/` flags: `i`, `g`, `m`, `s`, `x` |

## Parameters

| Method | Arguments |
|--------|-----------|
| `list_archives` | none (uses `RpcContext.memex_dir`) |
| `search` | `pattern` (required, human query or `/regex/flags`; see Search patterns below); `ignore_case` (bool, default false, `-i` case insensitive); `fixed_strings` (bool, default false, `-F` phrase or literal); `word_regexp` (bool, default false, `-w` whole word); `max_count` (non-negative integer, snippet groups printed, default 100, `0` means no cap); optional `service`, `account`, `archive`. Hits are `{ snippet, field, occurrences: [{ archive, conversation_id, field }] }`. CLI `--format json` / `toon` uses those objects. No `pcre2` flag. |
| `stats` | `service`+`account`, or `archive` |
| `ingest` | optional `inputs` (array of paths); optional `service`, `account`, `output`. Empty `inputs` scans `$HOME` (or `RpcContext.home`) for known export shapes. Inference: ChatGPT zip/dir, Meta DYI zip/dir (`social/meta`), X account archive (`social/x`), Grok dump, Telegram `result.json`, Obsidian, session_docs sqlite, `.agents/reports`, markdown. `-o` / `output` wins on explicit paths and is an error on a home scan. |
| `scoped_path` | `service`, `account` |
| `infer_grok_export` | `input` (export directory or backend JSON path) |

New files are always `{account}.majestic`. Leftover `{account}.archive` is still
listed when there is no sibling same-stem `.majestic`. Do not mmap `.zst`.
Grok dumps land at `$HOME/memex/agents/grok/<account>.majestic`.
ChatGPT exports land at `$HOME/memex/agents/chatgpt/<USER>.majestic`. Account is
`$USER`, never an email from `user.json`. Facebook DYI lands at
`$HOME/memex/social/meta/<account>.majestic`. Account is `facebook-{account}-` or `instagram-{account}-` from
the zip or directory name when it matches, else `$USER`, never email. X account
archives land at `$HOME/memex/social/x/<username>.majestic`. Account is
`account.username` from `data/account.js`, never email. Duplicate
zip bytes in a group are skipped. Reports land at
`$HOME/memex/agents/reports/<USER>.majestic`. An Obsidian vault named
`Obsidian Vault` lands at `$HOME/memex/notes/obsidian/Obsidian Vault.majestic`.
Telegram Desktop JSON lands at `$HOME/memex/social/telegram/<USER>.majestic` when
`personal_information.username` is missing or empty. Optional `memex compress FILE`
writes `FILE.zst` at zstd level 3 with no dictionary and keeps the input.
`memex decompress FILE.zst` writes `FILE` without `.zst` and keeps the `.zst`.

## HTTP

`memex serve` (default `--bind 127.0.0.1:8741`, loopback, no auth):

- `POST http://127.0.0.1:8741/mcp`: one JSON-RPC 2.0 object (JSON or TOON). Same tools as `memex mcp`.
  Notifications (no `id`) return 204 empty. JSON default. TOON: `Content-Type: text/toon`,
  `Accept: text/toon`, `?format=toon`, or `memex serve --toon`. Spec:
  [TOON](https://github.com/toon-format/spec) (accessed: 2026-08-27).
- `GET http://127.0.0.1:8741/health`: 200 `ok`.

No TLS. No tokens. Do not bind `0.0.0.0` unless `--bind` says so.
