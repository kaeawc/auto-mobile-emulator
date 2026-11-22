//! Runtime binding to gfxstream's `stream_renderer_*` C API (virtio-gpu 3D on the host).
//!
//! The Android SDK ships the renderer as `emulator/lib64/libgfxstream_backend.dylib`; it is
//! `dlopen`ed here so emu-rs needs no build-time dependency on it. The renderer is process-wide
//! state, so there is at most one [`Renderer`] per process. Signatures follow
//! `docs/reference/gfxstream/virtio-gpu-gfxstream-renderer.h`.
#![cfg(target_os = "macos")]
// `sym!` transmutes dlsym results to the type of the struct field it initializes.
#![allow(clippy::missing_transmute_annotations)]

mod macho;

use std::collections::HashSet;
use std::ffi::{CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

pub use emu_hvf::HostRange;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("loading {0}: {1}")]
    Load(String, String),
    #[error("missing symbol {0}")]
    Symbol(&'static str),
    #[error("{0} failed ({1})")]
    Call(&'static str, i32),
    #[error("the gfxstream renderer is already initialized in this process")]
    AlreadyInitialized,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Capability sets gfxstream exposes (virtio-gpu capset ids).
pub mod capset {
    pub const GFXSTREAM_VULKAN: u32 = 3;
    pub const GFXSTREAM_GLES: u32 = 8;
    pub const GFXSTREAM_COMPOSER: u32 = 9;
    pub const ALL: [u32; 3] = [GFXSTREAM_VULKAN, GFXSTREAM_GLES, GFXSTREAM_COMPOSER];
}

/// `RendererFlags`.
pub mod flags {
    pub const USE_EGL: u64 = 1 << 0;
    pub const USE_GLES: u64 = 1 << 4;
    pub const USE_VK: u64 = 1 << 5;
}

pub const FENCE_FLAG: u32 = 1 << 0;
pub const FENCE_FLAG_RING_IDX: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fence {
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceCreateArgs {
    pub handle: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Box3 {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

#[repr(C)]
struct Iov {
    base: *mut c_void,
    len: usize,
}

#[repr(C)]
struct Param {
    key: u64,
    value: u64,
}

#[repr(C)]
struct Command {
    ctx_id: u32,
    cmd_size: u32,
    cmd: *mut u8,
    num_in_fences: u32,
    fences: *mut c_void,
}

#[repr(C)]
struct CreateBlob {
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
}

#[repr(C)]
struct Debug {
    debug_type: u32,
    message: *const c_char,
}

type FenceCb = extern "C" fn(*mut c_void, *mut Fence);
type DebugCb = extern "C" fn(*mut c_void, *mut Debug);

struct Api {
    init: unsafe extern "C" fn(*mut Param, u64) -> c_int,
    get_cap_set: unsafe extern "C" fn(u32, *mut u32, *mut u32),
    fill_caps: unsafe extern "C" fn(u32, u32, *mut c_void),
    context_create: unsafe extern "C" fn(u32, u32, *const c_char, u32) -> c_int,
    context_destroy: unsafe extern "C" fn(u32),
    resource_create: unsafe extern "C" fn(*mut ResourceCreateArgs, *mut Iov, u32) -> c_int,
    resource_unref: unsafe extern "C" fn(u32),
    attach_iov: unsafe extern "C" fn(c_int, *mut Iov, c_int) -> c_int,
    detach_iov: unsafe extern "C" fn(c_int, *mut *mut Iov, *mut c_int),
    ctx_attach_resource: unsafe extern "C" fn(c_int, c_int),
    ctx_detach_resource: unsafe extern "C" fn(c_int, c_int),
    submit_cmd: unsafe extern "C" fn(*mut Command) -> c_int,
    transfer_read_iov: unsafe extern "C" fn(u32, u32, u32, u32, u32, *mut Box3, u64, *mut Iov, c_int) -> c_int,
    transfer_write_iov: unsafe extern "C" fn(u32, u32, c_int, u32, u32, *mut Box3, u64, *mut Iov, u32) -> c_int,
    create_blob: unsafe extern "C" fn(u32, u32, *const CreateBlob, *const Iov, u32, *const c_void) -> c_int,
    resource_map: unsafe extern "C" fn(u32, *mut *mut c_void, *mut u64) -> c_int,
    resource_unmap: unsafe extern "C" fn(u32) -> c_int,
    resource_map_info: unsafe extern "C" fn(u32, *mut u32) -> c_int,
    create_fence: unsafe extern "C" fn(*const Fence) -> c_int,
}

fn dlerror() -> String {
    // SAFETY: dlerror returns a thread-local C string or null.
    unsafe {
        let e = libc::dlerror();
        if e.is_null() { "unknown error".into() } else { std::ffi::CStr::from_ptr(e).to_string_lossy().into_owned() }
    }
}

fn dlopen(path: &Path, global: bool) -> Result<*mut c_void> {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| Error::Load(path.display().to_string(), e.to_string()))?;
    let mode = libc::RTLD_NOW | if global { libc::RTLD_GLOBAL } else { libc::RTLD_LOCAL };
    // SAFETY: valid NUL-terminated path; the handle is intentionally never closed.
    let h = unsafe { libc::dlopen(c.as_ptr(), mode) };
    if h.is_null() { Err(Error::Load(path.display().to_string(), dlerror())) } else { Ok(h) }
}

macro_rules! sym {
    ($h:expr, $name:literal) => {{
        // SAFETY: the symbol name is a NUL-terminated literal; the transmute target matches the
        // C prototype in virtio-gpu-gfxstream-renderer.h.
        unsafe {
            let p = libc::dlsym($h, concat!($name, "\0").as_ptr().cast());
            if p.is_null() {
                return Err(Error::Symbol($name));
            }
            std::mem::transmute::<*mut c_void, _>(p)
        }
    }};
}

impl Api {
    fn load(h: *mut c_void) -> Result<Api> {
        Ok(Api {
            init: sym!(h, "stream_renderer_init"),
            get_cap_set: sym!(h, "stream_renderer_get_cap_set"),
            fill_caps: sym!(h, "stream_renderer_fill_caps"),
            context_create: sym!(h, "stream_renderer_context_create"),
            context_destroy: sym!(h, "stream_renderer_context_destroy"),
            resource_create: sym!(h, "stream_renderer_resource_create"),
            resource_unref: sym!(h, "stream_renderer_resource_unref"),
            attach_iov: sym!(h, "stream_renderer_resource_attach_iov"),
            detach_iov: sym!(h, "stream_renderer_resource_detach_iov"),
            ctx_attach_resource: sym!(h, "stream_renderer_ctx_attach_resource"),
            ctx_detach_resource: sym!(h, "stream_renderer_ctx_detach_resource"),
            submit_cmd: sym!(h, "stream_renderer_submit_cmd"),
            transfer_read_iov: sym!(h, "stream_renderer_transfer_read_iov"),
            transfer_write_iov: sym!(h, "stream_renderer_transfer_write_iov"),
            create_blob: sym!(h, "stream_renderer_create_blob"),
            resource_map: sym!(h, "stream_renderer_resource_map"),
            resource_unmap: sym!(h, "stream_renderer_resource_unmap"),
            resource_map_info: sym!(h, "stream_renderer_resource_map_info"),
            create_fence: sym!(h, "stream_renderer_create_fence"),
        })
    }
}

type FenceHandler = Arc<dyn Fn(Fence) + Send + Sync>;
static FENCE_HANDLER: OnceLock<FenceHandler> = OnceLock::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_fence(_: *mut c_void, fence: *mut Fence) {
    if fence.is_null() {
        return;
    }
    // SAFETY: the renderer passes a valid fence for the duration of the call.
    let f = unsafe { *fence };
    if let Some(h) = FENCE_HANDLER.get() {
        h(f);
    }
}

extern "C" fn on_debug(_: *mut c_void, d: *mut Debug) {
    if d.is_null() {
        return;
    }
    // SAFETY: valid for the duration of the call; message is NUL-terminated.
    let (kind, msg) = unsafe { ((*d).debug_type, std::ffi::CStr::from_ptr((*d).message).to_string_lossy().into_owned()) };
    // 1 error, 2 warn, 3 info, 4 debug.
    if kind <= 2 || (kind == 3 && VERBOSE.load(Ordering::Relaxed)) {
        eprintln!("gfxstream: {}", msg.trim_end());
    }
}

fn iovs(ranges: &[HostRange]) -> Vec<Iov> {
    ranges.iter().map(|r| Iov { base: r.as_ptr().cast(), len: r.len() }).collect()
}

fn ok(name: &'static str, rc: c_int) -> Result<()> {
    if rc == 0 { Ok(()) } else { Err(Error::Call(name, rc)) }
}

/// The initialized renderer. Calls may come from any thread.
pub struct Renderer {
    api: Api,
}

/// The renderer library inside `lib64`.
pub const BACKEND: &str = "libgfxstream_backend.dylib";

/// The libraries `root` (a file in `lib64`) links from `lib64` itself, directly or indirectly,
/// dependencies before their dependents; `root` is not included. Only `@rpath/<name>` and
/// `@loader_path/<name>` references to files present in `lib64` count.
pub fn bundled_deps(lib64: &Path, root: &str) -> Vec<PathBuf> {
    fn visit(lib64: &Path, name: &str, seen: &mut HashSet<String>, out: &mut Vec<PathBuf>) {
        if !seen.insert(name.to_string()) {
            return;
        }
        let path = lib64.join(name);
        for dep in macho::dylib_deps(&path).unwrap_or_default() {
            let Some(dep) = dep.strip_prefix("@rpath/").or_else(|| dep.strip_prefix("@loader_path/")) else { continue };
            if !dep.contains('/') && lib64.join(dep).is_file() {
                visit(lib64, dep, seen, out);
            }
        }
        out.push(path);
    }
    let mut out = Vec::new();
    visit(lib64, root, &mut HashSet::new(), &mut out);
    out.pop();
    out
}

impl Renderer {
    /// Loads the renderer from a `lib64` directory: the SDK's `emulator/lib64` or one installed by
    /// `emu -provision-renderer`. The backend's `@rpath` dependencies are preloaded from the same
    /// directory, leaves first, so dyld finds each one already loaded by install name and the
    /// executable needs no rpath to it.
    pub fn load(lib64: &Path) -> Result<Renderer> {
        for dep in bundled_deps(lib64, BACKEND) {
            dlopen(&dep, true)?;
        }
        let h = dlopen(&lib64.join(BACKEND), false)?;
        Ok(Renderer { api: Api::load(h)? })
    }

    /// Initializes the renderer for a `width`×`height` primary display. `on_fence` runs on a
    /// renderer thread whenever a fence created with [`Renderer::create_fence`] signals.
    pub fn init(&self, width: u32, height: u32, renderer_flags: u64, verbose: bool, on_fence: FenceHandler) -> Result<()> {
        if INITIALIZED.swap(true, Ordering::SeqCst) {
            return Err(Error::AlreadyInitialized);
        }
        VERBOSE.store(verbose, Ordering::Relaxed);
        let _ = FENCE_HANDLER.set(on_fence);
        let mut params = [
            Param { key: 1, value: 0 },
            Param { key: 2, value: renderer_flags },
            Param { key: 3, value: on_fence_ptr() },
            Param { key: 4, value: u64::from(width) },
            Param { key: 5, value: u64::from(height) },
            Param { key: 6, value: on_debug_ptr() },
        ];
        // SAFETY: params outlive the call; the callbacks are 'static extern fns.
        ok("stream_renderer_init", unsafe { (self.api.init)(params.as_mut_ptr(), params.len() as u64) })
    }

    /// (max version, max size) of a capability set, or None if unsupported.
    pub fn capset_info(&self, id: u32) -> Option<(u32, u32)> {
        let (mut ver, mut size) = (0u32, 0u32);
        // SAFETY: out-pointers reference live locals.
        unsafe { (self.api.get_cap_set)(id, &mut ver, &mut size) };
        (size > 0).then_some((ver, size))
    }

    pub fn capset(&self, id: u32, version: u32) -> Vec<u8> {
        let Some((_, size)) = self.capset_info(id) else { return Vec::new() };
        let mut buf = vec![0u8; size as usize];
        // SAFETY: buf has exactly the size the renderer reported for this capset.
        unsafe { (self.api.fill_caps)(id, version, buf.as_mut_ptr().cast()) };
        buf
    }

    pub fn context_create(&self, ctx_id: u32, name: &[u8], context_init: u32) -> Result<()> {
        // SAFETY: name pointer/length pair describes a live slice.
        ok("context_create", unsafe { (self.api.context_create)(ctx_id, name.len() as u32, name.as_ptr().cast(), context_init) })
    }

    pub fn context_destroy(&self, ctx_id: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.context_destroy)(ctx_id) }
    }

    /// Creates a resource, optionally backed by guest memory ranges.
    pub fn resource_create(&self, mut args: ResourceCreateArgs, backing: &[HostRange]) -> Result<()> {
        let mut v = iovs(backing);
        let p = if v.is_empty() { std::ptr::null_mut() } else { v.as_mut_ptr() };
        // SAFETY: the iovecs point into guest RAM, which outlives the renderer.
        ok("resource_create", unsafe { (self.api.resource_create)(&mut args, p, v.len() as u32) })
    }

    pub fn resource_unref(&self, id: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.resource_unref)(id) }
    }

    pub fn attach_backing(&self, id: u32, backing: &[HostRange]) -> Result<()> {
        // The renderer keeps the iovec array, so it must outlive the resource: leak it.
        let v = Box::leak(iovs(backing).into_boxed_slice());
        // SAFETY: the array is leaked (lives forever) and points into guest RAM.
        ok("attach_iov", unsafe { (self.api.attach_iov)(id as c_int, v.as_mut_ptr(), v.len() as c_int) })
    }

    pub fn detach_backing(&self, id: u32) {
        let (mut p, mut n) = (std::ptr::null_mut(), 0);
        // SAFETY: out-pointers reference live locals.
        unsafe { (self.api.detach_iov)(id as c_int, &mut p, &mut n) }
    }

    pub fn ctx_attach_resource(&self, ctx: u32, res: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.ctx_attach_resource)(ctx as c_int, res as c_int) }
    }

    pub fn ctx_detach_resource(&self, ctx: u32, res: u32) {
        // SAFETY: plain call.
        unsafe { (self.api.ctx_detach_resource)(ctx as c_int, res as c_int) }
    }

    pub fn submit(&self, ctx: u32, cmd: &mut [u8]) -> Result<()> {
        let mut c =
            Command { ctx_id: ctx, cmd_size: cmd.len() as u32, cmd: cmd.as_mut_ptr(), num_in_fences: 0, fences: std::ptr::null_mut() };
        // SAFETY: the command buffer lives for the call.
        ok("submit_cmd", unsafe { (self.api.submit_cmd)(&mut c) })
    }

    /// Copies resource contents into `dst` (guest backing when `dst` is empty).
    #[allow(clippy::too_many_arguments)]
    pub fn transfer_read(
        &self,
        res: u32,
        ctx: u32,
        level: u32,
        stride: u32,
        layer_stride: u32,
        mut b: Box3,
        offset: u64,
        dst: &[HostRange],
    ) -> Result<()> {
        let mut v = iovs(dst);
        let p = if v.is_empty() { std::ptr::null_mut() } else { v.as_mut_ptr() };
        // SAFETY: iovecs (if any) point into memory valid for the call.
        ok("transfer_read_iov", unsafe {
            (self.api.transfer_read_iov)(res, ctx, level, stride, layer_stride, &mut b, offset, p, v.len() as c_int)
        })
    }

    /// Reads into a host buffer (e.g. a scanout frame).
    #[allow(clippy::too_many_arguments)]
    pub fn read_into(&self, res: u32, stride: u32, b: Box3, out: &mut [u8]) -> Result<()> {
        // SAFETY: `out` is a live, exclusively borrowed buffer for the duration of the call.
        let r = unsafe { HostRange::from_raw(out.as_mut_ptr(), out.len()) };
        self.transfer_read(res, 0, 0, stride, 0, b, 0, &[r])
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transfer_write(&self, res: u32, ctx: u32, level: u32, stride: u32, layer_stride: u32, mut b: Box3, offset: u64) -> Result<()> {
        // SAFETY: no iovecs: the renderer uses the resource's attached guest backing.
        ok("transfer_write_iov", unsafe {
            (self.api.transfer_write_iov)(res, ctx, level as c_int, stride, layer_stride, &mut b, offset, std::ptr::null_mut(), 0)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_blob(
        &self,
        ctx: u32,
        res: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
        backing: &[HostRange],
    ) -> Result<()> {
        let b = CreateBlob { blob_mem, blob_flags, blob_id, size };
        let v = Box::leak(iovs(backing).into_boxed_slice());
        let p = if v.is_empty() { std::ptr::null() } else { v.as_ptr() };
        // SAFETY: the blob descriptor lives for the call; the iovec array is leaked because the
        // renderer may keep it for guest-memory blobs.
        ok("create_blob", unsafe { (self.api.create_blob)(ctx, res, &b, p, v.len() as u32, std::ptr::null()) })
    }

    /// Host mapping of a mappable blob.
    pub fn map(&self, res: u32) -> Result<HostRange> {
        let (mut p, mut size) = (std::ptr::null_mut(), 0u64);
        // SAFETY: out-pointers reference live locals.
        ok("resource_map", unsafe { (self.api.resource_map)(res, &mut p, &mut size) })?;
        // SAFETY: the renderer keeps the mapping alive until resource_unmap/unref.
        Ok(unsafe { HostRange::from_raw(p.cast(), size as usize) })
    }

    pub fn unmap(&self, res: u32) -> Result<()> {
        // SAFETY: plain call.
        ok("resource_unmap", unsafe { (self.api.resource_unmap)(res) })
    }

    pub fn map_info(&self, res: u32) -> u32 {
        let mut info = 0u32;
        // SAFETY: out-pointer references a live local.
        let _ = unsafe { (self.api.resource_map_info)(res, &mut info) };
        info
    }

    pub fn create_fence(&self, f: Fence) -> Result<()> {
        // SAFETY: the fence lives for the call.
        ok("create_fence", unsafe { (self.api.create_fence)(&f) })
    }
}

fn on_fence_ptr() -> u64 {
    on_fence as FenceCb as usize as u64
}

fn on_debug_ptr() -> u64 {
    on_debug as DebugCb as usize as u64
}

/// Points the renderer at its Vulkan loader and ICDs (MoltenVK by default). gfxstream resolves
/// `lib64/vulkan/*` and `lib64/gles_*` relative to its "launcher directory", which defaults to
/// the executable's directory; set it to the parent of the renderer's `lib64` (the SDK's
/// `emulator/`, or `<sdk>/emu-rs/renderer/<revision>/`).
///
/// Call before spawning threads that might read the environment.
pub fn configure_environment(emulator_dir: &Path, icd: Option<&str>) {
    let loader = emulator_dir.join("lib64/vulkan/libvulkan.dylib");
    let mut vars = vec![("ANDROID_EMULATOR_LAUNCHER_DIR", emulator_dir.as_os_str().to_owned())];
    if loader.is_file() {
        vars.push(("ANDROID_EMU_VK_LOADER_PATH", loader.into_os_string()));
    }
    if let Some(icd) = icd {
        vars.push(("ANDROID_EMU_VK_ICD", icd.into()));
    }
    for (k, v) in vars {
        if std::env::var_os(k).is_none() {
            // SAFETY: documented precondition: called while the process is single-threaded
            // with respect to environment access.
            unsafe { std::env::set_var(k, v) };
        }
    }
}

/// Makes `<exe dir>/lib64` lead to `lib64`. gfxstream `dlopen`s some libraries by bare name
/// (`libshadertranslator.dylib`), which dyld resolves only through the executable's rpaths, one
/// of which is `@executable_path/lib64` (`apps/emu/build.rs`). In an SDK overlay `lib64` is
/// already there (a real directory, left alone); for a build in `target/` a symlink is created,
/// or repointed when it leads to another renderer (an old revision, another SDK). Best effort:
/// skipped if the directory isn't writable. Returns the link when it leads to `lib64`.
pub fn link_beside_exe(lib64: &Path) -> Option<PathBuf> {
    let link = std::env::current_exe().ok()?.parent()?.join("lib64");
    match std::fs::read_link(&link) {
        Ok(target) if target == lib64 => return Some(link),
        Ok(_) => {
            // Replace atomically: a concurrent emu may be resolving through the old link.
            let tmp = link.with_file_name(format!(".lib64.{}", std::process::id()));
            let _ = std::fs::remove_file(&tmp);
            std::os::unix::fs::symlink(lib64, &tmp).ok()?;
            std::fs::rename(&tmp, &link).ok()?;
        }
        Err(_) if link.symlink_metadata().is_ok() => return None, // a real directory
        Err(_) => std::os::unix::fs::symlink(lib64, &link).ok()?,
    }
    Some(link)
}

/// A renderer mapping widened to `len` bytes (whole guest pages). Renderer allocations are
/// page-aligned and page-granular, so the extra tail is part of the same allocation.
pub fn page_range(host: HostRange, len: usize) -> HostRange {
    // SAFETY: see above; the range starts at the renderer's page-aligned mapping.
    unsafe { HostRange::from_raw(host.as_ptr(), len.max(host.len())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_deps_are_ordered_leaves_first() {
        let dir = std::env::temp_dir().join(format!("emu-gfxstream-deps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let write = |name: &str, deps: &[&str]| {
            std::fs::write(dir.join(name), macho::tests::fake_dylib(&format!("@rpath/{name}"), deps)).expect("write");
        };
        // backend -> common -> {metrics -> abseil, abseil}; backend -> abseil; a cycle back to
        // common; system and missing libraries are skipped; unrelated files aren't loaded.
        write(BACKEND, &["@rpath/libcommon.dylib", "@rpath/libabseil.dylib", "/usr/lib/libc++.1.dylib", "@rpath/libmissing.dylib"]);
        write("libcommon.dylib", &["@rpath/libmetrics.dylib", "@loader_path/libabseil.dylib"]);
        write("libmetrics.dylib", &["@rpath/libabseil.dylib", "@rpath/libcommon.dylib"]);
        write("libabseil.dylib", &[]);
        write("libunrelated.dylib", &[]);
        let order: Vec<String> =
            bundled_deps(&dir, BACKEND).iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
        assert_eq!(order, ["libabseil.dylib", "libmetrics.dylib", "libcommon.dylib"]);
        assert!(bundled_deps(&dir.join("nowhere"), BACKEND).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
