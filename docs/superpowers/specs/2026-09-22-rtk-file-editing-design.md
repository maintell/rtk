# rtk 文件操作改进设计：`rtk edit` / `rtk patch`（含读侧改写增强）

- 日期：2026-09-22
- 分支：`feature/file-editing`
- 状态：待评审
- 背景：当前 rtk 是"只读过滤器"——文件操作全在读侧（read/grep/find/ls/diff），
  无任何写侧能力；`sed`/`awk`/PowerShell 写命令在重写层为忽略/透传。

## 1. 背景与痛点（用户确认）

1. **编辑输入费 token**：agent 改文件要么整文件重写，要么用 `sed -i`/`Set-Content`
   这类在 Windows 上不可靠或语义含糊的命令。
2. **编辑后验证费 token**：改完 `cat`/`Get-Content` 回读，整份文件再次进入上下文。
3. **跨平台可靠性**：写命令的 Windows 兼容性（本 fork 已系统修过读侧命令）。

## 2. 决策记录（头脑风暴确认）

| 决策点 | 结论 |
|---|---|
| 范围 | 读侧改写增强 + 写侧编辑能力，**都要，分期实施** |
| 写侧形态 | **方案 A：双小命令 `rtk edit` + `rtk patch`**（否决批量 `rtk mutate`=B、"只回显压缩"=C） |
| 安全边界 | **保守**：写能力仅显式子命令；**hook 永不改写写命令**（复杂/批量写命令误译可能损坏文件） |
| 上游约束 | 纯 fork 自用，不拘束（可自由加依赖：`diffy`） |
| 分期 | P1 读侧增强 ✅ → P2 写侧 MVP（`rtk edit` ✅ / `rtk patch` ✅）→ P3 扩展（多文件 patch ✅ / `--backup` ✅；B 形态评估：不做） |
| 接口取舍 | 不做 `--json` 输出（YAGNI；文本回执已机器可读），后续按需加 |

## 3. 架构与组件（§1）

```
src/core/edit.rs             # 共享内核：编码/换行保真、匹配计数、原子写、±3 行 diff 回显、token 记账
src/cmds/system/edit_cmd.rs  # rtk edit
src/cmds/system/patch_cmd.rs # rtk patch（unified diff 解析 + 模糊定位，用 diffy）
src/hooks/init.rs            # agent 指导：编辑用 rtk edit/patch；验证改用 rtk read --range（禁止整文件回读）
src/discover/rules.rs        # 不新增任何写命令改写（保守红线）
Cargo.toml                   # 新增依赖：diffy
```

- 回显 diff 的渲染复用 `diff_cmd.rs` 现成手写 LCS/similarity 引擎，不加渲染依赖。
- 编码回退复用现有 `encoding_rs` 依赖。
- 两命令接入现有 `tracking` 机制，`rtk gain` 可见编辑类节省。

## 4. 命令接口（§2）

```
rtk edit <file> --find STR --replace STR [--all] [--regex] [--preview] [--range a-b]
rtk edit src\app.rs --find "fn main(" --replace "pub fn main("
  → 1 replacement @ line 42
    @@ -40,3 +40,3 @@
     use std::io;
    -fn main() {
    +pub fn main() {

rtk patch <file> < changes.diff      # 单文件
rtk patch <file> --dry-run ...       # 零写入预演
  → 2 hunks applied to app.rs (+6/-2)
    ...改动区 ±3 行...
```

**核心契约（防误伤设计）**

1. `--find` 默认要求**唯一匹配**：0 处 → `no match`；>1 处且未给 `--all`/`--range` →
   拒绝且不写入，并报全部命中行号（agent 据此收敛）。
2. **输入只含改动行，输出只有一行回执 + 至多 ~20 行片段** —— 同时解决痛点 1 和 2。
3. `--preview` / `--dry-run`：完整执行匹配与定位，仅不写盘，输出与真执行同形。
4. `--regex`：`--find` 按 Rust regex 语义；仍受唯一匹配/计数规则约束（捕获组引用 `$1` 支持）。
5. patch 多 hunk **all-or-nothing**：任一 hunk 定位失败（含歧义匹配）→ 整文件不落盘。
   歧义失败时列出候选行号，提示补充上下文。

**文件字节保真**

- 换行风格：按主导比例判定 CRLF/LF，匹配在归一化文本上做，写回恢复原风格。
- 编码：严格 UTF-8（保留 BOM）→ 失败则 `encoding_rs` 试 GBK(936) → 再失败按"非文本"
  拒绝编辑（给出原因）。
- 二进制：首 8KB 含 NUL → 拒绝。

## 5. 数据流与错误处理（§3）

`rtk edit` 数据流：

```
参数解析 → 读字节 → 编码探测 → 换行归一化 → 查找计数
  ├─ 违反匹配规则 → stderr 一行诊断 + exit 1（零写入）
  └─ 通过 → 替换 → 生成 ±3 行 diff → 恢复换行/编码 → temp 文件写入
            → fs::rename 原子替换（同目录；Windows 走 ReplaceFile 语义）
            → stdout 回执 → tracking 记账 → exit 0
```

`rtk patch` 数据流：解析（diffy）→ 逐 hunk 定位（fuzz）→ 内存中全部成功才写盘 → 同上。

**退出码约定**（对齐 rtk 现有风格，供 hook/agent 判别）：

| code | 含义 |
|---|---|
| 0 | 成功（或 --dry-run 可行） |
| 1 | 匹配失败（no match / 多处匹配 / hunk 定位失败）——未写入任何字节 |
| 2 | 用法/解析错误（参数冲突、diff 格式非法、二进制/编码拒绝） |
| 3+ | IO 失败（权限/磁盘满/目标被锁）——写一半的情况被原子写排除，文件保持原样 |

**错误消息原则**：一行、含文件名、含行号、含可行动建议（如 `--all` 或 `--range 40-60`）。

## 6. 安全边界（§4）

- hook（PreToolUse 重写）**不改写任何写操作命令**：`sed -i`、`Set-Content`、
  `Out-File`、`>` 重定向等一律放行原样执行，维持现状。
- rtk 写路径仅在**显式调用** `rtk edit` / `rtk patch` 时发生；无隐式副作用。
- 不提供递归批量写（P3 的多文件 patch 也仅限 diff 中显式列出的文件）。
- 拒改清单：二进制、探测失败的编码、`.git/` 内部路径。

## 7. 读侧增强明细（P1，先行小步）

重写规则/内核小项（全部读侧，符合保守红线）：

1. **`rtk read` 新增 `--range <a>-<b>`（行窗口）— ✅ 已实现（fb7a8b3）**：当前 read 只有
   head/tail/max-lines，无任意区间；验证流（§4 契约）依赖它。实现为
   `range_window`/`nth_newline_end` + dispatch 手动解析（非 meta 命令的 clap 错误会被
   `run_fallback` 吞掉退化成 exec，坏值必须走内核解析才能拿到干净的 exit 2）。
   内核提供确定性优先级 head > tail > range，clap `conflicts_with_all` 仅作声明。
2. ~~`type <file>`/`more <file>` → `rtk read`~~ — **砍掉（复核后）**。二者是
   `IGNORED_PREFIXES` 成员、在 Windows 下本就可用（cmd 内建/`more.com`），改写的净收益
   只有回显压缩，但 `type f > out`、`more f | findstr` 等重定向/管道形态广泛存在，
   且 `more` 在 cmd/PS 语义不同——误伤面 > 收益，违反"宁可漏改写不可错改写"。
3. ~~`Get-Content -ReadCount/-Head/-Tail` 补齐~~ — **复核发现 #3439 已覆盖**
   （`-TotalCount/-First/-Head`→`--max-lines`，`-Tail/-Last`→`--tail-lines`），无需改动。
4. ~~`sed -n 'A,Bp' file` → `rtk read --range`~~ — **砍掉（复核后）**。Windows PATH 上
   根本没有 sed，无"不能用"问题；Linux/mac 场景非本 fork 用户痛点；且 sed 与 discover
   层的 IGNORED 语义纠缠。agent 在 Windows 读区间已有 `Get-Content`/`rtk read --range` 路径。
5. `init` 指导文档：✅ 已交付——新增 "Editing (write side)" 章节，教 agent 用
   `rtk edit`/`rtk patch` 做省 token 编辑、用 `rtk read --range` 做验证回读。

## 8. 测试策略（§5）

**单元测试（内核，纯函数优先）**：
- 匹配规则：唯一/零/多（含 --all/--range/--preview 组合矩阵）
- 换行保真：CRLF/LF/混合文件的读→改→写往返恒等
- 编码：UTF-8 BOM 保留、GBK 往返、混合失败拒绝、NUL 二进制拒绝
- patch：行号偏移容错、±fuzz、歧义多解拒绝（列候选行号）、all-or-nothing 无副作用
- 回显：≤1 行回执 + ≤N 行片段的上限契约断言

**集成测试（tests/ 风格，走真二进制 + tempfile）**：
- edit 端到端、改后 `rtk read --range` 验证闭环、退出码矩阵
- Windows 专属：反斜杠路径、只读文件、被占用文件（共享冲突→exit 3+ 且文件原样）

**验收标准（每阶段）**：
- P1：新规则经 `rtk rewrite` 快照断言；误译 = 0（宁可漏改写不可错改写）。
- P2：编辑类 token 实测对比（agent 工作流场景：改 3 处 + 验证），相对
  `sed/cat` 基线节省 ≥70%；`cargo test` Windows 全绿；fmt/clippy 零告警。
- P3：多文件 patch ✅（git/纯 `---`/`+++` 双格式解析、`-p N` 剥前缀、跨文件 all-or-nothing + 写阶段字节回滚、`/dev/null` 增删、绝对/`..`/`.git` 路径拒绝）；`--backup` ✅（edit+patch 均产出 `<file>.bak`）。B 形态（结构化批量 mutate）复盘：**不做**——多文件 patch 已覆盖批量场景，`rtk mutate` 的 JSON/转义复杂度在 PowerShell 下反噬，YAGNI。

## 9. 风险与开放问题

| 风险 | 缓解 |
|---|---|
| diffy 的 fuzz 语义与我们期望不完全一致 | 集成测试锁定；必要时自写定位器（~200 行） |
| GBK/UTF-8 误判 | 探测失败即拒绝（宁可不做），报告可 `--encoding` 显式覆盖（P3） |
| Windows 文件占用/杀软锁 | 原子写失败→文件原样 + exit 3；测试覆盖 |
| `--regex` 灾难性回溯 | regex crate 线性时间保证，不用 onig/背引用引擎 |
