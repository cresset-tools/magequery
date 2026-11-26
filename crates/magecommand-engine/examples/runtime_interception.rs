//! Print the developer-mode runtime `interception` cache payload as JSON
//! (keys sorted), for diffing against the entry Magento saved.
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("magento root"));
    let t = std::time::Instant::now();
    let magento = magequery_core::Magento::open(&root).expect("open magento");
    let defs = magecommand_engine::interception::runtime_definitions(&magento, &root);
    let map = magecommand_engine::interception::runtime_interception_map(&magento, &defs);
    eprintln!("{} entries in {:?}", map.len(), t.elapsed());
    let body: Vec<String> = map
        .iter()
        .map(|(k, v)| format!("{}:{}", serde_json::to_string(k).unwrap(), v))
        .collect();
    println!("{{{}}}", body.join(","));
}
