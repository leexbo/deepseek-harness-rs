## 1. 编码约束 (Coding Constraints)

- **[Error] (MUST)**: 库层强制 `thiserror`，装配层强制 `anyhow`。**(FORBIDDEN)**: 业务逻辑出现 `unwrap()` / `expect()`。
- **[Deps] (MUST)**: 统一锁定于 `[workspace.dependencies]`。**(FORBIDDEN)**: 未经指令授权的 `cargo update`。
- **[Wasm] (MUST)**: 100% 重放确定性。**(FORBIDDEN)**: 组件内直读系统时钟/随机数，强制通过 WASI 显式 import。
- **[FFI] (MUST)**: Linux 沙箱隔离使用纯 Rust 实现 (如 `landlock`)。**(FORBIDDEN)**: 引入任何 C 交付物。
- **[Docs] (MUST)**: `dsh-host` 与 `dsh-wit` 公开 API 强制 `#![deny(missing_docs)]`。
- **[GPUI] (MUST)**: UI 开发前强制挂载 `gpui-kit` 与 `gpui-kit-design-guides` 上下文。**(FORBIDDEN)**: 手搓已有基础控件。弹层 (Popover/Tooltip) 强制遵守：`根级渲染 (Root-render) + 锚定计算 (Anchor) + 遮蔽打断 (Occlude)`。

## 2. CI/CD 测试门禁 (Test Gates)

执行管线：`fmt-check` -> `clippy` -> `test` -> `wit` -> `component-contracts` -> `e2e` -> `links`。

- **[L1-Unit]**: 纯逻辑校验（编解码 / 状态机 / 配置合并）。
- **[L2-Contract]**: `tests/` 目录通过 Wasmtime 真实实例化，验证 World 语义、不变式与取消逻辑。
- **[L3-E2E]**: Mock Provider 驱动全链路，断言日志 Bit-exact 可重放。
- **[Regression] (MUST)**: 任何行为修复 (Fix) **必须** 携带回归锁测试 (Regression Test)。
- **[Flaky] (FORBIDDEN)**: 严禁使用 `#[ignore]` 或跳过 (Skip) 机制掩盖偶发失败，必须修复根因。

## 3. Git 交付契约 (Commit Protocol)

- **[Format] (MUST)**: 遵循 Conventional Commits 规范 `<type>(<scope>): <subject>`。
    - `type` ∈ `feat` | `fix` | `docs` | `refactor` | `perf` | `test` | `chore`。
    - `subject`: 英文祈使句 (如 `add xyz` / `fix abc`)，**结尾不加句号**。
    - `body`: 说明动机 (Motivation) 与机制 (Mechanism)。若是 `fix`，需明确指出回归锁所在文件及用例。
- **[Signature] (MUST)**: Commit 提交信息末行强制追加 Agent 签名。
    - 格式: `Co-Authored-By: <当前模型名> <bot@dsh-agent.local>`
    - (例:  `Co-Authored-By: GLM 5.3 <bot@dsh-agent.local>`)