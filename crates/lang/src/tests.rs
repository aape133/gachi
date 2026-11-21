use super::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[test]
fn a_choice_finds_its_language() {
    assert_eq!(resolve("ko-KR"), "ko-KR");
    assert_eq!(resolve("ko"), "ko-KR", "the language alone");
    assert_eq!(
        resolve("KO_kr.UTF-8"),
        "ko-KR",
        "a locale variable's spelling"
    );
    assert_eq!(
        resolve("en-GB"),
        "en-US",
        "another region of a language we have"
    );
    assert_eq!(resolve("ja"), "ja-JP");
    assert_eq!(resolve("fr-FR"), BASE, "a language we lack: English");
    assert_eq!(resolve("auto"), resolve(system()));
}

#[test]
fn the_macro_speaks_the_thread_language() {
    let name = "Sam";
    assert_eq!(
        with("en-US", || tr!("room.name_host_now", name)),
        "Sam is the host now."
    );
    assert_eq!(
        with("ja-JP", || tr!("room.name_host_now", name = "Sam")),
        "Samさんがホストになりました。"
    );
    assert_eq!(
        with("ko-KR", || tr!("room.name_host_now", name)),
        "이제 Sam님이 방장이에요."
    );
    assert_eq!(with("fr", get), BASE);
}

#[test]
fn holes_are_filled_by_name() {
    let n: &dyn Display = &3;
    assert_eq!(
        fill("{n} of {n}, {other} {", &[("n", n)]),
        "3 of 3, {other} {"
    );
    assert_eq!(fill("no holes", &[]), "no holes");
}

#[test]
fn language_files_read_as_json() {
    let m = parse(r#"{ "a": "x\"y\\n\n", "b": "é🎸" , "c": "" }"#).unwrap();
    assert_eq!(m["a"], "x\"y\\n\n");
    assert_eq!(m["b"], "é🎸");
    assert_eq!(m["c"], "");
    assert!(parse(r#"{"a": "1", "a": "2"}"#).is_err(), "a key twice");
    assert!(parse(r#"{"a": 1}"#).is_err(), "only strings");
    assert!(parse("{}").unwrap().is_empty());
}

/* ── the language files against the code that uses them ── */

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn holes(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        rest = &rest[i + 1..];
        if let Some(j) = rest.find('}') {
            let name = &rest[..j];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.insert(name.to_string());
            }
        }
    }
    out
}

/// Every file of the app's own source that may name a key.
fn sources() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut walk = vec![
        repo().join("app/src"),
        repo().join("crates"),
        repo().join("ui"),
    ];
    while let Some(dir) = walk.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                if !p.ends_with("locales") && !p.ends_with("target") {
                    walk.push(p);
                }
            } else if matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("rs" | "js" | "html")
            ) && !p.ends_with("tests.rs")
            {
                out.push((p.clone(), std::fs::read_to_string(&p).unwrap()));
            }
        }
    }
    out
}

/// The keys a source names: `tr!("…"`, `tr('…'`, `data-i18n…="…"`.
fn named_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for pat in [
        "tr!(\"",
        "tr('",
        "data-i18n=\"",
        "data-i18n-html=\"",
        "data-i18n-title=\"",
        "data-i18n-placeholder=\"",
        "data-i18n-aria=\"",
    ] {
        let close = if pat.ends_with('\'') { '\'' } else { '"' };
        let mut rest = text;
        while let Some(i) = rest.find(pat) {
            rest = &rest[i + pat.len()..];
            if let Some(j) = rest.find(close) {
                out.push(rest[..j].to_string());
            }
        }
    }
    out
}

#[test]
fn every_language_has_every_sentence_with_its_holes() {
    let t = tables();
    let base = &t[0];
    for (i, (code, _)) in FILES.iter().enumerate().skip(1) {
        let missing: Vec<_> = base.keys().filter(|k| !t[i].contains_key(*k)).collect();
        assert!(missing.is_empty(), "{code} lacks {missing:?}");
        let extra: Vec<_> = t[i].keys().filter(|k| !base.contains_key(*k)).collect();
        assert!(extra.is_empty(), "{code} has keys en-US lacks: {extra:?}");
        for (k, v) in &t[i] {
            assert_eq!(
                holes(v),
                holes(&base[k]),
                "{code} {k}: the {{holes}} differ from en-US"
            );
        }
    }
}

#[test]
fn the_list_and_the_folder_agree() {
    let dir = repo().join("ui/locales");
    let files: BTreeSet<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.ends_with(".json") && n != "languages.json")
        .map(|n| n.trim_end_matches(".json").to_string())
        .collect();
    let listed: BTreeSet<String> = languages().map(str::to_string).collect();
    assert_eq!(
        files, listed,
        "ui/locales/languages.json lists every language file, and only those"
    );
}

#[test]
fn the_code_names_only_keys_that_exist_and_every_key_is_used() {
    let base = &tables()[0];
    let srcs = sources();
    for (path, text) in &srcs {
        for k in named_keys(text) {
            assert!(
                base.contains_key(&k),
                "{}: no key \"{k}\" in en-US.json",
                path.display()
            );
        }
    }
    // a key may also be named as a plain string (lists of keys in app.js)
    let all: String = srcs.iter().map(|(_, t)| t.as_str()).collect();
    let unused: Vec<_> = base
        .keys()
        .filter(|k| {
            *k != "language.name"
                && !all.contains(&format!("\"{k}\""))
                && !all.contains(&format!("'{k}'"))
        })
        .collect();
    assert!(unused.is_empty(), "keys nothing uses: {unused:?}");
}
