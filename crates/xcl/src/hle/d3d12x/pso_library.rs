//! Memory-mapped `ID3D12PipelineLibrary` persistence (`XCL_PSO_LIBRARY=0`: off).
//!
//! Backing a driver pipeline library with a read-only Windows file mapping kept open for the
//! lifetime of `ID3D12PipelineLibrary` avoids duplicating hundreds of megabytes to >1 GiB of
//! private heap commit on load, and writing new snapshots through a separate file-backed mapping
//! into a fresh generation file (`gen-NNNNNNNN.plib`) avoids both `Vec<u8>` serialize spikes and
//! in-place replacement of an actively mapped file (which fails on Windows or triggers driver
//! device removal).

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{Interface, HSTRING};
use windows::Win32::Graphics::Direct3D12::*;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileSizeEx, SetEndOfFile, SetFilePointerEx, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL,
    FILE_BEGIN, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READONLY, PAGE_READWRITE,
};

const D3D12_ERROR_ADAPTER_NOT_FOUND_HR: i32 = 0x887E_0001u32 as i32;
const D3D12_ERROR_DRIVER_VERSION_MISMATCH_HR: i32 = 0x887E_0002u32 as i32;

/// Whether the on-disk memory-mapped pipeline library is enabled (`XCL_PSO_LIBRARY=0` disables).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_PSO_LIBRARY").map_or(true, |v| v.trim() != "0"))
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// A read-only Windows file mapping kept open (file + section + mapped view) with zero private
/// heap copy. Dropping unmaps the view and closes both handles in reverse order.
pub struct MappedBlob {
    path: PathBuf,
    file: HANDLE,
    mapping: HANDLE,
    view: *const u8,
    len: usize,
    /// The file's bytes with room after them, where it is not mapped (`open`).
    copy: Vec<u8>,
}

// The view is strictly read-only and backed by immutable OS handles owned uniquely by `MappedBlob`.
unsafe impl Send for MappedBlob {}
unsafe impl Sync for MappedBlob {}

impl MappedBlob {
    /// Open `path` read-only with shared read/write/delete access and map its entire contents.
    /// Returns `None` when the file does not exist, is empty, or cannot be mapped.
    pub fn open(path: &Path) -> Option<Self> {
        let wide = wide_path(path);
        unsafe {
            let file = CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            );
            if file == INVALID_HANDLE_VALUE || file.is_null() {
                return None;
            }
            let mut size = 0i64;
            if GetFileSizeEx(file, &mut size) == 0 || size <= 0 {
                CloseHandle(file);
                return None;
            }
            let Ok(len) = usize::try_from(size) else {
                CloseHandle(file);
                return None;
            };
            // AMD's driver reads a dword at the end of the blob in CreatePipelineLibrary
            // (amdxc64.dll+0x176cd5). A view ends with its file's last page, so where the file
            // ends less than 4 bytes before that, the read runs into the unmapped page after it.
            // Such a file (4 lengths in 4096) is read into memory with room after it.
            const PAGE: usize = 4096;
            if (PAGE - len % PAGE) % PAGE < 4 {
                CloseHandle(file);
                let mut copy = std::fs::read(path).ok().filter(|c| c.len() == len)?;
                copy.extend_from_slice(&[0; 4]);
                return Some(Self { path: path.to_path_buf(), file: std::ptr::null_mut(), mapping: std::ptr::null_mut(), view: copy.as_ptr(), len, copy });
            }
            let mapping = CreateFileMappingW(file, std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null());
            if mapping.is_null() {
                CloseHandle(file);
                return None;
            }
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                CloseHandle(mapping);
                CloseHandle(file);
                return None;
            }
            Some(Self {
                path: path.to_path_buf(),
                file,
                mapping,
                view: view.Value as *const u8,
                len,
                copy: Vec::new(),
            })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn as_ptr(&self) -> *const c_void {
        self.view as *const c_void
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.view, self.len) }
    }
}

impl Drop for MappedBlob {
    fn drop(&mut self) {
        unsafe {
            if !self.view.is_null() && self.copy.is_empty() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as *mut c_void });
            }
            if !self.mapping.is_null() {
                CloseHandle(self.mapping);
            }
            if !self.file.is_null() && self.file != INVALID_HANDLE_VALUE {
                CloseHandle(self.file);
            }
        }
    }
}

/// A read-write Windows file mapping for serializing a pipeline library directly into file-backed
/// pages without allocating a matching heap `Vec<u8>`.
pub struct MappedWriteFile {
    path: PathBuf,
    file: HANDLE,
    mapping: HANDLE,
    view: *mut u8,
    len: usize,
    committed: bool,
}

impl MappedWriteFile {
    /// Create (or truncate) `path` with exact byte length `len` and map it writable.
    pub fn create(path: &Path, len: usize) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let size = i64::try_from(len).ok()?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let wide = wide_path(path);
        unsafe {
            let file = CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                std::ptr::null(),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            );
            if file == INVALID_HANDLE_VALUE || file.is_null() {
                return None;
            }
            let mut new_pos = 0i64;
            if SetFilePointerEx(file, size, &mut new_pos, FILE_BEGIN) == 0 || SetEndOfFile(file) == 0 {
                CloseHandle(file);
                let _ = std::fs::remove_file(path);
                return None;
            }
            let mapping = CreateFileMappingW(
                file,
                std::ptr::null(),
                PAGE_READWRITE,
                ((len as u64) >> 32) as u32,
                (len as u64 & 0xFFFF_FFFF) as u32,
                std::ptr::null(),
            );
            if mapping.is_null() {
                CloseHandle(file);
                let _ = std::fs::remove_file(path);
                return None;
            }
            let view = MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, len);
            if view.Value.is_null() {
                CloseHandle(mapping);
                CloseHandle(file);
                let _ = std::fs::remove_file(path);
                return None;
            }
            Some(Self {
                path: path.to_path_buf(),
                file,
                mapping,
                view: view.Value as *mut u8,
                len,
                committed: false,
            })
        }
    }

    pub fn as_mut_ptr(&mut self) -> *mut c_void {
        self.view as *mut c_void
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.view, self.len) }
    }

    /// Unmap and close the file cleanly, keeping the written file on disk.
    pub fn finish(mut self) -> PathBuf {
        self.committed = true;
        self.close_handles();
        self.path.clone()
    }

    fn close_handles(&mut self) {
        unsafe {
            if !self.view.is_null() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view as *mut c_void });
                self.view = std::ptr::null_mut();
            }
            if !self.mapping.is_null() {
                CloseHandle(self.mapping);
                self.mapping = std::ptr::null_mut();
            }
            if !self.file.is_null() && self.file != INVALID_HANDLE_VALUE {
                CloseHandle(self.file);
                self.file = INVALID_HANDLE_VALUE;
            }
        }
    }
}

impl Drop for MappedWriteFile {
    fn drop(&mut self) {
        self.close_handles();
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Generational `.plib` directory manager: each save writes a new `gen-NNNNNNNN.plib` through a
/// `.tmp` staging file so any earlier generation kept memory-mapped by `ID3D12PipelineLibrary` is
/// never truncated or replaced while open.
pub struct LibraryStore {
    dir: PathBuf,
    next_gen: AtomicU64,
}

impl LibraryStore {
    pub fn new(dir: PathBuf) -> Self {
        let latest = latest_generation(&dir).map_or(0, |(g, _)| g);
        Self { dir, next_gen: AtomicU64::new(latest + 1) }
    }

    pub fn for_adapter(adapter_name: &str) -> Self {
        let key = format!("{:016x}", crate::gpu::shader::fnv64(adapter_name.as_bytes()));
        Self::new(crate::gpu::recomp::dxc::cache_dir().join("pso-library").join(key))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Open the newest non-empty generation file as a read-only memory mapping and clean up older
    /// unmapped generations from previous runs.
    pub fn open_latest(&self) -> Option<(u64, MappedBlob)> {
        let (gen, path) = latest_generation(&self.dir)?;
        let blob = MappedBlob::open(&path)?;
        self.next_gen.fetch_max(gen + 1, Ordering::AcqRel);
        cleanup_older_generations(&self.dir, gen);
        Some((gen, blob))
    }

    /// Write a fresh generation of `len` bytes using a file-backed writable mapping and commit it
    /// atomically via rename. Never touches any earlier generation currently kept open.
    pub fn write_generation(&self, len: usize, write_fn: impl FnOnce(&mut [u8]) -> bool) -> Option<(u64, PathBuf)> {
        if len == 0 {
            return None;
        }
        let gen = self.next_gen.fetch_add(1, Ordering::AcqRel);
        let final_path = self.dir.join(generation_name(gen));
        let tmp_path = self.dir.join(format!("gen-{gen:08x}-{}.tmp", std::process::id()));
        let mut mapped = MappedWriteFile::create(&tmp_path, len)?;
        let ok = write_fn(mapped.as_mut_slice());
        if !ok {
            drop(mapped);
            return None;
        }
        let staged = mapped.finish();
        if std::fs::rename(&staged, &final_path).is_err() {
            let _ = std::fs::remove_file(&staged);
            return None;
        }
        Some((gen, final_path))
    }
}

fn generation_name(gen: u64) -> String {
    format!("gen-{gen:08x}.plib")
}

fn parse_generation(file_name: &str) -> Option<u64> {
    let hex = file_name.strip_prefix("gen-")?.strip_suffix(".plib")?;
    if hex.is_empty() {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

fn latest_generation(dir: &Path) -> Option<(u64, PathBuf)> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(gen) = parse_generation(name) else { continue };
        if !entry.metadata().ok().is_some_and(|m| m.len() > 0) {
            continue;
        }
        if best.as_ref().is_none_or(|&(b, _)| gen > b) {
            best = Some((gen, path));
        }
    }
    best
}

fn cleanup_older_generations(dir: &Path, keep_gen: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if parse_generation(name).is_some_and(|g| g < keep_gen) || name.ends_with(".tmp") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Minimum newly stored pipelines before triggering a periodic background generation save.
const SAVE_BATCH_THRESHOLD: u64 = 32;
/// Minimum spacing between periodic background saves while pipelines are streaming in.
const SAVE_MIN_INTERVAL: Duration = Duration::from_secs(15);
/// Idle delay after the last stored pipeline before flushing any remaining tail (`>= 1` dirty).
const SAVE_IDLE_FLUSH: Duration = Duration::from_secs(5);
/// Maximum time `flush()` waits on shutdown so exit can never hang behind a slow driver call.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Pure save-scheduling decision (unit-testable on CPU):
/// - Save immediately when `dirty >= SAVE_BATCH_THRESHOLD` and at least `SAVE_MIN_INTERVAL`
///   elapsed since the last save.
/// - Or save any tail (`dirty > 0`) once no new pipeline has been stored for `SAVE_IDLE_FLUSH`.
pub fn should_background_save(dirty: u64, now_ms: u64, last_store_ms: u64, last_save_ms: u64) -> bool {
    if dirty == 0 {
        return false;
    }
    let since_save = now_ms.saturating_sub(last_save_ms);
    let since_store = now_ms.saturating_sub(last_store_ms);
    (dirty >= SAVE_BATCH_THRESHOLD && since_save >= SAVE_MIN_INTERVAL.as_millis() as u64)
        || (last_store_ms > 0
            && since_store >= SAVE_IDLE_FLUSH.as_millis() as u64
            && since_save >= SAVE_IDLE_FLUSH.as_millis() as u64)
}

struct SendPso(ID3D12PipelineState);
unsafe impl Send for SendPso {}

struct SendLib(ID3D12PipelineLibrary1);
unsafe impl Send for SendLib {}

enum SaveCmd {
    Store(String, SendPso),
    Flush(mpsc::SyncSender<Option<PathBuf>>),
}

unsafe fn create_empty_library(dev1: &ID3D12Device1) -> Option<ID3D12PipelineLibrary1> {
    // Pass an explicit NULL pointer with length 0 rather than `&[]`, whose dangling
    // non-null slice pointer vkd3d-proton rejects with E_INVALIDARG.
    let mut lib: Option<ID3D12PipelineLibrary1> = None;
    (Interface::vtable(dev1).CreatePipelineLibrary)(
        Interface::as_raw(dev1),
        std::ptr::null(),
        0,
        &ID3D12PipelineLibrary1::IID,
        &mut lib as *mut _ as *mut _,
    )
    .ok()
    .ok()?;
    lib
}

/// Maximum idle read-only `ID3D12PipelineLibrary1` handles kept in `read_pool`. Each handle
/// borrows the same startup `MappedBlob` (zero extra blob RAM) so concurrent `Load*` callers
/// never share an `ID3D12PipelineLibrary1` instance and never stall behind one another.
const MAX_POOLED_READ_HANDLES: usize = 16;

struct ReadHandleGuard<'a> {
    pool: &'a std::sync::Mutex<Vec<ID3D12PipelineLibrary1>>,
    lib: Option<ID3D12PipelineLibrary1>,
}

impl ReadHandleGuard<'_> {
    fn handle(&self) -> &ID3D12PipelineLibrary1 {
        self.lib.as_ref().unwrap()
    }
}

impl Drop for ReadHandleGuard<'_> {
    fn drop(&mut self) {
        if let Some(lib) = self.lib.take() {
            let mut pool = self.pool.lock().unwrap();
            if pool.len() < MAX_POOLED_READ_HANDLES {
                pool.push(lib);
            }
        }
    }
}

/// Process-wide D3D12 pipeline library split into:
/// - `read_pool`: checked-out pool of independent read-only `ID3D12PipelineLibrary1` handles
///   opened from the startup `MappedBlob`. Windows `D3D12Core.dll` mutates internal state during
///   `Load*` and crashes (`D3D12Core.dll+0x7dfa`) when two threads call `Load*` concurrently on
///   the same `ID3D12PipelineLibrary1` instance; checking out an exclusive handle per caller keeps
///   every handle strictly single-threaded while preserving lock-free concurrent `Load*` scaling.
/// - `write_lib`: owned exclusively by the dedicated `xcl-plib-save` worker thread, fed via an
///   `mpsc` channel so `store()` returns in ~1 µs with zero lock contention on compile workers and
///   `D3D12Core.dll` never sees concurrent `StorePipeline` + `Load*` or `Serialize` races.
pub struct MmapPipelineLibrary {
    /// Declared BEFORE `read_backing` and `dev1` so Rust drops every pooled handle before
    /// unmapping `read_backing` or releasing `dev1`.
    read_pool: std::sync::Mutex<Vec<ID3D12PipelineLibrary1>>,
    /// Kept alive for the entire lifetime of `read_pool` handles because D3D12 requires the
    /// initial blob memory to remain valid until every `ID3D12PipelineLibrary` is destroyed.
    read_backing: Option<Arc<MappedBlob>>,
    /// Free-threaded D3D12 device used to lazily open additional read-only handles from
    /// `read_backing` when multiple threads call `Load*` concurrently.
    dev1: ID3D12Device1,
    /// Async command channel to the dedicated `xcl-plib-save` thread owning `write_lib`.
    /// The mutex protects only the ~50 ns `mpsc::Sender::send` (never any D3D12 call).
    save_tx: Option<std::sync::Mutex<mpsc::Sender<SaveCmd>>>,
    dirty: Arc<AtomicU64>,
}

// Every `ID3D12PipelineLibrary1` in `read_pool` is exclusively owned by one thread at a time via
// `ReadHandleGuard`; `dev1.CreatePipelineLibrary` is free-threaded per D3D12 spec; `read_backing`
// is read-only immutable memory; `save_tx` is synchronized by its Mutex.
unsafe impl Sync for MmapPipelineLibrary {}
unsafe impl Send for MmapPipelineLibrary {}

impl MmapPipelineLibrary {
    /// Open the latest memory-mapped pipeline library on `device`, falling back to a fresh empty
    /// library if none exists or if the driver rejects the cached generation (driver/adapter mismatch).
    pub unsafe fn open_or_create(device: &ID3D12Device, adapter_name: &str) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let dev1: ID3D12Device1 = device.cast().ok()?;
        let store = LibraryStore::for_adapter(adapter_name);
        let mut initial_read_handles: Vec<ID3D12PipelineLibrary1> = Vec::new();
        let mut backing: Option<Arc<MappedBlob>> = None;

        if let Some((gen, blob)) = store.open_latest() {
            match dev1.CreatePipelineLibrary::<ID3D12PipelineLibrary1>(blob.as_slice()) {
                Ok(lib) => {
                    crate::xlog_info!(
                        "D3D12.X: opened memory-mapped pipeline library gen-{gen:08x} ({:.1} MiB {})",
                        blob.len() as f64 / (1024.0 * 1024.0),
                        if blob.copy.is_empty() { "file-backed" } else { "read into memory: the file ends at a page boundary" }
                    );
                    initial_read_handles.push(lib);
                    backing = Some(Arc::new(blob));
                }
                Err(e) => {
                    let stale_path = blob.path().to_path_buf();
                    let code = e.code().0;
                    drop(blob);
                    if !super::real::driver_cache::refused(code) {
                        // Transient failure (device removed/hung/reset or OOM): preserve the valid
                        // `.plib` on disk instead of starting an empty library that would later
                        // write a smaller generation superseding it.
                        crate::xlog_warn!(
                            "D3D12.X: pipeline library gen-{gen:08x} open failed transiently ({code:#x}) – preserving cached file"
                        );
                        return None;
                    }
                    // Remove any driver-rejected generation (driver/adapter mismatch, corrupt
                    // E_INVALIDARG, etc.) so a broken file does not remain the latest generation.
                    let _ = std::fs::remove_file(&stale_path);
                    let _ = (D3D12_ERROR_DRIVER_VERSION_MISMATCH_HR, D3D12_ERROR_ADAPTER_NOT_FOUND_HR);
                    crate::xlog_info!(
                        "D3D12.X: pipeline library gen-{gen:08x} rejected ({code:#x}) – removed stale file and starting fresh library"
                    );
                }
            }
        }

        // Open a separate write-only `ID3D12PipelineLibrary1` from the same read-only `MappedBlob`
        // (zero extra RAM) so `xcl-plib-save` exclusively owns `StorePipeline` + `Serialize`.
        // If a valid `backing` exists and the 2nd handle fails to open, disable saving for this
        // run rather than overwriting the valid `.plib` with an empty generation.
        let write_lib = match &backing {
            Some(blob) => dev1.CreatePipelineLibrary::<ID3D12PipelineLibrary1>(blob.as_slice()).ok(),
            None => create_empty_library(&dev1),
        };
        if initial_read_handles.is_empty() && write_lib.is_none() {
            return None;
        }

        let dirty = Arc::new(AtomicU64::new(0));
        let save_tx = write_lib.map(|wlib| {
            let (tx, rx) = mpsc::channel::<SaveCmd>();
            let write_backing = backing.clone();
            let dirty_worker = dirty.clone();
            let send_lib = SendLib(wlib);
            let _ = std::thread::Builder::new().name("xcl-plib-save".into()).spawn(move || {
                run_save_worker(send_lib, write_backing, store, rx, dirty_worker);
            });
            std::sync::Mutex::new(tx)
        });

        Some(Self {
            read_pool: std::sync::Mutex::new(initial_read_handles),
            read_backing: backing,
            dev1,
            save_tx,
            dirty,
        })
    }

    /// Check out exclusive ownership of a read-only `ID3D12PipelineLibrary1` handle backed by
    /// `read_backing`, lazily opening a new handle from the same mmap when concurrent threads
    /// overlap. Never holds `read_pool`'s mutex across any D3D12 driver call.
    unsafe fn checkout_read_handle(&self) -> Option<ReadHandleGuard<'_>> {
        let blob = self.read_backing.as_ref()?;
        if let Some(lib) = self.read_pool.lock().unwrap().pop() {
            return Some(ReadHandleGuard { pool: &self.read_pool, lib: Some(lib) });
        }
        let lib = self.dev1.CreatePipelineLibrary::<ID3D12PipelineLibrary1>(blob.as_slice()).ok()?;
        Some(ReadHandleGuard { pool: &self.read_pool, lib: Some(lib) })
    }

    pub unsafe fn load_compute(&self, key: &str, desc: &D3D12_COMPUTE_PIPELINE_STATE_DESC) -> Option<ID3D12PipelineState> {
        if key.is_empty() || desc.pRootSignature.is_none() || desc.CS.pShaderBytecode.is_null() || desc.CS.BytecodeLength == 0 {
            return None;
        }
        let guard = self.checkout_read_handle()?;
        let name = HSTRING::from(key);
        guard.handle().LoadComputePipeline(&name, desc).ok()
    }

    pub unsafe fn load_graphics(&self, key: &str, desc: &D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> Option<ID3D12PipelineState> {
        if key.is_empty() || desc.pRootSignature.is_none() {
            return None;
        }
        let guard = self.checkout_read_handle()?;
        let name = HSTRING::from(key);
        guard.handle().LoadGraphicsPipeline(&name, desc).ok()
    }

    pub unsafe fn load_stream(&self, key: &str, desc: &D3D12_PIPELINE_STATE_STREAM_DESC) -> Option<ID3D12PipelineState> {
        if key.is_empty() || desc.pPipelineStateSubobjectStream.is_null() || desc.SizeInBytes == 0 {
            return None;
        }
        let guard = self.checkout_read_handle()?;
        let name = HSTRING::from(key);
        guard.handle().LoadPipeline(&name, desc).ok()
    }

    /// Enqueue `(key, pso)` for the dedicated `xcl-plib-save` worker thread. Returns immediately
    /// (~1 µs) with zero driver lock contention on compile workers.
    pub unsafe fn store(&self, key: &str, pso: &ID3D12PipelineState) -> bool {
        if key.is_empty() {
            return false;
        }
        let Some(tx) = self.save_tx.as_ref() else { return false };
        tx.lock().unwrap().send(SaveCmd::Store(key.to_string(), SendPso(pso.clone()))).is_ok()
    }

    pub fn dirty_count(&self) -> u64 {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Blocking shutdown flush (e.g. `WM_CLOSE` or `XCL_RUN_SECONDS`): asks `xcl-plib-save` to
    /// drain any queued `StorePipeline` calls and write a fresh `.plib` generation (bounded by
    /// `FLUSH_TIMEOUT` so shutdown can never hang).
    pub unsafe fn flush(&self) -> Option<PathBuf> {
        let tx = self.save_tx.as_ref()?;
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if tx.lock().unwrap().send(SaveCmd::Flush(reply_tx)).is_err() {
            return None;
        }
        reply_rx.recv_timeout(FLUSH_TIMEOUT).ok().flatten()
    }
}

fn run_save_worker(
    write_lib: SendLib,
    _backing: Option<Arc<MappedBlob>>,
    store: LibraryStore,
    rx: mpsc::Receiver<SaveCmd>,
    dirty_counter: Arc<AtomicU64>,
) {
    let lib = write_lib.0;
    let epoch = Instant::now();
    let now_ms = || epoch.elapsed().as_millis() as u64;
    let mut dirty: u64 = 0;
    let mut last_store_ms: u64 = 0;
    let mut last_save_ms: u64 = 0;

    let save_now = |dirty: &mut u64, last_save_ms: &mut u64| -> Option<PathBuf> {
        if *dirty == 0 {
            return None;
        }
        let count = *dirty;
        let size = unsafe { lib.GetSerializedSize() };
        let saved = if size > 0 {
            store.write_generation(size, |buf| unsafe { lib.Serialize(buf) }.is_ok())
        } else {
            None
        };
        if let Some((gen, path)) = saved {
            *dirty = 0;
            dirty_counter.store(0, Ordering::Release);
            *last_save_ms = now_ms();
            crate::xlog_info!(
                "D3D12.X: saved pipeline library gen-{gen:08x} ({:.1} MiB, {} new pipelines)",
                size as f64 / (1024.0 * 1024.0),
                count
            );
            Some(path)
        } else {
            None
        }
    };

    let apply_store = |key: String, pso: SendPso, dirty: &mut u64, last_store_ms: &mut u64| {
        let name = HSTRING::from(key);
        if unsafe { lib.StorePipeline(&name, &pso.0) }.is_ok() {
            *dirty += 1;
            dirty_counter.store(*dirty, Ordering::Release);
            *last_store_ms = now_ms();
        }
        drop(pso);
    };

    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(SaveCmd::Store(key, pso)) => {
                apply_store(key, pso, &mut dirty, &mut last_store_ms);
                // Drain any burst of queued stores before checking periodic save.
                let mut pending_flush = None;
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        SaveCmd::Store(k, p) => apply_store(k, p, &mut dirty, &mut last_store_ms),
                        SaveCmd::Flush(reply) => {
                            pending_flush = Some(reply);
                            break;
                        }
                    }
                }
                if let Some(reply) = pending_flush {
                    let saved = save_now(&mut dirty, &mut last_save_ms);
                    let _ = reply.send(saved);
                } else if should_background_save(dirty, now_ms(), last_store_ms, last_save_ms) {
                    save_now(&mut dirty, &mut last_save_ms);
                }
            }
            Ok(SaveCmd::Flush(reply)) => {
                let mut extra_flushes = Vec::new();
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        SaveCmd::Store(k, p) => apply_store(k, p, &mut dirty, &mut last_store_ms),
                        SaveCmd::Flush(r) => extra_flushes.push(r),
                    }
                }
                let saved = save_now(&mut dirty, &mut last_save_ms);
                let _ = reply.send(saved.clone());
                for r in extra_flushes {
                    let _ = r.send(saved.clone());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if should_background_save(dirty, now_ms(), last_store_ms, last_save_ms) {
                    save_now(&mut dirty, &mut last_save_ms);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = save_now(&mut dirty, &mut last_save_ms);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!("xcl-pso-lib-{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn generation_parsing_and_latest_selection() {
        assert_eq!(parse_generation("gen-00000001.plib"), Some(1));
        assert_eq!(parse_generation("gen-00000010.plib"), Some(0x10));
        assert_eq!(parse_generation("gen-00000002-1234.tmp"), None);
        assert_eq!(parse_generation("gen-.plib"), None);
        assert_eq!(parse_generation("other.plib"), None);

        let dir = TempDir::new("latest");
        std::fs::write(dir.0.join("gen-00000001.plib"), b"a").unwrap();
        std::fs::write(dir.0.join("gen-0000000a.plib"), b"bb").unwrap();
        std::fs::write(dir.0.join("gen-0000000b.plib"), b"").unwrap();
        std::fs::write(dir.0.join("gen-00000002.plib"), b"c").unwrap();
        std::fs::write(dir.0.join("gen-000000ff-99.tmp"), b"tmp").unwrap();
        let (gen, path) = latest_generation(&dir.0).unwrap();
        assert_eq!(gen, 0xA, "empty newer generation must be skipped in favor of last valid non-empty file");
        assert!(path.ends_with("gen-0000000a.plib"));
    }

    #[test]
    fn mapped_blob_rejects_empty_and_missing_files() {
        let dir = TempDir::new("empty");
        assert!(MappedBlob::open(&dir.0.join("missing.plib")).is_none());
        let empty = dir.0.join("empty.plib");
        std::fs::write(&empty, b"").unwrap();
        assert!(MappedBlob::open(&empty).is_none());
    }

    /// The driver reads a dword at the end of the blob: it is readable whatever the file's length,
    /// also where the file ends with its last page or less than 4 bytes before.
    #[test]
    fn mapped_blob_has_a_readable_dword_after_its_end() {
        use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT};
        let dir = TempDir::new("end");
        for len in [4096usize, 8190, 8191, 4093, 4092, 100] {
            let path = dir.0.join(format!("{len}.plib"));
            let payload: Vec<u8> = (0..len).map(|i| (i * 7) as u8 | 1).collect();
            std::fs::write(&path, &payload).unwrap();
            let blob = MappedBlob::open(&path).unwrap();
            assert_eq!(blob.as_slice(), payload.as_slice(), "{len}");
            let end = blob.as_ptr() as usize + len;
            for p in [end, end + 3] {
                let mut i: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                assert!(unsafe { VirtualQuery(p as *const c_void, &mut i, std::mem::size_of_val(&i)) } != 0 && i.State == MEM_COMMIT, "{len}");
            }
            assert_eq!(unsafe { (end as *const u32).read_unaligned() }, 0, "{len}");
        }
    }

    #[test]
    fn mapped_blob_reads_and_keeps_file_open_across_generation_save() {
        let dir = TempDir::new("gen-save");
        let store = LibraryStore::new(dir.0.clone());

        let payload1: Vec<u8> = (0..16_384usize).map(|i| (i.wrapping_mul(31) ^ 0x5A) as u8).collect();
        let (gen1, path1) = store
            .write_generation(payload1.len(), |buf| {
                buf.copy_from_slice(&payload1);
                true
            })
            .expect("gen1 write");
        assert_eq!(gen1, 1);

        // Open gen1 memory-mapped and keep its mapping open while saving gen2.
        let (opened_gen, blob1) = store.open_latest().expect("open gen1");
        assert_eq!(opened_gen, 1);
        assert_eq!(blob1.as_slice(), payload1.as_slice());

        // Rejected/failed serialization must never overwrite gen1 or publish a new generation.
        assert!(store.write_generation(0, |_| true).is_none());
        assert!(store.write_generation(1024, |_| false).is_none());

        let payload2: Vec<u8> = (0..32_768usize).map(|i| (i.wrapping_mul(17) ^ 0xA5) as u8).collect();
        let (gen2, path2) = store
            .write_generation(payload2.len(), |buf| {
                buf.copy_from_slice(&payload2);
                true
            })
            .expect("gen2 write while gen1 mapped");
        assert!(gen2 > gen1);
        assert_ne!(path1, path2);

        // gen1 mapping MUST remain completely valid and unmodified while open.
        assert_eq!(blob1.as_slice(), payload1.as_slice());
        drop(blob1);

        // Re-opening latest selects gen2 and cleans up stale gen1.
        let (opened_gen2, blob2) = store.open_latest().expect("open gen2");
        assert_eq!(opened_gen2, gen2);
        assert_eq!(blob2.as_slice(), payload2.as_slice());
        assert!(!path1.exists(), "older generation should be cleaned up once unmapped");
        drop(blob2);
    }
}
