#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

use {
    color_eyre::eyre::Result,
    eframe::egui::{
        self,
        mutex::{Mutex, MutexGuard},
        Color32, ProgressBar, RichText, Sense, Ui,
    },
    image::{io::Reader as ImageReader, GenericImageView},
    lazy_static::lazy_static,
    rayon::prelude::*,
    serde::{Deserialize, Serialize},
    std::{
        fs::{self, File, OpenOptions},
        io::{self, BufRead, Write},
        iter::zip,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::{self, available_parallelism},
        time::Instant,
    },
};

#[derive(Debug, PartialEq)]
enum Direction {
    Forwards,
    Backwards,
    None,
}

const APPLICATION_NAME: &str = "Image duplicate finder";
const UI_WINDOW_WIDTH: f32 = 1280.0;
const UI_WINDOW_HEIGHT: f32 = 1000.0;
const SCANNING_DATA_FILE_NAME: &str = "image_duplicates.scan.dat";
const ANALYSING_DATA_FILE_NAME: &str = "image_duplicates.corr.dat";
/// Folder within the scanned folder's root, to move the duplicates too
/// this folder will be ignored when scanning for files, so the files are
/// not deleted but ignored for further scans.
const POTENTIAL_DUPLICATES_FOLDER: &str = "potential_duplicates";
/// How many combinations are calculated at once. For large folders with many thousands of files, this will split the work into chunks.
/// If there is no chunking, a large amount of files will crash the program caused be out of memory (OOM).
const ANALYSIS_CHUNK_SIZE: i32 = 5_000_000;
/// Will split the image into 4x4 tiles
const TILE_COUNT_PER_SIDE: u32 = 4;
/// Not worth saving if the resemblance is too low, it will only slow down the process and generate unnecessary data.
const CORRELATION_THRESHOLD: f32 = 0.95;

lazy_static! {
// TODO: Move arc mutexes from MyApp to here.
static ref UNDO_LIST: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
static ref AUTO_FORWARD_DIRECTION: Arc<Mutex<Direction>> = Arc::new(Mutex::new(Direction::None));
static ref MAX_WORKERS: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
}

mod chunk;

fn main() -> Result<()> {
    color_eyre::install()?;
    // TODO: moving a file to duplicates folder also removes all lines regarding that file from the data files
    // TODO: add replace function to replace the smaller one with the bigger: [<-] & [->] buttons between images

    {
        // Setting the max workers during the start of the application.
        let mut max_workers = MAX_WORKERS.lock();
        *max_workers = available_parallelism().unwrap().get();
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([UI_WINDOW_WIDTH, UI_WINDOW_HEIGHT]).with_resizable(false).with_maximize_button(false),
        ..Default::default()
    };
    let _ = eframe::run_native(
        APPLICATION_NAME,
        options,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Box::<MyApp>::default()
        }),
    );
    Ok(())
}

#[derive(Default)]
struct MyApp {
    folder_path: Option<String>,
    progress: Arc<Mutex<f64>>,
    recursive: bool,
    scanning: Arc<AtomicBool>,
    analysing: Arc<AtomicBool>,
    time_remaining: Arc<Mutex<String>>,
    display_images: Arc<AtomicBool>,
    correlation_entry_idx: Arc<Mutex<usize>>,
    correlation_data: Arc<Mutex<Vec<CorrelationEntry>>>,
    first_path: Arc<Mutex<String>>,
    second_path: Arc<Mutex<String>>,
    clicked_on_image: Arc<Mutex<String>>,
    workers: u16,
}

trait DbPathProvider {
    fn db_path_name() -> &'static str;
}
impl DbPathProvider for DataEntry {
    fn db_path_name() -> &'static str {
        SCANNING_DATA_FILE_NAME
    }
}

impl DbPathProvider for CorrelationEntry {
    fn db_path_name() -> &'static str {
        ANALYSING_DATA_FILE_NAME
    }
}

fn get_db_path<T>(folder_path: &str) -> String
where
    T: DbPathProvider,
{
    format!("{folder_path}/{}", T::db_path_name())
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct DataEntry {
    filename: String,
    data: Vec<u8>,
}

impl DataEntry {
    /// Creates a new data entry, with the given name and the data slice.
    fn new(file_path: impl Into<String>, data: &[u8]) -> Self {
        Self { filename: file_path.into(), data: data.into() }
    }

    /// Generates a list of all potential image files (recursively), that are within the given folder.
    ///
    /// Will only check for jpg/jpeg & png files. `WebP is currently not supported.`
    fn generate_file_list(folder_path: &str, recursive: bool) -> Vec<String> {
        let mut file_list = Vec::new();
        visit_dirs(Path::new(folder_path), recursive, &mut |entry| {
            if let Some(extension) = entry.extension() {
                match if let Some(ext) = extension.to_ascii_lowercase().to_str() {
                    ext
                } else {
                    eprintln!("Could not get extention.");
                    ""
                } {
                    "png" | "jpg" | "jpeg" => {
                        if let Some(filepath) = entry.to_str() {
                            file_list.push(filepath.to_string());
                        }
                    }
                    _ => {}
                }
            }
        });
        file_list.sort();
        file_list
    }

    /// Will open the file at the given path and then return the data vector with the average tile brightness for each of the tiles.
    #[inline]
    fn calculate_image_tile_data(file_path: &str) -> Vec<u8> {
        let mut tiles_averages = Vec::<u8>::with_capacity(TILE_COUNT_PER_SIDE as usize);
        match ImageReader::open(file_path) {
            Ok(opened_image) => match opened_image.decode() {
                Ok(decoded_image) => {
                    let (width, height) = decoded_image.dimensions();
                    let rect_width = width / TILE_COUNT_PER_SIDE;
                    let rect_height = height / TILE_COUNT_PER_SIDE;
                    for i in 0..TILE_COUNT_PER_SIDE {
                        for j in 0..TILE_COUNT_PER_SIDE {
                            let x = i * rect_width;
                            let y = j * rect_height;
                            let tile = decoded_image.view(x, y, rect_width, rect_height).to_image();
                            let tile_pixel_sum: u64 = tile.par_iter().map(|&v| u64::from(v)).sum();
                            let tile_average = (tile_pixel_sum as f64 / (4.0 * f64::from(rect_width) * f64::from(rect_height))).round() as u8; // need to divide by 4, since we read RGBA
                            tiles_averages.push(tile_average);
                        }
                    }
                }
                Err(e) => eprintln!("Could not process image {file_path}, error: {e:?}"),
            },
            Err(e) => eprintln!("Could not process image {file_path}, error: {e:?}"),
        };
        tiles_averages
    }

    /// Reads the entries from a given folder, determining the correct name automatically.
    ///
    /// Returning a Result vec of all entries.
    fn read_from_folder(folder_path: &str) -> Result<Vec<Self>> {
        let target_file = get_db_path::<Self>(folder_path);
        let target_path = Path::new(&target_file);
        let file_handle = File::open(target_path)?;
        Ok(io::BufReader::new(file_handle)
            .lines()
            .filter_map(|line_result| match line_result {
                Ok(line) if !line.is_empty() => Some(line.clone()),
                _ => None,
            })
            .map(|line| {
                let entry: DataEntry = serde_json::from_str(&line).expect("Could not extract entry from string");
                entry
            })
            .collect())
    }

    /// Extracts only the file names from the given data slice
    fn extract_filenames(entries: &[Self]) -> Vec<String> {
        entries.iter().map(|entry| entry.filename.clone()).collect()
    }

    /// Creates the database file in the given folder path to store the data.
    fn create_db(folder_path: &str) -> Result<()> {
        let target_file = get_db_path::<Self>(folder_path);
        let target_path = Path::new(&target_file);
        if !target_path.exists() {
            File::create(&target_file)?;
        }
        Ok(())
    }

    /// Will save the entry to the given file. Each entry will be put on a new line.
    ///
    /// New lines are automatically added.
    fn save(&self, mutex_file_path: &MutexGuard<File>) -> Result<()> {
        serde_json::to_writer(&**mutex_file_path, self)?;
        writeln!(&**mutex_file_path)?;
        Ok(())
    }

    /// Will check all the jpg/png files in the given folder (recursively).
    ///
    /// The data is then stored in the given folder.
    ///
    /// The progress and time remaining are automatically updated during the process. Worker count can be adjusted.
    fn scan(folder_path: &str, scanning: &Arc<AtomicBool>, recursive: bool, progress: &Arc<Mutex<f64>>, time_remaining: &Arc<Mutex<String>>, worker_count: u16) {
        println!("Starting {}recursive scanning", if recursive { "" } else { "non-" });
        let db_path = get_db_path::<Self>(folder_path);
        Self::create_db(folder_path).expect("Somehow the selected folder was deleted and is no longer available.");
        {
            // Reset from last run and drops it afterwards since it is no longer needed. The update will create a new lock.
            *progress.lock() = 0.0;
        }

        let files_to_scan_list = Self::generate_file_list(folder_path, recursive);
        println!("Found {} potential files for scanning.", files_to_scan_list.len());

        let existing_entries = Self::read_from_folder(folder_path).expect("Could not read entries from folder.");
        let existing_entries_filenames = Self::extract_filenames(&existing_entries);
        println!("Found {} entries already in in database.", existing_entries_filenames.len());

        // Filtering out all entries, that already exist in the database.
        let files_to_scan: Vec<String> = files_to_scan_list.iter().filter(|filename| !existing_entries_filenames.contains(*filename)).cloned().collect();

        if files_to_scan.is_empty() {
            println!("Nothing to do, all files have already been scanned!");
            scanning.store(false, Ordering::Relaxed);
        } else {
            print!("Scanning {} files, skipping already scanned files. ", files_to_scan.len());
            println!("Using {worker_count} workers");

            let files_processed = Arc::new(Mutex::new(0));
            let files_to_process = files_to_scan.len();
            let work_queue = Arc::new(Mutex::new(files_to_scan.clone()));

            let db_file = OpenOptions::new().append(true).open(&db_path).expect("Cannot open file");
            let db_file = Arc::new(Mutex::new(db_file));

            let start_time = Instant::now();
            crossbeam::scope(|scope| {
                for _worker in 0..worker_count {
                    let work_queue = Arc::clone(&work_queue);
                    let files_processed = Arc::clone(&files_processed);
                    let worker_db_file = Arc::clone(&db_file);

                    scope.spawn(move |_| {
                        'file_iteration_loop: while let Some(file) = {
                            let mut queue = work_queue.lock();
                            queue.pop()
                        } {
                            // Calculate data
                            if Path::new(&file).exists() {
                                let averages = Self::calculate_image_tile_data(&file);
                                let entry = DataEntry::new(&file, &averages);
                                {
                                    let worker_db_file = worker_db_file.lock();
                                    // TODO: do we want to stop if the saving fails? Should not fail under normal conditions.
                                    let _ = entry.save(&worker_db_file);
                                }
                            } else {
                                // We simply continue and ignore the incident.
                                eprintln!("File {file} has been removed and can not be read. Continuing.");
                            }
                            {
                                *files_processed.lock() += 1;
                            }

                            let processed = *files_processed.lock();
                            let avg_time_per_file = start_time.elapsed() / (processed + 1) as u32;

                            let remaining_files = files_to_process - processed;
                            let estimated_remaining_time = avg_time_per_file * remaining_files as u32;
                            if processed % 10 == 0 {
                                let mut time_remaining = time_remaining.lock();
                                *time_remaining = format!("Estimated: {estimated_remaining_time:06.2?} remaining.");
                            }

                            {
                                // Update progress
                                let mut progress_percent = progress.lock();
                                let processed = *files_processed.lock();
                                *progress_percent = (1.0 + processed as f64) / files_to_process as f64;
                            }

                            if !scanning.load(Ordering::Relaxed) {
                                break 'file_iteration_loop;
                            }
                        }
                    });
                }
            })
            .unwrap();

            scanning.store(false, Ordering::Relaxed);

            let elapsed = start_time.elapsed().as_millis();
            println!("Scanning took: {elapsed} ms.");
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct CorrelationEntry {
    first_path: String,
    second_path: String,
    corr: f32,
}

impl CorrelationEntry {
    fn new(first_path: impl Into<String>, second_path: impl Into<String>, corr: f32) -> Self {
        Self { first_path: first_path.into(), second_path: second_path.into(), corr }
    }

    fn get_correlation_db_path(folder_path: &str) -> String {
        format!("{folder_path}/{ANALYSING_DATA_FILE_NAME}")
    }

    fn create_correlation_db(folder_path: &str) -> Result<()> {
        let target_file = Self::get_correlation_db_path(folder_path);
        let target_path = Path::new(&target_file);
        if !target_path.exists() {
            File::create(&target_file)?;
        }
        Ok(())
    }

    fn read_correlation_db(folder_path: &str) -> Result<Vec<Self>, Box<dyn std::error::Error>> {
        let file_name = Self::get_correlation_db_path(folder_path);
        let file_handle = File::open(file_name)?;
        let mut result: Vec<Self> = io::BufReader::new(file_handle)
            .lines()
            .map(|line| line.and_then(|line_str| serde_json::from_str(&line_str).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))))
            .collect::<Result<Vec<Self>, _>>()?;
        result.sort_by(|a, b| b.corr.partial_cmp(&a.corr).unwrap_or(std::cmp::Ordering::Equal));
        Ok(result)
    }

    fn save(&self, mutex_file_path: &MutexGuard<File>) -> Result<()> {
        serde_json::to_writer(&**mutex_file_path, self)?;
        writeln!(&**mutex_file_path)?;
        Ok(())
    }

    #[inline]
    fn correlation_coefficient(a: &[u8], b: &[u8]) -> f32 {
        // https://en.wikipedia.org/wiki/Pearson_correlation_coefficient
        if a.len() != b.len() {
            return 0.0;
        }
        let len_a = a.len() as f32;
        let len_b = b.len() as f32;
        // data is small, therefore making a copy is fast, type conversion is expensive too
        let a_f32: Vec<f32> = a.iter().map(|v| f32::from(*v)).collect();
        let b_f32: Vec<f32> = b.iter().map(|v| f32::from(*v)).collect();
        let mean_a: f32 = a_f32.iter().sum::<f32>() / len_a;
        let mean_b: f32 = b_f32.iter().sum::<f32>() / len_b;
        let var_a: f32 = a_f32.iter().map(|v| (v - mean_a).powi(2)).sum::<f32>() / (len_a - 1.0);
        let var_b: f32 = b_f32.iter().map(|v| (v - mean_b).powi(2)).sum::<f32>() / (len_b - 1.0);
        if var_a == 0.0 || var_b == 0.0 {
            return 0.0;
        }

        let cov = zip(a_f32, b_f32).map(|(a, b)| (a - mean_a) * (b - mean_b)).sum::<f32>() / (len_a - 1.0);
        (cov / (var_a * var_b).sqrt()).abs() // we are only interested in the absolute value
    }

    fn analyse(folder_path: &str, analysing: &Arc<AtomicBool>, progress: &Arc<Mutex<f64>>, time_remaining: &Arc<Mutex<String>>, worker_count: u16) {
        println!("Using {worker_count} workers.");
        println!("Analysing database in {folder_path}. Calculating correlation coefficients for entries.");
        Self::create_correlation_db(folder_path).expect("TODO: panic message");
        let db_path = get_db_path::<Self>(folder_path);

        let data_entries = DataEntry::read_from_folder(folder_path).expect("Could not read entries from folder.");
        let file_names = DataEntry::extract_filenames(&data_entries);
        println!("Found {} entries for analysis", data_entries.len());

        let total_combinations = file_names.len() * (file_names.len() - 1) / 2;
        let chunk_count = (total_combinations as f64 / f64::from(ANALYSIS_CHUNK_SIZE)).ceil() as u64;

        println!("Expecting {chunk_count} chunks of {ANALYSIS_CHUNK_SIZE} combinations for the total of {total_combinations} combinations.");
        println!("Loading data...");

        let mut chunk_counter = 0;
        let mut chunked_combinations = chunk::ChunkedCombinations::new(&file_names, 2, ANALYSIS_CHUNK_SIZE.try_into().unwrap());

        let outer_start_time = Instant::now();
        #[allow(clippy::while_let_on_iterator)]
        while let Some(chunk) = chunked_combinations.next() {
            {
                // Reset from last run
                *progress.lock() = 0.0;
            }

            let start_time = Instant::now();

            let combinations_data: Vec<(String, Vec<u8>, String, Vec<u8>)> = chunk
                .par_iter()
                .map(|combination| {
                    let first = combination.first().unwrap().to_owned();
                    let second = combination.last().unwrap().to_owned();
                    let first_data = data_entries.iter().find(|e| e.filename == *first).unwrap().data.clone();
                    let second_data = data_entries.iter().find(|e| e.filename == *second).unwrap().data.clone();
                    (first.clone(), first_data, second.clone(), second_data)
                })
                .collect();

            println!("\rLoading data took: {} seconds.", start_time.elapsed().as_secs());

            let start_time = Instant::now();
            let already_performed = CorrelationEntry::read_correlation_db(&db_path).unwrap_or_default();
            dbg!(&already_performed.len()); // TODO: debug why this is always 0
            let combinations_data_filtered: Vec<(String, Vec<u8>, String, Vec<u8>)> = if already_performed.is_empty() {
                combinations_data
            } else {
                combinations_data.par_iter().filter(|(first, _, second, _)| !already_performed.iter().any(|e| e.first_path == *first && e.second_path == *second)).cloned().collect()
            };
            println!("Filtering data took: {} seconds.", start_time.elapsed().as_secs());

            let combinations_to_process = combinations_data_filtered.len();
            println!("Processing {combinations_to_process} combinations, using {worker_count} workers.");

            let combinations_processed = Arc::new(Mutex::new(0));
            let work_queue = Arc::new(Mutex::new(combinations_data_filtered.clone()));

            let target_db_file_handle = OpenOptions::new().append(true).open(&db_path).expect("Cannot open file");
            let target_db_file_mutex = Arc::new(Mutex::new(target_db_file_handle));

            let start_time = Instant::now();
            crossbeam::scope(|scope| {
                for _worker in 0..worker_count {
                    let work_queue = Arc::clone(&work_queue);
                    let files_processed = Arc::clone(&combinations_processed);
                    let worker_db_file = Arc::clone(&target_db_file_mutex);

                    scope.spawn(move |_| {
                        'combinations_loop: while let Some((first, first_data, second, second_data)) = {
                            let mut work_item = work_queue.lock();
                            work_item.pop()
                        } {
                            let corr: f32 = Self::correlation_coefficient(&first_data, &second_data);
                            let entry = CorrelationEntry::new(&first, &second, corr);
                            {
                                if entry.corr > CORRELATION_THRESHOLD {
                                    let worker_db_file = &worker_db_file.lock();
                                    let _ = entry.save(worker_db_file);
                                }
                            }
                            {
                                *files_processed.lock() += 1;
                            }

                            let processed = *files_processed.lock();
                            let avg_time_per_file = start_time.elapsed() / (processed + 1);

                            let remaining_files = combinations_to_process - processed as usize;
                            let estimated_remaining_time = avg_time_per_file * remaining_files as u32;
                            if processed % 10 == 0 {
                                *time_remaining.lock() = format!(
                                    "Processing chunk {} of {chunk_count}. Processed {processed} of {combinations_to_process}. Estimated: {estimated_remaining_time:06.2?} remaining.",
                                    chunk_counter + 1
                                );
                            }
                            {
                                // Update progress
                                *progress.lock() = (1.0 + f64::from(*files_processed.lock())) / combinations_to_process as f64;
                            }
                            if !analysing.load(Ordering::Relaxed) {
                                break 'combinations_loop;
                            }
                        }
                    });
                }
            })
            .unwrap();

            let elapsed = start_time.elapsed().as_millis();
            println!("\nChunk Analysing took: {elapsed} ms");

            chunk_counter += 1;
            println!("Processed chunk {chunk_counter} of {chunk_count}");
        }
        analysing.store(false, Ordering::Relaxed);
        let elapsed = outer_start_time.elapsed().as_millis();
        println!("Analysis done! Analysing took: {elapsed} ms");
    }
}

fn visit_dirs(folder_path: &Path, recursive: bool, callback: &mut dyn FnMut(&PathBuf)) {
    if folder_path.is_dir() {
        // Do not check our created folder
        if folder_path.display().to_string().contains(POTENTIAL_DUPLICATES_FOLDER) {
            return;
        }
        for entry in fs::read_dir(folder_path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() && recursive {
                visit_dirs(&path, recursive, callback);
            } else {
                callback(&path);
            }
        }
    }
}

fn create_ui_image_component(ui: &mut Ui, file_path: &str, image: egui::Image, max_size: (f32, f32), to_delete: &Arc<Mutex<String>>, root_folder: &str) {
    let filename = Path::new(&file_path).file_name().unwrap().to_str().unwrap();
    let display_filename = shorten_string(filename, 40);
    let scaled_image = image.fit_to_original_size(1.0).max_width(max_size.0).max_height(max_size.1).sense(Sense::click());
    let duplicate_folder_path = format!("{root_folder}/{POTENTIAL_DUPLICATES_FOLDER}");
    ui.vertical(|ui| {
        match ImageReader::open(file_path) {
            Ok(image) => {
                let dim = image.into_dimensions().expect("Could not get dimensions");
                if ui.add(scaled_image).clicked() {
                    if !Path::new(&duplicate_folder_path).exists() {
                        let _ = fs::create_dir(&duplicate_folder_path);
                    }
                    {
                        let mut u_list = UNDO_LIST.lock();
                        u_list.push(file_path.to_string());
                    }
                    let _ = fs::rename(file_path, format!("{duplicate_folder_path}/{filename}"));
                    let mut binding = to_delete.lock();
                    *binding = file_path.to_string();
                }
                ui.label(display_filename);
                ui.label(format!("Parent folder: {}", Path::new(file_path).parent().unwrap().file_name().unwrap().to_str().unwrap()));
                ui.label(format!("Image dimensions: {}x{}", dim.0, dim.1));
            }
            Err(e) => {
                /*eprint!("Could not open image {e}")*/
                drop(e);
            }
        };
    });
}

impl eframe::App for MyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.set_pixels_per_point(1.5);
        let progress_bar = ProgressBar::new(*self.progress.lock() as f32).show_percentage();
        let time_remain_scan = Arc::clone(&self.time_remaining);
        let time_remain_analyse = Arc::clone(&self.time_remaining);
        let display_images = Arc::clone(&self.display_images);
        let correlation_entry_idx = Arc::clone(&self.correlation_entry_idx);
        let first_path = Arc::clone(&self.first_path);
        let second_path = Arc::clone(&self.second_path);
        let correlation_data = Arc::clone(&self.correlation_data);
        let image_clicked = Arc::clone(&self.clicked_on_image);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label("Select a folder to be scanned.");

            if ui.button("Select folder…").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.folder_path = Some(path.display().to_string());
                    // Set workers only initially, when its initialised with 0
                    // Since the slider is not displayed before a folder is selected, we can do this here.
                    if self.workers == 0 {
                        self.workers = *MAX_WORKERS.lock() as u16;
                    }
                }
            }

            if let Some(selected_folder) = &self.folder_path {
                let shortened_folder_path = shorten_string(selected_folder.as_str(), 60);
                let mut can_analyse = false;
                let mut can_show_results = false;
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("Selected folder:");
                        ui.monospace(shortened_folder_path);
                    });
                    ui.checkbox(&mut self.recursive, "Recursive scanning");
                    if Path::new(format!("{selected_folder}/{SCANNING_DATA_FILE_NAME}").as_str()).exists() {
                        // ui.label("Database found.");
                        can_analyse = true;
                    }
                    ui.horizontal(|ui| {
                        if self.scanning.load(Ordering::Relaxed) {
                            if ui.button("Stop scanning").clicked() {
                                self.scanning.store(false, Ordering::Relaxed);
                            }
                        } else if ui.button("Start scanning").clicked() {
                            let scanning = Arc::clone(&self.scanning);
                            scanning.store(true, Ordering::Relaxed);
                            let analysing = Arc::clone(&self.analysing);
                            analysing.store(false, Ordering::Relaxed); // cancel analysing if scanning is started
                            let folder = selected_folder.clone();
                            let recursive = self.recursive;
                            let prog = self.progress.clone();
                            let use_workers = self.workers;
                            thread::spawn(move || DataEntry::scan(&folder, &scanning, recursive, &prog.clone(), &time_remain_scan.clone(), use_workers));
                        }

                        if self.analysing.load(Ordering::Relaxed) {
                            if ui.button("Stop analysing").clicked() {
                                self.analysing.store(false, Ordering::Relaxed);
                            }
                        } else if can_analyse && ui.button("Analyse data").clicked() {
                            let analysing = Arc::clone(&self.analysing);
                            analysing.store(true, Ordering::Relaxed);
                            let scanning = Arc::clone(&self.scanning);
                            scanning.store(false, Ordering::Relaxed); // cancel scanning if analysing is started
                            let folder = selected_folder.clone();
                            let prog = self.progress.clone();
                            let use_workers = self.workers;
                            thread::spawn(move || CorrelationEntry::analyse(&folder, &analysing, &prog, &time_remain_analyse.clone(), use_workers));
                        }
                        let max_workers = MAX_WORKERS.lock();
                        ui.add(egui::Slider::new(&mut self.workers, 1..=*max_workers as u16).text("Max workers")).on_hover_ui(|ui| {
                            ui.label("Select the number of maximum workers to be used for scanning/analysing.");
                        });
                    });
                    {
                        let scanning = self.scanning.load(Ordering::Relaxed);
                        let analysing = self.analysing.load(Ordering::Relaxed);
                        if scanning || analysing {
                            ui.add(progress_bar);
                            let time_remaining = self.time_remaining.lock().clone();
                            if time_remaining.is_empty() {
                                ui.label("Remaining time unknown.");
                            } else {
                                ui.label(time_remaining);
                            }
                        }
                    }
                });
                let analysing = self.analysing.load(Ordering::Relaxed);
                let scanning = self.scanning.load(Ordering::Relaxed);

                if Path::new(format!("{selected_folder}/{ANALYSING_DATA_FILE_NAME}").as_str()).exists() {
                    // ui.label("Analysis data found.");
                    can_show_results = true;
                }

                if !analysing && !scanning && can_analyse {
                    if display_images.load(Ordering::Relaxed) {
                        if ui.button("Close results").clicked() {
                            correlation_data.lock().clear();
                            display_images.store(false, Ordering::Relaxed);
                        }
                    } else if can_show_results && ui.button("Show results").clicked() {
                        let mut c_data = correlation_data.lock();
                        *c_data = CorrelationEntry::read_correlation_db(selected_folder).unwrap().iter().filter(|e| e.corr > 0.9).cloned().collect();
                        display_images.store(true, Ordering::Relaxed);
                        {
                            let entry = &c_data.get(*correlation_entry_idx.lock()).unwrap();
                            {
                                let mut first_path = first_path.lock();
                                (*first_path).clone_from(&entry.first_path);
                            }
                            let mut second_path = second_path.lock();
                            (*second_path).clone_from(&entry.second_path);
                        }
                    }
                }

                if display_images.load(Ordering::Relaxed) {
                    ui.horizontal(|ui| {
                        let first_path = &mut self.first_path.lock();
                        let second_path = &mut self.second_path.lock();
                        if ui.button("Previous").clicked() {
                            let mut idx = correlation_entry_idx.lock();
                            let c_data = correlation_data.lock();

                            *AUTO_FORWARD_DIRECTION.lock() = Direction::Backwards;

                            if *idx > 0 {
                                *idx -= 1;
                            } else {
                                *idx = c_data.len() - 1;
                            }

                            {
                                let entry = c_data.get(*idx).expect("Somehow the index got out of bounds");
                                (*first_path).clone_from(&entry.first_path);
                                (*second_path).clone_from(&entry.second_path);
                            }
                        };
                        if ui.button("Next").clicked() {
                            let mut idx = correlation_entry_idx.lock();
                            let c_data = correlation_data.lock();

                            *AUTO_FORWARD_DIRECTION.lock() = Direction::Forwards;

                            if *idx < (c_data.len() - 1) {
                                *idx += 1;
                            } else {
                                *idx = 0;
                            }

                            {
                                let entry = c_data.get(*idx).expect("Somehow the index got out of bounds");
                                (*first_path).clone_from(&entry.first_path);
                                (*second_path).clone_from(&entry.second_path);
                            }
                        };
                        let first_path_exists = Path::new(&first_path.clone()).exists();
                        let second_path_exists = Path::new(&second_path.clone()).exists();
                        if !first_path_exists || !second_path_exists {
                            let mut direction = AUTO_FORWARD_DIRECTION.lock();
                            let mut idx = correlation_entry_idx.lock();
                            let c_data = correlation_data.lock();

                            if *direction == Direction::Forwards {
                                if *idx < (c_data.len() - 1) {
                                    *idx += 1;
                                } else {
                                    *idx = 0;
                                }
                            } else if *direction == Direction::Backwards {
                                if *idx > 0 {
                                    *idx -= 1;
                                } else {
                                    *idx = c_data.len() - 1;
                                }
                            } else {
                                *direction = Direction::Forwards;
                            }

                            {
                                let entry = c_data.get(*idx).expect("Somehow the index got out of bounds");
                                (*first_path).clone_from(&entry.first_path);
                                (*second_path).clone_from(&entry.second_path);
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Clicking on the images will move them into the \"potential_duplicates\" folder").color(Color32::RED));
                        {
                            let mut undo_list = UNDO_LIST.lock();
                            if !undo_list.is_empty() && ui.button("Restore last move").clicked() {
                                if let Some(original_path) = undo_list.pop() {
                                    let duplicate_file_name = Path::new(&original_path).file_name().unwrap().to_str().unwrap();
                                    let duplicate_file_path = format!("{selected_folder}/{POTENTIAL_DUPLICATES_FOLDER}/{duplicate_file_name}");
                                    match fs::rename(duplicate_file_path, &original_path) {
                                        Ok(()) => println!("Restored {original_path}"),
                                        Err(e) => {
                                            eprintln!("Failed to restore {original_path}. Error: {e}");
                                        }
                                    }
                                }
                            }
                        }
                    });

                    let first_path = &self.first_path.lock().clone();
                    let second_path = &self.second_path.lock().clone();

                    let image1 = egui::Image::new(format!("file://{}", &first_path));
                    let image2 = egui::Image::new(format!("file://{}", &second_path));

                    // TODO: hand over resolution to image function, and highlight higher resolution if not equal
                    let available_size = ui.available_size();
                    ui.horizontal(|ui| {
                        // Can not calculate the sizing inside, since it changes after the first image is inserted
                        create_ui_image_component(ui, first_path, image1, (available_size.x / 2.025, available_size.y / 1.3), &image_clicked, selected_folder);
                        create_ui_image_component(ui, second_path, image2, (available_size.x / 2.025, available_size.y / 1.3), &image_clicked, selected_folder);
                    });

                    {
                        let c_data = correlation_data.lock();
                        let corr = &c_data.get(*correlation_entry_idx.lock()).unwrap().corr;
                        ui.label(format!("Resemblance: {:.2}%", corr * 100.0));
                    }
                }
            }
        });
        // Update ui continuously
        ctx.request_repaint();
    }
}

// Will shorten the string, so that the middle part is not displayed if its too long
fn shorten_string(input_string: &str, max_length: usize) -> String {
    if input_string.len() <= max_length {
        input_string.to_string()
    } else {
        let input_len = input_string.len();
        let diff = input_len - max_length;
        let middle_idx = 1 + max_length / 2;
        let first_cutoff = &input_string[..middle_idx - 1];
        let second_cutoff = &input_string[middle_idx + diff..];
        let mut result = String::from(first_cutoff);
        result.push('…');
        result.push_str(second_cutoff);
        result
    }
}

#[cfg(test)]
mod test {
    use {
        crate::{shorten_string, CorrelationEntry},
        pretty_assertions::assert_eq,
    };

    #[test]
    fn test_corr() {
        #![allow(clippy::float_cmp)]
        let one: Vec<u8> = vec![1, 2, 3, 4, 5, 6];
        let two: Vec<u8> = vec![2, 4, 7, 9, 12, 14];
        let result = CorrelationEntry::correlation_coefficient(&one, &two);
        assert_eq!(0.998_381_5, result);
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
