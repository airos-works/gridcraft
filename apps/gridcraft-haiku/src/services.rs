//! The app's system services on air/OS: Open and Save panels (`BFilePanel`), the clipboard
//! (`BClipboard`: text, HTML and pictures), printing (the Be print kit, `print`), links, and making GridCraft the preferred application for
//! spreadsheets (`BMimeType`).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};

use gridcraft_ui_egui::{Services, printing};

use crate::host::{self, RawEvent, kind};
use crate::print;

/// The application's signature (also in `GridCraft.rdef`).
pub const SIGNATURE: &str = "application/x-vnd.storytold-GridCraft";

/// The spreadsheet types GridCraft opens: it becomes their preferred application on start where
/// none is set ([`claim_document_types`]), so a double-click in Tracker opens them.
const DOCUMENT_TYPES: &[&str] = &[
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.ms-excel.sheet.macroEnabled.12",
    "application/vnd.ms-excel.sheet.binary.macroEnabled.12",
    "application/vnd.ms-excel",
    "application/vnd.oasis.opendocument.spreadsheet",
    "text/csv",
    "text/tab-separated-values",
];

/// What File › Open offers (the desktop app's filter).
const OPEN_EXTENSIONS: &[&str] = &["xlsx", "xlsm", "xls", "xlsb", "ods", "csv", "tsv", "txt", "json"];
/// What Insert › Pictures offers.
const PICTURE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg"];
/// The types a Save panel's suggested name may carry; anything else saves as a workbook.
const SAVE_EXTENSIONS: &[&str] = &["xlsx", "csv", "html", "pdf"];

/// Makes GridCraft the preferred application of the spreadsheet types that have none.
pub fn claim_document_types() {
    let claimed = host::claim_types(SIGNATURE, DOCUMENT_TYPES);
    if claimed > 0 {
        log::info!("preferred application for {claimed} spreadsheet types that had none");
    }
}

/// The file panels. The app asks for a path and waits for it (`Services::pick_open` returns
/// one), so the interface thread waits for the panel's answer; whatever else the window sends
/// meanwhile is kept for the shell, in order.
#[derive(Default)]
pub struct Dialogs {
    next: Cell<i32>,
    /// Events that arrived while a panel was open.
    pub deferred: RefCell<VecDeque<(RawEvent, String)>>,
    /// Paths picked in Save panels, for the shell to type once written.
    pub saved: RefCell<Vec<PathBuf>>,
}

impl Dialogs {
    fn pick(&self, save: bool, name: &str, extensions: &[&str], title: &str) -> Option<String> {
        let id = self.next.get().wrapping_add(1);
        self.next.set(id);
        host::file_panel(id, save, name, extensions, &format!("GridCraft: {title}"));
        loop {
            host::wait(None);
            while let Some((ev, text)) = host::next_event() {
                if ev.kind == kind::DIALOG_ANSWER && ev.id == id {
                    let picked = ev.x != 0.0 && !text.is_empty();
                    if picked && save {
                        self.saved.borrow_mut().push(PathBuf::from(&text));
                    }
                    return picked.then_some(text);
                }
                self.deferred.borrow_mut().push_back((ev, text));
            }
        }
    }
}

/// The Save panel's name: the suggested one, with a type GridCraft writes.
fn save_name(suggested: &str) -> String {
    let path = std::path::Path::new(suggested);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Book1");
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).filter(|e| SAVE_EXTENSIONS.contains(&e.as_str()));
    format!("{stem}.{}", ext.as_deref().unwrap_or("xlsx"))
}

fn clip_text(mime: &str) -> Option<String> {
    let bytes = host::clipboard_get(mime)?;
    let s = String::from_utf8_lossy(&bytes).trim_end_matches('\0').to_string();
    (!s.is_empty()).then_some(s)
}

/// The clipboard's plain text.
pub fn clipboard_text() -> Option<String> {
    clip_text("text/plain")
}

/// The clipboard's picture (PNG or JPEG), for Paste when it holds no text.
pub fn clipboard_image() -> Option<image::RgbaImage> {
    ["image/png", "image/jpeg"].iter().find_map(|mime| {
        let bytes = host::clipboard_get(mime)?;
        image::load_from_memory(&bytes).map_err(|e| log::warn!("clipboard {mime}: {e}")).ok().map(|i| i.to_rgba8())
    })
}

/// Opens an http, https or mailto link with its preferred application (the desktop app's
/// rule: the system never gets anything else).
fn open_link(target: &str) {
    let gridcraft_engine::links::LinkKind::External(target) = gridcraft_engine::links::classify(target) else {
        log::warn!("refused to open {target:?}: only http, https and mailto links are opened");
        return;
    };
    if !host::open_url(&target) {
        log::warn!("couldn't open {target}");
    }
}

/// The window's title (the active workbook's), which names print jobs in the print server's queue.
static TITLE: Mutex<String> = Mutex::new(String::new());

/// Notes the window's title, for the next print job's name.
pub fn set_title(title: &str) {
    if let Ok(mut t) = TITLE.lock() {
        title.clone_into(&mut t);
    }
}

/// File › Print's printer list: the print server's printers, found on a worker.
fn discover_printers() -> Receiver<Vec<printing::Printer>> {
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new().name("print discovery".into()).spawn(move || {
        let printers = print::printers().into_iter().map(|p| printing::Printer { name: p.name, label: String::new(), default: p.default });
        let _ = tx.send(printers.collect());
    });
    if let Err(e) = spawned {
        log::error!("starting printer discovery: {e}");
    }
    rx
}

/// File › Print's Print: the print-ready PDF drawn into a `BPrintJob` for the chosen printer on
/// a worker. Shift+Print (or `GRIDCRAFT_PRINT_SETUP=1`) opens the printer driver's page setup and
/// job setup first (paper, resolution, the driver's own options); their answers are kept for that
/// printer in `~/config/settings/GridCraft/printers` and used by every later job on it.
fn submit_print(pdf: Vec<u8>, settings: printing::Settings) -> Receiver<Result<String, String>> {
    let (tx, rx) = channel();
    let title = TITLE.lock().map(|t| t.clone()).unwrap_or_default();
    let title = title.strip_suffix(" — Edited").unwrap_or(&title).trim();
    let options = print::Options {
        title: if title.is_empty() { "GridCraft".to_owned() } else { title.to_owned() },
        copies: settings.copies.clamp(1, 999),
        collate: true,
        duplex: settings.duplex.map(|d| match d {
            printing::Duplex::Off => print::Duplex::Off,
            printing::Duplex::LongEdge => print::Duplex::LongEdge,
            printing::Duplex::ShortEdge => print::Duplex::ShortEdge,
        }),
        grayscale: settings.grayscale,
        // libprint's PostScript and PCL drivers start in monochrome: a workbook prints in colour
        // unless Grayscale or the printer's job setup says otherwise.
        color: true,
        // Each sheet's page setup decides its pages' orientation.
        landscape: None,
        setup: print::shift_down() || std::env::var_os("GRIDCRAFT_PRINT_SETUP").is_some_and(|v| v != "0"),
        pages: None,
    };
    let store = print::Store::new(crate::config_dir().map(|d| d.join("printers")));
    let spawned = std::thread::Builder::new().name("print".into()).spawn(move || {
        let started = std::time::Instant::now();
        let result = print::print_pdf(Some(&settings.printer), pdf, &options, &store).map_err(|e| e.to_string());
        match &result {
            Ok(receipt) => log::info!("printed on {} in {:.1} s: {receipt}", settings.printer, started.elapsed().as_secs_f32()),
            Err(e) => log::warn!("printing on {}: {e}", settings.printer),
        }
        let _ = tx.send(result);
    });
    if let Err(e) = spawned {
        log::error!("starting the print worker: {e}");
    }
    rx
}

/// The services the app gets on air/OS.
pub fn services(dialogs: Rc<Dialogs>) -> Services {
    let (open, picture, save) = (dialogs.clone(), dialogs.clone(), dialogs);
    Services {
        copy_html: Some(Box::new(
            |html, text| if host::clipboard_set(text, Some(html)) { Ok(()) } else { Err("the clipboard is unavailable".into()) },
        )),
        paste_html: Some(Box::new(|| clip_text("text/html"))),
        paste_text: Some(Box::new(clipboard_text)),
        paste_image: Some(Box::new(clipboard_image)),
        pick_open: Some(Box::new(move || open.pick(false, "", OPEN_EXTENSIONS, "Open"))),
        pick_picture: Some(Box::new(move || picture.pick(false, "", PICTURE_EXTENSIONS, "Insert Picture"))),
        pick_save: Some(Box::new(move |suggested: &str| save.pick(true, &save_name(suggested), &[], "Save As"))),
        open_url: Some(Box::new(open_link)),
        printers: Some(Box::new(discover_printers)),
        print_native: Some(Box::new(submit_print)),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::save_name;

    #[test]
    fn save_names_keep_a_written_type() {
        assert_eq!(save_name("Book1"), "Book1.xlsx");
        assert_eq!(save_name("/boot/home/Budget.xlsm"), "Budget.xlsx");
        assert_eq!(save_name("Report.PDF"), "Report.pdf");
        assert_eq!(save_name("data.csv"), "data.csv");
    }
}
