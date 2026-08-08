use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::{client, part_download_url, Manifest, Source};

pub const PART_MAX_ATTEMPTS: u32 = 5;

pub struct Progress {
    pub label: String,
    pub done: u64,
    pub total: u64,
}

pub trait Reporter: Send {
    fn report(&mut self, p: Progress);
}

pub struct NullReporter;
impl Reporter for NullReporter {
    fn report(&mut self, _: Progress) {}
}

fn sleep_backoff(attempt: u32) {
    std::thread::sleep(std::time::Duration::from_millis(500 * attempt as u64));
}

/// Download one part to a temp file, with retries on failure. A part is only
/// appended to the output after it downloads completely, so retries are safe.
fn download_part_to_file(c: &reqwest::blocking::Client, url: &str, tmp: &Path) -> Result<u64> {
    for attempt in 1..=PART_MAX_ATTEMPTS {
        let res = match c.get(url).send() {
            Ok(r) => r,
            Err(e) => {
                if attempt == PART_MAX_ATTEMPTS {
                    return Err(anyhow!("connect: {e}"));
                }
                sleep_backoff(attempt);
                continue;
            }
        };
        if !res.status().is_success() {
            let err = anyhow!("HTTP {}", res.status());
            if attempt == PART_MAX_ATTEMPTS {
                return Err(err);
            }
            let _ = err;
            sleep_backoff(attempt);
            continue;
        }
        let mut f = match File::create(tmp) {
            Ok(f) => f,
            Err(e) => return Err(anyhow!("create temp: {e}")),
        };
        let mut reader = res;
        let mut buf = [0u8; 256 * 1024];
        let mut got: u64 = 0;
        let mut io_err: Option<std::io::Error> = None;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = f.write_all(&buf[..n]) {
                        io_err = Some(e);
                        break;
                    }
                    got += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    io_err = Some(e);
                    break;
                }
            }
        }
        if let Some(err) = io_err {
            f.sync_all().ok();
            let _ = std::fs::remove_file(tmp);
            if attempt == PART_MAX_ATTEMPTS {
                return Err(anyhow!("write temp: {err}"));
            }
            sleep_backoff(attempt);
            continue;
        }
        f.sync_all().ok();
        return Ok(got);
    }
    bail!("part download failed")
}

/// Download all parts, reassemble them into `out_path`, and hash every byte.
/// `part_url` maps a part name to its download URL (tests inject a local
/// server here).
pub fn assemble(
    src: &Source,
    manifest: &Manifest,
    out_path: &Path,
    mut reporter: Box<dyn Reporter>,
) -> Result<String> {
    assemble_with_urls(
        src,
        manifest,
        out_path,
        &mut *reporter,
        &move |name: &str| part_download_url(src, name),
    )
}

pub fn assemble_with_urls(
    src: &Source,
    manifest: &Manifest,
    out_path: &Path,
    reporter: &mut dyn Reporter,
    part_url: &dyn Fn(&str) -> String,
) -> Result<String> {
    let res = assemble_with_urls_inner(src, manifest, out_path, reporter, part_url);
    if res.is_err() {
        std::fs::remove_file(out_path).ok();
    }
    res
}

fn assemble_with_urls_inner(
    src: &Source,
    manifest: &Manifest,
    out_path: &Path,
    reporter: &mut dyn Reporter,
    part_url: &dyn Fn(&str) -> String,
) -> Result<String> {
    let _ = src;
    let c = client()?;
    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(out_path)
        .with_context(|| format!("create {}", out_path.display()))?;
    let mut hasher = Sha256::new();
    let mut out_len: u64 = 0;

    let tmp = std::env::temp_dir().join(format!(
        "arasaka-part-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));

    let total_parts = manifest.parts.len() as u64;
    for (i, name) in manifest.parts.iter().enumerate() {
        let url = part_url(name);
        reporter.report(Progress {
            label: format!("downloading part {} ({}/{})", name, i + 1, total_parts),
            done: i as u64,
            total: total_parts,
        });
        let got = download_part_to_file(&c, &url, &tmp).with_context(|| format!("part {name}"))?;
        if got == 0 {
            std::fs::remove_file(&tmp).ok();
            return Err(anyhow!("part {name}: empty download"));
        }
        let mut tf = File::open(&tmp).context("reopen temp part")?;
        let mut buf = [0u8; 256 * 1024];
        loop {
            let n = tf.read(&mut buf).context("read temp part")?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).context("write output")?;
            hasher.update(&buf[..n]);
            out_len += n as u64;
        }
        std::fs::remove_file(&tmp).ok();
        reporter.report(Progress {
            label: format!("part {} stored", name),
            done: i as u64 + 1,
            total: total_parts,
        });
    }

    out.sync_all()?;

    let digest = hex::encode(hasher.finalize());
    let res = if out_len != manifest.total {
        Err(anyhow!(
            "assembled size mismatch: got {}, manifest.total {}",
            out_len,
            manifest.total
        ))
    } else if !manifest.sha256.is_empty() && digest != manifest.sha256 {
        Err(anyhow!(
            "sha256 mismatch: got {}, expected {}",
            digest,
            manifest.sha256
        ))
    } else {
        Ok(digest)
    };
    res
}

/// Verify an existing file against the manifest without re-downloading.
pub fn verify_file(path: &Path, manifest: &Manifest) -> Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    let mut len: u64 = 0;
    loop {
        match f.read(&mut buf)? {
            0 => break,
            n => {
                hasher.update(&buf[..n]);
                len += n as u64;
            }
        }
    }
    let digest = hex::encode(hasher.finalize());
    if len != manifest.total {
        return Err(anyhow!(
            "size mismatch: got {}, manifest.total {}",
            len,
            manifest.total
        ));
    }
    if !manifest.sha256.is_empty() && digest != manifest.sha256 {
        return Err(anyhow!(
            "sha256 mismatch: got {}, expected {}",
            digest,
            manifest.sha256
        ));
    }
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use super::*;
    use crate::Manifest;

    struct Rec(Vec<Progress>);
    impl Reporter for Rec {
        fn report(&mut self, p: Progress) {
            self.0.push(p);
        }
    }

    fn tmpfile(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "arasaka-usb-test-{}-{}-{}.iso",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn verify_rejects_short_file() {
        let p = tmpfile("short");
        let mut f = File::create(&p).unwrap();
        f.write_all(b"short").unwrap();
        drop(f);
        let m = Manifest {
            file: "x.iso".into(),
            total: 10,
            sha256: String::new(),
            parts: vec![],
        };
        assert!(verify_file(&p, &m).is_err());
        fs::remove_file(&p).ok();
    }

    #[test]
    fn verify_accepts_matching_file() {
        let p = tmpfile("good");
        let data = b"hello world";
        let mut f = File::create(&p).unwrap();
        f.write_all(data).unwrap();
        drop(f);
        let digest = hex::encode(Sha256::digest(data));
        let m = Manifest {
            file: "x.iso".into(),
            total: data.len() as u64,
            sha256: digest.clone(),
            parts: vec![],
        };
        let got = verify_file(&p, &m).unwrap();
        assert_eq!(got, digest);
        fs::remove_file(&p).ok();
    }

    #[test]
    fn assemble_concatenates_parts_and_hashes() {
        use std::net::{TcpListener, TcpStream};
        use std::sync::{Arc, Mutex};
        use std::thread;

        type Parts = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let parts: Parts = Arc::new(Mutex::new(vec![
            ("a.iso.part.00".to_string(), b"AAA".to_vec()),
            ("a.iso.part.01".to_string(), b"BBB".to_vec()),
        ]));
        let parts_srv = Arc::clone(&parts);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let buf = Arc::clone(&parts_srv);
                thread::spawn(move || handle_req(&mut s, &buf));
            }
        });

        fn handle_req(s: &mut TcpStream, parts: &Parts) {
            use std::io::{BufRead, BufReader};
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).ok();
            let path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .trim_start_matches('/');
            let body = parts
                .lock()
                .unwrap()
                .iter()
                .find(|(n, _)| n == path)
                .map(|(_, b)| b.clone())
                .unwrap_or_default();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            s.write_all(resp.as_bytes()).unwrap();
            s.write_all(&body).unwrap();
        }

        let src = Source {
            repo: "x/y".into(),
            tag: "t".into(),
        };
        let manifest = Manifest {
            file: "a.iso".into(),
            total: 6,
            sha256: hex::encode(Sha256::digest(b"AAABBB")),
            parts: vec!["a.iso.part.00".into(), "a.iso.part.01".into()],
        };
        let out = tmpfile("asm");
        let mut prog: Rec = Rec(Vec::new());
        let base = format!("http://{addr}/");
        let digest = assemble_with_urls(&src, &manifest, &out, &mut prog, &|name: &str| {
            format!("{base}{}", urlencoding::encode(name))
        })
        .unwrap();
        assert_eq!(digest, hex::encode(Sha256::digest(b"AAABBB")));
        assert_eq!(fs::read(&out).unwrap(), b"AAABBB");
        assert_eq!(prog.0.len(), 4);
        fs::remove_file(&out).ok();
    }

    #[test]
    fn assemble_rejects_size_mismatch() {
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let resp = "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nABC";
                s.write_all(resp.as_bytes()).unwrap();
            }
        });
        let src = Source {
            repo: "x/y".into(),
            tag: "t".into(),
        };
        let manifest = Manifest {
            file: "a.iso".into(),
            total: 999,
            sha256: String::new(),
            parts: vec!["a.iso.part.00".into()],
        };
        let out = tmpfile("mismatch");
        let base = format!("http://{addr}/");
        let mut prog = Rec(Vec::new());
        let r = assemble_with_urls(&src, &manifest, &out, &mut prog, &|_name: &str| {
            format!("{base}x")
        });
        assert!(r.is_err());
        assert!(!out.exists());
    }
}
