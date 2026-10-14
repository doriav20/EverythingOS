//! Search, sort and snapshot benchmarks on a synthetic index.
//!
//! The index size defaults to one million entries; set `EVOS_BENCH_ENTRIES`
//! (e.g. to 10000000) to measure larger volumes.

use std::hint::black_box;
use std::sync::atomic::AtomicBool;

use criterion::{criterion_group, criterion_main, Criterion};
use evos_core::query::{parse, ParseOptions};
use evos_core::search::Query;
use evos_core::sort::{sort_hits, SortKey, SortSpec};
use evos_core::{persist, EntryId, Index, Meta, ROOT};

const WORDS: &[&str] = &[
    "report", "Invoice", "photo", "IMG", "backup", "notes", "Draft", "final", "src", "lib", "main",
    "test", "data", "Config", "readme", "video", "music", "Project", "build", "cache",
];
const EXTS: &[&str] = &[
    "txt", "jpg", "png", "rs", "pdf", "mp4", "json", "md", "tar.gz", "log",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

/// Builds a tree with roughly 1 folder per 12 files.
fn synthetic(entries: usize) -> Index {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut ix = Index::new(b"/data", Meta::dir(0));
    let mut dirs: Vec<EntryId> = vec![ROOT];
    for i in 0..entries {
        let parent = dirs[(rng.next() % dirs.len() as u64) as usize];
        let base = format!("{}_{}{}", rng.pick(WORDS), rng.pick(WORDS), i);
        if i % 13 == 0 {
            dirs.push(ix.insert(parent, base.as_bytes(), Meta::dir(i as i64)));
        } else {
            let name = format!("{base}.{}", rng.pick(EXTS));
            let size = rng.next() % (64 << 20);
            ix.insert(
                parent,
                name.as_bytes(),
                Meta::file(size, (rng.next() % (1 << 50)) as i64),
            );
        }
    }
    ix
}

fn bench(c: &mut Criterion) {
    let entries: usize = std::env::var("EVOS_BENCH_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let ix = synthetic(entries);
    let vols = [(0u16, &ix)];
    let cancel = AtomicBool::new(false);
    let opts = ParseOptions::default();

    let mut group = c.benchmark_group(format!("search_{entries}"));
    for (label, q) in [
        ("substring", "invoice"),
        ("two_terms", "report final"),
        ("wildcard_ext", "*.jpg"),
        ("path_substring", "photo_/src"),
        ("ext_and_size", "ext:mp4;pdf size:>16mb"),
        ("regex", "regex:^img_.*\\d{3}\\.png$"),
        ("everything", ""),
    ] {
        let query = Query::compile(&parse(q, &opts)).unwrap();
        group.bench_function(label, |b| {
            b.iter(|| black_box(query.run(&vols, None, &cancel).unwrap().len()))
        });
    }
    group.finish();

    let all = Query::compile(&parse("", &opts))
        .unwrap()
        .run(&vols, None, &cancel)
        .unwrap();
    let mut group = c.benchmark_group(format!("sort_{entries}"));
    group.sample_size(10);
    for key in [SortKey::Name, SortKey::Path, SortKey::Size] {
        group.bench_function(format!("{key:?}"), |b| {
            b.iter_batched_ref(
                || all.clone(),
                |hits| {
                    sort_hits(
                        hits,
                        &vols,
                        SortSpec {
                            key,
                            descending: false,
                        },
                    )
                },
                criterion::BatchSize::LargeInput,
            )
        });
    }
    group.finish();

    let mut snapshot = Vec::new();
    persist::write_snapshot(&ix, b"", &mut snapshot).unwrap();
    let mut group = c.benchmark_group(format!("snapshot_{entries}"));
    group.sample_size(10);
    group.bench_function("write", |b| {
        b.iter(|| {
            let mut out = Vec::with_capacity(snapshot.len());
            persist::write_snapshot(&ix, b"", &mut out).unwrap();
            black_box(out.len())
        })
    });
    group.bench_function("load", |b| {
        b.iter(|| black_box(persist::read_snapshot(snapshot.as_slice()).unwrap().0.len()))
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
