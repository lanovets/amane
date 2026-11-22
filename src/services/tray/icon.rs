use std::env;
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::LazyLock;

use png::{BitDepth, ColorType, Encoder};

use crate::Value;
use crate::graphics::image;
use crate::services::apps::icons;

// under the cache folder, on disk: a runtime folder is memory, and no process's numbers count it
const FOLDER: &str = "tray";

// the biggest side read, since a program can send any size it likes
const LARGEST: usize = 512;

// an icon is drawn small, so the smallest pixmap with both sides this long is plenty
const WANTED: usize = 32;

// read once, so a new icon theme shows after a restart
static THEMES: LazyLock<Vec<Vec<PathBuf>>> = LazyLock::new(icons::read_icon_folders);

/*
 * the file to draw: an icon the program names, or the pixels it
 * sends, saved as a png so they are drawn like any image file;
 * while the item needs attention its attention icon comes first
 */
pub fn find(
    property: &dyn Fn(&str) -> Value,
    id: u32,
    attention: bool,
    kept: Option<&Path>,
) -> Option<PathBuf> {
    let kinds: &[&str] = if attention {
        &["AttentionIcon", "Icon"]
    } else {
        &["Icon"]
    };

    let folder = property("IconThemePath");

    for kind in kinds {
        let name = property(&format!("{kind}Name"));

        if let Some(file) = named(name.text(), folder.text()) {
            return Some(file);
        }
    }

    /*
     * pixels come as one value per byte, a quarter of a million for a
     * big icon, so they are only read when the program says they changed
     */
    if let Some(kept) = kept.filter(|kept| saved_here(kept)) {
        return Some(kept.to_path_buf());
    }

    for kind in kinds {
        if let Some(file) = save(&property(&format!("{kind}Pixmap")), id) {
            return Some(file);
        }
    }

    None
}

fn named(name: &str, folder: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }

    // some programs name the file itself
    if Path::new(name).is_absolute() {
        return Path::new(name).is_file().then(|| PathBuf::from(name));
    }

    if !folder.is_empty()
        && let Some(file) = icons::look_up_icon_file_in_folder(name, Path::new(folder))
    {
        return Some(file);
    }

    icons::look_up_icon_file(name, &THEMES)
}

// only icons saved here are removed, never a file a theme or a program owns
pub fn remove(icon: Option<&Path>) {
    if let Some(icon) = icon.filter(|icon| saved_here(icon)) {
        let _ = fs::remove_file(icon);

        image::gone(icon);
    }
}

// the icons of a shell that is gone, which had no chance to remove them
pub fn sweep() {
    let Ok(files) = fs::read_dir(folder()) else {
        return;
    };

    for file in files.flatten() {
        let name = file.file_name();

        // each file starts with the number of the process that saved it
        let Some((process, _)) = name.to_str().and_then(|name| name.split_once('-')) else {
            continue;
        };

        if !Path::new("/proc").join(process).exists() {
            let _ = fs::remove_file(file.path());
        }
    }
}

fn saved_here(icon: &Path) -> bool {
    icon.parent() == Some(&folder())
}

// $XDG_CACHE_HOME/amane/tray, a folder of its own: sweep removes every file in it that is not a shell's
fn folder() -> PathBuf {
    let cache = match env::var_os("XDG_CACHE_HOME").filter(|folder| !folder.is_empty()) {
        Some(folder) => PathBuf::from(folder),
        None => match env::var_os("HOME").filter(|home| !home.is_empty()) {
            Some(home) => PathBuf::from(home).join(".cache"),
            None => env::temp_dir(),
        },
    };

    cache.join("amane").join(FOLDER)
}

// named after its pixels, so an icon sent again is the same file and is not decoded twice
fn save(pixmaps: &Value, id: u32) -> Option<PathBuf> {
    let (width, height, pixels) = chosen(pixmaps)?;

    let mut hasher = DefaultHasher::new();

    (width, &pixels).hash(&mut hasher);

    let name = format!("{}-{id}-{:x}.png", process::id(), hasher.finish());

    let folder = folder();

    let path = folder.join(name);

    if path.is_file() {
        return Some(path);
    }

    fs::create_dir_all(&folder).ok()?;

    // a file that failed halfway would be taken for this icon the next time
    if write(&path, width, height, &pixels).is_none() {
        let _ = fs::remove_file(&path);

        return None;
    }

    Some(path)
}

fn write(path: &Path, width: usize, height: usize, pixels: &[u8]) -> Option<()> {
    let file = BufWriter::new(File::create(path).ok()?);

    let mut encoder = Encoder::new(file, width as u32, height as u32);

    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);

    let mut writer = encoder.write_header().ok()?;

    writer.write_image_data(pixels).ok()?;

    writer.finish().ok()
}

/*
 * a program sends its icon in several sizes, each as (width, height, bytes);
 * of the ones that are as long as they say, the smallest with both sides at
 * least WANTED comes back, or the largest of all when none is that big, as
 * rgba. a bigger one would be saved, read and shrunk again for nothing
 */
fn chosen(pixmaps: &Value) -> Option<(usize, usize, Vec<u8>)> {
    let mut big_enough: Option<(usize, usize, &[Value])> = None;
    let mut largest: Option<(usize, usize, &[Value])> = None;

    for pixmap in pixmaps.list() {
        let [width, height, bytes] = pixmap.list() else {
            continue;
        };

        // a negative size turns into zero here
        let width = width.number() as usize;
        let height = height.number() as usize;

        if width == 0 || height == 0 || width > LARGEST || height > LARGEST {
            continue;
        }

        let bytes = bytes.list();

        if bytes.len() != width * height * 4 {
            continue;
        }

        let area = width * height;

        if largest.is_none_or(|(held_width, held_height, _)| area > held_width * held_height) {
            largest = Some((width, height, bytes));
        }

        if width >= WANTED
            && height >= WANTED
            && big_enough.is_none_or(|(held_width, held_height, _)| area < held_width * held_height)
        {
            big_enough = Some((width, height, bytes));
        }
    }

    let (width, height, bytes) = big_enough.or(largest)?;

    let mut pixels = Vec::with_capacity(bytes.len());

    for [alpha, red, green, blue] in bytes.as_chunks::<4>().0 {
        for channel in [red, green, blue, alpha] {
            pixels.push(channel.number() as u8);
        }
    }

    Some((width, height, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::image;

    fn pixmap(width: f64, height: f64, bytes: &[u8]) -> Value {
        let bytes = bytes.iter().map(|byte| Value::Number(f64::from(*byte)));

        Value::List(vec![
            Value::Number(width),
            Value::Number(height),
            Value::List(bytes.collect()),
        ])
    }

    #[test]
    fn picks_the_smallest_pixmap_that_is_big_enough() {
        let square = |side: usize| pixmap(side as f64, side as f64, &vec![7; side * side * 4]);

        let pixmaps = Value::List(vec![
            square(16),
            square(64),
            square(32),
            // says it is bigger than the bytes it sent
            pixmap(4.0, 4.0, &[0; 8]),
            pixmap(-1.0, 1.0, &[0; 4]),
            pixmap(4096.0, 4096.0, &[]),
        ]);

        let (width, height, _) = chosen(&pixmaps).expect("failed to pick a pixmap");

        assert_eq!((width, height), (32, 32));

        // an app with only small icons gets the biggest one available
        let small = Value::List(vec![square(1), square(16), square(24)]);

        let (width, height, _) = chosen(&small).expect("failed to pick a small pixmap");

        assert_eq!((width, height), (24, 24));

        assert!(chosen(&Value::Nothing).is_none());
    }

    #[test]
    fn two_pixmaps_of_the_same_size_keep_the_first() {
        let square = |side: usize| pixmap(side as f64, side as f64, &vec![7; side * side * 4]);

        // the same area, a different shape: neither is 32 px, so the fallback
        // takes one of them, and the first is the one that stays
        let wide = pixmap(32.0, 8.0, &vec![7; 32 * 8 * 4]);

        let same = Value::List(vec![square(16), wide]);

        let (width, height, _) = chosen(&same).expect("failed to pick a pixmap");

        assert_eq!((width, height), (16, 16));
    }

    #[test]
    fn turns_argb_into_rgba() {
        let pixmaps = Value::List(vec![pixmap(2.0, 1.0, &[255, 10, 20, 30, 128, 40, 50, 60])]);

        let (width, height, pixels) = chosen(&pixmaps).expect("failed to pick a pixmap");

        assert_eq!((width, height), (2, 1));
        assert_eq!(pixels, [10, 20, 30, 255, 40, 50, 60, 128]);
    }

    #[test]
    fn saves_pixels_once_and_removes_only_its_own() {
        let pixmaps = Value::List(vec![pixmap(1.0, 2.0, &[255, 1, 2, 3, 0, 4, 5, 6])]);

        let path = save(&pixmaps, 7).expect("failed to save the icon");

        assert_eq!(save(&pixmaps, 7), Some(path.clone()));

        let bitmap = image::read(&path).expect("failed to read the saved icon");

        assert_eq!(*bitmap.pixels, [1, 2, 3, 255, 4, 5, 6, 0]);

        // a file somewhere else is the program's own
        let other = env::temp_dir().join(format!("amane-tray-test-{}.png", process::id()));

        fs::write(&other, "").expect("failed to write a file");

        remove(Some(&other));
        remove(Some(&path));

        assert!(other.exists());
        assert!(!path.exists());

        let _ = fs::remove_file(other);
    }

    #[test]
    fn a_named_file_has_to_exist() {
        assert_eq!(named("", ""), None);
        assert_eq!(named("/nowhere/amane-tray-missing.png", ""), None);

        let folder = env::temp_dir().join(format!("amane-tray-icons-{}", process::id()));

        fs::create_dir_all(&folder).expect("failed to make a folder");

        let file = folder.join("amane-tray-test-icon.png");

        fs::write(&file, "").expect("failed to write a file");

        let own_folder = folder.to_string_lossy();

        assert_eq!(
            named("amane-tray-test-icon", &own_folder),
            Some(file.clone())
        );
        assert_eq!(named(&file.to_string_lossy(), ""), Some(file));

        let _ = fs::remove_dir_all(folder);
    }
}
