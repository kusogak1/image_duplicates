use {
    super::POTENTIAL_DUPLICATES_FOLDER,
    std::{
        fs,
        path::{Path, PathBuf},
    },
};

pub(crate) fn visit_dirs(folder_path: &Path, recursive: bool, callback: &mut dyn FnMut(&PathBuf)) {
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

// Will shorten the string, so that the middle part is not displayed if its too long
pub(crate) fn shorten_string(input_string: &str, max_length: usize) -> String {
    let input_length = input_string.len();
    if input_length <= max_length {
        return input_string.to_string();
    }

    let half_length = (max_length - 1) / 2; // -1 accounts for the ellipsis
    let start_slice = if max_length % 2 == 0 { &input_string[..=half_length] } else { &input_string[..half_length] }; // when the desired size is even, we want to take one extra symbol from before the ellipsis.
    let end_slice = &input_string[input_length - half_length..];

    format!("{start_slice}…{end_slice}")
}

// Will shorten the string, so that the middle part is not displayed if its too long
pub(crate) fn force_string_length(input_string: &str, max_length: usize) -> String {
    let mut padded = input_string.to_string();
    let current_len = input_string.len();
    if current_len < max_length {
        let padding = max_length - current_len;
        padded.push_str(&" ".repeat(padding));
        return padded;
    }
    let input_length = input_string.len();
    let half_length = (max_length - 1) / 2; // -1 accounts for the ellipsis
    let start_slice = if max_length % 2 == 0 { &input_string[..=half_length] } else { &input_string[..half_length] }; // when the desired size is even, we want to take one extra symbol from before the ellipsis.
    let end_slice = &input_string[input_length - half_length..];

    format!("{start_slice}…{end_slice}")
}
