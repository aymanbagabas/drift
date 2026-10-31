//! Generate `THEME_NAMES` from the bundled `themes/*.toml` files so the picker
//! list and the themes we actually ship can't drift apart — the file names are
//! the single source of truth. `ansi` sorts first (the default, and the only
//! entry without syntax highlighting); the rest stay alphabetical.

use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=themes");

    let mut names: Vec<String> = fs::read_dir("themes")
        .expect("themes/ directory should exist")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? == "toml" {
                Some(path.file_stem()?.to_str()?.to_owned())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    // Stable sort keeps the alphabetical order within each group; `false` (ansi)
    // sorts before `true` (everything else), so ansi lands first.
    names.sort_by_key(|n| n != "ansi");

    let items: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
    let code = format!(
        "/// Built-in theme names, generated at build time from `themes/*.toml`.\n\
         pub const THEME_NAMES: &[&str] = &[{}];\n",
        items.join(", ")
    );

    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("theme_names.rs");
    fs::write(out, code).expect("write generated theme_names.rs");
}
