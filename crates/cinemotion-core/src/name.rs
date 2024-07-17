#![feature(decl_macro)]
use bevy_ecs::prelude::*;
use derive_more::Display;
use std::{ops::Deref, sync::Arc};

#[derive(Component, Display, Debug, Clone, PartialEq, Eq, Hash, PartialOrd)]
pub struct Name(Arc<str>);

impl Deref for Name {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Create a new name from a string.
///
/// ```
/// use cinemotion_core::prelude::*;
///
/// let name = "test".to_string();
/// let name = Name::from(name);
/// ```
///
impl From<String> for Name {
    fn from(s: String) -> Self {
        Self(s.into())
    }
}

/// Create a new name from a str.
///
/// ```
/// use cinemotion_core::prelude::*;
/// let name = Name::from("test");
/// ```
///
impl From<&str> for Name {
    fn from(s: &str) -> Self {
        Self(s.into())
    }
}

/// Create a new name from a string literal.
///
/// ```
/// use cinemotion_core::prelude::*;
/// let name = name!("test");
/// ```
///
#[macro_export]
macro_rules! name {
    ($name:expr) => {{
        Name::from($name)
    }};
}
