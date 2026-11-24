//! Native SettingsHost and settings controls backed by the Rust service.
use crate::{
    api,
    icons::*,
    state::{AppState, Page},
};
use nsh_base::{button, input};
use nsh_core::prelude::*;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
struct Settings {
    app: AppState,
    page: Signal<&'static str>,
    query: Signal<String>,
    status: Signal<String>,
    failed: Signal<bool>,
    busy: Signal<bool>,
    accounts: Signal<Vec<Value>>,
    loaded: Signal<bool>,
    editor: Signal<bool>,
    editing: Signal<bool>,
    account_id: Signal<String>,
    engine: Signal<String>,
    connection: Signal<String>,
    provider: Signal<String>,
    endpoint: Signal<String>,
    secret: Signal<String>,
    auth_json: Signal<String>,
    model: Signal<String>,
    models: Signal<String>,
    revision: Signal<String>,
    deleting: Signal<String>,
}
const PAGES: &[(&str, &str, &str, &str, &str)] = &[
    (
        "appearance",
        "general",
        "外观",
        "Appearance",
        "语言、主题、主色与对话阅读偏好。改动立即生效。",
    ),
    (
        "chat",
        "general",
        "对话",
        "Chat",
        "发送方式、新对话默认权限、编辑器与 Diff 偏好。",
    ),
    (
        "notifications",
        "general",
        "通知",
        "Notifications",
        "后台会话完成、需要审批或等待输入时如何提醒你。",
    ),
    (
        "shortcuts",
        "general",
        "键盘快捷键",
        "Keyboard shortcuts",
        "查看常用组合键。",
    ),
    (
        "agents",
        "agent",
        "Agents 和接入点",
        "Agents and providers",
        "选择引擎，查看它的接入方式、模型和价格。配置保存在当前连接的服务中。",
    ),
    (
        "agent-extensions",
        "agent",
        "Agent 扩展",
        "Agent extensions",
        "管理 Agent 扩展。",
    ),
    (
        "capabilities",
        "agent",
        "能力",
        "Capabilities",
        "查看当前服务提供的能力。",
    ),
    (
        "archives",
        "data",
        "归档",
        "Archives",
        "查看和恢复已归档的对话。",
    ),
    (
        "import",
        "data",
        "导入历史会话",
        "Import history",
        "导入其他客户端的历史会话。",
    ),
    (
        "access",
        "system",
        "访问与登录",
        "Access and sign-in",
        "管理服务访问与登录。",
    ),
    (
        "extensions",
        "system",
        "平台扩展",
        "Platform extensions",
        "管理平台扩展。",
    ),
    (
        "update",
        "system",
        "版本更新",
        "Updates",
        "查看当前版本和更新。",
    ),
    (
        "operations",
        "system",
        "运维",
        "Operations",
        "服务状态与运行环境。",
    ),
    (
        "desktop",
        "system",
        "桌面与连接",
        "Desktop and connection",
        "桌面客户端与服务连接。",
    ),
];
fn tr(app: AppState, zh: &'static str, en: &'static str) -> &'static str {
    if app.prefs.with(|v| v["values"]["language"] == "en") {
        en
    } else {
        zh
    }
}
impl Settings {
    fn message(self, failed: bool, message: String) {
        self.failed.set(failed);
        self.status.set(message);
    }
    fn save(self, key: &'static str, value: Value) {
        if self.busy.get_untracked() {
            return;
        }
        self.busy.set(true);
        self.status.set(String::new());
        let mut body = self.app.prefs.get_untracked();
        body["values"][key] = value;
        spawn(async move {
            match api::request("PUT", "/api/settings/ui", Some(body)).await {
                Ok(v) => {
                    self.app.palette.dark.set(v["values"]["theme"] == "dark");
                    self.app
                        .palette
                        .system
                        .set(v["values"]["theme"].as_str().unwrap_or("system") == "system");
                    self.app.prefs.set(v);
                    self.message(false, tr(self.app, "已保存", "Saved").into());
                }
                Err(e) => {
                    self.message(
                        true,
                        format!(
                            "{}: {e}",
                            tr(
                                self.app,
                                "设置未保存，请重试",
                                "Setting was not saved. Try again"
                            )
                        ),
                    );
                    self.app.load_preferences();
                }
            }
            self.busy.set(false);
        });
    }
    fn load_accounts(self) {
        self.loaded.set(false);
        spawn(async move {
            match api::request("GET", "/api/settings/credential-accounts", None).await {
                Ok(v) => {
                    self.accounts
                        .set(v["accounts"].as_array().cloned().unwrap_or_default());
                    self.loaded.set(true);
                }
                Err(e) => {
                    self.loaded.set(true);
                    self.message(
                        true,
                        format!(
                            "{}: {e}",
                            tr(self.app, "无法加载接入点", "Could not load providers")
                        ),
                    );
                }
            }
        });
    }
    fn edit(self, account: Option<Value>) {
        self.editing.set(account.is_some());
        let a = account.unwrap_or(Value::Null);
        self.account_id
            .set(a["account_id"].as_str().unwrap_or("").into());
        self.engine.set(
            a["worker_engine"]
                .as_str()
                .or(a["engine"].as_str())
                .unwrap_or("openai")
                .into(),
        );
        self.connection
            .set(a["connection"].as_str().unwrap_or("custom_endpoint").into());
        self.provider
            .set(a["provider"].as_str().unwrap_or("").into());
        self.endpoint
            .set(a["base_url"].as_str().unwrap_or("").into());
        self.model
            .set(a["default_model"].as_str().unwrap_or("").into());
        self.models.set(
            a["models"]
                .as_array()
                .map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
        );
        self.revision
            .set(a["revision"].as_str().unwrap_or("").into());
        self.secret.set(String::new());
        self.auth_json.set(String::new());
        self.deleting.set(String::new());
        self.status.set(String::new());
        self.editor.set(true);
    }
    fn save_account(self) {
        if self.busy.get_untracked() {
            return;
        }
        let id = self.account_id.get_untracked().trim().to_owned();
        let engine = self.engine.get_untracked().trim().to_owned();
        if id.is_empty()
            || id.len() > 128
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_- .".contains(c))
        {
            self.message(true, tr(self.app, "请填写凭据 ID，仅使用字母、数字、空格、点、下划线或连字符。", "Enter a credential ID using letters, numbers, spaces, dots, underscores or hyphens.").into());
            return;
        }
        if engine.is_empty() {
            self.message(
                true,
                tr(self.app, "请填写引擎。", "Enter an engine.").into(),
            );
            return;
        }
        let endpoint = self.endpoint.get_untracked().trim().to_owned();
        let connection = self.connection.get_untracked();
        if connection == "custom_endpoint"
            && !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
        {
            self.message(
                true,
                tr(
                    self.app,
                    "请填写以 http:// 或 https:// 开头的 Base URL。",
                    "Enter a Base URL starting with http:// or https://.",
                )
                .into(),
            );
            return;
        }
        let secret = self.secret.get_untracked();
        let auth = self.auth_json.get_untracked();
        if !self.editing.get_untracked() && secret.trim().is_empty() && auth.trim().is_empty() {
            self.message(
                true,
                tr(
                    self.app,
                    "请填写 Secret / Token / API Key 或 Auth JSON。",
                    "Enter a Secret / Token / API Key or Auth JSON.",
                )
                .into(),
            );
            return;
        }
        if !auth.trim().is_empty()
            && serde_json::from_str::<Value>(&auth)
                .ok()
                .is_none_or(|v| !v.is_object())
        {
            self.message(
                true,
                tr(
                    self.app,
                    "Auth JSON 必须为有效的 JSON 对象。",
                    "Auth JSON must be a valid JSON object.",
                )
                .into(),
            );
            return;
        }
        let models = split_models(&self.models.get_untracked());
        let model = self.model.get_untracked().trim().to_owned();
        let mut body = json!({"worker_engine":engine,"connection":connection,"provider":self.provider.get_untracked().trim(),"base_url":endpoint,"target_model":model,"models":models,"create_only":!self.editing.get_untracked()});
        if self.editing.get_untracked() {
            body["expected_revision"] = json!(self.revision.get_untracked());
        }
        if !secret.trim().is_empty() {
            body["secret"] = json!(secret);
        }
        if !auth.trim().is_empty() {
            body["codex_auth_json"] = json!(auth);
        }
        self.busy.set(true);
        spawn(async move {
            match api::request(
                "PUT",
                &format!("/api/settings/credential-accounts/{}", path_segment(&id)),
                Some(body),
            )
            .await
            {
                Ok(_) => {
                    self.editor.set(false);
                    self.secret.set(String::new());
                    self.auth_json.set(String::new());
                    self.message(
                        false,
                        tr(self.app, "接入点已保存。", "Provider saved.").into(),
                    );
                    self.load_accounts();
                    self.app.load_catalog();
                }
                Err(e) => self.message(
                    true,
                    format!(
                        "{}: {e}",
                        tr(
                            self.app,
                            "无法保存接入点，请检查配置并重试",
                            "Could not save provider. Check the configuration and try again"
                        )
                    ),
                ),
            }
            self.busy.set(false);
        });
    }
    fn delete_account(self, id: String) {
        if self.busy.get_untracked() {
            return;
        }
        self.busy.set(true);
        spawn(async move {
            match api::request(
                "DELETE",
                &format!("/api/settings/credential-accounts/{}", path_segment(&id)),
                None,
            )
            .await
            {
                Ok(_) => {
                    self.deleting.set(String::new());
                    self.message(
                        false,
                        tr(self.app, "接入点已删除。", "Provider deleted.").into(),
                    );
                    self.load_accounts();
                    self.app.load_catalog();
                }
                Err(e) => self.message(
                    true,
                    format!(
                        "{}: {e}",
                        tr(self.app, "删除失败，请重试", "Deletion failed. Try again")
                    ),
                ),
            }
            self.busy.set(false);
        });
    }
}
fn path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn split_models(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for model in s
        .split(|c: char| c == ',' || c == '\n')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !out.iter().any(|m| m == model) {
            out.push(model.to_owned());
        }
    }
    out
}
fn action(
    s: Settings,
    zh: &'static str,
    en: &'static str,
    f: impl Fn() + 'static,
) -> impl IntoElement {
    let p = s.app.palette;
    button()
        .h(px(32.0))
        .px(px(12.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(move || p.line())
        .bg(move || p.panel())
        .hover(move |v| v.bg(move || p.hover()))
        .items_center()
        .justify_center()
        .disabled(move || s.busy.get())
        .on_press(f)
        .child(
            text(move || tr(s.app, zh, en))
                .text_size(px(13.0))
                .text_color(move || p.ink()),
        )
}
fn row(
    s: Settings,
    zh: &'static str,
    en: &'static str,
    note_zh: &'static str,
    note_en: &'static str,
    control: impl IntoElement,
) -> impl IntoElement {
    let p = s.app.palette;
    view()
        .px(px(20.0))
        .py(px(16.0))
        .flex_row()
        .items_center()
        .gap(px(16.0))
        .border_b_1()
        .border_b_color(move || p.line())
        .child(
            view()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(4.0))
                .child(
                    text(move || tr(s.app, zh, en))
                        .text_size(px(14.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(move || p.ink()),
                )
                .child(when(
                    move || !note_zh.is_empty(),
                    move || {
                        text(move || tr(s.app, note_zh, note_en))
                            .text_size(px(13.0))
                            .line_height(px(20.0))
                            .text_color(move || p.muted())
                    },
                )),
        )
        .child(control)
}
fn section(
    s: Settings,
    zh: &'static str,
    en: &'static str,
    content: impl IntoElement,
) -> impl IntoElement {
    let p = s.app.palette;
    view()
        .gap(px(8.0))
        .child(
            text(move || tr(s.app, zh, en))
                .px(px(4.0))
                .text_size(px(14.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || p.ink()),
        )
        .child(
            view()
                .rounded(px(16.0))
                .border_1()
                .border_color(move || p.line())
                .bg(move || p.panel())
                .overflow_hidden()
                .child(content),
        )
}
fn choices(
    s: Settings,
    key: &'static str,
    fallback: &'static str,
    options: &'static [(&'static str, &'static str, &'static str)],
) -> impl IntoElement {
    let p = s.app.palette;
    view()
        .flex_row()
        .gap(px(2.0))
        .p(px(3.0))
        .rounded(px(9.0))
        .bg(move || p.sidebar())
        .child(options.iter().map(move |&(value, zh, en)| {
            button()
                .accessibility_label(zh)
                .test_id(format!("setting-{key}-{value}"))
                .h(px(30.0))
                .px(px(10.0))
                .rounded(px(7.0))
                .items_center()
                .justify_center()
                .disabled(move || s.busy.get())
                .bg(move || {
                    if s.app
                        .prefs
                        .with(|v| v["values"][key].as_str().unwrap_or(fallback) == value)
                    {
                        p.panel()
                    } else {
                        p.sidebar()
                    }
                })
                .hover(move |v| v.bg(move || p.hover()))
                .on_press(move || s.save(key, json!(value)))
                .child(
                    text(move || tr(s.app, zh, en))
                        .text_size(px(13.0))
                        .text_color(move || p.ink()),
                )
        }))
}
fn appearance(s: Settings) -> impl IntoElement {
    view().gap(px(32.0))
        .child(section(s, "界面", "Interface", view()
            .child(row(s, "语言", "Language", "选择界面使用的语言。", "Language used by the interface.", choices(s,"language","zh",&[("zh","简体中文","简体中文"),("en","English","English")])))
            .child(row(s, "主题", "Theme", "跟随系统或手动选择明暗。", "Follow the system or choose light or dark.", choices(s,"theme","system",&[("system","跟随系统","System"),("light","浅色","Light"),("dark","深色","Dark")])))))
        .child(section(s, "对话阅读", "Reading", view()
            .child(row(s, "正文字号", "Text size", "只影响聊天正文。", "Applies to chat messages only.", choices(s,"readingFontScale","md",&[("sm","小","Small"),("md","标准","Default"),("lg","大","Large")])))
            .child(row(s, "内容宽度", "Content width", "", "", choices(s,"readingContentWidth","default",&[("narrow","窄","Narrow"),("default","标准","Default"),("wide","宽","Wide")])))
            .child(row(s, "紧凑排版", "Compact layout", "缩小消息之间的间距。", "Tightens spacing between messages.", choices(s,"readingDensity","comfortable",&[("comfortable","关闭","Off"),("compact","开启","On")])))))
        .child(note(s, "主色、界面字体和动态效果由当前原生客户端固定提供；此版本暂不支持修改。", "Accent, interface fonts and motion use this native client's defaults. They cannot be changed in this version."))
}
fn chat(s: Settings) -> impl IntoElement {
    view().gap(px(32.0))
        .child(section(s, "输入与发送", "Compose and send", view()
            .child(row(s,"发送键","Send key","Enter 发送，Shift+Enter 换行；也可选择 ⌘+Enter 发送。","Enter sends and Shift+Enter inserts a new line. You can also send with ⌘+Enter.",choices(s,"sendKey","enter",&[("enter","Enter 发送","Enter"),("mod-enter","⌘+Enter 发送","⌘+Enter")])))))
        .child(section(s, "运行中的对话", "Running chats", row(s,"发送消息","Sending messages","当前 HTTP 对话提供方在回答期间禁用发送。引导、队列和额度重置后继续需要 Agent 运行时支持。","This HTTP chat provider disables sending while answering. Steering, queues and continuing after quota reset require an Agent runtime.",text(""))))
        .child(section(s,"工作区与变更","Workspace and changes",row(s,"编辑器与 Diff","Editor and Diff","当前原生客户端尚未提供编辑器集成和 Diff 视图。","Editor integration and Diff views are unavailable in this native client.",text(""))))
}
fn note(s: Settings, zh: &'static str, en: &'static str) -> impl IntoElement {
    let p = s.app.palette;
    text(move || tr(s.app, zh, en))
        .text_size(px(13.0))
        .line_height(px(20.0))
        .text_color(move || p.muted())
}
fn field(
    s: Settings,
    zh: &'static str,
    en: &'static str,
    id: &'static str,
    value: Signal<String>,
    secret: bool,
    multiline: bool,
    locked: bool,
) -> impl IntoElement {
    let p = s.app.palette;
    view()
        .gap(px(6.0))
        .child(
            text(move || tr(s.app, zh, en))
                .text_size(px(13.0))
                .text_color(move || p.ink()),
        )
        .child(
            input()
                .test_id(id)
                .accessibility_label(zh)
                .value(move || value.get())
                .on_change(move |v: &str| value.set(v.to_owned()))
                .masked(secret)
                .multiline(multiline)
                .disabled(move || {
                    (if id == "account-id" {
                        s.editing.get()
                    } else {
                        locked
                    }) || s.busy.get()
                })
                .w_full()
                .min_h(px(if multiline { 72.0 } else { 34.0 }))
                .px(px(10.0))
                .py(px(7.0))
                .rounded(px(8.0))
                .border_1()
                .border_color(move || p.line())
                .bg(move || p.bg())
                .text_size(px(13.0))
                .text_color(move || p.ink()),
        )
}
fn account_editor(s: Settings) -> impl IntoElement {
    let p = s.app.palette;
    section(s,"接入方式","Access method",view().p(px(20.0)).gap(px(16.0))
        .child(field(s,"凭据 ID","Credential ID","account-id",s.account_id,false,false,s.editing.get_untracked()))
        .child(field(s,"引擎","Engine","account-engine",s.engine,false,false,false))
        .child(view().flex_row().gap(px(8.0)).child([("official","官方账号","Official account"),("custom_endpoint","自定义端点","Custom endpoint")].into_iter().map(move |(v,zh,en)| {
            button().h(px(34.0)).px(px(12.0)).rounded(px(8.0)).border_1().border_color(move || p.line())
                .bg(move || if s.connection.get()==v {p.hover()}else{p.panel()}).disabled(move || s.busy.get()).on_press(move || s.connection.set(v.into()))
                .child(text(move || tr(s.app,zh,en)).text_size(px(13.0)).text_color(move || p.ink()))
        })))
        .child(when(move || s.connection.get()=="custom_endpoint",move || field(s,"Base URL","Base URL","account-base-url",s.endpoint,false,false,false)))
        .child(field(s,"提供方名称","Provider name","account-provider",s.provider,false,false,false))
        .child(field(s,"Secret / Token / API Key","Secret / Token / API Key","account-secret",s.secret,true,false,false))
        .child(when(move || s.connection.get()=="official",move || field(s,"Auth JSON（可选）","Auth JSON (optional)","account-auth-json",s.auth_json,true,false,false)))
        .child(note(s,"修改时留空会保留已保存的密钥。密钥不会从服务读取。","Leave blank when editing to keep the saved secret. Secrets are never read back from the service."))
        .child(field(s,"默认模型","Default model","account-default-model",s.model,false,false,false))
        .child(field(s,"模型列表（每行一个，或用逗号分隔）","Models (one per line or comma separated)","account-models",s.models,false,true,false))
        .child(note(s,"这里只保存接入配置。CLI 登录、导入、模型探测和连通测试需要尚未提供的 Agent 运行时。","This saves access configuration. CLI sign-in, imports, model discovery and connectivity tests require the unavailable Agent runtime."))
        .child(view().flex_row().gap(px(8.0)).child(action(s,"保存","Save",move || s.save_account())).child(action(s,"取消","Cancel",move || {s.editor.set(false);s.secret.set(String::new());s.auth_json.set(String::new());}))))
}
fn agents(s: Settings) -> impl IntoElement {
    let p = s.app.palette;
    view().gap(px(24.0))
        .child(view().flex_row().gap(px(8.0)).child(action(s,"添加接入点","Add provider",move || s.edit(None)))
            .child(action(s,"刷新","Refresh",move || {s.load_accounts();s.app.load_catalog();})))
        .child(when(move || s.editor.get(),move || account_editor(s)))
        .child(section(s,"已保存的接入点","Saved providers",view()
            .child(when(move || !s.loaded.get(),move || view().p(px(20.0)).child(note(s,"正在加载…","Loading…"))))
            .child(when(move || s.loaded.get() && s.accounts.with(|a| a.is_empty()),move || view().p(px(20.0)).child(note(s,"还没有保存接入点。添加接入点以配置引擎、凭据和模型。","No saved providers. Add one to configure an engine, credential and models."))))
            .child(each(move || s.accounts.get(),|a|a["account_id"].as_str().unwrap_or("").to_owned(),move |a,_|{
                let data=a.get_untracked();let edit_data=data.clone();let id=data["account_id"].as_str().unwrap_or("").to_owned();let delete_id=id.clone();let confirm_id=id.clone();let yes_id=id.clone();
                view().px(px(20.0)).py(px(16.0)).gap(px(10.0)).border_b_1().border_b_color(move || p.line())
                    .child(view().flex_row().items_center().gap(px(12.0))
                        .child(view().flex_1().gap(px(4.0))
                            .child(text(id).text_size(px(14.0)).font_weight(FontWeight::MEDIUM).text_color(move || p.ink()))
                            .child(text(format!("{} · {}",data["worker_engine"].as_str().unwrap_or(""),data["base_url"].as_str().filter(|s| !s.is_empty()).unwrap_or("official"))).text_size(px(12.0)).text_color(move || p.muted())))
                        .child(action(s,"编辑","Edit",move || s.edit(Some(edit_data.clone()))))
                        .child(action(s,"删除","Delete",move || s.deleting.set(delete_id.clone()))))
                    .child(text(format!("{}: {}",tr(s.app,"默认模型","Default model"),data["default_model"].as_str().unwrap_or("—"))).text_size(px(13.0)).text_color(move || p.muted()))
                    .child(when(move || s.deleting.get()==confirm_id,move || view().gap(px(8.0))
                        .child(note(s,"删除此接入点及其已保存的密钥？此操作无法撤销。","Delete this provider and its saved secret? This cannot be undone."))
                        .child(view().flex_row().gap(px(8.0)).child(action(s,"确认删除","Confirm deletion",{let yes_id=yes_id.clone();move || s.delete_account(yes_id.clone())})).child(action(s,"取消","Cancel",move || s.deleting.set(String::new()))))))
            }))))
        .child(model_defaults(s))
}
fn model_defaults(s: Settings) -> impl IntoElement {
    let p = s.app.palette;
    section(s,"模型","Models", view().child(each(move || s.app.catalog.get(),|c|c["credential_id"].as_str().unwrap_or("").to_owned(),move |c,_|{
        let data=c.get_untracked();let credential=data["credential_id"].as_str().unwrap_or("").to_owned();let available=data["http_compatible"]==true;
        view().p(px(20.0)).gap(px(8.0)).border_b_1().border_b_color(move || p.line())
            .child(text(data["label"].as_str().unwrap_or("Provider").to_owned()).text_size(px(14.0)).font_weight(FontWeight::MEDIUM).text_color(move || p.ink()))
            .child(data["models"].as_array().cloned().unwrap_or_default().into_iter().map(move |m|{
                let model=m["id"].as_str().unwrap_or("").to_owned();let cred=credential.clone();let model_set=model.clone();let cred_set=cred.clone();
                view().flex_row().items_center().gap(px(12.0)).child(text(m["label"].as_str().unwrap_or(&model).to_owned()).flex_1().text_size(px(13.0)).text_color(move || p.ink()))
                    .child(button().h(px(30.0)).px(px(10.0)).rounded(px(7.0)).bg(move || p.sidebar()).disabled(move || s.busy.get() || !available)
                        .on_press(move || s.save("defaultModel",json!({"credentialId":cred_set,"modelId":model_set})))
                        .child(text(move || if s.app.prefs.with(|v|v["values"]["defaultModel"]["credentialId"]==cred && v["values"]["defaultModel"]["modelId"]==model) {tr(s.app,"默认","Default")}else{tr(s.app,"设为默认","Set default")}).text_size(px(12.0)).text_color(move || p.ink())))
            }))
            .child(when(move || !available,move || note(s,"此接入点仅保存元数据；当前 HTTP 对话不支持此引擎。","This provider stores metadata only. This engine is unavailable for HTTP conversations.")))
    })).child(when(move || s.app.catalog.with(|c|c.is_empty()),move || view().p(px(20.0)).child(note(s,"当前没有配置可用模型。","No models are configured.")))))
}
fn other(s: Settings) -> impl IntoElement {
    let p = s.app.palette;
    view().gap(px(24.0))
        .child(when(move || s.page.get()=="archives",move || section(s,"已归档对话","Archived chats",view().p(px(20.0)).gap(px(12.0)).child(note(s,"归档对话保留在当前服务中，可查看或恢复。","Archived chats remain on this service and can be viewed or restored."))
            .child(action(s,"打开归档","Open archives",move || {s.app.page.set(Page::Archives);s.app.refresh_threads();})))))
        .child(when(move || s.page.get()=="shortcuts",move || section(s,"对话","Chat",view()
            .child(row(s,"发送消息","Send message","使用当前发送键偏好。","Uses your send key preference.",text(move || if s.app.prefs.with(|v|v["values"]["sendKey"]=="mod-enter") {"⌘ Enter"}else{"Enter"}).text_size(px(13.0)).text_color(move || p.ink())))
            .child(row(s,"换行","New line","", "",text(move || if s.app.prefs.with(|v|v["values"]["sendKey"]=="mod-enter") {"Enter"}else{"Shift Enter"}).text_size(px(13.0)).text_color(move || p.ink())))
            .child(view().p(px(20.0)).child(note(s,"当前原生客户端尚不支持自定义快捷键。","Custom keyboard bindings are unavailable in this native client."))))))
        .child(when(move || s.page.get()=="desktop",move || section(s,"服务连接","Service connection",view().p(px(20.0)).gap(px(8.0))
            .child(text(api::url("")).text_size(px(13.0)).selectable(true).text_color(move || p.ink()))
            .child(note(s,"服务地址在构建时配置。此原生客户端通过 NSH HTTP 服务连接本机 Muteki。","The service address is configured at build time. This native client connects to Muteki through NSH HTTP.")))))
        .child(when(move || s.page.get()=="update",move || section(s,"当前版本","Current version",view().p(px(20.0)).gap(px(8.0))
            .child(text(format!("Muteki {}",env!("CARGO_PKG_VERSION"))).text_size(px(14.0)).text_color(move || p.ink()))
            .child(note(s,"当前原生客户端尚未配置自动更新服务。","An automatic update service is not configured for this native client.")))))
        .child(when(move || !["archives","shortcuts","desktop","update"].contains(&s.page.get()),move || section(s,"服务能力","Service capability",view().p(px(20.0)).gap(px(12.0))
            .child(note(s,"当前服务尚未提供此页面所需的运行时能力。配置没有被更改。","The current service does not provide the runtime capability required by this page. No configuration has been changed."))
            .child(text(move || match s.page.get(){"notifications"=>tr(s.app,"系统通知与声音需要原生通知服务；当前未接入。","System notifications and sound require a native notification service, which is not connected."),"access"=>tr(s.app,"此本机服务没有提供账户登录或访问策略管理 API。","This local service does not expose account sign-in or access policy management APIs."),"operations"=>tr(s.app,"当前服务只提供对话、共享偏好、接入点与用量接口；未提供运维管理接口。","This service provides conversations, shared preferences, providers and usage APIs. Operations management is unavailable."),"import"=>tr(s.app,"导入历史会话需要会话导入适配器；当前未配置。","History import requires a session import adapter, which is not configured."),_=>tr(s.app,"扩展和 Agent 能力管理需要 Agent 运行时；当前版本仅支持 HTTP 对话。","Extension and Agent capability management require an Agent runtime. This version supports HTTP conversations.")}).text_size(px(13.0)).line_height(px(20.0)).text_color(move || p.muted())))))
}
pub fn settings(app: AppState) -> impl IntoElement {
    let p = app.palette;
    let s = Settings {
        app,
        page: signal("appearance"),
        query: signal(String::new()),
        status: signal(String::new()),
        failed: signal(false),
        busy: signal(false),
        accounts: signal(vec![]),
        loaded: signal(false),
        editor: signal(false),
        editing: signal(false),
        account_id: signal(String::new()),
        engine: signal(String::new()),
        connection: signal(String::new()),
        provider: signal(String::new()),
        endpoint: signal(String::new()),
        secret: signal(String::new()),
        auth_json: signal(String::new()),
        model: signal(String::new()),
        models: signal(String::new()),
        revision: signal(String::new()),
        deleting: signal(String::new()),
    };
    view().test_id("settings").flex_1().min_w(px(0.0)).flex_row().bg(move || p.bg())
        .child(view().w(px(272.0)).min_h(px(0.0)).bg(move || p.sidebar()).border_r_1().border_r_color(move || p.line())
            .child(view().p(px(12.0)).gap(px(12.0))
                .child(button().h(px(30.0)).px(px(8.0)).on_press(move || app.page.set(Page::Chat)).child(text(move || tr(app,"返回应用","Back to app")).text_size(px(13.0)).text_color(move || p.muted())))
                .child(text(move || tr(app,"设置","Settings")).px(px(8.0)).text_size(px(20.0)).font_weight(FontWeight::SEMIBOLD).text_color(move || p.ink()))
                .child(input().test_id("settings-search").accessibility_label("搜索设置").placeholder(move || tr(app,"搜索设置","Search settings")).value(move || s.query.get()).on_change(move |v:&str|s.query.set(v.into())).h(px(34.0)).px(px(10.0)).rounded(px(8.0)).border_1().border_color(move || p.line()).bg(move || p.panel()).text_size(px(13.0)).text_color(move || p.ink())))
            .child(scroll().flex_1().child(view().px(px(12.0)).pb(px(20.0)).gap(px(20.0))
                .child([("general","通用","General"),("agent","Agent","Agent"),("data","数据","Data"),("system","系统","System")].into_iter().map(move |(group,zh,en)|{
                    view().gap(px(2.0)).child(text(move || tr(app,zh,en)).px(px(12.0)).pb(px(6.0)).text_size(px(13.0)).text_color(move || p.muted()))
                        .child(PAGES.iter().filter(move |entry|entry.1==group).map(move |&(id,_,zh,en,_)|when(move || {
                            let q=s.query.get().trim().to_lowercase();format!("{zh} {en} {id}").to_lowercase().contains(&q)
                        },move || button().test_id(format!("settings-nav-{id}")).h(px(36.0)).px(px(12.0)).rounded(px(12.0)).flex_row().items_center().gap(px(12.0)).bg(move ||if s.page.get()==id{p.hover()}else{p.sidebar()}).hover(move |v|v.bg(move ||p.hover())).on_press(move || {s.page.set(id);s.status.set(String::new());if id=="agents" && !s.loaded.get_untracked(){s.load_accounts();}})
                            .child(icon(match id{"chat"=>CHAT,"agents"=>SETTINGS,"archives"=>ARCHIVE,"operations"=>USAGE,_=>SETTINGS}))
                            .child(text(move ||tr(app,zh,en)).text_size(px(14.0)).text_color(move ||p.ink())))))
                }))
                .child(when(move || {let q=s.query.get().trim().to_lowercase();!PAGES.iter().any(|(id,_,zh,en,_)|format!("{zh} {en} {id}").to_lowercase().contains(&q))},move || note(s,"没有匹配的设置","No matching settings"))))))
        .child(scroll().flex_1().min_w(px(0.0)).child(view().w_full().max_w(move ||px(if s.page.get()=="agents"{1080.0}else{760.0})).self_center().px(px(32.0)).pt(px(40.0)).pb(px(64.0)).gap(px(32.0))
            .child(view().gap(px(4.0))
                .child(text(move || {let entry=PAGES.iter().find(|e|e.0==s.page.get()).unwrap();tr(app,entry.2,entry.3)}).text_size(px(22.0)).line_height(px(32.0)).font_weight(FontWeight::SEMIBOLD).text_color(move ||p.ink()))
                .child(text(move || {let entry=PAGES.iter().find(|e|e.0==s.page.get()).unwrap();if app.prefs.with(|v|v["values"]["language"]=="en"){match entry.0{"appearance"=>"Language, theme, accent and reading preferences. Changes apply immediately.","chat"=>"Sending, default permissions, editor and diff preferences.","agents"=>"Pick an engine to see its access methods, models and prices. Settings are stored on the connected service.",_=>entry.3}}else{entry.4}}).text_size(px(14.0)).line_height(px(24.0)).text_color(move ||p.muted())))
            .child(when(move || !s.status.get().is_empty() || s.busy.get(),move || text(move || if s.busy.get(){tr(app,"正在保存…","Saving…").into()}else{s.status.get()}).test_id("settings-status").text_size(px(13.0)).line_height(px(20.0)).text_color(move ||if s.failed.get(){rgb(0xa45f65).into()}else{p.muted()})))
            .child(when(move ||s.page.get()=="appearance",move ||appearance(s)))
            .child(when(move ||s.page.get()=="chat",move ||chat(s)))
            .child(when(move ||s.page.get()=="agents",move ||agents(s)))
            .child(when(move ||!["appearance","chat","agents"].contains(&s.page.get()),move ||other(s)))))
}
