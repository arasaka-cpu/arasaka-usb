use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Device {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub removable: bool,
    pub model: String,
}

#[cfg(target_os = "linux")]
pub fn list_devices() -> Result<Vec<Device>> {
    let mut out = Vec::new();
    let sys = Path::new("/sys/class/block");
    let entries = std::fs::read_dir(sys).with_context(|| "read /sys/class/block")?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("sr")
            || name.starts_with("zram")
            || name.starts_with("fd")
        {
            continue;
        }
        let removable = read_sys(&sys.join(&name), "removable") == "1";
        if !removable {
            continue;
        }
        let sectors = read_sys(&sys.join(&name), "size")
            .trim()
            .parse::<u64>()
            .unwrap_or(0);
        if sectors == 0 {
            continue;
        }
        let model = read_sys(&sys.join(&name), "device/model")
            .trim()
            .to_string();
        let size = sectors * 512;
        out.push(Device {
            name: name.clone(),
            path: format!("/dev/{name}"),
            size,
            removable: true,
            model,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(target_os = "linux")]
fn read_sys(dir: &Path, attr: &str) -> String {
    std::fs::read_to_string(dir.join(attr)).unwrap_or_default()
}

#[cfg(target_os = "windows")]
pub fn list_devices() -> Result<Vec<Device>> {
    windows_list_devices()
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn list_devices() -> Result<Vec<Device>> {
    anyhow::bail!("device enumeration not supported on this platform")
}

fn open_write(path: &str) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("open {} for writing (need root/admin)", path))
}

/// Write a raw image file to a block device. `progress_cb` receives bytes
/// written so far.
pub fn flash(dev: &Device, image: &Path, mut progress_cb: impl FnMut(u64)) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        flash_linux(dev, image, &mut progress_cb)
    }
    #[cfg(target_os = "windows")]
    {
        flash_windows(dev, image, &mut progress_cb)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        anyhow::bail!("flashing not supported on this platform")
    }
}

#[cfg(target_os = "linux")]
fn flash_linux(dev: &Device, image: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
    let mut src = File::open(image).with_context(|| format!("open {}", image.display()))?;
    let mut dst = open_write(&dev.path)?;
    let buf = vec![0u8; 1024 * 1024];
    let mut chunk = std::io::BufReader::new(&mut src);
    let mut written: u64 = 0;
    let mut buf2 = vec![0u8; 1024 * 1024];
    loop {
        let n = chunk.read(&mut buf2).context("read image")?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf2[..n]).context("write device")?;
        written += n as u64;
        progress(written);
    }
    dst.sync_all().context("sync device")?;
    let _ = buf;
    Ok(())
}

#[cfg(target_os = "windows")]
fn flash_windows(dev: &Device, image: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
    let mut src = File::open(image).with_context(|| format!("open {}", image.display()))?;
    let mut dst = open_write(&dev.path)?;
    let mut chunk = std::io::BufReader::new(&mut src);
    let mut buf = vec![0u8; 1024 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = chunk.read(&mut buf).context("read image")?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).context("write device")?;
        written += n as u64;
        progress(written);
    }
    dst.sync_all().context("sync device")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_list_devices() -> Result<Vec<Device>> {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW};
    const DRIVE_REMOVABLE: u32 = 2;
    let mut out = Vec::new();
    for c in b'A'..=b'Z' {
        let letter = format!("{}:", c as char);
        let wide: Vec<u16> = letter.encode_utf16().chain(std::iter::once(0)).collect();
        let ty = unsafe { GetDriveTypeW(wide.as_ptr()) };
        if ty != DRIVE_REMOVABLE {
            continue;
        }
        let mut free_avail: u64 = 0;
        let mut total: u64 = 0;
        let mut free_total: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_avail, &mut total, &mut free_total)
        };
        if ok == 0 {
            continue;
        }
        out.push(Device {
            name: format!("{}:", c as char),
            path: format!("\\\\.\\{}", letter),
            size: total,
            removable: true,
            model: format!("removable drive {}", letter),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_struct_roundtrip() {
        let d = Device {
            name: "sdb".into(),
            path: "/dev/sdb".into(),
            size: 16000000000,
            removable: true,
            model: "Test".into(),
        };
        assert_eq!(d.path, "/dev/sdb");
        assert!(d.removable);
    }
}
