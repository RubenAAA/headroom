//! Prints one line per top-level item of a Rust file:
//! `start_line end_line kind name`. `start_line` includes attributes and
//! doc comments. Impls print as `Type` or `Trait@for@Type`.

use quote::ToTokens;
use syn::spanned::Spanned;

fn tokens(t: &impl ToTokens) -> String {
    t.to_token_stream().to_string().replace(' ', "")
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: itemspan <file.rs>");
    let src = std::fs::read_to_string(&path).expect("readable file");
    let file = syn::parse_file(&src).expect("parseable Rust");
    for item in &file.items {
        let (kind, name, attrs): (&str, String, &[syn::Attribute]) = match item {
            syn::Item::Fn(f) => ("fn", f.sig.ident.to_string(), &f.attrs),
            syn::Item::Struct(s) => ("struct", s.ident.to_string(), &s.attrs),
            syn::Item::Enum(e) => ("enum", e.ident.to_string(), &e.attrs),
            syn::Item::Const(c) => ("const", c.ident.to_string(), &c.attrs),
            syn::Item::Static(s) => ("static", s.ident.to_string(), &s.attrs),
            syn::Item::Type(t) => ("type", t.ident.to_string(), &t.attrs),
            syn::Item::Trait(t) => ("trait", t.ident.to_string(), &t.attrs),
            syn::Item::Mod(m) => ("mod", m.ident.to_string(), &m.attrs),
            syn::Item::Use(u) => ("use", "-".to_string(), &u.attrs),
            syn::Item::Macro(m) => ("macro", tokens(&m.mac.path), &m.attrs),
            syn::Item::Impl(i) => {
                let ty = tokens(&i.self_ty);
                let name = match &i.trait_ {
                    Some((_, p, _)) => format!("{}@for@{ty}", tokens(p)),
                    None => ty,
                };
                ("impl", name, &i.attrs)
            }
            _ => ("other", "-".to_string(), &[]),
        };
        let span = item.span();
        let start = attrs
            .iter()
            .map(|a| a.span().start().line)
            .fold(span.start().line, usize::min);
        println!("{start} {} {kind} {name}", span.end().line);
    }
}
