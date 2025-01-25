use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

use phoenix_api::ibprovider::cmd::{Command, CompletionKind};
use phoenix_api::Handle;
use safeverbs::ffi;

use crate::driver::{Error, DRV_CTX};
use crate::rx_recv_impl;

#[inline]
fn phoenix_ibv_context_as_handle(ibv_ctx: &ffi::ibv_context) -> Handle {
    Handle((ibv_ctx as *const ffi::ibv_context).addr() as u64)
}

/// Ask the provider service to call ibv_open_context(device).
///
/// # Safety
///
/// The `device` ptr must be [valid]. See more in [`CStr::from_ptr`].
///
/// [valid]: core::ptr#safety
#[no_mangle]
pub unsafe extern "C" fn phoenix_cmd_get_context(
    device: *const c_char,
    ibv_ctx: &ffi::ibv_context,
) -> c_int {
    let device = unsafe { CStr::from_ptr(device) };
    let device = device.to_owned().to_string_lossy().into_owned();
    let req = Command::GetContext(device, phoenix_ibv_context_as_handle(ibv_ctx));
    DRV_CTX
        .with(|ctx| {
            ctx.service.send_cmd(req)?;
            rx_recv_impl!(ctx.service, CompletionKind::GetContext, { Ok(0) })
        })
        .unwrap_or_else(|e| e.as_i32())
}

#[no_mangle]
pub extern "C" fn phoenix_cmd_alloc_pd(ibv_ctx: &ffi::ibv_context, pd: &mut ffi::ibv_pd) -> c_int {
    let req = Command::AllocPd(phoenix_ibv_context_as_handle(ibv_ctx));
    DRV_CTX
        .with(|ctx| {
            ctx.service.send_cmd(req)?;
            rx_recv_impl!(ctx.service, CompletionKind::AllocPd, pd_handle, {
                pd.handle = pd_handle.0 as u32;
                pd.context = ibv_ctx as *const ffi::ibv_context as _;
                Ok(0)
            })
        })
        .unwrap_or_else(|e| e.as_i32())
}
