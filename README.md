# grapes

Like `grep -ri`, but for several patterns at once: **grapes** recursively
searches directories and reports only the files in which *every* pattern
matches somewhere, in any order.

```console
$ grapes TODO unsafe
src/ffi.rs:// TODO: audit this
src/ffi.rs:    unsafe { libc::free(ptr) }
```

Here `src/ffi.rs` is listed because it contains both `TODO` and `unsafe`. A
file that contains only one of them is skipped.

## Installation

Requires a Rust toolchain that supports edition 2024 (Rust 1.85+).

```sh
cargo install --path .
# or just build it:
cargo build --release   # binary at target/release/grapes
```

## Usage

```
grapes [-n] [-d DIR]... [--color[=WHEN]] [--] PATTERN...
```

| Option            | Description                                                                 |
|-------------------|-----------------------------------------------------------------------------|
| `-d DIR`, `-dDIR` | Directory to search recursively. Can be repeated. Defaults to `.`.          |
| `-n`, `--line-number` | Prefix each line with its 1-based line number: `path:N:line`. |
| `--color=WHEN`    | Highlight output: `auto` (default; only when stdout is a terminal), `always` or `never`. `--colour` is also accepted. |
| `--`              | End of options. Use it before patterns that start with `-`.                 |
| `-h`, `--help`    | Show usage.                                                                 |

### Matching rules

- Patterns are regular expressions using the
  [`regex`](https://docs.rs/regex) crate's syntax. They are always
  **case-insensitive**.
- Matching is **line-based**: each pattern has to match within a single line,
  but different patterns can match on different lines.
- A file matches when **all** patterns match somewhere in it.
- For each matching file, every line that any pattern hits is printed in file
  order as `path:line`, or as `path:N:line` with `-n`.
- With colour on, each match is highlighted in bold 24-bit colour chosen by
  its pattern's position on the command line: 1st coral red, 2nd sky blue,
  3rd spring green, 4th amber, 5th lavender, 6th teal, 7th rose pink, 8th
  tangerine. After the 8th pattern the colours start over. Where matches of
  different patterns overlap, the pattern given earlier wins. The colours
  need a terminal with true-colour support.
- If a matching file contains NUL bytes, it is treated as binary and grapes
  prints `Binary file PATH matches` in place of its lines.
- Symlinks inside the tree are not followed, the same as `grep -r`. Hidden
  files and directories (e.g. `.git`) are searched, and `.gitignore` is not
  applied.
- Files are visited in sorted order, so the output is the same on every run.

### Exit status

| Code | Meaning                                                        |
|------|----------------------------------------------------------------|
| `0`  | At least one file matched all patterns.                        |
| `1`  | No file matched.                                               |
| `2`  | Bad usage, an invalid pattern, or an I/O error (e.g. a missing directory or an unreadable file). |

### Examples

```sh
# Files that mention both "parse" and "error", anywhere under src/ and tests/
grapes -d src -d tests parse error

# Same, with line numbers
grapes -n -d src -d tests parse error

# Regexes work; patterns starting with '-' need --
grapes 'fn \w+_test' -- '-> Result'

# Pipe-friendly: no colour unless forced
grapes --color=always config load | less -R
```
