//! qaqh-ui DSL → A2UI v1.0 wire 的 Rust 移植（对齐桌面端
//! `src/genui/source/decoder.ts` + `compiler/{ir,parse,jsonl}.ts` + `v2/translate.ts`）。
//!
//! 链路：`split_content_parts`（Markdown 分帧，decoder 语义）→
//! `compile_source`（parse → normalize_bindings → validate，partial 容尾/剪枝）→
//! `translate_create / translate_update`（IR → A2UI v1.0 消息，含 root 包装根、
//! `/s/` 状态域、`qaqh:arith|logic.*` 表达式降级、`qaqh.action.*` 事件、
//! `qaqh.dev/repeat` 扩展）。这是 TUI 接真实模型输出的最后一块拼图：
//! daemon 送来的 assistant 文本经此链路即可喂给 demo 已有的
//! SurfaceRegistry → 布局 → 交互全链。

use std::collections::BTreeMap;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::{
    ActionEvent, ChildList, CreateSurface, EventSpec, Message, UpdateComponents, UpdateDataModel,
    WireComponent, A2UI_VERSION, REPEAT_EXTENSION,
};

pub const V2_CATALOG_ID: &str = "qaqh.dev:standard-v2";
pub const MAX_NODES: usize = 512;
pub const MAX_TREE_DEPTH: usize = 16;

/// JSON 数值：整数进整数出（对齐 JS `JSON.stringify(Number)` 的序列化形状，
/// 保证与桌面端 translate 产物逐字节可比）。
fn num_value(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9.007_199_254_740_991e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IR（对齐 compiler/ir.ts）
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropKind {
    Str,
    Num,
    Bool,
    Expr,
    Action,
    List,
    Binding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateType {
    Str,
    Num,
    Integer,
    Bool,
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Neq,
    Gt,
    Lt,
    And,
    Or,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Lit(Value),
    State(String),
    Item(String),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum Action {
    Set { key: String, value: Expr },
    Reset,
    FollowUp { prompt: String, fields: Vec<(String, String)> },
    Copy { value: Expr },
    OpenLink(String),
    Append { key: String, value: Expr },
    Remove { key: String, value: Expr },
}

#[derive(Debug, Clone)]
pub enum PropValue {
    Lit(Value),
    Expr(Expr),
    Action(Action),
    List(Vec<String>),
    Binding(String),
    /// for/repeat：source 是 list 态 key，as_ 是循环变量名。
    Repeat(String, String),
}
#[derive(Debug, Clone)]
pub struct IrNode {
    pub id: String,
    pub type_: String,
    pub props: BTreeMap<String, PropValue>,
    pub line: usize,
    /// for/repeat：本节点（含子树）按 list 态逐项渲染模板。
    pub repeat: Option<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct StateDecl {
    pub type_: StateType,
    pub initial: Value,
}

#[derive(Debug, Clone, Default)]
pub struct SurfaceIr {
    pub surface_id: String,
    pub title: Option<String>,
    pub fallback: Option<String>,
    pub root_id: Option<String>,
    pub states: BTreeMap<String, StateDecl>,
    pub order: Vec<String>,
    pub nodes: BTreeMap<String, IrNode>,
    /// 编译期软告警（孤儿节点），只在 seal 后计算。
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GenUiError {
    pub message: String,
    pub line: Option<usize>,
}

impl std::fmt::Display for GenUiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}（第 {line} 行）", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

pub type GenUiResult<T> = Result<T, GenUiError>;

fn ui_err(line: usize, message: impl Into<String>) -> GenUiError {
    GenUiError { message: message.into(), line: Some(line) }
}

/// 受信组件目录（封闭）：未知组件/属性一律拒绝。`show` 是通用属性。
pub const CATALOG: &[&str] = &[
    "Card", "Stack", "Row", "Title", "Text", "Metric", "Badge", "Button", "Divider", "TextInput",
    "Checkbox", "TodoList", "Todo", "TagList",
];

pub fn prop_kind(component: &str, name: &str) -> Option<PropKind> {
    use PropKind::*;
    if name == "show" {
        return Some(Expr);
    }
    Some(match (component, name) {
        ("Card", "title") | ("TodoList", "title") => Str,
        ("Card", "children")
        | ("Stack", "children")
        | ("Row", "children")
        | ("TodoList", "children") => List,
        ("Title", "text") | ("Text", "text") | ("Badge", "text") | ("Todo", "text") => Expr,
        ("Metric", "label") | ("TextInput", "label") | ("Checkbox", "label") => Str,
        ("Metric", "value") | ("Todo", "meta") => Expr,
        ("Button", "label") => Str,
        ("Button", "onPress") => Action,
        ("Button", "disabled") => Expr,
        ("TextInput", "value")
        | ("Checkbox", "checked")
        | ("Todo", "done")
        | ("TagList", "items") => Binding,
        ("TextInput", "placeholder") | ("TextInput", "pattern") => Str,
        ("TextInput", "required") => Expr,
        ("TextInput", "maxLength") => Num,
        _ => return None,
    })
}

/// binding 属性要求的状态类型。
fn binding_type(component: &str, name: &str) -> Option<StateType> {
    match (component, name) {
        ("TextInput", "value") => Some(StateType::Str),
        ("Checkbox", "checked") | ("Todo", "done") => Some(StateType::Bool),
        ("TagList", "items") => Some(StateType::List),
        _ => None,
    }
}

/// 无 root 时的森林顶层：没有被任何 children 引用的节点（保声明顺序）。
pub fn forest_roots(ir: &SurfaceIr) -> Vec<String> {
    let mut nested = std::collections::BTreeSet::new();
    for id in &ir.order {
        if let Some(PropValue::List(items)) = ir.nodes[id].props.get("children") {
            nested.extend(items.iter().cloned());
        }
    }
    ir.order.iter().filter(|id| !nested.contains(*id)).cloned().collect()
}

/// 裸 `expr{state}` 简写 → binding（TextInput.value={name} 等价 ui.bind）。
pub fn normalize_bindings(ir: &mut SurfaceIr) {
    for id in &ir.order {
        let node = ir.nodes.get_mut(id).expect("order 与 nodes 同步");
        let contract_ok = |name: &str| prop_kind(&node.type_, name) == Some(PropKind::Binding);
        for (name, value) in node.props.iter_mut() {
            if !contract_ok(name) {
                continue;
            }
            if let PropValue::Expr(Expr::State(key)) = value {
                *value = PropValue::Binding(key.clone());
            }
        }
    }
}

/// 编译期软告警（只在 seal 后算）：孤儿节点——声明了却没挂进任何 children。
pub fn surface_warnings(ir: &SurfaceIr) -> Vec<String> {
    let Some(root) = &ir.root_id else { return vec![] };
    if !ir.nodes.contains_key(root) {
        return vec![];
    }
    let mut referenced = std::collections::BTreeSet::new();
    for id in &ir.order {
        if let Some(PropValue::List(items)) = ir.nodes[id].props.get("children") {
            referenced.extend(items.iter().cloned());
        }
    }
    let orphans: Vec<&String> =
        ir.order.iter().filter(|id| *id != root && !referenced.contains(*id)).collect();
    if orphans.is_empty() {
        return vec![];
    }
    let head: Vec<&str> = orphans.iter().take(8).map(|s| s.as_str()).collect();
    let tail = if orphans.len() > 8 { format!(" 等 {} 个", orphans.len()) } else { String::new() };
    vec![format!(
        "节点 {}{tail} 未被任何 children 引用，不会渲染；补进容器的 children 或删掉声明",
        head.join("、")
    )]
}

fn check_expr(expr: &Expr, ir: &SurfaceIr, line: usize) -> GenUiResult<()> {
    match expr {
        Expr::Lit(_) => Ok(()),
        Expr::State(key) => {
            if ir.states.contains_key(key) {
                Ok(())
            } else {
                Err(ui_err(line, format!("表达式引用未知状态 {key}")))
            }
        }
        Expr::Item(_) => Ok(()), // 循环变量：作用域在渲染期检查
        Expr::Not(v) => check_expr(v, ir, line),
        Expr::Bin(_, l, r) => {
            check_expr(l, ir, line)?;
            check_expr(r, ir, line)
        }
    }
}

/// 编译期校验：目录封闭性、引用完整性、拓扑（环/深度）。partial = 流式部分编译，
/// children 前向引用先剪枝、root 未到齐先平铺。
pub fn validate_surface(ir: &mut SurfaceIr, partial: bool) -> GenUiResult<()> {
    if ir.surface_id.is_empty() {
        return Err(GenUiError { message: "surface 缺少 id".into(), line: None });
    }
    if ir.order.len() > MAX_NODES {
        return Err(GenUiError { message: format!("节点数超过预算（{MAX_NODES}）"), line: None });
    }
    let node_ids: std::collections::BTreeSet<&String> = ir.order.iter().collect();
    for id in ir.order.clone() {
        let node = ir.nodes[&id].clone();
        if !CATALOG.contains(&node.type_.as_str()) {
            return Err(ui_err(node.line, format!("未知组件类型:{}", node.type_)));
        }
        for (name, value) in &node.props {
            let Some(kind) = prop_kind(&node.type_, name) else {
                return Err(ui_err(node.line, format!("{} 不支持属性 {}", node.type_, name)));
            };
            let fail = || ui_err(node.line, format!("属性 {name} 需要 {kind:?}"));
            match (kind, value) {
                (PropKind::Expr, PropValue::Expr(e)) => check_expr(e, ir, node.line)?,
                // expr 槽位同样收字面量（TS：value.kind !== "lit" 才 fail）。
                (PropKind::Expr, PropValue::Lit(_)) => {}
                (PropKind::Expr, _) => return Err(fail()),
                (PropKind::Str | PropKind::Num | PropKind::Bool, PropValue::Lit(v)) => {
                    // 对齐 TS typeof 语义：整数与浮点都是 number。
                    let ok = match (kind, v) {
                        (PropKind::Str, Value::String(_)) => true,
                        (PropKind::Num, Value::Number(_)) => true,
                        (PropKind::Bool, Value::Bool(_)) => true,
                        _ => false,
                    };
                    if !ok {
                        return Err(fail());
                    }
                }
                (PropKind::Str | PropKind::Num | PropKind::Bool, PropValue::Expr(e)) => {
                    check_expr(e, ir, node.line)?;
                    if let Expr::Lit(v) = e {
                        let ok = match (kind, v) {
                            (PropKind::Str, Value::String(_)) => true,
                            (PropKind::Num, Value::Number(_)) => true,
                            (PropKind::Bool, Value::Bool(_)) => true,
                            _ => false,
                        };
                        if !ok {
                            return Err(fail());
                        }
                    }
                }
                (PropKind::Str | PropKind::Num | PropKind::Bool, _) => return Err(fail()),
                (PropKind::Action, PropValue::Action(action)) => {
                    let has = |key: &str| ir.states.contains_key(key);
                    match action {
                        Action::Set { key, value } => {
                            if !has(key) {
                                return Err(ui_err(node.line, format!("动作引用未知状态 {key}")));
                            }
                            check_expr(value, ir, node.line)?;
                        }
                        Action::FollowUp { fields, .. } => {
                            for (label, key) in fields {
                                if !has(key) {
                                    return Err(ui_err(
                                        node.line,
                                        format!("followUp 字段 {label} 引用未知状态 {key}"),
                                    ));
                                }
                            }
                        }
                        Action::Append { key, value } | Action::Remove { key, value } => {
                            let Some(state) = ir.states.get(key) else {
                                return Err(ui_err(node.line, format!("动作引用未知状态 {key}")));
                            };
                            if state.type_ != StateType::List {
                                return Err(ui_err(
                                    node.line,
                                    format!("append/remove 需要 list 型状态 {key}"),
                                ));
                            }
                            check_expr(value, ir, node.line)?;
                        }
                        Action::Copy { value } => check_expr(value, ir, node.line)?,
                        Action::Reset | Action::OpenLink(_) => {}
                    }
                }
                (PropKind::Action, _) => return Err(fail()),
                (PropKind::List, PropValue::List(items)) => {
                    if partial {
                        // 流式：children 前向引用未到齐先剪掉，等下一帧补齐。
                        let node = ir.nodes.get_mut(&id).expect("存在");
                        if let Some(PropValue::List(items)) = node.props.get_mut("children") {
                            items.retain(|child| node_ids.contains(child));
                        }
                    } else {
                        for child in items {
                            if !node_ids.contains(child) {
                                return Err(ui_err(
                                    node.line,
                                    format!("children 引用未知节点 {child}"),
                                ));
                            }
                        }
                    }
                }
                (PropKind::List, _) => return Err(fail()),
                (PropKind::Binding, PropValue::Binding(key)) => {
                    let Some(state) = ir.states.get(key) else {
                        return Err(ui_err(node.line, format!("绑定引用未知状态 {key}")));
                    };
                    if let Some(expected) = binding_type(&node.type_, name) {
                        if state.type_ != expected {
                            return Err(ui_err(
                                node.line,
                                format!("绑定 {key} 需要 {expected:?} 型状态"),
                            ));
                        }
                    }
                }
                (PropKind::Binding, _) => {
                    return Err(ui_err(
                        node.line,
                        format!("属性 {name} 需要 binding（用 ui.bind(\"key\") 或直接写 {{状态名}}）"),
                    ));
                }
            }
        }
    }

    if let Some(root) = ir.root_id.clone() {
        if node_ids.contains(&root) {
            // ok
        } else if partial {
            ir.root_id = None; // 流式：root 指向的节点未到齐先平铺
        } else {
            return Err(GenUiError { message: format!("root 引用未知节点 {root}"), line: None });
        }
    }

    // for/repeat：source 必须是 list 态；循环变量不得与状态同名。
    for id in ir.order.clone() {
        let Some((source, as_)) = ir.nodes[&id].repeat.clone() else { continue };
        match ir.states.get(&source) {
            Some(state) if state.type_ == StateType::List => {}
            Some(_) if partial => continue, // 流式：数据源未到齐先跳过
            Some(_) => {
                return Err(ui_err(ir.nodes[&id].line, format!("repeat 需要 list 型状态 {source}")))
            }
            None if partial => continue,
            None => {
                return Err(ui_err(ir.nodes[&id].line, format!("repeat 引用未知状态 {source}")))
            }
        }
        if ir.states.contains_key(&as_) {
            return Err(ui_err(ir.nodes[&id].line, format!("循环变量 {as_} 与状态同名")));
        }
    }

    // 环与深度：对全部节点 DFS（纯环节点没有外部引用者，不能只从根出发）。
    let mut visiting = std::collections::BTreeSet::new();
    let mut done = std::collections::BTreeSet::new();
    for id in ir.order.clone() {
        visit_children(ir, &id, 1, &mut visiting, &mut done)?;
    }
    Ok(())
}

fn visit_children(
    ir: &SurfaceIr,
    id: &str,
    depth: usize,
    visiting: &mut std::collections::BTreeSet<String>,
    done: &mut std::collections::BTreeSet<String>,
) -> GenUiResult<()> {
    if done.contains(id) {
        return Ok(());
    }
    if visiting.contains(id) {
        return Err(GenUiError {
            message: format!("children 形成环:{id}"),
            line: ir.nodes.get(id).map(|n| n.line),
        });
    }
    if depth > MAX_TREE_DEPTH {
        return Err(GenUiError {
            message: format!("节点树深度超过 {MAX_TREE_DEPTH}"),
            line: ir.nodes.get(id).map(|n| n.line),
        });
    }
    visiting.insert(id.to_string());
    if let Some(PropValue::List(items)) = ir.nodes[id].props.get("children") {
        for child in items.clone() {
            visit_children(ir, &child, depth + 1, visiting, done)?;
        }
    }
    visiting.remove(id);
    done.insert(id.to_string());
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 围栏分帧（对齐 source/decoder.ts 的纯函数路径）
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiFormat {
    Tsx,
    Jsonl,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    Markdown(String),
    Ui {
        format: UiFormat,
        source: String,
        /// false = 围栏未闭合（流式生成中）；缺结尾永远不得伪造成 seal。
        sealed: bool,
        surface_id: Option<String>,
        fallback: Option<String>,
    },
}

const UI_INFOS: [&str; 4] = [
    "qaqh-ui v=0.1 format=tsx",
    "qaqh-ui v=0.1 format=jsonl",
    "qaqh-ui v=0.2 format=tsx",
    "qaqh-ui v=0.2 format=jsonl",
];

fn ui_format_of(info: &str) -> Option<UiFormat> {
    let tokens: Vec<&str> = info.trim().split_whitespace().collect();
    if tokens.first() != Some(&"qaqh-ui") || tokens.len() > 3 {
        return None;
    }
    let mut format = None;
    for token in &tokens[1..] {
        let (key, value) = token.split_once('=')?;
        match key {
            "v" if value == "0.1" || value == "0.2" => {}
            "format" if value == "tsx" => format = Some(UiFormat::Tsx),
            "format" if value == "jsonl" => format = Some(UiFormat::Jsonl),
            _ => return None,
        }
    }
    format
}

/// 不完整尾行是否仍可能长成 qaqh-ui 围栏头（holdback 判定）。
fn is_holdback_candidate(line: &str) -> bool {
    if line.chars().count() > 64 {
        return false;
    }
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return false;
    }
    let ticks = body.chars().take_while(|c| *c == '`').count();
    let rest = &body[ticks..];
    if rest.contains('`') {
        return false;
    }
    if ticks < 3 {
        // 反引号还没长够 3 个：后面可能续成 ```qaqh-ui，只有空余行才缓冲。
        return rest.trim().is_empty();
    }
    let info = rest.trim();
    info.is_empty() || UI_INFOS.iter().any(|valid| valid.starts_with(info))
}

fn unescape(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 从源码提取展示字段（surfaceId/fallback）；失败即缺省，不影响分帧。
fn describe(format: UiFormat, source: &str) -> (Option<String>, Option<String>) {
    if format == UiFormat::Tsx {
        let id = Regex::new(r#"ui\.surface\(\s*"((?:[^"\\]|\\.)*)""#).unwrap()
            .captures(source).and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let fallback = Regex::new(r#"fallback:\s*"((?:[^"\\]|\\.)*)""#).unwrap()
            .captures(source).and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        return (id.map(|s| unescape(&s)), fallback.map(|s| unescape(&s)));
    }
    for raw in source.split('\n').take(50) {
        let line = raw.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<Value>(line) {
            if record.get("op").and_then(Value::as_str) == Some("surface.open") {
                let id = record.get("surface").and_then(Value::as_str).filter(|s| !s.is_empty());
                let fb = record.get("fallback").and_then(Value::as_str).filter(|s| !s.is_empty());
                return (id.map(str::to_string), fb.map(str::to_string));
            }
        }
    }
    (None, None)
}

/// 全量分帧（decoder 纯函数路径）。final = 流已结束：不完整尾行按字面处理，
/// 不吞用户可见文本。demo/全量场景每次对全文重扫，语义与增量水位封装一致。
pub fn split_content_parts(text: &str, final_: bool) -> Vec<ContentPart> {
    if !text.contains('`') {
        if text.is_empty() {
            return vec![];
        }
        let body = text.strip_suffix('\n').unwrap_or(text);
        return if body.is_empty() { vec![] } else { vec![ContentPart::Markdown(body.to_string())] };
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop(); // 尾部 \n 的切分伪影不是真实行
    }
    // 流式：末尾不完整行先摘出（holdback 候选判定用）。
    let pending: Option<String> =
        if !final_ && !text.ends_with('\n') { lines.pop().map(str::to_string) } else { None };

    let mut parts: Vec<ContentPart> = vec![];
    let mut md: Vec<String> = vec![];
    let mut ui: Option<(UiFormat, Vec<String>, usize)> = None; // (format, source 行, 开门长度)
    let mut fence_length = 0usize; // 普通围栏开门长度；0 = 不在围栏内

    let flush_md = |md: &mut Vec<String>, parts: &mut Vec<ContentPart>| {
        let joined = md.join("\n");
        if !joined.is_empty() {
            parts.push(ContentPart::Markdown(joined));
        }
        md.clear();
    };

    for line in &lines {
        if let Some((format, source, open_len)) = ui.as_mut() {
            let close = fence_close_len(line);
            if close.is_some_and(|len| len >= *open_len) {
                let source_text = source.join("\n");
                let (surface_id, fallback) = describe(*format, &source_text);
                parts.push(ContentPart::Ui {
                    format: *format,
                    source: source_text,
                    sealed: true,
                    surface_id,
                    fallback,
                });
                ui = None;
            } else {
                source.push((*line).to_string());
            }
            continue;
        }
        if fence_length > 0 {
            if fence_close_len(line).is_some_and(|len| len >= fence_length) {
                fence_length = 0;
            }
            md.push((*line).to_string());
            continue;
        }
        if let Some((ticks, info)) = fence_open(line) {
            if !info.contains('`') {
                if let Some(format) = ui_format_of(info) {
                    flush_md(&mut md, &mut parts);
                    ui = Some((format, vec![], ticks));
                    continue;
                }
                fence_length = ticks;
            }
        }
        md.push((*line).to_string());
    }

    // 收尾：holdback 判定——顶层 md 上、可能长成围栏头的尾行先缓冲（不当 Markdown）。
    let pending = match pending {
        Some(line) if ui.is_none() && is_holdback_candidate(&line) => None,
        other => other,
    };
    if let Some((format, source, open_len)) = ui {
        let sealed = pending
            .as_deref()
            .and_then(fence_close_len)
            .is_some_and(|len| len >= open_len);
        let mut source = source;
        if !sealed {
            if let Some(line) = &pending {
                source.push(line.clone());
            }
        }
        let source_text = source.join("\n");
        let (surface_id, fallback) = describe(format, &source_text);
        parts.push(ContentPart::Ui { format, source: source_text, sealed, surface_id, fallback });
    } else {
        if let Some(line) = &pending {
            md.push(line.clone());
        }
        flush_md(&mut md, &mut parts);
    }
    parts
}

/// /^ {0,3}(`{3,})(.*)$/ → (开门长度, info)
fn fence_open(line: &str) -> Option<(usize, &str)> {
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return None;
    }
    let ticks = body.chars().take_while(|c| *c == '`').count();
    if ticks < 3 {
        return None;
    }
    Some((ticks, &body[ticks..]))
}

/// /^ {0,3}(`{3,})\s*$/ → 开门长度（关门不允许 info）
fn fence_close_len(line: &str) -> Option<usize> {
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return None;
    }
    let ticks = body.chars().take_while(|c| *c == '`').count();
    if ticks < 3 || !body[ticks..].trim().is_empty() {
        return None;
    }
    Some(ticks)
}

// ─────────────────────────────────────────────────────────────────────────────
// tsx DSL 解析器（对齐 compiler/parse.ts）
// ─────────────────────────────────────────────────────────────────────────────

type Statement = (String, usize); // (text, line)

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// 深度 0 的分号切语句；跳过字符串与注释；容尾模式丢弃结尾未终结语句。
pub fn split_statements(source: &str, tolerant_tail: bool) -> Vec<Statement> {
    let mut statements: Vec<Statement> = vec![];
    let bytes = source.as_bytes();
    let mut start: Option<usize> = None;
    let mut start_line = 1usize;
    let mut line = 1usize;
    let mut depth = 0i32;
    let mut i = 0usize;
    macro_rules! push_stmt {
        ($end:expr) => {{
            if let Some(s) = start {
                let text = source[s..$end].trim();
                if !text.is_empty() {
                    statements.push((text.to_string(), start_line));
                }
                start = None;
            }
        }};
    }
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            if i + 1 >= bytes.len() && tolerant_tail {
                return statements; // 未闭合注释 = 半截
            }
            i += 2;
            continue;
        }
        // 非 ASCII 字节（CJK 等）只出现在字符串/注释内部或非法内容里：
        // 逐字节跳过，保证后续 source[i..] 切片都在字符边界上。
        if c >= 0x80 {
            i += 1;
            continue;
        }
        if start.is_none() {
            if c.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            start = Some(i);
            start_line = line;
        }
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            if i >= bytes.len() && tolerant_tail {
                return statements; // 未闭合字符串 = 尾部半截语句
            }
            i += 1;
            continue;
        }
        if c == b'(' || c == b'[' || c == b'{' {
            depth += 1;
        }
        if c == b')' || c == b']' || c == b'}' {
            depth -= 1;
        }
        if c == b';' && depth <= 0 {
            push_stmt!(i);
            i += 1;
            continue;
        }
        // 宽容换行（模型省略分号）：新语句关键字起始 + 前一非空白是换行 + 上文可终止。
        if depth <= 0
            && start.is_some()
            && (source[i..].starts_with("ui.") || source[i..].starts_with("const "))
            && previous_non_space_is_newline(source, i)
            && ends_statement_before(source, start.unwrap(), i)
        {
            push_stmt!(i);
            start = Some(i);
            start_line = line;
            continue;
        }
        i += 1;
    }
    if tolerant_tail {
        if let Some(s) = start {
            if depth > 0 || !statements_closed_tail(source, s) {
                return statements;
            }
        }
        push_stmt!(source.len());
        return statements;
    }
    push_stmt!(source.len());
    statements
}

fn statements_closed_tail(source: &str, start: usize) -> bool {
    let bytes = source.as_bytes();
    let mut depth = 0i32;
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            i += 1;
            continue;
        }
        if c == b'(' || c == b'[' || c == b'{' {
            depth += 1;
        }
        if c == b')' || c == b']' || c == b'}' {
            depth -= 1;
        }
        i += 1;
    }
    let tail = source[start..].trim_end();
    depth <= 0
        && (tail.ends_with(')') || tail.ends_with('}') || tail.ends_with('"') || tail.ends_with('\''))
}

fn previous_non_space_is_newline(source: &str, index: usize) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = index as isize - 1;
    while cursor >= 0 && (bytes[cursor as usize] == b' ' || bytes[cursor as usize] == b'\t' || bytes[cursor as usize] == b'\r') {
        cursor -= 1;
    }
    cursor >= 0 && bytes[cursor as usize] == b'\n'
}

fn ends_statement_before(source: &str, start: usize, index: usize) -> bool {
    let head = &source[start..index];
    (head.contains(')') || head.contains('"')) && !head.trim().is_empty()
}

fn parse_string_literal(text: &str, line: usize) -> GenUiResult<String> {
    let trimmed = text.trim();
    let re = Regex::new(r#"^"((?:[^"\\]|\\.)*)"$"#).unwrap();
    let re1 = Regex::new(r#"^'((?:[^'\\]|\\.)*)'$"#).unwrap();
    if let Some(c) = re.captures(trimmed) {
        return Ok(unescape(c.get(1).unwrap().as_str()));
    }
    if let Some(c) = re1.captures(trimmed) {
        return Ok(unescape(c.get(1).unwrap().as_str()));
    }
    Err(ui_err(line, format!("需要字符串字面量:{text}")))
}

/// 顶层逗号切分（忽略括号/字符串内）。
fn split_top(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut parts: Vec<String> = vec![];
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
        } else if c == b'(' || c == b'[' || c == b'{' {
            depth += 1;
        } else if c == b')' || c == b']' || c == b'}' {
            depth -= 1;
        } else if c == b',' && depth <= 0 {
            parts.push(text[start..i].trim().to_string());
            start = i + 1;
        }
        i += 1;
    }
    if start <= text.len() {
        parts.push(text[start..].trim().to_string());
    }
    if parts.len() == 1 && parts[0].is_empty() {
        parts.pop();
    }
    parts
}

/// 取调用括号内的参数（要求语句以 prefix 开头、以 ) 收尾）。
fn call_args(statement: &Statement, prefix: &Regex) -> GenUiResult<Vec<String>> {
    let (text, line) = statement;
    if !prefix.is_match(text) {
        return Err(ui_err(*line, format!("无法识别的语句:{text}")));
    }
    let open = text.find('(').ok_or_else(|| ui_err(*line, "缺左括号"))?;
    let inner = &text[open + 1..text.rfind(')').ok_or_else(|| ui_err(*line, "缺右括号"))?];
    Ok(split_top(inner))
}

#[derive(Debug, Clone, Default)]
struct Symbol {
    key: String,
    item: bool,
    derived: bool,
    expr: Option<Expr>,
}

type Symbols = BTreeMap<String, Symbol>;

fn has_state_key(symbols: &Symbols, key: &str) -> bool {
    symbols.values().any(|s| s.key == key && !s.item)
}

/// 表达式优先级级联：or → and → 比较 → 加减/拼接 → 乘除 → 一元 ! → 原子。
fn parse_expression(text: &str, symbols: &Symbols, line: usize) -> GenUiResult<Expr> {
    parse_binary(text, symbols, line, 0)
}

const BINARY_LEVELS: [&[&str]; 5] = [
    &["||"],
    &["&&"],
    &["==", "!=", ">=", "<=", ">", "<"],
    &["+", "-"],
    &["*", "/"],
];

fn op_kind(op: &str) -> BinOp {
    match op {
        // tsx 符号形式
        "||" => BinOp::Or,
        "&&" => BinOp::And,
        "==" => BinOp::Eq,
        "!=" => BinOp::Neq,
        ">=" | ">" => BinOp::Gt,
        "<=" | "<" => BinOp::Lt,
        "+" => BinOp::Add,
        "-" => BinOp::Sub,
        "*" => BinOp::Mul,
        "/" => BinOp::Div,
        // jsonl wire 形式（BINARY_OPS）
        "add" => BinOp::Add,
        "sub" => BinOp::Sub,
        "mul" => BinOp::Mul,
        "div" => BinOp::Div,
        "eq" => BinOp::Eq,
        "neq" => BinOp::Neq,
        "gt" => BinOp::Gt,
        "lt" => BinOp::Lt,
        "and" => BinOp::And,
        "or" => BinOp::Or,
        _ => unreachable!(),
    }
}

fn parse_binary(text: &str, symbols: &Symbols, line: usize, level: usize) -> GenUiResult<Expr> {
    if level >= BINARY_LEVELS.len() {
        return parse_unary(text, symbols, line);
    }
    let trimmed = text.trim();
    if let Some((index, op)) = find_top(trimmed, BINARY_LEVELS[level]) {
        let left = parse_binary(&trimmed[..index], symbols, line, level)?;
        let right = parse_binary(&trimmed[index + op.len()..], symbols, line, level)?;
        return Ok(Expr::Bin(op_kind(op), Box::new(left), Box::new(right)));
    }
    parse_binary(trimmed, symbols, line, level + 1)
}

/// 深度 0、字符串外、最左操作符（左结合）；多字符 op 按顺序优先匹配。
fn find_top<'a>(text: &'a str, ops: &[&'a str]) -> Option<(usize, &'a str)> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c >= 0x80 {
            i += 1;
            continue; // 非 ASCII 只在字符串内（已整体跳过）或非法内容，逐字节略过
        }
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == b'(' || c == b'[' {
            depth += 1;
        } else if c == b')' || c == b']' {
            depth -= 1;
        } else if depth == 0 && i > 0 {
            for op in ops {
                if text[i..].starts_with(op) && is_operator_position(text, i, op) {
                    return Some((i, *op));
                }
            }
        }
        i += 1;
    }
    None
}

/// 操作符两侧（越过空白）必须是操作数边界；单字符 op 遇双字符前缀时让位。
fn is_operator_position(text: &str, index: usize, op: &str) -> bool {
    let bytes = text.as_bytes();
    let mut left = index as isize - 1;
    while left >= 0 && (bytes[left as usize] as char).is_ascii_whitespace() {
        left -= 1;
    }
    let mut right = index + op.len();
    while right < bytes.len() && (bytes[right] as char).is_ascii_whitespace() {
        right += 1;
    }
    if left < 0 || right >= bytes.len() {
        return false;
    }
    let two_char = {
        let two = &bytes[index..(index + 2).min(bytes.len())];
        two == b"==" || two == b"!=" || two == b">=" || two == b"<=" || two == b"&&" || two == b"||"
    };
    if two_char && op.len() == 1 {
        return false;
    }
    let l = bytes[left as usize];
    let r = bytes[right];
    (is_ident_char(l) || l == b')' || l == b'"')
        && (is_ident_char(r) || r == b'(' || r == b'!' || r == b'"' || r == b'-')
}

fn parse_unary(text: &str, symbols: &Symbols, line: usize) -> GenUiResult<Expr> {
    let trimmed = text.trim();
    if trimmed.starts_with('!') && !trimmed.starts_with("!=") {
        return Ok(Expr::Not(Box::new(parse_unary(&trimmed[1..], symbols, line)?)));
    }
    parse_primary(trimmed, symbols, line)
}

fn parse_primary(text: &str, symbols: &Symbols, line: usize) -> GenUiResult<Expr> {
    let mut trimmed = text.trim();
    let mut stripped = false;
    // 括号复合表达式：整体被一对括号包住时剥掉，重新进入完整级联。
    loop {
        if !trimmed.starts_with('(') {
            break;
        }
        let bytes = trimmed.as_bytes();
        let mut depth = 0i32;
        let mut close = None;
        for (i, c) in bytes.iter().enumerate() {
            if *c == b'(' {
                depth += 1;
            }
            if *c == b')' {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
        }
        match close {
            Some(i) if i == trimmed.len() - 1 => {
                trimmed = trimmed[1..trimmed.len() - 1].trim();
                if trimmed.is_empty() {
                    return Err(ui_err(line, "空括号表达式"));
                }
                stripped = true;
            }
            _ => break,
        }
    }
    let re_str = Regex::new(r#"^"(?:[^"\\]|\\.)*"$"#).unwrap();
    if re_str.is_match(trimmed) {
        return Ok(Expr::Lit(Value::String(unescape(&trimmed[1..trimmed.len() - 1]))));
    }
    let re_num = Regex::new(r"^[+-]?\d+(?:\.\d+)?$").unwrap();
    if re_num.is_match(trimmed) {
        return Ok(Expr::Lit(num_value(trimmed.parse::<f64>().unwrap())));
    }
    if trimmed == "true" || trimmed == "false" {
        return Ok(Expr::Lit(Value::Bool(trimmed == "true")));
    }
    let re_ident = Regex::new(r"^[A-Za-z_$][\w$]*$").unwrap();
    if re_ident.is_match(trimmed) {
        let symbol = symbols.get(trimmed).ok_or_else(|| ui_err(line, format!("表达式引用未知状态 {trimmed}")))?;
        if symbol.item {
            return Ok(Expr::Item(trimmed.to_string()));
        }
        if symbol.derived {
            return Ok(symbol.expr.clone().expect("derived 必带 expr"));
        }
        return Ok(Expr::State(symbol.key.clone()));
    }
    if stripped {
        return parse_binary(trimmed, symbols, line, 0);
    }
    Err(ui_err(line, format!("不支持的表达式:{trimmed}")))
}

fn parse_action(text: &str, symbols: &Symbols, line: usize) -> GenUiResult<Action> {
    let trimmed = text.trim();
    let re_reset = Regex::new(r"^ui\.reset\(\s*\)$").unwrap();
    if re_reset.is_match(trimmed) {
        return Ok(Action::Reset);
    }
    let re_set = Regex::new(r#"^ui\.set\(\s*("(?:[^"\\]|\\.)*")\s*,\s*([\s\S]+)\s*\)$"#).unwrap();
    if let Some(c) = re_set.captures(trimmed) {
        let key = parse_string_literal(c.get(1).unwrap().as_str(), line)?;
        if !has_state_key(symbols, &key) {
            return Err(ui_err(line, format!("动作引用未知状态 {key}")));
        }
        return Ok(Action::Set { key, value: parse_expression(c.get(2).unwrap().as_str(), symbols, line)? });
    }
    let re_follow = Regex::new(r#"^ui\.followUp\(\s*("(?:[^"\\]|\\.)*")\s*(?:,\s*(\{[\s\S]*\})\s*)?\)$"#).unwrap();
    if let Some(c) = re_follow.captures(trimmed) {
        let prompt = parse_string_literal(c.get(1).unwrap().as_str(), line)?;
        let keys: Vec<&String> = symbols.values().map(|s| &s.key).collect();
        let mut fields: Vec<(String, String)> = vec![];
        for (label, raw) in object_entries(c.get(2).map(|m| m.as_str()).unwrap_or("{}"), line)? {
            let name = raw.trim();
            let re_ident = Regex::new(r"^[A-Za-z_$][\w$]*$").unwrap();
            let re_quoted = Regex::new(r#"^"(?:[^"\\]|\\.)*"$"#).unwrap();
            let key = if re_ident.is_match(name) {
                symbols.get(name).map(|s| s.key.clone())
            } else if re_quoted.is_match(name) {
                Some(unescape(&name[1..name.len() - 1]))
            } else {
                None
            };
            let Some(key) = key else {
                return Err(ui_err(line, format!("followUp 字段 {label} 引用未知状态 {name}（字段值应为状态名或其 \"key\"）")));
            };
            if !keys.contains(&&key) {
                return Err(ui_err(line, format!("followUp 字段 {label} 引用未知状态 {name}")));
            }
            fields.push((label, key));
        }
        return Ok(Action::FollowUp { prompt, fields });
    }
    let re_copy = Regex::new(r"^ui\.copy\(\s*([\s\S]+)\s*\)$").unwrap();
    if let Some(c) = re_copy.captures(trimmed) {
        return Ok(Action::Copy { value: parse_expression(c.get(1).unwrap().as_str(), symbols, line)? });
    }
    let re_link = Regex::new(r#"^ui\.openLink\(\s*("(?:[^"\\]|\\.)*")\s*\)$"#).unwrap();
    if let Some(c) = re_link.captures(trimmed) {
        let url = parse_string_literal(c.get(1).unwrap().as_str(), line)?;
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(ui_err(line, format!("openLink 只允许 http(s) 链接:{url}")));
        }
        return Ok(Action::OpenLink(url));
    }
    let re_keyed = Regex::new(r#"^ui\.(append|remove)\(\s*("(?:[^"\\]|\\.)*")\s*,\s*([\s\S]+)\s*\)$"#).unwrap();
    if let Some(c) = re_keyed.captures(trimmed) {
        let key = parse_string_literal(c.get(2).unwrap().as_str(), line)?;
        if !has_state_key(symbols, &key) {
            return Err(ui_err(line, format!("动作引用未知状态 {key}")));
        }
        let value = parse_expression(c.get(3).unwrap().as_str(), symbols, line)?;
        return Ok(if c.get(1).unwrap().as_str() == "append" {
            Action::Append { key, value }
        } else {
            Action::Remove { key, value }
        });
    }
    Err(ui_err(line, format!("不支持的动作:{trimmed}")))
}

/// 对象字面量顶层键值切分；键允许 IDENT 或带引号字符串（followUp 标签可中文）。
fn object_entries(text: &str, line: usize) -> GenUiResult<Vec<(String, String)>> {
    let stripped = text.trim().trim_start_matches('{').trim_end_matches('}');
    let key_re = Regex::new(r#"(?:"((?:[^"\\]|\\.)*)"|([A-Za-z_$][\w$]*))\s*:\s*([\s\S]+)$"#).unwrap();
    let mut entries = vec![];
    for part in split_top(stripped) {
        let Some(c) = key_re.captures(part.trim()) else {
            return Err(ui_err(line, format!("无法解析选项:{part}")));
        };
        let key = c.get(1).or(c.get(2)).unwrap().as_str().to_string();
        entries.push((key, c.get(3).unwrap().as_str().to_string()));
    }
    Ok(entries)
}

/// JSX 自闭合元素：<Type name={expr} name="str" ... />；不支持 children 语法与 spread。
fn parse_jsx_node(text: &str, symbols: &Symbols, line: usize) -> GenUiResult<(String, BTreeMap<String, PropValue>)> {
    let re = Regex::new(r"^<\s*([A-Za-z][\w]*)\s*([\s\S]*)/>\s*$").unwrap();
    let Some(c) = re.captures(text.trim()) else {
        return Err(ui_err(line, format!("只支持自闭合 JSX 元素:{text}")));
    };
    let type_ = c.get(1).unwrap().as_str().to_string();
    let rest = c.get(2).map(|m| m.as_str()).unwrap_or("");
    let mut props: BTreeMap<String, PropValue> = BTreeMap::new();
    let bytes = rest.as_bytes();
    let mut index = 0usize;
    let re_name = Regex::new(r#"^([A-Za-z_$][\w$]*)\s*=\s*"#).unwrap();
    while index < bytes.len() {
        if !rest.is_char_boundary(index) {
            index += 1;
            continue;
        }
        while index < bytes.len() && (bytes[index] as char).is_ascii_whitespace() {
            index += 1;
        }
        let Some(name_match) = re_name.captures(&rest[index..]) else {
            if rest[index..].trim().is_empty() {
                break;
            }
            return Err(ui_err(line, format!("无法解析属性:{}", &rest[index..(index + 30).min(rest.len())])));
        };
        let name = name_match.get(1).unwrap().as_str().to_string();
        index += name_match.get(0).unwrap().len();
        if !rest[index..].starts_with('{') {
            let re_strval = Regex::new(r#"^"((?:[^"\\]|\\.)*)""#).unwrap();
            if let Some(sv) = re_strval.captures(&rest[index..]) {
                props.insert(name, PropValue::Lit(Value::String(unescape(sv.get(1).unwrap().as_str()))));
                index += sv.get(0).unwrap().len();
                continue;
            }
            return Err(ui_err(line, format!("属性值需要 {{expr}} 或字符串:{}", &rest[index..(index + 30).min(rest.len())])));
        }
        // 平衡花括号取值；扫描从 '{' 本身开始（与 TS 一致，外层计入深度），
        // 字符串字面量整体跳过。
        let mut depth = 0i32;
        let brace_start = index;
        while index < bytes.len() {
            let ch = bytes[index];
            if ch == b'"' || ch == b'\'' || ch == b'`' {
                let quote = ch;
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    if bytes[index] == b'\\' {
                        index += 2;
                        continue;
                    }
                    index += 1;
                }
                index += 1;
                continue;
            }
            if ch == b'{' {
                depth += 1;
            }
            if ch == b'}' {
                depth -= 1;
                if depth == 0 {
                    index += 1;
                    break;
                }
            }
            index += 1;
        }
        if depth != 0 {
            return Err(ui_err(line, format!("属性值花括号不闭合:{name}")));
        }
        let raw = rest[brace_start + 1..index - 1].trim();
        let re_bind = Regex::new(r#"^ui\.bind\(\s*("(?:[^"\\]|\\.)*")\s*\)$"#).unwrap();
        let re_for = Regex::new(r#"^ui\.for\(\s*("(?:[^"\\]|\\.)*")\s*,\s*("(?:[^"\\]|\\.)*")\s*\)$"#).unwrap();
        let re_action = Regex::new(r"^ui\.(?:set|reset|followUp|copy|openLink|append|remove)\(").unwrap();
        let re_list = Regex::new(r#"^\[\s*(?:"(?:[^"\\]|\\.)*"\s*,?\s*)*\]$"#).unwrap();
        if let Some(b) = re_bind.captures(raw) {
            props.insert(name, PropValue::Binding(parse_string_literal(b.get(1).unwrap().as_str(), line)?));
        } else if name == "repeat" {
            let Some(f) = re_for.captures(raw) else {
                return Err(ui_err(line, "repeat 需要 ui.for(\"list态\", \"循环变量\")"));
            };
            let source = parse_string_literal(f.get(1).unwrap().as_str(), line)?;
            let as_ = parse_string_literal(f.get(2).unwrap().as_str(), line)?;
            props.insert(name, PropValue::Repeat(source, as_));
        } else if re_action.is_match(raw) {
            props.insert(name, PropValue::Action(parse_action(raw, symbols, line)?));
        } else if re_list.is_match(raw) {
            let items: Vec<String> = if raw.len() > 2 {
                raw[1..raw.len() - 1]
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| parse_string_literal(s.trim(), line))
                    .collect::<GenUiResult<_>>()?
            } else {
                vec![]
            };
            props.insert(name, PropValue::List(items));
        } else {
            props.insert(name, PropValue::Expr(parse_expression(raw, symbols, line)?));
        }
    }
    Ok((type_, props))
}

/// 解析整个 surface 源码为 IR。ui.surface 必须第一条、ui.end 必须最后。
pub fn parse_surface(source: &str, final_: bool) -> GenUiResult<SurfaceIr> {
    let statements = split_statements(source, !final_);
    if statements.is_empty() {
        return Err(GenUiError { message: "UI 源码为空".into(), line: None });
    }
    let mut symbols: Symbols = BTreeMap::new();
    let mut states: BTreeMap<String, StateDecl> = BTreeMap::new();
    let mut order: Vec<String> = vec![];
    let mut nodes: BTreeMap<String, IrNode> = BTreeMap::new();
    let mut surface_id: Option<String> = None;
    let mut title = None;
    let mut fallback = None;
    let mut root_id: Option<String> = None;
    let mut ended = false;

    // 预扫描 ui.for：循环变量先注册为 item 伪符号。
    let re_for_prescan = Regex::new(r#"ui\.for\(\s*"(?:[^"\\]|\\.)*"\s*,\s*"([^"\\]+)"\s*\)"#).unwrap();
    for cap in re_for_prescan.captures_iter(source) {
        let var_name = cap.get(1).unwrap().as_str().to_string();
        symbols.entry(var_name.clone()).or_insert(Symbol { key: var_name, item: true, ..Default::default() });
    }

    for (index, statement) in statements.iter().enumerate() {
        let (text, line) = statement;
        if ended {
            return Err(ui_err(*line, "ui.end() 之后不允许再有语句"));
        }
        if index == 0 {
            if !text.starts_with("ui.surface(") {
                return Err(ui_err(*line, "第一条语句必须是 ui.surface(...)"));
            }
            let args = call_args(statement, &Regex::new(r"^ui\.surface\(").unwrap())?;
            surface_id = Some(parse_string_literal(args.first().map(String::as_str).unwrap_or(""), *line)?);
            if let Some(opts) = args.get(1) {
                for (key, value) in object_entries(opts, *line)? {
                    match key.as_str() {
                        "root" => root_id = Some(parse_string_literal(&value, *line)?),
                        "title" => title = Some(parse_string_literal(&value, *line)?),
                        "fallback" => fallback = Some(parse_string_literal(&value, *line)?),
                        _ => return Err(ui_err(*line, format!("ui.surface 不支持选项 {key}"))),
                    }
                }
            }
            continue;
        }
        let re_state = Regex::new(r#"^const\s+([A-Za-z_$][\w$]*)\s*=\s*ui\.state\("#).unwrap();
        if let Some(sm) = re_state.captures(text) {
            let symbol = sm.get(1).unwrap().as_str().to_string();
            let inner_start = text.find('(').unwrap() + 1;
            let inner = &text[inner_start..text.rfind(')').ok_or_else(|| ui_err(*line, "缺右括号"))?];
            let args = split_top(inner);
            let key = parse_string_literal(args.first().map(String::as_str).unwrap_or(""), *line)?;
            let type_text = parse_string_literal(args.get(1).map(String::as_str).unwrap_or(""), *line)?;
            let type_ = match type_text.as_str() {
                "string" => StateType::Str,
                "number" => StateType::Num,
                "integer" => StateType::Integer,
                "boolean" => StateType::Bool,
                "list" => StateType::List,
                _ => return Err(ui_err(*line, format!("不支持的状态类型 {type_text}"))),
            };
            if args.len() != 3 {
                return Err(ui_err(*line, "ui.state(key, type, initial) 需要 3 个参数"));
            }
            let initial_src = args[2].trim();
            let initial = match type_ {
                StateType::Str => Value::String(parse_string_literal(&args[2], *line)?),
                StateType::Bool => {
                    if initial_src == "true" {
                        Value::Bool(true)
                    } else if initial_src == "false" {
                        Value::Bool(false)
                    } else {
                        return Err(ui_err(*line, "boolean 初始值必须是 true/false"));
                    }
                }
                StateType::List => {
                    let re_list = Regex::new(r#"^\[\s*(?:"(?:[^"\\]|\\.)*"\s*,?\s*)*\]$"#).unwrap();
                    if !re_list.is_match(initial_src) {
                        return Err(ui_err(*line, "list 初始值必须是字符串数组"));
                    }
                    let items: Vec<String> = if initial_src.len() > 2 {
                        initial_src[1..initial_src.len() - 1]
                            .split(',')
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| parse_string_literal(s.trim(), *line))
                            .collect::<GenUiResult<_>>()?
                    } else {
                        vec![]
                    };
                    Value::Array(items.into_iter().map(Value::String).collect())
                }
                StateType::Num | StateType::Integer => {
                    let n: f64 = initial_src.parse().map_err(|_| ui_err(*line, "number 初始值必须是有限数"))?;
                    if !n.is_finite() {
                        return Err(ui_err(*line, "number 初始值必须是有限数"));
                    }
                    if type_ == StateType::Integer && !(n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n <= 9.007_199_254_740_991e15) {
                        return Err(ui_err(*line, "integer 初始值必须是安全整数"));
                    }
                    Value::Number(num_value(n).as_number().cloned().unwrap_or(serde_json::Number::from(0)))
                }
            };
            if symbols.contains_key(&symbol) || states.contains_key(&key) {
                return Err(ui_err(*line, format!("重复的状态声明 {symbol}/{key}")));
            }
            symbols.insert(symbol, Symbol { key: key.clone(), ..Default::default() });
            states.insert(key, StateDecl { type_, initial });
            continue;
        }
        if text.starts_with("ui.node(") {
            let args = call_args(statement, &Regex::new(r"^ui\.node\(").unwrap())?;
            if args.len() != 2 {
                return Err(ui_err(*line, "ui.node(id, <Component ... />) 需要 2 个参数"));
            }
            let id = parse_string_literal(&args[0], *line)?;
            let (type_, props) = parse_jsx_node(&args[1], &symbols, *line)?;
            if !CATALOG.contains(&type_.as_str()) {
                return Err(ui_err(*line, format!("未知组件类型:{type_}")));
            }
            if nodes.contains_key(&id) || order.contains(&id) {
                return Err(ui_err(*line, format!("重复的节点 id {id}")));
            }
            let mut repeat = None;
            let mut props = props;
            if let Some(PropValue::Repeat(source, as_)) = props.remove("repeat") {
                repeat = Some((source, as_));
            }
            nodes.insert(id.clone(), IrNode { id: id.clone(), type_, props, line: *line, repeat });
            order.push(id);
            continue;
        }
        let re_derive = Regex::new(r#"^const\s+([A-Za-z_$][\w$]*)\s*=\s*ui\.derive\(([\s\S]+)\)$"#).unwrap();
        if let Some(dm) = re_derive.captures(text) {
            let symbol = dm.get(1).unwrap().as_str().to_string();
            if symbols.contains_key(&symbol) {
                return Err(ui_err(*line, format!("重复的声明 {symbol}")));
            }
            let expr = parse_expression(dm.get(2).unwrap().as_str(), &symbols, *line)?;
            symbols.insert(symbol, Symbol { key: String::new(), derived: true, expr: Some(expr), item: false });
            continue;
        }
        if text.starts_with("ui.end(") {
            if !Regex::new(r"^ui\.end\(\s*\)$").unwrap().is_match(text) {
                return Err(ui_err(*line, "ui.end() 不接受参数"));
            }
            ended = true;
            continue;
        }
        if Regex::new(r"^ui\.(set|reset|followUp|copy|openLink|append|remove)\(").unwrap().is_match(text) {
            return Err(ui_err(*line, "动作只能作为事件值（如 onPress）出现"));
        }
        return Err(ui_err(*line, format!("无法识别的语句:{text}")));
    }

    let Some(surface_id) = surface_id else {
        return Err(GenUiError { message: "缺少 ui.surface(...)".into(), line: None });
    };
    if !ended && final_ {
        return Err(GenUiError { message: "缺少 ui.end()".into(), line: None });
    }
    if order.len() > MAX_NODES {
        return Err(GenUiError { message: format!("节点数超过预算（{MAX_NODES}）"), line: None });
    }
    Ok(SurfaceIr { surface_id, title, fallback, root_id, states, order, nodes, warnings: vec![] })
}

// ─────────────────────────────────────────────────────────────────────────────
// jsonl 载体（对齐 compiler/jsonl.ts）
// ─────────────────────────────────────────────────────────────────────────────

const BINARY_OPS: [&str; 10] = ["add", "sub", "mul", "div", "eq", "neq", "gt", "lt", "and", "or"];
const ACTION_KINDS: [&str; 7] = ["set", "reset", "followUp", "copy", "openLink", "append", "remove"];

#[derive(Default)]
struct WireCtx {
    derives: BTreeMap<String, Expr>,
}

fn wire_expr(value: &Value, line: usize, ctx: &WireCtx) -> GenUiResult<Expr> {
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => Ok(Expr::Lit(value.clone())),
        Value::Object(record) => {
            if let Some(lit) = record.get("lit") {
                if lit.is_string() || lit.is_number() || lit.is_boolean() {
                    return Ok(Expr::Lit(lit.clone()));
                }
                return Err(ui_err(line, format!("无法解析表达式:{}", lit)));
            }
            if let Some(Value::String(key)) = record.get("state") {
                // 派生态按声明顺序内联；wire 上不出现 derive 引用。
                if let Some(expr) = ctx.derives.get(key) {
                    return Ok(expr.clone());
                }
                return Ok(Expr::State(key.clone()));
            }
            if let Some(Value::String(item)) = record.get("item") {
                return Ok(Expr::Item(item.clone()));
            }
            if let Some(inner) = record.get("not") {
                return Ok(Expr::Not(Box::new(wire_expr(inner, line, ctx)?)));
            }
            if let Some(Value::String(op)) = record.get("op") {
                if BINARY_OPS.contains(&op.as_str()) {
                    let left = wire_expr(record.get("left").unwrap_or(&Value::Null), line, ctx)?;
                    let right = wire_expr(record.get("right").unwrap_or(&Value::Null), line, ctx)?;
                    return Ok(Expr::Bin(op_kind(op), Box::new(left), Box::new(right)));
                }
            }
            Err(ui_err(line, format!("无法解析表达式:{value}")))
        }
        _ => Err(ui_err(line, format!("无法解析表达式:{value}"))),
    }
}

fn wire_action(value: &Value, line: usize, ctx: &WireCtx) -> GenUiResult<Action> {
    let Value::Object(record) = value else {
        return Err(ui_err(line, format!("无法解析动作:{value}")));
    };
    if record.get("reset") == Some(&Value::Bool(true)) {
        return Ok(Action::Reset);
    }
    if let Some(Value::String(key)) = record.get("set") {
        return Ok(Action::Set { key: key.clone(), value: wire_expr(record.get("value").unwrap_or(&Value::Null), line, ctx)? });
    }
    if record.contains_key("copy") {
        return Ok(Action::Copy { value: wire_expr(record.get("copy").unwrap_or(&Value::Null), line, ctx)? });
    }
    if let Some(Value::String(url)) = record.get("openLink") {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(ui_err(line, format!("openLink 只允许 http(s) 链接:{url}")));
        }
        return Ok(Action::OpenLink(url.clone()));
    }
    if let Some(key) = record.get("append").or_else(|| record.get("remove")).and_then(Value::as_str) {
        if key.is_empty() {
            return Err(ui_err(line, "append/remove 需要状态 key"));
        }
        let value = wire_expr(record.get("value").unwrap_or(&Value::Null), line, ctx)?;
        return Ok(if record.contains_key("append") {
            Action::Append { key: key.to_string(), value }
        } else {
            Action::Remove { key: key.to_string(), value }
        });
    }
    if let Some(Value::String(prompt)) = record.get("followUp") {
        let fields_raw = record.get("fields");
        if fields_raw.is_some_and(|f| !f.is_array()) {
            return Err(ui_err(line, "followUp fields 必须是数组"));
        }
        let mut fields = vec![];
        for item in fields_raw.and_then(Value::as_array).unwrap_or(&vec![]) {
            let Some(Value::String(label)) = item.get("label") else {
                return Err(ui_err(line, "followUp 字段需要 label/key"));
            };
            let Some(Value::String(key)) = item.get("key") else {
                return Err(ui_err(line, "followUp 字段需要 label/key"));
            };
            if key.is_empty() {
                return Err(ui_err(line, "followUp 字段需要 label/key"));
            }
            fields.push((label.clone(), key.clone()));
        }
        return Ok(Action::FollowUp { prompt: prompt.clone(), fields });
    }
    Err(ui_err(line, format!("无法解析动作:{value}")))
}

fn wire_prop(value: &Value, line: usize, ctx: &WireCtx) -> GenUiResult<PropValue> {
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => Ok(PropValue::Lit(value.clone())),
        Value::Array(items) => {
            if !items.iter().all(Value::is_string) {
                return Err(ui_err(line, "children 数组必须是字符串 id"));
            }
            Ok(PropValue::List(items.iter().filter_map(Value::as_str).map(str::to_string).collect()))
        }
        Value::Object(record) => {
            if let Some(expr) = record.get("expr") {
                return Ok(PropValue::Expr(wire_expr(expr, line, ctx)?));
            }
            if let Some(Value::String(key)) = record.get("bind") {
                if key.is_empty() {
                    return Err(ui_err(line, "绑定需要状态 key"));
                }
                return Ok(PropValue::Binding(key.clone()));
            }
            if let (Some(Value::String(source)), Some(Value::String(as_))) = (record.get("source"), record.get("as")) {
                if source.is_empty() || as_.is_empty() {
                    return Err(ui_err(line, "repeat 需要 source/as"));
                }
                return Ok(PropValue::Repeat(source.clone(), as_.clone()));
            }
            if ACTION_KINDS.iter().any(|kind| record.contains_key(*kind)) {
                return Ok(PropValue::Action(wire_action(value, line, ctx)?));
            }
            // 裸表达式 wire（{state}/{lit}/{item}/{not}/{op:二元}）直接作为 prop 值。
            if record.contains_key("state")
                || record.contains_key("lit")
                || record.contains_key("item")
                || record.contains_key("not")
                || record.get("op").and_then(Value::as_str).is_some_and(|op| BINARY_OPS.contains(&op))
            {
                return Ok(PropValue::Expr(wire_expr(value, line, ctx)?));
            }
            Err(ui_err(line, format!("无法解析属性值:{value}")))
        }
        _ => Err(ui_err(line, format!("无法解析属性值:{value}"))),
    }
}

pub fn parse_surface_jsonl(source: &str, final_: bool) -> GenUiResult<SurfaceIr> {
    let mut states: BTreeMap<String, StateDecl> = BTreeMap::new();
    let mut order: Vec<String> = vec![];
    let mut nodes: BTreeMap<String, IrNode> = BTreeMap::new();
    let mut ctx = WireCtx::default();
    let mut surface_id: Option<String> = None;
    let mut title = None;
    let mut fallback = None;
    let mut root_id: Option<String> = None;
    let mut closed = false;

    let lines: Vec<&str> = source.split('\n').collect();
    for (index, raw) in lines.iter().enumerate() {
        let text = raw.trim();
        if text.is_empty() {
            continue;
        }
        let line_no = index + 1;
        let Ok(record) = serde_json::from_str::<Value>(text) else {
            // 流式容尾：最后一行半截 JSON 等下一帧补齐。
            if !final_ && index == lines.len() - 1 {
                break;
            }
            return Err(ui_err(line_no, format!("jsonl 行不是合法 JSON:{}", &text[..text.len().min(40)])));
        };
        let Some(op) = record.get("op").and_then(Value::as_str) else {
            return Err(ui_err(line_no, "jsonl record 缺少 op"));
        };
        if closed {
            return Err(ui_err(line_no, "surface.close 之后不允许再有 record"));
        }
        match op {
            "surface.open" => {
                if surface_id.is_some() {
                    return Err(ui_err(line_no, "重复的 surface.open"));
                }
                let Some(id) = record.get("surface").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                    return Err(ui_err(line_no, "surface.open 缺少 surface id"));
                };
                surface_id = Some(id.to_string());
                if let Some(Value::String(root)) = record.get("root") {
                    root_id = Some(root.clone());
                }
                if let Some(Value::String(t)) = record.get("title") {
                    title = Some(t.clone());
                }
                if let Some(Value::String(f)) = record.get("fallback") {
                    fallback = Some(f.clone());
                }
            }
            "state" => {
                if surface_id.is_none() {
                    return Err(ui_err(line_no, "state 必须在 surface.open 之后"));
                }
                let Some(Value::String(key)) = record.get("key") else {
                    return Err(ui_err(line_no, "state 缺少 key"));
                };
                let type_text = record.get("type").and_then(Value::as_str).unwrap_or("");
                let type_ = match type_text {
                    "string" => StateType::Str,
                    "number" => StateType::Num,
                    "integer" => StateType::Integer,
                    "boolean" => StateType::Bool,
                    "list" => StateType::List,
                    _ => return Err(ui_err(line_no, format!("不支持的状态类型 {type_text}"))),
                };
                if states.contains_key(key) {
                    return Err(ui_err(line_no, format!("重复的状态声明 {key}")));
                }
                let initial = record.get("initial").cloned().unwrap_or(Value::Null);
                let ok = match type_ {
                    StateType::List => initial.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
                    StateType::Str => initial.is_string(),
                    StateType::Bool => initial.is_boolean(),
                    StateType::Num | StateType::Integer => {
                        initial.as_f64().is_some_and(|n| n.is_finite())
                    }
                };
                if !ok {
                    return Err(ui_err(line_no, format!("状态 {key} 初始值与类型不符")));
                }
                if type_ == StateType::Integer
                    && !initial.as_f64().is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n >= 0.0)
                {
                    return Err(ui_err(line_no, "integer 初始值必须是安全整数"));
                }
                states.insert(key.clone(), StateDecl { type_, initial });
            }
            "derive" => {
                if surface_id.is_none() {
                    return Err(ui_err(line_no, "derive 必须在 surface.open 之后"));
                }
                let Some(Value::String(key)) = record.get("key") else {
                    return Err(ui_err(line_no, "derive 缺少 key"));
                };
                if ctx.derives.contains_key(key) || states.contains_key(key) {
                    return Err(ui_err(line_no, format!("重复的声明 {key}")));
                }
                let expr = wire_expr(record.get("expr").unwrap_or(&Value::Null), line_no, &ctx)?;
                ctx.derives.insert(key.clone(), expr);
            }
            "node" => {
                if surface_id.is_none() {
                    return Err(ui_err(line_no, "node 必须在 surface.open 之后"));
                }
                let Some(Value::String(id)) = record.get("id") else {
                    return Err(ui_err(line_no, "node 缺少 id"));
                };
                let Some(Value::String(component)) = record.get("component") else {
                    return Err(ui_err(line_no, "node 缺少 component"));
                };
                if let Some(props) = record.get("props") {
                    if !props.is_object() {
                        return Err(ui_err(line_no, "node props 必须是对象"));
                    }
                }
                if nodes.contains_key(id) || order.contains(id) {
                    return Err(ui_err(line_no, format!("重复的节点 id {id}")));
                }
                let mut decoded: BTreeMap<String, PropValue> = BTreeMap::new();
                let mut repeat = None;
                for (name, value) in record.get("props").and_then(Value::as_object).unwrap_or(&Map::new()) {
                    let decoded_value = wire_prop(value, line_no, &ctx)?;
                    if name == "repeat" {
                        if let PropValue::Repeat(source, as_) = decoded_value {
                            repeat = Some((source, as_));
                            continue;
                        }
                    }
                    decoded.insert(name.clone(), decoded_value);
                }
                nodes.insert(id.clone(), IrNode { id: id.clone(), type_: component.clone(), props: decoded, line: line_no, repeat });
                order.push(id.clone());
            }
            "surface.close" => {
                if surface_id.is_none() {
                    return Err(ui_err(line_no, "surface.close 必须在 surface.open 之后"));
                }
                closed = true;
            }
            other => return Err(ui_err(line_no, format!("未知的 jsonl op:{other}"))),
        }
    }
    let Some(surface_id) = surface_id else {
        return Err(GenUiError { message: "jsonl 缺少 surface.open".into(), line: None });
    };
    if !closed && final_ {
        return Err(GenUiError { message: "jsonl 缺少 surface.close".into(), line: None });
    }
    if order.len() > MAX_NODES {
        return Err(GenUiError { message: format!("节点数超过预算（{MAX_NODES}）"), line: None });
    }
    Ok(SurfaceIr { surface_id, title, fallback, root_id, states, order, nodes, warnings: vec![] })
}

// ─────────────────────────────────────────────────────────────────────────────
// 编译入口与 IR → A2UI wire 降级（对齐 compiler/index.ts + v2/translate.ts）
// ─────────────────────────────────────────────────────────────────────────────

/// 源码 → 校验过的 SurfaceIR（parse → normalize → validate → 软告警）。
pub fn compile_source(source: &str, format: UiFormat, final_: bool) -> GenUiResult<SurfaceIr> {
    let mut ir = match format {
        UiFormat::Jsonl => parse_surface_jsonl(source, final_)?,
        UiFormat::Tsx => parse_surface(source, final_)?,
    };
    normalize_bindings(&mut ir);
    validate_surface(&mut ir, !final_)?;
    if final_ {
        ir.warnings = surface_warnings(&ir);
    }
    Ok(ir)
}

fn state_path(key: &str) -> String {
    format!("/s/{key}")
}

/// v0.1 表达式 → `qaqh:arith/logic.*` 函数调用（无表达式语言进 wire）。
pub fn lower_expr(expr: &Expr) -> Value {
    match expr {
        Expr::Lit(v) => v.clone(),
        Expr::State(key) => json!({ "@path": state_path(key) }),
        // 模板项本身：list 态是 string[]，项即字符串 → 相对空路径。
        Expr::Item(_) => json!({ "@path": "" }),
        Expr::Not(value) => json!({ "@call": "qaqh:logic.not", "args": { "value": lower_expr(value) } }),
        Expr::Bin(op, left, right) => {
            let name = match op {
                BinOp::Add => "qaqh:arith.add",
                BinOp::Sub => "qaqh:arith.sub",
                BinOp::Mul => "qaqh:arith.mul",
                BinOp::Div => "qaqh:arith.div",
                BinOp::Eq => "qaqh:logic.eq",
                BinOp::Neq => "qaqh:logic.neq",
                BinOp::Gt => "qaqh:logic.gt",
                BinOp::Lt => "qaqh:logic.lt",
                BinOp::And => "qaqh:logic.and",
                BinOp::Or => "qaqh:logic.or",
            };
            json!({ "@call": name, "args": { "left": lower_expr(left), "right": lower_expr(right) } })
        }
    }
}

/// 动作降级：全部变成命名事件，renderer 本地拦截（qaqh.action. 前缀）。
pub fn lower_action(action: &Action) -> Value {
    match action {
        Action::Set { key, value } => {
            json!({ "name": "qaqh.action.set", "context": { "key": key, "value": lower_expr(value) } })
        }
        Action::Reset => json!({ "name": "qaqh.action.reset", "context": {} }),
        Action::Append { key, value } => {
            json!({ "name": "qaqh.action.append", "context": { "key": key, "value": lower_expr(value) } })
        }
        Action::Remove { key, value } => {
            json!({ "name": "qaqh.action.remove", "context": { "key": key, "value": lower_expr(value) } })
        }
        Action::Copy { value } => json!({ "name": "qaqh.action.copy", "context": { "value": lower_expr(value) } }),
        Action::OpenLink(url) => json!({ "name": "qaqh.action.openLink", "context": { "url": url } }),
        Action::FollowUp { prompt, fields } => json!({
            "name": "qaqh.action.followUp",
            "context": {
                "prompt": prompt,
                "fields": fields.iter().map(|(label, key)| json!({ "label": label, "key": key })).collect::<Vec<_>>(),
            }
        }),
    }
}

fn lower_prop(prop: &PropValue) -> Value {
    match prop {
        PropValue::Lit(v) => v.clone(),
        PropValue::Expr(e) => lower_expr(e),
        PropValue::Action(a) => lower_action(a),
        // 非 children 的 list 属性（目录里不存在）按字面量数组降级。
        PropValue::List(items) => Value::Array(items.iter().map(|s| Value::String(s.clone())).collect()),
        PropValue::Binding(key) => json!({ "@path": state_path(key) }),
        PropValue::Repeat(..) => unreachable!("repeat 在 parse 阶段已抽离"),
    }
}

fn to_wire_component(node: &IrNode) -> WireComponent {
    let mut props: Map<String, Value> = Map::new();
    let mut children: Option<Vec<String>> = None;
    let mut on_press = None;
    let mut show = None;
    for (name, prop) in &node.props {
        if name == "children" {
            if let PropValue::List(items) = prop {
                children = Some(items.clone());
            }
            continue; // 容器关系走 child/children 信封，不走属性表
        }
        if name == "onPress" {
            if let PropValue::Action(a) = prop {
                let event = lower_action(a);
                on_press = Some(ActionEvent {
                    event: EventSpec {
                        name: event["name"].as_str().unwrap_or("").to_string(),
                        user_message: None,
                        context: event.get("context").and_then(Value::as_object).cloned(),
                    },
                });
            }
            continue;
        }
        let lowered = lower_prop(prop);
        if name == "show" {
            show = Some(lowered);
        } else {
            props.insert(name.clone(), lowered);
        }
    }
    let metadata = node.repeat.as_ref().map(|(source, as_)| {
        let mut extensions = Map::new();
        extensions.insert(
            REPEAT_EXTENSION.to_string(),
            json!({ "path": state_path(source), "as": as_ }),
        );
        crate::Metadata { extensions: Some(extensions) }
    });
    WireComponent {
        id: node.id.clone(),
        component: node.type_.clone(),
        child: None,
        children: children.map(ChildList::Static),
        on_press,
        show,
        metadata,
        props,
    }
}

/// 组装规范根：A2UI 要求 id="root" 的组件存在（森林包一层 Stack）。
pub fn translate_components(ir: &SurfaceIr) -> Vec<WireComponent> {
    let mut components: Vec<WireComponent> = ir.order.iter().map(|id| to_wire_component(&ir.nodes[id])).collect();
    let inner_root = ir
        .root_id
        .clone()
        .or_else(|| forest_roots(ir).first().cloned())
        .unwrap_or_default();
    components.push(WireComponent {
        id: "root".to_string(),
        component: "Stack".to_string(),
        child: None,
        children: if inner_root.is_empty() { None } else { Some(ChildList::Static(vec![inner_root])) },
        on_press: None,
        show: None,
        metadata: None,
        props: Map::new(),
    });
    components
}

fn initial_states(ir: &SurfaceIr) -> Map<String, Value> {
    ir.states.iter().map(|(k, v)| (k.clone(), v.initial.clone())).collect()
}

/// seal（整帧）：createSurface 一次性带全组件（含包装根）与初始 dataModel。
pub fn translate_create(ir: &SurfaceIr) -> Message {
    Message {
        version: A2UI_VERSION.to_string(),
        create_surface: Some(CreateSurface {
            surface_id: ir.surface_id.clone(),
            catalog_id: Some(V2_CATALOG_ID.to_string()),
            send_data_model: false,
            components: translate_components(ir),
            data_model: Map::from_iter([("s".to_string(), Value::Object(initial_states(ir)))]),
        }),
        update_components: None,
        update_data_model: None,
        delete_surface: None,
        call_renderer_function: None,
        agent_function_response: None,
    }
}

/// 流式（每帧）：updateComponents 全量 + updateDataModel 整体替换 /s/。
pub fn translate_update(ir: &SurfaceIr) -> Vec<Message> {
    vec![
        Message {
            version: A2UI_VERSION.to_string(),
            create_surface: None,
            update_components: Some(UpdateComponents {
                surface_id: ir.surface_id.clone(),
                components: translate_components(ir),
            }),
            update_data_model: None,
            delete_surface: None,
            call_renderer_function: None,
            agent_function_response: None,
        },
        Message {
            version: A2UI_VERSION.to_string(),
            create_surface: None,
            update_components: None,
            update_data_model: Some(UpdateDataModel {
                surface_id: ir.surface_id.clone(),
                path: Some("/s".to_string()),
                value: Value::Object(initial_states(ir)),
            }),
            delete_surface: None,
            call_renderer_function: None,
            agent_function_response: None,
        },
    ]
}

/// 把一条 ContentPart::Ui 喂进 registry：首帧 create（对齐 SurfaceView2 的
/// `!created → translateCreate`），后续帧 update。返回 IR 供状态栏展示；
/// 解析失败返回具名错误（调用方降级占位卡）。
pub fn ingest_ui_part(
    registry: &mut crate::Registry,
    part: &ContentPart,
    created: &mut std::collections::BTreeSet<String>,
) -> GenUiResult<SurfaceIr> {
    let ContentPart::Ui { format, source, sealed, .. } = part else {
        return Err(GenUiError { message: "不是 UI 分帧".into(), line: None });
    };
    let ir = compile_source(source, *format, *sealed)?;
    if !created.contains(&ir.surface_id) {
        registry.apply(translate_create(&ir)).map_err(|e| GenUiError { message: e.to_string(), line: None })?;
    } else {
        for msg in translate_update(&ir) {
            registry.apply(msg).map_err(|e| GenUiError { message: e.to_string(), line: None })?;
        }
    }
    created.insert(ir.surface_id.clone());
    Ok(ir)
}

// ─────────────────────────────────────────────────────────────────────────────
// 真实配方 fixture（逐字取自桌面端 genui-check.tsx，仅围栏版本改为 v=0.2——
// 这就是 backend_prompt.md 教会模型后，真实会话里 assistant 文本的形状）
// ─────────────────────────────────────────────────────────────────────────────

/// ASK_TODO：TodoList + TextInput + followUp 提交（disabled 随输入联动）。
pub const ASK_TODO_FENCE: &str = concat!(
    "说明文字\n",
    "```qaqh-ui v=0.2 format=tsx\n",
    "ui.surface(\"plan\", { root: \"card\", title: \"计划与确认\" });\n",
    "const t_frame = ui.state(\"t_frame\", \"boolean\", false);\n",
    "const t_genui = ui.state(\"t_genui\", \"boolean\", false);\n",
    "const confirm_note = ui.state(\"confirm_note\", \"string\", \"\");\n",
    "ui.node(\"card\", <TodoList title=\"执行计划\" children={[\"t1\", \"t2\", \"summary\", \"ask\", \"send\"]} />);\n",
    "ui.node(\"t1\", <Todo text=\"分帧器\" done={ui.bind(\"t_frame\")} meta=\"阶段 1\" />);\n",
    "ui.node(\"t2\", <Todo text=\"生成式界面\" done={ui.bind(\"t_genui\")} />);\n",
    "ui.node(\"summary\", <Text text={\"进度：\" + t_frame} />);\n",
    "ui.node(\"ask\", <TextInput label=\"补充说明\" value={confirm_note} required={true} />);\n",
    "ui.node(\"send\", <Button label=\"提交确认\" disabled={confirm_note == \"\"} onPress={ui.followUp(\"请按计划继续\", { \"说明\": confirm_note })} />);\n",
    "ui.end();\n",
    "```\n",
);

/// V2_COUNTER：Metric + set/reset + TagList 增删 + repeat 模板行。
/// 注意包装根只能有一个：两张顶层卡必须显式嵌一个 Stack（translate.ts 的
/// rootComponentId 只取 rootId ?? forestRoots[0]，多顶层会被孤儿告警）。
pub const V2_COUNTER_FENCE: &str = concat!(
    "```qaqh-ui v=0.2 format=tsx\n",
    "ui.surface(\"counter\", { root: \"wrap\", fallback: \"v2 计数器，可增加或重置。\" });\n",
    "const count = ui.state(\"count\", \"integer\", 0);\n",
    "const tags = ui.state(\"tags\", \"list\", [\"alpha\"]);\n",
    "ui.node(\"wrap\", <Stack children={[\"card\", \"tagcard\"]} />);\n",
    "ui.node(\"card\", <Card title=\"v2 计数器\" children={[\"value\", \"increment\", \"reset\"]} />);\n",
    "ui.node(\"value\", <Metric label=\"当前计数\" value={count} />);\n",
    "ui.node(\"increment\", <Button label=\"增加\" onPress={ui.set(\"count\", count + 1)} />);\n",
    "ui.node(\"reset\", <Button label=\"重置\" onPress={ui.reset()} />);\n",
    "ui.node(\"tagcard\", <Card title=\"标签\" children={[\"tags\", \"add\", \"del\", \"rows\"]} />);\n",
    "ui.node(\"tags\", <TagList items={tags} />);\n",
    "ui.node(\"add\", <Button label=\"加 beta\" onPress={ui.append(\"tags\", \"beta\")} />);\n",
    "ui.node(\"del\", <Button label=\"删 beta\" onPress={ui.remove(\"tags\", \"beta\")} />);\n",
    "ui.node(\"rows\", <Text text={\"· \" + item} repeat={ui.for(\"tags\", \"item\")} />);\n",
    "ui.end();\n",
    "```\n",
);

/// FORM_JSONL：jsonl 载体 + 表单组件 + show 条件渲染。
pub const FORM_JSONL_FENCE: &str = concat!(
    "```qaqh-ui v=0.2 format=jsonl\n",
    "{\"op\":\"surface.open\",\"surface\":\"form\",\"root\":\"card\",\"fallback\":\"表单演示\"}\n",
    "{\"op\":\"state\",\"key\":\"name\",\"type\":\"string\",\"initial\":\"\"}\n",
    "{\"op\":\"state\",\"key\":\"dark\",\"type\":\"boolean\",\"initial\":false}\n",
    "{\"op\":\"state\",\"key\":\"tags\",\"type\":\"list\",\"initial\":[\"alpha\"]}\n",
    "{\"op\":\"node\",\"id\":\"card\",\"component\":\"Card\",\"props\":{\"title\":\"表单\",\"children\":[\"name\",\"on\",\"off\",\"badge\",\"tags\",\"add\",\"del\",\"rows\"]}}\n",
    "{\"op\":\"node\",\"id\":\"name\",\"component\":\"TextInput\",\"props\":{\"label\":\"名称\",\"value\":{\"bind\":\"name\"},\"placeholder\":\"输入名称\",\"required\":true,\"maxLength\":10}}\n",
    "{\"op\":\"node\",\"id\":\"on\",\"component\":\"Button\",\"props\":{\"label\":\"开启\",\"onPress\":{\"set\":\"dark\",\"value\":{\"lit\":true}}}}\n",
    "{\"op\":\"node\",\"id\":\"off\",\"component\":\"Button\",\"props\":{\"label\":\"关闭\",\"onPress\":{\"set\":\"dark\",\"value\":{\"lit\":false}}}}\n",
    "{\"op\":\"node\",\"id\":\"badge\",\"component\":\"Badge\",\"props\":{\"text\":\"深色已开启\",\"show\":{\"state\":\"dark\"}}}\n",
    "{\"op\":\"node\",\"id\":\"tags\",\"component\":\"TagList\",\"props\":{\"items\":{\"bind\":\"tags\"}}}\n",
    "{\"op\":\"node\",\"id\":\"add\",\"component\":\"Button\",\"props\":{\"label\":\"加标签\",\"onPress\":{\"append\":\"tags\",\"value\":{\"lit\":\"beta\"}}}}\n",
    "{\"op\":\"node\",\"id\":\"del\",\"component\":\"Button\",\"props\":{\"label\":\"移除标签\",\"onPress\":{\"remove\":\"tags\",\"value\":{\"lit\":\"beta\"}}}}\n",
    "{\"op\":\"node\",\"id\":\"rows\",\"component\":\"Text\",\"props\":{\"text\":{\"op\":\"add\",\"left\":{\"lit\":\"· \"},\"right\":{\"item\":\"item\"}},\"repeat\":{\"source\":\"tags\",\"as\":\"item\"}}}\n",
    "{\"op\":\"surface.close\"}\n",
    "```\n",
);

/// 流式表格（genui-check 的 stages，版本改 v=0.2）：md → holdback → 开门 →
/// 部分编译 → seal 转正 → 尾部 md。
pub const TABLE_RECORDS: &str = concat!(
    "{\"op\":\"surface.open\",\"surface\":\"table\",\"root\":\"card\",\"title\":\"会议名单\"}\n",
    "{\"op\":\"state\",\"key\":\"rows\",\"type\":\"list\",\"initial\":[\"张三\",\"李四\"]}\n",
    "{\"op\":\"node\",\"id\":\"card\",\"component\":\"Card\",\"props\":{\"title\":\"名单\",\"children\":[\"r1\",\"r2\",\"r3\"]}}\n",
    "{\"op\":\"node\",\"id\":\"r1\",\"component\":\"Text\",\"props\":{\"text\":\"张三\"}}\n",
);
pub const TABLE_DONE_TAIL: &str = concat!(
    "{\"op\":\"node\",\"id\":\"r2\",\"component\":\"Text\",\"props\":{\"text\":\"李四\"}}\n",
    "{\"op\":\"node\",\"id\":\"r3\",\"component\":\"Text\",\"props\":{\"text\":\"王五\"}}\n",
    "{\"op\":\"surface.close\"}\n",
);

/// genui-check 的分阶段全文：每一步都是一次 push(final=false)。
pub fn table_stages() -> Vec<String> {
    let fence_head = "```qaqh-ui v=0.2 format=jsonl\n";
    vec![
        "说明文字".to_string(),
        "说明文字\n```qaqh-u".to_string(),
        format!("说明文字\n{fence_head}"),
        format!("说明文字\n{fence_head}{TABLE_RECORDS}"),
        format!("说明文字\n{fence_head}{TABLE_RECORDS}{TABLE_DONE_TAIL}```\n结尾"),
    ]
}
