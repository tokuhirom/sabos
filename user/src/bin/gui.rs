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
use alloc::format;
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
const OPCODE_MOUSE: u32 = 8;
const OPCODE_WINDOW_CREATE: u32 = 16;
const OPCODE_WINDOW_CLOSE: u32 = 17;
const OPCODE_WINDOW_MOVE: u32 = 18;
const OPCODE_WINDOW_CLEAR: u32 = 19;
const OPCODE_WINDOW_RECT: u32 = 20;
const OPCODE_WINDOW_TEXT: u32 = 21;
const OPCODE_WINDOW_PRESENT: u32 = 22;
const OPCODE_WINDOW_MOUSE: u32 = 23;

const IPC_BUF_SIZE: usize = 2048;
const CURSOR_W: u32 = 8;
const CURSOR_H: u32 = 8;
const HUD_TICK_INTERVAL_DEFAULT: u32 = 30;
const HUD_X: u32 = 8;
const HUD_Y: u32 = 8;
const HUD_W: u32 = 360;
const HUD_H: u32 = 136;
const HUD_BG: (u8, u8, u8) = (16, 16, 40);
const HUD_BORDER: (u8, u8, u8) = (80, 120, 200);
const HUD_TITLE: (u8, u8, u8) = (255, 220, 120);
const HUD_TEXT: (u8, u8, u8) = (220, 240, 255);
const HUD_WARN: (u8, u8, u8) = (255, 120, 120);
const HUD_OK: (u8, u8, u8) = (120, 220, 120);
const HUD_BAR_BG: (u8, u8, u8) = (24, 24, 60);
const HUD_BAR_FILL: (u8, u8, u8) = (90, 180, 255);
const WINDOW_BG: (u8, u8, u8) = (24, 28, 44);
const WINDOW_BORDER: (u8, u8, u8) = (80, 120, 200);
const WINDOW_TITLE_BG: (u8, u8, u8) = (36, 44, 72);
const WINDOW_TITLE_TEXT: (u8, u8, u8) = (255, 220, 120);
const WINDOW_TITLE_H: u32 = 24;
const WINDOW_BORDER_W: u32 = 2;

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

struct Window {
    id: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    content_w: u32,
    content_h: u32,
    title: String,
    buf: Vec<u8>,
}

#[derive(Clone, Copy)]
struct DragState {
    id: u32,
    offset_x: i32,
    offset_y: i32,
}

struct WindowManager {
    windows: Vec<Window>,
    next_id: u32,
    active_id: Option<u32>,
    drag: Option<DragState>,
    last_mouse: syscall::MouseState,
    mouse_seq: u32,
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

    let mut wm = WindowManager::new();

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
    let mut hud_tick_interval: u32 = HUD_TICK_INTERVAL_DEFAULT;

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
                    if payload.len() == 1 || payload.len() == 5 {
                        hud_enabled = payload[0] != 0;
                        if payload.len() == 5 {
                            // 更新間隔（tick）は 0 を避けるため最低 1 に丸める
                            let interval = read_u32(payload, 1).unwrap_or(HUD_TICK_INTERVAL_DEFAULT);
                            hud_tick_interval = if interval == 0 { 1 } else { interval };
                        }
                        hud_tick = 0;
                        if hud_enabled {
                            let _ = wm.present_all(&mut state);
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
                OPCODE_MOUSE => {
                    if payload.is_empty() {
                        // マウス状態を返す（最後に更新された値）
                        let mut out = [0u8; 16];
                        out[0..4].copy_from_slice(&wm.last_mouse.x.to_le_bytes());
                        out[4..8].copy_from_slice(&wm.last_mouse.y.to_le_bytes());
                        out[8..12].copy_from_slice(&(wm.last_mouse.buttons as u32).to_le_bytes());
                        out[12..16].copy_from_slice(&wm.mouse_seq.to_le_bytes());

                        let mut resp = [0u8; IPC_BUF_SIZE];
                        resp[0..4].copy_from_slice(&opcode.to_le_bytes());
                        resp[4..8].copy_from_slice(&0i32.to_le_bytes());
                        resp[8..12].copy_from_slice(&(out.len() as u32).to_le_bytes());
                        resp[12..12 + out.len()].copy_from_slice(&out);
                        let _ = syscall::ipc_send(sender, &resp[..12 + out.len()]);
                        continue;
                    } else {
                        status = -10;
                    }
                }
                OPCODE_WINDOW_CREATE => {
                    if payload.len() >= 12 {
                        let w = read_u32(payload, 0).unwrap_or(0);
                        let h = read_u32(payload, 4).unwrap_or(0);
                        let len = read_u32(payload, 8).unwrap_or(0) as usize;
                        if 12 + len == payload.len() {
                            let title_bytes = &payload[12..12 + len];
                            if let Ok(title) = core::str::from_utf8(title_bytes) {
                                match wm.create_window(&state, w, h, title) {
                                    Ok(id) => {
                                        let mut resp = [0u8; IPC_BUF_SIZE];
                                        resp[0..4].copy_from_slice(&opcode.to_le_bytes());
                                        resp[4..8].copy_from_slice(&0i32.to_le_bytes());
                                        resp[8..12].copy_from_slice(&4u32.to_le_bytes());
                                        resp[12..16].copy_from_slice(&id.to_le_bytes());
                                        let _ = syscall::ipc_send(sender, &resp[..16]);
                                        continue;
                                    }
                                    Err(code) => status = code,
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
                OPCODE_WINDOW_CLOSE => {
                    if payload.len() == 4 {
                        let id = read_u32(payload, 0).unwrap_or(0);
                        status = if wm.close_window(id) { 0 } else { -10 };
                    } else {
                        status = -10;
                    }
                }
                OPCODE_WINDOW_MOVE => {
                    if payload.len() == 12 {
                        let id = read_u32(payload, 0).unwrap_or(0);
                        let x = read_i32(payload, 4).unwrap_or(0);
                        let y = read_i32(payload, 8).unwrap_or(0);
                        status = if wm.move_window(&state, id, x, y) { 0 } else { -10 };
                    } else {
                        status = -10;
                    }
                }
                OPCODE_WINDOW_CLEAR => {
                    if payload.len() == 7 {
                        let id = read_u32(payload, 0).unwrap_or(0);
                        let r = payload[4];
                        let g = payload[5];
                        let b = payload[6];
                        status = if wm.clear_window(id, r, g, b) { 0 } else { -10 };
                    } else {
                        status = -10;
                    }
                }
                OPCODE_WINDOW_RECT => {
                    if payload.len() == 23 {
                        let id = read_u32(payload, 0).unwrap_or(0);
                        let x = read_u32(payload, 4).unwrap_or(0);
                        let y = read_u32(payload, 8).unwrap_or(0);
                        let w = read_u32(payload, 12).unwrap_or(0);
                        let h = read_u32(payload, 16).unwrap_or(0);
                        let r = payload[20];
                        let g = payload[21];
                        let b = payload[22];
                        status = if wm.draw_rect(id, x, y, w, h, r, g, b) { 0 } else { -10 };
                    } else {
                        status = -10;
                    }
                }
                OPCODE_WINDOW_TEXT => {
                    if payload.len() >= 22 {
                        let id = read_u32(payload, 0).unwrap_or(0);
                        let x = read_u32(payload, 4).unwrap_or(0);
                        let y = read_u32(payload, 8).unwrap_or(0);
                        let fg = (payload[12], payload[13], payload[14]);
                        let bg = (payload[15], payload[16], payload[17]);
                        let len = read_u32(payload, 18).unwrap_or(0) as usize;
                        if 22 + len == payload.len() {
                            let text_bytes = &payload[22..22 + len];
                            let text = core::str::from_utf8(text_bytes).map_err(|_| ()).ok();
                            if let Some(text) = text {
                                status = if wm.draw_text(id, x, y, fg, bg, text) { 0 } else { -10 };
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
                OPCODE_WINDOW_PRESENT => {
                    if payload.len() == 4 {
                        let _id = read_u32(payload, 0).unwrap_or(0);
                        if wm.present_all(&mut state).is_err() {
                            status = -99;
                        }
                    } else {
                        status = -10;
                    }
                }
                OPCODE_WINDOW_MOUSE => {
                    if payload.len() == 4 {
                        let id = read_u32(payload, 0).unwrap_or(0);
                        let (x, y, buttons, seq) = wm.window_mouse_state(id);
                        let mut out = [0u8; 16];
                        out[0..4].copy_from_slice(&x.to_le_bytes());
                        out[4..8].copy_from_slice(&y.to_le_bytes());
                        out[8..12].copy_from_slice(&(buttons as u32).to_le_bytes());
                        out[12..16].copy_from_slice(&seq.to_le_bytes());

                        let mut resp = [0u8; IPC_BUF_SIZE];
                        resp[0..4].copy_from_slice(&opcode.to_le_bytes());
                        resp[4..8].copy_from_slice(&0i32.to_le_bytes());
                        resp[8..12].copy_from_slice(&(out.len() as u32).to_le_bytes());
                        resp[12..12 + out.len()].copy_from_slice(&out);
                        let _ = syscall::ipc_send(sender, &resp[..12 + out.len()]);
                        continue;
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
            wm.update_mouse(&mut state, &mut cursor, &mouse_state);
        }

        if hud_enabled {
            hud_tick = hud_tick.wrapping_add(1);
            if hud_tick >= hud_tick_interval {
                hud_tick = 0;
                let _ = wm.present_all(&mut state);
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

impl WindowManager {
    fn new() -> Self {
        Self {
            windows: Vec::new(),
            next_id: 1,
            active_id: None,
            drag: None,
            last_mouse: syscall::MouseState {
                x: 0,
                y: 0,
                dx: 0,
                dy: 0,
                buttons: 0,
                _pad: [0; 3],
            },
            mouse_seq: 0,
        }
    }

    fn create_window(&mut self, state: &GuiState, w: u32, h: u32, title: &str) -> Result<u32, i32> {
        if w == 0 || h == 0 {
            return Err(-10);
        }
        if w > state.width || h > state.height {
            return Err(-10);
        }
        let content_w = w.saturating_sub(WINDOW_BORDER_W * 2);
        let content_h = h.saturating_sub(WINDOW_BORDER_W * 2 + WINDOW_TITLE_H);
        if content_w == 0 || content_h == 0 {
            return Err(-10);
        }
        let buf_len = (content_w as usize)
            .saturating_mul(content_h as usize)
            .saturating_mul(4);
        let mut buf = Vec::with_capacity(buf_len);
        buf.resize(buf_len, 0);
        fill_buf(&mut buf, content_w, content_h, WINDOW_BG.0, WINDOW_BG.1, WINDOW_BG.2);

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let x = ((state.width - w) / 2) as i32;
        let y = ((state.height - h) / 2) as i32;
        self.windows.push(Window {
            id,
            x,
            y,
            w,
            h,
            content_w,
            content_h,
            title: title.into(),
            buf,
        });
        self.active_id = Some(id);
        Ok(id)
    }

    fn close_window(&mut self, id: u32) -> bool {
        let idx = match self.find_window_index(id) {
            Some(v) => v,
            None => return false,
        };
        self.windows.remove(idx);
        if self.active_id == Some(id) {
            self.active_id = self.windows.last().map(|w| w.id);
        }
        true
    }

    fn move_window(&mut self, state: &GuiState, id: u32, x: i32, y: i32) -> bool {
        let Some(win) = self.find_window_mut(id) else { return false; };
        let max_x = state.width.saturating_sub(win.w) as i32;
        let max_y = state.height.saturating_sub(win.h) as i32;
        win.x = x.clamp(0, max_x);
        win.y = y.clamp(0, max_y);
        true
    }

    fn clear_window(&mut self, id: u32, r: u8, g: u8, b: u8) -> bool {
        let Some(win) = self.find_window_mut(id) else { return false; };
        fill_buf(&mut win.buf, win.content_w, win.content_h, r, g, b);
        true
    }

    fn draw_rect(&mut self, id: u32, x: u32, y: u32, w: u32, h: u32, r: u8, g: u8, b: u8) -> bool {
        let Some(win) = self.find_window_mut(id) else { return false; };
        draw_rect_buf(&mut win.buf, win.content_w, win.content_h, x, y, w, h, r, g, b)
    }

    fn draw_text(
        &mut self,
        id: u32,
        x: u32,
        y: u32,
        fg: (u8, u8, u8),
        bg: (u8, u8, u8),
        text: &str,
    ) -> bool {
        let Some(win) = self.find_window_mut(id) else { return false; };
        draw_text_buf(&mut win.buf, win.content_w, win.content_h, x, y, fg, bg, text)
    }

    fn present_all(&mut self, state: &mut GuiState) -> Result<(), ()> {
        clear_screen(state, 8, 8, 16);
        for win in &self.windows {
            draw_window_frame(state, win);
            blit_window_content(state, win);
        }
        present(state)?;
        Ok(())
    }

    fn update_mouse(
        &mut self,
        state: &mut GuiState,
        cursor: &mut CursorState,
        mouse: &syscall::MouseState,
    ) {
        let prev_buttons = self.last_mouse.buttons;
        self.last_mouse = *mouse;
        self.mouse_seq = self.mouse_seq.wrapping_add(1);

        let left_now = (mouse.buttons & 0x01) != 0;
        let left_prev = (prev_buttons & 0x01) != 0;

        if left_now && !left_prev {
            if let Some(id) = self.find_window_at(mouse.x, mouse.y) {
                self.bring_to_top(id);
                if self.hit_title_bar(id, mouse.x, mouse.y) {
                    if let Some(win) = self.find_window(id) {
                        self.drag = Some(DragState {
                            id,
                            offset_x: mouse.x - win.x,
                            offset_y: mouse.y - win.y,
                        });
                    }
                }
            }
        }

        if !left_now && left_prev {
            self.drag = None;
        }

        let mut moved = false;
        if let Some(drag) = self.drag {
            if let Some(win) = self.find_window_mut(drag.id) {
                let max_x = state.width.saturating_sub(win.w) as i32;
                let max_y = state.height.saturating_sub(win.h) as i32;
                let new_x = (mouse.x - drag.offset_x).clamp(0, max_x);
                let new_y = (mouse.y - drag.offset_y).clamp(0, max_y);
                if new_x != win.x || new_y != win.y {
                    win.x = new_x;
                    win.y = new_y;
                    moved = true;
                }
            }
        }

        if moved {
            let _ = self.present_all(state);
            cursor.visible = false;
        }

        let _ = update_cursor(state, cursor, mouse);
    }

    fn window_mouse_state(&self, id: u32) -> (i32, i32, u8, u32) {
        let Some(win) = self.find_window(id) else {
            return (-1, -1, self.last_mouse.buttons, self.mouse_seq);
        };
        let (cx, cy) = window_content_origin(win);
        let mx = self.last_mouse.x;
        let my = self.last_mouse.y;
        if mx >= cx && my >= cy && mx < cx + win.content_w as i32 && my < cy + win.content_h as i32 {
            (mx - cx, my - cy, self.last_mouse.buttons, self.mouse_seq)
        } else {
            (-1, -1, self.last_mouse.buttons, self.mouse_seq)
        }
    }

    fn find_window_index(&self, id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    fn find_window(&self, id: u32) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    fn find_window_mut(&mut self, id: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    fn find_window_at(&self, x: i32, y: i32) -> Option<u32> {
        for win in self.windows.iter().rev() {
            if x >= win.x && y >= win.y && x < win.x + win.w as i32 && y < win.y + win.h as i32 {
                return Some(win.id);
            }
        }
        None
    }

    fn bring_to_top(&mut self, id: u32) {
        if self.active_id == Some(id) {
            return;
        }
        if let Some(idx) = self.find_window_index(id) {
            let win = self.windows.remove(idx);
            self.windows.push(win);
            self.active_id = Some(id);
        }
    }

    fn hit_title_bar(&self, id: u32, x: i32, y: i32) -> bool {
        let Some(win) = self.find_window(id) else { return false; };
        let bx = win.x + WINDOW_BORDER_W as i32;
        let by = win.y + WINDOW_BORDER_W as i32;
        let bw = (win.w - WINDOW_BORDER_W * 2) as i32;
        let bh = WINDOW_TITLE_H as i32;
        x >= bx && x < bx + bw && y >= by && y < by + bh
    }
}

fn window_content_origin(win: &Window) -> (i32, i32) {
    let x = win.x + WINDOW_BORDER_W as i32;
    let y = win.y + WINDOW_BORDER_W as i32 + WINDOW_TITLE_H as i32;
    (x, y)
}

fn clear_screen(state: &mut GuiState, r: u8, g: u8, b: u8) {
    fill_buf(&mut state.buf, state.width, state.height, r, g, b);
}

fn fill_buf(buf: &mut [u8], w: u32, h: u32, r: u8, g: u8, b: u8) {
    let mut i = 0;
    let total = (w as usize).saturating_mul(h as usize).saturating_mul(4);
    while i + 3 < total {
        buf[i] = r;
        buf[i + 1] = g;
        buf[i + 2] = b;
        buf[i + 3] = 0;
        i += 4;
    }
}

fn draw_window_frame(state: &mut GuiState, win: &Window) {
    let x = win.x.max(0) as u32;
    let y = win.y.max(0) as u32;
    let w = win.w;
    let h = win.h;
    let _ = draw_rect(state, x, y, w, h, WINDOW_BORDER.0, WINDOW_BORDER.1, WINDOW_BORDER.2);
    let _ = draw_rect(
        state,
        x + WINDOW_BORDER_W,
        y + WINDOW_BORDER_W,
        w - WINDOW_BORDER_W * 2,
        h - WINDOW_BORDER_W * 2,
        WINDOW_BG.0,
        WINDOW_BG.1,
        WINDOW_BG.2,
    );
    let _ = draw_rect(
        state,
        x + WINDOW_BORDER_W,
        y + WINDOW_BORDER_W,
        w - WINDOW_BORDER_W * 2,
        WINDOW_TITLE_H,
        WINDOW_TITLE_BG.0,
        WINDOW_TITLE_BG.1,
        WINDOW_TITLE_BG.2,
    );
    let _ = draw_text(
        state,
        x + WINDOW_BORDER_W + 6,
        y + WINDOW_BORDER_W + 4,
        WINDOW_TITLE_TEXT,
        WINDOW_TITLE_BG,
        win.title.as_str(),
    );
}

fn blit_window_content(state: &mut GuiState, win: &Window) {
    let (cx, cy) = window_content_origin(win);
    let cx = cx.max(0) as u32;
    let cy = cy.max(0) as u32;
    let w = win.content_w;
    let h = win.content_h;
    for row in 0..h {
        let src_offset = (row * w * 4) as usize;
        let dst_y = cy + row;
        if dst_y >= state.height {
            break;
        }
        let dst_offset = ((dst_y * state.width + cx) * 4) as usize;
        let len = (w * 4) as usize;
        if dst_offset + len <= state.buf.len() && src_offset + len <= win.buf.len() {
            state.buf[dst_offset..dst_offset + len]
                .copy_from_slice(&win.buf[src_offset..src_offset + len]);
        }
    }
}

fn draw_rect_buf(
    buf: &mut [u8],
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    rw: u32,
    rh: u32,
    r: u8,
    g: u8,
    b: u8,
) -> bool {
    if x >= w || y >= h {
        return false;
    }
    let max_x = (x + rw).min(w);
    let max_y = (y + rh).min(h);
    for yy in y..max_y {
        let row_offset = (yy * w * 4) as usize;
        for xx in x..max_x {
            let idx = row_offset + (xx * 4) as usize;
            if idx + 3 < buf.len() {
                buf[idx] = r;
                buf[idx + 1] = g;
                buf[idx + 2] = b;
                buf[idx + 3] = 0;
            }
        }
    }
    true
}

fn draw_text_buf(
    buf: &mut [u8],
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    text: &str,
) -> bool {
    if x >= w || y >= h {
        return false;
    }
    let mut cursor_x = x;
    let mut cursor_y = y;
    for ch in text.chars() {
        if ch == '\n' {
            cursor_x = x;
            cursor_y = cursor_y.saturating_add(9);
            if cursor_y + 8 > h {
                return false;
            }
            continue;
        }
        if cursor_x + 8 > w {
            cursor_x = x;
            cursor_y = cursor_y.saturating_add(9);
            if cursor_y + 8 > h {
                return false;
            }
        }
        draw_char_buf(buf, w, h, cursor_x, cursor_y, fg, bg, ch);
        cursor_x = cursor_x.saturating_add(9);
    }
    true
}

fn draw_char_buf(
    buf: &mut [u8],
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    ch: char,
) {
    let glyph = font8x8::BASIC_FONTS
        .get(ch)
        .unwrap_or_else(|| font8x8::BASIC_FONTS.get('?').unwrap());

    for (row, &bits) in glyph.iter().enumerate() {
        for col in 0..8 {
            let on = (bits >> col) & 1 == 1;
            let px = x + col as u32;
            let py = y + row as u32;
            if px >= w || py >= h {
                continue;
            }
            let idx = ((py * w + px) * 4) as usize;
            if idx + 3 >= buf.len() {
                continue;
            }
            let (r, g, b) = if on { fg } else { bg };
            buf[idx] = r;
            buf[idx + 1] = g;
            buf[idx + 2] = b;
            buf[idx + 3] = 0;
        }
    }
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

    draw_rect(state, HUD_X, HUD_Y, HUD_W, HUD_H, HUD_BG.0, HUD_BG.1, HUD_BG.2)?;
    // 枠線
    let _ = draw_line(state, HUD_X, HUD_Y, HUD_X + HUD_W - 1, HUD_Y, HUD_BORDER.0, HUD_BORDER.1, HUD_BORDER.2);
    let _ = draw_line(state, HUD_X, HUD_Y + HUD_H - 1, HUD_X + HUD_W - 1, HUD_Y + HUD_H - 1, HUD_BORDER.0, HUD_BORDER.1, HUD_BORDER.2);
    let _ = draw_line(state, HUD_X, HUD_Y, HUD_X, HUD_Y + HUD_H - 1, HUD_BORDER.0, HUD_BORDER.1, HUD_BORDER.2);
    let _ = draw_line(state, HUD_X + HUD_W - 1, HUD_Y, HUD_X + HUD_W - 1, HUD_Y + HUD_H - 1, HUD_BORDER.0, HUD_BORDER.1, HUD_BORDER.2);

    // タイトル帯
    let _ = draw_rect(state, HUD_X + 1, HUD_Y + 1, HUD_W - 2, 18, 24, 24, 60);
    let _ = draw_text(state, HUD_X + 8, HUD_Y + 4, HUD_TITLE, HUD_BG, "MEMINFO HUD");

    let mut text = String::new();
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

    draw_text(state, HUD_X + 8, HUD_Y + 28, HUD_TEXT, HUD_BG, text.as_str())?;

    // 簡易バー（使用率）
    if let (Some(t), Some(a)) = (total, allocated) {
        let ratio = if t == 0 { 0 } else { a * 100 / t };
        let bar_x = HUD_X + 8;
        let bar_y = HUD_Y + HUD_H - 20;
        let bar_w = HUD_W - 16;
        let bar_h = 8;
        let _ = draw_rect(state, bar_x, bar_y, bar_w, bar_h, HUD_BAR_BG.0, HUD_BAR_BG.1, HUD_BAR_BG.2);
        let fill_w = (bar_w as u64 * ratio / 100) as u32;
        let _ = draw_rect(state, bar_x, bar_y, fill_w, bar_h, HUD_BAR_FILL.0, HUD_BAR_FILL.1, HUD_BAR_FILL.2);
        let color = if ratio >= 80 { HUD_WARN } else { HUD_OK };
        let _ = draw_text(
            state,
            bar_x + 4,
            bar_y - 12,
            color,
            HUD_BG,
            &format!("alloc {}%", ratio),
        );
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

fn read_i32(buf: &[u8], offset: usize) -> Option<i32> {
    read_u32(buf, offset).map(|v| v as i32)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
