// gui.rs — GUI サービス（user space）
//
// IPC で描画要求を受け取り、バックバッファに描画してから
// draw_blit でフレームバッファへ転送する。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator_gui.rs"]
mod allocator;
#[path = "../syscall.rs"]
mod syscall;

use alloc::vec::Vec;
use core::panic::PanicInfo;
use font8x8::UnicodeFonts;

const OPCODE_CLEAR: u32 = 1;
const OPCODE_RECT: u32 = 2;
const OPCODE_LINE: u32 = 3;
const OPCODE_PRESENT: u32 = 4;
const OPCODE_CIRCLE: u32 = 5;
const OPCODE_TEXT: u32 = 6;

const IPC_BUF_SIZE: usize = 2048;

struct GuiState {
    width: u32,
    height: u32,
    buf: Vec<u8>,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    gui_loop();
}

fn gui_loop() -> ! {
    let mut state = match init_state() {
        Ok(s) => s,
        Err(_) => loop {
            syscall::sleep(1000);
        },
    };

    let mut buf = [0u8; IPC_BUF_SIZE];
    let mut sender: u64 = 0;

    loop {
        let n = syscall::ipc_recv(&mut sender, &mut buf, 0);
        if n < 0 {
            continue;
        }
        let n = n as usize;
        if n < 8 {
            continue;
        }

        let opcode = read_u32(&buf, 0).unwrap_or(0);
        let len = read_u32(&buf, 4).unwrap_or(0) as usize;
        if 8 + len > n {
            continue;
        }
        let payload = &buf[8..8 + len];

        let mut status: i32 = 0;
        match opcode {
            OPCODE_CLEAR => {
                if payload.len() == 3 {
                    clear(&mut state, payload[0], payload[1], payload[2]);
                } else {
                    status = -10;
                }
            }
            OPCODE_RECT => {
                if payload.len() == 19 {
                    let x = read_u32(payload, 0).unwrap_or(0);
                    let y = read_u32(payload, 4).unwrap_or(0);
                    let w = read_u32(payload, 8).unwrap_or(0);
                    let h = read_u32(payload, 12).unwrap_or(0);
                    let r = payload[16];
                    let g = payload[17];
                    let b = payload[18];
                    if draw_rect(&mut state, x, y, w, h, r, g, b).is_err() {
                        status = -10;
                    }
                } else {
                    status = -10;
                }
            }
            OPCODE_LINE => {
                if payload.len() == 19 {
                    let x0 = read_u32(payload, 0).unwrap_or(0);
                    let y0 = read_u32(payload, 4).unwrap_or(0);
                    let x1 = read_u32(payload, 8).unwrap_or(0);
                    let y1 = read_u32(payload, 12).unwrap_or(0);
                    let r = payload[16];
                    let g = payload[17];
                    let b = payload[18];
                    if draw_line(&mut state, x0, y0, x1, y1, r, g, b).is_err() {
                        status = -10;
                    }
                } else {
                    status = -10;
                }
            }
            OPCODE_PRESENT => {
                if present(&state).is_err() {
                    status = -99;
                }
            }
            OPCODE_CIRCLE => {
                if payload.len() == 17 {
                    let cx = read_u32(payload, 0).unwrap_or(0);
                    let cy = read_u32(payload, 4).unwrap_or(0);
                    let r = read_u32(payload, 8).unwrap_or(0);
                    let red = payload[12];
                    let green = payload[13];
                    let blue = payload[14];
                    let filled = payload[15] != 0;
                    let _ = payload[16]; // 予約（将来のアルファなど）
                    if draw_circle(&mut state, cx, cy, r, red, green, blue, filled).is_err() {
                        status = -10;
                    }
                } else {
                    status = -10;
                }
            }
            OPCODE_TEXT => {
                if payload.len() >= 18 {
                    let x = read_u32(payload, 0).unwrap_or(0);
                    let y = read_u32(payload, 4).unwrap_or(0);
                    let fg = (payload[8], payload[9], payload[10]);
                    let bg = (payload[11], payload[12], payload[13]);
                    let len = read_u32(payload, 14).unwrap_or(0) as usize;
                    if 18 + len == payload.len() {
                        let text_bytes = &payload[18..18 + len];
                        let text = core::str::from_utf8(text_bytes).map_err(|_| ()).ok();
                        if let Some(text) = text {
                            if draw_text(&mut state, x, y, fg, bg, text).is_err() {
                                status = -10;
                            }
                        } else {
                            status = -10;
                        }
                    } else {
                        status = -10;
                    }
                } else {
                    status = -10;
                }
            }
            _ => {
                status = -10;
            }
        }

        let mut resp = [0u8; IPC_BUF_SIZE];
        resp[0..4].copy_from_slice(&opcode.to_le_bytes());
        resp[4..8].copy_from_slice(&status.to_le_bytes());
        resp[8..12].copy_from_slice(&0u32.to_le_bytes());
        let _ = syscall::ipc_send(sender, &resp[..12]);
    }
}

fn init_state() -> Result<GuiState, &'static str> {
    let mut info = syscall::FramebufferInfo {
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: 0,
        bytes_per_pixel: 0,
    };
    if syscall::get_fb_info(&mut info) < 0 {
        return Err("get_fb_info failed");
    }

    let width = info.width;
    let height = info.height;
    let pixel_count = width as usize * height as usize;
    let mut buf = Vec::with_capacity(pixel_count * 4);
    buf.resize(pixel_count * 4, 0);

    Ok(GuiState { width, height, buf })
}

fn clear(state: &mut GuiState, r: u8, g: u8, b: u8) {
    let mut i = 0;
    while i + 3 < state.buf.len() {
        state.buf[i] = r;
        state.buf[i + 1] = g;
        state.buf[i + 2] = b;
        state.buf[i + 3] = 0;
        i += 4;
    }
}

fn draw_rect(state: &mut GuiState, x: u32, y: u32, w: u32, h: u32, r: u8, g: u8, b: u8) -> Result<(), ()> {
    if w == 0 || h == 0 {
        return Err(());
    }
    if x >= state.width || y >= state.height {
        return Err(());
    }
    let end_x = x.checked_add(w).ok_or(())?;
    let end_y = y.checked_add(h).ok_or(())?;
    if end_x > state.width || end_y > state.height {
        return Err(());
    }

    for yy in y..end_y {
        for xx in x..end_x {
            set_pixel(state, xx, yy, r, g, b)?;
        }
    }
    Ok(())
}

fn draw_line(state: &mut GuiState, x0: u32, y0: u32, x1: u32, y1: u32, r: u8, g: u8, b: u8) -> Result<(), ()> {
    if x0 >= state.width || y0 >= state.height || x1 >= state.width || y1 >= state.height {
        return Err(());
    }

    let mut x0 = x0 as i32;
    let mut y0 = y0 as i32;
    let x1 = x1 as i32;
    let y1 = y1 as i32;
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;

    loop {
        if x0 >= 0 && y0 >= 0 {
            set_pixel(state, x0 as u32, y0 as u32, r, g, b)?;
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = err * 2;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }

    Ok(())
}

fn set_pixel(state: &mut GuiState, x: u32, y: u32, r: u8, g: u8, b: u8) -> Result<(), ()> {
    if x >= state.width || y >= state.height {
        return Err(());
    }
    let idx = (y as usize * state.width as usize + x as usize) * 4;
    if idx + 3 >= state.buf.len() {
        return Err(());
    }
    state.buf[idx] = r;
    state.buf[idx + 1] = g;
    state.buf[idx + 2] = b;
    state.buf[idx + 3] = 0;
    Ok(())
}

fn draw_circle(
    state: &mut GuiState,
    cx: u32,
    cy: u32,
    r: u32,
    red: u8,
    green: u8,
    blue: u8,
    filled: bool,
) -> Result<(), ()> {
    if r == 0 {
        return Err(());
    }
    if cx >= state.width || cy >= state.height {
        return Err(());
    }
    if cx + r >= state.width || cy + r >= state.height {
        return Err(());
    }
    if r > cx || r > cy {
        return Err(());
    }

    let mut x = r as i32;
    let mut y = 0i32;
    let mut err = 1 - x;
    let cx = cx as i32;
    let cy = cy as i32;

    while x >= y {
        if filled {
            // 水平スパンで塗りつぶし
            draw_hline(state, cx - x, cx + x, cy + y, red, green, blue)?;
            draw_hline(state, cx - x, cx + x, cy - y, red, green, blue)?;
            draw_hline(state, cx - y, cx + y, cy + x, red, green, blue)?;
            draw_hline(state, cx - y, cx + y, cy - x, red, green, blue)?;
        } else {
            // 輪郭のみ
            set_pixel(state, (cx + x) as u32, (cy + y) as u32, red, green, blue)?;
            set_pixel(state, (cx - x) as u32, (cy + y) as u32, red, green, blue)?;
            set_pixel(state, (cx + x) as u32, (cy - y) as u32, red, green, blue)?;
            set_pixel(state, (cx - x) as u32, (cy - y) as u32, red, green, blue)?;
            set_pixel(state, (cx + y) as u32, (cy + x) as u32, red, green, blue)?;
            set_pixel(state, (cx - y) as u32, (cy + x) as u32, red, green, blue)?;
            set_pixel(state, (cx + y) as u32, (cy - x) as u32, red, green, blue)?;
            set_pixel(state, (cx - y) as u32, (cy - x) as u32, red, green, blue)?;
        }

        y += 1;
        if err < 0 {
            err += 2 * y + 1;
        } else {
            x -= 1;
            err += 2 * (y - x) + 1;
        }
    }

    Ok(())
}

fn draw_hline(
    state: &mut GuiState,
    x0: i32,
    x1: i32,
    y: i32,
    r: u8,
    g: u8,
    b: u8,
) -> Result<(), ()> {
    if y < 0 || y >= state.height as i32 {
        return Err(());
    }
    let mut x0 = x0;
    let mut x1 = x1;
    if x0 > x1 {
        core::mem::swap(&mut x0, &mut x1);
    }
    if x0 < 0 || x1 >= state.width as i32 {
        return Err(());
    }
    let y = y as u32;
    for x in x0..=x1 {
        set_pixel(state, x as u32, y, r, g, b)?;
    }
    Ok(())
}

fn draw_text(
    state: &mut GuiState,
    x: u32,
    y: u32,
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    text: &str,
) -> Result<(), ()> {
    if x >= state.width || y >= state.height {
        return Err(());
    }

    let char_w: u32 = 8;
    let char_h: u32 = 8;
    let spacing: u32 = 1;
    let line_advance = char_h + spacing;

    let mut cursor_x = x;
    let mut cursor_y = y;

    for ch in text.chars() {
        if ch == '\n' {
            cursor_x = x;
            cursor_y = cursor_y.checked_add(line_advance).ok_or(())?;
            if cursor_y + char_h > state.height {
                return Err(());
            }
            continue;
        }

        if cursor_x + char_w > state.width {
            cursor_x = x;
            cursor_y = cursor_y.checked_add(line_advance).ok_or(())?;
            if cursor_y + char_h > state.height {
                return Err(());
            }
        }

        draw_char(state, cursor_x, cursor_y, fg, bg, ch)?;
        cursor_x = cursor_x.checked_add(char_w + spacing).ok_or(())?;
    }

    Ok(())
}

fn draw_char(
    state: &mut GuiState,
    x: u32,
    y: u32,
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    ch: char,
) -> Result<(), ()> {
    let glyph = font8x8::BASIC_FONTS
        .get(ch)
        .unwrap_or_else(|| font8x8::BASIC_FONTS.get('?').unwrap());

    for (row, &bits) in glyph.iter().enumerate() {
        for col in 0..8 {
            let on = (bits >> col) & 1 == 1;
            let px = x + col as u32;
            let py = y + row as u32;
            if on {
                set_pixel(state, px, py, fg.0, fg.1, fg.2)?;
            } else {
                set_pixel(state, px, py, bg.0, bg.1, bg.2)?;
            }
        }
    }
    Ok(())
}

fn present(state: &GuiState) -> Result<(), ()> {
    if syscall::draw_blit(0, 0, state.width, state.height, &state.buf) < 0 {
        return Err(());
    }
    Ok(())
}

fn read_u32(buf: &[u8], offset: usize) -> Option<u32> {
    if offset + 4 > buf.len() {
        return None;
    }
    Some(u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ]))
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
