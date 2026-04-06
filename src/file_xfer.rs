//! Host → Guest file transfer (SPICE drag-and-drop).
//!
//! Protocol (per `spice/vd_agent.h`):
//!
//! 1. Host sends `VDAGENTD_FILE_XFER_START` with `arg1 = transfer id` and a
//!    GKeyFile INI blob in the body describing the file:
//!
//!    ```ini
//!    [vdagent-file-xfer]
//!    name=document.pdf
//!    size=12345
//!    ```
//!
//! 2. Agent creates the target file under the save directory (uniquing the
//!    name on collision), preallocates it, and replies with
//!    `VDAGENTD_FILE_XFER_STATUS arg1=id arg2=CAN_SEND_DATA`.
//! 3. Host streams one or more `VDAGENTD_FILE_XFER_DATA` messages. Each body
//!    is a packed `VDAgentFileXferDataMessage { u32 id; u64 size; u8 data[]; }`.
//! 4. When the cumulative byte count reaches the announced size, the agent
//!    closes the file and replies with `STATUS=SUCCESS`.
//! 5. Remote `STATUS=CANCELLED` or any local error tears the task down and
//!    unlinks the partial file.
//!
//! Multiple transfers may be in flight concurrently — they are keyed by `id`.

use crate::udscs::{
    send_msg, VDAGENTD_FILE_XFER_STATUS, VD_AGENT_FILE_XFER_STATUS_CANCELLED,
    VD_AGENT_FILE_XFER_STATUS_CAN_SEND_DATA, VD_AGENT_FILE_XFER_STATUS_ERROR,
    VD_AGENT_FILE_XFER_STATUS_NOT_ENOUGH_SPACE, VD_AGENT_FILE_XFER_STATUS_SUCCESS,
};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A single in-flight file transfer.
struct XferTask {
    file: File,
    path: PathBuf,
    file_size: u64,
    written: u64,
}

/// All active transfers, plus the directory new files land in.
pub struct Xfers {
    tasks: HashMap<u32, XferTask>,
    save_dir: PathBuf,
}

impl Xfers {
    pub fn new() -> Self {
        let save_dir = resolve_save_dir();
        eprintln!(
            "wayland-vdagent: file-xfer save dir = {}",
            save_dir.display()
        );
        let _ = std::fs::create_dir_all(&save_dir);
        Self {
            tasks: HashMap::new(),
            save_dir,
        }
    }

    /// Handle `VDAGENTD_FILE_XFER_START`. Body layout:
    ///
    /// ```text
    /// u32 id
    /// u8[] keyfile_text   // [vdagent-file-xfer]\nname=..\nsize=..\n
    /// ```
    ///
    /// (The daemon forwards the full `VDAgentFileXferStartMessage` struct in
    /// the body, arg1/arg2 are both zero.)
    pub fn start(&mut self, daemon: &Arc<Mutex<UnixStream>>, data: &[u8]) {
        if data.len() < 4 {
            eprintln!("wayland-vdagent: file-xfer START: body too short");
            return;
        }
        let id = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let keyfile_bytes = &data[4..];

        if self.tasks.contains_key(&id) {
            eprintln!(
                "wayland-vdagent: file-xfer id {id} already in flight, ignoring duplicate START"
            );
            return;
        }

        let (name, size) = match parse_start_keyfile(keyfile_bytes) {
            Some(v) => v,
            None => {
                eprintln!(
                    "wayland-vdagent: file-xfer START {id}: malformed keyfile, {} bytes",
                    keyfile_bytes.len()
                );
                reply_status(daemon, id, VD_AGENT_FILE_XFER_STATUS_ERROR);
                return;
            }
        };

        eprintln!("wayland-vdagent: file-xfer START id={id} name={name:?} size={size}");

        match available_space(&self.save_dir) {
            Some(free) if size > free => {
                eprintln!("wayland-vdagent: file-xfer {id}: not enough space ({size} > {free})");
                reply_status(daemon, id, VD_AGENT_FILE_XFER_STATUS_NOT_ENOUGH_SPACE);
                return;
            }
            _ => {}
        }

        let (file, path) = match create_unique_file(&self.save_dir, &name) {
            Some(v) => v,
            None => {
                eprintln!("wayland-vdagent: file-xfer {id}: could not create destination");
                reply_status(daemon, id, VD_AGENT_FILE_XFER_STATUS_ERROR);
                return;
            }
        };

        // Preallocate (matches the reference implementation's ftruncate).
        if file.set_len(size).is_err() {
            eprintln!("wayland-vdagent: file-xfer {id}: ftruncate failed");
            let _ = std::fs::remove_file(&path);
            reply_status(daemon, id, VD_AGENT_FILE_XFER_STATUS_ERROR);
            return;
        }

        self.tasks.insert(
            id,
            XferTask {
                file,
                path,
                file_size: size,
                written: 0,
            },
        );
        reply_status(daemon, id, VD_AGENT_FILE_XFER_STATUS_CAN_SEND_DATA);
    }

    /// Handle `VDAGENTD_FILE_XFER_DATA`. Body layout:
    ///
    /// ```text
    /// u32 id
    /// u64 size       // chunk size
    /// u8[size] data
    /// ```
    pub fn data(&mut self, daemon: &Arc<Mutex<UnixStream>>, data: &[u8]) {
        if data.len() < 12 {
            eprintln!("wayland-vdagent: file-xfer DATA: body too short");
            return;
        }
        let id = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let chunk_size = u64::from_le_bytes(data[4..12].try_into().unwrap()) as usize;
        let chunk = match data.get(12..12 + chunk_size) {
            Some(s) => s,
            None => {
                eprintln!(
                    "wayland-vdagent: file-xfer DATA {id}: truncated (have {}, need {})",
                    data.len().saturating_sub(12),
                    chunk_size
                );
                self.fail(daemon, id);
                return;
            }
        };

        let task = match self.tasks.get_mut(&id) {
            Some(t) => t,
            None => {
                eprintln!("wayland-vdagent: file-xfer DATA for unknown id {id}");
                return;
            }
        };

        if let Err(e) = task.file.write_all(chunk) {
            eprintln!("wayland-vdagent: file-xfer {id}: write failed: {e}");
            self.fail(daemon, id);
            return;
        }
        task.written += chunk.len() as u64;

        if task.written < task.file_size {
            return;
        }
        if task.written > task.file_size {
            eprintln!(
                "wayland-vdagent: file-xfer {id}: overshoot ({} > {})",
                task.written, task.file_size
            );
            self.fail(daemon, id);
            return;
        }

        // Exact match — done.
        let task = self.tasks.remove(&id).unwrap();
        eprintln!(
            "wayland-vdagent: file-xfer {id} complete: {} ({} bytes)",
            task.path.display(),
            task.file_size
        );
        drop(task.file); // flush + close
        reply_status(daemon, id, VD_AGENT_FILE_XFER_STATUS_SUCCESS);
    }

    /// Handle a status notification from the host side. Body is a
    /// `VDAgentFileXferStatusMessage { u32 id; u32 result; u8 data[]; }`.
    /// Any non-`CAN_SEND_DATA` code means the transfer is torn down.
    pub fn remote_status(&mut self, data: &[u8]) {
        if data.len() < 8 {
            eprintln!("wayland-vdagent: file-xfer STATUS: body too short");
            return;
        }
        let id = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let status = u32::from_le_bytes(data[4..8].try_into().unwrap());
        if status == VD_AGENT_FILE_XFER_STATUS_CAN_SEND_DATA {
            return;
        }
        if let Some(task) = self.tasks.remove(&id) {
            let reason = match status {
                VD_AGENT_FILE_XFER_STATUS_CANCELLED => "cancelled by host",
                VD_AGENT_FILE_XFER_STATUS_ERROR => "host reported error",
                _ => "host terminated",
            };
            eprintln!(
                "wayland-vdagent: file-xfer {id} {reason}, removing {}",
                task.path.display()
            );
            drop(task.file);
            let _ = std::fs::remove_file(&task.path);
        }
    }

    fn fail(&mut self, daemon: &Arc<Mutex<UnixStream>>, id: u32) {
        if let Some(task) = self.tasks.remove(&id) {
            drop(task.file);
            let _ = std::fs::remove_file(&task.path);
        }
        reply_status(daemon, id, VD_AGENT_FILE_XFER_STATUS_ERROR);
    }
}

fn reply_status(daemon: &Arc<Mutex<UnixStream>>, id: u32, status: u32) {
    let d = daemon.lock().unwrap();
    send_msg(&d, VDAGENTD_FILE_XFER_STATUS, id, status, &[]);
}

/// Minimal GKeyFile reader: we only need `name` and `size` under the
/// `[vdagent-file-xfer]` section. A full GKeyFile parser is overkill.
fn parse_start_keyfile(data: &[u8]) -> Option<(String, u64)> {
    // Strip trailing NUL — GLib writes keyfiles null-terminated.
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let text = std::str::from_utf8(&data[..end]).ok()?;

    let mut in_section = false;
    let mut name: Option<String> = None;
    let mut size: Option<u64> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_section = line == "[vdagent-file-xfer]";
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "name" => name = Some(v.trim().to_string()),
                "size" => size = v.trim().parse().ok(),
                _ => {}
            }
        }
    }
    Some((name?, size?))
}

/// Resolve a save directory: XDG_DOWNLOAD_DIR → user-dirs.dirs → $HOME/Downloads → /tmp.
fn resolve_save_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = read_user_dirs_download() {
        return d;
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Downloads");
    }
    PathBuf::from("/tmp")
}

fn read_user_dirs_download() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let text = std::fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("XDG_DOWNLOAD_DIR=") {
            let value = rest.trim().trim_matches('"');
            if let Some(rel) = value.strip_prefix("$HOME/") {
                return home.map(|h| h.join(rel));
            }
            return Some(PathBuf::from(value));
        }
    }
    None
}

/// Try to open `<dir>/<name>` with `O_CREAT|O_EXCL`; on EEXIST, try
/// `<dir>/<name> (1)`, `<dir>/<name> (2)`, ..., up to 64 attempts. Returns
/// the final open handle and chosen path.
fn create_unique_file(dir: &Path, name: &str) -> Option<(File, PathBuf)> {
    // Only keep the last path component — the host should have stripped any
    // directories already, but defend against `../` traversal anyway.
    let leaf = Path::new(name).file_name()?.to_owned();
    let base: PathBuf = dir.join(&leaf);

    for i in 0..64 {
        let candidate = if i == 0 {
            base.clone()
        } else {
            uniquified(&base, i)
        };
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(f) => return Some((f, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                eprintln!(
                    "wayland-vdagent: file-xfer create {} failed: {e}",
                    candidate.display()
                );
                return None;
            }
        }
    }
    None
}

/// Produce `basename (n).ext` from `basename.ext`.
fn uniquified(base: &Path, n: u32) -> PathBuf {
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = base.extension().and_then(|s| s.to_str());
    let parent = base.parent().unwrap_or_else(|| Path::new(""));
    let name = match ext {
        Some(e) => format!("{stem} ({n}).{e}"),
        None => format!("{stem} ({n})"),
    };
    parent.join(name)
}

/// Free space available on the filesystem holding `path`, via `statvfs(3)`.
fn available_space(path: &Path) -> Option<u64> {
    let cpath = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stat: libc_statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc_statvfs_fn(cpath.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    Some(stat.f_bsize as u64 * stat.f_bavail as u64)
}

// Minimal `statvfs` FFI; avoids pulling in the `libc` crate for a single call.
#[repr(C)]
#[derive(Default)]
#[allow(non_camel_case_types)]
struct libc_statvfs {
    f_bsize: u64,
    f_frsize: u64,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: u64,
    f_flag: u64,
    f_namemax: u64,
    __reserved: [i32; 6],
}

extern "C" {
    #[link_name = "statvfs"]
    fn libc_statvfs_fn(path: *const i8, buf: *mut libc_statvfs) -> i32;
}
