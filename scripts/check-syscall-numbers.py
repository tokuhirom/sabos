#!/usr/bin/env python3
"""check-syscall-numbers.py — PAL ファイルの syscall 番号を検証するスクリプト

libs/sabos-syscall/src/lib.rs の定数定義を正（canonical source）として、
rust-std-sabos/*.rs にハードコードされた syscall 番号が正しいか検証する。

PAL ファイルは sysroot パッチのため外部 crate に依存できないので、
番号を直接書く必要がある。このスクリプトが安全網となる。

検出パターン:
  1. const SYS_XXX: u64 = NN;     — ローカル定数定義
  2. in("rax") NNu64, // SYS_XXX  — インライン asm リテラル
"""

import re
import sys
from pathlib import Path

# プロジェクトルートを特定（スクリプトの親ディレクトリの親）
SCRIPT_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = SCRIPT_DIR.parent

# 正となる定義ファイル
CANONICAL_FILE = PROJECT_ROOT / "libs" / "sabos-syscall" / "src" / "lib.rs"

# 検証対象ディレクトリ
PAL_DIR = PROJECT_ROOT / "rust-std-sabos"

# パターン: pub const SYS_XXX: u64 = NN;
RE_CANONICAL = re.compile(r"pub const (SYS_\w+):\s*u64\s*=\s*(\d+)\s*;")

# パターン1: const SYS_XXX: u64 = NN; （PAL ファイル内のローカル定数）
RE_PAL_CONST = re.compile(r"const (SYS_\w+):\s*u64\s*=\s*(\d+)\s*;")

# パターン2: in("rax") NNu64, // SYS_XXX （インライン asm リテラル）
# NNu64 の NN を取得し、コメントから SYS_XXX を取得する
RE_PAL_ASM = re.compile(r'in\("rax"\)\s+(\d+)u64\s*,\s*//\s*(SYS_\w+)')


def load_canonical() -> dict[str, int]:
    """正となる syscall 番号を読み込む"""
    if not CANONICAL_FILE.exists():
        print(f"ERROR: canonical file not found: {CANONICAL_FILE}", file=sys.stderr)
        sys.exit(2)

    result = {}
    for line in CANONICAL_FILE.read_text().splitlines():
        m = RE_CANONICAL.search(line)
        if m:
            name, number = m.group(1), int(m.group(2))
            result[name] = number
    return result


def check_pal_files(canonical: dict[str, int]) -> list[str]:
    """PAL ファイルを検証し、不一致のリストを返す"""
    errors = []

    if not PAL_DIR.exists():
        # rust-std-sabos/ が存在しない場合はスキップ（PAL 未実装）
        return errors

    for path in sorted(PAL_DIR.glob("*.rs")):
        content = path.read_text()
        rel_path = path.relative_to(PROJECT_ROOT)

        for i, line in enumerate(content.splitlines(), start=1):
            # パターン1: ローカル定数定義
            m = RE_PAL_CONST.search(line)
            if m:
                name, number = m.group(1), int(m.group(2))
                if name in canonical and canonical[name] != number:
                    errors.append(
                        f"  {rel_path}:{i}: {name} = {number} "
                        f"(expected {canonical[name]})"
                    )
                elif name not in canonical:
                    errors.append(
                        f"  {rel_path}:{i}: {name} = {number} "
                        f"(not found in canonical source)"
                    )

            # パターン2: インライン asm リテラル
            m = RE_PAL_ASM.search(line)
            if m:
                number, name = int(m.group(1)), m.group(2)
                if name in canonical and canonical[name] != number:
                    errors.append(
                        f"  {rel_path}:{i}: {name} = {number}u64 "
                        f"(expected {canonical[name]})"
                    )
                elif name not in canonical:
                    errors.append(
                        f"  {rel_path}:{i}: {name} = {number}u64 "
                        f"(not found in canonical source)"
                    )

    return errors


def main():
    canonical = load_canonical()
    print(f"Loaded {len(canonical)} syscall definitions from canonical source")

    errors = check_pal_files(canonical)

    if errors:
        print(f"\nFAILED: {len(errors)} syscall number mismatch(es) found:")
        for e in errors:
            print(e)
        sys.exit(1)
    else:
        print("PASSED: all PAL syscall numbers match canonical source")
        sys.exit(0)


if __name__ == "__main__":
    main()
