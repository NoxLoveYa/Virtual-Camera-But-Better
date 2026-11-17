//! The native "open file" dialog for the `pick_file` command (plan 5.5): an `IFileOpenDialog`, owned by the app
//! window, run on its own thread so the webview keeps drawing and answering while it is open.

use windows::core::{HSTRING, HRESULT, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
    SIGDN_FILESYSPATH,
};

/// What the file is for: picks the filter list and the title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickKind {
    /// A picture: PNG, JPEG, BMP, GIF.
    Image,
    /// A colour lookup table: `.cube` or `.png`.
    Lut,
}

impl PickKind {
    /// The `kind` argument of `pick_file`: `"image"` or `"lut"`.
    pub fn parse(kind: &str) -> Result<PickKind, String> {
        match kind {
            "image" => Ok(PickKind::Image),
            "lut" => Ok(PickKind::Lut),
            other => Err(format!("unknown file kind \"{other}\" (expected \"image\" or \"lut\")")),
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            PickKind::Image => "Choose a picture",
            PickKind::Lut => "Choose a color lookup table",
        }
    }

    /// The file type list as (name, pattern) pairs: the kind's own filter first, then every file.
    pub fn filters(self) -> [(&'static str, &'static str); 2] {
        match self {
            PickKind::Image => [
                ("Images (*.png;*.jpg;*.jpeg;*.bmp;*.gif)", "*.png;*.jpg;*.jpeg;*.bmp;*.gif"),
                ("All files (*.*)", "*.*"),
            ],
            PickKind::Lut => [
                ("LUT files (*.cube;*.png)", "*.cube;*.png"),
                ("All files (*.*)", "*.*"),
            ],
        }
    }
}

/// `HRESULT_FROM_WIN32(ERROR_CANCELLED)`: what `Show` returns when the user closes the dialog.
const CANCELLED: HRESULT = HRESULT(0x8007_04C7_u32 as i32);

/// Show the dialog and wait for it: the full path of the chosen file, or `None` when the user cancelled. `owner` is
/// the window handle (`HWND` as an integer) the dialog belongs to, `None` for no owner. Blocks the calling thread
/// for as long as the dialog is open, so call it from a worker thread; the dialog itself runs on a thread of its
/// own with a single-threaded COM apartment, which the shell dialogs need.
pub fn pick_file(kind: PickKind, owner: Option<isize>) -> Result<Option<String>, String> {
    std::thread::Builder::new()
        .name("file dialog".into())
        .spawn(move || run_dialog(kind, owner))
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "the file dialog crashed".to_string())?
}

fn run_dialog(kind: PickKind, owner: Option<isize>) -> Result<Option<String>, String> {
    // SAFETY: plain COM calls on this thread; the strings the filter list points to live until `Show` returned.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| e.message())?;
        let result = show(kind, owner);
        CoUninitialize();
        result
    }
}

/// # Safety
/// COM must be initialized on this thread.
unsafe fn show(kind: PickKind, owner: Option<isize>) -> Result<Option<String>, String> {
    let fail = |e: windows::core::Error| format!("{} (0x{:08X})", e.message().trim(), e.code().0 as u32);
    let dialog: IFileOpenDialog =
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(fail)?;

    let names: Vec<(HSTRING, HSTRING)> = kind
        .filters()
        .iter()
        .map(|(n, p)| (HSTRING::from(*n), HSTRING::from(*p)))
        .collect();
    let specs: Vec<COMDLG_FILTERSPEC> = names
        .iter()
        .map(|(n, p)| COMDLG_FILTERSPEC {
            pszName: PCWSTR(n.as_ptr()),
            pszSpec: PCWSTR(p.as_ptr()),
        })
        .collect();
    dialog.SetFileTypes(&specs).map_err(fail)?;
    dialog.SetFileTypeIndex(1).map_err(fail)?;
    dialog.SetTitle(&HSTRING::from(kind.title())).map_err(fail)?;
    let options = dialog.GetOptions().map_err(fail)?;
    dialog
        .SetOptions(options | FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST)
        .map_err(fail)?;

    let hwnd = owner.map(|h| HWND(h as *mut core::ffi::c_void));
    match dialog.Show(hwnd) {
        Ok(()) => {}
        Err(e) if e.code() == CANCELLED => return Ok(None),
        Err(e) => return Err(fail(e)),
    }
    let item = dialog.GetResult().map_err(fail)?;
    let path = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(fail)?;
    let text = path.to_string().map_err(|e| e.to_string());
    CoTaskMemFree(Some(path.0 as *const core::ffi::c_void));
    text.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse_and_have_their_filters() {
        assert_eq!(PickKind::parse("image"), Ok(PickKind::Image));
        assert_eq!(PickKind::parse("lut"), Ok(PickKind::Lut));
        assert!(PickKind::parse("movie").unwrap_err().contains("movie"));
        assert!(PickKind::parse("").is_err());

        let f = PickKind::Image.filters();
        assert_eq!(f[0].1, "*.png;*.jpg;*.jpeg;*.bmp;*.gif");
        assert!(f[0].0.starts_with("Images"));
        let f = PickKind::Lut.filters();
        assert_eq!(f[0].1, "*.cube;*.png");
        assert!(f[0].0.starts_with("LUT files"));
        for kind in [PickKind::Image, PickKind::Lut] {
            let f = kind.filters();
            assert_eq!(f[1], ("All files (*.*)", "*.*"));
        }
    }
}
