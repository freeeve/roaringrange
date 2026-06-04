//! Embed text lines (one per stdin line) with a model2vec `RRM2` artifact and
//! print each embedding as space-separated f32s — the harness for validating the
//! in-browser embedder against Python model2vec.
//!
//! ```sh
//! printf 'hello world\nCRISPR-Cas9\n' | \
//!   cargo run --release --example m2v_embed --features vector -- potion.rrm2
//! ```

use roaringrange::Model2vec;
use std::io::BufRead;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: m2v_embed <model.rrm2>   (texts on stdin, one per line)");
        std::process::exit(2);
    }
    let bytes = std::fs::read(&args[1]).expect("read rrm2");
    let m = Model2vec::from_bytes(&bytes).expect("parse rrm2");
    let stdin = std::io::stdin();
    let mut out = String::new();
    for line in stdin.lock().lines() {
        let v = m.embed(&line.expect("stdin line"));
        let row: Vec<String> = v.iter().map(|x| format!("{x:.6}")).collect();
        out.push_str(&row.join(" "));
        out.push('\n');
    }
    print!("{out}");
}
