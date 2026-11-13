//! Procedural macros of `rustdv-veryl`: `#[modport]` and `#[clocking]`.
//! See `rustdv_veryl::clocking` for how they are used.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::{Field, Fields, Ident, ItemStruct, Type, parse_macro_input};

/// The role a field attribute gives a field.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Input,
    Output,
    Modport,
}

struct FieldSpec {
    ident: Ident,
    ty: Type,
    role: Role,
    /// The signal or scope name the field binds to by default.
    name: String,
}

/// Removes the role attribute of each field and returns the fields' specs.
fn take_fields(item: &mut ItemStruct, allow_modport: bool) -> syn::Result<Vec<FieldSpec>> {
    let Fields::Named(fields) = &mut item.fields else {
        return Err(syn::Error::new(
            item.span(),
            "expected a struct with named fields",
        ));
    };
    let mut specs = Vec::new();
    for field in fields.named.iter_mut() {
        let role = take_role(field, allow_modport)?;
        let ident = field.ident.clone().expect("named field");
        specs.push(FieldSpec {
            name: ident.unraw().to_string(),
            ident,
            ty: field.ty.clone(),
            role,
        });
    }
    Ok(specs)
}

fn take_role(field: &mut Field, allow_modport: bool) -> syn::Result<Role> {
    let mut role = None;
    let mut rest = Vec::new();
    for attr in field.attrs.drain(..) {
        let this = if attr.path().is_ident("input") {
            Some(Role::Input)
        } else if attr.path().is_ident("output") {
            Some(Role::Output)
        } else if attr.path().is_ident("modport") && allow_modport {
            Some(Role::Modport)
        } else {
            None
        };
        match this {
            Some(this) => {
                attr.meta.require_path_only()?;
                if role.replace(this).is_some() {
                    return Err(syn::Error::new(
                        attr.span(),
                        "a field takes one of `#[input]`, `#[output]` and `#[modport]`",
                    ));
                }
            }
            None => rest.push(attr),
        }
    }
    field.attrs = rest;
    role.ok_or_else(|| {
        let expected = if allow_modport {
            "`#[input]`, `#[output]` or `#[modport]`"
        } else {
            "`#[input]` or `#[output]`"
        };
        syn::Error::new(field.span(), format!("the field needs {expected}"))
    })
}

/// Adds the hidden field holding the bound members.
fn push_hidden_field(item: &mut ItemStruct, ty: &Ident) {
    let Fields::Named(fields) = &mut item.fields else {
        unreachable!("checked by take_fields");
    };
    fields.named.push(syn::parse_quote! {
        #[doc(hidden)]
        __rustdv_veryl: #ty
    });
}

/// Declares the members of an interface's modport as value fields:
/// `#[input]` fields are sampled, `#[output]` fields are driven. Use it as a
/// `#[modport]` field of a `#[clocking]` struct, which binds the members by
/// field name under the scope of the interface instance.
#[proc_macro_attribute]
pub fn modport(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = TokenStream2::from(attr);
    let mut item = parse_macro_input!(item as ItemStruct);
    match expand_modport(attr, &mut item) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_modport(attr: TokenStream2, item: &mut ItemStruct) -> syn::Result<TokenStream2> {
    if !attr.is_empty() {
        return Err(syn::Error::new(
            attr.span(),
            "`#[modport]` takes no arguments",
        ));
    }
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new(
            item.generics.span(),
            "a modport struct cannot be generic",
        ));
    }
    let specs = take_fields(item, false)?;
    let name = item.ident.clone();
    let members = format_ident!("__RustdvVeryl{}Members", name);
    push_hidden_field(item, &members);

    let rt = quote!(::rustdv_veryl::clocking);
    let member_fields = specs.iter().map(|f| {
        let (ident, ty) = (&f.ident, &f.ty);
        quote!(#ident: #rt::Member<#ty>)
    });
    let binds = specs.iter().map(|f| {
        let (ident, ty, path) = (&f.ident, &f.ty, &f.name);
        let m = format_ident!("__m_{}", ident.unraw());
        quote!(let (#m, #ident) = #rt::Member::<#ty>::bind(scope, #path)?;)
    });
    let values = specs.iter().map(|f| &f.ident);
    let member_inits = specs.iter().map(|f| {
        let ident = &f.ident;
        let m = format_ident!("__m_{}", ident.unraw());
        quote!(#ident: #m)
    });
    let loads = specs.iter().filter(|f| f.role == Role::Input).map(|f| {
        let ident = &f.ident;
        quote!(self.#ident = self.__rustdv_veryl.#ident.load();)
    });
    let stores = specs.iter().filter(|f| f.role == Role::Output).map(|f| {
        let ident = &f.ident;
        quote!(self.__rustdv_veryl.#ident.store(&self.#ident);)
    });

    Ok(quote! {
        #item

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        #[derive(Debug)]
        struct #members {
            #(#member_fields,)*
        }

        impl #rt::Modport for #name {
            fn __bind(
                scope: &#rt::__private::HierarchyHandle,
            ) -> ::core::result::Result<Self, #rt::__private::TestError> {
                #(#binds)*
                ::core::result::Result::Ok(Self {
                    #(#values,)*
                    __rustdv_veryl: #members { #(#member_inits,)* },
                })
            }

            fn __load(&mut self) {
                #(#loads)*
            }

            fn __store(&mut self) {
                #(#stores)*
            }
        }
    })
}

/// Makes the struct a clocking block: `#[input]` and `#[output]` fields are
/// signals, `#[modport]` fields are `#[modport]` structs. Generates `new`,
/// `tick` and the waits `posedge`, `negedge`, `edge`, `changed` and `wait`,
/// and a `<Name>Paths` struct overriding the path each field binds to.
#[proc_macro_attribute]
pub fn clocking(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr = TokenStream2::from(attr);
    let mut item = parse_macro_input!(item as ItemStruct);
    match expand_clocking(attr, &mut item) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_clocking(attr: TokenStream2, item: &mut ItemStruct) -> syn::Result<TokenStream2> {
    if !attr.is_empty() {
        return Err(syn::Error::new(
            attr.span(),
            "`#[clocking]` takes no arguments",
        ));
    }
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new(
            item.generics.span(),
            "a clocking struct cannot be generic",
        ));
    }
    let specs = take_fields(item, true)?;
    let name = item.ident.clone();
    let vis = item.vis.clone();
    let state = format_ident!("__RustdvVerylClocking{}", name);
    let paths = format_ident!("{}Paths", name);
    push_hidden_field(item, &state);

    let rt = quote!(::rustdv_veryl::clocking);
    let signals: Vec<&FieldSpec> = specs.iter().filter(|f| f.role != Role::Modport).collect();
    let modports: Vec<&FieldSpec> = specs.iter().filter(|f| f.role == Role::Modport).collect();

    let state_fields = signals.iter().map(|f| {
        let (ident, ty) = (&f.ident, &f.ty);
        quote!(#ident: #rt::Member<#ty>)
    });
    let path_fields = specs.iter().map(|f| {
        let ident = &f.ident;
        let doc = format!("Path of `{}`; defaults to `\"{}\"`.", f.name, f.name);
        quote!(#[doc = #doc] pub #ident: ::core::option::Option<&'a str>)
    });
    let binds = specs.iter().map(|f| {
        let (ident, ty, default) = (&f.ident, &f.ty, &f.name);
        let path = quote!(paths.#ident.unwrap_or(#default));
        if f.role == Role::Modport {
            quote! {
                let #ident = <#ty as #rt::Modport>::__bind(
                    &#rt::modport_scope(scope, #path)?,
                )?;
            }
        } else {
            let m = format_ident!("__m_{}", ident.unraw());
            quote!(let (#m, #ident) = #rt::Member::<#ty>::bind(scope, #path)?;)
        }
    });
    let values = specs.iter().map(|f| &f.ident);
    let state_inits = signals.iter().map(|f| {
        let ident = &f.ident;
        let m = format_ident!("__m_{}", ident.unraw());
        quote!(#ident: #m)
    });
    let loads = signals
        .iter()
        .filter(|f| f.role == Role::Input)
        .map(|f| {
            let ident = &f.ident;
            quote!(self.#ident = self.__rustdv_veryl.#ident.load();)
        })
        .chain(modports.iter().map(|f| {
            let ident = &f.ident;
            quote!(#rt::Modport::__load(&mut self.#ident);)
        }));
    let stores = signals
        .iter()
        .filter(|f| f.role == Role::Output)
        .map(|f| {
            let ident = &f.ident;
            quote!(self.__rustdv_veryl.#ident.store(&self.#ident);)
        })
        .chain(modports.iter().map(|f| {
            let ident = &f.ident;
            quote!(#rt::Modport::__store(&mut self.#ident);)
        }));
    let paths_doc = format!(
        "Paths the fields of [`{name}`] bind to, under the scope given to \
         [`{name}::new`]; a field left `None` binds to its own name."
    );

    Ok(quote! {
        #item

        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        #[derive(Debug)]
        struct #state {
            clock: #rt::__private::LogicHandle,
            #(#state_fields,)*
        }

        #[doc = #paths_doc]
        #[derive(Clone, Copy, Debug, Default)]
        #vis struct #paths<'a> {
            #(#path_fields,)*
            #[doc(hidden)]
            pub __marker: ::core::marker::PhantomData<&'a ()>,
        }

        impl #name {
            /// Binds the fields under `scope`, at the paths `paths` gives
            /// or their own names, to be synchronized with the rising edges
            /// of `clock`. Fails if a signal is missing or does not fit its
            /// field's type.
            #vis fn new(
                scope: &#rt::__private::HierarchyHandle,
                clock: #rt::__private::LogicHandle,
                paths: #paths<'_>,
            ) -> ::core::result::Result<Self, #rt::__private::TestError> {
                #(#binds)*
                ::core::result::Result::Ok(Self {
                    #(#values,)*
                    __rustdv_veryl: #state { clock, #(#state_inits,)* },
                })
            }

            /// Drives the outputs changed since the last tick, waits for the
            /// rising edge of the clock, and samples every input.
            #vis async fn tick(&mut self) {
                #rt::tick(self).await
            }

            /// Waits until `f` goes from false to true at a clock edge.
            #vis fn posedge<'a>(
                &'a mut self,
                f: impl FnMut(&Self) -> bool + 'a,
            ) -> #rt::Wait<'a, Self> {
                #rt::waits::posedge(self, f)
            }

            /// Waits until `f` goes from true to false at a clock edge.
            #vis fn negedge<'a>(
                &'a mut self,
                f: impl FnMut(&Self) -> bool + 'a,
            ) -> #rt::Wait<'a, Self> {
                #rt::waits::negedge(self, f)
            }

            /// Waits until `f` changes at a clock edge, either way.
            #vis fn edge<'a>(
                &'a mut self,
                f: impl FnMut(&Self) -> bool + 'a,
            ) -> #rt::Wait<'a, Self> {
                #rt::waits::edge(self, f)
            }

            /// Waits until the value of `f` changes at a clock edge.
            #vis fn changed<'a, T: ::core::cmp::PartialEq + 'a>(
                &'a mut self,
                f: impl FnMut(&Self) -> T + 'a,
            ) -> #rt::Wait<'a, Self> {
                #rt::waits::changed(self, f)
            }

            /// Waits until `f` holds; returns at once if it already does.
            #vis fn wait<'a>(
                &'a mut self,
                f: impl FnMut(&Self) -> bool + 'a,
            ) -> #rt::Wait<'a, Self> {
                #rt::waits::wait(self, f)
            }
        }

        impl #rt::ClockingBlock for #name {
            fn __clock(&self) -> #rt::__private::LogicHandle {
                self.__rustdv_veryl.clock
            }

            fn __load(&mut self) {
                #(#loads)*
            }

            fn __store(&mut self) {
                #(#stores)*
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Wait macros: `posedge!(cb.bus.done)` and friends
// ---------------------------------------------------------------------------

/// The argument of a wait macro: an expression rooted at the clocking block
/// variable, which the closure reads through its argument instead.
struct WaitArgs {
    cb: Ident,
    expr: syn::Expr,
}

impl syn::parse::Parse for WaitArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let expr: syn::Expr = input.parse()?;
        if !input.is_empty() {
            return Err(input.error(
                "a wait macro takes one expression; to limit the wait, use the `_within` \
                 form (`posedge_within!(cb.bus.done, 8)`)",
            ));
        }
        WaitArgs::new(expr)
    }
}

impl WaitArgs {
    fn new(mut expr: syn::Expr) -> syn::Result<Self> {
        let cb = root_ident(&expr).ok_or_else(|| {
            syn::Error::new(
                expr.span(),
                "the expression must start from the clocking block variable, e.g. `cb.bus.done`",
            )
        })?;
        let param = Ident::new("__rustdv_veryl_cb", proc_macro2::Span::mixed_site());
        syn::visit_mut::VisitMut::visit_expr_mut(
            &mut Rename {
                from: cb.clone(),
                to: param,
            },
            &mut expr,
        );
        Ok(Self { cb, expr })
    }
}

/// The variable an expression over a clocking block starts from: `cb` in
/// `cb.bus.done`, `cb.cnt % 4 == 0` or `(cb.a, cb.b)`.
fn root_ident(expr: &syn::Expr) -> Option<Ident> {
    match expr {
        syn::Expr::Path(p) => p.path.get_ident().cloned(),
        syn::Expr::Field(x) => root_ident(&x.base),
        syn::Expr::MethodCall(x) => root_ident(&x.receiver),
        syn::Expr::Index(x) => root_ident(&x.expr),
        syn::Expr::Binary(x) => root_ident(&x.left),
        syn::Expr::Unary(x) => root_ident(&x.expr),
        syn::Expr::Paren(x) => root_ident(&x.expr),
        syn::Expr::Cast(x) => root_ident(&x.expr),
        syn::Expr::Tuple(x) => x.elems.first().and_then(root_ident),
        _ => None,
    }
}

/// Renames the clocking block variable to the closure argument.
struct Rename {
    from: Ident,
    to: Ident,
}

impl syn::visit_mut::VisitMut for Rename {
    fn visit_expr_path_mut(&mut self, p: &mut syn::ExprPath) {
        if p.qself.is_none() && p.path.is_ident(&self.from) {
            p.path = self.to.clone().into();
        }
    }
}

fn expand_wait(method: &str, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as WaitArgs);
    let WaitArgs { cb, expr } = args;
    let method = Ident::new(method, proc_macro2::Span::call_site());
    let param = Ident::new("__rustdv_veryl_cb", proc_macro2::Span::mixed_site());
    quote! {
        #cb.#method(move |#param| #expr).await
    }
    .into()
}

/// Waits until a `bool` over a clocking block goes from false to true at a
/// clock edge, like SystemVerilog's `@(posedge cb.done)`:
/// `posedge!(cb.bus.done)`. It stands for
/// `cb.posedge(|c| c.bus.done).await`, so it is used in async code. It never
/// times out; `posedge_within!` limits the wait.
#[proc_macro]
pub fn posedge(input: TokenStream) -> TokenStream {
    expand_wait("posedge", input)
}

/// Waits until a `bool` over a clocking block goes from true to false at a
/// clock edge, like SystemVerilog's `@(negedge cb.done)`. See `posedge!`.
#[proc_macro]
pub fn negedge(input: TokenStream) -> TokenStream {
    expand_wait("negedge", input)
}

/// Waits until a `bool` over a clocking block changes either way at a clock
/// edge, like SystemVerilog's `@(edge cb.done)`. See `posedge!`.
#[proc_macro]
pub fn edge(input: TokenStream) -> TokenStream {
    expand_wait("edge", input)
}

/// Waits until a value over a clocking block changes at a clock edge, like
/// SystemVerilog's `@(cb.state)`: `changed!(cb.state)`, or
/// `changed!((cb.a, cb.b))` for several. See `posedge!`.
#[proc_macro]
pub fn changed(input: TokenStream) -> TokenStream {
    expand_wait("changed", input)
}

/// Arguments of a `_within` wait macro: the expression, then the edge limit.
struct WaitWithinArgs {
    wait: WaitArgs,
    edges: syn::Expr,
    /// The source text of the expression, naming the wait in its error.
    label: String,
}

impl syn::parse::Parse for WaitWithinArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let expr: syn::Expr = input.parse()?;
        input.parse::<syn::Token![,]>()?;
        let edges: syn::Expr = input.parse()?;
        if !input.is_empty() {
            return Err(input.error("expected `(expression, edges)`"));
        }
        let label = expr_text(&expr);
        Ok(Self {
            wait: WaitArgs::new(expr)?,
            edges,
            label,
        })
    }
}

/// The expression as written, for messages: `cb.bus.done`, not the
/// token-spaced `cb . bus . done`.
fn expr_text(expr: &syn::Expr) -> String {
    let mut text = quote!(#expr).to_string();
    for (spaced, tight) in [
        (" . ", "."),
        (" (", "("),
        ("( ", "("),
        (" )", ")"),
        (" [", "["),
        ("[ ", "["),
        (" ]", "]"),
        (" ,", ","),
    ] {
        text = text.replace(spaced, tight);
    }
    text
}

fn expand_wait_within(method: &str, input: TokenStream) -> TokenStream {
    let WaitWithinArgs { wait, edges, label } = parse_macro_input!(input as WaitWithinArgs);
    let WaitArgs { cb, expr } = wait;
    let method = Ident::new(method, proc_macro2::Span::call_site());
    let param = Ident::new("__rustdv_veryl_cb", proc_macro2::Span::mixed_site());
    quote! {
        #cb.#method(move |#param| #expr).within(#edges).label(#label).await
    }
    .into()
}

/// `posedge!` limited to a number of clock edges:
/// `posedge_within!(cb.bus.done, 8)?`. Returns
/// `Result<(), ClockingTimeout>`, naming the wait by its expression.
#[proc_macro]
pub fn posedge_within(input: TokenStream) -> TokenStream {
    expand_wait_within("posedge", input)
}

/// `negedge!` limited to a number of clock edges. See `posedge_within!`.
#[proc_macro]
pub fn negedge_within(input: TokenStream) -> TokenStream {
    expand_wait_within("negedge", input)
}

/// `edge!` limited to a number of clock edges. See `posedge_within!`.
#[proc_macro]
pub fn edge_within(input: TokenStream) -> TokenStream {
    expand_wait_within("edge", input)
}

/// `changed!` limited to a number of clock edges. See `posedge_within!`.
#[proc_macro]
pub fn changed_within(input: TokenStream) -> TokenStream {
    expand_wait_within("changed", input)
}
