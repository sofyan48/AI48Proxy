use anyhow::{Context, Result, bail, ensure};
#[cfg(windows)]
use std::ffi::CStr;
use std::{
    ffi::{CString, c_char, c_int, c_void},
    path::{Path, PathBuf},
    slice,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct AbiVersion(pub u32);
impl AbiVersion {
    pub const CURRENT: Self = Self(1);
}

#[repr(C)]
#[derive(Debug)]
pub struct PluginBuffer {
    pub ptr: *mut u8,
    pub len: usize,
}
unsafe impl Send for PluginBuffer {}
unsafe impl Sync for PluginBuffer {}

#[repr(C)]
struct HostApi {
    abi_version: u32,
    host_ctx: *mut c_void,
    call: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const u8,
            usize,
            *mut PluginBuffer,
        ) -> c_int,
    >,
    free_buffer: Option<unsafe extern "C" fn(*mut c_void, usize)>,
}

#[repr(C)]
struct PluginApi {
    abi_version: u32,
    call: Option<unsafe extern "C" fn(*const c_char, *const u8, usize, *mut PluginBuffer) -> c_int>,
    free_buffer: Option<unsafe extern "C" fn(*mut u8, usize)>,
    shutdown: Option<unsafe extern "C" fn()>,
}

type Init = unsafe extern "C" fn(*const HostApi, *mut PluginApi) -> c_int;

pub struct NativePlugin {
    path: PathBuf,
    #[cfg(unix)]
    handle: *mut c_void,
    #[cfg(windows)]
    handle: *mut c_void,
    api: PluginApi,
}
unsafe impl Send for NativePlugin {}
unsafe impl Sync for NativePlugin {}

impl NativePlugin {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        ensure!(path.is_file(), "plugin library does not exist");
        let handle = unsafe { open_library(&path)? };
        let init: Init = unsafe { load_symbol(handle, b"cliproxy_plugin_init\0")? };
        let host = HostApi {
            abi_version: AbiVersion::CURRENT.0,
            host_ctx: std::ptr::null_mut(),
            call: None,
            free_buffer: None,
        };
        let mut api = PluginApi {
            abi_version: 0,
            call: None,
            free_buffer: None,
            shutdown: None,
        };
        let code = unsafe { init(&host, &mut api) };
        if code != 0 {
            unsafe {
                close_library(handle);
            }
            bail!("plugin initialization failed with code {code}");
        }
        ensure!(
            api.abi_version == AbiVersion::CURRENT.0,
            "unsupported plugin ABI version"
        );
        ensure!(
            api.call.is_some() && api.free_buffer.is_some() && api.shutdown.is_some(),
            "plugin API is incomplete"
        );
        Ok(Self { path, handle, api })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn call(&self, method: &str, request: &[u8]) -> Result<Vec<u8>> {
        let method = CString::new(method).context("plugin method contains NUL")?;
        let mut output = PluginBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
        };
        let code = unsafe {
            (self.api.call.expect("validated plugin call"))(
                method.as_ptr(),
                request.as_ptr(),
                request.len(),
                &mut output,
            )
        };
        if code != 0 {
            bail!("plugin call failed with code {code}");
        }
        if output.len == 0 {
            return Ok(Vec::new());
        }
        ensure!(
            !output.ptr.is_null(),
            "plugin returned a null response buffer"
        );
        let bytes = unsafe { slice::from_raw_parts(output.ptr, output.len).to_vec() };
        unsafe {
            (self.api.free_buffer.expect("validated plugin free"))(output.ptr, output.len);
        }
        Ok(bytes)
    }
}

impl Drop for NativePlugin {
    fn drop(&mut self) {
        if let Some(shutdown) = self.api.shutdown {
            unsafe {
                shutdown();
            }
        }
        unsafe {
            close_library(self.handle);
        }
    }
}

#[cfg(unix)]
unsafe fn open_library(path: &Path) -> Result<*mut c_void> {
    let path =
        CString::new(path.as_os_str().as_encoded_bytes()).context("plugin path contains NUL")?;
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    ensure!(!handle.is_null(), "unable to load native plugin");
    Ok(handle)
}
#[cfg(unix)]
unsafe fn load_symbol<T: Copy>(handle: *mut c_void, name: &[u8]) -> Result<T> {
    let symbol = unsafe { libc::dlsym(handle, name.as_ptr().cast()) };
    ensure!(!symbol.is_null(), "native plugin init symbol is missing");
    Ok(unsafe { std::mem::transmute_copy(&symbol) })
}
#[cfg(unix)]
unsafe fn close_library(handle: *mut c_void) {
    if !handle.is_null() {
        unsafe {
            libc::dlclose(handle);
        }
    }
}

#[cfg(windows)]
unsafe fn open_library(path: &Path) -> Result<*mut c_void> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
    ensure!(handle != 0, "unable to load native plugin");
    Ok(handle as *mut c_void)
}
#[cfg(windows)]
unsafe fn load_symbol<T: Copy>(handle: *mut c_void, name: &[u8]) -> Result<T> {
    let name = CStr::from_bytes_with_nul(name)?;
    let symbol = unsafe {
        windows_sys::Win32::System::LibraryLoader::GetProcAddress(handle as _, name.as_ptr().cast())
    };
    ensure!(!symbol.is_null(), "native plugin init symbol is missing");
    Ok(unsafe { std::mem::transmute_copy(&symbol) })
}
#[cfg(windows)]
unsafe fn close_library(handle: *mut c_void) {
    if !handle.is_null() {
        unsafe {
            windows_sys::Win32::Foundation::FreeLibrary(handle as _);
        }
    }
}
