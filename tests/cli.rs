use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Creates a fresh fixture directory with the given files.
fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for (file, body) in files {
        fs::write(dir.join(file), body).unwrap();
    }
    dir
}

fn grapes(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grapes")).args(args).output().unwrap();
    (out.status.code().unwrap(), String::from_utf8(out.stdout).unwrap())
}

#[test]
fn line_numbers() {
    let dir = fixture("line_numbers", &[("a.txt", "foo\nskip\nbar foo\n"), ("b.txt", "foo only\n")]);
    let d = dir.to_str().unwrap();
    let a = dir.join("a.txt");
    let a = a.to_str().unwrap();

    let (code, out) = grapes(&["--color=never", "-d", d, "foo", "bar"]);
    assert_eq!(code, 0);
    assert_eq!(out, format!("{a}:foo\n{a}:bar foo\n"));

    for flag in ["-n", "--line-number"] {
        let (code, out) = grapes(&["--color=never", flag, "-d", d, "foo", "bar"]);
        assert_eq!(code, 0);
        assert_eq!(out, format!("{a}:1:foo\n{a}:3:bar foo\n"));
    }

    let (_, out) = grapes(&["--color=always", "-n", "-d", d, "foo", "bar"]);
    assert!(out.contains("\x1b[32m\x1b[K3\x1b[m\x1b[K\x1b[36m\x1b[K:\x1b[m\x1b[K"), "{out:?}");

    let (code, out) = grapes(&["-n", "-d", d, "nomatch"]);
    assert_eq!((code, out.as_str()), (1, ""));
}
