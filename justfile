# Do not use `set working-directory := justfile_directory()`; justfile functions are invalid in `set`.
# Recipes cd with justfile_directory(); that function is valid in recipes, not in `set`.
# nice 19 + idle I/O class. Cargo and nextest pick CPU count.
# wild via .cargo/config.toml. Do not hardcode jobs; other machines differ.

cargo := "nice -n 19 ionice -c 3 cargo"

# Bare `just` lists recipes. It must not run fmt or check.
default:
    @just --list

# Format-check, clippy, nextest. Does not install.
check:
    cd {{ justfile_directory() }} && {{ cargo }} fmt --all -- --check
    cd {{ justfile_directory() }} && {{ cargo }} clippy --all-targets -- -D warnings
    cd {{ justfile_directory() }} && {{ cargo }} nextest run

# Runs `just check`, then cargo install a stripped `memex` into ~/.local/bin, then man pages.
install:
    just --justfile "{{ justfile_directory() }}/justfile" check
    cd {{ justfile_directory() }} && {{ cargo }} install --path . --locked --force --root "{{ home_directory() }}/.local"
    strip --strip-unneeded "{{ home_directory() }}/.local/bin/memex"
    just --justfile "{{ justfile_directory() }}/justfile" man

# Copy man pages to ~/.local/share/man/man1. View without installing: man -l man/memex.1
man:
    mkdir -p "{{ home_directory() }}/.local/share/man/man1"
    cp "{{ justfile_directory() }}/man/memex.1" "{{ home_directory() }}/.local/share/man/man1/memex.1"
    cp "{{ justfile_directory() }}/man/memex-search.1" "{{ home_directory() }}/.local/share/man/man1/memex-search.1"
    @echo "Installed man pages. View without installing: man -l {{ justfile_directory() }}/man/memex.1"

# rustfmt the crate. Run from any cwd; recipes cd to this justfile's directory.
fmt:
    cd {{ justfile_directory() }} && {{ cargo }} fmt --all

# Resolve crates.io/menhera versions into Cargo.lock. Does not compile.
update:
    cd {{ justfile_directory() }} && {{ cargo }} update

# Zstd levels 1-22 on uncompressed archives under $HOME/memex. Does not delete sources.
# niced 19 + idle I/O. Writes JSONL and markdown under $HOME/memex/bench-zstd/.
# Temps are gio trash'd after each level. Never deletes Downloads.
bench-zstd *args:
    cd {{ justfile_directory() }} && {{ cargo }} run --release -- bench-zstd {{ args }}
