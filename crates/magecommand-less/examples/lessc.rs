//! Dev tool: compile one `.less` file to stdout (fixture debugging).
//!
//! ```sh
//! cargo run -p magecommand-less --example lessc -- path/to/file.less
//! ```

use magecommand_less::{
    compile, FileInfo, ImportError, ImportPayload, ImportRequest, ImportResolver, LessOptions,
    ResolvedImport,
};
use std::path::PathBuf;
use std::sync::Arc;

struct FsResolver {
    root: PathBuf,
}

impl ImportResolver for FsResolver {
    fn resolve(&self, req: &ImportRequest) -> Result<ResolvedImport, ImportError> {
        let raw = req.path.as_str();
        let force_css = req.options.css == Some(true);
        let force_less = req.options.css == Some(false);
        let is_css = force_css || (!force_less && raw.ends_with(".css"));

        let mut candidate = self.root.join(raw);
        if candidate.extension().is_none() && !is_css {
            candidate.set_extension("less");
        }

        let bytes = std::fs::read_to_string(&candidate).map_err(|e| ImportError::Io {
            path: candidate.display().to_string(),
            message: e.to_string(),
        })?;

        let dir = candidate
            .parent()
            .map(|p| format!("{}/", p.display()))
            .unwrap_or_default();
        let file = FileInfo {
            filename: candidate.display().to_string(),
            current_directory: dir,
            ..FileInfo::default()
        };

        let payload = if is_css {
            ImportPayload::Css(Arc::from(bytes.as_str()))
        } else if req.options.inline {
            ImportPayload::Inline(Arc::from(bytes.as_str()))
        } else {
            ImportPayload::Less(Arc::from(bytes.as_str()))
        };

        Ok(ResolvedImport { file, payload })
    }

    fn load_binary(&self, path: &str, current_directory: &str) -> Option<Vec<u8>> {
        let base = if current_directory.is_empty() {
            self.root.clone()
        } else {
            PathBuf::from(current_directory)
        };
        std::fs::read(base.join(path)).ok()
    }
}

fn main() {
    let path = PathBuf::from(std::env::args().nth(1).expect("usage: lessc <file.less>"));
    let src = std::fs::read_to_string(&path).expect("read input");
    let opts = LessOptions {
        filename: Some(path.display().to_string()),
        ..LessOptions::default()
    };
    let resolver = FsResolver {
        root: path.parent().map(PathBuf::from).unwrap_or_default(),
    };
    match compile(&src, &opts, &resolver) {
        Ok(css) => print!("{}", css.code),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
