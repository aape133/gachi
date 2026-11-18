//! Embeds the language files (ui/locales, listed in languages.json) into the build.

use std::{env, fs, path::PathBuf};

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../ui/locales");
    println!("cargo:rerun-if-changed={}", dir.display());
    let list = fs::read_to_string(dir.join("languages.json")).expect("ui/locales/languages.json");
    let codes: Vec<String> = list
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    assert!(codes.first().map(String::as_str) == Some("en-US"), "languages.json starts with en-US");
    let mut out = String::from("pub(crate) static FILES: &[(&str, &str)] = &[\n");
    for code in &codes {
        let path = dir.join(format!("{code}.json"));
        assert!(path.is_file(), "{} is listed in languages.json but missing", path.display());
        println!("cargo:rerun-if-changed={}", path.display());
        out += &format!("    ({code:?}, include_str!({:?})),\n", path.display().to_string());
    }
    out += "];\n";
    fs::write(PathBuf::from(env::var("OUT_DIR").unwrap()).join("files.rs"), out).unwrap();
}
