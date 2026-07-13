//! Live database-backed [`Magento`] queries and their raw-to-public conversions.

use super::super::*;
#[cfg(feature = "db")]
use crate::db;

impl Magento {
    /// One product by exact SKU, as the database stores it. Live DB.
    #[cfg(feature = "db")]
    pub fn product_by_sku(&self, sku: &str) -> Result<Option<Product>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_product(conn, &cfg.table_prefix, db::ProductIdent::Sku(sku))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product(r, false)))
    }

    /// One product by entity_id (`matched_by_id` is set on the result).
    #[cfg(feature = "db")]
    pub fn product_by_id(&self, id: u32) -> Result<Option<Product>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_product(conn, &cfg.table_prefix, db::ProductIdent::Id(id))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product(r, true)))
    }

    /// A product's related / up-sell / cross-sell links by SKU. `reverse` flips to
    /// the products that link *to* this one. Live DB.
    #[cfg(feature = "db")]
    pub fn product_links_by_sku(&self, sku: &str, reverse: bool) -> Result<Option<ProductLinks>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw =
            db::fetch_product_links(conn, &cfg.table_prefix, db::ProductIdent::Sku(sku), reverse)
                .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product_links(r, false)))
    }

    /// A product's links by entity_id (`matched_by_id` is set on the result). Live DB.
    #[cfg(feature = "db")]
    pub fn product_links_by_id(&self, id: u32, reverse: bool) -> Result<Option<ProductLinks>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw =
            db::fetch_product_links(conn, &cfg.table_prefix, db::ProductIdent::Id(id), reverse)
                .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product_links(r, true)))
    }

    /// The category tree, pre-order flattened (`level` 1 = a root tree), each root
    /// tagged with the store groups using it. Live DB.
    #[cfg(feature = "db")]
    pub fn category_tree(&self) -> Result<Vec<CategoryTreeNode>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (nodes, roots) =
            db::fetch_category_nodes(conn, &cfg.table_prefix).map_err(Error::Db)?;

        let mut by_parent: std::collections::BTreeMap<u32, Vec<&db::DbCategoryNode>> =
            std::collections::BTreeMap::new();
        for n in &nodes {
            by_parent.entry(n.parent_id).or_default().push(n);
        }
        for children in by_parent.values_mut() {
            children.sort_by_key(|n| (n.position, n.id));
        }
        let to_node = |n: &db::DbCategoryNode| CategoryTreeNode {
            id: n.id,
            name: n.name.clone().unwrap_or_else(|| format!("(category {})", n.id)),
            level: n.level,
            direct_products: n.direct_products,
            active: n.active,
            in_menu: n.in_menu,
            anchor: n.anchor,
            root_of: roots
                .iter()
                .filter(|(root, _)| *root == n.id)
                .map(|(_, g)| g.clone())
                .collect(),
        };
        // Pre-order DFS from the roots (children of the global root, id 1),
        // cycle-guarded by a visited set.
        let mut out = Vec::with_capacity(nodes.len());
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<&db::DbCategoryNode> = by_parent
            .get(&1)
            .map(|roots| roots.iter().rev().copied().collect())
            .unwrap_or_default();
        while let Some(n) = stack.pop() {
            if !seen.insert(n.id) {
                continue;
            }
            out.push(to_node(n));
            if let Some(children) = by_parent.get(&n.id) {
                stack.extend(children.iter().rev());
            }
        }
        Ok(out)
    }

    /// Categories whose name or url_key contains `needle` (case-insensitive). Live DB.
    #[cfg(feature = "db")]
    pub fn categories_like(&self, needle: &str) -> Result<Vec<CategoryHit>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (nodes, _) = db::fetch_category_nodes(conn, &cfg.table_prefix).map_err(Error::Db)?;
        let n = needle.to_lowercase();
        let mut hits: Vec<CategoryHit> = nodes
            .into_iter()
            .filter(|c| {
                c.name.as_deref().is_some_and(|x| x.to_lowercase().contains(&n))
                    || c.url_key.as_deref().is_some_and(|x| x.to_lowercase().contains(&n))
            })
            .map(|c| CategoryHit {
                id: c.id,
                name: c.name.unwrap_or_else(|| format!("(category {})", c.id)),
                url_key: c.url_key,
                level: c.level,
                active: c.active,
            })
            .collect();
        hits.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        Ok(hits)
    }

    /// One category by id: per-scope values, the visibility diagnosis (own scopes + the
    /// ancestor walk), direct vs indexed product counts, rewrites. `include_products`
    /// lists the direct assignments; `indexed_store` (`Some(None)` = the first store
    /// view, `Some(Some(code))` = that store) lists the store's *index* — what the
    /// storefront shows, anchor-inherited included. Live DB.
    #[cfg(feature = "db")]
    pub fn category(
        &self,
        id: u32,
        include_products: bool,
        indexed_store: Option<Option<&str>>,
    ) -> Result<Option<Category>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw =
            db::fetch_category_card(conn, &cfg.table_prefix, id, include_products, indexed_store)
                .map_err(Error::Db)?;
        Ok(raw.map(to_category))
    }

    /// Every CMS page/block row matching an exact identifier (several rows can share
    /// one, scoped to different stores), or all rows with `None`. `include_content`
    /// keeps the full content on each entry. Live DB.
    #[cfg(feature = "db")]
    pub fn cms_entries(
        &self,
        kind: CmsKind,
        ident: Option<&str>,
        include_content: bool,
    ) -> Result<Vec<CmsEntry>> {
        let sel = match ident {
            Some(i) => db::CmsSelector::Identifier(i),
            None => db::CmsSelector::All,
        };
        self.cms_fetch(kind, sel, include_content)
    }

    /// One CMS row by its numeric id — the unambiguous handle when an identifier is
    /// shared by several store-scoped rows.
    #[cfg(feature = "db")]
    pub fn cms_entry_by_id(
        &self,
        kind: CmsKind,
        id: u32,
        include_content: bool,
    ) -> Result<Option<CmsEntry>> {
        Ok(self.cms_fetch(kind, db::CmsSelector::Id(id), include_content)?.into_iter().next())
    }

    #[cfg(feature = "db")]
    fn cms_fetch(
        &self,
        kind: CmsKind,
        sel: db::CmsSelector<'_>,
        include_content: bool,
    ) -> Result<Vec<CmsEntry>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raws =
            db::fetch_cms_entries(conn, &cfg.table_prefix, kind, sel).map_err(Error::Db)?;
        Ok(raws
            .into_iter()
            .map(|r| {
                let content = r.content.unwrap_or_default();
                let preview: String = content
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(160)
                    .collect();
                CmsEntry {
                    kind,
                    id: r.id,
                    identifier: r.identifier,
                    title: r.title,
                    active: r.active,
                    stores: r.stores,
                    created: r.created,
                    updated: r.updated,
                    page_layout: r.page_layout,
                    meta_title: r.meta_title,
                    has_layout_update: r.has_layout_update,
                    content_len: content.chars().count(),
                    content_preview: preview,
                    content: include_content.then_some(content),
                }
            })
            .collect())
    }

    /// CMS search by identifier/title substring.
    #[cfg(feature = "db")]
    pub fn cms_like(
        &self,
        kind: CmsKind,
        needle: &str,
        limit: usize,
    ) -> Result<(Vec<CmsHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_cms_like(conn, &cfg.table_prefix, kind, needle, limit).map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(|(id, identifier, title, active)| CmsHit {
                    id,
                    identifier,
                    title,
                    active,
                    stores: Vec::new(),
                })
                .collect(),
            truncated,
        ))
    }

    /// All four OAuth credentials for one integration, by integration id. `None` = the
    /// integration has no consumer. This is the ONLY method that returns the secrets — an
    /// explicit, opt-in escape hatch for scripting (the owner already has DB access);
    /// [`Self::integrations`] never exposes them. Deliberately **not** `Serialize`, so no
    /// `--json` path can carry a secret. Live DB.
    #[cfg(feature = "db")]
    pub fn integration_credentials(
        &self,
        integration_id: u32,
    ) -> Result<Option<IntegrationCredentials>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_integration_secrets(conn, &cfg.table_prefix, integration_id)
            .map_err(Error::Db)?;
        Ok(raw.map(
            |(consumer_key, consumer_secret, access_token, access_secret, revoked)| {
                IntegrationCredentials {
                    consumer_key,
                    consumer_secret,
                    access_token,
                    access_secret,
                    revoked,
                }
            },
        ))
    }

    /// API integrations with their token state and granted ACL resources (titled from
    /// the static acl.xml index — a missing title flags a stale grant). Filtered by a
    /// name substring. Token secrets are never returned (use [`Self::integration_token`]
    /// for the explicit opt-in). Live DB.
    #[cfg(feature = "db")]
    pub fn integrations(&self, filter: Option<&str>) -> Result<Vec<Integration>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raws = db::fetch_integrations(conn, &cfg.table_prefix).map_err(Error::Db)?;
        let needle = filter.map(str::to_lowercase);
        Ok(raws
            .into_iter()
            .filter(|(_, name, ..)| {
                needle.as_deref().map_or(true, |n| name.to_lowercase().contains(n))
            })
            .map(
                |(id, name, email, endpoint, status, setup, created_at, updated_at, token, rules)| {
                    let rules: Vec<AdminRule> = rules
                        .into_iter()
                        .map(|(resource, allow)| AdminRule {
                            title: self
                                .acl_resource(&resource)
                                .map(|r| r.title)
                                .filter(|t| !t.is_empty()),
                            resource,
                            allow,
                        })
                        .collect();
                    let all_resources =
                        rules.iter().any(|r| r.resource == "Magento_Backend::all" && r.allow);
                    Integration {
                        id,
                        name,
                        email: email.filter(|e| !e.is_empty()),
                        endpoint: endpoint.filter(|e| !e.is_empty()),
                        status: match status {
                            0 => "inactive".to_string(),
                            1 => "active".to_string(),
                            2 => "recreated".to_string(),
                            other => format!("status {other}"),
                        },
                        setup: if setup == 1 { "config".to_string() } else { "manual".to_string() },
                        created_at,
                        updated_at,
                        token,
                        all_resources,
                        rules,
                    }
                },
            )
            .collect())
    }

    /// The tax picture: classes (flagging ones no rule references — a product class in
    /// no rule ships untaxed), rules with their class combinations and rates, and rates
    /// no rule uses. Live DB.
    #[cfg(feature = "db")]
    pub fn tax_info(&self) -> Result<TaxInfo> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_tax_info(conn, &cfg.table_prefix).map_err(Error::Db)?;

        let class_name = |id: u32| -> String {
            raw.classes
                .iter()
                .find(|(cid, ..)| *cid == id)
                .map(|(_, n, _)| n.clone())
                .unwrap_or_else(|| format!("(class {id})"))
        };
        let rate_of = |id: u32| -> Option<TaxRate> {
            raw.rates.iter().find(|(rid, ..)| *rid == id).map(
                |(id, code, country, region, postcode, rate)| TaxRate {
                    id: *id,
                    code: code.clone(),
                    country: country.clone(),
                    region: region.clone(),
                    postcode: postcode.clone(),
                    rate: rate.clone(),
                },
            )
        };

        let rules: Vec<TaxRule> = raw
            .rules
            .iter()
            .map(|(rule_id, code, priority, calculate_subtotal)| {
                let mine: Vec<&(u32, u32, u32, u32)> =
                    raw.links.iter().filter(|(r, ..)| r == rule_id).collect();
                let mut customer: Vec<u32> = mine.iter().map(|(_, _, c, _)| *c).collect();
                let mut product: Vec<u32> = mine.iter().map(|(_, _, _, p)| *p).collect();
                let mut rate_ids: Vec<u32> = mine.iter().map(|(_, ra, _, _)| *ra).collect();
                customer.sort_unstable();
                customer.dedup();
                product.sort_unstable();
                product.dedup();
                rate_ids.sort_unstable();
                rate_ids.dedup();
                TaxRule {
                    id: *rule_id,
                    code: code.clone(),
                    priority: *priority,
                    calculate_subtotal: *calculate_subtotal,
                    customer_classes: customer.into_iter().map(class_name).collect(),
                    product_classes: product.into_iter().map(class_name).collect(),
                    rates: rate_ids.into_iter().filter_map(rate_of).collect(),
                }
            })
            .collect();

        let used_customer: std::collections::HashSet<u32> =
            raw.links.iter().map(|(_, _, c, _)| *c).collect();
        let used_product: std::collections::HashSet<u32> =
            raw.links.iter().map(|(_, _, _, p)| *p).collect();
        let used_rates: std::collections::HashSet<u32> =
            raw.links.iter().map(|(_, r, _, _)| *r).collect();

        let classes = raw
            .classes
            .iter()
            .map(|(id, name, class_type)| TaxClassInfo {
                id: *id,
                name: name.clone(),
                in_rules: if class_type == "CUSTOMER" {
                    used_customer.contains(id)
                } else {
                    used_product.contains(id)
                },
                class_type: class_type.clone(),
            })
            .collect();
        let unused_rates = raw
            .rates
            .iter()
            .filter(|(id, ..)| !used_rates.contains(id))
            .map(|(id, code, country, region, postcode, rate)| TaxRate {
                id: *id,
                code: code.clone(),
                country: country.clone(),
                region: region.clone(),
                postcode: postcode.clone(),
                rate: rate.clone(),
            })
            .collect();

        Ok(TaxInfo { classes, rules, unused_rates })
    }

    /// One catalog price rule by rule_id. Live DB.
    #[cfg(feature = "db")]
    pub fn catalog_rule(&self, id: u32) -> Result<Option<CatalogRule>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_catalog_rule(conn, &cfg.table_prefix, id).map_err(Error::Db)?;
        Ok(raw.map(|r| {
            let amount = r.discount_amount.as_deref().unwrap_or("?");
            let action = match r.simple_action.as_deref() {
                Some("by_percent") => format!("{amount}% off"),
                Some("by_fixed") => format!("{amount} off"),
                Some("to_percent") => format!("price becomes {amount}% of original"),
                Some("to_fixed") => format!("price set to {amount}"),
                Some(other) => format!("{other} ({amount})"),
                None => "(no action)".to_string(),
            };
            CatalogRule {
                rule_id: r.rule_id,
                name: r.name,
                description: r.description,
                active: r.active,
                from_date: r.from_date,
                to_date: r.to_date,
                in_window: r.in_window,
                action,
                sort_order: r.sort_order,
                stop_rules_processing: r.stop_rules_processing,
                websites: r.websites,
                customer_groups: r.customer_groups,
                conditions: r.conditions,
                matched_products: r.matched_products,
            }
        }))
    }

    /// Catalog rules by name/description substring (empty = all), with materialized
    /// product counts.
    #[cfg(feature = "db")]
    pub fn catalog_rules_like(
        &self,
        needle: &str,
        limit: usize,
    ) -> Result<(Vec<CatalogRuleHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_catalog_rules(conn, &cfg.table_prefix, needle, limit).map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(|(rule_id, name, active, from_date, to_date, matched_products)| {
                    CatalogRuleHit { rule_id, name, active, from_date, to_date, matched_products }
                })
                .collect(),
            truncated,
        ))
    }

    /// One cart price rule by rule_id. Live DB.
    #[cfg(feature = "db")]
    pub fn sales_rule(&self, id: u32) -> Result<Option<SalesRule>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_sales_rule(conn, &cfg.table_prefix, db::RuleIdent::Id(id))
            .map_err(Error::Db)?;
        Ok(raw.map(to_sales_rule))
    }

    /// The rule behind an exact coupon code, with the matched coupon attached.
    #[cfg(feature = "db")]
    pub fn sales_rule_by_coupon(&self, code: &str) -> Result<Option<SalesRule>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_sales_rule(conn, &cfg.table_prefix, db::RuleIdent::Coupon(code))
            .map_err(Error::Db)?;
        Ok(raw.map(to_sales_rule))
    }

    /// Rule search by name/description substring, newest first.
    #[cfg(feature = "db")]
    pub fn sales_rules_like(&self, needle: &str, limit: usize) -> Result<(Vec<SalesRuleHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_sales_rules_like(conn, &cfg.table_prefix, needle, limit)
                .map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(|(rule_id, name, active, from_date, to_date)| SalesRuleHit {
                    rule_id,
                    name,
                    active,
                    from_date,
                    to_date,
                })
                .collect(),
            truncated,
        ))
    }

    /// The scope tree — websites → store groups → store views (admin scopes excluded),
    /// root categories named, plus the currency rate table. Live DB.
    #[cfg(feature = "db")]
    pub fn store_tree(&self) -> Result<StoreTree> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_store_tree(conn, &cfg.table_prefix).map_err(Error::Db)?;
        let websites = raw
            .websites
            .into_iter()
            .map(|(id, code, name, is_default, default_group)| WebsiteNode {
                groups: raw
                    .groups
                    .iter()
                    .filter(|(_, wid, ..)| *wid == id)
                    .map(|(gid, _, gname, root, default_store)| StoreGroupNode {
                        id: *gid,
                        name: gname.clone(),
                        root_category_id: *root,
                        root_category: raw.category_names.get(root).cloned(),
                        is_default: *gid == default_group,
                        views: raw
                            .views
                            .iter()
                            .filter(|(_, _, _, _, vgid, _)| vgid == gid)
                            .map(|(vid, vcode, vname, _, _, active)| StoreViewNode {
                                id: *vid,
                                code: vcode.clone(),
                                name: vname.clone(),
                                active: *active,
                                is_default: vid == default_store,
                            })
                            .collect(),
                    })
                    .collect(),
                id,
                code,
                name,
                is_default,
            })
            .collect();
        Ok(StoreTree { websites, currency_rates: raw.currency_rates })
    }

    /// Every order status with its state mapping(s), filtered by a status/label/state
    /// substring. Statuses mapped to no state sort last. Live DB.
    #[cfg(feature = "db")]
    pub fn order_statuses(&self, filter: Option<&str>) -> Result<Vec<OrderStatus>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (statuses, states) =
            db::fetch_order_statuses(conn, &cfg.table_prefix).map_err(Error::Db)?;
        let needle = filter.map(str::to_lowercase);
        let mut out: Vec<OrderStatus> = statuses
            .into_iter()
            .map(|(status, label)| OrderStatus {
                states: states
                    .iter()
                    .filter(|(st, ..)| *st == status)
                    .map(|(_, state, is_default, visible_on_front)| OrderStatusState {
                        state: state.clone(),
                        is_default: *is_default,
                        visible_on_front: *visible_on_front,
                    })
                    .collect(),
                status,
                label,
            })
            .filter(|s| match &needle {
                Some(n) => {
                    s.status.to_lowercase().contains(n)
                        || s.label.to_lowercase().contains(n)
                        || s.states.iter().any(|st| st.state.to_lowercase().contains(n))
                }
                None => true,
            })
            .collect();
        out.sort_by(|a, b| {
            let key = |s: &OrderStatus| {
                (s.states.is_empty(), s.states.first().map(|st| st.state.clone()), s.status.clone())
            };
            key(a).cmp(&key(b))
        });
        Ok(out)
    }

    /// Every customer group with its tax class and member count, filtered by a
    /// code/id/tax-class substring. Live DB.
    #[cfg(feature = "db")]
    pub fn customer_groups(&self, filter: Option<&str>) -> Result<Vec<CustomerGroup>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let rows = db::fetch_customer_groups(conn, &cfg.table_prefix).map_err(Error::Db)?;
        let needle = filter.map(str::to_lowercase);
        Ok(rows
            .into_iter()
            .map(|(id, code, tax_class_id, tax_class, members)| CustomerGroup {
                id,
                code,
                tax_class_id,
                tax_class,
                members,
            })
            .filter(|g| match &needle {
                Some(n) => {
                    g.code.to_lowercase().contains(n)
                        || g.id.to_string() == *n
                        || g.tax_class.as_deref().is_some_and(|t| t.to_lowercase().contains(n))
                }
                None => true,
            })
            .collect())
    }

    /// The sales increment sequences (per entity type × store): the profile plus the
    /// sequence table's high-water mark, and the computed next increment id. Live DB.
    #[cfg(feature = "db")]
    pub fn sales_sequences(&self, filter: Option<&str>) -> Result<Vec<SalesSequence>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let rows = db::fetch_sales_sequences(conn, &cfg.table_prefix).map_err(Error::Db)?;
        let needle = filter.map(str::to_lowercase);
        Ok(rows
            .into_iter()
            .filter(|(entity, ..)| {
                needle.as_deref().map_or(true, |n| entity.to_lowercase().contains(n))
            })
            .map(|(entity_type, store, prefix, suffix, step, active, max, warn, current)| {
                let next_value = current.map(|c| c + step).unwrap_or(1);
                let next_increment = format!(
                    "{}{:09}{}",
                    prefix.as_deref().unwrap_or(""),
                    next_value,
                    suffix.as_deref().unwrap_or(""),
                );
                SalesSequence {
                    entity_type,
                    store,
                    prefix,
                    suffix,
                    step,
                    active,
                    current,
                    next_increment,
                    max_value: max,
                    warning_value: warn,
                }
            })
            .collect())
    }

    /// One sales document (invoice/shipment/creditmemo) by exact increment id. Live DB.
    #[cfg(feature = "db")]
    pub fn sales_document(
        &self,
        kind: SalesDocKind,
        increment: &str,
    ) -> Result<Option<SalesDocument>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_sales_document(conn, &cfg.table_prefix, kind, increment)
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_sales_document(kind, r)))
    }

    /// Document search by increment substring, newest first.
    #[cfg(feature = "db")]
    pub fn sales_documents_like(
        &self,
        kind: SalesDocKind,
        needle: &str,
        limit: usize,
    ) -> Result<(Vec<SalesDocumentHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_sales_documents_like(conn, &cfg.table_prefix, kind, needle, limit)
                .map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(|(increment_id, order_increment, created_at, amount)| SalesDocumentHit {
                    increment_id,
                    order_increment,
                    created_at,
                    amount,
                })
                .collect(),
            truncated,
        ))
    }

    /// One quote (cart) by entity_id. Live DB.
    #[cfg(feature = "db")]
    pub fn quote(&self, id: u64) -> Result<Option<Quote>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_quote(conn, &cfg.table_prefix, id).map_err(Error::Db)?;
        Ok(raw.map(to_quote))
    }

    /// Quote search by customer email substring, newest first.
    #[cfg(feature = "db")]
    pub fn quotes_like(&self, needle: &str, limit: usize) -> Result<(Vec<QuoteHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_quotes_like(conn, &cfg.table_prefix, needle, limit).map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(|(entity_id, active, email, qty, total, currency, updated)| QuoteHit {
                    entity_id,
                    active,
                    customer_email: email,
                    items_qty: qty,
                    grand_total: total,
                    currency,
                    updated_at: updated,
                })
                .collect(),
            truncated,
        ))
    }

    /// One customer by exact email. Live DB.
    #[cfg(feature = "db")]
    pub fn customer_by_email(&self, email: &str) -> Result<Option<Customer>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_customer(conn, &cfg.table_prefix, db::CustomerIdent::Email(email))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_customer(r, false)))
    }

    /// One customer by entity_id.
    #[cfg(feature = "db")]
    pub fn customer_by_id(&self, id: u32) -> Result<Option<Customer>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_customer(conn, &cfg.table_prefix, db::CustomerIdent::Id(id))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_customer(r, true)))
    }

    /// Customer search: email or name substring, newest first.
    #[cfg(feature = "db")]
    pub fn customers_like(&self, needle: &str, limit: usize) -> Result<(Vec<CustomerHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_customers_like(conn, &cfg.table_prefix, needle, limit).map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(|(entity_id, email, name, group, created_at, _)| CustomerHit {
                    entity_id,
                    email,
                    name: name.unwrap_or_default(),
                    group,
                    created_at,
                })
                .collect(),
            truncated,
        ))
    }

    /// One order by exact increment_id. Live DB.
    #[cfg(feature = "db")]
    pub fn order_by_increment(&self, increment: &str) -> Result<Option<Order>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_order(conn, &cfg.table_prefix, db::OrderIdent::Increment(increment))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_order(r, false)))
    }

    /// One order by entity_id (`matched_by_id` set on the result).
    #[cfg(feature = "db")]
    pub fn order_by_id(&self, id: u32) -> Result<Option<Order>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_order(conn, &cfg.table_prefix, db::OrderIdent::Id(id))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_order(r, true)))
    }

    /// Order search: increment_id or customer email substring, newest first.
    #[cfg(feature = "db")]
    pub fn orders_like(&self, needle: &str, limit: usize) -> Result<(Vec<OrderHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_orders_like(conn, &cfg.table_prefix, needle, limit).map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(
                    |(entity_id, increment_id, status, grand_total, currency, email, created)| {
                        OrderHit {
                            entity_id,
                            increment_id,
                            status,
                            grand_total,
                            currency,
                            customer_email: email,
                            created_at: created,
                        }
                    },
                )
                .collect(),
            truncated,
        ))
    }

    /// Light lookup: the SKU of an entity_id (for shadow-note checks). Live DB.
    #[cfg(feature = "db")]
    pub fn product_sku_of_id(&self, id: u32) -> Result<Option<String>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        Ok(db::fetch_product_identity(conn, &cfg.table_prefix, &db::ProductIdent::Id(id))
            .map_err(Error::Db)?
            .map(|(_, sku, _)| sku))
    }

    /// Every price the database stores for a product, by exact SKU. Live DB.
    #[cfg(feature = "db")]
    pub fn product_prices_by_sku(&self, sku: &str) -> Result<Option<ProductPrices>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_product_prices(conn, &cfg.table_prefix, db::ProductIdent::Sku(sku))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product_prices(r, false)))
    }

    /// Every price for a product, by entity_id.
    #[cfg(feature = "db")]
    pub fn product_prices_by_id(&self, id: u32) -> Result<Option<ProductPrices>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let raw = db::fetch_product_prices(conn, &cfg.table_prefix, db::ProductIdent::Id(id))
            .map_err(Error::Db)?;
        Ok(raw.map(|r| to_product_prices(r, true)))
    }

    /// SKU-substring search, `limit + 1` fetched to flag truncation.
    #[cfg(feature = "db")]
    pub fn products_like(&self, needle: &str, limit: usize) -> Result<(Vec<ProductHit>, bool)> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (rows, truncated) =
            db::fetch_products_like(conn, &cfg.table_prefix, needle, limit).map_err(Error::Db)?;
        Ok((
            rows.into_iter()
                .map(|(entity_id, sku, type_id, name, status)| ProductHit {
                    entity_id,
                    sku,
                    type_id,
                    name,
                    enabled: status.map(|s| s == 1),
                })
                .collect(),
            truncated,
        ))
    }

    /// Admin users from the live `admin_user` table, each joined with its role name;
    /// lock state and login age computed on the DB server's clock. Sorted by username.
    #[cfg(feature = "db")]
    pub fn admin_users(&self) -> Result<Vec<AdminUser>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let rows = db::fetch_admin_users(conn, &cfg.table_prefix).map_err(Error::Db)?;
        Ok(rows
            .into_iter()
            .map(|u| AdminUser {
                id: u.id,
                username: u.username,
                firstname: u.firstname,
                lastname: u.lastname,
                email: u.email,
                active: u.active,
                role: u.role,
                created: u.created,
                last_login: u.last_login,
                last_login_secs: u.last_login_secs,
                logins: u.logins,
                failures: u.failures,
                locked: u.locked,
                lock_expires: u.lock_expires,
                locale: u.locale,
            })
            .collect())
    }

    /// Admin roles from the live `authorization_role`/`authorization_rule` tables: each
    /// with its member usernames and permission rules, every rule's resource id joined
    /// with its title from the static acl.xml index (`None` title = no module declares
    /// the resource — a stale rule of an uninstalled module). Sorted by role name.
    #[cfg(feature = "db")]
    pub fn admin_roles(&self) -> Result<Vec<AdminRole>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let (roles, members, rules) =
            db::fetch_admin_roles(conn, &cfg.table_prefix).map_err(Error::Db)?;
        Ok(roles
            .into_iter()
            .map(|(id, name)| {
                let users: Vec<String> = members
                    .iter()
                    .filter(|(rid, _)| *rid == id)
                    .map(|(_, u)| u.clone())
                    .collect();
                let rules: Vec<AdminRule> = rules
                    .iter()
                    .filter(|(rid, _, _)| *rid == id)
                    .map(|(_, resource, allow)| AdminRule {
                        title: self
                            .acl_resource(resource)
                            .map(|r| r.title)
                            .filter(|t| !t.is_empty()),
                        resource: resource.clone(),
                        allow: *allow,
                    })
                    .collect();
                let all_resources =
                    rules.iter().any(|r| r.resource == "Magento_Backend::all" && r.allow);
                AdminRole { id, name, users, all_resources, rules }
            })
            .collect())
    }

    fn eav_setup_index(&self) -> &eav::EavSetupIndex {
        self.eav_setup.get_or_init(|| eav::EavSetupIndex::build(&self.index.modules, &self.index.vfs))
    }

    /// Setup-script attribute calls (`addAttribute`/`updateAttribute`/`removeAttribute`
    /// with literal arguments) across the enabled modules — the static "who created this
    /// attribute" half of `eav`. Optionally filtered by exact attribute code. Core
    /// catalog attributes won't appear (Magento installs them from data arrays, not
    /// `addAttribute`); the value is third-party and project attributes.
    pub fn eav_setup_refs(&self, code: Option<&str>) -> Vec<EavSetupRef> {
        let refs = &self.eav_setup_index().refs;
        match code {
            Some(c) => refs.iter().filter(|r| r.code == c).cloned().collect(),
            None => refs.clone(),
        }
    }

    /// The `eav_entity_type` rows with attribute counts. Live DB (clean [`Error::Db`]
    /// when unreachable).
    #[cfg(feature = "db")]
    pub fn eav_entity_types(&self) -> Result<Vec<EavEntityType>> {
        Ok(self
            .eav_fetch_entities()?
            .into_iter()
            .map(|e| EavEntityType {
                code: e.code,
                entity_table: e.entity_table,
                attributes: e.attributes,
            })
            .collect())
    }

    /// Live attributes, optionally restricted to one entity type (aliases accepted:
    /// `product` → `catalog_product`). Sorted by (entity, code).
    #[cfg(feature = "db")]
    pub fn eav_attributes(&self, entity: Option<&str>) -> Result<Vec<EavAttribute>> {
        let entities = self.eav_fetch_entities()?;
        let wanted = entity.map(|e| eav::resolve_entity_alias(e));
        let rows = self.eav_fetch_attributes()?;
        Ok(rows
            .into_iter()
            .filter(|r| wanted.as_deref().map_or(true, |w| r.entity_code == w))
            .map(|r| to_eav_attribute(r, &entities))
            .collect())
    }

    /// The full card(s) for an exact attribute code — one per entity type declaring it
    /// (`name` exists on both products and categories): the live row plus set
    /// memberships, options, and the static setup-script join.
    #[cfg(feature = "db")]
    pub fn eav_attribute_cards(&self, code: &str) -> Result<Vec<EavAttributeCard>> {
        let entities = self.eav_fetch_entities()?;
        let rows: Vec<db::DbEavAttribute> = self
            .eav_fetch_attributes()?
            .into_iter()
            .filter(|r| r.code == code)
            .collect();

        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        let refs = self.eav_setup_refs(Some(code));
        let mut cards = Vec::new();
        for row in rows {
            let (sets, total_sets) =
                db::fetch_eav_sets(conn, &cfg.table_prefix, row.attribute_id, &row.entity_code)
                    .map_err(Error::Db)?;
            let options = db::fetch_eav_options(conn, &cfg.table_prefix, row.attribute_id)
                .map_err(Error::Db)?;
            // Setup calls naming this entity, plus ones whose entity we couldn't resolve.
            let setup_refs: Vec<EavSetupRef> = refs
                .iter()
                .filter(|r| !r.entity_known || r.entity == row.entity_code)
                .cloned()
                .collect();
            let entity_table =
                entities.iter().find(|e| e.code == row.entity_code).and_then(|e| e.entity_table.clone());
            cards.push(EavAttributeCard {
                attribute: to_eav_attribute(row, &entities),
                entity_table,
                sets: sets
                    .into_iter()
                    .map(|(set, group)| EavSetMembership { set, group })
                    .collect(),
                total_sets,
                options,
                setup_refs,
            });
        }
        Ok(cards)
    }

    #[cfg(feature = "db")]
    fn eav_fetch_entities(&self) -> Result<Vec<db::DbEavEntity>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_eav_entities(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(feature = "db")]
    fn eav_fetch_attributes(&self) -> Result<Vec<db::DbEavAttribute>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_eav_attributes(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(feature = "db")]
    pub(crate) fn fetch_patch_list(&self) -> Result<Vec<String>> {
        let cfg = self.db_config()?;
        let conn = default_connection(&cfg)?;
        db::fetch_patch_list(conn, &cfg.table_prefix).map_err(Error::Db)
    }

    #[cfg(not(feature = "db"))]
    pub(crate) fn fetch_patch_list(&self) -> Result<Vec<String>> {
        Err(Error::Db("the `db` feature is not enabled in this build".to_string()))
    }

}

/// Assemble [`SalesRule`]: decode coupon_type and the `simple_action` into a readable
/// discount summary.
#[cfg(feature = "db")]
fn to_sales_rule(raw: db::DbSalesRule) -> SalesRule {
    let amount = raw.discount_amount.as_deref().unwrap_or("?");
    let action = match raw.simple_action.as_deref() {
        Some("by_percent") => format!("{amount}% off"),
        Some("by_fixed") => format!("{amount} off per item"),
        Some("cart_fixed") => format!("{amount} off the cart"),
        Some("buy_x_get_y") => format!(
            "buy X get {} free (step {})",
            raw.discount_qty.as_deref().unwrap_or("?"),
            raw.discount_step.unwrap_or(0),
        ),
        Some(other) => format!("{other} ({amount})"),
        None => "(no action)".to_string(),
    };
    let coupon_type = match raw.coupon_type {
        1 => "no coupon needed".to_string(),
        2 => "specific coupon".to_string(),
        3 => "auto-generated coupons".to_string(),
        other => format!("coupon_type {other}"),
    };
    let coupon = |(code, times_used, usage_limit, usage_per_customer, expiration_date, expired): (
        String,
        u64,
        Option<u64>,
        Option<u64>,
        Option<String>,
        bool,
    )| RuleCoupon {
        code,
        times_used,
        usage_limit: usage_limit.filter(|&l| l > 0),
        usage_per_customer: usage_per_customer.filter(|&l| l > 0),
        expiration_date,
        expired,
    };
    SalesRule {
        rule_id: raw.rule_id,
        name: raw.name,
        description: raw.description,
        active: raw.active,
        from_date: raw.from_date,
        to_date: raw.to_date,
        in_window: raw.in_window,
        coupon_type,
        action,
        apply_to_shipping: raw.apply_to_shipping,
        free_shipping: raw.free_shipping,
        stop_rules_processing: raw.stop_rules_processing,
        sort_order: raw.sort_order,
        uses_per_customer: raw.uses_per_customer,
        uses_per_coupon: raw.uses_per_coupon,
        times_used: raw.times_used,
        websites: raw.websites,
        customer_groups: raw.customer_groups,
        conditions: raw.conditions,
        coupon_count: raw.coupon_count,
        coupons: raw.coupons.into_iter().map(coupon).collect(),
        matched_coupon: raw.matched_coupon.map(coupon),
    }
}

/// Assemble [`SalesDocument`]: decode the kind-specific state.
#[cfg(feature = "db")]
fn to_sales_document(kind: SalesDocKind, raw: db::DbSalesDocument) -> SalesDocument {
    let state = raw.state.map(|s| match (kind, s) {
        (SalesDocKind::Invoice, 1) => "open".to_string(),
        (SalesDocKind::Invoice, 2) => "paid".to_string(),
        (SalesDocKind::Invoice, 3) => "canceled".to_string(),
        (SalesDocKind::Creditmemo, 1) => "open".to_string(),
        (SalesDocKind::Creditmemo, 2) => "refunded".to_string(),
        (SalesDocKind::Creditmemo, 3) => "canceled".to_string(),
        (_, other) => format!("state {other}"),
    });
    SalesDocument {
        kind,
        entity_id: raw.entity_id,
        increment_id: raw.increment_id,
        state,
        order_increment: raw.order_increment,
        order_status: raw.order_status,
        created_at: raw.created_at,
        currency: raw.currency,
        totals: raw
            .totals
            .into_iter()
            .map(|(key, amount, base_amount)| OrderTotal { key, amount, base_amount })
            .collect(),
        transaction_id: raw.transaction_id,
        total_qty: raw.total_qty,
        items: raw
            .items
            .into_iter()
            .map(|(sku, name, qty, price, row_total)| SalesDocumentItem {
                sku,
                name,
                qty,
                price,
                row_total,
            })
            .collect(),
        tracks: raw
            .tracks
            .into_iter()
            .map(|(carrier, title, number)| {
                (
                    carrier.unwrap_or_default(),
                    title.unwrap_or_default(),
                    number.unwrap_or_default(),
                )
            })
            .collect(),
    }
}

/// Assemble [`Quote`]: blend the totals (subtotal + grand total from the quote row,
/// shipping/tax/discount from the shipping address, where checkout collects them) and
/// flatten the payment blob like the order card does.
#[cfg(feature = "db")]
fn to_quote(raw: db::DbQuote) -> Quote {
    let (subtotal, base_subtotal, grand_total, base_grand_total) = raw.quote_totals;
    let (shipping, base_shipping, tax, base_tax, discount, base_discount) = raw.address_totals;
    let totals = vec![
        OrderTotal { key: "subtotal".into(), amount: subtotal, base_amount: base_subtotal },
        OrderTotal { key: "shipping".into(), amount: shipping, base_amount: base_shipping },
        OrderTotal { key: "tax".into(), amount: tax, base_amount: base_tax },
        OrderTotal { key: "discount".into(), amount: discount, base_amount: base_discount },
        OrderTotal { key: "grand_total".into(), amount: grand_total, base_amount: base_grand_total },
    ];
    let payment = raw.payment.map(|(method, blob)| {
        let additional: Vec<(String, String)> = blob
            .as_deref()
            .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
            .and_then(|v| match v {
                serde_json::Value::Object(map) => Some(
                    map.into_iter()
                        .map(|(k, v)| {
                            let val = match v {
                                serde_json::Value::String(s) => s,
                                other => other.to_string(),
                            };
                            (k, val)
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        OrderPayment { method, last_trans_id: None, additional }
    });
    let customer_name = match (&raw.customer_firstname, &raw.customer_lastname) {
        (Some(f), Some(l)) => Some(format!("{f} {l}")),
        (Some(f), None) => Some(f.clone()),
        (None, Some(l)) => Some(l.clone()),
        _ => None,
    };

    Quote {
        entity_id: raw.entity_id,
        active: raw.active,
        store: raw.store,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        updated_secs: raw.updated_secs,
        converted_at: raw.converted_at,
        customer_id: raw.customer_id,
        customer_email: raw.customer_email,
        customer_name,
        guest: raw.guest,
        checkout_method: raw.checkout_method,
        quote_currency: raw.quote_currency,
        base_currency: raw.base_currency,
        items_qty: raw.items_qty,
        is_virtual: raw.is_virtual,
        coupon: raw.coupon,
        applied_rule_ids: raw.applied_rule_ids,
        reserved_order_id: raw.reserved_order_id,
        order_increment: raw.order_increment,
        totals,
        items: raw
            .items
            .into_iter()
            .map(|(sku, name, product_type, is_child, qty, price, row_total, discount)| {
                QuoteItem { sku, name, product_type, is_child, qty, price, row_total, discount }
            })
            .collect(),
        addresses: raw
            .addresses
            .into_iter()
            .map(
                |(kind, first, last, company, street, postcode, city, country, method, desc)| {
                    QuoteAddress {
                        kind,
                        name: [first, last].into_iter().flatten().collect::<Vec<_>>().join(" "),
                        company,
                        street: street.map(|s| s.replace('\n', ", ")),
                        postcode,
                        city,
                        country,
                        shipping_method: method,
                        shipping_description: desc,
                    }
                },
            )
            .collect(),
        payment,
    }
}

/// Assemble [`Customer`]: decode the newsletter status, name the addresses, and pass
/// custom EAV values through the shared scope machinery (single `default` scope —
/// customer attributes aren't store-scoped).
#[cfg(feature = "db")]
fn to_customer(raw: db::DbCustomer, matched_by_id: bool) -> Customer {
    let newsletter_status = |s: i64| match s {
        1 => "subscribed".to_string(),
        2 => "not active".to_string(),
        3 => "unsubscribed".to_string(),
        4 => "unconfirmed".to_string(),
        other => format!("status {other}"),
    };
    let name = [raw.firstname.clone(), raw.lastname.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");

    let mut values: Vec<ProductValue> = Vec::new();
    for v in &raw.values {
        let scope = ProductScopeValue {
            store: "default".to_string(),
            label: None,
            value: v.value.clone().unwrap_or_else(|| "NULL".to_string()),
        };
        match values.iter_mut().find(|e| e.attribute == v.attribute) {
            Some(e) => e.scopes.push(scope),
            None => values.push(ProductValue {
                attribute: v.attribute.clone(),
                backend_type: v.backend_type.clone(),
                input: v.input.clone(),
                scopes: vec![scope],
            }),
        }
    }
    values.sort_by(|a, b| a.attribute.cmp(&b.attribute));

    Customer {
        entity_id: raw.entity_id,
        email: raw.email,
        name,
        group: raw.group,
        website: raw.website,
        created_in: raw.created_in,
        created_at: raw.created_at,
        active: raw.active,
        confirmed: raw.confirmed,
        locked: raw.locked,
        lock_expires: raw.lock_expires,
        failures: raw.failures,
        last_login: raw.last_login,
        last_logout: raw.last_logout,
        dob: raw.dob,
        taxvat: raw.taxvat,
        addresses: raw
            .addresses
            .into_iter()
            .map(|(id, f, l, company, street, postcode, city, region, country, telephone, db, ds)| {
                CustomerAddress {
                    id,
                    name: [f, l].into_iter().flatten().collect::<Vec<_>>().join(" "),
                    company,
                    street: street.map(|s| s.replace('\n', ", ")),
                    postcode,
                    city,
                    region,
                    country,
                    telephone,
                    default_billing: db,
                    default_shipping: ds,
                }
            })
            .collect(),
        newsletter: raw
            .newsletter
            .into_iter()
            .map(|(store, status)| CustomerNewsletter { store, status: newsletter_status(status) })
            .collect(),
        values,
        orders: CustomerOrders {
            count: raw.order_stats.0,
            lifetime: raw.order_stats.1,
            first_at: raw.order_stats.2,
            last_at: raw.order_stats.3,
            last_increment: raw.last_order.as_ref().map(|(i, _)| i.clone()),
            last_status: raw.last_order.and_then(|(_, s)| s),
        },
        guest_orders: raw.guest_orders,
        matched_by_id,
    }
}

/// Assemble [`Order`]: decode document states, join tracks onto their shipments, and
/// flatten the payment's `additional_information` JSON (top-level keys; nested values
/// re-serialized compactly).
#[cfg(feature = "db")]
fn to_order(raw: db::DbOrder, matched_by_id: bool) -> Order {
    let invoice_state = |s: Option<i64>| {
        s.map(|s| match s {
            1 => "open".to_string(),
            2 => "paid".to_string(),
            3 => "canceled".to_string(),
            other => format!("state {other}"),
        })
    };
    let memo_state = |s: Option<i64>| {
        s.map(|s| match s {
            1 => "open".to_string(),
            2 => "refunded".to_string(),
            3 => "canceled".to_string(),
            other => format!("state {other}"),
        })
    };

    let payment = raw.payment.map(|(method, last_trans_id, blob)| {
        let additional: Vec<(String, String)> = blob
            .as_deref()
            .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok())
            .and_then(|v| match v {
                serde_json::Value::Object(map) => Some(
                    map.into_iter()
                        .map(|(k, v)| {
                            let val = match v {
                                serde_json::Value::String(s) => s,
                                other => other.to_string(),
                            };
                            (k, val)
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        OrderPayment { method, last_trans_id, additional }
    });

    let customer_name = match (&raw.customer_firstname, &raw.customer_lastname) {
        (Some(f), Some(l)) => Some(format!("{f} {l}")),
        (Some(f), None) => Some(f.clone()),
        (None, Some(l)) => Some(l.clone()),
        _ => None,
    };

    Order {
        entity_id: raw.entity_id,
        increment_id: raw.increment_id,
        state: raw.state,
        status: raw.status,
        status_label: raw.status_label,
        store: raw.store,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        customer_id: raw.customer_id,
        customer_email: raw.customer_email,
        customer_name,
        guest: raw.guest,
        order_currency: raw.order_currency,
        base_currency: raw.base_currency,
        total_qty: raw.total_qty,
        coupon: raw.coupon,
        applied_rule_ids: raw.applied_rule_ids,
        shipping_method: raw.shipping_method,
        shipping_description: raw.shipping_description,
        totals: raw
            .totals
            .into_iter()
            .map(|(key, amount, base_amount)| OrderTotal { key, amount, base_amount })
            .collect(),
        items: raw
            .items
            .into_iter()
            .map(
                |(sku, name, product_type, is_child, ordered, invoiced, shipped, refunded, canceled, price, row_total)| {
                    OrderItem {
                        sku,
                        name,
                        product_type,
                        is_child,
                        qty_ordered: ordered,
                        qty_invoiced: invoiced,
                        qty_shipped: shipped,
                        qty_refunded: refunded,
                        qty_canceled: canceled,
                        price,
                        row_total,
                    }
                },
            )
            .collect(),
        addresses: raw
            .addresses
            .into_iter()
            .map(
                |(kind, first, last, company, street, postcode, city, country, telephone)| {
                    let name = [first, last].into_iter().flatten().collect::<Vec<_>>().join(" ");
                    OrderAddress {
                        kind,
                        name,
                        company,
                        street: street.map(|s| s.replace('\n', ", ")),
                        postcode,
                        city,
                        country,
                        telephone,
                    }
                },
            )
            .collect(),
        payment,
        transactions: raw
            .transactions
            .into_iter()
            .map(|(txn_id, kind, closed, created_at)| OrderTransaction {
                txn_id,
                kind,
                closed,
                created_at,
            })
            .collect(),
        invoices: raw
            .invoices
            .into_iter()
            .map(|(increment_id, state, total, created_at)| OrderDocument {
                increment_id,
                state: invoice_state(state),
                total,
                created_at,
            })
            .collect(),
        shipments: raw
            .shipments
            .into_iter()
            .map(|(sid, increment_id, qty, created_at)| OrderShipment {
                increment_id,
                qty,
                created_at,
                tracks: raw
                    .tracks
                    .iter()
                    .filter(|(parent, ..)| *parent == sid)
                    .map(|(_, carrier, title, number)| {
                        (
                            carrier.clone().unwrap_or_default(),
                            title.clone().unwrap_or_default(),
                            number.clone().unwrap_or_default(),
                        )
                    })
                    .collect(),
            })
            .collect(),
        creditmemos: raw
            .creditmemos
            .into_iter()
            .map(|(increment_id, state, total, created_at)| OrderDocument {
                increment_id,
                state: memo_state(state),
                total,
                created_at,
            })
            .collect(),
        history: raw
            .history
            .into_iter()
            .map(|(status, comment, created_at, notified)| OrderComment {
                status,
                comment,
                created_at,
                notified,
            })
            .collect(),
        in_grid: raw.in_grid,
        quote_id: raw.quote_id,
        matched_by_id,
    }
}

/// Assemble [`Category`]: per-scope values with Yes/No labels for the boolean flags,
/// the admin-style breadcrumb, and the visibility walk — the category's own effectively
/// inactive scopes plus every ancestor whose inactivity hides the subtree.
#[cfg(feature = "db")]
fn to_category(raw: db::DbCategoryCard) -> Category {
    let scope_name = |store_id: u32| -> String {
        if store_id == 0 {
            "default".to_string()
        } else {
            let code = raw
                .stores
                .get(&store_id)
                .cloned()
                .unwrap_or_else(|| format!("{store_id}"));
            format!("stores/{code}")
        }
    };

    let mut values: Vec<ProductValue> = Vec::new();
    for v in &raw.values {
        let label = match (v.attribute.as_str(), v.value.as_deref()) {
            ("is_active" | "include_in_menu" | "is_anchor", Some("1")) => {
                Some("Yes".to_string())
            }
            ("is_active" | "include_in_menu" | "is_anchor", Some("0")) => Some("No".to_string()),
            _ => None,
        };
        let scope = ProductScopeValue {
            store: scope_name(v.store_id),
            label,
            value: v.value.clone().unwrap_or_else(|| "NULL".to_string()),
        };
        match values.iter_mut().find(|e| e.attribute == v.attribute) {
            Some(e) => e.scopes.push(scope),
            None => values.push(ProductValue {
                attribute: v.attribute.clone(),
                backend_type: v.backend_type.clone(),
                input: v.input.clone(),
                scopes: vec![scope],
            }),
        }
    }
    for v in &mut values {
        v.scopes.sort_by(|a, b| {
            (a.store != "default").cmp(&(b.store != "default")).then_with(|| a.store.cmp(&b.store))
        });
    }
    const ORDER: [&str; 10] = [
        "name",
        "is_active",
        "include_in_menu",
        "is_anchor",
        "url_key",
        "url_path",
        "display_mode",
        "available_sort_by",
        "default_sort_by",
        "landing_page",
    ];
    let rank = |a: &str| ORDER.iter().position(|f| *f == a).unwrap_or(ORDER.len());
    values.sort_by(|a, b| {
        rank(&a.attribute).cmp(&rank(&b.attribute)).then_with(|| a.attribute.cmp(&b.attribute))
    });

    // Effective inactivity per entity: the default row unless a store row overrides it.
    // No is_active row at all = active (the attribute default).
    let store_ids: Vec<u32> = {
        let mut v: Vec<u32> = raw.stores.keys().copied().filter(|&s| s > 0).collect();
        v.sort();
        v
    };
    let inactive_scopes = |entity: u32| -> Vec<String> {
        let row = |store: u32| {
            raw.active_rows
                .iter()
                .find(|(e, s, _)| *e == entity && *s == store)
                .and_then(|(_, _, v)| *v)
        };
        match row(0) {
            Some(0) => {
                let enabling: Vec<u32> =
                    store_ids.iter().copied().filter(|&s| row(s) == Some(1)).collect();
                if enabling.is_empty() {
                    vec!["all scopes".to_string()]
                } else {
                    let mut out = vec!["default".to_string()];
                    out.extend(
                        store_ids
                            .iter()
                            .copied()
                            .filter(|&s| row(s) != Some(1))
                            .map(scope_name),
                    );
                    out
                }
            }
            _ => store_ids.iter().copied().filter(|&s| row(s) == Some(0)).map(scope_name).collect(),
        }
    };
    let mut visibility: Vec<CategoryVisibilityIssue> = Vec::new();
    let own = inactive_scopes(raw.id);
    if !own.is_empty() {
        visibility.push(CategoryVisibilityIssue {
            ancestor_id: None,
            ancestor_name: None,
            scopes: own,
        });
    }
    for (aid, aname) in &raw.ancestors {
        let scopes = inactive_scopes(*aid);
        if !scopes.is_empty() {
            visibility.push(CategoryVisibilityIssue {
                ancestor_id: Some(*aid),
                ancestor_name: Some(aname.clone()),
                scopes,
            });
        }
    }

    // Admin-style breadcrumb: ancestors past the tree root.
    let breadcrumb = raw
        .ancestors
        .iter()
        .skip(1)
        .map(|(_, n)| n.as_str())
        .collect::<Vec<_>>()
        .join(" > ");
    let parent_name = raw.ancestors.last().map(|(_, n)| n.clone());

    Category {
        id: raw.id,
        path: raw.path,
        level: raw.level,
        position: raw.position,
        parent_id: (raw.parent_id > 0).then_some(raw.parent_id),
        parent_name,
        children: raw.children,
        breadcrumb,
        values,
        visibility,
        direct_products: raw.direct_products,
        indexed: raw
            .indexed
            .into_iter()
            .map(|(store, products)| CategoryIndexCount { store, products })
            .collect(),
        rewrites: raw
            .rewrites
            .into_iter()
            .map(|(request_path, store, redirect)| ProductRewrite { request_path, store, redirect })
            .collect(),
        root_of: raw.root_of,
        products: raw
            .products
            .into_iter()
            .map(|(entity_id, sku, name, position)| CategoryProduct {
                entity_id,
                sku,
                name,
                position,
            })
            .collect(),
        indexed_store: raw.indexed_store,
        indexed_products: raw.indexed_products.map(|rows| {
            rows.into_iter()
                .map(|(entity_id, sku, name, position, is_parent, visibility)| {
                    CategoryIndexedProduct {
                        entity_id,
                        sku,
                        name,
                        position,
                        via_anchor: !is_parent,
                        visibility,
                    }
                })
                .collect()
        }),
    }
}

/// Assemble [`ProductPrices`]: the EAV price attributes reuse the product scope
/// grouping; tier/rule/index rows resolve website codes and customer-group names.
#[cfg(feature = "db")]
fn to_product_prices(raw: db::DbProductPrices, matched_by_id: bool) -> ProductPrices {
    let website = |id: u32| -> String {
        if id == 0 {
            "(all)".to_string()
        } else {
            raw.websites.get(&id).cloned().unwrap_or_else(|| format!("website/{id}"))
        }
    };
    let group = |id: u32| -> String {
        raw.customer_groups.get(&id).cloned().unwrap_or_else(|| format!("group/{id}"))
    };

    let mut attributes: Vec<ProductValue> = Vec::new();
    for v in &raw.values {
        let store = if v.store_id == 0 {
            "default".to_string()
        } else {
            let code = raw
                .stores
                .get(&v.store_id)
                .cloned()
                .unwrap_or_else(|| format!("{}", v.store_id));
            format!("stores/{code}")
        };
        let scope = ProductScopeValue {
            store,
            label: None,
            value: v.value.clone().unwrap_or_else(|| "NULL".to_string()),
        };
        match attributes.iter_mut().find(|e| e.attribute == v.attribute) {
            Some(e) => e.scopes.push(scope),
            None => attributes.push(ProductValue {
                attribute: v.attribute.clone(),
                backend_type: v.backend_type.clone(),
                input: v.input.clone(),
                scopes: vec![scope],
            }),
        }
    }
    for v in &mut attributes {
        v.scopes.sort_by(|a, b| {
            (a.store != "default").cmp(&(b.store != "default")).then_with(|| a.store.cmp(&b.store))
        });
    }
    const ORDER: [&str; 7] = [
        "price",
        "special_price",
        "special_from_date",
        "special_to_date",
        "cost",
        "msrp",
        "minimal_price",
    ];
    let rank = |a: &str| ORDER.iter().position(|f| *f == a).unwrap_or(ORDER.len());
    attributes.sort_by(|a, b| {
        rank(&a.attribute).cmp(&rank(&b.attribute)).then_with(|| a.attribute.cmp(&b.attribute))
    });

    ProductPrices {
        entity_id: raw.entity_id,
        sku: raw.sku,
        type_id: raw.type_id,
        price_scope_website: raw.price_scope_website,
        attributes,
        tier_prices: raw
            .tiers
            .into_iter()
            .map(|(w, all, g, qty, value, percentage)| TierPrice {
                website: website(w),
                customer_group: if all { "ALL GROUPS".to_string() } else { group(g) },
                qty,
                value,
                percentage,
            })
            .collect(),
        rule_prices: raw
            .rules
            .into_iter()
            .map(|(date, g, w, rule_price)| RulePrice {
                date,
                website: website(w),
                customer_group: group(g),
                rule_price,
            })
            .collect(),
        index: raw
            .index
            .into_iter()
            .map(|(g, w, price, final_price, min_price, max_price, tier_price)| IndexedPrice {
                website: website(w),
                customer_group: group(g),
                price,
                final_price,
                min_price,
                max_price,
                tier_price,
            })
            .collect(),
        children: raw
            .children
            .into_iter()
            .map(|c| ChildPrice {
                sku: c.sku,
                entity_id: c.entity_id,
                enabled: c.enabled,
                price: c.price,
                special_price: c.special,
                final_min: c.final_min,
                final_max: c.final_max,
                selection_price: c.selection_price,
                selection_percent: c.selection_percent,
            })
            .collect(),
        bundle_price_type: raw.bundle_price_type,
        matched_by_id,
    }
}

/// Assemble the public [`Product`] from the raw rows: group values per attribute with
/// the default scope first, and resolve human labels where the data allows — Yes/No for
/// booleans, the `Status`/`Visibility` source-model constants (hardcoded faithfully to
/// core), tax classes from `tax_class`, and admin option labels for table-source
/// select/multiselect values.
#[cfg(feature = "db")]
fn to_product_links(raw: db::DbProductLinks, matched_by_id: bool) -> ProductLinks {
    let conv = |t: db::DbLinkTarget| {
        let enabled = t.status.map(|s| s == 1);
        let visibility = t.visibility.map(|v| {
            match v {
                1 => "Not Visible Individually",
                2 => "Catalog",
                3 => "Search",
                4 => "Catalog, Search",
                _ => "?",
            }
            .to_string()
        });
        // Won't render in the block: disabled, or Not Visible Individually.
        let hidden = enabled == Some(false) || t.visibility == Some(1);
        ProductLinkTarget {
            position: t.position,
            sku: t.sku,
            name: t.name,
            enabled,
            visibility,
            in_stock: t.in_stock,
            hidden,
        }
    };
    ProductLinks {
        entity_id: raw.entity_id,
        sku: raw.sku,
        type_id: raw.type_id,
        name: raw.name,
        reverse: raw.reverse,
        related: raw.related.into_iter().map(conv).collect(),
        up_sells: raw.up_sells.into_iter().map(conv).collect(),
        cross_sells: raw.cross_sells.into_iter().map(conv).collect(),
        matched_by_id,
    }
}

#[cfg(feature = "db")]
fn to_product(raw: db::DbProduct, matched_by_id: bool) -> Product {
    let label_of = |v: &db::DbProductValue, value: &str| -> Option<String> {
        match (v.attribute.as_str(), v.input.as_deref()) {
            ("status", _) => match value {
                "1" => Some("Enabled".to_string()),
                "2" => Some("Disabled".to_string()),
                _ => None,
            },
            ("visibility", _) => match value {
                "1" => Some("Not Visible Individually".to_string()),
                "2" => Some("Catalog".to_string()),
                "3" => Some("Search".to_string()),
                "4" => Some("Catalog, Search".to_string()),
                _ => None,
            },
            ("tax_class_id", _) => {
                value.parse::<u32>().ok().and_then(|id| raw.tax_classes.get(&id).cloned())
            }
            (_, Some("boolean")) => match value {
                "1" => Some("Yes".to_string()),
                "0" => Some("No".to_string()),
                _ => None,
            },
            (_, Some("select")) => value
                .parse::<u32>()
                .ok()
                .and_then(|o| raw.option_labels.get(&(v.attribute_id, o)).cloned()),
            (_, Some("multiselect")) => {
                let labels: Vec<String> = value
                    .split(',')
                    .filter_map(|part| {
                        part.trim()
                            .parse::<u32>()
                            .ok()
                            .and_then(|o| raw.option_labels.get(&(v.attribute_id, o)).cloned())
                    })
                    .collect();
                (!labels.is_empty()).then(|| labels.join(", "))
            }
            _ => None,
        }
    };

    let mut values: Vec<ProductValue> = Vec::new();
    for v in &raw.values {
        // The `config` scope convention: `default` = store_id 0, else `stores/<code>` —
        // a store view *coded* "default" (nearly every install has one) must not collide
        // with the default scope.
        let store = if v.store_id == 0 {
            "default".to_string()
        } else {
            let code = raw
                .stores
                .get(&v.store_id)
                .cloned()
                .unwrap_or_else(|| format!("{}", v.store_id));
            format!("stores/{code}")
        };
        let value = v.value.clone().unwrap_or_else(|| "NULL".to_string());
        let scope = ProductScopeValue {
            store,
            label: v.value.as_deref().and_then(|val| label_of(v, val)),
            value,
        };
        match values.iter_mut().find(|e| e.attribute == v.attribute) {
            Some(e) => e.scopes.push(scope),
            None => values.push(ProductValue {
                attribute: v.attribute.clone(),
                backend_type: v.backend_type.clone(),
                input: v.input.clone(),
                scopes: vec![scope],
            }),
        }
    }
    for v in &mut values {
        v.scopes.sort_by(|a, b| {
            (a.store != "default").cmp(&(b.store != "default")).then_with(|| a.store.cmp(&b.store))
        });
    }
    // The everyday attributes first, the rest alphabetical.
    const FIRST: [&str; 6] = ["name", "status", "visibility", "price", "special_price", "url_key"];
    let rank = |a: &str| FIRST.iter().position(|f| *f == a).unwrap_or(FIRST.len());
    values.sort_by(|a, b| {
        rank(&a.attribute).cmp(&rank(&b.attribute)).then_with(|| a.attribute.cmp(&b.attribute))
    });

    // Image roles come for free from the product's role attributes (default scope): the
    // file a gallery entry fills as base/small/thumbnail/swatch. `no_selection` = unset.
    let roles_by_file: Vec<(String, &str)> =
        [("image", "base"), ("small_image", "small"), ("thumbnail", "thumbnail"), ("swatch_image", "swatch")]
            .iter()
            .filter_map(|(attr, role)| {
                raw.values
                    .iter()
                    .find(|v| v.attribute == *attr && v.store_id == 0)
                    .and_then(|v| v.value.as_deref())
                    .filter(|s| !s.is_empty() && *s != "no_selection")
                    .map(|f| (f.to_string(), *role))
            })
            .collect();
    let media: Vec<ProductMedia> = raw
        .media
        .into_iter()
        .map(|(file, media_type, label, position, disabled)| {
            let roles = roles_by_file
                .iter()
                .filter(|(f, _)| *f == file)
                .map(|(_, r)| r.to_string())
                .collect();
            ProductMedia { file, media_type, label, position, disabled, roles }
        })
        .collect();

    Product {
        entity_id: raw.entity_id,
        sku: raw.sku,
        type_id: raw.type_id,
        attribute_set: raw.attribute_set,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        websites: raw.websites,
        values,
        stock: raw
            .stock
            .into_iter()
            .map(|(source, quantity, in_stock)| ProductSourceStock { source, quantity, in_stock })
            .collect(),
        legacy_stock: raw
            .legacy_stock
            .map(|(qty, in_stock, manage_stock)| ProductLegacyStock { qty, in_stock, manage_stock }),
        categories: raw
            .categories
            .into_iter()
            .map(|(id, breadcrumb)| ProductCategory { id, breadcrumb })
            .collect(),
        rewrites: raw
            .rewrites
            .into_iter()
            .map(|(request_path, store, redirect)| ProductRewrite { request_path, store, redirect })
            .collect(),
        media,
        parents: raw.parents,
        super_attributes: raw.super_attributes,
        children: raw
            .children
            .into_iter()
            .map(|(entity_id, sku, enabled, options, qty, in_stock, default_qty)| ProductChild {
                sku,
                entity_id,
                enabled,
                options,
                qty,
                in_stock,
                default_qty,
            })
            .collect(),
        bundle_options: raw
            .bundle_options
            .into_iter()
            .map(|o| BundleOption {
                title: o.title,
                required: o.required,
                input_type: o.input_type,
                selections: o
                    .selections
                    .into_iter()
                    .map(
                        |(entity_id, sku, enabled, qty, is_default, price, price_percent, in_stock)| {
                            BundleSelection {
                                sku,
                                entity_id,
                                enabled,
                                qty,
                                is_default,
                                price,
                                price_percent,
                                in_stock,
                            }
                        },
                    )
                    .collect(),
            })
            .collect(),
        matched_by_id,
    }
}

/// Map a raw DB attribute row to the public type: decode `is_global`, split `apply_to`,
/// and derive the value table (`<entity_table>_<backend_type>`, honoring the rare
/// `value_table_prefix`; `static` attributes live on the entity table itself).
#[cfg(feature = "db")]
fn to_eav_attribute(r: db::DbEavAttribute, entities: &[db::DbEavEntity]) -> EavAttribute {
    let entity = entities.iter().find(|e| e.code == r.entity_code);
    let value_table = if r.backend_type == "static" {
        None
    } else {
        entity.and_then(|e| {
            let base = e.value_table_prefix.clone().or_else(|| e.entity_table.clone())?;
            Some(format!("{base}_{}", r.backend_type))
        })
    };
    EavAttribute {
        code: r.code,
        entity_type: r.entity_code,
        attribute_id: r.attribute_id,
        label: r.label,
        backend_type: r.backend_type,
        frontend_input: r.frontend_input,
        required: r.required,
        unique: r.unique,
        user_defined: r.user_defined,
        default_value: r.default_value,
        source_model: r.source_model.map(ClassName::new),
        backend_model: r.backend_model.map(ClassName::new),
        frontend_model: r.frontend_model.map(ClassName::new),
        catalog: r.catalog.map(|c| EavCatalogFlags {
            scope: match c.is_global {
                1 => EavScope::Global,
                2 => EavScope::Website,
                _ => EavScope::Store,
            },
            searchable: c.searchable,
            filterable: c.filterable,
            filterable_in_search: c.filterable_in_search,
            comparable: c.comparable,
            used_in_listing: c.used_in_listing,
            used_for_sort_by: c.used_for_sort_by,
            visible_on_front: c.visible_on_front,
            apply_to: c
                .apply_to
                .map(|a| {
                    a.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        }),
        value_table,
    }
}
