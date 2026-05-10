use std::mem::transmute;
use std::path::Path;
use thiserror::Error;
use warp_util::path::TargetDirError;
use windows::core::{s, w, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{COORD, HPCON};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type CreatePseudoConsoleFn =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type ResizePseudoConsoleFn = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type ClosePseudoConsoleFn = unsafe extern "system" fn(HPCON);
type ShowHidePseudoConsoleFn = unsafe extern "system" fn(HPCON, bool) -> HRESULT;
type ReleasePseudoConsoleFn = unsafe extern "system" fn(HPCON) -> HRESULT;

pub struct ConptyApi {
    create: CreatePseudoConsoleFn,
    resize: ResizePseudoConsoleFn,
    close: ClosePseudoConsoleFn,
    show_hide: Option<ShowHidePseudoConsoleFn>,
    release: ReleasePseudoConsoleFn,
}

#[derive(Error, Debug)]
pub enum ConptyApiError {
    #[error("Failed to construct target directory: {0}")]
    NoTargetDirectory(#[from] TargetDirError),
    #[error(
        "Failed to load ConPTY library module: {windows_error:#}. DLL file exists: {dll_file_exists:?}"
    )]
    LoadLibraryFailed {
        #[source]
        windows_error: windows::core::Error,
        dll_file_exists: Result<bool, std::io::Error>,
    },
    #[error("Failed to get procedure address for {fn_name:?}")]
    GetProcAddressFailed { fn_name: String },
}

impl ConptyApi {
    pub(super) unsafe fn load() -> Result<Self, ConptyApiError> {
        type LoadedFn = unsafe extern "system" fn() -> isize;

        // On Windows 10 1903+ the ConPTY functions live in conpty.dll with
        // "Conpty" prefixed names. On Windows 11 they were moved into
        // kernel32.dll with plain names (no "Conpty" prefix) and the
        // standalone conpty.dll no longer exists. Try conpty.dll first,
        // then kernel32.dll.
        let mut conpty_module = None;
        let mut last_error = None;
        for dll in [w!("conpty.dll"), w!("kernel32.dll")] {
            match LoadLibraryW(dll) {
                Ok(m) => {
                    conpty_module = Some(m);
                    break;
                }
                Err(e) => last_error = Some(e),
            }
        }
        let conpty_module = match conpty_module {
            Some(m) => m,
            None => {
                let windows_error = last_error.unwrap();
                let dll_file_exists = Path::new("./conpty.dll").try_exists();
                return Err(ConptyApiError::LoadLibraryFailed {
                    windows_error,
                    dll_file_exists,
                });
            }
        };

        // Try plain name (kernel32) first, then Conpty-prefixed name (conpty.dll).
        unsafe fn load_proc(
            module: windows::Win32::Foundation::HMODULE,
            plain: PCSTR,
            conpty_prefixed: PCSTR,
        ) -> Option<unsafe extern "system" fn() -> isize> {
            GetProcAddress(module, plain).or_else(|| GetProcAddress(module, conpty_prefixed))
        }

        let create = load_proc(conpty_module, s!("CreatePseudoConsole"), s!("ConptyCreatePseudoConsole"))
            .ok_or_else(|| ConptyApiError::GetProcAddressFailed {
                fn_name: "CreatePseudoConsole".to_string(),
            })?;
        let resize = load_proc(conpty_module, s!("ResizePseudoConsole"), s!("ConptyResizePseudoConsole"))
            .ok_or_else(|| ConptyApiError::GetProcAddressFailed {
                fn_name: "ResizePseudoConsole".to_string(),
            })?;
        let close = load_proc(conpty_module, s!("ClosePseudoConsole"), s!("ConptyClosePseudoConsole"))
            .ok_or_else(|| ConptyApiError::GetProcAddressFailed {
                fn_name: "ClosePseudoConsole".to_string(),
            })?;
        // ShowHidePseudoConsole is optional — not present on all Windows 11 builds.
        let show_hide = load_proc(conpty_module, s!("ShowHidePseudoConsole"), s!("ConptyShowHidePseudoConsole"));
        let release = load_proc(conpty_module, s!("ReleasePseudoConsole"), s!("ConptyReleasePseudoConsole"))
            .ok_or_else(|| ConptyApiError::GetProcAddressFailed {
                fn_name: "ReleasePseudoConsole".to_string(),
            })?;

        Ok(ConptyApi {
            create: transmute::<LoadedFn, CreatePseudoConsoleFn>(create),
            resize: transmute::<LoadedFn, ResizePseudoConsoleFn>(resize),
            close: transmute::<LoadedFn, ClosePseudoConsoleFn>(close),
            show_hide: show_hide.map(|f| transmute::<LoadedFn, ShowHidePseudoConsoleFn>(f)),
            release: transmute::<LoadedFn, ReleasePseudoConsoleFn>(release),
        })
    }

    pub(super) unsafe fn create(
        &self,
        size: COORD,
        mut pipe: HANDLE,
        flags: u32,
    ) -> Result<HPCON, windows::core::Error> {
        let mut pty_handle = HPCON::default();
        let result = (self.create)(size, pipe, pipe, flags, &mut pty_handle)
            .ok()
            .map(|_| pty_handle);
        windows::core::Free::free(&mut pipe);
        result
    }

    pub(super) unsafe fn resize(
        &self,
        pty_handle: HPCON,
        size: COORD,
    ) -> Result<(), windows::core::Error> {
        (self.resize)(pty_handle, size).ok()
    }

    pub(super) unsafe fn close(&self, pty_handle: HPCON) {
        (self.close)(pty_handle)
    }

    pub(super) unsafe fn show_hide(
        &self,
        pty_handle: HPCON,
        visible: bool,
    ) -> windows::core::Result<()> {
        if let Some(f) = &self.show_hide {
            f(pty_handle, visible).ok()
        } else {
            Ok(())
        }
    }

    pub(super) unsafe fn release(&self, pty_handle: HPCON) -> windows::core::Result<()> {
        (self.release)(pty_handle).ok()
    }
}
