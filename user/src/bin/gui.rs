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
#[path = "../json.rs"]
mod json;
#[path = "../syscall.rs"]
mod syscall;

use alloc::vec::Vec;
use alloc::string::String;
use core::fmt::Write;
use core::panic::PanicInfo;
use font8x8::UnicodeFonts;

const OPCODE_CLEAR: u32 = 1;
const OPCODE_RECT: u32 = 2;
const OPCODE_LINE: u32 = 3;
const OPCODE_PRESENT: u32 = 4;
const OPCODE_CIRCLE: u32 = 5;
const OPCODE_TEXT: u32 = 6;
const OPCODE_HUD: u32 = 7;

const IPC_BUF_SIZE: usize = 2048;
const CURSOR_W: u32 = 8;
const CURSOR_H: u32 = 8;
const HUD_TICK_INTERVAL: u32 = 30;
const HUD_X: u32 = 8;
const HUD_Y: u32 = 8;
const HUD_W: u32 = 320;
const HUD_H: u32 = 120;

struct GuiState {
    width: u32,
    height: u32,
    buf: Vec<u8>,
}

struct CursorState {
    x: i32,
    y: i32,
    visible: bool,
    buttons: u8,
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
    let mut cursor = CursorState {
        x: 0,
        y: 0,
        visible: false,
        buttons: 0,
    };
    let mut hud_enabled = false;
    let mut hud_tick: u32 = 0;

    loop {
        let n = syscall::ipc_recv(&mut sender, &mut buf, 16);
        if n >= 0 {
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
                    } else if cursor.visible {
                        let _ = draw_cursor(&state, cursor.x, cursor.y, cursor.buttons);
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
                OPCODE_HUD => {
                    if payload.len() == 1 {
                        hud_enabled = payload[0] != 0;
                        hud_tick = 0;
                        if hud_enabled {
                            let _ = draw_hud(&mut state);
                            if present(&state).is_err() {
                                status = -99;
                            } else if cursor.visible {
                                let _ = draw_cursor(&state, cursor.x, cursor.y, cursor.buttons);
                            }
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

        let mut mouse_state = syscall::MouseState {
            x: 0,
            y: 0,
            dx: 0,
            dy: 0,
            buttons: 0,
            _pad: [0; 3],
        };
        if syscall::mouse_read(&mut mouse_state) > 0 {
            let _ = update_cursor(&mut state, &mut cursor, &mouse_state);
        }

        if hud_enabled {
            hud_tick = hud_tick.wrapping_add(1);
            if hud_tick >= HUD_TICK_INTERVAL {
                hud_tick = 0;
                let _ = draw_hud(&mut state);
                if present(&state).is_ok() && cursor.visible {
                    let _ = draw_cursor(&state, cursor.x, cursor.y, cursor.buttons);
                }
            }
        }
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

fn update_cursor(state: &GuiState, cursor: &mut CursorState, mouse: &syscall::MouseState) -> Result<(), ()> {
    if cursor.visible {
        restore_cursor(state, cursor.x, cursor.y)?;
    }

    let x = mouse.x;
    let y = mouse.y;
    draw_cursor(state, x, y, mouse.buttons)?;
    cursor.x = x;
    cursor.y = y;
    cursor.visible = true;
    cursor.buttons = mouse.buttons;
    Ok(())
}

fn restore_cursor(state: &GuiState, x: i32, y: i32) -> Result<(), ()> {
    if x < 0 || y < 0 {
        return Ok(());
    }
    let x0 = x as u32;
    let y0 = y as u32;
    if x0 >= state.width || y0 >= state.height {
        return Ok(());
    }
    let w = core::cmp::min(CURSOR_W, state.width - x0);
    let h = core::cmp::min(CURSOR_H, state.height - y0);
    if w == 0 || h == 0 {
        return Ok(());
    }

    let mut tmp = Vec::with_capacity((w * h * 4) as usize);
    tmp.resize((w * h * 4) as usize, 0);
    for row in 0..h {
        let src_y = y0 + row;
        let src_offset = ((src_y * state.width + x0) * 4) as usize;
        let dst_offset = (row * w * 4) as usize;
        let len = (w * 4) as usize;
        tmp[dst_offset..dst_offset + len]
            .copy_from_slice(&state.buf[src_offset..src_offset + len]);
    }

    if syscall::draw_blit(x0, y0, w, h, &tmp) < 0 {
        return Err(());
    }
    Ok(())
}

fn draw_cursor(state: &GuiState, x: i32, y: i32, buttons: u8) -> Result<(), ()> {
    if x < 0 || y < 0 {
        return Ok(());
    }
    let x0 = x as u32;
    let y0 = y as u32;
    if x0 >= state.width || y0 >= state.height {
        return Ok(());
    }

    let mut r = 255;
    let mut g = 255;
    let mut b = 255;
    if buttons & 0x01 != 0 {
        r = 255;
        g = 64;
        b = 64;
    } else if buttons & 0x02 != 0 {
        r = 64;
        g = 255;
        b = 64;
    } else if buttons & 0x04 != 0 {
        r = 64;
        g = 160;
        b = 255;
    }

    for dy in 0..CURSOR_H {
        for dx in 0..CURSOR_W {
            let px = x0 + dx;
            let py = y0 + dy;
            if px >= state.width || py >= state.height {
                continue;
            }
            let on = if buttons == 0 {
                dx == 0 || dy == 0 || dx == dy
            } else {
                true
            };
            if on {
                if syscall::draw_pixel(px, py, r, g, b) < 0 {
                    return Err(());
                }
            }
        }
    }
    Ok(())
}

fn draw_hud(state: &mut GuiState) -> Result<(), ()> {
    let mut buf = [0u8; 1024];
    let result = syscall::get_mem_info(&mut buf);
    if result < 0 {
        return Err(());
    }

    let len = result as usize;
    let Ok(s) = core::str::from_utf8(&buf[..len]) else {
        return Err(());
    };

    let total = json::json_find_u64(s, "total_frames");
    let allocated = json::json_find_u64(s, "allocated_frames");
    let free = json::json_find_u64(s, "free_frames");
    let free_kib = json::json_find_u64(s, "free_kib");
    let heap_start = json::json_find_u64(s, "heap_start");
    let heap_size = json::json_find_u64(s, "heap_size");
    let heap_source = json::json_find_str(s, "heap_source").unwrap_or("-");

    if HUD_X + HUD_W > state.width || HUD_Y + HUD_H > state.height {
        return Err(());
    }

    draw_rect(state, HUD_X, HUD_Y, HUD_W, HUD_H, 16, 16, 40)?;

    let mut text = String::new();
    let _ = writeln!(text, "MEMINFO HUD");
    if let Some(v) = total {
        let _ = writeln!(text, "total_frames: {}", v);
    }
    if let Some(v) = allocated {
        let _ = writeln!(text, "allocated_frames: {}", v);
    }
    if let Some(v) = free {
        let _ = writeln!(text, "free_frames: {}", v);
    }
    if let Some(v) = free_kib {
        let _ = writeln!(text, "free_kib: {}", v);
    }
    if let Some(v) = heap_start {
        let _ = writeln!(text, "heap_start: {}", v);
    }
    if let Some(v) = heap_size {
        let _ = writeln!(text, "heap_size: {}", v);
    }
    let _ = writeln!(text, "heap_source: {}", heap_source);

    draw_text(state, HUD_X + 8, HUD_Y + 8, (255, 255, 255), (16, 16, 40), text.as_str())?;
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
