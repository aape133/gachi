//! Measures the bridge between two processes, as the DAW and the app will use it.
//!
//! cargo run -p gachi-bridge --release --example roundtrip [seconds] [block]
//!
//! This process plays the app: it creates the bridge, waits on the event, reads each block
//! and sends it straight back. It starts a copy of itself as the "plugin", which acts like a
//! DAW audio thread: one block every `block` frames at 48 kHz (64 → 1.33 ms), hand it up,
//! then poll for the answer. Both threads join MMCSS "Pro Audio", like a real driver thread
//! and the app's bridge thread.
//!
//! Reports, per block:
//! - handoff: plugin wrote the block → app has read it (the app's wake-up)
//! - round trip: plugin wrote the block → the app's answer is in the plugin's ring

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(windows)]
mod imp {
    use gachi_bridge::{AppEnd, PluginEnd, CHANNELS};
    use std::time::Duration;
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };

    fn qpc() -> i64 {
        let mut t = 0i64;
        // SAFETY: valid out-pointer; QPC is consistent across processes on one machine.
        unsafe { QueryPerformanceCounter(&mut t) };
        t
    }

    fn us(ticks: i64) -> f64 {
        let mut f = 0i64;
        // SAFETY: valid out-pointer.
        unsafe { QueryPerformanceFrequency(&mut f) };
        ticks as f64 * 1e6 / f as f64
    }

    fn pro_audio() {
        use windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
        let task: Vec<u16> = "Pro Audio".encode_utf16().chain(Some(0)).collect();
        let mut index = 0u32;
        // SAFETY: NUL-terminated task name and a valid out-pointer.
        let h = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
        if h.is_null() {
            eprintln!("(MMCSS unavailable: running at normal priority)");
        }
    }

    fn report(name: &str, mut v: Vec<f64>) {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        println!(
            "{name:<11} median {:>6.1} µs   p99 {:>6.1} µs   p99.9 {:>7.1} µs   max {:>7.1} µs   ({} blocks)",
            p(0.5),
            p(0.99),
            p(0.999),
            v[v.len() - 1],
            v.len()
        );
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("--plugin") {
            return plugin(&args[2], args[3].parse().unwrap(), args[4].parse().unwrap());
        }
        let secs: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10.0);
        let block: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(64);
        let name = format!("gachi-bridge-roundtrip-{}", std::process::id());
        let app = AppEnd::create(&name).expect("create bridge");
        println!(
            "block {block} frames ({:.2} ms at 48 kHz), {secs} s",
            block as f64 / 48.0
        );

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--plugin", &name, &secs.to_string(), &block.to_string()])
            .spawn()
            .expect("start the plugin process");

        pro_audio();
        let mut buf = vec![0f32; block * CHANNELS];
        let mut handoff = Vec::new();
        let mut idle = 0;
        loop {
            if !app.wait(500) && app.up_available() == 0 {
                idle += 1;
                if idle > 4 {
                    break; // the plugin finished
                }
                continue;
            }
            idle = 0;
            while app.up_available() >= block {
                app.read_up(&mut buf);
                let now = qpc();
                // the plugin put its write time into the first frame (bit for bit)
                let sent = (buf[0].to_bits() as i64) | ((buf[1].to_bits() as i64) << 32);
                handoff.push(us(now - sent));
                app.write_down(&buf);
            }
        }
        child.wait().unwrap();
        handoff.drain(..handoff.len().min(100)); // warm-up
        report("handoff", handoff);
    }

    fn plugin(name: &str, secs: f64, block: usize) {
        // the app may still be setting up
        let mut end = None;
        for _ in 0..50 {
            match PluginEnd::open(name, 1) {
                Ok(e) => {
                    end = Some(e);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let mut end = end.expect("open the bridge");
        end.set_format(48_000, block as u32);
        pro_audio();
        let mut f = 0i64;
        // SAFETY: valid out-pointer.
        unsafe { QueryPerformanceFrequency(&mut f) };
        let period = (f as f64 * block as f64 / 48_000.0) as i64;
        let blocks = (secs * 48_000.0 / block as f64) as usize;
        let mut up = vec![0.25f32; block * CHANNELS];
        let mut down = vec![0f32; block * CHANNELS];
        let mut round = Vec::with_capacity(blocks);
        let mut late = 0usize;
        let mut next = qpc();
        for _ in 0..blocks {
            // wait for the next "DAW callback" (spin: sleep is far too coarse for 1.3 ms)
            while qpc() < next {
                std::hint::spin_loop();
            }
            next += period;
            let t = qpc();
            up[0] = f32::from_bits(t as u32);
            up[1] = f32::from_bits((t >> 32) as u32);
            end.write_up(&up);
            // the answer must come back within this block to be played in it
            loop {
                if end.down_available() >= block {
                    end.read_down(&mut down);
                    round.push(us(qpc() - t));
                    break;
                }
                if qpc() - t > period {
                    late += 1;
                    break;
                }
                std::hint::spin_loop();
            }
        }
        // drain what came after its block ended
        while end.read_down(&mut down) > 0 {}
        round.drain(..round.len().min(100));
        report("round trip", round);
        println!(
            "answers later than one block: {late} of {blocks}; frames dropped: {}",
            end.overruns
        );
    }
}
