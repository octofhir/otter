//! Parse each file given with oxc 0.139 and 0.154, interleaved, and print
//! the best of N parses per release.
use std::time::Instant;

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    let reps = 7;
    let (mut old_total, mut new_total) = (0.0, 0.0);
    for path in &files {
        let text = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
        let (mut old_best, mut new_best) = (f64::MAX, f64::MAX);
        for _ in 0..reps {
            let alloc = old_alloc::Allocator::default();
            let t = Instant::now();
            let r = old_parser::Parser::new(&alloc, &text, old_span::SourceType::cjs()).parse();
            old_best = old_best.min(t.elapsed().as_secs_f64() * 1e3);
            std::hint::black_box(r.program.body.len());
            let alloc = new_alloc::Allocator::default();
            let t = Instant::now();
            let r = new_parser::Parser::new(&alloc, &text, new_span::SourceType::cjs()).parse();
            new_best = new_best.min(t.elapsed().as_secs_f64() * 1e3);
            std::hint::black_box(r.program.body.len());
        }
        old_total += old_best;
        new_total += new_best;
        println!("{:>8.2} {:>8.2} ms  {}", old_best, new_best, path.rsplit('/').next().unwrap());
    }
    println!("{:>8.2} {:>8.2} ms  total (0.139 / 0.154)", old_total, new_total);
}
