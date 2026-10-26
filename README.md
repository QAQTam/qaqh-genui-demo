# qaqh-genui-demo

A2UI v1.0 → ratatui 渲染可行性 demo，是 `qaqh-tui` 的 genui 前置验证工程。

## 内容

- `src/dsl.rs` — DSL 词法/语法扫描与 IR 构建（tsx 与 jsonl 两种载体）
- `src/main.rs` — ratatui 渲染端：把 IR 落到终端组件（Card/Stack/Row/Text/Metric/Badge/Button/Divider/Table/TodoList/TagList 等）
- `frames/` — 验证产物：`*.ansi` 为终端转储，`*.png` 为截图，`scripts/ansi2png.py` 负责转换
- `Cargo.toml` — 依赖 ratatui 0.30、serde、serde_json、unicode-width、regex

## 运行

```bash
cargo run
```

## 与下游的关系

本工程是原型验证。其中 DSL 编译链后续移植进 `qaqh-tui-app` 的 `src/genui/`：

- `src/genui/dsl/{tsx,jsonl,decoder,ir,translate}.rs` 沿用本工程 `dsl.rs` 的解析语义
- `src/genui/{eval,store,wire}.rs` 是在渲染端之外新增的状态域与求值层

两边的对应关系不保证逐行一致：`qaqh-tui-app/src/genui/` 是落地版本，本工程保留实验路径与截图证据。
