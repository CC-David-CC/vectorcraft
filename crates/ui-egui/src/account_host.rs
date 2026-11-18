//! The native side of a WebOS document window. Storage and authorization live in the desktop.

use crate::{VectorcraftApp, fileio, io};
use serde_json::{Value, json};
use vectorcraft_doc::NodeId;
use vectorcraft_engine::EngineError;
use vectorcraft_engine::cmd::fileio::{SaveJob, SaveMode};

pub const MAX_ACCOUNT_PROJECT_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountSaveKey {
    pub ticket: u64,
    pub document_id: u64,
    pub revision: u64,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountProjectState {
    pub document_id: u64,
    pub revision: u64,
    pub path: Option<String>,
    pub dirty: bool,
    pub unfinished_edit: bool,
    pub operation_pending: bool,
    pub capturable: bool,
    pub autosave_eligible: bool,
}

pub enum AccountIntent {
    New { request_id: u64, settings: Value },
    Open { request_id: u64, path: Option<String> },
    ChooseSave { request_id: u64, suggested: String },
    Save(AccountSaveKey, Vec<u8>),
    Close,
}

pub type AccountDispatch = Box<dyn FnMut(AccountIntent) -> Result<(), String>>;

struct PendingSave {
    key: AccountSaveKey,
    job: SaveJob,
    warnings: Vec<String>,
    unknown: bool,
}

#[derive(Default)]
pub(crate) struct AccountHost {
    document: Option<u64>,
    serial: u64,
    dialog: Option<(u64, bool)>,
    save: Option<PendingSave>,
    input_pending: bool,
}

/// Account paths are labels and resource identities. They are never used for filesystem access.
pub fn account_project_path(path: &str) -> bool {
    path.len() <= 2048
        && fileio::extension(path) == vectorcraft_format::EXTENSION
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

impl VectorcraftApp {
    pub(crate) fn route_account_command(&mut self, command: &str, params: &Value) -> Option<Result<Value, String>> {
        self.services.account_dispatch.as_ref()?;
        let result = match command {
            "file.new" if self.account_host.document.is_some() => {
                self.begin_account_dialog(false, |request_id| AccountIntent::New { request_id, settings: params.clone() })
            }
            "file.open" | "document.open" => self.open_account_picker(params.get("path").and_then(Value::as_str)),
            "file.close" | "file.closeAll" | "app.quit" | "document.close" => self.account_intent(AccountIntent::Close),
            "document.save" => return Some(self.account_save_command(false)),
            id if id.starts_with("file.openRecent") => {
                let path = id
                    .strip_prefix("file.openRecent")
                    .and_then(|slot| slot.parse::<usize>().ok())
                    .and_then(|slot| slot.checked_sub(1))
                    .and_then(|slot| io::recent_files(self).get(slot))
                    .cloned();
                self.open_account_picker(path.as_deref())
            }
            "file.newFromTemplate" | "file.openWith" | "file.revert" | "window.newWindow" | "edit.newDocumentFromState" => {
                Err("This account window opens one .vectorcraft project. Use Desktop to open another.".into())
            }
            _ => return None,
        };
        Some(result.map(|_| Value::Null))
    }

    pub(crate) fn account_intent(&mut self, intent: AccountIntent) -> Result<(), String> {
        self.services.account_dispatch.as_mut().ok_or("This view has no account host.")?(intent)
    }

    fn begin_account_dialog(&mut self, save: bool, intent: impl FnOnce(u64) -> AccountIntent) -> Result<(), String> {
        if self.account_host.dialog.is_some() {
            return Err("An account dialog is already open.".into());
        }
        let request_id = self.next_account_id()?;
        self.account_intent(intent(request_id))?;
        self.account_host.dialog = Some((request_id, save));
        Ok(())
    }

    pub(crate) fn open_account_picker(&mut self, path: Option<&str>) -> Result<(), String> {
        if path.is_some_and(|path| !account_project_path(path)) {
            return Err("Choose a .vectorcraft project from account Files.".into());
        }
        self.begin_account_dialog(false, |request_id| AccountIntent::Open { request_id, path: path.map(str::to_string) })
    }

    pub fn start_account_document(
        &mut self,
        path: Option<&str>,
        bytes: Option<&[u8]>,
        settings: Value,
        recover: bool,
    ) -> Result<AccountProjectState, String> {
        if self.services.account_dispatch.is_none() || self.account_host.document.is_some() || !self.session.documents().is_empty() {
            return Err("This window already owns a document or has no account host.".into());
        }
        if path.is_some_and(|path| !account_project_path(path)) {
            return Err("Choose an account-relative .vectorcraft path.".into());
        }
        if let Some(bytes) = bytes {
            if bytes.is_empty() || bytes.len() > MAX_ACCOUNT_PROJECT_BYTES {
                return Err("Invalid account project size.".into());
            }
            let native = vectorcraft_format::load_file_capped(bytes, MAX_ACCOUNT_PROJECT_BYTES as u64).map_err(|error| error.to_string())?;
            if native.doc.template {
                return Err("Open a normal .vectorcraft project in this account window.".into());
            }
            drop(native);
            // Use the normal open path to restore native view, text layout and linked-preview warnings.
            let name = path.map(fileio::file_name).unwrap_or_else(|| "Recovered.vectorcraft".to_string());
            io::open_bytes(self, &name, bytes, path.map(str::to_string))?;
            if recover {
                self.session.doc_mut().map_err(|error| error.to_string())?.mark_unsaved();
            }
        } else if !recover && path.is_none() {
            self.run("file.new", settings)?;
        } else {
            return Err("The account project bytes are missing.".into());
        }
        self.account_host.document = self.session.active().map(|document| document.uid);
        let state = self.account_project_state().ok_or("The account project did not open.")?;
        if state.path.as_deref() != path {
            return Err("The account editor did not adopt the requested path.".into());
        }
        if let Some(path) = path {
            io::note_recent(self, path);
        }
        Ok(state)
    }

    fn account_transient_edit(&self) -> bool {
        self.ime_marked.is_some() || self.session.pointer_held() || self.session.tool_busy()
    }

    pub fn account_input_pending(&mut self, pending: bool) {
        self.account_host.input_pending = pending;
    }

    pub fn account_project_state(&self) -> Option<AccountProjectState> {
        let uid = self.account_host.document?;
        let document = self.session.documents().iter().find(|document| document.uid == uid)?;
        let transient_edit = self.account_transient_edit();
        let unfinished_edit = transient_edit || self.session.tool_wants_text();
        let operation_pending = self.account_host.input_pending
            || self.account_host.save.is_some()
            || self.account_host.dialog.is_some()
            || self.ui.dialog.is_some()
            || self.file_dialog_open()
            || self.session.tool_waiting()
            || self.session.script_running()
            || crate::opening::long_operation_busy(self)
            || self.session.documents().len() != 1;
        Some(AccountProjectState {
            document_id: uid,
            revision: document.revision,
            path: document.path.clone(),
            dirty: document.is_dirty(),
            unfinished_edit,
            operation_pending,
            capturable: !transient_edit && self.session.documents().len() == 1,
            autosave_eligible: !unfinished_edit && !operation_pending && document.is_dirty() && document.path.is_some(),
        })
    }

    fn account_revision(&self, document_id: u64, revision: u64) -> Result<AccountProjectState, String> {
        self.account_project_state()
            .filter(|state| (state.document_id, state.revision) == (document_id, revision))
            .ok_or_else(|| "The account project changed before the request completed.".to_string())
    }

    pub fn capture_account_recovery(&self, document_id: u64, revision: u64) -> Result<Vec<u8>, String> {
        let state = self.account_revision(document_id, revision)?;
        let document = self.session.active().filter(|document| document.uid == document_id).ok_or("The account project is unavailable.")?;
        if !state.capturable || !state.dirty {
            return Err("Finish the active edit before capturing recovery.".into());
        }
        let bytes = vectorcraft_format::save_with(&document.doc, &vectorcraft_format::SaveOptions::default()).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_ACCOUNT_PROJECT_BYTES {
            return Err("The recovery snapshot exceeds the account project limit.".into());
        }
        Ok(bytes)
    }

    fn next_account_id(&mut self) -> Result<u64, String> {
        let serial = self.account_host.serial.checked_add(1).ok_or("Too many account requests.")?;
        self.account_host.serial = serial;
        Ok(serial)
    }

    pub(crate) fn account_save_command(&mut self, save_as: bool) -> Result<Value, String> {
        let state = self.account_project_state().ok_or("No account project is open.")?;
        if !state.capturable || state.operation_pending {
            return Err("Finish the active edit or operation before saving.".into());
        }
        if !save_as && let Some(path) = state.path {
            let (key, bytes) = self.prepare_account_save(state.document_id, state.revision, &path, false)?;
            return self.dispatch_account_save(key, bytes);
        }
        let suggested = state.path.as_deref().map(fileio::file_name).unwrap_or_else(|| "Untitled.vectorcraft".into());
        self.begin_account_dialog(true, |request_id| AccountIntent::ChooseSave { request_id, suggested })?;
        Ok(json!({"pending":"account-save-destination"}))
    }

    pub fn finish_account_dialog(&mut self, request_id: u64, destination: Result<Option<&str>, &str>) -> Result<Value, String> {
        let Some((id, save)) = self.account_host.dialog else {
            return Err("No account dialog is open.".into());
        };
        if id != request_id || !save && matches!(destination, Ok(Some(_))) {
            return Err("The account dialog reply does not match the active request.".into());
        }
        self.account_host.dialog = None;
        let Some(path) = destination? else { return Ok(Value::Null) };
        let state = self.account_project_state().ok_or("No account project is open.")?;
        let (key, bytes) = self.prepare_account_save(state.document_id, state.revision, path, false)?;
        self.dispatch_account_save(key, bytes)
    }

    fn dispatch_account_save(&mut self, key: AccountSaveKey, bytes: Vec<u8>) -> Result<Value, String> {
        let ticket = key.ticket.to_string();
        if let Err(error) = self.account_intent(AccountIntent::Save(key, bytes)) {
            self.account_host.save = None;
            return Err(error);
        }
        Ok(json!({"pending":"account-storage","ticket":ticket}))
    }

    pub fn prepare_account_save(
        &mut self,
        document_id: u64,
        revision: u64,
        path: &str,
        automatic: bool,
    ) -> Result<(AccountSaveKey, Vec<u8>), String> {
        let state = self.account_revision(document_id, revision)?;
        if !account_project_path(path)
            || !state.capturable
            || state.operation_pending
            || automatic && (!state.autosave_eligible || state.path.as_deref() != Some(path))
        {
            return Err("The account project or destination is unavailable for saving.".into());
        }
        io::remember_view(self);
        let plan =
            fileio::save_plan(&self.session, SaveMode::SaveAs, &json!({"path":path,"format":"vectorcraft"})).map_err(|error| error.to_string())?;
        let job = fileio::save_job(&mut self.session, plan).map_err(|error| error.to_string())?;
        let document = self.session.active().filter(|document| document.uid == document_id).ok_or("The account project is unavailable.")?;
        let revision = document.revision;
        let mut bytes = None;
        let result = job
            .write(|written, payload| {
                if written != path || bytes.is_some() || payload.is_empty() || payload.len() > MAX_ACCOUNT_PROJECT_BYTES {
                    return Err(EngineError::Other("An account save requires one bounded native project.".into()));
                }
                bytes = Some(payload.to_vec());
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        let bytes = bytes.ok_or("The native encoder did not write the account project.")?;
        let key = AccountSaveKey { ticket: self.next_account_id()?, document_id, revision, path: path.to_string() };
        self.account_host.save = Some(PendingSave { key: key.clone(), job, warnings: io::notes_of(&result), unknown: false });
        self.status("Saving to the account…");
        Ok((key, bytes))
    }

    pub fn finish_account_save(&mut self, key: &AccountSaveKey, result: Result<(), &str>, unknown: bool) -> Result<bool, String> {
        let pending =
            self.account_host.save.as_mut().filter(|pending| &pending.key == key).ok_or("The save reply does not match the pending write.")?;
        if unknown {
            pending.unknown = true;
            self.status("The account write result is unconfirmed. Verify it before retrying.");
            return Ok(false);
        }
        if pending.unknown && result.is_err() {
            return Err("The unconfirmed account write requires verification.".into());
        }
        let pending = self.account_host.save.take().ok_or("No account write is pending.")?;
        match result {
            Ok(()) => {
                let document = self
                    .session
                    .active_mut()
                    .filter(|document| document.uid == key.document_id)
                    .ok_or("The account project closed before save confirmation.")?;
                pending.job.finish(document);
                io::note_recent(self, &key.path);
                io::report_saved(self, &key.path, &json!({"warnings":pending.warnings}));
                Ok(true)
            }
            Err(message) => {
                self.status(format!("Account save failed: {message}"));
                Ok(false)
            }
        }
    }

    pub fn account_review(&self, document_id: u64, revision: u64, dirty: bool, unfinished_edit: bool) -> Result<(), String> {
        let state = self.account_revision(document_id, revision)?;
        if state.operation_pending || state.dirty != dirty || state.unfinished_edit != unfinished_edit {
            return Err("The account project is busy or changed after review.".into());
        }
        Ok(())
    }

    /// Scope and consent are checked by Desktop before it dispatches this exact target.
    pub fn execute_account_operation(&mut self, document_id: u64, path: Option<&str>, operation: &str, args: Value) -> Result<Value, String> {
        let state = self
            .account_project_state()
            .filter(|state| state.document_id == document_id && state.path.as_deref() == path)
            .ok_or("The account project binding changed.")?;
        let document = self.session.active().filter(|document| document.uid == document_id).ok_or("The account project is unavailable.")?;
        let args = args.as_object().ok_or("Expected an operation argument object.")?;
        match operation {
            "document.inspect" if args.is_empty() => {
                let layers = document.doc.layers.iter().take(256).map(|layer| json!({"id":layer.id.0.to_string(),"name":layer.name}));
                Ok(json!({"revision":state.revision.to_string(),"dirty":state.dirty,"layers":layers.collect::<Vec<_>>() }))
            }
            "layer.rename" if args.len() == 2 => {
                if state.unfinished_edit || state.operation_pending {
                    return Err("Finish the active edit or operation before renaming.".into());
                }
                let id = args
                    .get("layerId")
                    .and_then(Value::as_str)
                    .and_then(|id| id.parse::<u64>().ok())
                    .map(NodeId)
                    .filter(|id| document.doc.node(*id).is_some_and(vectorcraft_doc::Node::is_layer))
                    .ok_or("Unknown VectorCraft layer.")?;
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control))
                    .ok_or("Choose a valid layer name.")?;
                self.run("layer.setProps", json!({"id":id.0,"name":name}))?;
                let revision = self.session.active().ok_or("The account project closed.")?.revision;
                Ok(json!({"layerId":id.0.to_string(),"name":name,"revision":revision.to_string()}))
            }
            _ => Err("Unsupported VectorCraft account operation.".into()),
        }
    }
}

#[cfg(test)]
mod tests;
