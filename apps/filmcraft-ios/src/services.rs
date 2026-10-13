//! Assemble native services without exposing platform objects to the shared UI.
use std::path::{Path, PathBuf};

use filmcraft_ui_egui::HostHooks;

fn support_directory() -> Result<PathBuf, String> {
    use objc2_foundation::{NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask};
    let directories =
        NSFileManager::defaultManager().URLsForDirectory_inDomains(NSSearchPathDirectory::ApplicationSupportDirectory, NSSearchPathDomainMask::UserDomainMask);
    let path = directories.firstObject().and_then(|url| url.path()).ok_or("iOS Application Support directory is unavailable")?;
    let expected = crate::storage::app_support_dir(std::env::var_os("HOME"))?;
    let root = PathBuf::from(path.to_string()).join(expected.file_name().ok_or("Application Support name is missing")?);
    // The OS can spell the same container through /var or /private/var.
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    if root.canonicalize().map_err(|error| error.to_string())? != expected.canonicalize().map_err(|error| error.to_string())? {
        return Err("Application Support is outside the expected app container".into());
    }
    Ok(root)
}

/// Install the platform's file dialogs and browser into `hooks`. `ui` is woken when a picker answers.
pub fn install(hooks: &mut HostHooks, root: &Path, ui: egui::Context) -> Result<(), String> {
    crate::documents::install(hooks, root.to_path_buf(), ui)?;
    crate::browser::install(hooks);
    Ok(())
}

/// The app's support folder, created on first use.
pub fn root() -> Result<PathBuf, String> {
    support_directory()
}
