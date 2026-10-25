//! Parse and write throughput over the generated corpora: the LSP set,
//! sway's tree and the three classic shapes. Pin a core and take the best
//! of a few runs, the machine is rarely idle and the clock moves.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use stein_json::{Node, ROOT, count, parse, parse_vec};

#[path = "../src/parse/tests/corpus.rs"]
mod corpus;

fn parsing(c: &mut Criterion) {
    for corpus in corpus::all() {
        let src = corpus.text.as_bytes();
        let n = count(src).unwrap_or(0);
        let mut nodes = vec![Node::EMPTY; n];
        let mut grown = Vec::new();
        let mut g = c.benchmark_group(corpus.name);
        g.throughput(Throughput::Bytes(src.len() as u64));
        g.bench_function("count", |b| b.iter(|| count(black_box(src))));
        g.bench_function("parse", |b| {
            b.iter(|| parse(black_box(src), &mut nodes).map(|d| d.nodes().len()))
        });
        g.bench_function("parse_vec", |b| {
            b.iter(|| parse_vec(black_box(src), &mut grown).map(|d| d.nodes().len()))
        });
        g.finish();
    }
}

/// A thousand didChange messages, since one is too small to time.
fn small_messages(c: &mut Criterion) {
    let msgs: Vec<String> = (0..1000).map(corpus::did_change).collect();
    let bytes: usize = msgs.iter().map(String::len).sum();
    let mut nodes = [Node::EMPTY; 64];
    let mut grown = Vec::new();
    let mut g = c.benchmark_group("lsp_did_change_x1000");
    g.throughput(Throughput::Bytes(bytes as u64));
    g.bench_function("parse", |b| {
        b.iter(|| {
            let mut total = 0;
            for m in &msgs {
                total += parse(black_box(m.as_bytes()), &mut nodes).map_or(0, |d| d.nodes().len());
            }
            total
        })
    });
    g.bench_function("parse_vec", |b| {
        b.iter(|| {
            let mut total = 0;
            for m in &msgs {
                total +=
                    parse_vec(black_box(m.as_bytes()), &mut grown).map_or(0, |d| d.nodes().len());
            }
            total
        })
    });
    g.finish();
}

/// The token array of a semantic tokens response, decoded to `u32`s once
/// parsed, which is what an editor does with it.
fn tokens(c: &mut Criterion) {
    let src = corpus::semantic_tokens(400_000);
    let src = src.as_bytes();
    let mut nodes = Vec::new();
    let doc = parse_vec(src, &mut nodes).ok();
    let Some(doc) = doc else {
        return;
    };
    let data = doc
        .get(ROOT, "result")
        .and_then(|r| doc.get(r, "data"))
        .unwrap_or(ROOT);
    let mut out = Vec::new();
    let mut g = c.benchmark_group("lsp_semantic_tokens");
    g.throughput(Throughput::Bytes(src.len() as u64));
    g.bench_function("u32s", |b| b.iter(|| doc.u32s(black_box(data), &mut out)));
    g.finish();
}

fn writing(c: &mut Criterion) {
    let plain: String = corpus::WORDS_TEXT.repeat(8192);
    let mut rng = corpus::Rng(9);
    let mut noisy = String::new();
    while noisy.len() < plain.len() {
        noisy.push_str(rng.pick(&["plain words ", "a \"quote\" ", "line\nbreak ", "tab\there "]));
    }
    let mut out = vec![0u8; plain.len() * 2 + 16];
    let mut g = c.benchmark_group("write_string");
    g.throughput(Throughput::Bytes(plain.len() as u64));
    g.bench_function("plain", |b| {
        b.iter(|| stein_json::write::string(&mut out, 0, black_box(&plain)))
    });
    g.throughput(Throughput::Bytes(noisy.len() as u64));
    g.bench_function("escapes", |b| {
        b.iter(|| stein_json::write::string(&mut out, 0, black_box(&noisy)))
    });
    g.finish();
}

fn config() -> Criterion {
    Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_millis(1500))
}

criterion_group! {
    name = benches;
    config = config();
    targets = parsing, small_messages, tokens, writing
}
criterion_main!(benches);
