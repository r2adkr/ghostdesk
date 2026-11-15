//! UI Automation (COM, MTA): 요소 목록·패턴 실행·값 읽기/쓰기.
//!
//! 규칙: 포커스·전경을 바꾸는 호출(IUIAutomationElement::SetFocus 등)은 절대 쓰지 않는다.
//! IsOffscreen 은 화면 밖에 둔 창에서 늘 참이므로 쓰지 않는다. 모든 호출 스레드는 MTA 로 초기화한다.

use std::sync::OnceLock;

use anyhow::{anyhow, bail, Result};
use windows::core::{Interface, BSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BOOL, VT_BSTR, VT_I4};
use windows::Win32::UI::Accessibility::*;

use super::w32::Hwnd;

/// 이 스레드를 MTA 로 (이미 초기화되어 있으면 그대로)
pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// MTA 에서 만든 UIA 객체는 MTA 스레드 사이에서 함께 쓸 수 있다
struct Uia(IUIAutomation);
unsafe impl Send for Uia {}
unsafe impl Sync for Uia {}

fn automation() -> Result<IUIAutomation> {
    static U: OnceLock<Option<Uia>> = OnceLock::new();
    com_init();
    let u = U.get_or_init(|| unsafe {
        let a: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
            .or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER))
            .ok()?;
        if let Ok(a2) = a.cast::<IUIAutomation2>() {
            let _ = a2.SetConnectionTimeout(2000);
            let _ = a2.SetTransactionTimeout(3000);
        }
        Some(Uia(a))
    });
    u.as_ref().map(|u| u.0.clone()).ok_or_else(|| anyhow!("UI Automation 을 초기화하지 못했습니다"))
}

/// 스레드 사이에서 옮길 수 있는 UIA 요소 (모든 사용 스레드는 MTA)
#[derive(Clone)]
pub struct UiEl(pub IUIAutomationElement);
unsafe impl Send for UiEl {}
unsafe impl Sync for UiEl {}

/// 요소 하나의 정보 (캐시로 한 번에 읽음)
#[derive(Clone)]
pub struct Node {
    pub el: UiEl,
    pub ctype: i32,
    pub name: String,
    pub aid: String,
    pub class: String,
    /// 화면 좌표 (l, t, r, b), 실제 픽셀
    pub rect: (i32, i32, i32, i32),
    pub enabled: bool,
    pub focused: bool,
    pub hwnd: Hwnd,
    pub value: Option<String>,
    pub readonly: bool,
    pub toggle: Option<i32>,
    pub invoke: bool,
    pub select: bool,
    pub selected: Option<bool>,
    pub expand: bool,
}

pub fn type_name(ct: i32) -> &'static str {
    match ct {
        50000 => "Button",
        50001 => "Calendar",
        50002 => "CheckBox",
        50003 => "ComboBox",
        50004 => "Edit",
        50005 => "Hyperlink",
        50006 => "Image",
        50007 => "ListItem",
        50008 => "List",
        50009 => "Menu",
        50010 => "MenuBar",
        50011 => "MenuItem",
        50012 => "ProgressBar",
        50013 => "RadioButton",
        50014 => "ScrollBar",
        50015 => "Slider",
        50016 => "Spinner",
        50017 => "StatusBar",
        50018 => "Tab",
        50019 => "TabItem",
        50020 => "Text",
        50021 => "ToolBar",
        50022 => "ToolTip",
        50023 => "Tree",
        50024 => "TreeItem",
        50025 => "Custom",
        50026 => "Group",
        50027 => "Thumb",
        50028 => "DataGrid",
        50029 => "DataItem",
        50030 => "Document",
        50031 => "SplitButton",
        50032 => "Window",
        50033 => "Pane",
        50034 => "Header",
        50035 => "HeaderItem",
        50036 => "Table",
        50037 => "TitleBar",
        50038 => "Separator",
        50039 => "SemanticZoom",
        50040 => "AppBar",
        _ => "Element",
    }
}

pub const CT_BUTTON: i32 = 50000;
pub const CT_CHECKBOX: i32 = 50002;
pub const CT_COMBOBOX: i32 = 50003;
pub const CT_EDIT: i32 = 50004;
pub const CT_HYPERLINK: i32 = 50005;
pub const CT_LISTITEM: i32 = 50007;
pub const CT_MENUBAR: i32 = 50010;
pub const CT_MENUITEM: i32 = 50011;
pub const CT_RADIO: i32 = 50013;
pub const CT_SCROLLBAR: i32 = 50014;
pub const CT_TABITEM: i32 = 50019;
pub const CT_TREEITEM: i32 = 50024;
pub const CT_DOCUMENT: i32 = 50030;
pub const CT_TITLEBAR: i32 = 50037;

enum Var {
    Bool(bool),
    I32(i32),
    Str(String),
    None,
}

unsafe fn take_var(mut v: VARIANT) -> Var {
    let inner = &v.Anonymous.Anonymous;
    let out = if inner.vt == VT_BOOL {
        Var::Bool(inner.Anonymous.boolVal.0 != 0)
    } else if inner.vt == VT_I4 {
        Var::I32(inner.Anonymous.lVal)
    } else if inner.vt == VT_BSTR {
        Var::Str((*inner.Anonymous.bstrVal).to_string())
    } else {
        Var::None
    };
    let _ = VariantClear(&mut v);
    out
}

fn cached(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Var {
    unsafe { el.GetCachedPropertyValue(id).map(|v| take_var(v)).unwrap_or(Var::None) }
}

fn cached_bool(el: &IUIAutomationElement, id: UIA_PROPERTY_ID) -> Option<bool> {
    match cached(el, id) {
        Var::Bool(b) => Some(b),
        _ => None,
    }
}

const PROPS: [UIA_PROPERTY_ID; 17] = [
    UIA_ControlTypePropertyId,
    UIA_NamePropertyId,
    UIA_AutomationIdPropertyId,
    UIA_ClassNamePropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_IsEnabledPropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_NativeWindowHandlePropertyId,
    UIA_IsValuePatternAvailablePropertyId,
    UIA_ValueValuePropertyId,
    UIA_ValueIsReadOnlyPropertyId,
    UIA_IsTogglePatternAvailablePropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_IsInvokePatternAvailablePropertyId,
    UIA_IsSelectionItemPatternAvailablePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
    UIA_IsExpandCollapsePatternAvailablePropertyId,
];

fn cache_request(a: &IUIAutomation) -> Result<IUIAutomationCacheRequest> {
    unsafe {
        let c = a.CreateCacheRequest()?;
        for p in PROPS {
            c.AddProperty(p)?;
        }
        Ok(c)
    }
}

fn read_node(el: &IUIAutomationElement) -> Node {
    unsafe {
        let r = el.CachedBoundingRectangle().unwrap_or_default();
        let has_value = cached_bool(el, UIA_IsValuePatternAvailablePropertyId).unwrap_or(false);
        let has_toggle = cached_bool(el, UIA_IsTogglePatternAvailablePropertyId).unwrap_or(false);
        let has_sel = cached_bool(el, UIA_IsSelectionItemPatternAvailablePropertyId).unwrap_or(false);
        Node {
            el: UiEl(el.clone()),
            ctype: el.CachedControlType().map(|c| c.0).unwrap_or(0),
            name: el.CachedName().map(|b| b.to_string()).unwrap_or_default(),
            aid: el.CachedAutomationId().map(|b| b.to_string()).unwrap_or_default(),
            class: el.CachedClassName().map(|b| b.to_string()).unwrap_or_default(),
            rect: (r.left, r.top, r.right, r.bottom),
            enabled: el.CachedIsEnabled().map(|b| b.as_bool()).unwrap_or(true),
            focused: el.CachedHasKeyboardFocus().map(|b| b.as_bool()).unwrap_or(false),
            hwnd: el.CachedNativeWindowHandle().map(|h| h.0 as Hwnd).unwrap_or(0),
            value: if has_value {
                match cached(el, UIA_ValueValuePropertyId) {
                    Var::Str(s) => Some(s),
                    _ => None,
                }
            } else {
                None
            },
            readonly: !has_value || cached_bool(el, UIA_ValueIsReadOnlyPropertyId).unwrap_or(true),
            toggle: if has_toggle {
                match cached(el, UIA_ToggleToggleStatePropertyId) {
                    Var::I32(i) => Some(i),
                    _ => None,
                }
            } else {
                None
            },
            invoke: cached_bool(el, UIA_IsInvokePatternAvailablePropertyId).unwrap_or(false),
            select: has_sel,
            selected: if has_sel { cached_bool(el, UIA_SelectionItemIsSelectedPropertyId) } else { None },
            expand: cached_bool(el, UIA_IsExpandCollapsePatternAvailablePropertyId).unwrap_or(false),
        }
    }
}

/// 창의 컨트롤 보기 트리를 깊이 우선으로 읽는다: (깊이, 요소). 제목 표시줄·메뉴 막대 하위는 건너뛴다.
/// keep(node) 가 false 인 요소의 하위는 내려가지 않는다 (예: 창 내용 영역 밖).
pub fn snapshot(top: Hwnd, max_depth: usize, max_visits: usize, keep: impl Fn(&Node) -> bool) -> Result<Vec<(usize, Node)>> {
    let a = automation()?;
    unsafe {
        let cache = cache_request(&a)?;
        let walker = a.ControlViewWalker()?;
        let root = a
            .ElementFromHandleBuildCache(HWND(top as *mut _), &cache)
            .map_err(|e| anyhow!("이 창의 UI Automation 정보를 읽지 못했습니다: {e}"))?;
        let mut out = Vec::new();
        let mut stack: Vec<(IUIAutomationElement, usize)> = vec![(root, 0)];
        let mut visits = 0usize;
        while let Some((el, d)) = stack.pop() {
            visits += 1;
            if visits > max_visits {
                break;
            }
            let n = read_node(&el);
            let descend = d == 0 || (keep(&n) && !matches!(n.ctype, CT_TITLEBAR | CT_MENUBAR | CT_SCROLLBAR));
            if d > 0 {
                out.push((d, n));
            }
            if !descend || d >= max_depth {
                continue;
            }
            let mut kids = Vec::new();
            let mut c = walker.GetFirstChildElementBuildCache(&el, &cache);
            while let Ok(ch) = c {
                kids.push(ch.clone());
                if kids.len() > 2000 {
                    break;
                }
                c = walker.GetNextSiblingElementBuildCache(&ch, &cache);
            }
            for k in kids.into_iter().rev() {
                stack.push((k, d + 1));
            }
        }
        Ok(out)
    }
}

/// 창(또는 컨트롤) 핸들의 요소
pub fn element_for(h: Hwnd) -> Result<UiEl> {
    let a = automation()?;
    unsafe { Ok(UiEl(a.ElementFromHandle(HWND(h as *mut _))?)) }
}

/// 메뉴 막대의 최상위 항목 이름 (HMENU 가 없는 앱용)
pub fn menubar_items(top: Hwnd) -> Vec<(String, bool)> {
    let Ok(nodes) = snapshot_menubar(top) else { return vec![] };
    nodes
}

fn snapshot_menubar(top: Hwnd) -> Result<Vec<(String, bool)>> {
    let a = automation()?;
    unsafe {
        let cache = cache_request(&a)?;
        let walker = a.ControlViewWalker()?;
        let root = a.ElementFromHandleBuildCache(HWND(top as *mut _), &cache)?;
        let mut out = Vec::new();
        let mut c = walker.GetFirstChildElementBuildCache(&root, &cache);
        while let Ok(ch) = c {
            if ch.CachedControlType().map(|c| c.0).unwrap_or(0) == CT_MENUBAR {
                let mut m = walker.GetFirstChildElementBuildCache(&ch, &cache);
                while let Ok(item) = m {
                    let n = read_node(&item);
                    out.push((n.name.clone(), n.enabled));
                    m = walker.GetNextSiblingElementBuildCache(&item, &cache);
                }
            }
            c = walker.GetNextSiblingElementBuildCache(&ch, &cache);
        }
        Ok(out)
    }
}

impl UiEl {
    fn pattern<T: Interface>(&self, id: UIA_PATTERN_ID) -> Option<T> {
        unsafe { self.0.GetCurrentPatternAs::<T>(id).ok() }
    }

    pub fn name(&self) -> String {
        unsafe { self.0.CurrentName().map(|b| b.to_string()).unwrap_or_default() }
    }

    pub fn invoke(&self) -> Result<()> {
        let p: IUIAutomationInvokePattern = self.pattern(UIA_InvokePatternId).ok_or_else(|| anyhow!("Invoke 를 지원하지 않습니다"))?;
        unsafe { p.Invoke()? };
        Ok(())
    }

    pub fn toggle(&self) -> Result<()> {
        let p: IUIAutomationTogglePattern = self.pattern(UIA_TogglePatternId).ok_or_else(|| anyhow!("Toggle 을 지원하지 않습니다"))?;
        unsafe { p.Toggle()? };
        Ok(())
    }

    pub fn toggle_state(&self) -> Option<i32> {
        let p: IUIAutomationTogglePattern = self.pattern(UIA_TogglePatternId)?;
        unsafe { p.CurrentToggleState().ok().map(|s| s.0) }
    }

    pub fn select(&self) -> Result<()> {
        let p: IUIAutomationSelectionItemPattern =
            self.pattern(UIA_SelectionItemPatternId).ok_or_else(|| anyhow!("선택(SelectionItem)을 지원하지 않습니다"))?;
        unsafe { p.Select()? };
        Ok(())
    }

    pub fn is_selected(&self) -> Option<bool> {
        let p: IUIAutomationSelectionItemPattern = self.pattern(UIA_SelectionItemPatternId)?;
        unsafe { p.CurrentIsSelected().ok().map(|b| b.as_bool()) }
    }

    /// 펼침/접힘 전환 (트리 항목 등). 콤보 상자에는 쓰지 않는다 (목록이 사용자 화면에 그려진다).
    pub fn expand_toggle(&self) -> Result<String> {
        let p: IUIAutomationExpandCollapsePattern =
            self.pattern(UIA_ExpandCollapsePatternId).ok_or_else(|| anyhow!("펼치기를 지원하지 않습니다"))?;
        unsafe {
            let st = p.CurrentExpandCollapseState()?;
            if st == ExpandCollapseState_Expanded || st == ExpandCollapseState_PartiallyExpanded {
                p.Collapse()?;
                Ok("접음".into())
            } else {
                p.Expand()?;
                Ok("펼침".into())
            }
        }
    }

    pub fn value(&self) -> Option<String> {
        let p: IUIAutomationValuePattern = self.pattern(UIA_ValuePatternId)?;
        unsafe { p.CurrentValue().ok().map(|b| b.to_string()) }
    }

    pub fn set_value(&self, v: &str) -> Result<()> {
        let p: IUIAutomationValuePattern = self.pattern(UIA_ValuePatternId).ok_or_else(|| anyhow!("값(Value) 설정을 지원하지 않습니다"))?;
        unsafe {
            if p.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(false) {
                bail!("읽기 전용입니다");
            }
            p.SetValue(&BSTR::from(v))?;
        }
        Ok(())
    }

    /// 문서 전체 글자 (TextPattern)
    pub fn document_text(&self) -> Option<String> {
        let p: IUIAutomationTextPattern = self.pattern(UIA_TextPatternId)?;
        unsafe { p.DocumentRange().ok()?.GetText(-1).ok().map(|b| b.to_string()) }
    }

    /// 선택된 글자 (TextPattern)
    pub fn selection_text(&self) -> Option<String> {
        let p: IUIAutomationTextPattern = self.pattern(UIA_TextPatternId)?;
        unsafe {
            let arr = p.GetSelection().ok()?;
            let n = arr.Length().ok()?;
            let mut s = String::new();
            for i in 0..n {
                if let Ok(r) = arr.GetElement(i) {
                    if let Ok(t) = r.GetText(-1) {
                        s.push_str(&t.to_string());
                    }
                }
            }
            Some(s)
        }
    }

    /// 문서 전체 선택 (TextPattern, 포커스는 바꾸지 않음)
    pub fn select_all_text(&self) -> Result<()> {
        let p: IUIAutomationTextPattern = self.pattern(UIA_TextPatternId).ok_or_else(|| anyhow!("TextPattern 없음"))?;
        unsafe { p.DocumentRange()?.Select()? };
        Ok(())
    }

    pub fn legacy_description(&self) -> Option<String> {
        let p: IUIAutomationLegacyIAccessiblePattern = self.pattern(UIA_LegacyIAccessiblePatternId)?;
        unsafe { p.CurrentDescription().ok().map(|b| b.to_string()).filter(|s| !s.is_empty()) }
    }

    pub fn current_rect(&self) -> Option<(i32, i32, i32, i32)> {
        unsafe { self.0.CurrentBoundingRectangle().ok().map(|r| (r.left, r.top, r.right, r.bottom)) }
    }

    pub fn current_enabled(&self) -> bool {
        unsafe { self.0.CurrentIsEnabled().map(|b| b.as_bool()).unwrap_or(true) }
    }

    /// 자식 요소들 중 이름이 맞는 것 (콤보 상자 목록 항목 등) — 펼치지 않고 찾는다
    pub fn find_descendant_named(&self, want: &str) -> Option<UiEl> {
        let a = automation().ok()?;
        unsafe {
            let cond = a.CreateTrueCondition().ok()?;
            let arr = self.0.FindAll(TreeScope_Descendants, &cond).ok()?;
            let n = arr.Length().ok()?;
            let want = want.trim().to_lowercase();
            let mut partial = None;
            for i in 0..n.min(2000) {
                let Ok(e) = arr.GetElement(i) else { continue };
                let nm = e.CurrentName().map(|b| b.to_string()).unwrap_or_default().trim().to_lowercase();
                if nm == want {
                    return Some(UiEl(e));
                }
                if partial.is_none() && !want.is_empty() && nm.contains(&want) {
                    partial = Some(UiEl(e));
                }
            }
            partial
        }
    }
}
