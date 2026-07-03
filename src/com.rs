//! `com` — COM apartment lifecycle for the engine's worker threads.
//!
//! `CLAUDE.md` (COM threading) requires every worker thread that touches COM to
//! call `CoInitializeEx(COINIT_MULTITHREADED)` on entry and `CoUninitialize` on
//! exit. [`ComMta`] is that RAII guard: construct it at the top of a worker
//! thread body and drop it (implicitly, on thread exit) to balance the init.
//! Every engine worker (capture, encode, mux) runs in the **multithreaded
//! apartment** — WGC's free-threaded frame pool, the async hardware MFT, and the
//! Sink Writer are all MTA-friendly.
//!
//! ## Why the engine is all-MTA (and how COM crosses threads)
//! `windows` 0.62 interface types are `!Send + !Sync` — each wraps a bare
//! `NonNull` pointer with no thread-safety marker. The pipeline nonetheless
//! moves D3D11 textures and Media Foundation samples between threads (over
//! `crossbeam` channels and a shared latest-frame cell). This is sound because
//! every object we move is either a multithread-protected, free-threaded
//! D3D11/DXGI object (see [`crate::gpu::GpuContext`]) or an MTA-agile Media
//! Foundation / WGC object, and all threads share the one MTA. The concrete
//! message/frame types that cross threads carry a local `unsafe impl Send` with
//! a `SAFETY` note stating exactly that invariant; there is deliberately **no**
//! blanket `Send` wrapper, so each crossing is justified at its definition. See
//! `DECISIONS.md`.

use core::marker::PhantomData;

use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// RAII guard that initializes the multithreaded apartment for the current
/// thread and uninitializes it on drop. `!Send`/`!Sync` on purpose — a COM
/// apartment is a property of the thread that entered it, so the guard must be
/// created and dropped on the same thread.
pub struct ComMta {
    _not_send: PhantomData<*const ()>,
}

impl ComMta {
    /// Enter the multithreaded apartment on the calling thread.
    pub fn initialize() -> Self {
        // SAFETY: MTA init for the calling thread. `S_FALSE` (the apartment was
        // already initialized as MTA) is not an error; we still pair a
        // `CoUninitialize` on drop to keep the init/uninit count balanced.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        Self {
            _not_send: PhantomData,
        }
    }
}

impl Drop for ComMta {
    fn drop(&mut self) {
        // SAFETY: balances the `CoInitializeEx` in `initialize` on this thread.
        unsafe {
            CoUninitialize();
        }
    }
}

/// RAII guard for the Media Foundation platform. `CLAUDE.md` requires `MFStartup`
/// once (on `main`) and `MFShutdown` on exit; construct one [`MediaFoundation`]
/// early on the main thread and hold it for the process lifetime. MF is
/// reference-counted, so nesting is harmless, but one owner is clearest.
pub struct MediaFoundation {
    _not_send: PhantomData<*const ()>,
}

impl MediaFoundation {
    /// Start the Media Foundation platform (full, including the async work queue).
    pub fn startup() -> Result<Self, windows::core::Error> {
        // SAFETY: `MFStartup` pairs with the `MFShutdown` in `Drop`; `MF_VERSION`
        // is the crate's matching version constant.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
        Ok(Self {
            _not_send: PhantomData,
        })
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        // SAFETY: balances the `MFStartup` in `startup`.
        unsafe {
            let _ = MFShutdown();
        }
    }
}
