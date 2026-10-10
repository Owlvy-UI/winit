//! Drag and drop logic that does not touch the browser.

use std::io;
use std::ops::ControlFlow;

use winit_core::data_transfer::{DataTransfer, TransferType, TypeHint, TypedData};
use winit_core::event_loop::DndAction;

/// Largest string read from one type of a drop, in bytes.
pub(crate) const MAX_STRING_BYTES: usize = 16 * 1024 * 1024;

/// Largest file read from a drop, in bytes.
pub(crate) const MAX_FILE_BYTES: u32 = 256 * 1024 * 1024;

/// Most types listed for one drag.
pub(crate) const MAX_TYPES: usize = 64;

/// Most fetches waiting for the data of one drag.
pub(crate) const MAX_PENDING_FETCHES: usize = 256;

/// The actions a drag source allows, read from `DataTransfer.effectAllowed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Allowed {
    pub(crate) copy: bool,
    pub(crate) move_: bool,
    pub(crate) link: bool,
}

impl Allowed {
    /// Parses an `effectAllowed` value. Unknown values allow nothing.
    pub(crate) fn from_effect_allowed(value: &str) -> Self {
        let (copy, move_, link) = match value {
            "copy" => (true, false, false),
            "move" => (false, true, false),
            "link" => (false, false, true),
            "copyMove" => (true, true, false),
            "copyLink" => (true, false, true),
            "linkMove" => (false, true, true),
            "all" | "uninitialized" => (true, true, true),
            _ => (false, false, false),
        };

        Self { copy, move_, link }
    }

    /// Whether the source allows an action.
    pub(crate) fn contains(self, action: DndAction) -> bool {
        match action {
            DndAction::Copy => self.copy,
            DndAction::Move => self.move_,
            DndAction::Link => self.link,
            _ => false,
        }
    }
}

/// The `effectAllowed` value for the actions of an outgoing drag.
pub(crate) fn effect_allowed(actions: &[DndAction]) -> &'static str {
    let copy = actions.contains(&DndAction::Copy);
    let move_ = actions.contains(&DndAction::Move);
    let link = actions.contains(&DndAction::Link);

    match (copy, move_, link) {
        (true, true, true) => "all",
        (true, true, false) => "copyMove",
        (true, false, true) => "copyLink",
        (false, true, true) => "linkMove",
        (true, false, false) => "copy",
        (false, true, false) => "move",
        (false, false, true) => "link",
        (false, false, false) => "none",
    }
}

/// The `dropEffect` value for an action.
pub(crate) fn drop_effect(action: Option<DndAction>) -> &'static str {
    match action {
        Some(DndAction::Copy) => "copy",
        Some(DndAction::Move) => "move",
        Some(DndAction::Link) => "link",
        _ => "none",
    }
}

/// The action of a `dropEffect` value, `None` for `none` and unknown values.
pub(crate) fn action_of_drop_effect(value: &str) -> Option<DndAction> {
    match value {
        "copy" => Some(DndAction::Copy),
        "move" => Some(DndAction::Move),
        "link" => Some(DndAction::Link),
        _ => None,
    }
}

/// The action requested with modifier keys: Shift for move, Control for copy, both for link.
pub(crate) fn requested_action(shift: bool, control: bool) -> Option<DndAction> {
    match (shift, control) {
        (true, true) => Some(DndAction::Link),
        (true, false) => Some(DndAction::Move),
        (false, true) => Some(DndAction::Copy),
        (false, false) => None,
    }
}

/// Picks the action answered for a drag position.
///
/// The requested action is taken when the application and the source allow it. Otherwise the
/// first action of `valid` that the source allows is taken.
pub(crate) fn choose_action(
    valid: &[DndAction],
    allowed: Allowed,
    requested: Option<DndAction>,
) -> Option<DndAction> {
    if let Some(requested) = requested {
        if valid.contains(&requested) && allowed.contains(requested) {
            return Some(requested);
        }
    }

    valid.iter().copied().find(|action| allowed.contains(*action))
}

/// Whether a size reported by the browser lies within a limit.
pub(crate) fn within_limit(size: f64, limit: u32) -> bool {
    size.is_finite() && size >= 0.0 && size <= f64::from(limit)
}

/// Image extensions and their MIME types.
const IMAGES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("svg", "image/svg+xml"),
    ("avif", "image/avif"),
    ("tiff", "image/tiff"),
    ("ico", "image/x-icon"),
];

/// Audio extensions and their MIME types.
const AUDIO: &[(&str, &str)] = &[
    ("mp3", "audio/mpeg"),
    ("ogg", "audio/ogg"),
    ("wav", "audio/wav"),
    ("webm", "audio/webm"),
    ("flac", "audio/flac"),
    ("aac", "audio/aac"),
    ("m4a", "audio/mp4"),
];

/// The essence of a MIME type: lowercase, without parameters.
fn essence(mime: &str) -> String {
    mime.split_once(';').map_or(mime, |(essence, _)| essence).trim().to_ascii_lowercase()
}

/// The hint of a string type of `DataTransfer.types`.
pub(crate) fn hint_of_string_type(mime: &str) -> Option<TypeHint> {
    match essence(mime).as_str() {
        "text/plain" | "text" => Some(TypeHint::Plaintext),
        "text/html" => Some(TypeHint::Html),
        "text/uri-list" | "url" => Some(TypeHint::UriList),
        "text/rtf" | "application/rtf" => Some(TypeHint::Rtf),
        _ => None,
    }
}

/// The extension of a MIME type in a table.
fn extension_in(table: &[(&'static str, &str)], mime: &str) -> Option<&'static str> {
    table.iter().find(|(_, known)| *known == mime).map(|(extension, _)| *extension)
}

/// The hint of a dropped file with the given MIME type.
pub(crate) fn hint_of_file_type(mime: &str) -> Option<TypeHint> {
    let mime = essence(mime);

    if mime.starts_with("image/") {
        let extension_hint = match mime.as_str() {
            "image/jpg" | "image/pjpeg" => Some("jpg"),
            "image/vnd.microsoft.icon" => Some("ico"),
            _ => extension_in(IMAGES, &mime),
        };
        Some(TypeHint::Image { extension_hint })
    } else if mime.starts_with("audio/") {
        let extension_hint = match mime.as_str() {
            "audio/mp3" => Some("mp3"),
            "audio/x-wav" | "audio/wave" => Some("wav"),
            _ => extension_in(AUDIO, &mime),
        };
        Some(TypeHint::Audio { extension_hint })
    } else {
        None
    }
}

/// The MIME type an outgoing hint is set under with `DataTransfer.setData`.
pub(crate) fn string_mime_of_hint(hint: TypeHint) -> Option<&'static str> {
    match hint {
        TypeHint::Plaintext => Some("text/plain"),
        TypeHint::Html => Some("text/html"),
        TypeHint::UriList => Some("text/uri-list"),
        TypeHint::Rtf => Some("text/rtf"),
        _ => None,
    }
}

/// Splits a `text/uri-list` into its URIs, without comments and empty lines.
pub(crate) fn parse_uri_list(list: &str) -> Vec<String> {
    list.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Joins URIs into a `text/uri-list`.
pub(crate) fn join_uri_list(uris: &[String]) -> String {
    uris.join("\r\n")
}

/// Whether a string read from a drop fits [`MAX_STRING_BYTES`].
pub(crate) fn string_fits(text: &str) -> bool {
    text.len() <= MAX_STRING_BYTES
}

/// What a type of an incoming drag refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// A string read with `DataTransfer.getData`.
    String,
    /// The file at this index of `DataTransfer.files`.
    File { index: usize, name: Option<String> },
}

/// A type offered by a drag on the web.
///
/// Strings carry their name in `DataTransfer.types`. Dropped files carry their MIME type and,
/// after the drop, their file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebTransferType {
    mime: String,
    source: Source,
    hint: Option<TypeHint>,
}

impl WebTransferType {
    /// A type read as a string.
    pub(crate) fn string(mime: &str) -> Self {
        Self { mime: mime.to_owned(), source: Source::String, hint: hint_of_string_type(mime) }
    }

    /// A dropped file, at `index` of `DataTransfer.files`.
    pub(crate) fn file(index: usize, mime: &str) -> Self {
        Self {
            mime: mime.to_owned(),
            source: Source::File { index, name: None },
            hint: hint_of_file_type(mime),
        }
    }

    /// The MIME type of a file, or the name of a string type in `DataTransfer.types`.
    #[must_use]
    pub fn mime(&self) -> &str {
        &self.mime
    }

    /// The name of a dropped file. `None` for strings, and for files before the drop.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        match &self.source {
            Source::File { name, .. } => name.as_deref(),
            Source::String => None,
        }
    }

    /// The index of a file in `DataTransfer.files`.
    pub(crate) fn file_index(&self) -> Option<usize> {
        match self.source {
            Source::File { index, .. } => Some(index),
            Source::String => None,
        }
    }

    /// Sets the file name read on the drop.
    pub(crate) fn set_file_name(&mut self, file_name: String) {
        if let Source::File { name, .. } = &mut self.source {
            *name = Some(file_name);
        }
    }

    /// Whether two types refer to the same data, whether or not the file name is known.
    fn same_data(&self, other: &Self) -> bool {
        match (&self.source, &other.source) {
            (Source::String, Source::String) => self.mime == other.mime,
            (Source::File { index, .. }, Source::File { index: other_index, .. }) => {
                index == other_index
            },
            _ => false,
        }
    }
}

impl TransferType for WebTransferType {
    fn hint(&self) -> Option<TypeHint> {
        self.hint
    }

    fn matches(&self, other: &dyn TransferType) -> bool {
        if let Some(other) = other.cast_ref::<Self>() {
            return self.same_data(other);
        }

        match (self.hint, other.hint()) {
            (Some(mine), Some(theirs)) => mine.matches(&theirs),
            _ => false,
        }
    }
}

/// Index of the first offered type matching `wanted`.
pub(crate) fn find_type(types: &[WebTransferType], wanted: &dyn TransferType) -> Option<usize> {
    types.iter().position(|offered| offered.matches(wanted))
}

/// The types of an incoming drag, as handed to the application.
#[derive(Debug, Clone)]
pub(crate) struct WebDataTransfer {
    pub(crate) types: Vec<WebTransferType>,
}

impl DataTransfer for WebDataTransfer {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> ControlFlow<()>,
    ) {
        for type_ in &self.types {
            if func(type_).is_break() {
                break;
            }
        }
    }
}

/// The content of one type of a drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Payload {
    Text(String),
    Bytes(Vec<u8>),
}

/// Data read from a drop.
#[derive(Debug)]
pub(crate) struct WebData {
    type_: WebTransferType,
    payload: Payload,
}

impl WebData {
    pub(crate) fn new(type_: WebTransferType, payload: Payload) -> Self {
        Self { type_, payload }
    }

    fn bytes(&self) -> &[u8] {
        match &self.payload {
            Payload::Text(text) => text.as_bytes(),
            Payload::Bytes(bytes) => bytes,
        }
    }
}

impl TypedData for WebData {
    fn type_(&self) -> &dyn TransferType {
        &self.type_
    }

    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        Some(Box::new(io::Cursor::new(self.bytes().to_vec())))
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        if self.type_.hint != Some(TypeHint::UriList) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "the data is not a URI list"));
        }

        Ok(parse_uri_list(&self.try_as_string()?))
    }

    fn try_as_string(&self) -> io::Result<String> {
        match &self.payload {
            Payload::Text(text) => Ok(text.clone()),
            Payload::Bytes(bytes) => String::from_utf8(bytes.clone())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_allowed_values_parse() {
        assert_eq!(Allowed::from_effect_allowed("copyMove"), Allowed {
            copy: true,
            move_: true,
            link: false
        });
        assert_eq!(Allowed::from_effect_allowed("uninitialized"), Allowed {
            copy: true,
            move_: true,
            link: true
        });
        assert_eq!(Allowed::from_effect_allowed("none"), Allowed::default());
        assert_eq!(Allowed::from_effect_allowed(""), Allowed::default());
        assert_eq!(Allowed::from_effect_allowed("COPY"), Allowed::default());
    }

    #[test]
    fn every_action_set_round_trips_through_effect_allowed() {
        let sets: &[&[DndAction]] = &[
            &[],
            &[DndAction::Copy],
            &[DndAction::Move],
            &[DndAction::Link],
            &[DndAction::Move, DndAction::Copy],
            &[DndAction::Copy, DndAction::Link],
            &[DndAction::Link, DndAction::Move],
            &[DndAction::Copy, DndAction::Move, DndAction::Link],
        ];

        for set in sets {
            let allowed = Allowed::from_effect_allowed(effect_allowed(set));
            for action in [DndAction::Copy, DndAction::Move, DndAction::Link] {
                assert_eq!(allowed.contains(action), set.contains(&action), "{set:?}");
            }
        }
    }

    #[test]
    fn ask_and_private_are_never_allowed() {
        let all = Allowed::from_effect_allowed("all");
        assert!(!all.contains(DndAction::Ask));
        assert!(!all.contains(DndAction::Private));
        assert_eq!(effect_allowed(&[DndAction::Ask, DndAction::Private]), "none");
    }

    #[test]
    fn drop_effects_map_both_ways() {
        for action in [DndAction::Copy, DndAction::Move, DndAction::Link] {
            assert_eq!(action_of_drop_effect(drop_effect(Some(action))), Some(action));
        }
        assert_eq!(drop_effect(None), "none");
        assert_eq!(drop_effect(Some(DndAction::Ask)), "none");
        assert_eq!(action_of_drop_effect("none"), None);
        assert_eq!(action_of_drop_effect("Move"), None);
        assert_eq!(action_of_drop_effect(""), None);
    }

    #[test]
    fn modifiers_request_actions() {
        assert_eq!(requested_action(false, false), None);
        assert_eq!(requested_action(true, false), Some(DndAction::Move));
        assert_eq!(requested_action(false, true), Some(DndAction::Copy));
        assert_eq!(requested_action(true, true), Some(DndAction::Link));
    }

    #[test]
    fn the_first_valid_action_the_source_allows_is_chosen() {
        let copy_move = Allowed::from_effect_allowed("copyMove");
        let valid = [DndAction::Move, DndAction::Copy];

        assert_eq!(choose_action(&valid, copy_move, None), Some(DndAction::Move));
        assert_eq!(
            choose_action(&valid, Allowed::from_effect_allowed("copy"), None),
            Some(DndAction::Copy)
        );
        assert_eq!(choose_action(&valid, Allowed::from_effect_allowed("link"), None), None);
        assert_eq!(choose_action(&[], copy_move, None), None);
        assert_eq!(
            choose_action(&[DndAction::Ask], Allowed::from_effect_allowed("all"), None),
            None
        );
    }

    #[test]
    fn a_requested_action_wins_when_both_sides_allow_it() {
        let all = Allowed::from_effect_allowed("all");
        let valid = [DndAction::Move, DndAction::Copy];

        assert_eq!(choose_action(&valid, all, Some(DndAction::Copy)), Some(DndAction::Copy));
        assert_eq!(choose_action(&valid, all, Some(DndAction::Link)), Some(DndAction::Move));
        assert_eq!(
            choose_action(&valid, Allowed::from_effect_allowed("move"), Some(DndAction::Copy)),
            Some(DndAction::Move)
        );
    }

    #[test]
    fn sizes_are_checked_against_limits() {
        assert!(within_limit(0.0, 10));
        assert!(within_limit(10.0, 10));
        assert!(!within_limit(10.5, 10));
        assert!(!within_limit(-1.0, 10));
        assert!(!within_limit(f64::NAN, 10));
        assert!(!within_limit(f64::INFINITY, u32::MAX));
        assert!(within_limit(0.0, 0));
        assert!(within_limit(f64::from(MAX_FILE_BYTES), MAX_FILE_BYTES));
        assert!(!within_limit(f64::from(MAX_FILE_BYTES) + 1.0, MAX_FILE_BYTES));
    }

    #[test]
    fn strings_up_to_the_limit_fit() {
        assert!(string_fits(""));
        assert!(string_fits(&"a".repeat(MAX_STRING_BYTES)));
        assert!(!string_fits(&"a".repeat(MAX_STRING_BYTES + 1)));
    }

    #[test]
    fn string_types_map_to_hints() {
        assert_eq!(hint_of_string_type("text/plain"), Some(TypeHint::Plaintext));
        assert_eq!(hint_of_string_type("Text/Plain;charset=utf-8"), Some(TypeHint::Plaintext));
        assert_eq!(hint_of_string_type("text/html"), Some(TypeHint::Html));
        assert_eq!(hint_of_string_type("text/uri-list"), Some(TypeHint::UriList));
        assert_eq!(hint_of_string_type("Files"), None);
        assert_eq!(hint_of_string_type("application/x-moz-file"), None);
        assert_eq!(hint_of_string_type(""), None);
    }

    #[test]
    fn file_types_map_to_hints() {
        assert_eq!(
            hint_of_file_type("image/png"),
            Some(TypeHint::Image { extension_hint: Some("png") })
        );
        assert_eq!(
            hint_of_file_type("image/jpeg"),
            Some(TypeHint::Image { extension_hint: Some("jpg") })
        );
        assert_eq!(
            hint_of_file_type("image/x-unknown"),
            Some(TypeHint::Image { extension_hint: None })
        );
        assert_eq!(
            hint_of_file_type("audio/mpeg"),
            Some(TypeHint::Audio { extension_hint: Some("mp3") })
        );
        assert_eq!(hint_of_file_type("application/pdf"), None);
        assert_eq!(hint_of_file_type(""), None);
    }

    #[test]
    fn outgoing_hints_have_types() {
        assert_eq!(string_mime_of_hint(TypeHint::Plaintext), Some("text/plain"));
        assert_eq!(string_mime_of_hint(TypeHint::Image { extension_hint: None }), None);
        assert_eq!(string_mime_of_hint(TypeHint::Html), Some("text/html"));
        assert_eq!(string_mime_of_hint(TypeHint::UriList), Some("text/uri-list"));
        assert_eq!(string_mime_of_hint(TypeHint::Audio { extension_hint: Some("mp3") }), None);
    }

    #[test]
    fn uri_lists_parse_and_join() {
        let list = "# comment\r\nhttps://a.example/\r\n\r\n  https://b.example/x  \n";
        assert_eq!(parse_uri_list(list), ["https://a.example/", "https://b.example/x"]);
        assert!(parse_uri_list("").is_empty());

        let uris = ["https://a.example/".to_owned(), "https://b.example/".to_owned()];
        assert_eq!(parse_uri_list(&join_uri_list(&uris)), uris);
        assert_eq!(join_uri_list(&[]), "");
    }

    #[test]
    fn types_match_hints_and_each_other() {
        let text = WebTransferType::string("text/plain");
        let png = WebTransferType::file(0, "image/png");
        let pdf = WebTransferType::file(1, "application/pdf");
        let types = [text.clone(), png.clone(), pdf.clone()];

        assert_eq!(find_type(&types, &TypeHint::Plaintext), Some(0));
        assert_eq!(find_type(&types, &TypeHint::Image { extension_hint: None }), Some(1));
        assert_eq!(find_type(&types, &TypeHint::Image { extension_hint: Some("png") }), Some(1));
        assert_eq!(find_type(&types, &TypeHint::Image { extension_hint: Some("gif") }), None);
        assert_eq!(find_type(&types, &TypeHint::Html), None);
        assert_eq!(find_type(&types, &pdf), Some(2));
        assert_eq!(find_type(&[], &TypeHint::Plaintext), None);

        let mut named = pdf.clone();
        named.set_file_name("a.pdf".to_owned());
        assert_eq!(find_type(&types, &named), Some(2));
        assert_eq!(named.file_name(), Some("a.pdf"));
        assert_eq!(text.file_name(), None);
        assert_eq!(png.file_index(), Some(0));
        assert_eq!(text.file_index(), None);
    }

    #[test]
    fn data_reads_as_text_bytes_and_uris() {
        let uris = WebData::new(
            WebTransferType::string("text/uri-list"),
            Payload::Text("https://a.example/\r\n".to_owned()),
        );
        assert_eq!(uris.try_as_uris().unwrap(), ["https://a.example/"]);
        assert_eq!(uris.try_as_bytes().unwrap(), b"https://a.example/\r\n");

        let text =
            WebData::new(WebTransferType::string("text/plain"), Payload::Text("hi".to_owned()));
        assert!(text.try_as_uris().is_err());
        assert_eq!(text.try_as_string().unwrap(), "hi");

        let invalid =
            WebData::new(WebTransferType::file(0, "image/png"), Payload::Bytes(vec![0xff]));
        assert_eq!(invalid.try_as_string().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(invalid.try_as_bytes().unwrap(), [0xff]);

        let empty = WebData::new(WebTransferType::file(0, "image/png"), Payload::Bytes(Vec::new()));
        assert_eq!(empty.try_as_bytes().unwrap(), b"");
        assert_eq!(empty.try_as_string().unwrap(), "");
    }
}
