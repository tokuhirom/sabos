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
const TITLE_BAR_BG: (u8, u8, u8) = (36, 44, 72);
const INFO_TEXT: (u8, u8, u8) = (180, 200, 220);
const DISPLAY_TEXT: (u8, u8, u8) = (220, 240, 255);
const QUIT_TEXT: (u8, u8, u8) = (255, 160, 160);

const PAD: u32 = 12;
const GAP: u32 = 8;
const TITLE_BAR_H: u32 = 24;
const DISPLAY_H: u32 = 40;
const BTN_W: u32 = 70;
const BTN_H: u32 = 50;
const QUIT_W: u32 = 24;
const QUIT_H: u32 = 18;

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
    quit_x: u32,
    quit_y: u32,
    quit_w: u32,
    quit_h: u32,
    title_bar_x: u32,
    title_bar_y: u32,
    title_bar_w: u32,
    title_bar_h: u32,
}

struct WindowState {
    x: u32,
    y: u32,
    dragging: bool,
    drag_dx: i32,
    drag_dy: i32,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    calc_main();
}

fn calc_main() -> ! {
    let mut gui = gui_client::GuiClient::new();
    let (screen_w, screen_h) = screen_size();
    let panel_size = panel_size();
    let mut window = WindowState {
        x: center_pos(screen_w, panel_size.0),
        y: center_pos(screen_h, panel_size.1),
        dragging: false,
        drag_dx: 0,
        drag_dy: 0,
    };
    let mut layout = build_layout(window.x, window.y);

    let mut state = CalcState {
        left: 0,
        op: None,
        entry: String::from("0"),
        input: InputTarget::Left,
        fresh_entry: false,
        error: false,
    };

    draw_ui(&mut gui, &layout, &state);

    // マウスの更新カウンタとボタン状態で「クリックの立ち上がり」を検出する
    let mut last_seq: u32 = 0;
    let mut last_buttons: u8 = 0;

    loop {
        // GUI サービスからマウス状態を取得してクリックを判定する
        if let Ok(mouse) = gui.mouse_state() {
            if mouse.seq != last_seq {
                let left_now = (mouse.buttons & 0x1) != 0;
                let left_prev = (last_buttons & 0x1) != 0;
                last_seq = mouse.seq;
                last_buttons = mouse.buttons;

                // 左クリックの立ち上がりでドラッグ開始 or ボタン押下
                if left_now && !left_prev {
                    if hit_title_bar(&layout, mouse.x, mouse.y) {
                        window.dragging = true;
                        window.drag_dx = mouse.x - window.x as i32;
                        window.drag_dy = mouse.y - window.y as i32;
                    } else if let Some(action) = hit_test(&layout, mouse.x, mouse.y) {
                        if apply_action(&mut state, action) {
                            update_display(&mut gui, &layout, &state);
                            let _ = gui.present();
                        }
                    }
                }

                // 左ボタンを離したらドラッグ終了
                if !left_now && left_prev {
                    window.dragging = false;
                }
            }

            // ドラッグ中はウィンドウ位置を更新して再描画
            if window.dragging {
                let mut new_x = mouse.x - window.drag_dx;
                let mut new_y = mouse.y - window.drag_dy;
                new_x = new_x.clamp(0, screen_w as i32 - panel_size.0 as i32);
                new_y = new_y.clamp(0, screen_h as i32 - panel_size.1 as i32);
                let new_x = new_x as u32;
                let new_y = new_y as u32;
                if new_x != window.x || new_y != window.y {
                    window.x = new_x;
                    window.y = new_y;
                    layout = build_layout(window.x, window.y);
                    draw_ui(&mut gui, &layout, &state);
                }
            }
        }
        // 入力待ちで固まらないように短い sleep を挟む
        syscall::sleep(16);
    }
}

fn panel_size() -> (u32, u32) {
    let panel_w = PAD * 2 + (BTN_W * 4) + (GAP * 3);
    let panel_h = PAD * 2 + TITLE_BAR_H + GAP + DISPLAY_H + GAP + (BTN_H * 4) + (GAP * 3) + 20;
    (panel_w, panel_h)
}

fn screen_size() -> (u32, u32) {
    let mut info = syscall::FramebufferInfo {
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: 0,
        bytes_per_pixel: 0,
    };
    let _ = syscall::get_fb_info(&mut info);
    (info.width, info.height)
}

fn center_pos(total: u32, size: u32) -> u32 {
    if total > size {
        (total - size) / 2
    } else {
        0
    }
}

fn build_layout(x: u32, y: u32) -> Layout {
    let (panel_w, panel_h) = panel_size();

    let display_x = x + PAD;
    let display_y = y + PAD + TITLE_BAR_H + GAP;
    let display_w = panel_w - PAD * 2;
    let display_h = DISPLAY_H;
    let button_x = x + PAD;
    let button_y = display_y + display_h + GAP;
    let quit_x = x + panel_w - PAD - QUIT_W;
    let quit_y = y + PAD - 2;
    let title_bar_x = x + 2;
    let title_bar_y = y + 2;
    let title_bar_w = panel_w - 4;
    let title_bar_h = TITLE_BAR_H;

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
        quit_x,
        quit_y,
        quit_w: QUIT_W,
        quit_h: QUIT_H,
        title_bar_x,
        title_bar_y,
        title_bar_w,
        title_bar_h,
    }
}

fn draw_ui(gui: &mut gui_client::GuiClient, layout: &Layout, state: &CalcState) {
    let _ = gui.clear(BG.0, BG.1, BG.2);

    // パネル
    let _ = gui.rect(layout.x, layout.y, layout.panel_w, layout.panel_h, PANEL_BORDER.0, PANEL_BORDER.1, PANEL_BORDER.2);
    let _ = gui.rect(layout.x + 2, layout.y + 2, layout.panel_w - 4, layout.panel_h - 4, PANEL_BG.0, PANEL_BG.1, PANEL_BG.2);

    // タイトルバー
    let _ = gui.rect(
        layout.title_bar_x,
        layout.title_bar_y,
        layout.title_bar_w,
        layout.title_bar_h,
        PANEL_BORDER.0,
        PANEL_BORDER.1,
        PANEL_BORDER.2,
    );
    let _ = gui.rect(
        layout.title_bar_x + 1,
        layout.title_bar_y + 1,
        layout.title_bar_w - 2,
        layout.title_bar_h - 2,
        TITLE_BAR_BG.0,
        TITLE_BAR_BG.1,
        TITLE_BAR_BG.2,
    );

    // タイトル
    let title_x = layout.x + PAD;
    let title_y = layout.y + PAD + 2;
    let _ = gui.text(title_x, title_y, TITLE_TEXT, TITLE_BAR_BG, "SABOS CALC");
    draw_quit_button(gui, layout);

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

    // 操作説明
    let info_y = layout.button_y + (BTN_H + GAP) * 4 + 4;
    let _ = gui.text(layout.x + PAD, info_y, INFO_TEXT, PANEL_BG, "Click: 0-9 + - * / =  C(clear)  Q(quit)");

    let _ = gui.present();
}

fn draw_button(gui: &mut gui_client::GuiClient, x: u32, y: u32, w: u32, h: u32, label: &str) {
    let _ = gui.rect(x, y, w, h, BUTTON_BORDER.0, BUTTON_BORDER.1, BUTTON_BORDER.2);
    let _ = gui.rect(x + 2, y + 2, w - 4, h - 4, BUTTON_BG.0, BUTTON_BG.1, BUTTON_BG.2);
    let text_x = x + (w / 2) - 4;
    let text_y = y + (h / 2) - 4;
    let _ = gui.text(text_x, text_y, BUTTON_TEXT, BUTTON_BG, label);
}

fn draw_quit_button(gui: &mut gui_client::GuiClient, layout: &Layout) {
    let x = layout.quit_x;
    let y = layout.quit_y;
    let w = layout.quit_w;
    let h = layout.quit_h;
    let _ = gui.rect(x, y, w, h, BUTTON_BORDER.0, BUTTON_BORDER.1, BUTTON_BORDER.2);
    let _ = gui.rect(x + 1, y + 1, w - 2, h - 2, BUTTON_BG.0, BUTTON_BG.1, BUTTON_BG.2);
    let _ = gui.text(x + 6, y + 5, QUIT_TEXT, BUTTON_BG, "Q");
}

fn draw_display(gui: &mut gui_client::GuiClient, layout: &Layout, state: &CalcState) {
    let _ = gui.rect(layout.display_x, layout.display_y, layout.display_w, layout.display_h, DISPLAY_BORDER.0, DISPLAY_BORDER.1, DISPLAY_BORDER.2);
    let _ = gui.rect(layout.display_x + 2, layout.display_y + 2, layout.display_w - 4, layout.display_h - 4, DISPLAY_BG.0, DISPLAY_BG.1, DISPLAY_BG.2);

    // エラー時は ERR 表示に切り替える
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

#[derive(Clone, Copy)]
enum CalcAction {
    Digit(char),
    Operator(char),
    Equal,
    Clear,
    Quit,
}

fn apply_action(state: &mut CalcState, action: CalcAction) -> bool {
    // クリックされたボタンに応じて状態を更新する
    match action {
        CalcAction::Digit(ch) => {
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
        CalcAction::Operator(op) => {
            if state.error {
                return false;
            }
            handle_operator(state, op);
            true
        }
        CalcAction::Equal => {
            if state.error {
                return false;
            }
            handle_equals(state);
            true
        }
        CalcAction::Clear => {
            reset_state(state);
            true
        }
        CalcAction::Quit => {
            syscall::exit();
        }
    }
}

fn hit_test(layout: &Layout, x: i32, y: i32) -> Option<CalcAction> {
    if x < 0 || y < 0 {
        return None;
    }
    let x = x as u32;
    let y = y as u32;

    // まずは終了ボタンを優先
    if x >= layout.quit_x && x < layout.quit_x + layout.quit_w &&
        y >= layout.quit_y && y < layout.quit_y + layout.quit_h {
        return Some(CalcAction::Quit);
    }

    // ボタン配置（4x4）
    let labels = [
        ['7', '8', '9', '/'],
        ['4', '5', '6', '*'],
        ['1', '2', '3', '-'],
        ['0', 'C', '=', '+'],
    ];
    for (row, cols) in labels.iter().enumerate() {
        for (col, label) in cols.iter().enumerate() {
            let bx = layout.button_x + (BTN_W + GAP) * col as u32;
            let by = layout.button_y + (BTN_H + GAP) * row as u32;
            if x >= bx && x < bx + BTN_W && y >= by && y < by + BTN_H {
                return match label {
                    '0'..='9' => Some(CalcAction::Digit(*label)),
                    '+' | '-' | '*' | '/' => Some(CalcAction::Operator(*label)),
                    '=' => Some(CalcAction::Equal),
                    'C' => Some(CalcAction::Clear),
                    _ => None,
                };
            }
        }
    }
    None
}

fn hit_title_bar(layout: &Layout, x: i32, y: i32) -> bool {
    if x < 0 || y < 0 {
        return false;
    }
    let x = x as u32;
    let y = y as u32;
    x >= layout.title_bar_x
        && x < layout.title_bar_x + layout.title_bar_w
        && y >= layout.title_bar_y
        && y < layout.title_bar_y + layout.title_bar_h
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
