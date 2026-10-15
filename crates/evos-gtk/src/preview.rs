//! The preview pane: images and text of the selected result.
//!
//! Content comes from the daemon, which reads files without changing their
//! access times and only if this user could read them.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib, pango};

use crate::backend::Backend;
use crate::row_item::RowItem;

/// Largest image shown.
const IMAGE_LIMIT: u32 = 16 << 20;
/// Text shown from the start of a file.
const TEXT_LIMIT: u32 = 256 << 10;

pub struct Preview {
    pub widget: gtk::Box,
    title: gtk::Label,
    details: gtk::Label,
    stack: gtk::Stack,
    picture: gtk::Picture,
    text: gtk::TextView,
    message: adw::StatusPage,
    backend: Backend,
    /// The path being shown, so late answers for older selections are
    /// dropped.
    current: RefCell<Vec<u8>>,
}

fn extension(name: &str) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => name[dot + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// Bytes that look like text: no NUL in the first few kilobytes, or UTF-16
/// with a byte order mark.
fn looks_like_text(data: &[u8]) -> bool {
    matches!(data.get(..2), Some([0xff, 0xfe] | [0xfe, 0xff]))
        || !data[..data.len().min(8192)].contains(&0)
}

impl Preview {
    pub fn new(backend: Backend) -> Rc<Self> {
        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .css_classes(["heading"])
            .build();
        let details = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .build();
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .build();
        let text = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(8)
            .right_margin(8)
            .top_margin(8)
            .bottom_margin(8)
            .build();
        let text_scroll = gtk::ScrolledWindow::builder()
            .child(&text)
            .vexpand(true)
            .build();
        let message = adw::StatusPage::builder()
            .icon_name("view-reveal-symbolic")
            .title("No Preview")
            .build();
        message.add_css_class("compact");
        let stack = gtk::Stack::builder().vexpand(true).build();
        stack.add_named(&message, Some("message"));
        stack.add_named(&picture, Some("image"));
        stack.add_named(&text_scroll, Some("text"));

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .width_request(280)
            // A named group, so screen readers (and the UI tests) find it.
            .accessible_role(gtk::AccessibleRole::Group)
            .build();
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(12)
            .margin_end(12)
            .margin_top(8)
            .build();
        header.append(&title);
        header.append(&details);
        widget.append(&header);
        widget.append(&stack);
        widget.update_property(&[gtk::accessible::Property::Label("Preview")]);

        let preview = Rc::new(Preview {
            widget,
            title,
            details,
            stack,
            picture,
            text,
            message,
            backend,
            current: RefCell::default(),
        });
        preview.show(None);
        preview
    }

    fn message(&self, icon: &str, title: &str, description: Option<&str>) {
        self.message.set_icon_name(Some(icon));
        self.message.set_title(title);
        self.message.set_description(description);
        self.stack.set_visible_child_name("message");
    }

    /// Shows the preview of `item` (or a hint when nothing is selected).
    pub fn show(self: &Rc<Self>, item: Option<RowItem>) {
        let Some(item) = item.filter(|i| i.loaded()) else {
            self.current.replace(Vec::new());
            self.title.set_text("");
            self.details.set_text("");
            return self.message("view-reveal-symbolic", "No Selection", None);
        };
        let path = item.path();
        if *self.current.borrow() == path {
            return;
        }
        self.current.replace(path.clone());
        let name = item.name();
        self.title.set_text(&name);
        let mut details = vec![item.type_name()];
        if !item.size().is_empty() {
            details.push(item.size());
        }
        if !item.modified().is_empty() {
            details.push(item.modified());
        }
        self.details.set_text(&details.join(" · "));
        if item.is_dir() {
            return self.message("folder-symbolic", "Folder", None);
        }
        let image = evos_core::types::IMAGE.contains(&extension(&name).as_str());
        let limit = if image { IMAGE_LIMIT } else { TEXT_LIMIT };
        self.message("content-loading-symbolic", "Loading…", None);
        let preview = self.clone();
        let wanted = path.clone();
        self.backend.call(
            move |c| c.read_file(&path, limit),
            move |result| {
                if *preview.current.borrow() != wanted {
                    return;
                }
                match result {
                    Ok(data) => preview.show_content(data, image),
                    Err(e) => preview.message("dialog-warning-symbolic", "No Preview", Some(&e)),
                }
            },
        );
    }

    fn show_content(&self, data: Vec<u8>, image: bool) {
        if image {
            match gdk::Texture::from_bytes(&glib::Bytes::from_owned(data)) {
                Ok(texture) => {
                    self.picture.set_paintable(Some(&texture));
                    self.stack.set_visible_child_name("image");
                }
                Err(_) => self.message(
                    "image-missing-symbolic",
                    "No Preview",
                    Some("This image format cannot be shown."),
                ),
            }
        } else if looks_like_text(&data) {
            let text = evos_core::props::decode_text(data);
            self.text.buffer().set_text(&String::from_utf8_lossy(&text));
            self.stack.set_visible_child_name("text");
        } else {
            self.message(
                "application-x-executable-symbolic",
                "No Preview",
                Some("Binary file"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_detection() {
        assert!(looks_like_text(b"hello\nworld"));
        assert!(looks_like_text("שלום".as_bytes()));
        assert!(looks_like_text(&[0xff, 0xfe, b'h', 0]));
        assert!(!looks_like_text(b"\x7fELF\x02\x01\x01\0\0"));
        assert!(looks_like_text(b""));
        assert_eq!(extension("a.Tar.GZ"), "gz");
        assert_eq!(extension(".bashrc"), "");
    }
}
