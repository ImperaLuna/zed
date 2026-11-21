//! Finds font files through fontconfig, so the text system can read only the
//! fonts it needs instead of every font on the system.
//!
//! Reading all system fonts opens and parses every font file (hundreds on a
//! typical desktop, a quarter of a second on a cold disk). Fontconfig keeps a
//! cache of what each file holds, so asking it which file has a family, or
//! which file draws a character, reads that cache instead of the fonts.

use std::{
    ffi::{CStr, CString, OsStr},
    os::{raw::c_int, unix::ffi::OsStrExt},
    path::PathBuf,
    ptr,
};

use fontconfig_sys::{
    FcCharSet, FcCharSetAddChar, FcCharSetCreate, FcCharSetDestroy, FcCharSetHasChar, FcConfig,
    FcConfigDestroy, FcConfigSubstitute, FcDefaultSubstitute, FcFontList, FcFontSet,
    FcFontSetDestroy, FcFontSort, FcInitLoadConfigAndFonts, FcMatchPattern, FcObjectSetAdd,
    FcObjectSetCreate, FcObjectSetDestroy, FcPattern, FcPatternAddBool, FcPatternAddCharSet,
    FcPatternAddString, FcPatternCreate, FcPatternDestroy, FcPatternGetCharSet, FcPatternGetString,
    FcResultMatch,
    constants::{FC_CHARSET, FC_COLOR, FC_FAMILY, FC_FILE},
};

/// A loaded fontconfig configuration with its font cache.
pub(crate) struct Fontconfig {
    config: *mut FcConfig,
}

// SAFETY: the `FcConfig` is owned by this value and only used through
// `&mut self`, so it is never touched from two threads at once.
unsafe impl Send for Fontconfig {}
// SAFETY: as above; `&Fontconfig` gives no access to the configuration.
unsafe impl Sync for Fontconfig {}

impl Fontconfig {
    /// Loads the user's fontconfig configuration. `None` when fontconfig
    /// can't load it, in which case the caller reads every font instead.
    pub(crate) fn new() -> Option<Self> {
        // SAFETY: takes no arguments; returns an owned config or null.
        let config = unsafe { FcInitLoadConfigAndFonts() };
        (!config.is_null()).then_some(Self { config })
    }

    /// The files holding faces whose family is `family`, under any of the
    /// family's names. Empty when no installed font has that family.
    pub(crate) fn family_files(&mut self, family: &str) -> Vec<PathBuf> {
        let Ok(family) = CString::new(family) else {
            return Vec::new();
        };
        let Some(pattern) = Pattern::new() else {
            return Vec::new();
        };
        // SAFETY: the pattern is live and both strings are NUL-terminated.
        unsafe { FcPatternAddString(pattern.0, FC_FAMILY.as_ptr(), family.as_ptr().cast()) };
        let mut files = self
            .list(&pattern, &[FC_FILE])
            .iter()
            .filter_map(|font| pattern_file(font))
            .collect::<Vec<_>>();
        files.sort();
        files.dedup();
        files
    }

    /// The family names of every installed font.
    pub(crate) fn family_names(&mut self) -> Vec<String> {
        let Some(pattern) = Pattern::new() else {
            return Vec::new();
        };
        self.list(&pattern, &[FC_FAMILY])
            .iter()
            .filter_map(|font| pattern_string(font, FC_FAMILY))
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    }

    /// The file fontconfig picks to draw `ch`, following the user's
    /// configuration (preferred fonts, language). `color` prefers a color
    /// font, for emoji. `None` when no installed font has the character.
    pub(crate) fn file_for_char(&mut self, ch: char, color: bool) -> Option<PathBuf> {
        let pattern = Pattern::new()?;
        let charset = CharSet::new(ch)?;
        // SAFETY: pattern and charset are live; fontconfig copies the charset.
        unsafe {
            FcPatternAddCharSet(pattern.0, FC_CHARSET.as_ptr(), charset.0);
            if color {
                FcPatternAddBool(pattern.0, FC_COLOR.as_ptr(), 1);
            }
            FcConfigSubstitute(self.config, pattern.0, FcMatchPattern);
            FcDefaultSubstitute(pattern.0);
        }
        let mut result = FcResultMatch;
        // SAFETY: config and pattern are live; `trim` keeps only fonts that add
        // coverage, and we ask for no combined charset.
        let sorted =
            FontSet(unsafe { FcFontSort(self.config, pattern.0, 1, ptr::null_mut(), &mut result) });
        sorted
            .iter()
            .find(|&font| pattern_has_char(font, ch))
            .and_then(|font| pattern_file(font))
    }

    fn list(&mut self, pattern: &Pattern, objects: &[&CStr]) -> FontSet {
        // SAFETY: takes no arguments; returns an owned object set or null.
        let object_set = unsafe { FcObjectSetCreate() };
        if object_set.is_null() {
            return FontSet(ptr::null_mut());
        }
        for object in objects {
            // SAFETY: the object set is live and the name is NUL-terminated.
            unsafe { FcObjectSetAdd(object_set, object.as_ptr()) };
        }
        // SAFETY: config, pattern and object set are live; the font set we
        // get back is owned by `FontSet`.
        let set = unsafe { FcFontList(self.config, pattern.0, object_set) };
        // SAFETY: created above and not used after this.
        unsafe { FcObjectSetDestroy(object_set) };
        FontSet(set)
    }
}

impl Drop for Fontconfig {
    fn drop(&mut self) {
        // SAFETY: we own the config and nothing uses it after this.
        unsafe { FcConfigDestroy(self.config) };
    }
}

struct Pattern(*mut FcPattern);

impl Pattern {
    fn new() -> Option<Self> {
        // SAFETY: takes no arguments; returns an owned pattern or null.
        let pattern = unsafe { FcPatternCreate() };
        (!pattern.is_null()).then_some(Self(pattern))
    }
}

impl Drop for Pattern {
    fn drop(&mut self) {
        // SAFETY: we own the pattern.
        unsafe { FcPatternDestroy(self.0) };
    }
}

struct CharSet(*mut FcCharSet);

impl CharSet {
    fn new(ch: char) -> Option<Self> {
        // SAFETY: takes no arguments; returns an owned charset or null.
        let charset = unsafe { FcCharSetCreate() };
        if charset.is_null() {
            return None;
        }
        // SAFETY: the charset is live.
        unsafe { FcCharSetAddChar(charset, u32::from(ch)) };
        Some(Self(charset))
    }
}

impl Drop for CharSet {
    fn drop(&mut self) {
        // SAFETY: we own the charset.
        unsafe { FcCharSetDestroy(self.0) };
    }
}

/// An owned font set, as `FcFontList` and `FcFontSort` return.
struct FontSet(*mut FcFontSet);

impl FontSet {
    fn iter(&self) -> impl Iterator<Item = *mut FcPattern> + '_ {
        let fonts: &[*mut FcPattern] = if self.0.is_null() {
            &[]
        } else {
            // SAFETY: a live font set holds `nfont` patterns at `fonts`.
            unsafe {
                let set = &*self.0;
                if set.fonts.is_null() || set.nfont <= 0 {
                    &[]
                } else {
                    std::slice::from_raw_parts(set.fonts, set.nfont as usize)
                }
            }
        };
        fonts.iter().copied()
    }
}

impl Drop for FontSet {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own the set, and destroying it also releases the
            // patterns it holds.
            unsafe { FcFontSetDestroy(self.0) };
        }
    }
}

/// The first value of a string property. Borrowed from the pattern, so it
/// must not outlive the font set the pattern belongs to.
fn pattern_string<'a>(pattern: *mut FcPattern, object: &CStr) -> Option<&'a CStr> {
    let mut value = ptr::null_mut();
    // SAFETY: the pattern is live and fontconfig writes a borrowed string.
    let result = unsafe { FcPatternGetString(pattern, object.as_ptr(), 0 as c_int, &mut value) };
    // SAFETY: on a match the value is a NUL-terminated string the pattern owns.
    (result == FcResultMatch && !value.is_null()).then(|| unsafe { CStr::from_ptr(value.cast()) })
}

fn pattern_file(pattern: *mut FcPattern) -> Option<PathBuf> {
    pattern_string(pattern, FC_FILE).map(|file| OsStr::from_bytes(file.to_bytes()).into())
}

fn pattern_has_char(pattern: *mut FcPattern, ch: char) -> bool {
    let mut charset = ptr::null_mut();
    // SAFETY: the pattern is live and fontconfig writes a borrowed charset.
    let result = unsafe { FcPatternGetCharSet(pattern, FC_CHARSET.as_ptr(), 0, &mut charset) };
    // SAFETY: on a match the charset is owned by the live pattern.
    result == FcResultMatch
        && !charset.is_null()
        && unsafe { FcCharSetHasChar(charset, u32::from(ch)) } != 0
}
