// calc.rs — GUI 電卓アプリ（user space）
//
// GUI サービスに描画要求を送り、キーボード入力で計算する。
// まずは四則演算とクリアだけの最小実装。

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
#[path = "../syscall.rs"]
mod syscall;

use alloc::string::String;
use alloc::format;
use core::panic::PanicInfo;

const BG: (u8, u8, u8) = (16, 16, 24);
const PANEL_BG: (u8, u8, u8) = (28, 32, 48);
const PANEL_BORDER: (u8, u8, u8) = (90, 120, 180);
const DISPLAY_BG: (u8, u8, u8) = (8, 12, 20);
const DISPLAY_BORDER: (u8, u8, u8) = (120, 140, 200);
const BUTTON_BG: (u8, u8, u8) = (40, 48, 72);
const BUTTON_BORDER: (u8, u8, u8) = (80, 100, 160);
const BUTTON_TEXT: (u8, u8, u8) = (230, 240, 255);
const TITLE_TEXT: (u8, u8, u8) = (255, 220, 120);
const INFO_TEXT: (u8, u8, u8) = (180, 200, 220);
const DISPLAY_TEXT: (u8, u8, u8) = (220, 240, 255);

const PAD: u32 = 12;
const GAP: u32 = 8;
const TITLE_H: u32 = 18;
const DISPLAY_H: u32 = 40;
const BTN_W: u32 = 70;
const BTN_H: u32 = 50;

const MAX_DIGITS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputTarget {
    Left,
    Right,
}

struct CalcState {
    left: i64,
    op: Option<char>,
    entry: String,
    input: InputTarget,
    fresh_entry: bool,
    error: bool,
}

struct Layout {
    x: u32,
    y: u32,
    panel_w: u32,
    panel_h: u32,
    display_x: u32,
    display_y: u32,
    display_w: u32,
    display_h: u32,
    button_x: u32,
    button_y: u32,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    calc_main();
}

fn calc_main() -> ! {
    let mut gui = gui_client::GuiClient::new();
    let layout = build_layout();

    let mut state = CalcState {
        left: 0,
        op: None,
        entry: String::from("0"),
        input: InputTarget::Left,
        fresh_entry: false,
        error: false,
    };

    draw_ui(&mut gui, &layout, &state);

    loop {
        let ch = syscall::read_char();
        if ch == 'q' || ch == 'Q' {
            syscall::exit();
        }
        if handle_input(&mut state, ch) {
            update_display(&mut gui, &layout, &state);
            let _ = gui.present();
        }
    }
}

fn build_layout() -> Layout {
    let mut info = syscall::FramebufferInfo {
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: 0,
        bytes_per_pixel: 0,
    };
    let _ = syscall::get_fb_info(&mut info);

    let panel_w = PAD * 2 + (BTN_W * 4) + (GAP * 3);
    let panel_h = PAD * 2 + TITLE_H + GAP + DISPLAY_H + GAP + (BTN_H * 4) + (GAP * 3) + 20;

    let x = if info.width > panel_w {
        (info.width - panel_w) / 2
    } else {
        0
    };
    let y = if info.height > panel_h {
        (info.height - panel_h) / 2
    } else {
        0
    };

    let display_x = x + PAD;
    let display_y = y + PAD + TITLE_H + GAP;
    let display_w = panel_w - PAD * 2;
    let display_h = DISPLAY_H;
    let button_x = x + PAD;
    let button_y = display_y + display_h + GAP;

    Layout {
        x,
        y,
        panel_w,
        panel_h,
        display_x,
        display_y,
        display_w,
        display_h,
        button_x,
        button_y,
    }
}

fn draw_ui(gui: &mut gui_client::GuiClient, layout: &Layout, state: &CalcState) {
    let _ = gui.clear(BG.0, BG.1, BG.2);

    // パネル
    let _ = gui.rect(layout.x, layout.y, layout.panel_w, layout.panel_h, PANEL_BORDER.0, PANEL_BORDER.1, PANEL_BORDER.2);
    let _ = gui.rect(layout.x + 2, layout.y + 2, layout.panel_w - 4, layout.panel_h - 4, PANEL_BG.0, PANEL_BG.1, PANEL_BG.2);

    // タイトル
    let title_x = layout.x + PAD;
    let title_y = layout.y + PAD;
    let _ = gui.text(title_x, title_y, TITLE_TEXT, PANEL_BG, "SABOS CALC");

    // ディスプレイ
    draw_display(gui, layout, state);

    // ボタン
    let labels = [
        ["7", "8", "9", "/"],
        ["4", "5", "6", "*"],
        ["1", "2", "3", "-"],
        ["0", "C", "=", "+"],
    ];
    for (row, cols) in labels.iter().enumerate() {
        for (col, label) in cols.iter().enumerate() {
            let x = layout.button_x + (BTN_W + GAP) * col as u32;
            let y = layout.button_y + (BTN_H + GAP) * row as u32;
            draw_button(gui, x, y, BTN_W, BTN_H, label);
        }
    }

    // キー説明
    let info_y = layout.button_y + (BTN_H + GAP) * 4 + 4;
    let _ = gui.text(layout.x + PAD, info_y, INFO_TEXT, PANEL_BG, "Keys: 0-9 + - * / =  C(clear)  Q(quit)");

    let _ = gui.present();
}

fn draw_button(gui: &mut gui_client::GuiClient, x: u32, y: u32, w: u32, h: u32, label: &str) {
    let _ = gui.rect(x, y, w, h, BUTTON_BORDER.0, BUTTON_BORDER.1, BUTTON_BORDER.2);
    let _ = gui.rect(x + 2, y + 2, w - 4, h - 4, BUTTON_BG.0, BUTTON_BG.1, BUTTON_BG.2);
    let text_x = x + (w / 2) - 4;
    let text_y = y + (h / 2) - 4;
    let _ = gui.text(text_x, text_y, BUTTON_TEXT, BUTTON_BG, label);
}

fn draw_display(gui: &mut gui_client::GuiClient, layout: &Layout, state: &CalcState) {
    let _ = gui.rect(layout.display_x, layout.display_y, layout.display_w, layout.display_h, DISPLAY_BORDER.0, DISPLAY_BORDER.1, DISPLAY_BORDER.2);
    let _ = gui.rect(layout.display_x + 2, layout.display_y + 2, layout.display_w - 4, layout.display_h - 4, DISPLAY_BG.0, DISPLAY_BG.1, DISPLAY_BG.2);

    let text = if state.error {
        String::from("ERR")
    } else {
        state.entry.clone()
    };
    let text_x = layout.display_x + 6;
    let text_y = layout.display_y + 12;
    let _ = gui.text(text_x, text_y, DISPLAY_TEXT, DISPLAY_BG, text.as_str());
}

fn update_display(gui: &mut gui_client::GuiClient, layout: &Layout, state: &CalcState) {
    draw_display(gui, layout, state);
}

fn handle_input(state: &mut CalcState, ch: char) -> bool {
    match ch {
        '0'..='9' => {
            if state.error {
                reset_state(state);
            }
            if state.fresh_entry {
                state.entry.clear();
                state.fresh_entry = false;
            }
            push_digit(state, ch);
            true
        }
        '+' | '-' | '*' | '/' => {
            if state.error {
                return false;
            }
            handle_operator(state, ch);
            true
        }
        '=' => {
            if state.error {
                return false;
            }
            handle_equals(state);
            true
        }
        'c' | 'C' => {
            reset_state(state);
            true
        }
        _ => false,
    }
}

fn push_digit(state: &mut CalcState, ch: char) {
    if state.entry == "0" {
        state.entry.clear();
    }
    if state.entry.len() < MAX_DIGITS {
        state.entry.push(ch);
    }
}

fn handle_operator(state: &mut CalcState, op: char) {
    if let Some(current_op) = state.op {
        if state.input == InputTarget::Right && !state.fresh_entry {
            let right = parse_entry(state);
            match compute(state.left, current_op, right) {
                Ok(v) => {
                    state.left = v;
                    state.entry = format!("{}", v);
                }
                Err(_) => {
                    state.error = true;
                    return;
                }
            }
        }
    } else {
        state.left = parse_entry(state);
    }

    state.op = Some(op);
    state.input = InputTarget::Right;
    state.fresh_entry = true;
}

fn handle_equals(state: &mut CalcState) {
    let Some(op) = state.op else { return; };
    if state.fresh_entry {
        return;
    }
    let right = parse_entry(state);
    match compute(state.left, op, right) {
        Ok(v) => {
            state.left = v;
            state.entry = format!("{}", v);
            state.op = None;
            state.input = InputTarget::Left;
            state.fresh_entry = false;
        }
        Err(_) => {
            state.error = true;
        }
    }
}

fn compute(left: i64, op: char, right: i64) -> Result<i64, ()> {
    match op {
        '+' => Ok(left.saturating_add(right)),
        '-' => Ok(left.saturating_sub(right)),
        '*' => Ok(left.saturating_mul(right)),
        '/' => {
            if right == 0 {
                Err(())
            } else {
                Ok(left / right)
            }
        }
        _ => Err(()),
    }
}

fn parse_entry(state: &CalcState) -> i64 {
    state.entry.parse::<i64>().unwrap_or(0)
}

fn reset_state(state: &mut CalcState) {
    state.left = 0;
    state.op = None;
    state.entry.clear();
    state.entry.push('0');
    state.input = InputTarget::Left;
    state.fresh_entry = false;
    state.error = false;
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
