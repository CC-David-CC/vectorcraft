//! The WebOS account protocol; standalone VectorCraft uses the separate page API.

use std::{cell::RefCell, collections::VecDeque, rc::Rc};

use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array};
use serde_json::{Value, json};
use vectorcraft_ui_egui::VectorcraftApp;
use vectorcraft_ui_egui::account_host::{AccountDispatch, AccountIntent, AccountProjectState, AccountSaveKey, MAX_ACCOUNT_PROJECT_BYTES};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use web_sys::{MessageEvent, Window};

const PREFIX: &str = "vectorcraft:account-";

struct Connection {
    parent: Window,
    origin: String,
    generation: String,
}

fn set(packet: &Object, key: &str, value: impl Into<JsValue>) -> Result<(), String> {
    if Reflect::set(packet, &JsValue::from_str(key), &value.into()).ok() == Some(true) {
        Ok(())
    } else {
        Err("Could not encode a VectorCraft account message.".into())
    }
}

fn field(packet: &JsValue, key: &str) -> Option<JsValue> {
    Reflect::get(packet, &JsValue::from_str(key)).ok()
}

fn text(packet: &JsValue, key: &str, max: usize) -> Option<String> {
    field(packet, key)?.as_string().filter(|value| value.len() <= max)
}

fn nullable_text(packet: &JsValue, key: &str, max: usize) -> Option<Option<String>> {
    let value = field(packet, key)?;
    if value.is_null() || value.is_undefined() { Some(None) } else { Some(Some(value.as_string().filter(|value| value.len() <= max)?)) }
}

fn integer(packet: &JsValue, key: &str) -> Option<u64> {
    text(packet, key, 20)?.parse().ok()
}

fn token(packet: &JsValue, key: &str) -> Option<String> {
    text(packet, key, 36).filter(|value| value.len() == 36 && value.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-'))
}

fn save_fields(packet: &Object, key: &AccountSaveKey) -> Result<(), String> {
    set(packet, "ticket", key.ticket.to_string())?;
    set(packet, "documentId", key.document_id.to_string())?;
    set(packet, "revision", key.revision.to_string())?;
    set(packet, "path", key.path.as_str())
}

fn state_fields(packet: &Object, state: Option<&AccountProjectState>) -> Result<(), String> {
    set(packet, "available", state.is_some())?;
    if let Some(state) = state {
        set(packet, "documentId", state.document_id.to_string())?;
        set(packet, "revision", state.revision.to_string())?;
        set(packet, "dirty", state.dirty)?;
        set(packet, "unfinishedEdit", state.unfinished_edit)?;
        set(packet, "operationPending", state.operation_pending)?;
        set(packet, "capturable", state.capturable)?;
        set(packet, "autosaveEligible", state.autosave_eligible)?;
    }
    Ok(())
}

fn attach_bytes(packet: &Object, bytes: &[u8]) -> Result<Array, String> {
    let buffer = Uint8Array::from(bytes).buffer();
    set(packet, "bytes", buffer.clone())?;
    Ok(Array::of1(&buffer))
}

impl Connection {
    fn packet(&self, name: &str) -> Result<Object, String> {
        let packet = Object::new();
        set(&packet, "type", format!("{PREFIX}{name}"))?;
        set(&packet, "generation", self.generation.as_str())?;
        Ok(packet)
    }

    fn send(&self, packet: &Object, transfer: &Array) -> Result<(), String> {
        self.parent.post_message_with_transfer(packet, &self.origin, transfer).map_err(|_| "Could not reach the VectorCraft account host.".into())
    }

    fn intent(&self, intent: AccountIntent) -> Result<(), String> {
        if let AccountIntent::Save(key, bytes) = intent {
            let packet = self.packet("save")?;
            save_fields(&packet, &key)?;
            return self.send(&packet, &attach_bytes(&packet, &bytes)?);
        }
        if let AccountIntent::Close = intent {
            return self.send(&self.packet("close-intent")?, &Array::new());
        }
        let packet = self.packet("dialog")?;
        let request_id = match intent {
            AccountIntent::New { request_id, settings } => {
                set(&packet, "operation", "new")?;
                set(&packet, "settings", settings.to_string())?;
                request_id
            }
            AccountIntent::Open { request_id, path } => {
                set(&packet, "operation", "open")?;
                set(&packet, "path", path.map_or(JsValue::NULL, JsValue::from))?;
                request_id
            }
            AccountIntent::ChooseSave { request_id, suggested } => {
                set(&packet, "operation", "save")?;
                set(&packet, "suggested", suggested)?;
                request_id
            }
            _ => return Err("Unexpected account dialog.".into()),
        };
        set(&packet, "requestId", request_id.to_string())?;
        self.send(&packet, &Array::new())
    }
}

#[derive(Default)]
struct Inbox {
    messages: VecDeque<(String, JsValue)>,
    context: Option<egui::Context>,
}

pub struct Account {
    browser: Window,
    connection: Rc<Connection>,
    inbox: Rc<RefCell<Inbox>>,
    listener: Closure<dyn FnMut(MessageEvent)>,
    profile: String,
    recent: Vec<String>,
    state: Option<AccountProjectState>,
    lease: Option<String>,
    finalized: bool,
}

impl Account {
    pub fn connect(profile: &str) -> Result<Self, String> {
        if profile.is_empty()
            || profile.len() > 128
            || !profile.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':'))
        {
            return Err("VectorCraft needs a valid WebOS account profile.".into());
        }
        let browser = web_sys::window().ok_or("No browser window.")?;
        let parent = browser.parent().ok().flatten().ok_or("No parent desktop.")?;
        let origin = browser.location().origin().map_err(|_| "No browser origin.")?;
        if Object::is(browser.as_ref(), parent.as_ref()) || parent.location().origin().ok().as_deref() != Some(origin.as_str()) {
            return Err("VectorCraft requires a same-origin parent desktop.".into());
        }
        let mut entropy = [0u8; 16];
        browser
            .crypto()
            .and_then(|crypto| crypto.get_random_values_with_u8_array(&mut entropy))
            .map_err(|_| "Could not identify this VectorCraft window.")?;
        let connection = Rc::new(Connection { parent, origin, generation: entropy.iter().map(|byte| format!("{byte:02x}")).collect() });
        let inbox = Rc::new(RefCell::new(Inbox::default()));
        let recv = inbox.clone();
        let expected = connection.clone();
        let listener = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if event.origin() != expected.origin || !event.source().is_some_and(|source| Object::is(source.as_ref(), expected.parent.as_ref())) {
                return;
            }
            let data = event.data();
            if text(&data, "generation", 32).as_deref() != Some(expected.generation.as_str()) {
                return;
            }
            let Some(kind) = text(&data, "type", 64) else { return };
            let Some(kind) = kind.strip_prefix(PREFIX) else { return };
            if !matches!(
                kind,
                "start" | "dialog-result" | "save-result" | "recovery-capture" | "autosave" | "session-save" | "operation" | "session" | "close"
            ) {
                return;
            }
            let Ok(mut inbox) = recv.try_borrow_mut() else { return };
            if inbox.messages.len() < 8 {
                inbox.messages.push_back((kind.into(), data));
                if let Some(context) = &inbox.context {
                    context.request_repaint();
                }
            }
        });
        browser
            .add_event_listener_with_callback("message", listener.as_ref().unchecked_ref())
            .map_err(|_| "Could not listen for VectorCraft account messages.")?;
        Ok(Self {
            browser,
            connection,
            inbox,
            listener,
            profile: profile.to_string(),
            recent: Vec::new(),
            state: None,
            lease: None,
            finalized: false,
        })
    }

    pub fn bind(&self, context: &egui::Context) -> AccountDispatch {
        self.inbox.borrow_mut().context = Some(context.clone());
        let connection = self.connection.clone();
        Box::new(move |intent| connection.intent(intent))
    }

    pub fn ready(&self) -> Result<(), String> {
        self.connection.send(&self.connection.packet("ready")?, &Array::new())
    }

    fn prefs_key(&self) -> String {
        format!("vectorcraft.preferences.account.{}", self.profile)
    }

    pub fn load_prefs(&self, session: &mut vectorcraft_engine::Session) {
        let parsed = super::web::local_storage()
            .ok()
            .and_then(|store| store.get_item(&self.prefs_key()).ok().flatten())
            .filter(|data| data.len() <= 1024 * 1024)
            .and_then(|data| serde_json::from_str(&data).ok());
        if let Some(prefs) = parsed {
            session.prefs = prefs;
        }
    }

    fn recent_key(&self) -> String {
        format!("vectorcraft.recent.account.{}", self.profile)
    }

    pub fn load_recent(&mut self, app: &mut VectorcraftApp) {
        let parsed = super::web::local_storage()
            .ok()
            .and_then(|store| store.get_item(&self.recent_key()).ok().flatten())
            .filter(|data| data.len() <= 256 * 1024)
            .and_then(|data| serde_json::from_str::<Vec<String>>(&data).ok());
        if let Some(paths) = parsed {
            self.recent = paths.into_iter().filter(|path| vectorcraft_ui_egui::account_host::account_project_path(path)).take(30).collect();
            app.ui.recent_files.clone_from(&self.recent);
        }
    }

    pub fn save_prefs(&mut self, app: &mut VectorcraftApp) {
        if app.ui.recent_files != self.recent {
            self.recent.clone_from(&app.ui.recent_files);
            let result = (|| {
                let value = serde_json::to_string(&self.recent).map_err(|error| error.to_string())?;
                super::web::local_storage()?.set_item(&self.recent_key(), &value).map_err(super::web::js_err)
            })();
            if let Err(error) = result {
                log::warn!("Could not retain recent account projects: {error}");
            }
        }
        if !app.take_prefs_changed() {
            return;
        }
        let result = (|| {
            let value = serde_json::to_string(&app.session.prefs).map_err(|error| error.to_string())?;
            super::web::local_storage()?.set_item(&self.prefs_key(), &value).map_err(super::web::js_err)
        })();
        if let Err(error) = result {
            log::warn!("Could not retain account preferences: {error}");
        }
    }

    pub fn paused(&self) -> bool {
        self.lease.is_some() || self.finalized
    }

    pub fn process(&mut self, app: &mut VectorcraftApp) {
        let messages = std::mem::take(&mut self.inbox.borrow_mut().messages);
        for (kind, data) in messages {
            if self.paused() && kind != "session" {
                continue;
            }
            if let Err(error) = self.respond(app, &kind, &data) {
                log::warn!("VectorCraft account message failed: {error}");
            }
        }
    }

    pub fn publish(&mut self, app: &VectorcraftApp) -> Result<(), String> {
        if self.paused() {
            return Ok(());
        }
        let state = app.account_project_state();
        if self.state != state {
            let packet = self.connection.packet("project-state")?;
            state_fields(&packet, state.as_ref())?;
            self.connection.send(&packet, &Array::new())?;
            self.state = state;
        }
        Ok(())
    }

    fn respond(&mut self, app: &mut VectorcraftApp, kind: &str, data: &JsValue) -> Result<(), String> {
        match kind {
            "dialog-result" => return self.dialog_result(app, data),
            "save-result" => return self.save_result(app, data),
            _ => {}
        }
        let request_id = token(data, "requestId").ok_or("Invalid account request identity.")?;
        let mut transfer = Array::new();
        let reply = match kind {
            "start" => "started",
            "recovery-capture" => "recovery-captured",
            "autosave" => "autosave-result",
            "session-save" => "session-save-result",
            "operation" => "operation-result",
            "session" => "session-result",
            "close" => "close-result",
            _ => return Err("Unknown account request.".into()),
        };
        let packet = self.connection.packet(reply)?;
        set(&packet, "requestId", request_id)?;
        let result = match kind {
            "start" => self.start(app, data, &packet),
            "session" => self.session(app, data, &packet),
            "close" => {
                let clean = app.account_project_state().is_none_or(|state| !state.dirty && !state.unfinished_edit && !state.operation_pending);
                set(&packet, "approved", clean)?;
                if clean && field(data, "interactive").and_then(|value| value.as_bool()) == Some(true) {
                    self.finalized = true;
                }
                Ok(())
            }
            _ => document_reply(app, kind, data, &packet, &mut transfer),
        };
        if let Err(error) = result {
            set(&packet, "ok", false)?;
            set(&packet, "message", error)?;
        }
        self.connection.send(&packet, &transfer)
    }

    fn start(&self, app: &mut VectorcraftApp, data: &JsValue, reply: &Object) -> Result<(), String> {
        let path = nullable_text(data, "path", 2048).ok_or("Invalid account path.")?;
        let recover = field(data, "recover").and_then(|value| value.as_bool()).unwrap_or(false);
        let settings = nullable_text(data, "newSettings", 4096)
            .ok_or("Invalid new document settings.")?
            .map(|value| serde_json::from_str::<Value>(&value))
            .transpose()
            .map_err(|_| "Invalid new document settings.")?;
        if settings.as_ref().is_some_and(|value| !value.is_object()) {
            return Err("Invalid new document settings.".into());
        }
        if nullable_text(data, "importName", 255) != Some(None) {
            return Err("The account editor needs a native VectorCraft project.".into());
        }
        let bytes = field(data, "bytes").ok_or("Missing account bytes field.")?;
        let bytes = if bytes.is_null() {
            None
        } else {
            if settings.is_some() {
                return Err("New document settings cannot replace an existing project's contents.".into());
            }
            let buffer = bytes.dyn_into::<ArrayBuffer>().map_err(|_| "Invalid account project bytes.")?;
            if !(1..=MAX_ACCOUNT_PROJECT_BYTES).contains(&(buffer.byte_length() as usize)) {
                return Err("The account project exceeds its size limit.".into());
            }
            Some(Uint8Array::new(buffer.as_ref()).to_vec())
        };
        let state = app.start_account_document(path.as_deref(), bytes.as_deref(), settings.unwrap_or_else(|| json!({})), recover)?;
        set(reply, "documentId", state.document_id.to_string())?;
        set(reply, "revision", state.revision.to_string())?;
        set(reply, "path", state.path.map_or(JsValue::NULL, JsValue::from))?;
        set(reply, "ok", true)
    }

    fn dialog_result(&self, app: &mut VectorcraftApp, data: &JsValue) -> Result<(), String> {
        let request_id = integer(data, "requestId").ok_or("Invalid account dialog identity.")?;
        let path = text(data, "path", 2048);
        let error = text(data, "message", 250);
        let result = match text(data, "outcome", 16).as_deref() {
            Some("destination") => Ok(Some(path.as_deref().ok_or("Missing save destination.")?)),
            Some("cancelled" | "opened") => Ok(None),
            Some("failed") => Err(error.as_deref().unwrap_or("The account dialog failed.")),
            _ => return Err("Invalid account dialog outcome.".into()),
        };
        if let Err(error) = app.finish_account_dialog(request_id, result) {
            app.status(error);
        }
        Ok(())
    }

    fn save_result(&self, app: &mut VectorcraftApp, data: &JsValue) -> Result<(), String> {
        let key = AccountSaveKey {
            ticket: integer(data, "ticket").ok_or("Invalid save ticket.")?,
            document_id: integer(data, "documentId").ok_or("Invalid save document.")?,
            revision: integer(data, "revision").ok_or("Invalid save revision.")?,
            path: text(data, "path", 2048).ok_or("Invalid save path.")?,
        };
        let detail = text(data, "message", 250);
        let result = match text(data, "outcome", 16).as_deref() {
            Some("unknown") => {
                app.finish_account_save(&key, Err("The save outcome is unknown."), true)?;
                return Ok(());
            }
            Some("failed") => Err(detail.as_deref().unwrap_or("The account write failed.")),
            Some("written") => Ok(()),
            _ => return Err("Invalid account save outcome.".into()),
        };
        let packet = self.connection.packet("save-settled")?;
        save_fields(&packet, &key)?;
        match app.finish_account_save(&key, result, false) {
            Ok(adopted) => set(&packet, "adopted", adopted)?,
            Err(error) => set(&packet, "message", error)?,
        }
        let state = Object::new();
        state_fields(&state, app.account_project_state().as_ref())?;
        set(&packet, "projectState", state)?;
        self.connection.send(&packet, &Array::new())
    }

    fn session(&mut self, app: &VectorcraftApp, data: &JsValue, reply: &Object) -> Result<(), String> {
        let operation = text(data, "operation", 12).ok_or("Invalid session operation.")?;
        let lease = token(data, "lease").ok_or("Invalid account lease.")?;
        set(reply, "operation", operation.as_str())?;
        set(reply, "lease", lease.as_str())?;
        match operation.as_str() {
            "pause" if self.lease.is_none() && !self.finalized => {
                app.account_review(
                    integer(data, "documentId").ok_or("Invalid pause document.")?,
                    integer(data, "revision").ok_or("Invalid pause revision.")?,
                    field(data, "dirty").and_then(|value| value.as_bool()).ok_or("Missing dirty state.")?,
                    field(data, "unfinishedEdit").and_then(|value| value.as_bool()).ok_or("Missing native edit state.")?,
                )?;
                self.lease = Some(lease);
            }
            "release" | "finalize" if self.lease.as_ref() == Some(&lease) => {
                self.lease = None;
                self.finalized = operation == "finalize";
            }
            _ => return Err("The account lease is unavailable.".into()),
        }
        set(reply, "ok", true)
    }
}

fn document_reply(app: &mut VectorcraftApp, kind: &str, data: &JsValue, reply: &Object, transfer: &mut Array) -> Result<(), String> {
    let document = integer(data, "documentId").ok_or("Invalid account document identity.")?;
    set(reply, "documentId", document.to_string())?;
    if kind == "operation" {
        let path = nullable_text(data, "path", 2048).ok_or("Invalid operation resource.")?;
        let operation = text(data, "command", 64).ok_or("Invalid account operation.")?;
        let args = text(data, "args", 2048).ok_or("Invalid account operation arguments.")?;
        let value = serde_json::from_str(&args).map_err(|_| "Invalid account operation arguments.")?;
        let result = app.execute_account_operation(document, path.as_deref(), &operation, value)?;
        return set(reply, "result", result.to_string());
    }
    let revision = integer(data, "revision").ok_or("Invalid account revision.")?;
    set(reply, "revision", revision.to_string())?;
    if kind == "recovery-capture" {
        *transfer = attach_bytes(reply, &app.capture_account_recovery(document, revision)?)?;
        return Ok(());
    }
    let path = text(data, "path", 2048).ok_or("Invalid account path.")?;
    set(reply, "path", path.as_str())?;
    if kind == "autosave"
        && app.account_project_state().is_some_and(|current| {
            current.document_id == document
                && current.path.as_deref() == Some(path.as_str())
                && (current.revision != revision || !current.autosave_eligible)
        })
    {
        return set(reply, "outcome", "busy");
    }
    if kind == "session-save" {
        set(reply, "reviewedRevision", revision.to_string())?;
    }
    match app.prepare_account_save(document, revision, &path, kind == "autosave") {
        Ok((key, bytes)) => {
            save_fields(reply, &key)?;
            *transfer = attach_bytes(reply, &bytes)?;
            set(reply, "outcome", "captured")
        }
        Err(error) => {
            set(reply, "outcome", "failed")?;
            set(reply, "message", error)
        }
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        if let Err(error) = self.browser.remove_event_listener_with_callback("message", self.listener.as_ref().unchecked_ref()) {
            log::warn!("Could not remove the VectorCraft account listener: {error:?}");
        }
    }
}
