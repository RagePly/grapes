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
use ignore::{WalkBuilder, WalkState};
use memchr::{memchr, memrchr};
use memmap2::Mmap;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::ops::Range;
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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
const MATCH_COLORS: [&str; 8] = [
    "\x1b[1;38;2;255;85;85m\x1b[K",   // coral red
    "\x1b[1;38;2;255;190;60m\x1b[K",  // amber
    "\x1b[1;38;2;90;170;255m\x1b[K",  // sky blue
    "\x1b[1;38;2;130;220;90m\x1b[K",  // spring green
    "\x1b[1;38;2;190;130;255m\x1b[K", // lavender
    "\x1b[1;38;2;40;210;195m\x1b[K",  // teal
    "\x1b[1;38;2;255;120;190m\x1b[K", // rose pink
    "\x1b[1;38;2;255;140;50m\x1b[K",  // tangerine
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

/// A highlight span within a line: `(start, end, pattern_index)`.
type Span = (usize, usize, usize);

/// A line containing at least one match.
struct Hit<'a> {
    /// 1-based line number within the file (0 if not computed).
    number: usize,
    line: &'a [u8],
    /// Index range into `Matches::spans`: sorted, non-overlapping spans.
    spans: Range<usize>,
}

/// All matching lines of a file. Spans live in one shared vector so that
/// matching lines don't each need an allocation.
struct Matches<'a> {
    hits: Vec<Hit<'a>>,
    spans: Vec<Span>,
}

/// A file is searched in blocks of about this size, each extended to the end
/// of its last line. Small enough that counting a block's lines right after
/// searching it still finds the block in the CPU cache.
const BLOCK: usize = 256 * 1024;

/// A raw match: `(line_start, line_end, start, end, pattern)`, with the line
/// bounds relative to the block and start/end relative to the line.
type RawMatch = (usize, usize, usize, usize, usize);

/// Returns the matching lines if every regex matches in `data`, else None.
/// Line numbers are only computed when `numbers` is set (else they are 0).
fn search<'a>(data: &'a [u8], regexes: &[Regex], numbers: bool) -> Option<Matches<'a>> {
    let body = data.strip_suffix(b"\n").unwrap_or(data);
    // With several patterns, most files fail on one of them: bail out early.
    if regexes.len() > 1 && !regexes.iter().all(|re| re.is_match(body)) {
        return None;
    }

    let mut found = vec![false; regexes.len()];
    let mut out = Matches { hits: Vec::new(), spans: Vec::new() };
    let mut raw = Vec::new();
    let (mut number, mut counted) = (1, 0);
    let mut bs = 0;
    while bs <= body.len() {
        let be = match memchr(b'\n', body.get(bs + BLOCK..).unwrap_or_default()) {
            Some(i) => bs + BLOCK + i,
            None => body.len(),
        };
        let block = &body[bs..be];
        raw.clear();
        search_block(block, regexes, &mut raw, &mut found);
        if regexes.len() > 1 {
            raw.sort_by_key(|r| r.0); // stable: keeps each line's spans in pattern order
        }

        let mut k = 0;
        while k < raw.len() {
            let (ls, le) = (raw[k].0, raw[k].1);
            let j = k + raw[k..].iter().take_while(|r| r.0 == ls).count();
            if numbers {
                number += count_newlines(&body[counted..bs + ls]);
                counted = bs + ls;
            }
            let first = out.spans.len();
            let mut prev_end = 0;
            for &(_, _, s, e, p) in &raw[k..j] {
                if s == e {
                    continue;
                }
                if s < prev_end {
                    // Spans overlap or are out of order: resolve them properly.
                    out.spans.truncate(first);
                    let spans: Vec<Span> = raw[k..j].iter().map(|r| (r.2, r.3, r.4)).collect();
                    out.spans.extend(paint(le - ls, &spans));
                    break;
                }
                let has_prev = out.spans.len() > first;
                match out.spans.last_mut() {
                    // Touching spans of the same pattern form one span.
                    Some(last) if has_prev && last.1 == s && last.2 == p => last.1 = e,
                    _ => out.spans.push((s, e, p)),
                }
                prev_end = e;
            }
            let number = if numbers { number } else { 0 };
            let line = &block[ls..le];
            out.hits.push(Hit { number, line, spans: first..out.spans.len() });
            k = j;
        }
        if numbers {
            number += count_newlines(&body[counted..be]);
            counted = be;
        }
        bs = be + 1;
    }
    found.iter().all(|&f| f).then_some(out)
}

/// Finds all per-line matches of all regexes in `block` (whole lines only).
///
/// Each regex scans the whole block at once, which lets the regex engine run
/// its fast literal prefilters across it. A match that stays within one line
/// is the same match a per-line search would find, so it is used as is.
/// Empty matches and matches that span a newline are rare; for those, every
/// line the match touches is searched again on its own, and the scan resumes
/// at the next line. That is exact: everything before the leftmost match has
/// no match, and every line the match touches gets a per-line search.
fn search_block(block: &[u8], regexes: &[Regex], raw: &mut Vec<RawMatch>, found: &mut [bool]) {
    let line_start = |at: usize| memrchr(b'\n', &block[..at]).map_or(0, |i| i + 1);
    let line_end = |at: usize| memchr(b'\n', &block[at..]).map_or(block.len(), |i| at + i);
    for (i, re) in regexes.iter().enumerate() {
        let before = raw.len();
        let mut line: Option<(usize, usize)> = None; // line of the last match
        let mut pos = 0;
        while pos <= block.len() {
            let Some(m) = re.find_at(block, pos) else { break };
            let (s, e) = (m.start(), m.end());
            if s < e && memchr(b'\n', &block[s..e]).is_none() {
                let (ls, le) = match line {
                    Some((ls, le)) if s < le => (ls, le),
                    _ => *line.insert((line_start(s), line_end(e))),
                };
                raw.push((ls, le, s - ls, e - ls, i));
                pos = e;
                continue;
            }
            let ls = line_start(s);
            let le = line_end(if s < e { e - 1 } else { s });
            // Drop this pattern's matches already taken from these lines.
            while raw.len() > before && raw.last().is_some_and(|r| r.0 >= ls) {
                raw.pop();
            }
            let mut start = ls;
            for l in block[ls..le].split(|&b| b == b'\n') {
                let end = start + l.len();
                raw.extend(re.find_iter(l).map(|mm| (start, end, mm.start(), mm.end(), i)));
                start = end + 1;
            }
            line = None;
            pos = le + 1;
        }
        found[i] |= raw.len() > before;
    }
}

/// Counts newlines. Written so that LLVM vectorises it: summing byte
/// comparisons in u8 lanes per 255-byte block beats `memchr_iter().count()` on
/// text with short lines.
fn count_newlines(bytes: &[u8]) -> usize {
    bytes
        .chunks(255)
        .map(|c| c.iter().fold(0u8, |n, &b| n + (b == b'\n') as u8) as usize)
        .sum()
}

/// Resolves possibly overlapping `(start, end, pattern)` spans into sorted,
/// non-overlapping ones. Where patterns overlap, the earlier pattern on the
/// command line wins; adjacent bytes of the same pattern form one span.
fn paint(len: usize, spans: &[Span]) -> Vec<Span> {
    let mut owner = vec![usize::MAX; len];
    for &(s, e, p) in spans {
        for o in &mut owner[s..e] {
            *o = (*o).min(p);
        }
    }
    let mut out: Vec<Span> = Vec::new();
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

fn print_hit(out: &mut impl Write, path: &str, hit: &Hit, spans: &[Span], opts: &Opts) -> io::Result<()> {
    if !opts.color {
        out.write_all(path.as_bytes())?;
        out.write_all(b":")?;
        if opts.line_number {
            write!(out, "{}:", hit.number)?;
        }
        out.write_all(hit.line)?;
        return out.write_all(b"\n");
    }
    for part in [C_FILE, path, C_END, C_SEP, ":", C_END] {
        out.write_all(part.as_bytes())?;
    }
    if opts.line_number {
        write!(out, "{C_LINENO}{}{C_END}{C_SEP}:{C_END}", hit.number)?;
    }
    let mut pos = 0;
    for &(s, e, p) in spans {
        out.write_all(&hit.line[pos..s])?;
        out.write_all(MATCH_COLORS[p % MATCH_COLORS.len()].as_bytes())?;
        out.write_all(&hit.line[s..e])?;
        out.write_all(C_END.as_bytes())?;
        pos = e;
    }
    out.write_all(&hit.line[pos..])?;
    out.write_all(b"\n")
}

/// State shared by all search threads.
struct Shared<'a> {
    opts: &'a Opts,
    any_match: AtomicBool,
    had_error: AtomicBool,
    /// Set when writing to stdout failed; all threads stop.
    quit: AtomicBool,
    write_error: Mutex<Option<io::Error>>,
}

/// Per-thread searcher. Output of each file is formatted into `out` and
/// written to stdout in whole-file chunks, so lines of different files never
/// interleave.
struct Worker<'s, 'a> {
    shared: &'s Shared<'a>,
    /// Per-thread clones: a shared Regex's scratch-space pool is contended.
    regexes: Vec<Regex>,
    data: Vec<u8>,
    out: Vec<u8>,
}

const FLUSH_AT: usize = 64 * 1024;
/// Files at least this large are memory-mapped instead of read.
const MMAP_AT: u64 = 4 * 1024 * 1024;

impl Worker<'_, '_> {
    fn flush(&mut self) {
        if self.out.is_empty() || self.shared.quit.load(Ordering::Relaxed) {
            return;
        }
        if let Err(err) = io::stdout().lock().write_all(&self.out) {
            self.shared.quit.store(true, Ordering::Relaxed);
            self.shared.write_error.lock().unwrap().get_or_insert(err);
        }
        self.out.clear();
    }

    fn error(&self, msg: impl std::fmt::Display) {
        eprintln!("grapes: {msg}");
        self.shared.had_error.store(true, Ordering::Relaxed);
    }

    /// Reads a file: large files are memory-mapped (returned), smaller ones
    /// are read into the reusable `self.data` buffer (returns None).
    fn load(&mut self, path: &std::path::Path) -> io::Result<Option<Mmap>> {
        let mut file = fs::File::open(path)?;
        let len = file.metadata()?.len();
        if len >= MMAP_AT {
            // SAFETY: the map is read-only and dropped before the next file. If
            // another process truncates the file meanwhile we may get SIGBUS;
            // ripgrep accepts the same trade-off.
            return unsafe { Mmap::map(&file) }.map(Some);
        }
        self.data.clear();
        file.read_to_end(&mut self.data)?;
        Ok(None)
    }

    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                self.error(err);
                return WalkState::Continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            return WalkState::Continue;
        }
        let path = entry.path();
        // Like grep: "./x" is shown as "x" for the default dir, else DIR/x.
        let display = path.strip_prefix("./").unwrap_or(path).to_string_lossy();
        let mmap = match self.load(path) {
            Ok(m) => m,
            Err(err) => {
                self.error(format_args!("{display}: {err}"));
                return WalkState::Continue;
            }
        };
        let data = mmap.as_deref().unwrap_or(&self.data);
        let opts = self.shared.opts;
        let Some(matches) = search(data, &self.regexes, opts.line_number) else {
            return WalkState::Continue;
        };
        self.shared.any_match.store(true, Ordering::Relaxed);

        if memchr::memchr(0, data).is_some() {
            writeln!(self.out, "Binary file {display} matches").unwrap();
        } else {
            for hit in &matches.hits {
                let spans = &matches.spans[hit.spans.clone()];
                print_hit(&mut self.out, &display, hit, spans, opts).unwrap(); // Vec can't fail
            }
        }
        drop(matches);
        if self.out.len() >= FLUSH_AT {
            self.flush();
        }
        if self.shared.quit.load(Ordering::Relaxed) { WalkState::Quit } else { WalkState::Continue }
    }
}

impl Drop for Worker<'_, '_> {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Searches all dirs in parallel. Returns whether any file matched.
fn run(opts: &Opts, regexes: &[Regex], had_error: &mut bool) -> io::Result<bool> {
    let mut walk = WalkBuilder::new(&opts.dirs[0]);
    for dir in &opts.dirs[1..] {
        walk.add(dir);
    }
    // Search everything, like grep -r: no ignore files, hidden files included.
    walk.standard_filters(false).follow_links(false);

    let shared = Shared {
        opts,
        any_match: AtomicBool::new(false),
        had_error: AtomicBool::new(false),
        quit: AtomicBool::new(false),
        write_error: Mutex::new(None),
    };
    walk.build_parallel().run(|| {
        let mut worker = Worker {
            shared: &shared,
            regexes: regexes.to_vec(),
            data: Vec::new(),
            out: Vec::new(),
        };
        Box::new(move |entry| worker.visit(entry))
    });

    *had_error = shared.had_error.into_inner();
    match shared.write_error.into_inner().unwrap() {
        Some(err) => Err(err),
        None => Ok(shared.any_match.into_inner()),
    }
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
        // multi_line so ^/$ behave per line in the whole-buffer search.
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
        let m = search(data, &[re("alpha"), re("beta")], true).unwrap();
        let numbers: Vec<_> = m.hits.iter().map(|h| h.number).collect();
        assert_eq!(numbers, [1, 3, 4]);
        let spans: Vec<_> = m.hits.iter().map(|h| &m.spans[h.spans.clone()]).collect();
        assert_eq!(spans, [&[(0, 5, 0)][..], &[(0, 4, 1)], &[(0, 5, 0), (6, 10, 1)]]);
    }

    /// The reference semantics: search each line on its own.
    type Lines = Vec<(Vec<u8>, Vec<(usize, usize)>)>;

    fn per_line(data: &[u8], pat: &str) -> Lines {
        let re = RegexBuilder::new(pat).case_insensitive(true).build().unwrap();
        let body = data.strip_suffix(b"\n").unwrap_or(data);
        body.split(|&b| b == b'\n')
            .filter(|l| re.is_match(l))
            .map(|l| {
                // Non-empty matches, with touching ones joined.
                let mut spans: Vec<(usize, usize)> = Vec::new();
                for m in re.find_iter(l).filter(|m| !m.is_empty()) {
                    match spans.last_mut() {
                        Some(last) if last.1 == m.start() => last.1 = m.end(),
                        _ => spans.push((m.start(), m.end())),
                    }
                }
                (l.to_vec(), spans)
            })
            .collect()
    }

    #[test]
    fn block_boundaries_keep_lines_and_numbers() {
        // ~3.5 blocks of varied-length lines. Every 7th line matches `a\s*b`;
        // every 3rd ends in "a" so that, with the next line's leading "b", the
        // whole-buffer search also finds matches across newlines.
        let mut data = Vec::new();
        let mut n = 0;
        while data.len() < 3 * super::BLOCK + super::BLOCK / 2 {
            n += 1;
            let tail = if n % 7 == 0 { "a b" } else if n % 3 == 0 { "a" } else { "" };
            data.extend(format!("b{n} {} {tail}\n", "x".repeat(n % 97)).as_bytes());
        }
        let body = data.strip_suffix(b"\n").unwrap();
        let re = RegexBuilder::new(r"a\s*b").case_insensitive(true).multi_line(true).build();
        let re = re.unwrap();
        for numbers in [false, true] {
            let m = search(&data, std::slice::from_ref(&re), numbers).unwrap();
            let got: Vec<_> = m.hits.iter().map(|h| (h.number, h.line)).collect();
            let want: Vec<_> = (body.split(|&b| b == b'\n').enumerate())
                .filter(|(_, l)| l.ends_with(b"a b"))
                .map(|(i, l)| (if numbers { i + 1 } else { 0 }, l))
                .collect();
            assert!(want.len() > 1000);
            assert_eq!(got, want);
        }
    }

    #[test]
    fn matches_per_line_semantics() {
        let data = b"a b\na\nb\nxa  b a b\n\nab\n  \nzzz\naab b\n";
        let pats = [r"a\s+b", r"a\s*b", r"\s+", r"b?", r"^", r"$", r"[^z]", r"^\s*$", r"b\n?a", r"a", r"b?a"];
        for pat in pats {
            let re = RegexBuilder::new(pat).case_insensitive(true).multi_line(true).build().unwrap();
            let got: Vec<_> = search(data, &[re], false).map_or(vec![], |m| {
                m.hits
                    .iter()
                    .map(|h| {
                        let spans = m.spans[h.spans.clone()].iter().map(|s| (s.0, s.1));
                        (h.line.to_vec(), spans.collect())
                    })
                    .collect()
            });
            assert_eq!(got, per_line(data, pat), "pattern {pat:?}");
        }
    }

    #[test]
    fn same_pattern_spans_merge() {
        assert_eq!(paint(6, &[(0, 2, 0), (2, 4, 0), (5, 6, 1)]), [(0, 4, 0), (5, 6, 1)]);
    }
}
