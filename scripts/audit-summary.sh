#!/usr/bin/env bash
# audit-summary.sh — 为审计者（Auditor）生成 commit 的结构化预筛摘要。
# 用途：调用方在送审前运行本脚本，把输出随 diff 一并交给审计者，
#       审计者据此定级（L1/L2/L3），不必重读全量上下文重建背景。
# 用法:
#   scripts/audit-summary.sh              # 审计 HEAD
#   scripts/audit-summary.sh <commit>     # 审计指定 commit
#   scripts/audit-summary.sh <range>      # 审计范围，如 HEAD~3..HEAD
#   scripts/audit-summary.sh --with-test  # 附带 cargo test 结果（慢）
# 退出码恒为 0（纯摘要，不做判定）。
set -euo pipefail

WITH_TEST=0
TARGET="HEAD"
for arg in "$@"; do
    case "$arg" in
        --with-test) WITH_TEST=1 ;;
        *) TARGET="$arg" ;;
    esac
done

# 高风险路径：命中即建议 L3 深审（协议 / 历史合法性 / 持久化 / 沙箱 / 重试）。
HIGH_RISK='^src/(context|agent_loop|registry|session_log|providers|http_trace)\.rs$|^src/tools\.rs$|^Cargo\.(toml|lock)$'
# 低风险路径：只动这些时建议 L1 浅审。
LOW_RISK='^(docs/|prompts/|memory/|scripts/|tests/|.*\.md$)|_test\.rs$'

files=$(git diff-tree --no-commit-id --name-only -r "$TARGET")
if [[ -z "$files" ]]; then
    echo "audit-summary: $TARGET 无文件改动（merge commit 或空范围？）" >&2
    exit 0
fi

echo "═══════════════════════════════════════════════════════════"
echo "审计预筛摘要 / Audit Summary — $TARGET"
echo "═══════════════════════════════════════════════════════════"
echo
echo "── commit 信息 ──"
git log -1 --pretty=format:"%h %ad %an%n%s%n%b" --date=short "$TARGET" | head -20
echo
echo
echo "── 改动统计 ──"
git diff --stat "$TARGET~1" "$TARGET" 2>/dev/null | tail -5 || git show --stat "$TARGET" | tail -5
echo

echo "── 风险路径命中 ──"
hits=$(echo "$files" | grep -E "$HIGH_RISK" || true)
low_only=$(echo "$files" | grep -vE "$LOW_RISK" || true)
if [[ -n "$hits" ]]; then
    echo "$hits" | sed 's/^/  ⚠ /'
    echo "  → 建议级别: L3 深审（完整两道关卡 + 行为级验证证据）"
elif [[ -z "$low_only" ]]; then
    echo "  全部改动落在低风险路径（测试/文档/prompts/scripts）。"
    echo "  → 建议级别: L1 浅审（仅规格符合性核对）"
else
    echo "  未命中高风险路径。"
    echo "  → 建议级别: L2 标准审（两道关卡，质量关聚焦高风险信号清单）"
fi
echo

echo "── 测试映射（改动的 pub fn ↔ 测试引用）──"
changed_fns=$(git diff "$TARGET~1" "$TARGET" 2>/dev/null \
    | grep -oE '^\+.*(pub )?fn [a-z_0-9]+' | grep -oE 'fn [a-z_0-9]+' \
    | sed 's/^fn //' | sort -u || true)
if [[ -z "$changed_fns" ]]; then
    echo "  （无新增/修改的函数签名）"
else
    while IFS= read -r fn; do
        # 粗粒度统计：tests/ 目录引用 + src/ 中含 test 字样的引用行。
        # 各 grep 均 || true 兜底，避免无匹配时触发 pipefail。
        test_hits=$( { grep -rn "\b$fn\b" tests/ 2>/dev/null || true; \
                       grep -rn "\b$fn\b" src/ 2>/dev/null | grep -iE "test" || true; } \
                     | wc -l )
        if [[ "$test_hits" -gt 0 ]]; then
            echo "  ✓ $fn — 测试引用 $test_hits 处"
        else
            echo "  ✗ $fn — 未找到测试引用，需人工确认覆盖"
        fi
    done <<< "$changed_fns"
fi
echo

echo "── 验证声明核对 ──"
if git log -1 --pretty=%B "$TARGET" | grep -qiE "cargo (test|build)|测试|全绿|passed"; then
    echo "  commit message 含验证声明，审计者请对照随附的构建/测试输出核实。"
else
    echo "  ⚠ commit message 未声明验证方式（缺少 cargo test/build 结果说明）。"
fi
echo

if [[ "$WITH_TEST" -eq 1 ]]; then
    echo "── cargo test 实跑结果 ──"
    if cargo test 2>&1 | grep -E "^test result|error\[|FAILED"; then
        :
    fi
    echo
fi

echo "── 建议送审材料清单 ──"
echo "  1. 本摘要"
echo "  2. git show $TARGET 的完整 diff"
echo "  3. cargo build + cargo test 输出（--with-test 或调用方另附）"
echo "  4. 若级别为 L3：行为级验证证据（复现脚本 / 针对性测试输出）"
