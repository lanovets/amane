mod shrink;
mod soften;
mod svg;

use std::any::TypeId;
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::thread;

use png::{ColorType, Decoder, Transformations};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::changes;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

const JPEG_SIGNATURE: [u8; 3] = [0xFF, 0xD8, 0xFF];

// rgba pixels with plain, not premultiplied, alpha
pub struct Bitmap {
    width: u32,
    height: u32,

    // shared with the gpu, which draws from the same pixels instead of a copy
    pub pixels: Arc<Vec<u8>>,
}

impl Bitmap {
    // one clear pixel, for a texture slot that has to hold something
    pub fn empty() -> Self {
        Self {
            width: 1,
            height: 1,
            pixels: Arc::new(vec![0; 4]),
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

/*
 * what a window waiting on an image counts as having read, so a finished
 * decode only draws the windows still missing a picture, not every window
 */
struct Decoded;

// a file, the size it is shrunk to cover (none for its own size), and how far it is blurred
type Key = (PathBuf, Option<(u32, u32)>, u32);

// covers no window draws anymore are kept for reuse; past this many the
// oldest goes, because a cover from a track half an hour ago will not return
const KEPT: usize = 32;

// every image asked for: none while it is still decoding, or when it could not be read
#[derive(Default)]
struct Loaded {
    slots: HashMap<Key, Option<Arc<Bitmap>>>,

    // the keys in the order they were asked for, oldest first
    order: VecDeque<Key>,
}

static LOADED: LazyLock<Mutex<Loaded>> = LazyLock::new(|| Mutex::new(Loaded::default()));

/*
 * decoding a large image takes long enough to drop frames, so it happens
 * on its own thread; until it is done this gives none and the image is
 * left out, then the window is woken to draw it
 */
pub fn load(path: &Path, cover: Option<(u32, u32)>, blur: u32) -> Option<Arc<Bitmap>> {
    let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);

    let key = (path.to_path_buf(), cover, blur);

    if let Some(image) = loaded.slots.get(&key) {
        if image.is_none() {
            changes::note_read(TypeId::of::<Decoded>());
        }

        let image = image.clone();

        touch(&mut loaded, &key);

        return image;
    }

    loaded.slots.insert(key.clone(), None);

    touch(&mut loaded, &key);

    trim(&mut loaded);

    thread::spawn(move || decode(key));

    changes::note_read(TypeId::of::<Decoded>());

    None
}

// marks the key as the most recently asked for, so trim drops the coldest first
fn touch(loaded: &mut Loaded, key: &Key) {
    if let Some(at) = loaded.order.iter().position(|kept| kept == key) {
        loaded.order.remove(at);
    }

    loaded.order.push_back(key.clone());
}

// a shrunk image that no window holds, the only kind the cache keeps for reuse
fn unheld_cover(loaded: &Loaded, key: &Key) -> bool {
    key.1.is_some()
        && matches!(
            loaded.slots.get(key),
            Some(Some(image)) if Arc::strong_count(image) == 1
        )
}

/*
 * drops the covers no window draws anymore once the cache is past its size,
 * oldest first; a picture a window still holds stays, so nothing on screen
 * is ever let go from under it
 */
fn trim(loaded: &mut Loaded) {
    // only the covers nobody holds are counted: a picture a window draws, a
    // full size one and one still decoding are not part of what the cache
    // keeps, so they must not shrink what it may keep
    let unheld = loaded
        .order
        .iter()
        .filter(|key| unheld_cover(loaded, key))
        .count();

    let mut excess = unheld.saturating_sub(KEPT);

    let mut at = 0;

    while excess > 0 && at < loaded.order.len() {
        let key = loaded.order[at].clone();

        if unheld_cover(loaded, &key) {
            loaded.slots.remove(&key);
            loaded.order.remove(at);
            excess -= 1;
        } else {
            at += 1;
        }
    }
}

fn decode(key: Key) {
    let (path, cover, blur) = &key;

    // a broken file stays empty instead of taking the shell down
    let Some(image) = read(path) else {
        return;
    };

    // only the small copy is kept, the full size one is dropped here
    let image = match cover {
        Some((width, height)) => shrink::to_cover(image, *width, *height),
        None => image,
    };

    // blurred after shrinking, where there are far fewer pixels to average
    let image = soften::soften(image, *blur);

    // an image let go during its decode (see unused) is dropped here
    {
        let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);

        let Some(slot @ None) = loaded.slots.get_mut(&key) else {
            return;
        };

        *slot = Some(Arc::new(image));

        touch(&mut loaded, &key);

        trim(&mut loaded);
    }

    changes::mark(TypeId::of::<Decoded>());
}

/*
 * frees a full size image no window shows anymore; small copies stay until
 * trim passes its size, and decoding them again is slow
 */
pub fn forget(image: &Arc<Bitmap>) {
    let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);

    loaded.slots.retain(|(_, cover, _), kept| {
        let same = kept.as_ref().is_some_and(|kept| Arc::ptr_eq(kept, image));

        cover.is_some() || !same
    });

    let mut order = std::mem::take(&mut loaded.order);

    order.retain(|key| loaded.slots.contains_key(key));

    loaded.order = order;
}

// none for a file that can't be read or decoded, like one still being written
pub fn read(path: &Path) -> Option<Bitmap> {
    let bytes = std::fs::read(path).ok()?;

    // the file's first bytes say its format, whatever its name ends in
    if bytes.starts_with(&PNG_SIGNATURE) {
        return decode_png(&bytes);
    }

    if bytes.starts_with(&JPEG_SIGNATURE) {
        return decode_jpeg(&bytes);
    }

    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
    {
        return svg::rasterize(&bytes);
    }

    // only png, jpeg and svg are supported
    None
}

fn decode_png(bytes: &[u8]) -> Option<Bitmap> {
    let mut decoder = Decoder::new(Cursor::new(bytes));

    // palettes, tiny bit depths and 16 bit samples all turn into plain 8 bit channels
    decoder.set_transformations(Transformations::normalize_to_color8());

    let mut reader = decoder.read_info().ok()?;

    let size = reader.output_buffer_size()?;

    let mut pixels = vec![0; size];

    let frame = reader.next_frame(&mut pixels).ok()?;

    pixels.truncate(frame.buffer_size());

    let bitmap = Bitmap {
        width: frame.width,
        height: frame.height,
        pixels: Arc::new(expand(&pixels, frame.color_type)),
    };

    Some(bitmap)
}

// fills in the channels a png left out, so every pixel is red, green, blue, alpha
fn expand(pixels: &[u8], color_type: ColorType) -> Vec<u8> {
    match color_type {
        ColorType::Rgba => pixels.to_vec(),

        ColorType::Rgb => pixels
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),

        ColorType::GrayscaleAlpha => pixels
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[0], pixel[0], pixel[0], pixel[1]])
            .collect(),

        ColorType::Grayscale => pixels
            .iter()
            .flat_map(|&gray| [gray, gray, gray, 255])
            .collect(),

        ColorType::Indexed => panic!("failed to decode png: palette was not expanded"),
    }
}

fn decode_jpeg(bytes: &[u8]) -> Option<Bitmap> {
    // a bitmap has four channels, jpeg only stores three
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);

    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);

    let pixels = decoder.decode().ok()?;

    let info = decoder.info()?;

    let bitmap = Bitmap {
        width: u32::from(info.width),
        height: u32::from(info.height),
        pixels: Arc::new(pixels),
    };

    Some(bitmap)
}

// every copy of a file that was deleted, small ones too: its name never comes back
pub fn gone(path: &Path) {
    let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);

    loaded.slots.retain(|(file, _, _), _| file != path);

    let mut order = std::mem::take(&mut loaded.order);

    order.retain(|key| loaded.slots.contains_key(key));

    loaded.order = order;
}

// lets go of a full size copy no window draws, or of its decode
pub fn unused(path: &Path) {
    let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);

    loaded.slots.retain(|(file, cover, _), kept| {
        let unheld = kept
            .as_ref()
            .is_none_or(|kept| Arc::strong_count(kept) == 1);

        file != path || cover.is_some() || !unheld
    });

    let mut order = std::mem::take(&mut loaded.order);

    order.retain(|key| loaded.slots.contains_key(key));

    loaded.order = order;
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    // waits for the decode load() started
    fn decoded(path: &Path) -> Arc<Bitmap> {
        let started = Instant::now();

        loop {
            if let Some(image) = load(path, None, 0) {
                return image;
            }

            assert!(started.elapsed() < Duration::from_secs(5), "never decoded");

            thread::sleep(Duration::from_millis(5));
        }
    }

    fn held(path: &Path) -> bool {
        let loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);

        loaded.slots.contains_key(&(path.to_path_buf(), None, 0))
    }

    #[test]
    fn covers_nobody_draws_are_trimmed_but_a_drawn_one_stays() {
        let mut cache = Loaded::default();

        let cover = Some((8, 8));

        let drawn = (PathBuf::from("/tmp/drawn.png"), cover, 0);
        let image = Arc::new(Bitmap::empty());

        cache.slots.insert(drawn.clone(), Some(Arc::clone(&image)));
        cache.order.push_back(drawn.clone());

        for index in 0..KEPT + 4 {
            let key = (PathBuf::from(format!("/tmp/cover-{index}.png")), cover, 0);

            cache
                .slots
                .insert(key.clone(), Some(Arc::new(Bitmap::empty())));
            cache.order.push_back(key);
        }

        trim(&mut cache);

        // the oldest covers go, and the map never grows past the limit
        assert!(cache.slots.len() <= KEPT + 1);
        assert!(cache.slots.contains_key(&drawn));
        assert!(
            !cache
                .slots
                .contains_key(&(PathBuf::from("/tmp/cover-0.png"), cover, 0))
        );
    }

    #[test]
    fn the_cover_budget_counts_only_the_covers_nobody_holds() {
        let mut cache = Loaded::default();

        let cover = Some((8, 8));

        // a cover a window holds, a full size image and a decode in flight
        let mut held = Vec::new();

        for index in 0..KEPT {
            let drawn = (PathBuf::from(format!("/tmp/drawn-{index}.png")), cover, 0);
            let image = Arc::new(Bitmap::empty());
            cache.slots.insert(drawn.clone(), Some(Arc::clone(&image)));
            cache.order.push_back(drawn);
            held.push(image);

            let full = (PathBuf::from(format!("/tmp/full-{index}.png")), None, 0);
            cache
                .slots
                .insert(full.clone(), Some(Arc::new(Bitmap::empty())));
            cache.order.push_back(full);

            let decoding = (PathBuf::from(format!("/tmp/decoding-{index}.png")), cover, 0);
            cache.slots.insert(decoding.clone(), None);
            cache.order.push_back(decoding);
        }

        // and one cover more than the cache keeps
        for index in 0..=KEPT {
            let key = (PathBuf::from(format!("/tmp/cover-{index}.png")), cover, 0);
            cache
                .slots
                .insert(key.clone(), Some(Arc::new(Bitmap::empty())));
            cache.order.push_back(key);
        }

        trim(&mut cache);

        // the oldest cover nobody holds is the one that goes
        assert!(!cache.slots.contains_key(&(PathBuf::from("/tmp/cover-0.png"), cover, 0)));
        assert!(cache.slots.contains_key(&(PathBuf::from("/tmp/cover-1.png"), cover, 0)));

        // and the drawn one, the full size one and the decode stay
        assert!(cache.slots.contains_key(&(PathBuf::from("/tmp/drawn-0.png"), cover, 0)));
        assert!(cache.slots.contains_key(&(PathBuf::from("/tmp/full-0.png"), None, 0)));
        assert!(cache.slots.contains_key(&(PathBuf::from("/tmp/decoding-0.png"), cover, 0)));
    }

    #[test]
    fn unused_lets_go_only_of_pictures_nobody_draws() {
        let path = std::env::temp_dir().join(format!("amane-unused-{}.svg", std::process::id()));

        std::fs::write(&path, r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="red"/></svg>"#).unwrap();

        // a window still draws it: it stays
        let drawn = decoded(&path);

        unused(&path);

        assert!(held(&path));

        // nobody does: it goes
        drop(drawn);

        unused(&path);

        assert!(!held(&path));

        // let go while decoding: the decode is dropped when it finishes
        assert!(load(&path, None, 0).is_none());

        unused(&path);

        thread::sleep(Duration::from_millis(200));

        assert!(!held(&path));

        let _ = std::fs::remove_file(&path);
    }
}
