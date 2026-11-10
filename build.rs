//! Lists the files of `web/dist`, which `check.sh` builds before the binary, for
//! `src/listen/review.rs` to embed under the paths the page asks for them by.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=web/dist");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("web/dist");
    let mut table = String::from("&[");
    list(&root, &root, &mut table);
    table.push(']');
    let out = std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR");
    fs::write(Path::new(&out).join("assets.rs"), table).expect("OUT_DIR is writable");
}

fn list(root: &Path, dir: &Path, table: &mut String) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{}: {error}; build web/ first", dir.display()))
        .map(|entry| entry.expect("a readable web/dist").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            list(root, &path, table);
            continue;
        }
        let file = path.to_str().expect("a UTF-8 path");
        let served = format!(
            "/{}",
            path.strip_prefix(root)
                .expect("a file under web/dist")
                .display()
        );
        let kind = match path.extension().and_then(|extension| extension.to_str()) {
            Some("html") => "text/html; charset=utf-8",
            Some("js") => "text/javascript; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            _ => panic!("{}: a type the daemon cannot serve", path.display()),
        };
        write!(table, "({served:?}, {kind:?}, include_bytes!({file:?})),")
            .expect("a String takes writes");
    }
}
