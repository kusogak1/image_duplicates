use {
    crate::{
        data::{self, CorrelationEntry},
        util::{force_string_length, shorten_string},
        Direction, ImageDuplicatesApp, AUTO_FORWARD_DIRECTION, MAX_WORKERS, POTENTIAL_DUPLICATES_FOLDER, UI_SCALING_FACTOR, UNDO_LIST,
    },
    eframe::egui::{self, mutex::Mutex, Color32, InnerResponse, ProgressBar, RichText, Sense, Ui},
    std::{
        cmp::Ordering::{Equal, Greater, Less},
        fs,
        path::Path,
        sync::{atomic::Ordering, Arc},
        thread,
    },
};

pub(crate) fn create_folder_selection_block(app: &mut ImageDuplicatesApp, ui: &mut Ui) -> InnerResponse<()> {
    let longest_folder_name = app.folder_paths.iter().map(std::string::String::len).max().unwrap();

    ui.vertical(|ui| {
        for (idx, folder) in app.folder_paths.clone().iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label("Selected folder:");
                let shortened_folder_path = force_string_length(&folder.clone(), longest_folder_name.min(30));
                ui.label(
                    RichText::new(shortened_folder_path) /* .line_height(Some(16.0))*/
                        .monospace(),
                );
                if ui.button("remove").clicked() {
                    let _ = &app.folder_paths.remove(idx); // Cant fail, we get the index from the iterator.
                }
                ui.checkbox(app.recursive_paths.get_mut(idx).unwrap(), "Recursive scanning");
            });
        }
    })
}

pub(crate) fn create_scanning_item(
    app: &mut ImageDuplicatesApp, ui: &mut Ui, can_analyse: &mut bool, time_remain_scan: Arc<Mutex<String>>, time_remain_analyse: Arc<Mutex<String>>,
) -> InnerResponse<()> {
    let selected_folders = app.folder_paths.clone();
    ui.horizontal(|ui| {
        if app.scanning.load(Ordering::Relaxed) {
            if ui.button("Stop scanning").clicked() {
                app.scanning.store(false, Ordering::Relaxed);
            }
        } else if ui.button("Start scanning").clicked() {
            let scanning = Arc::clone(&app.scanning);
            scanning.store(true, Ordering::Relaxed);
            let analysing = Arc::clone(&app.analysing);
            analysing.store(false, Ordering::Relaxed); // cancel analysing if scanning is started
            let folders = selected_folders.clone();
            let recursive = app.recursive_paths.clone();
            let prog = app.progress.clone();
            let use_workers = app.workers;
            thread::spawn(move || data::DataEntry::scan_folders(&folders.clone(), &scanning, &recursive, &prog.clone(), &time_remain_scan.clone(), use_workers));
        }

        if app.analysing.load(Ordering::Relaxed) {
            if ui.button("Stop analysing").clicked() {
                app.analysing.store(false, Ordering::Relaxed);
            }
        } else if *can_analyse && ui.button("Analyse data").clicked() {
            let analysing = Arc::clone(&app.analysing);
            analysing.store(true, Ordering::Relaxed);
            let scanning = Arc::clone(&app.scanning);
            scanning.store(false, Ordering::Relaxed); // cancel scanning if analysing is started
            let folders = selected_folders.clone();
            let prog = app.progress.clone();
            let use_workers = app.workers;
            thread::spawn(move || data::CorrelationEntry::analyse_folders(&folders.clone(), &analysing, &prog, &time_remain_analyse.clone(), use_workers));
        }
        let max_workers = MAX_WORKERS.load(Ordering::Relaxed);
        ui.add(egui::Slider::new(&mut app.workers, 1..=max_workers as u16).text("Max workers")).on_hover_ui(|ui| {
            ui.label("Select the number of maximum workers to be used for scanning/analysing.");
        });
        if !selected_folders.is_empty() {
            let correlation_db_path = data::get_db_path::<data::CorrelationEntry>(&selected_folders[0].clone());
            if fs::exists(&correlation_db_path).expect("Could not check if file {db_path} exists.") && ui.button("Delete analysis data").clicked() {
                println!("Removing analysis database: {correlation_db_path}");
                match fs::remove_file(&correlation_db_path) {
                    Ok(()) => println!("Successfully removed analysis data at: {correlation_db_path}"),
                    Err(e) => println!("Could not removed analysis data at: {correlation_db_path}. Error {e}"),
                }
                app.analysing.store(false, Ordering::Relaxed);
            }
        }
    })
}

pub(crate) fn create_progress_bar_item(app: &mut ImageDuplicatesApp, ui: &mut Ui, progress_bar: ProgressBar) {
    let scanning = app.scanning.load(Ordering::Relaxed);
    let analysing = app.analysing.load(Ordering::Relaxed);
    if scanning || analysing {
        ui.add(progress_bar);
        let time_remaining = app.time_remaining.lock().clone();
        if time_remaining.is_empty() {
            ui.label("Remaining time unknown.");
        } else {
            ui.label(time_remaining);
        }
    }
}

pub(crate) fn create_result_items(app: &mut ImageDuplicatesApp, ui: &mut Ui, can_analyse: bool, can_show_results: bool) {
    let analysing = app.analysing.load(Ordering::Relaxed);
    let scanning = app.scanning.load(Ordering::Relaxed);
    if !analysing && !scanning && can_analyse {
        if app.display_images.load(Ordering::Relaxed) {
            if ui.button("Close results").clicked() {
                app.correlation_data.lock().clear();
                app.display_images.store(false, Ordering::Relaxed);
            }
        } else if can_show_results && ui.button("Show results").clicked() {
            let mut c_data = app.correlation_data.lock();
            for folder in app.folder_paths.clone() {
                let tmp: Vec<CorrelationEntry> = data::CorrelationEntry::read_correlation_db(&folder).unwrap().iter().filter(|e| e.corr > 0.9).cloned().collect();
                tmp.clone_into(&mut c_data);
            }
            app.display_images.store(true, Ordering::Relaxed);
            {
                let entry = &c_data.get(app.correlation_entry_idx.load(Ordering::Relaxed)).unwrap();
                {
                    let mut first_path = app.first_path.lock();
                    (*first_path).clone_from(&entry.first_path);
                }
                let mut second_path = app.second_path.lock();
                (*second_path).clone_from(&entry.second_path);
            }
        }
    }
}

pub(crate) fn create_result_display(app: &mut ImageDuplicatesApp, ui: &mut Ui, image_clicked: &Arc<Mutex<String>>) {
    if app.display_images.load(Ordering::Relaxed) {
        ui.horizontal(|ui| {
            let first_path = &mut app.first_path.lock();
            let second_path = &mut app.second_path.lock();
            if ui.button("Previous").clicked() {
                let mut idx = app.correlation_entry_idx.load(Ordering::Relaxed);
                let c_data = app.correlation_data.lock();

                *AUTO_FORWARD_DIRECTION.lock() = Direction::Backwards;

                if idx > 0 {
                    idx -= 1;
                } else {
                    idx = c_data.len() - 1;
                }
                app.correlation_entry_idx.store(idx, Ordering::Relaxed);

                {
                    let entry = c_data.get(idx).expect("Somehow the index got out of bounds");
                    (*first_path).clone_from(&entry.first_path);
                    (*second_path).clone_from(&entry.second_path);
                }
            };
            if ui.button("Next").clicked() {
                let mut idx = app.correlation_entry_idx.load(Ordering::Relaxed);
                let c_data = app.correlation_data.lock();

                *AUTO_FORWARD_DIRECTION.lock() = Direction::Forwards;

                if idx < (c_data.len() - 1) {
                    idx += 1;
                } else {
                    idx = 0;
                }
                app.correlation_entry_idx.store(idx, Ordering::Relaxed);

                {
                    let entry = c_data.get(idx).expect("Somehow the index got out of bounds");
                    (*first_path).clone_from(&entry.first_path);
                    (*second_path).clone_from(&entry.second_path);
                }
            };
            let first_path_exists = Path::new(&first_path.clone()).exists();
            let second_path_exists = Path::new(&second_path.clone()).exists();
            if !first_path_exists || !second_path_exists {
                let mut direction = AUTO_FORWARD_DIRECTION.lock();
                let mut idx = app.correlation_entry_idx.load(Ordering::Relaxed);
                let c_data = app.correlation_data.lock();

                match *direction {
                    Direction::Forwards => {
                        if idx < (c_data.len() - 1) {
                            idx += 1;
                        } else {
                            idx = 0;
                        }
                    }
                    Direction::Backwards => {
                        if idx > 0 {
                            idx -= 1;
                        } else {
                            idx = c_data.len() - 1;
                        }
                    }
                    Direction::None => {
                        if idx < (c_data.len() - 1) {
                            idx += 1;
                        } else {
                            idx = 0;
                        }
                        *direction = Direction::Forwards;
                    }
                };
                app.correlation_entry_idx.store(idx, Ordering::Relaxed);

                {
                    let entry = c_data.get(idx).expect("Somehow the index got out of bounds");
                    (*first_path).clone_from(&entry.first_path);
                    (*second_path).clone_from(&entry.second_path);
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Clicking on the images moves them into the \"potential_duplicates\" folder. Clicking the button will replace the smaller one.").color(Color32::RED));
            {
                let mut undo_list = UNDO_LIST.lock();
                if !undo_list.is_empty() && ui.button("Restore last move").clicked() {
                    if let Some(original_path) = undo_list.pop() {
                        let duplicate_file_name = Path::new(&original_path).file_name().unwrap().to_str().unwrap();
                        let selected_folder = Path::new(&original_path).parent().expect("Could not get dir of file").display().to_string();
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

        let first_path = &app.first_path.lock().clone();
        let second_path = &app.second_path.lock().clone();

        let image1 = egui::Image::new(format!("file://{}", &first_path));
        let image2 = egui::Image::new(format!("file://{}", &second_path));

        let available_size = ui.available_size();
        ui.horizontal(|ui| {
            // Can not calculate the sizing inside, since it changes after the first image is inserted
            let (w1, h1) = match image::ImageReader::open(first_path) {
                Ok(image) => image.into_dimensions().unwrap_or_default(),
                Err(e) => {
                    eprintln!("Error while reading file {first_path}: {e:?}");
                    (0, 0)
                }
            };

            let (w2, h2) = match image::ImageReader::open(second_path) {
                Ok(image) => image.into_dimensions().unwrap_or_default(),
                Err(e) => {
                    eprintln!("Error while reading file {second_path}: {e:?}");
                    (0, 0)
                }
            };

            let mut higher_res_1 = false;
            let mut higher_res_2 = false;
            let dimensions_comparison = (w1 * h1).cmp(&(w2 * h2));
            match dimensions_comparison {
                Less => {
                    higher_res_2 = true;
                }
                Equal => {}
                Greater => {
                    higher_res_1 = true;
                }
            };

            // 4 lines of text, each line is around 12px in height
            // Removing 25px for the button between them
            let image_info_height = 4.0 * 12.0 * UI_SCALING_FACTOR;
            // let selected_folder = Path::new(&image_clicked.lock().to_string()).parent().expect("Could not get dirname from file").display().to_string();
            create_ui_image_component(ui, first_path, image1, (available_size.x / 2.0 - 25.0, (available_size.y - image_info_height)), image_clicked, /*&selected_folder,*/ higher_res_1);
            match dimensions_comparison {
                Less => {
                    if ui.button("<-").clicked() {
                        println!("Overwriting {second_path} → {first_path}");
                        fs::rename(second_path, first_path).expect("Can not fail, unless files were altered in the meantime.");
                    };
                }
                Equal => {
                    ui.label("  ");
                }
                Greater => {
                    if ui.button("->").clicked() {
                        println!("Overwriting {first_path} → {second_path}");
                        fs::rename(first_path, second_path).expect("Can not fail, unless files were altered in the meantime.");
                    };
                }
            }
            create_ui_image_component(ui, second_path, image2, (available_size.x / 2.0 - 25.0, (available_size.y - image_info_height)), image_clicked, /*&selected_folder,*/ higher_res_2);
        });

        {
            let c_data = app.correlation_data.lock();
            let corr = &c_data.get(app.correlation_entry_idx.load(Ordering::Relaxed)).unwrap().corr;
            ui.label(format!("Resemblance: {:.2}%", corr * 100.0));
        }
    }
}

pub(crate) fn create_ui_image_component(ui: &mut Ui, file_path: &str, image: egui::Image, max_size: (f32, f32), to_delete: &Arc<Mutex<String>>, higher_res: bool) {
    let filename = Path::new(&file_path).file_name().unwrap().to_str().unwrap();
    let display_filename = shorten_string(filename, 37);
    let scaled_image = image.fit_to_original_size(1.0).max_width(max_size.0).max_height(max_size.1).sense(Sense::click());
    let fallback_folder = Path::new(&file_path).parent().expect("Could not get dirname from file").display().to_string();
    let root_folder = Path::new(&to_delete.lock().to_string()).parent().unwrap_or(Path::new(&fallback_folder)).display().to_string();
    let duplicate_folder_path = format!("{root_folder}/{POTENTIAL_DUPLICATES_FOLDER}");
    ui.vertical(|ui| {
        match image::ImageReader::open(file_path) {
            Ok(image) => {
                let (width, height) = image.into_dimensions().expect("Could not get dimensions");
                let stroke_colour = if higher_res { egui::Color32::GREEN } else { egui::Color32::TRANSPARENT };
                egui::Frame::default().inner_margin(2.0).fill(stroke_colour).show(ui, |ui| {
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
                });
                ui.label(display_filename);
                ui.label(format!("Parent folder: {}", shorten_string(Path::new(file_path).parent().unwrap().file_name().unwrap().to_str().unwrap(), 13)));
                ui.label(format!("Image dimensions: {width}x{height}"));
            }
            Err(e) => {
                /*eprint!("Could not open image {e}")*/
                drop(e);
            }
        };
    });
}
