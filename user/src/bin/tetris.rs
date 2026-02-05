// tetris.rs — GUI テトリス（user space）
//
// GUI IPC を使ってウィンドウ内にテトリスを描画する。

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

use alloc::format;
use core::panic::PanicInfo;

const BOARD_W: i32 = 10;
const BOARD_H: i32 = 20;
const CELL: u32 = 12;
const PAD: u32 = 12;
const TOP: u32 = 28;
const SIDE_W: u32 = 120;
const GAP: u32 = 8;

const BTN_W: u32 = 44;
const BTN_H: u32 = 28;

const DROP_MS: u64 = 500;
const TICK_MS: u64 = 50;

const BG: (u8, u8, u8) = (18, 22, 32);
const PANEL: (u8, u8, u8) = (24, 28, 44);
const BORDER: (u8, u8, u8) = (80, 120, 200);
const TEXT_FG: (u8, u8, u8) = (220, 240, 255);
const TEXT_ACCENT: (u8, u8, u8) = (255, 220, 120);

#[derive(Clone, Copy)]
struct Piece {
    kind: usize,
    rot: usize,
    x: i32,
    y: i32,
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    app_main();
}

fn app_main() -> ! {
    let mut gui = gui_client::GuiClient::new();
    let (win_w, win_h) = window_size();
    let win_id = match gui.window_create(win_w, win_h, "SABOS TETRIS") {
        Ok(id) => id,
        Err(_) => syscall::exit(),
    };

    let mut rng = XorShift32::new(syscall::getpid() as u32 ^ 0x1234_5678);
    let mut board = [[0u8; BOARD_W as usize]; BOARD_H as usize];
    let mut current = spawn_piece(&mut rng);
    let mut next = spawn_piece(&mut rng);
    let mut score: u64 = 0;
    let mut lines: u64 = 0;
    let mut game_over = false;

    let mut last_seq: u32 = 0;
    let mut last_buttons: u8 = 0;
    let mut fall_acc: u64 = 0;

    draw_all(&mut gui, win_id, &board, &current, &next, score, lines, game_over);

    loop {
        if let Ok(mouse) = gui.window_mouse_state(win_id) {
            if mouse.seq != last_seq {
                let left_now = (mouse.buttons & 0x1) != 0;
                let left_prev = (last_buttons & 0x1) != 0;
                last_seq = mouse.seq;
                last_buttons = mouse.buttons;

                if mouse.inside && left_now && !left_prev {
                    if hit_btn_left(mouse.x, mouse.y) {
                        try_move(&board, &mut current, -1, 0);
                    } else if hit_btn_right(mouse.x, mouse.y) {
                        try_move(&board, &mut current, 1, 0);
                    } else if hit_btn_down(mouse.x, mouse.y) {
                        if !try_move(&board, &mut current, 0, 1) {
                            lock_piece(&mut board, &current);
                            let cleared = clear_lines(&mut board);
                            add_score(&mut score, &mut lines, cleared);
                            current = next;
                            next = spawn_piece(&mut rng);
                            if collide(&board, &current) {
                                game_over = true;
                            }
                        }
                    } else if hit_btn_rotate(mouse.x, mouse.y) {
                        try_rotate(&board, &mut current);
                    } else if hit_btn_drop(mouse.x, mouse.y) {
                        while try_move(&board, &mut current, 0, 1) {}
                        lock_piece(&mut board, &current);
                        let cleared = clear_lines(&mut board);
                        add_score(&mut score, &mut lines, cleared);
                        current = next;
                        next = spawn_piece(&mut rng);
                        if collide(&board, &current) {
                            game_over = true;
                        }
                    } else if hit_btn_reset(mouse.x, mouse.y) {
                        board = [[0u8; BOARD_W as usize]; BOARD_H as usize];
                        current = spawn_piece(&mut rng);
                        next = spawn_piece(&mut rng);
                        score = 0;
                        lines = 0;
                        game_over = false;
                        fall_acc = 0;
                    }
                }
            }
        }

        if !game_over {
            fall_acc += TICK_MS;
            if fall_acc >= DROP_MS {
                fall_acc = 0;
                if !try_move(&board, &mut current, 0, 1) {
                    lock_piece(&mut board, &current);
                    let cleared = clear_lines(&mut board);
                    add_score(&mut score, &mut lines, cleared);
                    current = next;
                    next = spawn_piece(&mut rng);
                    if collide(&board, &current) {
                        game_over = true;
                    }
                }
            }
        }

        draw_all(&mut gui, win_id, &board, &current, &next, score, lines, game_over);
        syscall::sleep(TICK_MS as u64);
    }
}

fn window_size() -> (u32, u32) {
    let board_w = BOARD_W as u32 * CELL;
    let board_h = BOARD_H as u32 * CELL;
    let w = PAD * 2 + board_w + GAP + SIDE_W;
    let h = TOP + board_h + PAD;
    (w + 4, h + 28 + 4)
}

fn draw_all(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    board: &[[u8; BOARD_W as usize]; BOARD_H as usize],
    current: &Piece,
    next: &Piece,
    score: u64,
    lines: u64,
    game_over: bool,
) {
    let _ = gui.window_clear(win_id, BG.0, BG.1, BG.2);

    let (win_w, _win_h) = window_size();
    let inner_w = win_w - 8;
    let _ = gui.window_rect(win_id, 2, 2, inner_w, 28, BORDER.0, BORDER.1, BORDER.2);
    let _ = gui.window_rect(win_id, 4, 4, inner_w - 4, 24, PANEL.0, PANEL.1, PANEL.2);
    let _ = gui.window_text(win_id, 8, 8, TEXT_ACCENT, PANEL, "SABOS TETRIS");

    draw_board(gui, win_id, board, current);
    draw_side(gui, win_id, next, score, lines, game_over);

    let _ = gui.window_present(win_id);
}

fn draw_board(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    board: &[[u8; BOARD_W as usize]; BOARD_H as usize],
    current: &Piece,
) {
    let board_x = PAD;
    let board_y = TOP;
    let bw = BOARD_W as u32 * CELL;
    let bh = BOARD_H as u32 * CELL;

    let _ = gui.window_rect(win_id, board_x - 2, board_y - 2, bw + 4, bh + 4, BORDER.0, BORDER.1, BORDER.2);
    let _ = gui.window_rect(win_id, board_x, board_y, bw, bh, PANEL.0, PANEL.1, PANEL.2);

    for y in 0..BOARD_H {
        for x in 0..BOARD_W {
            let v = board[y as usize][x as usize];
            if v != 0 {
                draw_cell(gui, win_id, board_x, board_y, x, y, v);
            }
        }
    }

    for (dx, dy) in piece_cells(current) {
        let x = current.x + dx;
        let y = current.y + dy;
        if y >= 0 {
            let v = (current.kind + 1) as u8;
            draw_cell(gui, win_id, board_x, board_y, x, y, v);
        }
    }
}

fn draw_cell(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    board_x: u32,
    board_y: u32,
    x: i32,
    y: i32,
    v: u8,
) {
    let (r, g, b) = piece_color(v);
    let px = board_x + (x as u32) * CELL;
    let py = board_y + (y as u32) * CELL;
    let _ = gui.window_rect(win_id, px, py, CELL, CELL, r, g, b);
    let edge = (r.saturating_sub(40), g.saturating_sub(40), b.saturating_sub(40));
    let _ = gui.window_rect(win_id, px + 2, py + 2, CELL - 4, CELL - 4, edge.0, edge.1, edge.2);
}

fn draw_side(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    next: &Piece,
    score: u64,
    lines: u64,
    game_over: bool,
) {
    let board_x = PAD;
    let board_w = BOARD_W as u32 * CELL;
    let side_x = board_x + board_w + GAP;
    let side_y = TOP;

    let _ = gui.window_rect(win_id, side_x, side_y, SIDE_W, 200, PANEL.0, PANEL.1, PANEL.2);
    let _ = gui.window_text(win_id, side_x + 8, side_y + 8, TEXT_ACCENT, PANEL, "NEXT");

    // 次ピース表示（2x2セル相当）
    let preview_x = side_x + 16;
    let preview_y = side_y + 28;
    for (dx, dy) in piece_cells(next) {
        let x = preview_x + (dx as u32) * (CELL / 2);
        let y = preview_y + (dy as u32) * (CELL / 2);
        let v = (next.kind + 1) as u8;
        let (r, g, b) = piece_color(v);
        let _ = gui.window_rect(win_id, x, y, CELL / 2, CELL / 2, r, g, b);
    }

    let score_text = format!("SCORE {}", score);
    let lines_text = format!("LINES {}", lines);
    let _ = gui.window_text(win_id, side_x + 8, side_y + 90, TEXT_FG, PANEL, score_text.as_str());
    let _ = gui.window_text(win_id, side_x + 8, side_y + 108, TEXT_FG, PANEL, lines_text.as_str());

    draw_button(gui, win_id, btn_left_pos(), "<");
    draw_button(gui, win_id, btn_right_pos(), ">");
    draw_button(gui, win_id, btn_down_pos(), "V");
    draw_button(gui, win_id, btn_rotate_pos(), "R");
    draw_button(gui, win_id, btn_drop_pos(), "DROP");
    draw_button(gui, win_id, btn_reset_pos(), "RESET");

    if game_over {
        let _ = gui.window_text(win_id, side_x + 8, side_y + 170, (255, 120, 120), PANEL, "GAME OVER");
    }
}

fn draw_button(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    pos: (u32, u32, u32, u32),
    label: &str,
) {
    let (x, y, w, h) = pos;
    let _ = gui.window_rect(win_id, x, y, w, h, BORDER.0, BORDER.1, BORDER.2);
    let _ = gui.window_rect(win_id, x + 2, y + 2, w - 4, h - 4, PANEL.0, PANEL.1, PANEL.2);
    let tx = x + 6;
    let ty = y + 8;
    let _ = gui.window_text(win_id, tx, ty, TEXT_FG, PANEL, label);
}

fn btn_left_pos() -> (u32, u32, u32, u32) {
    let board_x = PAD;
    let board_w = BOARD_W as u32 * CELL;
    let side_x = board_x + board_w + GAP;
    (side_x + 4, TOP + 130, BTN_W, BTN_H)
}

fn btn_right_pos() -> (u32, u32, u32, u32) {
    let (x, y, _w, _h) = btn_left_pos();
    (x + BTN_W + 6, y, BTN_W, BTN_H)
}

fn btn_down_pos() -> (u32, u32, u32, u32) {
    let (x, y, _w, _h) = btn_left_pos();
    (x, y + BTN_H + 6, BTN_W, BTN_H)
}

fn btn_rotate_pos() -> (u32, u32, u32, u32) {
    let (x, y, _w, _h) = btn_right_pos();
    (x, y + BTN_H + 6, BTN_W, BTN_H)
}

fn btn_drop_pos() -> (u32, u32, u32, u32) {
    let board_x = PAD;
    let board_w = BOARD_W as u32 * CELL;
    let side_x = board_x + board_w + GAP;
    (side_x + 4, TOP + 200, SIDE_W - 8, BTN_H)
}

fn btn_reset_pos() -> (u32, u32, u32, u32) {
    let (x, y, w, h) = btn_drop_pos();
    (x, y + BTN_H + 6, w, h)
}

fn hit_btn_left(x: i32, y: i32) -> bool {
    hit_btn_at(x, y, btn_left_pos())
}
fn hit_btn_right(x: i32, y: i32) -> bool {
    hit_btn_at(x, y, btn_right_pos())
}
fn hit_btn_down(x: i32, y: i32) -> bool {
    hit_btn_at(x, y, btn_down_pos())
}
fn hit_btn_rotate(x: i32, y: i32) -> bool {
    hit_btn_at(x, y, btn_rotate_pos())
}
fn hit_btn_drop(x: i32, y: i32) -> bool {
    hit_btn_at(x, y, btn_drop_pos())
}
fn hit_btn_reset(x: i32, y: i32) -> bool {
    hit_btn_at(x, y, btn_reset_pos())
}

fn hit_btn_at(x: i32, y: i32, pos: (u32, u32, u32, u32)) -> bool {
    if x < 0 || y < 0 {
        return false;
    }
    let (bx, by, bw, bh) = pos;
    let x0 = x as u32;
    let y0 = y as u32;
    x0 >= bx && x0 < bx + bw && y0 >= by && y0 < by + bh
}

fn add_score(score: &mut u64, lines: &mut u64, cleared: u32) {
    let add = match cleared {
        1 => 100,
        2 => 300,
        3 => 500,
        4 => 800,
        _ => 0,
    };
    *score = score.saturating_add(add);
    *lines = lines.saturating_add(cleared as u64);
}

fn spawn_piece(rng: &mut XorShift32) -> Piece {
    let kind = (rng.next() % 7) as usize;
    Piece { kind, rot: 0, x: 3, y: -1 }
}

fn try_move(board: &[[u8; BOARD_W as usize]; BOARD_H as usize], piece: &mut Piece, dx: i32, dy: i32) -> bool {
    let mut next = *piece;
    next.x += dx;
    next.y += dy;
    if collide(board, &next) {
        false
    } else {
        *piece = next;
        true
    }
}

fn try_rotate(board: &[[u8; BOARD_W as usize]; BOARD_H as usize], piece: &mut Piece) {
    let mut next = *piece;
    next.rot = (next.rot + 1) % 4;
    if !collide(board, &next) {
        *piece = next;
    }
}

fn collide(board: &[[u8; BOARD_W as usize]; BOARD_H as usize], piece: &Piece) -> bool {
    for (dx, dy) in piece_cells(piece) {
        let x = piece.x + dx;
        let y = piece.y + dy;
        if x < 0 || x >= BOARD_W {
            return true;
        }
        if y >= BOARD_H {
            return true;
        }
        if y >= 0 {
            if board[y as usize][x as usize] != 0 {
                return true;
            }
        }
    }
    false
}

fn lock_piece(board: &mut [[u8; BOARD_W as usize]; BOARD_H as usize], piece: &Piece) {
    let v = (piece.kind + 1) as u8;
    for (dx, dy) in piece_cells(piece) {
        let x = piece.x + dx;
        let y = piece.y + dy;
        if x >= 0 && x < BOARD_W && y >= 0 && y < BOARD_H {
            board[y as usize][x as usize] = v;
        }
    }
}

fn clear_lines(board: &mut [[u8; BOARD_W as usize]; BOARD_H as usize]) -> u32 {
    let mut cleared = 0;
    let mut y = BOARD_H - 1;
    while y >= 0 {
        let mut full = true;
        for x in 0..BOARD_W {
            if board[y as usize][x as usize] == 0 {
                full = false;
                break;
            }
        }
        if full {
            for yy in (1..=y).rev() {
                board[yy as usize] = board[(yy - 1) as usize];
            }
            board[0] = [0u8; BOARD_W as usize];
            cleared += 1;
        } else {
            if y == 0 { break; }
            y -= 1;
        }
    }
    cleared
}

fn piece_cells(piece: &Piece) -> [(i32, i32); 4] {
    let mut out = [(0, 0); 4];
    let shape = &PIECES[piece.kind][piece.rot];
    let mut idx = 0usize;
    for y in 0..4 {
        for x in 0..4 {
            let v = shape[y * 4 + x];
            if v != 0 {
                out[idx] = (x as i32, y as i32);
                idx += 1;
                if idx == 4 {
                    return out;
                }
            }
        }
    }
    out
}

fn piece_color(v: u8) -> (u8, u8, u8) {
    match v {
        1 => (80, 200, 240),
        2 => (80, 120, 240),
        3 => (240, 200, 80),
        4 => (240, 120, 80),
        5 => (200, 80, 240),
        6 => (80, 200, 120),
        7 => (200, 80, 120),
        _ => (60, 60, 60),
    }
}

struct XorShift32 {
    state: u32,
}

impl XorShift32 {
    fn new(seed: u32) -> Self {
        let seed = if seed == 0 { 0x1234_5678 } else { seed };
        Self { state: seed }
    }

    fn next(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }
}

const PIECES: [[[u8; 16]; 4]; 7] = [
    // I
    [
        [
            0,0,0,0,
            1,1,1,1,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,0,1,0,
            0,0,1,0,
            0,0,1,0,
            0,0,1,0,
        ],
        [
            0,0,0,0,
            1,1,1,1,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,1,0,0,
            0,1,0,0,
            0,1,0,0,
            0,1,0,0,
        ],
    ],
    // J
    [
        [
            2,0,0,0,
            2,2,2,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,2,2,0,
            0,2,0,0,
            0,2,0,0,
            0,0,0,0,
        ],
        [
            0,0,0,0,
            2,2,2,0,
            0,0,2,0,
            0,0,0,0,
        ],
        [
            0,2,0,0,
            0,2,0,0,
            2,2,0,0,
            0,0,0,0,
        ],
    ],
    // L
    [
        [
            0,0,3,0,
            3,3,3,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,3,0,0,
            0,3,0,0,
            0,3,3,0,
            0,0,0,0,
        ],
        [
            0,0,0,0,
            3,3,3,0,
            3,0,0,0,
            0,0,0,0,
        ],
        [
            3,3,0,0,
            0,3,0,0,
            0,3,0,0,
            0,0,0,0,
        ],
    ],
    // O
    [
        [
            0,4,4,0,
            0,4,4,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,4,4,0,
            0,4,4,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,4,4,0,
            0,4,4,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,4,4,0,
            0,4,4,0,
            0,0,0,0,
            0,0,0,0,
        ],
    ],
    // S
    [
        [
            0,5,5,0,
            5,5,0,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,5,0,0,
            0,5,5,0,
            0,0,5,0,
            0,0,0,0,
        ],
        [
            0,5,5,0,
            5,5,0,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,5,0,0,
            0,5,5,0,
            0,0,5,0,
            0,0,0,0,
        ],
    ],
    // T
    [
        [
            0,6,0,0,
            6,6,6,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,6,0,0,
            0,6,6,0,
            0,6,0,0,
            0,0,0,0,
        ],
        [
            0,0,0,0,
            6,6,6,0,
            0,6,0,0,
            0,0,0,0,
        ],
        [
            0,6,0,0,
            6,6,0,0,
            0,6,0,0,
            0,0,0,0,
        ],
    ],
    // Z
    [
        [
            7,7,0,0,
            0,7,7,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,0,7,0,
            0,7,7,0,
            0,7,0,0,
            0,0,0,0,
        ],
        [
            7,7,0,0,
            0,7,7,0,
            0,0,0,0,
            0,0,0,0,
        ],
        [
            0,0,7,0,
            0,7,7,0,
            0,7,0,0,
            0,0,0,0,
        ],
    ],
];

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
