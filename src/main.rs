//! A2UI v1.0 → ratatui 渲染可行性 demo（自包含，只依赖 ratatui/serde/unicode-width）。
//!
//! 链路：wire 消息（serde 解析，含版本与单一操作校验）→ SurfaceRegistry（patch 流
//! + 预算 + 渲染闸门）→ DynamicValue 求值（@path 绑定 / @call 白名单函数）→
//! 布局（测高 + 摆矩形）→ 绘制（直接写 Buffer + 登记命中区）→ 交互（鼠标 +
//! 焦点环 + `qaqh.action.*` 分发 / followUp 组装回传文本）。
//!
//! fixture 对齐桌面端 `genui-check.tsx` 的 ASK_TODO / V2_COUNTER 配方——即
//! `translate.ts` 降级后的 A2UI v1.0 wire 形状（`/s/<key>` 状态域、`onPress.event`、
//! `qaqh.dev/repeat` 扩展、"root" 包装根）。
//!
//! 用法：
//! - `cargo run`                交互模式（鼠标点选 / Tab 焦点 / Enter 激活 / n 推进流式）
//! - `cargo run -- --headless`  无头渲染两帧（交互前 / 后）打印到 stdout 后退出
//! - `cargo test`               无头管线回归（解析 / 闸门 / 布局 / 点击 / 禁用逻辑）

use std::collections::BTreeMap;
use std::io::{self, Write};

use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Terminal;
use serde::Deserialize;
use serde_json::{Map, Value};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod dsl;

// ─────────────────────────────────────────────────────────────────────────────
// wire 类型（A2UI v1.0 agent → renderer 消息，对齐桌面端 genui/v2/types.ts）
// ─────────────────────────────────────────────────────────────────────────────

pub const A2UI_VERSION: &str = "v1.0";

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Message {
    pub version: String,
    #[serde(rename = "createSurface")]
    pub create_surface: Option<CreateSurface>,
    #[serde(rename = "updateComponents")]
    pub update_components: Option<UpdateComponents>,
    #[serde(rename = "updateDataModel")]
    pub update_data_model: Option<UpdateDataModel>,
    #[serde(rename = "deleteSurface")]
    pub delete_surface: Option<DeleteSurface>,
    #[serde(rename = "callRendererFunction")]
    pub call_renderer_function: Option<CallRendererFunction>,
    #[serde(rename = "agentFunctionResponse")]
    pub agent_function_response: Option<AgentFunctionResponse>,
}

impl Message {
    /// 解析一行 wire JSON；除信封字段外必须恰好携带一种操作。
    pub fn parse(line: &str) -> Result<Self, String> {
        let msg: Self =
            serde_json::from_str(line).map_err(|e| format!("JSON 解析失败: {e}"))?;
        if msg.version != A2UI_VERSION {
            return Err(format!("未知协议版本 {}（本 renderer 支持 {A2UI_VERSION}）", msg.version));
        }
        let ops = [
            msg.create_surface.is_some(),
            msg.update_components.is_some(),
            msg.update_data_model.is_some(),
            msg.delete_surface.is_some(),
            msg.call_renderer_function.is_some(),
            msg.agent_function_response.is_some(),
        ];
        match ops.iter().filter(|op| **op).count() {
            1 => Ok(msg),
            _ => Err("消息必须恰好携带一种操作键".to_string()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CreateSurface {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    #[serde(rename = "catalogId")]
    pub catalog_id: Option<String>,
    #[serde(rename = "sendDataModel", default)]
    pub send_data_model: bool,
    #[serde(default)]
    pub components: Vec<WireComponent>,
    #[serde(rename = "dataModel", default)]
    pub data_model: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UpdateComponents {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    #[serde(default)]
    pub components: Vec<WireComponent>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct UpdateDataModel {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    /// JSON Pointer；缺省或 "/" 指整个 dataModel。
    #[serde(default)]
    pub path: Option<String>,
    pub value: Value,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DeleteSurface {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CallRendererFunction {
    #[serde(rename = "surfaceId")]
    pub surface_id: Option<String>,
    #[serde(rename = "functionCallId")]
    pub function_call_id: String,
    #[serde(rename = "callFunction")]
    pub call_function: FunctionCall,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AgentFunctionResponse {
    #[serde(rename = "functionCallId")]
    pub function_call_id: String,
}

/// catalog 组件实例：信封字段固定，其余属性原样保留、按需转 `Dyn`。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WireComponent {
    pub id: String,
    pub component: String,
    #[serde(default)]
    pub child: Option<String>,
    #[serde(default)]
    pub children: Option<ChildList>,
    #[serde(rename = "onPress")]
    pub on_press: Option<ActionEvent>,
    #[serde(default)]
    pub show: Option<Value>,
    pub metadata: Option<Metadata>,
    #[serde(flatten)]
    pub props: Map<String, Value>,
}

impl WireComponent {
    pub fn prop_dyn(&self, name: &str) -> Option<Dyn> {
        self.props.get(name).map(to_dyn)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ChildList {
    Static(Vec<String>),
    Template(TemplateChildList),
}

/// spec 的「模板 + dataModel 路径」动态子列表。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TemplateChildList {
    #[serde(rename = "componentId")]
    pub component_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Metadata {
    #[serde(default)]
    pub extensions: Option<Map<String, Value>>,
}

/// 用户动作意图（组件的 `onPress.event`）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ActionEvent {
    pub event: EventSpec,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct EventSpec {
    pub name: String,
    #[serde(rename = "userMessage", default)]
    pub user_message: Option<Value>,
    #[serde(default)]
    pub context: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct FunctionCall {
    #[serde(rename = "@call")]
    pub name: String,
    #[serde(default)]
    pub args: Option<Map<String, Value>>,
}

/// 三态值：字面量 | `{"@path": ...}` 绑定 | `{"@call": ...}` 函数。
/// wire 上不单独建模——从原始 JSON 惰性判别，避免 flatten+untagged 脆弱组合。
#[derive(Debug, Clone, PartialEq)]
pub enum Dyn {
    Lit(Value),
    Path(String),
    Call { name: String, args: Map<String, Value> },
}

pub fn to_dyn(v: &Value) -> Dyn {
    match v {
        Value::Object(m) if m.contains_key("@path") => match m.get("@path") {
            Some(Value::String(p)) => Dyn::Path(p.clone()),
            _ => Dyn::Lit(v.clone()),
        },
        Value::Object(m) if m.contains_key("@call") => Dyn::Call {
            name: m.get("@call").and_then(Value::as_str).unwrap_or("").to_string(),
            args: m.get("args").and_then(Value::as_object).cloned().unwrap_or_default(),
        },
        other => Dyn::Lit(other.clone()),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JSON Pointer 增删改查（dataModel 底座；serde_json 自带查，写删手补）
// ─────────────────────────────────────────────────────────────────────────────

/// 桌面端约定：缺省或 "/" 指整个 dataModel。
fn normalize_ptr(path: Option<&str>) -> String {
    match path {
        None | Some("/") => String::new(),
        Some(p) => p.to_string(),
    }
}

fn unescape_seg(seg: &str) -> String {
    seg.replace("~1", "/").replace("~0", "~")
}

fn pointer_set(root: &mut Value, ptr: &str, value: Value) {
    if ptr.is_empty() {
        *root = value;
        return;
    }
    let segs: Vec<String> = ptr.trim_start_matches('/').split('/').map(unescape_seg).collect();
    let mut cur = root;
    for (i, seg) in segs.iter().enumerate() {
        let last = i == segs.len() - 1;
        if last {
            if let Some(obj) = cur.as_object_mut() {
                obj.insert(seg.clone(), value);
            } else if let Some(arr) = cur.as_array_mut() {
                if let Ok(idx) = seg.parse::<usize>() {
                    if idx < arr.len() {
                        arr[idx] = value;
                    } else {
                        arr.push(value);
                    }
                }
            }
            return;
        }
        if cur.as_object_mut().map(|o| o.get(seg).is_none_or(Value::is_null)).unwrap_or(true) {
            if let Some(obj) = cur.as_object_mut() {
                obj.insert(seg.clone(), Value::Object(Map::new()));
            }
        }
        let Some(next) = cur.pointer_mut(&format!("/{seg}")) else { return };
        cur = next;
    }
}

fn pointer_remove(root: &mut Value, ptr: &str) -> Option<Value> {
    let segs: Vec<String> = ptr.trim_start_matches('/').split('/').map(unescape_seg).collect();
    let (last, parents) = segs.split_last()?;
    let mut cur = root;
    for seg in parents {
        cur = cur.pointer_mut(&format!("/{seg}"))?;
    }
    match cur {
        Value::Object(obj) => obj.remove(last),
        Value::Array(arr) => last.parse::<usize>().ok().filter(|i| *i < arr.len()).map(|i| arr.remove(i)),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SurfaceRegistry：按序 patch 流 + 预算 + 渲染闸门（对齐桌面端 store.ts）
// ─────────────────────────────────────────────────────────────────────────────

pub const MAX_NODES: usize = 512;
pub const MAX_TREE_DEPTH: usize = 16;
pub const MAX_DATA_MODEL_BYTES: usize = 256 * 1024;
/// `translate.ts` 的 repeat 模板扩展键（vendor 命名空间）。
pub const REPEAT_EXTENSION: &str = "qaqh.dev/repeat";

#[derive(Debug, Clone)]
pub struct SurfaceState {
    pub surface_id: String,
    pub catalog_id: Option<String>,
    pub send_data_model: bool,
    pub components: BTreeMap<String, WireComponent>,
    pub data_model: Value,
    /// reset 动作的基线快照。
    pub initial_model: Value,
}

#[derive(Debug, Default)]
pub struct Registry {
    pub surfaces: BTreeMap<String, SurfaceState>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApplyError {
    Version(String),
    NoOp,
    UnknownSurface(String),
    Duplicate(String),
    Budget(String),
    Malformed(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(v) => write!(f, "未知协议版本 {v}"),
            Self::NoOp => write!(f, "消息不含任何已知操作键"),
            Self::UnknownSurface(id) => write!(f, "surface {id} 不存在"),
            Self::Duplicate(id) => write!(f, "surface {id} 已存在"),
            Self::Budget(m) => write!(f, "超预算: {m}"),
            Self::Malformed(m) => write!(f, "协议违规: {m}"),
        }
    }
}

/// apply 的结果附注（进状态栏，模拟真实 renderer 的降级/记录）。
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    Applied(&'static str),
    Unsupported(&'static str),
}

impl Registry {
    pub fn apply(&mut self, msg: Message) -> Result<Note, ApplyError> {
        if msg.version != A2UI_VERSION {
            return Err(ApplyError::Version(msg.version));
        }
        if let Some(c) = msg.create_surface {
            if self.surfaces.contains_key(&c.surface_id) {
                return Err(ApplyError::Duplicate(c.surface_id));
            }
            if c.components.len() > MAX_NODES {
                return Err(ApplyError::Budget(format!("组件数超过 {MAX_NODES}")));
            }
            let mut comps = BTreeMap::new();
            for comp in c.components {
                if comps.insert(comp.id.clone(), comp).is_some() {
                    return Err(ApplyError::Malformed("组件 id 重复".into()));
                }
            }
            let dm = Value::Object(c.data_model);
            if serde_json::to_string(&dm).unwrap_or_default().len() > MAX_DATA_MODEL_BYTES {
                return Err(ApplyError::Budget("dataModel 体积超限".into()));
            }
            self.surfaces.insert(
                c.surface_id.clone(),
                SurfaceState {
                    surface_id: c.surface_id,
                    catalog_id: c.catalog_id,
                    send_data_model: c.send_data_model,
                    components: comps,
                    initial_model: dm.clone(),
                    data_model: dm,
                },
            );
            return Ok(Note::Applied("create"));
        }
        if let Some(u) = msg.update_components {
            let Some(state) = self.surfaces.get_mut(&u.surface_id) else {
                return Err(ApplyError::UnknownSurface(u.surface_id));
            };
            for comp in u.components {
                if state.components.len() >= MAX_NODES && !state.components.contains_key(&comp.id) {
                    return Err(ApplyError::Budget(format!("组件数超过 {MAX_NODES}")));
                }
                state.components.insert(comp.id.clone(), comp);
            }
            return Ok(Note::Applied("updateComponents"));
        }
        if let Some(u) = msg.update_data_model {
            let Some(state) = self.surfaces.get_mut(&u.surface_id) else {
                return Err(ApplyError::UnknownSurface(u.surface_id));
            };
            let ptr = normalize_ptr(u.path.as_deref());
            pointer_set(&mut state.data_model, &ptr, u.value);
            if serde_json::to_string(&state.data_model).unwrap_or_default().len()
                > MAX_DATA_MODEL_BYTES
            {
                return Err(ApplyError::Budget("dataModel 体积超限".into()));
            }
            return Ok(Note::Applied("updateDataModel"));
        }
        if let Some(d) = msg.delete_surface {
            if self.surfaces.remove(&d.surface_id).is_none() {
                return Err(ApplyError::UnknownSurface(d.surface_id));
            }
            return Ok(Note::Applied("delete"));
        }
        if let Some(c) = msg.call_renderer_function {
            let _ = c;
            // demo 的 renderer 函数白名单为空：记录降级，不崩（P4 再接 copy/theme 等）。
            return Ok(Note::Unsupported("callRendererFunction（白名单为空）"));
        }
        if let Some(r) = msg.agent_function_response {
            let _ = r;
            return Ok(Note::Unsupported("agentFunctionResponse（无未决调用）"));
        }
        Err(ApplyError::NoOp)
    }

    /// 渲染闸门：root 存在、从 root 出发引用全部可解析、无环、不超深/超预算。
    /// 返回 Some(原因) = 还不能渲染（流式中或违规）。
    pub fn render_blocker(&self, surface_id: &str) -> Option<String> {
        let state = self.surfaces.get(surface_id)?;
        let root = state.components.get("root")?;
        let mut visiting = Vec::new();
        let mut done = Vec::new();
        let mut count = 0usize;
        let blocker = visit(state, root, 0, &mut visiting, &mut done, &mut count);
        if blocker.is_none() && count > MAX_NODES {
            return Some(format!("组件数超过 {MAX_NODES}"));
        }
        blocker
    }
}

fn visit(
    state: &SurfaceState,
    comp: &WireComponent,
    depth: usize,
    visiting: &mut Vec<String>,
    done: &mut Vec<String>,
    count: &mut usize,
) -> Option<String> {
    if done.contains(&comp.id) {
        return None;
    }
    if visiting.contains(&comp.id) {
        return Some(format!("children 形成环: {}", comp.id));
    }
    if depth > MAX_TREE_DEPTH {
        return Some(format!("组件树深度超过 {MAX_TREE_DEPTH}"));
    }
    *count += 1;
    visiting.push(comp.id.clone());
    for r in child_refs(comp) {
        match r {
            ChildRef::Id(id) => {
                let Some(child) = state.components.get(&id) else {
                    return Some(format!("引用未解析: {id}"));
                };
                if let Some(b) = visit(state, child, depth + 1, visiting, done, count) {
                    return Some(b);
                }
            }
            // 模板子列表：闸门只要求被引用的模板组件存在（展开在渲染期）。
            ChildRef::Template(id) => {
                if !state.components.contains_key(&id) {
                    return Some(format!("引用未解析: {id}"));
                }
            }
        }
    }
    visiting.pop();
    done.push(comp.id.clone());
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// 求值：@path 绑定 / @call 白名单 / truthy（对齐 SurfaceView2 的语义）
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub index: usize,
    pub item: Value,
}

pub fn resolve(dynv: &Dyn, dm: &Value, scope: Option<&Scope>) -> Result<Value, String> {
    match dynv {
        Dyn::Lit(v) => Ok(v.clone()),
        Dyn::Path(p) => resolve_path(p, dm, scope),
        Dyn::Call { name, args } => call_fn(name, args, dm, scope),
    }
}

pub fn resolve_value(v: &Value, dm: &Value, scope: Option<&Scope>) -> Result<Value, String> {
    resolve(&to_dyn(v), dm, scope)
}

fn resolve_path(p: &str, dm: &Value, scope: Option<&Scope>) -> Result<Value, String> {
    if p.is_empty() {
        // 模板当前项（translate 的 item 表达式 → 相对空路径）。
        return scope.map(|s| s.item.clone()).ok_or_else(|| "空路径绑定但无模板作用域".into());
    }
    if p == "/@index" {
        return Ok(scope.map(|s| Value::from(s.index as u64)).unwrap_or(Value::Null));
    }
    let ptr = if p == "/" { "" } else { p };
    Ok(dm.pointer(ptr).cloned().unwrap_or(Value::Null))
}

fn call_fn(name: &str, args: &Map<String, Value>, dm: &Value, scope: Option<&Scope>) -> Result<Value, String> {
    let arg = |k: &str| -> Result<Value, String> {
        match args.get(k) {
            Some(v) => resolve_value(v, dm, scope),
            None => Err(format!("函数 {name} 缺参数 {k}")),
        }
    };
    match name {
        "qaqh:arith.add" => {
            let (l, r) = (arg("left")?, arg("right")?);
            // 整数进整数出（避免 1.0 显示漂移）；任一侧非数字 → 字符串拼接。
            match (as_int(&l), as_int(&r)) {
                (Some(a), Some(b)) => Ok(Value::from(a + b)),
                _ if l.is_number() && r.is_number() => {
                    Ok(num_value(l.as_f64().unwrap_or(0.0) + r.as_f64().unwrap_or(0.0)))
                }
                _ => Ok(Value::String(format!("{}{}", display(&l), display(&r)))),
            }
        }
        "qaqh:arith.sub" | "qaqh:arith.mul" | "qaqh:arith.div" => {
            let (l, r) = (arg("left")?, arg("right")?);
            let op = name.trim_start_matches("qaqh:arith.");
            if let (Some(a), Some(b)) = (as_int(&l), as_int(&r)) {
                match op {
                    "sub" => Ok(Value::from(a - b)),
                    "mul" => Ok(Value::from(a * b)),
                    _ => {
                        if b == 0 {
                            return Err("qaqh:arith.div 除零".into());
                        }
                        Ok(Value::from(a / b))
                    }
                }
            } else {
                let (Some(a), Some(b)) = (l.as_f64(), r.as_f64()) else {
                    return Err(format!("{name} 需要数字参数"));
                };
                Ok(num_value(match op {
                    "sub" => a - b,
                    "mul" => a * b,
                    _ => {
                        if b == 0.0 {
                            return Err("qaqh:arith.div 除零".into());
                        }
                        a / b
                    }
                }))
            }
        }
        "qaqh:logic.eq" => Ok(Value::Bool(arg("left")? == arg("right")?)),
        "qaqh:logic.neq" => Ok(Value::Bool(arg("left")? != arg("right")?)),
        "qaqh:logic.gt" => Ok(Value::Bool(num_cmp(&arg("left")?, &arg("right")?) == Some(std::cmp::Ordering::Greater))),
        "qaqh:logic.lt" => Ok(Value::Bool(num_cmp(&arg("left")?, &arg("right")?) == Some(std::cmp::Ordering::Less))),
        "qaqh:logic.and" => Ok(Value::Bool(truthy(&arg("left")?) && truthy(&arg("right")?))),
        "qaqh:logic.or" => Ok(Value::Bool(truthy(&arg("left")?) || truthy(&arg("right")?))),
        "qaqh:logic.not" => Ok(Value::Bool(!truthy(&arg("value")?))),
        other => Err(format!("白名单外函数 {other}")),
    }
}

fn num_value(f: f64) -> Value {
    serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

fn as_int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
}

fn num_cmp(l: &Value, r: &Value) -> Option<std::cmp::Ordering> {
    match (l.as_f64(), r.as_f64()) {
        (Some(a), Some(b)) => a.partial_cmp(&b),
        _ => None,
    }
}

/// JS 风格 truthy（desktop truthy 的近似）：null/false/0/空串 为假，容器恒真。
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// 值的展示形态（对齐桌面端 display：字符串原样，bool "true"，null 空）。
pub fn display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// fixture：translate.ts 降级后的真实 wire 形状（对齐 genui-check.tsx 配方）
// ─────────────────────────────────────────────────────────────────────────────

/// ASK_TODO：TodoList + TextInput + followUp 提交按钮（disabled 随输入联动）。
pub const PLAN_CREATE: &str = r#"{"version":"v1.0","createSurface":{"surfaceId":"plan","catalogId":"qaqh.dev:standard-v2","sendDataModel":false,"components":[
  {"id":"root","component":"Stack","children":["card"]},
  {"id":"card","component":"TodoList","title":"执行计划","children":["t1","t2","summary","ask","send"]},
  {"id":"t1","component":"Todo","text":"分帧器","done":{"@path":"/s/t_frame"},"meta":"阶段 1"},
  {"id":"t2","component":"Todo","text":"生成式界面","done":{"@path":"/s/t_genui"}},
  {"id":"summary","component":"Text","text":{"@call":"qaqh:arith.add","args":{"left":"进度：","right":{"@path":"/s/t_frame"}}}},
  {"id":"ask","component":"TextInput","label":"补充说明","value":{"@path":"/s/confirm_note"},"placeholder":"（点此输入）"},
  {"id":"send","component":"Button","label":"提交确认",
   "disabled":{"@call":"qaqh:logic.eq","args":{"left":{"@path":"/s/confirm_note"},"right":""}},
   "onPress":{"event":{"name":"qaqh.action.followUp","context":{"prompt":"请按计划继续","fields":[{"label":"说明","key":"confirm_note"}]}}}}
],"dataModel":{"s":{"t_frame":false,"t_genui":false,"confirm_note":""}}}}"#;

/// V2_COUNTER：Metric + set/reset + TagList 增删 + repeat 模板行。
/// root 包装组件在最后——与桌面端 translate.ts 的 `components.push(root)` 一致。
pub const COUNTER_CREATE: &str = r#"{"version":"v1.0","createSurface":{"surfaceId":"counter","catalogId":"qaqh.dev:standard-v2","sendDataModel":false,"components":[
  {"id":"wrap","component":"Stack","children":["card","tagcard"]},
  {"id":"card","component":"Card","title":"v2 计数器","children":["value","increment","reset"]},
  {"id":"value","component":"Metric","label":"当前计数","value":{"@path":"/s/count"}},
  {"id":"increment","component":"Button","label":"增加","onPress":{"event":{"name":"qaqh.action.set","context":{"key":"count","value":{"@call":"qaqh:arith.add","args":{"left":{"@path":"/s/count"},"right":1}}}}}},
  {"id":"reset","component":"Button","label":"重置","onPress":{"event":{"name":"qaqh.action.reset","context":{}}}},
  {"id":"tagcard","component":"Card","title":"标签","children":["tags","add","del","rows"]},
  {"id":"tags","component":"TagList","items":{"@path":"/s/tags"}},
  {"id":"add","component":"Button","label":"加 beta","onPress":{"event":{"name":"qaqh.action.append","context":{"key":"tags","value":"beta"}}}},
  {"id":"del","component":"Button","label":"删 beta","onPress":{"event":{"name":"qaqh.action.remove","context":{"key":"tags","value":"beta"}}}},
  {"id":"rows","component":"Text","text":{"@call":"qaqh:arith.add","args":{"left":"· ","right":{"@path":""}}},
   "metadata":{"extensions":{"qaqh.dev/repeat":{"path":"/s/tags","as":"item"}}}},
  {"id":"root","component":"Stack","children":["wrap"]}
],"dataModel":{"s":{"count":0,"tags":["alpha"]}}}}"#;

/// 流式三阶段（table surface）：阶段 0 未创建 → 1 前向引用未到齐（占位）→ 2 到齐浮现。
pub const TABLE_STAGE1: &str = r#"{"version":"v1.0","createSurface":{"surfaceId":"table","catalogId":"qaqh.dev:standard-v2","sendDataModel":false,"components":[
  {"id":"root","component":"Stack","children":["card"]},
  {"id":"card","component":"Card","title":"会议名单","children":["r1","r2","r3"]},
  {"id":"r1","component":"Text","text":"张三"}
],"dataModel":{}}}"#;

pub const TABLE_STAGE2: &str = r#"{"version":"v1.0","updateComponents":{"surfaceId":"table","components":[
  {"id":"r2","component":"Text","text":"李四"},
  {"id":"r3","component":"Text","text":"王五"}
]}}"#;

fn feed_line(reg: &mut Registry, line: &str, log: &mut Vec<String>) {
    match Message::parse(line).and_then(|m| reg.apply(m).map(|_| ()).map_err(|e| e.to_string())) {
        Ok(()) => {}
        Err(e) => log.push(e),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// UI 状态与命中图
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompKey {
    pub surface: String,
    pub component: String,
    pub scope: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    Button,
    Toggle,
    Input,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub rect: Rect,
    pub key: CompKey,
    pub kind: HitKind,
}

#[derive(Debug)]
pub struct UiState {
    pub registry: Registry,
    /// surface 展示顺序。
    pub order: Vec<String>,
    pub stage: usize,
    pub scroll: u16,
    pub focus: Option<CompKey>,
    pub pressed: Option<CompKey>,
    pub editing: Option<(CompKey, String)>,
    pub outbox: Vec<String>,
    pub status: String,
    pub hits: Vec<Hit>,
    /// 本帧可聚焦组件序（绘制序）。
    pub interactive: Vec<CompKey>,
    pub mouse: (u16, u16),
    pub unsupported: Vec<String>,
}

impl UiState {
    pub fn new() -> Self {
        // 默认路径走完整 DSL 编译链：qaqh-ui 围栏（genui-check 真实配方）→
        // decoder 分帧 → parser → IR → translate → wire → registry。
        // 手写 wire fixture（PLAN_CREATE 等）只保留在测试里做交叉验证。
        let mut registry = Registry::default();
        let mut created = std::collections::BTreeSet::new();
        let mut order: Vec<String> = vec![];
        let mut warnings: Vec<String> = vec![];
        for fence in [dsl::ASK_TODO_FENCE, dsl::V2_COUNTER_FENCE] {
            for part in dsl::split_content_parts(fence, true) {
                if matches!(part, dsl::ContentPart::Markdown(_)) {
                    continue;
                }
                match dsl::ingest_ui_part(&mut registry, &part, &mut created) {
                    Ok(ir) => {
                        if !order.contains(&ir.surface_id) {
                            order.push(ir.surface_id.clone());
                        }
                        warnings.extend(ir.warnings.iter().cloned());
                    }
                    Err(e) => warnings.push(e.to_string()),
                }
            }
        }
        let status = if warnings.is_empty() {
            "demo 就绪：DSL 编译链（qaqh-ui 围栏 → IR → A2UI wire）已喂入 plan / counter，n 推进 table 流式".into()
        } else {
            format!("载入告警: {warnings:?}")
        };
        Self::build(registry, order, warnings, status)
    }

    fn build(registry: Registry, order: Vec<String>, warnings: Vec<String>, status: String) -> Self {
        Self {
            registry,
            order,
            stage: 0,
            scroll: 0,
            focus: None,
            pressed: None,
            editing: None,
            outbox: Vec::new(),
            status,
            hits: Vec::new(),
            interactive: Vec::new(),
            mouse: (0, 0),
            unsupported: warnings,
        }
    }

    /// `--fence <file>` 模式：读一份 assistant 全文（真实模型输出的形状），
    /// 走完整编译链后 headless 渲染。
    pub fn from_text(text: &str) -> Self {
        let mut registry = Registry::default();
        let mut created = std::collections::BTreeSet::new();
        let mut order: Vec<String> = vec![];
        let mut warnings: Vec<String> = vec![];
        for part in dsl::split_content_parts(text, true) {
            match &part {
                dsl::ContentPart::Markdown(md) => warnings.push(format!("[markdown] {}", md.lines().next().unwrap_or(""))),
                ui_part => match dsl::ingest_ui_part(&mut registry, ui_part, &mut created) {
                    Ok(ir) => {
                        if !order.contains(&ir.surface_id) {
                            order.push(ir.surface_id.clone());
                        }
                        warnings.extend(ir.warnings.iter().cloned());
                    }
                    Err(e) => warnings.push(format!("编译失败: {e}")),
                },
            }
        }
        let count = order.len();
        Self::build(registry, order, vec![], format!("--fence 载入 {count} 个 surface：{warnings:?}"))
    }

    /// 推进 table 流式阶段（n 键）：演示渲染闸门与前向引用。
    pub fn advance_stage(&mut self) {
        self.stage += 1;
        let mut log = Vec::new();
        match self.stage {
            1 => feed_line(&mut self.registry, TABLE_STAGE1, &mut log),
            2 => feed_line(&mut self.registry, TABLE_STAGE2, &mut log),
            _ => {
                self.stage = 2;
                self.status = "流式已到末段".into();
                return;
            }
        }
        if !self.order.iter().any(|id| id == "table") {
            self.order.push("table".into());
        }
        let stage = self.stage;
        self.status = if log.is_empty() {
            format!("流式阶段 {stage} 已喂入")
        } else {
            format!("流式阶段 {stage} 告警: {log:?}")
        };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 布局与绘制：组件树 → 测高 → 摆矩形 → 写 Buffer + 登记命中
// ─────────────────────────────────────────────────────────────────────────────

fn point_in(p: (u16, u16), r: Rect) -> bool {
    p.0 >= r.x && p.0 < r.x + r.width && p.1 >= r.y && p.1 < r.y + r.height
}

fn rect_h(r: Rect) -> u16 {
    r.height
}

fn intersect(a: Rect, b: Rect) -> Rect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let x2 = (a.x + a.width).min(b.x + b.width);
    let y2 = (a.y + a.height).min(b.y + b.height);
    Rect { x, y, width: x2.saturating_sub(x), height: y2.saturating_sub(y) }
}

/// 贪心折行：按词折、CJK 按字硬折（width 以显示列计）。
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in s.split(' ') {
        let sep = usize::from(!cur.is_empty());
        let w = word.width();
        if cur_w + sep + w <= width {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            cur_w += sep + w;
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        // 硬折长词（CJK 连续串整词超宽）。
        for ch in word.chars() {
            let cw = ch.width().unwrap_or(0);
            if cur_w + cw > width {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(ch);
            cur_w += cw;
        }
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// 子引用：静态 id 或模板展开项。
enum ChildRef {
    Id(String),
    Template(String),
}

fn child_refs(comp: &WireComponent) -> Vec<ChildRef> {
    let mut refs = Vec::new();
    if let Some(c) = &comp.child {
        refs.push(ChildRef::Id(c.clone()));
    }
    match &comp.children {
        Some(ChildList::Static(ids)) => {
            refs.extend(ids.iter().map(|id| ChildRef::Id(id.clone())));
        }
        Some(ChildList::Template(t)) => {
            refs.push(ChildRef::Template(t.component_id.clone()));
        }
        None => {}
    }
    refs
}

/// 展开一个组件的下一层：静态列表 + repeat 模板扩展 + 模板子列表。
/// 返回 (组件 id, 作用域)。repeat 只在无作用域时展开（对齐桌面端）。
fn expand_children(state: &SurfaceState, comp: &WireComponent, scope: Option<&Scope>) -> Vec<(String, Option<Scope>)> {
    let mut out: Vec<(String, Option<Scope>)> = Vec::new();
    for r in child_refs(comp) {
        match r {
            ChildRef::Id(id) => {
                let Some(child) = state.components.get(&id) else { continue };
                let repeat = child
                    .metadata
                    .as_ref()
                    .and_then(|m| m.extensions.as_ref())
                    .and_then(|e| e.get(REPEAT_EXTENSION))
                    .and_then(|v| v.get("path"))
                    .and_then(Value::as_str);
                if let (None, Some(path)) = (scope, repeat) {
                    if let Some(Value::Array(items)) = state.data_model.pointer(path) {
                        for (i, item) in items.iter().enumerate() {
                            out.push((id.clone(), Some(Scope { index: i, item: item.clone() })));
                        }
                    }
                    continue;
                }
                out.push((id, scope.cloned()));
            }
            ChildRef::Template(id) => {
                let path = match &comp.children {
                    Some(ChildList::Template(t)) => t.path.clone(),
                    _ => continue,
                };
                if let Some(Value::Array(items)) = state.data_model.pointer(&path) {
                    for (i, item) in items.iter().enumerate() {
                        out.push((id.clone(), Some(Scope { index: i, item: item.clone() })));
                    }
                }
            }
        }
    }
    out
}

/// 测高：给定宽度下组件占用的行数（布局先于绘制跑一遍树）。
fn measure(state: &SurfaceState, comp: &WireComponent, width: u16, scope: Option<&Scope>) -> u16 {
    let width = width.max(1);
    if let Some(show) = &comp.show {
        if let Ok(v) = resolve_value(show, &state.data_model, scope) {
            if !truthy(&v) {
                return 0;
            }
        }
    }
    match comp.component.as_str() {
        "Stack" => expand_children(state, comp, scope)
            .iter()
            .filter_map(|(id, sc)| state.components.get(id).map(|c| measure(state, c, width, sc.as_ref())))
            .sum(),
        "Card" | "TodoList" => {
            let inner = width.saturating_sub(2).max(1);
            let body: u16 = expand_children(state, comp, scope)
                .iter()
                .filter_map(|(id, sc)| state.components.get(id).map(|c| measure(state, c, inner, sc.as_ref())))
                .sum();
            body + 2
        }
        "Row" => {
            let kids = expand_children(state, comp, scope);
            let n = kids.len().max(1) as u16;
            kids.iter()
                .filter_map(|(id, sc)| {
                    state.components
                        .get(id)
                        .map(|c| measure(state, c, (width / n).max(1), sc.as_ref()))
                })
                .max()
                .unwrap_or(1)
        }
        "Text" => {
            let text = comp.prop_dyn("text")
                .and_then(|d| resolve(&d, &state.data_model, scope).ok())
                .map(|v| display(&v))
                .unwrap_or_default();
            wrap_text(&text, width as usize).len() as u16
        }
        "TagList" => {
            let items = comp.prop_dyn("items")
                .and_then(|d| resolve(&d, &state.data_model, scope).ok())
                .unwrap_or(Value::Null);
            match items {
                Value::Array(a) if !a.is_empty() => 1,
                _ => 1,
            }
        }
        _ => 1, // Title/Metric/Badge/Button/Divider/Checkbox/Todo/TextInput/未知
    }
}

/// 绘制上下文：拆散 UiState 的借用（registry 读 + hits/interactive 写互不相交）。
struct Frame<'a> {
    reg: &'a Registry,
    buf: &'a mut Buffer,
    clip: Rect,
    hits: &'a mut Vec<Hit>,
    interactive: &'a mut Vec<CompKey>,
    mouse: (u16, u16),
    focus: Option<CompKey>,
    pressed: Option<CompKey>,
    editing: Option<(CompKey, String)>,
    unsupported: &'a mut Vec<String>,
}

fn put(buf: &mut Buffer, clip: Rect, x: u16, y: u16, s: &str, style: Style) {
    if y < clip.y || y >= clip.y + clip.height || x < clip.x || x >= clip.x + clip.width {
        return;
    }
    let _ = buf.set_stringn(x, y, s, (clip.x + clip.width - x) as usize, style);
}

fn put_line(buf: &mut Buffer, clip: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y < clip.y || y >= clip.y + clip.height {
        return;
    }
    let max = (clip.x + clip.width).saturating_sub(x.max(clip.x)) as usize;
    let _ = buf.set_stringn(x.max(clip.x), y, text, max, style);
}

fn box_draw(buf: &mut Buffer, clip: Rect, area: Rect, title: &str, style: Style) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let right = area.x + area.width - 1;
    let bottom = area.y + area.height - 1;
    put(buf, clip, area.x, area.y, "┌", style);
    put(buf, clip, right, area.y, "┐", style);
    put(buf, clip, area.x, bottom, "└", style);
    put(buf, clip, right, bottom, "┘", style);
    for x in area.x + 1..right {
        put(buf, clip, x, area.y, "─", style);
        put(buf, clip, x, bottom, "─", style);
    }
    for y in area.y + 1..bottom {
        put(buf, clip, area.x, y, "│", style);
        put(buf, clip, right, y, "│", style);
    }
    if !title.is_empty() {
        put_line(buf, clip, area.x + 2, area.y, &format!(" {title} "), style);
    }
}

const S_BORDER: Style = Style::new().fg(Color::Blue);
const S_TITLE: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
const S_TEXT: Style = Style::new().fg(Color::Gray);
const S_DIM: Style = Style::new().fg(Color::DarkGray);
const S_ACCENT: Style = Style::new().fg(Color::Cyan);

fn comp_key(surface: &str, comp: &WireComponent, scope: Option<&Scope>) -> CompKey {
    CompKey {
        surface: surface.to_string(),
        component: comp.id.clone(),
        scope: scope.map(|s| s.index),
    }
}

fn draw_component(f: &mut Frame<'_>, surface: &SurfaceState, comp: &WireComponent, scope: Option<&Scope>, area: Rect, clip: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if let Some(show) = &comp.show {
        if let Ok(v) = resolve_value(show, &surface.data_model, scope) {
            if !truthy(&v) {
                return;
            }
        }
    }
    let clip = intersect(area, clip);
    let state = surface;
    match comp.component.as_str() {
        "Stack" => {
            let mut y = area.y;
            for (id, sc) in expand_children(state, comp, scope) {
                let Some(child) = state.components.get(&id) else { continue };
                let h = measure(state, child, area.width, sc.as_ref()).min(area.bottom().saturating_sub(y));
                if h == 0 {
                    continue;
                }
                draw_component(f, state, child, sc.as_ref(), Rect { x: area.x, y, width: area.width, height: h }, clip);
                y += h;
                if y >= area.bottom() {
                    break;
                }
            }
        }
        "Card" | "TodoList" => {
            let title = comp.prop_dyn("title")
                .and_then(|d| resolve(&d, &state.data_model, scope).ok())
                .map(|v| display(&v))
                .unwrap_or_default();
            box_draw(f.buf, clip, area, &title, S_BORDER);
            let inner = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            };
            let mut y = inner.y;
            for (id, sc) in expand_children(state, comp, scope) {
                let Some(child) = state.components.get(&id) else { continue };
                let h = measure(state, child, inner.width, sc.as_ref()).min(inner.bottom().saturating_sub(y));
                if h == 0 {
                    continue;
                }
                draw_component(f, state, child, sc.as_ref(), Rect { x: inner.x, y, width: inner.width, height: h }, intersect(inner, clip));
                y += h;
                if y >= inner.bottom() {
                    break;
                }
            }
        }
        "Row" => {
            let kids = expand_children(state, comp, scope);
            let n = kids.len().max(1) as u16;
            let w = (area.width / n).max(1);
            let mut x = area.x;
            for (id, sc) in kids {
                let Some(child) = state.components.get(&id) else { continue };
                draw_component(f, state, child, sc.as_ref(), Rect { x, y: area.y, width: w.min(area.right().saturating_sub(x)), height: area.height }, clip);
                x += w;
                if x >= area.right() {
                    break;
                }
            }
        }
        "Title" => {
            let text = comp.prop_dyn("text")
                .and_then(|d| resolve(&d, &state.data_model, scope).ok())
                .map(|v| display(&v))
                .unwrap_or_default();
            put_line(f.buf, clip, area.x, area.y, &text, S_TITLE);
        }
        "Text" => {
            let text = comp.prop_dyn("text")
                .and_then(|d| resolve(&d, &state.data_model, scope).ok())
                .map(|v| display(&v))
                .unwrap_or_default();
            for (i, line) in wrap_text(&text, area.width as usize).iter().enumerate() {
                let y = area.y + i as u16;
                if y >= area.bottom() {
                    break;
                }
                put_line(f.buf, clip, area.x, y, line, S_TEXT);
            }
        }
        "Metric" => {
            let label = comp.prop_dyn("label").and_then(|d| resolve(&d, &state.data_model, scope).ok()).map(|v| display(&v)).unwrap_or_default();
            let value = comp.prop_dyn("value").and_then(|d| resolve(&d, &state.data_model, scope).ok()).map(|v| display(&v)).unwrap_or_default();
            let vw = value.width() as u16;
            put_line(f.buf, clip, area.x, area.y, &label, S_DIM);
            if vw < area.width {
                put_line(f.buf, clip, area.x + area.width - vw, area.y, &value, S_TITLE);
            }
        }
        "Badge" => {
            let text = comp.prop_dyn("text").and_then(|d| resolve(&d, &state.data_model, scope).ok()).map(|v| display(&v)).unwrap_or_default();
            put_line(f.buf, clip, area.x, area.y, &format!("‹{text}›"), S_ACCENT);
        }
        "Divider" => {
            let line = "─".repeat(area.width as usize);
            put_line(f.buf, clip, area.x, area.y, &line, S_DIM);
        }
        "TagList" => {
            let items = comp.prop_dyn("items").and_then(|d| resolve(&d, &state.data_model, scope).ok()).unwrap_or(Value::Null);
            let mut x = area.x;
            if let Value::Array(arr) = items {
                for item in arr {
                    let tag = format!("‹{}›", display(&item));
                    put_line(f.buf, clip, x, area.y, &tag, S_ACCENT);
                    x += tag.width() as u16 + 1;
                    if x >= area.right() {
                        break;
                    }
                }
            }
        }
        "Button" => draw_button(f, surface, comp, scope, area),
        "Checkbox" => draw_check(f, surface, comp, scope, area),
        "Todo" => draw_todo(f, surface, comp, scope, area),
        "TextInput" => draw_input(f, surface, comp, scope, area),
        other => {
            put_line(f.buf, clip, area.x, area.y, &format!("⟨未支持组件: {other}⟩"), S_DIM);
            if !f.unsupported.iter().any(|u| u == other) {
                f.unsupported.push(other.to_string());
            }
        }
    }
}

fn draw_button(f: &mut Frame<'_>, state: &SurfaceState, comp: &WireComponent, scope: Option<&Scope>, area: Rect) {
    let label = comp.prop_dyn("label").and_then(|d| resolve(&d, &state.data_model, scope).ok()).map(|v| display(&v)).unwrap_or_default();
    let disabled = comp.prop_dyn("disabled").map(|d| resolve(&d, &state.data_model, scope).map(|v| truthy(&v))).transpose().ok().flatten().unwrap_or(false);
    let key = comp_key(&state.surface_id, comp, scope);
    let hovered = point_in(f.mouse, area);
    let focused = f.focus.as_ref() == Some(&key);
    let pressed = f.pressed.as_ref() == Some(&key) && hovered;
    let style = if disabled {
        S_DIM
    } else if pressed {
        Style::new().fg(Color::White).bg(Color::Blue).add_modifier(Modifier::BOLD)
    } else if hovered {
        Style::new().fg(Color::White).bg(Color::DarkGray)
    } else if focused {
        S_ACCENT.add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Green)
    };
    let text = format!("[ {label} ]");
    put_line(f.buf, intersect(area, f.clip), area.x, area.y, &text, style);
    f.hits.push(Hit { rect: Rect { width: text.width() as u16, ..area }, key: key.clone(), kind: HitKind::Button });
    f.interactive.push(key);
}

fn draw_check(f: &mut Frame<'_>, state: &SurfaceState, comp: &WireComponent, scope: Option<&Scope>, area: Rect) {
    let label = comp.prop_dyn("label").and_then(|d| resolve(&d, &state.data_model, scope).ok()).map(|v| display(&v)).unwrap_or_default();
    let checked = comp.prop_dyn("checked").map(|d| resolve(&d, &state.data_model, scope).map(|v| truthy(&v))).transpose().ok().flatten().unwrap_or(false);
    let key = comp_key(&state.surface_id, comp, scope);
    let focused = f.focus.as_ref() == Some(&key);
    let mark = if checked { "[x]" } else { "[ ]" };
    let style = if focused { S_ACCENT.add_modifier(Modifier::BOLD) } else { S_TEXT };
    put_line(f.buf, intersect(area, f.clip), area.x, area.y, &format!("{mark} {label}"), style);
    f.hits.push(Hit { rect: area, key: key.clone(), kind: HitKind::Toggle });
    f.interactive.push(key);
}

fn draw_todo(f: &mut Frame<'_>, state: &SurfaceState, comp: &WireComponent, scope: Option<&Scope>, area: Rect) {
    let text = comp.prop_dyn("text").and_then(|d| resolve(&d, &state.data_model, scope).ok()).map(|v| display(&v)).unwrap_or_default();
    let done = comp.prop_dyn("done").map(|d| resolve(&d, &state.data_model, scope).map(|v| truthy(&v))).transpose().ok().flatten().unwrap_or(false);
    let meta = comp.prop_dyn("meta").and_then(|d| resolve(&d, &state.data_model, scope).ok()).map(|v| display(&v)).unwrap_or_default();
    let key = comp_key(&state.surface_id, comp, scope);
    let focused = f.focus.as_ref() == Some(&key);
    let mark = if done { "[✓]" } else { "[ ]" };
    let box_style = if done { S_ACCENT } else if focused { S_ACCENT.add_modifier(Modifier::BOLD) } else { S_TEXT };
    let text_style = if done { S_DIM } else { Style::new().fg(Color::White) };
    put_line(f.buf, intersect(area, f.clip), area.x, area.y, mark, box_style);
    put_line(f.buf, intersect(area, f.clip), area.x + 4, area.y, &text, text_style);
    if !meta.is_empty() {
        put_line(f.buf, intersect(area, f.clip), area.x + 5 + text.width() as u16, area.y, &format!("· {meta}"), S_DIM);
    }
    f.hits.push(Hit { rect: area, key: key.clone(), kind: HitKind::Toggle });
    f.interactive.push(key);
}

fn draw_input(f: &mut Frame<'_>, state: &SurfaceState, comp: &WireComponent, scope: Option<&Scope>, area: Rect) {
    let label = comp.prop_dyn("label").and_then(|d| resolve(&d, &state.data_model, scope).ok()).map(|v| display(&v)).unwrap_or_default();
    let key = comp_key(&state.surface_id, comp, scope);
    let editing = f.editing.as_ref().map(|(k, _)| k == &key).unwrap_or(false);
    let current = if editing {
        f.editing.as_ref().map(|(_, b)| b.clone()).unwrap_or_default()
    } else {
        comp.prop_dyn("value").and_then(|d| resolve(&d, &state.data_model, scope).ok()).map(|v| display(&v)).unwrap_or_default()
    };
    let focused = f.focus.as_ref() == Some(&key);
    let label_style = S_DIM;
    let value_style = if focused { S_ACCENT.add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::White) };
    let cursor = if editing { "▌" } else if focused { "_" } else { "" };
    let x0 = area.x;
    let lx = label.width() as u16;
    put_line(f.buf, intersect(area, f.clip), x0, area.y, &label, label_style);
    put_line(f.buf, intersect(area, f.clip), x0 + lx + 1, area.y, "：", label_style);
    let mut vx = x0 + lx + 3;
    if current.is_empty() && !editing {
        let ph = comp.props.get("placeholder").map(|v| display(v)).unwrap_or_default();
        put_line(f.buf, intersect(area, f.clip), vx, area.y, &ph, S_DIM);
    } else {
        put_line(f.buf, intersect(area, f.clip), vx, area.y, &current, value_style);
        vx += current.width() as u16;
    }
    if !cursor.is_empty() {
        put_line(f.buf, intersect(area, f.clip), vx, area.y, cursor, S_ACCENT);
    }
    f.hits.push(Hit { rect: area, key: key.clone(), kind: HitKind::Input });
    f.interactive.push(key);
}

fn draw_placeholder(f: &mut Frame<'_>, surface_id: &str, reason: &str, area: Rect, sealed: bool) {
    box_draw(f.buf, f.clip, area, &format!("生成式界面 · {surface_id}"), Style::new().fg(Color::Yellow));
    let line = if sealed { format!("结构不完整: {reason}") } else { "生成中…".to_string() };
    put_line(f.buf, intersect(area, f.clip), area.x + 2, area.y + 1, &line, S_DIM);
}

/// 单个 surface 的测高（供滚动与摆位）。
fn surface_height(reg: &Registry, id: &str, width: u16) -> u16 {
    let Some(state) = reg.surfaces.get(id) else { return 3 };
    if reg.render_blocker(id).is_some() {
        return 3;
    }
    let Some(root) = state.components.get("root") else { return 3 };
    measure(state, root, width, None) + 2
}

fn render_frame(ui: &mut UiState, area: Rect, buf: &mut Buffer) {
    // 外壳：header 1 行 / body 弹性 / footer 2 行。
    let header = Rect { height: 1, ..area };
    let footer = Rect { y: area.bottom().saturating_sub(2), height: 2, ..area };
    let body = Rect { y: area.y + 1, height: area.height.saturating_sub(3), ..area };

    let head_text = " A2UI v1.0 → ratatui 可行性 demo │ [Tab] 焦点 [Enter/Space] 激活/提交 [n] 流式下一阶段 [j/k] 滚动 [q] 退出 │ 鼠标可用";
    put_line(buf, header, 0, header.y, head_text, Style::new().fg(Color::DarkGray));

    // 测高求总高 → clamp 滚动。
    let inner_w = body.width;
    let total: u16 = ui.order.iter().map(|id| surface_height(&ui.registry, id, inner_w) + 1).sum();
    ui.scroll = ui.scroll.min(total.saturating_sub(body.height));

    let UiState { registry, order, hits, interactive, focus, pressed, editing, mouse, unsupported, scroll, .. } = ui;
    let mut f = Frame {
        reg: registry,
        buf,
        clip: body,
        hits,
        interactive,
        mouse: *mouse,
        focus: focus.clone(),
        pressed: pressed.clone(),
        editing: editing.clone(),
        unsupported,
    };
    let mut y = body.y.saturating_sub(*scroll);
    for id in order.iter() {
        let h = surface_height(f.reg, id, inner_w);
        let area_s = Rect { x: body.x, y, width: inner_w, height: h };
        y += h + 1;
        if area_s.y + h <= body.y || area_s.y >= body.bottom() {
            continue;
        }
        draw_surface(&mut f, id, area_s);
    }

    // footer：回传队列尾行 + 状态。
    let out = ui.outbox.last().cloned().unwrap_or_else(|| "回传队列: 空（followUp 会把 prompt + dataModel 字段拼成一条普通消息）".into());
    let first = out.lines().next().unwrap_or("");
    put_line(buf, footer, 0, footer.y, &format!("→ 回传: {first}"), Style::new().fg(Color::Yellow));
    put_line(
        buf,
        footer,
        0,
        footer.y + 1,
        &format!("{} │ 未支持: {:?} │ F1 帮助略", ui.status, ui.unsupported),
        S_DIM,
    );
    let _ = rect_h(body);
}

fn draw_surface(f: &mut Frame<'_>, id: &str, area: Rect) {
    let Some(state) = f.reg.surfaces.get(id) else { return };
    match f.reg.render_blocker(id) {
        Some(reason) => {
            // sealed（消息源已定）→ 具名暴露违规；仍在流式（demo 里即 table）→ 生成中占位。
            let sealed = id != "table";
            draw_placeholder(f, id, &reason, area, sealed);
        }
        None => {
            let inner = Rect { x: area.x, y: area.y + 1, width: area.width, height: area.height - 1 };
            if let Some(root) = state.components.get("root").cloned() {
                draw_component(f, state, &root, None, inner, f.clip);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 交互：鼠标 + 焦点环 + 动作分发（qaqh.action.* / followUp）
// ─────────────────────────────────────────────────────────────────────────────

pub fn hit_at(ui: &UiState, p: (u16, u16)) -> Option<Hit> {
    ui.hits.iter().rev().find(|h| point_in(p, h.rect)).cloned()
}

fn bind_path_of(state: &SurfaceState, key: &CompKey, prop: &str) -> Option<String> {
    let comp = state.components.get(&key.component)?;
    match comp.prop_dyn(prop)? {
        Dyn::Path(p) if p.starts_with("/s/") => Some(p),
        _ => None,
    }
}

fn toggle(ui: &mut UiState, hit: &Hit) {
    let Some(state) = ui.registry.surfaces.get(&hit.key.surface) else { return };
    let prop = match hit.kind {
        HitKind::Toggle if state.components.get(&hit.key.component).map(|c| c.component.as_str()) == Some("Checkbox") => "checked",
        _ => "done",
    };
    let Some(path) = bind_path_of(state, &hit.key, prop) else {
        ui.status = format!("{} 无可写绑定（仅 /s/ 前缀可写）", hit.key.component);
        return;
    };
    let cur = state.data_model.pointer(&path).map(truthy).unwrap_or(false);
    let comp_id = hit.key.component.clone();
    let surface_id = hit.key.surface.clone();
    pointer_set(&mut ui.registry.surfaces.get_mut(&surface_id).unwrap().data_model, &path, Value::Bool(!cur));
    ui.status = format!("{} {} → {}", comp_id, prop, !cur);
}

fn start_edit(ui: &mut UiState, hit: &Hit) {
    let Some(state) = ui.registry.surfaces.get(&hit.key.surface) else { return };
    let cur = bind_path_of(state, &hit.key, "value")
        .and_then(|p| state.data_model.pointer(&p))
        .map(display)
        .unwrap_or_default();
    ui.focus = Some(hit.key.clone());
    ui.editing = Some((hit.key.clone(), cur));
    ui.status = "编辑中：Enter 提交，Esc 取消".into();
}

fn commit_edit(ui: &mut UiState) {
    let Some((key, buf)) = ui.editing.take() else { return };
    let Some(state) = ui.registry.surfaces.get_mut(&key.surface) else { return };
    let Some(path) = bind_path_of(state, &key, "value") else {
        ui.status = "输入框无可写绑定".into();
        return;
    };
    pointer_set(&mut state.data_model, &path, Value::String(buf.clone()));
    ui.status = format!("已写回 {path} = {buf:?}");
}

/// 动作分发（对齐桌面端 SurfaceView2 的本地拦截语义）。
pub fn dispatch(ui: &mut UiState, surface_id: &str, event: &EventSpec, scope: Option<&Scope>) {
    let Some(state) = ui.registry.surfaces.get_mut(surface_id) else { return };
    let ctx = event.context.clone().unwrap_or_default();
    let resolve_ctx = |k: &str| -> Result<Value, String> {
        ctx.get(k).map(|v| resolve_value(v, &state.data_model.clone(), scope)).unwrap_or(Err(format!("context 缺 {k}")))
    };
    match event.name.as_str() {
        "qaqh.action.set" => {
            let (Ok(key), Ok(value)) = (resolve_ctx("key"), resolve_ctx("value")) else {
                ui.status = "set 动作参数求值失败".into();
                return;
            };
            let path = format!("/s/{}", display(&key));
            pointer_set(&mut state.data_model, &path, value.clone());
            ui.status = format!("set {path} = {}", display(&value));
        }
        "qaqh.action.append" => {
            let (Ok(key), Ok(value)) = (resolve_ctx("key"), resolve_ctx("value")) else { return };
            let path = format!("/s/{}", display(&key));
            let cur = state.data_model.pointer(&path).cloned().unwrap_or(Value::Array(vec![]));
            if let Value::Array(mut arr) = cur {
                arr.push(value);
                pointer_set(&mut state.data_model, &path, Value::Array(arr));
                ui.status = format!("append {path}");
            }
        }
        "qaqh.action.remove" => {
            let (Ok(key), Ok(value)) = (resolve_ctx("key"), resolve_ctx("value")) else { return };
            let path = format!("/s/{}", display(&key));
            let cur = state.data_model.pointer(&path).cloned().unwrap_or(Value::Array(vec![]));
            if let Value::Array(mut arr) = cur {
                if let Some(i) = arr.iter().position(|v| *v == value) {
                    arr.remove(i);
                }
                pointer_set(&mut state.data_model, &path, Value::Array(arr));
                ui.status = format!("remove {path}");
            }
        }
        "qaqh.action.reset" => {
            state.data_model = state.initial_model.clone();
            ui.status = "reset → 初始 dataModel".into();
        }
        "qaqh.action.copy" => {
            let Ok(value) = resolve_ctx("value") else { return };
            ui.status = format!("copy（演示仅记录）: {}", display(&value));
        }
        "qaqh.action.openLink" => {
            let Ok(url) = resolve_ctx("url") else { return };
            ui.status = format!("openLink（真实 TUI 走 browser.rs）: {}", display(&url));
        }
        "qaqh.action.followUp" => {
            let Ok(prompt) = resolve_ctx("prompt") else { return };
            let fields = ctx.get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut lines = vec![display(&prompt)];
            for field in fields {
                let label = field.get("label").map(display).unwrap_or_default();
                let Some(key) = field.get("key").map(display) else { continue };
                let v = state.data_model.pointer(&format!("/s/{key}")).map(display).unwrap_or_default();
                lines.push(format!("- {label}：{v}"));
            }
            ui.outbox.push(lines.join("\n"));
            ui.status = "followUp 已组装（真实 TUI 走 sendMessage）".into();
        }
        other => ui.status = format!("命名事件透传（P2 转 agent 通道）: {other}"),
    }
}

fn activate_button(ui: &mut UiState, key: &CompKey) {
    let Some(state) = ui.registry.surfaces.get(&key.surface) else { return };
    let Some(comp) = state.components.get(&key.component) else { return };
    // disabled 语义在激活闸门处落实（视觉只是提示）：`disabled` prop 求值为真即拦。
    let disabled = comp.prop_dyn("disabled").map(|d| resolve(&d, &state.data_model, None).map(|v| truthy(&v))).transpose().ok().flatten().unwrap_or(false);
    if disabled {
        ui.status = "按钮已禁用（disabled 联动生效）".into();
        return;
    }
    let Some(event) = comp.on_press.as_ref().map(|a| a.event.clone()) else {
        ui.status = "按钮无动作".into();
        return;
    };
    let scope = key.scope.map(|i| Scope { index: i, item: Value::Null });
    let surface_id = key.surface.clone();
    dispatch(ui, &surface_id, &event, scope.as_ref());
}

pub fn handle_mouse(ui: &mut UiState, ev: MouseEvent) {
    let pos = (ev.column, ev.row);
    match ev.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            ui.mouse = pos;
            if let Some(hit) = hit_at(ui, pos) {
                match hit.kind {
                    HitKind::Button => ui.pressed = Some(hit.key),
                    HitKind::Toggle => toggle(ui, &hit),
                    HitKind::Input => start_edit(ui, &hit),
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(pressed) = ui.pressed.take() {
                if hit_at(ui, pos).is_some_and(|h| h.key == pressed) {
                    activate_button(ui, &pressed);
                }
            }
        }
        MouseEventKind::ScrollUp => ui.scroll = ui.scroll.saturating_sub(3),
        MouseEventKind::ScrollDown => ui.scroll = ui.scroll.saturating_add(3),
        MouseEventKind::Moved | MouseEventKind::Drag(..) => ui.mouse = pos,
        _ => {}
    }
}

pub fn handle_key(ui: &mut UiState, code: KeyCode) -> bool {
    // 编辑态优先吃键。
    if ui.editing.is_some() {
        match code {
            KeyCode::Enter => commit_edit(ui),
            KeyCode::Esc => {
                ui.editing = None;
                ui.status = "取消编辑".into();
            }
            KeyCode::Backspace => {
                if let Some((_, buf)) = ui.editing.as_mut() {
                    buf.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some((_, buf)) = ui.editing.as_mut() {
                    buf.push(c);
                }
            }
            _ => {}
        }
        return true;
    }
    match code {
        KeyCode::Tab => {
            if ui.interactive.is_empty() {
                return true;
            }
            let next = match ui.focus.as_ref().and_then(|k| ui.interactive.iter().position(|i| i == k)) {
                Some(i) => (i + 1) % ui.interactive.len(),
                None => 0,
            };
            ui.focus = Some(ui.interactive[next].clone());
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let Some(key) = ui.focus.clone() else { return true };
            let kind = ui.registry.surfaces.get(&key.surface)
                .and_then(|s| s.components.get(&key.component))
                .map(|c| c.component.clone());
            match kind.as_deref() {
                Some("Button") => activate_button(ui, &key),
                Some("TextInput") => start_edit_at(ui, &key),
                Some("Todo") | Some("Checkbox") => {
                    if let Some(hit) = ui.hits.iter().find(|h| h.key == key).cloned() {
                        toggle(ui, &hit);
                    }
                }
                _ => {}
            }
        }
        KeyCode::Char('j') | KeyCode::Down => ui.scroll = ui.scroll.saturating_add(2),
        KeyCode::Char('k') | KeyCode::Up => ui.scroll = ui.scroll.saturating_sub(2),
        KeyCode::Char('n') => ui.advance_stage(),
        KeyCode::Char('r') => *ui = UiState::new(),
        KeyCode::Char('q') => return false,
        _ => {}
    }
    true
}

fn start_edit_at(ui: &mut UiState, key: &CompKey) {
    if let Some(hit) = ui.hits.iter().find(|h| h.key == *key).cloned() {
        start_edit(ui, &hit);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 入口：headless 与交互两种模式共用同一渲染/交互核心
// ─────────────────────────────────────────────────────────────────────────────

const HEADLESS_W: u16 = 96;
const HEADLESS_H: u16 = 32;

fn render_to_string(ui: &mut UiState) -> String {
    let area = Rect::new(0, 0, HEADLESS_W, HEADLESS_H);
    let mut buf = Buffer::empty(area);
    render_frame(ui, area, &mut buf);
    let mut out = String::new();
    for y in 0..area.height {
        let mut row = String::new();
        // 按显示宽度推进：宽字符覆盖的占位格（被 reset 成空格）不重复还原，
        // 与真实终端的光标推进一致。
        let mut x = 0u16;
        while x < area.width {
            let sym = buf[(x, y)].symbol();
            row.push_str(sym);
            x += sym.width().max(1) as u16;
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

/// headless 模拟一轮真实交互：点 todo ×2 → 输入补充说明 → 提交 → 计数器 +2 → 加 beta。
/// 与交互模式走完全相同的 handle_mouse / handle_key 路径。
pub fn simulate(ui: &mut UiState) {
    let mut click = |ui: &mut UiState, want: &str| {
        // 命中图是上一帧的绘制产物：先渲一帧再取坐标。
        let area = Rect::new(0, 0, HEADLESS_W, HEADLESS_H);
        let mut buf = Buffer::empty(area);
        render_frame(ui, area, &mut buf);
        let Some(hit) = ui.hits.iter().find(|h| h.key.component == want).cloned() else { return };
        let p = (hit.rect.x + 1, hit.rect.y + rect_h(hit.rect) / 2);
        handle_mouse(ui, MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: p.0, row: p.1, modifiers: KeyModifiers::empty() });
        handle_mouse(ui, MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), column: p.0, row: p.1, modifiers: KeyModifiers::empty() });
    };
    click(ui, "t1");
    click(ui, "t2");
    // 输入框：聚焦 → 键入 → 提交（提交确认按钮由 disabled 联动解锁）。
    click(ui, "ask");
    for c in "先跑 lint 再跑测试".chars() {
        handle_key(ui, KeyCode::Char(c));
    }
    handle_key(ui, KeyCode::Enter);
    click(ui, "send");
    click(ui, "increment");
    click(ui, "increment");
    click(ui, "add");
}

fn headless() {
    let mut ui = UiState::new();
    println!("╭──── 帧 A：载入后（plan / counter 就位，按钮 disabled、tag 只有 alpha）");
    print!("{}", render_to_string(&mut ui));
    println!("╰────");
    simulate(&mut ui);
    println!("╭──── 帧 B：模拟交互后（todo 勾选、文本已写回、按钮解锁、计数 2、tag alpha+beta）");
    print!("{}", render_to_string(&mut ui));
    println!("╰────");
    println!("回传队列（真实实现走 sendMessage）:");
    for (i, msg) in ui.outbox.iter().enumerate() {
        println!("  [{}] {}", i + 1, msg.replace('\n', "\n      "));
    }
    let dm = &ui.registry.surfaces["plan"].data_model;
    println!("plan dataModel 终态: {}", serde_json::to_string(dm).unwrap_or_default());
}

fn interactive() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut ui = UiState::new();
    loop {
        terminal.draw(|frame| {
            render_frame(&mut ui, frame.area(), frame.buffer_mut());
        })?;
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                if !handle_key(&mut ui, k.code) {
                    break;
                }
            }
            Event::Mouse(m) => handle_mouse(&mut ui, m),
            Event::Resize(..) => {}
            _ => {}
        }
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.iter().position(|a| a == "--fence").and_then(|i| args.get(i + 1)) {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut ui = UiState::from_text(&text);
                print!("{}", render_to_string(&mut ui));
            }
            Err(e) => eprintln!("读取 {path} 失败: {e}"),
        }
        let _ = io::stdout().flush();
    } else if args.iter().any(|a| a == "--headless") {
        headless();
        let _ = io::stdout().flush();
    } else if let Err(e) = interactive() {
        eprintln!("终端初始化失败: {e}");
        std::process::exit(1);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 回归：wire 解析 / 渲染闸门 / 预算 / 求值 / 布局 / 交互全链
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_parse_accepts_real_shapes_and_rejects_bad() {
        assert!(Message::parse(PLAN_CREATE).is_ok());
        assert!(Message::parse(COUNTER_CREATE).is_ok());
        assert!(Message::parse(TABLE_STAGE2).is_ok());
        assert!(Message::parse(r#"{"version":"v0.9","createSurface":{"surfaceId":"x"}}"#).is_err());
        assert!(Message::parse(r#"{"version":"v1.0"}"#).is_err());
        assert!(Message::parse("not json").is_err());
    }

    #[test]
    fn render_gate_blocks_until_forward_refs_resolve() {
        let mut reg = Registry::default();
        let mut log = Vec::new();
        feed_line(&mut reg, TABLE_STAGE1, &mut log);
        assert!(log.is_empty());
        assert_eq!(reg.render_blocker("table"), Some("引用未解析: r2".into()));
        feed_line(&mut reg, TABLE_STAGE2, &mut log);
        assert!(log.is_empty());
        assert_eq!(reg.render_blocker("table"), None);
        assert_eq!(reg.render_blocker("missing"), None, "未知 surface 不设闸（上层按不存在处理）");
    }

    #[test]
    fn budget_rejects_oversized_surface() {
        let comps: Vec<String> = (0..600)
            .map(|i| format!(r#"{{"id":"c{i}","component":"Text","text":"x"}}"#))
            .collect();
        let msg = format!(
            r#"{{"version":"v1.0","createSurface":{{"surfaceId":"big","components":[{}]}}}}"#,
            comps.join(",")
        );
        let mut reg = Registry::default();
        assert!(matches!(reg.apply(Message::parse(&msg).unwrap()), Err(ApplyError::Budget(_))));
    }

    #[test]
    fn gate_detects_cycles() {
        let mut reg = Registry::default();
        let mut log = Vec::new();
        feed_line(
            &mut reg,
            r#"{"version":"v1.0","createSurface":{"surfaceId":"cyc","components":[
              {"id":"root","component":"Stack","children":["a"]},
              {"id":"a","component":"Stack","children":["b"]},
              {"id":"b","component":"Stack","children":["a"]}
            ],"dataModel":{}}}"#,
            &mut log,
        );
        assert!(log.is_empty());
        assert!(reg.render_blocker("cyc").unwrap().contains("形成环"));
    }

    #[test]
    fn eval_paths_calls_and_truthiness() {
        let dm: Value = serde_json::from_str(r#"{"s":{"count":2,"tags":["alpha"],"note":""}}"#).unwrap();
        let v = resolve_value(&serde_json::json!({"@path":"/s/count"}), &dm, None).unwrap();
        assert_eq!(v, Value::from(2));
        let v = resolve_value(
            &serde_json::json!({"@call":"qaqh:arith.add","args":{"left":"进度：","right":{"@path":"/s/count"}}}),
            &dm,
            None,
        )
        .unwrap();
        assert_eq!(v, Value::String("进度：2".into()));
        let v = resolve_value(
            &serde_json::json!({"@call":"qaqh:logic.eq","args":{"left":{"@path":"/s/note"},"right":""}}),
            &dm,
            None,
        )
        .unwrap();
        assert_eq!(v, Value::Bool(true));
        assert!(truthy(&Value::String("x".into())));
        assert!(!truthy(&Value::String(String::new())));
        let scope = Scope { index: 3, item: Value::String("beta".into()) };
        let v = resolve_value(&serde_json::json!({"@path":""}), &dm, Some(&scope)).unwrap();
        assert_eq!(v, Value::String("beta".into()));
        assert!(resolve_value(&serde_json::json!({"@call":"window.alert"}), &dm, None).is_err(), "白名单外必须拒绝");
    }

    #[test]
    fn pointer_set_remove_roundtrip() {
        let mut v: Value = serde_json::from_str(r#"{"s":{"a":1,"list":["x"]}}"#).unwrap();
        pointer_set(&mut v, "/s/a", Value::from(9));
        assert_eq!(v.pointer("/s/a"), Some(&Value::from(9)));
        pointer_set(&mut v, "/s/list/0", Value::String("y".into()));
        assert_eq!(v.pointer("/s/list/0"), Some(&Value::String("y".into())));
        pointer_set(&mut v, "/s/deep/nest", Value::Bool(true));
        assert_eq!(v.pointer("/s/deep/nest"), Some(&Value::Bool(true)));
        assert_eq!(pointer_remove(&mut v, "/s/list/0"), Some(Value::String("y".into())));
        assert_eq!(pointer_remove(&mut v, "/s/a"), Some(Value::from(9)));
        let mut root = Value::Object(Map::new());
        pointer_set(&mut root, "", Value::from(7));
        assert_eq!(root, Value::from(7));
    }

    #[test]
    fn full_pipeline_renders_fixture() {
        let mut ui = UiState::new();
        let text = render_to_string(&mut ui);
        for want in ["执行计划", "[ ] 分帧器", "进度：false", "提交确认", "当前计数", "‹alpha›", "· alpha"] {
            assert!(text.contains(want), "渲染缺 {want:?}\n{text}");
        }
    }

    #[test]
    fn simulate_drive_toggles_input_submit_and_counter() {
        let mut ui = UiState::new();
        simulate(&mut ui);
        let text = render_to_string(&mut ui);
        assert!(text.contains("[✓] 分帧器"), "\n{text}");
        assert!(text.contains("[✓] 生成式界面"));
        assert!(text.contains("进度：true"));
        assert!(text.contains("先跑 lint 再跑测试"));
        // disabled 随 confirm_note 解锁后提交成功。
        assert_eq!(ui.outbox.len(), 1);
        let msg = &ui.outbox[0];
        assert!(msg.starts_with("请按计划继续"));
        assert!(msg.contains("- 说明：先跑 lint 再跑测试"));
        // counter 走 @call set。
        assert!(text.contains("2"), "计数应为 2\n{text}");
        let dm = &ui.registry.surfaces["counter"].data_model;
        assert_eq!(dm.pointer("/s/count"), Some(&Value::from(2)));
        assert_eq!(dm.pointer("/s/tags"), Some(&serde_json::json!(["alpha", "beta"])));
    }

    #[test]
    fn disabled_button_blocks_follow_up_until_input() {
        let mut ui = UiState::new();
        let area = Rect::new(0, 0, HEADLESS_W, HEADLESS_H);
        let mut buf = Buffer::empty(area);
        render_frame(&mut ui, area, &mut buf);
        let hit = ui.hits.iter().find(|h| h.key.component == "send").cloned().unwrap();
        let p = (hit.rect.x + 1, hit.rect.y);
        for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
            handle_mouse(&mut ui, MouseEvent { kind, column: p.0, row: p.1, modifiers: KeyModifiers::empty() });
        }
        assert!(ui.outbox.is_empty(), "disabled 时 followUp 必须被拦");
        // 写入文本后（不走点击，直接改模型模拟 agent 端 updateDataModel）再点。
        pointer_set(&mut ui.registry.surfaces.get_mut("plan").unwrap().data_model, "/s/confirm_note", Value::String("ok".into()));
        render_frame(&mut ui, area, &mut buf);
        let hit = ui.hits.iter().find(|h| h.key.component == "send").cloned().unwrap();
        let p = (hit.rect.x + 1, hit.rect.y);
        for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
            handle_mouse(&mut ui, MouseEvent { kind, column: p.0, row: p.1, modifiers: KeyModifiers::empty() });
        }
        assert_eq!(ui.outbox.len(), 1);
    }

    #[test]
    fn streaming_stage_gate_shows_placeholder_then_surface() {
        let mut ui = UiState::new();
        ui.advance_stage();
        let text = render_to_string(&mut ui);
        assert!(text.contains("生成中"), "阶段 1 应占位\n{text}");
        ui.advance_stage();
        let text = render_to_string(&mut ui);
        for want in ["会议名单", "张三", "李四", "王五"] {
            assert!(text.contains(want), "阶段 2 应浮现 {want:?}\n{text}");
        }
    }

    // ── DSL 编译链（qaqh-ui 围栏 → IR → A2UI wire）──

    #[test]
    fn dsl_translate_counter_matches_handwritten_wire_exactly() {
        // 交叉验证：真实 compiler（tsx → translate）产物与按 translate.ts 约定
        // 手写的 wire fixture 逐字段相等——证明 demo 的 wire 层理解与桌面端一致。
        let source = dsl::V2_COUNTER_FENCE
            .split("```qaqh-ui v=0.2 format=tsx\n").nth(1).unwrap()
            .strip_suffix("```\n").unwrap();
        let ir = dsl::compile_source(source, dsl::UiFormat::Tsx, true).unwrap();
        let wire = dsl::translate_create(&ir);
        let expected = Message::parse(COUNTER_CREATE).unwrap();
        assert_eq!(wire.create_surface, expected.create_surface, "translate 产物与手写 wire 不一致");
    }

    #[test]
    fn dsl_ask_todo_is_the_default_interactive_plan_surface() {
        // UiState::new 走 DSL 链；既有交互 simulate 全部照常工作即证明等价。
        let mut ui = UiState::new();
        let text = render_to_string(&mut ui);
        for want in ["执行计划", "[ ] 分帧器 · 阶段 1", "进度：false", "提交确认"] {
            assert!(text.contains(want), "缺 {want:?}\n{text}");
        }
        simulate(&mut ui);
        let text = render_to_string(&mut ui);
        assert!(text.contains("进度：true"));
        assert!(ui.outbox.iter().any(|m| m.contains("- 说明：先跑 lint 再跑测试")));
        assert_eq!(ui.registry.surfaces["plan"].data_model.pointer("/s/confirm_note"),
                   Some(&Value::String("先跑 lint 再跑测试".into())));
    }

    #[test]
    fn dsl_decoder_streams_table_through_stages() {
        // genui-check 的分阶段全文：holdback → 部分编译（前向引用剪枝）→ seal 转正。
        let mut registry = Registry::default();
        let mut created = std::collections::BTreeSet::new();
        let mut saw_ui_part = false;
        for (i, stage) in dsl::table_stages().iter().enumerate() {
            let parts = dsl::split_content_parts(stage, false);
            match i {
                0 | 1 => assert!(parts.iter().all(|p| matches!(p, dsl::ContentPart::Markdown(_))),
                    "阶段 {i} 应只有 markdown（holdback 吞掉围栏头）: {parts:?}"),
                2 => assert!(parts.iter().any(|p| matches!(p, dsl::ContentPart::Ui { sealed: false, .. }))),
                3 | 4 => {
                    for part in &parts {
                        if matches!(part, dsl::ContentPart::Ui { .. }) {
                            saw_ui_part = true;
                            let ir = dsl::ingest_ui_part(&mut registry, part, &mut created).unwrap();
                            if i == 3 {
                                assert!(!ir.nodes.contains_key("r2"), "部分编译不应有 r2");
                            } else {
                                assert!(ir.nodes.contains_key("r3"), "seal 后应有 r3");
                            }
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        assert!(saw_ui_part);
        let dm = &registry.surfaces["table"].data_model;
        assert_eq!(dm.pointer("/s/rows"), Some(&serde_json::json!(["张三", "李四"])));
    }

    #[test]
    fn dsl_jsonl_form_and_show_condition() {
        let mut ui = UiState::from_text(dsl::FORM_JSONL_FENCE);
        let text = render_to_string(&mut ui);
        for want in ["表单", "输入名称", "开启", "关闭", "‹alpha›"] {
            assert!(text.contains(want), "缺 {want:?}\n{text}");
        }
        assert!(!text.contains("深色已开启"), "dark=false 时 show 应剪掉 badge\n{text}");
        // 点「开启」→ set dark=true → badge 浮现。
        let area = Rect::new(0, 0, HEADLESS_W, HEADLESS_H);
        let mut buf = Buffer::empty(area);
        render_frame(&mut ui, area, &mut buf);
        let hit = ui.hits.iter().find(|h| h.key.component == "on").cloned().unwrap();
        let p = (hit.rect.x + 1, hit.rect.y);
        for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
            handle_mouse(&mut ui, MouseEvent { kind, column: p.0, row: p.1, modifiers: KeyModifiers::empty() });
        }
        let text = render_to_string(&mut ui);
        assert!(text.contains("深色已开启"), "set dark=true 后 badge 应出现\n{text}");
    }

    #[test]
    fn dsl_tolerant_tail_renders_completed_prefix_only() {
        // 流式 tsx：截断在 t2 之后（缺 ask/send/ui.end）——已到齐的先渲染。
        let cut = dsl::ASK_TODO_FENCE.split("ui.node(\"summary\"").next().unwrap().to_string();
        let mut ui = UiState::from_text(&cut);
        let text = render_to_string(&mut ui);
        assert!(text.contains("分帧器") && text.contains("生成式界面"), "\n{text}");
        assert!(!text.contains("提交确认"), "截断处之后的节点不应渲染\n{text}");
    }

    #[test]
    fn dsl_rejects_unknown_component_and_out_of_catalog_props() {
        let bad = "```qaqh-ui v=0.2 format=tsx\nui.surface(\"x\");\nui.node(\"a\", <Widget text=\"hi\" />);\nui.end();\n```\n";
        let err = dsl::split_content_parts(bad, true).into_iter()
            .find_map(|p| match p {
                dsl::ContentPart::Ui { source, sealed, format, .. } =>
                    dsl::compile_source(&source, format, sealed).err(),
                _ => None,
            }).expect("未知组件必须具名报错");
        assert!(err.message.contains("未知组件类型"), "{err}");
        let bad_prop = "```qaqh-ui v=0.2 format=tsx\nui.surface(\"x\");\nui.node(\"a\", <Text text=\"hi\" color=\"red\" />);\nui.end();\n```\n";
        let err = dsl::split_content_parts(bad_prop, true).into_iter()
            .find_map(|p| match p {
                dsl::ContentPart::Ui { source, sealed, format, .. } =>
                    dsl::compile_source(&source, format, sealed).err(),
                _ => None,
            }).expect("目录外属性必须拒绝");
        assert!(err.message.contains("不支持属性"), "{err}");
    }
}
