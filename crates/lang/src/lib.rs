//! The language gachi speaks to its user.
//!
//! Every sentence lives in the language files, `ui/locales/<code>.json` (one per language,
//! listed in `languages.json`; `en-US` is the base every other one falls back to). The app
//! and its window share them: Rust looks a key up with [`tr!`], the page with `tr()` in
//! `ui/i18n.js`.
//!
//! ```
//! let name = "Sam";
//! let s = gachi_lang::tr!("room.name_connected_again", name);
//! # let _ = s;
//! ```
//!
//! The user picks a language in the settings, or leaves it to Windows' display language.
//! Logs stay in English and are not translated.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/files.rs"));

/// The language every other one falls back to.
pub const BASE: &str = "en-US";

/// The languages there are, as their codes ("en-US", "ko-KR", …), base first.
pub fn languages() -> impl Iterator<Item = &'static str> {
    FILES.iter().map(|(code, _)| *code)
}

/// The language files, parsed once.
fn tables() -> &'static [HashMap<String, String>] {
    static TABLES: OnceLock<Vec<HashMap<String, String>>> = OnceLock::new();
    TABLES.get_or_init(|| {
        FILES
            .iter()
            .map(|(code, text)| parse(text).unwrap_or_else(|e| panic!("ui/locales/{code}.json: {e}")))
            .collect()
    })
}

/// The settings' choice (index into FILES + 1; 0: follow the system).
static CHOSEN: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// a test's own language, whatever the others pick (see [`with`])
    static HERE: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The language a choice means: a code we have ("ko-KR"), else one of the same language
/// ("ko", "en-GB" → "ko-KR", "en-US"), else English. "auto" or "": the system's.
pub fn resolve(choice: &str) -> &'static str {
    FILES[index(choice)].0
}

fn index(choice: &str) -> usize {
    let choice = if choice.is_empty() || choice.eq_ignore_ascii_case("auto") { system() } else { choice };
    let want = choice.split('.').next().unwrap_or("").replace('_', "-");
    let lang = |s: &str| s.split('-').next().unwrap_or("").to_ascii_lowercase();
    let codes = || FILES.iter().map(|(c, _)| *c).enumerate();
    codes()
        .find(|(_, c)| c.eq_ignore_ascii_case(&want))
        .or_else(|| codes().find(|(_, c)| lang(c) == lang(&want)))
        .map_or(0, |(i, _)| i)
}

/// The language spoken now.
pub fn get() -> &'static str {
    FILES[current()].0
}

fn current() -> usize {
    HERE.with(Cell::get).unwrap_or_else(|| match CHOSEN.load(Ordering::Relaxed) {
        0 => index(""),
        n => n - 1,
    })
}

/// The settings' choice: a language code, or "auto" (anything we don't have works too:
/// see [`resolve`]).
pub fn choose(choice: &str) {
    let auto = choice.is_empty() || choice.eq_ignore_ascii_case("auto");
    CHOSEN.store(if auto { 0 } else { index(choice) + 1 }, Ordering::Relaxed);
}

/// Runs `f` with this thread speaking `code` (for tests, which run side by side).
pub fn with<R>(code: &str, f: impl FnOnce() -> R) -> R {
    let before = HERE.with(|h| h.replace(Some(index(code))));
    let out = f();
    HERE.with(|h| h.set(before));
    out
}

/// Windows' display language ("ko-KR"…; elsewhere the locale variables), else English.
pub fn system() -> &'static str {
    static SYSTEM: OnceLock<String> = OnceLock::new();
    SYSTEM.get_or_init(|| detect().unwrap_or_else(|| BASE.to_string()))
}

#[cfg(windows)]
fn detect() -> Option<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
        fn LCIDToLocaleName(locale: u32, name: *mut u16, len: i32, flags: u32) -> i32;
    }
    let mut buf = [0u16; 85];
    // SAFETY: plain calls; the buffer is as long as we say
    let n = unsafe { LCIDToLocaleName(GetUserDefaultUILanguage() as u32, buf.as_mut_ptr(), buf.len() as i32, 0) };
    (n > 1).then(|| String::from_utf16_lossy(&buf[..n as usize - 1]))
}

#[cfg(not(windows))]
fn detect() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty() && v != "C"))
}

/// A sentence in the language spoken now (English if that language lacks it; the key
/// itself if no file has it).
pub fn text(key: &'static str) -> &'static str {
    let t = tables();
    t[current()].get(key).or_else(|| t[0].get(key)).map_or(key, String::as_str)
}

/// `{name}` holes filled in; one without a value stays as written.
pub fn fill(s: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}').and_then(|close| args.iter().find(|(k, _)| *k == &after[..close]).map(|(_, v)| (close, v))) {
            Some((close, v)) => {
                out.push_str(&v.to_string());
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// For the window: every language file, the saved choice and the system's language, set
/// on `window` before the page's own scripts run (`ui/i18n.js` reads them).
pub fn page_script(choice: &str) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let mut js = String::from("window.GACHI_LOCALES={");
    for (i, (code, text)) in FILES.iter().enumerate() {
        if i > 0 {
            js.push(',');
        }
        js += &format!("{}:{}", quote(code), text.trim());
    }
    js += &format!("}};window.GACHI_LANG={};window.GACHI_SYSTEM_LANG={};", quote(choice), quote(system()));
    js
}

/// A sentence by its key, in the language spoken now, with its `{holes}` filled:
/// `tr!("room.default_name")`, `tr!("room.name_host_now", name)` (the variable `name`),
/// `tr!("room.name_host_now", name = peer.name)`.
/// Gives a `String`.
#[macro_export]
macro_rules! tr {
    ($key:literal $(, $name:ident $(= $val:expr)?)* $(,)?) => {
        $crate::fill($crate::text($key), &[$((stringify!($name), &$crate::__tr_arg!($name $(, $val)?) as &dyn ::std::fmt::Display)),*])
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __tr_arg {
    ($name:ident) => {
        $name
    };
    ($name:ident, $val:expr) => {
        $val
    };
}

/// A language file: one JSON object of strings.
fn parse(text: &str) -> Result<HashMap<String, String>, String> {
    let mut p = Parser { s: text.as_bytes(), i: 0 };
    let mut map = HashMap::new();
    p.expect(b'{')?;
    if p.peek() == Some(b'}') {
        return Ok(map);
    }
    loop {
        let k = p.string()?;
        p.expect(b':')?;
        let v = p.string()?;
        if map.insert(k.clone(), v).is_some() {
            return Err(format!("\"{k}\" twice"));
        }
        match p.next() {
            Some(b',') => continue,
            Some(b'}') => break,
            _ => return Err(format!("expected , or }} at byte {}", p.i)),
        }
    }
    Ok(map)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&mut self) -> Option<u8> {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
        self.s.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek();
        self.i += 1;
        c
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        match self.next() {
            Some(x) if x == c => Ok(()),
            _ => Err(format!("expected {} at byte {}", c as char, self.i)),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.s.get(self.i..self.i + 4).and_then(|b| std::str::from_utf8(b).ok()).ok_or("short \\u escape")?;
        self.i += 4;
        u32::from_str_radix(h, 16).map_err(|e| e.to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let c = *self.s.get(self.i).ok_or("unfinished string")?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.s.get(self.i).ok_or("unfinished escape")?;
                    self.i += 1;
                    let ch = match e {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'"' | b'\\' | b'/' => e as char,
                        b'u' => {
                            let mut u = self.hex4()?;
                            if (0xD800..0xDC00).contains(&u) && self.s.get(self.i..self.i + 2) == Some(b"\\u") {
                                self.i += 2;
                                u = 0x10000 + ((u - 0xD800) << 10) + (self.hex4()? - 0xDC00);
                            }
                            char::from_u32(u).ok_or("bad \\u escape")?
                        }
                        _ => return Err(format!("bad escape at byte {}", self.i)),
                    };
                    out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests;
