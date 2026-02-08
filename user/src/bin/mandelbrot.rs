// mandelbrot.rs — マンデルブロ集合レンダラー（GUI user space アプリ）
//
// マンデルブロ集合: 複素数 c に対して z_{n+1} = z_n^2 + c を反復し、
// |z| が発散しなければ集合に属する。発散までの反復回数に応じて色を付けると
// フラクタル（自己相似形）の美しいパターンが現れる。
//
// 実装:
// - no_std 環境のため、浮動小数点の代わりに 32.32 固定小数点演算を使用
// - GUI ウィンドウにピクセル単位で描画（window_rect の 1x1 矩形）
// - マウスクリックでズームイン、右パネルのボタンでリセット・ズームアウト

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

// 描画領域のピクセルサイズ
const VIEW_W: u32 = 256;
const VIEW_H: u32 = 192;

// レイアウト
const PAD: u32 = 8;
const TITLE_H: u32 = 28;
const SIDE_W: u32 = 100;
const GAP: u32 = 8;

// ボタン
const BTN_W: u32 = 88;
const BTN_H: u32 = 24;
const BTN_GAP: u32 = 6;

// マンデルブロ計算パラメータ
const MAX_ITER: u32 = 64;

// カラーテーマ
const BG: (u8, u8, u8) = (18, 22, 32);
const PANEL: (u8, u8, u8) = (24, 28, 44);
const BORDER: (u8, u8, u8) = (80, 120, 200);
const TEXT_FG: (u8, u8, u8) = (220, 240, 255);
const TEXT_ACCENT: (u8, u8, u8) = (255, 220, 120);

// 描画ティック（ミリ秒）
const TICK_MS: u64 = 50;

// ========== 固定小数点演算 (32.32 format) ==========
//
// i64 の上位 32 ビットが整数部、下位 32 ビットが小数部。
// 例: 1.0 = 0x0000_0001_0000_0000
//     2.5 = 0x0000_0002_8000_0000
//    -1.0 = 0xFFFF_FFFF_0000_0000
//
// 乗算は 64x64 → 128 ビットが必要なため、中間値を i128 で計算する。

type Fixed = i64;
const FRAC_BITS: u32 = 32;
const FOUR: Fixed = 4i64 << FRAC_BITS; // |z|^2 の発散閾値

/// 2つの固定小数点数を乗算
fn fmul(a: Fixed, b: Fixed) -> Fixed {
    ((a as i128 * b as i128) >> FRAC_BITS) as Fixed
}

/// 浮動小数点的な値を固定小数点で表現するためのヘルパー
/// num / den を固定小数点に変換
fn frac(num: i32, den: i32) -> Fixed {
    ((num as i64) << FRAC_BITS) / den as i64
}

// ========== マンデルブロ計算 ==========

/// ビュー範囲（複素平面上の矩形）
#[derive(Clone, Copy)]
struct ViewRange {
    // 複素平面上の左上 (cx_min, cy_min) と右下 (cx_max, cy_max)
    cx_min: Fixed,
    cy_min: Fixed,
    cx_max: Fixed,
    cy_max: Fixed,
}

impl ViewRange {
    /// デフォルトのビュー（マンデルブロ集合全体が見える範囲）
    fn default() -> Self {
        Self {
            cx_min: frac(-250, 100), // -2.5
            cy_min: frac(-125, 100), // -1.25
            cx_max: frac(100, 100),  //  1.0
            cy_max: frac(125, 100),  //  1.25
        }
    }

    /// ピクセル座標を複素数に変換
    fn pixel_to_complex(&self, px: u32, py: u32) -> (Fixed, Fixed) {
        let cx = self.cx_min + fmul(self.cx_max - self.cx_min, frac(px as i32, VIEW_W as i32));
        let cy = self.cy_min + fmul(self.cy_max - self.cy_min, frac(py as i32, VIEW_H as i32));
        (cx, cy)
    }

    /// 指定座標を中心にズームイン（倍率 2x）
    fn zoom_in(&self, center_px: u32, center_py: u32) -> Self {
        let (cx, cy) = self.pixel_to_complex(center_px, center_py);
        let half_w = (self.cx_max - self.cx_min) / 4; // 幅を半分に
        let half_h = (self.cy_max - self.cy_min) / 4;
        Self {
            cx_min: cx - half_w,
            cy_min: cy - half_h,
            cx_max: cx + half_w,
            cy_max: cy + half_h,
        }
    }

    /// ズームアウト（倍率 0.5x、中心を維持）
    fn zoom_out(&self) -> Self {
        let cx = (self.cx_min + self.cx_max) / 2;
        let cy = (self.cy_min + self.cy_max) / 2;
        let half_w = self.cx_max - self.cx_min; // 幅を 2 倍に
        let half_h = self.cy_max - self.cy_min;
        Self {
            cx_min: cx - half_w,
            cy_min: cy - half_h,
            cx_max: cx + half_w,
            cy_max: cy + half_h,
        }
    }
}

/// マンデルブロ集合の反復回数を計算する
///
/// z_{n+1} = z_n^2 + c  を反復し、|z|^2 > 4 になったら発散とみなす。
/// 返り値: 発散までの反復回数。MAX_ITER に達したら集合の内部。
fn mandelbrot_iter(cr: Fixed, ci: Fixed) -> u32 {
    let mut zr: Fixed = 0;
    let mut zi: Fixed = 0;
    for i in 0..MAX_ITER {
        let zr2 = fmul(zr, zr);
        let zi2 = fmul(zi, zi);
        if zr2 + zi2 > FOUR {
            return i;
        }
        // z = z^2 + c
        // (zr + zi*i)^2 = zr^2 - zi^2 + 2*zr*zi*i
        let new_zr = zr2 - zi2 + cr;
        let new_zi = 2 * fmul(zr, zi) + ci;
        zr = new_zr;
        zi = new_zi;
    }
    MAX_ITER
}

/// 反復回数からカラーを計算（スムースなグラデーション）
fn iter_to_color(iter: u32) -> (u8, u8, u8) {
    if iter == MAX_ITER {
        // 集合の内部は黒
        return (0, 0, 0);
    }
    // HSV 的なカラーマッピング: 反復回数を色相に変換
    // 複数の色帯を滑らかに遷移させる
    let t = (iter * 6) % 256;
    let phase = (iter * 6 / 256) % 6;
    let t = t as u8;
    match phase {
        0 => (t, 0, 128u8.saturating_sub(t / 2)),         // 黒→赤
        1 => (255, t, 0),                                   // 赤→黄
        2 => (255u8.saturating_sub(t), 255, t),             // 黄→シアン
        3 => (0, 255u8.saturating_sub(t), 255),             // シアン→青
        4 => (t / 2, 0, 255u8.saturating_sub(t)),           // 青→暗紫
        _ => (128u8.saturating_sub(t / 2), t / 3, t / 2),  // 暗紫→暗緑
    }
}

// ========== メインループ ==========

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    app_main();
}

fn app_main() -> ! {
    let mut gui = gui_client::GuiClient::new();

    // ウィンドウサイズ計算
    let win_w = PAD + VIEW_W + GAP + SIDE_W + PAD + 4;
    let win_h = TITLE_H + VIEW_H + PAD + 28 + 4;

    let win_id = match gui.window_create(win_w, win_h, "MANDELBROT") {
        Ok(id) => id,
        Err(_) => syscall::exit(),
    };

    let mut view = ViewRange::default();
    let mut zoom_level: u32 = 0;
    // ピクセルバッファ: 各ピクセルの反復回数を保持（再描画の高速化）
    // 256*192 = 49152 要素
    let mut iter_buf = [0u32; (VIEW_W * VIEW_H) as usize];

    let mut last_seq: u32 = 0;
    let mut last_buttons: u8 = 0;

    // 段階的レンダリング: 一度に全ピクセルを描画すると時間がかかるため、
    // 数行ずつ描画して present する（プログレッシブ表示）
    let mut render_row: u32;
    let mut rendering;
    let mut needs_render;

    // 初回は UI フレームを先に描画
    draw_frame(&mut gui, win_id, &view, zoom_level);
    let _ = gui.window_present(win_id);

    // レンダリング開始
    needs_render = false;
    rendering = true;
    render_row = 0;
    compute_rows(&view, &mut iter_buf, 0, VIEW_H);

    loop {
        // マウス入力処理
        if let Ok(mouse) = gui.window_mouse_state(win_id) {
            if mouse.seq != last_seq {
                let left_now = (mouse.buttons & 0x1) != 0;
                let left_prev = (last_buttons & 0x1) != 0;
                last_seq = mouse.seq;
                last_buttons = mouse.buttons;

                if mouse.inside && left_now && !left_prev {
                    // ビュー領域上のクリック → ズームイン
                    let vx = mouse.x - PAD as i32;
                    let vy = mouse.y - TITLE_H as i32;
                    if vx >= 0 && vy >= 0 && (vx as u32) < VIEW_W && (vy as u32) < VIEW_H {
                        view = view.zoom_in(vx as u32, vy as u32);
                        zoom_level += 1;
                        needs_render = true;
                    }

                    // ボタン判定
                    if hit_btn(mouse.x, mouse.y, btn_reset_pos()) {
                        view = ViewRange::default();
                        zoom_level = 0;
                        needs_render = true;
                    } else if hit_btn(mouse.x, mouse.y, btn_zoomout_pos()) {
                        view = view.zoom_out();
                        if zoom_level > 0 {
                            zoom_level -= 1;
                        }
                        needs_render = true;
                    }
                }
            }
        }

        // 新しいレンダリングが必要な場合
        if needs_render {
            needs_render = false;
            rendering = true;
            render_row = 0;
            // 全行を一括計算
            compute_rows(&view, &mut iter_buf, 0, VIEW_H);
            // UI フレームを描画
            draw_frame(&mut gui, win_id, &view, zoom_level);
        }

        // 段階的ピクセル描画
        if rendering {
            // 1ティックあたり 16 行ずつ描画
            let rows_per_tick = 16u32;
            let end_row = (render_row + rows_per_tick).min(VIEW_H);
            draw_pixels(&mut gui, win_id, &iter_buf, render_row, end_row);
            render_row = end_row;

            // サイドパネル更新（進捗表示）
            draw_side(&mut gui, win_id, &view, zoom_level, Some(render_row));

            let _ = gui.window_present(win_id);

            if render_row >= VIEW_H {
                rendering = false;
                // 最終描画
                draw_side(&mut gui, win_id, &view, zoom_level, None);
                let _ = gui.window_present(win_id);
            }
        }

        syscall::sleep(TICK_MS);
    }
}

/// 指定範囲の行について反復回数を計算
fn compute_rows(view: &ViewRange, buf: &mut [u32], start: u32, end: u32) {
    for y in start..end {
        for x in 0..VIEW_W {
            let (cr, ci) = view.pixel_to_complex(x, y);
            let iter = mandelbrot_iter(cr, ci);
            buf[(y * VIEW_W + x) as usize] = iter;
        }
    }
}

/// ピクセルを描画（指定行範囲）
fn draw_pixels(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    buf: &[u32],
    start_row: u32,
    end_row: u32,
) {
    let view_x0 = PAD;
    let view_y0 = TITLE_H;

    // 同じ色のピクセルを水平方向に連結して矩形としてまとめて描画する。
    // 1 ピクセルずつ IPC するとオーバーヘッドが大きいため、
    // 同色の連続ピクセルを 1 回の window_rect 呼び出しで描画する。
    for y in start_row..end_row {
        let mut x = 0u32;
        while x < VIEW_W {
            let iter = buf[(y * VIEW_W + x) as usize];
            let (r, g, b) = iter_to_color(iter);

            // 同じ色が続く範囲を探す
            let mut run = 1u32;
            while x + run < VIEW_W {
                let next_iter = buf[(y * VIEW_W + x + run) as usize];
                if next_iter != iter {
                    break;
                }
                run += 1;
            }

            let px = view_x0 + x;
            let py = view_y0 + y;
            let _ = gui.window_rect(win_id, px, py, run, 1, r, g, b);
            x += run;
        }
    }
}

/// UI フレーム（タイトルバー + ビュー枠 + サイドパネル背景）を描画
fn draw_frame(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    view: &ViewRange,
    zoom_level: u32,
) {
    let _ = gui.window_clear(win_id, BG.0, BG.1, BG.2);

    // タイトルバー
    let inner_w = PAD + VIEW_W + GAP + SIDE_W;
    let _ = gui.window_rect(win_id, 2, 2, inner_w, TITLE_H, BORDER.0, BORDER.1, BORDER.2);
    let _ = gui.window_rect(win_id, 4, 4, inner_w - 4, TITLE_H - 4, PANEL.0, PANEL.1, PANEL.2);
    let _ = gui.window_text(win_id, 8, 8, TEXT_ACCENT, PANEL, "MANDELBROT SET");

    // ビュー枠
    let _ = gui.window_rect(
        win_id, PAD - 2, TITLE_H - 2,
        VIEW_W + 4, VIEW_H + 4,
        BORDER.0, BORDER.1, BORDER.2,
    );

    // サイドパネル
    draw_side(gui, win_id, view, zoom_level, Some(0));
}

/// サイドパネルの描画
fn draw_side(
    gui: &mut gui_client::GuiClient,
    win_id: gui_client::WindowId,
    _view: &ViewRange,
    zoom_level: u32,
    progress: Option<u32>,
) {
    let side_x = PAD + VIEW_W + GAP;
    let side_y = TITLE_H;

    // パネル背景
    let _ = gui.window_rect(win_id, side_x, side_y, SIDE_W, VIEW_H, PANEL.0, PANEL.1, PANEL.2);

    // ズームレベル
    let zoom_text = format!("Zoom: {}x", 1u64 << zoom_level);
    let _ = gui.window_text(win_id, side_x + 6, side_y + 8, TEXT_ACCENT, PANEL, &zoom_text);

    // 反復回数
    let iter_text = format!("MaxIter:{}", MAX_ITER);
    let _ = gui.window_text(win_id, side_x + 6, side_y + 24, TEXT_FG, PANEL, &iter_text);

    // 進捗表示
    match progress {
        Some(row) if row < VIEW_H => {
            let pct = row * 100 / VIEW_H;
            let pct_text = format!("Render:{}%", pct);
            let _ = gui.window_text(win_id, side_x + 6, side_y + 40, (80, 220, 120), PANEL, &pct_text);
        }
        _ => {
            let _ = gui.window_text(win_id, side_x + 6, side_y + 40, (80, 220, 120), PANEL, "Done!");
        }
    }

    // 操作説明
    let _ = gui.window_text(win_id, side_x + 6, side_y + 60, TEXT_FG, PANEL, "Click to");
    let _ = gui.window_text(win_id, side_x + 6, side_y + 76, TEXT_FG, PANEL, "zoom in");

    // ボタン
    draw_button(gui, win_id, btn_zoomout_pos(), "ZOOM OUT");
    draw_button(gui, win_id, btn_reset_pos(), "RESET");
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

fn side_x() -> u32 {
    PAD + VIEW_W + GAP
}

fn btn_zoomout_pos() -> (u32, u32, u32, u32) {
    (side_x() + 6, TITLE_H + 100, BTN_W, BTN_H)
}

fn btn_reset_pos() -> (u32, u32, u32, u32) {
    let (x, y, _, _) = btn_zoomout_pos();
    (x, y + BTN_H + BTN_GAP, BTN_W, BTN_H)
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

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
