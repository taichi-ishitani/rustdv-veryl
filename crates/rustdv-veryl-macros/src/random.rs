//! Macros of `rustdv_veryl::random`: `#[randomize]`, `#[constraints]`,
//! `#[constraint_fn]`, `#[derive(RandEnum)]`, `randomize_with!`,
//! `std_randomize!` and `randcase!`.
//!
//! Constraint bodies are parsed as Rust and turned into code that builds
//! the constraint IR at run time through `random::__rt::Builder`.

use proc_macro2::{Span, TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    BinOp, Block, Expr, ExprLit, Fields, FnArg, Ident, ImplItem, ItemEnum, ItemFn, ItemImpl,
    ItemStruct, Lit, Pat, Path, RangeLimits, ReturnType, Stmt, Token, Type, UnOp,
};

fn rt() -> TokenStream2 {
    quote!(::rustdv_veryl::random::__rt)
}

/// The builder variable of generated code.
fn bv() -> Ident {
    Ident::new("__rv_b", Span::call_site())
}

fn err(span: Span, msg: impl std::fmt::Display) -> syn::Error {
    syn::Error::new(span, msg)
}

fn upper(name: &str) -> String {
    name.to_uppercase()
}

fn text_of(tokens: &TokenStream2) -> String {
    let mut text = tokens.to_string();
    for (spaced, tight) in [
        (" . ", "."),
        (" (", "("),
        ("( ", "("),
        (" )", ")"),
        (" [", "["),
        ("[ ", "["),
        (" ]", "]"),
        (" ,", ","),
        (" ;", ";"),
        (" :: ", "::"),
        (" !", "!"),
        ("! ", "!"),
    ] {
        text = text.replace(spaced, tight);
    }
    text.trim_end_matches(';').to_string()
}

// ===========================================================================
// Constraint bodies
// ===========================================================================

#[derive(Clone, PartialEq, Eq)]
enum Mode {
    /// `#[constraints] impl T`: single identifiers are fields of `T`.
    Type,
    /// `#[constraints(for T)] impl Set`: as `Type`; `self.x` is the set's.
    Set,
    /// `#[constraint_fn]`: single identifiers are parameters.
    Fn { name: String, params: Vec<String> },
    /// `randomize_with!`: fields, checked at run time; `local::x`.
    Inline,
    /// `std_randomize!`: the randomized locals.
    Std,
}

struct Cg {
    mode: Mode,
    loop_vars: Vec<String>,
    /// Field names used, for the compile-time check.
    fields: Vec<(String, Span)>,
    tmp: usize,
}

impl Cg {
    fn new(mode: Mode) -> Cg {
        Cg {
            mode,
            loop_vars: Vec::new(),
            fields: Vec::new(),
            tmp: 0,
        }
    }

    fn tmp(&mut self) -> Ident {
        self.tmp += 1;
        format_ident!("__rv_t{}", self.tmp)
    }

    // ---- statements ----

    fn block(&mut self, block: &Block) -> syn::Result<TokenStream2> {
        let mut out = TokenStream2::new();
        for s in &block.stmts {
            out.extend(self.stmt(s, true)?);
        }
        Ok(out)
    }

    /// Statements of a constraint body. A tail expression (no `;`) is a
    /// constraint too, except where the caller takes it.
    fn stmt(&mut self, s: &Stmt, tail_ok: bool) -> syn::Result<TokenStream2> {
        match s {
            Stmt::Expr(e, semi) => {
                let block_like = matches!(e, Expr::If(_) | Expr::ForLoop(_) | Expr::Block(_));
                if semi.is_none() && !tail_ok && !block_like {
                    return Err(err(e.span(), "expected `;`"));
                }
                self.stmt_expr(e)
            }
            Stmt::Macro(m) => self.stmt_macro(&m.mac),
            Stmt::Local(l) => Err(err(
                l.span(),
                "`let` is not allowed in constraints; use `{ expr }` for a Rust value",
            )),
            Stmt::Item(i) => Err(err(i.span(), "items are not allowed in constraints")),
        }
    }

    fn stmt_expr(&mut self, e: &Expr) -> syn::Result<TokenStream2> {
        let b = bv();
        match e {
            Expr::If(i) => self.stmt_if(i),
            Expr::ForLoop(f) => self.stmt_for(f),
            Expr::Macro(m) => self.stmt_macro(&m.mac),
            Expr::Call(c) if is_ident(&c.func, "soft") => {
                let [arg] = args::<1>(c)?;
                let v = self.expr(arg)?;
                let loc = loc(e.span());
                let text = text_of(&quote!(#e));
                Ok(quote! { { let __rv_c = #v; #b.soft(__rv_c, #loc, #text); } })
            }
            Expr::Call(c) if is_ident(&c.func, "unique") => {
                let [arg] = args::<1>(c)?;
                let list = self.operand_list(arg)?;
                let loc = loc(e.span());
                let text = text_of(&quote!(#e));
                Ok(quote! { { let __rv_l = #list; #b.unique(__rv_l, #loc, #text); } })
            }
            Expr::Call(c) if is_ident(&c.func, "solve_before") => {
                let [x, y] = args::<2>(c)?;
                let (x, y) = (self.operand_list(x)?, self.operand_list(y)?);
                Ok(quote! { {
                    let __rv_x = #x;
                    let __rv_y = #y;
                    #b.solve_before(__rv_x, __rv_y);
                } })
            }
            _ => {
                let v = self.expr(e)?;
                let loc = loc(e.span());
                let text = text_of(&quote!(#e));
                Ok(quote! { { let __rv_c = #v; #b.hard(__rv_c, #loc, #text); } })
            }
        }
    }

    fn stmt_if(&mut self, i: &syn::ExprIf) -> syn::Result<TokenStream2> {
        let b = bv();
        let c = self.expr(&i.cond)?;
        let then = self.block(&i.then_branch)?;
        let els = match &i.else_branch {
            None => TokenStream2::new(),
            Some((_, e)) => match &**e {
                Expr::Block(bl) => self.block(&bl.block)?,
                Expr::If(ei) => self.stmt_if(ei)?,
                other => return Err(err(other.span(), "expected a block")),
            },
        };
        Ok(quote! { {
            let __rv_c = #c;
            let __rv_k = #b.if_begin(__rv_c);
            if __rv_k != ::core::option::Option::Some(false) { #then }
            #b.if_else(__rv_k);
            if __rv_k != ::core::option::Option::Some(true) { #els }
            #b.if_end(__rv_k);
        } })
    }

    fn stmt_for(&mut self, f: &syn::ExprForLoop) -> syn::Result<TokenStream2> {
        let b = bv();
        let Pat::Ident(pi) = &*f.pat else {
            return Err(err(f.pat.span(), "expected a loop variable"));
        };
        let var = pi.ident.clone();
        let Expr::Range(r) = &*f.expr else {
            return Err(err(f.expr.span(), "expected a range `lo..hi`"));
        };
        let (Some(lo), Some(hi)) = (&r.start, &r.end) else {
            return Err(err(r.span(), "the range needs both bounds"));
        };
        let (lo, hi) = (self.expr(lo)?, self.expr(hi)?);
        let inclusive = matches!(r.limits, RangeLimits::Closed(_));
        self.loop_vars.push(var.to_string());
        let body = self.block(&f.body);
        self.loop_vars.pop();
        let body = body?;
        Ok(quote! { {
            let __rv_lo = #lo;
            let __rv_hi = #hi;
            for #var in #b.range(__rv_lo, __rv_hi, #inclusive) {
                let _ = #var;
                #body
            }
        } })
    }

    fn stmt_macro(&mut self, m: &syn::Macro) -> syn::Result<TokenStream2> {
        if !m.path.is_ident("dist") {
            return Err(err(m.span(), "only `dist!` is allowed in constraints"));
        }
        let b = bv();
        let d: DistArgs = m.parse_body()?;
        let x = self.expr(&d.x)?;
        let mut entries = TokenStream2::new();
        for en in &d.entries {
            let (lo, hi, inclusive) = match &en.key {
                Expr::Range(r) => {
                    let (Some(lo), Some(hi)) = (&r.start, &r.end) else {
                        return Err(err(r.span(), "the range needs both bounds"));
                    };
                    (
                        self.expr(lo)?,
                        self.expr(hi)?,
                        matches!(r.limits, RangeLimits::Closed(_)),
                    )
                }
                k => {
                    // A single value: evaluated once, both bounds.
                    let v = self.expr(k)?;
                    let t = self.tmp();
                    entries.extend(quote!(let #t = #v;));
                    (quote!(#t.clone()), quote!(#t.clone()), true)
                }
            };
            let w = self.expr(&en.weight)?;
            let each = en.each;
            entries.extend(quote! {
                let __rv_lo = #lo;
                let __rv_hi = #hi;
                let __rv_wt = #w;
                __rv_r.push((__rv_lo, __rv_hi, #inclusive, __rv_wt, #each));
            });
        }
        let loc = loc(m.span());
        let text = text_of(&quote!(#m));
        Ok(quote! { {
            let __rv_x = #x;
            let mut __rv_r = ::std::vec::Vec::new();
            #entries
            #b.dist(__rv_x, __rv_r, #loc, #text);
        } })
    }

    /// `[a, b]` as a list of operands, or one operand (an array).
    fn operand_list(&mut self, e: &Expr) -> syn::Result<TokenStream2> {
        let items: Vec<&Expr> = match e {
            Expr::Array(a) => a.elems.iter().collect(),
            other => vec![other],
        };
        let mut out = quote!(let mut __rv_v = ::std::vec::Vec::new(););
        for it in items {
            let v = self.expr(it)?;
            out.extend(quote!(__rv_v.push(#v);));
        }
        Ok(quote!({ #out __rv_v }))
    }

    // ---- expressions ----

    fn expr(&mut self, e: &Expr) -> syn::Result<TokenStream2> {
        let b = bv();
        let rt = rt();
        match e {
            Expr::Lit(ExprLit { lit, .. }) => match lit {
                Lit::Int(i) => {
                    let d = i.base10_digits();
                    Ok(quote!(#b.lit(#d)))
                }
                Lit::Bool(v) => {
                    let v = v.value;
                    Ok(quote!(#b.bool_lit(#v)))
                }
                other => Err(err(other.span(), "only integer and bool literals")),
            },
            Expr::Paren(p) => self.expr(&p.expr),
            Expr::Group(g) => self.expr(&g.expr),
            Expr::Path(p) => self.path(&p.path),
            Expr::Block(bl) => {
                self.check_rust_block(&bl.block)?;
                Ok(quote!(#b.konst(&(#bl))))
            }
            Expr::Field(f) => {
                let syn::Member::Named(name) = &f.member else {
                    return Err(err(f.member.span(), "expected a field name"));
                };
                if let Expr::Path(p) = &*f.base
                    && p.path.is_ident("self")
                {
                    if self.mode != Mode::Set {
                        return Err(err(
                            f.span(),
                            "`self.x` is only for the fields of a constraint set; write the field name alone",
                        ));
                    }
                    return Ok(quote!(#b.konst(&(#f))));
                }
                let base = self.expr(&f.base)?;
                let n = name.unraw().to_string();
                let t = self.tmp();
                Ok(quote!({ let #t = #base; #b.member(#t, #n) }))
            }
            Expr::Index(ix) => {
                let base = self.expr(&ix.expr)?;
                let idx = self.expr(&ix.index)?;
                let (t1, t2) = (self.tmp(), self.tmp());
                Ok(quote!({ let #t1 = #base; let #t2 = #idx; #b.index(#t1, #t2) }))
            }
            Expr::Unary(u) => {
                let op = match u.op {
                    UnOp::Neg(_) => quote!(#rt::UnOp::Neg),
                    UnOp::Not(_) => quote!(#rt::UnOp::LNot),
                    _ => return Err(err(u.span(), "unsupported operator")),
                };
                let a = self.expr(&u.expr)?;
                let t = self.tmp();
                Ok(quote!({ let #t = #a; #b.un(#op, #t) }))
            }
            Expr::Binary(bi) => {
                let op = bin_op(&bi.op)?;
                let l = self.expr(&bi.left)?;
                let r = self.expr(&bi.right)?;
                let (t1, t2) = (self.tmp(), self.tmp());
                Ok(quote!({ let #t1 = #l; let #t2 = #r; #b.bin(#rt::BinOp::#op, #t1, #t2) }))
            }
            Expr::Cast(c) => {
                let a = self.expr(&c.expr)?;
                let ty = &c.ty;
                let t = self.tmp();
                Ok(quote!({ let #t = #a; #b.cast::<#ty>(#t) }))
            }
            Expr::MethodCall(m) => self.method(m),
            Expr::Call(c) => self.call(c),
            other => Err(err(
                other.span(),
                "unsupported in constraints (see the constraint items in docs/random.md)",
            )),
        }
    }

    fn path(&mut self, p: &Path) -> syn::Result<TokenStream2> {
        let b = bv();
        if let Some(id) = p.get_ident() {
            let name = id.unraw().to_string();
            if self.loop_vars.contains(&name) {
                return Ok(quote!(#b.index_var(#id)));
            }
            if let Mode::Fn { params, .. } = &self.mode {
                if params.contains(&name) {
                    return Ok(quote!(#id.clone()));
                }
                return Err(err(
                    id.span(),
                    format!(
                        "`{name}` is not a parameter; a constraint function sees only its parameters"
                    ),
                ));
            }
            if name == "self" {
                return Err(err(id.span(), "`self` alone is not a value"));
            }
            if matches!(self.mode, Mode::Type | Mode::Set) {
                self.fields.push((name.clone(), id.span()));
            }
            return Ok(quote!(#b.field(#name)));
        }
        // `local::x`: a variable of the caller.
        if p.segments.len() == 2 && p.segments[0].ident == "local" {
            if self.mode != Mode::Inline {
                return Err(err(p.span(), "`local::` is only for `randomize_with!`"));
            }
            let x = &p.segments[1].ident;
            return Ok(quote!(#b.konst(&#x)));
        }
        Ok(quote!(#b.konst(&(#p))))
    }

    fn method(&mut self, m: &syn::ExprMethodCall) -> syn::Result<TokenStream2> {
        let b = bv();
        let rt = rt();
        let name = m.method.to_string();
        let recv = self.expr(&m.receiver)?;
        let t = self.tmp();
        match name.as_str() {
            "len" => Ok(quote!({ let #t = #recv; #b.len(#t) })),
            "sum" | "product" | "and" | "or" | "xor" => {
                Ok(quote!({ let #t = #recv; #b.reduce(#t, #name) }))
            }
            "inside" => {
                if m.args.len() != 1 {
                    return Err(err(m.span(), "`inside` takes one list"));
                }
                match &m.args[0] {
                    Expr::Array(a) => {
                        let mut items = quote!(let mut __rv_i = ::std::vec::Vec::new(););
                        for el in &a.elems {
                            match el {
                                Expr::Range(r) => {
                                    let (Some(lo), Some(hi)) = (&r.start, &r.end) else {
                                        return Err(err(r.span(), "the range needs both bounds"));
                                    };
                                    let (lo, hi) = (self.expr(lo)?, self.expr(hi)?);
                                    let inc = matches!(r.limits, RangeLimits::Closed(_));
                                    items.extend(quote! {
                                        let __rv_lo = #lo;
                                        let __rv_hi = #hi;
                                        __rv_i.push(#rt::InsideItem::Range(__rv_lo, __rv_hi, #inc));
                                    });
                                }
                                v => {
                                    let v = self.expr(v)?;
                                    items.extend(quote! {
                                        let __rv_v = #v;
                                        __rv_i.push(#rt::InsideItem::Value(__rv_v));
                                    });
                                }
                            }
                        }
                        Ok(quote!({ let #t = #recv; #items #b.inside(#t, __rv_i) }))
                    }
                    Expr::Block(bl) => {
                        self.check_rust_block(&bl.block)?;
                        Ok(quote!({ let #t = #recv; #b.inside_set(#t, &(#bl)) }))
                    }
                    other => Err(err(
                        other.span(),
                        "`inside` takes `[values and ranges]` or `{ a Rust collection }`",
                    )),
                }
            }
            _ => Err(err(m.method.span(), format!("unknown method `{name}`"))),
        }
    }

    fn call(&mut self, c: &syn::ExprCall) -> syn::Result<TokenStream2> {
        let b = bv();
        let rt = rt();
        if is_ident(&c.func, "bit_not") {
            let [a] = args::<1>(c)?;
            let a = self.expr(a)?;
            let t = self.tmp();
            return Ok(quote!({ let #t = #a; #b.un(#rt::UnOp::BitNot, #t) }));
        }
        for kw in ["soft", "unique", "solve_before"] {
            if is_ident(&c.func, kw) {
                return Err(err(c.span(), format!("`{kw}` is a statement")));
            }
        }
        let Expr::Path(fp) = &*c.func else {
            return Err(err(c.func.span(), "expected a constraint function"));
        };
        if let Mode::Fn { name, .. } = &self.mode
            && fp.path.is_ident(name)
        {
            return Err(err(c.span(), "constraint functions cannot be recursive"));
        }
        let mut binds = TokenStream2::new();
        let mut names = Vec::new();
        for a in &c.args {
            let v = self.expr(a)?;
            let t = self.tmp();
            binds.extend(quote!(let #t = #v;));
            names.push(t);
        }
        let f = &fp.path;
        Ok(quote!({ #binds #f(#b, #(#names),*) }))
    }

    /// A `{ expr }` is Rust code; in a constraint function it cannot see
    /// the parameters, which are not Rust values there.
    fn check_rust_block(&self, block: &Block) -> syn::Result<()> {
        if let Mode::Fn { params, .. } = &self.mode {
            fn walk(ts: TokenStream2, params: &[String]) -> Option<Span> {
                for t in ts {
                    match t {
                        TokenTree::Ident(i) if params.contains(&i.to_string()) => {
                            return Some(i.span());
                        }
                        TokenTree::Group(g) => {
                            if let Some(s) = walk(g.stream(), params) {
                                return Some(s);
                            }
                        }
                        _ => {}
                    }
                }
                None
            }
            if let Some(s) = walk(quote!(#block), params) {
                return Err(err(
                    s,
                    "a parameter cannot be used inside `{ }`: it may be a random variable",
                ));
            }
        }
        Ok(())
    }

    /// `const` assertions that the used fields exist on `target`.
    fn field_checks(&self, target: &TokenStream2) -> TokenStream2 {
        let rt = rt();
        let mut out = TokenStream2::new();
        let mut seen: Vec<&str> = Vec::new();
        for (name, span) in &self.fields {
            if seen.contains(&name.as_str()) {
                continue;
            }
            seen.push(name);
            let msg = format!("no field `{name}` in the randomized struct");
            out.extend(quote_spanned! {*span=>
                const _: () = ::core::assert!(
                    #rt::has_field(<#target as #rt::RandStruct>::FIELDS, #name),
                    #msg
                );
            });
        }
        out
    }
}

fn loc(span: Span) -> TokenStream2 {
    quote_spanned!(span=> ::core::file!(), ::core::line!())
}

fn is_ident(e: &Expr, name: &str) -> bool {
    matches!(e, Expr::Path(p) if p.path.is_ident(name))
}

fn args<const N: usize>(c: &syn::ExprCall) -> syn::Result<[&Expr; N]> {
    let v: Vec<&Expr> = c.args.iter().collect();
    v.try_into()
        .map_err(|_| err(c.span(), format!("expected {N} argument(s)")))
}

fn bin_op(op: &BinOp) -> syn::Result<Ident> {
    let n = match op {
        BinOp::Add(_) => "Add",
        BinOp::Sub(_) => "Sub",
        BinOp::Mul(_) => "Mul",
        BinOp::Div(_) => "Div",
        BinOp::Rem(_) => "Rem",
        BinOp::BitAnd(_) => "And",
        BinOp::BitOr(_) => "Or",
        BinOp::BitXor(_) => "Xor",
        BinOp::Shl(_) => "Shl",
        BinOp::Shr(_) => "Shr",
        BinOp::Eq(_) => "Eq",
        BinOp::Ne(_) => "Ne",
        BinOp::Lt(_) => "Lt",
        BinOp::Le(_) => "Le",
        BinOp::Gt(_) => "Gt",
        BinOp::Ge(_) => "Ge",
        BinOp::And(_) => "LAnd",
        BinOp::Or(_) => "LOr",
        other => {
            return Err(err(
                other.span(),
                "assignment is not allowed in constraints",
            ));
        }
    };
    Ok(Ident::new(n, op.span()))
}

struct DistEntry {
    key: Expr,
    each: bool,
    weight: Expr,
}

struct DistArgs {
    x: Expr,
    entries: Vec<DistEntry>,
}

impl Parse for DistArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let x: Expr = input.parse()?;
        let mut entries = Vec::new();
        while input.parse::<Token![,]>().is_ok() {
            if input.is_empty() {
                break;
            }
            let key = Expr::parse_without_eager_brace(input)?;
            input.parse::<Token![=>]>()?;
            let kw: Ident = input.parse()?;
            let each = match kw.to_string().as_str() {
                "each" => true,
                "total" => false,
                _ => return Err(err(kw.span(), "expected `each` or `total`")),
            };
            let weight = Expr::parse_without_eager_brace(input)?;
            entries.push(DistEntry { key, each, weight });
        }
        Ok(DistArgs { x, entries })
    }
}

// ===========================================================================
// #[randomize]
// ===========================================================================

struct RandFieldSpec {
    ident: Ident,
    name: String,
    rand: bool,
    width: Option<syn::LitInt>,
}

pub fn randomize(attr: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    if !attr.is_empty() {
        return Err(err(attr.span(), "`#[randomize]` takes no arguments"));
    }
    let mut st: ItemStruct = syn::parse2(item)?;
    if !st.generics.params.is_empty() {
        return Err(err(
            st.generics.span(),
            "generic structs are not supported by `#[randomize]`",
        ));
    }
    let Fields::Named(fields) = &mut st.fields else {
        return Err(err(st.span(), "expected a struct with named fields"));
    };
    let mut specs = Vec::new();
    for f in fields.named.iter_mut() {
        let mut rand = false;
        let mut width = None;
        let mut rest = Vec::new();
        for a in f.attrs.drain(..) {
            if a.path().is_ident("rand") {
                rand = true;
                if let syn::Meta::List(_) = &a.meta {
                    a.parse_nested_meta(|m| {
                        if m.path.is_ident("width") {
                            width = Some(m.value()?.parse::<syn::LitInt>()?);
                            Ok(())
                        } else {
                            Err(m.error("expected `width = N`"))
                        }
                    })?;
                }
            } else if a.path().is_ident("randc") {
                return Err(err(a.span(), "`#[randc]` is not supported yet"));
            } else {
                rest.push(a);
            }
        }
        f.attrs = rest;
        let ident = f.ident.clone().expect("named field");
        specs.push(RandFieldSpec {
            name: ident.unraw().to_string(),
            ident,
            rand,
            width,
        });
    }
    let name = st.ident.clone();
    let rt = rt();
    fields.named.push(syn::parse_quote! {
        #[doc(hidden)]
        pub __rand_state: #rt::RandState<#name>
    });

    let b = bv();
    let field_names: Vec<&String> = specs.iter().map(|s| &s.name).collect();
    let mut consts = TokenStream2::new();
    let mut declare = TokenStream2::new();
    let mut write_back = TokenStream2::new();
    let mut pre = TokenStream2::new();
    let mut post = TokenStream2::new();
    for s in &specs {
        let (id, n) = (&s.ident, &s.name);
        if s.rand {
            let c = format_ident!("{}", upper(n), span = id.span());
            consts.extend(quote! {
                #[allow(dead_code)]
                pub const #c: #rt::FieldName = #rt::FieldName::new(#n);
            });
            let width = match &s.width {
                Some(w) => quote!(::core::option::Option::Some(#w)),
                None => quote!(::core::option::Option::None),
            };
            declare.extend(quote! {
                let __rv_on = __rv_rand && self.__rand_state.is_rand_on(#n);
                let __rv_p = #rt::join(__rv_path, #n);
                if __rv_rand && !__rv_on { #b.note_rand_off(&__rv_p); }
                let __rv_o = #rt::RandField::declare(&self.#id, #b, &__rv_p, __rv_on, #width);
                __rv_m.push((#n, __rv_o));
            });
            write_back.extend(quote! {
                let __rv_on = self.__rand_state.is_rand_on(#n);
                #rt::RandField::write_back(&mut self.#id, __rv_sol, &#rt::join(__rv_path, #n), __rv_on);
            });
            pre.extend(quote!(#rt::RandField::pre(&mut self.#id);));
            post.extend(quote!(#rt::RandField::post(&mut self.#id);));
        } else {
            declare.extend(quote! {
                let __rv_o = (&#rt::Probe(&self.#id)).declare_state(#b, &#rt::join(__rv_path, #n));
                __rv_m.push((#n, __rv_o));
            });
        }
    }
    let name_str = name.to_string();
    Ok(quote! {
        #st

        #[allow(dead_code)]
        impl #name {
            #consts

            /// Solves the constraints and writes the solution.
            pub fn randomize(&mut self) -> ::core::result::Result<(), #rt::RandError> {
                <Self as #rt::Randomize>::randomize(self)
            }

            /// Like `randomize`, with a constraint set for this call only.
            pub fn randomize_with<C: #rt::Constraints<Self>>(&mut self, set: C)
                -> ::core::result::Result<(), #rt::RandError>
            {
                <Self as #rt::Randomize>::randomize_with(self, set)
            }

            /// Attaches a constraint set, kept by the object.
            pub fn with_constraints<C: #rt::Constraints<Self>>(self, set: C) -> Self {
                <Self as #rt::Randomize>::with_constraints(self, set)
            }

            pub fn set_constraints<C: #rt::Constraints<Self>>(&mut self, set: C) {
                <Self as #rt::Randomize>::set_constraints(self, set)
            }

            pub fn clear_constraints(&mut self) {
                <Self as #rt::Randomize>::clear_constraints(self)
            }

            pub fn rand_on(&mut self, f: #rt::FieldName) {
                <Self as #rt::Randomize>::rand_on(self, f)
            }

            pub fn rand_off(&mut self, f: #rt::FieldName) {
                <Self as #rt::Randomize>::rand_off(self, f)
            }

            pub fn rand_off_all(&mut self) {
                <Self as #rt::Randomize>::rand_off_all(self)
            }

            pub fn rand_on_all(&mut self) {
                <Self as #rt::Randomize>::rand_on_all(self)
            }

            pub fn is_rand_on(&self, f: #rt::FieldName) -> bool {
                <Self as #rt::Randomize>::is_rand_on(self, f)
            }

            pub fn constraint_on(&mut self, c: #rt::ConstraintName) {
                <Self as #rt::Randomize>::constraint_on(self, c)
            }

            pub fn constraint_off(&mut self, c: #rt::ConstraintName) {
                <Self as #rt::Randomize>::constraint_off(self, c)
            }

            pub fn is_constraint_on(&self, c: #rt::ConstraintName) -> bool {
                <Self as #rt::Randomize>::is_constraint_on(self, c)
            }

            pub fn srandom(&mut self, seed: u64) {
                <Self as #rt::Randomize>::srandom(self, seed)
            }

            pub fn get_randstate(&self) -> ::core::option::Option<u64> {
                <Self as #rt::Randomize>::get_randstate(self)
            }

            pub fn set_randstate(&mut self, state: u64) {
                <Self as #rt::Randomize>::set_randstate(self, state)
            }
        }

        impl #rt::RandStruct for #name {
            const FIELDS: &'static [&'static str] = &[#(#field_names),*];
            const NAME: &'static str = ::core::concat!(::core::module_path!(), "::", #name_str);

            fn __state(&self) -> &#rt::RandState<Self> {
                &self.__rand_state
            }

            fn __state_mut(&mut self) -> &mut #rt::RandState<Self> {
                &mut self.__rand_state
            }

            #[allow(unused_variables)]
            fn __fields(&self, #b: &mut #rt::Builder, __rv_path: &str, __rv_rand: bool) -> #rt::Operand {
                #[allow(unused_imports)]
                use #rt::{ViaField as _, ViaOther as _};
                let mut __rv_m: ::std::vec::Vec<(&'static str, #rt::Operand)> = ::std::vec::Vec::new();
                #declare
                #rt::Operand::Struct(__rv_m)
            }

            fn __type_blocks(&self, #b: &mut #rt::Builder) {
                #[allow(unused_imports)]
                use #rt::{TcReal as _, TcNone as _};
                (&#rt::TcProbe::<Self>::new()).type_blocks(self, #b)
            }

            #[allow(unused_variables)]
            fn __write_back(&mut self, __rv_sol: &#rt::Solution, __rv_path: &str) {
                #write_back
            }

            fn __pre(&mut self) {
                #[allow(unused_imports)]
                use #rt::{TcReal as _, TcNone as _};
                (&#rt::TcProbe::<Self>::new()).pre_randomize(self);
                #pre
            }

            fn __post(&mut self) {
                #[allow(unused_imports)]
                use #rt::{TcReal as _, TcNone as _};
                #post
                (&#rt::TcProbe::<Self>::new()).post_randomize(self);
            }
        }

        impl #rt::RandField for #name {
            fn declare(&self, b: &mut #rt::Builder, path: &str, rand: bool,
                       _width: ::core::option::Option<u32>) -> #rt::Operand {
                #rt::__declare_struct(self, b, path, rand)
            }

            fn write_back(&mut self, sol: &#rt::Solution, path: &str, rand: bool) {
                if rand {
                    #rt::RandStruct::__write_back(self, sol, path);
                }
            }

            fn pre(&mut self) {
                #rt::RandStruct::__pre(self)
            }

            fn post(&mut self) {
                #rt::RandStruct::__post(self)
            }
        }
    })
}

// ===========================================================================
// #[constraints]
// ===========================================================================

pub fn constraints(attr: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    let target: Option<Type> = if attr.is_empty() {
        None
    } else {
        let a: ForArg = syn::parse2(attr)?;
        Some(a.ty)
    };
    // `#[constraints(for T)] struct Set;` only marks the set.
    if let Ok(st) = syn::parse2::<ItemStruct>(item.clone()) {
        if target.is_none() {
            return Err(err(
                st.span(),
                "on a struct, write `#[constraints(for T)]`; random structs use `#[randomize]`",
            ));
        }
        return Ok(quote!(#st));
    }
    let im: ItemImpl = syn::parse2(item)?;
    if im.trait_.is_some() || !im.generics.params.is_empty() {
        return Err(err(im.span(), "expected `impl Type { .. }`"));
    }
    let self_ty = &im.self_ty;
    let owner = text_of(&quote!(#self_ty));
    let rt = rt();
    let b = bv();
    let mode = if target.is_some() {
        Mode::Set
    } else {
        Mode::Type
    };
    let mut cg = Cg::new(mode.clone());
    let mut consts = TokenStream2::new();
    let mut blocks = TokenStream2::new();
    let mut methods = TokenStream2::new();
    let mut replaces = TokenStream2::new();
    let mut pre = TokenStream2::new();
    let mut post = TokenStream2::new();
    for it in &im.items {
        let ImplItem::Fn(f) = it else {
            return Err(err(it.span(), "only constraint functions"));
        };
        let mut hook = None;
        let mut rep = Vec::new();
        let mut attrs = Vec::new();
        for a in &f.attrs {
            if a.path().is_ident("pre_randomize") {
                hook = Some(true);
            } else if a.path().is_ident("post_randomize") {
                hook = Some(false);
            } else if a.path().is_ident("replaces") {
                rep.push(a.parse_args::<Path>()?);
            } else {
                attrs.push(a.clone());
            }
        }
        if let Some(is_pre) = hook {
            if mode == Mode::Set {
                return Err(err(f.span(), "hooks belong to the random struct"));
            }
            let mut m = f.clone();
            m.attrs = attrs;
            let id = &m.sig.ident;
            if is_pre {
                pre.extend(quote!(self.#id();));
            } else {
                post.extend(quote!(self.#id();));
            }
            methods.extend(quote!(#m));
            continue;
        }
        if !f.sig.inputs.is_empty() || !matches!(f.sig.output, ReturnType::Default) {
            return Err(err(
                f.sig.span(),
                "a constraint block is `fn name() { .. }`, without parameters",
            ));
        }
        let fname = f.sig.ident.unraw().to_string();
        let cname = format_ident!("{}", upper(&fname), span = f.sig.ident.span());
        consts.extend(quote! {
            #[allow(dead_code)]
            pub const #cname: #rt::ConstraintName = #rt::ConstraintName::new(#owner, #fname);
        });
        let body = cg.block(&f.block)?;
        blocks.extend(quote! {
            #b.block(Self::#cname, |#b| { #body });
        });
        for r in rep {
            if mode != Mode::Set {
                return Err(err(r.span(), "`#[replaces]` is for constraint sets"));
            }
            let t = target.as_ref().expect("set target");
            let c = replaced_const(&r, t)?;
            replaces.extend(quote!(__rv_out.push(#c);));
        }
    }
    let checks = match &target {
        Some(t) => cg.field_checks(&quote!(#t)),
        None => cg.field_checks(&quote!(#self_ty)),
    };
    let tr = match &target {
        None => quote! {
            impl #rt::TypeConstraints for #self_ty {
                #[allow(unused_variables, unused_braces, clippy::all)]
                fn __type_blocks(&self, #b: &mut #rt::Builder) {
                    #blocks
                }
                fn __pre_randomize(&mut self) { #pre }
                fn __post_randomize(&mut self) { #post }
            }
        },
        Some(t) => quote! {
            impl #rt::Constraints<#t> for #self_ty {
                #[allow(unused_variables, unused_braces, clippy::all)]
                fn __blocks(&self, #b: &mut #rt::Builder) {
                    #blocks
                }
                fn __replaces(&self, __rv_out: &mut ::std::vec::Vec<#rt::ConstraintName>) {
                    #replaces
                }
            }
        },
    };
    Ok(quote! {
        #[allow(dead_code)]
        impl #self_ty {
            #consts
            #methods
        }
        #tr
        #checks
    })
}

struct ForArg {
    ty: Type,
}

impl Parse for ForArg {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        input.parse::<Token![for]>()?;
        Ok(ForArg { ty: input.parse()? })
    }
}

/// `#[replaces(name)]` -> `<Target>::NAME`; `#[replaces(Set::name)]` ->
/// `Set::NAME`.
fn replaced_const(p: &Path, target: &Type) -> syn::Result<TokenStream2> {
    let last = p.segments.last().expect("non-empty path");
    let c = format_ident!(
        "{}",
        upper(&last.ident.unraw().to_string()),
        span = last.ident.span()
    );
    if p.segments.len() == 1 {
        Ok(quote!(<#target>::#c))
    } else {
        let mut prefix: Punctuated<syn::PathSegment, Token![::]> = Punctuated::new();
        for s in p.segments.iter().take(p.segments.len() - 1) {
            prefix.push(s.clone());
        }
        Ok(quote!(#prefix::#c))
    }
}

// ===========================================================================
// #[constraint_fn]
// ===========================================================================

pub fn constraint_fn(attr: TokenStream2, item: TokenStream2) -> syn::Result<TokenStream2> {
    if !attr.is_empty() {
        return Err(err(attr.span(), "`#[constraint_fn]` takes no arguments"));
    }
    let f: ItemFn = syn::parse2(item)?;
    let rt = rt();
    let b = bv();
    let name = f.sig.ident.clone();
    let mut params = Vec::new();
    let mut casts = TokenStream2::new();
    let mut new_inputs = TokenStream2::new();
    for a in &f.sig.inputs {
        let FnArg::Typed(pt) = a else {
            return Err(err(a.span(), "constraint functions take no `self`"));
        };
        let Pat::Ident(pi) = &*pt.pat else {
            return Err(err(pt.pat.span(), "expected a parameter name"));
        };
        let id = &pi.ident;
        let ty = &pt.ty;
        params.push(id.unraw().to_string());
        new_inputs.extend(quote!(#id: #rt::Operand,));
        casts.extend(quote!(let #id = #b.cast::<#ty>(#id);));
    }
    let ReturnType::Type(_, ret) = &f.sig.output else {
        return Err(err(
            f.sig.span(),
            "a constraint function returns `bool` or a value",
        ));
    };
    let is_bool = matches!(&**ret, Type::Path(p) if p.path.is_ident("bool"));
    let mut cg = Cg::new(Mode::Fn {
        name: name.to_string(),
        params,
    });
    let stmts = &f.block.stmts;
    let (body, tail) = match stmts.last() {
        Some(Stmt::Expr(e, None)) if !matches!(e, Expr::If(_) | Expr::ForLoop(_)) => {
            let mut body = TokenStream2::new();
            for s in &stmts[..stmts.len() - 1] {
                body.extend(cg.stmt(s, false)?);
            }
            (body, Some(cg.expr(e)?))
        }
        _ => {
            let mut body = TokenStream2::new();
            for s in stmts {
                body.extend(cg.stmt(s, true)?);
            }
            (body, None)
        }
    };
    let fname = name.to_string();
    let exit = if is_bool {
        match tail {
            Some(t) => quote! {
                let __rv_tail = #t;
                #b.fn_exit_bool(::core::option::Option::Some(__rv_tail))
            },
            None => quote!(#b.fn_exit_bool(::core::option::Option::None)),
        }
    } else {
        let Some(t) = tail else {
            return Err(err(
                f.block.span(),
                "a constraint function returning a value ends with an expression",
            ));
        };
        quote! {
            let __rv_tail = #t;
            #b.fn_exit_value::<#ret>(__rv_tail)
        }
    };
    let vis = &f.vis;
    let attrs = &f.attrs;
    Ok(quote! {
        #(#attrs)*
        #[allow(unused_variables, unused_braces, clippy::all)]
        #vis fn #name(#b: &mut #rt::Builder, #new_inputs) -> #rt::Operand {
            #casts
            if !#b.fn_enter(#fname) {
                return #b.bool_lit(false);
            }
            #body
            #exit
        }
    })
}

// ===========================================================================
// #[derive(RandEnum)]
// ===========================================================================

pub fn rand_enum(item: TokenStream2) -> syn::Result<TokenStream2> {
    let e: ItemEnum = syn::parse2(item)?;
    let rt = rt();
    let name = &e.ident;
    let mut next: i128 = 0;
    let mut values = Vec::new();
    for v in &e.variants {
        if !matches!(v.fields, Fields::Unit) {
            return Err(err(v.span(), "`RandEnum` needs variants without fields"));
        }
        if let Some((_, d)) = &v.discriminant {
            next = discriminant(d)?;
        }
        values.push((v.ident.clone(), next));
        next += 1;
    }
    if values.is_empty() {
        return Err(err(e.span(), "`RandEnum` needs at least one variant"));
    }
    let (min, max) = (
        values.iter().map(|v| v.1).min().expect("non-empty"),
        values.iter().map(|v| v.1).max().expect("non-empty"),
    );
    let signed = min < 0;
    let bits = |v: i128| -> u32 {
        if v < 0 {
            128 - (!v).leading_zeros() + 1
        } else {
            (128 - v.leading_zeros()).max(1) + signed as u32
        }
    };
    let width = bits(min).max(bits(max));
    let idents: Vec<&Ident> = values.iter().map(|v| &v.0).collect();
    let vals: Vec<i128> = values.iter().map(|v| v.1).collect();
    Ok(quote! {
        impl #rt::RandValue for #name {
            const TY: ::core::option::Option<#rt::Ty> = ::core::option::Option::Some(#rt::Ty {
                width: #width,
                signed: #signed,
            });

            fn domain() -> ::core::option::Option<::std::vec::Vec<#rt::BigInt>> {
                ::core::option::Option::Some(::std::vec![#(#rt::BigInt::from(#vals)),*])
            }

            fn to_bigint(&self) -> #rt::BigInt {
                match self {
                    #(#name::#idents => #rt::BigInt::from(#vals),)*
                }
            }

            fn from_bigint(v: &#rt::BigInt) -> Self {
                let v = i128::try_from(v).expect("solved value in range");
                #(if v == #vals { return #name::#idents; })*
                ::core::unreachable!("a value outside the variants")
            }
        }
    })
}

fn discriminant(e: &Expr) -> syn::Result<i128> {
    match e {
        Expr::Lit(ExprLit {
            lit: Lit::Int(i), ..
        }) => i.base10_parse(),
        Expr::Unary(u) if matches!(u.op, UnOp::Neg(_)) => Ok(-discriminant(&u.expr)?),
        other => Err(err(other.span(), "`RandEnum` needs literal discriminants")),
    }
}

// ===========================================================================
// randomize_with!, std_randomize!, randcase!
// ===========================================================================

struct WithArgs {
    obj: Expr,
    block: Block,
}

impl Parse for WithArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let obj = input.parse()?;
        input.parse::<Token![,]>()?;
        let block = input.parse()?;
        Ok(WithArgs { obj, block })
    }
}

pub fn randomize_with(input: TokenStream2) -> syn::Result<TokenStream2> {
    let WithArgs { obj, block } = syn::parse2(input)?;
    let rt = rt();
    let b = bv();
    let mut cg = Cg::new(Mode::Inline);
    let body = cg.block(&block)?;
    Ok(quote! {
        #rt::Randomize::__randomize_inline(
            &mut #obj,
            &|#b: &mut #rt::Builder| {
                #[allow(unused_braces)]
                { #body }
            },
        )
    })
}

struct StdArgs {
    vars: Vec<Ident>,
    block: Block,
}

impl Parse for StdArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        syn::bracketed!(content in input);
        let vars = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?
            .into_iter()
            .collect();
        input.parse::<Token![,]>()?;
        let block = input.parse()?;
        Ok(StdArgs { vars, block })
    }
}

pub fn std_randomize(input: TokenStream2) -> syn::Result<TokenStream2> {
    let StdArgs { vars, block } = syn::parse2(input)?;
    let rt = rt();
    let b = bv();
    let mut cg = Cg::new(Mode::Std);
    let body = cg.block(&block)?;
    let names: Vec<String> = vars.iter().map(|v| v.to_string()).collect();
    Ok(quote! {
        match #rt::__std_randomize(
            &|#b: &mut #rt::Builder| {
                let mut __rv_m: ::std::vec::Vec<(&'static str, #rt::Operand)> = ::std::vec::Vec::new();
                #(
                    let __rv_o = #rt::RandField::declare(&#vars, #b, #names, true, ::core::option::Option::None);
                    __rv_m.push((#names, __rv_o));
                )*
                #rt::Operand::Struct(__rv_m)
            },
            &|#b: &mut #rt::Builder| {
                #[allow(unused_braces)]
                { #body }
            },
        ) {
            ::core::result::Result::Ok(__rv_s) => {
                #( #rt::RandField::write_back(&mut #vars, &__rv_s, #names, true); )*
                ::core::result::Result::Ok(())
            }
            ::core::result::Result::Err(e) => ::core::result::Result::<(), #rt::RandError>::Err(e),
        }
    })
}

struct CaseArm {
    weight: Expr,
    body: Expr,
}

struct CaseArgs {
    arms: Vec<CaseArm>,
}

impl Parse for CaseArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut arms = Vec::new();
        while !input.is_empty() {
            let weight = Expr::parse_without_eager_brace(input)?;
            input.parse::<Token![=>]>()?;
            let body: Expr = input.parse()?;
            arms.push(CaseArm { weight, body });
            if input.parse::<Token![,]>().is_err() {
                break;
            }
        }
        Ok(CaseArgs { arms })
    }
}

pub fn randcase(input: TokenStream2) -> syn::Result<TokenStream2> {
    let CaseArgs { arms } = syn::parse2(input)?;
    let rt = rt();
    let weights = arms.iter().map(|a| &a.weight);
    let idx = 0..arms.len();
    let bodies = arms.iter().map(|a| &a.body);
    Ok(quote! {
        match #rt::__randcase_pick(&[#((#weights) as u64),*]) {
            #(::core::option::Option::Some(#idx) => { #bodies; })*
            _ => {}
        }
    })
}
