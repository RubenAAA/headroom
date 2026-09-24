//! Guard the corporate-CA invariant at the construction boundary.
//!
//! A direct reqwest builder silently ignores SSL_CERT_FILE,
//! REQUESTS_CA_BUNDLE, and NODE_EXTRA_CA_CERTS. All production clients must
//! start from ssl_context's configured constructors.

use std::path::{Path, PathBuf};

fn rust_files(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn outbound_clients_use_the_tls_aware_constructors() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let ssl_context = source_root.join("ssl_context.rs");
    let mut files = Vec::new();
    rust_files(&source_root, &mut files);

    let forbidden = [
        "reqwest::Client::builder()",
        "reqwest::blocking::Client::builder()",
    ];
    let mut violations = Vec::new();
    for file in files {
        if file == ssl_context {
            continue;
        }
        let source = std::fs::read_to_string(&file).expect("read Rust source");
        for needle in forbidden {
            if source.contains(needle) {
                violations.push(format!("{} contains {needle}", file.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "outbound reqwest clients bypass the corporate-CA policy:\n{}",
        violations.join("\n")
    );
}
