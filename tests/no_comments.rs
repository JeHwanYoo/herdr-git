use ra_ap_rustc_lexer::{TokenKind, strip_shebang, tokenize};
use std::fs;
use std::path::Path;
use std::process::Command;

fn comment_locations(source: &str) -> Vec<(usize, usize)> {
    let mut offset = strip_shebang(source).unwrap_or(0);
    let mut line = 1;
    let mut column = source[..offset].chars().count() + 1;
    let mut locations = Vec::new();

    for token in tokenize(&source[offset..]) {
        if matches!(
            token.kind,
            TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
        ) {
            locations.push((line, column));
        }

        let end = offset + token.len as usize;
        let text = &source[offset..end];
        line += text.matches('\n').count();
        column = match text.rsplit_once('\n') {
            Some((_, last_line)) => last_line.chars().count() + 1,
            None => column + text.chars().count(),
        };
        offset = end;
    }

    locations
}

#[test]
fn repository_has_no_rust_comments() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.rs",
        ])
        .output()
        .expect("Git must be available to list Rust source files");
    assert!(
        output.status.success(),
        "Cannot list Rust source files: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut violations = Vec::new();
    for entry in output.stdout.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(entry).expect("Rust source paths must be UTF-8");
        let absolute_path = root.join(path);
        if !absolute_path.exists() {
            continue;
        }
        let source = fs::read_to_string(&absolute_path)
            .unwrap_or_else(|error| panic!("Cannot read {path}: {error}"));
        for (line, column) in comment_locations(&source) {
            violations.push(format!(
                "{path}:{line}:{column}: Rust comments are not allowed"
            ));
        }
    }

    assert!(violations.is_empty(), "\n{}", violations.join("\n"));
}

#[test]
fn detects_regular_and_documentation_comments() {
    for source in [
        "// comment",
        "/// documentation",
        "//! documentation",
        "/* comment */",
        "/** documentation */",
        "/*! documentation */",
        "/* outer /* inner */ comment */",
        "/* unterminated",
    ] {
        assert_eq!(comment_locations(source), [(1, 1)], "{source}");
    }
}

#[test]
fn allows_comment_markers_in_literals() {
    let source = r####"
        let url = "https://example.com";
        let escaped = "\" // /* \\";
        let raw = r###"" // /*"###;
        let bytes = b"// /*";
        let raw_bytes = br##"" // /*"##;
        let c_string = c"// /*";
        let raw_c_string = cr##"" // /*"##;
        let slash = '/';
        let quote = '\'';
        let byte = b'/';
        fn borrow<'a>(value: &'a str) -> &'a str { value }
    "####;

    assert!(comment_locations(source).is_empty());
}

#[test]
fn reports_locations_after_literals_and_multiline_comments() {
    let source = "let 한글 = r#\"//\"#; // first\r\n/* second\n nested /* third */ */ // fourth";
    assert_eq!(comment_locations(source), [(1, 19), (2, 1), (3, 24)]);
}

#[test]
fn allows_rust_script_shebangs() {
    let source = "#!/usr/bin/env rust-script // option\n// comment";
    assert_eq!(comment_locations(source), [(2, 1)]);
}
