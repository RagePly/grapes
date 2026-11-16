//! grapes: like `grep -ri`, but takes several patterns and only reports files
//! in which *every* pattern matches somewhere (in any order).
//!
//! Usage: grapes [-n] [-d DIR]... [--color[=WHEN]] [--] PATTERN...
//!
//! Each DIR (default: the current directory) is searched recursively (symlinks are not followed,
//! same as `grep -r`). Matching is case-insensitive and line-based. For each
//! matching file, every line hit by any of the patterns is printed in file
//! order as `path:line` (or `path:N:line` with -n), with each match highlighted in a 24-bit colour chosen
//! by its pattern's position on the command line (first pattern = red).

use regex::bytes::{Regex, RegexBuilder};
use std::fs;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::process::ExitCode;
use walkdir::WalkDir;

const USAGE: &str = "usage: grapes [-n] [-d DIR]... [--color[=WHEN]] [--] PATTERN...\n\
    Recursively search DIR (default: current directory) for files matching ALL\n\
    patterns (case-insensitive). -d may be repeated. -n (--line-number) prefixes\n\
    each line with its 1-based line number. WHEN is 'auto' (default),\n\
    'always' or 'never'. Use -- before patterns that start with '-'.";

// File name, line number and separator use grep's default GREP_COLORS
// (fn=35 ln=32 se=36).
const C_FILE: &str = "\x1b[35m\x1b[K";
const C_LINENO: &str = "\x1b[32m\x1b[K";
const C_SEP: &str = "\x1b[36m\x1b[K";
const C_END: &str = "\x1b[m\x1b[K";

/// Bold true-colour highlight per pattern, by CLI position; cycles after the
/// last entry. Tuned to be distinct from each other and readable on dark or
/// light backgrounds.
const PALETTE: [(u8, u8, u8); 8] = [
    (255, 85, 85),   // coral red
    (255, 190, 60),  // amber
    (90, 170, 255),  // sky blue
    (130, 220, 90),  // spring green
    (190, 130, 255), // lavender
    (40, 210, 195),  // teal
    (255, 120, 190), // rose pink
    (255, 140, 50),  // tangerine
];

struct Opts {
    color: bool,
    line_number: bool,
    dirs: Vec<String>,
    patterns: Vec<String>,
}

fn parse_args() -> Result<Opts, String> {
    let mut color = None;
    let mut line_number = false;
    let mut dirs = Vec::new();
    let mut patterns = Vec::new();
    let mut opts_done = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if opts_done || !arg.starts_with('-') || arg == "-" {
            patterns.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => opts_done = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-n" | "--line-number" => line_number = true,
            "--color" | "--colour" => color = Some(io::stdout().is_terminal()),
            "-d" => match args.next() {
                Some(dir) => dirs.push(dir),
                None => return Err("option '-d' requires a directory argument".into()),
            },
            _ if arg.starts_with("-d") => dirs.push(arg[2..].to_string()),
            _ => match arg.split_once('=') {
                Some(("--color" | "--colour", when)) => {
                    color = Some(match when {
                        "always" | "yes" | "force" => true,
                        "never" | "no" | "none" => false,
                        "auto" | "tty" | "if-tty" => io::stdout().is_terminal(),
                        _ => return Err(format!("invalid argument '{when}' for --color")),
                    })
                }
                _ => return Err(format!("unknown option '{arg}'")),
            },
        }
    }
    if patterns.is_empty() {
        return Err("no patterns given".into());
    }
    if dirs.is_empty() {
        dirs.push(".".into());
    }
    let color = color.unwrap_or_else(|| io::stdout().is_terminal());
    Ok(Opts { color, line_number, dirs, patterns })
}

/// A line containing at least one match, with sorted, non-overlapping
/// highlight spans `(start, end, pattern_index)`.
struct Hit<'a> {
    /// 1-based line number within the file.
    number: usize,
    line: &'a [u8],
    spans: Vec<(usize, usize, usize)>,
}

/// Returns the matching lines if every regex matches in `data`, else None.
fn search<'a>(data: &'a [u8], regexes: &[Regex]) -> Option<Vec<Hit<'a>>> {
    // Cheap whole-buffer prefilter: if a pattern can't match anywhere in the
    // file, it can't match any single line either.
    if !regexes.iter().all(|re| re.is_match(data)) {
        return None;
    }

    let body = data.strip_suffix(b"\n").unwrap_or(data);
    let mut found = vec![false; regexes.len()];
    let mut hits = Vec::new();
    for (idx, line) in body.split(|&b| b == b'\n').enumerate() {
        let mut hit = false;
        let mut spans = Vec::new();
        for (i, re) in regexes.iter().enumerate() {
            for m in re.find_iter(line) {
                found[i] = true;
                hit = true;
                if !m.is_empty() {
                    spans.push((m.start(), m.end(), i));
                }
            }
        }
        if hit {
            hits.push(Hit { number: idx + 1, line, spans: paint(line.len(), &spans) });
        }
    }
    found.iter().all(|&f| f).then_some(hits)
}

/// Resolves possibly overlapping `(start, end, pattern)` spans into sorted,
/// non-overlapping ones. Where patterns overlap, the earlier pattern on the
/// command line wins; adjacent bytes of the same pattern form one span.
fn paint(len: usize, spans: &[(usize, usize, usize)]) -> Vec<(usize, usize, usize)> {
    let mut owner = vec![usize::MAX; len];
    for &(s, e, p) in spans {
        for o in &mut owner[s..e] {
            *o = (*o).min(p);
        }
    }
    let mut out: Vec<(usize, usize, usize)> = Vec::new();
    for (i, &p) in owner.iter().enumerate() {
        if p == usize::MAX {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.1 == i && last.2 == p => last.1 = i + 1,
            _ => out.push((i, i + 1, p)),
        }
    }
    out
}

fn print_hit(out: &mut impl Write, path: &str, hit: &Hit, opts: &Opts) -> io::Result<()> {
    if !opts.color {
        write!(out, "{path}:")?;
        if opts.line_number {
            write!(out, "{}:", hit.number)?;
        }
        out.write_all(hit.line)?;
        return out.write_all(b"\n");
    }
    write!(out, "{C_FILE}{path}{C_END}{C_SEP}:{C_END}")?;
    if opts.line_number {
        write!(out, "{C_LINENO}{}{C_END}{C_SEP}:{C_END}", hit.number)?;
    }
    let mut pos = 0;
    for &(s, e, p) in &hit.spans {
        let (r, g, b) = PALETTE[p % PALETTE.len()];
        out.write_all(&hit.line[pos..s])?;
        write!(out, "\x1b[1;38;2;{r};{g};{b}m\x1b[K")?;
        out.write_all(&hit.line[s..e])?;
        out.write_all(C_END.as_bytes())?;
        pos = e;
    }
    out.write_all(&hit.line[pos..])?;
    out.write_all(b"\n")
}

fn run(opts: &Opts, regexes: &[Regex], had_error: &mut bool) -> io::Result<bool> {
    let mut out = BufWriter::new(io::stdout().lock());
    let mut any_match = false;

    let walker = opts.dirs.iter().flat_map(|d| WalkDir::new(d).sort_by_file_name());
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                eprintln!("grapes: {err}");
                *had_error = true;
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        // Like grep: "./x" is shown as "x" for the default dir, else DIR/x.
        let display = path.strip_prefix("./").unwrap_or(path).to_string_lossy();
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(err) => {
                eprintln!("grapes: {display}: {err}");
                *had_error = true;
                continue;
            }
        };
        let Some(hits) = search(&data, regexes) else { continue };
        any_match = true;

        if data.contains(&0) {
            writeln!(out, "Binary file {display} matches")?;
            continue;
        }
        for hit in &hits {
            print_hit(&mut out, &display, hit, opts)?;
        }
    }
    out.flush()?;
    Ok(any_match)
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("grapes: {msg}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let mut regexes = Vec::with_capacity(opts.patterns.len());
    for pat in &opts.patterns {
        // multi_line so ^/$ behave per line in the whole-buffer prefilter.
        match RegexBuilder::new(pat).case_insensitive(true).multi_line(true).build() {
            Ok(re) => regexes.push(re),
            Err(err) => {
                eprintln!("grapes: invalid pattern '{pat}': {err}");
                return ExitCode::from(2);
            }
        }
    }

    let mut had_error = false;
    match run(&opts, &regexes, &mut had_error) {
        Ok(_) if had_error => ExitCode::from(2),
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        // Broken pipe (e.g. `grapes foo | head`) is not an error.
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("grapes: {err}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RegexBuilder, paint, search};

    #[test]
    fn earlier_pattern_wins_overlap() {
        // "abcdef": pattern 1 covers b..e, pattern 0 covers c..d.
        assert_eq!(paint(6, &[(1, 5, 1), (2, 4, 0)]), [(1, 2, 1), (2, 4, 0), (4, 5, 1)]);
    }

    #[test]
    fn hits_carry_one_based_line_numbers() {
        let re = |p| RegexBuilder::new(p).case_insensitive(true).build().unwrap();
        let data = b"alpha\nnothing\nBETA\nalpha beta\n";
        let hits = search(data, &[re("alpha"), re("beta")]).unwrap();
        let numbers: Vec<_> = hits.iter().map(|h| h.number).collect();
        assert_eq!(numbers, [1, 3, 4]);
    }

    #[test]
    fn same_pattern_spans_merge() {
        assert_eq!(paint(6, &[(0, 2, 0), (2, 4, 0), (5, 6, 1)]), [(0, 4, 0), (5, 6, 1)]);
    }
}
