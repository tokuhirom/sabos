// syscall/graphics.rs — グラフィックス関連システムコール
//
// SYS_GET_FB_INFO, SYS_MOUSE_READ, SYS_DRAW_PIXEL/RECT/LINE/BLIT/TEXT

use crate::user_ptr::{UserSlice, SyscallError};
use super::user_slice_from_args;

/// SYS_GET_FB_INFO: フレームバッファ情報を取得する
///
/// 引数:
///   arg1 — 書き込み先バッファのポインタ（ユーザー空間）
///   arg2 — バッファ長
///
/// 戻り値:
///   書き込んだバイト数（成功時）
///   負の値（エラー時）
pub(crate) fn sys_get_fb_info(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_len = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    let Some(info) = crate::framebuffer::screen_info() else {
        return Err(SyscallError::Other);
    };

    let info_size = core::mem::size_of::<crate::framebuffer::FramebufferInfoSmall>();
    if buf_len < info_size {
        return Err(SyscallError::BufferOverflow);
    }

    let bytes = unsafe {
        core::slice::from_raw_parts(
            &info as *const _ as *const u8,
            info_size,
        )
    };
    buf[..info_size].copy_from_slice(bytes);
    Ok(info_size as u64)
}

/// SYS_MOUSE_READ: マウス状態を取得する
///
/// 引数:
///   arg1 — 書き込み先バッファ（ユーザー空間）
///   arg2 — バッファ長
///
/// 戻り値:
///   0（更新なし）
///   sizeof(MouseState)（更新あり）
///   負の値（エラー）
pub(crate) fn sys_mouse_read(arg1: u64, arg2: u64) -> Result<u64, SyscallError> {
    let buf_slice = user_slice_from_args(arg1, arg2)?;
    let buf = buf_slice.as_mut_slice();

    let state = match crate::mouse::read_state() {
        Some(s) => s,
        None => return Ok(0),
    };

    let size = core::mem::size_of::<crate::mouse::MouseState>();
    if buf.len() < size {
        return Err(SyscallError::InvalidArgument);
    }

    let src = unsafe {
        core::slice::from_raw_parts(
            (&state as *const crate::mouse::MouseState) as *const u8,
            size,
        )
    };
    buf[..size].copy_from_slice(src);
    Ok(size as u64)
}

/// SYS_DRAW_PIXEL: 1 ピクセルを描画する
///
/// 引数:
///   arg1 — x 座標
///   arg2 — y 座標
///   arg3 — RGB packed (0xRRGGBB)
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_draw_pixel(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let x = usize::try_from(arg1).map_err(|_| SyscallError::InvalidArgument)?;
    let y = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;

    let rgb = arg3 as u32;
    let r = ((rgb >> 16) & 0xFF) as u8;
    let g = ((rgb >> 8) & 0xFF) as u8;
    let b = (rgb & 0xFF) as u8;

    match crate::framebuffer::draw_pixel_global(x, y, r, g, b) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_DRAW_RECT: 矩形を描画する
///
/// 引数:
///   arg1 — x 座標
///   arg2 — y 座標
///   arg3 — width/height packed（上位 32bit = w, 下位 32bit = h）
///   arg4 — RGB packed (0xRRGGBB)
///
/// 戻り値:
///   0（成功時）
///   負の値（エラー時）
pub(crate) fn sys_draw_rect(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let x = usize::try_from(arg1).map_err(|_| SyscallError::InvalidArgument)?;
    let y = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;

    let w = (arg3 >> 32) as u32;
    let h = (arg3 & 0xFFFF_FFFF) as u32;
    let w = usize::try_from(w).map_err(|_| SyscallError::InvalidArgument)?;
    let h = usize::try_from(h).map_err(|_| SyscallError::InvalidArgument)?;

    let rgb = arg4 as u32;
    let r = ((rgb >> 16) & 0xFF) as u8;
    let g = ((rgb >> 8) & 0xFF) as u8;
    let b = (rgb & 0xFF) as u8;

    match crate::framebuffer::draw_rect_global(x, y, w, h, r, g, b) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_DRAW_LINE: 直線を描画する
///
/// 引数:
///   arg1 — x0/y0 packed（上位 32bit = x0, 下位 32bit = y0）
///   arg2 — x1/y1 packed（上位 32bit = x1, 下位 32bit = y1）
///   arg3 — RGB packed (0xRRGGBB)
pub(crate) fn sys_draw_line(arg1: u64, arg2: u64, arg3: u64) -> Result<u64, SyscallError> {
    let x0 = (arg1 >> 32) as u32;
    let y0 = (arg1 & 0xFFFF_FFFF) as u32;
    let x1 = (arg2 >> 32) as u32;
    let y1 = (arg2 & 0xFFFF_FFFF) as u32;

    let x0 = usize::try_from(x0).map_err(|_| SyscallError::InvalidArgument)?;
    let y0 = usize::try_from(y0).map_err(|_| SyscallError::InvalidArgument)?;
    let x1 = usize::try_from(x1).map_err(|_| SyscallError::InvalidArgument)?;
    let y1 = usize::try_from(y1).map_err(|_| SyscallError::InvalidArgument)?;

    let rgb = arg3 as u32;
    let r = ((rgb >> 16) & 0xFF) as u8;
    let g = ((rgb >> 8) & 0xFF) as u8;
    let b = (rgb & 0xFF) as u8;

    match crate::framebuffer::draw_line_global(x0, y0, x1, y1, r, g, b) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_DRAW_BLIT: 画像（RGBX）を描画する
///
/// 引数:
///   arg1 — x 座標
///   arg2 — y 座標
///   arg3 — width/height packed（上位 32bit = w, 下位 32bit = h）
///   arg4 — バッファポインタ（ユーザー空間）
pub(crate) fn sys_draw_blit(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let x = usize::try_from(arg1).map_err(|_| SyscallError::InvalidArgument)?;
    let y = usize::try_from(arg2).map_err(|_| SyscallError::InvalidArgument)?;

    let w = (arg3 >> 32) as u32;
    let h = (arg3 & 0xFFFF_FFFF) as u32;
    let w = usize::try_from(w).map_err(|_| SyscallError::InvalidArgument)?;
    let h = usize::try_from(h).map_err(|_| SyscallError::InvalidArgument)?;

    let pixel_count = w.checked_mul(h).ok_or(SyscallError::InvalidArgument)?;
    let byte_len = pixel_count.checked_mul(4).ok_or(SyscallError::InvalidArgument)?;
    let buf_slice = UserSlice::<u8>::from_raw(arg4, byte_len)?;
    let buf = buf_slice.as_slice();

    match crate::framebuffer::draw_blit_global(x, y, w, h, buf) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}

/// SYS_DRAW_TEXT: 文字列を描画する
///
/// 引数:
///   arg1 — x/y packed（上位 32bit = x, 下位 32bit = y）
///   arg2 — fg/bg packed（上位 32bit = fg, 下位 32bit = bg）
///   arg3 — 文字列ポインタ（ユーザー空間）
///   arg4 — 文字列長
pub(crate) fn sys_draw_text(arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> Result<u64, SyscallError> {
    let x = (arg1 >> 32) as u32;
    let y = (arg1 & 0xFFFF_FFFF) as u32;
    let x = usize::try_from(x).map_err(|_| SyscallError::InvalidArgument)?;
    let y = usize::try_from(y).map_err(|_| SyscallError::InvalidArgument)?;

    let fg = (arg2 >> 32) as u32;
    let bg = (arg2 & 0xFFFF_FFFF) as u32;
    let fg = (
        ((fg >> 16) & 0xFF) as u8,
        ((fg >> 8) & 0xFF) as u8,
        (fg & 0xFF) as u8,
    );
    let bg = (
        ((bg >> 16) & 0xFF) as u8,
        ((bg >> 8) & 0xFF) as u8,
        (bg & 0xFF) as u8,
    );

    let text_slice = user_slice_from_args(arg3, arg4)?;
    let text = text_slice.as_str().map_err(|_| SyscallError::InvalidUtf8)?;

    match crate::framebuffer::draw_text_global(x, y, fg, bg, text) {
        Ok(()) => Ok(0),
        Err(crate::framebuffer::DrawError::NotInitialized) => Err(SyscallError::Other),
        Err(_) => Err(SyscallError::InvalidArgument),
    }
}
