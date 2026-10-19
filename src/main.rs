use color_quant::NeuQuant;
use fast_image_resize::{
    images::{Image, ImageRef},
    FilterType as FT, PixelType, ResizeAlg, ResizeOptions, Resizer,
};
use image::{codecs::webp::WebPDecoder, imageops, AnimationDecoder, RgbaImage};
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Gauge, Paragraph},
    Frame,
};
use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Cursor, Read},
    os::unix::fs::MetadataExt,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const MAX: usize = 300 * 1024;
const MAXMS: u32 = 3000;
const IG: &str = "/data/data/com.instagram.android/cache/images.stash/clean";

struct Shared {
    phase: AtomicUsize,
    mt: AtomicUsize,
    md: AtomicUsize,
    st: AtomicUsize,
    sd: AtomicUsize,
    at: AtomicUsize,
    ad: AtomicUsize,
    fails: AtomicUsize,
    trimmed: AtomicUsize,
    newfiles: AtomicUsize,
    log: Mutex<Vec<String>>,
    failed: Mutex<Vec<String>>,
    err: Mutex<Option<String>>,
    start: Instant,
}
impl Shared {
    fn new() -> Self {
        Shared {
            phase: AtomicUsize::new(2),
            mt: AtomicUsize::new(0),
            md: AtomicUsize::new(0),
            st: AtomicUsize::new(0),
            sd: AtomicUsize::new(0),
            at: AtomicUsize::new(0),
            ad: AtomicUsize::new(0),
            fails: AtomicUsize::new(0),
            trimmed: AtomicUsize::new(0),
            newfiles: AtomicUsize::new(0),
            log: Mutex::new(vec![]),
            failed: Mutex::new(vec![]),
            err: Mutex::new(None),
            start: Instant::now(),
        }
    }
    fn say(&self, m: impl Into<String>) {
        if let Ok(mut l) = self.log.lock() {
            l.push(m.into());
        }
    }
    fn fail(&self, name: &str, why: &str) {
        self.fails.fetch_add(1, Relaxed);
        self.say(format!("FAIL {name}: {why}"));
        if let Ok(mut f) = self.failed.lock() {
            f.push(format!("{name}: {why}"));
        }
    }
}

struct Paths {
    home: PathBuf,
    out: PathBuf,
    orig: PathBuf,
    stage: PathBuf,
}
impl Paths {
    fn new() -> Self {
        let home = PathBuf::from(
            std::env::var("HOME").unwrap_or_else(|_| "/data/data/com.termux/files/home".into()),
        );
        let out = home.join("storage/shared/signal_stickers");
        Paths { orig: out.join("originals"), stage: home.join(".stk_stage"), out, home }
    }
}

fn fnv(b: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
fn kind(b: &[u8]) -> Option<&'static str> {
    if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("webp")
    } else if b.starts_with(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]) {
        Some("png")
    } else {
        None
    }
}
fn is_anim(b: &[u8]) -> bool {
    b.len() > 30 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" && &b[12..16] == b"VP8X" && b[20] & 2 != 0
}
fn files_in(d: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(d)
        .map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

// runs BEFORE the UI so the root manager popup can appear; you must approve it
fn root_copy(p: &Paths, s: &Shared) -> Result<(), String> {
    let md = fs::metadata(&p.home).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&p.stage);
    fs::create_dir_all(&p.stage).map_err(|e| e.to_string())?;
    let st = p.stage.display();
    let cmd = format!(
        "cd {IG} || exit 3; cp -f *.png%3f* *.webp%3f* '{st}/' 2>/dev/null; chown -R {}:{} '{st}'; chmod -R u+rw '{st}'",
        md.uid(),
        md.gid()
    );
    let status = Command::new("su")
        .arg("-c")
        .arg(&cmd)
        .status()
        .map_err(|e| format!("cannot run su: {e}"))?;
    if !status.success() {
        return Err(format!("root denied or failed (su exit {:?}). Approve the popup, or use: stk --no-root", status.code()));
    }
    let n = files_in(&p.stage).len();
    s.say(format!("root copy: {n} files from Instagram cache"));
    Ok(())
}

fn merge(p: &Paths, s: &Shared) -> Result<(), String> {
    fs::create_dir_all(&p.orig).map_err(|e| e.to_string())?;
    let old = files_in(&p.orig);
    let staged = files_in(&p.stage);
    s.mt.store(old.len() + staged.len(), Relaxed);
    let mut have: HashSet<String> = HashSet::new();
    for f in old {
        s.md.fetch_add(1, Relaxed);
        let stem = f.file_stem().and_then(|x| x.to_str()).unwrap_or("").to_string();
        if stem.len() == 16 && stem.bytes().all(|c| c.is_ascii_hexdigit()) {
            have.insert(stem);
            continue;
        }
        let Ok(b) = fs::read(&f) else { continue };
        let Some(ext) = kind(&b) else { continue };
        let name = format!("{:016x}", fnv(&b));
        if have.contains(&name) {
            let _ = fs::remove_file(&f);
        } else {
            let _ = fs::rename(&f, p.orig.join(format!("{name}.{ext}")));
            have.insert(name);
        }
    }
    for f in staged {
        s.md.fetch_add(1, Relaxed);
        let Ok(b) = fs::read(&f) else { continue };
        let Some(ext) = kind(&b) else { continue };
        let name = format!("{:016x}", fnv(&b));
        if have.insert(name.clone()) {
            fs::write(p.orig.join(format!("{name}.{ext}")), &b).map_err(|e| e.to_string())?;
            s.newfiles.fetch_add(1, Relaxed);
        }
    }
    s.say(format!("originals: {} unique files ({} new)", have.len(), s.newfiles.load(Relaxed)));
    Ok(())
}

fn decode_anim(bytes: &[u8]) -> Result<Vec<(RgbaImage, u32)>, String> {
    if let Ok(d) = WebPDecoder::new(Cursor::new(bytes)) {
        let mut out: Vec<(RgbaImage, u32)> = vec![];
        let mut t = 0u32;
        for f in d.into_frames() {
            let Ok(f) = f else { break };
            let (n, dn) = f.delay().numer_denom_ms();
            let mut ms = if dn == 0 { 50 } else { n / dn };
            if ms < 20 {
                ms = 50;
            }
            if !out.is_empty() && t + ms > MAXMS {
                break;
            }
            t += ms;
            out.push((f.into_buffer(), ms));
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }
    let im = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    Ok(vec![(im.to_rgba8(), 0)])
}

// SIMD (NEON/AVX2) resize with correct alpha handling
fn resize_simd(img: &RgbaImage, nw: u32, nh: u32, hq: bool) -> RgbaImage {
    let src = ImageRef::new(img.width(), img.height(), img.as_raw(), PixelType::U8x4).unwrap();
    let mut dst = Image::new(nw, nh, PixelType::U8x4);
    let alg = ResizeAlg::Convolution(if hq { FT::CatmullRom } else { FT::Bilinear });
    Resizer::new()
        .resize(&src, &mut dst, &ResizeOptions::new().resize_alg(alg))
        .unwrap();
    RgbaImage::from_raw(nw, nh, dst.into_vec()).unwrap()
}

fn fit(img: &RgbaImage, size: u32, up: bool, hq: bool) -> RgbaImage {
    let (w, h) = img.dimensions();
    let mut sc = size as f32 / w.max(h) as f32;
    if !up && sc > 1.0 {
        sc = 1.0;
    }
    let nw = ((w as f32 * sc).round() as u32).clamp(1, 512);
    let nh = ((h as f32 * sc).round() as u32).clamp(1, 512);
    let mut c = RgbaImage::new(512, 512);
    let x = ((512 - nw) / 2) as i64;
    let y = ((512 - nh) / 2) as i64;
    if (nw, nh) == (w, h) {
        imageops::replace(&mut c, img, x, y);
    } else {
        let r = resize_simd(img, nw, nh, hq);
        imageops::replace(&mut c, &r, x, y);
    }
    for p in c.pixels_mut() {
        if p.0[3] == 0 {
            p.0 = [0, 0, 0, 0];
        }
    }
    c
}

fn palette_of(frames: &[&RgbaImage]) -> NeuQuant {
    let total: usize = frames.iter().map(|f| (f.width() * f.height()) as usize).sum();
    let stride = (total / 300_000).max(1);
    let mut samp: Vec<u8> = Vec::with_capacity(1_300_000);
    let mut k = 0usize;
    for f in frames {
        for p in f.pixels() {
            if k % stride == 0 {
                samp.extend_from_slice(&p.0);
            }
            k += 1;
        }
    }
    NeuQuant::new(10, 256, &samp)
}

fn pal_of(nq: &NeuQuant) -> (Vec<u8>, Vec<u8>) {
    let m = nq.color_map_rgba();
    let mut p = Vec::with_capacity(768);
    let mut t = Vec::with_capacity(256);
    for c in m.chunks(4) {
        p.extend_from_slice(&c[..3]);
        t.push(c[3]);
    }
    (p, t)
}

fn indexize(nq: &NeuQuant, img: &RgbaImage) -> Vec<u8> {
    let mut keys = vec![u64::MAX; 1 << 16];
    let mut vals = vec![0u8; 1 << 16];
    img.pixels()
        .map(|p| {
            let k32 = u32::from_le_bytes(p.0);
            let k = k32 as u64;
            let h = (k32.wrapping_mul(0x9E3779B1) >> 16) as usize;
            if keys[h] == k {
                vals[h]
            } else {
                let v = nq.index_of(&p.0) as u8;
                keys[h] = k;
                vals[h] = v;
                v
            }
        })
        .collect()
}

fn enc(frames: &[&[u8]], dur: &[u32], pal: Option<&(Vec<u8>, Vec<u8>)>, anim: bool, comp: png::Compression) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 << 17);
    {
        let mut e = png::Encoder::new(&mut buf, 512, 512);
        match pal {
            Some((p, t)) => {
                e.set_color(png::ColorType::Indexed);
                e.set_palette(p.clone());
                e.set_trns(t.clone());
            }
            None => e.set_color(png::ColorType::Rgba),
        }
        e.set_depth(png::BitDepth::Eight);
        e.set_compression(comp);
        e.set_adaptive_filter(png::AdaptiveFilterType::Adaptive);
        if anim {
            e.set_animated(frames.len() as u32, 0).unwrap();
        }
        let mut w = e.write_header().unwrap();
        for (i, f) in frames.iter().enumerate() {
            if anim {
                w.set_frame_delay(dur[i].min(65535) as u16, 1000).unwrap();
            }
            w.write_image_data(f).unwrap();
        }
        w.finish().unwrap();
    }
    buf
}

fn attempt(f: &dyn Fn(png::Compression) -> Vec<u8>) -> Result<Vec<u8>, usize> {
    let o = f(png::Compression::Default);
    if o.len() <= MAX {
        return Ok(o);
    }
    if o.len() < MAX * 3 / 2 {
        let o2 = f(png::Compression::Best);
        if o2.len() <= MAX {
            return Ok(o2);
        }
    }
    Err(o.len())
}

fn static_one(img: &RgbaImage) -> Vec<u8> {
    let mut last = Vec::new();
    for s in [512u32, 448, 384, 320, 256, 192, 128] {
        let c = fit(img, s, true, true);
        if let Ok(o) = attempt(&|k| enc(&[&c.as_raw()[..]], &[0], None, false, k)) {
            return o;
        }
        let nq = palette_of(&[&c]);
        let idx = indexize(&nq, &c);
        let pal = pal_of(&nq);
        match attempt(&|k| enc(&[&idx[..]], &[0], Some(&pal), false, k)) {
            Ok(o) => return o,
            Err(_) => last = enc(&[&idx[..]], &[0], Some(&pal), false, png::Compression::Default),
        }
    }
    last
}

fn pick(fr: &[(RgbaImage, u32)], step: usize) -> Vec<(usize, u32)> {
    fr.chunks(step).enumerate().map(|(i, c)| (i * step, c.iter().map(|x| x.1).sum())).collect()
}

fn anim_one(fr: &[(RgbaImage, u32)]) -> (Vec<u8>, u32, bool) {
    let ladder: [(u32, usize, bool); 9] = [
        (512, 1, false), (512, 1, true), (512, 2, true), (384, 1, true), (384, 2, true),
        (320, 2, true), (256, 2, true), (256, 3, true), (192, 3, true),
    ];
    let mut scaled: HashMap<u32, Vec<RgbaImage>> = HashMap::new();
    let mut pals: HashMap<u32, ((Vec<u8>, Vec<u8>), Vec<Vec<u8>>)> = HashMap::new();
    for (size, step, pal) in ladder {
        scaled.entry(size).or_insert_with(|| fr.iter().map(|f| fit(&f.0, size, false, false)).collect());
        let sc = &scaled[&size];
        if pal {
            pals.entry(size).or_insert_with(|| {
                let refs: Vec<&RgbaImage> = sc.iter().collect();
                let nq = palette_of(&refs);
                let idx: Vec<Vec<u8>> = sc.iter().map(|f| indexize(&nq, f)).collect();
                (pal_of(&nq), idx)
            });
        }
        let g = pick(fr, step);
        let d: Vec<u32> = g.iter().map(|x| x.1).collect();
        let data: Vec<&[u8]> = if pal {
            let pp = &pals[&size];
            g.iter().map(|x| &pp.1[x.0][..]).collect()
        } else {
            g.iter().map(|x| &sc[x.0].as_raw()[..]).collect()
        };
        let pr = if pal { Some(&pals[&size].0) } else { None };
        if let Ok(o) = attempt(&|k| enc(&data, &d, pr, true, k)) {
            return (o, d.iter().sum(), false);
        }
    }
    let pp = &pals[&192];
    let g = pick(fr, 3);
    let d: Vec<u32> = g.iter().map(|x| x.1).collect();
    let data: Vec<&[u8]> = g.iter().map(|x| &pp.1[x.0][..]).collect();
    let (mut lo, mut hi) = (1usize, g.len());
    let mut best: Option<(Vec<u8>, usize)> = None;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let o = enc(&data[..mid], &d[..mid], Some(&pp.0), true, png::Compression::Default);
        if o.len() <= MAX {
            best = Some((o, mid));
            lo = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            hi = mid - 1;
        }
    }
    match best {
        Some((o, n)) => (o, d[..n].iter().sum(), true),
        None => (enc(&data[..1], &d[..1], Some(&pp.0), true, png::Compression::Default), d[0], true),
    }
}

fn process(items: &[PathBuf], threads: usize, total: &AtomicUsize, done: &AtomicUsize, anim: bool, out: &Path, redo: bool, s: &Shared) {
    total.store(items.len(), Relaxed);
    done.store(0, Relaxed);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
    pool.install(|| {
        items.par_iter().for_each(|p| {
            let name = p.file_stem().and_then(|x| x.to_str()).unwrap_or("x").to_string();
            let dest = out.join(format!("{name}.png"));
            if !redo && dest.exists() {
                done.fetch_add(1, Relaxed);
                return;
            }
            let r = catch_unwind(AssertUnwindSafe(|| -> Result<(), String> {
                let b = fs::read(p).map_err(|e| e.to_string())?;
                let data = if anim {
                    let fr = decode_anim(&b)?;
                    if fr.len() > 1 {
                        let (o, ms, tr) = anim_one(&fr);
                        if tr {
                            s.trimmed.fetch_add(1, Relaxed);
                            s.say(format!("trimmed to {:.1}s: {name}", ms as f32 / 1000.0));
                        }
                        o
                    } else {
                        static_one(&fr[0].0)
                    }
                } else {
                    let img = image::load_from_memory(&b).map_err(|e| e.to_string())?.to_rgba8();
                    static_one(&img)
                };
                fs::write(&dest, data).map_err(|e| e.to_string())
            }));
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => s.fail(&name, &e),
                Err(_) => s.fail(&name, "panic while converting"),
            }
            done.fetch_add(1, Relaxed);
        });
    });
}

fn run(p: &Paths, s: &Shared, redo: bool) -> Result<(), String> {
    s.phase.store(2, Relaxed);
    merge(p, s)?;
    let _ = fs::remove_dir_all(&p.stage);

    let mut st = vec![];
    let mut an = vec![];
    for f in files_in(&p.orig) {
        let mut h = [0u8; 32];
        let Ok(n) = fs::File::open(&f).and_then(|mut x| x.read(&mut h)) else { continue };
        if kind(&h[..n]).is_none() {
            continue;
        }
        if is_anim(&h[..n]) { an.push(f) } else { st.push(f) }
    }
    for d in ["static", "animated"] {
        fs::create_dir_all(p.out.join(d)).map_err(|e| e.to_string())?;
    }
    let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    s.phase.store(3, Relaxed);
    process(&st, cores, &s.st, &s.sd, false, &p.out.join("static"), redo, s);
    s.phase.store(4, Relaxed);
    process(&an, cores.min(4), &s.at, &s.ad, true, &p.out.join("animated"), redo, s);

    if let Ok(f) = s.failed.lock() {
        if !f.is_empty() {
            let _ = fs::write(p.out.join("failed.txt"), f.join("\n"));
        }
    }
    Ok(())
}

fn ratio(d: usize, t: usize) -> f64 {
    if t == 0 { 0.0 } else { (d as f64 / t as f64).min(1.0) }
}
fn stage(ph: usize, mine: usize, d: usize, t: usize) -> (f64, String) {
    if ph > mine {
        (1.0, format!("{d}/{t}"))
    } else if ph == mine {
        (ratio(d, t), format!("{d}/{t}"))
    } else {
        (0.0, "waiting".into())
    }
}
fn gauge(f: &mut Frame, area: Rect, title: &str, r: f64, label: String, color: Color) {
    f.render_widget(
        Gauge::default()
            .block(Block::bordered().title(title.to_string()))
            .gauge_style(Style::default().fg(color))
            .ratio(r.clamp(0.0, 1.0))
            .label(label),
        area,
    );
}
fn ui(f: &mut Frame, s: &Shared, rooted: bool) {
    let ph = s.phase.load(Relaxed);
    let ch = Layout::vertical([
        Constraint::Length(3), Constraint::Length(3), Constraint::Length(3),
        Constraint::Length(3), Constraint::Min(4),
    ])
    .split(f.area());
    gauge(f, ch[0], " 1 Root copy ", 1.0, if rooted { "done".into() } else { "skipped (--no-root)".into() }, Color::Cyan);
    let (r, l) = stage(ph, 2, s.md.load(Relaxed), s.mt.load(Relaxed));
    gauge(f, ch[1], " 2 Save originals ", r, l, Color::Blue);
    let (r, l) = stage(ph, 3, s.sd.load(Relaxed), s.st.load(Relaxed));
    gauge(f, ch[2], " 3 Static stickers ", r, l, Color::Green);
    let (r, l) = stage(ph, 4, s.ad.load(Relaxed), s.at.load(Relaxed));
    gauge(f, ch[3], " 4 Animated stickers ", r, l, Color::Magenta);

    let fails = s.fails.load(Relaxed);
    let title = format!(
        " fail {fails} | trimmed {} | {}s {}",
        s.trimmed.load(Relaxed),
        s.start.elapsed().as_secs(),
        if ph >= 5 { "| DONE, q to exit " } else { "| q quits " }
    );
    let h = ch[4].height.saturating_sub(2) as usize;
    let lines: Vec<Line> = {
        let l = s.log.lock().unwrap();
        let start = l.len().saturating_sub(h);
        l[start..].iter().map(|x| Line::from(x.clone())).collect()
    };
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(title)), ch[4]);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let no_root = args.iter().any(|a| a == "--no-root");
    let redo = args.iter().any(|a| a == "--redo");
    let paths = Paths::new();
    let sh = Arc::new(Shared::new());

    if !no_root {
        println!("Requesting root: approve the su popup now...");
        if let Err(e) = root_copy(&paths, &sh) {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    }

    let mut term = ratatui::init();
    std::panic::set_hook(Box::new(|_| {}));
    {
        let s = sh.clone();
        thread::spawn(move || {
            if let Err(e) = run(&paths, &s, redo) {
                *s.err.lock().unwrap() = Some(e);
            }
            s.phase.store(5, Relaxed);
        });
    }
    loop {
        let _ = term.draw(|f| ui(f, &sh, !no_root));
        if event::poll(Duration::from_millis(80)).unwrap_or(false) {
            if let Ok(Event::Key(k)) = event::read() {
                if k.kind == KeyEventKind::Press {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    match k.code {
                        KeyCode::Char('c') if ctrl => {
                            ratatui::restore();
                            std::process::exit(130);
                        }
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        _ => {}
                    }
                }
            }
        }
    }
    ratatui::restore();
    let done = sh.phase.load(Relaxed) >= 5;
    println!(
        "{} | static {} | animated {} | failed {} | trimmed {} | new originals {}",
        if done { "finished" } else { "STOPPED EARLY" },
        sh.st.load(Relaxed), sh.at.load(Relaxed), sh.fails.load(Relaxed),
        sh.trimmed.load(Relaxed), sh.newfiles.load(Relaxed)
    );
    if let Some(e) = sh.err.lock().unwrap().as_ref() {
        println!("ERROR: {e}");
    }
    println!("everything is in: Internal storage/signal_stickers/ (originals, static, animated)");
}
