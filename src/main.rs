use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use majestic::Error;
use majestic::acp;
use majestic::archive::write_stats;
use majestic::bench_zstd::{self, BenchZstdOptions};
use majestic::config::{CliOverlay, Config};
use majestic::home_dir;
use majestic::ingest::{IngestReport, ingest_from_flags_with_config};
use majestic::mcp;
#[cfg(test)]
use majestic::resolve_ingest_archive;
use majestic::rpc::RpcContext;
use majestic::scoped_archive_path;
use majestic::serve::{self, DEFAULT_BIND};
use majestic::zstd_file;
use majestic::{SearchExec, SearchFlags, SearchFormat, write_search_all_exec, write_search_exec};

/// Printed on `memex --help`.
const AFTER_HELP: &str = "\
Configuration

  Optional settings live at ~/.config/majestic/memex.toml
  ($XDG_CONFIG_HOME/majestic/memex.toml). That is not ~/.config/memex/ and
  not ~/majestic/memex.toml. A missing file uses crate defaults. MEMEX_CONFIG
  selects another TOML path (a leading ~ expands to $HOME). There is no
  --config flag; set MEMEX_CONFIG. Layers are crate defaults, then that file
  (figment), then MEMEX_* environment variables, then flags you passed. CLI
  flags override the file. --memex-dir overrides memex_dir.

  Home scan skips system trash by default (XDG Trash, directory name .Trash,
  and names that start with .Trash-). Extra skips belong in [scan]
  skip_directories. Crate defaults do not skip ~/.agents/trash. Skip lists
  have no CLI flags. See docs/memex.toml.example.

Logs

  The terminal shows error and info by default. It does not show warn, debug,
  or trace. Full logs go to the systemd journal:

    journalctl --user SYSLOG_IDENTIFIER=memex
    journalctl --user -t memex

  Override the terminal with MEMEX_LOG or RUST_LOG. If those are unset,
  [log] filter in memex.toml applies (default info means error and info).
  If journald is missing, the terminal still works.

JSON and TOON

  MCP, ACP, and HTTP POST /mcp accept JSON (default, for humans) and TOON
  (Token-Oriented Object Notation, for language-model tools). Spec:
  https://github.com/toon-format/spec (accessed: 2026-08-27). Media type
  text/toon. HTTP: Content-Type on the request; Accept or format=toon on
  the response. CLI: --toon on mcp, acp, and serve. Stdio default is one
  JSON object per line. --toon uses one TOON document then a blank line.
";

/// Printed on `memex search -h` and `memex search --help`.
const SEARCH_PATTERNS_HELP: &str = "\
Search patterns

  Want               Pattern
  OR                 lizard OR catfooding  or  lizard|catfooding
  AND, any order     lizard AND the  (implicit AND of bare words)
  Phrase             \"hello world\"  or  -F 'hello world'
  Case insensitive   -i  or  /Catfooding/i
  Whole word         -w
  Regex              /<regex>/   flags: i, g, m, s, x

A pattern that is not slash-wrapped is a human query. A single-quoted shell
string is just that query. Bare words join with implicit AND (lookaheads, any
order). AND requires both words in the same packed span (one title or one
message). It does not mean both words appear anywhere in the archive. AND and
OR are case-insensitive keywords. `|` is OR. Double quotes mark a contiguous
phrase; metacharacters in bare words and phrases are escaped. Slash-wrapped
`/pattern/flags` is PCRE2. `i` is case insensitive.
`g` means all matches (search already unique-hits by message). `m` multiline,
`s` dotall, `x` extended. Other flag letters are an error. `-F` keeps the
pattern as a literal, including slashes. `-i` / `-w` still apply when they
do not conflict.

Stdout is the report (`--format human` default, or `json`, or `toon`). Status
is tracing INFO on stderr (searching N archives, then searching k/N path
with a running unique-hit count). Search uses mmap text, grep-searcher, and
grep-pcre2 only. There is no PCRE1 and no rust-regex fallback. Hits are
unique (conversation id, field, entire packed span text). After uniqueness,
identical packed bodies print once, then occurrence rows list archive path,
conversation id, and field. Default print cap is 100 snippet groups
(`-m` / `--max-count`; `0` means no cap).
";

#[derive(Parser)]
#[command(name = "memex", version, about, after_help = AFTER_HELP)]
struct Cli {
    /// Memex data directory. Default is `$HOME/memex`. File key `memex_dir`.
    /// Overrides `memex_dir` in `~/.config/majestic/memex.toml`. Config path is
    /// `MEMEX_CONFIG` or `$XDG_CONFIG_HOME/majestic/memex.toml` (a missing file
    /// uses crate defaults).
    #[arg(long, global = true, value_name = "DIR")]
    memex_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

/// `--service` is the folder (`agents/grok`). `--account` is the username file stem.
#[derive(Args, Debug, Clone)]
struct Scope {
    /// Folder under `$HOME/memex` (`agents/grok`). On ingest, omitted means infer from the input.
    #[arg(long, value_name = "folder")]
    service: Option<String>,
    /// Username file stem. On ingest, omitted means infer (`xUsername`, `$USER`, or directory name).
    #[arg(long, value_name = "name")]
    account: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Ingest Grok dumps, ChatGPT conversations zips/dirs, Facebook DYI zips, X account archives, Telegram result.json, session JSONL, markdown, Obsidian, reports, or session_docs sqlite.
    Ingest {
        /// Output path. Wins over `--service`, `--account`, and shape inference. Error with a home scan.
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[command(flatten)]
        scope: Scope,
        /// Export dirs, zips, backend JSON, ChatGPT conversations, Facebook DYI, X account archives, Telegram result.json, JSONL, markdown, Obsidian vaults, reports, or session_docs sqlite. Omit to scan `$HOME` for known export shapes (max `scan.home_scan_max_depth` directory levels, default 8). Home scan skips system trash unless `scan.skip_system_trash` is false. Extra skips are `scan.skip_directories` in memex.toml (no skip flags).
        inputs: Vec<PathBuf>,
    },
    /// Search mmap archives under `$HOME/memex`. No scope flags means every archive.
    /// Case sensitive unless `-i` (case insensitive). Human query unless `/regex/flags`.
    #[command(after_help = SEARCH_PATTERNS_HELP, after_long_help = SEARCH_PATTERNS_HELP)]
    Search {
        /// Case insensitive (Unicode). File key `search.ignore_case`. The flag overrides the file.
        #[arg(short = 'i', long)]
        ignore_case: bool,
        /// Phrase or literal string.
        #[arg(short = 'F', long)]
        fixed_strings: bool,
        /// Whole word.
        #[arg(short = 'w', long)]
        word_regexp: bool,
        /// Snippet groups printed. File key `search.max_count`. Default 100. `0` means no cap.
        #[arg(short = 'm', long, value_name = "N")]
        max_count: Option<usize>,
        /// Report encoding on stdout. File key `search.format`. Default human. json and toon use the same hit objects as MCP. Status stays on stderr.
        #[arg(long, value_name = "human|json|toon", value_parser = parse_format_arg)]
        format: Option<SearchFormat>,
        /// Human query, or `/regex/flags`. `-F` treats it as a phrase or literal.
        pattern: String,
        #[command(flatten)]
        scope: Scope,
        /// One archive path. Wins over `--service` and `--account`.
        archive: Option<PathBuf>,
    },
    /// Print archive stats.
    Stats {
        #[command(flatten)]
        scope: Scope,
        /// `.majestic` archive path. Wins over `--service` and `--account`.
        archive: Option<PathBuf>,
    },
    /// JSON-RPC 2.0 MCP on stdin/stdout. Default is JSON, one object per line. `--toon` is TOON for language-model tools.
    Mcp {
        /// Respond in TOON (`text/toon`). Default is JSON. Agents may send TOON; humans should keep JSON.
        #[arg(long)]
        toon: bool,
    },
    /// ACP-shaped NDJSON JSON-RPC on stdin/stdout. Default is JSON. `--toon` is TOON for language-model tools.
    Acp {
        /// Respond in TOON (`text/toon`). Default is JSON.
        #[arg(long)]
        toon: bool,
    },
    /// JSON-RPC 2.0 MCP over HTTP. Loopback by default. No auth. JSON default; `--toon` or Accept text/toon for TOON.
    Serve {
        /// Listen address. Default 127.0.0.1:8741 (loopback). File key `serve.bind`.
        #[arg(long, default_value_t = DEFAULT_BIND)]
        bind: SocketAddr,
        /// Respond in TOON (`text/toon`). Default is JSON. Request Content-Type text/toon is also accepted.
        #[arg(long)]
        toon: bool,
    },
    /// Compress uncompressed archives with zstd (default level 3, no dictionary).
    /// File key `compress.level`. Writes `FILE.zst` next to each input. Never deletes the input.
    Compress {
        /// Uncompressed `.majestic` files (leftover `.archive` is allowed).
        #[arg(value_name = "FILE", num_args = 1..)]
        files: Vec<PathBuf>,
    },
    /// Decompress a zstd file. Writes `FILE` without `.zst`. Never deletes the `.zst`.
    Decompress {
        /// `FILE.zst` (for example `foo.majestic.zst`).
        file: PathBuf,
    },
    /// Zstd bench of uncompressed archives. Does not delete sources or Downloads.
    BenchZstd {
        /// Directory of uncompressed `*.archive` / `*.majestic`. Default `$HOME/memex`.
        dir: Option<PathBuf>,
        /// JSONL output path. Default `$HOME/memex/bench-zstd/report.jsonl`.
        #[arg(long)]
        jsonl: Option<PathBuf>,
        /// Markdown report path. Default `$HOME/memex/bench-zstd/report.md`.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Time first vs second ingest of discovered home exports into a temp archive.
        #[arg(long)]
        re_ingest: bool,
    },
}

fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };
    let mut config = match Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    config.overlay_cli(&cli_overlay_from(&cli, &matches));
    if let Ok(home) = home_dir() {
        config.expand_tildes(&home);
    }
    majestic::logging::init_from_directive(Some(config.log.filter.as_str()));
    apply_rayon_threads(config.jobs.rayon_threads);
    match run(cli, config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn apply_rayon_threads(threads: Option<usize>) {
    let Some(threads) = threads.filter(|threads| *threads > 0) else {
        return;
    };
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global();
}

fn is_cli(matches: &clap::ArgMatches, id: &str) -> bool {
    matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine)
}

fn parse_format_arg(value: &str) -> Result<SearchFormat, String> {
    SearchFormat::parse_name(value).map_err(|error| error.to_string())
}

fn fill_scope_overlay(overlay: &mut CliOverlay, scope: &Scope, matches: &clap::ArgMatches) {
    if is_cli(matches, "service") {
        overlay.service = scope.service.clone();
    }
    if is_cli(matches, "account") {
        overlay.account = scope.account.clone();
    }
}

fn cli_overlay_from(cli: &Cli, matches: &clap::ArgMatches) -> CliOverlay {
    let mut overlay = CliOverlay {
        memex_dir: cli.memex_dir.clone(),
        ..CliOverlay::default()
    };
    match &cli.command {
        Command::Ingest {
            output,
            scope,
            inputs,
        } => {
            if let Some(sub) = matches.subcommand_matches("ingest") {
                fill_scope_overlay(&mut overlay, scope, sub);
                if is_cli(sub, "output") {
                    overlay.ingest_output = output.clone();
                }
                if !inputs.is_empty() {
                    overlay.ingest_inputs = Some(inputs.clone());
                }
            }
        }
        Command::Search {
            ignore_case,
            fixed_strings,
            word_regexp,
            max_count,
            format,
            scope,
            archive,
            ..
        } => {
            if let Some(sub) = matches.subcommand_matches("search") {
                fill_scope_overlay(&mut overlay, scope, sub);
                if is_cli(sub, "ignore_case") {
                    overlay.ignore_case = Some(*ignore_case);
                }
                if is_cli(sub, "fixed_strings") {
                    overlay.fixed_strings = Some(*fixed_strings);
                }
                if is_cli(sub, "word_regexp") {
                    overlay.word_regexp = Some(*word_regexp);
                }
                if is_cli(sub, "max_count") {
                    overlay.max_count = *max_count;
                }
                if is_cli(sub, "format") {
                    overlay.search_format = *format;
                }
                if is_cli(sub, "archive") {
                    overlay.search_archive = archive.clone();
                }
            }
        }
        Command::Stats { scope, archive } => {
            if let Some(sub) = matches.subcommand_matches("stats") {
                fill_scope_overlay(&mut overlay, scope, sub);
                if is_cli(sub, "archive") {
                    overlay.stats_archive = archive.clone();
                }
            }
        }
        Command::Serve { bind, .. } => {
            if let Some(sub) = matches.subcommand_matches("serve")
                && is_cli(sub, "bind")
            {
                overlay.serve_bind = Some(bind.to_string());
            }
        }
        Command::BenchZstd {
            dir,
            jsonl,
            report,
            re_ingest,
        } => {
            if let Some(sub) = matches.subcommand_matches("bench-zstd") {
                if dir.is_some() {
                    overlay.bench_zstd_dir = dir.clone();
                }
                if is_cli(sub, "jsonl") {
                    overlay.bench_zstd_jsonl = jsonl.clone();
                }
                if is_cli(sub, "report") {
                    overlay.bench_zstd_report = report.clone();
                }
                if is_cli(sub, "re_ingest") {
                    overlay.bench_zstd_re_ingest = Some(*re_ingest);
                }
            }
        }
        Command::Mcp { .. }
        | Command::Acp { .. }
        | Command::Compress { .. }
        | Command::Decompress { .. } => {}
    }
    overlay
}

fn scope_from_config(config: &Config) -> Scope {
    Scope {
        service: config.scope.service.clone(),
        account: config.scope.account.clone(),
    }
}

fn run(cli: Cli, config: Config) -> Result<(), Error> {
    match cli.command {
        Command::Ingest { .. } => {
            let home = home_dir()?;
            let reports = ingest_from_flags_with_config(
                &home,
                config.ingest.output.as_deref(),
                config.scope.service.as_deref(),
                config.scope.account.as_deref(),
                &config.ingest.inputs,
                &config,
            )?;
            if reports.is_empty() {
                println!("no known export shapes under {}", home.display());
            } else {
                for report in reports {
                    println!("{}", ingest_report_line(&report));
                }
            }
            Ok(())
        }
        Command::Search { pattern, .. } => {
            let exec = SearchExec {
                flags: SearchFlags {
                    ignore_case: config.search.ignore_case,
                    fixed_strings: config.search.fixed_strings,
                    word_regexp: config.search.word_regexp,
                },
                max_count: config.search.max_count,
                format: config.search.format,
            };
            match search_archives_from_flags(
                config.search.archive.clone(),
                scope_from_config(&config),
                &config.memex_dir,
            )? {
                SearchArchives::One(path) => {
                    write_search_exec(&path, &pattern, exec, io::stdout().lock())?;
                }
                SearchArchives::All => {
                    write_search_all_exec(&config.memex_dir, &pattern, exec, io::stdout().lock())?;
                }
            }
            let _ = io::stdout().flush();
            Ok(())
        }
        Command::Stats { .. } => {
            let path = archive_from_flags(
                config.stats.archive.clone(),
                scope_from_config(&config),
                &config.memex_dir,
            )?;
            write_stats(&path, io::stdout().lock())?;
            let _ = io::stdout().flush();
            Ok(())
        }
        Command::Mcp { toon } => mcp::serve_stdio(
            &RpcContext::from_home_and_config(home_dir()?, config).with_prefer_toon(toon),
        ),
        Command::Acp { toon } => acp::serve_stdio(
            &RpcContext::from_home_and_config(home_dir()?, config).with_prefer_toon(toon),
        ),
        Command::Serve { toon, .. } => {
            let bind = config.serve.bind.parse().map_err(|err| {
                Error::InvalidParams(format!("serve.bind is not a listen address: {err}"))
            })?;
            serve::serve(
                RpcContext::from_home_and_config(home_dir()?, config).with_prefer_toon(toon),
                bind,
            )
        }
        Command::Compress { files } => {
            if files.is_empty() {
                return Err(Error::InvalidParams(
                    "pass at least one uncompressed archive path".into(),
                ));
            }
            for file in files {
                let out = zstd_file::compress_to_zst_with_level(&file, config.compress.level)?;
                println!("wrote {}", out.display());
            }
            Ok(())
        }
        Command::Decompress { file } => {
            let out = zstd_file::decompress_zst(&file)?;
            println!("wrote {}", out.display());
            Ok(())
        }
        Command::BenchZstd { .. } => {
            let options = BenchZstdOptions {
                dir: config.bench_zstd.dir.clone(),
                jsonl: config.bench_zstd.jsonl.clone(),
                report: config.bench_zstd.report.clone(),
                re_ingest: config.bench_zstd.re_ingest,
                home: home_dir()?,
            };
            bench_zstd::run_bench_zstd(options)
        }
    }
}

#[cfg(test)]
fn ingest_output_path(
    explicit: Option<PathBuf>,
    scope: Scope,
    inputs: &[PathBuf],
) -> Result<PathBuf, Error> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    resolve_ingest_archive(
        &home_dir()?,
        scope.service.as_deref(),
        scope.account.as_deref(),
        inputs,
    )
}

fn ingest_report_line(report: &IngestReport) -> String {
    let unique = if report
        .output
        .components()
        .any(|part| part.as_os_str() == "telegram")
    {
        "unique chats"
    } else {
        "unique conversations"
    };
    format!(
        "wrote {} ({} export dumps, {} {unique})",
        report.output.display(),
        report.exports,
        report.conversations
    )
}

fn archive_from_flags(
    explicit: Option<PathBuf>,
    scope: Scope,
    memex_dir: &std::path::Path,
) -> Result<PathBuf, Error> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    match (scope.service, scope.account) {
        (Some(service), Some(account)) => scoped_archive_path(memex_dir, &service, &account),
        _ => Err(Error::ArchiveUnspecified),
    }
}

/// One scoped file, or every archive under `$HOME/memex`.
#[derive(Debug, PartialEq, Eq)]
enum SearchArchives {
    All,
    One(PathBuf),
}

fn search_archives_from_flags(
    explicit: Option<PathBuf>,
    scope: Scope,
    memex_dir: &std::path::Path,
) -> Result<SearchArchives, Error> {
    if let Some(path) = explicit {
        return Ok(SearchArchives::One(path));
    }
    match (scope.service, scope.account) {
        (Some(service), Some(account)) => Ok(SearchArchives::One(scoped_archive_path(
            memex_dir, &service, &account,
        )?)),
        (None, None) => Ok(SearchArchives::All),
        _ => Err(Error::ArchiveUnspecified),
    }
}

#[cfg(test)]
mod tracing_init_tests {
    #[test]
    fn tracing_init_does_not_panic() {
        majestic::logging::init_from_directive(Some("info"));
        majestic::logging::init_from_directive(Some("info"));
    }
}

#[cfg(test)]
mod archive_flags_tests {
    use super::{
        Error, Scope, SearchArchives, archive_from_flags, ingest_output_path,
        search_archives_from_flags,
    };
    use std::path::{Path, PathBuf};

    fn scope(service: Option<&str>, account: Option<&str>) -> Scope {
        Scope {
            service: service.map(str::to_owned),
            account: account.map(str::to_owned),
        }
    }

    fn memex_dir() -> &'static Path {
        Path::new("/tmp/majestic-test-memex-dir")
    }

    #[test]
    fn explicit_archive_path_wins_over_service_and_account() {
        let path = PathBuf::from("/tmp/explicit.majestic");
        let got = archive_from_flags(
            Some(path.clone()),
            scope(Some("agents/grok"), Some("personal")),
            memex_dir(),
        )
        .expect("explicit path wins");
        assert_eq!(got, path);
    }

    #[test]
    fn missing_service_account_and_explicit_path_is_error() {
        let err = archive_from_flags(None, scope(None, None), memex_dir()).unwrap_err();
        assert!(
            matches!(err, Error::ArchiveUnspecified),
            "stats still needs --service and --account, or an explicit archive path"
        );
    }

    #[test]
    fn search_without_service_account_or_path_searches_all_archives() {
        let got = search_archives_from_flags(None, scope(None, None), memex_dir())
            .expect("search default is every archive under memex_dir");
        assert_eq!(got, SearchArchives::All);
    }

    #[test]
    fn search_explicit_archive_path_is_one_file() {
        let path = PathBuf::from("/tmp/explicit.majestic");
        let got = search_archives_from_flags(
            Some(path.clone()),
            scope(Some("agents/grok"), Some("personal")),
            memex_dir(),
        )
        .expect("explicit path wins");
        assert_eq!(got, SearchArchives::One(path));
    }

    #[test]
    fn search_service_without_account_is_error() {
        let err = search_archives_from_flags(None, scope(Some("agents/grok"), None), memex_dir())
            .unwrap_err();
        assert!(matches!(err, Error::ArchiveUnspecified));
    }

    #[test]
    fn search_account_without_service_is_error() {
        let err = search_archives_from_flags(None, scope(None, Some("personal")), memex_dir())
            .unwrap_err();
        assert!(matches!(err, Error::ArchiveUnspecified));
    }

    #[test]
    fn service_without_account_is_error() {
        let err =
            archive_from_flags(None, scope(Some("agents/grok"), None), memex_dir()).unwrap_err();
        assert!(matches!(err, Error::ArchiveUnspecified));
    }

    #[test]
    fn account_without_service_is_error() {
        let err = archive_from_flags(None, scope(None, Some("personal")), memex_dir()).unwrap_err();
        assert!(matches!(err, Error::ArchiveUnspecified));
    }

    #[test]
    fn ingest_explicit_output_wins_without_scope() {
        let path = PathBuf::from("/tmp/explicit.majestic");
        let got = ingest_output_path(Some(path.clone()), scope(None, None), &[])
            .expect("explicit -o wins without inferring");
        assert_eq!(got, path);
    }

    #[test]
    fn ingest_report_line_names_telegram_unique_chats() {
        let report = super::IngestReport {
            output: PathBuf::from("/tmp/memex/social/telegram/hunter.majestic"),
            exports: 76,
            conversations: 24,
        };
        let line = super::ingest_report_line(&report);
        assert_eq!(
            line,
            "wrote /tmp/memex/social/telegram/hunter.majestic (76 export dumps, 24 unique chats)"
        );
    }
}

#[cfg(test)]
mod cli_stdio_tests {
    use super::{Cli, Command, Config, DEFAULT_BIND};
    use clap::{CommandFactory, FromArgMatches, Parser};

    #[test]
    fn cli_mcp_and_acp_are_subcommands() {
        let mcp = Cli::try_parse_from(["memex", "mcp"]).expect("memex mcp");
        assert!(matches!(mcp.command, Command::Mcp { toon: false }));
        let acp = Cli::try_parse_from(["memex", "acp"]).expect("memex acp");
        assert!(matches!(acp.command, Command::Acp { toon: false }));
        let toon = Cli::try_parse_from(["memex", "mcp", "--toon"]).expect("memex mcp --toon");
        assert!(matches!(toon.command, Command::Mcp { toon: true }));
    }

    #[test]
    fn cli_search_fixed_strings_and_word_regexp_flags() {
        let cli = Cli::try_parse_from(["memex", "search", "-F", "-w", "-i", "food"])
            .expect("memex search -F -w -i");
        match cli.command {
            Command::Search {
                ignore_case,
                fixed_strings,
                word_regexp,
                pattern,
                format,
                ..
            } => {
                assert!(ignore_case);
                assert!(fixed_strings);
                assert!(word_regexp);
                assert_eq!(pattern, "food");
                assert_eq!(format, None);
            }
            _ => panic!("expected memex search"),
        }
    }

    #[test]
    fn cli_search_format_json() {
        use super::SearchFormat;
        let cli = Cli::try_parse_from(["memex", "search", "--format", "json", "food"])
            .expect("memex search --format json");
        match cli.command {
            Command::Search {
                format, pattern, ..
            } => {
                assert_eq!(format, Some(SearchFormat::Json));
                assert_eq!(pattern, "food");
            }
            _ => panic!("expected memex search"),
        }
        assert!(
            Cli::try_parse_from(["memex", "search", "--format", "xml", "food"]).is_err(),
            "unknown --format must fail"
        );
    }

    #[test]
    fn cli_ingest_without_inputs_is_home_scan() {
        let cli = Cli::try_parse_from(["memex", "ingest"]).expect("memex ingest");
        match cli.command {
            Command::Ingest {
                inputs,
                output,
                scope,
            } => {
                assert!(inputs.is_empty(), "omitted inputs means home scan");
                assert!(output.is_none());
                assert!(scope.service.is_none());
                assert!(scope.account.is_none());
            }
            _ => panic!("expected memex ingest"),
        }
    }

    #[test]
    fn cli_compress_takes_files() {
        let cli = Cli::try_parse_from(["memex", "compress", "foo.majestic"])
            .expect("memex compress FILE");
        match cli.command {
            Command::Compress { files } => {
                assert_eq!(files, vec![std::path::PathBuf::from("foo.majestic")]);
            }
            _ => panic!("expected memex compress"),
        }
    }

    #[test]
    fn cli_decompress_takes_zst() {
        let cli = Cli::try_parse_from(["memex", "decompress", "foo.majestic.zst"])
            .expect("memex decompress FILE.zst");
        match cli.command {
            Command::Decompress { file } => {
                assert_eq!(file, std::path::PathBuf::from("foo.majestic.zst"));
            }
            _ => panic!("expected memex decompress"),
        }
    }

    #[test]
    fn cli_bench_zstd_is_subcommand() {
        let cli = Cli::try_parse_from(["memex", "bench-zstd"]).expect("memex bench-zstd");
        match cli.command {
            Command::BenchZstd { re_ingest, .. } => assert!(!re_ingest),
            _ => panic!("expected memex bench-zstd"),
        }
        let cli = Cli::try_parse_from(["memex", "bench-zstd", "--re-ingest"])
            .expect("memex bench-zstd --re-ingest");
        match cli.command {
            Command::BenchZstd { re_ingest, .. } => assert!(re_ingest),
            _ => panic!("expected memex bench-zstd --re-ingest"),
        }
    }

    #[test]
    fn cli_overrides_config_file() {
        use std::fs;
        use std::path::Path;

        let dir =
            std::env::temp_dir().join(format!("majestic-cli-overlay-main-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("memex.toml");
        fs::write(
            &file,
            "memex_dir = \"/from-file\"\n[search]\nignore_case = true\n",
        )
        .expect("write toml");
        let mut cfg = Config::load_from_path_with_home(&file, Some(Path::new("/home/fake")))
            .expect("load file");
        assert_eq!(cfg.memex_dir.as_os_str(), "/from-file");
        assert!(cfg.search.ignore_case);
        let matches = Cli::command().get_matches_from([
            "memex",
            "--memex-dir",
            "/from-cli",
            "search",
            "pattern",
        ]);
        let cli = Cli::from_arg_matches(&matches).expect("parse");
        cfg.overlay_cli(&super::cli_overlay_from(&cli, &matches));
        assert_eq!(
            cfg.memex_dir.as_os_str(),
            "/from-cli",
            "CLI --memex-dir must win over the file"
        );
        assert!(
            cfg.search.ignore_case,
            "unpassed -i must keep ignore_case from the file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_mcp_default_bind_is_loopback() {
        let cli = Cli::try_parse_from(["memex", "serve"]).expect("memex serve");
        match cli.command {
            Command::Serve { bind, toon } => {
                assert_eq!(bind, DEFAULT_BIND);
                assert_eq!(bind.to_string(), "127.0.0.1:8741");
                assert!(bind.ip().is_loopback());
                assert_eq!(bind.port(), 8741);
                assert!(!toon);
            }
            _ => panic!("expected memex serve"),
        }
    }
}
