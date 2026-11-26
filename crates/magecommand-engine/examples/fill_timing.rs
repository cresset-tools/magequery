//! Time each step of dev upgrade's cache fill (computes, does not write).
use std::time::Instant;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let root = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let t0 = Instant::now();
    let mut t = Instant::now();
    let mut lap = |what: &str| {
        eprintln!("{what:<28} {:>7.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        t = Instant::now();
    };
    let mage = magequery_core::Magento::open(&root).unwrap();
    lap("open");
    let (defs, scopes) = magecommand_engine::interception::runtime_definitions(&mage, &root).unwrap();
    lap("runtime definitions");
    let map = magecommand_engine::interception::runtime_interception_map(&defs, &scopes);
    let json = serde_json::to_string(&map).unwrap();
    lap("interception map + json");
    let di = magecommand_engine::runtimecache::global_di_config(&mage, defs).unwrap();
    lap("global DI config");
    eprintln!("{:<28} {:>7.1} ms ({} + {} bytes)", "total", t0.elapsed().as_secs_f64() * 1000.0, json.len(), di.len());
}
