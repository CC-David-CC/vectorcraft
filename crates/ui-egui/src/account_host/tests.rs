use std::{cell::RefCell, rc::Rc};

use super::*;

fn editor() -> (VectorcraftApp, Rc<RefCell<Vec<AccountIntent>>>) {
    let events = Rc::new(RefCell::new(Vec::new()));
    let capture = events.clone();
    let mut app = VectorcraftApp::new(
        vectorcraft_engine::Session::new(),
        crate::Services {
            account_dispatch: Some(Box::new(move |event| {
                capture.borrow_mut().push(event);
                Ok(())
            })),
            download: Some(Box::new(|_, _| panic!("hosted project saves must use Storage"))),
            ..Default::default()
        },
    );
    let state = app.start_account_document(None, None, json!({"width":120,"height":90}), false).unwrap();
    assert!(!state.dirty);
    (app, events)
}

fn rectangle(app: &mut VectorcraftApp, x: f64) -> NodeId {
    let value = app.run("shape.rectangle", json!({"x":x,"y":10,"width":12,"height":15})).unwrap();
    NodeId(value["id"].as_u64().unwrap())
}

fn begin_save(app: &mut VectorcraftApp, events: &RefCell<Vec<AccountIntent>>, path: &str) -> (AccountSaveKey, Vec<u8>) {
    let result = app.run("file.saveAs", json!({})).unwrap();
    assert_eq!(result["pending"], "account-save-destination");
    let Some(AccountIntent::ChooseSave { request_id, .. }) = events.borrow_mut().pop() else { panic!("Save As intent missing") };
    assert!(app.finish_account_dialog(request_id + 1, Ok(Some(path))).is_err());
    app.finish_account_dialog(request_id, Ok(Some(path))).unwrap();
    let Some(AccountIntent::Save(key, bytes)) = events.borrow_mut().pop() else { panic!("Storage write missing") };
    (key, bytes)
}

#[test]
fn hosted_save_adopts_the_exact_receipt_and_leaves_later_edits_dirty() {
    let (mut app, events) = editor();
    let first = rectangle(&mut app, 10.0);
    let path = "Projects/A/Drawing.vectorcraft";
    let (key, bytes) = begin_save(&mut app, &events, path);
    assert!(app.account_project_state().unwrap().path.is_none());
    let second = rectangle(&mut app, 40.0);
    let saved = vectorcraft_format::load(&bytes).unwrap();
    assert!(saved.node(first).is_some() && saved.node(second).is_none());
    let other = AccountSaveKey { path: "Projects/B/Drawing.vectorcraft".into(), ..key.clone() };
    assert!(app.finish_account_save(&other, Ok(()), false).is_err());
    assert!(app.finish_account_save(&key, Ok(()), false).unwrap());
    assert!(app.finish_account_save(&key, Ok(()), false).is_err());
    let state = app.account_project_state().unwrap();
    assert_eq!(state.path.as_deref(), Some(path));
    assert!(state.dirty && state.revision > key.revision);
    assert!(app.session.active().unwrap().doc.node(second).is_some());
    app.run("edit.undo", json!({})).unwrap();
    assert!(!app.account_project_state().unwrap().dirty);
}

#[test]
fn hosted_destination_cancel_conflict_and_uncertainty_preserve_the_binding() {
    let (mut app, events) = editor();
    let own = "Projects/Original.vectorcraft";
    let (key, _) = begin_save(&mut app, &events, own);
    app.finish_account_save(&key, Ok(()), false).unwrap();
    rectangle(&mut app, 10.0);
    app.run("file.saveAs", json!({})).unwrap();
    let Some(AccountIntent::ChooseSave { request_id, .. }) = events.borrow_mut().pop() else { panic!("Save As intent missing") };
    app.finish_account_dialog(request_id, Ok(None)).unwrap();
    assert!(events.borrow().is_empty());
    let (other, _) = begin_save(&mut app, &events, "Projects/Other.vectorcraft");
    assert!(!app.finish_account_save(&other, Err("the destination changed"), false).unwrap());
    assert_eq!(app.account_project_state().unwrap().path.as_deref(), Some(own));
    app.run("file.save", json!({})).unwrap();
    let Some(AccountIntent::Save(key, _)) = events.borrow_mut().pop() else { panic!("ordinary Save intent missing") };
    assert_eq!(key.path, own);
    assert!(!app.finish_account_save(&key, Err("lost response"), true).unwrap());
    assert!(app.run("file.save", json!({})).is_err());
    let state = app.account_project_state().unwrap();
    assert!(state.dirty && state.operation_pending && state.capturable);
    assert!(app.capture_account_recovery(state.document_id, state.revision).is_ok());
    assert!(app.finish_account_save(&key, Err("retry"), false).is_err());
}

#[test]
fn hosted_native_recovery_and_scoped_layer_renaming_share_editor_commands() {
    let (mut app, _) = editor();
    let object = rectangle(&mut app, 10.0);
    let state = app.account_project_state().unwrap();
    let inspect = app.execute_account_operation(state.document_id, None, "document.inspect", json!({})).unwrap();
    let layer = inspect["layers"][0]["id"].as_str().unwrap();
    let args = json!({"layerId":layer,"name":"Ink"});
    assert!(app.execute_account_operation(state.document_id + 1, None, "layer.rename", args.clone()).is_err());
    assert!(app.execute_account_operation(state.document_id, Some("Wrong.vectorcraft"), "layer.rename", args.clone()).is_err());
    assert!(app.execute_account_operation(state.document_id, None, "file.save", json!({})).is_err());
    app.ime_marked = Some("preview".into());
    assert!(app.execute_account_operation(state.document_id, None, "layer.rename", args.clone()).is_err());
    assert!(app.capture_account_recovery(state.document_id, state.revision).is_err());
    app.ime_marked = None;
    app.execute_account_operation(state.document_id, None, "layer.rename", args).unwrap();
    let renamed = app.account_project_state().unwrap();
    assert!(app.capture_account_recovery(state.document_id, state.revision).is_err());
    let bytes = app.capture_account_recovery(renamed.document_id, renamed.revision).unwrap();
    let mut other = VectorcraftApp::new(
        vectorcraft_engine::Session::new(),
        crate::Services { account_dispatch: Some(Box::new(|_| Ok(()))), ..Default::default() },
    );
    assert!(other.start_account_document(Some("Art.vectorcraft"), Some(b"bad bytes"), json!({}), false).is_err());
    assert!(other.session.documents().is_empty());
    let recovered = other.start_account_document(None, Some(&bytes), json!({}), true).unwrap();
    assert!(recovered.dirty && recovered.path.is_none());
    let document = &other.session.active().unwrap().doc;
    assert!(document.node(object).is_some());
    assert_eq!(document.layers[0].name.as_deref(), Some("Ink"));
    app.run("edit.undo", json!({})).unwrap();
    assert_ne!(app.session.active().unwrap().doc.layers[0].name.as_deref(), Some("Ink"));
}

#[test]
fn hosted_native_drawing_round_trips_and_svg_copy_preserves_binding() {
    let (mut app, events) = editor();
    let shape = rectangle(&mut app, 10.0);
    app.run("path.moveAnchor", json!({"id":shape.0,"anchor":0,"x":4,"y":5})).unwrap();
    app.run("paint.setFill", json!({"ids":[shape.0],"color":"#1428aa"})).unwrap();
    let text = app.run("text.create", json!({"x":20,"y":24,"text":"Native text","size":16})).unwrap();
    let text = NodeId(text["id"].as_u64().unwrap());
    let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([20, 40, 170, 255]));
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png).unwrap();
    let data = vectorcraft_format::base64_encode(png.get_ref());
    let placed = app.run("file.place", json!({"name":"sample.png","dataBase64":data,"link":false,"at":[45,50]})).unwrap();
    let image = NodeId(placed["ids"][0].as_u64().unwrap());
    let path = "Projects/Native.vectorcraft";
    let (key, bytes) = begin_save(&mut app, &events, path);
    assert!(app.finish_account_save(&key, Ok(()), false).unwrap());

    let mut reopened = VectorcraftApp::new(
        vectorcraft_engine::Session::new(),
        crate::Services { account_dispatch: Some(Box::new(|_| Ok(()))), ..Default::default() },
    );
    let state = reopened.start_account_document(Some(path), Some(&bytes), json!({}), false).unwrap();
    assert!(!state.dirty);
    let source = &app.session.active().unwrap().doc;
    let restored = &reopened.session.active().unwrap().doc;
    for id in [shape, text, image] {
        assert_eq!(serde_json::to_value(source.node(id).unwrap()).unwrap(), serde_json::to_value(restored.node(id).unwrap()).unwrap());
    }
    assert_eq!(source.images.keys().collect::<Vec<_>>(), restored.images.keys().collect::<Vec<_>>());
    assert_eq!(restored.images.len(), 1);

    let downloads = Rc::new(RefCell::new(Vec::new()));
    let capture = downloads.clone();
    app.services.download = Some(Box::new(move |name, bytes| {
        capture.borrow_mut().push((name.to_string(), bytes.to_vec()));
        Ok(())
    }));
    app.run("file.saveCopy", json!({"path":"Native.svg","format":"svg","options":{"precision":3}})).unwrap();
    let downloads = downloads.borrow();
    assert_eq!(downloads.len(), 1);
    assert_eq!(downloads[0].0, "Native.svg");
    assert!(std::str::from_utf8(&downloads[0].1).unwrap().contains("<svg"));
    assert_eq!(app.account_project_state().unwrap().path.as_deref(), Some(path));
    assert!(events.borrow().is_empty());
}
