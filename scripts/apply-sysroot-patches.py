#!/usr/bin/env python3
"""apply-sysroot-patches.py — SABOS 用 sysroot パッチを正確に適用する

sed だとマクロ構文の挿入位置を間違えやすいため、Python で行単位に処理する。
全てのパッチは idempotent（既にパッチ済みならスキップ）。

使い方:
    python3 scripts/apply-sysroot-patches.py <STD_SRC_PATH>
"""

import sys
import os


def patch_file(filepath: str, check_marker: str, patch_fn) -> None:
    """ファイルにパッチを適用する。check_marker が既に含まれていればスキップ。"""
    rel = os.path.basename(os.path.dirname(filepath)) + "/" + os.path.basename(filepath)
    with open(filepath, "r") as f:
        content = f.read()

    if check_marker in content:
        print(f"[SKIP] {rel} already patched")
        return

    new_content = patch_fn(content)
    if new_content == content:
        print(f"[WARN] {rel} patch had no effect!")
        return

    with open(filepath, "w") as f:
        f.write(new_content)
    print(f"[PATCH] {rel}")


def insert_before_line(content: str, target_line: str, insertion: str) -> str:
    """content 内の target_line を含む行の直前に insertion を挿入する。
    最初にマッチした箇所のみ。"""
    lines = content.split("\n")
    result = []
    inserted = False
    for line in lines:
        if not inserted and target_line in line:
            result.append(insertion)
            inserted = True
        result.append(line)
    return "\n".join(result)


def insert_after_line(content: str, target_line: str, insertion: str) -> str:
    """content 内の target_line を含む行の直後に insertion を挿入する。
    最初にマッチした箇所のみ。"""
    lines = content.split("\n")
    result = []
    inserted = False
    for line in lines:
        result.append(line)
        if not inserted and target_line in line:
            result.append(insertion)
            inserted = True
    return "\n".join(result)


# ============================================================
# パッチ関数
# ============================================================

def patch_pal_mod(content: str) -> str:
    """sys/pal/mod.rs: sabos ブランチを _ => の直前に追加"""
    sabos_branch = (
        '    target_os = "sabos" => {\n'
        '        mod sabos;\n'
        '        pub use self::sabos::*;\n'
        '    }'
    )
    return insert_before_line(content, "    _ => {", sabos_branch)


def patch_alloc_mod(content: str) -> str:
    """sys/alloc/mod.rs: cfg_select! ブロックの末尾（}の直前）に sabos ブランチを追加。
    このファイルには cfg_select! が1つだけあり、末尾が } で終わる。
    cfg_select! の閉じ括弧を探してその前に挿入する。"""
    lines = content.split("\n")
    result = []
    # ファイル末尾方向から、"cfg_select!" の閉じ括弧 "}" を探す
    # alloc/mod.rs の最後の行は空行、その前が "}" で cfg_select! を閉じる
    # "    target_os = "zkvm" => {" ... "    }" の後、"}" の前に挿入
    #
    # 戦略: cfg_select! マクロ内を見つけるため、"cfg_select!" を探し、
    # そのブロックの末尾の "}" の直前に挿入する
    in_cfg_select = False
    cfg_select_depth = 0
    insert_idx = -1

    for i, line in enumerate(lines):
        if "cfg_select!" in line and not in_cfg_select:
            in_cfg_select = True
            cfg_select_depth = 0
        if in_cfg_select:
            cfg_select_depth += line.count("{") - line.count("}")
            if cfg_select_depth <= 0:
                # cfg_select! の閉じ括弧の行
                insert_idx = i
                break

    if insert_idx < 0:
        return content

    # insert_idx の行（cfg_select! の閉じ "}"）の前に sabos を挿入
    for i, line in enumerate(lines):
        if i == insert_idx:
            result.append('    target_os = "sabos" => {')
            result.append('        mod sabos;')
            result.append('    }')
        result.append(line)

    return "\n".join(result)


def patch_stdio_mod(content: str) -> str:
    """sys/stdio/mod.rs: sabos ブランチを _ => の直前に追加"""
    sabos_branch = (
        '    target_os = "sabos" => {\n'
        '        mod sabos;\n'
        '        pub use sabos::*;\n'
        '    }'
    )
    return insert_before_line(content, "    _ => {", sabos_branch)


def patch_thread_local_mod(content: str) -> str:
    """sys/thread_local/mod.rs: no_threads と guard の条件に sabos を追加"""
    # no_threads ブロック: 8スペース + target_os = "vexos", の後に追加
    content = insert_after_line(
        content,
        '        target_os = "vexos",',
        '        target_os = "sabos",'
    )
    # guard ブロック: 12スペース + target_os = "vexos", の後に追加
    content = insert_after_line(
        content,
        '            target_os = "vexos",',
        '            target_os = "sabos",'
    )
    return content


def patch_env_consts(content: str) -> str:
    """sys/env_consts.rs: cfg_unordered! 内の #[else] の直前に sabos エントリを追加。
    マクロ定義内の #[else] ではなく、マクロ呼び出し内の "// The fallback" コメントの前。"""
    sabos_entry = (
        '#[cfg(target_os = "sabos")]\n'
        'pub mod os {\n'
        '    pub const FAMILY: &str = "";\n'
        '    pub const OS: &str = "sabos";\n'
        '    pub const DLL_PREFIX: &str = "";\n'
        '    pub const DLL_SUFFIX: &str = "";\n'
        '    pub const DLL_EXTENSION: &str = "";\n'
        '    pub const EXE_SUFFIX: &str = ".elf";\n'
        '    pub const EXE_EXTENSION: &str = "elf";\n'
        '}\n'
    )
    # "// The fallback when none of the other gates match." の行の前に挿入
    return insert_before_line(content, "// The fallback when none of the other gates match.", sabos_entry)


def patch_io_error_mod(content: str) -> str:
    """sys/io/error/mod.rs: generic グループに sabos を追加"""
    # any( の中の target_os = "zkvm", の後に target_os = "sabos", を追加
    return insert_after_line(
        content,
        '        target_os = "zkvm",',
        '        target_os = "sabos",'
    )


def patch_random_mod(content: str) -> str:
    """sys/random/mod.rs: _ => {} の直前に sabos ブランチを追加"""
    sabos_branch = (
        '    target_os = "sabos" => {\n'
        '        mod sabos;\n'
        '        pub use sabos::fill_bytes;\n'
        '    }'
    )
    return insert_before_line(content, "    _ => {}", sabos_branch)


def patch_fs_mod(content: str) -> str:
    """sys/fs/mod.rs: _ => { の直前に sabos ブランチを追加"""
    sabos_branch = (
        '    target_os = "sabos" => {\n'
        '        mod sabos;\n'
        '        use sabos as imp;\n'
        '    }'
    )
    return insert_before_line(content, "    _ => {", sabos_branch)


def patch_os_mod(content: str) -> str:
    """os/mod.rs: xous の直後に sabos エントリを追加"""
    sabos_entry = (
        '#[cfg(target_os = "sabos")]\n'
        'pub mod sabos;'
    )
    return insert_after_line(content, 'pub mod xous;', sabos_entry)


# ============================================================
# メイン
# ============================================================

def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <STD_SRC_PATH>", file=sys.stderr)
        sys.exit(1)

    std_src = sys.argv[1]

    patches = [
        ("sys/pal/mod.rs", 'target_os = "sabos"', patch_pal_mod),
        ("sys/alloc/mod.rs", 'target_os = "sabos"', patch_alloc_mod),
        ("sys/stdio/mod.rs", 'target_os = "sabos"', patch_stdio_mod),
        ("sys/thread_local/mod.rs", 'target_os = "sabos"', patch_thread_local_mod),
        ("sys/env_consts.rs", 'target_os = "sabos"', patch_env_consts),
        ("sys/io/error/mod.rs", 'target_os = "sabos"', patch_io_error_mod),
        ("sys/random/mod.rs", 'target_os = "sabos"', patch_random_mod),
        ("sys/fs/mod.rs", 'target_os = "sabos"', patch_fs_mod),
        ("os/mod.rs", 'target_os = "sabos"', patch_os_mod),
    ]

    for rel_path, marker, patch_fn in patches:
        filepath = os.path.join(std_src, rel_path)
        patch_file(filepath, marker, patch_fn)


if __name__ == "__main__":
    main()
