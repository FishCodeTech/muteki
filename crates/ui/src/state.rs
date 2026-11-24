use crate::{
    api,
    composer::{self, Draft},
    theme::Palette,
};
use nsh_core::prelude::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Chat,
    Settings,
    Usage,
    Archives,
}
#[derive(Clone, Copy)]
pub struct AppState {
    owner: Owner,
    pub page: Signal<Page>,
    pub threads: Signal<Vec<Value>>,
    pub selected: Signal<String>,
    pub snapshot: Signal<Value>,
    pub draft: Signal<String>,
    pub search: Signal<String>,
    pub error: Signal<String>,
    pub loading: Signal<bool>,
    pub sending: Signal<bool>,
    pub prefs: Signal<Value>,
    pub usage: Signal<Value>,
    pub palette: Palette,
    pub sidebar: Signal<bool>,
    pub rename: Signal<bool>,
    pub title_draft: Signal<String>,
    pub catalog: Signal<Vec<Value>>,
    pub credential: Signal<String>,
    pub model: Signal<String>,
    pub drafts: Signal<BTreeMap<String, Draft>>,
    pub options_loaded: Signal<bool>,
    pub usage_offset: Signal<usize>,
    pub pending: Signal<BTreeMap<String, String>>,
}
impl AppState {
    pub fn new() -> Self {
        Self {
            owner: Owner::current(),
            page: signal(Page::Chat),
            threads: signal(vec![]),
            selected: signal(String::new()),
            snapshot: signal(Value::Null),
            draft: signal(String::new()),
            search: signal(String::new()),
            error: signal(String::new()),
            loading: signal(false),
            sending: signal(false),
            prefs: signal(json!({"version":0,"values":{}})),
            usage: signal(Value::Null),
            palette: Palette {
                dark: signal(false),
                system: signal(true),
            },
            sidebar: signal(true),
            rename: signal(false),
            title_draft: signal(String::new()),
            catalog: signal(vec![]),
            credential: signal(String::new()),
            model: signal(String::new()),
            drafts: signal(BTreeMap::new()),
            options_loaded: signal(false),
            usage_offset: signal(0),
            pending: signal(BTreeMap::new()),
        }
    }
    fn spawn(self, future: impl std::future::Future<Output = ()> + 'static) {
        self.owner.run(|| { nsh_core::element::spawn(future); });
    }
    pub fn refresh_threads(self) {
        self.loading.set(true);
        self.spawn(async move {
            match api::request("GET", "/api/threads", None).await {
                Ok(v) => {
                    self.threads
                        .set(v["threads"].as_array().cloned().unwrap_or_default());
                }
                Err(e) => self.error.set(e),
            }
            self.loading.set(false);
        });
    }
    pub fn open(self, id: String) {
        self.stash_draft();
        self.selected.set(id.clone());
        self.restore_draft(&id);
        self.options_loaded.set(false);
        self.error.set(String::new());
        self.snapshot.set(Value::Null);
        if self.page.get_untracked() != Page::Archives {
            self.page.set(Page::Chat);
        }
        self.rename.set(false);
        self.refresh_thread();
    }
    pub fn refresh_thread(self) {
        let id = self.selected.get_untracked();
        if id.is_empty() {
            return;
        }
        self.spawn(async move {
            match api::request("GET", &format!("/api/threads/{id}"), None).await {
                Ok(v) if self.selected.get_untracked() == id => {
                    if let Some(e) = v["state"]["last_error"]["message"].as_str() {
                        self.error.set(e.to_owned());
                    }
                    if !self.options_loaded.get_untracked() {
                        if !self.drafts.with_untracked(|d| d.contains_key(&id)) {
                            self.credential.set(
                                v["runtime"]["credential_id"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_owned(),
                            );
                            self.model
                                .set(v["runtime"]["model"].as_str().unwrap_or("").to_owned());
                        }
                        self.options_loaded.set(true);
                    }
                    let status = v["turns"]
                        .as_array()
                        .and_then(|t| t.last())
                        .and_then(|t| t["status"].as_str())
                        .unwrap_or("");
                    if status == "failed" {
                        if let Some(text) = self.pending.with_untracked(|p| p.get(&id).cloned()) {
                            if self.draft.get_untracked().is_empty() {
                                self.draft.set(text);
                                self.stash_draft();
                            }
                            self.pending.update(|p| {
                                p.remove(&id);
                            });
                        }
                    } else if status == "completed" || status == "cancelled" {
                        self.pending.update(|p| {
                            p.remove(&id);
                        });
                    }
                    self.snapshot.set(v);
                }
                Err(e) if self.selected.get_untracked() == id => self.error.set(e),
                _ => {}
            }
        });
    }
    pub fn running(self) -> bool {
        self.snapshot.with(|v| v["state"]["status"] == "running")
    }
    pub fn new_chat(self) {
        self.stash_draft();
        self.selected.set(String::new());
        self.snapshot.set(Value::Null);
        self.restore_draft("");
        self.options_loaded.set(true);
        self.apply_default();
        self.page.set(Page::Chat);
        self.error.set(String::new());
        self.rename.set(false);
    }
    pub fn send(self) {
        let message = self.draft.get_untracked().trim().to_owned();
        if message.is_empty() || self.sending.get_untracked() || self.running() {
            return;
        }
        if self
            .snapshot
            .with_untracked(|v| v["thread"]["archived"] == true)
        {
            self.error.set("请先恢复此归档对话".into());
            return;
        }
        let credential = self.credential.get_untracked();
        let model = self.model.get_untracked();
        if !composer::supported(&self.catalog.get_untracked(), &credential, &model) {
            self.error.set("请选择可用于文本对话的凭据和模型".into());
            return;
        }
        self.stash_draft();
        let origin = self.selected.get_untracked();
        let original_draft = self.draft.get_untracked();
        let runtime = composer::runtime(&credential, &model);
        self.sending.set(true);
        self.error.set(String::new());
        self.spawn(async move {
            let mut id = origin.clone();
            let result: Result<(), String> = async {
                if id.is_empty() {
                    let title: String = message.chars().take(48).collect();
                    let v = api::request(
                        "POST",
                        "/api/threads",
                        Some(json!({"command_id":crate::api::command_id(),"title":title,"mode":"conversation","runtime":runtime})),
                    )
                    .await?;
                    id = v["receipt"]["aggregate"]["id"]
                        .as_str()
                        .ok_or("服务器未返回对话 ID")?
                        .to_owned();
                    let saved = self
                        .drafts
                        .with_untracked(|d| d.get(&origin).cloned().unwrap_or_default());
                    self.drafts.update(|d| {
                        d.insert(id.clone(), saved);
                        d.remove(&origin);
                    });
                    if self.selected.get_untracked() == origin {
                        self.selected.set(id.clone());
                    }
                }
                api::request(
                    "POST",
                    &format!("/api/threads/{id}/commands"),
                    Some(api::command(
                        "conversation.turn.send",
                        json!({"text":message,"runtime":runtime}),
                    )),
                )
                .await?;
                self.pending.update(|p| {p.insert(id.clone(),original_draft.clone());});
                self.drafts.update(|d| {
                    if let Some(saved) = d.get_mut(&id) {
                        if saved.text == original_draft {
                            saved.text.clear();
                        }
                    }
                });
                if self.selected.get_untracked() == id {
                    if self.draft.get_untracked() == original_draft {
                        self.draft.set(String::new());
                    }
                    self.refresh_thread();
                }
                self.refresh_threads();
                Ok(())
            }
            .await;
            if let Err(e) = result {
                self.error.set(e);
            }
            self.sending.set(false);
        });
    }
    fn stash_draft(self) {
        let key = self.selected.get_untracked();
        let draft = Draft {
            text: self.draft.get_untracked(),
            credential: self.credential.get_untracked(),
            model: self.model.get_untracked(),
        };
        self.drafts.update(|d| {
            d.insert(key, draft);
        });
    }
    fn restore_draft(self, key: &str) {
        let draft = self
            .drafts
            .with_untracked(|d| d.get(key).cloned().unwrap_or_default());
        self.draft.set(draft.text);
        self.credential.set(draft.credential);
        self.model.set(draft.model);
    }
    fn apply_default(self) {
        if self.selected.get_untracked().is_empty() && self.credential.get_untracked().is_empty() {
            let (credential, model) = composer::default_choice(
                &self.prefs.get_untracked(),
                &self.catalog.get_untracked(),
            );
            self.credential.set(credential);
            self.model.set(model);
        }
    }
    pub fn choose_credential(self, credential: String) {
        if self.running() || self.sending.get_untracked() {
            return;
        }
        let model = composer::credential_model(&self.catalog.get_untracked(), &credential);
        self.credential.set(credential);
        self.model.set(model);
        self.stash_draft();
    }
    pub fn choose_model(self, model: String) {
        if self.running() || self.sending.get_untracked() {
            return;
        }
        self.model.set(model);
        self.stash_draft();
    }
    pub fn load_catalog(self) {
        self.spawn(async move {
            match api::request("GET", "/api/models/catalog", None).await {
                Ok(v) => {
                    self.catalog
                        .set(v["credentials"].as_array().cloned().unwrap_or_default());
                    self.apply_default();
                }
                Err(e) => self.error.set(format!("模型目录加载失败：{e}")),
            }
        });
    }
    pub fn thread_command(self, kind: &'static str, payload: Value) {
        let id = self.selected.get_untracked();
        if id.is_empty() {
            return;
        }
        self.spawn(async move {
            match api::request(
                "POST",
                &format!("/api/threads/{id}/commands"),
                Some(api::command(kind, payload)),
            )
            .await
            {
                Ok(_) => {
                    self.rename.set(false);
                    self.refresh_thread();
                    self.refresh_threads();
                }
                Err(e) => self.error.set(e),
            }
        });
    }
    pub fn load_preferences(self) {
        self.spawn(async move {
            match api::request("GET", "/api/settings/ui", None).await {
                Ok(v) => {
                    self.palette.dark.set(v["values"]["theme"] == "dark");
                    self.palette
                        .system
                        .set(v["values"]["theme"].as_str().unwrap_or("system") == "system");
                    self.prefs.set(v);
                    self.apply_default();
                }
                Err(e) => self.error.set(e),
            }
        });
    }
    pub fn save_preference(self, key: &'static str, value: &'static str) {
        let mut body = self.prefs.get_untracked();
        body["values"][key] = json!(value);
        self.spawn(async move {
            match api::request("PUT", "/api/settings/ui", Some(body)).await {
                Ok(v) => {
                    self.palette.dark.set(v["values"]["theme"] == "dark");
                    self.palette
                        .system
                        .set(v["values"]["theme"].as_str().unwrap_or("system") == "system");
                    self.prefs.set(v);
                    self.apply_default();
                    self.error.set(String::new());
                }
                Err(e) => {
                    self.error.set(format!("设置未保存：{e}"));
                    self.load_preferences();
                }
            }
        });
    }
    pub fn load_usage(self) {
        self.load_usage_page(0);
    }
    pub fn load_usage_page(self, offset: usize) {
        self.page.set(Page::Usage);
        self.loading.set(true);
        self.spawn(async move {
            match api::request("GET", &format!("/api/usage?limit=50&offset={offset}"), None).await {
                Ok(v) => {
                    self.usage_offset.set(offset);
                    self.usage.set(v);
                    self.error.set(String::new());
                }
                Err(e) => self.error.set(e),
            }
            self.loading.set(false);
        });
    }
}
