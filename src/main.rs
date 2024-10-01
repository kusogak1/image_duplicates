#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use {
    color_eyre::eyre::Result,
    eframe::egui::{self, mutex::Mutex, ProgressBar},
    lazy_static::lazy_static,
    std::{
        path::Path,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        thread::available_parallelism,
    },
    ui::{create_folder_selection_block, create_progress_bar_item, create_result_display, create_result_items, create_scanning_item},
};

mod chunk;
mod data;
mod ui;
mod util;

const APPLICATION_NAME: &str = "Image duplicate finder";
const UI_WINDOW_WIDTH: f32 = 1280.0;
const UI_WINDOW_HEIGHT: f32 = 1000.0;
const UI_SCALING_FACTOR: f32 = 1.5;
const SCANNING_DATA_FILE_NAME: &str = "image_duplicates.scan.dat";
const ANALYSING_DATA_FILE_NAME: &str = "image_duplicates.corr.dat";
/// Folder within the scanned folder's root, to move the duplicates too
///
/// this folder will be ignored when scanning for files, so the files are
///
/// not deleted but ignored for further scans.
const POTENTIAL_DUPLICATES_FOLDER: &str = "potential_duplicates";
/// How many combinations are calculated at once. For large folders with many thousands of files, this will split the work into chunks.
///
/// If there is no chunking, a large amount of files will crash the program caused be out of memory (OOM).
const ANALYSIS_CHUNK_SIZE: i32 = 5_000_000;
/// Will split the image into 4x4 tiles
const TILE_COUNT_PER_SIDE: u32 = 4;
/// Not worth saving if the resemblance is too low, it will only slow down the process and generate unnecessary data.
const CORRELATION_THRESHOLD: f32 = 0.95;
/// How many different root scan folders can be used
const MAX_FOLDER_SCANS: usize = 3;

#[derive(Debug, PartialEq)]
enum Direction {
    Forwards,
    Backwards,
    None,
}

lazy_static! {
    // TODO: Move arc mutexes from MyApp to here.
    static ref UNDO_LIST: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    static ref AUTO_FORWARD_DIRECTION: Arc<Mutex<Direction>> = Arc::new(Mutex::new(Direction::None));
    static ref MAX_WORKERS: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
}

fn main() -> Result<()> {
    color_eyre::install()?;

    {
        // Setting the max workers during the start of the application.
        MAX_WORKERS.store(available_parallelism().unwrap().get(), Ordering::Relaxed);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([UI_WINDOW_WIDTH, UI_WINDOW_HEIGHT]).with_resizable(true).with_maximize_button(false),
        ..Default::default()
    };
    let _ = eframe::run_native(
        APPLICATION_NAME,
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::<ImageDuplicatesApp>::default())
        }),
    );
    Ok(())
}

#[derive(Default)]
struct ImageDuplicatesApp {
    folder_paths: Vec<String>,
    progress: Arc<Mutex<f64>>,
    recursive_paths: Vec<bool>,
    scanning: Arc<AtomicBool>,
    analysing: Arc<AtomicBool>,
    time_remaining: Arc<Mutex<String>>,
    display_images: Arc<AtomicBool>,
    correlation_entry_idx: Arc<AtomicUsize>,
    correlation_data: Arc<Mutex<Vec<data::CorrelationEntry>>>,
    first_path: Arc<Mutex<String>>,
    second_path: Arc<Mutex<String>>,
    clicked_on_image: Arc<Mutex<String>>,
    workers: u16,
}

impl eframe::App for ImageDuplicatesApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.set_pixels_per_point(UI_SCALING_FACTOR);
        let progress_bar = ProgressBar::new(*self.progress.lock() as f32).show_percentage();
        let time_remain_scan = Arc::clone(&self.time_remaining);
        let time_remain_analyse = Arc::clone(&self.time_remaining);
        let image_clicked = Arc::clone(&self.clicked_on_image);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label("Select a folder to be scanned. You can select up to 3 folders.");

            if self.folder_paths.len() < MAX_FOLDER_SCANS {
                let btn = ui.button("Select folder…");
                if btn.clicked() && self.folder_paths.len() < MAX_FOLDER_SCANS {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.folder_paths.push(path.display().to_string());
                        self.recursive_paths.push(false); // init new value in recursive vector
                    }
                }
            }

            let mut can_analyse = false;
            let mut can_show_results = false;

            if !self.folder_paths.is_empty() {
                create_folder_selection_block(self, ui);
                let all_exists: Vec<bool> = self.folder_paths.iter().map(|path| Path::new(format!("{path}/{SCANNING_DATA_FILE_NAME}").as_str()).exists()).collect();
                if all_exists.iter().filter(|b| **b).count() == self.folder_paths.len() {
                    ui.label("All databaeses exist.");
                    can_analyse = true;
                }
                create_scanning_item(self, ui, &mut can_analyse, time_remain_scan, time_remain_analyse);
                create_progress_bar_item(self, ui, progress_bar);

                if self.folder_paths.iter().filter(|path| Path::new(&format!("{path}/{ANALYSING_DATA_FILE_NAME}")).exists()).count() == self.folder_paths.len() {
                    // ui.label("Analysis data found.");
                    can_show_results = true;
                }

                create_result_items(self, ui, can_analyse, can_show_results);
                create_result_display(self, ui, &image_clicked);
            }
        });

        // Update ui continuously
        ctx.request_repaint();
    }
}

#[cfg(test)]
mod test {
    use {
        crate::{data, util::shorten_string},
        pretty_assertions::assert_eq,
    };

    #[test]
    fn test_corr() {
        #![allow(clippy::float_cmp)]
        let one: Vec<u8> = vec![1, 2, 3, 4, 5, 6];
        let two: Vec<u8> = vec![2, 4, 7, 9, 12, 14];
        let result = data::CorrelationEntry::correlation_coefficient(&one, &two);
        assert_eq!(0.998_381_5, result);

        let one: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9];
        let two: Vec<u8> = vec![9, 8, 7, 6, 5, 4, 3, 2, 1];
        let result = data::CorrelationEntry::correlation_coefficient(&one, &two);
        assert_eq!(1.0, result);
    }

    #[test]
    fn test_str_short() {
        let s = "112233445566778899";
        let r = shorten_string(s, 20);
        assert_eq!(s, &r);
        let r = shorten_string(s, 5);
        assert_eq!("11…99", &r);
        let r = shorten_string(s, 6);
        assert_eq!("112…99", &r);
        let r = shorten_string(s, 7);
        assert_eq!("112…899", &r);
    }
}
