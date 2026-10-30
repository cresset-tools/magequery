//! The committed corpus gate (plan `.plans/magecommand-php-corpus.md`):
//! `cargo test -p magecommand-php` validates the parser with zero
//! environment, on every machine and in CI.
//!
//! Two parts:
//!   - `corpus/coverage/` — hand-written PHP, one file per construct family
//!     plus a few deliberate stress shapes; the manifest pins exact
//!     declaration counts and a stable per-file fingerprint digest. Every
//!     file is synthetic *on purpose*: a corpus of sampled real code was
//!     tried and removed (see `NOTES.md`, "why the corpus is synthetic") —
//!     it carried 79k lines to pin 504 declarations and contributed no
//!     construct this tier lacks.
//!   - `corpus/manifest.tsv` + `corpus/xfail.txt` — the gate. Zero-issue bar:
//!     every parse issue must be an xfail.txt entry, and every entry must
//!     actually issue (both directions — a fixed xfail must be removed, not
//!     forgotten).
//!
//! The manifest digest covers a small stable `fingerprint(meta)` rendered by
//! this test ([`FINGERPRINT_VERSION`] — fqcn, kind, flags, hierarchy, method
//! signatures, constants, cases) — NOT `{:?}` of `ClassMeta`, whose Debug
//! churns with every model edit. Intentional parser changes re-bless with:
//!
//! ```sh
//! MAGECOMMAND_CORPUS_BLESS=1 cargo test -p magecommand-php --test corpus
//! ```
//!
//! which rewrites the manifest, shows a per-file diff, and lists xfail
//! candidates. A plain run afterwards must be green.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use magecommand_php::{ClassKind, FileMeta, Visibility};
use sha2::{Digest, Sha256};

/// Bump when [`fingerprint`]'s rendering changes. A mismatch against the
/// manifest with an unchanged version is a parser regression; a version
/// bump forces a full re-bless (deliberate, reviewed via the manifest diff)
/// and is enforced — the version is written into manifest.tsv and asserted
/// on read, so digests from two renderings can never be compared.
///
/// v2: trait adaptations (`insteadof` / `as`) joined the rendering, and the
/// param line dropped a seventh column that restated `readonly` + promotion
/// already present in columns 2 and 3.
///
/// v3: declared properties joined the rendering (`F` lines: name, visibility,
/// static/readonly/hooked flags, type, default), now that the parser models
/// them for `new X()` parameter defaults.
const FINGERPRINT_VERSION: u32 = 3;

/// How many declarations of a fingerprint to show on a mismatch.
const FINGERPRINT_PRINT_LINES: usize = 12;

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn collect_php(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            // Irrelevant for `corpus/coverage/`, load-bearing for the
            // big-install smoke below: a real install carries per-package
            // `.git` dirs and `vendor/*/node_modules`. `differential.rs`
            // skips the same two, so both tools see the same file set.
            let name = entry.file_name();
            if name == ".git" || name == "node_modules" {
                continue;
            }
            collect_php(&path, out);
        } else if path.extension().is_some_and(|e| e == "php") {
            out.push(path);
        }
    }
}

// ---- manifest --------------------------------------------------------------

/// One manifest row. Columns: path, decls, methods, consts, cases,
/// file_sha256 (the committed file bytes), fp_sha256 (the rendered
/// fingerprint). The file hash catches accidental edits of the corpus
/// itself; the fingerprint catches parser-output drift. Both must match.
#[derive(Clone, PartialEq)]
struct Row {
    path: String,
    decls: usize,
    methods: usize,
    consts: usize,
    cases: usize,
    file_sha256: String,
    fp_sha256: String,
}

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.path, self.decls, self.methods, self.consts, self.cases, self.file_sha256,
            self.fp_sha256
        )
    }
}

/// The `fingerprint-version:` line is DATA, not commentary: it is written
/// from [`FINGERPRINT_VERSION`] and asserted on read, so a manifest blessed
/// under one rendering can never be checked against another. The prose after
/// it is for whoever opens the file.
const MANIFEST_HEADER: &str = concat!(
    "# magecommand-php corpus manifest — the regression gate for tests/corpus.rs.\n",
    "# path <TAB> decls <TAB> methods <TAB> consts <TAB> cases <TAB> file_sha256 <TAB> fp_sha256\n",
    "# fp_sha256 hashes the stable fingerprint(meta) rendered by the test:\n",
    "#   per declaration: kind, fqcn, flags (abstract/final/readonly/conditional), extends,\n",
    "#   implements, traits, trait adaptations (insteadof + as), class attributes, enum\n",
    "#   backing/cases, every constant (name, visibility, type, raw value), every property\n",
    "#   (name, visibility, static/readonly/hooked, type, raw default) and every method\n",
    "#   signature (visibility, static/abstract/final/returns-ref, return type, every param\n",
    "#   incl. types, defaults, promotion, readonly, by-ref, variadic). NOT ClassMeta's Debug.\n",
    "#   Re-bless intentional changes:\n",
    "#   MAGECOMMAND_CORPUS_BLESS=1 cargo test -p magecommand-php --test corpus\n"
);

const VERSION_PREFIX: &str = "# fingerprint-version: ";

fn manifest_path() -> PathBuf {
    corpus_dir().join("manifest.tsv")
}

/// Returns the version the manifest was blessed under (`None` for a manifest
/// predating the line) alongside its rows. The version is *reported*, never
/// asserted here: bless mode has to be able to read a manifest of any
/// version to diff against it — that is the whole point of a version bump.
fn read_manifest() -> (Option<u32>, BTreeMap<String, Row>) {
    let text = fs::read_to_string(manifest_path()).expect("read manifest.tsv");
    let recorded: Option<u32> = text
        .lines()
        .find_map(|l| l.strip_prefix(VERSION_PREFIX))
        .map(|v| v.trim().parse().expect("fingerprint-version is a number"));
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 7, "malformed manifest row: {line:?}");
        let row = Row {
            path: cols[0].to_owned(),
            decls: cols[1].parse().expect("decls count"),
            methods: cols[2].parse().expect("methods count"),
            consts: cols[3].parse().expect("consts count"),
            cases: cols[4].parse().expect("cases count"),
            file_sha256: cols[5].to_owned(),
            fp_sha256: cols[6].to_owned(),
        };
        assert!(
            out.insert(row.path.clone(), row).is_none(),
            "duplicate manifest row: {line:?}"
        );
    }
    (recorded, out)
}

fn write_manifest(rows: &[Row]) {
    let mut text = String::from(MANIFEST_HEADER);
    text.push_str(&format!("{VERSION_PREFIX}{FINGERPRINT_VERSION}\n"));
    for row in rows {
        text.push_str(&row.to_string());
        text.push('\n');
    }
    fs::write(manifest_path(), text).expect("write manifest.tsv");
}

// ---- xfail -----------------------------------------------------------------

/// The pinned exception list: `path` (relative to corpus/) per line, with an
/// optional trailing reason after ` — `. Pinned EXACTLY in both directions:
/// every file that produces issues must be listed, every listed file must
/// produce issues. The list may only shrink.
fn read_xfail() -> BTreeMap<String, String> {
    let path = corpus_dir().join("xfail.txt");
    let text = fs::read_to_string(path).expect("read xfail.txt");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (path, reason) = match line.split_once(" — ") {
            Some((p, r)) => (p.trim(), r.trim()),
            None => (line, "(no reason given)"),
        };
        assert!(
            out.insert(path.to_owned(), reason.to_owned()).is_none(),
            "duplicate xfail entry: {path}"
        );
    }
    out
}

// ---- fingerprint ------------------------------------------------------------

/// Escape a free-text atom (type expressions, default expressions, const
/// values) so tabs/newlines/backslashes can never inject a separator.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

fn vis(v: Visibility) -> &'static str {
    match v {
        Visibility::Public => "public",
        Visibility::Protected => "protected",
        Visibility::Private => "private",
        _ => "unknown",
    }
}

fn list(items: &[String]) -> String {
    items.join(",")
}

/// The stable parser-output fingerprint of one file, one line per fact.
/// Deliberately omits byte offsets (churn on any edit), `uses` maps
/// (already reflected in the resolved names they produce) and anything
/// Debug-derived.
fn fingerprint(meta: &FileMeta) -> String {
    let mut out = String::new();
    for d in &meta.declarations {
        let kind = match d.kind {
            ClassKind::Class => "class",
            ClassKind::Interface => "interface",
            ClassKind::Trait => "trait",
            ClassKind::Enum => "enum",
            _ => "unknown",
        };
        let mut flags = String::new();
        if d.is_abstract {
            flags.push('a');
        }
        if d.is_final {
            flags.push('f');
        }
        if d.is_readonly {
            flags.push('r');
        }
        if d.conditional {
            flags.push('c');
        }
        out.push_str(&format!(
            "D\t{}\t{}\t{}\text={}\timpl={}\ttraits={}\tattrs={}\tbacking={}\tcases={}\n",
            esc(&d.fqcn),
            kind,
            flags,
            list(&d.extends),
            list(&d.implements),
            list(&d.traits),
            list(&d.attributes),
            d.enum_backing.as_deref().map(esc).unwrap_or_default(),
            list(&d.cases),
        ));
        // Trait adaptations: the whole reason `trait-adaptations.php` exists.
        // Rendered in source order, which is the order they were written.
        for t in &d.trait_insteadof {
            out.push_str(&format!(
                "T\t{}\t{}\texcl={}\n",
                esc(&t.trait_fqcn),
                esc(&t.method),
                list(&t.excluded),
            ));
        }
        for a in &d.trait_aliases {
            out.push_str(&format!(
                "A\t{}\t{}\t{}\t{}\n",
                a.trait_fqcn.as_deref().map(esc).unwrap_or_default(),
                esc(&a.method),
                a.alias.as_deref().map(esc).unwrap_or_default(),
                a.visibility.map(vis).unwrap_or(""),
            ));
        }
        for c in &d.constants {
            out.push_str(&format!(
                "C\t{}\t{}\t{}\t{}\n",
                esc(&c.name),
                vis(c.visibility),
                c.ty.as_deref().map(esc).unwrap_or_default(),
                esc(&c.value),
            ));
        }
        // Properties: `F` (field), since `P` is a parameter line. The default
        // column carries a leading `=` so "no default" and an empty capture
        // stay distinct — for a typed property that is the difference between
        // uninitialized and a value.
        for p in &d.properties {
            let mut flags = String::new();
            if p.is_static {
                flags.push('s');
            }
            if p.is_readonly {
                flags.push('r');
            }
            if p.hooked {
                flags.push('h');
            }
            out.push_str(&format!(
                "F\t{}\t{}\t{}\t{}\t{}\n",
                esc(&p.name),
                vis(p.visibility),
                flags,
                p.ty.as_deref().map(esc).unwrap_or_default(),
                p.default.as_deref().map(|d| format!("={}", esc(d))).unwrap_or_default(),
            ));
        }
        for m in &d.methods {
            let mut flags = String::new();
            if m.is_static {
                flags.push('s');
            }
            if m.is_abstract {
                flags.push('a');
            }
            if m.is_final {
                flags.push('f');
            }
            if m.returns_ref {
                flags.push('r');
            }
            out.push_str(&format!(
                "M\t{}\t{}\t{}\t{}\n",
                esc(&m.name),
                vis(m.visibility),
                flags,
                m.return_type.as_deref().map(esc).unwrap_or_default(),
            ));
            for p in &m.params {
                let mut flags = String::new();
                if p.readonly {
                    flags.push('r');
                }
                if p.by_ref {
                    flags.push('&');
                }
                if p.variadic {
                    flags.push('v');
                }
                out.push_str(&format!(
                    "P\t{}\t{}\t{}\t{}\t{}\n",
                    esc(&p.name),
                    p.promoted.map(vis).unwrap_or(""),
                    flags,
                    p.ty.as_deref().map(esc).unwrap_or_default(),
                    p.default.as_deref().map(esc).unwrap_or_default(),
                ));
            }
        }
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Parse one corpus file and compute its manifest row.
fn row_for(rel: &str, bytes: &[u8]) -> (Row, FileMeta, String) {
    let meta = magecommand_php::parse_file(bytes);
    let fp = fingerprint(&meta);
    let row = Row {
        path: rel.to_owned(),
        decls: meta.declarations.len(),
        methods: meta
            .declarations
            .iter()
            .map(|d| d.methods.len())
            .sum(),
        consts: meta.declarations.iter().map(|d| d.constants.len()).sum(),
        cases: meta.declarations.iter().map(|d| d.cases.len()).sum(),
        file_sha256: sha256_hex(bytes),
        fp_sha256: sha256_hex(fp.as_bytes()),
    };
    (row, meta, fp)
}

fn head_lines(text: &str, max: usize) -> String {
    let mut out = text.lines().take(max).collect::<Vec<_>>().join("\n");
    // Only count the rest when there is a rest to report.
    let rest = text.lines().skip(max).count();
    if rest > 0 {
        out.push_str(&format!("\n… (+{rest} more)"));
    }
    out
}

// ---- the gate ----------------------------------------------------------------

#[test]
fn corpus_gate() {
    let corpus = corpus_dir();
    let mut abs = Vec::new();
    collect_php(&corpus.join("coverage"), &mut abs);
    // Manifest keys are `/`-joined regardless of host separator: the manifest
    // is blessed on one machine and checked on every other, Windows included.
    let mut files: Vec<String> = abs
        .iter()
        .map(|p| {
            p.strip_prefix(&corpus)
                .expect("corpus file under corpus dir")
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "corpus is empty — corpus/coverage/ has no .php files"
    );

    let started = Instant::now();
    let mut fresh: BTreeMap<String, Row> = BTreeMap::new();
    let mut metas: BTreeMap<String, FileMeta> = BTreeMap::new();
    let mut fps: BTreeMap<String, String> = BTreeMap::new();
    let mut bytes = 0usize;
    let mut problems: Vec<String> = Vec::new();

    for rel in &files {
        let bytes_of_file = fs::read(corpus.join(rel)).expect("read corpus file");
        bytes += bytes_of_file.len();
        let (row, meta, fp) = row_for(rel, &bytes_of_file);
        fresh.insert(row.path.clone(), row);
        metas.insert(rel.clone(), meta);
        fps.insert(rel.clone(), fp);
    }

    let issues: BTreeSet<String> = metas
        .iter()
        .filter(|(_, m)| !m.issues.is_empty())
        .map(|(p, _)| p.clone())
        .collect();

    // Bless on the VALUE, never on mere presence: a `MAGECOMMAND_CORPUS_BLESS`
    // left exported in a shell — or set to `0` by someone expecting it to
    // disable blessing — would otherwise turn every later `cargo test` in that
    // shell into a rubber stamp that rewrites the manifest and reports green.
    let blessing = match std::env::var("MAGECOMMAND_CORPUS_BLESS") {
        Ok(v) => {
            let v = v.trim();
            assert!(
                matches!(v, "0" | "1"),
                "MAGECOMMAND_CORPUS_BLESS must be 0 or 1, got {v:?}"
            );
            v == "1"
        }
        Err(_) => false,
    };

    if blessing {
        // Bless is an affordance, never a red: rewrite the manifest as the
        // new truth, show the diff, list xfail candidates. The gate enforces
        // xfail curation on the next plain run.
        bless(&fresh, &issues, &fps, started, files.len(), bytes);
        // BTreeMap already yields rows in path order.
        let rows: Vec<Row> = fresh.values().cloned().collect();
        write_manifest(&rows);
        return;
    }

    let (blessed_under, manifest) = read_manifest();
    // A manifest blessed under a different rendering cannot be compared row
    // by row: every digest would differ for reasons that are not regressions.
    assert_eq!(
        blessed_under,
        Some(FINGERPRINT_VERSION),
        "manifest.tsv was blessed under fingerprint version {blessed_under:?}, this test \
         renders v{FINGERPRINT_VERSION} — the digests are not comparable. Re-bless: \
         MAGECOMMAND_CORPUS_BLESS=1 cargo test -p magecommand-php --test corpus"
    );
    let xfail = read_xfail();

    // 1. Exact set match, both directions: a file without a manifest row, or
    //    a row without a file, means the corpus and manifest diverged.
    for rel in &files {
        if !manifest.contains_key(rel) {
            problems.push(format!(
                "{rel}: not in manifest.tsv — re-bless (MAGECOMMAND_CORPUS_BLESS=1) or revert"
            ));
        }
    }
    for path in manifest.keys() {
        if !files.contains(path) {
            problems.push(format!(
                "{path}: in manifest.tsv but no such corpus file — re-bless or restore the file"
            ));
        }
    }

    // 2. Per-file: file hash, counts, fingerprint digest.
    for path in &files {
        let Some(old) = manifest.get(path) else { continue };
        let new = &fresh[path];
        if old.file_sha256 != new.file_sha256 {
            problems.push(format!(
                "{path}: corpus file bytes changed (sha256 {} → {}) — \
                 edit was accidental? revert; else re-bless",
                old.file_sha256, new.file_sha256
            ));
            continue;
        }
        if old.decls != new.decls
            || old.methods != new.methods
            || old.consts != new.consts
            || old.cases != new.cases
            || old.fp_sha256 != new.fp_sha256
        {
            problems.push(format!(
                "{path}: parser output drifted — decls {}/{} methods {}/{} consts {}/{} cases \
                 {}/{} fp {}/{}\nnew fingerprint (first {FINGERPRINT_PRINT_LINES} lines):\n{}\n\
                 → a parser regression (fix it), or an intentional change (re-bless)",
                old.decls, new.decls,
                old.methods, new.methods,
                old.consts, new.consts,
                old.cases, new.cases,
                old.fp_sha256, new.fp_sha256,
                head_lines(&fps[path], FINGERPRINT_PRINT_LINES),
            ));
        }
    }

    // 3. Zero-issue gate, both directions.
    for path in &issues {
        if !xfail.contains_key(path) {
            let first = metas[path].issues.first().expect("issue present");
            problems.push(format!(
                "{path}: {} parse issue(s), first: @{}: {} — fix the parser, or (genuinely \
                 unsupported construct) add the file to xfail.txt with a one-line reason",
                metas[path].issues.len(),
                first.offset,
                first.message
            ));
        }
    }
    for path in xfail.keys() {
        if !issues.contains(path) {
            problems.push(format!(
                "{path}: listed in xfail.txt but parses clean — remove the entry (the list may \
                 only shrink)"
            ));
        }
    }

    let elapsed = started.elapsed();
    println!(
        "corpus: {} files · {:.1} KiB · {} decls · {} methods · {} xfail · \
         fingerprint v{FINGERPRINT_VERSION} · {:?}",
        files.len(),
        bytes as f64 / 1024.0,
        fresh.values().map(|r| r.decls).sum::<usize>(),
        fresh.values().map(|r| r.methods).sum::<usize>(),
        xfail.len(),
        elapsed,
    );

    assert!(
        problems.is_empty(),
        "corpus gate failed:\n{}",
        problems.join("\n")
    );
}

fn bless(
    fresh: &BTreeMap<String, Row>,
    issues: &BTreeSet<String>,
    fps: &BTreeMap<String, String>,
    started: Instant,
    n_files: usize,
    bytes: usize,
) {
    let (blessed_under, old) = if manifest_path().exists() {
        read_manifest()
    } else {
        (None, BTreeMap::new())
    };
    if blessed_under != Some(FINGERPRINT_VERSION) {
        println!(
            "bless: fingerprint version {blessed_under:?} → v{FINGERPRINT_VERSION} — every \
             fp_sha256 below changes by construction, not by regression"
        );
    }
    let mut changed = 0usize;
    let mut added = 0usize;
    let mut removed = 0usize;
    for (path, row) in fresh {
        match old.get(path) {
            None => {
                added += 1;
                println!("bless: + {path} ({} decls)", row.decls);
            }
            Some(o) if o != row => {
                changed += 1;
                println!(
                    "bless: ~ {path} — decls {}/{} methods {}/{} consts {}/{} cases {}/{} \
                     file {}/{} fp {}/{}",
                    o.decls, row.decls,
                    o.methods, row.methods,
                    o.consts, row.consts,
                    o.cases, row.cases,
                    o.file_sha256, row.file_sha256,
                    o.fp_sha256, row.fp_sha256,
                );
                if o.fp_sha256 != row.fp_sha256 {
                    println!(
                        "  new fingerprint (first {FINGERPRINT_PRINT_LINES} lines):\n{}",
                        head_lines(&fps[path], FINGERPRINT_PRINT_LINES)
                    );
                }
            }
            Some(_) => {}
        }
    }
    for path in old.keys() {
        if !fresh.contains_key(path) {
            removed += 1;
            println!("bless: - {path}");
        }
    }
    println!(
        "bless: {} rows ({} added, {} changed, {} removed) in {:.2}s — {} files · {:.1} KiB",
        fresh.len(),
        added,
        changed,
        removed,
        started.elapsed().as_secs_f64(),
        n_files,
        bytes as f64 / 1024.0,
    );
    if !issues.is_empty() {
        println!(
            "bless: xfail candidates (files with issues — add to xfail.txt with a reason, or \
             fix the parser): {}",
            issues.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
}

// ---- the big-install smoke (local deep net, non-CI) --------------------------

/// The reference-install smoke: parse the whole checkout, keep the `< 1%
/// files with issues` gate and the throughput print. Runs only when
/// `MAGECOMMAND_CORPUS` names an install — a runtime skip rather than
/// `#[ignore]`, so asking for it is enough and no second flag is needed.
///
/// There is deliberately NO default path: this test walks a 685 MB tree,
/// and a hardcoded fallback made it run on every plain `cargo test` on
/// whichever machine happened to have that directory. (Note that libtest
/// captures output from passing tests, so the skip line below shows only
/// under `--nocapture`.)
///
/// The committed corpus gate above is the CI net; this is the local deep
/// net against a full install.
#[test]
fn parse_entire_corpus() {
    let Some(root) = std::env::var_os("MAGECOMMAND_CORPUS").map(PathBuf::from) else {
        eprintln!("ok — no MAGECOMMAND_CORPUS");
        return;
    };
    assert!(
        root.is_dir(),
        "MAGECOMMAND_CORPUS is not a directory: {} — a typo here used to fall through to a \
         hardcoded default and pass having parsed nothing",
        root.display()
    );

    let mut files = Vec::new();
    for sub in ["vendor", "app", "lib", "generated"] {
        collect_php(&root.join(sub), &mut files);
    }
    assert!(!files.is_empty(), "no PHP files found under {}", root.display());

    let started = Instant::now();
    let mut bytes = 0usize;
    let mut declarations = 0usize;
    let mut files_with_issues: Vec<(PathBuf, usize, String)> = Vec::new();
    let mut total_issues = 0usize;

    for path in &files {
        let Ok(src) = fs::read(path) else { continue };
        bytes += src.len();
        let meta = magecommand_php::parse_file(&src);
        declarations += meta.declarations.len();
        if !meta.issues.is_empty() {
            total_issues += meta.issues.len();
            let first = &meta.issues[0];
            files_with_issues.push((
                path.clone(),
                meta.issues.len(),
                format!("@{}: {}", first.offset, first.message),
            ));
        }
    }
    let elapsed = started.elapsed();

    println!(
        "corpus: {} files · {:.1} MiB · {} declarations · {:.0} MiB/s · {:?}",
        files.len(),
        bytes as f64 / (1024.0 * 1024.0),
        declarations,
        bytes as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64(),
        elapsed
    );
    println!(
        "issues: {} across {} files",
        total_issues,
        files_with_issues.len()
    );
    files_with_issues.sort_by_key(|(_, n, _)| std::cmp::Reverse(*n));
    for (path, n, first) in files_with_issues.iter().take(25) {
        println!("  {n:>3} {} — {first}", path.display());
    }

    // The M1 acceptance bar is zero; while building toward it, the test
    // fails when parsing goes badly wrong rather than on every stray issue.
    let pct = files_with_issues.len() as f64 / files.len() as f64 * 100.0;
    assert!(
        pct < 1.0,
        "{:.2}% of files have parse issues — parser has a systemic gap",
        pct
    );
}
