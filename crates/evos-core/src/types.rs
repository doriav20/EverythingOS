//! File type classification: the extension lists behind the `audio:`,
//! `video:`, `pic:`, `doc:`, `zip:` and `exe:` macros, and human-readable
//! type descriptions for the Type column and the `type:` function.

use crate::store::extension;

pub const AUDIO: &[&str] = &[
    "aac", "ac3", "aif", "aifc", "aiff", "amr", "ape", "au", "dsf", "flac", "m4a", "m4b", "mid",
    "midi", "mka", "mp2", "mp3", "mpa", "oga", "ogg", "opus", "ra", "wav", "wma", "wv",
];

pub const VIDEO: &[&str] = &[
    "3g2", "3gp", "avi", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts", "ogm",
    "ogv", "rm", "ts", "vob", "webm", "wmv",
];

pub const IMAGE: &[&str] = &[
    "avif", "bmp", "gif", "heic", "heif", "ico", "jpeg", "jpg", "jxl", "png", "psd", "raw", "svg",
    "tga", "tif", "tiff", "webp", "xcf",
];

pub const DOCUMENT: &[&str] = &[
    "c", "chm", "cpp", "csv", "doc", "docx", "epub", "h", "htm", "html", "json", "md", "odp",
    "ods", "odt", "pdf", "ppt", "pptx", "rtf", "tex", "txt", "xls", "xlsx", "xml",
];

pub const COMPRESSED: &[&str] = &[
    "7z", "ace", "arj", "bz2", "cab", "deb", "gz", "iso", "lz", "lz4", "lzma", "rar", "rpm", "tar",
    "tbz2", "tgz", "txz", "xz", "zip", "zst",
];

pub const EXECUTABLE: &[&str] = &["appimage", "bin", "run", "sh"];

/// Descriptions for well-known extensions.
fn known(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "txt" => "Text Document",
        "md" => "Markdown Document",
        "pdf" => "PDF Document",
        "doc" | "docx" | "odt" | "rtf" => "Word Processing Document",
        "xls" | "xlsx" | "ods" | "csv" => "Spreadsheet",
        "ppt" | "pptx" | "odp" => "Presentation",
        "epub" => "E-book",
        "htm" | "html" => "HTML Document",
        "xml" => "XML Document",
        "json" => "JSON Document",
        "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg" => "Configuration File",
        "log" => "Log File",
        "png" => "PNG Image",
        "jpg" | "jpeg" => "JPEG Image",
        "gif" => "GIF Image",
        "bmp" => "Bitmap Image",
        "webp" => "WebP Image",
        "svg" => "SVG Image",
        "tif" | "tiff" => "TIFF Image",
        "heic" | "heif" => "HEIF Image",
        "avif" => "AVIF Image",
        "ico" => "Icon Image",
        "psd" | "xcf" => "Layered Image",
        "mp3" => "MP3 Audio",
        "flac" => "FLAC Audio",
        "wav" => "WAV Audio",
        "ogg" | "oga" | "opus" => "Ogg Audio",
        "m4a" | "aac" => "AAC Audio",
        "mp4" | "m4v" => "MP4 Video",
        "mkv" => "Matroska Video",
        "webm" => "WebM Video",
        "avi" => "AVI Video",
        "mov" => "QuickTime Video",
        "zip" => "ZIP Archive",
        "7z" => "7-Zip Archive",
        "rar" => "RAR Archive",
        "tar" => "Tar Archive",
        "gz" | "tgz" => "Gzip Archive",
        "xz" | "txz" => "XZ Archive",
        "bz2" | "tbz2" => "Bzip2 Archive",
        "zst" => "Zstandard Archive",
        "iso" => "Disc Image",
        "deb" => "Debian Package",
        "rpm" => "RPM Package",
        "appimage" => "AppImage Application",
        "sh" | "bash" | "zsh" => "Shell Script",
        "py" => "Python Source",
        "rs" => "Rust Source",
        "c" => "C Source",
        "h" | "hpp" => "C Header",
        "cpp" | "cc" | "cxx" => "C++ Source",
        "go" => "Go Source",
        "java" => "Java Source",
        "js" | "mjs" => "JavaScript Source",
        "ts" => "TypeScript Source",
        "css" => "Stylesheet",
        "sql" => "SQL Script",
        "desktop" => "Desktop Entry",
        "so" => "Shared Library",
        "o" | "a" => "Object File",
        "efu" => "Everything File List",
        _ => return None,
    })
}

/// The type description of an entry: "Folder", a known description such as
/// "PNG Image", or "XYZ File" for other extensions.
pub fn type_name(name: &[u8], is_dir: bool, executable: bool) -> String {
    if is_dir {
        return "Folder".to_owned();
    }
    let ext = String::from_utf8_lossy(extension(name)).to_lowercase();
    if let Some(desc) = known(&ext) {
        return desc.to_owned();
    }
    if ext.is_empty() {
        return if executable {
            "Program".to_owned()
        } else {
            "File".to_owned()
        };
    }
    format!("{} File", ext.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions() {
        assert_eq!(type_name(b"photos", true, false), "Folder");
        assert_eq!(type_name(b"a.PNG", false, false), "PNG Image");
        assert_eq!(type_name(b"notes.txt", false, false), "Text Document");
        assert_eq!(type_name(b"x.qqq", false, false), "QQQ File");
        assert_eq!(type_name(b"Makefile", false, false), "File");
        assert_eq!(type_name(b"tool", false, true), "Program");
        assert_eq!(type_name(".bashrc".as_bytes(), false, false), "File");
        assert_eq!(
            type_name("מסמך.pdf".as_bytes(), false, false),
            "PDF Document"
        );
    }
}
