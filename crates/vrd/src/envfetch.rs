//! Download the recommended free environments (`hyprvr_proto::environments`) from their
//! original sources and convert them locally into the environments folder.
//!
//! Downloads go through `curl` into `~/.cache/hyprvr/downloads` (resumable), converted files
//! are written to a hidden temp name and renamed into place, so the daemon never lists a
//! half-written image. Only missing files are fetched; `CREDITS.md` is rewritten each run.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hyprvr_proto::environments::{Process, RECOMMENDED, Recommended, credits_markdown};
use image::{ImageReader, Rgb, RgbImage};

/// Output size of converted panoramas.
const OUT_W: u32 = 8192;
const OUT_H: u32 = 4096;
const JPEG_QUALITY: u8 = 90;

pub enum Event<'a> {
    /// Starting file `index` (0-based) of `total`.
    Start { index: usize, total: usize, item: &'a Recommended },
    /// Download progress of the current file, 0..=100.
    Progress { percent: u8 },
    /// Downloaded; converting.
    Converting,
    /// File is in place.
    Done,
    /// File failed (the others continue).
    Failed { error: String },
}

/// Recommended files not present in `dir`.
pub fn missing(dir: &Path) -> Vec<&'static Recommended> {
    RECOMMENDED.iter().filter(|r| !dir.join(r.file).exists()).collect()
}

/// Exclusive lock on the environments folder for the duration of a fetch (CLI and daemon).
struct Lock(#[allow(dead_code)] File);

fn lock(dir: &Path) -> Result<Lock> {
    let f = File::options().create(true).truncate(false).write(true).open(dir.join(".fetch.lock"))?;
    // SAFETY: plain flock on an fd we own; released when the file is closed.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another environment download is already running");
    }
    Ok(Lock(f))
}

/// Fetch every missing recommended environment into `dir`. Returns the number of files added.
/// Errors for single files are reported through `on` and summarized in the returned error.
pub fn fetch_missing(dir: &Path, on: &mut dyn FnMut(Event)) -> Result<usize> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let _lock = lock(dir)?;
    let todo = missing(dir);
    let cache = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("hyprvr/downloads");
    std::fs::create_dir_all(&cache)?;
    let mut added = 0;
    let mut failed = Vec::new();
    for (index, item) in todo.iter().copied().enumerate() {
        on(Event::Start { index, total: todo.len(), item });
        match fetch_one(dir, &cache, item, on) {
            Ok(()) => {
                added += 1;
                on(Event::Done);
            }
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!("environment {}: {error}", item.file);
                on(Event::Failed { error });
                failed.push(item.title);
            }
        }
    }
    write_credits(dir)?;
    if !failed.is_empty() {
        bail!("{} of {} failed: {}", failed.len(), todo.len(), failed.join(", "));
    }
    Ok(added)
}

/// (Re)write `CREDITS.md` for the recommended files present in `dir`.
pub fn write_credits(dir: &Path) -> Result<()> {
    let present: Vec<&str> = RECOMMENDED.iter().map(|r| r.file).filter(|f| dir.join(f).exists()).collect();
    let p = dir.join("CREDITS.md");
    if present.is_empty() {
        let _ = std::fs::remove_file(p);
        return Ok(());
    }
    std::fs::write(p, credits_markdown(&|f| present.contains(&f)))?;
    Ok(())
}

fn fetch_one(dir: &Path, cache: &Path, item: &Recommended, on: &mut dyn FnMut(Event)) -> Result<()> {
    let src = cache.join(format!("{}.orig", item.file));
    if !src.exists() {
        download(item, &src, on)?;
    }
    on(Event::Converting);
    let tmp = dir.join(format!(".{}.tmp", item.file));
    let r = convert(item, &src, &tmp).and_then(|()| Ok(std::fs::rename(&tmp, dir.join(item.file))?));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    // Converted, or broken (it would fail again; download a fresh one next time).
    let _ = std::fs::remove_file(&src);
    r
}

fn user_agent() -> String {
    format!("hyprvr/{} (environment downloader; curl)", env!("CARGO_PKG_VERSION"))
}

/// Download `item.url` to `dest` with curl (resuming a previous partial download).
fn download(item: &Recommended, dest: &Path, on: &mut dyn FnMut(Event)) -> Result<()> {
    let part = PathBuf::from(format!("{}.part", dest.display()));
    let have = std::fs::metadata(&part).map_or(0, |m| m.len());
    if have != item.bytes || have == 0 {
        if have > item.bytes {
            let _ = std::fs::remove_file(&part);
        }
        let mut cmd = Command::new("curl");
        cmd.args(["--fail", "--location", "--silent", "--show-error", "--retry", "3", "--retry-delay", "2"])
            .args(["--connect-timeout", "20", "--speed-limit", "1024", "--speed-time", "60"])
            .args(["--user-agent", &user_agent()]);
        if part.exists() {
            cmd.args(["--continue-at", "-"]);
        }
        let mut child = cmd
            .arg("--output")
            .arg(&part)
            .arg(item.url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("running curl (is it installed?)")?;
        let mut last = u8::MAX;
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break st;
            }
            let got = std::fs::metadata(&part).map_or(0, |m| m.len());
            let pct = (got * 100 / item.bytes.max(1)).min(99) as u8;
            if pct != last {
                last = pct;
                on(Event::Progress { percent: pct });
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        if !status.success() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut e, &mut err);
            }
            let err = err.trim().trim_start_matches("curl: ");
            // The partial file is stale or the server can't resume: start over next time.
            if err.contains("416") || err.contains("byte ranges") {
                let _ = std::fs::remove_file(&part);
            }
            bail!("download failed: {}", if err.is_empty() { format!("curl exited with {status}") } else { err.to_string() });
        }
    }
    on(Event::Progress { percent: 100 });
    std::fs::rename(&part, dest)?;
    Ok(())
}

fn decode(path: &Path) -> Result<RgbImage> {
    let mut r = ImageReader::open(path)?.with_guessed_format()?;
    // The ESO panoramas are up to 22204x8063 (~540 MB as RGB): above the default 512 MB limit.
    r.no_limits();
    Ok(r.decode().context("decoding the downloaded image")?.into_rgb8())
}

fn write_jpeg(img: &RgbImage, dest: &Path) -> Result<()> {
    let f = std::io::BufWriter::new(File::create(dest)?);
    image::codecs::jpeg::JpegEncoder::new_with_quality(f, JPEG_QUALITY).encode_image(img)?;
    Ok(())
}

fn convert(item: &Recommended, src: &Path, dest: &Path) -> Result<()> {
    match item.process {
        Process::AsIs => {
            let (w, h) = ImageReader::open(src)?.with_guessed_format()?.into_dimensions().context("not an image")?;
            if w != 2 * h {
                bail!("unexpected size {w}x{h}");
            }
            std::fs::copy(src, dest)?;
        }
        Process::Jpeg => write_jpeg(&decode(src)?, dest)?,
        Process::PadGround => {
            let img = decode(src)?;
            let h = ((img.height() as u64 * OUT_W as u64) / img.width() as u64) as u32;
            if h == 0 || h > OUT_H {
                bail!("unexpected size {}x{}", img.width(), img.height());
            }
            // Area-averaging downscale; avoids `resize`'s f32 copy of the full image.
            let sky = image::imageops::thumbnail(&img, OUT_W, h);
            drop(img);
            write_jpeg(&pad_ground(&sky), dest)?;
        }
    }
    Ok(())
}

/// Put a sky panorama (full width, top part of the sphere) on a 2:1 canvas and fill the
/// ground: the horizon colours, blurred sideways, fading into a dark floor.
fn pad_ground(sky: &RgbImage) -> RgbImage {
    let (w, full_h) = sky.dimensions();
    if full_h >= OUT_H {
        return image::imageops::crop_imm(sky, 0, 0, w, OUT_H).to_image();
    }
    // Some panoramas end in a few near-black rows (stitching edge): don't carry those into
    // the ground. Drop trailing rows much darker than the rows just above, up to 2 %.
    let row_luma = |y: u32| -> f32 { (0..w).step_by(16).map(|x| sky.get_pixel(x, y).0.iter().map(|&c| c as f32).sum::<f32>()).sum::<f32>() };
    let mut sh = full_h;
    let reference = row_luma(full_h.saturating_sub(40).max(full_h / 2));
    while sh > full_h - full_h / 50 && row_luma(sh - 1) < reference * 0.6 {
        sh -= 1;
    }
    let mut out = RgbImage::new(w, OUT_H);
    image::imageops::replace(&mut out, &*image::imageops::crop_imm(sky, 0, 0, w, sh), 0, 0);
    // Horizon colour per column: mean of the last few rows, box-blurred horizontally
    // (wrapping around, it is a 360° image) so single bright pixels don't become streaks.
    let rows = 8.min(sh);
    let mut base = vec![[0f32; 3]; w as usize];
    for (x, b) in base.iter_mut().enumerate() {
        for y in sh - rows..sh {
            let p = sky.get_pixel(x as u32, y).0;
            for k in 0..3 {
                b[k] += p[k] as f32 / rows as f32;
            }
        }
    }
    let radius = (w / 20) as i64;
    let mut prefix = vec![[0f32; 3]; w as usize + 1];
    for x in 0..w as usize {
        for k in 0..3 {
            prefix[x + 1][k] = prefix[x][k] + base[x][k];
        }
    }
    let sum = |a: i64, b: i64| -> [f32; 3] {
        // Sum over [a, b) with wrap-around, assuming b - a <= w.
        let n = w as i64;
        let (a, b) = (a.rem_euclid(n), a.rem_euclid(n) + (b - a));
        let mut s = [0f32; 3];
        for k in 0..3 {
            s[k] = if b <= n { prefix[b as usize][k] - prefix[a as usize][k] } else { prefix[n as usize][k] - prefix[a as usize][k] + prefix[(b - n) as usize][k] };
        }
        s
    };
    let span = (2 * radius + 1) as f32;
    let horizon: Vec<[f32; 3]> = (0..w as i64)
        .map(|x| {
            let s = sum(x - radius, x + radius + 1);
            [s[0] / span, s[1] / span, s[2] / span]
        })
        .collect();
    let mut floor = [0f32; 3];
    for c in &horizon {
        for k in 0..3 {
            floor[k] += c[k] * 0.35 / w as f32;
        }
    }
    // Fade over the first half of the ground, then flat dark floor.
    let fade = ((OUT_H - sh) as f32 / 2.0).max(1.0);
    for y in sh..OUT_H {
        let t = ((y - sh) as f32 / fade).min(1.0);
        let t = t * t * (3.0 - 2.0 * t);
        for x in 0..w {
            let c = &horizon[x as usize];
            let px = [0, 1, 2].map(|k| (c[k] * (1.0 - t) + floor[k] * t).round().clamp(0.0, 255.0) as u8);
            out.put_pixel(x, y, Rgb(px));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_ground_fills_below_the_sky() {
        let sky = RgbImage::from_fn(256, 60, |x, y| Rgb([(x % 256) as u8, 200, if y == 59 { 100 } else { 0 }]));
        let out = pad_ground(&sky);
        assert_eq!(out.dimensions(), (256, OUT_H));
        assert_eq!(out.get_pixel(10, 0), sky.get_pixel(10, 0));
        // Right below the horizon: close to the horizon colour; at the bottom: dark and uniform.
        assert!(out.get_pixel(128, 60).0[1] > 150);
        let b0 = out.get_pixel(0, OUT_H - 1).0;
        let b1 = out.get_pixel(200, OUT_H - 1).0;
        assert_eq!(b0, b1);
        assert!(b0[1] < 90);
        // Dark edge rows are dropped instead of being smeared into the ground.
        let edged = RgbImage::from_fn(256, 100, |_, y| if y >= 98 { Rgb([0, 0, 0]) } else { Rgb([120, 120, 120]) });
        let out = pad_ground(&edged);
        assert!(out.get_pixel(5, 99).0[0] > 100, "{:?}", out.get_pixel(5, 99));
    }

    #[test]
    fn missing_lists_absent_files() {
        let d = std::env::temp_dir().join(format!("hyprvr-envfetch-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("lakeside.jpg"), b"x").unwrap();
        let m = missing(&d);
        assert_eq!(m.len(), RECOMMENDED.len() - 1);
        assert!(m.iter().all(|r| r.file != "lakeside.jpg"));
        write_credits(&d).unwrap();
        let c = std::fs::read_to_string(d.join("CREDITS.md")).unwrap();
        assert!(c.contains("Lakeside") && !c.contains("Paranal"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
