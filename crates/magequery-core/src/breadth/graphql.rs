//! Static graphql indexes.

use super::*;

// ---------- GraphQL schema (schema.graphqls) ----------

pub(crate) struct GqlIndex {
    types: HashMap<String, GqlType>,
}

impl GqlIndex {
    pub fn build(modules: &[Module], vfs: &Vfs) -> Self {
        let mut types: HashMap<String, GqlType> = HashMap::new();
        for (i, path, raws) in
            read_parse(modules, vfs, Area::Global, "schema.graphqls", crate::graphql::schema_graphqls)
        {
            let module = &modules[i].name;
            let src = |line: u32| Source {
                module: module.clone(),
                file: path.clone(),
                line,
                area: Area::Graphql,
            };
            for r in raws {
                let entry = types.entry(r.name.clone()).or_insert_with(|| GqlType {
                    name: r.name.clone(),
                    kind: kind_of(r.kind),
                    implements: Vec::new(),
                    type_resolver: None,
                    description: None,
                    fields: Vec::new(),
                    values: Vec::new(),
                    members: Vec::new(),
                    source: src(r.line),
                });
                for imp in r.implements {
                    if !entry.implements.contains(&imp) {
                        entry.implements.push(imp);
                    }
                }
                if let Some(tr) = directive_arg(&r.directives, "typeResolver", "class") {
                    entry.type_resolver = Some(ClassName::new(tr));
                }
                if let Some(d) =
                    directive_arg(&r.directives, "doc", "description").or(r.description)
                {
                    entry.description = Some(d);
                }
                for v in r.values {
                    if !entry.values.contains(&v) {
                        entry.values.push(v);
                    }
                }
                for m in r.members {
                    if !entry.members.contains(&m) {
                        entry.members.push(m);
                    }
                }
                // Fields union by name; a re-declaration replaces (last module wins,
                // matching the stitching reader) and takes the newer provenance.
                for f in r.fields {
                    let field = GqlField {
                        name: f.name,
                        args: f
                            .args
                            .into_iter()
                            .map(|a| GqlArg { name: a.name, ty: a.ty })
                            .collect(),
                        ty: f.ty,
                        resolver: directive_arg(&f.directives, "resolver", "class")
                            .map(ClassName::new),
                        description: directive_arg(&f.directives, "doc", "description")
                            .or(f.description),
                        deprecated: f
                            .directives
                            .iter()
                            .find(|d| d.name == "deprecated")
                            .map(|d| {
                                d.args
                                    .iter()
                                    .find(|(k, _)| k == "reason")
                                    .map(|(_, v)| v.clone())
                                    .unwrap_or_default()
                            }),
                        cacheable: directive_arg(&f.directives, "cache", "cacheable")
                            .map(|v| v != "false"),
                        source: src(f.line),
                    };
                    match entry.fields.iter_mut().find(|e| e.name == field.name) {
                        Some(e) => *e = field,
                        None => entry.fields.push(field),
                    }
                }
            }
        }
        Self { types }
    }

    /// One type by exact name.
    pub fn type_(&self, name: &str) -> Option<GqlType> {
        self.types.get(name).cloned()
    }

    /// Types whose name contains `filter` (case-insensitive; all when `None`), by name.
    pub fn types(&self, filter: Option<&str>) -> Vec<GqlType> {
        let needle = filter.map(str::to_lowercase);
        let mut v: Vec<GqlType> = self
            .types
            .values()
            .filter(|t| needle.as_ref().is_none_or(|n| t.name.to_lowercase().contains(n)))
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

fn kind_of(k: crate::graphql::RawGqlKind) -> GqlKind {
    use crate::graphql::RawGqlKind as R;
    match k {
        R::Object => GqlKind::Object,
        R::Interface => GqlKind::Interface,
        R::Input => GqlKind::Input,
        R::Enum => GqlKind::Enum,
        R::Union => GqlKind::Union,
        R::Scalar => GqlKind::Scalar,
    }
}

/// The value of `@directive(arg: …)`, when present.
fn directive_arg(
    directives: &[crate::graphql::RawDirective],
    directive: &str,
    arg: &str,
) -> Option<String> {
    directives
        .iter()
        .find(|d| d.name == directive)?
        .args
        .iter()
        .find(|(k, _)| k == arg)
        .map(|(_, v)| v.clone())
}
