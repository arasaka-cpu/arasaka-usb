use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{anyhow, Context, Result};

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
    let sys = Path::new("/sys/class/block");
    let entries = match std::fs::read_dir(sys) {
        Ok(e) => e,
        Err(e) => {
            crate::debug!("cannot read {}: {}", sys.display(), e);
            return Err(anyhow::Error::new(e).context(format!("read {}", sys.display())));
        }
    };
    crate::debug!("scanning {}", sys.display());
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let dir = sys.join(&name);
        if is_virtual_device(&name) {
            crate::debug!("skip {name}: virtual/optical/loop name");
            continue;
        }
        if dir.join("partition").exists() {
            crate::debug!("skip {name}: partition, not a whole disk");
            continue;
        }
        let removable = read_sys(&dir, "removable").trim().to_string();
        let usb_parent = parent_is_usb(&dir);
        if removable != "1" && !usb_parent {
            crate::debug!(
                "skip {name}: not removable (removable={removable:?}, usb_parent={usb_parent})"
            );
            continue;
        }
        let sectors = read_sys(&dir, "size").trim().parse::<u64>().unwrap_or(0);
        if sectors == 0 {
            crate::debug!("skip {name}: size attribute missing or 0");
            continue;
        }
        let size = sectors * 512;
        let dev_path = format!("/dev/{name}");
        if !Path::new(&dev_path).exists() {
            crate::debug!("skip {name}: {dev_path} not present in this environment");
            continue;
        }
        let model = read_sys(&dir, "device/model").trim().to_string();
        crate::debug!(
            "detect {name}: removable={removable:?} usb_parent={usb_parent} size={size} model={model:?}"
        );
        out.push(Device {
            name: name.clone(),
            path: dev_path,
            size,
            removable: true,
            model,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    crate::debug!("scan done: {} device(s) detected", out.len());
    Ok(out)
}

/// True for block names that are never a whole removable disk we can flash:
/// loop, ramdisk, optical, zram, floppy, device-mapper, mdraid, nbd, fuse.
#[cfg(target_os = "linux")]
fn is_virtual_device(name: &str) -> bool {
    name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("sr")
        || name.starts_with("zram")
        || name.starts_with("fd")
        || name.starts_with("dm-")
        || name.starts_with("md")
        || name.starts_with("nbd")
        || name.starts_with("fuse")
}

/// Fallback for USB sticks whose firmware reports `removable` as 0: detect by
/// walking the real sysfs path for a /usb segment (USB mass storage exposes
/// `.../usbN/N-N/N-N:1.0/.../block/sdX`).
#[cfg(target_os = "linux")]
fn parent_is_usb(dir: &Path) -> bool {
    match std::fs::canonicalize(dir) {
        Ok(p) => p.to_string_lossy().contains("/usb"),
        Err(e) => {
            crate::debug!("cannot resolve {}: {}", dir.display(), e);
            false
        }
    }
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
    let mut dst = open_for_write(dev)?;
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

#[cfg(target_os = "linux")]
fn empty_options() -> glib::Variant {
    let d = glib::VariantDict::new(None);
    d.end()
}

/// Open the device for raw writing. Prefers udisks2 (which triggers a polkit
/// authorization prompt, so no root shell is needed) and falls back to a
/// direct device open when udisks2 is not running.
#[cfg(target_os = "linux")]
fn open_for_write(dev: &Device) -> Result<File> {
    match udisks_open_write(&dev.name) {
        Ok(Some(f)) => return Ok(f),
        Ok(None) => crate::debug!("udisks2 not available; opening {} directly", dev.path),
        Err(e) => crate::debug!("udisks2 open failed ({e}); opening {} directly", dev.path),
    }
    open_write(&dev.path)
}

/// Ask udisks2 (org.freedesktop.UDisks2 on the system bus) for a writable fd
/// to the whole disk via `OpenForRestore`. This performs a polkit
/// authorization check, so the user gets the normal elevation prompt. Returns
/// `Ok(None)` when udisks2 is not reachable so callers can fall back.
#[cfg(target_os = "linux")]
fn udisks_open_write(name: &str) -> Result<Option<File>> {
    use gio::prelude::UnixFDListExtManual;
    use std::os::fd::FromRawFd;

    let conn = match gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>) {
        Ok(c) => c,
        Err(e) => {
            crate::debug!("no system bus: {e}");
            return Ok(None);
        }
    };
    let object = format!("/org/freedesktop/UDisks2/block_devices/{name}");

    // Release any mounted partitions so the whole-disk restore is not refused
    // because a filesystem is busy.
    unmount_partitions(&conn, name);

    let args = glib::Variant::tuple_from_iter([empty_options()]);
    // The `h` in the reply body is only an index into the message's fd list;
    // the real fd must be fetched with call_with_unix_fd_list (call_sync
    // would leave us with that index and nothing to resolve it to).
    let (reply, fd_list) = conn.call_with_unix_fd_list_sync(
        Some("org.freedesktop.UDisks2"),
        &object,
        "org.freedesktop.UDisks2.Block",
        "OpenForRestore",
        Some(&args),
        None,
        // Allow polkit to prompt the user for authorization and wait as long
        // as needed for the answer.
        gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION,
        -1,
        None::<&gio::UnixFDList>,
        None::<&gio::Cancellable>,
    )?;
    let (handle,): (glib::variant::Handle,) = reply
        .get()
        .ok_or_else(|| anyhow!("unexpected udisks2 OpenForRestore reply"))?;
    let fd_list = fd_list.ok_or_else(|| anyhow!("udisks2 returned no file descriptor"))?;
    let fd = fd_list
        .get(handle.0)
        .map_err(|_| anyhow!("udisks2 file descriptor missing from reply"))?;
    // SAFETY: g_unix_fd_list_get returns a copy of the fd that we own; the
    // device is released when the fd is closed (modern udisks2 has no Close
    // method).
    let file = unsafe { File::from_raw_fd(fd) };
    Ok(Some(file))
}

/// Best-effort unmount of the device itself and every partition of `name`
/// (e.g. sdc, sda1, nvme0n1p2) so the restore is not refused because a
/// filesystem is still mounted. A raw ISO image is often mounted on the whole
/// disk (`/dev/sdc` itself), not on a partition, so the disk must be checked
/// too.
#[cfg(target_os = "linux")]
fn unmount_partitions(conn: &gio::DBusConnection, name: &str) {
    let reply = conn.call_sync(
        Some("org.freedesktop.UDisks2"),
        "/org/freedesktop/UDisks2",
        "org.freedesktop.DBus.ObjectManager",
        "GetManagedObjects",
        None,
        None,
        gio::DBusCallFlags::NONE,
        -1,
        None::<&gio::Cancellable>,
    );
    let Ok(reply) = reply else { return };
    let objects = reply.child_value(0);
    for entry in objects.iter() {
        let Some(path) = entry.child_value(0).get::<glib::variant::ObjectPath>() else {
            continue;
        };
        let path = path.as_str();
        let Some(part) = path.rsplit('/').next() else {
            continue;
        };
        if part != name && !is_partition_of(name, part) {
            continue;
        }
        let object = format!("/org/freedesktop/UDisks2/block_devices/{part}");
        let args = glib::Variant::tuple_from_iter([empty_options()]);
        let _ = conn.call_sync(
            Some("org.freedesktop.UDisks2"),
            &object,
            "org.freedesktop.UDisks2.Filesystem",
            "Unmount",
            Some(&args),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            None::<&gio::Cancellable>,
        );
    }
}

#[cfg(target_os = "linux")]
fn is_partition_of(disk: &str, part: &str) -> bool {
    let Some(rest) = part.strip_prefix(disk) else {
        return false;
    };
    let digits = rest.strip_prefix('p').unwrap_or(rest);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
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
