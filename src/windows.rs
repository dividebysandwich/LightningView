#![allow(dead_code)]
use const_format::concatcp;
use std::{
    error::Error,
    io,
    path::PathBuf,
};
use winreg::{enums::*, RegKey};

use crate::formats::{IMAGE_RS_SUPPORTED_FORMATS, FITS_SUPPORTED_FORMATS, IMAGEREADER_SUPPORTED_FORMATS, RAW_SUPPORTED_FORMATS};
const CANONICAL_NAME: &str = "lightningview.exe";
// Must match the ProgId the WiX installer registers (wix/main.wxs). Using the same
// ProgId for the manual `/register` path and the installer prevents two different
// handlers — and therefore two "Open with" entries — for the same app.
const PROGID: &str = "LightningView.Image.1";
// Pre-2.5.10 Rust-only ProgId. Older versions registered under this name, which then
// showed up as a *second* "lightningview" entry next to LightningView.Image.1. We
// delete it during register/cleanup so the duplicate disappears.
const LEGACY_PROGID: &str = "LightningViewImageFile";

// Configuration for "Default Programs". StartMenuInternet is the key for browsers
// and they're expected to use the name of the exe as the key.
const DPROG_PATH: &str = concatcp!(r"SOFTWARE\Clients\StartMenuInternet\", CANONICAL_NAME);
const DPROG_INSTALLINFO_PATH: &str = concatcp!(DPROG_PATH, r"\InstallInfo");

const APPREG_BASE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\";
const PROGID_PATH: &str = concatcp!(r"SOFTWARE\Classes\", PROGID);
const LEGACY_PROGID_PATH: &str = concatcp!(r"SOFTWARE\Classes\", LEGACY_PROGID);
const REGISTERED_APPLICATIONS_KEY: &str = r"SOFTWARE\RegisteredApplications";

const DISPLAY_NAME: &str = "Lightning View Image Viewer";
const DESCRIPTION: &str = "Simple No-Fuss image viewer and browser";

/// Retrieve an EXE path by looking in the registry for the App Paths entry
fn get_exe_path(exe_name: &str) -> Result<PathBuf, Box<dyn Error>> {
    for root_name in &[HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let root = RegKey::predef(*root_name);
        if let Ok(subkey) = root.open_subkey(format!("{}{}", APPREG_BASE, exe_name)) {
            if let Ok(value) = subkey.get_value::<String, _>("") {
                let path = PathBuf::from(value);
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
    }

    Err(Box::new(io::Error::new(
        io::ErrorKind::NotFound,
        format!("Could not find path for {}", exe_name),
    )))
}

/// Register associations with Windows for being a browser
pub fn register_urlhandler() -> io::Result<()> {
    // This is used both by initial registration and OS-invoked reinstallation.
    // The expectations for the latter are documented here: https://docs.microsoft.com/en-us/windows/win32/shell/reg-middleware-apps#the-reinstall-command
    use std::env::current_exe;

    let exe_path = current_exe()?;
    let exe_name = exe_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_owned();

    let exe_path = exe_path.to_str().unwrap_or_default().to_owned();
    let icon_path = format!("\"{}\",0", exe_path);
    let open_command = format!("\"{}\" \"%1\"", exe_path);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    // Drop the legacy duplicate ProgId before (re)writing our registration, so an
    // upgrade from an older version doesn't leave a second handler behind.
    let _ = hkcu.delete_subkey_all(LEGACY_PROGID_PATH);

    // Configure our ProgID to point to the right command
    {
        let (progid_class, _) = hkcu.create_subkey(PROGID_PATH)?;
        progid_class.set_value("", &DISPLAY_NAME)?;

        let (progid_class_defaulticon, _) = progid_class.create_subkey("DefaultIcon")?;
        progid_class_defaulticon.set_value("", &icon_path)?;

        let (progid_class_shell_open_command, _) =
            progid_class.create_subkey(r"shell\open\command")?;
        progid_class_shell_open_command.set_value("", &open_command)?;
    }

    // Set up the Default Programs configuration for the app (https://docs.microsoft.com/en-us/windows/win32/shell/default-programs)
    {
        let (dprog, _) = hkcu.create_subkey(DPROG_PATH)?;
        dprog.set_value("", &DISPLAY_NAME)?;
        dprog.set_value("LocalizedString", &DISPLAY_NAME)?;

        let (dprog_capabilites, _) = dprog.create_subkey("Capabilities")?;
        dprog_capabilites.set_value("ApplicationName", &DISPLAY_NAME)?;
        dprog_capabilites.set_value("ApplicationIcon", &icon_path)?;
        dprog_capabilites.set_value("ApplicationDescription", &DESCRIPTION)?;

        let (dprog_capabilities_startmenu, _) = dprog_capabilites.create_subkey("Startmenu")?;
        dprog_capabilities_startmenu.set_value("StartMenuInternet", &CANONICAL_NAME)?;

        // Register for various file types, so that we'll be invoked for file:// URLs for these types (e.g.
        // by `cargo doc --open`.)
        let (dprog_capabilities_fileassociations, _) =
            dprog_capabilites.create_subkey("FileAssociations")?;

        let mut all_supported_formats: Vec<&str> = Vec::new();
        all_supported_formats.extend(&IMAGEREADER_SUPPORTED_FORMATS);
        all_supported_formats.extend(&IMAGE_RS_SUPPORTED_FORMATS);
        all_supported_formats.extend(&FITS_SUPPORTED_FORMATS);
        all_supported_formats.extend(&RAW_SUPPORTED_FORMATS);

        for filetype in all_supported_formats {
            dprog_capabilities_fileassociations.set_value(filetype, &PROGID)?;
        }

        let (dprog_defaulticon, _) = dprog.create_subkey("DefaultIcon")?;
        dprog_defaulticon.set_value("", &icon_path)?;

        // Set up reinstallation and show/hide icon commands (https://docs.microsoft.com/en-us/windows/win32/shell/reg-middleware-apps#registering-installation-information)
        let (dprog_installinfo, _) = dprog.create_subkey("InstallInfo")?;
        dprog_installinfo.set_value("ReinstallCommand", &format!("\"{}\" register", exe_path))?;
        dprog_installinfo.set_value("HideIconsCommand", &format!("\"{}\" hide-icons", exe_path))?;
        dprog_installinfo.set_value("ShowIconsCommand", &format!("\"{}\" show-icons", exe_path))?;

        // Only update IconsVisible if it hasn't been set already
        if dprog_installinfo
            .get_value::<u32, _>("IconsVisible")
            .is_err()
        {
            dprog_installinfo.set_value("IconsVisible", &1u32)?;
        }

        let (dprog_shell_open_command, _) = dprog.create_subkey(r"shell\open\command")?;
        dprog_shell_open_command.set_value("", &open_command)?;
    }

    // Set up a registered application for our Default Programs capabilities (https://docs.microsoft.com/en-us/windows/win32/shell/default-programs#registeredapplications)
    {
        let (registered_applications, _) =
            hkcu.create_subkey(r"SOFTWARE\RegisteredApplications")?;
        let dprog_capabilities_path = format!(r"{}\Capabilities", DPROG_PATH);
        registered_applications.set_value(DISPLAY_NAME, &dprog_capabilities_path)?;
    }

    // Application Registration (https://docs.microsoft.com/en-us/windows/win32/shell/app-registration)
    {
        let appreg_path = format!(r"{}{}", APPREG_BASE, exe_name);
        let (appreg, _) = hkcu.create_subkey(appreg_path)?;
        // This is used to resolve "lightningview.exe" -> full path, if needed.
        appreg.set_value("", &exe_path)?;
    }

    refresh_shell();

    Ok(())
}

fn refresh_shell() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_DWORD, SHCNF_FLUSH};

    // Notify the shell about the updated URL associations. (https://docs.microsoft.com/en-us/windows/win32/shell/default-programs#becoming-the-default-browser)
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_DWORD | SHCNF_FLUSH, None, None);
    }
}

/// Remove all the registry keys that we've set up
pub fn unregister_urlhandler() {
    cleanup_registrations();
}

/// Resolve the current executable's file name (e.g. "lightningview.exe").
fn current_exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| CANONICAL_NAME.to_string())
}

/// Remove every per-user (HKCU) registry entry this app — current or older
/// versions — may have created, including the legacy duplicate ProgId. A
/// machine-wide installer (HKLM) registration is left intact, so running this after
/// an MSI install leaves exactly one clean set of "Open with" / Default Programs
/// entries. Safe to call repeatedly and when nothing is registered.
pub fn cleanup_registrations() {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let exe_name = current_exe_name();

    // Current HKCU registration written by register_urlhandler().
    let _ = hkcu.delete_subkey_all(DPROG_PATH);
    let _ = hkcu.delete_subkey_all(PROGID_PATH);
    let _ = hkcu.delete_subkey_all(format!("{}{}", APPREG_BASE, exe_name));

    // Legacy Rust-only ProgId from versions before the ProgId was unified with the
    // installer — the source of the duplicate "lightningview" entry.
    let _ = hkcu.delete_subkey_all(LEGACY_PROGID_PATH);

    // RegisteredApplications entries are *values* under the key, not subkeys, so they
    // must be removed with delete_value (the previous delete_subkey was a no-op).
    if let Ok(registered_applications) =
        hkcu.open_subkey_with_flags(REGISTERED_APPLICATIONS_KEY, KEY_SET_VALUE)
    {
        let _ = registered_applications.delete_value(DISPLAY_NAME);
    }

    refresh_shell();
}

/// Set the "IconsVisible" flag to true (we don't have any icons)
fn show_icons() -> io::Result<()> {
    // The expectations for this are documented here: https://docs.microsoft.com/en-us/windows/win32/shell/reg-middleware-apps#the-show-icons-command
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (dprog_installinfo, _) = hkcu.create_subkey(DPROG_INSTALLINFO_PATH)?;
    dprog_installinfo.set_value("IconsVisible", &1u32)
}

/// Set the "IconsVisible" flag to false (we don't have any icons)
fn hide_icons() -> io::Result<()> {
    // The expectations for this are documented here: https://docs.microsoft.com/en-us/windows/win32/shell/reg-middleware-apps#the-hide-icons-command
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(dprog_installinfo) = hkcu.open_subkey(DPROG_INSTALLINFO_PATH) {
        dprog_installinfo.set_value("IconsVisible", &0u32)
    } else {
        Ok(())
    }
}

fn get_exe_relative_path(filename: &str) -> io::Result<PathBuf> {
    let mut path = std::env::current_exe()?;
    path.set_file_name(filename);
    Ok(path)
}


