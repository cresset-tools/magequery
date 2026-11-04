//! How `Magento::csp_whitelist` merges `csp_whitelist.xml`, pinned against Magento_Csp:
//! the global tier and the area tier are each DOM-merged on value id, then the area tier
//! is laid over the global one with hosts keyed by id and hashes keyed by digest.

use std::path::Path;

use magequery_core::{Area, CspValueKind, CspWhitelist, Magento};

fn put(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).expect("mkdir");
    std::fs::write(&p, body).expect("write");
}

fn module(root: &Path, name: &str, sequence: &[&str]) {
    let (vendor, short) = name.split_once('_').unwrap();
    let dir = format!("app/code/{vendor}/{short}");
    let seq: String = sequence.iter().map(|s| format!("<module name=\"{s}\"/>")).collect();
    put(
        root,
        &format!("{dir}/etc/module.xml"),
        &format!("<?xml version=\"1.0\"?>\n<config><module name=\"{name}\"><sequence>{seq}</sequence></module></config>\n"),
    );
    put(
        root,
        &format!("{dir}/registration.php"),
        &format!("<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, '{name}', __DIR__);\n"),
    );
}

fn whitelist(values: &str) -> String {
    format!("<?xml version=\"1.0\"?>\n<csp_whitelist><policies>{values}</policies></csp_whitelist>\n")
}

fn scaffold(root: &Path) {
    put(
        root,
        "app/etc/config.php",
        "<?php\nreturn ['modules' => ['Acme_Base' => 1, 'Acme_Child' => 1, 'Acme_Off' => 0]];\n",
    );
    module(root, "Acme_Base", &[]);
    module(root, "Acme_Child", &["Acme_Base"]);
    module(root, "Acme_Off", &[]);

    put(
        root,
        "app/code/Acme/Base/etc/csp_whitelist.xml",
        &whitelist(
            r#"<policy id="script-src"><values>
                <value id="cdn" type="host">cdn.base.test</value>
                <value id="h1" type="hash" algorithm="sha256">AAAA</value>
            </values></policy>"#,
        ),
    );
    // Same tier, same value id → replaces Base's `cdn`.
    put(
        root,
        "app/code/Acme/Child/etc/csp_whitelist.xml",
        &whitelist(
            r#"<policy id="script-src"><values>
                <value id="cdn" type="host">cdn.child.test</value>
            </values></policy>"#,
        ),
    );
    // Area tier: `h1` here carries a different digest, so (keyed by digest) it does
    // NOT replace the global `h1` — both hashes survive. `cdn` is replaced by id.
    put(
        root,
        "app/code/Acme/Child/etc/frontend/csp_whitelist.xml",
        &whitelist(
            r#"<policy id="script-src"><values>
                <value id="h1" type="hash" algorithm="sha256">BBBB</value>
                <value id="cdn" type="host">cdn.frontend.test</value>
            </values></policy>
            <policy id="img-src"><values>
                <value id="pix" type="host">pix.test</value>
            </values></policy>"#,
        ),
    );
    // Disabled modules contribute nothing.
    put(
        root,
        "app/code/Acme/Off/etc/csp_whitelist.xml",
        &whitelist(r#"<policy id="script-src"><values><value id="off" type="host">off.test</value></values></policy>"#),
    );
    // A theme's whitelist is reported, not merged.
    put(root, "app/design/frontend/Acme/shop/theme.xml", "<theme><title>Shop</title></theme>\n");
    put(
        root,
        "app/design/frontend/Acme/shop/etc/csp_whitelist.xml",
        &whitelist(r#"<policy id="font-src"><values><value id="fonts" type="host">fonts.test</value></values></policy>"#),
    );
}

fn hosts(wl: &CspWhitelist, policy: &str) -> Vec<String> {
    values(wl, policy, CspValueKind::Host)
}

fn values(wl: &CspWhitelist, policy: &str, kind: CspValueKind) -> Vec<String> {
    wl.policies
        .iter()
        .find(|p| p.id == policy)
        .map(|p| p.values.iter().filter(|v| v.kind == kind).map(|v| v.value.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn global_tier_merges_by_value_id() {
    let dir = tempfile::tempdir().unwrap();
    scaffold(dir.path());
    let mage = Magento::open(dir.path()).expect("open");

    let wl = mage.csp_whitelist(Area::Global);
    assert_eq!(hosts(&wl, "script-src"), ["cdn.child.test"]);
    assert_eq!(values(&wl, "script-src", CspValueKind::Hash), ["AAAA"]);
    let cdn = &wl.policies[0].values.iter().find(|v| v.id == "cdn").unwrap();
    assert_eq!(cdn.source.module.as_str(), "Acme_Child");
    assert_eq!(cdn.overrides.len(), 1);
    assert_eq!(cdn.overrides[0].module.as_str(), "Acme_Base");
    assert!(wl.themes.is_empty(), "themes only belong to frontend/adminhtml");
}

#[test]
fn area_tier_overlays_hosts_by_id_and_hashes_by_digest() {
    let dir = tempfile::tempdir().unwrap();
    scaffold(dir.path());
    let mage = Magento::open(dir.path()).expect("open");

    let wl = mage.csp_whitelist(Area::Frontend);
    let ids: Vec<&str> = wl.policies.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["img-src", "script-src"], "sorted by id");
    assert_eq!(hosts(&wl, "script-src"), ["cdn.frontend.test"]);
    assert_eq!(values(&wl, "script-src", CspValueKind::Hash), ["AAAA", "BBBB"]);
    assert_eq!(hosts(&wl, "img-src"), ["pix.test"]);
    let cdn = wl.policies[1].values.iter().find(|v| v.id == "cdn").unwrap();
    let chain: Vec<&str> = cdn.overrides.iter().map(|s| s.module.as_str()).collect();
    assert_eq!(chain, ["Acme_Base", "Acme_Child"]);

    assert_eq!(wl.themes.len(), 1);
    assert_eq!(wl.themes[0].theme, "frontend/Acme/shop");
    assert_eq!(wl.themes[0].policies[0].values[0].value, "fonts.test");

    // adminhtml has no area file: just the global tier, and no frontend theme.
    let admin = mage.csp_whitelist(Area::Adminhtml);
    assert_eq!(hosts(&admin, "script-src"), ["cdn.child.test"]);
    assert!(admin.themes.is_empty());
}
