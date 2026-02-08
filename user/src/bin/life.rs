// life.rs — Conway's Game of Life（GUI user space アプリ）
//
// ライフゲーム: セルの生死が近傍8マスの生存数で決まるセルオートマトン。
// ルール:
//   - 誕生: 死んだセルの周囲にちょうど 3 つの生きたセルがあれば、次世代で誕生する
//   - 生存: 生きたセルの周囲に 2 つまたは 3 つの生きたセルがあれば生存する
//   - 過疎/過密: それ以外は死亡する
//
// マウスでセルをクリックしてパターンを配置し、PLAY で進行を観察する。

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

// グリッドサイズ: 48x36 セル
const GRID_W: usize = 48;
const GRID_H: usize = 36;

// 1セルのピクセルサイズ
const CELL_SIZE: u32 = 8;

// レイアウト定数
const PAD: u32 = 8;
const TITLE_H: u32 = 28;
const SIDE_W: u32 = 100;
const GAP: u32 = 8;

// ボタンサイズ
const BTN_W: u32 = 88;
const BTN_H: u32 = 24;
const BTN_GAP: u32 = 6;

// シミュレーション速度（ミリ秒/世代）
const SIM_INTERVAL_MS: u64 = 150;
// 描画ループのティック間隔
const TICK_MS: u64 = 30;

// カラーテーマ（テトリスと統一）
const BG: (u8, u8, u8) = (18, 22, 32);
const PANEL: (u8, u8, u8) = (24, 28, 44);
const BORDER: (u8, u8, u8) = (80, 120, 200);
const TEXT_FG: (u8, u8, u8) = (220, 240, 255);
const TEXT_ACCENT: (u8, u8, u8) = (255, 220, 120);
const CELL_ALIVE: (u8, u8, u8) = (80, 220, 120);
const CELL_DEAD: (u8, u8, u8) = (30, 34, 50);
const GRID_LINE: (u8, u8, u8) = (40, 46, 64);

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    app_main();
}

fn app_main() -> ! {
    let mut gui = gui_client::GuiClient::new();

    let grid_px_w = GRID_W as u32 * CELL_SIZE;
    let grid_px_h = GRID_H as u32 * CELL_SIZE;
    // ウィンドウサイズ: パディング + グリッド + ギャップ + サイドパネル + パディング
    // 高さ: タイトル + グリッド + パディング
    // ウィンドウ装飾分として +4（左右2px）と +28+4（タイトルバー+下部）を加算
    let win_w = PAD + grid_px_w + GAP + SIDE_W + PAD + 4;
    let win_h = TITLE_H + grid_px_h + PAD + 28 + 4;

    let win_id = match gui.window_create(win_w, win_h, "GAME OF LIFE") {
        Ok(id) => id,
        Err(_) => syscall::exit(),
    };

    // グリッドのセル状態（ダブルバッファ）
    let mut grid = [[false; GRID_W]; GRID_H];
    let mut grid_next = [[false; GRID_W]; GRID_H];

    // Glider Gun パターンをプリセットとして配置
    place_glider_gun(&mut grid, 1, 1);

    let mut playing = false;
    let mut generation: u64 = 0;
    let mut sim_acc: u64 = 0;

    // マウス状態の追跡
    let mut last_seq: u32 = 0;
    let mut last_buttons: u8 = 0;

    // 描画
    draw_all(&mut gui, win_id, &grid, playing, generation);

    loop {
        // マウス入力処理
        if let Ok(mouse) = gui.window_mouse_state(win_id) {
            if mouse.seq != last_seq {
                let left_now = (mouse.buttons & 0x1) != 0;
                let left_prev = (last_buttons & 0x1) != 0;
                last_seq = mouse.seq;
                last_buttons = mouse.buttons;

                if mouse.inside && left_now && !left_prev {
                    // グリッド上のクリック判定
                    let grid_x0 = PAD as i32;
                    let grid_y0 = TITLE_H as i32;
                    let gx = mouse.x - grid_x0;
                    let gy = mouse.y - grid_y0;
                    if gx >= 0 && gy >= 0 {
                        let cx = gx as usize / CELL_SIZE as usize;
                        let cy = gy as usize / CELL_SIZE as usize;
                        if cx < GRID_W && cy < GRID_H {
                            // セルのトグル
                            grid[cy][cx] = !grid[cy][cx];
                        }
                    }

                    // ボタン判定
                    if hit_btn(mouse.x, mouse.y, btn_play_pos()) {
                        playing = !playing;
                        sim_acc = 0;
                    } else if hit_btn(mouse.x, mouse.y, btn_step_pos()) {
                        // 1世代だけ進める
                        step(&grid, &mut grid_next);
                        copy_grid(&mut grid, &grid_next);
                        generation += 1;
                        playing = false;
                    } else if hit_btn(mouse.x, mouse.y, btn_clear_pos()) {
                        clear_grid(&mut grid);
                        generation = 0;
                        playing = false;
                        sim_acc = 0;
                    } else if hit_btn(mouse.x, mouse.y, btn_random_pos()) {
                        randomize(&mut grid, syscall::getpid() as u32 ^ (generation as u32));
                        generation = 0;
                        playing = false;
                        sim_acc = 0;
                    } else if hit_btn(mouse.x, mouse.y, btn_glider_pos()) {
                        clear_grid(&mut grid);
                        place_glider_gun(&mut grid, 1, 1);
                        generation = 0;
                        playing = false;
                        sim_acc = 0;
                    }
                }
            }
        }

        // シミュレーション進行
        if playing {
            sim_acc += TICK_MS;
            if sim_acc >= SIM_INTERVAL_MS {
                sim_acc = 0;
                step(&grid, &mut grid_next);
                copy_grid(&mut grid, &grid_next);
                generation += 1;
            }
        }

        draw_all(&mut gui, win_id, &grid, playing, generation);
        syscall::sleep(TICK_MS);
    }
}

/// グリッドを 1 世代進める（Conway のルール）
fn step(current: &[[bool; GRID_W]; GRID_H], next: &mut [[bool; GRID_W]; GRID_H]) {
    for y in 0..GRID_H {
        for x in 0..GRID_W {
            let neighbors = count_neighbors(current, x, y);
            next[y][x] = if current[y][x] {
                // 生きたセル: 2 or 3 で生存、それ以外は死亡
                neighbors == 2 || neighbors == 3
            } else {
                // 死んだセル: ちょうど 3 で誕生
                neighbors == 3
            };
        }
    }
}

/// 近傍 8 マスの生存セル数をカウント
fn count_neighbors(grid: &[[bool; GRID_W]; GRID_H], x: usize, y: usize) -> u8 {
    let mut count = 0u8;
    for dy in [-1i32, 0, 1] {
        for dx in [-1i32, 0, 1] {
            if dx == 0 && dy == 0 {
                continue;
            }
            let nx = x as i32 + dx;
            let ny = y as i32 + dy;
            if nx >= 0 && nx < GRID_W as i32 && ny >= 0 && ny < GRID_H as i32 {
                if grid[ny as usize][nx as usize] {
                    count += 1;
                }
            }
        }
    }
    count
}

/// grid_next の内容を grid にコピー
fn copy_grid(dst: &mut [[bool; GRID_W]; GRID_H], src: &[[bool; GRID_W]; GRID_H]) {
    for y in 0..GRID_H {
        for x in 0..GRID_W {
            dst[y][x] = src[y][x];
        }
    }
}

/// グリッドを全消去
fn clear_grid(grid: &mut [[bool; GRID_W]; GRID_H]) {
    for row in grid.iter_mut() {
        for cell in row.iter_mut() {
            *cell = false;
        }
    }
}

/// ランダムにセルを配置する（XorShift32 疑似乱数）
fn randomize(grid: &mut [[bool; GRID_W]; GRID_H], seed: u32) {
    let mut rng = XorShift32::new(seed);
    for row in grid.iter_mut() {
        for cell in row.iter_mut() {
            // 約 25% の確率で生存
            *cell = (rng.next() % 4) == 0;
        }
    }
}

/// Gosper's Glider Gun を配置する
/// ライフゲームの代表的パターン: グライダーを無限に生成し続ける
fn place_glider_gun(grid: &mut [[bool; GRID_W]; GRID_H], ox: usize, oy: usize) {
    // Gosper's Glider Gun のセル座標（36x9）
    const PATTERN: [(usize, usize); 36] = [
        (24, 0),
        (22, 1), (24, 1),
        (12, 2), (13, 2), (20, 2), (21, 2), (34, 2), (35, 2),
        (11, 3), (15, 3), (20, 3), (21, 3), (34, 3), (35, 3),
        (0, 4), (1, 4), (10, 4), (16, 4), (20, 4), (21, 4),
        (0, 5), (1, 5), (10, 5), (14, 5), (16, 5), (17, 5), (22, 5), (24, 5),
        (10, 6), (16, 6), (24, 6),
        (11, 7), (15, 7),
        (12, 8), (13, 8),
    ];
    for &(px, py) in &PATTERN {
        let x = ox + px;
        let y = oy + py;
        if x < GRID_W && y < GRID_H {
            grid[y][x] = true;
        }
    }
}

/// 全画面を描画
fn draw_all(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    grid: &[[bool; GRID_W]; GRID_H],
    playing: bool,
    generation: u64,
) {
    let _ = gui.window_clear(win_id, BG.0, BG.1, BG.2);

    // タイトルバー
    let grid_px_w = GRID_W as u32 * CELL_SIZE;
    let inner_w = PAD + grid_px_w + GAP + SIDE_W;
    let _ = gui.window_rect(win_id, 2, 2, inner_w, TITLE_H, BORDER.0, BORDER.1, BORDER.2);
    let _ = gui.window_rect(win_id, 4, 4, inner_w - 4, TITLE_H - 4, PANEL.0, PANEL.1, PANEL.2);
    let _ = gui.window_text(win_id, 8, 8, TEXT_ACCENT, PANEL, "GAME OF LIFE");

    // グリッド描画
    draw_grid(gui, win_id, grid);

    // サイドパネル描画
    draw_side(gui, win_id, playing, generation, grid);

    let _ = gui.window_present(win_id);
}

/// グリッド部分を描画
fn draw_grid(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    grid: &[[bool; GRID_W]; GRID_H],
) {
    let grid_x0 = PAD;
    let grid_y0 = TITLE_H;
    let grid_px_w = GRID_W as u32 * CELL_SIZE;
    let grid_px_h = GRID_H as u32 * CELL_SIZE;

    // グリッド背景（枠線）
    let _ = gui.window_rect(
        win_id, grid_x0 - 2, grid_y0 - 2,
        grid_px_w + 4, grid_px_h + 4,
        BORDER.0, BORDER.1, BORDER.2,
    );
    let _ = gui.window_rect(
        win_id, grid_x0, grid_y0,
        grid_px_w, grid_px_h,
        GRID_LINE.0, GRID_LINE.1, GRID_LINE.2,
    );

    // セルを描画（生きたセルだけ明るくする）
    for y in 0..GRID_H {
        for x in 0..GRID_W {
            let px = grid_x0 + (x as u32) * CELL_SIZE;
            let py = grid_y0 + (y as u32) * CELL_SIZE;
            let (r, g, b) = if grid[y][x] { CELL_ALIVE } else { CELL_DEAD };
            // 内側を 1px 小さく描画してグリッド線を見せる
            let _ = gui.window_rect(
                win_id, px + 1, py + 1,
                CELL_SIZE - 1, CELL_SIZE - 1,
                r, g, b,
            );
        }
    }
}

/// サイドパネルを描画
fn draw_side(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    playing: bool,
    generation: u64,
    grid: &[[bool; GRID_W]; GRID_H],
) {
    let grid_px_w = GRID_W as u32 * CELL_SIZE;
    let side_x = PAD + grid_px_w + GAP;
    let side_y = TITLE_H;
    let grid_px_h = GRID_H as u32 * CELL_SIZE;

    // パネル背景
    let _ = gui.window_rect(
        win_id, side_x, side_y,
        SIDE_W, grid_px_h,
        PANEL.0, PANEL.1, PANEL.2,
    );

    // 世代数表示
    let gen_text = format!("Gen: {}", generation);
    let _ = gui.window_text(win_id, side_x + 6, side_y + 8, TEXT_ACCENT, PANEL, &gen_text);

    // 生存セル数表示
    let alive = count_alive(grid);
    let alive_text = format!("Alive: {}", alive);
    let _ = gui.window_text(win_id, side_x + 6, side_y + 24, TEXT_FG, PANEL, &alive_text);

    // 状態表示
    let status = if playing { "RUNNING" } else { "PAUSED" };
    let status_color = if playing { (80, 220, 120) } else { (220, 180, 80) };
    let _ = gui.window_text(win_id, side_x + 6, side_y + 44, status_color, PANEL, status);

    // ボタン描画
    let play_label = if playing { "PAUSE" } else { "PLAY" };
    draw_button(gui, win_id, btn_play_pos(), play_label);
    draw_button(gui, win_id, btn_step_pos(), "STEP");
    draw_button(gui, win_id, btn_clear_pos(), "CLEAR");
    draw_button(gui, win_id, btn_random_pos(), "RANDOM");
    draw_button(gui, win_id, btn_glider_pos(), "GLIDER");
}

/// 生存セル数をカウント
fn count_alive(grid: &[[bool; GRID_W]; GRID_H]) -> u32 {
    let mut count = 0u32;
    for row in grid.iter() {
        for &cell in row.iter() {
            if cell {
                count += 1;
            }
        }
    }
    count
}

/// ボタン描画
fn draw_button(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    pos: (u32, u32, u32, u32),
    label: &str,
) {
    let (x, y, w, h) = pos;
    let _ = gui.window_rect(win_id, x, y, w, h, BORDER.0, BORDER.1, BORDER.2);
    let _ = gui.window_rect(win_id, x + 2, y + 2, w - 4, h - 4, PANEL.0, PANEL.1, PANEL.2);
    let _ = gui.window_text(win_id, x + 6, y + 6, TEXT_FG, PANEL, label);
}

// --- ボタン位置の定義 ---

fn side_x() -> u32 {
    PAD + GRID_W as u32 * CELL_SIZE + GAP
}

fn btn_play_pos() -> (u32, u32, u32, u32) {
    (side_x() + 6, TITLE_H + 68, BTN_W, BTN_H)
}

fn btn_step_pos() -> (u32, u32, u32, u32) {
    let (x, y, _, _) = btn_play_pos();
    (x, y + BTN_H + BTN_GAP, BTN_W, BTN_H)
}

fn btn_clear_pos() -> (u32, u32, u32, u32) {
    let (x, y, _, _) = btn_step_pos();
    (x, y + BTN_H + BTN_GAP, BTN_W, BTN_H)
}

fn btn_random_pos() -> (u32, u32, u32, u32) {
    let (x, y, _, _) = btn_clear_pos();
    (x, y + BTN_H + BTN_GAP, BTN_W, BTN_H)
}

fn btn_glider_pos() -> (u32, u32, u32, u32) {
    let (x, y, _, _) = btn_random_pos();
    (x, y + BTN_H + BTN_GAP, BTN_W, BTN_H)
}

/// ボタンヒット判定
fn hit_btn(mx: i32, my: i32, pos: (u32, u32, u32, u32)) -> bool {
    if mx < 0 || my < 0 {
        return false;
    }
    let (bx, by, bw, bh) = pos;
    let x = mx as u32;
    let y = my as u32;
    x >= bx && x < bx + bw && y >= by && y < by + bh
}

/// XorShift32 疑似乱数生成器
struct XorShift32 {
    state: u32,
}

impl XorShift32 {
    fn new(seed: u32) -> Self {
        let seed = if seed == 0 { 0xDEAD_BEEF } else { seed };
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

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
