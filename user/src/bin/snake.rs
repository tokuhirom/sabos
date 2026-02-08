// snake.rs — スネークゲーム（GUI user space アプリ）
//
// 古典的なスネークゲーム:
// - 蛇は常に前進し、方向ボタンで上下左右に曲がれる
// - 餌を食べると蛇が 1 マス伸びてスコアが上がる
// - 壁か自分の体にぶつかるとゲームオーバー
//
// SABOS にはキーボード入力がないため、GUI ボタンで方向を操作する。

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

// グリッドサイズ
const GRID_W: usize = 20;
const GRID_H: usize = 20;
// セルのピクセルサイズ
const CELL: u32 = 12;

// 蛇の最大長（グリッド全体）
const MAX_LEN: usize = GRID_W * GRID_H;

// レイアウト
const PAD: u32 = 8;
const TITLE_H: u32 = 28;
const SIDE_W: u32 = 100;
const GAP: u32 = 8;

// ボタン
const BTN_W: u32 = 36;
const BTN_H: u32 = 28;
const BTN_GAP: u32 = 4;
const BTN_WIDE: u32 = 88;

// ゲーム速度（ミリ秒/ティック）
const MOVE_INTERVAL_MS: u64 = 150;
const TICK_MS: u64 = 30;

// カラーテーマ
const BG: (u8, u8, u8) = (18, 22, 32);
const PANEL: (u8, u8, u8) = (24, 28, 44);
const BORDER: (u8, u8, u8) = (80, 120, 200);
const TEXT_FG: (u8, u8, u8) = (220, 240, 255);
const TEXT_ACCENT: (u8, u8, u8) = (255, 220, 120);
const SNAKE_HEAD: (u8, u8, u8) = (80, 220, 120);
const SNAKE_BODY: (u8, u8, u8) = (60, 180, 100);
const FOOD_COLOR: (u8, u8, u8) = (240, 80, 80);
const GRID_BG: (u8, u8, u8) = (30, 34, 50);

/// 方向
#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    /// 逆方向かどうか（逆走防止）
    fn is_opposite(self, other: Dir) -> bool {
        matches!(
            (self, other),
            (Dir::Up, Dir::Down) | (Dir::Down, Dir::Up) |
            (Dir::Left, Dir::Right) | (Dir::Right, Dir::Left)
        )
    }

    /// 方向に対する移動量
    fn delta(self) -> (i32, i32) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    app_main();
}

fn app_main() -> ! {
    let mut gui = gui_client::GuiClient::new();

    let grid_px = GRID_W as u32 * CELL;
    let win_w = PAD + grid_px + GAP + SIDE_W + PAD + 4;
    let win_h = TITLE_H + grid_px + PAD + 28 + 4;

    let win_id = match gui.window_create(win_w, win_h, "SNAKE") {
        Ok(id) => id,
        Err(_) => syscall::exit(),
    };

    let mut rng = XorShift32::new(syscall::getpid() as u32 ^ 0xCAFE);

    // 蛇の体をリングバッファで管理
    // body[head_idx] が頭、body[(head_idx - len + 1 + MAX_LEN) % MAX_LEN] が尻尾
    let mut body = [(0u8, 0u8); MAX_LEN];
    let mut head_idx: usize;
    let mut len: usize = 3;
    let mut dir = Dir::Right;
    let mut next_dir = Dir::Right;
    let mut score: u32 = 0;
    let mut game_over = false;

    // 初期配置: グリッド中央付近に右向きの蛇（長さ 3）
    let start_x = GRID_W as u8 / 2;
    let start_y = GRID_H as u8 / 2;
    body[0] = (start_x - 2, start_y);
    body[1] = (start_x - 1, start_y);
    body[2] = (start_x, start_y);
    head_idx = 2;

    // 餌を配置
    let mut food = spawn_food(&body, head_idx, len, &mut rng);

    let mut move_acc: u64 = 0;
    let mut last_seq: u32 = 0;
    let mut last_buttons: u8 = 0;

    loop {
        // マウス入力
        if let Ok(mouse) = gui.window_mouse_state(win_id) {
            if mouse.seq != last_seq {
                let left_now = (mouse.buttons & 0x1) != 0;
                let left_prev = (last_buttons & 0x1) != 0;
                last_seq = mouse.seq;
                last_buttons = mouse.buttons;

                if mouse.inside && left_now && !left_prev {
                    // 方向ボタン
                    if hit_btn(mouse.x, mouse.y, btn_up_pos()) {
                        if !Dir::Up.is_opposite(dir) {
                            next_dir = Dir::Up;
                        }
                    } else if hit_btn(mouse.x, mouse.y, btn_down_pos()) {
                        if !Dir::Down.is_opposite(dir) {
                            next_dir = Dir::Down;
                        }
                    } else if hit_btn(mouse.x, mouse.y, btn_left_pos()) {
                        if !Dir::Left.is_opposite(dir) {
                            next_dir = Dir::Left;
                        }
                    } else if hit_btn(mouse.x, mouse.y, btn_right_pos()) {
                        if !Dir::Right.is_opposite(dir) {
                            next_dir = Dir::Right;
                        }
                    } else if hit_btn(mouse.x, mouse.y, btn_reset_pos()) {
                        // リセット
                        body[0] = (start_x - 2, start_y);
                        body[1] = (start_x - 1, start_y);
                        body[2] = (start_x, start_y);
                        head_idx = 2;
                        len = 3;
                        dir = Dir::Right;
                        next_dir = Dir::Right;
                        score = 0;
                        game_over = false;
                        move_acc = 0;
                        food = spawn_food(&body, head_idx, len, &mut rng);
                    }
                }
            }
        }

        // 蛇の移動
        if !game_over {
            move_acc += TICK_MS;
            if move_acc >= MOVE_INTERVAL_MS {
                move_acc = 0;
                dir = next_dir;
                let (dx, dy) = dir.delta();
                let (hx, hy) = body[head_idx];
                let nx = hx as i32 + dx;
                let ny = hy as i32 + dy;

                // 壁との衝突判定
                if nx < 0 || nx >= GRID_W as i32 || ny < 0 || ny >= GRID_H as i32 {
                    game_over = true;
                } else {
                    let nx = nx as u8;
                    let ny = ny as u8;

                    // 自分の体との衝突判定
                    if is_body(nx, ny, &body, head_idx, len) {
                        game_over = true;
                    } else {
                        // 移動: 新しい頭を追加
                        let new_head = (head_idx + 1) % MAX_LEN;
                        body[new_head] = (nx, ny);
                        head_idx = new_head;

                        // 餌を食べたか
                        if nx == food.0 && ny == food.1 {
                            len += 1;
                            score += 10;
                            food = spawn_food(&body, head_idx, len, &mut rng);
                        }
                        // 食べていない場合、尻尾は勝手に短くなる（len は増えない）
                    }
                }
            }
        }

        // 描画
        draw_all(&mut gui, win_id, &body, head_idx, len, food, score, game_over, dir);
        syscall::sleep(TICK_MS);
    }
}

/// 蛇の体にぶつかるか判定
fn is_body(x: u8, y: u8, body: &[(u8, u8); MAX_LEN], head_idx: usize, len: usize) -> bool {
    for i in 0..len {
        let idx = (head_idx + MAX_LEN - i) % MAX_LEN;
        if body[idx].0 == x && body[idx].1 == y {
            return true;
        }
    }
    false
}

/// 餌を蛇の体と重ならない位置にランダム配置
fn spawn_food(
    body: &[(u8, u8); MAX_LEN],
    head_idx: usize,
    len: usize,
    rng: &mut XorShift32,
) -> (u8, u8) {
    loop {
        let x = (rng.next() % GRID_W as u32) as u8;
        let y = (rng.next() % GRID_H as u32) as u8;
        if !is_body(x, y, body, head_idx, len) {
            return (x, y);
        }
    }
}

/// 全画面描画
fn draw_all(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    body: &[(u8, u8); MAX_LEN],
    head_idx: usize,
    len: usize,
    food: (u8, u8),
    score: u32,
    game_over: bool,
    _dir: Dir,
) {
    let _ = gui.window_clear(win_id, BG.0, BG.1, BG.2);

    let grid_px = GRID_W as u32 * CELL;

    // タイトルバー
    let inner_w = PAD + grid_px + GAP + SIDE_W;
    let _ = gui.window_rect(win_id, 2, 2, inner_w, TITLE_H, BORDER.0, BORDER.1, BORDER.2);
    let _ = gui.window_rect(win_id, 4, 4, inner_w - 4, TITLE_H - 4, PANEL.0, PANEL.1, PANEL.2);
    let _ = gui.window_text(win_id, 8, 8, TEXT_ACCENT, PANEL, "SABOS SNAKE");

    // グリッド枠
    let gx0 = PAD;
    let gy0 = TITLE_H;
    let _ = gui.window_rect(win_id, gx0 - 2, gy0 - 2, grid_px + 4, grid_px + 4, BORDER.0, BORDER.1, BORDER.2);
    let _ = gui.window_rect(win_id, gx0, gy0, grid_px, grid_px, GRID_BG.0, GRID_BG.1, GRID_BG.2);

    // 餌を描画
    let fx = gx0 + food.0 as u32 * CELL + 2;
    let fy = gy0 + food.1 as u32 * CELL + 2;
    let _ = gui.window_rect(win_id, fx, fy, CELL - 4, CELL - 4, FOOD_COLOR.0, FOOD_COLOR.1, FOOD_COLOR.2);

    // 蛇を描画
    for i in 0..len {
        let idx = (head_idx + MAX_LEN - i) % MAX_LEN;
        let (bx, by) = body[idx];
        let px = gx0 + bx as u32 * CELL;
        let py = gy0 + by as u32 * CELL;
        let (r, g, b) = if i == 0 { SNAKE_HEAD } else { SNAKE_BODY };
        let _ = gui.window_rect(win_id, px + 1, py + 1, CELL - 2, CELL - 2, r, g, b);
    }

    // サイドパネル
    let side_x = PAD + grid_px + GAP;
    let side_y = TITLE_H;
    let _ = gui.window_rect(win_id, side_x, side_y, SIDE_W, grid_px, PANEL.0, PANEL.1, PANEL.2);

    // スコア
    let score_text = format!("Score:{}", score);
    let _ = gui.window_text(win_id, side_x + 6, side_y + 8, TEXT_ACCENT, PANEL, &score_text);

    // 長さ
    let len_text = format!("Len: {}", len);
    let _ = gui.window_text(win_id, side_x + 6, side_y + 24, TEXT_FG, PANEL, &len_text);

    if game_over {
        let _ = gui.window_text(win_id, side_x + 6, side_y + 44, (255, 120, 120), PANEL, "GAME OVER");
    }

    // 方向ボタン（十字配置）
    draw_button(gui, win_id, btn_up_pos(), "^");
    draw_button(gui, win_id, btn_left_pos(), "<");
    draw_button(gui, win_id, btn_right_pos(), ">");
    draw_button(gui, win_id, btn_down_pos(), "V");

    // リセットボタン
    draw_button(gui, win_id, btn_reset_pos(), "RESET");

    let _ = gui.window_present(win_id);
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
    let _ = gui.window_text(win_id, x + 6, y + 8, TEXT_FG, PANEL, label);
}

// --- ボタン位置（十字配置） ---

fn side_x() -> u32 {
    PAD + GRID_W as u32 * CELL + GAP
}

/// 上ボタン: 十字の上
fn btn_up_pos() -> (u32, u32, u32, u32) {
    let cx = side_x() + SIDE_W / 2;
    (cx - BTN_W / 2, TITLE_H + 80, BTN_W, BTN_H)
}

/// 左ボタン: 十字の左
fn btn_left_pos() -> (u32, u32, u32, u32) {
    let (ux, uy, _, _) = btn_up_pos();
    (ux - BTN_W - BTN_GAP, uy + BTN_H + BTN_GAP, BTN_W, BTN_H)
}

/// 右ボタン: 十字の右
fn btn_right_pos() -> (u32, u32, u32, u32) {
    let (ux, uy, _, _) = btn_up_pos();
    (ux + BTN_W + BTN_GAP, uy + BTN_H + BTN_GAP, BTN_W, BTN_H)
}

/// 下ボタン: 十字の下
fn btn_down_pos() -> (u32, u32, u32, u32) {
    let (ux, uy, _, _) = btn_up_pos();
    (ux, uy + (BTN_H + BTN_GAP) * 2, BTN_W, BTN_H)
}

/// リセットボタン
fn btn_reset_pos() -> (u32, u32, u32, u32) {
    let (_, dy, _, _) = btn_down_pos();
    (side_x() + 6, dy + BTN_H + 16, BTN_WIDE, BTN_H)
}

fn hit_btn(mx: i32, my: i32, pos: (u32, u32, u32, u32)) -> bool {
    if mx < 0 || my < 0 {
        return false;
    }
    let (bx, by, bw, bh) = pos;
    let x = mx as u32;
    let y = my as u32;
    x >= bx && x < bx + bw && y >= by && y < by + bh
}

struct XorShift32 {
    state: u32,
}

impl XorShift32 {
    fn new(seed: u32) -> Self {
        Self { state: if seed == 0 { 0xBEEF_CAFE } else { seed } }
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

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
