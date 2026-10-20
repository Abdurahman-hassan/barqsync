//! How much barqsync's delta would send with smaller blocks / chunks: granularity OLD NEW
use barqsync::delta::{self, Params};
use std::fs::File;
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (of, nf) = (File::open(&a[0]).unwrap(), File::open(&a[1]).unwrap());
    let (os, ns) = (of.metadata().unwrap().len(), nf.metadata().unwrap().len());
    for bs in [64 << 10, 16 << 10, 4 << 10] {
        let p = Params { block_size: bs, ..Params::DEFAULT };
        let t = Instant::now();
        let d = delta::diff_blocks(&delta::block_signature(&nf, ns, &p).unwrap(), &delta::block_signature(&of, os, &p).unwrap());
        println!("blocks {:>2} KiB: literal {:>7.2} MB, signature {:>6.0} KB, {:>4.0} ms", bs >> 10, d.literal_bytes() as f64 / 1e6, (os / bs as u64 * 16) as f64 / 1e3, t.elapsed().as_secs_f64() * 1e3);
    }
    for (min, avg, max) in [(16 << 10, 32 << 10, 64 << 10), (4 << 10, 8 << 10, 16 << 10), (2 << 10, 4 << 10, 8 << 10)] {
        let p = Params { cdc_min: min, cdc_avg: avg, cdc_max: max, ..Params::DEFAULT };
        let t = Instant::now();
        let (o, n) = (delta::chunk_signature(&of, os, &p).unwrap(), delta::chunk_signature(&nf, ns, &p).unwrap());
        let d = delta::diff_chunks(&n, &o);
        println!("chunks avg {:>2} KiB: literal {:>7.2} MB, signature {:>6.0} KB, {:>4.0} ms", avg >> 10, d.literal_bytes() as f64 / 1e6, (o.chunks.len() * 28) as f64 / 1e3, t.elapsed().as_secs_f64() * 1e3);
    }
}
