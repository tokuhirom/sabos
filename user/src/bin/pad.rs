// pad.rs — GUI サンプルアプリ（user space）
//
// ウィンドウ API の汎用性確認用。
// クリックでカウンタを増やし、色を切り替える簡易パネル。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../gui_client.rs"]
mod gui_client;
#[path = "../json.rs"]
mod json;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use alloc::format;
use core::panic::PanicInfo;

const PANEL_BG: (u8, u8, u8) = (24, 28, 44);
const PANEL_BORDER: (u8, u8, u8) = (80, 120, 200);
const TITLE_TEXT: (u8, u8, u8) = (255, 220, 120);
const TEXT_FG: (u8, u8, u8) = (220, 240, 255);
const BUTTON_BG: (u8, u8, u8) = (40, 48, 72);
const BUTTON_BORDER: (u8, u8, u8) = (90, 110, 170);

const PAD: u32 = 12;
const GAP: u32 = 8;
const BTN_W: u32 = 120;
const BTN_H: u32 = 44;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    app_main();
}

fn app_main() -> ! {
    let mut gui = gui_client::GuiClient::new();
    let (content_w, content_h) = layout_size();
    let win_w = content_w + 4;
    let win_h = content_h + 4 + 24;
    let win_id = match gui.window_create(win_w, win_h, "SABOS PANEL") {
        Ok(id) => id,
        Err(_) => syscall::exit(),
    };

    let mut count: u64 = 0;
    let mut color_idx: usize = 0;
    let mut last_seq: u32 = 0;
    let mut last_buttons: u8 = 0;

    draw_ui(&mut gui, win_id, count, color_idx);

    loop {
        if let Ok(mouse) = gui.window_mouse_state(win_id) {
            if mouse.seq != last_seq {
                let left_now = (mouse.buttons & 0x1) != 0;
                let left_prev = (last_buttons & 0x1) != 0;
                last_seq = mouse.seq;
                last_buttons = mouse.buttons;

                if mouse.inside && left_now && !left_prev {
                    if hit_button_inc(mouse.x, mouse.y) {
                        count = count.saturating_add(1);
                        draw_ui(&mut gui, win_id, count, color_idx);
                    } else if hit_button_color(mouse.x, mouse.y) {
                        color_idx = (color_idx + 1) % COLORS.len();
                        draw_ui(&mut gui, win_id, count, color_idx);
                    }
                }
            }
        }
        syscall::sleep(16);
    }
}

const COLORS: [(u8, u8, u8); 4] = [
    (24, 28, 44),
    (30, 40, 64),
    (20, 36, 36),
    (44, 28, 28),
];

fn layout_size() -> (u32, u32) {
    let w = PAD * 2 + BTN_W * 2 + GAP;
    let h = PAD * 2 + BTN_H + GAP + 60;
    (w, h)
}

fn draw_ui(gui: &mut gui_client::GuiClient, win_id: gui_client::WindowId, count: u64, color_idx: usize) {
    let bg = COLORS[color_idx];
    let _ = gui.window_clear(win_id, bg.0, bg.1, bg.2);

    let (w, _h) = layout_size();
    let _ = gui.window_rect(win_id, 0, 0, w, 160, PANEL_BORDER.0, PANEL_BORDER.1, PANEL_BORDER.2);
    let _ = gui.window_rect(win_id, 2, 2, w - 4, 156, PANEL_BG.0, PANEL_BG.1, PANEL_BG.2);

    let _ = gui.window_text(win_id, PAD, PAD - 4, TITLE_TEXT, PANEL_BG, "SABOS GUI PANEL");

    let text = format!("count = {}", count);
    let _ = gui.window_text(win_id, PAD, PAD + 18, TEXT_FG, PANEL_BG, text.as_str());

    // ボタン: INC
    let inc_x = PAD;
    let inc_y = PAD + 44;
    draw_button(gui, win_id, inc_x, inc_y, BTN_W, BTN_H, "INC");

    // ボタン: COLOR
    let col_x = PAD + BTN_W + GAP;
    let col_y = inc_y;
    draw_button(gui, win_id, col_x, col_y, BTN_W, BTN_H, "COLOR");

    let _ = gui.window_present(win_id);
}

fn draw_button(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    label: &str,
) {
    let _ = gui.window_rect(win_id, x, y, w, h, BUTTON_BORDER.0, BUTTON_BORDER.1, BUTTON_BORDER.2);
    let _ = gui.window_rect(win_id, x + 2, y + 2, w - 4, h - 4, BUTTON_BG.0, BUTTON_BG.1, BUTTON_BG.2);
    let text_x = x + (w / 2) - 8;
    let text_y = y + (h / 2) - 4;
    let _ = gui.window_text(win_id, text_x, text_y, TEXT_FG, BUTTON_BG, label);
}

fn hit_button_inc(x: i32, y: i32) -> bool {
    hit_button_at(x, y, PAD as i32, (PAD + 44) as i32)
}

fn hit_button_color(x: i32, y: i32) -> bool {
    hit_button_at(x, y, (PAD + BTN_W + GAP) as i32, (PAD + 44) as i32)
}

fn hit_button_at(x: i32, y: i32, bx: i32, by: i32) -> bool {
    if x < 0 || y < 0 {
        return false;
    }
    let x0 = x as u32;
    let y0 = y as u32;
    let bx = bx as u32;
    let by = by as u32;
    x0 >= bx && x0 < bx + BTN_W && y0 >= by && y0 < by + BTN_H
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
