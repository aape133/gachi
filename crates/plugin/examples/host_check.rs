//! Drives the built plugin the way a VST3 host does, with a stand-in gachi app on the other
//! end of the bridge. Checks pass-through without the app, the round trip with it, and the
//! own-sound level the app sets.
//!
//! cargo build -p gachi-plugin --release
//! cargo run -p gachi-plugin --release --example host_check [path to gachi_plugin.dll]

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
    use gachi_bridge::{AppEnd, CHANNELS};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use vst3::{ComPtr, Interface, Steinberg::Vst::*, Steinberg::*};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    const BLOCK: usize = 64;
    /// what the stand-in app "mixes" for the other players
    const PEERS: f32 = 0.1;
    const INPUT: f32 = 0.5;

    fn check(ok: bool, what: &str) {
        println!("{} {what}", if ok { "OK  " } else { "FAIL" });
        if !ok {
            std::process::exit(1);
        }
    }

    struct Host {
        processor: ComPtr<IAudioProcessor>,
        _component: ComPtr<IComponent>,
        l_in: Vec<f32>,
        r_in: Vec<f32>,
        l_out: Vec<f32>,
        r_out: Vec<f32>,
    }

    impl Host {
        /// One block of constant `INPUT`; returns the left output's first sample.
        fn block(&mut self) -> f32 {
            self.run(2, 2, ProcessModes_::kRealtime)
        }

        /// The same with `ic` input and `oc` output channels, in a given process mode.
        fn run(&mut self, ic: i32, oc: i32, mode: i32) -> f32 {
            self.l_in.fill(INPUT);
            self.r_in.fill(INPUT);
            self.l_out.fill(-9.0);
            self.r_out.fill(-9.0);
            let mut ins = [self.l_in.as_mut_ptr(), self.r_in.as_mut_ptr()];
            let mut outs = [self.l_out.as_mut_ptr(), self.r_out.as_mut_ptr()];
            let mut in_bus = AudioBusBuffers { numChannels: ic, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: ins.as_mut_ptr() } };
            let mut out_bus = AudioBusBuffers { numChannels: oc, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: outs.as_mut_ptr() } };
            let mut data = ProcessData {
                processMode: mode,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32,
                numSamples: BLOCK as i32,
                numInputs: 1,
                numOutputs: 1,
                inputs: &mut in_bus,
                outputs: &mut out_bus,
                inputParameterChanges: std::ptr::null_mut(),
                outputParameterChanges: std::ptr::null_mut(),
                inputEvents: std::ptr::null_mut(),
                outputEvents: std::ptr::null_mut(),
                processContext: std::ptr::null_mut(),
            };
            // SAFETY: every pointer in `data` lives until the call returns
            let r = unsafe { self.processor.process(&mut data) };
            assert_eq!(r, kResultOk);
            if oc == 2 {
                assert_eq!(self.l_out, self.r_out, "both channels alike");
            }
            assert!(self.l_out.iter().all(|&s| s != -9.0), "the whole block written");
            self.l_out[0]
        }
    }

    pub fn main() {
        let dll = std::env::args().nth(1).unwrap_or_else(|| "target/release/gachi_plugin.dll".into());
        // our own bridge name: does not touch a gachi running on this PC
        std::env::set_var("GACHI_BRIDGE_NAME", format!("gachi-host-check-{}", std::process::id()));

        let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated path; the library stays loaded for the process's life
        let lib = unsafe { LoadLibraryW(wide.as_ptr()) };
        check(!lib.is_null(), &format!("load {dll}"));
        // SAFETY: the export VST3 hosts look up; its signature is fixed by the SDK
        let get = unsafe { GetProcAddress(lib, c"GetPluginFactory".as_ptr() as *const u8) }.expect("GetPluginFactory exported");
        let get: extern "system" fn() -> *mut IPluginFactory = unsafe { std::mem::transmute(get) };
        // SAFETY: the factory hands us one reference
        let factory = unsafe { ComPtr::from_raw(get()) }.expect("a factory");

        let mut info: PClassInfo = unsafe { std::mem::zeroed() };
        unsafe {
            check(factory.countClasses() == 2, "two classes (processor, controller)");
            check(factory.getClassInfo(0, &mut info) == kResultOk, "class info");
        }
        let name: String = info.name.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect();
        check(name == "gachi", &format!("named '{name}'"));
        let f2 = factory.cast::<IPluginFactory2>();
        check(f2.is_some(), "IPluginFactory2 (category: Fx)");

        let mut obj: *mut c_void = std::ptr::null_mut();
        let r = unsafe { factory.createInstance(info.cid.as_ptr(), IComponent::IID.as_ptr() as *const _, &mut obj) };
        check(r == kResultOk && !obj.is_null(), "create the processor");
        let component = unsafe { ComPtr::from_raw(obj as *mut IComponent) }.unwrap();
        let processor = component.cast::<IAudioProcessor>().expect("IAudioProcessor");
        unsafe {
            check(component.initialize(std::ptr::null_mut()) == kResultOk, "initialize");
            check(processor.getLatencySamples() == 0, "reports no latency");
            check(processor.getTailSamples() == u32::MAX, "infinite tail (keeps running in silence)");
            // a host that hands over null pointers gets an error, not a crash
            check(
                component.getBusInfo(MediaTypes_::kAudio as MediaType, BusDirections_::kInput as BusDirection, 0, std::ptr::null_mut()) == kInvalidArgument,
                "null bus info refused",
            );
            check(processor.setBusArrangements(std::ptr::null_mut(), 1, std::ptr::null_mut(), 1) == kResultFalse, "null arrangements refused");
            check(processor.process(std::ptr::null_mut()) == kInvalidArgument, "null process data refused");
            check(factory.createInstance(std::ptr::null(), IComponent::IID.as_ptr() as *const _, &mut std::ptr::null_mut()) == kInvalidArgument, "null class id refused");
            let mut setup = ProcessSetup {
                processMode: ProcessModes_::kRealtime,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32,
                maxSamplesPerBlock: BLOCK as i32,
                sampleRate: 48_000.0,
            };
            check(processor.setupProcessing(&mut setup) == kResultOk, "setup 48 kHz / 64");
            check(component.setActive(1) == kResultOk, "activate");
            processor.setProcessing(1);
        }
        let mut host = Host {
            processor,
            _component: component,
            l_in: vec![0.0; BLOCK],
            r_in: vec![0.0; BLOCK],
            l_out: vec![0.0; BLOCK],
            r_out: vec![0.0; BLOCK],
        };

        // 1. no app: the sound passes through unchanged
        for _ in 0..50 {
            let out = host.block();
            assert_eq!(out, INPUT);
        }
        check(true, "without the app: pass-through");
        check(host.run(1, 1, ProcessModes_::kRealtime) == INPUT, "mono track without the app: pass-through");
        check(host.run(1, 2, ProcessModes_::kRealtime) == INPUT, "mono in, stereo out without the app: both sides");
        unsafe {
            let (mut mono, mut stereo) = (SpeakerArr::kMono, SpeakerArr::kStereo);
            check(host.processor.setBusArrangements(&mut stereo, 1, &mut mono, 1) == kResultFalse, "stereo in, mono out refused");
            check(host.processor.setBusArrangements(&mut mono, 1, &mut mono, 1) == kResultTrue, "a mono track is accepted");
            let mut arr = 0;
            host.processor.getBusArrangement(BusDirections_::kInput as BusDirection, 0, &mut arr);
            check(arr == SpeakerArr::kMono, "and reported back as mono");
            check(host.processor.setBusArrangements(&mut stereo, 1, &mut stereo, 1) == kResultTrue, "back to stereo");
        }

        // 2. the app starts (in DAW mode): it answers each block with the others' mix
        let app = Arc::new(AppEnd::create(&gachi_bridge::name()).expect("app end"));
        let stop = Arc::new(AtomicBool::new(false));
        let received = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let echo = {
            let (app, stop, received) = (app.clone(), stop.clone(), received.clone());
            std::thread::spawn(move || {
                gachi_bridge::pro_audio();
                let mut buf = vec![0f32; 2048 * CHANNELS];
                while !stop.load(Relaxed) {
                    if !app.wait(50) && app.up_available() == 0 {
                        continue;
                    }
                    loop {
                        let n = app.read_up(&mut buf);
                        if n == 0 {
                            break;
                        }
                        if buf[..n * CHANNELS].iter().all(|&s| s == INPUT) {
                            received.fetch_add(n as u32, Relaxed);
                        }
                        let mix = vec![PEERS; n * CHANNELS];
                        app.write_down(&mix);
                    }
                }
            })
        };
        // the plugin finds the app within a second (its background thread looks twice a second)
        let t = Instant::now();
        while !app.plugin_alive() && t.elapsed() < Duration::from_secs(3) {
            host.block();
            std::thread::sleep(Duration::from_micros(1333));
        }
        check(app.plugin_alive(), &format!("found the app after {:?}", t.elapsed()));
        for _ in 0..5 {
            host.block(); // the format goes over with the first block after connecting
        }
        check(app.format() == (48_000, BLOCK as u32), "told the app the DAW's format");

        // 3. each block: own sound + the others' mix, back within the same block
        let (mut same, mut total) = (0, 0);
        for _ in 0..1000 {
            let out = host.block();
            total += 1;
            if (out - (INPUT + PEERS)).abs() < 1e-6 {
                same += 1;
            }
            std::thread::sleep(Duration::from_micros(1333));
        }
        check(same >= 990, &format!("mix back within the block: {same} of {total}"));
        check(received.load(Relaxed) >= 1000 * BLOCK as u32, "the app got the plugin's sound (unmixed)");

        // a mono track: your sound and the others' mix on its one channel
        host.run(1, 1, ProcessModes_::kRealtime);
        let out = host.run(1, 1, ProcessModes_::kRealtime);
        check((out - (INPUT + PEERS)).abs() < 1e-6, &format!("mono track with the app: {out}"));
        // a render to a file: the track as it is, nothing of the room in it, nothing sent
        let before = received.load(Relaxed);
        let out = host.run(2, 2, ProcessModes_::kOffline);
        check(out == INPUT && received.load(Relaxed) == before, &format!("offline render: plain pass-through ({out})"));
        host.block();

        // 4. the app's own-sound fader: down to 0 → only the others are heard
        app.set_own_gain(0.0);
        host.block(); // (the level moves there over one block)
        let out = host.block();
        check((out - PEERS).abs() < 1e-6, &format!("own sound off: output {out}"));
        app.set_own_gain(1.0);

        // 5. the app quits: the plugin passes through again, without waiting on anything
        stop.store(true, Relaxed);
        echo.join().unwrap();
        drop(app);
        let t = Instant::now();
        let mut slow = Duration::ZERO;
        for _ in 0..200 {
            let b = Instant::now();
            host.block();
            slow = slow.max(b.elapsed());
            std::thread::sleep(Duration::from_micros(1333));
        }
        let out = host.block();
        check(out == INPUT, &format!("app gone: pass-through ({out}), longest block {slow:?} over {:?}", t.elapsed()));
        check(slow < Duration::from_millis(1), "never held up the DAW thread");

        // 6. the app comes back later (the plugin let go of the bridge meanwhile, so the app
        //    starts on a fresh region): the plugin finds it again and tells it the format again
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(4) {
            host.block();
            std::thread::sleep(Duration::from_micros(1333));
        }
        let app = AppEnd::create(&gachi_bridge::name()).expect("app end again");
        check(app.format() == (0, 0), "the app's new bridge starts without a format");
        let t = Instant::now();
        while !app.plugin_alive() && t.elapsed() < Duration::from_secs(3) {
            app.beat();
            host.block();
            std::thread::sleep(Duration::from_micros(1333));
        }
        check(app.plugin_alive(), &format!("found the restarted app after {:?}", t.elapsed()));
        for _ in 0..5 {
            app.beat();
            host.block();
        }
        check(app.format() == (48_000, BLOCK as u32), &format!("told the restarted app the format: {:?}", app.format()));

        // 7. the DAW deactivates the plugin (sample rate change, project close): at once,
        //    not after the background thread's next look
        unsafe {
            host.processor.setProcessing(0);
            let t = Instant::now();
            host._component.setActive(0);
            let took = t.elapsed();
            check(took < Duration::from_millis(50), &format!("deactivated in {took:?}"));
            check(!app.plugin_alive(), "let go of the bridge");
            host._component.terminate();
        }
        println!("all checks passed");
    }
}
