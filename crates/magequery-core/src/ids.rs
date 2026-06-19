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

string_newtype! {
    /// A fully-qualified PHP class or interface name, e.g. `Magento\Catalog\Model\Product`.
    /// Stored without a leading backslash.
    ClassName
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

impl ClassName {
    /// Heuristic only — true if the name ends in `Interface`. Real interface-ness is
    /// determined by parsing the PHP declaration; this is just a cheap hint.
    pub fn looks_like_interface(&self) -> bool {
        self.0.ends_with("Interface")
    }
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
