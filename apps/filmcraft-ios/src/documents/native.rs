//! Audited UIKit boundary. Only the save worker waits; the main event loop never does.
use super::{ReservationBatch, bounded_read_limit, reserve_path, validate_managed_path};
use filmcraft_ui_egui::HostHooks;
use filmcraft_ui_egui::native_dialogs::FileDialog;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSData, NSError, NSFileCoordinator, NSFileCoordinatorReadingOptions, NSFileCoordinatorWritingOptions, NSObject, NSObjectProtocol, NSString, NSURL,
    NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions,
};
use objc2_ui_kit::{UIDocumentPickerDelegate, UIDocumentPickerViewController, UIViewController};
use objc2_uniform_type_identifiers::UTType;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

thread_local! {
    // UIKit's delegate is weak: hold both objects until its terminal callback.
    static ACTIVE: RefCell<Option<(Retained<PickerDelegate>, Retained<UIDocumentPickerViewController>)>> = const { RefCell::new(None) };
}

/// Where a document picker's answer goes. An answer wakes the UI so what asked runs again.
#[derive(Clone)]
struct Reply {
    answer: mpsc::Sender<Vec<String>>,
    ui: egui::Context,
}
impl Reply {
    /// The picked paths; none is a cancelled dialog.
    fn send(&self, paths: Vec<String>) {
        let _ = self.answer.send(paths);
        self.ui.request_repaint();
    }
}

enum Completion {
    Open(Reply, PathBuf),
    Export(mpsc::Sender<Result<Retained<NSURL>, String>>),
}
struct PickerState {
    completion: RefCell<Option<Completion>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. This class is confined to the
    // main thread and owns only Rust ivars; UIKit callbacks use the declared signatures.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = PickerState]
    struct PickerDelegate;
    // SAFETY: NSObjectProtocol has no additional invariants.
    unsafe impl NSObjectProtocol for PickerDelegate {}
    // SAFETY: All callbacks execute on UIKit's main thread; arguments are borrowed only
    // during the call. Selected NSURLs are retained before crossing to a worker.
    unsafe impl UIDocumentPickerDelegate for PickerDelegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn picked(&self, _picker: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
            if let Some(completion) = self.ivars().completion.borrow_mut().take() {
                match completion {
                    Completion::Open(reply, root) => {
                        let urls: Vec<Retained<NSURL>> = urls.iter().collect();
                        let result = std::thread::Builder::new().name("filmcraft-document-import".into()).spawn(move || {
                            let paths = read_selection(urls, &root).unwrap_or_else(|error| {
                                log::error!("Could not import the picked documents: {error}");
                                Vec::new()
                            });
                            reply.send(paths);
                        });
                        if let Err(error) = result {
                            log::error!("Could not start document import: {error}");
                        }
                    }
                    Completion::Export(reply) => {
                        let result = urls.iter().next().ok_or_else(|| "The document picker returned no destination".into());
                        let _ = reply.send(result);
                    }
                }
            }
            release_picker();
        }
        #[unsafe(method(documentPickerWasCancelled:))]
        fn cancelled(&self, _picker: &UIDocumentPickerViewController) {
            if let Some(completion) = self.ivars().completion.borrow_mut().take() {
                match completion {
                    Completion::Open(reply, _) => reply.send(Vec::new()),
                    Completion::Export(reply) => {
                        let _ = reply.send(Err("Save cancelled".into()));
                    }
                }
            }
            release_picker();
        }
    }
);

fn release_picker() {
    // Retain the receiver until after UIKit has returned from the callback.
    dispatch2::DispatchQueue::main().exec_async(|| {
        ACTIVE.with(|active| {
            active.borrow_mut().take();
        })
    });
}

fn root_controller(mtm: objc2::MainThreadMarker) -> Result<Retained<UIViewController>, String> {
    let window = crate::scenes::window(mtm);
    let window = window.ok_or("No active UIKit window")?;
    let root = window.rootViewController().ok_or("The UIKit window has no root controller")?;
    if root.presentedViewController().is_some() {
        return Err("Another UIKit dialog is already open".into());
    }
    Ok(root)
}

fn fail_completion(completion: Completion, error: String) {
    match completion {
        Completion::Open(reply, _) => {
            log::error!("The document picker could not open: {error}");
            reply.send(Vec::new());
        }
        Completion::Export(reply) => {
            let _ = reply.send(Err(error));
        }
    }
}
fn show_picker(picker: Retained<UIDocumentPickerViewController>, completion: Completion, mtm: objc2::MainThreadMarker) {
    let root = match root_controller(mtm) {
        Ok(root) => root,
        Err(error) => {
            fail_completion(completion, error);
            return;
        }
    };
    if ACTIVE.with(|active| active.borrow().is_some()) {
        fail_completion(completion, "A document picker is already open".into());
        return;
    }
    let allocated = PickerDelegate::alloc(mtm).set_ivars(PickerState { completion: RefCell::new(Some(completion)) });
    // SAFETY: NSObject init initializes this allocated subclass, on the main thread;
    // Rust ivars have been initialized and ownership transfers to Retained.
    let delegate: Retained<PickerDelegate> = unsafe { msg_send![super(allocated), init] };
    picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    ACTIVE.with(|active| *active.borrow_mut() = Some((delegate, picker.clone())));
    root.presentViewController_animated_completion(&picker, true, None);
}

/// Security access is tied to the retained immutable URL and balanced even on read errors.
struct SecurityAccess {
    url: Retained<NSURL>,
    started: bool,
}
impl SecurityAccess {
    fn new(url: Retained<NSURL>) -> Self {
        // SAFETY: NSURL is retained and immutable (the binding implements Send + Sync).
        // Foundation permits security scope on worker threads; Drop balances only success.
        let started = unsafe { url.startAccessingSecurityScopedResource() };
        Self { url, started }
    }
}
impl Drop for SecurityAccess {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: The retained URL is live, and this guard owns one successful start.
            unsafe { self.url.stopAccessingSecurityScopedResource() };
        }
    }
}
fn read_selection(urls: Vec<Retained<NSURL>>, root: &Path) -> Result<Vec<String>, String> {
    if urls.len() > 256 {
        return Err("Select at most 256 files at once".into());
    }
    let mut files = Vec::new();
    let mut batch = ReservationBatch::default();
    for url in urls {
        let access = SecurityAccess::new(url);
        let path = access.url.path().ok_or("The selected file has no file path")?.to_string();
        let name = super::file_name(&path)?;
        let managed = batch.reserve(root, &name)?;
        // The destination is remembered before the copy exists, so the tracker never takes the
        // imported file for a new document of the app's own.
        remember_destination(&managed, &access.url)?;
        coordinated_copy_in(&access.url, &managed)?;
        files.push(managed.to_str().ok_or("Non-Unicode managed file path")?.to_owned());
    }
    batch.commit();
    Ok(files)
}

/// Copy the file at `url` into `to`, streaming and without holding it in memory. It appears under its
/// name only once complete (a hidden temporary name until then), so nothing watches half a file.
fn coordinated_copy_in(url: &NSURL, to: &Path) -> Result<(), String> {
    let name = to.file_name().and_then(|n| n.to_str()).ok_or("The file has no usable name")?;
    let partial = to.with_file_name(format!(".{name}.part"));
    let outcome = RefCell::new(None);
    let reader = block2::RcBlock::new(|target: std::ptr::NonNull<NSURL>| {
        // SAFETY: Foundation guarantees a live immutable NSURL during the synchronous
        // coordination accessor; no pointer or borrowed Objective-C object escapes.
        let target = unsafe { target.as_ref() };
        let result = target
            .path()
            .ok_or_else(|| "The provider returned no local file path".to_string())
            .and_then(|source| super::copy_regular_file(Path::new(&source.to_string()), &partial));
        *outcome.borrow_mut() = Some(result);
    });
    let mut error: Option<Retained<NSError>> = None;
    NSFileCoordinator::new().coordinateReadingItemAtURL_options_error_byAccessor(url, NSFileCoordinatorReadingOptions::empty(), Some(&mut error), &reader);
    drop(reader);
    let result = match error {
        Some(error) => Err(error.to_string()),
        None => outcome.into_inner().unwrap_or_else(|| Err("The file provider did not allow reading".into())),
    };
    match result {
        Ok(()) => {
            let renamed = std::fs::rename(&partial, to).map_err(|e| e.to_string());
            if renamed.is_err() {
                let _ = std::fs::remove_file(&partial);
            }
            renamed
        }
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            Err(error)
        }
    }
}

fn bookmark_path(path: &Path) -> Result<PathBuf, String> {
    Ok(path.parent().ok_or("Missing managed document directory")?.join("metadata").join("destination.bookmark"))
}
fn remember_destination(path: &Path, url: &Retained<NSURL>) -> Result<(), String> {
    let access = SecurityAccess::new(url.clone());
    let url = &access.url;
    // iOS bookmarks preserve implicit security scope; macOS-only WithSecurityScope is
    // deliberately omitted. The picker-provided URL remains retained for this operation.
    let data = url
        .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(NSURLBookmarkCreationOptions::MinimalBookmark, None, None)
        .map_err(|e| e.to_string())?;
    let bookmark = bookmark_path(path)?;
    std::fs::create_dir_all(bookmark.parent().ok_or("Missing bookmark metadata directory")?).map_err(|e| e.to_string())?;
    crate::storage::write_atomic(&bookmark, &data.to_vec()).map_err(|e| e.to_string())
}
fn resolve_destination(path: &Path) -> Result<Option<Retained<NSURL>>, String> {
    let bookmark = bookmark_path(path)?;
    if !bookmark.exists() {
        return Ok(None);
    }
    let data = NSData::from_vec(bounded_read_limit(&bookmark, 1024 * 1024)?);
    // SAFETY: NSData and NSURL are retained, no borrowed objects escape, and the optional
    // stale output is null (we refresh the bookmark after each successful external write).
    let url = unsafe {
        NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
            &data,
            NSURLBookmarkResolutionOptions::WithoutUI,
            None,
            std::ptr::null_mut(),
        )
    }
    .map_err(|e| format!("The saved destination is unavailable: {e}. Use Save As to choose it again."))?;
    Ok(Some(url))
}

/// Replace the file at `url` with a copy of `from`, streaming, under file coordination.
fn coordinated_copy_out(url: Retained<NSURL>, from: &Path) -> Result<(), String> {
    let access = SecurityAccess::new(url);
    let outcome = RefCell::new(None);
    let writer = block2::RcBlock::new(|target: std::ptr::NonNull<NSURL>| {
        // SAFETY: NSFileCoordinator supplies a valid NSURL for the duration of its
        // synchronous accessor. It is never retained or used after the accessor returns.
        let target = unsafe { target.as_ref() };
        let result = target
            .path()
            .ok_or_else(|| "The provider returned no local file path".to_string())
            .and_then(|destination| super::copy_regular_file(from, Path::new(&destination.to_string())));
        *outcome.borrow_mut() = Some(result);
    });
    let mut error: Option<Retained<NSError>> = None;
    NSFileCoordinator::new().coordinateWritingItemAtURL_options_error_byAccessor(
        &access.url,
        NSFileCoordinatorWritingOptions::ForReplacing,
        Some(&mut error),
        &writer,
    );
    drop(writer);
    match error {
        Some(error) => Err(error.to_string()),
        None => outcome.into_inner().unwrap_or_else(|| Err("The file provider did not allow writing".into())),
    }
}

/// The document types a picker offers: the extensions of the first filter (the union of what the
/// app reads or places), or any data when none of them names a known type.
fn content_types(extensions: &[String]) -> Retained<NSArray<UTType>> {
    let mut types: Vec<Retained<UTType>> =
        extensions.iter().filter_map(|extension| UTType::typeWithFilenameExtension(&NSString::from_str(extension.as_str()))).collect();
    if types.is_empty() {
        types.extend(UTType::typeWithIdentifier(&NSString::from_str("public.data")));
    }
    NSArray::from_retained_slice(&types)
}

/// Present the open picker on the main queue; its answer is the imported snapshots' paths.
fn open_picker(reply: Reply, root: PathBuf, multiple: bool, extensions: Vec<String>) {
    dispatch2::DispatchQueue::main().exec_async(move || {
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            fail_completion(Completion::Open(reply, root), "UIKit main queue unavailable".into());
            return;
        };
        let types = content_types(&extensions);
        let picker = UIDocumentPickerViewController::initForOpeningContentTypes_asCopy(UIDocumentPickerViewController::alloc(mtm), &types, false);
        picker.setAllowsMultipleSelection(multiple);
        show_picker(picker, Completion::Open(reply, root), mtm);
    });
}

/// Take the managed file at `path`, which the app has just written, to where the user keeps it: the
/// remembered destination, or the one the export picker lets them choose. Streams the file; runs on a
/// worker, which may wait for the picker, never on the UI thread. `Ok(false)`: the user declined the picker.
pub fn push(root: &Path, path: &Path) -> Result<bool, String> {
    validate_managed_path(root, path)?;
    if let Some(destination) = resolve_destination(path)? {
        coordinated_copy_out(destination.clone(), path)?;
        remember_destination(path, &destination).map_err(|e| format!("Saved, but its destination could not be remembered: {e}"))?;
        return Ok(true);
    }
    // The picker copies a private staging name of the same file (a hard link where the volume allows
    // it, so a multi-gigabyte export is not stored twice).
    let stage = path.parent().ok_or("Missing managed directory")?.join("export");
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    let stage = stage.join(super::file_name(path.to_str().ok_or("Non-Unicode file path")?)?);
    let _ = std::fs::remove_file(&stage);
    if std::fs::hard_link(path, &stage).is_err() {
        super::copy_regular_file(path, &stage)?;
    }
    let (tx, rx) = mpsc::channel();
    let picker_stage = stage.clone();
    dispatch2::DispatchQueue::main().exec_async(move || {
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            let _ = tx.send(Err("UIKit main queue unavailable".into()));
            return;
        };
        let Some(stage) = picker_stage.to_str() else {
            let _ = tx.send(Err("Non-Unicode export path".into()));
            return;
        };
        let url = NSURL::fileURLWithPath(&NSString::from_str(stage));
        let urls = NSArray::from_retained_slice(&[url]);
        let picker = UIDocumentPickerViewController::initForExportingURLs_asCopy(UIDocumentPickerViewController::alloc(mtm), &urls, true);
        show_picker(picker, Completion::Export(tx), mtm);
    });
    let result = rx.recv().map_err(|_| "The document picker closed without saving".to_owned()).and_then(|result| result);
    let _ = std::fs::remove_file(&stage);
    match result {
        Ok(destination) => {
            // Never arm a managed path until UIKit confirms the export completed.
            remember_destination(path, &destination).map_err(|e| format!("Exported, but the destination could not be remembered: {e}"))?;
            Ok(true)
        }
        Err(error) if error == "Save cancelled" => Ok(false),
        Err(error) => Err(error),
    }
}

/// Install the file dialogs. `ui` is woken when a picker answers.
///
/// FilmCraft writes a saved project or an export straight to the path its dialog answered. That path is
/// a reserved file inside the app container; [`super::DocumentSync`] notices when it is written and takes
/// it to the place the user chose (the export picker the first time, the remembered destination after).
pub fn install(hooks: &mut HostHooks, root: PathBuf, ui: egui::Context) -> Result<(), String> {
    std::fs::create_dir_all(root.join("documents")).map_err(|e| e.to_string())?;
    hooks.run_dialog = Some(Box::new(move |dialog: FileDialog| {
        let (answer, receiver) = mpsc::channel();
        let reply = Reply { answer, ui: ui.clone() };
        let save = |reply: &Reply, name: &str| match reserve_path(&root, name)
            .and_then(|p| p.to_str().map(str::to_owned).ok_or_else(|| "Non-Unicode managed file path".to_string()))
        {
            Ok(path) => reply.send(vec![path]),
            Err(error) => {
                log::error!("Could not reserve a file for saving: {error}");
                reply.send(Vec::new());
            }
        };
        let open = |reply: Reply, exts: &[String], multiple: bool| {
            let extensions: Vec<String> = exts.iter().filter(|e| e.as_str() != "*").cloned().collect();
            open_picker(reply, root.clone(), multiple, extensions);
        };
        match dialog {
            FileDialog::SaveProject { name } => save(&reply, &name),
            FileDialog::SaveAs { name, .. } => {
                // A full path (Quick Export) keeps only its file name.
                let file = Path::new(&name).file_name().and_then(|n| n.to_str()).unwrap_or("Untitled").to_owned();
                save(&reply, &file)
            }
            FileDialog::OpenFiles { exts } => open(reply, &exts, true),
            FileDialog::OpenFile { exts, .. } | FileDialog::Relink { exts, .. } => open(reply, &exts, false),
            FileDialog::OpenProject => open(reply, &["fcproj".to_string()], false),
            FileDialog::Folder { .. } => {
                log::warn!("Picking a folder is not supported on iPadOS yet");
                reply.send(Vec::new());
            }
        }
        receiver
    }));
    Ok(())
}
