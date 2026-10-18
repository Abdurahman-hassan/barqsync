//! Evaluate block / chunk sizing rules for barqsync's delta on file pairs:
//!   rules OLD NEW [OLD NEW ...]
//! For each rule: the wire size of the receiver's signature (postcard), the literal bytes
//! and ops of the delta, for aligned blocks, content-defined chunks, and both merged.
use barqsync::delta::{self, Op, Params};
use std::fs::File;
use std::time::Instant;

fn unit(size: u64, div: f64) -> usize {
    (((size as f64).sqrt() / div) as usize).next_power_of_two().clamp(4 << 10, 64 << 10)
}

fn wire<T: serde::Serialize>(v: &T) -> usize {
    postcard::to_allocvec(v).unwrap().len()
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    println!("file\trule\tunit\tsame_size\tsig_blocks\tlit_blocks\tops_blocks\tsig_chunks\tlit_chunks\tops_chunks\tlit_merged\tops_merged\tms");
    for pair in a.chunks(2) {
        let (of, nf) = (File::open(&pair[0]).unwrap(), File::open(&pair[1]).unwrap());
        let (os, ns) = (of.metadata().unwrap().len(), nf.metadata().unwrap().len());
        let name = std::path::Path::new(&pair[1]).file_name().unwrap().to_string_lossy().to_string();
        let rules: Vec<(&str, Params)> = vec![
            ("fixed 64K/32K (current)", Params::DEFAULT),
            ("sqrt", scaled(unit(os, 1.0))),
            ("sqrt/2", scaled(unit(os, 2.0))),
            ("sqrt/4", scaled(unit(os, 4.0))),
            ("fixed 4K", scaled(4 << 10)),
        ];
        for (rule, p) in rules {
            let t = Instant::now();
            let (bo, bn) = (delta::block_signature(&of, os, &p).unwrap(), delta::block_signature(&nf, ns, &p).unwrap());
            let (co, cn) = (delta::chunk_signature(&of, os, &p).unwrap(), delta::chunk_signature(&nf, ns, &p).unwrap());
            let (db, dc) = (delta::diff_blocks(&bn, &bo), delta::diff_chunks(&cn, &co));
            let dm = delta::merge_in_place(&dc, &db);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let ops = |d: &delta::Delta| wire(&d.ops.iter().copied().collect::<Vec<Op>>());
            println!(
                "{name}\t{rule}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{ms:.0}",
                p.block_size,
                os == ns,
                wire(&bo),
                db.literal_bytes(),
                ops(&db),
                wire(&co),
                dc.literal_bytes(),
                ops(&dc),
                dm.literal_bytes(),
                ops(&dm)
            );
            assert!(dm.literal_bytes() <= db.literal_bytes().min(dc.literal_bytes()));
        }
    }
}

fn scaled(u: usize) -> Params {
    Params { block_size: u, cdc_min: u / 2, cdc_avg: u, cdc_max: u * 2, ..Params::DEFAULT }
}
