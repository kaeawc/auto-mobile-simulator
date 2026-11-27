//! The camera benchmark's measuring side (`scripts/bench/camera.sh` runs it):
//!
//! ```text
//! camera-bench fanout [--readers 1,2,4,8] [--secs 5] [--size 640x480@30] [--mixed] [--source fake-webcam|<source id>]
//! camera-bench latency --socket <path> --shot-cmd '<command writing a PNG to {}>' [--shots 30] [--size 640x480@30]
//!                      [--point 50,50] [--go-file <path>]
//! ```
//!
//! `fanout` is host-only: one source fanned out to N camera feeds, one per linked device, as the
//! simee server serves them (each with one reader, the device's camera, at its size), for each N.
//! Per N it prints this process's CPU (capture, fan-out, socket writes and the readers' reads,
//! which a device would do in its own process) as a share of one core, the slowest reader's frame
//! rate, and source-to-reader latency through the feed (p50, p95). `fake-webcam` is a camera-like
//! source (1280x720 frames, fitted to each reader's size, as a webcam's are); `webcam:<id>` is a
//! real one. `--mixed` gives the readers different sizes (640x480, 1280x720, 352x288, 320x240).
//!
//! `latency` serves a feed on `--socket` whose frames are one flat colour encoding the frame
//! number (mod 64: two bits per channel, levels 0, 85, 170, 255), which survives scaling, rotation,
//! mirroring and YUV round trips. Once a reader connects (a device's camera linked to this feed) it
//! prints `connected`, waits for `--go-file` to exist (when given), then takes `--shots` screenshots
//! with `--shot-cmd` (`{}` is the PNG path to write), reads the colour at `--point` (percent of the
//! width and height; the camera preview must cover it), and matches it to the newest frame of that
//! number sent before the shot returned. Latency is from sending a frame to the screenshot that
//! shows it, as an interval: the shot was taken between the command's start and its return.
//! Prints p50 and p95 of the interval's midpoint and of its upper end, and the feed side's CPU.
//!
//! Results are lines `RESULT<TAB>arm<TAB>metric<TAB>value`, which the script records.

#[cfg(unix)]
fn main() {
    if let Err(e) = bench::run(std::env::args().skip(1).collect()) {
        eprintln!("camera-bench: {e}");
        std::process::exit(2);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("camera-bench needs Unix sockets");
}

#[cfg(unix)]
mod bench {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use simee_camera::feed::{Feed, FeedReader, Hello};
    use simee_camera::{CameraFrame, FrameSpec, HostOpener, Hub, Opener, Result, Source, SourceSpec, Tick};

    type Res<T> = std::result::Result<T, String>;

    pub fn run(args: Vec<String>) -> Res<()> {
        let (mode, rest) = args.split_first().ok_or("usage: camera-bench fanout|latency [options] (see the source's header)")?;
        let opts = Opts::parse(rest)?;
        match mode.as_str() {
            "fanout" => fanout(&opts),
            "latency" => latency(&opts),
            other => Err(format!("unknown mode {other}: fanout or latency")),
        }
    }

    struct Opts {
        readers: Vec<usize>,
        secs: f64,
        spec: FrameSpec,
        mixed: bool,
        source: String,
        socket: Option<PathBuf>,
        shot_cmd: Option<String>,
        shots: usize,
        point: (f64, f64),
        go_file: Option<PathBuf>,
    }

    impl Opts {
        fn parse(args: &[String]) -> Res<Opts> {
            let mut o = Opts {
                readers: vec![1, 2, 4, 8],
                secs: 5.0,
                spec: FrameSpec::default(),
                mixed: false,
                source: "fake-webcam".into(),
                socket: None,
                shot_cmd: None,
                shots: 30,
                point: (50.0, 50.0),
                go_file: None,
            };
            let mut it = args.iter();
            while let Some(a) = it.next() {
                let mut val = || it.next().cloned().ok_or_else(|| format!("{a} needs a value"));
                match a.as_str() {
                    "--readers" => {
                        o.readers =
                            val()?.split(',').map(|n| n.trim().parse().map_err(|_| format!("--readers: {n}"))).collect::<Res<_>>()?
                    }
                    "--secs" => o.secs = val()?.parse().map_err(|_| "--secs: a number")?,
                    "--size" => o.spec = parse_spec(&val()?)?,
                    "--mixed" => o.mixed = true,
                    "--source" => o.source = val()?,
                    "--socket" => o.socket = Some(val()?.into()),
                    "--shot-cmd" => o.shot_cmd = Some(val()?),
                    "--shots" => o.shots = val()?.parse().map_err(|_| "--shots: a number")?,
                    "--point" => {
                        let v = val()?;
                        let (x, y) = v.split_once(',').ok_or("--point: X,Y in percent")?;
                        o.point = (x.parse().map_err(|_| "--point: X")?, y.parse().map_err(|_| "--point: Y")?);
                    }
                    "--go-file" => o.go_file = Some(val()?.into()),
                    other => return Err(format!("unknown option {other}")),
                }
            }
            Ok(o)
        }
    }

    fn parse_spec(s: &str) -> Res<FrameSpec> {
        let bad = || format!("{s}: expected WIDTHxHEIGHT@FPS");
        let (size, fps) = s.split_once('@').ok_or_else(bad)?;
        let (w, h) = size.split_once('x').ok_or_else(bad)?;
        let num = |v: &str| v.parse::<u32>().map_err(|_| bad());
        FrameSpec::new(num(w)?, num(h)?, num(fps)?).map_err(|e| e.to_string())
    }

    /// When each frame number was produced, for the last few seconds of frames.
    #[derive(Default)]
    struct Sent(Mutex<VecDeque<(u64, Instant)>>);

    impl Sent {
        fn record(&self, seq: u64) {
            let mut q = self.0.lock().unwrap_or_else(|p| p.into_inner());
            q.push_back((seq, Instant::now()));
            while q.len() > 4096 {
                q.pop_front();
            }
        }

        fn at(&self, seq: u64) -> Option<Instant> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).iter().rev().find(|(s, _)| *s == seq).map(|(_, t)| *t)
        }

        /// The newest frame whose number is `code` mod 64, produced by `before`.
        fn newest(&self, code: u64, before: Instant) -> Option<(u64, Instant)> {
            self.0.lock().unwrap_or_else(|p| p.into_inner()).iter().rev().find(|(s, t)| s % 64 == code && *t <= before).copied()
        }
    }

    /// Records when each frame a source produces was produced.
    struct Recorded {
        inner: Box<dyn Source>,
        sent: Arc<Sent>,
    }

    impl Source for Recorded {
        fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            let f = self.inner.frame(spec, tick)?;
            self.sent.record(tick.seq);
            Ok(f)
        }

        fn synthetic(&self) -> bool {
            self.inner.synthetic()
        }
    }

    /// A camera-like source: 1280x720 frames (one moving bar on grey), fitted to each reader's size
    /// as a webcam's frames are.
    struct FakeWebcam;

    impl Source for FakeWebcam {
        fn frame(&mut self, _spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            let (w, h) = (1280u32, 720u32);
            let mut bgra = vec![96u8; w as usize * h as usize * 4];
            let x = (tick.seq % 64) as usize * (w as usize / 64);
            for row in bgra.chunks_exact_mut(w as usize * 4) {
                row[x * 4..(x + 20) * 4].fill(255);
            }
            Ok(CameraFrame { width: w, height: h, bgra: bgra.into(), tick })
        }
    }

    /// A flat colour encoding the frame number mod 64.
    struct CounterColour;

    const LEVELS: [u8; 4] = [0, 85, 170, 255];

    impl Source for CounterColour {
        fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
            let code = (tick.seq % 64) as usize;
            let (r, g, b) = (LEVELS[code >> 4 & 3], LEVELS[code >> 2 & 3], LEVELS[code & 3]);
            let bgra = [b, g, r, 255].repeat(spec.width as usize * spec.height as usize);
            Ok(CameraFrame { width: spec.width, height: spec.height, bgra: bgra.into(), tick })
        }

        fn synthetic(&self) -> bool {
            true
        }
    }

    /// The bench's opener: `text:fake-webcam` and `text:counter` are the sources above; anything
    /// else as the host opens it. Every source is recorded.
    struct Bench {
        host: HostOpener,
        sent: Arc<Sent>,
    }

    impl Opener for Bench {
        fn canonical(&self, spec: &SourceSpec) -> Result<SourceSpec> {
            self.host.canonical(spec)
        }

        fn open(&self, source: &SourceSpec, spec: FrameSpec) -> Result<Box<dyn Source>> {
            let inner: Box<dyn Source> = match source {
                SourceSpec::Text(t) if t == "fake-webcam" => Box::new(FakeWebcam),
                SourceSpec::Text(t) if t == "counter" => Box::new(CounterColour),
                other => self.host.open(other, spec)?,
            };
            Ok(Box::new(Recorded { inner, sent: self.sent.clone() }))
        }
    }

    /// This process's CPU time (user and system) so far.
    fn cpu_time() -> Res<Duration> {
        if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
            let after = stat.rsplit_once(") ").ok_or("/proc/self/stat: no command field")?.1;
            let f: Vec<&str> = after.split_whitespace().collect();
            let ticks: u64 = f.get(11).and_then(|u| u.parse().ok()).unwrap_or(0) + f.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
            let hz = Command::new("getconf")
                .arg("CLK_TCK")
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
                .unwrap_or(100u64);
            return Ok(Duration::from_secs_f64(ticks as f64 / hz as f64));
        }
        let out =
            Command::new("ps").args(["-o", "time=", "-p", &std::process::id().to_string()]).output().map_err(|e| format!("ps: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let secs = text.trim().replace('-', ":").split(':').try_fold(0.0f64, |acc, part| part.parse::<f64>().map(|v| acc * 60.0 + v));
        secs.map(Duration::from_secs_f64).map_err(|_| format!("ps -o time: {text:?}"))
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return f64::NAN;
        }
        let i = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[i.min(sorted.len() - 1)]
    }

    fn result(arm: &str, metric: &str, value: f64) {
        println!("RESULT\t{arm}\t{metric}\t{value:.3}");
    }

    fn sock(tag: &str) -> PathBuf {
        PathBuf::from("/tmp").join(format!("simee-cambench-{}-{tag}.sock", std::process::id()))
    }

    const MIXED: [(u32, u32); 4] = [(640, 480), (1280, 720), (352, 288), (320, 240)];

    fn fanout(o: &Opts) -> Res<()> {
        let source = if o.source == "fake-webcam" {
            SourceSpec::Text("fake-webcam".into())
        } else {
            SourceSpec::parse(&o.source).map_err(|e| e.to_string())?
        };
        let sizes = if o.mixed { "mixed sizes".to_string() } else { format!("{}x{}", o.spec.width, o.spec.height) };
        let arm = format!("fan-out {} {sizes}@{}", o.source, o.spec.fps);
        for &n in &o.readers {
            let sent = Arc::new(Sent::default());
            let hub = Arc::new(Hub::new(Bench { host: HostOpener::new(), sent: sent.clone() }));
            let feeds: Vec<Feed> = (0..n)
                .map(|i| {
                    let path = sock(&format!("fan{n}-{i}"));
                    let _ = std::fs::remove_file(&path);
                    Feed::serve(hub.clone(), path, source.clone(), o.spec).map_err(|e| e.to_string())
                })
                .collect::<Res<_>>()?;
            let measuring = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let readers: Vec<_> = feeds
                .iter()
                .enumerate()
                .map(|(i, feed)| {
                    let (w, h) = if o.mixed { MIXED[i % MIXED.len()] } else { (o.spec.width, o.spec.height) };
                    let hello = Hello { width: w, height: h, fps: o.spec.fps };
                    let mut r = FeedReader::connect(feed.path(), hello).map_err(|e| e.to_string())?;
                    let (sent, measuring, stop) = (sent.clone(), measuring.clone(), stop.clone());
                    Ok(thread::spawn(move || {
                        let (mut frames, mut lat) = (0u64, Vec::new());
                        while !stop.load(Ordering::SeqCst) {
                            let Ok((header, _px)) = r.next(Duration::from_secs(5)) else { break };
                            let now = Instant::now();
                            if measuring.load(Ordering::SeqCst) {
                                frames += 1;
                                if let Some(t) = sent.at(header.seq) {
                                    lat.push(now.duration_since(t).as_secs_f64() * 1000.0);
                                }
                            }
                        }
                        (frames, lat)
                    }))
                })
                .collect::<Res<_>>()?;
            thread::sleep(Duration::from_secs(1));
            let (cpu0, t0) = (cpu_time()?, Instant::now());
            measuring.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_secs_f64(o.secs));
            measuring.store(false, Ordering::SeqCst);
            let (cpu1, wall) = (cpu_time()?, t0.elapsed());
            stop.store(true, Ordering::SeqCst);
            drop(feeds);
            let mut lat = Vec::new();
            let mut slowest = f64::INFINITY;
            for r in readers {
                let (frames, l) = r.join().map_err(|_| "a reader panicked")?;
                slowest = slowest.min(frames as f64 / wall.as_secs_f64());
                lat.extend(l);
            }
            lat.sort_by(f64::total_cmp);
            let cpu = (cpu1 - cpu0).as_secs_f64() / wall.as_secs_f64() * 100.0;
            let arm = format!("{arm}, N={n}");
            result(&arm, "CPU, % of one core", cpu);
            result(&arm, "CPU per device, % of one core", cpu / n as f64);
            result(&arm, "slowest reader, fps", slowest);
            result(&arm, "source to reader p50, ms", percentile(&lat, 0.5));
            result(&arm, "source to reader p95, ms", percentile(&lat, 0.95));
        }
        Ok(())
    }

    /// The frame number mod 64 a pixel's colour encodes, or None when a channel is far from every
    /// level (the point isn't on the camera preview).
    fn decode(r: u8, g: u8, b: u8) -> Option<u64> {
        let level = |v: u8| {
            let (i, d) = LEVELS.iter().enumerate().map(|(i, &l)| (i, l.abs_diff(v))).min_by_key(|&(_, d)| d)?;
            (d <= 30).then_some(i as u64)
        };
        Some(level(r)? << 4 | level(g)? << 2 | level(b)?)
    }

    /// The mean colour of a 9x9 patch of a PNG at `point` (percent).
    fn sample(path: &Path, point: (f64, f64)) -> Res<(u8, u8, u8)> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
        let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        let channels = match info.color_type {
            png::ColorType::Rgba => 4,
            png::ColorType::Rgb => 3,
            other => return Err(format!("screenshot colour type {other:?}")),
        };
        let (w, h) = (info.width as usize, info.height as usize);
        let (cx, cy) = ((w as f64 * point.0 / 100.0) as usize, (h as f64 * point.1 / 100.0) as usize);
        let mut sum = [0u64; 3];
        let mut n = 0u64;
        for y in cy.saturating_sub(4)..(cy + 5).min(h) {
            for x in cx.saturating_sub(4)..(cx + 5).min(w) {
                let i = (y * w + x) * channels;
                for c in 0..3 {
                    sum[c] += u64::from(buf[i + c]);
                }
                n += 1;
            }
        }
        let m = |c: usize| (sum[c] / n.max(1)) as u8;
        Ok((m(0), m(1), m(2)))
    }

    fn latency(o: &Opts) -> Res<()> {
        let socket = o.socket.clone().ok_or("latency needs --socket")?;
        let cmd = o.shot_cmd.clone().ok_or("latency needs --shot-cmd")?;
        let sent = Arc::new(Sent::default());
        let hub = Arc::new(Hub::new(Bench { host: HostOpener::new(), sent: sent.clone() }));
        let _ = std::fs::remove_file(&socket);
        let feed = Feed::serve(hub, &socket, SourceSpec::Text("counter".into()), o.spec).map_err(|e| e.to_string())?;
        eprintln!("camera-bench: serving the counter source on {}; link a device's camera to feed:{}", socket.display(), socket.display());
        let waited = Instant::now();
        while feed.readers() == 0 {
            if waited.elapsed() > Duration::from_secs(120) {
                return Err("no reader connected in 120 s: is the device's camera linked to the feed and open?".into());
            }
            thread::sleep(Duration::from_millis(100));
        }
        println!("connected");
        let (cpu0, t0) = (cpu_time()?, Instant::now());
        if let Some(go) = &o.go_file {
            while !go.exists() {
                thread::sleep(Duration::from_millis(100));
            }
        } else {
            thread::sleep(Duration::from_secs(2));
        }
        let shot = std::env::temp_dir().join(format!("camera-bench-{}.png", std::process::id()));
        let (mut mid, mut hi, mut bad) = (Vec::new(), Vec::new(), 0usize);
        for i in 0..o.shots {
            let _ = std::fs::remove_file(&shot);
            let line = cmd.replace("{}", &shot.display().to_string());
            let start = Instant::now();
            let status = Command::new("sh").arg("-c").arg(&line).status().map_err(|e| format!("{line}: {e}"))?;
            let end = Instant::now();
            if !status.success() {
                return Err(format!("{line}: {status}"));
            }
            let (r, g, b) = sample(&shot, o.point)?;
            let Some(code) = decode(r, g, b) else {
                bad += 1;
                eprintln!("shot {i}: colour {r},{g},{b} at the point isn't a counter colour (is the preview under --point?)");
                continue;
            };
            let Some((seq, at)) = sent.newest(code, end) else {
                bad += 1;
                continue;
            };
            let lo = start.saturating_duration_since(at).as_secs_f64() * 1000.0;
            let up = end.duration_since(at).as_secs_f64() * 1000.0;
            eprintln!("shot {i}: frame {seq} (code {code}), latency {lo:.1}..{up:.1} ms");
            mid.push((lo + up) / 2.0);
            hi.push(up);
        }
        let (cpu1, wall) = (cpu_time()?, t0.elapsed());
        let _ = std::fs::remove_file(&shot);
        mid.sort_by(f64::total_cmp);
        hi.sort_by(f64::total_cmp);
        let arm = format!("source to guest screen {}x{}@{}", o.spec.width, o.spec.height, o.spec.fps);
        result(&arm, "p50, ms", percentile(&mid, 0.5));
        result(&arm, "p95, ms", percentile(&mid, 0.95));
        result(&arm, "p50 upper bound, ms", percentile(&hi, 0.5));
        result(&arm, "p95 upper bound, ms", percentile(&hi, 0.95));
        result(&arm, "undecoded shots", bad as f64);
        result(&arm, "feed side CPU, % of one core", (cpu1 - cpu0).as_secs_f64() / wall.as_secs_f64() * 100.0);
        Ok(())
    }
}
