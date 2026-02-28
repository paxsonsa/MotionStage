use proc_macro::*;
use quote::quote;
use syn::*;

fn capitalize_string(s: &mut String) {
    if let Some(first) = s.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
}
