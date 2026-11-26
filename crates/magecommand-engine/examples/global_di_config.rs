//! Print the `GLOBAL__DICONFIG` payload (PHP-serialized), for diffing against
//! the entry Magento saved.
use std::io::Write;
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).expect("magento root"));
    let t = std::time::Instant::now();
    let magento = magequery_core::Magento::open(&root).expect("open magento");
    let (defs, _) = magecommand_engine::interception::runtime_definitions(&magento, &root).expect("di scopes");
    eprintln!("open + runtime definitions: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    match magecommand_engine::runtimecache::global_di_config(&magento, defs) {
        Ok(s) => {
            eprintln!("{} bytes in {:?}", s.len(), t.elapsed());
            std::io::stdout().write_all(s.as_bytes()).unwrap();
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
