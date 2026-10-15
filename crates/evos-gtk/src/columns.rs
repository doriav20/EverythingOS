//! The result columns, as in Everything's column chooser.

use evos_proto::{Row, SortKey};

pub struct ColumnDef {
    /// Stable id, used in settings and on the command line.
    pub id: &'static str,
    pub title: &'static str,
    pub width: i32,
    pub default_visible: bool,
    pub sort: SortKey,
    /// The `RowItem` property shown.
    pub property: &'static str,
    /// The property holds Pango markup (highlighted search terms).
    pub markup: bool,
    /// Right-aligned numbers.
    pub numeric: bool,
}

const fn col(
    id: &'static str,
    title: &'static str,
    width: i32,
    default_visible: bool,
    sort: SortKey,
    property: &'static str,
) -> ColumnDef {
    ColumnDef {
        id,
        title,
        width,
        default_visible,
        sort,
        property,
        markup: false,
        numeric: false,
    }
}

pub const COLUMNS: &[ColumnDef] = &[
    ColumnDef {
        markup: true,
        ..col("name", "Name", 280, true, SortKey::Name, "name-markup")
    },
    ColumnDef {
        markup: true,
        ..col("path", "Path", 300, true, SortKey::Path, "folder-markup")
    },
    ColumnDef {
        numeric: true,
        ..col("size", "Size", 90, true, SortKey::Size, "size")
    },
    col(
        "extension",
        "Extension",
        80,
        false,
        SortKey::Extension,
        "extension",
    ),
    col("type", "Type", 170, false, SortKey::Type, "type-name"),
    col(
        "date-modified",
        "Date Modified",
        140,
        true,
        SortKey::Modified,
        "modified",
    ),
    col(
        "date-created",
        "Date Created",
        140,
        false,
        SortKey::Created,
        "created",
    ),
    col(
        "date-accessed",
        "Date Accessed",
        140,
        false,
        SortKey::Accessed,
        "accessed",
    ),
    col(
        "attributes",
        "Attributes",
        80,
        false,
        SortKey::Attributes,
        "attributes",
    ),
    col(
        "date-recently-changed",
        "Date Recently Changed",
        150,
        false,
        SortKey::RecentlyChanged,
        "changed",
    ),
    ColumnDef {
        numeric: true,
        ..col(
            "run-count",
            "Run Count",
            80,
            false,
            SortKey::RunCount,
            "run-count",
        )
    },
    col(
        "date-run",
        "Date Run",
        140,
        false,
        SortKey::DateRun,
        "date-run",
    ),
];

impl ColumnDef {
    /// The column's value for a row, as exported (sizes in bytes).
    pub fn text(&self, row: &Row) -> String {
        let date = |ns: i64| {
            if ns == 0 {
                String::new()
            } else {
                crate::format::date(ns)
            }
        };
        let name = String::from_utf8_lossy(&row.name);
        match self.sort {
            SortKey::Name => name.into_owned(),
            SortKey::Path => String::from_utf8_lossy(&row.folder).into_owned(),
            SortKey::Size => row.size.map(|s| s.to_string()).unwrap_or_default(),
            SortKey::Extension => match name.rfind('.') {
                Some(dot) if dot > 0 && !row.is_dir => name[dot + 1..].to_owned(),
                _ => String::new(),
            },
            SortKey::Type => row.type_name.clone(),
            SortKey::Modified => date(row.mtime),
            SortKey::Created => date(row.created),
            SortKey::Accessed => date(row.accessed),
            SortKey::Attributes => row.attributes.clone(),
            SortKey::RecentlyChanged => date(row.changed),
            SortKey::RunCount => row.run_count.to_string(),
            SortKey::DateRun => date(row.date_run),
        }
    }
}

pub fn by_id(id: &str) -> Option<&'static ColumnDef> {
    COLUMNS.iter().find(|c| c.id == id)
}

/// The column showing a sort key.
pub fn for_sort(key: SortKey) -> &'static ColumnDef {
    COLUMNS
        .iter()
        .find(|c| c.sort == key)
        .expect("every sort key has a column")
}

/// Parses a `;`-separated column list as Everything's `-columns` takes
/// it, accepting sort names and their short forms (`dm`, `rc`, ...).
pub fn parse_list(list: &str) -> Vec<&'static str> {
    list.split([';', ','])
        .filter_map(|name| {
            let name = name.trim();
            by_id(&name.to_ascii_lowercase())
                .or_else(|| SortKey::parse(name).map(for_sort))
                .map(|c| c.id)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sort_key_has_one_column() {
        for key in SortKey::ALL {
            assert_eq!(for_sort(key).sort, key);
        }
        assert_eq!(COLUMNS.len(), SortKey::ALL.len());
    }

    #[test]
    fn column_lists() {
        assert_eq!(
            parse_list("Name;dm;Run-Count;bogus;size"),
            ["name", "date-modified", "run-count", "size"]
        );
    }
}
