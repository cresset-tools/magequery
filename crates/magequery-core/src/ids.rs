//! Domain newtypes. The public API is typed, never stringly-typed: a function that
//! wants a class name takes a [`ClassName`], not a `&str`.

use std::fmt;
use std::str::FromStr;

macro_rules! string_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[derive(serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_owned()) }
        }
        impl FromStr for $name {
            type Err = std::convert::Infallible;
            fn from_str(s: &str) -> Result<Self, Self::Err> { Ok(Self(s.to_owned())) }
        }
    };
}

/// A fully-qualified PHP class or interface name, e.g. `Magento\Catalog\Model\Product`.
/// Construction strips any leading backslash (`\Foo\Bar` ≡ `Foo\Bar`), mirroring Magento's
/// `ltrim($type, '\\')` at every config-read site — di.xml authors write both spellings
/// (Magento's own module-elasticsearch declares `type="\Magento\…"`), and they must merge
/// and compare as one name. Hand-written rather than via `string_newtype!` for exactly
/// this normalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[derive(serde::Serialize)]
#[serde(transparent)]
pub struct ClassName(String);

impl ClassName {
    pub fn new(s: impl Into<String>) -> Self {
        let s = s.into();
        if s.starts_with('\\') {
            Self(s.trim_start_matches('\\').to_owned())
        } else {
            Self(s)
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Heuristic only — true if the name ends in `Interface`. Real interface-ness is
    /// determined by parsing the PHP declaration; this is just a cheap hint.
    pub fn looks_like_interface(&self) -> bool {
        self.0.ends_with("Interface")
    }
}
impl fmt::Display for ClassName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl From<&str> for ClassName {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}
impl FromStr for ClassName {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(s))
    }
}

string_newtype! {
    /// A Magento module identifier, e.g. `Magento_Catalog`.
    ModuleName
}
string_newtype! {
    /// An event name dispatched via the event manager, e.g. `sales_order_place_after`.
    EventName
}
string_newtype! {
    /// A config path, e.g. `web/secure/base_url`.
    ConfigPath
}

/// A Magento application area. This is the fixed Magento 2.4 OSS set, in a stable order
/// (`Global` first, since every real area is `Global` overlaid by itself). We never
/// discover areas from the filesystem — `etc/` contains plenty of non-area subdirectories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    Global,
    Frontend,
    Adminhtml,
    Crontab,
    WebapiRest,
    WebapiSoap,
    Graphql,
}

impl Area {
    /// Every area, in canonical order. `Global` leads.
    pub const ALL: [Area; 7] = [
        Area::Global,
        Area::Frontend,
        Area::Adminhtml,
        Area::Crontab,
        Area::WebapiRest,
        Area::WebapiSoap,
        Area::Graphql,
    ];

    /// The on-disk directory segment under `etc/`, or `None` for [`Area::Global`]
    /// (whose config lives directly in `etc/`, not a subdirectory).
    pub fn dir(self) -> Option<&'static str> {
        match self {
            Area::Global => None,
            Area::Frontend => Some("frontend"),
            Area::Adminhtml => Some("adminhtml"),
            Area::Crontab => Some("crontab"),
            Area::WebapiRest => Some("webapi_rest"),
            Area::WebapiSoap => Some("webapi_soap"),
            Area::Graphql => Some("graphql"),
        }
    }

    pub fn as_str(self) -> &'static str {
        self.dir().unwrap_or("global")
    }
}

impl fmt::Display for Area {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse failure for [`Area`] — the one newtype where an invalid value is a real error.
#[derive(Debug, thiserror::Error)]
#[error("unknown area `{0}` (expected one of: global, frontend, adminhtml, crontab, webapi_rest, webapi_soap, graphql)")]
pub struct UnknownArea(pub String);

impl FromStr for Area {
    type Err = UnknownArea;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "global" => Area::Global,
            "frontend" => Area::Frontend,
            "adminhtml" => Area::Adminhtml,
            "crontab" => Area::Crontab,
            "webapi_rest" => Area::WebapiRest,
            "webapi_soap" => Area::WebapiSoap,
            "graphql" => Area::Graphql,
            other => return Err(UnknownArea(other.to_owned())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ClassName;

    #[test]
    fn class_name_strips_leading_backslash() {
        assert_eq!(ClassName::new("\\Foo\\Bar"), ClassName::new("Foo\\Bar"));
        assert_eq!(ClassName::new("\\Foo\\Bar").as_str(), "Foo\\Bar");
        assert_eq!(ClassName::from("\\Foo").as_str(), "Foo");
        // Interior backslashes are untouched.
        assert_eq!(ClassName::new("Foo\\Bar").as_str(), "Foo\\Bar");
    }
}
