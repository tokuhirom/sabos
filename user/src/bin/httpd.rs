// httpd.rs — SABOS 簡易 HTTP サーバー（user space）
//
// HTTP/1.1 だが keep-alive は使わず、1リクエストで接続を閉じる。

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

#[path = "../allocator.rs"]
mod allocator;
#[path = "../json.rs"]
mod json;
#[path = "../net.rs"]
mod net;
#[path = "../print.rs"]
mod print;
#[path = "../syscall.rs"]
mod syscall;

use alloc::string::String;
use alloc::vec::Vec;
use core::panic::PanicInfo;

const HTTP_PORT: u16 = 8080;
const FILE_BUFFER_SIZE: usize = 4096;
const MAX_REQUEST_SIZE: usize = 4096;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    allocator::init();
    httpd_main();
}

fn httpd_main() -> ! {
    loop {
        // リッスン開始
        let listener = match net::TcpListener::bind(HTTP_PORT) {
            Ok(l) => l,
            Err(_) => {
                syscall::sleep(500);
                continue;
            }
        };

        // 接続を受け付けるループ
        loop {
            match listener.accept() {
                Ok(stream) => {
                    handle_connection(stream);
                }
                Err(_) => {
                    syscall::sleep(10);
                }
            }
        }
    }
}

/// HTTP 接続を処理する
///
/// TcpStream を受け取り、HTTP リクエストを読み込んでレスポンスを返す。
/// stream は関数終了時に Drop で自動クローズされる。
fn handle_connection(stream: net::TcpStream) {
    let req = match read_http_request(&stream) {
        Ok(v) => v,
        Err(_) => {
            send_simple_response(&stream, 400, "Bad Request", "bad request\n");
            return;
        }
    };

    let req_text = match core::str::from_utf8(&req) {
        Ok(v) => v,
        Err(_) => {
            send_simple_response(&stream, 400, "Bad Request", "bad request\n");
            return;
        }
    };

    let mut lines = req_text.split("\r\n");
    let first = match lines.next() {
        Some(v) => v,
        None => {
            send_simple_response(&stream, 400, "Bad Request", "bad request\n");
            return;
        }
    };

    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");

    if method != "GET" {
        send_simple_response(&stream, 405, "Method Not Allowed", "method not allowed\n");
        return;
    }

    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        send_simple_response(&stream, 400, "Bad Request", "bad request\n");
        return;
    }

    if !path.starts_with('/') || path.contains("..") {
        send_simple_response(&stream, 400, "Bad Request", "bad request\n");
        return;
    }

    // パスが "/" で終わるか "/" そのものならディレクトリとして扱う
    if path == "/" || path.ends_with('/') {
        let dir_path = if path == "/" { "/" } else { &path[..path.len() - 1] };
        match list_directory(dir_path, path) {
            Ok(html) => {
                send_html_response(&stream, 200, "OK", &html);
            }
            Err(_) => {
                send_simple_response(&stream, 404, "Not Found", "not found\n");
            }
        }
        return;
    }

    // まずファイルとして開いてみる
    let data = match read_file(path) {
        Ok(v) => v,
        Err(_) => {
            // ファイルが見つからなければディレクトリとして試す
            match list_directory(path, path) {
                Ok(html) => {
                    send_html_response(&stream, 200, "OK", &html);
                    return;
                }
                Err(_) => {
                    send_simple_response(&stream, 404, "Not Found", "not found\n");
                    return;
                }
            }
        }
    };

    let content_type = guess_content_type(path);

    let mut header = String::new();
    header.push_str("HTTP/1.1 200 OK\r\n");
    header.push_str("Content-Type: ");
    header.push_str(content_type);
    header.push_str("\r\n");
    header.push_str("Content-Length: ");
    header.push_str(&itoa(data.len() as u64));
    header.push_str("\r\n");
    header.push_str("Connection: close\r\n\r\n");

    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&data);
    // stream は Drop で自動クローズ
}

/// HTTP リクエストを読み込む（ヘッダ終端 \r\n\r\n まで）
fn read_http_request(stream: &net::TcpStream) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 256];
    loop {
        let n = stream.read(&mut tmp).map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_REQUEST_SIZE {
            return Err(());
        }
    }
    Ok(buf)
}

fn read_file(path: &str) -> Result<Vec<u8>, ()> {
    let handle = syscall::open(path, syscall::HANDLE_RIGHTS_FILE_READ).map_err(|_| ())?;
    let mut out = Vec::new();
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    loop {
        let n = syscall::handle_read(&handle, &mut buf);
        if n < 0 {
            let _ = syscall::handle_close(&handle);
            return Err(());
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    let _ = syscall::handle_close(&handle);
    Ok(out)
}

/// ディレクトリの内容を HTML で返す
///
/// handle_enum で取得したエントリ名（改行区切り）を HTML のリンク一覧に変換する。
/// 各エントリはクリックでアクセスできるリンクになる。
fn list_directory(dir_path: &str, display_path: &str) -> Result<String, ()> {
    let handle = syscall::open(dir_path, syscall::HANDLE_RIGHTS_DIRECTORY_READ).map_err(|_| ())?;
    let mut buf = [0u8; FILE_BUFFER_SIZE];
    let n = syscall::handle_enum(&handle, &mut buf);
    let _ = syscall::handle_close(&handle);
    if n < 0 {
        return Err(());
    }

    let entries_text = if n > 0 {
        core::str::from_utf8(&buf[..n as usize]).map_err(|_| ())?
    } else {
        ""
    };

    // HTML を構築
    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\n");
    html.push_str("<title>Index of ");
    html.push_str(display_path);
    html.push_str("</title>\n");
    // 簡単なスタイル
    html.push_str("<style>body{font-family:monospace;margin:2em}a{text-decoration:none}a:hover{text-decoration:underline}li{margin:0.3em 0}</style>\n");
    html.push_str("</head><body>\n");
    html.push_str("<h1>Index of ");
    html.push_str(display_path);
    html.push_str("</h1>\n<hr>\n<ul>\n");

    // 親ディレクトリへのリンク（ルート以外）
    if dir_path != "/" {
        html.push_str("<li><a href=\"");
        // 親パスを計算
        let parent = parent_path(display_path);
        html.push_str(&parent);
        html.push_str("\">..</a></li>\n");
    }

    // エントリをリンクとして追加
    for entry in entries_text.split('\n') {
        let name = entry.trim();
        if name.is_empty() {
            continue;
        }
        html.push_str("<li><a href=\"");
        // リンク先のパスを構築
        if display_path.ends_with('/') {
            html.push_str(display_path);
        } else {
            html.push_str(display_path);
            html.push('/');
        }
        html.push_str(name);
        html.push_str("\">");
        html.push_str(name);
        html.push_str("</a></li>\n");
    }

    html.push_str("</ul>\n<hr>\n<p><em>SABOS httpd</em></p>\n</body></html>\n");
    Ok(html)
}

/// 親パスを返す（"/" なら "/" のまま）
fn parent_path(path: &str) -> String {
    // 末尾の "/" を除去してから最後の "/" を探す
    let trimmed = if path.ends_with('/') && path.len() > 1 {
        &path[..path.len() - 1]
    } else {
        path
    };
    match trimmed.rfind('/') {
        Some(0) => String::from("/"),
        Some(pos) => String::from(&trimmed[..pos + 1]),
        None => String::from("/"),
    }
}

/// Content-Type を推測する
fn guess_content_type(path: &str) -> &'static str {
    if path.ends_with(".HTML") || path.ends_with(".HTM")
        || path.ends_with(".html") || path.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".JSON") || path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".ELF") || path.ends_with(".elf") {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// HTML レスポンスを送信する
fn send_html_response(stream: &net::TcpStream, code: u32, reason: &str, body: &str) {
    let mut header = String::new();
    header.push_str("HTTP/1.1 ");
    header.push_str(&itoa(code as u64));
    header.push(' ');
    header.push_str(reason);
    header.push_str("\r\n");
    header.push_str("Content-Type: text/html; charset=utf-8\r\n");
    header.push_str("Content-Length: ");
    header.push_str(&itoa(body.as_bytes().len() as u64));
    header.push_str("\r\n");
    header.push_str("Connection: close\r\n\r\n");

    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn send_simple_response(stream: &net::TcpStream, code: u32, reason: &str, body: &str) {
    let mut header = String::new();
    header.push_str("HTTP/1.1 ");
    header.push_str(&itoa(code as u64));
    header.push(' ');
    header.push_str(reason);
    header.push_str("\r\n");
    header.push_str("Content-Type: text/plain\r\n");
    header.push_str("Content-Length: ");
    header.push_str(&itoa(body.as_bytes().len() as u64));
    header.push_str("\r\n");
    header.push_str("Connection: close\r\n\r\n");

    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn itoa(mut n: u64) -> String {
    let mut buf = [0u8; 20];
    let mut i = 0;
    if n == 0 {
        buf[0] = b'0';
        i = 1;
    } else {
        while n > 0 {
            buf[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i += 1;
        }
    }
    let mut s = String::new();
    for j in (0..i).rev() {
        s.push(buf[j] as char);
    }
    s
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    syscall::exit();
}
