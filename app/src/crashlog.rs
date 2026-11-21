//! What a crash was, into the log: Windows closes a program that touched memory it must not
//! (a driver's fault as often as ours) without a word, and the log just stopped. One line
//! says what it was, where (the module and the place in it) and on which thread.
//!
//! The line is put together and written without the heap and without a lock: the crash may
//! be in either, on any thread, and waiting for one of them here would leave a window that
//! hangs where the program should have closed.
#![cfg(windows)]

use std::ffi::c_void;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::OnceLock;

#[repr(C)]
struct Record {
    code: u32,
    flags: u32,
    record: *mut Record,
    address: *mut c_void,
    nparams: u32,
    info: [usize; 15],
}

#[repr(C)]
struct Pointers {
    record: *mut Record,
    context: *mut c_void,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetUnhandledExceptionFilter(
        f: Option<unsafe extern "system" fn(*mut Pointers) -> i32>,
    ) -> usize;
    fn GetModuleHandleExW(flags: u32, addr: *const u16, module: *mut isize) -> i32;
    fn GetModuleFileNameW(module: isize, buf: *mut u16, n: u32) -> u32;
    fn GetCurrentThreadId() -> u32;
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlCaptureStackBackTrace(skip: u32, n: u32, out: *mut *mut c_void, hash: *mut u32) -> u16;
}

/// A line of text in a fixed place (what does not fit is left out).
struct Line {
    text: [u8; 1024],
    len: usize,
}

impl std::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let n = s.len().min(self.text.len() - self.len);
        self.text[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// The module an address is in and how far into it: "gachi.exe+0x1234".
fn place(line: &mut Line, addr: usize) {
    let mut module = 0isize;
    let mut name = [0u16; 260];
    // SAFETY: plain lookups into buffers as long as we say; 4 = by an address inside the
    // module, 2 = without keeping it loaded
    let n = unsafe {
        if GetModuleHandleExW(4 | 2, addr as *const u16, &mut module) == 0 {
            let _ = write!(line, "{addr:#x}");
            return;
        }
        GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) as usize
    };
    let path = &name[..n.min(name.len())];
    let file = path
        .iter()
        .rposition(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
        .map_or(path, |i| &path[i + 1..]);
    for &c in file {
        let _ = line.write_char(if c < 0x7f { c as u8 as char } else { '?' });
    }
    let _ = write!(line, "+{:#x}", addr.wrapping_sub(module as usize));
}

/// The log file, to write the line into (its own handle: no lock is shared with the log).
static FILE: OnceLock<std::fs::File> = OnceLock::new();

unsafe extern "system" fn filter(p: *mut Pointers) -> i32 {
    // SAFETY: Windows hands a valid record to the filter
    let r = unsafe { &*(*p).record };
    let mut line = Line {
        text: [0; 1024],
        len: 0,
    };
    let _ = write!(
        line,
        "ERROR gachi::crashlog: crash: code={:#010x} at ",
        r.code
    );
    place(&mut line, r.address as usize);
    // SAFETY: a plain call
    let _ = write!(line, " thread={}", unsafe { GetCurrentThreadId() });
    // (an access violation: 0 read / 1 write / 8 execute, and the address it was)
    for d in &r.info[..(r.nparams as usize).min(2)] {
        let _ = write!(line, " {d:#x}");
    }
    // the frames that led there, as places in their modules (ours are of use with the
    // build's .pdb at hand)
    let mut frames = [std::ptr::null_mut::<c_void>(); 24];
    // SAFETY: the buffer is as long as we say
    let n = unsafe {
        RtlCaptureStackBackTrace(
            0,
            frames.len() as u32,
            frames.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    let _ = line.write_str(" via");
    for f in &frames[..n as usize] {
        let _ = line.write_char(' ');
        place(&mut line, *f as usize);
    }
    let _ = line.write_char('\n');
    let text = &line.text[..line.len];
    if let Some(mut f) = FILE.get() {
        let _ = f.write_all(text);
    }
    let _ = std::io::stderr().write_all(text);
    0 // go on as without us: the program is closed
}

/// From now on a crash leaves its line in `log` (and on the console, if there is one).
pub fn install(log: Option<std::fs::File>) {
    if let Some(f) = log {
        let _ = FILE.set(f);
    }
    // SAFETY: sets a process-wide handler; ours only reads the record and writes a line
    unsafe { SetUnhandledExceptionFilter(Some(filter)) };
}
